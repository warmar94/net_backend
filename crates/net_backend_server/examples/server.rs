//! The reference server: the framework with the `Auth`, `Storage` and `Chat` modules, configured
//! entirely from `config.toml` + `NBS__*` variables. It is the binary the deployment files in the
//! repository's `deploy/` folder install (Docker Compose or systemd); a game's own server binary
//! starts from this file and adds its routes, hooks and WebSocket handlers.
//!
//! ```text
//! cargo build --release -p net_backend_server --example server --all-features
//! # or only what a deployment uses, e.g. MySQL + mail:
//! cargo build --release -p net_backend_server --example server --no-default-features --features mysql,storage,chat,smtp
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
use net_backend_server::storage::Storage;
use net_backend_server::{Auth, NetBackendServer};

#[tokio::main]
async fn main() -> ExitCode {
    // `--help` and `--version` work without a configuration; everything else loads it.
    NetBackendServer::run_main(|config| NetBackendServer::new(config).module(Auth::new()).module(Storage::new()).module(Chat::new())).await
}
