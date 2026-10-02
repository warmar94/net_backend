//! The blocking interface in a game-loop shape: requests never block a frame; each frame polls
//! `Reply::try_take`. Headless and bounded (at most about 10 s of frames).
//!
//! ```text
//! NET_BACKEND_URL=http://127.0.0.1:8080 NET_BACKEND_EMAIL=player@example.com NET_BACKEND_PASSWORD="a long password" \
//!     cargo run -p net_backend_client --example blocking_loop
//! ```

use std::time::{Duration, Instant};

use net_backend_client::blocking::Client;
use net_backend_client::protocol::auth::{GetAccount, LoginRequest};
use net_backend_client::protocol::chat::ListRooms;
use net_backend_client::Error;

fn main() -> Result<(), Error> {
    let url = std::env::var("NET_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let (Ok(email), Ok(password)) = (std::env::var("NET_BACKEND_EMAIL"), std::env::var("NET_BACKEND_PASSWORD")) else {
        eprintln!("set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD (and NET_BACKEND_URL, default http://127.0.0.1:8080)");
        return Ok(());
    };
    let client = Client::new(&url)?;
    // Logging in at start-up may block: there is no frame yet.
    client.login(LoginRequest::new(email.as_str(), password.as_str()))?;
    // In the loop, nothing blocks.
    let mut me = client.send(GetAccount::new());
    let mut rooms = client.send(ListRooms::new());
    let started = Instant::now();
    let mut frame = 0u64;
    while started.elapsed() < Duration::from_secs(10) && !(me.is_taken() && rooms.is_taken()) {
        frame += 1;
        if let Some(answer) = me.try_take() {
            println!("frame {frame}: account {:?}", answer.map(|account| account.id));
        }
        if let Some(answer) = rooms.try_take() {
            match answer {
                Ok(page) => println!("frame {frame}: {} public room(s)", page.items.len()),
                Err(error) => println!("frame {frame}: rooms: {error}"),
            }
        }
        std::thread::sleep(Duration::from_millis(16)); // the rest of the frame
    }
    client.logout()?;
    Ok(())
}
