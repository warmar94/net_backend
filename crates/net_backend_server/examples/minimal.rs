//! A minimal game server: the framework plus one custom route. Headless.
//!
//! ```text
//! # PostgreSQL (the default feature):
//! NBS__DATABASE__URL=postgres://game:secret@127.0.0.1/game cargo run --example minimal
//! # or SQLite in memory, no database server needed:
//! NBS__DATABASE__URL=sqlite::memory: cargo run --example minimal --no-default-features --features sqlite
//!
//! curl http://127.0.0.1:8080/v1/info
//! curl -X POST http://127.0.0.1:8080/v1/game/roll -H 'content-type: application/json' -d '{"sides":20}'
//! ```
//!
//! Other commands of the same binary: `-- migrate`, `-- migrate status`, `-- config check`.

use std::process::ExitCode;

use net_backend_server::axum::routing::post;
use net_backend_server::axum::Json;
use net_backend_server::{ApiJson, AppError, AppState, Config, NetBackendServer};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Roll {
    sides: u32,
}

#[derive(Serialize)]
struct Rolled {
    sides: u32,
    value: u32,
    at: i64,
}

/// `POST /v1/game/roll {"sides":N}` → a pseudo-random roll (clock-based: an example, not a game rule).
async fn roll(state: net_backend_server::axum::extract::State<AppState>, ApiJson(roll): ApiJson<Roll>) -> Result<Json<Rolled>, AppError> {
    if !(2..=1000).contains(&roll.sides) {
        let mut details = net_backend_server::protocol::ValidationDetails::new();
        details.add("sides", "must be between 2 and 1000");
        return Err(AppError::validation(details));
    }
    let now = state.now();
    let value = u32::try_from(now.get().rem_euclid(i64::from(roll.sides))).unwrap_or(0) + 1;
    Ok(Json(Rolled { sides: roll.sides, value, at: now.get() }))
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let server = NetBackendServer::new(config).route("/v1/game/roll", post(roll));
    match server.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
