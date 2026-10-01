//! What games can hook into, and the revocation notifications.
//!
//! **Hooks** ([`crate::hooks`]): `before` hooks may change or refuse, `after` hooks observe.
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeRegister`] | before | an account is about to be created (email registration or a first Steam login); change the display name, or refuse (e.g. a reserved name) |
//! | [`AfterRegister`] | after | the account exists (committed) |
//! | [`BeforeLogin`] | before | credentials are valid, the session is about to be created; refuse to keep a player out (maintenance, a game ban) |
//! | [`AfterLogin`] | after | a session was created (login, registration, Steam) |
//! | [`BeforeAccountUpdate`] | before | `PATCH /v1/account`: change or refuse the new display name |
//! | [`AfterEmailVerified`] | after | an email address was confirmed |
//! | [`AfterPasswordChanged`] | after | a password was changed or reset |
//! | [`AfterSessionsRevoked`] | after | sessions were revoked in THIS process (logout, password change, ban, admin, refresh-token reuse) |
//!
//! ```
//! use net_backend_server::auth::events::BeforeRegister;
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeRegister, _, _>(|_ctx, event| async move {
//!     if event.display_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case("admin")) {
//!         return Ok(Decision::Reject(AppError::forbidden("this name is reserved")));
//!     }
//!     Ok(Decision::Continue(event))
//! });
//! # let _ = server;
//! ```
//!
//! **Revocations** ([`AuthService::subscribe_revocations`](super::AuthService::subscribe_revocations)):
//! a broadcast of every revocation, for whoever keeps connections open (the WebSocket hub closes
//! the affected sockets with [`Revocation::close_code`]: 4003 for a ban, else 4001).

use std::net::IpAddr;

use net_backend_protocol::auth::LinkedIdentity;
use net_backend_protocol::{CloseCode, UserId};

use super::steam::SteamIdentity;
use crate::hooks::Event;

/// How a session was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoginMethod {
    /// Email + password.
    Password,
    /// A Steam ticket.
    Steam,
    /// Created by a registration.
    Register,
}

impl LoginMethod {
    /// The name stored with the session and written to the audit log.
    pub fn as_str(self) -> &'static str {
        match self {
            LoginMethod::Password => "password",
            LoginMethod::Steam => "steam",
            LoginMethod::Register => "register",
        }
    }
}

/// An account is about to be created. Hooks may change `display_name` or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeRegister {
    /// The email address as entered (trimmed); `None` for a Steam account.
    pub email: Option<String>,
    /// The display name (hooks may change it; it is checked again afterwards).
    pub display_name: Option<String>,
    /// The linked identity for a provider account (Steam).
    pub identity: Option<LinkedIdentity>,
    /// The client address.
    pub ip: Option<IpAddr>,
}

impl Event for BeforeRegister {
    const NAME: &'static str = "auth.before_register";
}

/// An account was created.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterRegister {
    /// The new account.
    pub user_id: UserId,
    /// Its email address, if any.
    pub email: Option<String>,
    /// Its display name, if any.
    pub display_name: Option<String>,
    /// The linked identity for a provider account (Steam).
    pub identity: Option<LinkedIdentity>,
}

impl Event for AfterRegister {
    const NAME: &'static str = "auth.after_register";
}

/// Valid credentials; a session is about to be created. Refuse to keep the player out.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeLogin {
    /// The account.
    pub user_id: UserId,
    /// How.
    pub method: LoginMethod,
    /// What Steam said, for a Steam login (ban flags, the owner of a borrowed copy).
    pub steam: Option<SteamIdentity>,
    /// The client address.
    pub ip: Option<IpAddr>,
}

impl Event for BeforeLogin {
    const NAME: &'static str = "auth.before_login";
}

/// A session was created.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterLogin {
    /// The account.
    pub user_id: UserId,
    /// The new session.
    pub session_id: i64,
    /// How.
    pub method: LoginMethod,
    /// The client address.
    pub ip: Option<IpAddr>,
}

impl Event for AfterLogin {
    const NAME: &'static str = "auth.after_login";
}

/// `PATCH /v1/account`: hooks may change or refuse the new display name.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeAccountUpdate {
    /// The account.
    pub user_id: UserId,
    /// The new display name, if it changes.
    pub display_name: Option<String>,
}

impl Event for BeforeAccountUpdate {
    const NAME: &'static str = "auth.before_account_update";
}

/// An email address was confirmed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterEmailVerified {
    /// The account.
    pub user_id: UserId,
}

impl Event for AfterEmailVerified {
    const NAME: &'static str = "auth.after_email_verified";
}

/// A password was changed (knowing the old one) or reset (with a mail token).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterPasswordChanged {
    /// The account.
    pub user_id: UserId,
    /// Whether it was a reset.
    pub reset: bool,
}

