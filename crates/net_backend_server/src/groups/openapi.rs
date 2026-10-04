//! OpenAPI schemas of the protocol's groups types (mirror structs: the protocol crate has no
//! OpenAPI dependency). A test serializes the real types and compares the field names with these
//! schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A group as players see it.
#[derive(Serialize, ToSchema)]
pub(crate) struct GroupInfo {
    /// The id.
    id: i64,
    /// The name.
    name: String,
    /// The description.
    description: Option<String>,
    /// Anyone may join without an invitation.
    open: bool,
    /// The game's data about the group.
    #[schema(value_type = Option<Value>)]
    metadata: Option<serde_json::Value>,
    /// The owner (absent when the owner's account was deleted).
    owner: Option<i64>,
    /// How many members it has.
    members: u32,
    /// The most members a group may have on this server.
    max_members: u32,
    /// The group's chat room (members only), with the chat module.
    chat_room: Option<i64>,
    /// When it was created (unix ms).
    created_at: i64,
    /// The caller's role (`owner`, `admin`, `member`), when a member.
    role: Option<String>,
}

/// A page of groups, by name.
#[derive(Serialize, ToSchema)]
pub(crate) struct GroupPage {
    /// The groups.
    items: Vec<GroupInfo>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// The caller's groups.
#[derive(Serialize, ToSchema)]
pub(crate) struct GroupList {
    /// The groups with the caller's role, oldest membership first.
    groups: Vec<GroupInfo>,
}

/// One member.
#[derive(Serialize, ToSchema)]
pub(crate) struct GroupMember {
    /// The member.
    user: i64,
    /// Their display name.
    name: Option<String>,
    /// `owner`, `admin` or `member`.
    role: String,
    /// When they joined (unix ms).
    joined_at: i64,
}

/// A page of members, in the order they joined.
#[derive(Serialize, ToSchema)]
pub(crate) struct MemberPage {
    /// The members.
    items: Vec<GroupMember>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// An invitation the caller received.
#[derive(Serialize, ToSchema)]
pub(crate) struct GroupInvite {
    /// The group.
    group: GroupInfo,
    /// Who invited the caller.
    inviter: Option<i64>,
    /// When (unix ms).
    created_at: i64,
}

/// A page of invitations, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct InvitePage {
    /// The invitations.
    items: Vec<GroupInvite>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// `POST /v1/groups` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct CreateGroup {
    /// 3-32 characters, unique without regard to case.
    name: String,
    /// At most 500 characters.
    description: Option<String>,
    /// Anyone may join without an invitation (default false).
    open: bool,
    /// The game's data (at most the server's limit, 2 KiB by default).
    #[schema(value_type = Option<Value>)]
    metadata: Option<serde_json::Value>,
}

/// `PATCH /v1/groups/{group}` body: absent fields stay; an empty description and `metadata: null` remove them.
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateGroup {
    /// A new name.
    name: Option<String>,
    /// A new description (empty: none).
    description: Option<String>,
    /// Open or not.
    open: Option<bool>,
    /// New metadata (`null`: none).
    #[schema(value_type = Option<Value>)]
    metadata: Option<serde_json::Value>,
}

/// A player: the body of an invitation or a transfer.
#[derive(Serialize, ToSchema)]
pub(crate) struct Invitee {
    /// The player.
    user: i64,
}

/// A new role: `admin` or `member`.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoleChange {
    /// `admin` or `member`.
    role: String,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::groups as p;
    use net_backend_protocol::{Cursor, GroupId, Page, RoomId, UnixMillis, UserId};
    use serde_json::{json, Value};
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    fn properties<T: PartialSchema>() -> BTreeSet<String> {
        match T::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    fn keys(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value) {
            Ok(Value::Object(map)) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Every mirror has exactly the fields of the real type with all optional fields set.
    #[test]
    fn mirrors_match_the_protocol() {
        let info = p::GroupInfo::new(GroupId(5), "Night Owls", 2, 100, UnixMillis(1))
            .with_description("d")
            .with_metadata(json!(1))
            .with_owner(UserId(1))
            .with_chat_room(RoomId(9))
            .with_role(p::GroupRole::Owner);
        assert_eq!(properties::<GroupInfo>(), keys(&info));
        assert_eq!(properties::<GroupPage>(), keys(Page::new(vec![info.clone()], Some(Cursor::new("a")))));
        assert_eq!(properties::<GroupList>(), keys(p::GroupList::new(vec![info.clone()])));
        let member = p::GroupMember::new(UserId(1), p::GroupRole::Admin, UnixMillis(1)).with_name("Ada");
        assert_eq!(properties::<GroupMember>(), keys(&member));
        assert_eq!(properties::<MemberPage>(), keys(Page::new(vec![member], Some(Cursor::new("1")))));
        let invite = p::GroupInvite::new(info, Some(UserId(1)), UnixMillis(1));
        assert_eq!(properties::<GroupInvite>(), keys(&invite));
        assert_eq!(properties::<InvitePage>(), keys(Page::new(vec![invite], Some(Cursor::new("1")))));
        assert_eq!(properties::<CreateGroup>(), keys(p::CreateGroup::new("abc").with_description("d").open().with_metadata(json!(1))));
        let update = p::UpdateGroup::new().with_name("abc").with_description("d").with_open(true).with_metadata(json!(1));
        assert_eq!(properties::<UpdateGroup>(), keys(&update));
        assert_eq!(properties::<Invitee>(), keys(p::Invitee::new(UserId(1))));
        assert_eq!(properties::<RoleChange>(), keys(p::RoleChange::new(p::GroupRole::Admin)));
    }
}
