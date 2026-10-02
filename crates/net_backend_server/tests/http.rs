//! HTTP basics through the assembled router (in process): health, readiness, info (golden
//! against the protocol types), the protocol header and version check, error bodies (no
//! internals), body limits, timeouts, panics, the module system, hooks, the auth and rate-limit
//! seams, state access, OpenAPI, metrics, CORS and request ids. Runs with every database
//! feature (without SQLite the pool is lazy towards a closed port).
#![cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use common::{call, get as get_req, http_config, post_json};
use futures_util::future::BoxFuture;
use http::request::Parts;
use http::{Request, StatusCode};
use net_backend_server::hooks::{Decision, Event, HookCtx};
use net_backend_server::http::body_limit;
use net_backend_server::protocol::{codes, routes, ServerInfo, UserId, PROTOCOL_HEADER, PROTOCOL_VERSION};
use net_backend_server::rate_limit::{RateDecision, RateLimitKey, RateLimitStage, RateLimiter};
use net_backend_server::utoipa_axum::router::OpenApiRouter;
use net_backend_server::{ApiJson, AppError, AppState, AuthContext, Authenticator, Config, Db, Error, Ext, Module, NetBackendServer};
use serde::Deserialize;
use serde_json::{json, Value};

async fn router_of(server: NetBackendServer) -> axum::Router {
    server.build().await.expect("build").router()
}

#[derive(Deserialize)]
struct Craft {
    item: String,
}

async fn craft(ApiJson(body): ApiJson<Craft>) -> Result<Json<Value>, AppError> {
    Ok(Json(json!({ "crafted": body.item })))
}

#[tokio::test]
async fn health_info_and_protocol_header() {
    let router = router_of(NetBackendServer::new(http_config())).await;
    let (status, headers, body) = call(&router, get_req(routes::HEALTH)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status":"ok"}));
    assert_eq!(headers.get(PROTOCOL_HEADER).and_then(|v| v.to_str().ok()), Some("1"));
    assert!(headers.get("x-request-id").is_some());

    // Golden: /v1/info is exactly the protocol's ServerInfo.
    let (status, _, body) = call(&router, get_req(routes::INFO)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"protocol": PROTOCOL_VERSION, "min_protocol": PROTOCOL_VERSION, "modules": []}));
    let info: ServerInfo = serde_json::from_value(body.clone()).expect("decodes as the protocol type");
    assert_eq!(serde_json::to_value(&info).expect("encode"), body);
    assert!(info.supports(PROTOCOL_VERSION));
}

#[tokio::test]
async fn readiness() {
    let prepared = NetBackendServer::new(http_config()).build().await.expect("build");
    let router = prepared.router();
    let (status, _, body) = call(&router, get_req(routes::READY)).await;
    if net_backend_server::Dialect::Sqlite.is_enabled() {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"status":"ready"}));
    } else {
        // Lazy pool towards a closed port: not ready, and the answer names no host or driver error.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], codes::UNAVAILABLE);
        assert!(!body.to_string().contains("127.0.0.1"));
    }
    prepared.state().shutdown().trigger();
    let (status, _, body) = call(&router, get_req(routes::READY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], codes::UNAVAILABLE);
    // Liveness stays up while draining.
    assert_eq!(call(&router, get_req(routes::HEALTH)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn unsupported_protocol_version() {
    let router = router_of(NetBackendServer::new(http_config())).await;
    for bad in ["99", "0", "abc", ""] {
        let request = Request::get(routes::INFO).header(PROTOCOL_HEADER, bad).body(Body::empty()).expect("request");
        let (status, headers, body) = call(&router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(body["error"]["code"], codes::UNSUPPORTED_PROTOCOL);
        assert_eq!(body["error"]["details"], json!({"supported_min": 1, "supported_max": 1}));
        assert_eq!(headers.get(PROTOCOL_HEADER).and_then(|v| v.to_str().ok()), Some("1"));
    }
    let request = Request::get(routes::INFO).header(PROTOCOL_HEADER, "1").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.0, StatusCode::OK);
    // /v1/ws is never a 400 (the client would retry forever): a version refusal before an
    // upgrade is 403, a plain GET 426, and with the hub off the path answers 403.
    let request = Request::get(routes::WS).header(PROTOCOL_HEADER, "99").body(Body::empty()).expect("request");
    let (status, _, body) = call(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], codes::UNSUPPORTED_PROTOCOL);
    let (status, headers, body) = call(&router, get_req(routes::WS)).await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(headers.get("upgrade").and_then(|v| v.to_str().ok()), Some("websocket"));
    assert_eq!(body["error"]["code"], codes::BAD_REQUEST);
    let mut config = http_config();
    config.ws.enabled = false;
    let off = router_of(NetBackendServer::new(config)).await;
    let request = Request::get(routes::WS).header(PROTOCOL_HEADER, "99").body(Body::empty()).expect("request");
    let (status, _, body) = call(&off, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], codes::FORBIDDEN);
}

