//! The storage module through the assembled router (in process): every route, versions and
//! conditions (body and headers), isolation between users, batches, limits and quotas, the server
//! write lock, the hooks (before / in_tx / after), audited admin access, the concurrency races
//! (one winner among simultaneous conditional writes, the quota under concurrent creates, first
//! saves of many players at once: the MySQL gap-lock deadlock, also as an env-gated stress test),
//! the byte quota, the write rate and the server-owned collections.
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "storage", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::Router;
use http::{HeaderMap, Method, Request, StatusCode};
use net_backend_server::auth::audit::{self, AuditRecord};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::admin::{AdminPutObject, AuditQuery};
use net_backend_server::protocol::storage::{ObjectVersion, WriteAccess};
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
use net_backend_server::storage::events::{AfterStorageDelete, AfterStorageWrite, BeforeStorageWrite, InStorageWriteTx, Writer};
use net_backend_server::storage::{Storage, StorageConfig, StorageService};
use net_backend_server::{AppError, AppState, Config, ManualClock, NetBackendServer, PreparedServer, SecretString};
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";

#[derive(Default)]
struct Counters {
    after_writes: AtomicUsize,
    after_deletes: AtomicUsize,
}

struct Fx {
    prepared: PreparedServer,
    router: Router,
    counters: Arc<Counters>,
}

