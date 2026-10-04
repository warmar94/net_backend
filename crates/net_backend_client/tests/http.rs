//! The async client against the real server on loopback: typed calls, the session (register,
//! login, refresh before expiry, single-flight refresh, `token_expired` → refresh → one retry,
//! rotation + reuse, logout), errors (404 / 409 / 422 / 403 / 429), limits, and the honest
//! "never sent" answers. The refresh / logout races run against a scripted loopback server that
//! forces each interleaving.

mod common;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use common::{email, Server, PASSWORD};
use net_backend_client::protocol::admin::GetUser;
use net_backend_client::protocol::auth::{GetAccount, LoginRequest, RefreshRequest, RegisterRequest, TokenPair};
use net_backend_client::protocol::chat::ListRooms;
use net_backend_client::protocol::storage::{
    BatchGet, GetObject, ListObjects, ObjectRef, ObjectVersion, PutObject, RemoveObject, VersionConflict, WriteObject,
};
use net_backend_client::protocol::{codes, UnixMillis, UserId, PROTOCOL_VERSION};
use net_backend_client::{Client, Error};
use serde_json::json;

async fn registered(server: &Server, name: &str) -> Client {
    let client = server.client();
    client.register(RegisterRequest::new(email(name), PASSWORD).with_display_name(name)).await.expect("register");
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_calls_and_api_errors() {
    let server = Server::start();
    let client = server.client();
    let info = client.info().await.expect("info");
    assert!(info.supports(PROTOCOL_VERSION) && info.has_module("storage") && info.has_module("chat"));
    assert!(matches!(client.call(&GetAccount::new()).await, Err(Error::NotLoggedIn)), "an authed call without a session is never sent");

    let mut updates = client.token_updates();
    let session = client.register(RegisterRequest::new(email("ada"), PASSWORD).with_display_name("Ada")).await.expect("register");
    assert!(matches!(updates.try_changed(), Some(Some(_))), "the new pair is reported");
    let me = client.call(&GetAccount::new()).await.expect("me");
    assert_eq!((me.id, me.display_name.as_deref()), (session.account.id, Some("Ada")));

    // Storage: create, read, a stale conditional write (409 with the protocol's details), list, batch, delete.
    let ack = client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 1})).if_version(ObjectVersion::ABSENT))).await.expect("create");
    assert_eq!(ack.version, ObjectVersion::new(1));
    client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 2})))).await.expect("overwrite");
    let save = client.call(&GetObject::new("saves", "slot-1")).await.expect("read");
    assert_eq!((save.value["level"].as_i64(), save.version), (Some(2), ObjectVersion::new(2)));
    let stale = client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({})).if_version(ObjectVersion::new(1)))).await.expect_err("stale");
    assert_eq!((stale.status(), stale.code()), (Some(409), Some(codes::VERSION_CONFLICT)));
    let details = stale.api_error().and_then(|e| e.details_as::<VersionConflict>());
    assert_eq!(details.and_then(|d| d.current_version), Some(ObjectVersion::new(2)));
    assert_eq!(stale.was_sent(), Some(true));
    let page = client.call(&ListObjects::new("saves")).await.expect("list");
    assert_eq!(page.items.len(), 1);
    let batch = client.call(&BatchGet::new(vec![ObjectRef::new("saves", "slot-1")])).await.expect("batch");
    assert_eq!(batch.objects.len(), 1);
    client.call(&RemoveObject::new("saves", "slot-1").if_version(ObjectVersion::new(2))).await.expect("delete");
    let missing = client.call(&GetObject::new("saves", "slot-1")).await.expect_err("gone");
    assert_eq!((missing.status(), missing.code()), (Some(404), Some(codes::NOT_FOUND)));

    // 422 with field messages; a path that would need escaping is never sent; 403 for an admin route.
    let invalid = client.call(&WriteObject::new("saves", "slot-2", PutObject::new(json!("x".repeat(300 * 1024))))).await.expect_err("too big a value");
    assert!(invalid.status() == Some(422) || invalid.status() == Some(413), "{invalid:?}");
    let unsafe_path = client.call(&GetObject::new("saves", "../etc")).await.expect_err("refused");
    assert!(matches!(unsafe_path, Error::InvalidRequest(_)) && unsafe_path.was_sent() == Some(false));
    let forbidden = client.call(&GetUser::new(UserId(1))).await.expect_err("not an admin");
    assert_eq!((forbidden.status(), forbidden.code()), (Some(403), Some(codes::FORBIDDEN)));
    let rooms = client.call(&ListRooms::new()).await.expect("rooms");
    assert!(rooms.items.iter().any(|room| room.key.as_deref() == Some("world")));
}