async fn panics() -> &'static str {
    panic!("secret panic text 10.0.0.5")
}

#[tokio::test]
async fn every_error_is_the_protocol_body_without_internals() {
    let server = NetBackendServer::new(http_config())
        .route("/v1/game/craft", post(craft))
        .route("/v1/game/panic", get(panics))
        .route("/v1/game/raw500", get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "stack trace: db password=hunter2") }))
        .route("/v1/game/json500", get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"sql":"SELECT * FROM users"}))) }))
        .route("/v1/game/json409", get(|| async { (StatusCode::CONFLICT, Json(json!({"error":{"code":"craft_busy","message":"busy"}}))) }))
        .route("/v1/game/dberror", get(|| async { Err::<(), AppError>(sqlx::Error::Protocol("relation \"users\" does not exist".into()).into()) }));
    let router = router_of(server).await;

    let (status, _, body) = call(&router, get_req("/v1/nothing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], codes::NOT_FOUND);

    let (status, headers, body) = call(&router, get_req("/v1/game/craft")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
    assert!(headers.get("allow").is_some());
    assert_eq!(headers.get(PROTOCOL_HEADER).and_then(|v| v.to_str().ok()), Some("1"), "also on rewritten answers");

    for path in ["/v1/game/panic", "/v1/game/raw500", "/v1/game/json500", "/v1/game/dberror"] {
        let (status, _, body) = call(&router, get_req(path)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert_eq!(body, json!({"error":{"code":"internal","message":"internal server error"}}), "{path}");
    }
    // A game's own JSON 4xx passes unchanged.
    let (status, _, body) = call(&router, get_req("/v1/game/json409")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "craft_busy");

    // Malformed JSON, wrong shape, wrong content type.
    let (status, _, body) = call(&router, post_json("/v1/game/craft", "{nope")).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some(codes::BAD_REQUEST)));
    let (status, _, body) = call(&router, post_json("/v1/game/craft", r#"{"item":5}"#)).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some(codes::BAD_REQUEST)));
    let request = Request::post("/v1/game/craft").body(Body::from(r#"{"item":"x"}"#)).expect("request");
    let (status, _, body) = call(&router, request).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNSUPPORTED_MEDIA_TYPE, Some("unsupported_media_type")));
    let (status, _, body) = call(&router, post_json("/v1/game/craft", r#"{"item":"sword"}"#)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"crafted":"sword"})));
}

#[tokio::test]
async fn body_limits() {
    let big = format!(r#"{{"item":"{}"}}"#, "x".repeat(70 * 1024));
    let server = NetBackendServer::new(http_config())
        .route("/v1/game/craft", post(craft))
        .route("/v1/game/plain", post(|Json(v): Json<Value>| async move { Json(v) }))
        .route("/v1/game/upload", post(craft).layer(body_limit(1024 * 1024)));
    let router = router_of(server).await;
    // The default limit is the protocol's 64 KiB (not axum's 2 MB).
    let (status, _, body) = call(&router, post_json("/v1/game/craft", big.clone())).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], codes::PAYLOAD_TOO_LARGE);
    // axum's own Json rejection is normalised too.
    let (status, _, body) = call(&router, post_json("/v1/game/plain", big.clone())).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some(codes::PAYLOAD_TOO_LARGE)));
    // A raised per-route limit.
    let (status, _, _) = call(&router, post_json("/v1/game/upload", big)).await;
    assert_eq!(status, StatusCode::OK);
    let small = format!(r#"{{"item":"{}"}}"#, "x".repeat(60 * 1024));
    assert_eq!(call(&router, post_json("/v1/game/craft", small)).await.0, StatusCode::OK);

    // The configured default applies.
    let mut config = http_config();
    config.http.body_limit_bytes = 2048;
    let router = router_of(NetBackendServer::new(config).route("/v1/game/craft", post(craft))).await;
    let (status, _, _) = call(&router, post_json("/v1/game/craft", format!(r#"{{"item":"{}"}}"#, "x".repeat(3000)))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn request_timeout_answers_503() {
    let mut config = http_config();
    config.http.request_timeout_secs = 1;
    let router = router_of(NetBackendServer::new(config).route(
        "/v1/game/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(120)).await;
            "late"
        }),
    ))
    .await;
    let started = std::time::Instant::now();
    let (status, _, body) = call(&router, get_req("/v1/game/slow")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], codes::UNAVAILABLE);
    // Cut at the 1 s timeout, not after the handler's 120 s (generous bound: slow CI runners).
    assert!(started.elapsed() < Duration::from_secs(60), "{:?}", started.elapsed());
}

/// A module with one route and one hook.
struct Recorder {
    name: &'static str,
}

impl Module for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        let name = self.name;
        OpenApiRouter::new().route(&format!("/v1/{name}/ping"), get(move || async move { name }))
    }

    fn register_hooks(&self, hooks: &mut net_backend_server::Hooks) {
        let name = self.name;
        hooks.before::<Shout, _, _>(move |_ctx, mut event| async move {
            event.text.push_str(&format!(" {name}"));
            Ok(Decision::Continue(event))
        });
    }
}

#[derive(Debug)]
struct Shout {
    text: String,
}

impl Event for Shout {
    const NAME: &'static str = "test.shout";
}

#[tokio::test]
async fn modules_register_in_order() {
    let server = NetBackendServer::new(http_config()).module(Recorder { name: "zeta" }).module(Recorder { name: "alpha" }).before::<Shout, _, _>(
        |_ctx, mut event| async move {
            event.text.push_str(" game");
            Ok(Decision::Continue(event))
        },
    );
    assert_eq!(server.module_names(), ["zeta", "alpha"]);
    let prepared = server.build().await.expect("build");
    assert_eq!(prepared.state().modules(), ["zeta", "alpha"]);
    let router = prepared.router();
    // /v1/info lists them sorted.
    let (_, _, body) = call(&router, get_req(routes::INFO)).await;
    assert_eq!(body["modules"], json!(["alpha", "zeta"]));
    let (status, _, body) = call(&router, get_req("/v1/alpha/ping")).await;
    assert_eq!((status, body), (StatusCode::OK, json!("alpha")));
    // Hooks run in module registration order.
    let ctx = HookCtx::new(prepared.state().clone(), None);
    let event = prepared.state().hooks().run_before(&ctx, Shout { text: "hi".into() }).await.expect("hooks");
    // The game's hooks first, then the modules' in registration order.
    assert_eq!(event.text, "hi game zeta alpha");

    // Duplicate or invalid names, conflicting routes: errors, not panics.
    let duplicate = NetBackendServer::new(http_config()).module(Recorder { name: "a" }).module(Recorder { name: "a" });
    assert!(matches!(duplicate.build().await, Err(Error::Module(m)) if m.contains("registered twice")));
    let reserved = NetBackendServer::new(http_config()).module(Recorder { name: "app" });
    assert!(matches!(reserved.build().await, Err(Error::Module(m)) if m.contains("reserved")));
    let conflict = NetBackendServer::new(http_config()).route(routes::HEALTH, get(|| async { "mine" }));
    assert!(matches!(conflict.build().await, Err(Error::Module(m)) if m.contains("route registration failed")));
    let bad_path = NetBackendServer::new(http_config()).route("no-slash", get(|| async { "x" }));
    assert!(matches!(bad_path.build().await, Err(Error::Module(_))));
}

#[derive(Debug)]
struct Trade {
    amount: i64,
}

impl Event for Trade {
    const NAME: &'static str = "test.trade";
}

#[tokio::test]
async fn hooks_modify_reject_time_out_and_contain_panics() {
    let after_runs = Arc::new(AtomicUsize::new(0));
    let runs = after_runs.clone();
    let runs2 = after_runs.clone();
    let mut config = http_config();
    // Long enough for the quick hooks on a slow runner; the sleeping one (60 s) always runs out.
    config.server.hook_timeout_ms = 1500;
    let prepared = NetBackendServer::new(config)
        .before::<Trade, _, _>(|_ctx, mut trade| async move {
            trade.amount *= 2;
            Ok(Decision::Continue(trade))
        })
        .before::<Trade, _, _>(|_ctx, trade| async move {
            match trade.amount {
                a if a > 1000 => Ok(Decision::Reject(AppError::forbidden("too much"))),
                666 => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(Decision::Continue(trade))
                }
                84 => panic!("hook bug"),
                _ => Ok(Decision::Continue(trade)),
            }
        })
        .after::<Trade, _, _>(move |_ctx, _trade| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(AppError::conflict("logged, not returned"))
            }
        })
        .after::<Trade, _, _>(|_ctx, _trade| async move { panic!("after hook bug") })
        .after::<Trade, _, _>(move |_ctx, _trade| {
            let runs = runs2.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .build()
        .await
        .expect("build");
    let state = prepared.state();
    let ctx = HookCtx::new(state.clone(), None);
    assert_eq!(state.hooks().before_count::<Trade>(), 2);
    assert_eq!(state.hooks().after_count::<Trade>(), 3);

    let trade = state.hooks().run_before(&ctx, Trade { amount: 5 }).await.expect("passes");
    assert_eq!(trade.amount, 10, "modified by the first hook");
    let rejected = state.hooks().run_before(&ctx, Trade { amount: 600 }).await.expect_err("rejected");
    assert_eq!((rejected.status(), rejected.code()), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
    let timed_out = state.hooks().run_before(&ctx, Trade { amount: 333 }).await.expect_err("timed out");
    assert_eq!((timed_out.status(), timed_out.code()), (StatusCode::SERVICE_UNAVAILABLE, codes::HOOK_TIMEOUT));
    let panicked = state.hooks().run_before(&ctx, Trade { amount: 42 }).await.expect_err("panicked");
    assert_eq!((panicked.status(), panicked.code()), (StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL));

    // After hooks: every one runs, errors and panics are contained.
    state.hooks().run_after(&ctx, Arc::new(Trade { amount: 1 })).await;
    assert_eq!(after_runs.load(Ordering::SeqCst), 2);
    // A panic in the synchronous part of a hook closure is contained too.
    let prepared = NetBackendServer::new(http_config())
        .before::<Shout, _, _>(|_ctx, event| {
            if event.text == "boom" {
                panic!("sync part");
            }
            async move { Ok(Decision::Continue(event)) }
        })
        .build()
        .await
        .expect("build");
    let ctx2 = HookCtx::new(prepared.state().clone(), None);
    let caught = prepared.state().hooks().run_before(&ctx2, Shout { text: "boom".into() }).await.expect_err("contained");
    assert_eq!(caught.code(), codes::INTERNAL);
    // An event nobody hooks passes through.
    let shout = state.hooks().run_before(&ctx, Shout { text: "x".into() }).await.expect("no hooks");
    assert_eq!(shout.text, "x");
}

/// Authenticates `x-test-user: <id>`; `x-test-user: expired` is refused with `token_expired`.
struct HeaderAuth;

impl Authenticator for HeaderAuth {
    fn authenticate<'a>(&'a self, parts: &'a Parts, _state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        Box::pin(async move {
            match parts.headers.get("x-test-user").and_then(|v| v.to_str().ok()) {
                None => Ok(None),
                Some("expired") => Err(AppError::new(codes::TOKEN_EXPIRED, "the access token expired")),
                Some(id) => Ok(id.parse::<i64>().ok().map(|id| AuthContext::new(UserId(id)))),
            }
        })
    }
}

