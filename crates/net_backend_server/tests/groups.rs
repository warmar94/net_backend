//! The groups module: create (unique names, rules, metadata, the hooks), the name search and its
//! pages, invitations (a notification, accept, decline, revoke), open groups, roles (admins invite
//! and kick members; the owner sets roles and transfers), leaving (the owner last: the group goes),
//! the members list, deleting, the group chat room (members only, removed members cut off), the
//! limits (members, groups per player, invitations, the create rate) with concurrent joins, the
//! upkeep after an owner's account is deleted, and the wiring. The 0.2.0 review fixes: the
//! invitation rate, blocks and the invite hook, the update hook after the rights check, the chat
//! room deleted with its group (and orphans by the upkeep).
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "groups", feature = "chat", feature = "notifications", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::chat::Chat;
use net_backend_server::groups::events::{AfterGroupChange, BeforeGroupCreate, BeforeGroupInvite, BeforeGroupJoin, BeforeGroupUpdate, GroupChange};
use net_backend_server::groups::{GroupService, Groups, GroupsConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::notifications::{NotificationService, Notifications};
use net_backend_server::protocol::groups::GroupRole;
use net_backend_server::protocol::notifications::NotificationQuery;
use net_backend_server::protocol::{codes, routes, GroupId, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{AppError, AppState, Config, ManualClock, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tower::ServiceExt;

type Changes = Arc<Mutex<Vec<(GroupId, Option<UserId>, Option<UserId>, GroupChange)>>>;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";

struct Server {
    state: AppState,
    router: Router,
    changes: Changes,
    refused: Arc<Mutex<Option<UserId>>>,
    clock: Arc<ManualClock>,
    /// How many `BeforeGroupInvite` / `BeforeGroupUpdate` hooks ran.
    hooks: Arc<AtomicUsize>,
}

async fn start(url: &str, tweak: impl FnOnce(&mut GroupsConfig)) -> Server {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("groups-migrations");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut groups = GroupsConfig::default();
    groups.create_rate = 0;
    tweak(&mut groups);
    let changes: Changes = Arc::new(Mutex::new(Vec::new()));
    let refused: Arc<Mutex<Option<UserId>>> = Arc::new(Mutex::new(None));
    let (seen, refuse) = (changes.clone(), refused.clone());
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let hooks = Arc::new(AtomicUsize::new(0));
    let (invites, updates) = (hooks.clone(), hooks.clone());
    let server = NetBackendServer::new(config).clock(clock.clone()).module(Auth::new().with_config(auth).mailer(MemoryMailer::new()));
    #[cfg(feature = "friends")]
    let server = server.module(net_backend_server::friends::Friends::new());
    let prepared = server
        .module(Chat::new())
        .module(Notifications::new())
        .module(Groups::new().with_config(groups))
        // The game's rule: no invitation for the account 999 998 (a refusal by a hook).
        .before::<BeforeGroupInvite, _, _>(move |_ctx, invite| {
            let invites = invites.clone();
            async move {
                invites.fetch_add(1, Ordering::SeqCst);
                if invite.invitee == UserId(999_998) {
                    return Ok(Decision::Reject(AppError::forbidden("not this one")));
                }
                Ok(Decision::Continue(invite))
            }
        })
        .before::<BeforeGroupUpdate, _, _>(move |_ctx, update| {
            let updates = updates.clone();
            async move {
                updates.fetch_add(1, Ordering::SeqCst);
                Ok(Decision::Continue(update))
            }
        })
        // The game's name filter.
        .before::<BeforeGroupCreate, _, _>(|_ctx, create| async move {
            if create.request.name.to_lowercase().contains("admin") {
                return Ok(Decision::Reject(AppError::forbidden("this name is not allowed")));
            }
            Ok(Decision::Continue(create))
        })
        .before::<BeforeGroupJoin, _, _>(move |_ctx, join| {
            let refuse = refuse.clone();
            async move {
                if *refuse.lock().expect("lock") == Some(join.user) {
                    return Ok(Decision::Reject(AppError::forbidden("banned from groups")));
                }
                Ok(Decision::Continue(join))
            }
        })
        .after::<AfterGroupChange, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.group, event.actor, event.user, event.change));
                Ok(())
            }
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    Server { state: prepared.state().clone(), router: prepared.router(), changes, refused, clock, hooks }
}

