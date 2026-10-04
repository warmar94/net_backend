//! Groups (guilds, clans): created by a player, who owns it; joined by invitation (or directly
//! when the group is open); roles inside the group (owner, admin, member); metadata; a list with a
//! name search; a group chat room when the server runs the chat module.
//!
//! | Route | Request → answer |
//! |---|---|
//! | `GET /v1/groups` | [`ListGroups`] (query [`GroupQuery`]: a name prefix) → [`Page`]`<`[`GroupInfo`]`>` (by name) |
//! | `POST /v1/groups` | [`CreateGroup`] → [`GroupInfo`] (the caller is the owner) |
//! | `GET /v1/groups/mine` | [`MyGroups`] → [`GroupList`] (with the caller's role) |
//! | `GET /v1/groups/invites` | [`ListGroupInvites`] → [`Page`]`<`[`GroupInvite`]`>` (the caller's invitations) |
//! | `GET /v1/groups/{group}` | [`GetGroup`] → [`GroupInfo`] |
//! | `PATCH /v1/groups/{group}` | [`EditGroup`] ([`UpdateGroup`]) → [`GroupInfo`] (owner, admins) |
//! | `DELETE /v1/groups/{group}` | [`DeleteGroup`] → [`Ack`] (owner) |
//! | `GET /v1/groups/{group}/members` | [`ListGroupMembers`] → [`Page`]`<`[`GroupMember`]`>` |
//! | `POST /v1/groups/{group}/join` | [`JoinGroup`] → [`GroupInfo`] (an open group, or with an invitation) |
//! | `POST /v1/groups/{group}/leave` | [`LeaveGroup`] → [`Ack`] |
//! | `POST /v1/groups/{group}/invites` | [`InviteToGroup`] ([`Invitee`]) → [`Ack`] (owner, admins) |
//! | `POST /v1/groups/{group}/invites/accept` | [`AcceptGroupInvite`] → [`GroupInfo`] |
//! | `POST /v1/groups/{group}/invites/decline` | [`DeclineGroupInvite`] → [`Ack`] |
//! | `DELETE /v1/groups/{group}/invites/{user}` | [`RevokeGroupInvite`] → [`Ack`] (owner, admins) |
//! | `DELETE /v1/groups/{group}/members/{user}` | [`KickMember`] → [`Ack`] (owner: anyone; admins: members) |
//! | `PUT /v1/groups/{group}/members/{user}/role` | [`SetMemberRole`] ([`RoleChange`]) → [`Ack`] (owner) |
//! | `POST /v1/groups/{group}/transfer` | [`TransferGroup`] ([`Invitee`]) → [`Ack`] (owner: the member becomes the owner, the old owner an admin) |
//!
//! **Roles:** one owner (every right; hands the group over with [`TransferGroup`]), admins (edit the
//! group, invite, revoke invitations, kick members) and members. The owner leaves only as the last
//! member (the group is then deleted) or after a transfer.
//!
//! **Names** ([`group_name_problem`]): 3 to [`MAX_GROUP_NAME_CHARS`] characters, unique on the
//! server without regard to case. **Invitations** reach the invited player as a notification when
//! the server runs the notifications module, and are listed by [`ListGroupInvites`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::Ack;
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{GroupId, RoomId, UserId};
use crate::page::{Cursor, Page, PageRequest};
use crate::time::UnixMillis;

/// The shortest group name, in characters.
pub const MIN_GROUP_NAME_CHARS: usize = 3;
/// The longest group name, in characters.
pub const MAX_GROUP_NAME_CHARS: usize = 32;
/// The longest group description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 500;
/// The default largest group `metadata`, in bytes of its JSON (a server may configure another).
pub const DEFAULT_MAX_METADATA_BYTES: usize = 2048;
/// The longest name prefix a [`GroupQuery`] searches for, in characters.
pub const MAX_QUERY_CHARS: usize = 32;

/// What is wrong with a group name: `None` if fine (3 to [`MAX_GROUP_NAME_CHARS`] characters after
/// trimming, no control, invisible or direction-changing characters: [`crate::text::name_problem`]).
pub fn group_name_problem(name: &str) -> Option<String> {
    let count = name.trim().chars().count();
    if !(MIN_GROUP_NAME_CHARS..=MAX_GROUP_NAME_CHARS).contains(&count) {
        return Some(format!("must be {MIN_GROUP_NAME_CHARS} to {MAX_GROUP_NAME_CHARS} characters"));
    }
    crate::text::name_problem(name).map(str::to_string)
}

