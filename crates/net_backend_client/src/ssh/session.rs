//! [`SshSession`]: one SSH session (russh, ring) on one connection at a time, its commands and
//! (feature `sftp`) its SFTP subsystem. Deadlines: connect + key exchange + host key check +
//! authentication under ONE absolute deadline; each command / SFTP operation under its own. Each
//! connection's socket is wrapped in a kill switch: when the last handle (and the last running
//! command) is gone, the socket is closed, nothing lingers. A supervisor task watches the
//! connection: it reports the state and (with [`SshTarget::with_reconnect`](super::SshTarget::with_reconnect))
//! opens a new connection after a loss.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use net_backend_protocol::Secret;
use russh::client::{self, DisconnectReason, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, Preferred, Sig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Notify, Semaphore};
use tokio::time::Instant;
use zeroize::Zeroizing;

use super::known_hosts::{lookup_name, same_family, KnownHosts, MAX_KNOWN_HOSTS_BYTES};
use super::ssh_config::{home, read_limited, resolve, Resolved};
use super::{file_name, AuthKind, SshCommand, SshExit, SshOutput, SshPrompt, SshPromptRequest, SshStream, SshTarget};
use crate::{Error, Reply, MAX_TIMEOUT};

/// The largest private key file read.
const MAX_KEY_FILE_BYTES: u64 = 256 * 1024;
/// How long a closing connection or channel may take to say goodbye.
const GOODBYE: Duration = Duration::from_secs(1);
/// The most keyboard-interactive rounds answered for one login.
const MAX_PROMPT_ROUNDS: usize = 8;

// ---------------------------------------------------------------------------------------------
// The socket kill switch.

/// A TCP stream that fails every read and write once its owner's `oneshot::Sender` is dropped:
/// the session is gone, so russh's session task must end too.
struct Guarded {
    inner: TcpStream,
    kill: oneshot::Receiver<()>,
    killed: bool,
}

