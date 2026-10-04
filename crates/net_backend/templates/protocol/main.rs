//! The {{name}} client side: the message types of net_backend_protocol, ready for the HTTP and
//! WebSocket library of your choice (reqwest, ureq, tokio-tungstenite, ...). This program needs no
//! network: it prints what a client sends, where, and with which header.
//!
//! ```text
//! cargo run -p client
//! ```
//!
//! Every HTTP call is a type with its route and payload (`HttpCall`), every WebSocket request a type
//! with its kind (`WsCall`); the answers decode into the matching `Response` types. The guide:
//! <https://docs.rs/net_backend_protocol>; the API: API.md in the net_backend repository.

use net_backend_protocol::auth::{GetAccount, LoginRequest, RegisterRequest};
use net_backend_protocol::chat::{JoinRoom, SendMessage};
use net_backend_protocol::storage::{GetObject, PutObject, WriteObject};
use net_backend_protocol::version::GetServerInfo;
use net_backend_protocol::{
    HttpCall, PROTOCOL_HEADER, PROTOCOL_VERSION, RoomId, WsCall, WsRequestFrame,
};

/// The server `cargo run` starts in the server folder.
const SERVER: &str = "http://127.0.0.1:8080";

fn main() -> Result<(), serde_json::Error> {
    println!(
        "server {SERVER}, every request with the header `{PROTOCOL_HEADER}: {PROTOCOL_VERSION}`"
    );
    println!();
    println!("HTTP:");
    http(&GetServerInfo::new())?;
    http(&RegisterRequest::new(
        "player@example.com",
        "a long password",
    ))?;
    http(&LoginRequest::new("player@example.com", "a long password"))?;
    http(&GetAccount::new())?;
    let save = serde_json::json!({ "level": 3, "gold": 120 });
    http(&WriteObject::new("saves", "slot1", PutObject::new(save)))?;
    http(&GetObject::new("saves", "slot1"))?;
    println!();
    println!("WebSocket ({SERVER}/v1/ws, the access token as `Authorization: Bearer`):");
    ws(1, JoinRoom::new("world"))?;
    ws(2, SendMessage::new(RoomId(1), "hello"))?;
    Ok(())
}

/// One HTTP call: method, path and JSON body (or query).
fn http<C: HttpCall>(call: &C) -> Result<(), serde_json::Error> {
    let path = call.path().unwrap_or_default();
    let login = if C::ROUTE.auth { "  (logged in)" } else { "" };
    println!(
        "  {} {path}{login}\n      {}",
        C::ROUTE.method.as_str(),
        serde_json::to_string(call.payload())?
    );
    Ok(())
}

/// One WebSocket request frame.
fn ws<C: WsCall>(id: u64, call: C) -> Result<(), serde_json::Error> {
    println!(
        "  {}",
        serde_json::to_string(&WsRequestFrame::call(id, call))?
    );
    Ok(())
}