impl Server {
    fn service(&self) -> Arc<GroupService> {
        self.state.get::<GroupService>().expect("service")
    }

    async fn register(&self, email: &str, name: &str) -> (UserId, String) {
        let request = Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": email, "password": PASSWORD, "display_name": name}).to_string()))
            .expect("request");
        let (status, _, body) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn http(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
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
        let (status, answer) = self.http(method.clone(), path, body, token).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {answer}");
        answer
    }

    /// The error code of a request that must fail with `status`.
    async fn fails(&self, method: Method, path: &str, body: Option<Value>, token: &str, status: StatusCode) -> String {
        let (got, answer) = self.http(method.clone(), path, body, token).await;
        assert_eq!(got, status, "{method} {path}: {answer}");
        answer["error"]["code"].as_str().unwrap_or_default().to_string()
    }

    async fn create(&self, token: &str, body: Value) -> Value {
        self.ok(Method::POST, routes::groups::LIST, Some(body), token).await
    }

    /// The kinds of a player's notifications with their `data.group`, oldest first.
    async fn notes(&self, user: UserId) -> Vec<(String, Option<i64>)> {
        let service = self.state.get::<NotificationService>().expect("notifications");
        let page = service.list(&self.state, user, &NotificationQuery::new()).await.expect("list");
        page.items.iter().rev().map(|n| (n.kind.clone(), n.data.as_ref().and_then(|d| d["group"].as_i64()))).collect()
    }

    /// Whether the player may read the group's chat room.
    async fn in_chat(&self, room: i64, token: &str) -> bool {
        let (status, body) = self.http(Method::GET, &format!("/v1/chat/rooms/{room}/messages"), None, token).await;
        match status {
            StatusCode::OK => true,
            StatusCode::FORBIDDEN => {
                assert_eq!(body["error"]["code"], codes::NOT_A_MEMBER, "{body}");
                false
            }
            other => panic!("chat history: {other} {body}"),
        }
    }
}

/// Whether the chat room's row exists.
async fn room_exists(server: &Server, room: i64) -> bool {
    #[derive(sqlx::FromRow)]
    struct N {
        n: i64,
    }
    let query = Query::select().expr_as(Expr::col("id").count(), "n").from("chat_rooms").and_where(Expr::col("id").eq(room)).to_owned();
    server.state.db().fetch_one::<N, _>(&query).await.expect("count").n == 1
}

fn names(page: &Value) -> Vec<String> {
    page["items"].as_array().map(|items| items.iter().filter_map(|g| g["name"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn members(page: &Value) -> Vec<(i64, String)> {
    page["items"]
        .as_array()
        .map(|items| items.iter().map(|m| (m["user"].as_i64().unwrap_or(0), m["role"].as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default()
}

// ---- the suite ----------------------------------------------------------------------------------

async fn create_search_and_update(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (_bo, b) = server.register("bo@example.com", "Bo").await;
    let owls = server.create(&a, json!({"name": "  Night Owls ", "description": "We play at night", "metadata": {"tag": "NO"}})).await;
    let id = owls["id"].as_i64().expect("id");
    assert_eq!(
        (owls["name"].as_str(), owls["role"].as_str(), owls["members"].as_u64(), owls["owner"].as_i64(), owls["open"].as_bool()),
        (Some("Night Owls"), Some("owner"), Some(1), Some(ada.get()), Some(false)),
        "{owls}"
    );
    assert_eq!((owls["metadata"]["tag"].as_str(), owls["max_members"].as_u64(), owls["created_at"].as_i64()), (Some("NO"), Some(100), Some(T0)));
    let room = owls["chat_room"].as_i64().expect("a chat room");
    assert!(server.in_chat(room, &a).await && !server.in_chat(room, &b).await, "the room is the members'");
    // Names: unique without regard to case; the rules; the hook; metadata size.
    let fails = |body: Value, status: StatusCode| {
        let server = &server;
        let b = b.clone();
        async move { server.fails(Method::POST, routes::groups::LIST, Some(body), &b, status).await }
    };
    assert_eq!(fails(json!({"name": "NIGHT OWLS"}), StatusCode::CONFLICT).await, codes::CONFLICT);
    assert_eq!(fails(json!({"name": "ab"}), StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(fails(json!({"name": "a\u{202E}bcd"}), StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(fails(json!({"name": "Big Data", "metadata": "x".repeat(3000)}), StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(fails(json!({"name": "The Admins"}), StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    // The search: by name, a prefix without regard to case, pages.
    for name in ["Nightingales", "Dawn Patrol", "Nimbus"] {
        server.create(&b, json!({"name": name, "open": true})).await;
    }
    assert_eq!(names(&server.ok(Method::GET, routes::groups::LIST, None, &a).await), ["Dawn Patrol", "Night Owls", "Nightingales", "Nimbus"]);
    let found = server.ok(Method::GET, "/v1/groups?query=NIGHT", None, &a).await;
    assert_eq!(names(&found), ["Night Owls", "Nightingales"]);
    assert_eq!(found["items"][0]["role"], "owner", "the caller's role in the list");
    assert!(found["items"][1].get("role").is_none());
    let first = server.ok(Method::GET, "/v1/groups?query=n&limit=2", None, &a).await;
    let cursor = first["next_cursor"].as_str().expect("more").to_string();
    let rest = server.ok(Method::GET, &format!("/v1/groups?query=n&limit=2&cursor={}", cursor.replace(' ', "%20")), None, &a).await;
    assert_eq!(
        (names(&first), names(&rest), rest.get("next_cursor")),
        (vec!["Night Owls".to_string(), "Nightingales".to_string()], vec!["Nimbus".to_string()], None)
    );
    assert!(names(&server.ok(Method::GET, "/v1/groups?query=50%25_", None, &a).await).is_empty(), "% and _ are plain characters");
    let long = format!("/v1/groups?query={}", "x".repeat(40));
    assert_eq!(server.fails(Method::GET, &long, None, &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    // Updates: owner / admins only; a taken name; clearing.
    let path = format!("/v1/groups/{id}");
    assert_eq!(server.fails(Method::PATCH, &path, Some(json!({"open": true})), &b, StatusCode::FORBIDDEN).await, codes::NOT_A_MEMBER);
    assert_eq!(server.fails(Method::PATCH, &path, Some(json!({"name": "nimbus"})), &a, StatusCode::CONFLICT).await, codes::CONFLICT);
    let changed = server.ok(Method::PATCH, &path, Some(json!({"name": "Night Owls Club", "description": "", "metadata": null, "open": true})), &a).await;
    assert_eq!((changed["name"].as_str(), changed["open"].as_bool(), changed["role"].as_str()), (Some("Night Owls Club"), Some(true), Some("owner")));
    assert!(changed.get("description").is_none() && changed.get("metadata").is_none(), "{changed}");
    assert_eq!(server.ok(Method::GET, &path, None, &b).await["name"], "Night Owls Club");
    assert_eq!(server.fails(Method::GET, "/v1/groups/999999", None, &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::GET, "/v1/groups/abc", None, &a, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    assert_eq!(server.http(Method::GET, routes::groups::LIST, None, "not-a-token").await.0, StatusCode::UNAUTHORIZED);
}

async fn invitations_roles_and_chat(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    let (dee, d) = server.register("dee@example.com", "Dee").await;
    let owls = server.create(&a, json!({"name": "Night Owls"})).await;
    let id = owls["id"].as_i64().expect("id");
    let room = owls["chat_room"].as_i64().expect("room");
    let at = |tail: &str| format!("/v1/groups/{id}{tail}");
    // Closed: no direct join. An invitation: a notification (once), listed, accepted.
    assert_eq!(server.fails(Method::POST, &at("/join"), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    server.ok(Method::POST, &at("/invites"), Some(json!({"user": bo.get()})), &a).await;
    server.ok(Method::POST, &at("/invites"), Some(json!({"user": bo.get()})), &a).await;
    assert_eq!(server.notes(bo).await, [("groups.invite".to_string(), Some(id))]);
    let invites = server.ok(Method::GET, routes::groups::INVITES, None, &b).await;
    assert_eq!((invites["items"][0]["group"]["id"].as_i64(), invites["items"][0]["inviter"].as_i64()), (Some(id), Some(ada.get())), "{invites}");
    let joined = server.ok(Method::POST, &at("/invites/accept"), None, &b).await;
    assert_eq!((joined["role"].as_str(), joined["members"].as_u64()), (Some("member"), Some(2)));
    assert!(server.ok(Method::GET, routes::groups::INVITES, None, &b).await["items"].as_array().is_some_and(Vec::is_empty), "the invitation is used");
    assert!(server.in_chat(room, &b).await, "a member reads the group's room");
    assert_eq!(server.ok(Method::POST, &at("/join"), None, &b).await["role"], "member", "joining again: the same");
    // Declined and revoked invitations are gone.
    server.ok(Method::POST, &at("/invites"), Some(json!({"user": cy.get()})), &a).await;
    server.ok(Method::POST, &at("/invites/decline"), None, &c).await;
    server.ok(Method::POST, &at("/invites/decline"), None, &c).await;
    assert_eq!(server.fails(Method::POST, &at("/invites/accept"), None, &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    server.ok(Method::POST, &at("/invites"), Some(json!({"user": dee.get()})), &a).await;
    server.ok(Method::DELETE, &at(&format!("/invites/{dee}")), None, &a).await;
    assert_eq!(server.fails(Method::POST, &at("/invites/accept"), None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    // Invitation errors.
    assert_eq!(server.fails(Method::POST, &at("/invites"), Some(json!({"user": bo.get()})), &a, StatusCode::CONFLICT).await, codes::CONFLICT);
    assert_eq!(server.fails(Method::POST, &at("/invites"), Some(json!({"user": 999_999})), &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(
        server.fails(Method::POST, &at("/invites"), Some(json!({"user": ada.get()})), &a, StatusCode::UNPROCESSABLE_ENTITY).await,
        codes::VALIDATION_FAILED
    );
    assert_eq!(
        server.fails(Method::POST, &at("/invites"), Some(json!({"user": cy.get()})), &b, StatusCode::FORBIDDEN).await,
        codes::FORBIDDEN,
        "members do not invite"
    );
    assert_eq!(server.fails(Method::POST, &at("/invites"), Some(json!({"user": dee.get()})), &c, StatusCode::FORBIDDEN).await, codes::NOT_A_MEMBER);
    // Roles: the owner makes Bo an admin; admins invite and kick members, not admins or the owner.
    server.ok(Method::PUT, &at(&format!("/members/{bo}/role")), Some(json!({"role": "admin"})), &a).await;
    server.ok(Method::PATCH, &at(""), Some(json!({"open": true})), &b).await;
    server.ok(Method::POST, &at("/join"), None, &c).await;
    server.ok(Method::POST, &at("/join"), None, &d).await;
    server.ok(Method::PUT, &at(&format!("/members/{cy}/role")), Some(json!({"role": "admin"})), &a).await;
    assert_eq!(server.fails(Method::DELETE, &at(&format!("/members/{cy}")), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::DELETE, &at(&format!("/members/{ada}")), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(
        server.fails(Method::PUT, &at(&format!("/members/{dee}/role")), Some(json!({"role": "admin"})), &b, StatusCode::FORBIDDEN).await,
        codes::FORBIDDEN
    );
    assert!(server.in_chat(room, &d).await);
    server.ok(Method::DELETE, &at(&format!("/members/{dee}")), None, &b).await;
    server.ok(Method::DELETE, &at(&format!("/members/{dee}")), None, &b).await;
    assert!(!server.in_chat(room, &d).await, "a kicked member is out of the room");
    assert_eq!(server.notes(dee).await, [("groups.invite".to_string(), Some(id)), ("groups.kicked".to_string(), Some(id))], "the revoked invitation, the kick");
    for bad in [json!({"role": "owner"}), json!({"role": "chief"})] {
        assert_eq!(
            server.fails(Method::PUT, &at(&format!("/members/{cy}/role")), Some(bad), &a, StatusCode::UNPROCESSABLE_ENTITY).await,
            codes::VALIDATION_FAILED
        );
    }
    assert_eq!(server.fails(Method::DELETE, &at(&format!("/members/{ada}")), None, &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(
        server.fails(Method::PUT, &at(&format!("/members/{dee}/role")), Some(json!({"role": "admin"})), &a, StatusCode::NOT_FOUND).await,
        codes::NOT_FOUND
    );
    // The members list: join order, roles, names, pages.
    let list = server.ok(Method::GET, &at("/members"), None, &d).await;
    assert_eq!(members(&list), [(ada.get(), "owner".into()), (bo.get(), "admin".into()), (cy.get(), "admin".into())]);
    assert_eq!(list["items"][0]["name"], "Ada");
    let first = server.ok(Method::GET, &at("/members?limit=2"), None, &a).await;
    let next = server.ok(Method::GET, &at(&format!("/members?limit=2&cursor={}", first["next_cursor"].as_str().expect("more"))), None, &a).await;
    assert_eq!(members(&next), [(cy.get(), "admin".into())]);
    // The owner leaves last: transfer first; the old owner is an admin then and may leave.
    assert_eq!(server.fails(Method::POST, &at("/leave"), None, &a, StatusCode::CONFLICT).await, codes::CONFLICT);
    assert_eq!(server.fails(Method::POST, &at("/transfer"), Some(json!({"user": dee.get()})), &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &at("/transfer"), Some(json!({"user": cy.get()})), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    server.ok(Method::POST, &at("/transfer"), Some(json!({"user": bo.get()})), &a).await;
    let now = server.ok(Method::GET, &at(""), None, &a).await;
    assert_eq!((now["owner"].as_i64(), now["role"].as_str()), (Some(bo.get()), Some("admin")));
    assert_eq!(server.service().role_of(&server.state, GroupId(id), bo).await.expect("role"), Some(GroupRole::Owner));
    server.ok(Method::POST, &at("/leave"), None, &a).await;
    server.ok(Method::POST, &at("/leave"), None, &a).await;
    assert!(!server.in_chat(room, &a).await, "a member who left is out of the room");
    // Mine: the caller's groups with its role.
    let mine = server.ok(Method::GET, routes::groups::MINE, None, &b).await;
    assert_eq!(
        (mine["groups"][0]["id"].as_i64(), mine["groups"][0]["role"].as_str(), mine["groups"][0]["members"].as_u64()),
        (Some(id), Some("owner"), Some(2))
    );
    assert!(server.ok(Method::GET, routes::groups::MINE, None, &a).await["groups"].as_array().is_some_and(Vec::is_empty));
    // The join hook refuses.
    *server.refused.lock().expect("lock") = Some(dee);
    assert_eq!(server.fails(Method::POST, &at("/join"), None, &d, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    // Deleting: the owner only; everything goes, the chat room too.
    assert_eq!(server.fails(Method::DELETE, &at(""), None, &c, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    server.ok(Method::DELETE, &at(""), None, &b).await;
    assert_eq!(server.fails(Method::GET, &at(""), None, &b, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    for token in [&b, &c] {
        assert_eq!(server.fails(Method::GET, &format!("/v1/chat/rooms/{room}/messages"), None, token, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    }
    // A group whose last member (the owner) leaves is deleted.
    let solo = server.create(&d, json!({"name": "Solo"})).await["id"].as_i64().expect("id");
    server.ok(Method::POST, &format!("/v1/groups/{solo}/leave"), None, &d).await;
    assert_eq!(server.fails(Method::GET, &format!("/v1/groups/{solo}"), None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    // The after hook saw the changes.
    let changes = server.changes.lock().expect("lock").clone();
    let group = GroupId(id);
    for expected in [
        (group, Some(ada), Some(ada), GroupChange::Created),
        (group, Some(ada), Some(bo), GroupChange::Invited),
        (group, Some(bo), Some(bo), GroupChange::Joined),
        (group, Some(cy), Some(cy), GroupChange::InviteDeclined),
        (group, Some(ada), Some(dee), GroupChange::InviteRevoked),
        (group, Some(ada), Some(bo), GroupChange::RoleChanged(GroupRole::Admin)),
        (group, Some(bo), Some(dee), GroupChange::Kicked),
        (group, Some(ada), Some(bo), GroupChange::Transferred),
        (group, Some(ada), Some(ada), GroupChange::Left),
        (group, Some(bo), Some(bo), GroupChange::Deleted),
        (GroupId(solo), Some(dee), Some(dee), GroupChange::Deleted),
    ] {
        assert!(changes.contains(&expected), "{expected:?} in {changes:?}");
    }
    assert_eq!(changes.iter().filter(|c| c.3 == GroupChange::Invited && c.2 == Some(bo)).count(), 1, "inviting again is no change");
}

async fn limits_and_concurrency(url: &str) {
    let server = start(url, |g| {
        g.max_members = 4;
        g.max_groups_per_user = 2;
        g.max_invites = 2;
    })
    .await;
    let mut players = Vec::new();
    for i in 0..8 {
        players.push(server.register(&format!("p{i}@example.com"), &format!("P{i}")).await);
    }
    let (_p0, t0) = players[0].clone();
    // Groups per player: 2.
    let open = server.create(&t0, json!({"name": "Open One", "open": true})).await["id"].as_i64().expect("id");
    server.create(&t0, json!({"name": "Second"})).await;
    assert_eq!(server.fails(Method::POST, routes::groups::LIST, Some(json!({"name": "Third"})), &t0, StatusCode::FORBIDDEN).await, codes::QUOTA_EXCEEDED);
    // Invitations per group: 2.
    let second = server.ok(Method::GET, routes::groups::MINE, None, &t0).await["groups"][1]["id"].as_i64().expect("id");
    for (user, _) in &players[1..3] {
        server.ok(Method::POST, &format!("/v1/groups/{second}/invites"), Some(json!({"user": user.get()})), &t0).await;
    }
    let over = server.fails(Method::POST, &format!("/v1/groups/{second}/invites"), Some(json!({"user": players[3].0.get()})), &t0, StatusCode::FORBIDDEN).await;
    assert_eq!(over, codes::QUOTA_EXCEEDED);
    // Members per group: 7 players join at once, exactly 3 get in (4 with the owner).
    let mut tasks = Vec::new();
    for (user, _) in &players[1..8] {
        let (state, service, user) = (server.state.clone(), server.service(), *user);
        tasks.push(tokio::spawn(async move {
            let ctx = net_backend_server::HookCtx::new(state.clone(), None);
            service.join(&state, &ctx, user, GroupId(open), false).await.map(|_| ())
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.expect("task") {
            Ok(()) => ok += 1,
            Err(error) => assert_eq!(error.code(), codes::QUOTA_EXCEEDED, "{error:?}"),
        }
    }
    assert_eq!(ok, 3);
    assert_eq!(server.ok(Method::GET, &format!("/v1/groups/{open}"), None, &t0).await["members"], 4);
    assert_eq!(server.service().members_of(&server.state, GroupId(open)).await.expect("members").len(), 4);
    // Account deletion: the member goes with its account, the count follows.
    let member = server.service().members_of(&server.state, GroupId(open)).await.expect("members")[1];
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(member.get()));
    server.state.db().execute(&delete).await.expect("delete");
    assert_eq!(server.ok(Method::GET, &format!("/v1/groups/{open}"), None, &t0).await["members"], 3);
}

/// The upkeep after an owner's account is deleted straight from the database: the oldest admin
/// (before an older plain member) owns the group, else the oldest member; a group with nobody left
/// is deleted; the after hook sees the server as the actor.
async fn owner_upkeep(url: &str) {
    let server = start(url, |_| {}).await;
    let (owner, o) = server.register("up-owner@example.com", "Owner").await;
    let (m1, t1) = server.register("up-m1@example.com", "M1").await;
    let (a1, ta1) = server.register("up-a1@example.com", "A1").await;
    let (a2, ta2) = server.register("up-a2@example.com", "A2").await;
    let (m2, t2) = server.register("up-m2@example.com", "M2").await;
    let first = server.create(&o, json!({"name": "Officers First", "open": true})).await["id"].as_i64().expect("id");
    let second = server.create(&o, json!({"name": "Members Only", "open": true})).await["id"].as_i64().expect("id");
    let third = server.create(&o, json!({"name": "Alone"})).await["id"].as_i64().expect("id");
    // m1 joins before the admins; a1 before a2.
    for token in [&t1, &ta1, &ta2] {
        server.ok(Method::POST, &format!("/v1/groups/{first}/join"), None, token).await;
    }
    for user in [a1, a2] {
        server.ok(Method::PUT, &format!("/v1/groups/{first}/members/{}/role", user.get()), Some(json!({"role": "admin"})), &o).await;
    }
    server.ok(Method::POST, &format!("/v1/groups/{second}/join"), None, &t2).await;
    assert_eq!(server.service().upkeep(&server.state).await.expect("upkeep"), 0, "every group has its owner");

    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(owner.get()));
    server.state.db().execute(&delete).await.expect("delete");
    let info = server.ok(Method::GET, &format!("/v1/groups/{first}"), None, &t1).await;
    assert!(info.get("owner").is_none(), "no owner before the upkeep: {info}");
    assert_eq!(server.service().upkeep(&server.state).await.expect("upkeep"), 3);

    let info = server.ok(Method::GET, &format!("/v1/groups/{first}"), None, &ta1).await;
    assert_eq!((info["owner"].as_i64(), info["role"].as_str()), (Some(a1.get()), Some("owner")), "{info}");
    let roles = members(&server.ok(Method::GET, &format!("/v1/groups/{first}/members"), None, &t1).await);
    assert_eq!(roles, vec![(m1.get(), "member".to_string()), (a1.get(), "owner".to_string()), (a2.get(), "admin".to_string())]);
    // The new owner has the owner's rights.
    server.ok(Method::PUT, &format!("/v1/groups/{first}/members/{}/role", m1.get()), Some(json!({"role": "admin"})), &ta1).await;
    let info = server.ok(Method::GET, &format!("/v1/groups/{second}"), None, &t2).await;
    assert_eq!((info["owner"].as_i64(), info["role"].as_str()), (Some(m2.get()), Some("owner")), "{info}");
    assert_eq!(server.fails(Method::GET, &format!("/v1/groups/{third}"), None, &t1, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.service().upkeep(&server.state).await.expect("upkeep"), 0, "nothing left to fix");

    let changes = server.changes.lock().expect("lock").clone();
    for expected in [
        (GroupId(first), None, Some(a1), GroupChange::Transferred),
        (GroupId(second), None, Some(m2), GroupChange::Transferred),
        (GroupId(third), None, None, GroupChange::Deleted),
    ] {
        assert!(changes.contains(&expected), "{expected:?} in {changes:?}");
    }
    assert_eq!(changes.iter().filter(|c| c.1.is_none()).count(), 3, "the upkeep's changes only: {changes:?}");
}

/// A fresh database for one part of the suite. `backend`: `memory`, `file`, or a MySQL /
/// PostgreSQL base URL.
async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("groups-file");
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

/// The 0.2.0 review fixes: the invitation rate, a block refuses, the invite hook (after the
/// rights check, like the update hook), the chat room deleted with its group (delete, the owner
/// leaving last, the upkeep) and the orphans the upkeep deletes.
async fn review_fixes(url: &str) {
    let server = start(url, |g| g.invite_rate = 3).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (_bo, b) = server.register("bo@example.com", "Bo").await;
    let mut others = Vec::new();
    for n in 0..4 {
        others.push(server.register(&format!("p{n}@example.com"), &format!("P{n}")).await);
    }
    let group = server.create(&a, json!({"name": "Night Owls", "open": true})).await;
    let id = group["id"].as_i64().expect("id");
    let at = |tail: &str| format!("/v1/groups/{id}{tail}");
    server.ok(Method::POST, &at("/join"), None, &b).await;

    // A member (not an admin) is refused before the hooks run.
    let before = server.hooks.load(Ordering::SeqCst);
    assert_eq!(server.fails(Method::POST, &at("/invites"), Some(json!({"user": others[0].0.get()})), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(json!({"description": "x"})), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.hooks.load(Ordering::SeqCst), before, "no hook saw a refused request");
    // The hook refuses one account; the refusal counts on the rate like any invitation.
    assert_eq!(server.fails(Method::POST, &at("/invites"), Some(json!({"user": 999_998})), &a, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.hooks.load(Ordering::SeqCst), before + 1);

    // A player who blocked the inviter is not invited (with the friends module).
    #[cfg(feature = "friends")]
    {
        let (_, t) = &others[3];
        server.ok(Method::PUT, &format!("/v1/friends/blocks/{}", ada.get()), None, t).await;
        let code = server.fails(Method::POST, &at("/invites"), Some(json!({"user": others[3].0.get()})), &a, StatusCode::FORBIDDEN).await;
        assert_eq!(code, codes::FORBIDDEN);
        assert!(server.notes(others[3].0).await.is_empty(), "no notification");
    }
    #[cfg(not(feature = "friends"))]
    let _ = ada;
    // The rate (3 here): the host's next invitations pass until the bucket is empty.
    let mut limited = false;
    for (user, _) in &others[..3] {
        let (status, body) = server.http(Method::POST, &at("/invites"), Some(json!({"user": user.get()})), &a).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(body["error"]["code"], codes::RATE_LIMITED);
            limited = true;
            break;
        }
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert!(limited, "the invitations are rate limited");

    // SF-2: deleting the group deletes its chat room; so does the owner leaving last, and the upkeep.
    let room = group["chat_room"].as_i64().expect("room");
    assert!(room_exists(&server, room).await);
    server.ok(Method::DELETE, &at(""), None, &a).await;
    assert!(!room_exists(&server, room).await, "deleted with its group");
    let solo = server.create(&b, json!({"name": "Solo"})).await;
    let solo_room = solo["chat_room"].as_i64().expect("room");
    server.ok(Method::POST, &format!("/v1/groups/{}/leave", solo["id"]), None, &b).await;
    assert!(!room_exists(&server, solo_room).await, "the owner left last");
    let (gone, g) = server.register("gone@example.com", "Gone").await;
    let lonely = server.create(&g, json!({"name": "Lonely"})).await;
    let lonely_room = lonely["chat_room"].as_i64().expect("room");
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(gone.get()));
    server.state.db().execute(&delete).await.expect("delete");
    assert_eq!(server.service().upkeep(&server.state).await.expect("upkeep"), 1);
    assert!(!room_exists(&server, lonely_room).await, "the upkeep deleted the group and its room");

    // An orphan (the group row went without its room): the upkeep deletes it after an hour; a
    // living group keeps its room.
    let (_, d) = server.register("dee@example.com", "Dee").await;
    let living = server.create(&d, json!({"name": "Living"})).await;
    let living_room = living["chat_room"].as_i64().expect("room");
    let orphan = server.create(&d, json!({"name": "Orphan"})).await;
    let orphan_room = orphan["chat_room"].as_i64().expect("room");
    let mut delete = Query::delete();
    delete.from_table("game_groups").and_where(Expr::col("id").eq(orphan["id"].as_i64().expect("id")));
    server.state.db().execute(&delete).await.expect("delete the group row");
    server.service().upkeep(&server.state).await.expect("upkeep");
    assert!(room_exists(&server, orphan_room).await, "younger than an hour: kept");
    server.clock.advance(3_600_001);
    server.service().upkeep(&server.state).await.expect("upkeep");
    assert!(!room_exists(&server, orphan_room).await, "the orphan is gone");
    assert!(room_exists(&server, living_room).await, "a living group keeps its room");
}

async fn suite(backend: &str) {
    each!(backend, create_search_and_update, invitations_roles_and_chat, limits_and_concurrency, owner_upkeep, review_fixes);
}

// ---- runners ------------------------------------------------------------------------------------

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
async fn mysql_groups_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_groups_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The module's wiring: needs `auth` first, reads `[modules.groups]`, refuses settings given twice
/// or unknown keys, lists its routes in the OpenAPI document and its name in `/v1/info`; works
/// without the chat module (no room) and without the hub; the create rate.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_documents_and_rate() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Groups::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.groups]\nmax_members = 7\n").expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Groups::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<GroupService>().expect("service").config().max_members, 7);
    let error = NetBackendServer::new(file).module(Auth::new()).module(Groups::new().with_config(GroupsConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.groups]\nmax_member = 7\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Groups::new()).build().await.is_err(), "unknown keys are refused");

    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Groups::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/groups")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let (_, _, info) = common::call(&prepared.router(), common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "groups"]));

    // No chat module, no hub: groups work without a room; the create rate (2 per hour).
    let mut no_ws = config();
    no_ws.ws.enabled = false;
    let mut groups = GroupsConfig::default();
    groups.create_rate = 2;
    let prepared = NetBackendServer::new(no_ws).module(Auth::new()).module(Groups::new().with_config(groups)).build().await.expect("build without ws");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let request = Request::post(routes::auth::REGISTER)
        .header("content-type", "application/json")
        .body(Body::from(json!({"email": "solo@example.com", "password": PASSWORD}).to_string()))
        .expect("request");
    let (_, _, session) = common::call(&router, request).await;
    let token = session["tokens"]["access_token"].as_str().expect("token").to_string();
    let create = |name: &str| {
        Request::post(routes::groups::LIST)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"name": name}).to_string()))
            .expect("request")
    };
    let (status, _, group) = common::call(&router, create("First")).await;
    assert_eq!(status, StatusCode::OK, "{group}");
    assert!(group.get("chat_room").is_none(), "no chat module: no room");
    assert_eq!(common::call(&router, create("Second")).await.0, StatusCode::OK);
    let (status, _, body) = common::call(&router, create("Third")).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
}