impl Guarded {
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.killed && Pin::new(&mut self.kill).poll(cx).is_ready() {
            self.killed = true;
        }
        if self.killed {
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the SSH session was closed"))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for Guarded {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Guarded {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.killed {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------------------------
// The russh handler: host key check, strict key exchange, loss notification.

/// Why the connection ended, set once by the handler.
#[derive(Default)]
pub(crate) struct Lost {
    reason: Mutex<Option<String>>,
    notify: Notify,
}

impl Lost {
    fn set(&self, reason: String) {
        let mut slot = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.notify.notify_waiters();
    }

    fn get(&self) -> Option<String> {
        self.reason.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[derive(Debug)]
pub(crate) enum HandlerError {
    Russh(russh::Error),
    Refused(Error),
}

impl From<russh::Error> for HandlerError {
    fn from(error: russh::Error) -> Self {
        HandlerError::Russh(error)
    }
}

impl HandlerError {
    fn into_error(self) -> Error {
        match self {
            HandlerError::Refused(error) => error,
            HandlerError::Russh(error) => map_russh(error),
        }
    }
}

/// russh's words, in the matching kind.
fn map_russh(error: russh::Error) -> Error {
    match error {
        russh::Error::IO(e) => Error::network(e.to_string(), None),
        russh::Error::KeepaliveTimeout => Error::timeout("the server stopped answering keepalives", None),
        russh::Error::ConnectionTimeout | russh::Error::InactivityTimeout => Error::timeout(error.to_string(), None),
        russh::Error::Disconnect | russh::Error::HUP => Error::disconnected(error.to_string(), None),
        russh::Error::Version => Error::Ssh("the server did not send a valid SSH banner (is it an SSH server?)".into()),
        other => Error::Ssh(other.to_string()),
    }
}

/// Only printable characters of a server-supplied text, at most 200.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

pub(crate) struct ClientHandler {
    known: Arc<KnownHosts>,
    pinned: Vec<String>,
    allow_terrapin_vulnerable: bool,
    name: String,
    fingerprint: Arc<Mutex<Option<String>>>,
    lost: Arc<Lost>,
}

impl client::Handler for ClientHandler {
    type Error = HandlerError;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => match self.known.verify(&self.name, key, &self.pinned) {
                Ok(fingerprint) => {
                    *self.fingerprint.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
                    Ok(true)
                }
                Err(error) => Err(HandlerError::Refused(error)),
            },
            // Certificates are never negotiated (none are offered); refuse one anyway.
            PublicKeyOrCertificate::Certificate(_) => Err(HandlerError::Refused(Error::Ssh("the server presented a host certificate; not supported".into()))),
        }
    }

    async fn kex_done(&mut self, _shared_secret: Option<&[u8]>, names: &russh::Names, _session: &mut client::Session) -> Result<(), Self::Error> {
        match terrapin_refusal(names.strict_kex(), names.cipher.as_ref(), [names.client_mac.as_ref(), names.server_mac.as_ref()]) {
            Some(error) if !self.allow_terrapin_vulnerable => Err(HandlerError::Refused(error)),
            Some(_) => {
                tracing::warn!(
                    "net_backend_client: ssh: `{}` does not support strict key exchange and uses `{}`: Terrapin-vulnerable, accepted because of allow_terrapin_vulnerable",
                    self.name,
                    names.cipher.as_ref()
                );
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn disconnected(&mut self, reason: DisconnectReason<Self::Error>) -> Result<(), Self::Error> {
        let text = match reason {
            DisconnectReason::ReceivedDisconnect(info) => format!("the server closed the connection ({:?}: {})", info.reason_code, clean(&info.message)),
            DisconnectReason::Error(error) => error.into_error().to_string(),
        };
        self.lost.set(text);
        Ok(())
    }
}

/// Terrapin (CVE-2023-48795): without strict key exchange, ChaCha20-Poly1305 and CBC with an
/// encrypt-then-MAC MAC are practically attackable; AES-GCM and CTR are not. The client prefers
/// AES-GCM, so this only hits a server without strict key exchange that offers none of the safe
/// ciphers. OpenSSH 9.6 and later (and most distributions' backports) support strict key exchange.
fn terrapin_refusal(strict_kex: bool, cipher: &str, macs: [&str; 2]) -> Option<Error> {
    let etm = macs.iter().any(|mac| mac.ends_with("-etm@openssh.com"));
    let exposed = cipher.starts_with("chacha20-poly1305") || (cipher.contains("-cbc") && etm);
    (!strict_kex && exposed).then(|| {
        Error::Ssh(format!(
            "refused: the server does not support strict key exchange and only agreed on `{cipher}`, which is then open to the Terrapin attack (CVE-2023-48795); enable AES-GCM or strict key exchange on the server (OpenSSH 9.6+), or accept the risk with SshTarget::allow_terrapin_vulnerable(true)"
        ))
    })
}

/// The ciphers offered, most preferred first: AES-GCM (never Terrapin-exposed), then
/// ChaCha20-Poly1305 (fine with strict key exchange), then AES-CTR.
const CIPHERS: &[russh::cipher::Name] =
    &[russh::cipher::AES_256_GCM, russh::cipher::CHACHA20_POLY1305, russh::cipher::AES_256_CTR, russh::cipher::AES_192_CTR, russh::cipher::AES_128_CTR];

/// russh's client settings for a target: the keepalive, no inactivity timeout, Nagle off, AES-GCM
/// first, host key algorithms without SHA-1 RSA (and without RSA unless `ssh-rsa`), the types
/// already in known_hosts for this host first (as OpenSSH does).
fn client_config(target: &SshTarget, known_types: &[Algorithm]) -> client::Config {
    let usable: Vec<Algorithm> = Preferred::DEFAULT
        .key
        .iter()
        .filter(|algorithm| match algorithm {
            Algorithm::Rsa { hash } => cfg!(feature = "ssh-rsa") && hash.is_some(),
            _ => true,
        })
        .cloned()
        .collect();
    let (mut keys, rest): (Vec<Algorithm>, Vec<Algorithm>) = usable.into_iter().partition(|a| known_types.iter().any(|k| same_family(k, a)));
    keys.sort_by_key(|a| known_types.iter().position(|k| same_family(k, a)).unwrap_or(usize::MAX));
    keys.extend(rest);
    client::Config {
        keepalive_interval: Some(target.keepalive_interval),
        keepalive_max: usize::try_from(target.keepalive_max).unwrap_or(3),
        inactivity_timeout: None,
        nodelay: true,
        preferred: Preferred { key: Cow::Owned(keys), cipher: Cow::Borrowed(CIPHERS), ..Preferred::DEFAULT },
        ..client::Config::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Connecting.

/// Read the known_hosts files the target names (or `~/.ssh/known_hosts` when it names neither a
/// file nor a pinned fingerprint; a missing default file is just empty).
fn load_known_hosts(target: &SshTarget) -> Result<KnownHosts, Error> {
    let mut known = KnownHosts::default();
    if target.known_hosts.is_empty() && target.pinned.is_empty() {
        if let Some(path) = home().map(|h| h.join(".ssh").join("known_hosts")) {
            if path.exists() {
                known.add(&read_limited(&path, MAX_KNOWN_HOSTS_BYTES, "the known_hosts file")?);
            }
        }
    }
    for path in &target.known_hosts {
        known.add(&read_limited(path, MAX_KNOWN_HOSTS_BYTES, "the known_hosts file")?);
    }
    Ok(known)
}

struct Established {
    handle: Handle<ClientHandler>,
    fingerprint: String,
}

async fn establish(target: &SshTarget, kill: oneshot::Receiver<()>, lost: Arc<Lost>) -> Result<Established, Error> {
    // File I/O (ssh_config with its includes, known_hosts) on the blocking pool.
    let files = target.clone();
    let (resolved, known) = tokio::task::spawn_blocking(move || Ok::<_, Error>((resolve(&files)?, load_known_hosts(&files)?)))
        .await
        .map_err(|e| Error::network(format!("reading the SSH settings failed ({e})"), Some(false)))??;
    let name = lookup_name(&resolved.host, resolved.port);
    let known_types = known.known_algorithms(&name);
    let tcp = TcpStream::connect((resolved.host.as_str(), resolved.port));
    let tcp = match resolved.connect_timeout {
        Some(limit) => tokio::time::timeout(limit, tcp)
            .await
            .map_err(|_| Error::timeout(format!("TCP connect to `{name}` took longer than the ssh_config ConnectTimeout ({limit:?})"), Some(false)))?,
        None => tcp.await,
    }
    .map_err(|e| Error::network(format!("could not connect to `{name}`: {e}"), Some(false)))?;
    let _ = tcp.set_nodelay(true);
    let fingerprint = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        known: Arc::new(known),
        pinned: target.pinned.clone(),
        allow_terrapin_vulnerable: target.allow_terrapin_vulnerable,
        name,
        fingerprint: Arc::clone(&fingerprint),
        lost,
    };
    let stream = Guarded { inner: tcp, kill, killed: false };
    let mut handle = client::connect_stream(Arc::new(client_config(target, &known_types)), stream, handler).await.map_err(HandlerError::into_error)?;
    let fingerprint = fingerprint.lock().unwrap_or_else(PoisonError::into_inner).take();
    let Some(fingerprint) = fingerprint else {
        return Err(Error::Ssh("the server's host key was never checked".into()));
    };
    authenticate(&mut handle, &resolved, target).await?;
    Ok(Established { handle, fingerprint })
}

/// Load and decode a private key file on the blocking pool (decryption is slow on purpose).
async fn load_key(path: PathBuf, passphrase: Option<Secret>) -> Result<PrivateKey, String> {
    let task = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let file = std::fs::File::open(&path).map_err(|e| format!("cannot be read ({e})"))?;
        let mut text = Zeroizing::new(String::new());
        file.take(MAX_KEY_FILE_BYTES.saturating_add(1)).read_to_string(&mut text).map_err(|e| format!("cannot be read ({e})"))?;
        if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_KEY_FILE_BYTES {
            return Err(format!("is larger than {MAX_KEY_FILE_BYTES} bytes"));
        }
        russh::keys::decode_secret_key(&text, passphrase.as_ref().map(Secret::expose)).map_err(|e| match passphrase {
            Some(_) => format!("could not be decoded with the passphrase ({e})"),
            None => format!("could not be decoded ({e}; an encrypted key needs SshAuth::key_file_with_passphrase)"),
        })
    });
    task.await.map_err(|e| format!("could not be loaded ({e})"))?
}

/// The SSH agent of this platform.
async fn connect_agent() -> Result<AgentClient<Box<dyn AgentStream + Send + Unpin>>, String> {
    #[cfg(unix)]
    {
        AgentClient::connect_env().await.map(AgentClient::dynamic).map_err(|e| match e {
            russh::keys::Error::EnvVar(_) => "SSH_AUTH_SOCK is not set".to_string(),
            _ => "the agent socket could not be opened".to_string(),
        })
    }
    #[cfg(windows)]
    {
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            return Ok(agent.dynamic());
        }
        AgentClient::connect_pageant().await.map(AgentClient::dynamic).map_err(|_| "neither the OpenSSH agent nor Pageant is running".to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err("no SSH agent support on this platform".to_string())
    }
}

/// The RSA signature hash to use: SHA-2 only (never SHA-1 `ssh-rsa`).
async fn rsa_hash(handle: &Handle<ClientHandler>) -> Result<Option<HashAlg>, String> {
    match handle.best_supported_rsa_hash().await {
        Ok(Some(Some(hash))) => Ok(Some(hash)),
        Ok(Some(None)) => Err("the server only accepts SHA-1 RSA signatures (ssh-rsa), which are not used".to_string()),
        // The server did not say (no server-sig-algs): every OpenSSH since 7.2 takes SHA-256.
        Ok(None) | Err(_) => Ok(Some(HashAlg::Sha256)),
    }
}

async fn authenticate(handle: &mut Handle<ClientHandler>, resolved: &Resolved, target: &SshTarget) -> Result<(), Error> {
    let mut methods: Vec<AuthKind> = target.auth.iter().map(|a| a.0.clone()).collect();
    methods.extend(resolved.identity_files.iter().map(|path| AuthKind::KeyFile { path: path.clone(), passphrase: None }));
    if methods.is_empty() {
        return Err(Error::AuthFailed(
            "no authentication method (SshAuth::key_file, SshAuth::agent, SshAuth::password, SshAuth::keyboard_interactive, or IdentityFile in ssh_config)"
                .into(),
        ));
    }
    let user = resolved.user.as_str();
    let mut tried: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for method in methods {
        match method {
            AuthKind::KeyFile { path, passphrase } => {
                let label = format!("key file `{}`", file_name(&path));
                let key = match load_key(path, passphrase).await {
                    Ok(key) => key,
                    Err(why) => {
                        problems.push(format!("{label} {why}"));
                        continue;
                    }
                };
                let hash = if key.algorithm().is_rsa() {
                    if !cfg!(feature = "ssh-rsa") {
                        problems.push(format!("{label}: RSA keys need the `ssh-rsa` feature"));
                        continue;
                    }
                    match rsa_hash(handle).await {
                        Ok(hash) => hash,
                        Err(why) => {
                            problems.push(format!("{label}: {why}"));
                            continue;
                        }
                    }
                } else {
                    None
                };
                tried.push(label);
                if handle.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(map_russh)?.success() {
                    return Ok(());
                }
            }
            AuthKind::Agent => {
                let mut agent = match connect_agent().await {
                    Ok(agent) => agent,
                    Err(why) => {
                        problems.push(format!("agent: {why}"));
                        continue;
                    }
                };
                let identities = match agent.request_identities().await {
                    Ok(identities) => identities,
                    Err(e) => {
                        problems.push(format!("agent: could not list its keys ({e})"));
                        continue;
                    }
                };
                let mut offered = 0usize;
                for identity in identities {
                    let AgentIdentity::PublicKey { key, .. } = identity else { continue };
                    let hash = if key.algorithm().is_rsa() {
                        if !cfg!(feature = "ssh-rsa") {
                            continue;
                        }
                        match rsa_hash(handle).await {
                            Ok(hash) => hash,
                            Err(_) => continue,
                        }
                    } else {
                        None
                    };
                    offered = offered.saturating_add(1);
                    match handle.authenticate_publickey_with(user, key, hash, &mut agent).await {
                        Ok(result) if result.success() => return Ok(()),
                        Ok(_) => {}
                        Err(e) => problems.push(format!("agent: signing failed ({e})")),
                    }
                }
                tried.push(format!("agent ({offered} key(s))"));
            }
            AuthKind::Password(password) => {
                tried.push("password".into());
                if handle.authenticate_password(user, password.expose()).await.map_err(map_russh)?.success() {
                    return Ok(());
                }
            }
            AuthKind::KeyboardInteractive(responder) => {
                tried.push("keyboard-interactive".into());
                let mut reply = handle.authenticate_keyboard_interactive_start(user, None::<String>).await.map_err(map_russh)?;
                let mut rounds = 0usize;
                loop {
                    match reply {
                        KeyboardInteractiveAuthResponse::Success => return Ok(()),
                        KeyboardInteractiveAuthResponse::Failure { .. } => break,
                        KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                            rounds = rounds.saturating_add(1);
                            if rounds > MAX_PROMPT_ROUNDS {
                                problems.push(format!("keyboard-interactive: more than {MAX_PROMPT_ROUNDS} rounds of prompts"));
                                break;
                            }
                            let request = SshPromptRequest {
                                name: clean(&name),
                                instructions: clean(&instructions),
                                prompts: prompts.into_iter().map(|p| SshPrompt { text: clean(&p.prompt), echo: p.echo }).collect(),
                            };
                            let wanted = request.prompts.len();
                            let answers = match responder.respond(&request) {
                                Some(answers) if answers.len() == wanted => answers,
                                Some(_) => {
                                    problems.push("keyboard-interactive: the responder gave a wrong number of answers".into());
                                    break;
                                }
                                None => {
                                    problems.push(format!("keyboard-interactive: no answer for the server's {wanted} prompt(s)"));
                                    break;
                                }
                            };
                            // russh takes plain strings; they live only for this call.
                            let answers: Vec<String> = answers.iter().map(|a| a.expose().to_string()).collect();
                            reply = handle.authenticate_keyboard_interactive_respond(answers).await.map_err(map_russh)?;
                        }
                    }
                }
            }
        }
    }
    let mut why = if tried.is_empty() { "no method could be offered".to_string() } else { format!("the server accepted none of: {}", tried.join(", ")) };
    if !problems.is_empty() {
        why.push_str("; ");
        why.push_str(&problems.join("; "));
    }
    Err(Error::AuthFailed(why))
}

// ---------------------------------------------------------------------------------------------
// The session.

/// One established connection (one TCP socket). A reconnect makes a new one; commands keep the
/// one they started on.
pub(crate) struct Link {
    pub(crate) handle: Handle<ClientHandler>,
    lost: Arc<Lost>,
    /// The SFTP subsystem of this connection, opened on first use.
    #[cfg(feature = "sftp")]
    pub(crate) sftp: tokio::sync::Mutex<Option<Arc<russh_sftp::client::RawSftpSession>>>,
    /// Dropping it (the session and every command on this connection are gone) kills the socket.
    _kill: oneshot::Sender<()>,
}

impl Link {
    /// Whether this connection is gone (lost, or closed).
    pub(crate) fn is_gone(&self) -> bool {
        self.handle.is_closed() || self.lost.get().is_some()
    }

    /// Why it is gone.
    pub(crate) fn reason(&self) -> String {
        self.lost.get().unwrap_or_else(|| "the connection was lost".to_string())
    }

    /// Resolves once this connection is gone (lost, or closed): at once when the transport reports
    /// it, else within [`WATCH_EVERY`] (a closed handle sends no wake-up).
    #[cfg(feature = "sftp")]
    pub(crate) async fn gone(&self) {
        loop {
            let notified = self.lost.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_gone() {
                return;
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(WATCH_EVERY) => {}
            }
        }
    }
}

/// Where the session is: on a connection, waiting to reconnect, connecting again, or closed.
#[derive(Clone)]
pub(crate) enum Conn {
    Up(Arc<Link>),
    Reconnecting { attempt: u32, retry_in: Duration },
    Connecting,
    Closed,
}

pub(crate) struct Inner {
    fingerprint: String,
    pub(crate) channels: Arc<Semaphore>,
    pub(crate) target: SshTarget,
    /// The current connection (the supervisor holds the sender).
    conn: watch::Receiver<Conn>,
    /// A template for event streams (the supervisor holds the sender: streams end with it).
    events: Mutex<broadcast::Receiver<SshEvent>>,
    /// Set by [`SshSession::close`]: the next loss is the app's close, never reconnected.
    closed_by_app: AtomicBool,
    close_requested: Notify,
    /// Dropping it (the last handle and the last running command are gone) stops the supervisor.
    _stop: oneshot::Sender<()>,
}

/// The state of an SSH session ([`SshSession::state`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshState {
    /// Connected and authenticated: commands run.
    Connected,
    /// The connection was lost and [`SshTarget::with_reconnect`] is set: the next attempt starts
    /// after `retry_in`. New commands wait for it; none is re-run.
    Reconnecting {
        /// The number of the next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts.
        retry_in: Duration,
    },
    /// Connecting again (the attempt is running).
    Connecting,
    /// Closed for good (by the app, a loss without reconnect, out of attempts, or an error a
    /// reconnect cannot fix): see the last [`SshEvent::Closed`].
    Closed,
}

/// What happened to an SSH session ([`SshSession::events`]).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SshEvent {
    /// Connected again after a loss (`reconnected` is `true`: the first connection is the one
    /// [`SshSession::connect`] returned). Commands that waited go out now.
    Connected {
        /// Whether this is a reconnect.
        reconnected: bool,
    },
    /// The connection was lost (or an attempt failed); the next attempt starts after `retry_in`.
    Reconnecting {
        /// The number of the next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts.
        retry_in: Duration,
        /// Why the connection was lost or the attempt failed.
        error: Error,
    },
    /// Closed for good: `None` = closed by the app; otherwise why (the loss, or the last
    /// attempt's error). The last event.
    Closed {
        /// Why, unless the app closed it.
        error: Option<Error>,
    },
}

/// A stream of [`SshEvent`]s ([`SshSession::events`]).
pub struct SshEvents {
    receiver: broadcast::Receiver<SshEvent>,
}

impl fmt::Debug for SshEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SshEvents")
    }
}

