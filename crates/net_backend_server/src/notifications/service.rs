//! [`NotificationService`]: creating notifications (server code, other modules) and the players'
//! reads, marks and deletes.
//!
//! **Sending** runs the [`BeforeNotify`] hooks, stores the notification and trims the player's
//! oldest beyond `max_per_user` in one write transaction (a plain read of the ids past the limit,
//! then a DELETE by id: no gap locks on MySQL; a reported deadlock runs it again, [`Retry`]), then
//! pushes `notify.new` to every open connection of the player (through the hub's `Broadcaster`, so
//! on every instance) and runs the [`AfterNotify`] hooks.

use std::sync::Arc;

use net_backend_protocol::notifications::{
    is_valid_kind, text_problem, MarkAck, MarkNotifications, Notification, NotificationCount, NotificationQuery, MAX_KIND_BYTES,
};
use net_backend_protocol::{Cursor, NotificationId, Page, UnixMillis, UserId, ValidationDetails};
use serde_json::Value;

use super::config::NotificationsConfig;
use super::events::{AfterNotify, BeforeNotify};
use super::store::{self, CountRow, IdRow, NotificationRow};
use crate::db::Retry;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::state::AppState;

/// How many rows the purge and the trim delete per statement.
const BATCH: u64 = 1000;

/// A notification to send ([`NotificationService::send`]).
///
/// ```
/// use net_backend_server::notifications::NewNotification;
/// use net_backend_server::protocol::UserId;
///
/// let gift = NewNotification::new("gift").with_text("Ada sent you 50 gold").with_data(serde_json::json!({"gold": 50})).with_sender(UserId(42));
/// # let _ = gift;
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NewNotification {
    /// What it is about: 1-64 bytes of `a-z 0-9 _ . : -`, starting with a letter.
    pub kind: String,
    /// A text to show (at most 1000 characters, the chat text rules).
    pub text: Option<String>,
    /// The game's data (at most `max_data_bytes` of JSON).
    pub data: Option<Value>,
    /// The account that caused it.
    pub sender: Option<UserId>,
}

impl NewNotification {
    /// A notification of this kind, without text, data or sender.
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into(), text: None, data: None, sender: None }
    }

    /// The same notification with a text.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// The same notification with data.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// The same notification with a sender.
    pub fn with_sender(mut self, sender: UserId) -> Self {
        self.sender = Some(sender);
        self
    }

    /// The shape rules (422 `validation_failed` with the fields).
    pub(crate) fn validate(&self, max_data_bytes: usize) -> Result<(), AppError> {
        let mut details = ValidationDetails::new();
        if !is_valid_kind(&self.kind) {
            details.add("kind", format!("must be 1 to {MAX_KIND_BYTES} bytes of a-z, 0-9, _ . : - starting with a letter"));
        }
        if let Some(problem) = self.text.as_deref().and_then(text_problem) {
            details.add("text", problem);
        }
        if let Some(data) = &self.data {
            if net_backend_protocol::storage::value_bytes(data) > max_data_bytes {
                details.add("data", format!("is larger than {max_data_bytes} bytes"));
            }
        }
        details.into_result().map_err(AppError::from)
    }
}

/// Sends notifications and serves the players' reads, marks and deletes. A state value
/// (`Ext<NotificationService>` in handlers, `state.get::<NotificationService>()` elsewhere) once
/// the [`Notifications`](super::Notifications) module is registered. Server code and other
/// modules create notifications with [`send`](Self::send); players never do.
#[derive(Clone)]
pub struct NotificationService(Arc<NotificationsConfig>);

impl std::fmt::Debug for NotificationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NotificationService").field(&self.0).finish()
    }
}

fn decode(row: NotificationRow) -> Notification {
    let mut notification = Notification::new(NotificationId(row.id), row.kind, UnixMillis(row.created_at)).with_read(row.read_at.is_some());
    if let Some(text) = row.text {
        notification = notification.with_text(text);
    }
    if let Some(data) = row.data.and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(|v| !v.is_null()) {
        notification = notification.with_data(data);
    }
    if let Some(sender) = row.sender_id {
        notification = notification.with_sender(UserId(sender));
    }
    notification
}

fn count(row: CountRow) -> u64 {
    u64::try_from(row.n).unwrap_or(0)
}

impl NotificationService {
    pub(crate) fn new(config: NotificationsConfig) -> Self {
        Self(Arc::new(config))
    }

    /// The settings.
    pub fn config(&self) -> &NotificationsConfig {
        &self.0
    }

    /// Send a notification to `user`: the hooks, stored (the player's oldest beyond
    /// `max_per_user` deleted), pushed as `notify.new` to the player's open connections. 404 for
    /// an unknown account; 422 for a notification that breaks the rules; a hook's refusal as is.
    pub async fn send(&self, state: &AppState, user: UserId, notification: NewNotification) -> Result<Notification, AppError> {
        self.send_with(state, &HookCtx::new(state.clone(), None), user, notification).await
    }

