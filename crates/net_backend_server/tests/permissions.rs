//! Permissions: the declarations of modules and the game, the defaults, `[permissions]` (replacing a
//! role's defaults, granting to new roles), `admin` holding every declared permission, the
//! `RequirePermission` extractor and `AuthContext::require_permission` on a game route, a role granted
//! at run time working on the next request, the lobbies module's `lobbies.manage` (staff act as the
//! host of any lobby), and the build errors (an undeclared name in `[permissions]`, `admin` listed, a
//! module permission without the module's prefix, a name declared twice).
#![cfg(all(feature = "sqlite", feature = "lobbies"))]

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::routing::get;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::lobbies::Lobbies;
use net_backend_server::permissions::{Permission, PermissionName, Permissions, RequirePermission};
use net_backend_server::protocol::{codes, routes, UserId};
use net_backend_server::{AppError, AppState, AuthContext, Config, Module, NetBackendServer};
use serde_json::{json, Value};

const PASSWORD: &str = "correct horse battery";
const MUTE: Permission = Permission::new("game.mute", "Mute a player in the game's own chat").granted_to(&["moderator"]);
const BAN_WORDS: Permission = Permission::new("game.words", "Change the word filter");

struct Mute;
impl PermissionName for Mute {
    const NAME: &'static str = MUTE.name();
}

async fn mute(RequirePermission(staff, ..): RequirePermission<Mute>) -> String {
    format!("muted by {}", staff.user_id)
}

async fn words(State(state): State<AppState>, caller: AuthContext) -> Result<String, AppError> {
    caller.require_permission(&state, BAN_WORDS.name())?;
    Ok("words changed".into())
}

fn auth() -> Auth {
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.rate_limits = false;
    Auth::new().with_config(auth)
}

fn config(extra: &str) -> Config {
    let mut config = Config::from_toml_str(&format!("[database]\nurl = \"sqlite::memory:\"\n{extra}")).expect("config");
    config.database.migrations_dir = common::temp_dir("permissions-migrations");
    config
}

async fn call(router: &axum::Router, method: Method, path: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let (status, _, body) = common::call(router, request).await;
    (status, body)
}

