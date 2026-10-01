//! The audit log (`auth_audit_log`): who did what, when, from where. Security events of the auth
//! module and every admin action are written here, admin actions in the same transaction as the
//! change. Games add their own entries with [`record`] / [`record_tx`] (e.g. `game.item_granted`).
//!
//! Never put secrets into an entry: no passwords, tokens, hashes or mail bodies.
//!
//! Actions written by the auth module: `auth.register`, `auth.login`, `auth.login_failed`,
//! `auth.steam_login`, `auth.steam_linked`, `auth.logout`, `auth.refresh_reused`,
//! `auth.password_changed`, `auth.password_reset_requested`, `auth.password_reset`,
//! `auth.email_verified`, `admin.ban`, `admin.unban`, `admin.revoke_sessions`, `admin.role_grant`,
//! `admin.role_revoke`, `cli.user_create` (and the `cli.*` twins of the admin actions).

use std::net::IpAddr;

use net_backend_protocol::{UnixMillis, UserId};
use sea_query::{InsertStatement, Query};
use serde_json::Value;

use crate::db::{Db, DbError, DbTx};
use crate::http::RequestId;

/// The longest `data` text stored (longer details are cut and marked).
pub const MAX_DATA_BYTES: usize = 2048;

/// One entry to write.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AuditRecord {
    /// Who acted (`None`: the server, the command line, an anonymous request).
    pub actor: Option<UserId>,
    /// What happened: `[a-z0-9_.-]`, at most 64 bytes (`game.item_granted`).
    pub action: String,
    /// The kind of thing it happened to (`user`).
    pub target_type: Option<String>,
    /// Its id as text.
    pub target_id: Option<String>,
    /// The client address.
    pub ip: Option<IpAddr>,
    /// The request id.
    pub request_id: Option<String>,
    /// Details (JSON, at most [`MAX_DATA_BYTES`]).
    pub data: Option<Value>,
}

impl AuditRecord {
    /// An entry for this action.
    pub fn new(action: impl Into<String>) -> Self {
        Self { actor: None, action: action.into(), target_type: None, target_id: None, ip: None, request_id: None, data: None }
    }

    /// The same entry with an actor.
    pub fn actor(mut self, actor: Option<UserId>) -> Self {
        self.actor = actor;
        self
    }

    /// The same entry about a user.
    pub fn target_user(mut self, user: UserId) -> Self {
        self.target_type = Some("user".into());
        self.target_id = Some(user.get().to_string());
        self
    }

    /// The same entry with another target.
    pub fn target(mut self, target_type: impl Into<String>, target_id: impl Into<String>) -> Self {
        self.target_type = Some(target_type.into());
        self.target_id = Some(target_id.into());
        self
    }

    /// The same entry with the client address.
    pub fn ip(mut self, ip: Option<IpAddr>) -> Self {
        self.ip = ip;
        self
    }

    /// The same entry with a request id.
    pub fn request_id(mut self, request_id: Option<&RequestId>) -> Self {
        self.request_id = request_id.map(|r| r.as_str().to_string());
        self
    }

    /// The same entry with details.
    pub fn data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    fn insert(&self, now: UnixMillis) -> Result<InsertStatement, DbError> {
        let action: String = self.action.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-')).take(64).collect();
        if action.is_empty() {
            return Err(DbError::Build("an audit action must be [a-z0-9_.-]".into()));
        }
        let data = self.data.as_ref().map(|d| {
            let text = d.to_string();
            if text.len() <= MAX_DATA_BYTES {
                text
            } else {
                serde_json::json!({ "truncated": true }).to_string()
            }
        });
        let cut = |s: &Option<String>, n: usize| s.as_ref().map(|s| s.chars().filter(|c| !c.is_control()).take(n).collect::<String>());
        let mut insert = Query::insert();
        insert
            .into_table("auth_audit_log")
            .columns(["actor_user_id", "action", "target_type", "target_id", "ip", "request_id", "data", "created_at"])
            .values([
                self.actor.map(|a| a.get()).into(),
                action.into(),
                cut(&self.target_type, 32).into(),
                cut(&self.target_id, 64).into(),
                self.ip.map(|ip| ip.to_string()).into(),
                cut(&self.request_id, 64).into(),
                data.into(),
                now.get().into(),
            ])
            .map_err(|e| DbError::Build(e.to_string()))?;
        Ok(insert)
    }
}

/// Write an entry (outside a transaction).
pub async fn record(db: &Db, now: UnixMillis, entry: &AuditRecord) -> Result<(), DbError> {
    db.execute(&entry.insert(now)?).await.map(|_| ())
}

/// Write an entry inside a transaction (with the change it describes).
pub async fn record_tx(tx: &mut DbTx, now: UnixMillis, entry: &AuditRecord) -> Result<(), DbError> {
    tx.execute(&entry.insert(now)?).await.map(|_| ())
}

/// Write an entry; a failure is logged, never answered (for security events next to a request).
pub(crate) async fn record_logged(db: &Db, now: UnixMillis, entry: &AuditRecord) {
    if let Err(error) = record(db, now, entry).await {
        tracing::error!(action = %entry.action, error = %error.describe(), "could not write an audit entry");
    }
}
