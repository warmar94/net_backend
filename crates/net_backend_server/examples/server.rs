//! The reference server: the framework with every module (`Auth`, `Storage`, `Chat`,
//! `Leaderboards`, `Notifications`, `Friends`, `Groups`, `OAuth`, `Lobbies`, `Matchmaking`, `Files`), configured entirely from `config.toml` +
//! `NBS__*` variables. It is the binary the deployment files in the
//! repository's `deploy/` folder install (Docker Compose or systemd); a game's own server binary
//! starts from this file and adds its routes, hooks and WebSocket handlers.
//!
//! ```text
//! cargo build --release -p net_backend_server --example server --all-features
//! # or only what a deployment uses, e.g. MySQL + mail:
//! cargo build --release -p net_backend_server --example server --no-default-features --features mysql,storage,chat,leaderboards,notifications,friends,groups,oauth,lobbies,matchmaking,files,smtp
//!
//! NBS_CONFIG=/etc/net-backend/config.toml server migrate        # apply migrations (a deploy step)
//! NBS_CONFIG=/etc/net-backend/config.toml server                # serve until SIGTERM / Ctrl-C
//! NBS_CONFIG=/etc/net-backend/config.toml server config check --connect
//! NBS_CONFIG=/etc/net-backend/config.toml server user:create admin@example.com --admin
//! NBS_CONFIG=/etc/net-backend/config.toml server healthcheck    # exit 0 when /readyz answers 200
//! ```
//!
//! Every command of the framework's command line works (`--help` lists them), `healthcheck`
//! included: container images without a shell or `curl` use it as their health check.

use std::process::ExitCode;

use net_backend_server::chat::Chat;
use net_backend_server::files::Files;
use net_backend_server::friends::Friends;
use net_backend_server::groups::Groups;
use net_backend_server::leaderboards::Leaderboards;
use net_backend_server::lobbies::Lobbies;
use net_backend_server::matchmaking::Matchmaking;
use net_backend_server::notifications::Notifications;
use net_backend_server::oauth::OAuth;
use net_backend_server::storage::Storage;
use net_backend_server::{Auth, NetBackendServer};

#[tokio::main]
async fn main() -> ExitCode {
    // `--help` and `--version` work without a configuration; everything else loads it.
    NetBackendServer::run_main(|config| {
        NetBackendServer::new(config)
            .module(Auth::new())
            .module(Storage::new())
            .module(Chat::new())
            .module(Leaderboards::new())
            .module(Notifications::new())
            .module(Friends::new())
            .module(Groups::new())
            // OpenID Connect logins: on once `[modules.oauth.providers.*]` is configured (else every
            // login answers 404).
            .module(OAuth::new())
            .module(Lobbies::new())
            // Queues from `[[modules.matchmaking.queues]]` (none: no queue to join).
            .module(Matchmaking::new())
            // Players' files in `[modules.files] dir` (the deployments mount a data volume there).
            .module(Files::new())
    })
    .await
}
