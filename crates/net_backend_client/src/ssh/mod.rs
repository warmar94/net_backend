//! SSH to the server machine's OpenSSH (feature `ssh`), and SFTP on it (feature `sftp`). For admin
//! tools: deploy scripts, log tails, backups. It never goes through `net_backend_server`.
//!
//! **ADMIN / DEV TOOLS ONLY.** An SSH key (or agent access) inside a program you give to players is
//! shell access for anyone who extracts it. Every connect is refused in a release build
//! (`cfg(not(debug_assertions))`) unless the target opts in with [`SshTarget::allow_in_release`].
//!
//! ```no_run
//! use net_backend_client::ssh::{SshAuth, SshSession, SshTarget};
//!
//! # async fn run() -> Result<(), net_backend_client::Error> {
//! let target = SshTarget::new("server.example.com", "deploy")
//!     .with_auth(SshAuth::agent())
//!     .with_known_hosts_file("/home/admin/.ssh/known_hosts");
//! let ssh = SshSession::connect(target).await?;
//! let output = ssh.run("systemctl is-active net-backend").await?;
//! println!("{} (exit {:?})", output.stdout_text(), output.exit.status);
//! ssh.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! **Host keys** are always checked: the server's key must be in a known_hosts file (read-only;
//! `~/.ssh/known_hosts` by default) or match a fingerprint pinned in code. An unknown, changed or
//! revoked key is [`Error::HostKey`], never accepted silently. Connections
//! that would be open to the Terrapin attack (CVE-2023-48795) are refused unless
//! [`SshTarget::allow_terrapin_vulnerable`] is set (the refusal is tested against an in-process mock server;
//! OpenSSH 9.6+ supports strict key exchange and connects). RSA keys need feature `ssh-rsa`; SHA-1 RSA
//! signatures are never used.
//!
//! **A lost connection** ends the session, unless the target opts in to
//! [`SshTarget::with_reconnect`]: then a new connection is opened with backoff, a command that was
//! running is never run again, and commands started meanwhile wait for it ([`SshReconnect`];
//! [`SshSession::state`], [`SshSession::events`]).
//!
//! **SFTP** (feature `sftp`): downloads keep 16 reads of 64 KiB in flight and write them in file
//! order; transfers report their progress (`SshSession::start_download` and the other `start_*`
//! methods return an `SftpTask`); the remote file handle is closed after every transfer, also a
//! cancelled or timed-out one.

mod known_hosts;
mod session;
#[cfg(feature = "sftp")]
mod sftp;
mod ssh_config;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::Secret;

pub use session::{SshChunk, SshEvent, SshEvents, SshRun, SshSession, SshState};
#[cfg(feature = "sftp")]
#[cfg_attr(docsrs, doc(cfg(feature = "sftp")))]
pub use sftp::{SftpEntry, SftpEntryKind, SftpProgress, SftpTask};

use crate::{Error, MAX_TIMEOUT};

/// Default limit for TCP connect + key exchange + host key check + authentication together: 15 s.
pub const DEFAULT_SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default time a command may run: 60 s.
pub const DEFAULT_SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Default limit of a command's output (stdout + stderr): 8 MiB.
pub const DEFAULT_SSH_MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Default time an SFTP operation (a whole transfer) may take: 5 min.
pub const DEFAULT_SFTP_TIMEOUT: Duration = Duration::from_secs(300);
/// Default limit of one SFTP transfer (upload or download): 256 MiB.
pub const DEFAULT_SFTP_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The longest command line accepted: 64 KiB.
pub const MAX_SSH_COMMAND_BYTES: usize = 64 * 1024;

/// How to log in. Several can be given ([`SshTarget::with_auth`]); they are tried in order.
/// **Keys are the recommendation** (a key file or the agent); a password and keyboard-interactive
/// (e.g. password + a 2FA code) are opt-in for servers that need them, with values typed by the
/// admin at runtime. There is deliberately no way to pass key bytes: keys are loaded at runtime from
/// the admin's machine, never baked into a binary. `Debug` shows the key file's name, never its
/// path, contents, passphrase or password.
#[derive(Clone)]
pub struct SshAuth(pub(crate) AuthKind);