async fn whoami(auth: AuthContext) -> Json<Value> {
    Json(json!({ "user": auth.user_id.0 }))
}

async fn maybe(auth: Option<AuthContext>) -> Json<Value> {
    Json(json!({ "user": auth.map(|a| a.user_id.0) }))
}

#[tokio::test]
async fn auth_seam() {
    // Without an authenticator every AuthContext route is 401.
    let router = router_of(NetBackendServer::new(http_config()).route("/v1/game/me", get(whoami))).await;
    let request = Request::get("/v1/game/me").header("x-test-user", "7").body(Body::empty()).expect("request");
    let (status, _, body) = call(&router, request).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some(codes::UNAUTHORIZED)));

    let router =
        router_of(NetBackendServer::new(http_config()).authenticator(HeaderAuth).route("/v1/game/me", get(whoami)).route("/v1/game/maybe", get(maybe))).await;
    let request = Request::get("/v1/game/me").header("x-test-user", "7").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.2, json!({"user": 7}));
    assert_eq!(call(&router, get_req("/v1/game/me")).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, get_req("/v1/game/maybe")).await.2, json!({"user": null}));
    // A refused credential: routes that need a user answer the authenticator's error; routes that
    // do not treat the request as anonymous (a client that always sends its last token can still
    // log in or refresh).
    let request = Request::get("/v1/game/me").header("x-test-user", "expired").body(Body::empty()).expect("request");
    let (status, _, body) = call(&router, request).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some(codes::TOKEN_EXPIRED)));
    let request = Request::get("/v1/game/maybe").header("x-test-user", "expired").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.2, json!({"user": null}));

    // Several authenticators: asked in order, the first that knows the request decides.
    struct Never;
    impl Authenticator for Never {
        fn authenticate<'a>(&'a self, _parts: &'a Parts, _state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
            Box::pin(async { Ok(None) })
        }
    }
    let router = router_of(NetBackendServer::new(http_config()).authenticator(Never).authenticator(HeaderAuth).route("/v1/game/me", get(whoami))).await;
    let request = Request::get("/v1/game/me").header("x-test-user", "9").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.2, json!({"user": 9}));
}