/// The leaderboard calls: the boards, a submitted score (`best` keeps the better one), the top,
/// the caller's rank and the ranks around it, an unknown board.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaderboard_calls() {
    use net_backend_client::protocol::leaderboards::{AroundQuery, GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore, SubmitScore, TopQuery};
    let server = Server::start();
    let ada = registered(&server, "lb-ada").await;
    let bo = registered(&server, "lb-bo").await;
    let boards = ada.call(&ListBoards::new()).await.expect("boards");
    assert_eq!(boards.boards.iter().map(|b| b.key.as_str()).collect::<Vec<_>>(), ["highscore"]);
    let ack = ada.call(&PostScore::new("highscore", SubmitScore::new(500).with_metadata(json!({"car": "red"})))).await.expect("submit");
    assert_eq!((ack.score, ack.changed, ack.rank), (500, true, 1));
    let ack = ada.call(&PostScore::new("highscore", SubmitScore::new(100))).await.expect("submit");
    assert_eq!((ack.score, ack.submitted, ack.changed), (500, 100, false));
    assert_eq!(bo.call(&PostScore::new("highscore", SubmitScore::new(900))).await.expect("submit").rank, 1);
    let top = ada.call(&GetLeaderboard::new("highscore").with_query(TopQuery::new().with_limit(1))).await.expect("top");
    assert_eq!((top.items.len(), top.items[0].name.as_deref(), top.next_cursor.is_some()), (1, Some("lb-bo"), true));
    let next = ada.call(&GetLeaderboard::new("highscore").with_query(TopQuery::new().after(top.next_cursor.clone().expect("cursor")))).await.expect("page 2");
    assert_eq!((next.items[0].rank, next.items[0].metadata.clone()), (2, Some(json!({"car": "red"}))));
    let me = ada.call(&GetMyRank::new("highscore")).await.expect("me");
    assert_eq!((me.entry.map(|e| e.rank), me.total), (Some(2), 2));
    let around = ada.call(&GetAroundMe::new("highscore").with_query(AroundQuery::new().with_counts(1, 1))).await.expect("around");
    assert_eq!(around.items.iter().map(|e| e.rank).collect::<Vec<_>>(), [1, 2]);
    let missing = ada.call(&GetMyRank::new("nope")).await.expect_err("no such board");
    assert_eq!((missing.status(), missing.code()), (Some(404), Some(codes::NOT_FOUND)));
    let unsafe_key = ada.call(&GetMyRank::new("a/b")).await.expect_err("never sent");
    assert_eq!(unsafe_key.was_sent(), Some(false));
}

/// The notification calls: a list (newest first, unread only), the count, marking, deleting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_calls() {
    use net_backend_client::protocol::notifications::{CountNotifications, DeleteNotification, MarkNotifications, NotificationQuery};
    use net_backend_client::protocol::NotificationId;
    let server = Server::start();
    let session = server.client();
    let me = session.register(RegisterRequest::new(email("nt-ada"), PASSWORD)).await.expect("register").account.id;
    let first = server.notify(me, "reward");
    let second = server.notify(me, "quest.done");
    let page = session.call(&NotificationQuery::new()).await.expect("list");
    assert_eq!(page.items.iter().map(|n| n.id.get()).collect::<Vec<_>>(), [second, first]);
    assert_eq!((page.items[0].kind.as_str(), page.items[0].text.as_deref(), page.items[0].read), ("quest.done", Some("hello"), false));
    let ack = session.call(&MarkNotifications::read(vec![NotificationId::new(first)])).await.expect("mark");
    assert_eq!((ack.changed, ack.unread), (1, 1));
    let unread = session.call(&NotificationQuery::new().unread_only()).await.expect("unread");
    assert_eq!(unread.items.len(), 1);
    session.call(&DeleteNotification::new(NotificationId::new(second))).await.expect("delete");
    let count = session.call(&CountNotifications::new()).await.expect("count");
    assert_eq!((count.unread, count.total), (0, 1));
    let invalid = session.call(&MarkNotifications::read(vec![])).await.expect_err("no ids");
    assert_eq!((invalid.status(), invalid.code()), (Some(422), Some(codes::VALIDATION_FAILED)));
}

