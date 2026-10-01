//! [`AuthService`]: everything the auth routes, the authenticator, the commands and the
//! WebSocket hub do with accounts, sessions and tokens.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::admin::{AdminUser, AuditEntry, AuditQuery, BanInfo, BanRequest, UserListQuery};
use net_backend_protocol::auth::{
    provider, AccessToken, Account, AuthSession, ChangePasswordRequest, ForgotPasswordRequest, LinkedIdentity, LoginRequest, LogoutRequest, RefreshRequest,
    RefreshToken, RegisterRequest, ResetPasswordRequest, SteamLoginRequest, TokenPair, UpdateAccountRequest, VerifyEmailRequest, EMAIL_MAX_BYTES,
    PASSWORD_MAX_BYTES, REFRESH_REUSE_GRACE_SECS,
};
use net_backend_protocol::page::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT};
use net_backend_protocol::{codes, Cursor, Page, UnixMillis, UserId};
use serde_json::json;
use tokio::sync::{broadcast, OnceCell, Semaphore};
use unicode_normalization::UnicodeNormalization;

use super::audit::{self, AuditRecord};
use super::config::AuthConfig;
use super::events::{
    AfterEmailVerified, AfterLogin, AfterPasswordChanged, AfterRegister, AfterSessionsRevoked, BeforeAccountUpdate, BeforeLogin, BeforeRegister, LoginMethod,
    Revocation, RevocationReason, RevokedSessions,
};
use super::password::Hasher;
use super::steam::{SteamError, SteamIdentity, SteamVerifier};
use super::store::{self, UserRow};
use super::tokens::{self, ACCESS_PREFIX, EMAIL_PREFIX, REFRESH_PREFIX};
use super::{invalid_token, AuthContext};
use crate::db::{Db, DbTx};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::RequestId;
use crate::mail::{Mail, MailQueue};
use crate::rate_limit::{ip_key, KeyedBuckets, RateDecision, RecentSet};
use crate::state::AppState;

/// The name of the refresh-derivation key in `auth_secrets`.
const DERIVATION_KEY: &str = "refresh_derivation";
/// How often a session's `last_used_at` is written at most.
const TOUCH_EVERY_MS: i64 = 60_000;
/// Expired access tokens are kept this long (so they answer `token_expired`, not `unauthorized`).
const KEEP_EXPIRED_ACCESS_MS: i64 = 7 * 24 * 3600 * 1000;
/// Ended sessions are kept this long (the admin view and the audit trail can still name them).
const KEEP_ENDED_SESSIONS_MS: i64 = 30 * 24 * 3600 * 1000;
/// A client network that logged in successfully stays "known" for this long (lockout bypass).
const KNOWN_NETWORK_MS: u64 = 30 * 24 * 3600 * 1000;
/// A Steam ticket is refused when it comes again within this time (replay).
const STEAM_TICKET_MEMORY_MS: u64 = 10 * 60 * 1000;
/// The revocation poll re-reads this much before its last position (commit delays).
const POLL_OVERLAP_MS: i64 = 2_000;

/// Where a request came from (for sessions, audit entries and hooks).
#[derive(Clone, Debug, Default)]
pub(crate) struct ReqInfo {
    pub(crate) ip: Option<IpAddr>,
    pub(crate) request_id: Option<RequestId>,
    pub(crate) user_agent: Option<String>,
}

/// Who performs an administrative action, and through what (`admin.*` or `cli.*` audit actions).
#[derive(Clone, Debug)]
pub(crate) struct Actor {
    pub(crate) user: Option<UserId>,
    pub(crate) info: ReqInfo,
    pub(crate) cli: bool,
}

impl Actor {
    pub(crate) fn cli() -> Self {
        Self { user: None, info: ReqInfo::default(), cli: true }
    }

    pub(crate) fn server() -> Self {
        Self { user: None, info: ReqInfo::default(), cli: false }
    }

    fn record(&self, action: &str, target: UserId) -> AuditRecord {
        let prefix = if self.cli { "cli" } else { "admin" };
        AuditRecord::new(format!("{prefix}.{action}")).actor(self.user).target_user(target).ip(self.info.ip).request_id(self.info.request_id.as_ref())
    }
}

/// The canonical form of an address as entered: trimmed and in Unicode NFC. The server stores it,
/// shows it and mails to it (only after the protocol's strict `local@domain` check).
pub(crate) fn canonical_email(email: &str) -> String {
    email.trim().nfc().collect()
}

/// The comparison key of an email address: trimmed, Unicode NFC, lower-cased (the unique key of
/// an account: case-insensitive on every database, whatever its collation). Internationalised
/// domains are not converted to punycode.
pub fn normalize_email(email: &str) -> String {
    canonical_email(email).to_lowercase().nfc().collect()
}

/// A password in Unicode NFC (the same password typed on different keyboards hashes the same).
pub(crate) fn nfc_password(password: String) -> String {
    if password.is_ascii() {
        password
    } else {
        password.nfc().collect()
    }
}

fn reauth_required() -> AppError {
    AppError::new(codes::REAUTHENTICATION_REQUIRED, "log in again, then retry (this needs a recent login)")
}

fn invalid_credentials() -> AppError {
    AppError::new(codes::INVALID_CREDENTIALS, "the email address or the password is wrong")
}

fn invalid_one_time_token() -> AppError {
    AppError::new(codes::INVALID_TOKEN, "the token is unknown, used or expired")
}

fn refresh_invalid() -> AppError {
    AppError::new(codes::UNAUTHORIZED, "the refresh token is invalid, expired or revoked; log in again")
}

fn banned(user: &UserRow) -> AppError {
    let error = AppError::new(codes::BANNED, "this account is banned");
    match user.banned_until {
        Some(until) => error.with_details(json!({ "until": until })),
        None => error,
    }
}

fn secs_to_ms(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX / 1000).saturating_mul(1000)
}

fn clean_user_agent(value: Option<&str>) -> Option<String> {
    value.map(|ua| ua.chars().filter(|c| !c.is_control()).take(255).collect::<String>()).filter(|ua| !ua.is_empty())
}

pub(crate) fn user_agent(headers: &http::HeaderMap) -> Option<String> {
    clean_user_agent(headers.get(http::header::USER_AGENT).and_then(|v| v.to_str().ok()))
}

struct Inner {
    config: AuthConfig,
    hasher: Hasher,
    mail: MailQueue,
    steam: Option<Arc<dyn SteamVerifier>>,
    revocations: broadcast::Sender<Revocation>,
    role_changes: broadcast::Sender<UserId>,
    key: OnceCell<Vec<u8>>,
    /// Failed logins per (email address, client network).
    login_failures: KeyedBuckets<(String, Option<IpAddr>)>,
    /// Failed logins per email address, from everywhere.
    account_failures: KeyedBuckets<String>,
    /// (email address, client network) pairs that logged in successfully.
    known_networks: RecentSet<(String, Option<IpAddr>)>,
    /// SHA-256 of recently accepted Steam tickets.
    steam_tickets: RecentSet<String>,
    mails: KeyedBuckets<String>,
    background: Arc<Semaphore>,
    v6_prefix: u8,
}

/// The revocation poll's position (see [`AuthService::poll_revocations`]).
#[derive(Debug, Default)]
pub(crate) struct PollCursor {
    since: i64,
    seen: HashMap<i64, i64>,
    swept_until: i64,
}

impl PollCursor {
    pub(crate) fn starting_at(now: i64) -> Self {
        Self { since: now, seen: HashMap::new(), swept_until: 0 }
    }
}

/// The accounts service (a state value: `Ext<AuthService>`, `state.get::<AuthService>()`).
/// Cheap to clone.
#[derive(Clone)]
pub struct AuthService(Arc<Inner>);

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("config", &self.0.config)
            .field("hasher", &self.0.hasher)
            .field("steam", &self.0.steam.is_some())
            .finish_non_exhaustive()
    }
}

impl AuthService {
    pub(crate) fn new(config: AuthConfig, hasher: Hasher, mail: MailQueue, steam: Option<Arc<dyn SteamVerifier>>) -> Self {
        let login_failures = KeyedBuckets::new(config.login_failures, Duration::from_secs(config.login_lockout_secs), 100_000);
        let account_failures = KeyedBuckets::new(config.account_failures_per_hour, Duration::from_secs(3600), 100_000);
        let known_networks = RecentSet::new(Duration::from_millis(KNOWN_NETWORK_MS), 100_000);
        let steam_tickets = RecentSet::new(Duration::from_millis(STEAM_TICKET_MEMORY_MS), 100_000);
        let mails = KeyedBuckets::new(config.mails_per_account_per_hour, Duration::from_secs(3600), 100_000);
        let v6_prefix = config.rate_limit_ipv6_prefix;
        let (revocations, _) = broadcast::channel(1024);
        let (role_changes, _) = broadcast::channel(256);
        Self(Arc::new(Inner {
            config,
            hasher,
            mail,
            steam,
            revocations,
            role_changes,
            key: OnceCell::new(),
            login_failures,
            account_failures,
            known_networks,
            steam_tickets,
            mails,
            background: Arc::new(Semaphore::new(256)),
            v6_prefix,
        }))
    }