/// What is wrong with a group description: `None` if fine (at most [`MAX_DESCRIPTION_CHARS`]
/// characters, the chat text rules of [`crate::text::message_problem`]; empty is fine).
pub fn description_problem(text: &str) -> Option<String> {
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Some(format!("is longer than {MAX_DESCRIPTION_CHARS} characters"));
    }
    if text.is_empty() {
        return None;
    }
    crate::text::message_problem(text).map(str::to_string)
}

/// A member's role in a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GroupRole {
    /// The one owner: every right.
    Owner,
    /// Edits the group, invites, revokes invitations, kicks members.
    Admin,
    /// A member.
    Member,
    /// A role from a newer server this version does not know (never sent by this one).
    #[serde(other)]
    Unknown,
}

/// A group as players see it.
///
/// JSON: `{"id":5,"name":"Night Owls","description":"…","open":false,"metadata":{"tag":"NO"},"owner":42,"members":12,"max_members":100,"chat_room":31,"created_at":1790000000000,"role":"admin"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupInfo {
    /// The id.
    pub id: GroupId,
    /// The name.
    pub name: String,
    /// The description, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether anyone may join without an invitation.
    #[serde(default)]
    pub open: bool,
    /// The game's data about the group (a tag, a banner, a level), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// The owner (absent when the owner's account was deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<UserId>,
    /// How many members it has.
    pub members: u32,
    /// The most members a group may have on this server.
    pub max_members: u32,
    /// The group's chat room (members only), when the server runs the chat module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_room: Option<RoomId>,
    /// When it was created.
    pub created_at: UnixMillis,
    /// The caller's role, when the caller is a member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<GroupRole>,
}

impl GroupInfo {
    /// A group without description, metadata, owner, chat room or role.
    pub fn new(id: GroupId, name: impl Into<String>, members: u32, max_members: u32, created_at: UnixMillis) -> Self {
        Self {
            id,
            name: name.into(),
            description: None,
            open: false,
            metadata: None,
            owner: None,
            members,
            max_members,
            chat_room: None,
            created_at,
            role: None,
        }
    }

    /// The same group with a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same group, open or not.
    pub fn with_open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// The same group with metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The same group with its owner.
    pub fn with_owner(mut self, owner: UserId) -> Self {
        self.owner = Some(owner);
        self
    }

    /// The same group with its chat room.
    pub fn with_chat_room(mut self, room: RoomId) -> Self {
        self.chat_room = Some(room);
        self
    }

    /// The same group with the caller's role.
    pub fn with_role(mut self, role: GroupRole) -> Self {
        self.role = Some(role);
        self
    }
}

/// The caller's groups ([`MyGroups`]).
///
/// JSON: `{"groups":[GroupInfo]}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupList {
    /// The groups, with the caller's role, oldest membership first.
    pub groups: Vec<GroupInfo>,
}

impl GroupList {
    /// A list.
    pub fn new(groups: Vec<GroupInfo>) -> Self {
        Self { groups }
    }
}

/// One member of a group.
///
/// JSON: `{"user":42,"name":"Ada","role":"owner","joined_at":1790000000000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupMember {
    /// The member.
    pub user: UserId,
    /// Their display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Their role.
    pub role: GroupRole,
    /// When they joined.
    pub joined_at: UnixMillis,
}

impl GroupMember {
    /// A member without a name.
    pub fn new(user: UserId, role: GroupRole, joined_at: UnixMillis) -> Self {
        Self { user, name: None, role, joined_at }
    }

    /// The same member with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// An invitation the caller received.
///
/// JSON: `{"group":GroupInfo,"inviter":42,"created_at":1790000000000}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupInvite {
    /// The group.
    pub group: GroupInfo,
    /// Who invited the caller (absent once that account is deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inviter: Option<UserId>,
    /// When.
    pub created_at: UnixMillis,
}

impl GroupInvite {
    /// An invitation.
    pub fn new(group: GroupInfo, inviter: Option<UserId>, created_at: UnixMillis) -> Self {
        Self { group, inviter, created_at }
    }
}

/// Create a group: `POST /v1/groups` → [`GroupInfo`]. The caller becomes its owner.
///
/// JSON: `{"name":"Night Owls","description":"We play at night","open":true,"metadata":{"tag":"NO"}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CreateGroup {
    /// The name ([`group_name_problem`]).
    pub name: String,
    /// A description ([`description_problem`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Anyone may join without an invitation. Default false.
    #[serde(default)]
    pub open: bool,
    /// The game's data about the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl CreateGroup {
    /// A closed group with this name.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), description: None, open: false, metadata: None }
    }

    /// The same request with a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same request for an open group.
    pub fn open(mut self) -> Self {
        self.open = true;
        self
    }

    /// The same request with metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The shape rules (the metadata size is the server's).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(problem) = group_name_problem(&self.name) {
            details.add("name", problem);
        }
        if let Some(problem) = self.description.as_deref().and_then(description_problem) {
            details.add("description", problem);
        }
        details.into_result()
    }
}