impl SshEvents {
    /// The next event; `None` after the session closed for good (its `Closed` event was delivered
    /// first). A reader that fell behind skips the oldest events.
    pub async fn next(&mut self) -> Option<SshEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// The next event if one is buffered (never blocks; no runtime needed).
    pub fn try_next(&mut self) -> Option<SshEvent> {
        loop {
            match self.receiver.try_recv() {
                Ok(event) => return Some(event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return None,
            }
        }
    }
}

/// One SSH session to a server (feature `ssh`). Cheap to clone (clones share it). Commands run in
/// parallel on their own channels (up to [`SshTarget::with_max_channels`]). The session closes
/// when [`close`](Self::close) is called, or when the last clone and the last running command are
/// dropped. A lost connection ends the session, unless [`SshTarget::with_reconnect`] is set: then
/// a new connection is opened (see [`SshReconnect`](super::SshReconnect)).
#[derive(Clone)]
pub struct SshSession {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for SshSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshSession").field("host", &self.inner.target.host).field("fingerprint", &self.inner.fingerprint).field("state", &self.state()).finish()
    }
}

/// How many events each event stream buffers.
const EVENT_BUFFER: usize = 64;
/// How often the supervisor also looks at the connection (besides the loss notice).
const WATCH_EVERY: Duration = Duration::from_millis(200);

