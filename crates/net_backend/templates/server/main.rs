//! The {{name}} server: net_backend_server with the `Auth` (accounts), `Storage` (saves) and `Chat`
//! modules, configured from `config.toml` in the current folder plus `NBS__*` environment variables.
//!
//! ```text
//! cargo run                                  # serve on http://127.0.0.1:8080 until Ctrl-C
//! cargo run -- --help                        # every command
//! cargo run -- user:create you@example.com --admin
//! cargo run -- config check --connect
//! cargo run -- healthcheck                   # exit 0 when the running server's /readyz answers 200
//! ```
//!
//! Every command of the framework's command line works (`serve` is the default), `healthcheck`
//! included: it asks the running server's `/readyz` on `server.bind` and exits 0 on `200`, 1
//! otherwise. The Docker image uses it as its health check.
//!
//! Add your game's routes, hooks and WebSocket handlers to the `NetBackendServer` below:
//! <https://docs.rs/net_backend_server>.

use std::process::ExitCode;

use net_backend_server::chat::Chat;
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
    })
    .await
}