/// Change a group: the body of `PATCH /v1/groups/{group}` (owner, admins). Absent fields stay; an
/// empty `description` removes it; `metadata: null` removes it.
///
/// JSON: `{"description":"","open":false}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateGroup {
    /// A new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A new description (empty: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Open or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    /// New metadata (`null`: none).
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// A field that is present decodes as `Some`, also when it is `null` (absent: `None`, by
/// `#[serde(default)]`).
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl UpdateGroup {
    /// A change of nothing (add fields).
    pub fn new() -> Self {
        Self::default()
    }

    /// The same change with a new name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same change with a new description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same change, open or closed.
    pub fn with_open(mut self, open: bool) -> Self {
        self.open = Some(open);
        self
    }

    /// The same change with new metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The shape rules.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(problem) = self.name.as_deref().and_then(group_name_problem) {
            details.add("name", problem);
        }
        if let Some(problem) = self.description.as_deref().and_then(description_problem) {
            details.add("description", problem);
        }
        details.into_result()
    }
}

/// The query of `GET /v1/groups`: `?query=night&cursor=…&limit=…` (groups whose name starts with
/// `query`, without regard to case; every group without it), by name.
///
/// JSON (as a query): `{"query":"night","limit":20}` (every field optional).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupQuery {
    /// The start of the name (at most [`MAX_QUERY_CHARS`] characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Where to continue (`next_cursor` of the previous page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many (default 50, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl GroupQuery {
    /// Every group, by name.
    pub fn new() -> Self {
        Self::default()
    }

    /// The groups whose name starts with `prefix`.
    pub fn starting_with(prefix: impl Into<String>) -> Self {
        Self { query: Some(prefix.into()), ..Self::default() }
    }

    /// The page after `cursor`.
    pub fn after(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same query with this limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The page part.
    pub fn page(&self) -> PageRequest {
        PageRequest { cursor: self.cursor.clone(), limit: self.limit }
    }
}

/// A player, in the body of an invitation or a transfer.
///
/// JSON: `{"user":42}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Invitee {
    /// The player.
    pub user: UserId,
}

impl Invitee {
    /// The player `user`.
    pub fn new(user: UserId) -> Self {
        Self { user }
    }
}

/// A new role, the body of `PUT /v1/groups/{group}/members/{user}/role`: `admin` or `member` (the
/// owner changes with [`TransferGroup`]).
///
/// JSON: `{"role":"admin"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoleChange {
    /// The role.
    pub role: GroupRole,
}

impl RoleChange {
    /// The role `role`.
    pub fn new(role: GroupRole) -> Self {
        Self { role }
    }
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(CreateGroup, Post, routes::groups::LIST, true, Json, GroupInfo);

