//! `load_test`: a load generator for servers built with `net_backend_server` (the reference server or
//! your own with the `Auth`, `Storage` and `Chat` modules). It speaks the protocol over HTTP(S) and
//! WS(S), so it measures the whole path through a reverse proxy such as Caddy.
//!
//! ```text
//! load_test --base https://api.example.com users --count 1000          # accounts + tokens -> users.jsonl
//! load_test --base https://api.example.com sockets --count 10000 --hold 120
//! load_test --base https://api.example.com chat --members 200 --senders 50 --rate 0.5 --duration 30
//! load_test --base https://api.example.com dm --pairs 20 --messages 50
//! load_test --base https://api.example.com saves --count 500
//! load_test --base https://api.example.com batch --count 1 --offset 900
//! load_test --base https://api.example.com http --route account --concurrency 64 --duration 30
//! ```
//!
//! Every scenario prints a JSON result and one `RESULT {…}` line. The server's own limits (login and
//! registration rates per address, the chat send rate, connections per user / address) shape what a
//! single load machine can do; see the crate README for the settings a load-test server uses.

mod net;
mod scenarios;
mod stats;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Load generator for net_backend_server (HTTP(S) + WS(S))", disable_version_flag = true)]
struct Cli {
    /// The server: `https://api.example.com` or `http://127.0.0.1:8080` (no path).
    #[arg(long, env = "LT_BASE")]
    base: String,
    /// The users file (`users` writes it, the other scenarios read it).
    #[arg(long, default_value = "users.jsonl")]
    users: PathBuf,
    /// Tokio worker threads (default: one per CPU).
    #[arg(long)]
    threads: Option<usize>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register accounts `<prefix>-<n>@example.com` (or log in where they exist) and write their tokens.
    Users {
        #[arg(long)]
        count: usize,
        #[arg(long, default_value = "lt")]
        prefix: String,
        /// The accounts' password (throwaway test accounts).
        #[arg(long, env = "LT_PASSWORD", default_value = "load-test-password")]
        password: String,
        #[arg(long, default_value_t = 32)]
        concurrency: usize,
    },
    /// Open authenticated WebSockets (users round-robin) and hold them.
    Sockets {
        #[arg(long)]
        count: usize,
        /// Seconds to hold every socket open.
        #[arg(long, default_value_t = 60)]
        hold: u64,
        /// Handshakes in flight at once.
        #[arg(long, default_value_t = 200)]
        concurrency: usize,
        /// Reconnect a socket the server closes during the hold (restart the server meanwhile: a
        /// reconnect storm).
        #[arg(long)]
        reconnect: bool,
    },
    /// Chat fan-out in one public room.
    Chat {
        #[arg(long, default_value = "world")]
        room: String,
        #[arg(long)]
        members: usize,
        #[arg(long)]
        senders: usize,
        /// Messages per second per sender.
        #[arg(long, default_value_t = 0.5)]
        rate: f64,
        /// Seconds of sending.
        #[arg(long, default_value_t = 30)]
        duration: u64,
        /// Seconds to wait for the last deliveries.
        #[arg(long, default_value_t = 15)]
        drain: u64,
    },
    /// DM bursts: both members of each pair send at once into their DM room.
    Dm {
        #[arg(long)]
        pairs: usize,
        /// Messages per member.
        #[arg(long, default_value_t = 50)]
        messages: u64,
        #[arg(long, default_value_t = 15)]
        drain: u64,
    },
    /// First saves of many players at the same moment (`if_absent` writes).
    Saves {
        #[arg(long)]
        count: usize,
        /// The value's size in bytes.
        #[arg(long, default_value_t = 4096)]
        bytes: usize,
        #[arg(long, default_value = "save")]
        collection: String,
        #[arg(long, default_value = "slot-1")]
        key: String,
    },
    /// Batch puts (default 16 objects x 256 KiB = the 4 MiB maximum) through the whole path.
    Batch {
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Skip this many users (use accounts that store nothing else: the byte quota is 4 MiB).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 16)]
        objects: usize,
        #[arg(long, default_value_t = 262_144)]
        object_bytes: usize,
    },
    /// Requests back to back on keep-alive connections.
    Http {
        #[arg(long, value_enum, default_value = "account")]
        route: scenarios::Route,
        #[arg(long, default_value_t = 32)]
        concurrency: usize,
        #[arg(long, default_value_t = 30)]
        duration: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(threads) = cli.threads {
        runtime.worker_threads(threads.max(1));
    }
    let runtime = match runtime.build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("load_test: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("load_test: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let target = net::Target::parse(&cli.base)?;
    let users = |needed: usize| scenarios::load_users(&cli.users, needed);
    match cli.command {
        Command::Users { count, prefix, password, concurrency } => scenarios::users(&target, &cli.users, count, &prefix, &password, concurrency).await,
        Command::Sockets { count, hold, concurrency, reconnect } => {
            scenarios::sockets(&target, &users(1)?, count, Duration::from_secs(hold), concurrency, reconnect).await
        }
        Command::Chat { room, members, senders, rate, duration, drain } => {
            if !(rate.is_finite() && rate >= 0.0) {
                return Err("--rate must be a number >= 0".into());
            }
            scenarios::chat(&target, &users(members)?, &room, members, senders, rate, Duration::from_secs(duration), Duration::from_secs(drain)).await
        }
        Command::Dm { pairs, messages, drain } => scenarios::dm(&target, &users(pairs * 2)?, pairs, messages, Duration::from_secs(drain)).await,
        Command::Saves { count, bytes, collection, key } => scenarios::saves(&target, &users(count)?, count, bytes, &collection, &key).await,
        Command::Batch { count, offset, objects, object_bytes } => {
            scenarios::batch(&target, &users(offset + count)?, count, offset, objects, object_bytes).await
        }
        Command::Http { route, concurrency, duration } => {
            scenarios::http_rate(&target, &users(1)?, route, concurrency.max(1), Duration::from_secs(duration)).await
        }
    }
}