/// The friends calls: the code, a request by code and by name, accepting, the list, blocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn friend_calls() {
    use net_backend_client::protocol::friends::{
        AcceptFriend, AddFriend, BlockUser, FriendState, FriendsHeartbeat, GetFriendCode, ListBlocks, ListFriendRequests, ListFriends, RemoveFriend,
    };
    let server = Server::start();
    let ada = registered(&server, "fr-ada").await;
    let bo = registered(&server, "fr-bo").await;
    let code = ada.call(&GetFriendCode::new()).await.expect("code").code;
    let sent = bo.call(&AddFriend::by_code(code.to_lowercase())).await.expect("request");
    assert_eq!((sent.state, sent.name.as_deref()), (FriendState::Sent, Some("fr-ada")));
    let received = ada.call(&ListFriendRequests::received()).await.expect("requests");
    assert_eq!(received.items.iter().map(|e| e.name.as_deref()).collect::<Vec<_>>(), [Some("fr-bo")]);
    let bo_id = received.items[0].user;
    let friend = ada.call(&AcceptFriend::new(bo_id)).await.expect("accept");
    assert_eq!((friend.state, friend.online), (FriendState::Friend, Some(false)));
    bo.call(&FriendsHeartbeat::new()).await.expect("heartbeat");
    let friends = ada.call(&ListFriends::new()).await.expect("friends");
    assert_eq!((friends.items[0].user, friends.items[0].online), (bo_id, Some(true)));
    ada.call(&RemoveFriend::new(bo_id)).await.expect("remove");
    ada.call(&BlockUser::new(bo_id)).await.expect("block");
    assert_eq!(ada.call(&ListBlocks::new()).await.expect("blocks").items[0].state, FriendState::Blocked);
    let refused = bo.call(&AddFriend::by_name("fr-ada")).await.expect_err("blocked");
    assert_eq!((refused.status(), refused.code()), (Some(403), Some(codes::FORBIDDEN)));
}

/// The Steam ID lookup and the friends settings: a Steam login, a Steam link on an email account,
/// the lookup, the findable setting, a caller without Steam.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steam_lookup_calls() {
    use common::{steam_id, steam_ticket, STEAM_IDENTITY};
    use net_backend_client::protocol::auth::SteamLoginRequest;
    use net_backend_client::protocol::friends::{FriendState, GetFriendSettings, SteamMatch, UpdateFriendSettings};
    let server = Server::start();
    let ada = server.client();
    let ada_id = ada.login_steam(SteamLoginRequest::new(steam_ticket(1), STEAM_IDENTITY)).await.expect("Steam login").account.id;
    let bo = registered(&server, "st-bo").await;
    let bo_id = bo.link_steam(SteamLoginRequest::new(steam_ticket(2), STEAM_IDENTITY)).await.expect("Steam link").account.id;
    let found = ada.call(&SteamMatch::new([steam_id(2), steam_id(3), steam_id(1)])).await.expect("lookup");
    assert_eq!(found.players.len(), 1, "{found:?}");
    assert_eq!(
        (found.players[0].steam_id.as_str(), found.players[0].user, found.players[0].name.as_deref()),
        (steam_id(2).to_string().as_str(), bo_id, Some("st-bo"))
    );
    assert_eq!(found.players[0].state, None);
    let back = bo.call(&SteamMatch::new([steam_id(1)])).await.expect("lookup back");
    assert_eq!((back.players[0].user, back.players[0].state), (ada_id, None::<FriendState>));
    assert!(bo.call(&GetFriendSettings::new()).await.expect("settings").steam_findable);
    assert!(!bo.call(&UpdateFriendSettings::new().steam_findable(false)).await.expect("hide").steam_findable);
    assert!(ada.call(&SteamMatch::new([steam_id(2)])).await.expect("lookup").players.is_empty(), "hidden");
    let cy = registered(&server, "st-cy").await;
    let refused = cy.call(&SteamMatch::new([steam_id(1)])).await.expect_err("no Steam linked");
    assert_eq!((refused.status(), refused.code()), (Some(403), Some(codes::FORBIDDEN)));
    let mut malformed = SteamMatch::default();
    malformed.steam_ids = vec!["not-a-steam-id".into()];
    let invalid = ada.call(&malformed).await.expect_err("malformed");
    assert_eq!((invalid.status(), invalid.code()), (Some(422), Some(codes::VALIDATION_FAILED)));
}