/// Connect once (TCP, key exchange, host key, login) under the target's connect timeout.
async fn open_link(target: &SshTarget) -> Result<(Arc<Link>, String), Error> {
    let deadline = deadline_after(target.connect_timeout);
    let (kill_switch, kill) = oneshot::channel::<()>();
    let lost = Arc::new(Lost::default());
    let established = tokio::time::timeout_at(deadline, establish(target, kill, Arc::clone(&lost))).await;
    let Established { handle, fingerprint } = match established {
        Err(_) => {
            let limit = target.connect_timeout;
            return Err(Error::timeout(
                format!("not connected within {limit:?} (TCP connect, key exchange, host key check and authentication together)"),
                None,
            ));
        }
        Ok(result) => result?,
    };
    let link = Link {
        handle,
        lost,
        #[cfg(feature = "sftp")]
        sftp: tokio::sync::Mutex::new(None),
        _kill: kill_switch,
    };
    Ok((Arc::new(link), fingerprint))
}

impl SshSession {
    /// Connect, check the host key, authenticate: all under the target's connect timeout. Refused in
    /// a release build unless [`SshTarget::allow_in_release`]. Needs a tokio runtime (without one:
    /// [`blocking::SshSession`](crate::blocking::SshSession)). An error here leaves nothing
    /// running (also with [`SshTarget::with_reconnect`]: reconnects follow a lost connection).
    pub async fn connect(target: SshTarget) -> Result<Self, Error> {
        let runtime = crate::runtime::current()?;
        if !target.is_allowed() {
            return Err(Error::invalid("SSH is disabled in release builds (an SSH key in a shipped program is shell access for anyone); SshTarget::allow_in_release(true) for internal admin tools"));
        }
        target.validate()?;
        let (link, fingerprint) = open_link(&target).await?;
        tracing::debug!("net_backend_client: ssh connected to `{}` ({fingerprint})", target.host);
        let (conn_sender, conn) = watch::channel(Conn::Up(link));
        let (events_sender, events) = broadcast::channel(EVENT_BUFFER);
        let (stop, stopped) = oneshot::channel();
        let inner = Arc::new(Inner {
            fingerprint,
            channels: Arc::new(Semaphore::new(target.max_channels)),
            target,
            conn,
            events: Mutex::new(events),
            closed_by_app: AtomicBool::new(false),
            close_requested: Notify::new(),
            _stop: stop,
        });
        runtime.spawn(supervise(Arc::downgrade(&inner), stopped, conn_sender, events_sender));
        Ok(Self { inner })
    }