#[derive(Clone)]
pub(crate) enum AuthKind {
    KeyFile { path: PathBuf, passphrase: Option<Secret> },
    Agent,
    Password(Secret),
    KeyboardInteractive(Arc<dyn SshPromptResponder>),
}

impl SshAuth {
    /// A private key file without a passphrase (OpenSSH, PKCS#8 or PuTTY format; ed25519 or ECDSA,
    /// RSA with feature `ssh-rsa`). Read when connecting (at most 256 KiB).
    pub fn key_file(path: impl Into<PathBuf>) -> Self {
        Self(AuthKind::KeyFile { path: path.into(), passphrase: None })
    }

    /// An encrypted private key file and its passphrase (typed by the admin at runtime).
    pub fn key_file_with_passphrase(path: impl Into<PathBuf>, passphrase: impl Into<Secret>) -> Self {
        Self(AuthKind::KeyFile { path: path.into(), passphrase: Some(passphrase.into()) })
    }

    /// The running SSH agent: `SSH_AUTH_SOCK` on Unix; on Windows the OpenSSH agent's named pipe
    /// (`\\.\pipe\openssh-ssh-agent`), then Pageant. Each key the agent offers is tried (RSA keys
    /// only with feature `ssh-rsa`; certificates are skipped).
    pub fn agent() -> Self {
        Self(AuthKind::Agent)
    }

    /// Opt-in: a password (SSH `password` method), typed by the admin at runtime. Prefer a key.
    pub fn password(password: impl Into<Secret>) -> Self {
        Self(AuthKind::Password(password.into()))
    }

    /// Opt-in: keyboard-interactive (the server asks questions, e.g. `Password:` then
    /// `Verification code:` for 2FA). `responder` answers each round; it must not block (collect
    /// the answers first, e.g. with [`SshPromptAnswers`]). At most 8 rounds.
    pub fn keyboard_interactive(responder: impl SshPromptResponder) -> Self {
        Self(AuthKind::KeyboardInteractive(Arc::new(responder)))
    }
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            AuthKind::KeyFile { path, passphrase } => {
                f.debug_struct("KeyFile").field("file", &file_name(path)).field("passphrase", &passphrase.as_ref().map(|_| "<redacted>")).finish()
            }
            AuthKind::Agent => f.write_str("Agent"),
            AuthKind::Password(_) => f.write_str("Password(<redacted>)"),
            AuthKind::KeyboardInteractive(_) => f.write_str("KeyboardInteractive"),
        }
    }
}

/// One prompt of a keyboard-interactive round (server-supplied text, control characters removed).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshPrompt {
    /// The question, e.g. `Password:` or `Verification code:`.
    pub text: String,
    /// Whether the answer may be shown while typed (`false` for secrets).
    pub echo: bool,
}

impl SshPrompt {
    /// A prompt (for tests of a responder).
    pub fn new(text: impl Into<String>, echo: bool) -> Self {
        Self { text: text.into(), echo }
    }
}

/// One keyboard-interactive round: the server's name, instructions and prompts (all untrusted).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshPromptRequest {
    /// The round's name (often empty).
    pub name: String,
    /// Instructions (often empty).
    pub instructions: String,
    /// The questions; answer each, in order.
    pub prompts: Vec<SshPrompt>,
}

impl SshPromptRequest {
    /// A request (for tests of a responder).
    pub fn new(prompts: Vec<SshPrompt>) -> Self {
        Self { name: String::new(), instructions: String::new(), prompts }
    }
}

/// Answers keyboard-interactive prompts ([`SshAuth::keyboard_interactive`]). Never block. Return
/// one answer per prompt, or `None` to give up.
pub trait SshPromptResponder: Send + Sync + 'static {
    /// Answer one round.
    fn respond(&self, request: &SshPromptRequest) -> Option<Vec<Secret>>;
}

