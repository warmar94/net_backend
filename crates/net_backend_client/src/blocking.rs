//! The blocking interface: the same client for programs without an async runtime (game loops of
//! other engines, simple tools). Always compiled.
//!
//! Every [`Client`] (and its clones) shares ONE private thread (`net-backend-client`) that runs a
//! tokio current-thread runtime; the async core runs there. Two ways to use it:
//!
//! - **block:** `client.call(&GetAccount::new())?` waits for the answer;
//! - **poll (game loops):** `client.send(GetAccount::new())` returns a [`Reply`] at once; call
//!   [`Reply::try_take`] every frame until the answer is there. Nothing ever blocks the frame.
//!
//! Blocking methods work on any thread, also tokio's `spawn_blocking` threads (the place for
//! blocking work inside a tokio program). Inside async code use the async [`crate::Client`] (or
//! `.await` a [`Reply`]): a blocking call there stalls that worker until the answer (tokio offers no
//! public way to tell an async worker from a `spawn_blocking` thread, so it is not refused; it never
//! deadlocks or panics, because the client's work runs on its own thread). The only refusal
//! ([`Error::InvalidRequest`]) is a blocking call on the client's own runtime thread (e.g. from an
//! SSH prompt responder), which would wait for itself.
//!
//! ```no_run
//! use net_backend_client::blocking::Client;
//! use net_backend_client::protocol::auth::{GetAccount, LoginRequest};
//!
//! # fn main() -> Result<(), net_backend_client::Error> {
//! let client = Client::new("https://api.example.com")?;
//! client.login(LoginRequest::new("player@example.com", "a long password"))?;
//! let mut me = client.send(GetAccount::new()); // never blocks
//! loop {
//!     // ... one frame of the game ...
//!     if let Some(answer) = me.try_take() {
//!         println!("hello {:?}", answer?.display_name);
//!         break;
//!     }
//! #   std::thread::sleep(std::time::Duration::from_millis(16));
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::Arc;

use net_backend_protocol::auth::{AuthSession, LoginRequest, RegisterRequest, SteamLoginRequest, TokenPair};
use net_backend_protocol::{HttpCall, ServerInfo};

use crate::runtime::{refuse_on_client_thread, RuntimeThread};
use crate::{ClientBuilder, Error, Reply, TokenUpdates};

/// The blocking client. Cheap to clone (clones share the runtime thread, the connection pool and
/// the session). The thread stops when the last clone (and every connection made from it) is
/// dropped.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: crate::Client,
    pub(crate) runtime: Arc<RuntimeThread>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("blocking::Client").field("client", &self.inner).finish()
    }
}

impl Client {
    /// A client for the server at `url` with the default settings; starts the runtime thread.
    pub fn new(url: &str) -> Result<Self, Error> {
        Self::from_builder(crate::Client::builder(url))
    }

    /// A client from async-client settings ([`crate::Client::builder`]).
    pub fn from_builder(builder: ClientBuilder) -> Result<Self, Error> {
        refuse_on_client_thread()?;
        let inner = builder.build()?;
        let runtime = RuntimeThread::start()?;
        Ok(Self { inner, runtime })
    }

    /// The async client behind this one (same session and pool), for code that has a runtime.
    pub fn async_client(&self) -> &crate::Client {
        &self.inner
    }

    /// Any typed call; blocks until the answer (see [`crate::Client::call`]).
    pub fn call<C: HttpCall + Clone + Send + Sync + 'static>(&self, call: &C) -> Result<C::Response, Error>
    where
        C::Response: Send + 'static,
    {
        let client = self.inner.clone();
        let call = call.clone();
        self.runtime.block(async move { client.call(&call).await })
    }

    /// Any typed call, without blocking: the answer arrives in the [`Reply`].
    /// [`Reply::cancel`] stops it: answered [`Error::Cancelled`] with `sent: Some(false)` when the
    /// request had not been handed to a connection (it is never sent), `None` after (it may have
    /// reached the server). A token refresh it was waiting for keeps running (it is shared).
    pub fn send<C: HttpCall + Send + Sync + 'static>(&self, call: C) -> Reply<C::Response>
    where
        C::Response: Send + 'static,
    {
        let client = self.inner.clone();
        self.runtime.spawn(async move { client.call(&call).await })
    }