impl Event for AfterPasswordChanged {
    const NAME: &'static str = "auth.after_password_changed";
}

/// Sessions were revoked.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterSessionsRevoked {
    /// What was revoked and why.
    pub revocation: Revocation,
}

impl Event for AfterSessionsRevoked {
    const NAME: &'static str = "auth.after_sessions_revoked";
}

/// Which sessions of a user a [`Revocation`] covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RevokedSessions {
    /// One session.
    One(i64),
    /// Every session.
    All,
    /// Every session but this one (a password change keeps the session that made it).
    AllExcept(i64),
}

/// Why sessions were revoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RevocationReason {
    /// The user logged out.
    Logout,
    /// The password was changed.
    PasswordChanged,
    /// The password was reset.
    PasswordReset,
    /// An admin banned the account.
    Banned,
    /// An admin (or the command line) revoked the sessions.
    Admin,
    /// A refresh token was used again after its grace window (possible theft).
    RefreshTokenReused,
}

impl RevocationReason {
    /// The name stored with the session (`revoke_reason`).
    pub fn as_str(self) -> &'static str {
        match self {
            RevocationReason::Logout => "logout",
            RevocationReason::PasswordChanged => "password_changed",
            RevocationReason::PasswordReset => "password_reset",
            RevocationReason::Banned => "banned",
            RevocationReason::Admin => "admin",
            RevocationReason::RefreshTokenReused => "refresh_reused",
        }
    }

    /// The reason stored as `name` ([`as_str`](Self::as_str)); unknown names count as `Admin`.
    pub fn from_name(name: &str) -> Self {
        match name {
            "logout" => RevocationReason::Logout,
            "password_changed" => RevocationReason::PasswordChanged,
            "password_reset" => RevocationReason::PasswordReset,
            "banned" => RevocationReason::Banned,
            "refresh_reused" => RevocationReason::RefreshTokenReused,
            _ => RevocationReason::Admin,
        }
    }
}

/// Sessions of a user were revoked: their tokens stop working at once; open connections of
/// those sessions should be closed with [`close_code`](Revocation::close_code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Revocation {
    /// The user.
    pub user_id: UserId,
    /// Which sessions.
    pub sessions: RevokedSessions,
    /// Why.
    pub reason: RevocationReason,
}

impl Revocation {
    /// A revocation.
    pub fn new(user_id: UserId, sessions: RevokedSessions, reason: RevocationReason) -> Self {
        Self { user_id, sessions, reason }
    }

    /// The WebSocket close code for the affected connections: 4003 (banned) or 4001.
    pub fn close_code(&self) -> CloseCode {
        match self.reason {
            RevocationReason::Banned => CloseCode::BANNED,
            _ => CloseCode::UNAUTHORIZED,
        }
    }

    /// Whether a connection of `user` authenticated with `session` is affected.
    pub fn applies_to(&self, user: UserId, session: Option<i64>) -> bool {
        if user != self.user_id {
            return false;
        }
        match (self.sessions, session) {
            (RevokedSessions::All, _) => true,
            (RevokedSessions::One(id), Some(s)) => id == s,
            (RevokedSessions::AllExcept(id), Some(s)) => id != s,
            (RevokedSessions::AllExcept(_), None) => true,
            (RevokedSessions::One(_), None) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocations() {
        let user = UserId(1);
        let ban = Revocation::new(user, RevokedSessions::All, RevocationReason::Banned);
        assert_eq!(ban.close_code(), CloseCode::BANNED);
        assert!(ban.applies_to(user, Some(5)) && ban.applies_to(user, None) && !ban.applies_to(UserId(2), Some(5)));
        let one = Revocation::new(user, RevokedSessions::One(5), RevocationReason::Logout);
        assert_eq!(one.close_code(), CloseCode::UNAUTHORIZED);
        assert!(one.applies_to(user, Some(5)) && !one.applies_to(user, Some(6)) && !one.applies_to(user, None));
        let others = Revocation::new(user, RevokedSessions::AllExcept(5), RevocationReason::PasswordChanged);
        assert!(!others.applies_to(user, Some(5)) && others.applies_to(user, Some(6)));
        assert_eq!(RevocationReason::RefreshTokenReused.as_str(), "refresh_reused");
        for reason in [
            RevocationReason::Logout,
            RevocationReason::PasswordChanged,
            RevocationReason::PasswordReset,
            RevocationReason::Banned,
            RevocationReason::Admin,
            RevocationReason::RefreshTokenReused,
        ] {
            assert_eq!(RevocationReason::from_name(reason.as_str()), reason);
        }
        assert_eq!(LoginMethod::Steam.as_str(), "steam");
    }
}