    /// The server's host key fingerprint (`SHA256:…`) as checked when the session connected (a
    /// reconnect checks the host key again, by the same rules).
    pub fn fingerprint(&self) -> &str {
        &self.inner.fingerprint
    }

    /// Whether the session is gone: closed for good, or (without reconnect) its connection was lost.
    pub fn is_closed(&self) -> bool {
        match &*self.inner.conn.borrow() {
            Conn::Closed => true,
            Conn::Up(link) => link.is_gone() && self.inner.target.reconnect.is_none(),
            Conn::Reconnecting { .. } | Conn::Connecting => false,
        }
    }

    /// The current state.
    pub fn state(&self) -> SshState {
        match &*self.inner.conn.borrow() {
            Conn::Up(link) if link.is_gone() && self.inner.target.reconnect.is_none() => SshState::Closed,
            Conn::Up(_) => SshState::Connected,
            Conn::Reconnecting { attempt, retry_in } => SshState::Reconnecting { attempt: *attempt, retry_in: *retry_in },
            Conn::Connecting => SshState::Connecting,
            Conn::Closed => SshState::Closed,
        }
    }

    /// What happens to the session (reconnecting, connected again, closed). Only events after this
    /// call.
    pub fn events(&self) -> SshEvents {
        SshEvents { receiver: self.inner.events.lock().unwrap_or_else(PoisonError::into_inner).resubscribe() }
    }

    /// Wait until the session is closed for good (async).
    pub async fn closed(&self) {
        let mut conn = self.inner.conn.clone();
        let _ = conn.wait_for(|c| matches!(c, Conn::Closed)).await;
    }

    /// Run a command and collect its whole output (stdout + stderr up to the output limit). A
    /// non-zero exit status is still `Ok` (check [`SshOutput::success`]). Errors: `Timeout`
    /// (`sent` says whether it may have started), `BodyTooLarge` (output limit; the command was
    /// stopped), `RequestTooLarge` (a command line over 64 KiB), `Disconnected`, `Ssh`, …
    /// Dropping the future stops the command (a `TERM` signal, then the channel is closed).
    pub async fn run(&self, command: impl Into<SshCommand>) -> Result<SshOutput, Error> {
        let mut run = self.run_streaming(command);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        while let Some(chunk) = run.next_chunk().await {
            match chunk.stream {
                SshStream::Stdout => stdout.extend_from_slice(&chunk.data),
                SshStream::Stderr => stderr.extend_from_slice(&chunk.data),
            }
        }
        let exit = run.finish().await?;
        Ok(SshOutput { exit, stdout, stderr })
    }

    /// Start a command and stream its output as it arrives ([`SshRun`]). Must be called inside a
    /// tokio runtime (otherwise the run ends at once with `InvalidRequest`).
    pub fn run_streaming(&self, command: impl Into<SshCommand>) -> SshRun {
        let command = command.into();
        let (chunks_sender, chunks) = mpsc::unbounded_channel();
        let (exit_sender, exit) = Reply::channel();
        let (cancel, cancelled) = oneshot::channel();
        let start = command.validate().and_then(|()| crate::runtime::current());
        match start {
            Ok(runtime) => {
                runtime.spawn(exec(Arc::clone(&self.inner), command, chunks_sender, exit_sender, cancelled));
            }
            Err(error) => {
                let _ = exit_sender.send(Err(error));
            }
        }
        SshRun { chunks, exit, _cancel: cancel }
    }