/// A ready-made [`SshPromptResponder`]: answers each prompt whose text contains a given word
/// (case-insensitive), e.g. `password` and `code`. A round with a prompt nothing matches gives up.
/// `Debug` shows the words, never the answers.
///
/// ```
/// use net_backend_client::ssh::{SshAuth, SshPromptAnswers};
///
/// // Both typed by the admin before connecting.
/// let (password, code) = ("typed-password", "123456");
/// let auth = SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("password", password).answer_containing("code", code));
/// # let _ = auth;
/// ```
#[derive(Clone, Default)]
pub struct SshPromptAnswers {
    answers: Vec<(String, Secret)>,
}

impl fmt::Debug for SshPromptAnswers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshPromptAnswers").field("words", &self.answers.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>()).finish()
    }
}

impl SshPromptAnswers {
    /// No answers yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer prompts containing `word` (case-insensitive) with `answer`. The first matching word wins.
    pub fn answer_containing(mut self, word: impl Into<String>, answer: impl Into<Secret>) -> Self {
        self.answers.push((word.into().to_lowercase(), answer.into()));
        self
    }
}

impl SshPromptResponder for SshPromptAnswers {
    fn respond(&self, request: &SshPromptRequest) -> Option<Vec<Secret>> {
        request
            .prompts
            .iter()
            .map(|prompt| {
                let text = prompt.text.to_lowercase();
                self.answers.iter().find(|(word, _)| text.contains(word.as_str())).map(|(_, answer)| answer.clone())
            })
            .collect()
    }
}

/// Automatic reconnect of an SSH session ([`SshTarget::with_reconnect`]; off unless set):
/// exponential backoff with full jitter, as for the WebSocket. A reconnect **never re-runs a
/// command**: a command that was running when the connection was lost is answered
/// [`Error::Disconnected`] (`sent: Some(true)` once it had started); an SFTP operation that was
/// running is answered [`Error::Disconnected`] too (`sent: Some(true)` once the server had answered
/// part of a transfer, else `None`) and is not repeated either; commands and
/// SFTP operations started while it reconnects wait for the new connection (bounded by their own
/// timeout). Not retried: host key, authentication, protocol ([`Error::Ssh`]) and invalid-settings
/// errors end the session. The delay before attempt `n` is a random value in
/// `0..=min(cap, base · 2^(n-1))`; the counter resets once a connection stayed up for
/// `stable_after`. Defaults: base 1 s, cap 30 s, no attempt limit, stable after 10 s.
#[derive(Clone, Debug)]
pub struct SshReconnect {
    base: Duration,
    cap: Duration,
    max_attempts: Option<u32>,
    stable_after: Duration,
    jitter: bool,
}

impl Default for SshReconnect {
    fn default() -> Self {
        Self { base: Duration::from_secs(1), cap: Duration::from_secs(30), max_attempts: None, stable_after: Duration::from_secs(10), jitter: true }
    }
}

impl SshReconnect {
    /// The first delay bound (default 1 s, 1 ms..=1 h).
    pub fn with_base(mut self, base: Duration) -> Self {
        self.base = base.clamp(Duration::from_millis(1), MAX_TIMEOUT);
        self
    }

    /// The largest delay (default 30 s, at most 1 h).
    pub fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap.min(MAX_TIMEOUT);
        self
    }

    /// Give up after this many failed attempts in a row (`None` = never; default).
    pub fn with_max_attempts(mut self, max: Option<u32>) -> Self {
        self.max_attempts = max;
        self
    }

    /// How long a connection must stay up before the attempt counter resets (default 10 s).
    pub fn with_stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after.min(MAX_TIMEOUT);
        self
    }

    /// Random jitter on (default) or off (exact delays, for tests).
    pub fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }

    /// The upper bound of the delay before attempt `attempt` (1-based).
    pub fn delay_bound(&self, attempt: u32) -> Duration {
        let factor = 2u32.checked_pow(attempt.saturating_sub(1).min(30)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap.max(self.base))
    }

    pub(crate) fn delay(&self, attempt: u32, random: u64) -> Duration {
        let bound = self.delay_bound(attempt);
        if !self.jitter {
            return bound;
        }
        let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(random % nanos.saturating_add(1))
    }

    pub(crate) fn may_retry(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }

    pub(crate) fn stable_after(&self) -> Duration {
        self.stable_after
    }
}

