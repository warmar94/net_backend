//! The auth module's settings: `[modules.auth]` in the configuration (or [`Auth::with_config`]).
//!
//! ```toml
//! [modules.auth]
//! app_name = "My Game"
//! mail_from = "My Game <no-reply@example.com>"
//! verify_url = "https://example.com/verify?token={token}"
//! reset_url = "https://example.com/reset?token={token}"
//! mailer = "smtp"                       # "log" (default) or "smtp" (feature `smtp`)
//! smtp_host = "smtp.example.com"
//! smtp_username = "no-reply@example.com"
//! smtp_password_file = "/run/credentials/game/smtp"
//! steam_app_id = 480
//! steam_identity = "my-game"
//! steam_web_api_key_file = "/run/credentials/game/steam"
//! ```
//!
//! Secrets (`smtp_password`, `steam_web_api_key`) also come as `<name>_file`; they never show in
//! `Debug` output or error messages.
//!
//! [`Auth::with_config`]: crate::auth::Auth::with_config

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{resolve_secret, SecretString};
use crate::error::Error;

/// Which mailer the auth module sends with (unless the app gives one with
/// [`Auth::mailer`](crate::auth::Auth::mailer)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MailerKind {
    /// Write mails to the log (development). Bodies (with their one-time links) only with
    /// `log_mailer_show_links`.
    #[default]
    Log,
    /// Send through SMTP (cargo feature `smtp`).
    Smtp,
}

/// How the SMTP connection is secured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS (port 587); refused if the server cannot.
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No encryption: only for a relay on the same machine (`127.0.0.1` / `localhost`).
    None,
}