async fn fixture(url: &str, tweak: impl FnOnce(&mut StorageConfig)) -> Fx {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("storage-migrations");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    let mut storage = StorageConfig::default();
    tweak(&mut storage);
    let counters = Arc::new(Counters::default());
    let (c1, c2) = (counters.clone(), counters.clone());
    let server = NetBackendServer::new(config)
        .clock(ManualClock::new(UnixMillis(T0)))
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Storage::new().with_config(storage))
        // A game rule: saves need a level; `rewrite` gets a marker added; `forbidden` is refused.
        .before::<BeforeStorageWrite, _, _>(|_ctx, mut write| async move {
            if write.collection == "saves" && write.value.get("level").is_none() {
                return Ok(Decision::Reject(AppError::bad_request("a save needs a level")));
            }
            if write.key == "rewrite" {
                write.value["checked"] = json!(true);
            }
            if write.key == "forbidden" {
                return Ok(Decision::Reject(AppError::forbidden("not this key")));
            }
            Ok(Decision::Continue(write))
        })
        // Inside the transaction: an audit row written with the save; `tx-refused` rolls both back.
        .in_tx::<InStorageWriteTx, _>(|tx, _ctx, write| {
            Box::pin(async move {
                let entry = AuditRecord::new("game.save_written").target_user(write.user_id).data(json!({ "key": write.key, "version": write.version.get() }));
                audit::record_tx(tx, UnixMillis(T0), &entry).await?;
                if write.key == "tx-refused" {
                    return Err(AppError::conflict("the inventory does not match"));
                }
                Ok(())
            })
        })
        .after::<AfterStorageWrite, _, _>(move |_ctx, _event| {
            let counters = c1.clone();
            async move {
                counters.after_writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .after::<AfterStorageDelete, _, _>(move |_ctx, _event| {
            let counters = c2.clone();
            async move {
                counters.after_deletes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
    let prepared = server.build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    Fx { prepared, router, counters }
}

impl Fx {
    fn state(&self) -> &AppState {
        self.prepared.state()
    }

    fn storage(&self) -> Arc<StorageService> {
        self.state().get::<StorageService>().expect("storage service")
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, token: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
        send(&self.router, method, path, body, token, headers).await
    }

    async fn register(&self, email: &str) -> (UserId, String) {
        let request = Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": email, "password": PASSWORD}).to_string()))
            .expect("request");
        let (status, _, body) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn audit_actions(&self, user: UserId) -> Vec<String> {
        let service = self.state().get::<AuthService>().expect("auth");
        let page = service.audit_log(self.state(), &AuditQuery::new().with_user(user).with_limit(100)).await.expect("audit");
        page.items.into_iter().map(|e| e.action).collect()
    }
}

async fn send(router: &Router, method: Method, path: &str, body: Option<Value>, token: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 32 << 20).await.expect("body");
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn object(collection: &str, key: &str) -> String {
    routes::storage_object_path(collection, key).expect("valid names")
}

fn etag(headers: &HeaderMap) -> Option<&str> {
    headers.get("etag").and_then(|v| v.to_str().ok())
}

// ---- the suite ----------------------------------------------------------------------------------

async fn crud_and_versions(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (ada, token) = fx.register("ada@example.com").await;
    let path = object("saves", "slot-1");
    // Create, read, update.
    let (status, headers, ack) = fx.send(Method::PUT, &path, Some(json!({"value": {"level": 3}})), &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!((ack["version"].as_i64(), etag(&headers)), (Some(1), Some("\"1\"")));
    let (status, headers, got) = fx.send(Method::GET, &path, None, &token, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag(&headers), Some("\"1\""));
    assert_eq!((got["value"].clone(), got["owner"].as_i64(), got["write"].as_str()), (json!({"level": 3}), Some(ada.get()), Some("owner")));
    assert_eq!(got["updated_at"], T0);
    let (status, _, ack) = fx.send(Method::PUT, &path, Some(json!({"value": {"level": 4}})), &token, &[]).await;
    assert_eq!((status, ack["version"].as_i64()), (StatusCode::OK, Some(2)), "last write wins without a condition");
    // Listings: no values, ordered by key, paged.
    for key in ["slot-3", "slot-2"] {
        assert_eq!(fx.send(Method::PUT, &object("saves", key), Some(json!({"value": {"level": 1}})), &token, &[]).await.0, StatusCode::OK);
    }
    let list = routes::storage_collection_path("saves").expect("path");
    let (status, _, page) = fx.send(Method::GET, &format!("{list}?limit=2"), None, &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let keys: Vec<&str> = page["items"].as_array().map(|a| a.iter().filter_map(|i| i["key"].as_str()).collect()).unwrap_or_default();
    assert_eq!(keys, ["slot-1", "slot-2"]);
    assert!(page["items"][0].get("value").is_none(), "listings carry no values");
    assert_eq!(page["items"][0]["size_bytes"], 11);
    assert_eq!(page["next_cursor"], "slot-2");
    let (_, _, rest) = fx.send(Method::GET, &format!("{list}?cursor=slot-2"), None, &token, &[]).await;
    assert_eq!(rest["items"].as_array().map(Vec::len), Some(1));
    assert!(rest.get("next_cursor").is_none());
    assert_eq!(fx.send(Method::GET, &format!("{list}?cursor=..%2F"), None, &token, &[]).await.0, StatusCode::BAD_REQUEST);
    // Delete: idempotent.
    assert_eq!(fx.send(Method::DELETE, &path, None, &token, &[]).await.0, StatusCode::OK);
    assert_eq!(fx.send(Method::GET, &path, None, &token, &[]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(fx.send(Method::DELETE, &path, None, &token, &[]).await.0, StatusCode::OK, "deleting an absent object is fine");
    // Another user sees nothing of Ada's.
    let (_, bo) = fx.register("bo@example.com").await;
    assert_eq!(fx.send(Method::GET, &object("saves", "slot-2"), None, &bo, &[]).await.0, StatusCode::NOT_FOUND);
    let (_, _, empty) = fx.send(Method::GET, &list, None, &bo, &[]).await;
    assert_eq!(empty["items"], json!([]));
    // Without a token: 401.
    let (status, _, _) = common::call(&fx.router, common::get(&path)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.counters.after_writes.load(Ordering::SeqCst), 4);
    assert_eq!(fx.counters.after_deletes.load(Ordering::SeqCst), 2);
}

async fn conditions(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (_, token) = fx.register("cond@example.com").await;
    let path = object("saves", "slot");
    let put = |value: Value| Some(json!({"value": value}));
    // Create-only: body `if_version: 0` or `If-None-Match: *`.
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"level": 1}, "if_version": 0})), &token, &[]).await.0, StatusCode::OK);
    let (status, _, body) = fx.send(Method::PUT, &path, put(json!({"level": 1})), &token, &[("if-none-match", "*")]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some(codes::VERSION_CONFLICT)));
    assert_eq!(body["error"]["details"], json!({"current_version": 1}));
    // Conditional update: the right version wins, a stale one is refused and changes nothing.
    let (status, headers, _) = fx.send(Method::PUT, &path, put(json!({"level": 2})), &token, &[("if-match", "\"1\"")]).await;
    assert_eq!((status, etag(&headers)), (StatusCode::OK, Some("\"2\"")));
    let (status, _, body) = fx.send(Method::PUT, &path, Some(json!({"value": {"level": 9}, "if_version": 1})), &token, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["details"], json!({"current_version": 2}));
    let (_, _, stored) = fx.send(Method::GET, &path, None, &token, &[]).await;
    assert_eq!(stored["value"], json!({"level": 2}));
    // A condition on an object that does not exist: a conflict without a current version.
    let (status, _, body) = fx.send(Method::PUT, &object("saves", "none"), Some(json!({"value": {"level": 1}, "if_version": 4})), &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::CONFLICT, json!({})));
    // Header and body must agree; malformed headers are refused.
    assert_eq!(
        fx.send(Method::PUT, &path, Some(json!({"value": {"level": 3}, "if_version": 1})), &token, &[("if-match", "\"2\"")]).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"level": 3}, "if_version": 2})), &token, &[("if-match", "\"2\"")]).await.0, StatusCode::OK);
    for bad in ["*", "3", "W/\"3\""] {
        assert_eq!(fx.send(Method::PUT, &path, put(json!({"level": 3})), &token, &[("if-match", bad)]).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    // Conditional deletes.
    let (status, _, body) = fx.send(Method::DELETE, &format!("{path}?if_version=1"), None, &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::CONFLICT, json!({"current_version": 3})));
    assert_eq!(fx.send(Method::DELETE, &path, None, &token, &[("if-none-match", "*")]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(fx.send(Method::DELETE, &path, None, &token, &[("if-match", "\"3\"")]).await.0, StatusCode::OK);
    let (status, _, body) = fx.send(Method::DELETE, &format!("{path}?if_version=3"), None, &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::CONFLICT, json!({})), "a named version of an absent object");
}

async fn batches(url: &str) {
    let fx = fixture(url, |c| {
        c.max_object_bytes = 512 * 1024;
        c.max_bytes_per_user = 64 << 20;
    })
    .await;
    let (_, token) = fx.register("batch@example.com").await;
    let item = |key: &str, value: Value| json!({"collection": "inv", "key": key, "value": value});
    // A batch put is one transaction: answers in request order.
    let (status, _, acks) =
        fx.send(Method::POST, routes::storage::BATCH_PUT, Some(json!({"objects": [item("a", json!(1)), item("b", json!(2))]})), &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{acks}");
    assert_eq!(acks["objects"].as_array().map(|a| a.iter().map(|o| o["key"].clone()).collect::<Vec<_>>()), Some(vec![json!("a"), json!("b")]));
    // All or nothing: the failing item is named, nothing of the batch is written.
    let failing = json!({"objects": [item("c", json!(3)), {"collection": "inv", "key": "a", "value": 9, "if_version": 7}]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(failing), &token, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["details"], json!({"index": 1, "current_version": 1}));
    assert_eq!(fx.send(Method::GET, &object("inv", "c"), None, &token, &[]).await.0, StatusCode::NOT_FOUND, "rolled back");
    // A hook's refusal and an in_tx refusal name their item too.
    let refused = json!({"objects": [item("c", json!(3)), item("forbidden", json!(1))]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(refused), &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::FORBIDDEN, json!({"index": 1})), "{body}");
    let refused = json!({"objects": [item("c", json!(3)), item("d", json!(4)), item("tx-refused", json!(1))]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(refused), &token, &[]).await;
    assert_eq!((status, body["error"]["details"]["index"].as_u64()), (StatusCode::CONFLICT, Some(2)), "{body}");
    assert_eq!(fx.send(Method::GET, &object("inv", "c"), None, &token, &[]).await.0, StatusCode::NOT_FOUND, "rolled back");
    // Batch get: request order, missing ones absent.
    let refs = json!({"objects": [{"collection": "inv", "key": "b"}, {"collection": "inv", "key": "zz"}, {"collection": "inv", "key": "a"}]});
    let (status, _, got) = fx.send(Method::POST, routes::storage::BATCH_GET, Some(refs), &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["objects"].as_array().map(|a| a.iter().map(|o| o["value"].clone()).collect::<Vec<_>>()), Some(vec![json!(2), json!(1)]));
    // Duplicates, empty and oversized batches are refused.
    let dup = json!({"objects": [{"collection": "inv", "key": "a"}, {"collection": "inv", "key": "a"}]});
    assert_eq!(fx.send(Method::POST, routes::storage::BATCH_GET, Some(dup), &token, &[]).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(fx.send(Method::POST, routes::storage::BATCH_PUT, Some(json!({"objects": []})), &token, &[]).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let seventeen: Vec<Value> = (0..17).map(|i| item(&format!("k{i}"), json!(i))).collect();
    assert_eq!(fx.send(Method::POST, routes::storage::BATCH_PUT, Some(json!({"objects": seventeen})), &token, &[]).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    // A batch READ over 4 MiB of values: 413 (9 objects of ~500 KiB).
    let big = "x".repeat(500 * 1024);
    for n in 0..9 {
        let (status, _, body) = fx.send(Method::PUT, &object("big", &format!("b{n}")), Some(json!({"value": big})), &token, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let refs: Vec<Value> = (0..9).map(|n| json!({"collection": "big", "key": format!("b{n}")})).collect();
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_GET, Some(json!({"objects": refs})), &token, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some(codes::PAYLOAD_TOO_LARGE)));
}

async fn limits_and_quota(url: &str) {
    let fx = fixture(url, |c| {
        c.max_object_bytes = 1024;
        c.max_objects_per_user = 3;
    })
    .await;
    let (_, token) = fx.register("limits@example.com").await;
    // Size: the value's JSON.
    let (status, _, body) = fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": "x".repeat(1100)})), &token, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(codes::VALIDATION_FAILED)));
    // Names: a path segment that is not a storage name is a bad request.
    // (`/v1/storage/_batch/get` itself is the POST route: a GET there is 405.)
    for path in ["/v1/storage/_x/k", "/v1/storage/a%20b/c", "/v1/storage/a/..", "/v1/storage/a/b%2Fc"] {
        assert_eq!(fx.send(Method::GET, path, None, &token, &[]).await.0, StatusCode::BAD_REQUEST, "{path}");
    }
    // Quota: 3 objects; updating one of them is still fine, a 4th is not.
    for key in ["a", "b", "c"] {
        assert_eq!(fx.send(Method::PUT, &object("misc", key), Some(json!({"value": 1})), &token, &[]).await.0, StatusCode::OK);
    }
    let (status, _, body) = fx.send(Method::PUT, &object("misc", "d"), Some(json!({"value": 1})), &token, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::QUOTA_EXCEEDED)));
    assert_eq!(fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": 2})), &token, &[]).await.0, StatusCode::OK);
    let batch = json!({"objects": [{"collection": "misc", "key": "b", "value": 5}, {"collection": "misc", "key": "e", "value": 5}]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(batch), &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::FORBIDDEN, json!({"index": 1})), "{body}");
    assert_eq!(fx.send(Method::DELETE, &object("misc", "c"), None, &token, &[]).await.0, StatusCode::OK);
    assert_eq!(fx.send(Method::PUT, &object("misc", "d"), Some(json!({"value": 1})), &token, &[]).await.0, StatusCode::OK, "room again");
    // The body limit of a PUT: far over the value limit, refused before parsing.
    let huge = "y".repeat(400 * 1024);
    assert_eq!(fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": huge})), &token, &[]).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

async fn server_lock_and_service(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (user, token) = fx.register("lock@example.com").await;
    let storage = fx.storage();
    let path = object("wallet", "gold");
    // Server code writes a locked object; the owner reads but cannot write or delete it.
    let ack =
        storage.put(fx.state(), user, "wallet", "gold", AdminPutObject::new(json!({"gold": 100})).with_write(WriteAccess::Server)).await.expect("server write");
    assert_eq!(ack.version, ObjectVersion(1));
    let (status, _, got) = fx.send(Method::GET, &path, None, &token, &[]).await;
    assert_eq!((status, got["write"].as_str()), (StatusCode::OK, Some("server")));
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"gold": 999999}})), &token, &[]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"gold": 999999}, "if_version": 1})), &token, &[]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(fx.send(Method::DELETE, &path, None, &token, &[]).await.0, StatusCode::FORBIDDEN);
    let batch = json!({"objects": [{"collection": "wallet", "key": "other", "value": 1}, {"collection": "wallet", "key": "gold", "value": 1}]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(batch), &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::FORBIDDEN, json!({"index": 1})));
    // The server still writes (keeping the lock) and reads.
    let ack =
        storage.put(fx.state(), user, "wallet", "gold", AdminPutObject::new(json!({"gold": 90})).if_version(ObjectVersion(1))).await.expect("server update");
    assert_eq!(ack.version, ObjectVersion(2));
    let object = storage.get(fx.state(), user, "wallet", "gold").await.expect("get").expect("exists");
    assert_eq!((object.value, object.write), (json!({"gold": 90}), WriteAccess::Server));
    let stale = storage.put(fx.state(), user, "wallet", "gold", AdminPutObject::new(json!(0)).if_version(ObjectVersion(1))).await;
    assert_eq!(stale.err().map(|e| e.code().to_string()).as_deref(), Some(codes::VERSION_CONFLICT));
    // Unlocking hands it back to the owner; the server deletes too.
    storage.put(fx.state(), user, "wallet", "gold", AdminPutObject::new(json!({"gold": 1})).with_write(WriteAccess::Owner)).await.expect("unlock");
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"gold": 2}})), &token, &[]).await.0, StatusCode::OK);
    storage.delete(fx.state(), user, "wallet", "gold", None).await.expect("server delete");
    assert!(storage.get(fx.state(), user, "wallet", "gold").await.expect("get").is_none());
    // Server writes for an account that does not exist: 404.
    let missing = storage.put(fx.state(), UserId(999_999), "wallet", "gold", AdminPutObject::new(json!(1))).await;
    assert_eq!(missing.err().map(|e| e.status()), Some(StatusCode::NOT_FOUND));
    assert!(storage.get(fx.state(), user, "../x", "y").await.is_err(), "names are checked for server code too");
}