/// The last component of a path, for logs and errors (a full path can hold a user name).
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| "<no file name>".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Where the settings of a connection come from.
#[derive(Clone)]
pub(crate) enum TargetSource {
    Direct,
    /// A `Host` alias of an ssh_config file (`None` = `~/.ssh/config`), resolved when connecting.
    Config(Option<PathBuf>),
}

/// One SSH server and how to reach it ([`SshSession::connect`]). Private fields + builder.
///
/// ```
/// use std::time::Duration;
/// use net_backend_client::ssh::{SshAuth, SshTarget};
///
/// let target = SshTarget::new("server.example.com", "deploy")
///     .with_port(2222)
///     .with_auth(SshAuth::agent())
///     .with_auth(SshAuth::key_file("/home/admin/.ssh/id_ed25519"))
///     .with_known_hosts_file("/home/admin/.ssh/known_hosts")
///     .with_command_timeout(Duration::from_secs(30));
/// assert!(target.validate().is_ok());
/// ```
#[derive(Clone)]
pub struct SshTarget {
    pub(crate) host: String,
    pub(crate) source: TargetSource,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) auth: Vec<SshAuth>,
    pub(crate) known_hosts: Vec<PathBuf>,
    pub(crate) pinned: Vec<String>,
    pub(crate) connect_timeout: Duration,
    pub(crate) keepalive_interval: Duration,
    pub(crate) keepalive_max: u32,
    pub(crate) command_timeout: Duration,
    pub(crate) max_output_bytes: u64,
    pub(crate) sftp_timeout: Duration,
    pub(crate) max_transfer_bytes: u64,
    pub(crate) max_channels: usize,
    pub(crate) allow_terrapin_vulnerable: bool,
    pub(crate) allow_in_release: bool,
    pub(crate) reconnect: Option<SshReconnect>,
}