    /// The settings.
    pub fn config(&self) -> &AuthConfig {
        &self.0.config
    }

    /// Receive every revocation from now on (logout, password change, ban, admin, refresh-token
    /// reuse): this process's at once, and those of OTHER processes (the command line, another
    /// instance) within `revocation_poll_secs` (default 5 s) while the server runs, read back from
    /// the sessions table one session at a time. The same revocation may arrive twice (once at
    /// once, once from the poll): treat it idempotently. A receiver that falls more than 1024
    /// behind gets `Lagged` and should re-check its connections with
    /// [`authenticate_token`](Self::authenticate_token).
    pub fn subscribe_revocations(&self) -> broadcast::Receiver<Revocation> {
        self.0.revocations.subscribe()
    }

    /// Receive the user of every role grant / revocation made in THIS process (admin routes, server
    /// code); the WebSocket hub refreshes the roles of that user's open sockets. Changes made by
    /// other processes (the command line, another instance) reach open sockets through the hub's
    /// periodic refresh (`ws.roles_refresh_secs`).
    pub fn subscribe_role_changes(&self) -> broadcast::Receiver<UserId> {
        self.0.role_changes.subscribe()
    }

    /// The roles of several users (users without roles are absent).
    pub async fn roles_of_users(&self, state: &AppState, users: &[UserId]) -> Result<HashMap<UserId, Vec<String>>, AppError> {
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        let mut roles: HashMap<UserId, Vec<String>> = HashMap::new();
        for row in state.db().fetch_all::<store::RoleRow, _>(&store::roles_of(&ids)).await? {
            roles.entry(UserId(row.user_id)).or_default().push(row.role);
        }
        Ok(roles)
    }