/// The auth module's settings. Build in code from [`AuthConfig::default`] by changing fields, or
/// let the module read `[modules.auth]`.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct AuthConfig {
    /// Access-token lifetime, seconds. Default 3600 (the protocol's default).
    pub access_token_ttl_secs: u64,
    /// Refresh-token lifetime, seconds; every refresh starts a new one. Default 30 days.
    pub refresh_token_ttl_secs: u64,
    /// Whether `POST /v1/auth/register` is open. Default true (false: 403 `forbidden`; accounts
    /// come from Steam logins or `user:create`).
    pub allow_registration: bool,
    /// Whether a password login needs a verified email address (403 `email_not_verified`).
    /// Default false.
    pub login_requires_verified_email: bool,
    /// Send the verification mail when an account registers. Default true.
    pub send_verification_on_register: bool,

    /// argon2id memory cost in KiB. Default 19456 (19 MiB, OWASP's minimum for argon2id).
    pub argon2_memory_kib: u32,
    /// argon2id iterations. Default 2.
    pub argon2_iterations: u32,
    /// argon2id lanes. Default 1.
    pub argon2_parallelism: u32,
    /// How many hashes run at once on the blocking pool (each needs `argon2_memory_kib`).
    /// Default 0 = the number of CPU cores.
    pub hash_concurrency: usize,
    /// How long a login waits for a free hashing slot before answering 503. Default 10 s.
    pub hash_queue_timeout_secs: u64,

    /// Email-verification token lifetime, seconds. Default 24 h.
    pub verify_token_ttl_secs: u64,
    /// Password-reset token lifetime, seconds. Default 1 h.
    pub reset_token_ttl_secs: u64,
    /// The link in the verification mail; `{token}` is replaced by the token. Without it the mail
    /// contains the token itself (for a game that asks the player to paste it).
    pub verify_url: Option<String>,
    /// The link in the reset mail (`{token}` replaced), or none (the token itself).
    pub reset_url: Option<String>,
    /// The name in mail subjects and texts. Default `Game`.
    pub app_name: String,
    /// The sender address (`Name <address>` or `address`). Required with `mailer = "smtp"`.
    pub mail_from: Option<String>,
    /// The mailer. Default `log`.
    pub mailer: MailerKind,
    /// The log mailer writes whole mail bodies (with their one-time links) to the log. Default
    /// false: only recipient (masked) and subject. Development only.
    pub log_mailer_show_links: bool,
    /// How many mails may wait to be sent; more are dropped (and logged). Default 1000.
    pub mail_queue: usize,
    /// How many mails are sent at once. Default 4.
    pub mail_concurrency: usize,
    /// The SMTP server.
    pub smtp_host: Option<String>,
    /// The SMTP port. Default: 587 (starttls), 465 (tls), 25 (none).
    pub smtp_port: Option<u16>,
    /// The SMTP security. Default `starttls`.
    pub smtp_tls: SmtpTls,
    /// The SMTP user name.
    pub smtp_username: Option<String>,
    /// The SMTP password (a secret).
    pub smtp_password: Option<SecretString>,
    /// A file holding the SMTP password instead.
    pub smtp_password_file: Option<PathBuf>,
    /// The time limit of one SMTP send, seconds. Default 15.
    pub smtp_timeout_secs: u64,

    /// The Steam app id (enables Steam login with the built-in verifier, feature `steam`).
    pub steam_app_id: Option<u32>,
    /// The identity string the game passes to `GetAuthTicketForWebApi` (tickets for another
    /// identity are refused).
    pub steam_identity: Option<String>,
    /// The Steamworks Web API publisher key (a secret).
    pub steam_web_api_key: Option<SecretString>,
    /// A file holding the key instead.
    pub steam_web_api_key_file: Option<PathBuf>,
    /// The Steam Web API base URL. Default `https://partner.steam-api.com` (publisher keys);
    /// `http://` only for a loopback address (tests).
    pub steam_api_url: String,
    /// The time limit of one Steam check, seconds. Default 10.
    pub steam_timeout_secs: u64,
    /// Refuse accounts with a VAC ban. Default false.
    pub steam_reject_vac_banned: bool,
    /// Refuse accounts the publisher banned. Default true.
    pub steam_reject_publisher_banned: bool,
    /// Accept a borrowed copy (Family Sharing: the owner differs from the player). Default true.
    pub steam_allow_family_sharing: bool,
    /// Linking or unlinking a login provider (Steam) needs a session started at most this many
    /// seconds ago (else 403 `reauthentication_required`: log in again). Default 600.
    pub link_reauth_secs: u64,
    /// Mail the account's owner when a login provider is linked. Default true.
    pub notify_on_link: bool,
    /// A password reset (proof of the mailbox; often after a compromise) also unlinks the account's
    /// login providers. Default true.
    pub unlink_identities_on_reset: bool,

    /// Turn the built-in rate limits on or off. Default true.
    pub rate_limits: bool,
    /// Per client address and route: logins (password and Steam) per minute. Default 10.
    pub login_per_minute: u32,
    /// Per client address: registrations per hour. Default 10.
    pub register_per_hour: u32,
    /// Per client address: refreshes per minute. Default 60.
    pub refresh_per_minute: u32,
    /// Per client address and route: forgot / reset / verify / resend per minute. Default 10.
    pub email_routes_per_minute: u32,
    /// Per account: mails (verification, reset) per hour; more are not sent (the answer stays
    /// the same). Default 3.
    pub mails_per_account_per_hour: u32,
    /// Per email address: failed logins before the account's logins are slowed down. Default 5.
    pub login_failures: u32,
    /// The time those failures take to be forgotten, seconds (then one more attempt per
    /// `login_lockout_secs / login_failures`). Default 900.
    pub login_lockout_secs: u64,
    /// Per email address, from all client addresses together: failed logins per hour. Above it only
    /// client networks that logged in to this account successfully before (in the last 30 days,
    /// remembered by this process) may still try. Default 50.
    pub account_failures_per_hour: u32,
    /// IPv6 clients are counted by this prefix (a /64 is one home line or server). Default 64.
    pub rate_limit_ipv6_prefix: u8,

    /// List the `/v1/admin` routes in the public OpenAPI document. Default false.
    pub admin_in_openapi: bool,
    /// How often expired tokens and old sessions are deleted, seconds. Default 3600 (0 = never).
    pub purge_interval_secs: u64,
    /// Audit entries older than this many days are deleted by the purge. Default 365 (0 = keep).
    pub audit_retention_days: u32,
    /// How often the server reads revocations made by other processes (the command line, another
    /// instance) from the database and passes them to `subscribe_revocations` receivers, seconds.
    /// Default 5 (0 = never: only this process's revocations are broadcast).
    pub revocation_poll_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            access_token_ttl_secs: net_backend_protocol::auth::DEFAULT_ACCESS_TOKEN_TTL_SECS,
            refresh_token_ttl_secs: net_backend_protocol::auth::DEFAULT_REFRESH_TOKEN_TTL_SECS,
            allow_registration: true,
            login_requires_verified_email: false,
            send_verification_on_register: true,
            argon2_memory_kib: 19 * 1024,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            hash_concurrency: 0,
            hash_queue_timeout_secs: 10,
            verify_token_ttl_secs: 24 * 3600,
            reset_token_ttl_secs: 3600,
            verify_url: None,
            reset_url: None,
            app_name: "Game".into(),
            mail_from: None,
            mailer: MailerKind::Log,
            log_mailer_show_links: false,
            mail_queue: 1000,
            mail_concurrency: 4,
            smtp_host: None,
            smtp_port: None,
            smtp_tls: SmtpTls::Starttls,
            smtp_username: None,
            smtp_password: None,
            smtp_password_file: None,
            smtp_timeout_secs: 15,
            steam_app_id: None,
            steam_identity: None,
            steam_web_api_key: None,
            steam_web_api_key_file: None,
            steam_api_url: "https://partner.steam-api.com".into(),
            steam_timeout_secs: 10,
            steam_reject_vac_banned: false,
            steam_reject_publisher_banned: true,
            steam_allow_family_sharing: true,
            link_reauth_secs: 600,
            notify_on_link: true,
            unlink_identities_on_reset: true,
            rate_limits: true,
            login_per_minute: 10,
            register_per_hour: 10,
            refresh_per_minute: 60,
            email_routes_per_minute: 10,
            mails_per_account_per_hour: 3,
            login_failures: 5,
            login_lockout_secs: 900,
            account_failures_per_hour: 50,
            rate_limit_ipv6_prefix: 64,
            admin_in_openapi: false,
            purge_interval_secs: 3600,
            audit_retention_days: 365,
            revocation_poll_secs: 5,
        }
    }
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets print as `<redacted>` through SecretString; mail and Steam ids are not secret.
        f.debug_struct("AuthConfig")
            .field("access_token_ttl_secs", &self.access_token_ttl_secs)
            .field("refresh_token_ttl_secs", &self.refresh_token_ttl_secs)
            .field("allow_registration", &self.allow_registration)
            .field("argon2", &(self.argon2_memory_kib, self.argon2_iterations, self.argon2_parallelism))
            .field("mailer", &self.mailer)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_password", &self.smtp_password)
            .field("steam_app_id", &self.steam_app_id)
            .field("steam_web_api_key", &self.steam_web_api_key)
            .field("rate_limits", &self.rate_limits)
            .finish_non_exhaustive()
    }
}

