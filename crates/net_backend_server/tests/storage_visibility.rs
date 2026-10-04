//! Storage objects other players read: the visibility of a write (private by default, public,
//! friends), kept by later writes without one, set in a batch; another player's object and
//! collection through `/v1/users/{user}/storage/...` (404 / left out when they may not read it);
//! `friends` without the friends module refused.
//!
//! Runs on SQLite (in memory) here, and on MySQL / PostgreSQL with `NBS_TEST_MYSQL_URL` /
//! `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "storage", feature = "friends", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use axum::body::Body;
use axum::Router;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::friends::{Friends, FriendsConfig};
use net_backend_server::protocol::routes;
use net_backend_server::storage::{Storage, StorageConfig};
use net_backend_server::{Config, NetBackendServer, SecretString};
use serde_json::{json, Value};

const PASSWORD: &str = "correct horse battery";

fn auth_config() -> AuthConfig {
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth
}

async fn start(url: &str, friends: bool) -> Router {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("storage-vis-migrations");
    let mut storage = StorageConfig::default();
    storage.write_rate = 0;
    let mut server = NetBackendServer::new(config).module(Auth::new().with_config(auth_config()));
    if friends {
        let mut settings = FriendsConfig::default();
        settings.request_rate = 0;
        server = server.module(Friends::new().with_config(settings));
    }
    let prepared = server.module(Storage::new().with_config(storage)).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    prepared.router()
}

async fn send(router: &Router, method: Method, path: &str, body: Option<Value>, token: &str) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let (status, _, body) = common::call(router, request).await;
    (status, body)
}

