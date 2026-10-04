//! The token file: a round trip on its own (save, load, another server, remove), damaged and
//! foreign files, owner-only permissions on Unix, no temporary files left behind, and the client
//! keeping it up to date against the real server on loopback (register, refresh, a resumed start,
//! explicit tokens, logout, a refused refresh), async and blocking.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{email, Server, PASSWORD};
use net_backend_client::protocol::auth::{AccessToken, GetAccount, RefreshToken, RegisterRequest, TokenPair};
use net_backend_client::protocol::UnixMillis;
use net_backend_client::{Client, Error, TokenFile};

/// A fresh, empty folder for one test under the target dir.
fn folder(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("token-file").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test folder");
    dir
}

fn fake_pair(n: u32) -> TokenPair {
    TokenPair::new(AccessToken::new(format!("nbsa_fake-{n}")), UnixMillis(1_000), RefreshToken::new(format!("nbsr_fake-secret-{n}")), UnixMillis(2_000))
}

/// The names in `dir` (to see that no temporary file is left).
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).expect("read dir").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn a_round_trip_on_its_own() {
    let dir = folder("round-trip");
    let file = TokenFile::new(dir.join("nested").join("session.json"));
    let server = "https://api.example.com";
    assert!(matches!(file.load(server), Ok(None)), "no file: no session");
    file.save(server, &fake_pair(1)).expect("save (the missing folder is created)");
    let back = file.load(server).expect("load").expect("stored");
    assert_eq!((back.access_token.expose(), back.refresh_token.expose()), ("nbsa_fake-1", "nbsr_fake-secret-1"));
    assert_eq!((back.access_expires_at, back.refresh_expires_at), (UnixMillis(1_000), UnixMillis(2_000)));
    // Overwritten atomically: the new pair, and nothing else in the folder.
    file.save(server, &fake_pair(2)).expect("save again");
    assert_eq!(file.load(server).expect("load").map(|p| p.refresh_token.expose().to_string()).as_deref(), Some("nbsr_fake-secret-2"));
    assert_eq!(names(&dir.join("nested")), vec!["session.json".to_string()]);
    // Tokens are never handed to another server.
    assert!(matches!(file.load("https://other.example.com"), Ok(None)));
    file.remove().expect("remove");
    assert!(!file.path().exists());
    file.remove().expect("removing a missing file is fine");
}