    /// [`send`](Self::send) with the hook context of a request.
    pub async fn send_with(&self, state: &AppState, ctx: &HookCtx, user: UserId, notification: NewNotification) -> Result<Notification, AppError> {
        let max = self.0.max_data_bytes;
        notification.validate(max)?;
        let event = state.hooks().run_before(ctx, BeforeNotify { user_id: user, notification }).await?;
        let new = event.notification;
        new.validate(max)?;
        let data = match &new.data {
            Some(value) if !value.is_null() => Some(serde_json::to_vec(value).map_err(AppError::internal)?),
            _ => None,
        };
        let now = state.now().get();
        let keep = u64::from(self.0.max_per_user);
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let id = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let row =
                    store::New { user: user.get(), kind: &new.kind, text: new.text.as_deref(), data: data.clone(), sender: new.sender.map(UserId::get), now };
                let id = match tx.insert_id(&store::insert(row)?, "id").await {
                    Ok(id) => id,
                    Err(error) if error.is_foreign_key_violation() => {
                        return Err(AppError::not_found(if new.sender.is_some() { "no such account (recipient or sender)" } else { "no such account" }))
                    }
                    Err(error) => return Err(error.into()),
                };
                let old: Vec<i64> = tx.fetch_all::<IdRow, _>(&store::overflow(user.get(), keep, BATCH)).await?.into_iter().map(|r| r.id).collect();
                if !old.is_empty() {
                    tx.execute(&store::delete_ids(&old)).await?;
                }
                Ok(id)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let mut stored = Notification::new(NotificationId(id), new.kind, UnixMillis(now));
        if let Some(text) = new.text {
            stored = stored.with_text(text);
        }
        if let Some(data) = new.data.filter(|v| !v.is_null()) {
            stored = stored.with_data(data);
        }
        if let Some(sender) = new.sender {
            stored = stored.with_sender(sender);
        }
        if let Err(error) = state.ws().push_user(user, &stored) {
            tracing::warn!(%error, user = user.get(), "notifications: the notify.new push failed (the notification is stored)");
        }
        state.hooks().run_after(ctx, Arc::new(AfterNotify { user_id: user, notification: stored.clone() })).await;
        Ok(stored)
    }

    /// A page of `user`'s notifications, newest first.
    pub async fn list(&self, state: &AppState, user: UserId, query: &NotificationQuery) -> Result<Page<Notification>, AppError> {
        let before = match &query.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let limit = u64::from(query.limit_or_default());
        let unread_only = query.unread_only.unwrap_or(false);
        let mut rows = state.db().fetch_all::<NotificationRow, _>(&store::page(user.get(), before, unread_only, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        Ok(Page::new(rows.into_iter().map(decode).collect(), next))
    }

    /// How many notifications `user` has, and how many are unread.
    pub async fn count(&self, state: &AppState, user: UserId) -> Result<NotificationCount, AppError> {
        let unread = count(state.db().fetch_one::<CountRow, _>(&store::count(user.get(), true)).await?);
        let total = count(state.db().fetch_one::<CountRow, _>(&store::count(user.get(), false)).await?);
        Ok(NotificationCount::new(unread, total))
    }

    async fn unread(state: &AppState, user: UserId) -> Result<u64, AppError> {
        Ok(count(state.db().fetch_one::<CountRow, _>(&store::count(user.get(), true)).await?))
    }

    /// Mark some or all of `user`'s notifications read or unread (other players' ids are skipped).
    pub async fn mark(&self, state: &AppState, user: UserId, mark: &MarkNotifications) -> Result<MarkAck, AppError> {
        mark.validate()?;
        let wanted: Vec<i64> = mark.ids.iter().map(|id| id.get()).collect();
        let ids: Vec<i64> = state
            .db()
            .fetch_all::<IdRow, _>(&store::to_mark(user.get(), (!mark.all).then_some(wanted.as_slice()), mark.read))
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();
        let mut changed = 0u64;
        let read_at = mark.read.then(|| state.now().get());
        for chunk in ids.chunks(BATCH as usize) {
            changed = changed.saturating_add(state.db().execute(&store::mark(chunk, read_at)).await?);
        }
        Ok(MarkAck::new(changed, Self::unread(state, user).await?))
    }

    /// Delete one of `user`'s notifications; `true` if it was there.
    pub async fn delete(&self, state: &AppState, user: UserId, id: NotificationId) -> Result<bool, AppError> {
        Ok(state.db().execute(&store::delete(user.get(), id.get())).await? > 0)
    }

    /// Delete the notifications older than `retention_days` (in batches); how many.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let Some(cutoff) = self.0.cutoff(state.now().get()) else { return Ok(0) };
        let mut deleted = 0u64;
        loop {
            let ids: Vec<i64> = state.db().fetch_all::<IdRow, _>(&store::expired(cutoff, BATCH)).await?.into_iter().map(|r| r.id).collect();
            if ids.is_empty() {
                return Ok(deleted);
            }
            deleted = deleted.saturating_add(state.db().execute(&store::delete_ids(&ids)).await?);
            if (ids.len() as u64) < BATCH {
                return Ok(deleted);
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_rows() {
        assert!(NewNotification::new("reward").with_text("hi").validate(10).is_ok());
        let error = NewNotification::new("Reward").with_text(" ").with_data(serde_json::json!("x".repeat(20))).validate(10).err();
        let details = error.map(|e| e.api_error().details.clone().unwrap_or_default()).unwrap_or_default();
        for field in ["kind", "text", "data"] {
            assert!(details["fields"][field].is_array(), "{field}: {details}");
        }
        let row = NotificationRow { id: 7, kind: "gift".into(), text: None, data: Some(b"null".to_vec()), sender_id: Some(3), read_at: Some(1), created_at: 5 };
        let n = decode(row);
        assert_eq!((n.data, n.sender, n.read), (None, Some(UserId(3)), true));
    }
}