/// The groups calls: create, search, invite, accept, roles, members, the chat room, leave, delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_calls() {
    use net_backend_client::protocol::groups::{
        AcceptGroupInvite, CreateGroup, DeleteGroup, GroupQuery, GroupRole, InviteToGroup, Invitee, ListGroupInvites, ListGroupMembers, ListGroups, MyGroups,
        SetMemberRole,
    };
    let server = Server::start();
    let ada = registered(&server, "gr-ada").await;
    let bo = registered(&server, "gr-bo").await;
    let group = ada.call(&CreateGroup::new("Client Crew").with_description("from the client")).await.expect("create");
    assert_eq!((group.role, group.members, group.chat_room.is_some()), (Some(GroupRole::Owner), 1, true));
    let found = bo.call(&ListGroups::new().with_query(GroupQuery::starting_with("client"))).await.expect("search");
    assert_eq!(found.items.iter().map(|g| g.id).collect::<Vec<_>>(), [group.id]);
    let bo_id = bo.call(&net_backend_client::protocol::auth::GetAccount::new()).await.expect("me").id;
    ada.call(&InviteToGroup::new(group.id, Invitee::new(bo_id))).await.expect("invite");
    assert_eq!(bo.call(&ListGroupInvites::new()).await.expect("invites").items[0].group.id, group.id);
    let joined = bo.call(&AcceptGroupInvite::new(group.id)).await.expect("accept");
    assert_eq!((joined.role, joined.members), (Some(GroupRole::Member), 2));
    ada.call(&SetMemberRole::new(group.id, bo_id, GroupRole::Admin)).await.expect("role");
    let members = bo.call(&ListGroupMembers::new(group.id)).await.expect("members");
    assert_eq!(members.items.iter().map(|m| m.role).collect::<Vec<_>>(), [GroupRole::Owner, GroupRole::Admin]);
    assert_eq!(bo.call(&MyGroups::new()).await.expect("mine").groups[0].role, Some(GroupRole::Admin));
    let refused = bo.call(&DeleteGroup::new(group.id)).await.expect_err("not the owner");
    assert_eq!((refused.status(), refused.code()), (Some(403), Some(codes::FORBIDDEN)));
    ada.call(&DeleteGroup::new(group.id)).await.expect("delete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_room_calls() {
    use net_backend_client::protocol::chat::{
        CreateRoom, DeleteRoom, EditRoom, GetRoom, InviteToRoom, JoinChatRoom, KickFromRoom, LeaveChatRoom, ListReceipts, ListRoomMembers, MarkRead, MyRooms,
        PublicRooms, RoomKind, RoomRole, RoomUser, RoomVisibility, SetRoomRole, TransferRoom, UnreadQuery, UpdateRoom,
    };
    let server = Server::start();
    let ada = registered(&server, "cr-ada").await;
    let bo = registered(&server, "cr-bo").await;
    let bo_id = bo.call(&net_backend_client::protocol::auth::GetAccount::new()).await.expect("me").id;
    let room = ada.call(&CreateRoom::new("Client Den").public()).await.expect("create");
    assert_eq!((room.kind, room.visibility, room.role), (RoomKind::Player, Some(RoomVisibility::Public), Some(RoomRole::Owner)));
    assert!(bo.call(&PublicRooms::new()).await.expect("public").items.iter().any(|r| r.id == room.id));
    let joined = bo.call(&JoinChatRoom::new(room.id)).await.expect("join");
    assert_eq!(joined.role, Some(RoomRole::Member));
    ada.call(&SetRoomRole::new(room.id, bo_id, RoomRole::Moderator)).await.expect("role");
    let members = bo.call(&ListRoomMembers::new(room.id)).await.expect("members");
    assert_eq!(members.items.iter().map(|m| m.role).collect::<Vec<_>>(), [RoomRole::Owner, RoomRole::Moderator]);
    let renamed = bo.call(&EditRoom::new(room.id, UpdateRoom::new().with_name("Client Hall"))).await.expect("rename");
    assert_eq!(renamed.name.as_deref(), Some("Client Hall"));
    ada.call(&TransferRoom::new(room.id, RoomUser::new(bo_id))).await.expect("hand on");
    assert_eq!(ada.call(&GetRoom::new(room.id)).await.expect("get").owner, Some(bo_id));
    assert_eq!(bo.call(&MyRooms::new()).await.expect("mine").items[0].role, Some(RoomRole::Owner));
    let unread = bo.call(&UnreadQuery::new(vec![room.id])).await.expect("unread");
    assert_eq!(unread.rooms[0].unread, 0);
    assert!(bo.call(&ListReceipts::new(room.id)).await.expect("receipts").receipts.is_empty());
    let missing = bo.call(&MarkRead::new(room.id, net_backend_client::protocol::MessageId(1))).await.expect_err("no such message");
    assert_eq!(missing.status(), Some(404));
    let ada_id = ada.call(&net_backend_client::protocol::auth::GetAccount::new()).await.expect("me").id;
    bo.call(&KickFromRoom::new(room.id, ada_id)).await.expect("kick");
    let refused = ada.call(&JoinChatRoom::new(room.id)).await.expect_err("banned");
    assert_eq!((refused.status(), refused.code()), (Some(403), Some(codes::FORBIDDEN)));
    bo.call(&InviteToRoom::new(room.id, RoomUser::new(ada_id))).await.expect("invite lifts the ban");
    ada.call(&LeaveChatRoom::new(room.id)).await.expect("decline");
    bo.call(&DeleteRoom::new(room.id)).await.expect("delete");
    assert_eq!(bo.call(&GetRoom::new(room.id)).await.expect_err("gone").status(), Some(404));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lobby_and_matchmaking_calls() {
    use net_backend_client::protocol::lobbies::{
        CreateLobby, EditLobby, GetLobby, JoinLobbyByCode, KickFromLobby, LeaveLobby, LobbyCode, LobbySearch, LobbyState, LobbyVisibility, MyLobbies,
        NewLobbyCode, SetLobbyReady, SetReady, UpdateLobby,
    };
    use net_backend_client::protocol::matchmaking::{CancelTicket, CreateTicket, GetTicket, ListQueues, TicketStatus};
    let server = Server::start();
    let ada = registered(&server, "lb-ada").await;
    let bo = registered(&server, "lb-bo").await;
    let lobby = ada.call(&CreateLobby::new(4).with_visibility(LobbyVisibility::Private).with_meta("mode", "client")).await.expect("create");
    let code = lobby.code.clone().expect("the host sees the code");
    assert_eq!(lobby.code_number, Some(code.to_u64()));
    // The number form travels back to the text form.
    let joined = bo.call(&JoinLobbyByCode::from_number(code.to_u64()).expect("code")).await.expect("join");
    assert_eq!(joined.players.len(), 2);
    let bo_id = joined.players[1].user;
    bo.call(&SetLobbyReady::new(lobby.id, SetReady::new(true))).await.expect("ready");
    let change = UpdateLobby::new().with_visibility(LobbyVisibility::Public).set_meta("map", "dust");
    let changed = ada.call(&EditLobby::new(lobby.id, change)).await.expect("edit");
    assert_eq!((changed.visibility, changed.metadata.get("map").map(String::as_str)), (LobbyVisibility::Public, Some("dust")));
    let found = bo.call(&LobbySearch::new().with_filter("mode", "client").including_full()).await.expect("search");
    assert_eq!(found.items.iter().map(|l| l.id).collect::<Vec<_>>(), [lobby.id]);
    let renewed = ada.call(&NewLobbyCode::new(lobby.id)).await.expect("code");
    assert_ne!(renewed.code.as_ref().map(LobbyCode::as_str), Some(code.as_str()));
    assert!(bo.call(&GetLobby::new(lobby.id)).await.expect("get").players[1].ready);
    let refused = ada.call(&KickFromLobby::new(lobby.id, lobby.host.expect("host"))).await.expect_err("own id");
    assert_eq!(refused.code(), Some(codes::VALIDATION_FAILED));
    ada.call(&KickFromLobby::new(lobby.id, bo_id)).await.expect("kick");
    assert!(bo.call(&MyLobbies::new()).await.expect("mine").lobbies.is_empty());
    let closed = ada.call(&EditLobby::new(lobby.id, UpdateLobby::new().with_state(LobbyState::Closed))).await.expect("close");
    assert_eq!(closed.state, LobbyState::Closed);
    assert_eq!(ada.call(&LeaveLobby::new(lobby.id)).await.expect_err("gone").status(), Some(404));

    let queues = ada.call(&ListQueues::new()).await.expect("queues");
    assert_eq!(queues.queues.iter().map(|q| q.key.as_str()).collect::<Vec<_>>(), ["duel"]);
    let ticket = ada.call(&CreateTicket::new("duel")).await.expect("queue");
    assert_eq!(ticket.status, TicketStatus::Waiting);
    assert_eq!(ada.call(&GetTicket::new()).await.expect("ticket").id, ticket.id);
    ada.call(&CancelTicket::new()).await.expect("cancel");
    assert_eq!(ada.call(&GetTicket::new()).await.expect_err("gone").status(), Some(404));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_access_token_is_refreshed_before_it_expires_single_flight() {
    let server = Server::start();
    let client = server.client();
    client.login(LoginRequest::new(email("bob"), PASSWORD)).await.expect_err("no account yet");
    client.register(RegisterRequest::new(email("bob"), PASSWORD)).await.expect("register");
    let first: TokenPair = client.tokens().expect("tokens");
    // The same pair, but stored as if its access token had just expired: every caller wants a
    // refresh first.
    let mut stale = first.clone();
    stale.access_expires_at = UnixMillis(UnixMillis::now().get() - 1_000);
    client.resume(stale);
    // Twenty calls at once: ONE refresh for all of them.
    let calls: Vec<_> = (0..20)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.call(&GetAccount::new()).await })
        })
        .collect();
    for call in calls {
        call.await.expect("join").expect("me");
    }
    let second = client.tokens().expect("tokens");
    assert_ne!(second.refresh_token.expose(), first.refresh_token.expose(), "rotated");
    // The first refresh token again, inside the 30 s grace window: the server answers the SAME
    // pair it gave the client, so there was exactly one refresh.
    let plain = server.client();
    let grace = plain.call(&RefreshRequest::new(first.refresh_token.clone())).await.expect("grace answer");
    assert_eq!(grace.access_token.expose(), second.access_token.expose(), "exactly one refresh happened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_expired_is_refreshed_and_retried_once() {
    let server = Server::start();
    let client = Client::builder(&server.base).refresh_margin(Duration::ZERO).build().expect("client");
    client.register(RegisterRequest::new(email("cyd"), PASSWORD)).await.expect("register");
    let before = client.tokens().expect("tokens");
    // The server's clock passes the access token's expiry; this machine's clock does not. With a zero
    // margin the client would not refresh for ~1 h by itself, so the refresh below can ONLY come from
    // the server's 401 `token_expired` (the live test could not reach this path: proactive refresh
    // always came first there).
    server.advance(Duration::from_secs(3601));
    let mut updates = client.token_updates();
    client.call(&GetAccount::new()).await.expect("401 token_expired -> refresh -> retry");
    let after = client.tokens().expect("tokens");
    assert_ne!(after.access_token.expose(), before.access_token.expose());
    assert!(matches!(updates.try_changed(), Some(Some(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_refresh_token_ends_the_session() {
    let server = Server::start();
    let client = registered(&server, "dee").await;
    let old = client.tokens().expect("tokens");
    client.refresh().await.expect("rotate");
    // Within the grace window the old token answers the same pair (a retry is harmless) ...
    let other = server.client();
    other.resume(old.clone());
    let same = other.refresh().await.expect("grace");
    assert_eq!(same.access_token.expose(), client.tokens().expect("tokens").access_token.expose());
    // ... after it, a reuse revokes the whole session.
    server.advance(Duration::from_secs(31));
    let stale = server.client();
    stale.resume(old);
    let mut updates = stale.token_updates();
    let ended = stale.refresh().await.expect_err("reused");
    assert!(matches!(&ended, Error::SessionEnded { code, .. } if code == codes::REFRESH_TOKEN_REUSED), "{ended:?}");
    assert!(ended.needs_login() && stale.tokens().is_none());
    assert!(matches!(updates.try_changed(), Some(None)), "the end is reported");
    // The family is revoked: the first client's session is over too.
    client.forget_session();
    client.resume(same);
    let gone = client.call(&GetAccount::new()).await.expect_err("revoked");
    assert!(gone.needs_login() || gone.status() == Some(401), "{gone:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_and_logout_everywhere() {
    let server = Server::start();
    let a = registered(&server, "eve").await;
    let b = server.client();
    b.login(LoginRequest::new(email("eve"), PASSWORD)).await.expect("second device");
    let old = a.tokens().expect("tokens");
    let mut updates = a.token_updates();
    a.logout().await.expect("logout");
    assert!(a.tokens().is_none() && matches!(updates.try_changed(), Some(None)));
    assert!(matches!(a.logout().await, Err(Error::NotLoggedIn)));
    // The old tokens no longer work.
    a.resume(old);
    assert!(a.call(&GetAccount::new()).await.expect_err("revoked").needs_login());
    // Everywhere: the other device is logged out too.
    let c = server.client();
    c.login(LoginRequest::new(email("eve"), PASSWORD)).await.expect("third device");
    c.logout_everywhere().await.expect("everywhere");
    let ended = b.call(&GetAccount::new()).await.expect_err("logged out elsewhere");
    assert!(ended.needs_login(), "{ended:?}");
    assert!(b.tokens().is_none());
}

/// The generic `call` of a logout (no refresh token in the body) sends the access token, as
/// `logout` does: the route takes either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_logout_call_sends_the_access_token() {
    let server = Server::start();
    let a = registered(&server, "gil").await;
    let b = server.client();
    b.login(LoginRequest::new(email("gil"), PASSWORD)).await.expect("second device");
    a.call(&net_backend_client::protocol::auth::LogoutRequest::everywhere()).await.expect("logout everywhere through call");
    let ended = b.call(&GetAccount::new()).await.expect_err("logged out elsewhere");
    assert!(ended.needs_login(), "{ended:?}");
    // Not logged in: no token to send, the server refuses (401).
    let nobody = server.client();
    let refused = nobody.call(&net_backend_client::protocol::auth::LogoutRequest::this_session()).await.expect_err("no token");
    assert_eq!(refused.status(), Some(401), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_answer_429_with_retry_after() {
    let server = Server::start_with(|setup| setup.auth.rate_limits = true);
    let client = registered(&server, "fay").await;
    let mut limited = None;
    for _ in 0..20 {
        match client.login(LoginRequest::new(email("fay"), "wrong password!")).await {
            Err(error) if error.status() == Some(429) => {
                limited = Some(error);
                break;
            }
            Err(error) => assert_eq!(error.code(), Some(codes::INVALID_CREDENTIALS)),
            Ok(_) => panic!("a wrong password logged in"),
        }
    }
    let limited = limited.expect("a 429 after repeated failures");
    assert_eq!(limited.code(), Some(codes::RATE_LIMITED));
    assert!(limited.retry_after().is_some_and(|wait| wait > Duration::ZERO), "{limited:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_and_body_limits() {
    let server = Server::start_with(|setup| setup.storage.max_objects_per_user = 1);
    let client = Client::builder(&server.base).max_response_bytes(4096).build().expect("client");
    client.register(RegisterRequest::new(email("gus"), PASSWORD)).await.expect("register");
    client.call(&WriteObject::new("saves", "a", PutObject::new(json!({"blob": "x".repeat(20_000)})))).await.expect("one object");
    let quota = client.call(&WriteObject::new("saves", "b", PutObject::new(json!(1)))).await.expect_err("quota");
    assert_eq!((quota.status(), quota.code()), (Some(403), Some(codes::QUOTA_EXCEEDED)));
    let big = client.call(&GetObject::new("saves", "a")).await.expect_err("too large");
    assert!(matches!(big, Error::BodyTooLarge { limit: 4096, .. }) && big.was_sent() == Some(true), "{big:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_and_silent_servers() {
    // Nothing listens: never sent.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let client = Client::new(&format!("http://127.0.0.1:{port}")).expect("client");
    let error = client.info().await.expect_err("refused");
    assert!(matches!(error, Error::Network { sent: Some(false), .. }), "{error:?}");
    // A server that accepts and never answers: the one deadline ends the call ("maybe sent").
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = silent.local_addr().expect("addr");
    let keep = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let held = Arc::clone(&keep);
    tokio::spawn(async move {
        while let Ok((socket, _)) = silent.accept().await {
            held.lock().await.push(socket);
        }
    });
    let client = Client::builder(&format!("http://{addr}")).timeout(Duration::from_millis(300)).build().expect("client");
    let error = client.info().await.expect_err("timeout");
    assert!(matches!(error, Error::Timeout { sent: None, .. }), "{error:?}");
}

/// A server that accepts connections and never answers.
async fn silent_server() -> String {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = silent.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_can_have_its_own_deadline() {
    use net_backend_client::protocol::GetServerInfo;
    use tokio::time::Instant;

    // Shorter than the builder's (15 s by default): the call ends at its own deadline.
    let silent = silent_server().await;
    let client = Client::new(&silent).expect("client");
    let started = Instant::now();
    let error = client.call_with_timeout(&GetServerInfo::new(), Duration::from_millis(200)).await.expect_err("timeout");
    assert!(matches!(error, Error::Timeout { sent: None, .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    // Longer than the builder's (100 ms): the call's own deadline holds.
    let quick = Client::builder(&silent).timeout(Duration::from_millis(100)).build().expect("client");
    let started = Instant::now();
    let error = quick.call_with_timeout(&GetServerInfo::new(), Duration::from_millis(700)).await.expect_err("timeout");
    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
    assert!(started.elapsed() >= Duration::from_millis(650), "ended at the builder's 100 ms: {:?}", started.elapsed());
    let started = Instant::now();
    assert!(matches!(quick.call(&GetServerInfo::new()).await, Err(Error::Timeout { .. })));
    assert!(started.elapsed() < Duration::from_millis(600), "the builder's deadline still applies to `call`");
    // Clamped, never a panic: zero becomes 1 ms, a huge one 1 h.
    assert!(matches!(client.call_with_timeout(&GetServerInfo::new(), Duration::ZERO).await, Err(Error::Timeout { .. })));
    let server = Server::start();
    let alice = registered(&server, "deadline").await;
    let me = alice.call_with_timeout(&GetAccount::new(), Duration::MAX).await.expect("a long deadline");
    assert_eq!(me.display_name.as_deref(), Some("deadline"));
    // The deadline covers waiting for a token refresh too: an expired access token, a refresh
    // that cannot finish in 1 ms.
    let waiting = Client::new(&silent).expect("client");
    let later = UnixMillis(UnixMillis::now().get() + 3_600_000);
    let expired = UnixMillis(UnixMillis::now().get() - 1_000);
    waiting.resume(TokenPair::new(
        net_backend_client::protocol::auth::AccessToken::new("nbsa_fake_deadline"),
        expired,
        net_backend_client::protocol::auth::RefreshToken::new("nbsr_fake_deadline"),
        later,
    ));
    let error = waiting.call_with_timeout(&GetAccount::new(), Duration::from_millis(150)).await.expect_err("timeout");
    assert!(matches!(error, Error::Timeout { sent: Some(false), .. }), "{error:?}");
}

#[test]
fn urls_and_runtimes_are_checked_without_panics() {
    assert!(matches!(Client::new("http://game.example.com"), Err(Error::InvalidRequest(_))), "plain http to a public host");
    assert!(Client::builder("http://game.example.com").allow_insecure_http(true).build().is_ok());
    assert!(Client::new("https://game.example.com").is_ok() && Client::new("http://localhost:8080").is_ok());
    assert!(matches!(Client::new("game.example.com"), Err(Error::InvalidRequest(_))));
    // An async call polled outside any tokio runtime: an error at the first poll, never a panic.
    let client = Client::new("https://game.example.com").expect("client");
    let mut call = Box::pin(client.info());
    let mut context = Context::from_waker(Waker::noop());
    match call.as_mut().poll(&mut context) {
        Poll::Ready(Err(Error::InvalidRequest(why))) => assert!(why.contains("tokio runtime"), "{why}"),
        other => panic!("expected InvalidRequest, got {:?}", other.map(|r| r.map(|_| ()))),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_never_show_in_debug_output() {
    let server = Server::start();
    let client = registered(&server, "hal").await;
    let tokens = client.tokens().expect("tokens");
    let text = format!("{client:?} {:?} {tokens:?}", client.token_updates());
    assert!(!text.contains(tokens.access_token.expose()) && !text.contains(tokens.refresh_token.expose()), "{text}");
}

/// A scripted auth server on 127.0.0.1 for the refresh / logout races: each refresh or logout
/// request is handed to the test (`arrived`), which releases its answer when the interleaving
/// wants it. A refresh answers a new pair (`nbsa_new` / `nbsr_new`), a logout `{}`.
struct RaceServer {
    base: String,
    arrived: tokio::sync::mpsc::UnboundedReceiver<(String, tokio::sync::oneshot::Sender<()>)>,
}

async fn race_server() -> RaceServer {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (tell, arrived) = tokio::sync::mpsc::unbounded_channel::<(String, tokio::sync::oneshot::Sender<()>)>();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let tell = tell.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let length = text
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }
                let path = text.split(' ').nth(1).unwrap_or_default().to_string();
                let (release, released) = tokio::sync::oneshot::channel();
                if tell.send((path.clone(), release)).is_err() {
                    return;
                }
                let _ = released.await;
                let answer = if path == "/v1/auth/refresh" {
                    let now = UnixMillis::now().get();
                    serde_json::to_string(&TokenPair::new(
                        net_backend_client::protocol::AccessToken::new("nbsa_new"),
                        UnixMillis(now + 3_600_000),
                        net_backend_client::protocol::RefreshToken::new("nbsr_new"),
                        UnixMillis(now + 86_400_000),
                    ))
                    .expect("json")
                } else {
                    "{}".to_string()
                };
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    RaceServer { base, arrived }
}

impl RaceServer {
    /// The next request that arrived (its path) and the handle that releases its answer.
    async fn next(&mut self) -> (String, tokio::sync::oneshot::Sender<()>) {
        tokio::time::timeout(common::WAIT, self.arrived.recv()).await.expect("a request in time").expect("the server runs")
    }
}

fn race_pair() -> TokenPair {
    let now = UnixMillis::now().get();
    TokenPair::new(
        net_backend_client::protocol::AccessToken::new("nbsa_old"),
        UnixMillis(now + 3_600_000),
        net_backend_client::protocol::RefreshToken::new("nbsr_old"),
        UnixMillis(now + 86_400_000),
    )
}

/// A refresh whose answer lands after a logout completed is not stored: the client stays logged
/// out and `token_updates` ends at `None` (forced order: refresh sent, logout done, refresh answered).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_answered_after_a_logout_does_not_log_back_in() {
    let mut server = race_server().await;
    let client = Client::new(&server.base).expect("client");
    client.resume(race_pair());
    let mut updates = client.token_updates();
    let refreshing = tokio::spawn({
        let client = client.clone();
        async move { client.refresh().await }
    });
    let (path, release_refresh) = server.next().await;
    assert_eq!(path, "/v1/auth/refresh");
    let logout = tokio::spawn({
        let client = client.clone();
        async move { client.logout().await }
    });
    let (path, release_logout) = server.next().await;
    assert_eq!(path, "/v1/auth/logout");
    release_logout.send(()).expect("release");
    logout.await.expect("join").expect("logged out");
    assert!(!client.is_logged_in());
    release_refresh.send(()).expect("release");
    let refreshed = refreshing.await.expect("join");
    assert!(matches!(refreshed, Err(Error::NotLoggedIn)), "{refreshed:?}");
    assert!(!client.is_logged_in(), "the revoked session's new pair was not installed");
    let mut last = None;
    while let Some(change) = updates.try_changed() {
        last = Some(change);
    }
    assert!(matches!(last, Some(None)), "the updates end at None");
}

/// A refresh that completes while the logout is on its way rotates the pair of the SAME session:
/// the logout's success drops it (forced order: logout sent, refresh done, logout answered).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logout_answered_after_a_refresh_still_logs_out() {
    let mut server = race_server().await;
    let client = Client::new(&server.base).expect("client");
    client.resume(race_pair());
    let logout = tokio::spawn({
        let client = client.clone();
        async move { client.logout().await }
    });
    let (path, release_logout) = server.next().await;
    assert_eq!(path, "/v1/auth/logout");
    let refreshing = tokio::spawn({
        let client = client.clone();
        async move { client.refresh().await }
    });
    let (path, release_refresh) = server.next().await;
    assert_eq!(path, "/v1/auth/refresh");
    release_refresh.send(()).expect("release");
    let pair = refreshing.await.expect("join").expect("refreshed");
    assert_eq!(pair.access_token.expose(), "nbsa_new");
    assert!(client.is_logged_in());
    let mut updates = client.token_updates();
    release_logout.send(()).expect("release");
    logout.await.expect("join").expect("logged out");
    assert!(!client.is_logged_in(), "the logout dropped the refreshed pair of its session");
    assert!(matches!(updates.try_changed(), Some(None)));
}
