//! The {{name}} client: net_backend_client against the server. It logs in (registering the account
//! on the first run), writes and reads a save, joins the public chat room `world`, sends a message
//! and prints the room's messages for 10 seconds.
//!
//! ```text
//! {{client_run}}
//! {{client_run}} -- https://api.example.com
//! ```
//!
//! The first form talks to the server at `http://127.0.0.1:8080`, the second to the one it names
//! (so does the variable `NET_BACKEND_URL`). net_backend_client sends plain `http://` only to this
//! machine (127.0.0.1, ::1, localhost); any other server is reached over `https://`.
//!
//! On this machine the client uses a development account unless the variables `NET_BACKEND_EMAIL`
//! and `NET_BACKEND_PASSWORD` name one; any other server needs these two variables (the development
//! password is in this file). Client guide: <https://docs.rs/net_backend_client>.

use std::process::ExitCode;
use std::time::Duration;

use net_backend_client::Client;
use net_backend_client::protocol::auth::{GetAccount, LoginRequest, RegisterRequest};
use net_backend_client::protocol::chat::{ChatMessage, JoinRoom, SendMessage};
use net_backend_client::protocol::storage::{GetObject, PutObject, WriteObject};
use net_backend_client::protocol::{PROTOCOL_VERSION, codes};
use net_backend_client::ws::{WsEvent, WsSettings};

/// The server when neither an argument nor `NET_BACKEND_URL` names one.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";
/// The development account (registered on the first run; used on this machine only).
const DEV_EMAIL: &str = "player@example.com";
const DEV_PASSWORD: &str = "dev password 1234";
/// How long the client prints the chat room's messages.
const LISTEN: Duration = Duration::from_secs(10);

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let url = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("NET_BACKEND_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    match run(&url).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            if url == DEFAULT_URL {
                eprintln!(
                    "(is the server running? Start it with `cargo run` in the server's folder)"
                );
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (email, password) =
        match (
            std::env::var("NET_BACKEND_EMAIL"),
            std::env::var("NET_BACKEND_PASSWORD"),
        ) {
            (Ok(email), Ok(password)) => (email, password),
            _ if is_loopback(url) => (DEV_EMAIL.to_string(), DEV_PASSWORD.to_string()),
            _ => return Err(
                "set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD for a server on another machine"
                    .into(),
            ),
        };

    let client = Client::new(url)?;
    let info = client.info().await?;
    println!(
        "server {url}: protocol {}, modules {:?}",
        info.protocol, info.modules
    );
    if !info.supports(PROTOCOL_VERSION) {
        return Err(format!(
            "this client speaks protocol {PROTOCOL_VERSION}: update the client or the server"
        )
        .into());
    }

    // Log in; on the first run, register the account instead.
    match client
        .login(LoginRequest::new(email.as_str(), password.as_str()))
        .await
    {
        Ok(_) => println!("logged in as {email}"),
        Err(error) if error.is(codes::INVALID_CREDENTIALS) => {
            client
                .register(RegisterRequest::new(email.as_str(), password.as_str()))
                .await?;
            println!("registered {email}");
        }
        Err(error) => return Err(error.into()),
    }
    let account = client.call(&GetAccount::new()).await?;
    println!("account {}", account.id);

    // A save: write an object, read it back.
    if info.has_module("storage") {
        let save = serde_json::json!({ "level": 3, "gold": 120 });
        let written = client
            .call(&WriteObject::new("saves", "slot1", PutObject::new(save)))
            .await?;
        let read = client.call(&GetObject::new("saves", "slot1")).await?;
        println!(
            "saved version {}, read back {}",
            written.version.get(),
            read.value
        );
    }

    // Chat over the WebSocket: join `world`, say hello, print the room's messages.
    if info.has_module("chat") {
        let ws = client.connect_ws(WsSettings::default()).await?;
        let mut events = ws.events();
        let mut messages = ws.subscribe::<ChatMessage>();
        let room = ws.request(&JoinRoom::new("world")).await?;
        println!("joined `world`; listening for {} s", LISTEN.as_secs());
        ws.request(&SendMessage::new(room.id, "hello from the {{name}} client"))
            .await?;
        let until = tokio::time::Instant::now() + LISTEN;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(until) => break,
                message = messages.next() => match message {
                    Some(Ok(message)) => {
                        let sender = match message.sender_name {
                            Some(name) => name,
                            None => format!("player {}", message.sender),
                        };
                        println!("[world] {sender}: {}", message.text);
                    }
                    Some(Err(error)) => println!("({error})"),
                    None => break,
                },
                event = events.next() => match event {
                    // Room membership ends with each connection: join again after a reconnect.
                    Some(WsEvent::Connected { reconnected: true }) => {
                        ws.request(&JoinRoom::new("world")).await?;
                    }
                    Some(WsEvent::Closed { error }) => {
                        println!("the WebSocket closed: {error:?}");
                        break;
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        ws.close();
    }

    client.logout().await?;
    println!("logged out");
    Ok(())
}

/// Whether the URL names this machine (127.0.0.0/8, ::1 or localhost).
fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