impl fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("from_ssh_config", &matches!(self.source, TargetSource::Config(_)))
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("known_hosts_files", &self.known_hosts.len())
            .field("pinned_fingerprints", &self.pinned.len())
            .field("connect_timeout", &self.connect_timeout)
            .field("keepalive", &(self.keepalive_interval, self.keepalive_max))
            .field("command_timeout", &self.command_timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("sftp_timeout", &self.sftp_timeout)
            .field("max_transfer_bytes", &self.max_transfer_bytes)
            .field("max_channels", &self.max_channels)
            .field("allow_terrapin_vulnerable", &self.allow_terrapin_vulnerable)
            .field("allow_in_release", &self.allow_in_release)
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

impl SshTarget {
    /// A server by host name (or IP address), port 22, logging in as `user`. Add at least one
    /// [`SshAuth`].
    pub fn new(host: impl Into<String>, user: impl Into<String>) -> Self {
        Self::blank(host.into(), TargetSource::Direct, Some(user.into()))
    }

    /// A `Host` alias of the user's `~/.ssh/config`: `HostName`, `Port`, `User`, `IdentityFile` (no
    /// passphrase) and `ConnectTimeout` are taken from it when connecting. Settings given here win.
    /// `Match` blocks and `%` tokens are not supported; `ProxyJump` / `ProxyCommand` are ignored (the
    /// connection goes straight to the host).
    pub fn from_ssh_config(alias: impl Into<String>) -> Self {
        Self::blank(alias.into(), TargetSource::Config(None), None)
    }

    /// Like [`from_ssh_config`](Self::from_ssh_config) with another config file.
    pub fn from_ssh_config_file(path: impl Into<PathBuf>, alias: impl Into<String>) -> Self {
        Self::blank(alias.into(), TargetSource::Config(Some(path.into())), None)
    }

    fn blank(host: String, source: TargetSource, user: Option<String>) -> Self {
        Self {
            host,
            source,
            port: None,
            user,
            auth: Vec::new(),
            known_hosts: Vec::new(),
            pinned: Vec::new(),
            connect_timeout: DEFAULT_SSH_CONNECT_TIMEOUT,
            keepalive_interval: Duration::from_secs(15),
            keepalive_max: 3,
            command_timeout: DEFAULT_SSH_COMMAND_TIMEOUT,
            max_output_bytes: DEFAULT_SSH_MAX_OUTPUT_BYTES,
            sftp_timeout: DEFAULT_SFTP_TIMEOUT,
            max_transfer_bytes: DEFAULT_SFTP_MAX_BYTES,
            max_channels: 8,
            allow_terrapin_vulnerable: false,
            allow_in_release: false,
            reconnect: None,
        }
    }

    /// The port (default 22, or the config's `Port`).
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// The user to log in as (wins over the config's `User`).
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Add an authentication method; tried in the order added (before any `IdentityFile` of the config).
    pub fn with_auth(mut self, auth: SshAuth) -> Self {
        self.auth.push(auth);
        self
    }

    /// Read host keys from this known_hosts file (call again for more files). Once a file or a
    /// pinned fingerprint is given, `~/.ssh/known_hosts` is no longer read. Read-only: nothing is
    /// ever added. OpenSSH format: patterns (`*`, `?`, `!`), hashed hosts (`|1|…`), `[host]:port`,
    /// `@revoked` (`@cert-authority` lines are ignored: host certificates are not supported).
    pub fn with_known_hosts_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts.push(path.into());
        self
    }

    /// **Trust the server key with this fingerprint** (`SHA256:…` as `ssh-keygen -lf` prints it)
    /// without a known_hosts entry. Only pin a fingerprint you checked on the server itself (for
    /// example `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`): a wrong pin trusts an attacker.
    /// `@revoked` lines of the known_hosts files that are read still apply.
    pub fn trust_host_key_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.pinned.push(fingerprint.into().trim().to_string());
        self
    }

    /// One deadline for TCP connect + key exchange + host key check + authentication (default 15 s,
    /// clamped to 1 s..=1 h). A server that trickles bytes cannot stretch it.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
        self
    }

    /// Keepalive: when nothing arrived for `interval` (default 15 s, 1 s..=1 h), ask the server for a
    /// sign of life; after `max_missed` (default 3, 1..=100) unanswered asks the connection is lost.
    pub fn with_keepalive(mut self, interval: Duration, max_missed: u32) -> Self {
        self.keepalive_interval = interval.clamp(Duration::from_secs(1), MAX_TIMEOUT);
        self.keepalive_max = max_missed.clamp(1, 100);
        self
    }

    /// The default time a command may run (default 60 s, 1 ms..=1 h); a command can override it.
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT);
        self
    }

    /// The default output limit of a command, stdout + stderr (default 8 MiB, at least 1 KiB); at
    /// the limit the command is stopped and answered [`Error::BodyTooLarge`].
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes.max(1024);
        self
    }

    /// The time one SFTP operation (a whole transfer) may take (default 5 min, 1 ms..=1 h).
    pub fn with_sftp_timeout(mut self, timeout: Duration) -> Self {
        self.sftp_timeout = timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT);
        self
    }

    /// The largest SFTP upload or download (default 256 MiB, at least 1 KiB).
    pub fn with_max_transfer_bytes(mut self, bytes: u64) -> Self {
        self.max_transfer_bytes = bytes.max(1024);
        self
    }

    /// How many commands / SFTP operations may run at once (default 8, 1..=64); more wait for a free
    /// channel (their timeout keeps counting). OpenSSH allows 10 channels per connection by default.
    pub fn with_max_channels(mut self, channels: usize) -> Self {
        self.max_channels = channels.clamp(1, 64);
        self
    }

    /// **INSECURE, for old servers only: accept a Terrapin-vulnerable connection** (default
    /// `false`). Without it, a server that does not support strict key exchange (OpenSSH before 9.6
    /// without the distribution's backport) and ends up with ChaCha20-Poly1305 or a CBC +
    /// encrypt-then-MAC cipher is refused (CVE-2023-48795). AES-GCM is preferred and not affected,
    /// so most such servers connect anyway; update the server instead of setting this.
    pub fn allow_terrapin_vulnerable(mut self, allow: bool) -> Self {
        self.allow_terrapin_vulnerable = allow;
        self
    }

    /// Allow connecting in a release build (default `false`: every connect of a release build is
    /// refused with `InvalidRequest`). Only for internal admin / dev tools: an SSH key in a program
    /// that reaches players is shell access for anyone who extracts it.
    pub fn allow_in_release(mut self, allow: bool) -> Self {
        self.allow_in_release = allow;
        self
    }

    /// Reconnect automatically after the connection is lost (default: off; a lost connection
    /// ends the session). See [`SshReconnect`]: a command is never re-run. The first connection
    /// is the one [`SshSession::connect`] returns (an error there is the caller's answer).
    pub fn with_reconnect(mut self, reconnect: SshReconnect) -> Self {
        self.reconnect = Some(reconnect);
        self
    }

    /// The host name (or the ssh_config alias).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port given here (the config's `Port`, else 22, is used when `None`).
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The user given here.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Whether SSH may connect in this build: always in debug builds, in release builds only with
    /// [`allow_in_release`](Self::allow_in_release).
    pub fn is_allowed(&self) -> bool {
        allowed(cfg!(debug_assertions), self.allow_in_release)
    }

    /// Check what can be checked before connecting: host, user and port syntax, the pinned
    /// fingerprints' format, and that there is a way to authenticate (an ssh_config alias is only
    /// read when connecting).
    pub fn validate(&self) -> Result<(), Error> {
        check_host(&self.host)?;
        if let Some(user) = &self.user {
            check_user(user)?;
        }
        if self.port == Some(0) {
            return Err(Error::invalid("the SSH port must not be 0"));
        }
        for pin in &self.pinned {
            check_fingerprint(pin)?;
        }
        match self.source {
            TargetSource::Direct if self.user.is_none() => Err(Error::invalid("no SSH user given")),
            TargetSource::Direct if self.auth.is_empty() => Err(Error::invalid("no SSH authentication method given (SshAuth::key_file or SshAuth::agent)")),
            _ => Ok(()),
        }
    }
}