    /// Every session revoked at or after `since` (by any process), oldest first, at most 1000:
    /// `(revoked at, revocation of that one session)`. The revocation poll is built on it; a hub may
    /// call it itself after a restart or a `Lagged`.
    pub async fn revocations_since(&self, state: &AppState, since: UnixMillis) -> Result<Vec<(UnixMillis, Revocation)>, AppError> {
        let rows = state.db().fetch_all::<store::RevokedRow, _>(&store::revoked_since(since.get(), 1000)).await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let reason = RevocationReason::from_name(r.revoke_reason.as_deref().unwrap_or(""));
                (UnixMillis(r.revoked_at.unwrap_or(since.get())), Revocation::new(UserId(r.user_id), RevokedSessions::One(r.id), reason))
            })
            .collect())
    }

    /// One revocation poll: broadcast sessions revoked since the cursor that were not seen yet, and
    /// forget the rotation nonces whose grace window ended. Returns how many were broadcast.
    pub(crate) async fn poll_revocations(&self, state: &AppState, cursor: &mut PollCursor, broadcast: bool) -> Result<usize, AppError> {
        let mut sent = 0;
        if broadcast {
            let from = cursor.since.saturating_sub(POLL_OVERLAP_MS);
            for (at, revocation) in self.revocations_since(state, UnixMillis(from)).await? {
                let RevokedSessions::One(id) = revocation.sessions else { continue };
                if cursor.seen.insert(id, at.get()).is_none() {
                    let _ = self.0.revocations.send(revocation);
                    sent += 1;
                }
                cursor.since = cursor.since.max(at.get());
            }
            let keep_from = cursor.since.saturating_sub(POLL_OVERLAP_MS);
            cursor.seen.retain(|_, at| *at >= keep_from);
        }
        let cutoff = Self::now(state).saturating_sub(secs_to_ms(REFRESH_REUSE_GRACE_SECS));
        if cutoff > cursor.swept_until {
            state.db().execute(&store::clear_nonces(cursor.swept_until, cutoff)).await?;
            cursor.swept_until = cutoff;
        }
        Ok(sent)
    }

    /// Build the dummy password hash now (so the first unknown-address login costs one hash).
    pub(crate) async fn warm_up(&self) -> Result<(), AppError> {
        self.0.hasher.warm_up().await
    }

    pub(crate) fn mail_queue(&self) -> &MailQueue {
        &self.0.mail
    }

    fn ctx(state: &AppState, info: &ReqInfo) -> HookCtx {
        HookCtx::new(state.clone(), info.request_id.clone())
    }

    fn now(state: &AppState) -> i64 {
        state.now().get()
    }

    // ---- tokens and sessions ----------------------------------------------------------------

    /// Check an access token (the WebSocket hub calls this for the handshake and for `auth`
    /// messages): `Ok` with the user, session and roles; 401 `token_expired` for an expired one;
    /// 401 `unauthorized` for an unknown, malformed or revoked one; 403 `banned`.
    pub async fn authenticate_token(&self, state: &AppState, token: &str) -> Result<AuthContext, AppError> {
        if !tokens::has_shape(token, ACCESS_PREFIX) {
            return Err(invalid_token());
        }
        let db = state.db();
        let Some(row) = db.fetch_optional::<store::AccessRow, _>(&store::access_by_hash(&tokens::hash(token))).await? else {
            return Err(invalid_token());
        };
        let now = Self::now(state);
        // The ban first: a ban also revokes every session, and the client must learn "banned"
        // (HTTP 403, WebSocket 4003), not "unauthorized".
        if row.banned_at.is_some() && row.banned_until.is_none_or(|until| until > now) {
            let error = AppError::new(codes::BANNED, "this account is banned");
            return Err(match row.banned_until {
                Some(until) => error.with_details(json!({ "until": until })),
                None => error,
            });
        }
        if row.session_revoked_at.is_some() {
            return Err(invalid_token());
        }
        if row.expires_at <= now {
            return Err(AppError::new(codes::TOKEN_EXPIRED, "the access token expired; refresh it"));
        }
        let roles = db.fetch_all::<store::RoleRow, _>(&store::roles_of(&[row.user_id])).await?.into_iter().map(|r| r.role).collect();
        if now - row.session_last_used_at > TOUCH_EVERY_MS {
            let db = db.clone();
            let session = row.session_id;
            tokio::spawn(async move {
                if let Err(error) = db.execute(&store::touch_session(session, now, None)).await {
                    tracing::warn!(error = %error.describe(), "could not record a session's last use");
                }
            });
        }
        Ok(AuthContext::new(UserId(row.user_id)).with_session(row.session_id).with_roles(roles).with_session_started_at(UnixMillis(row.session_created_at)))
    }

    /// A new session with a fresh token pair (inside the caller's transaction).
    async fn issue_session(&self, tx: &mut DbTx, user: i64, method: LoginMethod, info: &ReqInfo, now: i64) -> Result<(i64, TokenPair), AppError> {
        let config = &self.0.config;
        let access = tokens::random_token(ACCESS_PREFIX)?;
        let refresh = tokens::random_token(REFRESH_PREFIX)?;
        let access_expires = now.saturating_add(secs_to_ms(config.access_token_ttl_secs));
        let refresh_expires = now.saturating_add(secs_to_ms(config.refresh_token_ttl_secs));
        let session = tx
            .insert_id(&store::insert_session(user, method.as_str(), info.ip.map(|ip| ip.to_string()), info.user_agent.clone(), now, refresh_expires)?, "id")
            .await?;
        tx.execute(&store::insert_token(store::ACCESS, session, user, &tokens::hash(&access), access_expires, now)?).await?;
        tx.execute(&store::insert_token(store::REFRESH, session, user, &tokens::hash(&refresh), refresh_expires, now)?).await?;
        Ok((session, TokenPair::new(AccessToken::new(access), UnixMillis(access_expires), RefreshToken::new(refresh), UnixMillis(refresh_expires))))
    }

    /// The server's key for deriving rotated token pairs (generated once, kept in `auth_secrets`).
    async fn derivation_key(&self, db: &Db, now: i64) -> Result<&[u8], AppError> {
        let key = self
            .0
            .key
            .get_or_try_init(|| async {
                if let Some(row) = db.fetch_optional::<store::SecretRow, _>(&store::secret(DERIVATION_KEY)).await? {
                    return tokens::unhex(&row.value).ok_or_else(|| AppError::internal(std::io::Error::other("the stored derivation key is not hex")));
                }
                let fresh = tokens::new_key_hex()?;
                if let Err(error) = db.execute(&store::insert_secret(DERIVATION_KEY, &fresh, now)?).await {
                    // Another process may have created it at the same moment: read the winner.
                    if !error.is_unique_violation() {
                        return Err(AppError::from(error));
                    }
                }
                let row = db.fetch_one::<store::SecretRow, _>(&store::secret(DERIVATION_KEY)).await?;
                tokens::unhex(&row.value).ok_or_else(|| AppError::internal(std::io::Error::other("the stored derivation key is not hex")))
            })
            .await?;
        Ok(key.as_slice())
    }

    /// `POST /v1/auth/refresh`: rotate the refresh token (see the protocol's rules).
    pub(crate) async fn refresh(&self, state: &AppState, info: &ReqInfo, request: RefreshRequest) -> Result<TokenPair, AppError> {
        let token = request.refresh_token.expose();
        if !tokens::has_shape(token, REFRESH_PREFIX) {
            return Err(refresh_invalid());
        }
        let db = state.db();
        let now = Self::now(state);
        let Some(row) = db.fetch_optional::<store::RefreshRow, _>(&store::refresh_by_hash(&tokens::hash(token))).await? else {
            return Err(refresh_invalid());
        };
        // The ban first (a ban revokes every session; the client must learn "banned").
        let user = db.fetch_optional::<UserRow, _>(&store::user_by_id(row.user_id)).await?.ok_or_else(refresh_invalid)?;
        if user.is_banned(now) {
            return Err(banned(&user));
        }
        if row.session_revoked_at.is_some() || row.expires_at <= now {
            return Err(refresh_invalid());
        }
        let key = self.derivation_key(db, now).await?.to_vec();
        let grace_ms = secs_to_ms(REFRESH_REUSE_GRACE_SECS);
        if row.used_at.is_none() {
            // A fresh random nonce per rotation: the replacement pair is HMAC(key, nonce, old token).
            // The nonce lives only through the grace window, so the key plus an old token cannot
            // compute later tokens.
            let nonce = tokens::new_key_hex()?;
            let access = tokens::derive(&key, &nonce, token, "access", ACCESS_PREFIX)?;
            let refresh = tokens::derive(&key, &nonce, token, "refresh", REFRESH_PREFIX)?;
            let config = &self.0.config;
            let access_expires = now.saturating_add(secs_to_ms(config.access_token_ttl_secs));
            let refresh_expires = now.saturating_add(secs_to_ms(config.refresh_token_ttl_secs));
            let mut tx = db.begin().await?;
            if tx.execute(&store::mark_refresh_used(row.id, now, &nonce)).await? == 1 {
                tx.execute(&store::insert_token(store::ACCESS, row.session_id, row.user_id, &tokens::hash(&access), access_expires, now)?).await?;
                tx.execute(&store::insert_token(store::REFRESH, row.session_id, row.user_id, &tokens::hash(&refresh), refresh_expires, now)?).await?;
                tx.execute(&store::touch_session(row.session_id, now, Some(refresh_expires))).await?;
                tx.execute(&store::clear_session_nonces(row.session_id, now.saturating_sub(grace_ms))).await?;
                tx.commit().await?;
                return Ok(TokenPair::new(AccessToken::new(access), UnixMillis(access_expires), RefreshToken::new(refresh), UnixMillis(refresh_expires)));
            }
            // Another request used it a moment ago: answer like a reuse (normally the grace case).
            tx.rollback().await?;
        }
        let used = db.fetch_optional::<store::UsedAtRow, _>(&store::refresh_used_at(row.id)).await?;
        let used_at = used.as_ref().and_then(|r| r.used_at).unwrap_or(now);
        if now.saturating_sub(used_at) <= grace_ms {
            // Within the grace window: the same pair as the first use (derived again, never stored).
            let Some(nonce) = used.and_then(|r| r.rotation_nonce) else { return Err(refresh_invalid()) };
            let access = tokens::derive(&key, &nonce, token, "access", ACCESS_PREFIX)?;
            let refresh = tokens::derive(&key, &nonce, token, "refresh", REFRESH_PREFIX)?;
            let access_row = db.fetch_optional::<store::TokenRow, _>(&store::token_by_hash(store::ACCESS, &tokens::hash(&access))).await?;
            let refresh_row = db.fetch_optional::<store::TokenRow, _>(&store::token_by_hash(store::REFRESH, &tokens::hash(&refresh))).await?;
            return match (access_row, refresh_row) {
                (Some(a), Some(r)) if a.session_id == row.session_id && r.session_id == row.session_id => {
                    Ok(TokenPair::new(AccessToken::new(access), UnixMillis(a.expires_at), RefreshToken::new(refresh), UnixMillis(r.expires_at)))
                }
                // The derivation key changed (the secrets row was deleted): cannot repeat the pair.
                _ => Err(refresh_invalid()),
            };
        }
        // A reuse after the grace window: the token family is revoked (possible theft).
        let revocation = Revocation::new(UserId(row.user_id), RevokedSessions::One(row.session_id), RevocationReason::RefreshTokenReused);
        let mut tx = db.begin().await?;
        tx.execute(&store::clear_nonce(row.id)).await?;
        tx.execute(&store::revoke_sessions(row.user_id, Some(row.session_id), None, RevocationReason::RefreshTokenReused.as_str(), now)).await?;
        let record = AuditRecord::new("auth.refresh_reused")
            .target_user(UserId(row.user_id))
            .ip(info.ip)
            .request_id(info.request_id.as_ref())
            .data(json!({ "session": row.session_id }));
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        tracing::warn!(user = row.user_id, session = row.session_id, "a used refresh token was presented again; the session was revoked");
        self.notify(state, info, revocation).await;
        Err(AppError::new(codes::REFRESH_TOKEN_REUSED, "the refresh token was already used; the session was revoked, log in again"))
    }

    /// Tell subscribers and `after` hooks about a revocation.
    async fn notify(&self, state: &AppState, info: &ReqInfo, revocation: Revocation) {
        let _ = self.0.revocations.send(revocation);
        state.hooks().run_after(&Self::ctx(state, info), Arc::new(AfterSessionsRevoked { revocation })).await;
    }

    /// Revoke sessions of `user` and tell everyone (also the open connections). Returns how many.
    pub async fn revoke_sessions(&self, state: &AppState, revocation: Revocation) -> Result<u64, AppError> {
        self.revoke_with(state, &ReqInfo::default(), revocation, None).await
    }

    async fn revoke_with(&self, state: &AppState, info: &ReqInfo, revocation: Revocation, record: Option<AuditRecord>) -> Result<u64, AppError> {
        let now = Self::now(state);
        let (only, except) = match revocation.sessions {
            RevokedSessions::One(id) => (Some(id), None),
            RevokedSessions::AllExcept(id) => (None, Some(id)),
            _ => (None, None),
        };
        let mut tx = state.db().begin().await?;
        let count = tx.execute(&store::revoke_sessions(revocation.user_id.get(), only, except, revocation.reason.as_str(), now)).await?;
        if let Some(record) = record {
            audit::record_tx(&mut tx, UnixMillis(now), &record.data(json!({ "sessions": count }))).await?;
        }
        tx.commit().await?;
        self.notify(state, info, revocation).await;
        Ok(count)
    }

    // ---- accounts ---------------------------------------------------------------------------

    async fn accounts_of(db: &Db, users: Vec<UserRow>) -> Result<Vec<Account>, AppError> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = users.iter().map(|u| u.id).collect();
        let roles = db.fetch_all::<store::RoleRow, _>(&store::roles_of(&ids)).await?;
        let identities = db.fetch_all::<store::IdentityRow, _>(&store::identities_of(&ids)).await?;
        Ok(users
            .into_iter()
            .map(|u| {
                let mut account = Account::new(UserId(u.id), UnixMillis(u.created_at))
                    .with_roles(roles.iter().filter(|r| r.user_id == u.id).map(|r| r.role.clone()).collect())
                    .with_identities(
                        identities.iter().filter(|i| i.user_id == u.id).map(|i| LinkedIdentity::new(i.provider.clone(), i.subject.clone())).collect(),
                    );
                if let Some(email) = u.email {
                    account = account.with_email(email, u.email_verified_at.is_some());
                }
                if let Some(name) = u.display_name {
                    account = account.with_display_name(name);
                }
                account
            })
            .collect())
    }

    async fn user(db: &Db, user: UserId) -> Result<UserRow, AppError> {
        db.fetch_optional::<UserRow, _>(&store::user_by_id(user.get())).await?.ok_or_else(|| AppError::not_found("no such account"))
    }

    /// The account of `user` (`GET /v1/account`).
    pub async fn account(&self, state: &AppState, user: UserId) -> Result<Account, AppError> {
        let row = Self::user(state.db(), user).await?;
        Self::accounts_of(state.db(), vec![row]).await?.pop().ok_or_else(|| AppError::not_found("no such account"))
    }

    /// `POST /v1/auth/register`.
    pub(crate) async fn register(&self, state: &AppState, info: &ReqInfo, mut request: RegisterRequest) -> Result<AuthSession, AppError> {
        // NFC first: a decomposed address (as some keyboards type it) is the same address.
        request.email = canonical_email(&request.email);
        request.validate()?;
        if !self.0.config.allow_registration {
            return Err(AppError::forbidden("registration is closed on this server"));
        }
        let email = canonical_email(&request.email);
        let normalized = normalize_email(&email);
        let ctx = Self::ctx(state, info);
        let event = BeforeRegister { email: Some(email.clone()), display_name: request.display_name.clone(), identity: None, ip: info.ip };
        let event = state.hooks().run_before(&ctx, event).await?;
        let display_name = event.display_name;
        if let Some(name) = &display_name {
            UpdateAccountRequest::new().with_display_name(name.clone()).validate()?;
        }
        // Hash first, so a taken address still costs the hash (the answer itself, 409, is the
        // protocol's deliberate trade-off).
        let hash = self.0.hasher.hash(nfc_password(request.password.into_inner())).await?;
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        let user = match tx.insert_id(&store::insert_user(Some(&email), Some(&normalized), display_name.as_deref(), None, now)?, "id").await {
            Ok(id) => id,
            Err(error) if error.is_unique_violation() => {
                let _ = tx.rollback().await;
                return Err(AppError::new(codes::EMAIL_TAKEN, "this email address is already registered"));
            }
            Err(error) => return Err(error.into()),
        };
        tx.execute(&store::insert_credentials(user, &hash, now)?).await?;
        let (session, tokens) = self.issue_session(&mut tx, user, LoginMethod::Register, info, now).await?;
        let record = AuditRecord::new("auth.register").actor(Some(UserId(user))).target_user(UserId(user)).ip(info.ip).request_id(info.request_id.as_ref());
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        let user_id = UserId(user);
        state
            .hooks()
            .run_after(&ctx, Arc::new(AfterRegister { user_id, email: Some(email.clone()), display_name: display_name.clone(), identity: None }))
            .await;
        state.hooks().run_after(&ctx, Arc::new(AfterLogin { user_id, session_id: session, method: LoginMethod::Register, ip: info.ip })).await;
        if self.0.config.send_verification_on_register {
            if let Err(error) = self.send_verification(state, user, &email, &normalized).await {
                tracing::warn!(user, %error, "could not queue the verification mail");
            }
        }
        Ok(AuthSession::new(self.account(state, user_id).await?, tokens))
    }

    /// `POST /v1/auth/login`.
    pub(crate) async fn login(&self, state: &AppState, info: &ReqInfo, request: LoginRequest) -> Result<AuthSession, AppError> {
        let normalized = normalize_email(&request.email);
        let password = nfc_password(request.password.into_inner());
        if normalized.is_empty() || normalized.len() > EMAIL_MAX_BYTES || password.is_empty() || password.len() > PASSWORD_MAX_BYTES {
            return Err(invalid_credentials());
        }
        // The lockout policy (see docs): failures count per (address, client network) - a stranger
        // cannot lock the owner out from elsewhere - and per address from everywhere; above that
        // looser account-wide ceiling only networks that logged in to the account before may try.
        let limits = self.0.config.rate_limits;
        let pair = (normalized.clone(), info.ip.map(|ip| ip_key(ip, self.0.v6_prefix)));
        if limits {
            if let RateDecision::Deny { retry_after_ms } = self.0.login_failures.peek(&pair) {
                return Err(AppError::rate_limited(retry_after_ms));
            }
            if let RateDecision::Deny { retry_after_ms } = self.0.account_failures.peek(&normalized) {
                if !self.0.known_networks.contains(&pair) {
                    return Err(AppError::rate_limited(retry_after_ms));
                }
            }
        }
        // ONE query (account + hash) and one hash (a dummy without an account), whether or not the
        // address has an account: the same awaited work on both failure paths.
        let db = state.db();
        let row = db.fetch_optional::<store::LoginRow, _>(&store::login_by_email(&normalized)).await?;
        let (user, stored) = match row {
            Some(row) => (Some(row.user), row.password_hash),
            None => (None, None),
        };
        let matches = self.0.hasher.verify(password.clone(), stored.clone()).await?;
        let now = Self::now(state);
        let known = user.as_ref().map(|u| u.id);
        let (Some(user), true) = (user, matches) else {
            if limits {
                let _ = self.0.login_failures.check(pair.clone());
                let _ = self.0.account_failures.check(normalized.clone());
            }
            // Failures on existing accounts are audited, in the background (never awaited here:
            // the answer must not take longer for a known address). Unknown addresses are not
            // audited: no enumeration through the log, and no row per guess.
            if let Some(id) = known {
                let record = AuditRecord::new("auth.login_failed").target_user(UserId(id)).ip(info.ip).request_id(info.request_id.as_ref());
                let db = db.clone();
                tokio::spawn(async move { audit::record_logged(&db, UnixMillis(now), &record).await });
            }
            return Err(invalid_credentials());
        };
        self.0.login_failures.reset(&pair);
        self.0.known_networks.insert(pair);
        if user.is_banned(now) {
            return Err(banned(&user));
        }
        if self.0.config.login_requires_verified_email && user.email_verified_at.is_none() {
            return Err(AppError::new(codes::EMAIL_NOT_VERIFIED, "confirm your email address first"));
        }
        let ctx = Self::ctx(state, info);
        state.hooks().run_before(&ctx, BeforeLogin { user_id: UserId(user.id), method: LoginMethod::Password, steam: None, ip: info.ip }).await?;
        if stored.as_deref().is_some_and(|s| self.0.hasher.needs_rehash(s)) {
            match self.0.hasher.hash(password).await {
                Ok(hash) => {
                    if let Err(error) = db.execute(&store::update_credentials(user.id, &hash, now)).await {
                        tracing::warn!(error = %error.describe(), "could not store a rehashed password");
                    }
                }
                Err(error) => tracing::warn!(%error, "could not rehash a password"),
            }
        }
        self.start_session(state, info, user.id, LoginMethod::Password, "auth.login", None).await
    }

    async fn start_session(
        &self,
        state: &AppState,
        info: &ReqInfo,
        user: i64,
        method: LoginMethod,
        action: &str,
        data: Option<serde_json::Value>,
    ) -> Result<AuthSession, AppError> {
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        let (session, tokens) = self.issue_session(&mut tx, user, method, info, now).await?;
        let mut record = AuditRecord::new(action).actor(Some(UserId(user))).target_user(UserId(user)).ip(info.ip).request_id(info.request_id.as_ref());
        if let Some(data) = data {
            record = record.data(data);
        }
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        state.hooks().run_after(&Self::ctx(state, info), Arc::new(AfterLogin { user_id: UserId(user), session_id: session, method, ip: info.ip })).await;
        Ok(AuthSession::new(self.account(state, UserId(user)).await?, tokens))
    }

    /// `POST /v1/auth/steam`: check the ticket, then log in, link (with a Bearer token) or create.
    pub(crate) async fn steam_login(
        &self,
        state: &AppState,
        info: &ReqInfo,
        current: Option<AuthContext>,
        request: SteamLoginRequest,
    ) -> Result<AuthSession, AppError> {
        request.validate()?;
        let Some(verifier) = &self.0.steam else {
            return Err(AppError::not_found("Steam login is not enabled on this server"));
        };
        let config = &self.0.config;
        // The CONFIGURED identity goes to Steam (never the client's): a ticket the player issued to
        // another service (another identity) cannot be replayed here. Setup requires it.
        let Some(identity) = config.steam_identity.as_deref() else {
            return Err(AppError::not_found("Steam login is not enabled on this server"));
        };
        if identity != request.identity {
            return Err(AppError::new(codes::STEAM_AUTH_FAILED, "the ticket was made for another identity"));
        }
        let ticket_key = tokens::hash(&request.ticket_hex.expose().to_ascii_lowercase());
        if self.0.steam_tickets.contains(&ticket_key) {
            return Err(AppError::new(codes::STEAM_AUTH_FAILED, "this ticket was used already; get a new one"));
        }
        let steam = match verifier.verify(request.ticket_hex.expose(), identity).await {
            Ok(identity) => identity,
            Err(SteamError::Rejected(reason)) => {
                tracing::info!(%reason, "Steam refused a ticket");
                return Err(AppError::new(codes::STEAM_AUTH_FAILED, "Steam refused the ticket"));
            }
            Err(error) => {
                tracing::warn!(%error, "Steam could not check a ticket");
                return Err(AppError::new(codes::STEAM_AUTH_FAILED, "Steam could not be asked; retry later"));
            }
        };
        if (steam.publisher_banned && config.steam_reject_publisher_banned) || (steam.vac_banned && config.steam_reject_vac_banned) {
            return Err(AppError::new(codes::BANNED, "this Steam account is banned"));
        }
        if steam.is_borrowed() && !config.steam_allow_family_sharing {
            return Err(AppError::forbidden("a borrowed copy (Family Sharing) cannot log in"));
        }
        self.0.steam_tickets.insert(ticket_key);
        let subject = steam.steam_id.to_string();
        let db = state.db();
        let now = Self::now(state);
        let existing = db.fetch_optional::<UserRow, _>(&store::user_by_identity(provider::STEAM, &subject)).await?;
        let user = match (existing, current) {
            (Some(user), Some(current)) if user.id != current.user_id.get() => {
                return Err(AppError::conflict("this Steam account is linked to another account"));
            }
            (Some(user), _) => {
                self.check_steam_login(state, info, &user, &steam, now).await?;
                user
            }
            (None, Some(current)) => self.link_steam(state, info, &current, &steam, &subject, now).await?,
            (None, None) => {
                let user = self.create_steam_account(state, info, &steam, &subject).await?;
                self.check_steam_login(state, info, &user, &steam, now).await?;
                user
            }
        };
        let data = json!({ "steam_id": subject, "borrowed": steam.is_borrowed() });
        self.start_session(state, info, user.id, LoginMethod::Steam, "auth.steam_login", Some(data)).await
    }

    /// The ban and the `BeforeLogin` hook for a Steam login.
    async fn check_steam_login(&self, state: &AppState, info: &ReqInfo, user: &UserRow, steam: &SteamIdentity, now: i64) -> Result<(), AppError> {
        if user.is_banned(now) {
            return Err(banned(user));
        }
        let event = BeforeLogin { user_id: UserId(user.id), method: LoginMethod::Steam, steam: Some(steam.clone()), ip: info.ip };
        state.hooks().run_before(&Self::ctx(state, info), event).await.map(|_| ())
    }

    /// Link a Steam account to the logged-in account: only with a recent login, only one Steam
    /// account per account, after the ban check and the `BeforeLogin` hook; audited; the owner is
    /// mailed.
    async fn link_steam(
        &self,
        state: &AppState,
        info: &ReqInfo,
        current: &AuthContext,
        steam: &SteamIdentity,
        subject: &str,
        now: i64,
    ) -> Result<UserRow, AppError> {
        if !current.is_recent_login(UnixMillis(now), secs_to_ms(self.0.config.link_reauth_secs)) {
            return Err(reauth_required());
        }
        let db = state.db();
        let user = Self::user(db, current.user_id).await?;
        if db.fetch_one::<store::CountRow, _>(&store::count_identities(user.id, Some(provider::STEAM))).await?.n > 0 {
            return Err(AppError::conflict("this account already has a Steam account linked; unlink it first"));
        }
        self.check_steam_login(state, info, &user, steam, now).await?;
        match db.execute(&store::insert_identity(user.id, provider::STEAM, subject, now)?).await {
            Ok(_) => {}
            Err(error) if error.is_unique_violation() => return Err(AppError::conflict("this Steam account is linked to another account")),
            Err(error) => return Err(error.into()),
        }
        let record = AuditRecord::new("auth.steam_linked")
            .actor(Some(current.user_id))
            .target_user(current.user_id)
            .ip(info.ip)
            .request_id(info.request_id.as_ref())
            .data(json!({ "steam_id": subject }));
        audit::record_logged(db, UnixMillis(now), &record).await;
        if let (true, Some(email)) = (self.0.config.notify_on_link, &user.email) {
            let config = &self.0.config;
            let text = format!(
                "A Steam account was linked to your {app} account. You can now log in with it.\n\nIf this was not you, change your password (or reset it) and unlink Steam in the game.\n",
                app = config.app_name
            );
            self.0.mail.enqueue(Mail::new(email.clone(), format!("{}: Steam account linked", config.app_name), text));
        }
        Ok(user)
    }

    /// `DELETE /v1/account/identities/{provider}` (the caller; needs a recent login, refuses to
    /// remove the last way to log in) and the admin route (`admin` set).
    pub(crate) async fn unlink_identity(&self, state: &AppState, info: &ReqInfo, user: UserId, provider: &str, admin: Option<&Actor>) -> Result<(), AppError> {
        let db = state.db();
        let row = Self::user(db, user).await?;
        let linked = db.fetch_one::<store::CountRow, _>(&store::count_identities(row.id, Some(provider))).await?.n;
        if linked == 0 {
            return Err(AppError::not_found("no such linked login"));
        }
        if admin.is_none() {
            let all = db.fetch_one::<store::CountRow, _>(&store::count_identities(row.id, None)).await?.n;
            let has_password = db.fetch_optional::<store::CredentialRow, _>(&store::credentials(row.id)).await?.is_some();
            if !has_password && all <= linked {
                return Err(AppError::conflict("this is the account's only way to log in; set a password first"));
            }
        }
        let now = Self::now(state);
        let mut tx = db.begin().await?;
        tx.execute(&store::delete_identities(row.id, Some(provider))).await?;
        let record = match admin {
            Some(actor) => actor.record("identity_unlink", user),
            None => AuditRecord::new("auth.identity_unlinked").actor(Some(user)).target_user(user).ip(info.ip).request_id(info.request_id.as_ref()),
        };
        audit::record_tx(&mut tx, UnixMillis(now), &record.data(json!({ "provider": provider }))).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The caller's unlink: a recent login is required.
    pub(crate) async fn unlink_own_identity(&self, state: &AppState, info: &ReqInfo, current: &AuthContext, provider: &str) -> Result<(), AppError> {
        if !current.is_recent_login(state.now(), secs_to_ms(self.0.config.link_reauth_secs)) {
            return Err(reauth_required());
        }
        self.unlink_identity(state, info, current.user_id, provider, None).await
    }

    async fn create_steam_account(&self, state: &AppState, info: &ReqInfo, steam: &SteamIdentity, subject: &str) -> Result<UserRow, AppError> {
        let ctx = Self::ctx(state, info);
        let identity = LinkedIdentity::new(provider::STEAM, subject);
        let event = BeforeRegister { email: None, display_name: None, identity: Some(identity.clone()), ip: info.ip };
        let event = state.hooks().run_before(&ctx, event).await?;
        if let Some(name) = &event.display_name {
            UpdateAccountRequest::new().with_display_name(name.clone()).validate()?;
        }
        let db = state.db();
        let now = Self::now(state);
        let mut tx = db.begin().await?;
        let user = tx.insert_id(&store::insert_user(None, None, event.display_name.as_deref(), None, now)?, "id").await?;
        match tx.execute(&store::insert_identity(user, provider::STEAM, subject, now)?).await {
            Ok(_) => {}
            Err(error) if error.is_unique_violation() => {
                // A parallel first login created it a moment ago: use that account.
                let _ = tx.rollback().await;
                return db
                    .fetch_optional::<UserRow, _>(&store::user_by_identity(provider::STEAM, subject))
                    .await?
                    .ok_or_else(|| AppError::conflict("retry the login"));
            }
            Err(error) => return Err(error.into()),
        }
        let record = AuditRecord::new("auth.register")
            .actor(Some(UserId(user)))
            .target_user(UserId(user))
            .ip(info.ip)
            .request_id(info.request_id.as_ref())
            .data(json!({ "provider": provider::STEAM, "steam_id": steam.steam_id.to_string() }));
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        state
            .hooks()
            .run_after(&ctx, Arc::new(AfterRegister { user_id: UserId(user), email: None, display_name: event.display_name, identity: Some(identity) }))
            .await;
        Self::user(db, UserId(user)).await
    }

    /// `POST /v1/auth/logout`: with the access token, or with the session's refresh token.
    pub(crate) async fn logout(&self, state: &AppState, info: &ReqInfo, current: Option<AuthContext>, request: LogoutRequest) -> Result<(), AppError> {
        let (user, session) = match current.as_ref().and_then(|c| c.session_id.map(|s| (c.user_id, s))) {
            Some(found) => found,
            None => {
                let token =
                    request.refresh_token.as_ref().map(|t| t.expose()).filter(|t| tokens::has_shape(t, REFRESH_PREFIX)).ok_or_else(AppError::unauthorized)?;
                let row = state.db().fetch_optional::<store::RefreshRow, _>(&store::refresh_by_hash(&tokens::hash(token))).await?;
                match row {
                    Some(row) if row.session_revoked_at.is_none() => {
                        // A used or expired token still ends its own session, but "everywhere"
                        // needs a current one (an old token from a backup must not log the owner
                        // out of every device).
                        let stale = row.used_at.is_some() || row.expires_at <= Self::now(state);
                        if stale && request.everywhere {
                            return Err(refresh_invalid());
                        }
                        (UserId(row.user_id), row.session_id)
                    }
                    _ => return Err(refresh_invalid()),
                }
            }
        };
        let sessions = if request.everywhere { RevokedSessions::All } else { RevokedSessions::One(session) };
        let record = AuditRecord::new("auth.logout").actor(Some(user)).target_user(user).ip(info.ip).request_id(info.request_id.as_ref());
        let record = record.data(json!({ "everywhere": request.everywhere }));
        self.revoke_with(state, info, Revocation::new(user, sessions, RevocationReason::Logout), Some(record)).await.map(|_| ())
    }

    /// `PATCH /v1/account`.
    pub(crate) async fn update_account(
        &self,
        state: &AppState,
        info: &ReqInfo,
        current: &AuthContext,
        request: UpdateAccountRequest,
    ) -> Result<Account, AppError> {
        request.validate()?;
        let ctx = Self::ctx(state, info);
        let event = state.hooks().run_before(&ctx, BeforeAccountUpdate { user_id: current.user_id, display_name: request.display_name.clone() }).await?;
        if let Some(name) = &event.display_name {
            UpdateAccountRequest::new().with_display_name(name.clone()).validate()?;
            state.db().execute(&store::update_display_name(current.user_id.get(), Some(name), Self::now(state))).await?;
        }
        self.account(state, current.user_id).await
    }

    /// `POST /v1/account/password`: other sessions are revoked.
    pub(crate) async fn change_password(
        &self,
        state: &AppState,
        info: &ReqInfo,
        current: &AuthContext,
        request: ChangePasswordRequest,
    ) -> Result<(), AppError> {
        request.validate()?;
        let db = state.db();
        let key = (format!("user:{}", current.user_id.get()), None);
        if self.0.config.rate_limits {
            if let crate::rate_limit::RateDecision::Deny { retry_after_ms } = self.0.login_failures.peek(&key) {
                return Err(AppError::rate_limited(retry_after_ms));
            }
        }
        let stored = db.fetch_optional::<store::CredentialRow, _>(&store::credentials(current.user_id.get())).await?.map(|c| c.password_hash);
        let current_password = nfc_password(request.current_password.into_inner());
        if current_password.len() > PASSWORD_MAX_BYTES || !self.0.hasher.verify(current_password, stored).await? {
            if self.0.config.rate_limits {
                let _ = self.0.login_failures.check(key);
            }
            return Err(AppError::new(codes::INVALID_CREDENTIALS, "the current password is wrong"));
        }
        let hash = self.0.hasher.hash(nfc_password(request.new_password.into_inner())).await?;
        let now = Self::now(state);
        db.execute(&store::update_credentials(current.user_id.get(), &hash, now)).await?;
        // A pending reset mail must not undo the change.
        db.execute(&store::delete_unused_email_tokens(current.user_id.get(), store::PURPOSE_RESET)).await?;
        let sessions = match current.session_id {
            Some(session) => RevokedSessions::AllExcept(session),
            None => RevokedSessions::All,
        };
        let record = AuditRecord::new("auth.password_changed")
            .actor(Some(current.user_id))
            .target_user(current.user_id)
            .ip(info.ip)
            .request_id(info.request_id.as_ref());
        self.revoke_with(state, info, Revocation::new(current.user_id, sessions, RevocationReason::PasswordChanged), Some(record)).await?;
        state.hooks().run_after(&Self::ctx(state, info), Arc::new(AfterPasswordChanged { user_id: current.user_id, reset: false })).await;
        Ok(())
    }

    // ---- mail flows -------------------------------------------------------------------------

    fn link(template: Option<&str>, token: &str) -> String {
        match template {
            Some(template) => template.replace("{token}", token),
            None => token.to_string(),
        }
    }

    /// Whether another mail to this account is allowed now (counts it).
    fn mail_allowed(&self, key: &str) -> bool {
        !self.0.config.rate_limits || self.0.mails.check(key.to_string()).is_allow()
    }

    async fn new_email_token(&self, state: &AppState, user: i64, purpose: &str, normalized: &str, ttl_secs: u64) -> Result<String, AppError> {
        let token = tokens::random_token(EMAIL_PREFIX)?;
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        tx.execute(&store::delete_unused_email_tokens(user, purpose)).await?;
        tx.execute(&store::insert_email_token(user, purpose, normalized, &tokens::hash(&token), now.saturating_add(secs_to_ms(ttl_secs)), now)?).await?;
        tx.commit().await?;
        Ok(token)
    }

    async fn send_verification(&self, state: &AppState, user: i64, email: &str, normalized: &str) -> Result<(), AppError> {
        let config = &self.0.config;
        let token = self.new_email_token(state, user, store::PURPOSE_VERIFY, normalized, config.verify_token_ttl_secs).await?;
        let hours = config.verify_token_ttl_secs / 3600;
        let text = format!(
            "Please confirm your email address for {app}:\n\n{link}\n\nThis is valid for {hours} hours and works once. If you did not create an account, ignore this mail.\n",
            app = config.app_name,
            link = Self::link(config.verify_url.as_deref(), &token),
        );
        self.0.mail.enqueue(Mail::new(email, format!("{}: confirm your email address", config.app_name), text));
        Ok(())
    }

    /// `POST /v1/auth/email/resend`.
    pub(crate) async fn resend_verification(&self, state: &AppState, current: &AuthContext) -> Result<(), AppError> {
        let user = Self::user(state.db(), current.user_id).await?;
        let (Some(email), Some(normalized), None) = (user.email, user.email_normalized, user.email_verified_at) else {
            return Ok(());
        };
        if !self.mail_allowed(&normalized) {
            return Err(AppError::rate_limited(60_000));
        }
        self.send_verification(state, user.id, &email, &normalized).await
    }

    /// `POST /v1/auth/email/verify`.
    pub(crate) async fn verify_email(&self, state: &AppState, info: &ReqInfo, request: VerifyEmailRequest) -> Result<(), AppError> {
        let token = request.token.expose();
        if !tokens::has_shape(token, EMAIL_PREFIX) {
            return Err(invalid_one_time_token());
        }
        let db = state.db();
        let now = Self::now(state);
        let row = db.fetch_optional::<store::EmailTokenRow, _>(&store::email_token(store::PURPOSE_VERIFY, &tokens::hash(token))).await?;
        let Some(row) = row.filter(|r| r.used_at.is_none() && r.expires_at > now) else {
            return Err(invalid_one_time_token());
        };
        let user = Self::user(db, UserId(row.user_id)).await.map_err(|_| invalid_one_time_token())?;
        if user.email_normalized.as_deref() != Some(row.email_normalized.as_str()) {
            return Err(invalid_one_time_token());
        }
        let mut tx = db.begin().await?;
        if tx.execute(&store::use_email_token(row.id, now)).await? != 1 {
            return Err(invalid_one_time_token());
        }
        tx.execute(&store::set_email_verified(user.id, now)).await?;
        let record =
            AuditRecord::new("auth.email_verified").actor(Some(UserId(user.id))).target_user(UserId(user.id)).ip(info.ip).request_id(info.request_id.as_ref());
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        state.hooks().run_after(&Self::ctx(state, info), Arc::new(AfterEmailVerified { user_id: UserId(user.id) })).await;
        Ok(())
    }

    /// `POST /v1/auth/password/forgot`: always the same answer, at once; the lookup and the mail
    /// happen in the background (no timing difference between known and unknown addresses).
    pub(crate) fn forgot_password(&self, state: &AppState, info: &ReqInfo, request: ForgotPasswordRequest) {
        let normalized = normalize_email(&request.email);
        if normalized.is_empty() || normalized.len() > EMAIL_MAX_BYTES || normalized.chars().any(char::is_control) {
            return;
        }
        let Ok(permit) = self.0.background.clone().try_acquire_owned() else {
            tracing::warn!("too many password-reset requests are being handled; one was dropped");
            return;
        };
        let service = self.clone();
        let state = state.clone();
        let info = info.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = service.send_reset(&state, &info, &normalized).await {
                tracing::warn!(%error, "could not handle a password-reset request");
            }
        });
    }

    async fn send_reset(&self, state: &AppState, info: &ReqInfo, normalized: &str) -> Result<(), AppError> {
        if !self.mail_allowed(normalized) {
            return Ok(());
        }
        let Some(user) = state.db().fetch_optional::<UserRow, _>(&store::user_by_email(normalized)).await? else {
            return Ok(());
        };
        let Some(email) = user.email.clone() else { return Ok(()) };
        let config = &self.0.config;
        let token = self.new_email_token(state, user.id, store::PURPOSE_RESET, normalized, config.reset_token_ttl_secs).await?;
        let minutes = config.reset_token_ttl_secs / 60;
        let text = format!(
            "Someone asked to reset the password of your {app} account. To choose a new password:\n\n{link}\n\nThis is valid for {minutes} minutes and works once. If it was not you, ignore this mail: your password stays as it is.\n",
            app = config.app_name,
            link = Self::link(config.reset_url.as_deref(), &token),
        );
        self.0.mail.enqueue(Mail::new(email, format!("{}: reset your password", config.app_name), text));
        let record = AuditRecord::new("auth.password_reset_requested").target_user(UserId(user.id)).ip(info.ip).request_id(info.request_id.as_ref());
        audit::record_logged(state.db(), state.now(), &record).await;
        Ok(())
    }

    /// `POST /v1/auth/password/reset`: every session of the account is revoked.
    pub(crate) async fn reset_password(&self, state: &AppState, info: &ReqInfo, request: ResetPasswordRequest) -> Result<(), AppError> {
        request.validate()?;
        let token = request.token.expose();
        if !tokens::has_shape(token, EMAIL_PREFIX) {
            return Err(invalid_one_time_token());
        }
        let db = state.db();
        let now = Self::now(state);
        let row = db.fetch_optional::<store::EmailTokenRow, _>(&store::email_token(store::PURPOSE_RESET, &tokens::hash(token))).await?;
        let Some(row) = row.filter(|r| r.used_at.is_none() && r.expires_at > now) else {
            return Err(invalid_one_time_token());
        };
        let user = Self::user(db, UserId(row.user_id)).await.map_err(|_| invalid_one_time_token())?;
        let hash = self.0.hasher.hash(nfc_password(request.new_password.into_inner())).await?;
        let now = Self::now(state);
        let mut tx = db.begin().await?;
        if tx.execute(&store::use_email_token(row.id, now)).await? != 1 {
            return Err(invalid_one_time_token());
        }
        if tx.execute(&store::update_credentials(user.id, &hash, now)).await? == 0 {
            tx.execute(&store::insert_credentials(user.id, &hash, now)?).await?;
        }
        // The mail reached the inbox: that proves the address (if it is still the account's).
        if user.email_normalized.as_deref() == Some(row.email_normalized.as_str()) {
            tx.execute(&store::set_email_verified(user.id, now)).await?;
        }
        tx.execute(&store::delete_unused_email_tokens(user.id, store::PURPOSE_RESET)).await?;
        tx.execute(&store::revoke_sessions(user.id, None, None, RevocationReason::PasswordReset.as_str(), now)).await?;
        // A reset often follows a compromise: a login provider linked by an intruder goes too.
        let unlinked = if self.0.config.unlink_identities_on_reset { tx.execute(&store::delete_identities(user.id, None)).await? } else { 0 };
        let record = AuditRecord::new("auth.password_reset")
            .target_user(UserId(user.id))
            .ip(info.ip)
            .request_id(info.request_id.as_ref())
            .data(json!({ "identities_unlinked": unlinked }));
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        // The mailbox owner proved themselves: the account-wide failure ceiling starts over.
        self.0.account_failures.reset(&row.email_normalized);
        self.notify(state, info, Revocation::new(UserId(user.id), RevokedSessions::All, RevocationReason::PasswordReset)).await;
        state.hooks().run_after(&Self::ctx(state, info), Arc::new(AfterPasswordChanged { user_id: UserId(user.id), reset: true })).await;
        Ok(())
    }

    // ---- administration ---------------------------------------------------------------------

    fn page_limit(limit: Option<u32>) -> u64 {
        u64::from(limit.map_or(DEFAULT_PAGE_LIMIT, |l| l.clamp(1, MAX_PAGE_LIMIT)))
    }

    fn cursor_id(cursor: Option<&Cursor>) -> Result<Option<i64>, AppError> {
        match cursor {
            None => Ok(None),
            Some(cursor) => cursor.as_str().parse::<i64>().map(Some).map_err(|_| AppError::bad_request("the cursor is not valid")),
        }
    }

    async fn admin_users(state: &AppState, rows: Vec<UserRow>) -> Result<Vec<AdminUser>, AppError> {
        let db = state.db();
        let now = Self::now(state);
        let ids: Vec<i64> = rows.iter().map(|u| u.id).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let active = db.fetch_all::<store::SessionStatRow, _>(&store::active_sessions_of(&ids, now)).await?;
        let seen = db.fetch_all::<store::LastSeenRow, _>(&store::last_seen_of(&ids)).await?;
        let bans: Vec<Option<BanInfo>> = rows
            .iter()
            .map(|u| {
                u.banned_at.map(|at| {
                    let mut ban = BanInfo::new(UnixMillis(at));
                    if let Some(until) = u.banned_until {
                        ban = ban.with_until(UnixMillis(until));
                    }
                    if let Some(reason) = &u.ban_reason {
                        ban = ban.with_reason(reason.clone());
                    }
                    ban
                })
            })
            .collect();
        let accounts = Self::accounts_of(db, rows).await?;
        Ok(accounts
            .into_iter()
            .zip(bans)
            .map(|(account, ban)| {
                let id = account.id.get();
                let mut user =
                    AdminUser::new(account).with_active_sessions(active.iter().find(|s| s.user_id == id).map_or(0, |s| u32::try_from(s.n).unwrap_or(u32::MAX)));
                if let Some(at) = seen.iter().find(|s| s.user_id == id).and_then(|s| s.last_seen) {
                    user = user.with_last_seen_at(UnixMillis(at));
                }
                if let Some(ban) = ban {
                    user = user.with_ban(ban);
                }
                user
            })
            .collect())
    }

    /// `GET /v1/admin/users`.
    pub async fn list_users(&self, state: &AppState, query: &UserListQuery) -> Result<Page<AdminUser>, AppError> {
        query.validate()?;
        let limit = Self::page_limit(query.limit);
        let before = Self::cursor_id(query.cursor.as_ref())?;
        let mut rows = state.db().fetch_all::<UserRow, _>(&store::list_users(query.q.as_deref(), before, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|u| Cursor::new(u.id.to_string())) } else { None };
        Ok(Page::new(Self::admin_users(state, rows).await?, next))
    }

    /// `GET /v1/admin/users/{user}`.
    pub async fn admin_user(&self, state: &AppState, user: UserId) -> Result<AdminUser, AppError> {
        let row = Self::user(state.db(), user).await?;
        Self::admin_users(state, vec![row]).await?.pop().ok_or_else(|| AppError::not_found("no such account"))
    }

    /// Ban an account: its sessions are revoked (connections close with 4003).
    pub(crate) async fn ban(&self, state: &AppState, actor: &Actor, user: UserId, request: BanRequest) -> Result<(), AppError> {
        request.validate()?;
        let now = Self::now(state);
        if request.until.is_some_and(|until| until.get() <= now) {
            return Err(AppError::validation({
                let mut details = net_backend_protocol::ValidationDetails::new();
                details.add("until", "must lie in the future");
                details
            }));
        }
        if actor.user == Some(user) {
            return Err(AppError::conflict("you cannot ban yourself"));
        }
        Self::user(state.db(), user).await?;
        // Admins are not banned over HTTP or from server code: remove the role first (the command
        // line, which the operator controls, may).
        if !actor.cli && self.has_role(state, user, net_backend_protocol::admin::ADMIN_ROLE).await? {
            return Err(AppError::conflict("this account is an admin; remove the admin role first"));
        }
        let mut tx = state.db().begin().await?;
        tx.execute(&store::set_ban(user.get(), Some((now, request.until.map(|u| u.get()), request.reason.clone())), now)).await?;
        let revoked = tx.execute(&store::revoke_sessions(user.get(), None, None, RevocationReason::Banned.as_str(), now)).await?;
        let record = actor.record("ban", user).data(json!({ "until": request.until.map(|u| u.get()), "reason": request.reason, "sessions": revoked }));
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        self.notify(state, &actor.info, Revocation::new(user, RevokedSessions::All, RevocationReason::Banned)).await;
        Ok(())
    }

    /// Lift a ban.
    pub(crate) async fn unban(&self, state: &AppState, actor: &Actor, user: UserId) -> Result<(), AppError> {
        Self::user(state.db(), user).await?;
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        tx.execute(&store::set_ban(user.get(), None, now)).await?;
        audit::record_tx(&mut tx, UnixMillis(now), &actor.record("unban", user)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revoke every session of an account.
    pub(crate) async fn admin_revoke(&self, state: &AppState, actor: &Actor, user: UserId) -> Result<u64, AppError> {
        Self::user(state.db(), user).await?;
        let record = actor.record("revoke_sessions", user);
        self.revoke_with(state, &actor.info, Revocation::new(user, RevokedSessions::All, RevocationReason::Admin), Some(record)).await
    }

    /// Grant (or revoke) a role. Takes effect with the account's next request; open WebSockets of
    /// this process get the new roles at once (`subscribe_role_changes`).
    pub(crate) async fn set_role(&self, state: &AppState, actor: &Actor, user: UserId, role: &str, grant: bool) -> Result<(), AppError> {
        if !net_backend_protocol::admin::is_valid_role(role) {
            return Err(AppError::bad_request("a role is 1-64 bytes of [a-z0-9_.-], starting with a letter"));
        }
        if !grant && actor.user == Some(user) && role == net_backend_protocol::admin::ADMIN_ROLE {
            return Err(AppError::conflict("you cannot remove your own admin role"));
        }
        Self::user(state.db(), user).await?;
        if !grant && role == net_backend_protocol::admin::ADMIN_ROLE && self.has_role(state, user, role).await? {
            let admins = state.db().fetch_one::<store::CountRow, _>(&store::count_role(role)).await?.n;
            if admins <= 1 {
                return Err(AppError::conflict("this is the last admin; grant the role to someone else first"));
            }
        }
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        if grant {
            match tx.execute(&store::insert_role(user.get(), role, now)?).await {
                Ok(_) => {}
                Err(error) if error.is_unique_violation() => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        } else if tx.execute(&store::delete_role(user.get(), role)).await? == 0 {
            return Ok(());
        }
        let record = actor.record(if grant { "role_grant" } else { "role_revoke" }, user).data(json!({ "role": role }));
        audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
        tx.commit().await?;
        let _ = self.0.role_changes.send(user);
        Ok(())
    }

    async fn has_role(&self, state: &AppState, user: UserId, role: &str) -> Result<bool, AppError> {
        Ok(state.db().fetch_all::<store::RoleRow, _>(&store::roles_of(&[user.get()])).await?.iter().any(|r| r.role == role))
    }

    /// Unlink a login provider from an account from server code (audited as `admin.identity_unlink`).
    pub async fn unlink_user_identity(&self, state: &AppState, user: UserId, provider: &str) -> Result<(), AppError> {
        self.unlink_identity(state, &ReqInfo::default(), user, provider, Some(&Actor::server())).await
    }

    /// `GET /v1/admin/audit`.
    pub async fn audit_log(&self, state: &AppState, query: &AuditQuery) -> Result<Page<AuditEntry>, AppError> {
        query.validate()?;
        let limit = Self::page_limit(query.limit);
        let before = Self::cursor_id(query.cursor.as_ref())?;
        let mut rows =
            state.db().fetch_all::<store::AuditRow, _>(&store::list_audit(query.user.map(|u| u.get()), query.action.as_deref(), before, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let items = rows
            .into_iter()
            .map(|r| {
                let mut entry = AuditEntry::new(r.id, r.action, UnixMillis(r.created_at));
                if let Some(actor) = r.actor_user_id {
                    entry = entry.with_actor(UserId(actor));
                }
                if let (Some(kind), Some(id)) = (r.target_type, r.target_id) {
                    entry = entry.with_target(kind, id);
                }
                if let Some(ip) = r.ip {
                    entry = entry.with_ip(ip);
                }
                if let Some(id) = r.request_id {
                    entry = entry.with_request_id(id);
                }
                if let Some(data) = r.data.and_then(|d| serde_json::from_str(&d).ok()) {
                    entry = entry.with_data(data);
                }
                entry
            })
            .collect();
        Ok(Page::new(items, next))
    }

    /// Ban an account from server code (audited as `admin.ban` without an actor).
    pub async fn ban_user(&self, state: &AppState, user: UserId, request: BanRequest) -> Result<(), AppError> {
        self.ban(state, &Actor::server(), user, request).await
    }

    /// Lift a ban from server code.
    pub async fn unban_user(&self, state: &AppState, user: UserId) -> Result<(), AppError> {
        self.unban(state, &Actor::server(), user).await
    }

    /// Grant (`grant = true`) or revoke a role from server code.
    pub async fn set_user_role(&self, state: &AppState, user: UserId, role: &str, grant: bool) -> Result<(), AppError> {
        self.set_role(state, &Actor::server(), user, role, grant).await
    }

    // ---- command line -----------------------------------------------------------------------

    /// Find an account by id (digits) or email address.
    pub(crate) async fn find_user(&self, state: &AppState, key: &str) -> Result<UserRow, AppError> {
        let db = state.db();
        let row = match key.trim().parse::<i64>() {
            Ok(id) => db.fetch_optional::<UserRow, _>(&store::user_by_id(id)).await?,
            Err(_) => db.fetch_optional::<UserRow, _>(&store::user_by_email(&normalize_email(key))).await?,
        };
        row.ok_or_else(|| AppError::not_found("no such account"))
    }

    /// Create an account (command line): returns its id.
    pub(crate) async fn create_user(
        &self,
        state: &AppState,
        email: &str,
        password: String,
        display_name: Option<String>,
        admin: bool,
        verified: bool,
    ) -> Result<UserId, AppError> {
        let mut request = RegisterRequest::new(email, password);
        if let Some(name) = display_name {
            request = request.with_display_name(name);
        }
        request.email = canonical_email(&request.email);
        request.validate()?;
        let email = canonical_email(&request.email);
        let normalized = normalize_email(&email);
        let hash = self.0.hasher.hash(nfc_password(request.password.into_inner())).await?;
        let now = Self::now(state);
        let mut tx = state.db().begin().await?;
        let verified_at = verified.then_some(now);
        let user = match tx.insert_id(&store::insert_user(Some(&email), Some(&normalized), request.display_name.as_deref(), verified_at, now)?, "id").await {
            Ok(id) => id,
            Err(error) if error.is_unique_violation() => return Err(AppError::new(codes::EMAIL_TAKEN, "this email address is already registered")),
            Err(error) => return Err(error.into()),
        };
        tx.execute(&store::insert_credentials(user, &hash, now)?).await?;
        if admin {
            tx.execute(&store::insert_role(user, net_backend_protocol::admin::ADMIN_ROLE, now)?).await?;
        }
        audit::record_tx(
            &mut tx,
            UnixMillis(now),
            &AuditRecord::new("cli.user_create").target_user(UserId(user)).data(json!({ "admin": admin, "verified": verified })),
        )
        .await?;
        tx.commit().await?;
        Ok(UserId(user))
    }

    /// Delete expired tokens and old sessions; returns the number of rows deleted.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let db = state.db();
        let now = Self::now(state);
        let day = 24 * 3600 * 1000;
        let mut deleted = 0;
        deleted += db.execute(&store::delete_before(store::ACCESS, "expires_at", now.saturating_sub(KEEP_EXPIRED_ACCESS_MS))).await?;
        deleted += db.execute(&store::delete_before(store::REFRESH, "expires_at", now.saturating_sub(day))).await?;
        deleted += db.execute(&store::delete_before(store::EMAIL_TOKENS, "expires_at", now.saturating_sub(day))).await?;
        deleted += db.execute(&store::delete_old_sessions(now.saturating_sub(KEEP_ENDED_SESSIONS_MS))).await?;
        let retention = self.0.config.audit_retention_days;
        if retention > 0 {
            deleted += db.execute(&store::delete_before(store::AUDIT, "created_at", now.saturating_sub(i64::from(retention).saturating_mul(day)))).await?;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(normalize_email("  Ada.Lovelace@Example.COM "), "ada.lovelace@example.com");
        // NFC: a decomposed "e + combining diaeresis" equals the composed letter.
        assert_eq!(normalize_email("Zoe\u{308}@Example.com"), normalize_email("zo\u{eb}@example.com"));
        assert_eq!(canonical_email(" Zoe\u{308}@Example.com "), "Zo\u{eb}@Example.com");
        assert_eq!(nfc_password("pa\u{73}\u{301}word".into()), "pa\u{15b}word");
        assert_eq!(AuthService::link(Some("https://x/v?t={token}"), "nbse_1"), "https://x/v?t=nbse_1");
        assert_eq!(AuthService::link(None, "nbse_1"), "nbse_1");
        assert_eq!(clean_user_agent(Some("game/1.0\r\nX: y")).as_deref(), Some("game/1.0X: y"));
        assert_eq!(clean_user_agent(Some("")), None);
        assert_eq!(secs_to_ms(u64::MAX), (i64::MAX / 1000) * 1000);
        assert_eq!(AuthService::page_limit(Some(0)), 1);
        assert_eq!(AuthService::page_limit(None), u64::from(DEFAULT_PAGE_LIMIT));
        assert!(AuthService::cursor_id(Some(&Cursor::new("x"))).is_err());
        assert_eq!(AuthService::cursor_id(Some(&Cursor::new("12"))).ok().flatten(), Some(12));
    }
}
