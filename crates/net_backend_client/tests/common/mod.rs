//! The real `net_backend_server` on loopback for the client's tests: SQLite in memory, Auth +
//! Storage + Chat + Leaderboards + Notifications + Friends + Groups + OAuth + Lobbies + Matchmaking (a `duel` queue) + Files, Steam login
//! through a fake verifier ([`STEAM_IDENTITY`], tickets from [`steam_ticket`]; no network), a manual clock (tests move the server's time to expire tokens), an echo request
//! and a slow request. It runs on its own thread with its own runtime, so async and blocking tests
//! can use it alike. Bounded: every wait in the tests has ONE overall deadline.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use net_backend_client::protocol::{CloseCode, ServerPush, UnixMillis, UserId, WsCall};
use net_backend_server::auth::steam::{FakeSteamVerifier, SteamIdentity};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::chat::{Chat, ChatConfig, RoomSpec};
use net_backend_server::files::{Files, FilesConfig};
use net_backend_server::friends::Friends;
use net_backend_server::groups::Groups;
use net_backend_server::leaderboards::{BoardSpec, Leaderboards, LeaderboardsConfig};
use net_backend_server::lobbies::Lobbies;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::matchmaking::{Matchmaking, MatchmakingConfig, QueueSpec};
use net_backend_server::notifications::{NewNotification, NotificationService, Notifications};
use net_backend_server::oauth::{OAuth, OAuthConfig};
use net_backend_server::protocol::admin::BanRequest;
use net_backend_server::storage::{Storage, StorageConfig};
use net_backend_server::{AppState, Config, ManualClock, NetBackendServer, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// The identity string of the test servers' Steam login.
pub const STEAM_IDENTITY: &str = "client-tests";

/// An obviously made-up SteamID64 (account numbers above 4 000 000 000).
pub const fn steam_id(n: u64) -> u64 {
    76_561_197_960_265_728 + 4_000_000_000 + n
}

/// The ticket the test servers' fake Steam verifier accepts for `steam_id(n)` (n = 1 to 8).
pub fn steam_ticket(n: u64) -> String {
    format!("0b{n:02x}")
}

/// Numbers the test servers' file folders.
static FILES_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A password every test account uses.
pub const PASSWORD: &str = "correct horse battery";
/// The upper bound of every wait in the tests (a condition, never a fixed time).
pub const WAIT: Duration = Duration::from_secs(60);
/// The name of every thread of the test server (its runtime's workers and blocking threads too).
pub const SERVER_THREADS: &str = "test-server";

/// A request the test server answers with the caller's id.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Echo {
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Echoed {
    pub text: String,
    pub user: i64,
}

impl WsCall for Echo {
    type Response = Echoed;
    const KIND: &'static str = "test.echo";
}

/// How many `Slow` requests the server started (all tests of a binary share it).
pub static SLOW_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A request the test server answers after `millis`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Slow {
    pub millis: u64,
}

impl WsCall for Slow {
    type Response = net_backend_client::protocol::Ack;
    const KIND: &'static str = "test.slow";
}

/// A server push.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
    pub n: u32,
}

impl ServerPush for Note {
    const KIND: &'static str = "test.note";
}

/// What a test may change before the server starts.
pub struct Setup {
    pub config: Config,
    pub auth: AuthConfig,
    pub storage: StorageConfig,
    /// The OpenID Connect module's settings (no providers by default).
    pub oauth: OAuthConfig,
    /// The files module's settings (a folder under `target/tmp`).
    pub files: FilesConfig,
}

pub struct Server {
    pub state: AppState,
    pub base: String,
    pub clock: Arc<ManualClock>,
    handle: tokio::runtime::Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start() -> Server {
        Self::start_with(|_| {})
    }

