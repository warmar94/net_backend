//! Log in (or register), read the account, save and load a storage object: the async client.
//! Headless and bounded (every call has the client's 15 s deadline).
//!
//! ```text
//! NET_BACKEND_URL=http://127.0.0.1:8080 NET_BACKEND_EMAIL=player@example.com NET_BACKEND_PASSWORD="a long password" \
//!     cargo run -p net_backend_client --example quickstart
//! ```

use net_backend_client::protocol::auth::{GetAccount, LoginRequest, RegisterRequest};
use net_backend_client::protocol::storage::{GetObject, PutObject, WriteObject};
use net_backend_client::protocol::{codes, PROTOCOL_VERSION};
use net_backend_client::{Client, Error};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let url = std::env::var("NET_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let (Ok(email), Ok(password)) = (std::env::var("NET_BACKEND_EMAIL"), std::env::var("NET_BACKEND_PASSWORD")) else {
        eprintln!("set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD (and NET_BACKEND_URL, default http://127.0.0.1:8080)");
        return Ok(());
    };
    let client = Client::new(&url)?;
    let info = client.info().await?;
    println!("server speaks protocol {}..={}, modules {:?}", info.min_protocol, info.protocol, info.modules);
    if !info.supports(PROTOCOL_VERSION) {
        eprintln!("this client speaks protocol {PROTOCOL_VERSION}: update one of them");
        return Ok(());
    }
    // Log in; on a fresh server, register instead.
    match client.login(LoginRequest::new(email.as_str(), password.as_str())).await {
        Ok(_) => println!("logged in"),
        Err(error) if error.is(codes::INVALID_CREDENTIALS) => {
            client.register(RegisterRequest::new(email.as_str(), password.as_str())).await?;
            println!("registered");
        }
        Err(error) => return Err(error),
    }
    // A real app stores every new token pair (the refresh token rotates); here we only report them.
    let mut updates = client.token_updates();
    let me = client.call(&GetAccount::new()).await?;
    println!("account {} ({:?})", me.id, me.email);
    if info.has_module("storage") {
        let ack = client.call(&WriteObject::new("saves", "quickstart", PutObject::new(serde_json::json!({"level": 3})))).await?;
        let save = client.call(&GetObject::new("saves", "quickstart")).await?;
        println!("saved version {}; loaded {}", ack.version.get(), save.value);
    }
    if let Some(change) = updates.try_changed() {
        println!("the tokens changed (logged in: {})", change.is_some());
    }
    client.logout().await?;
    println!("logged out");
    Ok(())
}
