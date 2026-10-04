//! Every route of the protocol (`routes::ALL`, each with its `HttpCall`) is served by a server with
//! every module, with the protocol's method, and documented at the protocol's path; a game's own
//! `HttpCall` marked `auth` is guarded by the mount itself.
#![cfg(all(
    feature = "sqlite",
    feature = "storage",
    feature = "chat",
    feature = "leaderboards",
    feature = "notifications",
    feature = "friends",
    feature = "groups",
    feature = "oauth",
    feature = "lobbies",
    feature = "matchmaking",
    feature = "files"
))]

mod common;

use axum::body::Body;
use http::{Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::chat::Chat;
use net_backend_server::files::{Files, FilesConfig};
use net_backend_server::friends::Friends;
use net_backend_server::groups::Groups;
use net_backend_server::http::call::{Call, CallResult, Reply};
use net_backend_server::leaderboards::Leaderboards;
use net_backend_server::lobbies::Lobbies;
use net_backend_server::matchmaking::Matchmaking;
use net_backend_server::notifications::Notifications;
use net_backend_server::oauth::OAuth;
use net_backend_server::protocol::http_call::placeholders;
use net_backend_server::protocol::routes::{self, HttpMethod};
use net_backend_server::protocol::PathParams;
use net_backend_server::storage::{Storage, StorageConfig};
use net_backend_server::NetBackendServer;
use serde_json::Value;

#[tokio::test]
async fn every_protocol_route_is_served_and_documented() {
    let mut auth = AuthConfig::default();
    auth.admin_in_openapi = true;
    let mut storage = StorageConfig::default();
    storage.admin_in_openapi = true;
    let mut files = FilesConfig::default();
    files.dir = common::temp_dir("routes-files");
    let prepared = NetBackendServer::new(common::http_config())
        .module(Auth::new().with_config(auth))
        .module(Storage::new().with_config(storage))
        .module(Chat::new())
        .module(Leaderboards::new())
        .module(Notifications::new())
        .module(Friends::new())
        .module(Groups::new())
        .module(OAuth::new())
        .module(Lobbies::new())
        .module(Matchmaking::new())
        .module(Files::new().with_config(files))
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    let router = prepared.router();
    for route in routes::ALL {
        let method = route.method.as_str();
        assert!(spec["paths"][route.path][method.to_ascii_lowercase()].is_object(), "{method} {} is not documented", route.path);
        // A filled path (sample parameters) answers with the route's own handler: never 405, and
        // a route that needs a token says so (401) before anything else.
        let mut params = PathParams::new();
        for name in placeholders(route.path) {
            params.insert(name, if matches!(name, "user" | "room" | "message" | "id" | "group" | "file") { "1" } else { "sample" });
        }
        let path = params.fill(route.path).expect("a path");
        let request = Request::builder().method(method).uri(&path).header("content-type", "application/json").body(Body::from("{}")).expect("request");
        let (status, _, body) = common::call(&router, request).await;
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}: {body}");
        if route.auth {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} without a token: {body}");
        }
        // Another method on the same path is not served (unless the protocol defines it too).
        let other = if route.method == HttpMethod::Patch { "POST" } else { "PATCH" };
        if !routes::ALL.iter().any(|r| r.path == route.path && r.method.as_str() == other) {
            let request = Request::builder().method(other).uri(&path).body(Body::empty()).expect("request");
            assert_eq!(common::call(&router, request).await.0, StatusCode::METHOD_NOT_ALLOWED, "{other} {path}");
        }
    }
    // The routes without an `HttpCall` (the file upload and download): documented, guarded.
    for route in routes::BINARY {
        let method = route.method.as_str();
        assert!(spec["paths"][route.path][method.to_ascii_lowercase()].is_object(), "{method} {} is not documented", route.path);
        let path = route.path.replace("{file}", "1");
        let request = Request::builder().method(method).uri(&path).body(Body::empty()).expect("request");
        assert_eq!(common::call(&router, request).await.0, StatusCode::UNAUTHORIZED, "{method} {path} without a token");
    }
}

/// A game's own `HttpCall`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Craft {
    item: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Crafted {
    ok: bool,
}

impl net_backend_server::protocol::HttpCall for Craft {
    type Payload = Craft;
    type Response = Crafted;
    const ROUTE: routes::Route = routes::Route::new(HttpMethod::Post, "/v1/game/craft", true);
    const PAYLOAD: net_backend_server::protocol::PayloadKind = net_backend_server::protocol::PayloadKind::Json;

    fn payload(&self) -> &Craft {
        self
    }

    fn from_parts(_params: &PathParams, payload: Craft) -> Result<Self, net_backend_server::protocol::ApiError> {
        Ok(payload)
    }
}

/// The same call, public.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Peek {
    item: String,
}

impl net_backend_server::protocol::HttpCall for Peek {
    type Payload = Peek;
    type Response = Crafted;
    const ROUTE: routes::Route = routes::Route::new(HttpMethod::Post, "/v1/game/peek", false);
    const PAYLOAD: net_backend_server::protocol::PayloadKind = net_backend_server::protocol::PayloadKind::Json;

    fn payload(&self) -> &Peek {
        self
    }

    fn from_parts(_params: &PathParams, payload: Peek) -> Result<Self, net_backend_server::protocol::ApiError> {
        Ok(payload)
    }
}

// Neither handler asks for the caller: the route's `auth` flag alone must guard it.
async fn craft(Call(call): Call<Craft>) -> CallResult<Craft> {
    Ok(Reply::new(Crafted { ok: !call.item.is_empty() }))
}

async fn peek(Call(call): Call<Peek>) -> CallResult<Peek> {
    Ok(Reply::new(Crafted { ok: !call.item.is_empty() }))
}

/// S3: a route marked "token required" is guarded by the mount itself, for a game's own call too.
#[tokio::test]
async fn game_calls_marked_auth_need_a_token() {
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.rate_limits = false;
    let prepared = NetBackendServer::new(common::http_config())
        .module(Auth::new().with_config(auth))
        .call::<Craft, _, _, _>(craft)
        .call::<Peek, _, _, _>(peek)
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let body = r#"{"item":"sword"}"#;
    let post = |path: &str, token: Option<&str>| {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.body(Body::from(body)).expect("request")
    };
    let (status, _, answer) = common::call(&router, post("/v1/game/craft", None)).await;
    assert_eq!((status, answer["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("unauthorized")), "{answer}");
    let (status, _, _) = common::call(&router, post("/v1/game/craft", Some("not-a-token"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, answer) = common::call(&router, post("/v1/game/peek", None)).await;
    assert_eq!((status, answer["ok"].as_bool()), (StatusCode::OK, Some(true)), "a public call stays public");
    let register = Request::post(routes::auth::REGISTER)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"email":"crafter@example.com","password":"correct horse battery"}"#))
        .expect("request");
    let (status, _, session) = common::call(&router, register).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let token = session["tokens"]["access_token"].as_str().expect("token").to_string();
    let (status, _, answer) = common::call(&router, post("/v1/game/craft", Some(&token))).await;
    assert_eq!((status, answer["ok"].as_bool()), (StatusCode::OK, Some(true)), "{answer}");
    // Without any authenticator (no auth module) a guarded route is never served anonymously.
    let bare = NetBackendServer::new(common::http_config()).call::<Craft, _, _, _>(craft).build().await.expect("build");
    assert_eq!(common::call(&bare.router(), post("/v1/game/craft", None)).await.0, StatusCode::UNAUTHORIZED);
}
