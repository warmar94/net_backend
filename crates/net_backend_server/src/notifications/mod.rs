//! Notifications: stored per player, pushed live, read / unread, deleted, kept for a retention; the
//! protocol's `/v1/notifications` routes and `notify.*` WebSocket kinds (cargo feature
//! `notifications`, module [`Notifications`]).
//!
//! - **Creating** is server-side only: [`NotificationService::send`] (from a hook, a route, a
//!   module) with a [`NewNotification`] (kind, text, data, sender). It stores the notification,
//!   deletes the player's oldest beyond `max_per_user` (200) and pushes `notify.new` to every open
//!   connection of the player (through the hub's `Broadcaster`: every instance).
//! - **The player** (HTTP routes, all with a Bearer token, and the same over the WebSocket):
//!   `GET /v1/notifications` / `notify.list` (newest first, cursor pages, `unread_only`),
//!   `GET /v1/notifications/count` / `notify.count` (unread and total),
//!   `POST /v1/notifications/mark` / `notify.mark` (ids or all, read or unread),
//!   `DELETE /v1/notifications/{id}` / `notify.delete`.
//! - **Retention:** a background task deletes notifications older than `retention_days` (30).
//!   Deleting an account deletes its notifications; a deleted sender becomes absent.
//! - **Hooks** ([`events`]): [`BeforeNotify`](events::BeforeNotify) (change or refuse, e.g. a
//!   muted kind) and [`AfterNotify`](events::AfterNotify).

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod handlers;
mod migrations;
mod module;
mod openapi;
mod service;
mod store;

pub use config::NotificationsConfig;
pub use module::Notifications;
pub use service::{NewNotification, NotificationService};