/// The release-build guard.
pub(crate) fn allowed(debug_build: bool, allow_in_release: bool) -> bool {
    debug_build || allow_in_release
}

/// A host name or address: no whitespace, control characters or leading `-`, 1..=255 bytes.
pub(crate) fn check_host(host: &str) -> Result<(), Error> {
    if host.is_empty() || host.len() > 255 || host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::invalid("the SSH host is not a valid host name or address"));
    }
    Ok(())
}

/// A user name: 1..=255 bytes, no whitespace or control characters.
pub(crate) fn check_user(user: &str) -> Result<(), Error> {
    if user.is_empty() || user.len() > 255 || user.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::invalid("the SSH user name is empty or has whitespace / control characters"));
    }
    Ok(())
}

/// `SHA256:` followed by the unpadded base64 of 32 bytes (43 characters).
pub(crate) fn check_fingerprint(pin: &str) -> Result<(), Error> {
    let ok = pin.strip_prefix("SHA256:").is_some_and(|b64| b64.len() == 43 && b64.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid("a pinned host key fingerprint must look like `SHA256:` + 43 base64 characters (as `ssh-keygen -lf` prints it)"))
    }
}

/// A command to run ([`SshSession::run`]). The command line goes to the server's shell as it is:
/// quote arguments yourself. It may hold secrets: its `Debug` shows only its length, and the crate
/// never logs it. Prefer passing secrets through [`with_stdin`](Self::with_stdin) (a command line is
/// visible to other users of the server in its process list).
#[derive(Clone)]
pub struct SshCommand {
    pub(crate) command: String,
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_output_bytes: Option<u64>,
    pub(crate) stdin: Option<Vec<u8>>,
}

