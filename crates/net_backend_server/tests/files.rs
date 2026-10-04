//! The files module: uploads (multipart, the meta part, defaults from the file part, the SHA-256,
//! names and content types), downloads (bytes, headers, 304), listings (own and another player's
//! readable files, pages), the usage, settings (visibility private / public / friends / shared, the
//! share list, metadata), who may read, change and delete, the hooks, nothing left of a refused or
//! broken upload, the limits (file size, file count, byte quota, also under concurrent uploads),
//! the rate, the wiring and a game's own store. The 0.2.0 review fixes: the settings hook
//! (`BeforeFileUpdate`: refuse or change a visibility) and settings changes under the row's lock
//! (concurrent share lists, a change racing a delete).
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "files", feature = "friends", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]
// The helpers serve the SQLite-only tests too; without SQLite (the MySQL / PostgreSQL CI runs) part
// of them stays unused.
#![cfg_attr(not(feature = "sqlite"), allow(dead_code, unused_imports))]

mod common;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::Router;
use futures_util::future::BoxFuture;
use futures_util::stream::StreamExt;
use http::{HeaderMap, Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::files::backend::ByteStream;
use net_backend_server::files::events::{AfterFileChange, BeforeFileUpdate, BeforeFileUpload, FileChange};
use net_backend_server::files::{FileService, FileStore, Files, FilesConfig};
use net_backend_server::friends::{Friends, FriendsConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::{codes, routes, UnixMillis};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{AppError, Config, ManualClock, NetBackendServer, PreparedServer, SecretString};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";
const BOUNDARY: &str = "nbs-test-boundary-7d1f";

type Events = Arc<Mutex<Vec<(i64, FileChange)>>>;

struct Server {
    prepared: PreparedServer,
    router: Router,
    dir: PathBuf,
    events: Events,
}

/// One part of a multipart body.
enum Part<'a> {
    Meta(Value),
    File { name: Option<&'a str>, content_type: Option<&'a str>, bytes: &'a [u8] },
    Other(&'a str),
}

fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::Meta(value) => {
                body.extend_from_slice(b"Content-Disposition: form-data; name=\"meta\"\r\nContent-Type: application/json\r\n\r\n");
                body.extend_from_slice(value.to_string().as_bytes());
            }
            Part::File { name, content_type, bytes } => {
                let file_name = name.map(|n| format!("; filename=\"{n}\"")).unwrap_or_default();
                body.extend_from_slice(format!("Content-Disposition: form-data; name=\"file\"{file_name}\r\n").as_bytes());
                if let Some(content_type) = content_type {
                    body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
                }
                body.extend_from_slice(b"\r\n");
                body.extend_from_slice(bytes);
            }
            Part::Other(name) => {
                body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\nx").as_bytes());
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Every file the local store holds (not counting its `tmp` folder).
fn stored_files(dir: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n != "tmp") {
                        walk(&path, count);
                    }
                } else {
                    *count += 1;
                }
            }
        }
    }
    let mut count = 0;
    walk(dir, &mut count);
    count
}

async fn start(url: &str, tweak: impl FnOnce(&mut FilesConfig)) -> Server {
    start_with(url, |_| {}, tweak).await
}

async fn start_with(url: &str, server: impl FnOnce(&mut Config), tweak: impl FnOnce(&mut FilesConfig)) -> Server {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("files-migrations");
    server(&mut config);
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    let dir = common::temp_dir("files-store");
    let mut files = FilesConfig::default();
    files.dir = dir.clone();
    files.upload_rate = 0;
    tweak(&mut files);
    let mut friends = FriendsConfig::default();
    friends.request_rate = 0;
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let prepared = NetBackendServer::new(config)
        .clock(Arc::new(ManualClock::new(UnixMillis(T0))))
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Friends::new().with_config(friends))
        .module(Files::new().with_config(files))
        .before::<BeforeFileUpload, _, _>(|_ctx, upload| async move {
            if upload.name == "refused.bin" {
                return Ok(Decision::Reject(AppError::forbidden("not this file")));
            }
            Ok(Decision::Continue(upload))
        })
        // The game's review: `never.map` is never public; `pending.map` stays private until the
        // game reviewed it (the hook changes the visibility).
        .before::<BeforeFileUpdate, _, _>(|_ctx, mut update| async move {
            if update.visibility == Some(net_backend_server::protocol::files::FileVisibility::Public) {
                if update.name.as_deref() == Some("never.map") {
                    return Ok(Decision::Reject(AppError::forbidden("not public")));
                }
                if update.name.as_deref() == Some("pending.map") {
                    update.visibility = Some(net_backend_server::protocol::files::FileVisibility::Private);
                }
            }
            Ok(Decision::Continue(update))
        })
        .after::<AfterFileChange, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.file.get(), event.change));
                Ok(())
            }
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    Server { prepared, router, dir, events }
}