    /// `GET /v1/info`.
    pub fn info(&self) -> Result<ServerInfo, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.info().await })
    }

    /// Create an account and log in (the session keeps the tokens).
    pub fn register(&self, request: RegisterRequest) -> Result<AuthSession, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.register(request).await })
    }

    /// Log in with email + password (the session keeps the tokens).
    pub fn login(&self, request: LoginRequest) -> Result<AuthSession, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.login(request).await })
    }

    /// Log in with a Steam Web API ticket (the session keeps the tokens).
    pub fn login_steam(&self, request: SteamLoginRequest) -> Result<AuthSession, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.login_steam(request).await })
    }

    /// Link Steam to the logged-in account (see [`crate::Client::link_steam`]).
    pub fn link_steam(&self, request: SteamLoginRequest) -> Result<AuthSession, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.link_steam(request).await })
    }

    /// Get a new token pair now.
    pub fn refresh(&self) -> Result<TokenPair, Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.refresh().await })
    }

    /// Log out this session (see [`crate::Client::logout`]).
    pub fn logout(&self) -> Result<(), Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.logout().await })
    }

    /// Log out every session of the account.
    pub fn logout_everywhere(&self) -> Result<(), Error> {
        let client = self.inner.clone();
        self.runtime.block(async move { client.logout_everywhere().await })
    }

    /// Use these tokens (the app's stored session).
    pub fn resume(&self, tokens: TokenPair) {
        self.inner.resume(tokens);
    }

    /// Drop the tokens locally.
    pub fn forget_session(&self) {
        self.inner.forget_session();
    }

    /// The current tokens, if logged in.
    pub fn tokens(&self) -> Option<TokenPair> {
        self.inner.tokens()
    }

    /// Whether there is a session.
    pub fn is_logged_in(&self) -> bool {
        self.inner.is_logged_in()
    }

    /// Every change of the tokens ([`TokenUpdates::try_changed`] works without a runtime).
    pub fn token_updates(&self) -> TokenUpdates {
        self.inner.token_updates()
    }
}

/// A blocking SSH session (feature `ssh`): [`crate::ssh::SshSession`] on a private runtime thread.
/// Cheap to clone.
#[cfg(feature = "ssh")]
#[cfg_attr(docsrs, doc(cfg(feature = "ssh")))]
#[derive(Clone, Debug)]
pub struct SshSession {
    inner: crate::ssh::SshSession,
    runtime: Arc<RuntimeThread>,
}

#[cfg(feature = "ssh")]
impl SshSession {
    /// Connect (see [`crate::ssh::SshSession::connect`]); blocks until connected or refused.
    pub fn connect(target: crate::ssh::SshTarget) -> Result<Self, Error> {
        refuse_on_client_thread()?;
        let runtime = RuntimeThread::start()?;
        let inner = runtime.block(crate::ssh::SshSession::connect(target))?;
        Ok(Self { inner, runtime })
    }

    /// The async session behind this one.
    pub fn async_session(&self) -> &crate::ssh::SshSession {
        &self.inner
    }

    /// The server's host key fingerprint (`SHA256:…`).
    pub fn fingerprint(&self) -> &str {
        self.inner.fingerprint()
    }

    /// Whether the session is gone (see [`crate::ssh::SshSession::is_closed`]).
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// The current state (connected, reconnecting, closed).
    pub fn state(&self) -> crate::ssh::SshState {
        self.inner.state()
    }

    /// What happens to the session; poll it with [`SshEvents::try_next`](crate::ssh::SshEvents::try_next).
    pub fn events(&self) -> crate::ssh::SshEvents {
        self.inner.events()
    }

    /// Run a command and block until it ends (see [`crate::ssh::SshSession::run`]).
    pub fn run(&self, command: impl Into<crate::ssh::SshCommand>) -> Result<crate::ssh::SshOutput, Error> {
        let session = self.inner.clone();
        let command = command.into();
        self.runtime.block(async move { session.run(command).await })
    }

    /// Start a command without blocking; poll its [`SshRun`](crate::ssh::SshRun) with
    /// `try_next_chunk` / `try_finish`. Keep this session (or a clone) alive while it runs: the
    /// runtime thread stops with the last clone.
    pub fn run_streaming(&self, command: impl Into<crate::ssh::SshCommand>) -> crate::ssh::SshRun {
        self.runtime.enter(|| self.inner.run_streaming(command))
    }

