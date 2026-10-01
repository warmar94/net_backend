//! Golden tests of the typed HTTP calls: every `HttpCall` names exactly one route of
//! `routes::ALL` (and every route has one), its path fills from its parameters and comes back
//! through `from_parts`, and the payload is the documented JSON / query.

use std::collections::BTreeSet;

use net_backend_protocol::admin::{
    AdminPutObject, AuditQuery, BanRequest, BanUser, GetUser, GetUserObject, GrantRole, ListUserObjects, RemoveUserObject, RevokeRole, RevokeSessions,
    UnbanUser, UnlinkUserIdentity, UserListQuery, WriteUserObject,
};
use net_backend_protocol::auth::{
    ChangePasswordRequest, ForgotPasswordRequest, GetAccount, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, ResendVerification,
    ResetPasswordRequest, SteamLoginRequest, UnlinkIdentity, UpdateAccountRequest, VerifyEmailRequest,
};
use net_backend_protocol::chat::{DeleteMessage, ListDirects, ListMessages, ListRooms, OpenDirect};
use net_backend_protocol::http_call::placeholders;
use net_backend_protocol::routes::{self, HttpMethod, Route};
use net_backend_protocol::storage::{BatchGet, BatchPut, GetObject, ListObjects, ObjectRef, ObjectVersion, PutObject, RemoveObject, WriteAccess, WriteObject};
use net_backend_protocol::version::GetServerInfo;
use net_backend_protocol::{codes, Cursor, HttpCall, MessageId, PageRequest, PayloadKind, RoomId, UserId};
use serde_json::{json, Value};

/// One line per call type: (route, payload kind, a sample call's path, the sample's payload JSON).
struct Facts {
    route: Route,
    payload: PayloadKind,
    path: Option<String>,
    body: Value,
}

/// The sample's facts, after checking that the path parameters are exactly the template's
/// placeholders and that `from_parts(path_params, payload as JSON)` gives the same call back (same
/// path, same payload JSON: what a server rebuilds from the wire).
fn facts<C: HttpCall>(call: C) -> Facts {
    let params = call.path_params();
    let names: BTreeSet<&str> = params.iter().map(|(n, _)| n).collect();
    assert_eq!(names, placeholders(C::ROUTE.path).into_iter().collect::<BTreeSet<_>>(), "{}", C::ROUTE.path);
    let body = serde_json::to_value(call.payload()).unwrap_or(Value::Null);
    let payload: C::Payload = serde_json::from_value(body.clone()).unwrap_or_else(|e| panic!("{}: {e}", C::ROUTE.path));
    let back = C::from_parts(&params, payload).unwrap_or_else(|e| panic!("{}: {e:?}", C::ROUTE.path));
    assert_eq!(back.path(), call.path());
    assert_eq!(serde_json::to_value(back.payload()).ok(), Some(body.clone()));
    Facts { route: C::ROUTE, payload: C::PAYLOAD, path: call.path(), body }
}

fn every_call() -> Vec<Facts> {
    let user = UserId(42);
    vec![
        facts(GetServerInfo::new()),
        facts(RegisterRequest::new("a@example.com", "correct horse battery")),
        facts(LoginRequest::new("a@example.com", "correct horse battery")),
        facts(SteamLoginRequest::new("0a0b", "my-game")),
        facts(RefreshRequest::new("nbsr_x")),
        facts(LogoutRequest::everywhere()),
        facts(VerifyEmailRequest::new("nbse_x")),
        facts(ResendVerification::new()),
        facts(ForgotPasswordRequest::new("a@example.com")),
        facts(ResetPasswordRequest::new("nbse_x", "a new long password")),
        facts(GetAccount::new()),
        facts(UpdateAccountRequest::new().with_display_name("Ada")),
        facts(ChangePasswordRequest::new("old password!", "new password!")),
        facts(UnlinkIdentity::new("steam")),
        facts(ListObjects::new("saves").with_page(PageRequest::after(Cursor::new("slot-1")).with_limit(10))),
        facts(GetObject::new("saves", "slot-1")),
        facts(WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 3})).if_version(ObjectVersion(2)))),
        facts(RemoveObject::new("saves", "slot-1").if_version(ObjectVersion(3))),
        facts(BatchGet::new(vec![ObjectRef::new("saves", "a")])),
        facts(BatchPut::new(vec![])),
        facts(ListRooms::new()),
        facts(ListMessages::new(RoomId(12)).with_page(PageRequest::first().with_limit(20))),
        facts(DeleteMessage::new(RoomId(12), MessageId(981))),
        facts(OpenDirect::new(UserId(7))),
        facts(ListDirects::new()),
        facts(UserListQuery::new().with_search("ada")),
        facts(GetUser::new(user)),
        facts(BanUser::new(user, BanRequest::new().with_reason("spam"))),
        facts(UnbanUser::new(user)),
        facts(RevokeSessions::new(user)),
        facts(UnlinkUserIdentity::new(user, "steam")),
        facts(GrantRole::new(user, "moderator")),
        facts(RevokeRole::new(user, "moderator")),
        facts(AuditQuery::new().with_user(user)),
        facts(ListUserObjects::new(user, "saves")),
        facts(GetUserObject::new(user, "saves", "slot-1")),
        facts(WriteUserObject::new(user, "saves", "slot-1", AdminPutObject::new(json!(1)).with_write(WriteAccess::Server))),
        facts(RemoveUserObject::new(user, "saves", "slot-1")),
    ]
}