    /// Groups by name: `GET /v1/groups?query=…` → [`Page`]`<`[`GroupInfo`]`>`.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListGroups {
        /// The name prefix and the page.
        pub query: GroupQuery,
    }

    impl ListGroups {
        /// Every group, first page.
        pub fn new() -> Self {
            Self::default()
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: GroupQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for ListGroups {
        type Payload = GroupQuery;
        type Response = Page<GroupInfo>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::groups::LIST, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &GroupQuery {
            &self.query
        }

        fn from_parts(_params: &PathParams, query: GroupQuery) -> Result<Self, ApiError> {
            Ok(Self::new().with_query(query))
        }
    }

    /// The caller's groups: `GET /v1/groups/mine` → [`GroupList`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct MyGroups {}

    impl MyGroups {
        /// The call.
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HttpCall for MyGroups {
        type Payload = NoPayload;
        type Response = GroupList;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::groups::MINE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new())
        }
    }

    /// The caller's invitations: `GET /v1/groups/invites?cursor=…&limit=…` →
    /// [`Page`]`<`[`GroupInvite`]`>` (newest first).
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListGroupInvites {
        /// Which page.
        pub page: PageRequest,
    }

    impl ListGroupInvites {
        /// The first page.
        pub fn new() -> Self {
            Self::default()
        }

        /// The same call for this page.
        pub fn with_page(mut self, page: PageRequest) -> Self {
            self.page = page;
            self
        }
    }

    impl HttpCall for ListGroupInvites {
        type Payload = PageRequest;
        type Response = Page<GroupInvite>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::groups::INVITES, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn from_parts(_params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new().with_page(page))
        }
    }

    /// A call naming one group in the path, without a payload.
    macro_rules! group_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The group.
                pub group: GroupId,
            }

            impl $name {
                /// The call for `group`.
                pub fn new(group: GroupId) -> Self {
                    Self { group }
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("group", self.group)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("group")?))
                }
            }
        };
    }

    group_call!(
        /// One group: `GET /v1/groups/{group}` → [`GroupInfo`] (with the caller's role when a member).
        GetGroup,
        Get,
        routes::groups::ONE,
        GroupInfo
    );
    group_call!(
        /// Delete a group (its owner): `DELETE /v1/groups/{group}` → [`Ack`].
        DeleteGroup,
        Delete,
        routes::groups::ONE,
        Ack
    );
    group_call!(
        /// Join a group: `POST /v1/groups/{group}/join` → [`GroupInfo`]. An open group, or one that
        /// invited the caller (the invitation is used); a member already: the group again.
        JoinGroup,
        Post,
        routes::groups::JOIN,
        GroupInfo
    );
    group_call!(
        /// Leave a group: `POST /v1/groups/{group}/leave` → [`Ack`]. The owner leaves only as the last
        /// member (the group is deleted then).
        LeaveGroup,
        Post,
        routes::groups::LEAVE,
        Ack
    );
    group_call!(
        /// Accept an invitation: `POST /v1/groups/{group}/invites/accept` → [`GroupInfo`] (404 without
        /// one).
        AcceptGroupInvite,
        Post,
        routes::groups::ACCEPT,
        GroupInfo
    );
    group_call!(
        /// Decline an invitation: `POST /v1/groups/{group}/invites/decline` → [`Ack`] (also when there
        /// was none).
        DeclineGroupInvite,
        Post,
        routes::groups::DECLINE,
        Ack
    );

    /// A group's members: `GET /v1/groups/{group}/members?cursor=…&limit=…` →
    /// [`Page`]`<`[`GroupMember`]`>` (in the order they joined).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListGroupMembers {
        /// The group.
        pub group: GroupId,
        /// Which page.
        pub page: PageRequest,
    }

    impl ListGroupMembers {
        /// The first page of `group`'s members.
        pub fn new(group: GroupId) -> Self {
            Self { group, page: PageRequest::first() }
        }

        /// The same call for this page.
        pub fn with_page(mut self, page: PageRequest) -> Self {
            self.page = page;
            self
        }
    }

    impl HttpCall for ListGroupMembers {
        type Payload = PageRequest;
        type Response = Page<GroupMember>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::groups::MEMBERS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("group", self.group)
        }

        fn from_parts(params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("group")?).with_page(page))
        }
    }

    /// A call naming one group in the path, with a JSON body.
    macro_rules! group_body_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $body:ident, $field:ident, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Debug, PartialEq)]
            #[non_exhaustive]
            pub struct $name {
                /// The group.
                pub group: GroupId,
                /// The body.
                pub $field: $body,
            }

            impl $name {
                /// The call for `group` with this body.
                pub fn new(group: GroupId, $field: $body) -> Self {
                    Self { group, $field }
                }
            }

            impl HttpCall for $name {
                type Payload = $body;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Json;

                fn payload(&self) -> &$body {
                    &self.$field
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("group", self.group)
                }

                fn from_parts(params: &PathParams, $field: $body) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("group")?, $field))
                }
            }
        };
    }

    group_body_call!(
        /// Change a group (owner, admins): `PATCH /v1/groups/{group}` with an [`UpdateGroup`] →
        /// [`GroupInfo`].
        EditGroup,
        Patch,
        routes::groups::ONE,
        UpdateGroup,
        update,
        GroupInfo
    );
    group_body_call!(
        /// Invite a player (owner, admins): `POST /v1/groups/{group}/invites` with an [`Invitee`] →
        /// [`Ack`] (also when the player was invited already).
        InviteToGroup,
        Post,
        routes::groups::GROUP_INVITES,
        Invitee,
        invitee,
        Ack
    );
    group_body_call!(
        /// Hand the group to a member (owner): `POST /v1/groups/{group}/transfer` with an [`Invitee`]
        /// (the new owner) → [`Ack`]; the old owner becomes an admin.
        TransferGroup,
        Post,
        routes::groups::TRANSFER,
        Invitee,
        to,
        Ack
    );

    /// A call naming a group and a player in the path.
    macro_rules! member_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The group.
                pub group: GroupId,
                /// The player.
                pub user: UserId,
            }

            impl $name {
                /// The call for `user` in `group`.
                pub fn new(group: GroupId, user: UserId) -> Self {
                    Self { group, user }
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("group", self.group).with("user", self.user)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("group")?, params.id("user")?))
                }
            }
        };
    }

    member_call!(
        /// Withdraw an invitation (owner, admins): `DELETE /v1/groups/{group}/invites/{user}` →
        /// [`Ack`] (also when there was none).
        RevokeGroupInvite,
        Delete,
        routes::groups::INVITE,
        Ack
    );
    member_call!(
        /// Remove a member (owner: anyone; admins: members): `DELETE /v1/groups/{group}/members/{user}`
        /// → [`Ack`] (also when the player is no member).
        KickMember,
        Delete,
        routes::groups::MEMBER,
        Ack
    );

    /// Change a member's role (owner): `PUT /v1/groups/{group}/members/{user}/role` with a
    /// [`RoleChange`] → [`Ack`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct SetMemberRole {
        /// The group.
        pub group: GroupId,
        /// The member.
        pub user: UserId,
        /// The new role.
        pub change: RoleChange,
    }

    impl SetMemberRole {
        /// Give `user` in `group` the role `role`.
        pub fn new(group: GroupId, user: UserId, role: GroupRole) -> Self {
            Self { group, user, change: RoleChange::new(role) }
        }
    }

    impl HttpCall for SetMemberRole {
        type Payload = RoleChange;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Put, routes::groups::ROLE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &RoleChange {
            &self.change
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("group", self.group).with("user", self.user)
        }

        fn from_parts(params: &PathParams, change: RoleChange) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("group")?, params.id("user")?, change.role))
        }
    }
}