/// Denies the route `/v1/game/limited/{n}` for user 13 (after auth) and the route
/// `/v1/game/closed` before auth; records the keys it saw.
#[derive(Clone, Default)]
struct TestLimiter(Arc<Mutex<Vec<RateLimitKey>>>);

impl RateLimiter for TestLimiter {
    fn check(&self, key: &RateLimitKey) -> RateDecision {
        if let Ok(mut seen) = self.0.lock() {
            seen.push(key.clone());
        }
        let route = key.route.as_deref();
        if key.stage == RateLimitStage::AfterAuth && route == Some("/v1/game/limited/{n}") && key.user == Some(UserId(13)) {
            RateDecision::Deny { retry_after_ms: 1500 }
        } else if key.stage == RateLimitStage::BeforeAuth && route == Some("/v1/game/closed") {
            RateDecision::Deny { retry_after_ms: 10 }
        } else {
            RateDecision::Allow
        }
    }
}

/// Counts its calls, then delegates to `HeaderAuth`.
#[derive(Clone, Default)]
struct CountingAuth(Arc<AtomicUsize>);

impl Authenticator for CountingAuth {
    fn authenticate<'a>(&'a self, parts: &'a Parts, state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        HeaderAuth.authenticate(parts, state)
    }
}

#[tokio::test]
async fn rate_limit_seam() {
    let limiter = TestLimiter::default();
    let auth = CountingAuth::default();
    let router = router_of(
        NetBackendServer::new(http_config())
            .authenticator(auth.clone())
            .rate_limiter(limiter.clone())
            .route("/v1/game/limited/{n}", get(|| async { "ok" }))
            .route("/v1/game/closed", get(|| async { "ok" })),
    )
    .await;
    let request = Request::get("/v1/game/limited/5").header("x-test-user", "13").body(Body::empty()).expect("request");
    let (status, headers, body) = call(&router, request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], codes::RATE_LIMITED);
    assert_eq!(body["error"]["details"]["retry_after_ms"], 1500);
    assert_eq!(headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("2"));
    let request = Request::get("/v1/game/limited/5").header("x-test-user", "14").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.0, StatusCode::OK);
    let seen = limiter.0.lock().map(|s| s.clone()).unwrap_or_default();
    assert_eq!(seen[0].route.as_deref(), Some("/v1/game/limited/{n}"), "the pattern, never the raw path");
    assert_eq!((seen[0].stage, seen[0].user), (RateLimitStage::BeforeAuth, None), "asked before authentication, without a user");
    assert_eq!((seen[1].stage, seen[1].user), (RateLimitStage::AfterAuth, Some(UserId(13))));
    // A refusal before authentication never reaches the authenticator (token guessing is throttled).
    let calls = auth.0.load(Ordering::SeqCst);
    let request = Request::get("/v1/game/closed").header("x-test-user", "expired").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(auth.0.load(Ordering::SeqCst), calls);
    let key = RateLimitKey::new(None, Some("/x".into()), None, RateLimitStage::BeforeAuth);
    assert_eq!(limiter.check(&key), RateDecision::Allow);
}

