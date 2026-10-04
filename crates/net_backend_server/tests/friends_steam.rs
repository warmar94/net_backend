//! The friends module's Steam ID lookup (`POST /v1/friends/steam`) and settings
//! (`GET` / `PUT /v1/friends/settings`): accounts found by their linked Steam account (by a Steam
//! link on an email account and by a Steam-only account); never the caller, a Steam ID without an
//! account, a banned account (found again after a timed ban ends), a player who turned
//! `steam_findable` off (found again when it turns it on), or a player with a block between it and
//! the caller in either direction; the caller's relation (friend, sent, received); duplicates; the
//! order; the hook (refuse, remove, no widening); the audit entry; account deletion with a
//! settings row; malformed Steam IDs, the cap, a caller without Steam, the rate, and the route
//! without Steam login. Steam itself is a fake verifier (no network).
//!
//! The suite runs on SQLite in memory here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "friends", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::steam::{FakeSteamVerifier, SteamIdentity};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::friends::events::BeforeSteamMatch;
use net_backend_server::friends::{FriendService, Friends, FriendsConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::protocol::admin::{AuditQuery, BanRequest};
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{AppError, AppState, Config, ManualClock, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";
const IDENTITY: &str = "test-game";

/// Obviously made-up SteamID64s: account numbers above 4 000 000 000 (`76561197960265728` is
/// account number 0).
const fn steam(n: u64) -> u64 {
    76_561_197_960_265_728 + 4_000_000_000 + n
}

/// The ticket the fake verifier accepts for `steam(n)`.
fn ticket(n: u64) -> String {
    format!("0a{n:02x}")
}

/// What the hook does: refuse this account, remove this Steam ID, try to add this one.
#[derive(Default)]
struct HookRules {
    refuse: Option<UserId>,
    remove: Option<u64>,
    add: Option<u64>,
}

struct Fx {
    state: AppState,
    router: Router,
    clock: Arc<ManualClock>,
    rules: Arc<Mutex<HookRules>>,
}

async fn start(url: &str, steam_login: bool, tweak: impl FnOnce(&mut FriendsConfig)) -> Fx {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("friends-steam-migrations");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut module = Auth::new();
    if steam_login {
        auth.steam_identity = Some(IDENTITY.into());
        let fake = FakeSteamVerifier::new(IDENTITY);
        for n in 1..=12 {
            fake.add_ticket(ticket(n), SteamIdentity::new(steam(n)));
        }
        module = module.steam_verifier(fake);
    }
    let mut friends = FriendsConfig::default();
    friends.request_rate = 0;
    friends.steam_rate = 0;
    tweak(&mut friends);
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let rules = Arc::new(Mutex::new(HookRules::default()));
    let seen = rules.clone();
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(module.with_config(auth))
        .module(Friends::new().with_config(friends))
        .before::<BeforeSteamMatch, _, _>(move |_ctx, mut lookup| {
            let seen = seen.clone();
            async move {
                let rules = seen.lock().expect("lock");
                if rules.refuse == Some(lookup.user) {
                    return Ok(Decision::Reject(AppError::forbidden("not in this game")));
                }
                if let Some(remove) = rules.remove {
                    lookup.steam_ids.retain(|id| *id != remove);
                }
                if let Some(add) = rules.add {
                    lookup.steam_ids.push(add);
                }
                Ok(Decision::Continue(lookup))
            }
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    Fx { state: prepared.state().clone(), router: prepared.router(), clock, rules }
}

impl Fx {
    async fn http(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let response = self.router.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> Value {
        let (status, answer) = self.http(method, path, body, Some(token)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        answer
    }

    /// The error code of a request that must fail with `status`.
    async fn fails(&self, method: Method, path: &str, body: Option<Value>, token: &str, status: StatusCode) -> Value {
        let (got, answer) = self.http(method, path, body, Some(token)).await;
        assert_eq!(got, status, "{path}: {answer}");
        answer
    }

    /// An email account with a display name; with `link`, Steam account `steam(link)` linked to it.
    async fn player(&self, email: &str, name: &str, link: Option<u64>) -> (UserId, String) {
        let body = json!({"email": email, "password": PASSWORD, "display_name": name});
        let (status, session) = self.http(Method::POST, routes::auth::REGISTER, Some(body), None).await;
        assert_eq!(status, StatusCode::OK, "{session}");
        let user = UserId(session["account"]["id"].as_i64().expect("id"));
        let token = session["tokens"]["access_token"].as_str().expect("token").to_string();
        if let Some(n) = link {
            let linked = self.ok(Method::POST, routes::auth::STEAM, Some(json!({"ticket_hex": ticket(n), "identity": IDENTITY})), &token).await;
            assert_eq!(linked["account"]["id"].as_i64(), Some(user.get()), "linked to the same account: {linked}");
        }
        (user, token)
    }

    /// A Steam-only account (the first Steam login creates it; no display name).
    async fn steam_player(&self, n: u64) -> (UserId, String) {
        let (status, session) = self.http(Method::POST, routes::auth::STEAM, Some(json!({"ticket_hex": ticket(n), "identity": IDENTITY})), None).await;
        assert_eq!(status, StatusCode::OK, "{session}");
        (UserId(session["account"]["id"].as_i64().expect("id")), session["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    /// The lookup's answer as (steam_id, user, name, state) rows.
    async fn lookup(&self, token: &str, ids: &[u64]) -> Vec<(String, i64, Option<String>, Option<String>)> {
        let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
        let answer = self.ok(Method::POST, routes::friends::STEAM, Some(json!({ "steam_ids": ids })), token).await;
        answer["players"]
            .as_array()
            .expect("players")
            .iter()
            .map(|p| {
                (
                    p["steam_id"].as_str().expect("steam_id").to_string(),
                    p["user"].as_i64().expect("user"),
                    p["name"].as_str().map(str::to_string),
                    p["state"].as_str().map(str::to_string),
                )
            })
            .collect()
    }

    async fn findable(&self, token: &str) -> bool {
        self.ok(Method::GET, routes::friends::SETTINGS, None, token).await["steam_findable"].as_bool().expect("flag")
    }
}

fn row(n: u64, user: UserId, name: Option<&str>, state: Option<&str>) -> (String, i64, Option<String>, Option<String>) {
    (steam(n).to_string(), user.get(), name.map(str::to_string), state.map(str::to_string))
}

// ---- the suite ----------------------------------------------------------------------------------

async fn lookup_and_settings(url: &str) {
    let fx = start(url, true, |_| {}).await;
    let (ada, a) = fx.player("ada@example.com", "Ada", Some(1)).await;
    let (bo, b) = fx.steam_player(2).await;
    let (cy, _) = fx.player("cy@example.com", "Cy", Some(3)).await;
    let (dee, d) = fx.player("dee@example.com", "Dee", Some(4)).await;
    let (eve, e) = fx.player("eve@example.com", "Eve", Some(5)).await;
    let (fay, _) = fx.player("fay@example.com", "Fay", Some(6)).await;
    let (hal, h) = fx.player("hal@example.com", "Hal", Some(8)).await;
    let (_gus, g) = fx.player("gus@example.com", "Gus", None).await;
    // steam(7) has no account; steam(1) is the caller's own; steam(2) sent twice.
    let all = [1, 2, 3, 4, 5, 6, 7, 8, 2].map(steam);

    // Everyone is findable at first: in the request's order, once each, never the caller.
    let found = fx.lookup(&a, &all).await;
    assert_eq!(
        found,
        [
            row(2, bo, None, None),
            row(3, cy, Some("Cy"), None),
            row(4, dee, Some("Dee"), None),
            row(5, eve, Some("Eve"), None),
            row(6, fay, Some("Fay"), None),
            row(8, hal, Some("Hal"), None)
        ]
    );
    assert!(fx.lookup(&a, &[]).await.is_empty(), "an empty list answers an empty list");
    assert!(fx.lookup(&a, &[steam(1), steam(7)]).await.is_empty(), "the caller and a Steam ID without an account");

    // Settings: findable by default, `{}` changes nothing, off and on again.
    assert!(fx.findable(&d).await);
    assert_eq!(fx.ok(Method::PUT, routes::friends::SETTINGS, Some(json!({})), &d).await, json!({"steam_findable": true}));
    assert_eq!(fx.ok(Method::PUT, routes::friends::SETTINGS, Some(json!({"steam_findable": false})), &d).await, json!({"steam_findable": false}));
    assert_eq!(fx.ok(Method::PUT, routes::friends::SETTINGS, Some(json!({"steam_findable": false})), &d).await, json!({"steam_findable": false}), "again");
    assert!(!fx.findable(&d).await);
    assert!(fx.findable(&a).await, "another player's setting is its own");

    // A ban (timed), blocks in both directions, relations.
    let auth = fx.state.get::<AuthService>().expect("auth");
    auth.ban_user(&fx.state, cy, BanRequest::new().with_until(UnixMillis(T0 + 60_000))).await.expect("ban");
    fx.ok(Method::PUT, &format!("/v1/friends/blocks/{}", ada.get()), None, &e).await;
    fx.ok(Method::PUT, &format!("/v1/friends/blocks/{}", fay.get()), None, &a).await;
    fx.ok(Method::POST, routes::friends::REQUESTS, Some(json!({"user": hal.get()})), &a).await;
    fx.ok(Method::POST, routes::friends::REQUESTS, Some(json!({"user": ada.get()})), &b).await;
    fx.ok(Method::POST, &format!("/v1/friends/requests/{}/accept", bo.get()), None, &a).await;
    assert_eq!(fx.lookup(&a, &all).await, [row(2, bo, None, Some("friend")), row(8, hal, Some("Hal"), Some("sent"))]);
    assert_eq!(fx.lookup(&h, &[steam(1)]).await, [row(1, ada, Some("Ada"), Some("received"))], "the other side of the request");
    assert!(fx.lookup(&e, &[steam(1)]).await.is_empty(), "the blocker does not find the blocked player either");
    assert_eq!(fx.lookup(&b, &[steam(4), steam(5)]).await, [row(5, eve, Some("Eve"), None)], "hidden for everyone; blocks only between the two");

    // The ban ends; the hidden player turns findable on again.
    fx.clock.advance(61_000);
    fx.ok(Method::PUT, routes::friends::SETTINGS, Some(json!({"steam_findable": true})), &d).await;
    assert_eq!(fx.lookup(&a, &[steam(3), steam(4)]).await, [row(3, cy, Some("Cy"), None), row(4, dee, Some("Dee"), None)]);

    // The hook: refuse one player, remove a Steam ID, never widen the lookup.
    {
        let mut rules = fx.rules.lock().expect("lock");
        rules.refuse = Some(hal);
        rules.remove = Some(steam(3));
        rules.add = Some(steam(4));
    }
    let refused = fx.fails(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": [steam(1).to_string()]})), &h, StatusCode::FORBIDDEN).await;
    assert_eq!(refused["error"]["message"], "not in this game", "{refused}");
    assert_eq!(fx.lookup(&a, &[steam(2), steam(3)]).await, [row(2, bo, None, Some("friend"))], "steam(3) removed, steam(4) not added");
    *fx.rules.lock().expect("lock") = HookRules::default();

    // One audit entry per lookup: the counts, never the Steam IDs.
    let mut query = AuditQuery::new();
    query.action = Some("friends.steam_match".into());
    query.user = Some(ada);
    let entries = auth.audit_log(&fx.state, &query).await.expect("audit").items;
    assert!(entries.len() >= 5, "{entries:?}");
    let newest = serde_json::to_value(&entries[0]).expect("json");
    assert_eq!((newest["data"]["asked"].as_u64(), newest["data"]["found"].as_u64()), (Some(2), Some(1)), "{newest}");
    assert!(!newest.to_string().contains(&steam(2).to_string()), "no Steam ID in the audit log: {newest}");

    // A caller without Steam; malformed Steam IDs; the cap.
    let not_linked = fx.fails(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": [steam(1).to_string()]})), &g, StatusCode::FORBIDDEN).await;
    assert_eq!(not_linked["error"]["code"], codes::FORBIDDEN, "{not_linked}");
    let bad = json!({"steam_ids": ["abc", steam(1).to_string(), "76561197960265728", format!("0{}", steam(1)), "103582791429521412", ""]});
    let answer = fx.fails(Method::POST, routes::friends::STEAM, Some(bad), &a, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(answer["error"]["code"], codes::VALIDATION_FAILED, "{answer}");
    let text = answer.to_string();
    for (index, bad) in [(0, true), (1, false), (2, true), (3, true), (4, true), (5, true)] {
        assert_eq!(text.contains(&format!("steam_ids[{index}]")), bad, "entry {index}: {text}");
    }
    let numbers = fx.http(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": [steam(1)]})), Some(&a)).await;
    assert!(numbers.0.is_client_error(), "a SteamID64 as a JSON number is refused: {numbers:?}");
    let cap: Vec<String> = (1..=501).map(|n| steam(100 + n).to_string()).collect();
    let over = fx.fails(Method::POST, routes::friends::STEAM, Some(json!({ "steam_ids": cap })), &a, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert!(over.to_string().contains("at most 500"), "{over}");
    let full: Vec<u64> = (1..=500).map(|n| steam(100 + n)).collect();
    assert!(fx.lookup(&a, &full).await.is_empty(), "500 Steam IDs are fine (none of them has an account)");

    // Account deletion takes the settings row with it (cascade), unlinking leaves the account out.
    fx.ok(Method::PUT, routes::friends::SETTINGS, Some(json!({"steam_findable": false})), &e).await;
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(eve.get()));
    fx.state.db().execute(&delete).await.expect("delete the account");
    assert!(fx.lookup(&b, &[steam(5)]).await.is_empty());
    auth.unlink_user_identity(&fx.state, dee, "steam").await.expect("unlink");
    assert!(fx.lookup(&a, &[steam(4)]).await.is_empty(), "an unlinked Steam account finds nobody");
    let service = fx.state.get::<FriendService>().expect("friends");
    assert_eq!(service.steam_id_of(&fx.state, ada).await.expect("own"), Some(steam(1)));
    assert_eq!(service.steam_id_of(&fx.state, dee).await.expect("unlinked"), None);
}

/// A fresh database for one part of the suite. `backend`: `memory`, or a MySQL / PostgreSQL base
/// URL.
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
    lookup_and_settings(&url).await;
    #[cfg(any(feature = "mysql", feature = "postgres"))]
    if let Some((base, name)) = cleanup {
        common::drop_database(&base, &name).await;
    }
    #[cfg(not(any(feature = "mysql", feature = "postgres")))]
    let _ = cleanup;
}

// ---- runners ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("memory").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_friends_steam_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_friends_steam_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The per-player rate (429 with `retry_after_ms`, per player, refilled with time), the route
/// without Steam login (404; the settings still work), the setting's bounds in the file, the
/// OpenAPI document.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn rate_and_wiring() {
    let fx = start("sqlite::memory:", true, |c| {
        c.steam_rate = 2;
        c.steam_rate_window_secs = 600;
    })
    .await;
    let (_, a) = fx.player("ada@example.com", "Ada", Some(1)).await;
    let (_, b) = fx.player("bo@example.com", "Bo", Some(2)).await;
    let (_, g) = fx.player("gus@example.com", "Gus", None).await;
    let body = || Some(json!({"steam_ids": [steam(2).to_string()]}));
    for _ in 0..2 {
        fx.ok(Method::POST, routes::friends::STEAM, body(), &a).await;
    }
    let limited = fx.fails(Method::POST, routes::friends::STEAM, body(), &a, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(limited["error"]["code"], codes::RATE_LIMITED, "{limited}");
    assert!(limited["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0 && ms <= 300_000), "{limited}");
    fx.ok(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": [steam(1).to_string()]})), &b).await;
    // Malformed requests do not use up the rate; a caller without Steam is refused after it.
    fx.fails(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": ["x"]})), &b, StatusCode::UNPROCESSABLE_ENTITY).await;
    fx.ok(Method::POST, routes::friends::STEAM, Some(json!({"steam_ids": [steam(1).to_string()]})), &b).await;
    fx.fails(Method::POST, routes::friends::STEAM, body(), &g, StatusCode::FORBIDDEN).await;

    // Without Steam login: 404 for the lookup; the settings still answer.
    let off = start("sqlite::memory:", false, |_| {}).await;
    let (_, a) = off.player("ada@example.com", "Ada", None).await;
    let answer = off.fails(Method::POST, routes::friends::STEAM, body(), &a, StatusCode::NOT_FOUND).await;
    assert_eq!(answer["error"]["code"], codes::NOT_FOUND, "{answer}");
    assert!(off.findable(&a).await);
    assert!(off.http(Method::POST, routes::friends::STEAM, body(), None).await.0 == StatusCode::UNAUTHORIZED, "needs a login");

    // The settings' bounds from the file.
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.friends]\nsteam_max_ids = 2001\n").expect("config");
    let error = NetBackendServer::new(file).module(Auth::new()).module(Friends::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("steam_max_ids"), "{error}");
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.friends]\nsteam_max_ids = 50\nsteam_rate = 0\n").expect("config");
    let built = NetBackendServer::new(file).module(Auth::new()).module(Friends::new()).build().await.expect("build");
    assert_eq!(built.state().get::<FriendService>().expect("service").config().steam_max_ids, 50);

    // The OpenAPI document lists the three routes with their schemas.
    let spec: Value = serde_json::from_str(built.openapi_json()).expect("json");
    assert!(spec["paths"]["/v1/friends/steam"]["post"].is_object());
    assert!(spec["paths"]["/v1/friends/settings"]["get"].is_object() && spec["paths"]["/v1/friends/settings"]["put"].is_object());
    for schema in ["SteamMatch", "SteamMatchResult", "SteamPlayer", "FriendSettings", "UpdateFriendSettings"] {
        assert!(spec["components"]["schemas"][schema].is_object(), "{schema}");
    }
}
