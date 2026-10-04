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
use net_backend_protocol::chat::{
    CreateRoom, DeleteMessage, DeleteRoom, EditMessage, EditRoom, GetRoom, InviteToRoom, JoinChatRoom, KickFromRoom, LeaveChatRoom, ListDirects, ListMessages,
    ListReceipts, ListRoomMembers, ListRooms, MarkRead, MyRooms, OpenDirect, PublicRooms, RoomRole, RoomUser, RoomVisibility, SetRoomRole, TransferRoom,
    UnreadQuery, UpdateRoom,
};
use net_backend_protocol::files::{DeleteFile, EditFile, FileQuery, FileVisibility, GetFile, GetFileUsage, ListFiles, UpdateFile};
use net_backend_protocol::friends::{
    AcceptFriend, AddFriend, BlockUser, CancelFriendRequest, DeclineFriend, FriendsHeartbeat, GetFriendCode, GetFriendSettings, ListBlocks, ListFriendRequests,
    ListFriends, RemoveFriend, RequestQuery, ResetFriendCode, SteamMatch, UnblockUser, UpdateFriendSettings,
};
use net_backend_protocol::groups::{
    AcceptGroupInvite, CreateGroup, DeclineGroupInvite, DeleteGroup, EditGroup, GetGroup, GroupQuery, GroupRole, InviteToGroup, Invitee, JoinGroup, KickMember,
    LeaveGroup, ListGroupInvites, ListGroupMembers, ListGroups, MyGroups, RevokeGroupInvite, SetMemberRole, TransferGroup, UpdateGroup,
};
use net_backend_protocol::http_call::placeholders;
use net_backend_protocol::leaderboards::{AroundQuery, GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore, RankQuery, SubmitScore, TopQuery};
use net_backend_protocol::lobbies::{
    CreateLobby, EditLobby, GetLobby, JoinLobby, JoinLobbyByCode, KickFromLobby, LeaveLobby, LobbyPlayer, LobbySearch, LobbyState, LobbyVisibility, MyLobbies,
    NewLobbyCode, SetLobbyReady, SetReady, TransferLobby, UpdateLobby,
};
use net_backend_protocol::matchmaking::{CancelTicket, CreateTicket, GetTicket, ListQueues};
use net_backend_protocol::notifications::{CountNotifications, DeleteNotification, MarkNotifications, NotificationQuery};
use net_backend_protocol::oauth::{OAuthLogin, OAuthToken};
use net_backend_protocol::routes::{self, HttpMethod, Route};
use net_backend_protocol::storage::{
    BatchGet, BatchPut, GetObject, GetPlayerObject, ListObjects, ListPlayerObjects, ObjectRef, ObjectVersion, PutObject, RemoveObject, WriteAccess, WriteObject,
};
use net_backend_protocol::version::GetServerInfo;
use net_backend_protocol::{
    codes, Cursor, FileId, GroupId, HttpCall, LobbyId, MessageId, NotificationId, PageRequest, PayloadKind, RoomId, UnixMillis, UserId,
};
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
        facts(OAuthLogin::new("google", OAuthToken::new("aGVhZGVy.cGF5bG9hZA.c2ln").with_nonce("n-1"))),
        facts(GetPlayerObject::new(user, "profile", "public")),
        facts(ListPlayerObjects::new(user, "levels").with_page(PageRequest::first().with_limit(5))),
        facts(ListFiles::new().with_query(FileQuery::of(user).with_limit(10))),
        facts(GetFileUsage::new()),
        facts(GetFile::new(FileId(12))),
        facts(EditFile::new(FileId(12), UpdateFile::new().with_visibility(FileVisibility::Public).with_metadata(serde_json::Value::Null))),
        facts(DeleteFile::new(FileId(12))),
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
        facts(EditMessage::new(RoomId(12), MessageId(981), "hello again")),
        facts(MarkRead::new(RoomId(12), MessageId(981))),
        facts(ListReceipts::new(RoomId(12))),
        facts(UnreadQuery::new(vec![RoomId(12), RoomId(13)])),
        facts(CreateRoom::new("Night Owls").public()),
        facts(MyRooms::new()),
        facts(PublicRooms::new().with_page(PageRequest::first().with_limit(10))),
        facts(GetRoom::new(RoomId(12))),
        facts(EditRoom::new(RoomId(12), UpdateRoom::new().with_name("Early Birds").with_visibility(RoomVisibility::Private))),
        facts(DeleteRoom::new(RoomId(12))),
        facts(JoinChatRoom::new(RoomId(12))),
        facts(LeaveChatRoom::new(RoomId(12))),
        facts(ListRoomMembers::new(RoomId(12)).with_page(PageRequest::first().with_limit(20))),
        facts(InviteToRoom::new(RoomId(12), RoomUser::new(UserId(7)))),
        facts(KickFromRoom::new(RoomId(12), UserId(7))),
        facts(SetRoomRole::new(RoomId(12), UserId(7), RoomRole::Moderator)),
        facts(TransferRoom::new(RoomId(12), RoomUser::new(UserId(7)))),
        facts(ListBoards::new()),
        facts(GetLeaderboard::new("highscore").with_query(TopQuery::new().after(Cursor::new("-5.9.1")).with_limit(10).at(UnixMillis(7)))),
        facts(PostScore::new("highscore", SubmitScore::new(1200).with_metadata(json!({"car": "red"})))),
        facts(GetMyRank::new("highscore").with_query(RankQuery::new().at(UnixMillis(7)))),
        facts(GetAroundMe::new("highscore").with_query(AroundQuery::new().with_counts(2, 3))),
        facts(NotificationQuery::new().after(Cursor::new("90")).with_limit(20).unread_only()),
        facts(CountNotifications::new()),
        facts(MarkNotifications::read(vec![NotificationId(31), NotificationId(32)])),
        facts(DeleteNotification::new(NotificationId(31))),
        facts(ListFriends::new().with_page(PageRequest::first().with_limit(20))),
        facts(RemoveFriend::new(UserId(7))),
        facts(ListFriendRequests::sent().with_query(RequestQuery::sent().after(Cursor::new("12")).with_limit(5))),
        facts(AddFriend::by_code("K7M2Q9XD")),
        facts(CancelFriendRequest::new(UserId(7))),
        facts(AcceptFriend::new(UserId(7))),
        facts(DeclineFriend::new(UserId(7))),
        facts(ListBlocks::new()),
        facts(BlockUser::new(UserId(7))),
        facts(UnblockUser::new(UserId(7))),
        facts(GetFriendCode::new()),
        facts(ResetFriendCode::new()),
        facts(FriendsHeartbeat::new()),
        facts(SteamMatch::new([76_561_201_960_265_729, 76_561_201_960_265_730])),
        facts(GetFriendSettings::new()),
        facts(UpdateFriendSettings::new().steam_findable(false)),
        facts(ListGroups::new().with_query(GroupQuery::starting_with("night").with_limit(10))),
        facts(CreateGroup::new("Night Owls").open()),
        facts(MyGroups::new()),
        facts(ListGroupInvites::new()),
        facts(GetGroup::new(GroupId(5))),
        facts(EditGroup::new(GroupId(5), UpdateGroup::new().with_open(true))),
        facts(DeleteGroup::new(GroupId(5))),
        facts(ListGroupMembers::new(GroupId(5)).with_page(PageRequest::first().with_limit(20))),
        facts(JoinGroup::new(GroupId(5))),
        facts(LeaveGroup::new(GroupId(5))),
        facts(InviteToGroup::new(GroupId(5), Invitee::new(UserId(7)))),
        facts(AcceptGroupInvite::new(GroupId(5))),
        facts(DeclineGroupInvite::new(GroupId(5))),
        facts(RevokeGroupInvite::new(GroupId(5), UserId(7))),
        facts(KickMember::new(GroupId(5), UserId(7))),
        facts(SetMemberRole::new(GroupId(5), UserId(7), GroupRole::Admin)),
        facts(TransferGroup::new(GroupId(5), Invitee::new(UserId(7)))),
        facts(CreateLobby::new(4).with_visibility(LobbyVisibility::Private).with_meta("mode", "ranked")),
        facts(MyLobbies::new()),
        facts(LobbySearch::new().with_filter("mode", "ranked").with_limit(10)),
        facts(JoinLobbyByCode::new("K7M2-Q9XD")),
        facts(GetLobby::new(LobbyId(7))),
        facts(EditLobby::new(LobbyId(7), UpdateLobby::new().with_state(LobbyState::InGame).remove_meta("map"))),
        facts(JoinLobby::new(LobbyId(7))),
        facts(LeaveLobby::new(LobbyId(7))),
        facts(SetLobbyReady::new(LobbyId(7), SetReady::new(true))),
        facts(NewLobbyCode::new(LobbyId(7))),
        facts(TransferLobby::new(LobbyId(7), LobbyPlayer::new(UserId(9)))),
        facts(KickFromLobby::new(LobbyId(7), UserId(9))),
        facts(ListQueues::new()),
        facts(CreateTicket::new("duel").with_attributes(json!({"rating": 1520}))),
        facts(GetTicket::new()),
        facts(CancelTicket::new()),
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
    let edit = find(HttpMethod::Patch, routes::chat::MESSAGE);
    assert_eq!((edit.payload, &edit.body, edit.path.as_deref()), (PayloadKind::Json, &json!({"text": "hello again"}), Some("/v1/chat/rooms/12/messages/981")));
    let read = find(HttpMethod::Put, routes::chat::READ);
    assert_eq!((read.payload, &read.body, read.path.as_deref()), (PayloadKind::Json, &json!({"message": 981}), Some("/v1/chat/rooms/12/read")));
    assert_eq!(find(HttpMethod::Post, routes::chat::UNREAD).body, json!({"rooms": [12, 13]}));
    assert_eq!(find(HttpMethod::Post, routes::chat::ROOMS).body, json!({"name": "Night Owls", "visibility": "public"}));
    assert_eq!(find(HttpMethod::Patch, routes::chat::ROOM).body, json!({"name": "Early Birds", "visibility": "private"}));
    let role = find(HttpMethod::Put, routes::chat::ROOM_ROLE);
    assert_eq!((role.path.as_deref(), &role.body), (Some("/v1/chat/rooms/12/members/7/role"), &json!({"role": "moderator"})));
    assert_eq!(find(HttpMethod::Delete, routes::chat::ROOM_MEMBER).path.as_deref(), Some("/v1/chat/rooms/12/members/7"));
    assert_eq!(find(HttpMethod::Get, routes::chat::ROOMS_MINE).path.as_deref(), Some("/v1/chat/rooms/mine"));
    assert_eq!(find(HttpMethod::Put, routes::admin::ROLE).path.as_deref(), Some("/v1/admin/users/42/roles/moderator"));
    assert_eq!(find(HttpMethod::Put, routes::admin::USER_OBJECT).path.as_deref(), Some("/v1/admin/users/42/storage/saves/slot-1"));
    assert_eq!(find(HttpMethod::Get, routes::INFO).path.as_deref(), Some("/v1/info"));
    assert_eq!(find(HttpMethod::Get, routes::storage::PLAYER_OBJECT).path.as_deref(), Some("/v1/users/42/storage/profile/public"));
    let player_list = find(HttpMethod::Get, routes::storage::PLAYER_COLLECTION);
    assert_eq!((player_list.path.as_deref(), &player_list.body), (Some("/v1/users/42/storage/levels"), &json!({"limit": 5})));
    let files = find(HttpMethod::Get, routes::files::LIST);
    assert_eq!((files.payload, &files.body), (PayloadKind::Query, &json!({"owner": 42, "limit": 10})));
    let edit = find(HttpMethod::Patch, routes::files::ONE);
    assert_eq!((edit.payload, &edit.body, edit.path.as_deref()), (PayloadKind::Json, &json!({"visibility": "public", "metadata": null}), Some("/v1/files/12")));
    let oauth = find(HttpMethod::Post, routes::auth::OAUTH);
    assert_eq!(
        (oauth.payload, &oauth.body, oauth.path.as_deref()),
        (PayloadKind::Json, &json!({"id_token": "aGVhZGVy.cGF5bG9hZA.c2ln", "nonce": "n-1"}), Some("/v1/auth/oauth/google"))
    );
    assert_eq!(find(HttpMethod::Post, routes::leaderboards::SCORES).path.as_deref(), Some("/v1/leaderboards/highscore/scores"));
    let top = find(HttpMethod::Get, routes::leaderboards::BOARD);
    assert_eq!((top.payload, &top.body), (PayloadKind::Query, &json!({"cursor": "-5.9.1", "limit": 10, "at": 7})));
    let around = find(HttpMethod::Get, routes::leaderboards::AROUND);
    assert_eq!((around.payload, &around.body), (PayloadKind::Query, &json!({"above": 2, "below": 3})));
    assert_eq!(find(HttpMethod::Delete, routes::notifications::ONE).path.as_deref(), Some("/v1/notifications/31"));
    let list = find(HttpMethod::Get, routes::notifications::LIST);
    assert_eq!((list.payload, &list.body), (PayloadKind::Query, &json!({"cursor": "90", "limit": 20, "unread_only": true})));
    let mark = find(HttpMethod::Post, routes::notifications::MARK);
    assert_eq!((mark.payload, &mark.body), (PayloadKind::Json, &json!({"ids": [31, 32], "read": true})));
    let count = find(HttpMethod::Get, routes::notifications::COUNT);
    assert_eq!((count.payload, &count.body, count.path.as_deref()), (PayloadKind::Query, &json!({}), Some("/v1/notifications/count")));
    assert_eq!(find(HttpMethod::Post, routes::friends::ACCEPT).path.as_deref(), Some("/v1/friends/requests/7/accept"));
    assert_eq!(find(HttpMethod::Put, routes::friends::BLOCK).path.as_deref(), Some("/v1/friends/blocks/7"));
    let requests = find(HttpMethod::Get, routes::friends::REQUESTS);
    assert_eq!((requests.payload, &requests.body), (PayloadKind::Query, &json!({"direction": "sent", "cursor": "12", "limit": 5})));
    assert_eq!(find(HttpMethod::Put, routes::groups::ROLE).path.as_deref(), Some("/v1/groups/5/members/7/role"));
    assert_eq!(find(HttpMethod::Delete, routes::groups::INVITE).path.as_deref(), Some("/v1/groups/5/invites/7"));
    let role = find(HttpMethod::Put, routes::groups::ROLE);
    assert_eq!((role.payload, &role.body), (PayloadKind::Json, &json!({"role": "admin"})));
    let search = find(HttpMethod::Get, routes::groups::LIST);
    assert_eq!((search.payload, &search.body), (PayloadKind::Query, &json!({"query": "night", "limit": 10})));
    assert_eq!(find(HttpMethod::Delete, routes::lobbies::MEMBER).path.as_deref(), Some("/v1/lobbies/7/members/9"));
    let edit = find(HttpMethod::Patch, routes::lobbies::ONE);
    assert_eq!((edit.payload, &edit.body), (PayloadKind::Json, &json!({"state": "in_game", "metadata": {"map": null}})));
    let lobby_search = find(HttpMethod::Post, routes::lobbies::SEARCH);
    assert_eq!(lobby_search.body, json!({"filters": [{"key": "mode", "value": "ranked"}], "limit": 10}));
    let ticket = find(HttpMethod::Post, routes::matchmaking::TICKET);
    assert_eq!((ticket.payload, &ticket.body), (PayloadKind::Json, &json!({"queue": "duel", "attributes": {"rating": 1520}})));
    assert_eq!(find(HttpMethod::Delete, routes::matchmaking::TICKET).payload, PayloadKind::Empty);
    let steam = find(HttpMethod::Post, routes::friends::STEAM);
    assert_eq!((steam.payload, &steam.body), (PayloadKind::Json, &json!({"steam_ids": ["76561201960265729", "76561201960265730"]})));
    let settings = find(HttpMethod::Put, routes::friends::SETTINGS);
    assert_eq!((settings.payload, &settings.body), (PayloadKind::Json, &json!({"steam_findable": false})));
    assert_eq!(find(HttpMethod::Get, routes::friends::SETTINGS).payload, PayloadKind::Empty);
    let add = find(HttpMethod::Post, routes::friends::REQUESTS);
    assert_eq!((add.payload, &add.body), (PayloadKind::Json, &json!({"code": "K7M2Q9XD"})));
    let submit = find(HttpMethod::Post, routes::leaderboards::SCORES);
    assert_eq!((submit.payload, &submit.body), (PayloadKind::Json, &json!({"score": 1200, "metadata": {"car": "red"}})));
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
    let bad_id = PathParams::new().with("id", "abc");
    assert_eq!(DeleteNotification::from_parts(&bad_id, NoPayload::new()).err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
    let bad_board = PathParams::new().with("board", "High Score");
    assert_eq!(GetMyRank::from_parts(&bad_board, RankQuery::new()).err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
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