    /// Close the session (politely, bounded), also while it reconnects; running commands end with
    /// `Disconnected`. Never reconnected.
    pub async fn close(&self) {
        self.inner.closed_by_app.store(true, Ordering::SeqCst);
        self.inner.close_requested.notify_one();
        let link = match &*self.inner.conn.borrow() {
            Conn::Up(link) => Some(Arc::clone(link)),
            _ => None,
        };
        if let Some(link) = link {
            let _ = tokio::time::timeout(GOODBYE, link.handle.disconnect(Disconnect::ByApplication, "", "en")).await;
            link.lost.set("the session was closed by the app".into());
        }
        let mut conn = self.inner.conn.clone();
        let _ = tokio::time::timeout(GOODBYE, conn.wait_for(|c| matches!(c, Conn::Closed))).await;
    }
}

/// The connection to use now: the current one, or (while reconnecting) the next one, bounded by
/// `deadline`; cancelled when `cancel` fires.
pub(crate) async fn current_link(inner: &Inner, deadline: Instant, cancel: &mut oneshot::Receiver<()>) -> Race<Result<Arc<Link>, Error>> {
    let mut conn = inner.conn.clone();
    loop {
        let now = match &*conn.borrow_and_update() {
            Conn::Up(link) if !link.is_gone() => Some(Ok(Arc::clone(link))),
            Conn::Up(link) if inner.target.reconnect.is_none() => Some(Err(Error::disconnected(link.reason(), Some(false)))),
            Conn::Closed => Some(Err(Error::disconnected("the SSH session is closed", Some(false)))),
            _ => None,
        };
        if let Some(result) = now {
            return Race::Done(result);
        }
        match race(conn.changed(), deadline, cancel).await {
            Race::Done(Ok(())) => {}
            Race::Done(Err(_)) => return Race::Done(Err(Error::disconnected("the SSH session is closed", Some(false)))),
            Race::TimedOut => return Race::TimedOut,
            Race::Cancelled => return Race::Cancelled,
        }
    }
}

/// Errors a reconnect cannot fix (the same rules as `bevy_net_backend`).
fn is_permanent(error: &Error) -> bool {
    matches!(error, Error::HostKey { .. } | Error::AuthFailed(_) | Error::Ssh(_) | Error::InvalidRequest(_))
}

/// The supervisor: waits for the connection to be lost, then reconnects (if set) or closes. Ends
/// when the session is closed for good or every handle is gone (`stop`).
async fn supervise(inner: Weak<Inner>, mut stop: oneshot::Receiver<()>, conn: watch::Sender<Conn>, events: broadcast::Sender<SshEvent>) {
    let finish = |error: Option<Error>| {
        if let Some(error) = &error {
            tracing::info!("net_backend_client: the SSH session closed for good ({error})");
        }
        conn.send_replace(Conn::Closed);
        let _ = events.send(SshEvent::Closed { error });
    };
    let mut attempt: u32 = 0;
    let mut up_since = Instant::now();
    loop {
        // 1. Wait until the current connection is gone.
        let Conn::Up(link) = conn.borrow().clone() else { return };
        loop {
            let Some(session) = inner.upgrade() else { return };
            let notified = link.lost.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if link.is_gone() {
                break;
            }
            drop(session);
            tokio::select! {
                _ = &mut stop => return,
                () = notified => {}
                () = tokio::time::sleep(WATCH_EVERY) => {}
            }
        }
        let Some(session) = inner.upgrade() else { return };
        if session.closed_by_app.load(Ordering::SeqCst) {
            return finish(None);
        }
        let lost = Error::disconnected(link.reason(), None);
        drop(link);
        let Some(policy) = session.target.reconnect.clone() else { return finish(Some(lost)) };
        tracing::debug!("net_backend_client: ssh `{}` lost ({lost}); reconnecting", session.target.host);
        if up_since.elapsed() >= policy.stable_after() {
            attempt = 0;
        }
        // 2. Reconnect with backoff until a connection is up or the answer is final.
        let mut last = lost;
        loop {
            attempt = attempt.saturating_add(1);
            if !policy.may_retry(attempt) {
                return finish(Some(last));
            }
            let delay = policy.delay(attempt, crate::tls::random_u64());
            conn.send_replace(Conn::Reconnecting { attempt, retry_in: delay });
            let _ = events.send(SshEvent::Reconnecting { attempt, retry_in: delay, error: last.clone() });
            tokio::select! {
                _ = &mut stop => return,
                () = session.close_requested.notified() => return finish(None),
                () = tokio::time::sleep(delay) => {}
            }
            conn.send_replace(Conn::Connecting);
            let target = session.target.clone();
            let opened = tokio::select! {
                _ = &mut stop => return,
                () = session.close_requested.notified() => return finish(None),
                opened = open_link(&target) => opened,
            };
            match opened {
                Ok((link, fingerprint)) => {
                    tracing::debug!("net_backend_client: ssh reconnected to `{}` ({fingerprint})", target.host);
                    if session.closed_by_app.load(Ordering::SeqCst) {
                        return finish(None);
                    }
                    conn.send_replace(Conn::Up(link));
                    let _ = events.send(SshEvent::Connected { reconnected: true });
                    up_since = Instant::now();
                    break;
                }
                Err(error) if is_permanent(&error) => return finish(Some(error)),
                Err(error) => last = error,
            }
        }
        drop(session);
    }
}

pub(crate) fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout.min(MAX_TIMEOUT)).unwrap_or(now)
}

/// One chunk of a running command's output, as it arrived (a chunk may end in the middle of a line
/// or of a UTF-8 character: join chunks before splitting lines). `Debug` shows the length only.
#[derive(Clone)]
#[non_exhaustive]
pub struct SshChunk {
    /// stdout or stderr.
    pub stream: SshStream,
    /// The bytes.
    pub data: Vec<u8>,
}

impl fmt::Debug for SshChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshChunk").field("stream", &self.stream).field("bytes", &self.data.len()).finish()
    }
}

impl SshChunk {
    /// The bytes as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// A running command ([`SshSession::run_streaming`]): its output chunks, then exactly one exit (or
/// error). Dropping it stops the command (a `TERM` signal, then its channel is closed; the remote
/// process may keep running). Works from a game loop too ([`try_next_chunk`](Self::try_next_chunk),
/// [`try_finish`](Self::try_finish)).
pub struct SshRun {
    chunks: mpsc::UnboundedReceiver<SshChunk>,
    exit: Reply<SshExit>,
    _cancel: oneshot::Sender<()>,
}

impl fmt::Debug for SshRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshRun").field("finished", &self.exit.is_taken()).finish()
    }
}