async fn register(router: &Router, email: &str) -> (i64, String) {
    let (status, _, body) = common::call(router, common::post_json(routes::auth::REGISTER, json!({"email": email, "password": PASSWORD}).to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (body["account"]["id"].as_i64().expect("id"), body["tokens"]["access_token"].as_str().expect("token").to_string())
}

async fn keys(router: &Router, path: &str, token: &str) -> Vec<String> {
    let (status, body) = send(router, Method::GET, path, None, token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["items"].as_array().map(|items| items.iter().filter_map(|i| i["key"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}

async fn visibility_rules(url: &str) {
    let router = start(url, true).await;
    let (alice, a) = register(&router, "alice@example.com").await;
    let (_, b) = register(&router, "bob@example.com").await;
    let (carol, c) = register(&router, "carol@example.com").await;

    // Public, private (the default), friends.
    let put = |key: &str| format!("/v1/storage/profile/{key}");
    let (status, body) = send(&router, Method::PUT, &put("card"), Some(json!({"value": {"title": "Ace"}, "visibility": "public"})), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    send(&router, Method::PUT, &put("secret"), Some(json!({"value": 1})), &a).await;
    send(&router, Method::PUT, &put("pals"), Some(json!({"value": 2, "visibility": "friends"})), &a).await;
    let (_, own) = send(&router, Method::GET, &put("card"), None, &a).await;
    assert_eq!(own["visibility"], "public");
    let (_, own) = send(&router, Method::GET, &put("secret"), None, &a).await;
    assert_eq!(own["visibility"], "private");

    // Bob reads the public one only.
    let other = |key: &str| format!("/v1/users/{alice}/storage/profile/{key}");
    let (status, card) = send(&router, Method::GET, &other("card"), None, &b).await;
    assert_eq!((status, card["value"]["title"].as_str(), card["owner"].as_i64()), (StatusCode::OK, Some("Ace"), Some(alice)), "{card}");
    assert_eq!(send(&router, Method::GET, &other("secret"), None, &b).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&router, Method::GET, &other("pals"), None, &b).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&router, Method::GET, &other("nothing"), None, &b).await.0, StatusCode::NOT_FOUND);
    let list = format!("/v1/users/{alice}/storage/profile");
    assert_eq!(keys(&router, &list, &b).await, vec!["card".to_string()]);
    assert_eq!(keys(&router, &list, &a).await, vec!["card".to_string(), "pals".to_string(), "secret".to_string()], "the owner sees all");

    // Carol becomes a friend and reads `friends` objects too.
    let (status, body) = send(&router, Method::POST, routes::friends::REQUESTS, Some(json!({"user": carol})), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(&router, Method::POST, &format!("/v1/friends/requests/{alice}/accept"), None, &c).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(send(&router, Method::GET, &other("pals"), None, &c).await.0, StatusCode::OK);
    assert_eq!(keys(&router, &list, &c).await, vec!["card".to_string(), "pals".to_string()]);

    // A write without a visibility keeps it; a batch sets it; private again hides it.
    send(&router, Method::PUT, &put("card"), Some(json!({"value": {"title": "King"}})), &a).await;
    assert_eq!(send(&router, Method::GET, &other("card"), None, &b).await.1["value"]["title"], "King");
    let batch = json!({"objects": [{"collection": "levels", "key": "one", "value": [1], "visibility": "public"}]});
    let (status, body) = send(&router, Method::POST, routes::storage::BATCH_PUT, Some(batch), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(keys(&router, &format!("/v1/users/{alice}/storage/levels"), &b).await, vec!["one".to_string()]);
    send(&router, Method::PUT, &put("card"), Some(json!({"value": {}, "visibility": "private"})), &a).await;
    assert_eq!(send(&router, Method::GET, &other("card"), None, &b).await.0, StatusCode::NOT_FOUND);
    let (status, body) = send(&router, Method::PUT, &put("card"), Some(json!({"value": {}, "visibility": "galaxy"})), &a).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Nobody writes another player's objects through these paths.
    assert_eq!(send(&router, Method::PUT, &other("card"), Some(json!({"value": {}})), &b).await.0, StatusCode::METHOD_NOT_ALLOWED);
}

async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        #[cfg(any(feature = "mysql", feature = "postgres"))]
        base => {
            let (url, name) = common::fresh_database(base).await;
            (url, Some((base.to_string(), name)))
        }
        #[cfg(not(any(feature = "mysql", feature = "postgres")))]
        other => panic!("no backend for {other}"),
    }
}

async fn suite(backend: &str) {
    let (url, cleanup) = database(backend).await;
    visibility_rules(&url).await;
    #[cfg(any(feature = "mysql", feature = "postgres"))]
    if let Some((base, name)) = cleanup {
        common::drop_database(&base, &name).await;
    }
    #[cfg(not(any(feature = "mysql", feature = "postgres")))]
    let _ = cleanup;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("memory").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_storage_visibility_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_storage_visibility_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// Without the friends module `friends` is refused.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn without_the_friends_module() {
    let router = start("sqlite::memory:", false).await;
    let (_, a) = register(&router, "solo@example.com").await;
    let (status, body) = send(&router, Method::PUT, "/v1/storage/profile/card", Some(json!({"value": 1, "visibility": "friends"})), &a).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = send(&router, Method::PUT, "/v1/storage/profile/card", Some(json!({"value": 1, "visibility": "public"})), &a).await;
    assert_eq!(status, StatusCode::OK);
}

/// The write hook sees the visibility: it refuses `public` in one collection, turns it into
/// `private` in another (the game reviews first), and a hook's `friends` without the friends
/// module is refused like a client's.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_write_hook_sees_and_changes_the_visibility() {
    use net_backend_server::hooks::Decision;
    use net_backend_server::protocol::storage::ObjectVisibility;
    use net_backend_server::storage::events::BeforeStorageWrite;
    use net_backend_server::AppError;

    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    let mut storage = StorageConfig::default();
    storage.write_rate = 0;
    let prepared = NetBackendServer::new(config)
        .module(Auth::new().with_config(auth_config()))
        .module(Storage::new().with_config(storage))
        .before::<BeforeStorageWrite, _, _>(|_ctx, mut write| async move {
            match (write.collection.as_str(), write.visibility) {
                ("drafts", Some(ObjectVisibility::Public)) => return Ok(Decision::Reject(AppError::forbidden("drafts stay private"))),
                ("pending", Some(ObjectVisibility::Public)) => write.visibility = Some(ObjectVisibility::Private),
                ("odd", _) => write.visibility = Some(ObjectVisibility::Friends),
                _ => {}
            }
            Ok(Decision::Continue(write))
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let (alice, a) = register(&router, "hooked-a@example.com").await;
    let (_, b) = register(&router, "hooked-b@example.com").await;
    let public = json!({"value": 1, "visibility": "public"});
    let (status, body) = send(&router, Method::PUT, "/v1/storage/drafts/one", Some(public.clone()), &a).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send(&router, Method::PUT, "/v1/storage/pending/one", Some(public.clone()), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, own) = send(&router, Method::GET, "/v1/storage/pending/one", None, &a).await;
    assert_eq!(own["visibility"], "private", "the hook kept it private");
    assert_eq!(send(&router, Method::GET, &format!("/v1/users/{alice}/storage/pending/one"), None, &b).await.0, StatusCode::NOT_FOUND);
    let (status, body) = send(&router, Method::PUT, "/v1/storage/odd/one", Some(json!({"value": 1})), &a).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "the hook's visibility is checked again: {body}");
    let (status, body) = send(&router, Method::PUT, "/v1/storage/other/one", Some(public), &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
