//! The leaderboards module through the assembled router (in process): the boards and their
//! periods, every score mode (best / latest / sum) and order (desc / asc), unique ranks with ties
//! broken by time, pages with cursors, the caller's rank and the ranks around it, metadata and
//! names, daily / weekly resets with reads of a finished period and the purge of old periods, the
//! hooks, server-only boards, the submit rate, account deletion, concurrent submissions (one
//! player's sum never loses one; many players' first scores at once), the wiring and the
//! documents.
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "leaderboards", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::Router;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::leaderboards::events::{AfterScoreSubmit, BeforeScoreSubmit};
use net_backend_server::leaderboards::{BoardSpec, LeaderboardService, Leaderboards, LeaderboardsConfig};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::leaderboards::{Period, ScoreMode, ScoreOrder, SubmitScore};
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{AppError, AppState, Config, ManualClock, NetBackendServer, PreparedServer, SecretString};
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const PASSWORD: &str = "correct horse battery";

/// What the after hook saw: (board, score, changed, rank).
type Seen = Arc<std::sync::Mutex<Vec<(String, i64, bool, u64)>>>;

struct Fx {
    prepared: PreparedServer,
    router: Router,
    clock: Arc<ManualClock>,
    after: Seen,
}

fn boards() -> Vec<BoardSpec> {
    vec![
        BoardSpec::new("highscore").with_name("High score"),
        BoardSpec::new("race").with_order(ScoreOrder::Asc),
        BoardSpec::new("latest").with_mode(ScoreMode::Latest),
        BoardSpec::new("kills").with_mode(ScoreMode::Sum),
        BoardSpec::new("daily").with_period(Period::Daily),
        BoardSpec::new("weekly").with_mode(ScoreMode::Sum).with_period(Period::Weekly),
        BoardSpec::new("ranked").with_client_submit(false),
    ]
}

async fn fixture(url: &str, tweak: impl FnOnce(&mut LeaderboardsConfig)) -> Fx {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("leaderboards-migrations");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    // The tests move the clock by days.
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    auth.refresh_token_ttl_secs = 30 * 24 * 3600;
    let mut lb = LeaderboardsConfig::default();
    lb.boards = boards();
    lb.purge_interval_secs = 0;
    lb.submit_rate = 0;
    tweak(&mut lb);
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let after: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = after.clone();
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Leaderboards::new().with_config(lb))
        // A game rule: no high score above 1 000 000; a score of 777 counts as 700.
        .before::<BeforeScoreSubmit, _, _>(|_ctx, mut submit| async move {
            if submit.board == "highscore" && submit.score > 1_000_000 {
                return Ok(Decision::Reject(AppError::forbidden("not possible")));
            }
            if submit.score == 777 {
                submit.score = 700;
            }
            Ok(Decision::Continue(submit))
        })
        .after::<AfterScoreSubmit, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.board.clone(), event.score, event.changed, event.rank));
                Ok(())
            }
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    Fx { prepared, router, clock, after }
}

impl Fx {
    fn state(&self) -> &AppState {
        self.prepared.state()
    }