impl SshRun {
    /// The next output chunk; `None` once the command produced its last one.
    pub async fn next_chunk(&mut self) -> Option<SshChunk> {
        self.chunks.recv().await
    }

    /// The next output chunk if one arrived (never blocks; no runtime needed).
    pub fn try_next_chunk(&mut self) -> Option<SshChunk> {
        self.chunks.try_recv().ok()
    }

    /// Wait for the end: the exit status, or why it did not run to its end. Chunks not read yet
    /// are dropped.
    pub async fn finish(self) -> Result<SshExit, Error> {
        let SshRun { exit, _cancel, .. } = self;
        let result = exit.await;
        drop(_cancel);
        result
    }

    /// The end, if it came (never blocks; read the chunks first: they all arrive before it).
    pub fn try_finish(&mut self) -> Option<Result<SshExit, Error>> {
        self.exit.try_take()
    }
}

// ---------------------------------------------------------------------------------------------
// Requests.

pub(crate) enum Race<T> {
    Done(T),
    TimedOut,
    Cancelled,
}

/// Run `future` until it finishes, the deadline passes, or the request is cancelled (its handle
/// dropped). After `Cancelled`, `cancel` must not be polled again.
pub(crate) async fn race<F: Future>(future: F, deadline: Instant, cancel: &mut oneshot::Receiver<()>) -> Race<F::Output> {
    tokio::select! {
        biased;
        _ = cancel => Race::Cancelled,
        () = tokio::time::sleep_until(deadline) => Race::TimedOut,
        output = future => Race::Done(output),
    }
}

/// Open a session channel under the request's deadline without leaking it: when the request is
/// cancelled or times out while the server is still opening it, a small task waits (bounded) for
/// the channel and closes it, so the server's channel slots (`MaxSessions`) are not used up.
async fn open_channel(link: &Arc<Link>, deadline: Instant, cancel: &mut oneshot::Receiver<()>) -> Race<Result<russh::Channel<client::Msg>, russh::Error>> {
    let owner = Arc::clone(link);
    let mut opening = tokio::spawn(async move { owner.handle.channel_open_session().await });
    match race(&mut opening, deadline, cancel).await {
        Race::Done(Ok(result)) => Race::Done(result),
        Race::Done(Err(join)) => Race::Done(Err(russh::Error::IO(io::Error::other(join.to_string())))),
        other => {
            tokio::spawn(async move {
                if let Ok(Ok(Ok(channel))) = tokio::time::timeout(GOODBYE.saturating_mul(5), opening).await {
                    let _ = tokio::time::timeout(GOODBYE, channel.close()).await;
                }
            });
            match other {
                Race::TimedOut => Race::TimedOut,
                _ => Race::Cancelled,
            }
        }
    }
}

/// Best effort: ask the remote process to stop and close the channel (bounded).
async fn stop_channel(channel: &russh::Channel<client::Msg>) {
    let _ = tokio::time::timeout(GOODBYE, async {
        let _ = channel.signal(Sig::TERM).await;
        let _ = channel.eof().await;
        let _ = channel.close().await;
    })
    .await;
}

fn signal_name(signal: &Sig) -> String {
    match signal {
        Sig::Custom(name) => clean(name),
        other => format!("{other:?}"),
    }
}