    pub fn start_with(tweak: impl FnOnce(&mut Setup) + Send + 'static) -> Server {
        let (ready_sender, ready) = std::sync::mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = std::thread::Builder::new().name(SERVER_THREADS.into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name(SERVER_THREADS).enable_all().build().expect("runtime");
            let handle = runtime.handle().clone();
            runtime.block_on(async move {
                let mut config = Config::default();
                config.database.url = SecretString::new("sqlite::memory:");
                config.database.migrations_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("..")
                    .join("target")
                    .join("tmp")
                    .join(format!("client-tests-{}", std::process::id()));
                config.server.shutdown_grace_secs = 5;
                let mut auth = AuthConfig::default();
                auth.argon2_memory_kib = 64;
                auth.argon2_iterations = 1;
                auth.purge_interval_secs = 0;
                auth.rate_limits = false;
                auth.steam_identity = Some(STEAM_IDENTITY.into());
                let mut files = FilesConfig::default();
                files.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target").join("tmp").join(format!(
                    "client-files-{}-{}",
                    std::process::id(),
                    FILES_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                files.upload_rate = 0;
                let mut setup = Setup { config, auth, storage: StorageConfig::default(), oauth: OAuthConfig::default(), files };
                tweak(&mut setup);
                let mut chat = ChatConfig::default();
                chat.rooms = vec![RoomSpec::new("world").with_name("World")];
                let mut boards = LeaderboardsConfig::default();
                boards.boards = vec![BoardSpec::new("highscore").with_name("High score")];
                let clock = Arc::new(ManualClock::new(UnixMillis::now()));
                let prepared = NetBackendServer::new(setup.config)
                    .clock(Arc::clone(&clock))
                    .module(Auth::new().with_config(setup.auth).mailer(MemoryMailer::new()).steam_verifier({
                        let steam = FakeSteamVerifier::new(STEAM_IDENTITY);
                        for n in 1..=8 {
                            steam.add_ticket(steam_ticket(n), SteamIdentity::new(steam_id(n)));
                        }
                        steam
                    }))
                    .module(Storage::new().with_config(setup.storage))
                    .module(Chat::new().with_config(chat))
                    .module(Leaderboards::new().with_config(boards))
                    .module(Notifications::new())
                    .module(Friends::new())
                    .module(Groups::new())
                    .module(OAuth::new().with_config(setup.oauth))
                    .module(Files::new().with_config(setup.files))
                    .module(Lobbies::new())
                    .module(Matchmaking::new().with_config({
                        let mut matchmaking = MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2));
                        matchmaking.interval_ms = 50;
                        matchmaking
                    }))
                    .ws_call::<Echo, _, _>(|ctx, echo: Echo| async move { Ok(Echoed { text: echo.text, user: ctx.auth.user_id.get() }) })
                    .ws_call::<Slow, _, _>(|_ctx, slow: Slow| async move {
                        SLOW_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(slow.millis)).await;
                        Ok(net_backend_client::protocol::Ack::new())
                    })
                    .build()
                    .await
                    .expect("build");
                prepared.migrate().await.expect("migrate");
                let state = prepared.state().clone();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let base = format!("http://{}", listener.local_addr().expect("addr"));
                let _ = ready_sender.send((state, base, clock, handle));
                let _ = prepared
                    .serve_with_shutdown(listener, async move {
                        let _ = stopped.await;
                    })
                    .await;
            });
            runtime.shutdown_timeout(Duration::from_secs(5));
        });
        let thread = thread.expect("the test server thread");
        let (state, base, clock, handle) = ready.recv_timeout(WAIT).expect("the test server did not start");
        Server { state, base, clock, handle, stop: Some(stop), thread: Some(thread) }
    }

    /// A client for this server.
    pub fn client(&self) -> net_backend_client::Client {
        net_backend_client::Client::new(&self.base).expect("client")
    }

    /// Move the server's clock forward.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(i64::try_from(by.as_millis()).unwrap_or(i64::MAX));
    }

    /// Run `future` on the server's runtime and wait for it (from async or plain tests).
    pub fn run<T: Send + 'static>(&self, future: impl Future<Output = T> + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(WAIT).expect("the server task did not finish")
    }

    pub fn ban(&self, user: UserId) {
        let state = self.state.clone();
        self.run(async move {
            let service = state.get::<AuthService>().expect("auth");
            service.ban_user(&state, user, BanRequest::new()).await.expect("ban");
        });
    }

    /// Send a notification as server code; its id.
    pub fn notify(&self, user: UserId, kind: &str) -> i64 {
        let state = self.state.clone();
        let kind = kind.to_string();
        self.run(async move {
            let service = state.get::<NotificationService>().expect("notifications");
            service.send(&state, user, NewNotification::new(kind).with_text("hello")).await.expect("send").id.get()
        })
    }

    pub fn push(&self, user: UserId, note: Note) {
        self.state.ws().push_user(user, &note).expect("push");
    }

    pub fn close_user(&self, user: UserId, code: CloseCode) -> usize {
        self.state.ws().close_user(user, code, "test")
    }

    pub fn connections_of(&self, user: UserId) -> usize {
        self.state.ws().connections_of(user).len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A unique test address.
pub fn email(name: &str) -> String {
    format!("{name}@example.com")
}

/// Wait (async) until `check` is true, with ONE overall deadline.
pub async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