impl fmt::Debug for SshCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshCommand")
            .field("command_bytes", &self.command.len())
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("stdin_bytes", &self.stdin.as_ref().map(Vec::len))
            .finish()
    }
}

impl SshCommand {
    /// A command line (non-empty, no NUL, at most [`MAX_SSH_COMMAND_BYTES`]).
    pub fn new(command: impl Into<String>) -> Self {
        Self { command: command.into(), timeout: None, max_output_bytes: None, stdin: None }
    }

    /// This command's time limit (instead of the target's; clamped to 1 ms..=1 h), counted from
    /// the call, waiting for a free channel included.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT));
        self
    }

    /// This command's output limit, stdout + stderr (instead of the target's; at least 1 KiB).
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = Some(bytes.max(1024));
        self
    }

    /// Bytes written to the command's stdin, followed by end-of-file (without it, stdin is closed at once).
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// The command line. Do not log it.
    pub fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.command.trim().is_empty() {
            return Err(Error::invalid("the SSH command is empty"));
        }
        if self.command.contains('\0') {
            return Err(Error::invalid("the SSH command contains a NUL byte"));
        }
        if self.command.len() > MAX_SSH_COMMAND_BYTES {
            return Err(Error::RequestTooLarge { limit: MAX_SSH_COMMAND_BYTES as u64, size: self.command.len() as u64 });
        }
        Ok(())
    }
}

impl From<&str> for SshCommand {
    fn from(command: &str) -> Self {
        Self::new(command)
    }
}

impl From<String> for SshCommand {
    fn from(command: String) -> Self {
        Self::new(command)
    }
}

/// How a command ended. A non-zero status is still `Ok`: the command ran (check [`success`](Self::success)).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshExit {
    /// The exit status, if the server sent one.
    pub status: Option<u32>,
    /// The signal that ended it (`"TERM"`, `"KILL"`, …), if the server sent one.
    pub signal: Option<String>,
    /// Bytes of stdout received.
    pub stdout_bytes: u64,
    /// Bytes of stderr received.
    pub stderr_bytes: u64,
}

impl SshExit {
    /// Whether the exit status is 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// Which output stream a chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SshStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A finished command with its whole output ([`SshSession::run`]). `Debug` shows lengths only
/// (output may hold secrets).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshOutput {
    /// How it ended.
    pub exit: SshExit,
    /// Everything it wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything it wrote to stderr.
    pub stderr: Vec<u8>,
}

impl fmt::Debug for SshOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshOutput").field("exit", &self.exit).field("stdout_bytes", &self.stdout.len()).field("stderr_bytes", &self.stderr.len()).finish()
    }
}