#[tokio::test]
async fn roles_grants_and_the_lobbies_staff_permission() {
    let prepared = NetBackendServer::new(config("[permissions]\nsupport = [\"game.mute\", \"game.words\", \"lobbies.manage\"]\n"))
        .module(auth())
        .module(Lobbies::new())
        .permission(MUTE)
        .permission(BAN_WORDS)
        .route("/v1/game/mute", get(mute))
        .route("/v1/game/words", get(words))
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let (state, router) = (prepared.state().clone(), prepared.router());
    let mut players = Vec::new();
    for name in ["host", "player", "moderator", "support", "admin"] {
        let request = common::post_json(routes::auth::REGISTER, json!({"email": format!("{name}@example.com"), "password": PASSWORD}).to_string());
        let (status, _, body) = common::call(&router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        players.push((UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string()));
    }
    let auth = state.get::<AuthService>().expect("auth");
    for (index, role) in [(2, "moderator"), (3, "support"), (4, "admin")] {
        auth.set_user_role(&state, players[index].0, role, true).await.expect("grant");
    }
    let token = |n: usize| players[n].1.clone();

    // The declarations and the effective grants.
    let permissions = state.get::<Permissions>().expect("permissions");
    let names: Vec<&str> = permissions.declared().map(|p| p.name()).collect();
    assert_eq!(names, ["game.mute", "game.words", "lobbies.manage"]);
    assert_eq!(permissions.roles_with("lobbies.manage"), ["moderator", "support"]);
    assert_eq!(permissions.of_roles(&["support".into()]), ["game.mute", "game.words", "lobbies.manage"]);

    // The game's routes: 401 without a caller, 403 without the permission, 200 with it.
    let anonymous = common::call(&router, Request::get("/v1/game/mute").body(Body::empty()).expect("request")).await;
    assert_eq!(anonymous.0, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&router, Method::GET, "/v1/game/mute", &token(1), None).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::FORBIDDEN)), "{body}");
    for n in [2, 3, 4] {
        assert_eq!(call(&router, Method::GET, "/v1/game/mute", &token(n), None).await.0, StatusCode::OK, "player {n}");
    }
    assert_eq!(call(&router, Method::GET, "/v1/game/words", &token(2), None).await.0, StatusCode::FORBIDDEN, "admin only by default");
    assert_eq!(call(&router, Method::GET, "/v1/game/words", &token(3), None).await.0, StatusCode::OK, "granted in [permissions]");
    assert_eq!(call(&router, Method::GET, "/v1/game/words", &token(4), None).await.0, StatusCode::OK, "admin holds every one");
    // A role granted at run time works on the next request.
    auth.set_user_role(&state, players[1].0, "moderator", true).await.expect("grant");
    assert_eq!(call(&router, Method::GET, "/v1/game/mute", &token(1), None).await.0, StatusCode::OK);
    auth.set_user_role(&state, players[1].0, "moderator", false).await.expect("revoke");

    // lobbies.manage: staff act as the host of another player's lobby.
    let (status, lobby) = call(&router, Method::POST, routes::lobbies::LIST, &token(0), Some(json!({"max_players": 4}))).await;
    assert_eq!(status, StatusCode::OK, "{lobby}");
    let id = lobby["id"].as_i64().expect("id");
    assert_eq!(call(&router, Method::POST, &format!("/v1/lobbies/{id}/join"), &token(1), None).await.0, StatusCode::OK);
    let change = json!({"metadata": {"note": "checked"}});
    assert_eq!(call(&router, Method::PATCH, &format!("/v1/lobbies/{id}"), &token(1), Some(change.clone())).await.0, StatusCode::FORBIDDEN);
    let (status, changed) = call(&router, Method::PATCH, &format!("/v1/lobbies/{id}"), &token(2), Some(change)).await;
    assert_eq!((status, changed["metadata"]["note"].as_str()), (StatusCode::OK, Some("checked")), "a moderator by default");
    let kick = format!("/v1/lobbies/{id}/members/{}", players[1].0);
    assert_eq!(call(&router, Method::DELETE, &kick, &token(3), None).await.0, StatusCode::OK, "support by [permissions]");
    let (status, closed) = call(&router, Method::PATCH, &format!("/v1/lobbies/{id}"), &token(4), Some(json!({"state": "closed"}))).await;
    assert_eq!((status, closed["state"].as_str()), (StatusCode::OK, Some("closed")), "admin");
}

/// A module declaring a permission outside its own name.
struct Stray;
impl Module for Stray {
    fn name(&self) -> &'static str {
        "stray"
    }
    fn permissions(&self) -> Vec<Permission> {
        vec![Permission::new("other.thing", "x")]
    }
}

#[tokio::test]
async fn build_errors() {
    let error =
        |config: Config, server: fn(Config) -> NetBackendServer| async move { server(config).build().await.err().map(|e| e.to_string()).unwrap_or_default() };
    let typo = error(config("[permissions]\nmoderator = [\"game.mutte\"]\n"), |c| NetBackendServer::new(c).module(auth()).permission(MUTE)).await;
    assert!(typo.contains("`game.mutte`") && typo.contains("a typo"), "{typo}");
    let admin = error(config("[permissions]\nadmin = []\n"), |c| NetBackendServer::new(c).module(auth())).await;
    assert!(admin.contains("permissions.admin"), "{admin}");
    let stray = error(config(""), |c| NetBackendServer::new(c).module(auth()).module(Stray)).await;
    assert!(stray.contains("must start with `stray.`"), "{stray}");
    let twice = error(config(""), |c| NetBackendServer::new(c).module(auth()).permission(MUTE).permission(MUTE)).await;
    assert!(twice.contains("declared twice"), "{twice}");
    assert!(Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[permissions]\nmoderator = 3\n").is_err(), "a list of names");
    // Without anything declared every check says no, also for admin.
    let prepared = NetBackendServer::new(config("")).module(auth()).build().await.expect("build");
    assert_eq!(prepared.state().get::<Permissions>().expect("permissions").declared().count(), 0);
    let admin = AuthContext::new(UserId(1)).with_roles(vec!["admin".into()]);
    assert!(!admin.has_permission(prepared.state(), "game.mute"));
}