#[test]
fn damaged_and_foreign_files_are_refused_without_quoting_them() {
    let dir = folder("damaged");
    let file = TokenFile::new(dir.join("session.json"));
    let server = "https://api.example.com";
    let damaged: [&[u8]; 5] = [
        b"",
        b"{\"format\":1,\"server\":\"https://api.example.com\",\"tokens\":{\"access_token\":\"nbsa_fake-leak\"",
        b"[\"nbsr_fake-leak\"]",
        b"{\"format\":1,\"server\":\"https://api.example.com\",\"tokens\":{\"token_type\":\"Bearer\",\"access_token\":7,\"access_expires_at\":\"nbsr_fake-leak\"}}",
        &[0xff, 0xfe, 0x00, 0x41],
    ];
    for content in damaged {
        std::fs::write(file.path(), content).expect("write");
        let error = file.load(server).expect_err("damaged");
        let text = format!("{error} {error:?}");
        assert!(matches!(error, Error::InvalidRequest(_)) && text.contains("not a valid token file"), "{text}");
        assert!(!text.contains("leak"), "the content is never quoted: {text}");
    }
    // Another format number.
    std::fs::write(file.path(), br#"{"format":99,"server":"x","tokens":{}}"#).expect("write");
    assert!(matches!(file.load(server), Err(Error::InvalidRequest(ref why)) if why.contains("format 99")));
    // Too large to be a token file.
    std::fs::write(file.path(), vec![b' '; 70 * 1024]).expect("write");
    assert!(matches!(file.load(server), Err(Error::InvalidRequest(ref why)) if why.contains("not a token file")));
    // A folder where the file should be: cannot be read (and cannot be replaced).
    let folder_path = dir.join("a-folder");
    std::fs::create_dir_all(&folder_path).expect("folder");
    let as_file = TokenFile::new(&folder_path);
    assert!(as_file.load(server).is_err());
    assert!(as_file.save(server, &fake_pair(1)).is_err());
    assert!(!names(&dir).iter().any(|n| n.ends_with(".tmp")), "a failed save leaves no temporary file: {:?}", names(&dir));
}

#[cfg(unix)]
#[test]
fn the_file_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let dir = folder("unix-mode");
    let path = dir.join("fresh").join("session.json");
    let file = TokenFile::new(&path);
    file.save("https://api.example.com", &fake_pair(1)).expect("save");
    let mode = |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir.join("fresh")), 0o700, "a folder it creates is owner-only too");
    // A file someone else made world-readable is replaced by an owner-only one.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    file.save("https://api.example.com", &fake_pair(2)).expect("save");
    assert_eq!(mode(&path), 0o600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_keeps_the_file_up_to_date() {
    let server = Server::start();
    let dir = folder("client");
    let path = dir.join("session.json");
    let file = TokenFile::new(&path);

    // A first start: no file, no session.
    let client = Client::builder(&server.base).token_file(&path).build().expect("client");
    assert!(!client.is_logged_in());
    let session = client.register(RegisterRequest::new(email("filed"), PASSWORD)).await.expect("register");
    let stored = file.load(&client.server_url()).expect("load").expect("written at register");
    assert_eq!(stored.refresh_token.expose(), session.tokens.refresh_token.expose());

    // A refresh rotates the refresh token: the file follows.
    let fresh = client.refresh().await.expect("refresh");
    assert_ne!(fresh.refresh_token.expose(), session.tokens.refresh_token.expose());
    let stored = file.load(&client.server_url()).expect("load").expect("stored");
    assert_eq!(stored.refresh_token.expose(), fresh.refresh_token.expose());
    drop(client);

    // The next start resumes from the file.
    let resumed = Client::builder(&server.base).token_file(&path).build().expect("client");
    assert!(resumed.is_logged_in());
    let me = resumed.call(&GetAccount::new()).await.expect("a call with the stored session");
    assert_eq!(me.id, session.account.id);
    // ... and an expired access token is refreshed at the next call; the file gets that pair too.
    server.advance(Duration::from_secs(2 * 3600));
    resumed.call(&GetAccount::new()).await.expect("refreshed, then sent");
    let stored = file.load(&resumed.server_url()).expect("load").expect("stored");
    assert_eq!(stored.refresh_token.expose(), resumed.tokens().expect("tokens").refresh_token.expose());

    // Logout deletes it; nothing else was ever left in the folder.
    resumed.logout().await.expect("logout");
    assert!(!path.exists());
    assert!(names(&dir).is_empty(), "{:?}", names(&dir));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn damaged_foreign_and_explicit_starts() {
    let server = Server::start();
    let dir = folder("starts");
    let path = dir.join("session.json");

    // Damaged: the client starts without a session; the next login overwrites the file.
    std::fs::write(&path, b"{ not json").expect("write");
    let client = Client::builder(&server.base).token_file(&path).build().expect("a damaged file does not stop the start");
    assert!(!client.is_logged_in());
    client.register(RegisterRequest::new(email("damaged"), PASSWORD)).await.expect("register");
    assert!(TokenFile::new(&path).load(&client.server_url()).expect("valid again").is_some());

    // A file of another server: not used, not sent there.
    let other = Client::builder("http://127.0.0.1:9").token_file(&path).build().expect("client");
    assert!(!other.is_logged_in());

    // Explicit tokens win over the file's and are written to it.
    let explicit = client.tokens().expect("tokens");
    std::fs::remove_file(&path).expect("remove");
    let client = Client::builder(&server.base).tokens(explicit.clone()).token_file(&path).build().expect("client");
    assert!(client.is_logged_in());
    let stored = TokenFile::new(&path).load(&client.server_url()).expect("load").expect("written at build");
    assert_eq!(stored.refresh_token.expose(), explicit.refresh_token.expose());

    // A file that exists but cannot be read stops the build.
    let unreadable = Client::builder(&server.base).token_file(&dir).build();
    assert!(matches!(unreadable, Err(Error::InvalidRequest(ref why)) if why.contains("token file")), "{unreadable:?}");

    // A refused refresh ends the session: the file is deleted.
    let client = Client::builder(&server.base).token_file(&path).build().expect("client");
    client.resume(fake_pair(7));
    assert_eq!(TokenFile::new(&path).load(&client.server_url()).expect("load").map(|p| p.access_token.expose().to_string()).as_deref(), Some("nbsa_fake-7"));
    let ended = client.call(&GetAccount::new()).await.expect_err("an unknown refresh token is refused");
    assert!(ended.needs_login(), "{ended:?}");
    assert!(!path.exists(), "the ended session's file is gone");
}

#[test]
fn the_blocking_client_uses_it_too() {
    let server = Server::start();
    let dir = folder("blocking");
    let path = dir.join("session.json");
    let builder = Client::builder(&server.base).token_file(&path);
    let client = net_backend_client::blocking::Client::from_builder(builder).expect("client");
    client.register(RegisterRequest::new(email("blocking-file"), PASSWORD)).expect("register");
    drop(client);
    let client = net_backend_client::blocking::Client::from_builder(Client::builder(&server.base).token_file(&path)).expect("client");
    assert!(client.is_logged_in());
    client.call(&GetAccount::new()).expect("resumed");
    client.logout().expect("logout");
    assert!(!path.exists());
}