#[test]
fn every_route_has_exactly_one_call() {
    let calls = every_call();
    let from_calls: Vec<(HttpMethod, &str, bool)> = calls.iter().map(|f| (f.route.method, f.route.path, f.route.auth)).collect();
    let unique: BTreeSet<(&str, &str)> = from_calls.iter().map(|(m, p, _)| (m.as_str(), *p)).collect();
    assert_eq!(unique.len(), calls.len(), "two calls name the same route");
    let table: Vec<(HttpMethod, &str, bool)> = routes::ALL.iter().map(|r| (r.method, r.path, r.auth)).collect();
    for route in &table {
        assert!(from_calls.contains(route), "no HttpCall for {} {}", route.0, route.1);
    }
    for route in &from_calls {
        assert!(table.contains(route), "{} {} (auth {}) is not in routes::ALL", route.0, route.1, route.2);
    }
}

#[test]
fn paths_and_payloads() {
    let calls = every_call();
    let find = |method: HttpMethod, path: &str| calls.iter().find(|f| f.route.method == method && f.route.path == path).unwrap_or_else(|| panic!("{path}"));
    // Paths fill from the parameters.
    assert_eq!(find(HttpMethod::Get, routes::storage::OBJECT).path.as_deref(), Some("/v1/storage/saves/slot-1"));
    assert_eq!(find(HttpMethod::Delete, routes::chat::MESSAGE).path.as_deref(), Some("/v1/chat/rooms/12/messages/981"));
    assert_eq!(find(HttpMethod::Put, routes::admin::ROLE).path.as_deref(), Some("/v1/admin/users/42/roles/moderator"));
    assert_eq!(find(HttpMethod::Put, routes::admin::USER_OBJECT).path.as_deref(), Some("/v1/admin/users/42/storage/saves/slot-1"));
    assert_eq!(find(HttpMethod::Get, routes::INFO).path.as_deref(), Some("/v1/info"));
    // Payloads: JSON bodies, query strings, nothing.
    let put = find(HttpMethod::Put, routes::storage::OBJECT);
    assert_eq!((put.payload, &put.body), (PayloadKind::Json, &json!({"value": {"level": 3}, "if_version": 2})));
    let list = find(HttpMethod::Get, routes::storage::COLLECTION);
    assert_eq!((list.payload, &list.body), (PayloadKind::Query, &json!({"cursor": "slot-1", "limit": 10})));
    let delete = find(HttpMethod::Delete, routes::storage::OBJECT);
    assert_eq!((delete.payload, &delete.body), (PayloadKind::Query, &json!({"if_version": 3})));
    let admin_put = find(HttpMethod::Put, routes::admin::USER_OBJECT);
    assert_eq!(admin_put.body, json!({"value": 1, "write": "server"}));
    for (method, path) in
        [(HttpMethod::Get, routes::account::ME), (HttpMethod::Post, routes::auth::RESEND_VERIFICATION), (HttpMethod::Delete, routes::chat::MESSAGE)]
    {
        let call = find(method, path);
        assert_eq!((call.payload, &call.body), (PayloadKind::Empty, &json!({})), "{path}");
    }
    // GET routes never carry a JSON body.
    for call in &calls {
        if call.route.method == HttpMethod::Get {
            assert_ne!(call.payload, PayloadKind::Json, "{}", call.route.path);
        }
    }
}

#[test]
fn bad_path_parameters_are_refused() {
    use net_backend_protocol::{NoPayload, PathParams};
    let bad_name = PathParams::new().with("collection", "_batch").with("key", "get");
    assert_eq!(GetObject::from_parts(&bad_name, NoPayload::new()).err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
    let bad_id = PathParams::new().with("room", "twelve").with("message", "1");
    assert_eq!(DeleteMessage::from_parts(&bad_id, NoPayload::new()).err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
    let bad_role = PathParams::new().with("user", "1").with("role", "Admin");
    assert_eq!(GrantRole::from_parts(&bad_role, NoPayload::new()).err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
    // Unsafe values never become a path.
    assert_eq!(GetObject::new("saves", "a/b").path(), None);
    assert_eq!(UnlinkIdentity::new("st eam").path(), None);
}

#[test]
fn path_safety_and_query_pairs() {
    use net_backend_protocol::http_call::{is_path_safe, query_pairs};
    for good in ["slot-1", "a.b_c~d", "user@host:1", "x!$&'()*+,;=", "42"] {
        assert!(is_path_safe(good), "{good}");
    }
    for bad in ["", ".", "..", "a/b", "a?b", "a#b", "a%20b", r"a\b", "a\"b", "a<b", "a>b", "a^b", "a`b", "a{b", "a|b", "a}b", "a b", "é"] {
        assert!(!is_path_safe(bad), "{bad}");
    }
    // `None` fields are left out (never `null`), numbers become text.
    let page = PageRequest::after(Cursor::new("k9")).with_limit(5);
    let mut pairs = query_pairs(&page).expect("pairs");
    pairs.sort();
    assert_eq!(pairs, vec![("cursor".to_string(), "k9".to_string()), ("limit".to_string(), "5".to_string())]);
    assert_eq!(query_pairs(&PageRequest::first()).expect("pairs"), Vec::<(String, String)>::new());
    assert_eq!(query_pairs(&json!({"a": true, "b": null})).expect("pairs"), vec![("a".to_string(), "true".to_string())]);
    assert!(query_pairs(&json!({"nested": {"x": 1}})).is_err());
    assert!(query_pairs(&json!([1, 2])).is_err());
}