impl Server {
    async fn raw(&self, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let response = self.router.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 64 << 20).await.expect("body");
        (status, headers, bytes)
    }

    async fn json(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let (status, _, bytes) = self.raw(request).await;
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn upload(&self, token: &str, parts: &[Part<'_>]) -> (StatusCode, Value) {
        let request = Request::post(routes::files::LIST)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(Body::from(multipart(parts)))
            .expect("request");
        let (status, _, bytes) = self.raw(request).await;
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn upload_ok(&self, token: &str, parts: &[Part<'_>]) -> Value {
        let (status, body) = self.upload(token, parts).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn download(&self, token: &str, file: i64, if_none_match: Option<&str>) -> (StatusCode, HeaderMap, Bytes) {
        let mut request = Request::get(format!("/v1/files/{file}/content")).header("authorization", format!("Bearer {token}"));
        if let Some(tag) = if_none_match {
            request = request.header("if-none-match", tag);
        }
        self.raw(request.body(Body::empty()).expect("request")).await
    }

    async fn register(&self, email: &str) -> (i64, String) {
        let request = common::post_json(routes::auth::REGISTER, json!({"email": email, "password": PASSWORD}).to_string());
        let (status, _, body) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (body["account"]["id"].as_i64().expect("id"), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn usage(&self, token: &str) -> (u64, u64) {
        let (status, body) = self.json(Method::GET, routes::files::USAGE, None, token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (body["files"].as_u64().unwrap_or(99), body["bytes"].as_u64().unwrap_or(99))
    }

    async fn ids(&self, path: &str, token: &str) -> Vec<i64> {
        let (status, body) = self.json(Method::GET, path, None, token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["items"].as_array().map(|items| items.iter().filter_map(|i| i["id"].as_i64()).collect()).unwrap_or_default()
    }
}

// ---- the suite (every database) -------------------------------------------------------------------

async fn uploads_reads_and_settings(url: &str) {
    let server = start(url, |_| {}).await;
    let (alice, a) = server.register("alice@example.com").await;
    let (bob, b) = server.register("bob@example.com").await;
    let (carol, c) = server.register("carol@example.com").await;
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

    // Upload with settings; the server's SHA-256 matches; the owner reads it back.
    let meta = json!({"name": "level-3.map", "metadata": {"title": "Caves"}, "sha256": sha(&bytes)});
    let info =
        server.upload_ok(&a, &[Part::Meta(meta), Part::File { name: Some("ignored.bin"), content_type: Some("application/x-level"), bytes: &bytes }]).await;
    let file = info["id"].as_i64().expect("id");
    assert_eq!(
        (info["name"].as_str(), info["content_type"].as_str(), info["size"].as_u64()),
        (Some("level-3.map"), Some("application/x-level"), Some(300_000))
    );
    assert_eq!((info["sha256"].as_str(), info["visibility"].as_str(), info["owner"].as_i64()), (Some(sha(&bytes).as_str()), Some("private"), Some(alice)));
    assert_eq!(info["metadata"], json!({"title": "Caves"}));
    let (status, headers, body) = server.download(&a, file, None).await;
    assert_eq!((status, body.len()), (StatusCode::OK, bytes.len()));
    assert!(body[..] == bytes[..], "the bytes come back unchanged");
    let etag = format!("\"{}\"", sha(&bytes));
    assert_eq!(headers.get("etag").and_then(|v| v.to_str().ok()), Some(etag.as_str()));
    assert_eq!(headers.get("content-type").and_then(|v| v.to_str().ok()), Some("application/x-level"));
    assert_eq!(headers.get("x-content-type-options").and_then(|v| v.to_str().ok()), Some("nosniff"));
    assert!(headers.get("content-disposition").and_then(|v| v.to_str().ok()).is_some_and(|d| d.starts_with("attachment; filename=\"level-3.map\"")));
    assert_eq!(server.download(&a, file, Some(&etag)).await.0, StatusCode::NOT_MODIFIED);
    assert_eq!(server.usage(&a).await, (1, 300_000));

    // Defaults from the file part (the last part of a path; the part's content type).
    let shot = server.upload_ok(&a, &[Part::File { name: Some("C:\\shots\\screen 1.png"), content_type: Some("image/png"), bytes: b"\x89PNG" }]).await;
    assert_eq!((shot["name"].as_str(), shot["content_type"].as_str()), (Some("screen 1.png"), Some("image/png")));
    let bare = server.upload_ok(&a, &[Part::File { name: None, content_type: None, bytes: b"" }]).await;
    assert_eq!((bare["name"].as_str(), bare["content_type"].as_str(), bare["size"].as_u64()), (Some("file"), Some("application/octet-stream"), Some(0)));

    // Private: others get 404 for the settings, the bytes and in the owner's list.
    let one = format!("/v1/files/{file}");
    assert_eq!(server.json(Method::GET, &one, None, &b).await.0, StatusCode::NOT_FOUND);
    assert_eq!(server.download(&b, file, None).await.0, StatusCode::NOT_FOUND);
    assert!(server.ids(&format!("/v1/files?owner={alice}"), &b).await.is_empty());
    assert_eq!(server.ids(routes::files::LIST, &a).await.len(), 3);

    // Public: every player reads it; only the owner changes or deletes it.
    let (status, body) = server.json(Method::PATCH, &one, Some(json!({"visibility": "public"})), &a).await;
    assert_eq!((status, body["visibility"].as_str()), (StatusCode::OK, Some("public")), "{body}");
    assert_eq!(server.ids(&format!("/v1/files?owner={alice}"), &b).await, vec![file]);
    assert_eq!(server.download(&c, file, None).await.0, StatusCode::OK);
    assert_eq!(server.json(Method::PATCH, &one, Some(json!({"name": "mine.map"})), &b).await.0, StatusCode::FORBIDDEN);
    assert_eq!(server.json(Method::DELETE, &one, None, &b).await.0, StatusCode::FORBIDDEN);

    // Shared with bob: bob reads it, carol does not; the owner sees the list, bob does not.
    let (status, body) = server.json(Method::PATCH, &one, Some(json!({"visibility": "shared", "shared_with": [bob, bob, alice]})), &a).await;
    assert_eq!((status, body["shared_with"].clone()), (StatusCode::OK, json!([bob])), "{body}");
    let (status, seen) = server.json(Method::GET, &one, None, &b).await;
    assert_eq!((status, seen.get("shared_with").is_none()), (StatusCode::OK, true), "{seen}");
    assert_eq!(server.download(&c, file, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(server.ids(&format!("/v1/files?owner={alice}"), &b).await, vec![file]);
    assert!(server.ids(&format!("/v1/files?owner={alice}"), &c).await.is_empty());
    // Another field alone keeps the list; metadata null clears.
    let (_, body) = server.json(Method::PATCH, &one, Some(json!({"metadata": null})), &a).await;
    assert_eq!((body["shared_with"].clone(), body.get("metadata").is_none()), (json!([bob]), true), "{body}");
    assert_eq!(server.json(Method::PATCH, &one, Some(json!({"shared_with": [999_999]})), &a).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(server.json(Method::PATCH, &one, Some(json!({"visibility": "public", "shared_with": [bob]})), &a).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    // Friends: carol becomes alice's friend and reads it; bob (not a friend, no longer shared) does not.
    let (status, body) = server.json(Method::POST, routes::friends::REQUESTS, Some(json!({"user": carol})), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server.json(Method::POST, &format!("/v1/friends/requests/{alice}/accept"), None, &c).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server.json(Method::PATCH, &one, Some(json!({"visibility": "friends"})), &a).await;
    assert_eq!((status, body.get("shared_with").is_none()), (StatusCode::OK, true), "leaving `shared` drops the list: {body}");
    assert_eq!(server.download(&c, file, None).await.0, StatusCode::OK);
    assert_eq!(server.download(&b, file, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(server.json(Method::DELETE, &one, None, &b).await.0, StatusCode::NOT_FOUND, "a non-reader learns nothing");

    // Refusals leave nothing behind: a hook, a wrong SHA-256, bad names / types, missing parts.
    let before = stored_files(&server.dir);
    let (status, _) = server.upload(&a, &[Part::File { name: Some("refused.bin"), content_type: None, bytes: b"x" }]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server.upload(&a, &[Part::Meta(json!({"sha256": sha(b"other")})), Part::File { name: None, content_type: None, bytes: b"x" }]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    for meta in [json!({"name": "a/b"}), json!({"content_type": "text/html; charset=utf-8"}), json!({"visibility": "galaxy"}), json!({"shared_with": [bob]})] {
        let (status, body) = server.upload(&a, &[Part::Meta(meta.clone()), Part::File { name: None, content_type: None, bytes: b"x" }]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{meta}: {body}");
    }
    let (status, body) = server.upload(&a, &[Part::Meta(json!({}))]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(codes::VALIDATION_FAILED)));
    assert_eq!(server.upload(&a, &[Part::File { name: None, content_type: None, bytes: b"x" }, Part::Meta(json!({}))]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(server.upload(&a, &[Part::Other("extra")]).await.0, StatusCode::BAD_REQUEST);
    let not_multipart = Request::post(routes::files::LIST)
        .header("authorization", format!("Bearer {a}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("request");
    assert_eq!(server.raw(not_multipart).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(stored_files(&server.dir), before, "nothing stored by the refused uploads");
    assert_eq!(server.usage(&a).await.0, 3);

    // Pages, newest first.
    let first = server.ids("/v1/files?limit=2", &a).await;
    assert_eq!(first.len(), 2);
    let (_, page) = server.json(Method::GET, "/v1/files?limit=2", None, &a).await;
    let next = page["next_cursor"].as_str().expect("a next page").to_string();
    let rest = server.ids(&format!("/v1/files?limit=2&cursor={next}"), &a).await;
    assert_eq!(rest.len(), 1);
    assert!(first[0] > first[1] && first[1] > rest[0]);
    assert_eq!(server.json(Method::GET, "/v1/files?cursor=abc", None, &a).await.0, StatusCode::BAD_REQUEST);

    // Delete: the row and the bytes; then 404.
    let stored = stored_files(&server.dir);
    assert_eq!(server.json(Method::DELETE, &one, None, &a).await.0, StatusCode::OK);
    assert_eq!(server.json(Method::GET, &one, None, &a).await.0, StatusCode::NOT_FOUND);
    assert_eq!(stored_files(&server.dir), stored - 1);
    assert_eq!(server.usage(&a).await, (2, 4));
    let events = server.events.lock().expect("lock").clone();
    assert_eq!(events.first(), Some(&(file, FileChange::Uploaded)));
    assert_eq!(events.last(), Some(&(file, FileChange::Deleted)));
    assert!(events.contains(&(file, FileChange::Updated)));
    // Without a token: 401.
    let anonymous = Request::get(format!("/v1/files/{file}/content")).body(Body::empty()).expect("request");
    assert_eq!(server.raw(anonymous).await.0, StatusCode::UNAUTHORIZED);
}

async fn limits_and_concurrency(url: &str) {
    let server = start(url, |files| {
        files.max_file_bytes = 1000;
        files.max_files_per_user = 3;
        files.max_bytes_per_user = 2500;
    })
    .await;
    let (_, a) = server.register("limits@example.com").await;
    let kilo = vec![1u8; 1001];
    let (status, body) = server.upload(&a, &[Part::File { name: None, content_type: None, bytes: &kilo }]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some(codes::PAYLOAD_TOO_LARGE)), "{body}");
    server.upload_ok(&a, &[Part::File { name: None, content_type: None, bytes: &kilo[..900] }]).await;
    server.upload_ok(&a, &[Part::File { name: None, content_type: None, bytes: &kilo[..900] }]).await;
    let (status, body) = server.upload(&a, &[Part::File { name: None, content_type: None, bytes: &kilo[..900] }]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::QUOTA_EXCEEDED)), "the byte quota: {body}");
    server.upload_ok(&a, &[Part::File { name: None, content_type: None, bytes: &kilo[..10] }]).await;
    let (status, body) = server.upload(&a, &[Part::File { name: None, content_type: None, bytes: b"x" }]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::QUOTA_EXCEEDED)), "the file count: {body}");
    assert_eq!(server.usage(&a).await, (3, 1810));
    assert_eq!(stored_files(&server.dir), 3);

    // Concurrent uploads of a fresh player: exactly the file quota is stored.
    let (_, b) = server.register("racer@example.com").await;
    let server = Arc::new(server);
    let mut tasks = Vec::new();
    for n in 0..8u8 {
        let (server, b) = (server.clone(), b.clone());
        tasks.push(tokio::spawn(async move { server.upload(&b, &[Part::File { name: None, content_type: None, bytes: &[n; 50] }]).await.0 }));
    }
    let mut statuses = HashMap::new();
    for task in tasks {
        *statuses.entry(task.await.expect("task")).or_insert(0) += 1;
    }
    assert_eq!(statuses.get(&StatusCode::OK), Some(&3), "{statuses:?}");
    assert_eq!(statuses.get(&StatusCode::FORBIDDEN), Some(&5), "{statuses:?}");
    assert_eq!(server.usage(&b).await, (3, 150));
    assert_eq!(stored_files(&server.dir), 6, "the refused uploads left no bytes");
}

async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("files-file");
            (format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/")), None)
        }
        #[cfg(any(feature = "mysql", feature = "postgres"))]
        base => {
            let (url, name) = common::fresh_database(base).await;
            (url, Some((base.to_string(), name)))
        }
        #[cfg(not(any(feature = "mysql", feature = "postgres")))]
        other => panic!("no backend for {other}"),
    }
}

macro_rules! each {
    ($backend:expr, $($part:ident),* $(,)?) => {
        $(
            let (url, cleanup) = database($backend).await;
            $part(&url).await;
            #[cfg(any(feature = "mysql", feature = "postgres"))]
            if let Some((base, name)) = cleanup {
                common::drop_database(&base, &name).await;
            }
            #[cfg(not(any(feature = "mysql", feature = "postgres")))]
            let _ = cleanup;
        )*
    };
}

/// The settings hook refuses or changes a visibility; concurrent share-list changes and a change
/// racing a delete never answer 500.
async fn update_hook_and_lock(url: &str) {
    let server = start(url, |_| {}).await;
    let (_alice, a) = server.register("hook-a@example.com").await;
    let mut players = Vec::new();
    for n in 0..4 {
        players.push(server.register(&format!("hook-{n}@example.com")).await.0);
    }
    let info = server.upload_ok(&a, &[Part::File { name: Some("level.map"), content_type: None, bytes: b"level" }]).await;
    let one = format!("/v1/files/{}", info["id"].as_i64().expect("id"));
    let (status, body) = server.json(Method::PATCH, &one, Some(json!({"name": "never.map", "visibility": "public"})), &a).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::FORBIDDEN)), "{body}");
    let (_, now) = server.json(Method::GET, &one, None, &a).await;
    assert_eq!((now["name"].as_str(), now["visibility"].as_str()), (Some("level.map"), Some("private")), "a refused change changes nothing");
    let (status, body) = server.json(Method::PATCH, &one, Some(json!({"name": "pending.map", "visibility": "public"})), &a).await;
    assert_eq!((status, body["visibility"].as_str(), body["name"].as_str()), (StatusCode::OK, Some("private"), Some("pending.map")), "{body}");
    // Concurrent share lists: each answers 200, the last one wins.
    let changes = players.iter().map(|user| {
        let body = json!({"visibility": "shared", "shared_with": [user]});
        server.json(Method::PATCH, &one, Some(body), &a)
    });
    for (status, body) in futures_util::future::join_all(changes).await {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, now) = server.json(Method::GET, &one, None, &a).await;
    assert_eq!(now["shared_with"].as_array().map(Vec::len), Some(1), "{now}");
    // A change racing a delete: 200 or 404, never 500.
    let (patch, delete) = tokio::join!(
        server.json(Method::PATCH, &one, Some(json!({"visibility": "shared", "shared_with": [players[0]]})), &a),
        server.json(Method::DELETE, &one, None, &a)
    );
    assert!(patch.0 == StatusCode::OK || patch.0 == StatusCode::NOT_FOUND, "{patch:?}");
    assert_eq!(delete.0, StatusCode::OK, "{delete:?}");
    assert_eq!(server.json(Method::GET, &one, None, &a).await.0, StatusCode::NOT_FOUND);
}

async fn suite(backend: &str) {
    each!(backend, uploads_reads_and_settings, limits_and_concurrency, update_hook_and_lock);
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("memory").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    suite("file").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_files_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_files_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

// ---- slow uploads and the purge (SQLite) -----------------------------------------------------------

/// An upload body that sends `pieces` pieces of `piece` bytes, `pause` apart; with `stall`, it
/// stops sending after that many pieces (the connection stays open).
fn slow_body(pieces: usize, piece: usize, pause: Duration, stall: Option<usize>) -> Body {
    let head =
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"slow.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n");
    let tail = format!("\r\n--{BOUNDARY}--\r\n");
    let stream = futures_util::stream::unfold(0usize, move |n| {
        let (head, tail) = (head.clone(), tail.clone());
        async move {
            if n == 0 {
                return Some((Ok::<Bytes, io::Error>(Bytes::from(head)), 1));
            }
            if n > pieces + 1 {
                return None;
            }
            if stall.is_some_and(|after| n > after) {
                std::future::pending::<()>().await;
            }
            if n == pieces + 1 {
                return Some((Ok(Bytes::from(tail)), n + 1));
            }
            tokio::time::sleep(pause).await;
            Some((Ok(Bytes::from(vec![b'x'; piece])), n + 1))
        }
    });
    Body::from_stream(stream)
}

impl Server {
    async fn slow_upload(&self, token: &str, body: Body) -> (StatusCode, Value) {
        let request = Request::post(routes::files::LIST)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(body)
            .expect("request");
        let (status, _, bytes) = self.raw(request).await;
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// Wait until the store's `tmp` folder is empty (a cut write's part file is removed on drop).
async fn no_part_files(dir: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(dir.join("tmp")).map(|d| d.count()).unwrap_or(0) != 0 {
        assert!(std::time::Instant::now() < deadline, "a part file stayed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Uploads are timed by data, not by `http.request_timeout_secs`: a slow upload that keeps sending
/// finishes; one that stops sending fails after `http.upload_idle_timeout_secs`; one longer than
/// `http.upload_timeout_secs` fails; neither leaves bytes. The purge removes the bytes of an
/// account deleted straight from the database and keeps every file a row names.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_uploads_and_the_purge() {
    let timed = |config: &mut Config| {
        config.http.request_timeout_secs = 1;
        config.http.upload_idle_timeout_secs = 1;
        config.http.upload_timeout_secs = 0;
    };
    let server = start_with("sqlite::memory:", timed, |_| {}).await;
    let (ada, a) = server.register("slow-ada@example.com").await;
    let (_, b) = server.register("slow-bo@example.com").await;

    // 8 pieces 300 ms apart: 2.4 s, well past the 1 s request limit, never idle for 1 s.
    let started = std::time::Instant::now();
    let (status, body) = server.slow_upload(&a, slow_body(8, 1000, Duration::from_millis(300), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(started.elapsed() > Duration::from_secs(2));
    assert_eq!((body["size"].as_u64(), body["sha256"].as_str()), (Some(8000), Some(sha(&[b'x'; 8000]).as_str())), "{body}");

    // A body that stops: 503 after the idle limit, nothing kept.
    let (status, body) = server.slow_upload(&b, slow_body(8, 1000, Duration::from_millis(100), Some(2))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some(codes::UNAVAILABLE)), "{body}");
    assert!(body["error"]["message"].as_str().unwrap_or_default().contains("stalled"), "{body}");
    no_part_files(&server.dir).await;
    assert_eq!(server.usage(&b).await, (0, 0));
    assert_eq!(stored_files(&server.dir), 1, "only Ada's file");

    // The purge: Bo's file has a row (kept); Ada's account is deleted straight from the database.
    server.upload_ok(&b, &[Part::File { name: Some("kept.bin"), content_type: None, bytes: b"kept" }]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let service = server.prepared.state().get::<FileService>().expect("service");
    assert_eq!(service.purge_orphans(server.prepared.state(), Duration::ZERO).await.expect("purge"), 0, "every file has its row");
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(ada));
    server.prepared.state().db().execute(&delete).await.expect("delete the account");
    assert_eq!(stored_files(&server.dir), 2, "the bytes outlive the cascaded row");
    assert_eq!(service.purge_orphans(server.prepared.state(), Duration::from_secs(3600)).await.expect("purge"), 0, "too young");
    assert_eq!(service.purge_orphans(server.prepared.state(), Duration::ZERO).await.expect("purge"), 1);
    assert_eq!(stored_files(&server.dir), 1);
    assert_eq!(server.usage(&b).await, (1, 4));

    // The overall upload limit.
    let capped = |config: &mut Config| {
        config.http.request_timeout_secs = 1;
        config.http.upload_idle_timeout_secs = 1;
        config.http.upload_timeout_secs = 1;
    };
    let server = start_with("sqlite::memory:", capped, |_| {}).await;
    let (_, c) = server.register("slow-cy@example.com").await;
    let (status, body) = server.slow_upload(&c, slow_body(10, 1000, Duration::from_millis(300), None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body["error"]["message"].as_str().unwrap_or_default().contains("took too long"), "{body}");
    no_part_files(&server.dir).await;
    assert_eq!(stored_files(&server.dir), 0);
}

// ---- wiring (SQLite) ---------------------------------------------------------------------------------

/// A game's own store: bytes in memory.
#[derive(Default)]
struct MemoryStore {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl FileStore for MemoryStore {
    fn put<'a>(&'a self, key: &'a str, mut data: ByteStream<'a>) -> BoxFuture<'a, io::Result<u64>> {
        Box::pin(async move {
            let mut bytes = Vec::new();
            while let Some(chunk) = data.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            let n = bytes.len() as u64;
            self.files.lock().expect("lock").insert(key.to_string(), bytes);
            Ok(n)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<ByteStream<'static>>> {
        Box::pin(async move {
            let bytes = self.files.lock().expect("lock").get(key).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(futures_util::stream::once(async move { Ok(Bytes::from(bytes)) }).boxed())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.files.lock().expect("lock").remove(key);
            Ok(())
        })
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_documents_rate_and_stores() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Files::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let dir = common::temp_dir("files-wiring").display().to_string().replace('\\', "/");
    let file =
        Config::from_toml_str(&format!("[database]\nurl = \"sqlite::memory:\"\n[modules.files]\ndir = \"{dir}\"\nmax_files_per_user = 7\n")).expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Files::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<net_backend_server::files::FileService>().expect("service").config().max_files_per_user, 7);
    let twice = NetBackendServer::new(file).module(Auth::new()).module(Files::new().with_config(FilesConfig::default())).build().await.err();
    assert!(twice.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let mut big = FilesConfig::default();
    big.max_file_bytes = 64 * 1024 * 1024;
    let error = NetBackendServer::new(config())
        .module(Auth::new())
        .module(Files::new().with_config(big))
        .build()
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("http.max_body_bytes"), "{error}");

    // Documents, /v1/info, the `friends` visibility without the friends module, the rate, a game's store.
    let store = Arc::new(MemoryStore::default());
    let mut settings = FilesConfig::default();
    settings.upload_rate = 2;
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.rate_limits = false;
    let prepared = NetBackendServer::new(config())
        .module(Auth::new().with_config(auth))
        .module(Files::new().with_config(settings).store(StoreRef(store.clone())))
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for (path, method) in [
        (routes::files::LIST, "post"),
        (routes::files::LIST, "get"),
        (routes::files::CONTENT, "get"),
        (routes::files::ONE, "patch"),
        (routes::files::USAGE, "get"),
    ] {
        assert!(spec["paths"][path][method].is_object(), "{method} {path}");
    }
    let server = Server { router: prepared.router(), prepared, dir: PathBuf::new(), events: Arc::new(Mutex::new(Vec::new())) };
    let (_, info, body) = common::call(&server.router, common::get(routes::INFO)).await;
    assert_eq!(body["modules"], json!(["auth", "files"]), "{info:?}");
    let (_, a) = server.register("wiring@example.com").await;
    let (status, body) = server.upload(&a, &[Part::Meta(json!({"visibility": "friends"})), Part::File { name: None, content_type: None, bytes: b"x" }]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let stored = server.upload_ok(&a, &[Part::File { name: Some("a.txt"), content_type: Some("text/plain"), bytes: b"in memory" }]).await;
    assert_eq!(store.files.lock().expect("lock").len(), 1, "the game's store holds the bytes");
    let (status, _, bytes) = server.download(&a, stored["id"].as_i64().unwrap_or(0), None).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, &b"in memory"[..]));
    let (status, body) = server.upload(&a, &[Part::File { name: None, content_type: None, bytes: b"x" }]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    let _ = server.prepared.state();
}

/// A shared handle to the memory store (the test keeps one to look inside).
struct StoreRef(Arc<MemoryStore>);

impl FileStore for StoreRef {
    fn put<'a>(&'a self, key: &'a str, data: ByteStream<'a>) -> BoxFuture<'a, io::Result<u64>> {
        self.0.put(key, data)
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<ByteStream<'static>>> {
        self.0.get(key)
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.0.delete(key)
    }
}
