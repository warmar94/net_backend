//! The WebSocket (feature `ws`): join the public room `world`, send a message, print the room's
//! messages for 30 s, and rejoin after every reconnect. Headless and bounded.
//!
//! ```text
//! NET_BACKEND_URL=http://127.0.0.1:8080 NET_BACKEND_EMAIL=player@example.com NET_BACKEND_PASSWORD="a long password" \
//!     cargo run -p net_backend_client --example chat --features ws
//! ```

use std::time::Duration;

use net_backend_client::protocol::auth::LoginRequest;
use net_backend_client::protocol::chat::{ChatMessage, JoinRoom, SendMessage};
use net_backend_client::ws::{WsEvent, WsSettings};
use net_backend_client::{Client, Error};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let url = std::env::var("NET_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let (Ok(email), Ok(password)) = (std::env::var("NET_BACKEND_EMAIL"), std::env::var("NET_BACKEND_PASSWORD")) else {
        eprintln!("set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD (and NET_BACKEND_URL, default http://127.0.0.1:8080)");
        return Ok(());
    };
    let client = Client::new(&url)?;
    client.login(LoginRequest::new(email.as_str(), password.as_str())).await?;
    let ws = client.connect_ws(WsSettings::default()).await?;
    let mut events = ws.events();
    let mut messages = ws.subscribe::<ChatMessage>();
    let room = ws.request(&JoinRoom::new("world")).await?;
    ws.request(&SendMessage::new(room.id, "hello from net_backend_client")).await?;
    let until = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => break,
            message = messages.next() => match message {
                Some(Ok(message)) => println!("[{}] {}: {}", message.room, message.sender_name.as_deref().unwrap_or("?"), message.text),
                Some(Err(error)) => println!("({error}: reload the history)"),
                None => break,
            },
            event = events.next() => match event {
                Some(WsEvent::Connected { reconnected: true }) => {
                    // Membership ends with each connection: join again (and reload what was missed).
                    ws.request(&JoinRoom::new("world")).await?;
                    println!("(reconnected, rejoined)");
                }
                Some(WsEvent::Closed { error }) => {
                    println!("closed: {error:?}");
                    break;
                }
                Some(other) => println!("({other:?})"),
                None => break,
            },
        }
    }
    ws.close();
    client.logout().await?;
    Ok(())
}