pub use calls::{
    AcceptGroupInvite, DeclineGroupInvite, DeleteGroup, EditGroup, GetGroup, InviteToGroup, JoinGroup, KickMember, LeaveGroup, ListGroupInvites,
    ListGroupMembers, ListGroups, MyGroups, RevokeGroupInvite, SetMemberRole, TransferGroup,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_descriptions() {
        assert!(group_name_problem("Night Owls").is_none());
        assert!(group_name_problem("ab").is_some() && group_name_problem(&"x".repeat(MAX_GROUP_NAME_CHARS + 1)).is_some());
        assert!(group_name_problem("   abc   ").is_none(), "trimmed");
        assert!(group_name_problem("a\u{202E}bc").is_some());
        assert!(group_name_problem("ab\ncd").is_some());
        assert!(description_problem("").is_none() && description_problem("We play\nat night").is_none());
        assert!(description_problem(&"é".repeat(MAX_DESCRIPTION_CHARS + 1)).is_some());
    }

    #[test]
    fn json_and_rules() {
        let create = CreateGroup::new("Night Owls").open().with_metadata(serde_json::json!({"tag": "NO"}));
        assert_eq!(serde_json::to_string(&create).ok().as_deref(), Some(r#"{"name":"Night Owls","open":true,"metadata":{"tag":"NO"}}"#));
        assert!(create.validate().is_ok());
        assert!(CreateGroup::new("x").validate().is_err());
        assert_eq!(serde_json::from_str::<CreateGroup>(r#"{"name":"abc"}"#).ok().map(|c| c.open), Some(false));
        assert!(UpdateGroup::new().with_description("").validate().is_ok());
        assert!(UpdateGroup::new().with_name("x").validate().is_err());
        let clear = serde_json::from_str::<UpdateGroup>(r#"{"metadata":null}"#).ok().and_then(|u| u.metadata);
        assert_eq!(clear, Some(Value::Null), "a present null clears the metadata");
        assert_eq!(serde_json::from_str::<UpdateGroup>("{}").ok().map(|u| u.metadata), Some(None));
        assert_eq!(serde_json::to_string(&UpdateGroup::new().with_metadata(Value::Null)).ok().as_deref(), Some(r#"{"metadata":null}"#));
        let info = GroupInfo::new(GroupId(5), "Night Owls", 2, 100, UnixMillis(1)).with_owner(UserId(42)).with_role(GroupRole::Admin);
        assert_eq!(
            serde_json::to_string(&info).ok().as_deref(),
            Some(r#"{"id":5,"name":"Night Owls","open":false,"owner":42,"members":2,"max_members":100,"created_at":1,"role":"admin"}"#)
        );
        assert_eq!(serde_json::from_str::<GroupRole>(r#""officer""#).ok(), Some(GroupRole::Unknown));
        assert_eq!(serde_json::to_string(&RoleChange::new(GroupRole::Member)).ok().as_deref(), Some(r#"{"role":"member"}"#));
        assert_eq!(GroupQuery::starting_with("ni").with_limit(5).page().limit, Some(5));
    }
}