async fn exec(
    inner: Arc<Inner>,
    command: SshCommand,
    chunks: mpsc::UnboundedSender<SshChunk>,
    exit: oneshot::Sender<Result<SshExit, Error>>,
    mut cancel: oneshot::Receiver<()>,
) {
    let timeout = command.timeout.unwrap_or(inner.target.command_timeout);
    let limit = command.max_output_bytes.unwrap_or(inner.target.max_output_bytes);
    let deadline = deadline_after(timeout);
    let finish = |result| {
        let _ = exit.send(result);
    };
    // 1. A free channel slot, a connection (waiting while it reconnects), then a channel: nothing
    // is sent to the shell yet.
    let _permit = match race(Arc::clone(&inner.channels).acquire_owned(), deadline, &mut cancel).await {
        Race::Done(Ok(permit)) => permit,
        Race::Done(Err(_)) => return finish(Err(Error::disconnected("the session is closing", Some(false)))),
        Race::TimedOut => return finish(Err(Error::timeout(format!("not sent: no free channel within {timeout:?}"), Some(false)))),
        Race::Cancelled => return,
    };
    let (link, mut channel) = loop {
        let link = match current_link(&inner, deadline, &mut cancel).await {
            Race::Done(Ok(link)) => link,
            Race::Done(Err(error)) => return finish(Err(error)),
            Race::TimedOut => return finish(Err(Error::timeout(format!("not sent: not connected within {timeout:?}"), Some(false)))),
            Race::Cancelled => return,
        };
        match open_channel(&link, deadline, &mut cancel).await {
            Race::Done(Ok(channel)) => break (link, channel),
            // The connection died under it: with reconnect, wait for the next one.
            Race::Done(Err(_)) if link.is_gone() && inner.target.reconnect.is_some() => {}
            Race::Done(Err(e)) if link.is_gone() => return finish(Err(Error::disconnected(format!("could not open a channel: {e}"), Some(false)))),
            Race::Done(Err(e)) => return finish(Err(Error::Ssh(format!("could not open a channel: {e}")))),
            Race::TimedOut => return finish(Err(Error::timeout(format!("not sent: the server did not open a channel within {timeout:?}"), Some(false)))),
            Race::Cancelled => return,
        }
    };
    // 2. The exec request.
    match race(channel.exec(true, command.command.as_bytes()), deadline, &mut cancel).await {
        Race::Done(Ok(())) => {}
        Race::Done(Err(e)) => return finish(Err(Error::Ssh(format!("could not send the command: {e}")))),
        Race::TimedOut => {
            stop_channel(&channel).await;
            return finish(Err(Error::timeout(format!("the exec request could not be written within {timeout:?}; the command may have started"), None)));
        }
        Race::Cancelled => {
            stop_channel(&channel).await;
            return;
        }
    }
    // 3. Its answer, the output, the exit.
    let mut started = false;
    let mut gone = false;
    let (mut stdout, mut stderr) = (0u64, 0u64);
    let (mut status, mut signal) = (None, None);
    let mut stdin = command.stdin;
    loop {
        let message = match race(channel.wait(), deadline, &mut cancel).await {
            Race::Done(message) => message,
            Race::TimedOut => {
                stop_channel(&channel).await;
                let (why, sent) = if started {
                    (
                        format!("the command ran longer than {timeout:?}; its channel was closed after a TERM signal (the remote process may keep running)"),
                        Some(true),
                    )
                } else {
                    (format!("the server did not answer the exec request within {timeout:?}; the command may have started"), None)
                };
                return finish(Err(Error::timeout(why, sent)));
            }
            Race::Cancelled => {
                stop_channel(&channel).await;
                return;
            }
        };
        let runs = matches!(
            message,
            Some(
                ChannelMsg::Success | ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. } | ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. }
            )
        );
        if runs && !started {
            started = true;
            // stdin, then end-of-file (a command reading stdin must not wait forever).
            let input = stdin.take();
            let write = async {
                if let Some(input) = input {
                    channel.data(input.as_slice()).await?;
                }
                channel.eof().await
            };
            match race(write, deadline, &mut cancel).await {
                Race::Done(_) => {}
                Race::TimedOut => {
                    stop_channel(&channel).await;
                    return finish(Err(Error::timeout(format!("stdin could not be written within {timeout:?}; the command was started"), Some(true))));
                }
                Race::Cancelled => {
                    stop_channel(&channel).await;
                    return;
                }
            }
        }
        match message {
            Some(ChannelMsg::Failure) if !started => {
                stop_channel(&channel).await;
                return finish(Err(Error::Ssh("the server refused to run the command".into())));
            }
            Some(ChannelMsg::Data { data }) => {
                stdout = stdout.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if stdout.saturating_add(stderr) > limit {
                    stop_channel(&channel).await;
                    return finish(Err(Error::BodyTooLarge { limit }));
                }
                let _ = chunks.send(SshChunk { stream: SshStream::Stdout, data: data.to_vec() });
            }
            Some(ChannelMsg::ExtendedData { data, ext }) => {
                stderr = stderr.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if stdout.saturating_add(stderr) > limit {
                    stop_channel(&channel).await;
                    return finish(Err(Error::BodyTooLarge { limit }));
                }
                if ext == 1 {
                    let _ = chunks.send(SshChunk { stream: SshStream::Stderr, data: data.to_vec() });
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name: sig, .. }) => signal = Some(signal_name(&sig)),
            Some(ChannelMsg::Close) => break,
            // The channel's sender is gone: the session ended under it.
            None => {
                gone = true;
                break;
            }
            Some(_) => {}
        }
    }
    let _ = tokio::time::timeout(GOODBYE, channel.close()).await;
    if status.is_none() && signal.is_none() && (gone || link.handle.is_closed()) {
        return finish(Err(Error::disconnected("the connection was lost before the command ended", Some(true))));
    }
    drop(chunks);
    finish(Ok(SshExit { status, signal, stdout_bytes: stdout, stderr_bytes: stderr }));
}

#[cfg(test)]
mod tests {
    use russh::keys::Algorithm;

    use super::{client_config, terrapin_refusal};
    use crate::ssh::{SshAuth, SshTarget};

    #[test]
    fn terrapin_exposed_combinations_need_strict_kex() {
        let plain = ["hmac-sha2-256", "hmac-sha2-256"];
        let etm = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-256-etm@openssh.com"];
        assert!(terrapin_refusal(false, "chacha20-poly1305@openssh.com", plain).is_some());
        assert!(terrapin_refusal(false, "aes256-cbc", etm).is_some());
        assert!(terrapin_refusal(false, "aes256-cbc", plain).is_none());
        assert!(terrapin_refusal(false, "aes256-ctr", etm).is_none(), "CTR-EtM is not practically exploitable");
        assert!(terrapin_refusal(false, "aes256-gcm@openssh.com", etm).is_none());
        assert!(terrapin_refusal(true, "chacha20-poly1305@openssh.com", etm).is_none());
        let why = terrapin_refusal(false, "chacha20-poly1305@openssh.com", plain).map(|e| e.to_string()).unwrap_or_default();
        assert!(why.contains("Terrapin") && why.contains("allow_terrapin_vulnerable"), "{why}");
    }

    #[test]
    fn aes_gcm_is_preferred_and_known_key_types_come_first() {
        let target = SshTarget::new("h", "u").with_auth(SshAuth::agent());
        let config = client_config(&target, &[]);
        assert_eq!(config.preferred.cipher.first().map(AsRef::as_ref), Some("aes256-gcm@openssh.com"));
        assert_eq!(config.preferred.key.first(), Some(&Algorithm::Ed25519));
        let p256 = Algorithm::Ecdsa { curve: russh::keys::EcdsaCurve::NistP256 };
        let config = client_config(&target, std::slice::from_ref(&p256));
        assert_eq!(config.preferred.key.first(), Some(&p256));
    }

    #[test]
    fn the_client_offers_no_sha1_rsa_and_rsa_only_with_the_feature() {
        let config = client_config(&SshTarget::new("h", "u").with_auth(SshAuth::agent()), &[]);
        let keys = config.preferred.key.to_vec();
        assert!(!keys.contains(&Algorithm::Rsa { hash: None }));
        assert_eq!(keys.iter().any(|k| matches!(k, Algorithm::Rsa { .. })), cfg!(feature = "ssh-rsa"));
        assert!(config.preferred.kex.iter().any(|k| k.as_ref() == "kex-strict-c-v00@openssh.com"), "strict key exchange is offered");
        assert_eq!(config.inactivity_timeout, None);
    }
}