async fn hooks(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (user, token) = fx.register("hooks@example.com").await;
    // Before: refuse and rewrite.
    let (status, _, body) = fx.send(Method::PUT, &object("saves", "x"), Some(json!({"value": {"nolevel": true}})), &token, &[]).await;
    assert_eq!((status, body["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("a save needs a level")));
    assert_eq!(fx.send(Method::PUT, &object("misc", "forbidden"), Some(json!({"value": 1})), &token, &[]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(fx.send(Method::PUT, &object("misc", "rewrite"), Some(json!({"value": {"a": 1}})), &token, &[]).await.0, StatusCode::OK);
    let (_, _, got) = fx.send(Method::GET, &object("misc", "rewrite"), None, &token, &[]).await;
    assert_eq!(got["value"], json!({"a": 1, "checked": true}));
    // In the transaction: the game's row is written with the object, or both roll back.
    let (status, _, body) = fx.send(Method::PUT, &object("misc", "tx-refused"), Some(json!({"value": 1})), &token, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some(codes::CONFLICT)));
    assert_eq!(fx.send(Method::GET, &object("misc", "tx-refused"), None, &token, &[]).await.0, StatusCode::NOT_FOUND);
    let actions = fx.audit_actions(user).await;
    assert_eq!(actions.iter().filter(|a| *a == "game.save_written").count(), 1, "only the committed write left its row: {actions:?}");
}

async fn admin_access(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (player, player_token) = fx.register("player@example.com").await;
    let (boss, boss_token) = fx.register("boss@example.com").await;
    fx.state().get::<AuthService>().expect("auth").set_user_role(fx.state(), boss, "admin", true).await.expect("admin");
    assert_eq!(fx.send(Method::PUT, &object("saves", "s1"), Some(json!({"value": {"level": 1}})), &player_token, &[]).await.0, StatusCode::OK);
    let one = routes::admin_object_path(player, "saves", "s1").expect("path");
    let list = routes::admin_storage_path(player, "saves").expect("path");
    // Players are not admins.
    assert_eq!(fx.send(Method::GET, &one, None, &player_token, &[]).await.0, StatusCode::FORBIDDEN);
    // Admins read, list, write (with the lock) and delete; every access is audited.
    let (status, headers, got) = fx.send(Method::GET, &one, None, &boss_token, &[]).await;
    assert_eq!((status, etag(&headers)), (StatusCode::OK, Some("\"1\"")));
    assert_eq!(got["value"], json!({"level": 1}));
    let (status, _, page) = fx.send(Method::GET, &list, None, &boss_token, &[]).await;
    assert_eq!((status, page["items"].as_array().map(Vec::len)), (StatusCode::OK, Some(1)));
    let (status, _, ack) = fx.send(Method::PUT, &one, Some(json!({"value": {"level": 50}, "write": "server", "if_version": 1})), &boss_token, &[]).await;
    assert_eq!((status, ack["version"].as_i64()), (StatusCode::OK, Some(2)), "{ack}");
    assert_eq!(fx.send(Method::PUT, &object("saves", "s1"), Some(json!({"value": {"level": 99}})), &player_token, &[]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(fx.send(Method::PUT, &one, Some(json!({"value": 1, "write": "moderators"})), &boss_token, &[]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(fx.send(Method::DELETE, &one, None, &boss_token, &[]).await.0, StatusCode::OK);
    assert_eq!(fx.send(Method::GET, &one, None, &boss_token, &[]).await.0, StatusCode::NOT_FOUND);
    let actions = fx.audit_actions(player).await;
    for action in ["admin.storage_read", "admin.storage_list", "admin.storage_write", "admin.storage_delete"] {
        assert!(actions.iter().any(|a| a == action), "{action} missing: {actions:?}");
    }
}

/// Simultaneous conditional writes: exactly one wins, every other gets 409. Simultaneous creates
/// never pass the quota.
async fn races(url: &str) {
    let fx = fixture(url, |c| c.max_objects_per_user = 5).await;
    let (_, token) = fx.register("race@example.com").await;
    let path = object("saves", "contested");
    assert_eq!(fx.send(Method::PUT, &path, Some(json!({"value": {"level": 0}})), &token, &[]).await.0, StatusCode::OK);
    let mut tasks = Vec::new();
    for n in 0..12 {
        let (router, token, path) = (fx.router.clone(), token.clone(), path.clone());
        tasks.push(tokio::spawn(async move { send(&router, Method::PUT, &path, Some(json!({"value": {"level": n}, "if_version": 1})), &token, &[]).await.0 }));
    }
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.expect("task"));
    }
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 1, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::CONFLICT).count(), 11, "{statuses:?}");
    let (_, headers, _) = fx.send(Method::GET, &path, None, &token, &[]).await;
    assert_eq!(etag(&headers), Some("\"2\""));
    // 12 creates at once with room for 4 more objects: exactly 4 succeed.
    let mut tasks = Vec::new();
    for n in 0..12 {
        let (router, token) = (fx.router.clone(), token.clone());
        tasks.push(tokio::spawn(async move { send(&router, Method::PUT, &object("misc", &format!("k{n}")), Some(json!({"value": n})), &token, &[]).await.0 }));
    }
    let mut created = 0;
    for task in tasks {
        let status = task.await.expect("task");
        assert!(status == StatusCode::OK || status == StatusCode::FORBIDDEN, "{status}");
        created += usize::from(status == StatusCode::OK);
    }
    assert_eq!(created, 4);
}

/// The byte quota (owner writes only; a write that does not grow always passes), the object quota
/// does not bind server code, the write rate, the server-owned collections.
async fn bytes_rate_and_server_collections(url: &str) {
    let fx = fixture(url, |c| {
        c.max_bytes_per_user = 100;
        c.max_objects_per_user = 2;
        c.write_rate = 12;
        c.write_rate_window_secs = 3600;
        c.server_collections = vec!["server".into(), "wallet".into()];
    })
    .await;
    let (user, token) = fx.register("bytes@example.com").await;
    let storage = fx.storage();
    // 60 bytes, then 50 more: over 100.
    let sixty = json!("x".repeat(58));
    assert_eq!(fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": sixty})), &token, &[]).await.0, StatusCode::OK);
    let fifty = json!("y".repeat(48));
    let (status, _, body) = fx.send(Method::PUT, &object("misc", "b"), Some(json!({"value": fifty})), &token, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::QUOTA_EXCEEDED)), "{body}");
    // Shrinking an object is always fine, and frees room.
    assert_eq!(fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": 1})), &token, &[]).await.0, StatusCode::OK);
    assert_eq!(fx.send(Method::PUT, &object("misc", "b"), Some(json!({"value": fifty})), &token, &[]).await.0, StatusCode::OK);
    // Server code is never refused by the quotas (objects or bytes); its objects still count.
    storage.put(fx.state(), user, "misc", "c", AdminPutObject::new(json!("z".repeat(200)))).await.expect("server write over the quotas");
    assert_eq!(fx.send(Method::PUT, &object("misc", "a"), Some(json!({"value": 2})), &token, &[]).await.0, StatusCode::OK, "no growth");
    // Server collections: the owner reads but never writes or deletes there, even new keys.
    for path in [object("wallet", "gold"), object("server", "flags"), object("server.inv", "x")] {
        let (status, _, body) = fx.send(Method::PUT, &path, Some(json!({"value": 1})), &token, &[]).await;
        assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::FORBIDDEN)), "{path}: {body}");
    }
    let batch = json!({"objects": [{"collection": "misc", "key": "a", "value": 3}, {"collection": "wallet", "key": "gold", "value": 1}]});
    let (status, _, body) = fx.send(Method::POST, routes::storage::BATCH_PUT, Some(batch), &token, &[]).await;
    assert_eq!((status, body["error"]["details"].clone()), (StatusCode::FORBIDDEN, json!({"index": 1})));
    let ack = storage.put(fx.state(), user, "wallet", "gold", AdminPutObject::new(json!({"gold": 5}))).await.expect("the server writes there");
    assert_eq!(ack.version, ObjectVersion(1));
    let (status, _, got) = fx.send(Method::GET, &object("wallet", "gold"), None, &token, &[]).await;
    assert_eq!((status, got["write"].as_str()), (StatusCode::OK, Some("server")), "server-locked by default");
    assert_eq!(fx.send(Method::DELETE, &object("wallet", "gold"), None, &token, &[]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        fx.send(Method::PUT, &object("wallets", "x"), Some(json!({"value": 1})), &token, &[]).await.0,
        StatusCode::FORBIDDEN,
        "quota (2 objects + the server's)"
    );
    // The write rate: 12 per hour per user; this test spent them all by now.
    let mut limited = None;
    for n in 0..13 {
        let (status, headers, body) = fx.send(Method::DELETE, &object("misc", &format!("none{n}")), None, &token, &[]).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            limited = Some((headers, body));
            break;
        }
    }
    let (_, body) = limited.expect("the write rate limits");
    assert_eq!(body["error"]["code"], codes::RATE_LIMITED);
    assert!(body["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0));
    // Reads are not limited; server code is not limited.
    assert_eq!(fx.send(Method::GET, &object("misc", "a"), None, &token, &[]).await.0, StatusCode::OK);
    storage.put(fx.state(), user, "misc", "a", AdminPutObject::new(json!(4))).await.expect("server write");
}

/// B2: the first saves of many NEW players at the same moment (last write wins, no objects yet).
/// On MySQL an UPDATE that matched nothing took a gap lock, and the INSERTs of two such
/// transactions deadlocked; now each player's write reads first and then inserts.
async fn first_saves(url: &str, players: usize) {
    let fx = fixture(url, |_| {}).await;
    let mut tokens = Vec::new();
    for n in 0..players {
        tokens.push(fx.register(&format!("first{n}@example.com")).await.1);
    }
    for round in 0..2 {
        let mut tasks = Vec::new();
        for token in &tokens {
            let (router, token) = (fx.router.clone(), token.clone());
            // Round 0 creates, round 1 overwrites: both last-write-wins.
            tasks.push(tokio::spawn(async move {
                send(&router, Method::PUT, &object("saves", "slot-1"), Some(json!({"value": {"level": round + 1}})), &token, &[]).await
            }));
        }
        for task in tasks {
            let (status, _, body) = task.await.expect("task");
            assert_eq!(status, StatusCode::OK, "round {round}: {body}");
            assert_eq!(body["version"].as_i64(), Some(round + 1));
        }
    }
}

async fn suite(url: &str) {
    crud_and_versions(url).await;
    conditions(url).await;
    batches(url).await;
    limits_and_quota(url).await;
    server_lock_and_service(url).await;
    hooks(url).await;
    admin_access(url).await;
    races(url).await;
    bytes_rate_and_server_collections(url).await;
    first_saves(url, 12).await;
}

// ---- runners ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("sqlite::memory:").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    let dir = common::temp_dir("storage-file");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    suite(&url).await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_storage_suite() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_storage_suite() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

/// B2 stress (VPS tester): 40 new players save their first object at the same moment, twice.
#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server): the first-save deadlock stress test"]
async fn mysql_first_saves_stress() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    first_saves(&url, 40).await;
    common::drop_database(&base, &name).await;
}

/// B2 stress on PostgreSQL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server): the first-save deadlock stress test"]
async fn postgres_first_saves_stress() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    first_saves(&url, 40).await;
    common::drop_database(&base, &name).await;
}

/// The module's wiring: needs `auth` first, refuses settings given twice, lists its routes in the
/// OpenAPI document (admin routes only on request), and every storage route of the protocol is
/// served with its method.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_and_documents() {
    use net_backend_server::protocol::HttpCall;
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Storage::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let error = NetBackendServer::new(config()).module(Storage::new()).module(Auth::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("registered before it"), "{error}");
    let both = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.storage]\nmax_objects_per_user = 5\n").expect("config");
    let ok = NetBackendServer::new(both.clone()).module(Auth::new()).module(Storage::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<StorageService>().expect("service").config().max_objects_per_user, 5);
    let error = NetBackendServer::new(both).module(Auth::new()).module(Storage::new().with_config(StorageConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let mut tight = config();
    tight.http.max_body_bytes = 64 * 1024;
    let error = NetBackendServer::new(tight).module(Auth::new()).module(Storage::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("http.max_body_bytes"), "{error}");

    let mut documented = StorageConfig::default();
    documented.admin_in_openapi = true;
    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Storage::new().with_config(documented)).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.contains("/storage")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    assert!(spec["components"]["schemas"]["StorageObject"].is_object());
    // A value is any JSON (the protocol's `serde_json::Value`), not only an object.
    let value = &spec["components"]["schemas"]["StorageObject"]["properties"]["value"];
    assert!(value.is_object() && value["type"] != json!("object"), "{value}");
    let info: Value = {
        let (_, _, body) = common::call(&prepared.router(), common::get(routes::INFO)).await;
        body
    };
    assert_eq!(info["modules"], json!(["auth", "storage"]));
    let plain = NetBackendServer::new(config()).module(Auth::new()).module(Storage::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(plain.openapi_json()).expect("json");
    assert!(spec["paths"][routes::admin::USER_OBJECT].is_null(), "admin storage routes stay out of the public document");
    assert!(spec["paths"][routes::storage::OBJECT]["put"].is_object());
    // Served with the protocol's methods (another method on a storage path: 405).
    let (status, _, _) =
        common::call(&plain.router(), Request::patch(net_backend_server::protocol::storage::GetObject::ROUTE.path).body(Body::empty()).expect("req")).await;
    assert!(status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND, "{status}");
    let (status, _, _) = common::call(&plain.router(), Request::patch("/v1/storage/saves/a").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[allow(dead_code)]
fn writer_is_public(writer: Writer) -> bool {
    matches!(writer, Writer::Owner | Writer::Server | Writer::Admin(_))
}