    fn service(&self) -> Arc<LeaderboardService> {
        self.state().get::<LeaderboardService>().expect("service")
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

    async fn players(&self, prefix: &str, n: usize) -> Vec<(UserId, String)> {
        let mut out = Vec::new();
        for i in 0..n {
            out.push(self.register(&format!("{prefix}{i}@example.com"), &format!("{prefix}{i}")).await);
        }
        out
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> (StatusCode, Value) {
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

    async fn submit(&self, token: &str, board: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, &format!("/v1/leaderboards/{board}/scores"), Some(body), token).await
    }

    async fn ok_submit(&self, token: &str, board: &str, score: i64) -> Value {
        let (status, ack) = self.submit(token, board, json!({ "score": score })).await;
        assert_eq!(status, StatusCode::OK, "{board} {score}: {ack}");
        ack
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        let (status, body) = self.send(Method::GET, path, None, token).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }
}

fn users_of(page: &Value) -> Vec<i64> {
    page["items"].as_array().map(|items| items.iter().filter_map(|e| e["user"].as_i64()).collect()).unwrap_or_default()
}

fn ranks_of(page: &Value) -> Vec<u64> {
    page["items"].as_array().map(|items| items.iter().filter_map(|e| e["rank"].as_u64()).collect()).unwrap_or_default()
}

// ---- the suite ----------------------------------------------------------------------------------

async fn boards_and_errors(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (_, token) = fx.register("ada@example.com", "Ada").await;
    let list = fx.get(routes::leaderboards::BOARDS, &token).await;
    let keys: Vec<&str> = list["boards"].as_array().map(|b| b.iter().filter_map(|x| x["key"].as_str()).collect()).unwrap_or_default();
    assert_eq!(keys, ["daily", "highscore", "kills", "latest", "race", "ranked", "weekly"], "ordered by key");
    let daily = &list["boards"][0];
    let start = Period::Daily.start_of(UnixMillis(T0)).expect("start").get();
    assert_eq!((daily["period"].as_str(), daily["period_start"].as_i64(), daily["period_end"].as_i64()), (Some("daily"), Some(start), Some(start + DAY)));
    let high = &list["boards"][1];
    assert_eq!((high["name"].as_str(), high["mode"].as_str(), high["order"].as_str()), (Some("High score"), Some("best"), Some("desc")));
    assert!(high.get("period_start").is_none(), "all-time boards have no period: {high}");
    assert_eq!(list["boards"][5]["client_submit"], false);
    // Unknown board, invalid key, no token, malformed bodies.
    assert_eq!(fx.send(Method::GET, "/v1/leaderboards/nope", None, &token).await.0, StatusCode::NOT_FOUND);
    assert_eq!(fx.submit(&token, "nope", json!({"score": 1})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(fx.send(Method::GET, "/v1/leaderboards/High", None, &token).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(fx.send(Method::GET, routes::leaderboards::BOARDS, None, "not-a-token").await.0, StatusCode::UNAUTHORIZED);
    let (status, body) = fx.submit(&token, "highscore", json!({"score": i64::MIN})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(codes::VALIDATION_FAILED)), "{body}");
    let (status, _) = fx.submit(&token, "highscore", json!({"score": 1, "metadata": "x".repeat(2000)})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "metadata over the limit");
    assert_eq!(fx.submit(&token, "highscore", json!({"score": "ten"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(fx.send(Method::GET, "/v1/leaderboards/highscore?cursor=x.y.z", None, &token).await.0, StatusCode::BAD_REQUEST);
    // An empty board.
    let empty = fx.get("/v1/leaderboards/highscore", &token).await;
    assert_eq!((empty["items"].as_array().map(Vec::len), empty.get("next_cursor")), (Some(0), None));
    let me = fx.get("/v1/leaderboards/highscore/me", &token).await;
    assert_eq!((me.get("entry"), me["total"].as_u64()), (None, Some(0)));
    let around = fx.get("/v1/leaderboards/highscore/around", &token).await;
    assert_eq!(around["items"].as_array().map(Vec::len), Some(0));
}

async fn modes_orders_and_ties(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let (ada, a) = fx.register("ada@example.com", "Ada").await;
    let (bo, b) = fx.register("bo@example.com", "Bo").await;
    let (cy, c) = fx.register("cy@example.com", "Cy").await;
    // best / desc: a lower score keeps the stored one, a higher one replaces it with its metadata.
    let (status, ack) = fx.submit(&a, "highscore", json!({"score": 500, "metadata": {"car": "red"}})).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!((ack["score"].as_i64(), ack["submitted"].as_i64(), ack["changed"].as_bool(), ack["rank"].as_u64()), (Some(500), Some(500), Some(true), Some(1)));
    assert!(ack.get("period_start").is_none());
    let ack = fx.ok_submit(&a, "highscore", 300).await;
    assert_eq!((ack["score"].as_i64(), ack["submitted"].as_i64(), ack["changed"].as_bool()), (Some(500), Some(300), Some(false)));
    fx.clock.advance(1000);
    fx.ok_submit(&b, "highscore", 900).await;
    fx.clock.advance(1000);
    // Cy ties Ada later: Ada (earlier) ranks first.
    assert_eq!(fx.ok_submit(&c, "highscore", 500).await["rank"], 3);
    let top = fx.get("/v1/leaderboards/highscore", &a).await;
    assert_eq!(users_of(&top), [bo.get(), ada.get(), cy.get()]);
    assert_eq!(ranks_of(&top), [1, 2, 3]);
    assert_eq!((top["items"][0]["name"].as_str(), top["items"][1]["metadata"].clone()), (Some("Bo"), json!({"car": "red"})));
    assert_eq!(top["items"][1]["achieved_at"], T0, "the time the score was reached");
    // The hook: refusals and rewrites; the after hook sees the result.
    let (status, body) = fx.submit(&a, "highscore", json!({"score": 2_000_000})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::FORBIDDEN)));
    let ack = fx.ok_submit(&c, "highscore", 777).await;
    assert_eq!((ack["submitted"].as_i64(), ack["score"].as_i64(), ack["rank"].as_u64()), (Some(700), Some(700), Some(2)));
    assert!(fx.after.lock().expect("lock").iter().any(|e| *e == ("highscore".to_string(), 700, true, 2)));
    // asc: lower is better.
    fx.ok_submit(&a, "race", 61_000).await;
    fx.ok_submit(&b, "race", 59_500).await;
    assert!(!fx.ok_submit(&b, "race", 60_000).await["changed"].as_bool().unwrap_or(true), "a slower time is not better");
    assert_eq!(users_of(&fx.get("/v1/leaderboards/race", &a).await), [bo.get(), ada.get()]);
    // latest: replaced every time; metadata goes with the newest submission.
    fx.ok_submit(&a, "latest", 50).await;
    let ack = fx.ok_submit(&a, "latest", 10).await;
    assert_eq!((ack["score"].as_i64(), ack["changed"].as_bool()), (Some(10), Some(true)));
    // sum: adds up, saturating.
    fx.ok_submit(&a, "kills", 3).await;
    fx.ok_submit(&a, "kills", 4).await;
    assert_eq!(fx.ok_submit(&a, "kills", -2).await["score"], 5);
    fx.ok_submit(&b, "kills", i64::MAX).await;
    assert_eq!(fx.ok_submit(&b, "kills", 10).await["score"], i64::MAX);
    let ack = fx.ok_submit(&c, "kills", -i64::MAX).await;
    assert_eq!(fx.ok_submit(&c, "kills", -5).await["score"], -i64::MAX, "{ack}");
    assert_eq!(users_of(&fx.get("/v1/leaderboards/kills", &a).await), [bo.get(), ada.get(), cy.get()]);
    // My rank.
    let me = fx.get("/v1/leaderboards/kills/me", &a).await;
    assert_eq!(
        (me["entry"]["rank"].as_u64(), me["entry"]["score"].as_i64(), me["entry"]["name"].as_str(), me["total"].as_u64()),
        (Some(2), Some(5), Some("Ada"), Some(3))
    );
}

async fn pages_and_around(url: &str) {
    let fx = fixture(url, |_| {}).await;
    let players = fx.players("p", 9).await;
    // Scores 10, 20, …, 90: player i has (i+1)*10; the best is p8.
    for (i, (_, token)) in players.iter().enumerate() {
        fx.ok_submit(token, "highscore", (i as i64 + 1) * 10).await;
    }
    let token = &players[0].1;
    let first = fx.get("/v1/leaderboards/highscore?limit=4", token).await;
    assert_eq!(ranks_of(&first), [1, 2, 3, 4]);
    assert_eq!(users_of(&first)[0], players[8].0.get());
    let cursor = first["next_cursor"].as_str().expect("more pages").to_string();
    let second = fx.get(&format!("/v1/leaderboards/highscore?limit=4&cursor={cursor}"), token).await;
    assert_eq!(ranks_of(&second), [5, 6, 7, 8]);
    let cursor = second["next_cursor"].as_str().expect("one more").to_string();
    let last = fx.get(&format!("/v1/leaderboards/highscore?limit=4&cursor={cursor}"), token).await;
    assert_eq!((ranks_of(&last), last.get("next_cursor")), (vec![9], None));
    assert_eq!(users_of(&last), [players[0].0.get()]);
    // Around p4 (rank 5): two above, one below.
    let around = fx.get("/v1/leaderboards/highscore/around?above=2&below=1", &players[4].1).await;
    assert_eq!(ranks_of(&around), [3, 4, 5, 6]);
    assert_eq!(users_of(&around), [players[6].0.get(), players[5].0.get(), players[4].0.get(), players[3].0.get()]);
    // At the top and the bottom the window is cut; the defaults are 5 and 5.
    assert_eq!(ranks_of(&fx.get("/v1/leaderboards/highscore/around?above=3&below=0", &players[8].1).await), [1]);
    assert_eq!(ranks_of(&fx.get("/v1/leaderboards/highscore/around", &players[0].1).await), [4, 5, 6, 7, 8, 9]);
    assert_eq!(ranks_of(&fx.get("/v1/leaderboards/highscore/around?above=500&below=500", &players[4].1).await).len(), 9, "clamped to 50");
    // The service reads any player's rank.
    let rank = fx.service().rank(fx.state(), players[2].0, "highscore", None).await.expect("rank");
    assert_eq!(rank.entry.map(|e| e.rank), Some(7));
}

async fn periods_and_purge(url: &str) {
    let fx = fixture(url, |lb| lb.keep_periods = 2).await;
    let (ada, a) = fx.register("ada@example.com", "Ada").await;
    let today = Period::Daily.start_of(UnixMillis(T0)).expect("start").get();
    let ack = fx.ok_submit(&a, "daily", 40).await;
    assert_eq!(ack["period_start"], today);
    fx.ok_submit(&a, "weekly", 5).await;
    // The next day: a new period, the old one still readable with `at`.
    fx.clock.set(UnixMillis(today + DAY + 10));
    let ack = fx.ok_submit(&a, "daily", 7).await;
    assert_eq!((ack["period_start"].as_i64(), ack["score"].as_i64(), ack["rank"].as_u64()), (Some(today + DAY), Some(7), Some(1)));
    let now = fx.get("/v1/leaderboards/daily", &a).await;
    assert_eq!((now["period_start"].as_i64(), now["period_end"].as_i64()), (Some(today + DAY), Some(today + 2 * DAY)));
    assert_eq!(now["items"][0]["score"], 7);
    let yesterday = fx.get(&format!("/v1/leaderboards/daily?at={}", today + 5), &a).await;
    assert_eq!((yesterday["period_start"].as_i64(), yesterday["items"][0]["score"].as_i64()), (Some(today), Some(40)));
    let me = fx.get(&format!("/v1/leaderboards/daily/me?at={}", today), &a).await;
    assert_eq!(me["entry"]["score"], 40);
    // A week sums within its period only.
    let week = Period::Weekly.start_of(UnixMillis(T0)).expect("week").get();
    fx.clock.set(UnixMillis(week + 7 * DAY + 1));
    assert_eq!(fx.ok_submit(&a, "weekly", 3).await["score"], 3, "a new week starts from zero");
    // The purge keeps the current period and `keep_periods` (2) finished ones.
    fx.clock.set(UnixMillis(today + 3 * DAY + 1));
    fx.ok_submit(&a, "daily", 1).await;
    let deleted = fx.service().purge(fx.state()).await.expect("purge");
    assert_eq!(deleted, 1, "only the day three days ago goes");
    let gone = fx.get(&format!("/v1/leaderboards/daily?at={today}"), &a).await;
    assert_eq!(gone["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(fx.get(&format!("/v1/leaderboards/daily?at={}", today + DAY), &a).await["items"][0]["score"], 7);
    // Server code removes a score.
    assert!(fx.service().remove(fx.state(), ada, "daily", None).await.expect("remove"));
    assert!(!fx.service().remove(fx.state(), ada, "daily", None).await.expect("remove"));
}

async fn server_boards_rate_and_accounts(url: &str) {
    let fx = fixture(url, |lb| {
        lb.submit_rate = 3;
        lb.submit_rate_window_secs = 3600;
    })
    .await;
    let (ada, a) = fx.register("ada@example.com", "Ada").await;
    // A server-only board: 403 for players, fine for server code (hooks see the submitter).
    let (status, body) = fx.submit(&a, "ranked", json!({"score": 1})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some(codes::FORBIDDEN)));
    let ack = fx.service().submit(fx.state(), ada, "ranked", SubmitScore::new(1500)).await.expect("server submit");
    assert_eq!((ack.score, ack.rank), (1500, 1));
    assert!(fx.service().submit(fx.state(), UserId(999_999), "ranked", SubmitScore::new(1)).await.is_err(), "no such account");
    // The rate: 3 per hour (the refused one above counted too).
    fx.ok_submit(&a, "highscore", 1).await;
    fx.ok_submit(&a, "highscore", 2).await;
    let (status, body) = fx.submit(&a, "highscore", json!({"score": 3})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    assert!(body["error"]["details"]["retry_after_ms"].as_u64().is_some());
    fx.service().submit(fx.state(), ada, "highscore", SubmitScore::new(5)).await.expect("server code is not limited");
    // Deleting the account deletes its scores.
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(ada.get()));
    fx.state().db().execute(&delete).await.expect("delete account");
    let (_, b) = fx.register("bo@example.com", "Bo").await;
    assert_eq!(fx.get("/v1/leaderboards/highscore/me", &b).await["total"], 0);
}

async fn concurrent_submissions(url: &str) {
    let fx = Arc::new(fixture(url, |_| {}).await);
    let (ada, a) = fx.register("ada@example.com", "Ada").await;
    // One player's sum: every submission counts.
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (fx, a) = (fx.clone(), a.clone());
        tasks.push(tokio::spawn(async move { fx.submit(&a, "kills", json!({"score": 2})).await.0 }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("task"), StatusCode::OK);
    }
    let me = fx.service().rank(fx.state(), ada, "kills", None).await.expect("rank");
    assert_eq!(me.entry.map(|e| e.score), Some(32));
    // Many players' first scores at once.
    let players = fx.players("q", 10).await;
    let mut tasks = Vec::new();
    for (i, (_, token)) in players.into_iter().enumerate() {
        let fx = fx.clone();
        tasks.push(tokio::spawn(async move { fx.submit(&token, "highscore", json!({"score": i as i64})).await.0 }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("task"), StatusCode::OK);
    }
    let top = fx.get("/v1/leaderboards/highscore?limit=100", &a).await;
    assert_eq!(ranks_of(&top), (1..=10).collect::<Vec<u64>>());
}

/// A fresh database for one part of the suite: every part counts the rows of its boards, so none
/// may see another's. `backend`: `memory`, `file`, or a MySQL / PostgreSQL base URL.
async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("leaderboards-file");
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

async fn suite(backend: &str) {
    each!(backend, boards_and_errors, modes_orders_and_ties, pages_and_around, periods_and_purge, server_boards_rate_and_accounts, concurrent_submissions);
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
async fn mysql_leaderboards_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_leaderboards_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}
/// The module's wiring: needs `auth` first, refuses settings given twice or unknown keys, reads
/// `[modules.leaderboards]`, lists its routes in the OpenAPI document and its name in `/v1/info`.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_and_documents() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Leaderboards::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let text = "[database]\nurl = \"sqlite::memory:\"\n[modules.leaderboards]\nsubmit_rate = 5\n[[modules.leaderboards.boards]]\nkey = \"race\"\norder = \"asc\"\nperiod = \"weekly\"\n";
    let file = Config::from_toml_str(text).expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Leaderboards::new()).build().await.expect("from the file");
    let service = ok.state().get::<LeaderboardService>().expect("service");
    assert_eq!((service.config().submit_rate, service.config().board("race").map(|b| b.period)), (5, Some(Period::Weekly)));
    let error = NetBackendServer::new(file).module(Auth::new()).module(Leaderboards::new().with_config(LeaderboardsConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad =
        Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[[modules.leaderboards.boards]]\nkey = \"race\"\nreset = \"weekly\"\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Leaderboards::new()).build().await.is_err(), "unknown keys are refused");

    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Leaderboards::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/leaderboards")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    assert!(spec["components"]["schemas"]["LeaderboardPage"].is_object());
    let (_, _, info) = common::call(&prepared.router(), common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "leaderboards"]));
    let _ = net_backend_server::leaderboards::events::Submitter::Player;
}