fn template_ok(url: &Option<String>) -> bool {
    url.as_deref().is_none_or(|u| (u.starts_with("https://") || u.starts_with("http://")) && u.contains("{token}") && !u.chars().any(char::is_whitespace))
}

/// Whether a URL's host is a loopback name or address.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(host, _)| host),
    };
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl AuthConfig {
    /// Resolve the `*_file` secrets and check every setting (all problems at once).
    pub fn resolve(mut self) -> Result<AuthConfig, Error> {
        self.smtp_password = resolve_secret("modules.auth.smtp_password", self.smtp_password.take(), self.smtp_password_file.as_deref())?;
        self.smtp_password_file = None;
        self.steam_web_api_key = resolve_secret("modules.auth.steam_web_api_key", self.steam_web_api_key.take(), self.steam_web_api_key_file.as_deref())?;
        self.steam_web_api_key_file = None;
        self.validate()?;
        Ok(self)
    }

    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let mut need = |ok: bool, problem: &str| {
            if !ok {
                problems.push(format!("modules.auth.{problem}"));
            }
        };
        need((60..=7 * 24 * 3600).contains(&self.access_token_ttl_secs), "access_token_ttl_secs must be between 60 and 604800");
        need(
            (3600..=400 * 24 * 3600).contains(&self.refresh_token_ttl_secs) && self.refresh_token_ttl_secs > self.access_token_ttl_secs,
            "refresh_token_ttl_secs must be between 3600 and 400 days and longer than the access token",
        );
        need((8..=4 * 1024 * 1024).contains(&self.argon2_memory_kib), "argon2_memory_kib must be between 8 and 4194304");
        need((1..=64).contains(&self.argon2_iterations), "argon2_iterations must be between 1 and 64");
        need((1..=64).contains(&self.argon2_parallelism), "argon2_parallelism must be between 1 and 64");
        need(self.argon2_memory_kib >= 8 * self.argon2_parallelism, "argon2_memory_kib must be at least 8 x argon2_parallelism");
        need(self.hash_concurrency <= 1024, "hash_concurrency must be at most 1024");
        need((1..=300).contains(&self.hash_queue_timeout_secs), "hash_queue_timeout_secs must be between 1 and 300");
        need((300..=30 * 24 * 3600).contains(&self.verify_token_ttl_secs), "verify_token_ttl_secs must be between 300 and 30 days");
        need((300..=7 * 24 * 3600).contains(&self.reset_token_ttl_secs), "reset_token_ttl_secs must be between 300 and 7 days");
        need(template_ok(&self.verify_url), "verify_url must be an http(s) URL containing {token}");
        need(template_ok(&self.reset_url), "reset_url must be an http(s) URL containing {token}");
        need(!self.app_name.trim().is_empty() && !self.app_name.chars().any(char::is_control), "app_name must not be empty or contain control characters");
        need((1..=1_000_000).contains(&self.mail_queue), "mail_queue must be between 1 and 1000000");
        need((1..=64).contains(&self.mail_concurrency), "mail_concurrency must be between 1 and 64");
        need((1..=300).contains(&self.smtp_timeout_secs), "smtp_timeout_secs must be between 1 and 300");
        if self.mailer == MailerKind::Smtp {
            need(cfg!(feature = "smtp"), "mailer = \"smtp\" needs the `smtp` feature of net_backend_server");
            need(self.smtp_host.as_deref().is_some_and(|h| !h.trim().is_empty()), "smtp_host is required with mailer = \"smtp\"");
            need(self.mail_from.is_some(), "mail_from is required with mailer = \"smtp\"");
            need(
                self.smtp_tls != SmtpTls::None
                    || self.smtp_host.as_deref().is_some_and(|h| h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())),
                "smtp_tls = \"none\" is only allowed for a relay on this machine (localhost / 127.0.0.1)",
            );
            need(self.smtp_username.is_some() == self.smtp_password.is_some(), "smtp_username and smtp_password go together");
        }
        if let Some(from) = &self.mail_from {
            need(from.contains('@') && !from.chars().any(char::is_control), "mail_from is not a mail address");
        }
        let steam_any = self.steam_app_id.is_some() || self.steam_web_api_key.is_some();
        if steam_any {
            need(self.steam_app_id.is_some() && self.steam_web_api_key.is_some(), "steam_app_id and steam_web_api_key go together");
            need(self.steam_identity.as_deref().is_some_and(|i| !i.trim().is_empty()), "steam_identity is required for Steam login");
        }
        if let Some(identity) = &self.steam_identity {
            need(identity.len() <= 256 && !identity.chars().any(char::is_control), "steam_identity must be at most 256 bytes without control characters");
        }
        let api = &self.steam_api_url;
        need(
            !api.ends_with('/') && (api.starts_with("https://") || (api.starts_with("http://") && is_loopback_url(api))),
            "steam_api_url must be an https:// URL without a trailing / (http:// only for a loopback address)",
        );
        need((1..=120).contains(&self.steam_timeout_secs), "steam_timeout_secs must be between 1 and 120");
        for (value, name) in [
            (self.login_per_minute, "login_per_minute"),
            (self.register_per_hour, "register_per_hour"),
            (self.refresh_per_minute, "refresh_per_minute"),
            (self.email_routes_per_minute, "email_routes_per_minute"),
            (self.mails_per_account_per_hour, "mails_per_account_per_hour"),
            (self.login_failures, "login_failures"),
            (self.account_failures_per_hour, "account_failures_per_hour"),
        ] {
            need((1..=1_000_000).contains(&value), &format!("{name} must be between 1 and 1000000"));
        }
        need((1..=7 * 24 * 3600).contains(&self.login_lockout_secs), "login_lockout_secs must be between 1 and 604800");
        need(self.purge_interval_secs == 0 || self.purge_interval_secs >= 60, "purge_interval_secs must be 0 (never) or at least 60");
        need((60..=7 * 24 * 3600).contains(&self.link_reauth_secs), "link_reauth_secs must be between 60 and 604800");
        need((32..=128).contains(&self.rate_limit_ipv6_prefix), "rate_limit_ipv6_prefix must be between 32 and 128");
        need(self.revocation_poll_secs <= 3600, "revocation_poll_secs must be at most 3600");
        need(self.audit_retention_days <= 36500, "audit_retention_days must be at most 36500");
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(AuthConfig::default().validate().is_ok());
    }

    #[test]
    fn problems_are_collected_and_secrets_hidden() {
        let config = AuthConfig {
            access_token_ttl_secs: 1,
            verify_url: Some("https://example.com/verify".into()),
            steam_web_api_key: Some(SecretString::new("STEAM-KEY-SECRET")),
            steam_api_url: "http://steam.example.com".into(),
            ..AuthConfig::default()
        };
        let error = config.clone().resolve().err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["access_token_ttl_secs", "verify_url", "steam_app_id and steam_web_api_key", "steam_identity", "steam_api_url"] {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert!(!error.contains("STEAM-KEY-SECRET"));
        assert!(!format!("{config:?}").contains("STEAM-KEY-SECRET"));
    }

    #[test]
    fn loopback_urls() {
        for ok in ["http://127.0.0.1:9000", "http://localhost", "http://[::1]:80/x", "http://127.0.0.2"] {
            assert!(is_loopback_url(ok), "{ok}");
        }
        for bad in ["http://example.com", "http://10.0.0.1:80", "http://127.0.0.1.example.com"] {
            assert!(!is_loopback_url(bad), "{bad}");
        }
    }

    #[test]
    fn toml_section() {
        let config: AuthConfig = toml::from_str("app_name = \"X\"\nmailer = \"log\"\nsmtp_tls = \"tls\"").unwrap_or_default();
        assert_eq!(config.app_name, "X");
        assert_eq!(config.smtp_tls, SmtpTls::Tls);
        assert!(toml::from_str::<AuthConfig>("unknown_key = 1").is_err());
    }
}