impl SshOutput {
    /// stdout as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Whether the exit status is 0.
    pub fn success(&self) -> bool {
        self.exit.success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_answers_match_words_and_give_up_on_unknown_prompts() {
        let answers = SshPromptAnswers::new().answer_containing("Password", "fake-pw-1").answer_containing("code", "123456");
        let request = SshPromptRequest::new(vec![SshPrompt::new("Password: ", false), SshPrompt::new("Verification code: ", true)]);
        let got: Option<Vec<String>> = answers.respond(&request).map(|a| a.iter().map(|s| s.expose().to_string()).collect());
        assert_eq!(got, Some(vec!["fake-pw-1".to_string(), "123456".to_string()]));
        assert!(answers.respond(&SshPromptRequest::new(vec![SshPrompt::new("Favourite colour?", true)])).is_none());
        let debug = format!("{answers:?} {:?} {:?}", SshAuth::password("fake-pw-1"), SshAuth::keyboard_interactive(answers.clone()));
        assert!(!debug.contains("fake-pw-1") && !debug.contains("123456"), "{debug}");
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        let policy = SshReconnect::default().with_base(Duration::from_millis(100)).with_cap(Duration::from_secs(2)).with_jitter(false);
        assert_eq!(policy.delay_bound(1), Duration::from_millis(100));
        assert_eq!(policy.delay_bound(3), Duration::from_millis(400));
        assert_eq!(policy.delay(40, 0), Duration::from_secs(2));
        let jitter = SshReconnect::default().with_base(Duration::MAX).with_cap(Duration::MAX);
        assert!(jitter.delay(5, u64::MAX) <= MAX_TIMEOUT);
        assert!(SshReconnect::default().with_max_attempts(Some(2)).may_retry(2));
        assert!(!SshReconnect::default().with_max_attempts(Some(2)).may_retry(3));
        assert_eq!(SshReconnect::default().delay_bound(1), Duration::from_secs(1));
        assert!(format!("{:?}", SshTarget::new("h", "u").with_reconnect(SshReconnect::default())).contains("reconnect: Some"));
    }

    #[test]
    fn the_release_guard() {
        assert!(allowed(true, false), "debug builds always");
        assert!(!allowed(false, false), "release builds refuse by default");
        assert!(allowed(false, true), "release builds with allow_in_release");
        assert_eq!(SshTarget::new("h", "u").is_allowed(), cfg!(debug_assertions));
        assert!(SshTarget::new("h", "u").allow_in_release(true).is_allowed());
    }

    #[test]
    fn targets_are_validated() {
        let ok = SshTarget::new("host.example.com", "deploy").with_auth(SshAuth::agent());
        assert!(ok.validate().is_ok());
        assert!(ok.clone().with_port(0).validate().is_err());
        assert!(SshTarget::new("host.example.com", "deploy").validate().is_err(), "no auth");
        assert!(SshTarget::new("", "deploy").with_auth(SshAuth::agent()).validate().is_err());
        assert!(SshTarget::new("-oProxyCommand=x", "deploy").with_auth(SshAuth::agent()).validate().is_err());
        assert!(SshTarget::new("host", "de ploy").with_auth(SshAuth::agent()).validate().is_err());
        let pin = format!("SHA256:{}", "A".repeat(43));
        assert!(ok.clone().trust_host_key_fingerprint(pin).validate().is_ok());
        assert!(ok.clone().trust_host_key_fingerprint("SHA256:short").validate().is_err());
        assert!(SshTarget::from_ssh_config("build").validate().is_ok());
    }

    #[test]
    fn debug_output_never_shows_secrets_or_full_paths() {
        let target = SshTarget::new("host", "deploy")
            .with_auth(SshAuth::key_file_with_passphrase("/home/someone/.ssh/id_ed25519", "fake-pass-1234"))
            .with_known_hosts_file("/home/someone/.ssh/known_hosts");
        let debug = format!("{target:?} {:?}", SshCommand::new("echo fake-secret-arg"));
        assert!(!debug.contains("fake-pass") && !debug.contains("someone") && !debug.contains("fake-secret"), "{debug}");
        assert!(debug.contains("id_ed25519"));
    }

    #[test]
    fn commands_are_checked() {
        assert!(SshCommand::new("  ").validate().is_err());
        assert!(SshCommand::new("a\0b").validate().is_err());
        assert!(matches!(SshCommand::new("x".repeat(MAX_SSH_COMMAND_BYTES + 1)).validate(), Err(Error::RequestTooLarge { .. })));
        let command = SshCommand::new("x").with_timeout(Duration::MAX).with_max_output_bytes(1);
        assert_eq!((command.timeout, command.max_output_bytes), (Some(MAX_TIMEOUT), Some(1024)));
    }
}
