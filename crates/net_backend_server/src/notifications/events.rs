//! What games can hook into in the notifications module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeNotify`] | before | a notification is about to be stored for a player (from server code or a module); change it, or refuse (e.g. the player muted that kind) |
//! | [`AfterNotify`] | after | it is stored (and pushed to the player's open connections) |
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::notifications::events::BeforeNotify;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeNotify, _, _>(|_ctx, notify| async move {
//!     // The game keeps a list of muted kinds per player; here: nobody gets "promo".
//!     if notify.notification.kind == "promo" {
//!         return Ok(Decision::Reject(AppError::forbidden("muted")));
//!     }
//!     Ok(Decision::Continue(notify))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::notifications::Notification;
use net_backend_protocol::UserId;

use super::service::NewNotification;
use crate::hooks::Event;

/// A notification is about to be stored. Hooks may change `notification` (checked again
/// afterwards) or refuse (the caller of [`NotificationService::send`](super::NotificationService::send)
/// gets the error and nothing is stored).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeNotify {
    /// The player who gets it.
    pub user_id: UserId,
    /// The notification.
    pub notification: NewNotification,
}

impl Event for BeforeNotify {
    const NAME: &'static str = "notifications.before_notify";
}

/// A notification is stored and pushed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterNotify {
    /// The player who got it.
    pub user_id: UserId,
    /// The stored notification.
    pub notification: Notification,
}

impl Event for AfterNotify {
    const NAME: &'static str = "notifications.after_notify";
}
