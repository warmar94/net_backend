//! The {{name}} server, built with net_backend_server. Its modules: {{modules_doc}}.
//! It reads `config.toml` in the current folder plus `NBS__*` environment variables.
//!
//! ```text
//! cargo run                                  # serve on http://127.0.0.1:8080 until Ctrl-C
//! cargo run -- --help                        # every command
{{user_create}}//! cargo run -- config check --connect
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

{{imports}}
#[tokio::main]
async fn main() -> ExitCode {
    // `--help` and `--version` work without a configuration; everything else loads it.
{{main_body}}
}
