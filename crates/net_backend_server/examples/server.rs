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
//! Every command of the framework's command line works (`--help` lists them). `healthcheck` is
//! this binary's own: it asks the running server's `/readyz` on `server.bind` (an unspecified
//! address such as `0.0.0.0` means loopback) and exits 0 on `200`, 1 otherwise. Container
//! images without a shell or `curl` use it as their health check.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use net_backend_server::chat::Chat;
use net_backend_server::storage::Storage;
use net_backend_server::{Auth, Config, NetBackendServer};

/// How long the health check waits for the server (connect, write, read each).
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> ExitCode {
    let healthcheck = std::env::args_os().nth(1).is_some_and(|arg| arg == "healthcheck");
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if healthcheck {
        return match ready(config.server.bind) {
            Ok(()) => ExitCode::SUCCESS,
            Err(problem) => {
                eprintln!("healthcheck: {problem}");
                ExitCode::FAILURE
            }
        };
    }
    let server = NetBackendServer::new(config).module(Auth::new()).module(Storage::new()).module(Chat::new());
    match server.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// `GET /readyz` on the listening address; `Ok` on a `200` answer.
fn ready(bind: SocketAddr) -> Result<(), String> {
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let addr = SocketAddr::new(ip, bind.port());
    let mut stream = TcpStream::connect_timeout(&addr, HEALTHCHECK_TIMEOUT).map_err(|e| format!("cannot connect to {addr}: {e}"))?;
    stream.set_read_timeout(Some(HEALTHCHECK_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(HEALTHCHECK_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").map_err(|e| format!("cannot send the request: {e}"))?;
    // The status line is all that matters; read at most a small answer.
    let mut answer = Vec::with_capacity(256);
    stream.take(4096).read_to_end(&mut answer).map_err(|e| format!("cannot read the answer: {e}"))?;
    let status_line = answer.split(|&b| b == b'\n').next().map(String::from_utf8_lossy).unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status == "200" {
        Ok(())
    } else {
        Err(format!("/readyz answered `{}`", status_line.trim()))
    }
}