struct GameState {
    motd: &'static str,
}

#[tokio::test]
async fn state_access() {
    async fn motd(Ext(game): Ext<GameState>, State(db): State<Db>, State(config): State<Arc<Config>>) -> Json<Value> {
        Json(json!({ "motd": game.motd, "dialect": db.dialect().name(), "timeout": config.http.request_timeout_secs }))
    }
    async fn missing(Ext(_n): Ext<u64>) -> &'static str {
        "never"
    }
    let router = router_of(
        NetBackendServer::new(http_config()).state(GameState { motd: "welcome" }).route("/v1/game/motd", get(motd)).route("/v1/game/missing", get(missing)),
    )
    .await;
    let (status, _, body) = call(&router, get_req("/v1/game/motd")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["motd"], "welcome");
    assert_eq!(body["timeout"], 30);
    let (status, _, body) = call(&router, get_req("/v1/game/missing")).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::INTERNAL_SERVER_ERROR, Some(codes::INTERNAL)));
}

/// A documented game route.
#[utoipa::path(post, path = "/v1/game/craft", tag = "game", responses((status = 200, description = "Crafted")))]
async fn documented_craft(ApiJson(body): ApiJson<Craft>) -> Json<Value> {
    Json(json!({ "crafted": body.item }))
}

#[tokio::test]
async fn openapi_document() {
    let router = router_of(NetBackendServer::new(http_config()).routes(net_backend_server::utoipa_axum::routes!(documented_craft))).await;
    let (status, headers, doc) = call(&router, get_req("/v1/openapi.json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").and_then(|v| v.to_str().ok()), Some("application/json"));
    assert_eq!(doc["info"]["title"], "Game backend API");
    for path in [routes::HEALTH, routes::READY, routes::INFO, "/v1/game/craft"] {
        assert!(doc["paths"][path].is_object(), "{path} missing: {}", doc["paths"]);
    }
    // The mirrored schemas have exactly the protocol types' fields.
    let schemas = &doc["components"]["schemas"];
    let mut info_fields: Vec<String> = schemas["ServerInfo"]["properties"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    info_fields.sort();
    let mut real: Vec<String> =
        serde_json::to_value(ServerInfo::new(vec![])).ok().and_then(|v| v.as_object().map(|o| o.keys().cloned().collect())).unwrap_or_default();
    real.sort();
    assert_eq!(info_fields, real);
    let error_fields: Vec<String> = schemas["ApiError"]["properties"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    for field in ["code", "message", "details"] {
        assert!(error_fields.iter().any(|f| f == field), "{field}");
    }
    assert!(schemas["ErrorBody"]["properties"]["error"].is_object());
    // The documented route works; the UI is off by default.
    assert_eq!(call(&router, post_json("/v1/game/craft", r#"{"item":"a"}"#)).await.0, StatusCode::OK);
    assert_eq!(call(&router, get_req("/v1/docs")).await.0, StatusCode::NOT_FOUND);

    let mut config = http_config();
    config.openapi.ui = true;
    config.openapi.ui_script_url = Some("https://cdn.example.org/viewer@1.2.3/standalone.js".into());
    config.openapi.ui_script_integrity = Some("sha384-AAAAbbbb".into());
    let router = router_of(NetBackendServer::new(config)).await;
    let (status, headers, page) = call(&router, get_req("/v1/docs")).await;
    assert_eq!(status, StatusCode::OK);
    let page = page.as_str().unwrap_or_default().to_string();
    assert!(page.contains("/v1/openapi.json") && page.contains(r#"integrity="sha384-AAAAbbbb""#) && page.contains("crossorigin"), "{page}");
    let csp = headers.get("content-security-policy").and_then(|v| v.to_str().ok()).unwrap_or_default();
    assert!(csp.contains("script-src https://cdn.example.org/viewer@1.2.3/standalone.js;"), "{csp}");

    let mut config = http_config();
    config.openapi.enabled = false;
    let router = router_of(NetBackendServer::new(config)).await;
    assert_eq!(call(&router, get_req("/v1/openapi.json")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_endpoint() {
    let prepared = NetBackendServer::new(http_config()).build().await.expect("build");
    assert!(prepared.metrics_router().is_none(), "off by default");
    let mut config = http_config();
    config.metrics.enabled = true;
    let prepared = NetBackendServer::new(config).build().await.expect("build");
    let router = prepared.router();
    assert_eq!(call(&router, get_req(routes::INFO)).await.0, StatusCode::OK);
    // Never on the API listener: metrics have their own (metrics.bind, loopback by default).
    assert_eq!(call(&router, get_req("/metrics")).await.0, StatusCode::NOT_FOUND);
    let metrics = prepared.metrics_router().expect("metrics router");
    let (status, headers, text) = call(&metrics, get_req("/metrics")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/plain")));
    let text = text.as_str().unwrap_or_default().to_string();
    assert!(text.contains("nbs_http_requests_total") && text.contains("route=\"/v1/info\""), "{text}");
}

#[tokio::test]
async fn cors_and_request_ids() {
    // CORS off: no headers.
    let router = router_of(NetBackendServer::new(http_config())).await;
    let request = Request::get(routes::INFO).header("origin", "https://example.com").body(Body::empty()).expect("request");
    let (_, headers, _) = call(&router, request).await;
    assert!(headers.get("access-control-allow-origin").is_none());
    // A client's request id is ignored unless trusted.
    let request = Request::get(routes::HEALTH).header("x-request-id", "client-id-1").body(Body::empty()).expect("request");
    let (_, headers, _) = call(&router, request).await;
    assert_ne!(headers.get("x-request-id").and_then(|v| v.to_str().ok()), Some("client-id-1"));

    let mut config = http_config();
    config.cors.allowed_origins = vec!["https://example.com".into()];
    config.http.trust_request_id = true;
    let router = router_of(NetBackendServer::new(config)).await;
    let request = Request::options(routes::INFO)
        .header("origin", "https://example.com")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .expect("request");
    let (status, headers, _) = call(&router, request).await;
    assert!(status.is_success(), "{status}");
    assert_eq!(headers.get("access-control-allow-origin").and_then(|v| v.to_str().ok()), Some("https://example.com"));
    // Conditional storage writes from a page: the preflight allows `If-Match` / `If-None-Match`, and
    // answers expose `ETag` / `Retry-After`.
    let request = Request::options(routes::INFO)
        .header("origin", "https://example.com")
        .header("access-control-request-method", "PUT")
        .header("access-control-request-headers", "if-match,if-none-match,content-type,authorization")
        .body(Body::empty())
        .expect("request");
    let (status, headers, _) = call(&router, request).await;
    assert!(status.is_success(), "{status}");
    let allowed = headers.get("access-control-allow-headers").and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    assert!(allowed.contains("if-match") && allowed.contains("if-none-match"), "{allowed}");
    let request = Request::get(routes::INFO).header("origin", "https://example.com").body(Body::empty()).expect("request");
    let exposed = call(&router, request).await.1.get("access-control-expose-headers").and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    assert!(exposed.contains("etag") && exposed.contains("retry-after"), "{exposed}");
    let request = Request::get(routes::INFO).header("origin", "https://evil.example").body(Body::empty()).expect("request");
    assert!(call(&router, request).await.1.get("access-control-allow-origin").is_none());
    let request = Request::get(routes::HEALTH).header("x-request-id", "client-id-1").body(Body::empty()).expect("request");
    assert_eq!(call(&router, request).await.1.get("x-request-id").and_then(|v| v.to_str().ok()), Some("client-id-1"));
    let request = Request::get(routes::HEALTH).header("x-request-id", "bad id\twith tab").body(Body::empty()).expect("request");
    assert_ne!(call(&router, request).await.1.get("x-request-id").and_then(|v| v.to_str().ok()), Some("bad id\twith tab"));
}

#[tokio::test]
async fn framework_rejections_get_protocol_statuses() {
    let server =
        NetBackendServer::new(http_config())
            .route("/v1/game/plain", post(|Json(v): Json<Craft>| async move { v.item }))
            .route("/v1/game/odd", get(|| async { (StatusCode::BAD_REQUEST, Json(json!({"reason":"not a protocol body"}))) }))
            .route(
                "/v1/game/raw",
                post(|body: Body| async move {
                    axum::body::to_bytes(body, usize::MAX).await.map(|b| b.len().to_string()).map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)
                }),
            );
    let mut config = http_config();
    config.http.max_body_bytes = 128 * 1024;
    let router = router_of(server).await;
    // axum's `Json` answers 422 for a wrong shape; the protocol code for that is 400 `bad_request`.
    let (status, _, body) = call(&router, post_json("/v1/game/plain", r#"{"item":5}"#)).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some(codes::BAD_REQUEST)));
    // A game's JSON 4xx that is not a protocol error body is rewritten.
    let (status, _, body) = call(&router, get_req("/v1/game/odd")).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some(codes::BAD_REQUEST)));
    // The hard body cap also limits handlers reading the raw body stream.
    let (status, _, _) = call(&router, post_json("/v1/game/raw", "x".repeat(1024))).await;
    assert_eq!(status, StatusCode::OK);
    let router = router_of(NetBackendServer::new(config).route(
        "/v1/game/raw",
        post(|body: Body| async move { axum::body::to_bytes(body, usize::MAX).await.map(|b| b.len().to_string()).map_err(|_| StatusCode::PAYLOAD_TOO_LARGE) }),
    ))
    .await;
    let (status, _, body) = call(&router, post_json("/v1/game/raw", "x".repeat(200 * 1024))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::PAYLOAD_TOO_LARGE, Some(codes::PAYLOAD_TOO_LARGE)));
}

#[tokio::test]
async fn unknown_module_sections_are_refused() {
    let mut config = http_config();
    let mut section = toml::Table::new();
    section.insert("max_chars".into(), toml::Value::Integer(5));
    config.modules.insert("chta".into(), toml::Value::Table(section.clone()));
    let result = NetBackendServer::new(config.clone()).build().await;
    assert!(matches!(&result, Err(Error::Config(p)) if p.iter().any(|p| p.contains("[modules.chta]"))), "{result:?}");
    config.modules.remove("chta");
    config.modules.insert("zeta".into(), toml::Value::Table(section));
    assert!(NetBackendServer::new(config).module(Recorder { name: "zeta" }).build().await.is_ok());
}

#[tokio::test]
async fn invalid_config_refuses_to_build() {
    let mut config = http_config();
    config.http.request_timeout_secs = 0;
    assert!(matches!(NetBackendServer::new(config).build().await, Err(Error::Config(p)) if p.iter().any(|p| p.contains("request_timeout_secs"))));
}