    /// Close the connection (blocks briefly).
    pub fn close(&self) {
        let session = self.inner.clone();
        let _ = self.runtime.block(async move {
            session.close().await;
            Ok(())
        });
    }
}

#[cfg(feature = "sftp")]
#[cfg_attr(docsrs, doc(cfg(feature = "sftp")))]
impl SshSession {
    /// Write `data` to `remote` (see [`crate::ssh::SshSession::upload`]).
    pub fn upload(&self, remote: &str, data: impl Into<Vec<u8>>) -> Result<u64, Error> {
        let (session, remote, data) = (self.inner.clone(), remote.to_string(), data.into());
        self.runtime.block(async move { session.upload(&remote, data).await })
    }

    /// Copy the local file `local` to `remote`.
    pub fn upload_file(&self, local: impl AsRef<std::path::Path>, remote: &str) -> Result<u64, Error> {
        let (session, local, remote) = (self.inner.clone(), local.as_ref().to_path_buf(), remote.to_string());
        self.runtime.block(async move { session.upload_file(&local, &remote).await })
    }

    /// Read `remote` into memory.
    pub fn download(&self, remote: &str) -> Result<Vec<u8>, Error> {
        let (session, remote) = (self.inner.clone(), remote.to_string());
        self.runtime.block(async move { session.download(&remote).await })
    }

    /// Copy `remote` to the local file `local`.
    pub fn download_file(&self, remote: &str, local: impl AsRef<std::path::Path>) -> Result<u64, Error> {
        let (session, remote, local) = (self.inner.clone(), remote.to_string(), local.as_ref().to_path_buf());
        self.runtime.block(async move { session.download_file(&remote, &local).await })
    }

    /// Start an upload without blocking; poll its [`SftpTask`](crate::ssh::SftpTask) with
    /// `try_progress` / `try_finish` (see [`crate::ssh::SshSession::start_upload`]). Dropping the
    /// task cancels the transfer. Keep this session (or a clone) alive while it runs.
    pub fn start_upload(&self, remote: &str, data: impl Into<Vec<u8>>) -> crate::ssh::SftpTask<u64> {
        self.runtime.enter(|| self.inner.start_upload(remote, data))
    }

    /// Start copying a local file to `remote` without blocking (see [`start_upload`](Self::start_upload)).
    pub fn start_upload_file(&self, local: impl AsRef<std::path::Path>, remote: &str) -> crate::ssh::SftpTask<u64> {
        self.runtime.enter(|| self.inner.start_upload_file(local, remote))
    }

    /// Start reading `remote` into memory without blocking (see [`start_upload`](Self::start_upload)).
    pub fn start_download(&self, remote: &str) -> crate::ssh::SftpTask<Vec<u8>> {
        self.runtime.enter(|| self.inner.start_download(remote))
    }

    /// Start copying `remote` to the local file `local` without blocking (see
    /// [`start_upload`](Self::start_upload)).
    pub fn start_download_file(&self, remote: &str, local: impl AsRef<std::path::Path>) -> crate::ssh::SftpTask<u64> {
        self.runtime.enter(|| self.inner.start_download_file(remote, local))
    }

    /// List the remote directory `path`.
    pub fn list_dir(&self, path: &str) -> Result<Vec<crate::ssh::SftpEntry>, Error> {
        let (session, path) = (self.inner.clone(), path.to_string());
        self.runtime.block(async move { session.list_dir(&path).await })
    }

    /// Create the remote directory `path`.
    pub fn create_dir(&self, path: &str) -> Result<(), Error> {
        let (session, path) = (self.inner.clone(), path.to_string());
        self.runtime.block(async move { session.create_dir(&path).await })
    }

    /// Remove the remote file `path`.
    pub fn remove_file(&self, path: &str) -> Result<(), Error> {
        let (session, path) = (self.inner.clone(), path.to_string());
        self.runtime.block(async move { session.remove_file(&path).await })
    }

    /// Remove the empty remote directory `path`.
    pub fn remove_dir(&self, path: &str) -> Result<(), Error> {
        let (session, path) = (self.inner.clone(), path.to_string());
        self.runtime.block(async move { session.remove_dir(&path).await })
    }

    /// Rename or move `from` to `to`.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), Error> {
        let (session, from, to) = (self.inner.clone(), from.to_string(), to.to_string());
        self.runtime.block(async move { session.rename(&from, &to).await })
    }
}
