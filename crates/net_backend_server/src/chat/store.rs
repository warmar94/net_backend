//! The chat module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the chat suite on SQLite locally and on MySQL / PostgreSQL (env-gated).

use sea_query::{Cond, Expr, ExprTrait, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::{MEMBERS, MESSAGES, READS, ROOMS};
use crate::db::{DbError, Dialect};

pub(crate) const KIND_ROOM: &str = "room";
pub(crate) const KIND_DM: &str = "dm";
pub(crate) const KIND_GROUP: &str = "group";
pub(crate) const KIND_PLAYER: &str = "player";

/// Member roles (`chat_members.role`): group rooms' rows are `member`; player rooms use them all.
pub(crate) const OWNER: &str = "owner";
pub(crate) const MODERATOR: &str = "moderator";
pub(crate) const MEMBER: &str = "member";
pub(crate) const INVITED: &str = "invited";
pub(crate) const BANNED: &str = "banned";
/// The roles of a member (who reads and writes).
pub(crate) const ACTIVE: [&str; 3] = [OWNER, MODERATOR, MEMBER];
/// The roles that take a place in a player room (members and open invitations).
pub(crate) const PLACES: [&str; 4] = [OWNER, MODERATOR, MEMBER, INVITED];

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const ROOM_COLUMNS: [&str; 9] = ["id", "kind", "room_key", "name", "max_members", "dm_a", "dm_b", "last_activity_at", "is_public"];

#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct RoomRow {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) room_key: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) max_members: Option<i64>,
    pub(crate) dm_a: Option<i64>,
    pub(crate) dm_b: Option<i64>,
    pub(crate) last_activity_at: i64,
    pub(crate) is_public: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MessageRow {
    pub(crate) id: i64,
    pub(crate) room_id: i64,
    pub(crate) sender_id: i64,
    pub(crate) sender_name: Option<String>,
    pub(crate) body: String,
    pub(crate) nonce: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) deleted_at: Option<i64>,
    pub(crate) edited_at: Option<i64>,
}

/// One row of `chat_members`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct MemberRow {
    pub(crate) id: i64,
    pub(crate) room_id: i64,
    pub(crate) user_id: i64,
    pub(crate) role: String,
    pub(crate) created_at: i64,
}

/// One read marker.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct ReadRow {
    pub(crate) room_id: i64,
    pub(crate) user_id: i64,
    pub(crate) message_id: i64,
    pub(crate) read_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

fn rooms() -> SelectStatement {
    let mut select = Query::select();
    select.columns(ROOM_COLUMNS).from(ROOMS);
    select
}

pub(crate) fn room_by_id(id: i64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("id").eq(id));
    select
}

pub(crate) fn room_by_key(key: &str) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("room_key").eq(key));
    select
}

/// The direct-message room of two users (`a` < `b`).
pub(crate) fn dm_room(a: i64, b: i64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("dm_a").eq(a)).and_where(Expr::col("dm_b").eq(b));
    select
}

/// A page of public rooms by id.
pub(crate) fn public_rooms(after: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("kind").eq(KIND_ROOM));
    if let Some(after) = after {
        select.and_where(Expr::col("id").gt(after));
    }
    select.order_by("id", Order::Asc).limit(limit);
    select
}

/// A page of a user's direct-message rooms, newest activity first, after the cursor
/// `(activity, id)`.
pub(crate) fn dms_of(user: i64, before: Option<(i64, i64)>, limit: u64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("kind").eq(KIND_DM)).cond_where(Cond::any().add(Expr::col("dm_a").eq(user)).add(Expr::col("dm_b").eq(user)));
    if let Some((activity, id)) = before {
        select.cond_where(
            Cond::any()
                .add(Expr::col("last_activity_at").lt(activity))
                .add(Cond::all().add(Expr::col("last_activity_at").eq(activity)).add(Expr::col("id").lt(id))),
        );
    }
    select.order_by("last_activity_at", Order::Desc).order_by("id", Order::Desc).limit(limit);
    select
}

/// A new room.
pub(crate) fn insert_room(
    kind: &str,
    key: Option<&str>,
    name: Option<&str>,
    max_members: Option<i64>,
    dm: Option<(i64, i64)>,
    now: i64,
) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(ROOMS)
        .columns(["kind", "room_key", "name", "max_members", "dm_a", "dm_b", "created_at", "last_activity_at"])
        .values([
            kind.into(),
            key.map(str::to_string).into(),
            name.map(str::to_string).into(),
            max_members.into(),
            dm.map(|d| d.0).into(),
            dm.map(|d| d.1).into(),
            now.into(),
            now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

/// A group room another module creates (`origin`: `lobbies`, `groups`), without members yet.
pub(crate) fn insert_module_room(origin: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(ROOMS)
        .columns(["kind", "origin", "created_at", "last_activity_at"])
        .values([KIND_GROUP.into(), origin.into(), now.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// The group rooms `origin` created before `before`, with an id above `after`, oldest first.
#[cfg(any(feature = "groups", feature = "lobbies"))]
pub(crate) fn module_rooms(origin: &str, before: i64, after: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("id")
        .from(ROOMS)
        .and_where(Expr::col("origin").eq(origin))
        .and_where(Expr::col("id").gt(after))
        .and_where(Expr::col("kind").eq(KIND_GROUP))
        .and_where(Expr::col("created_at").lt(before))
        .order_by("id", Order::Asc)
        .limit(limit);
    select
}

/// Every row of a room (any role).
pub(crate) fn all_rows(room: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("room_id").eq(room));
    select
}

/// Change a public room's name and cap.
pub(crate) fn update_room(id: i64, name: Option<&str>, max_members: Option<i64>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(ROOMS).value("name", name.map(str::to_string)).value("max_members", max_members).and_where(Expr::col("id").eq(id));
    update
}

/// A message makes the room the newest in its members' lists.
pub(crate) fn touch_room(id: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(ROOMS).value("last_activity_at", now).and_where(Expr::col("id").eq(id));
    update
}

const MEMBER_COLUMNS: [&str; 5] = ["id", "room_id", "user_id", "role", "created_at"];

/// The row of `user` in `room` (any role).
pub(crate) fn member(room: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn insert_member(room: i64, user: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(MEMBERS).columns(["room_id", "user_id", "created_at"]).values([room.into(), user.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn delete_member(room: i64, user: i64) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MEMBERS).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("user_id").eq(user));
    delete
}

/// The display names of accounts (absent rows: no such account).
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

pub(crate) fn insert_message(room: i64, sender: i64, sender_name: Option<&str>, body: &str, nonce: Option<&str>, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(MESSAGES)
        .columns(["room_id", "sender_id", "sender_name", "body", "nonce", "created_at"])
        .values([room.into(), sender.into(), sender_name.map(str::to_string).into(), body.into(), nonce.map(str::to_string).into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

const MESSAGE_COLUMNS: [&str; 9] = ["id", "room_id", "sender_id", "sender_name", "body", "nonce", "created_at", "deleted_at", "edited_at"];

/// A page of a room's history: newest first, before the message id `before`, not deleted, not
/// older than `since`.
pub(crate) fn history(room: i64, before: Option<i64>, since: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MESSAGE_COLUMNS).from(MESSAGES).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("deleted_at").is_null());
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    if let Some(since) = since {
        select.and_where(Expr::col("created_at").gte(since));
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

pub(crate) fn message(room: i64, id: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MESSAGE_COLUMNS).from(MESSAGES).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("id").eq(id));
    select
}

/// Delete a message (moderation): the body is cleared, the row stays (ids keep growing).
/// 0 rows: deleted already.
pub(crate) fn delete_message(id: i64, by: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(MESSAGES)
        .value("body", "")
        .value("deleted_at", now)
        .value("deleted_by", by)
        .and_where(Expr::col("id").eq(id))
        .and_where(Expr::col("deleted_at").is_null());
    update
}

/// Change a message's text (an edit). 0 rows: deleted meanwhile.
pub(crate) fn edit_message(id: i64, body: &str, by: Option<i64>, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(MESSAGES)
        .value("body", body)
        .value("edited_at", now)
        .value("edited_by", by)
        .and_where(Expr::col("id").eq(id))
        .and_where(Expr::col("deleted_at").is_null());
    update
}

/// Up to `limit` ids of messages older than the retention, oldest first (the purge deletes in
/// batches: one huge DELETE would hold its locks for long).
pub(crate) fn expired(cutoff: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(MESSAGES).and_where(Expr::col("created_at").lt(cutoff)).order_by("id", Order::Asc).limit(limit);
    select
}

/// Delete these messages (a batch of [`expired`]).
pub(crate) fn delete_messages(ids: &[i64]) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MESSAGES).and_where(Expr::col("id").is_in(ids.iter().copied()));
    delete
}

// ---- player rooms --------------------------------------------------------------------------------

fn locked(mut select: SelectStatement, dialect: Dialect) -> SelectStatement {
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// The room row, locked for the transaction (MySQL `FOR UPDATE`, PostgreSQL `FOR NO KEY UPDATE`,
/// SQLite: the write transaction).
pub(crate) fn lock_room(id: i64, dialect: Dialect) -> SelectStatement {
    locked(room_by_id(id), dialect)
}

/// The account row, locked for the transaction (`None`: no such account).
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    locked(select, dialect)
}

/// A new player room.
pub(crate) fn insert_player_room(name: &str, public: bool, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(ROOMS)
        .columns(["kind", "name", "is_public", "created_at", "last_activity_at"])
        .values([KIND_PLAYER.into(), name.into(), i64::from(public).into(), now.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// Rename a player room and / or change its visibility.
pub(crate) fn update_player_room(id: i64, name: Option<&str>, public: Option<bool>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(ROOMS).and_where(Expr::col("id").eq(id));
    if let Some(name) = name {
        update.value("name", name);
    }
    if let Some(public) = public {
        update.value("is_public", i64::from(public));
    }
    update
}

/// Delete a room (its members, messages and read markers go with it: `ON DELETE CASCADE`).
pub(crate) fn delete_room(id: i64) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(ROOMS).and_where(Expr::col("id").eq(id));
    delete
}

/// A page of public player rooms by id.
pub(crate) fn public_player_rooms(after: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("kind").eq(KIND_PLAYER)).and_where(Expr::col("is_public").eq(1));
    if let Some(after) = after {
        select.and_where(Expr::col("id").gt(after));
    }
    select.order_by("id", Order::Asc).limit(limit);
    select
}

/// These rooms.
pub(crate) fn rooms_by_id(ids: &[i64]) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("id").is_in(ids.iter().copied()));
    select
}

pub(crate) fn insert_member_role(room: i64, user: i64, role: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(MEMBERS)
        .columns(["room_id", "user_id", "role", "created_at"])
        .values([room.into(), user.into(), role.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// Give a row a new role (and a new `since`).
pub(crate) fn set_role(id: i64, role: &str, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(MEMBERS).value("role", role).value("created_at", now).and_where(Expr::col("id").eq(id));
    update
}

/// Give a row a new role, keeping its `since`.
pub(crate) fn change_role(id: i64, role: &str) -> UpdateStatement {
    let mut update = Query::update();
    update.table(MEMBERS).value("role", role).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn delete_member_row(id: i64) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MEMBERS).and_where(Expr::col("id").eq(id));
    delete
}

/// A page of a room's rows with one of `roles`, oldest first.
pub(crate) fn room_rows(room: i64, roles: &[&str], after: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("role").is_in(roles.iter().copied()));
    if let Some(after) = after {
        select.and_where(Expr::col("id").gt(after));
    }
    select.order_by("id", Order::Asc).limit(limit);
    select
}

/// How many rows of a room have one of `roles`.
pub(crate) fn count_rows(room: i64, roles: &[&str]) -> SelectStatement {
    let mut select = Query::select();
    select
        .expr_as(Expr::col("id").count(), "n")
        .from(MEMBERS)
        .and_where(Expr::col("room_id").eq(room))
        .and_where(Expr::col("role").is_in(roles.iter().copied()));
    select
}

/// How many player rooms `user` owns.
pub(crate) fn count_owned(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(MEMBERS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("role").eq(OWNER));
    select
}

/// The oldest row of `room` with `role` (the next owner).
pub(crate) fn oldest_with(room: i64, role: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(MEMBER_COLUMNS)
        .from(MEMBERS)
        .and_where(Expr::col("room_id").eq(room))
        .and_where(Expr::col("role").eq(role))
        .order_by("id", Order::Asc)
        .limit(1);
    select
}

/// A page of `user`'s rows with one of `roles` in player rooms, oldest first.
pub(crate) fn memberships(user: i64, roles: &[&str], after: Option<i64>, limit: u64) -> SelectStatement {
    let mut player = Query::select();
    player
        .column((ROOMS, "id"))
        .from(ROOMS)
        .and_where(Expr::col((ROOMS, "id")).equals((MEMBERS, "room_id")))
        .and_where(Expr::col((ROOMS, "kind")).eq(KIND_PLAYER));
    let mut select = Query::select();
    for column in MEMBER_COLUMNS {
        select.column((MEMBERS, column));
    }
    select
        .from(MEMBERS)
        .and_where(Expr::col((MEMBERS, "user_id")).eq(user))
        .and_where(Expr::col((MEMBERS, "role")).is_in(roles.iter().copied()))
        .and_where(Expr::exists(player));
    if let Some(after) = after {
        select.and_where(Expr::col((MEMBERS, "id")).gt(after));
    }
    select.order_by((MEMBERS, "id"), Order::Asc).limit(limit);
    select
}

/// The rows of these rooms with `role` (the owners).
pub(crate) fn with_role_in(rooms: &[i64], role: &str) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("room_id").is_in(rooms.iter().copied())).and_where(Expr::col("role").eq(role));
    select
}

/// `user`'s rows in these rooms.
pub(crate) fn rows_of_user_in(user: i64, rooms: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("room_id").is_in(rooms.iter().copied()));
    select
}

/// Player rooms without an owner (the owner's account was deleted), at most `limit`.
pub(crate) fn ownerless_player_rooms(limit: u64) -> SelectStatement {
    let mut owners = Query::select();
    owners
        .column((MEMBERS, "id"))
        .from(MEMBERS)
        .and_where(Expr::col((MEMBERS, "room_id")).equals((ROOMS, "id")))
        .and_where(Expr::col((MEMBERS, "role")).eq(OWNER));
    let mut select = Query::select();
    select.column((ROOMS, "id")).from(ROOMS).and_where(Expr::col((ROOMS, "kind")).eq(KIND_PLAYER)).and_where(Expr::not_exists(owners)).limit(limit);
    select
}

// ---- read markers --------------------------------------------------------------------------------

const READ_COLUMNS: [&str; 4] = ["room_id", "user_id", "message_id", "read_at"];

/// `user`'s markers in these rooms.
pub(crate) fn reads_of(user: i64, rooms: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(READ_COLUMNS).from(READS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("room_id").is_in(rooms.iter().copied()));
    select
}

/// Move a marker forward. 0 rows: no marker stored, or it is at `message` or beyond.
pub(crate) fn advance_read(room: i64, user: i64, message: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(READS)
        .value("message_id", message)
        .value("read_at", now)
        .and_where(Expr::col("room_id").eq(room))
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("message_id").lt(message));
    update
}

pub(crate) fn insert_read(room: i64, user: i64, message: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(READS)
        .columns(["room_id", "user_id", "message_id", "read_at"])
        .values([room.into(), user.into(), message.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// The newest `limit` markers of a room.
pub(crate) fn receipts(room: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(READ_COLUMNS).from(READS).and_where(Expr::col("room_id").eq(room)).order_by("read_at", Order::Desc).order_by("id", Order::Desc).limit(limit);
    select
}

/// How many messages of `room` are unread for `user` (after `after`, not the user's own, not
/// deleted, not older than `since`), counted up to `cap`.
pub(crate) fn unread_count(room: i64, user: i64, after: Option<i64>, since: Option<i64>, cap: u64) -> SelectStatement {
    let mut ids = Query::select();
    ids.column("id")
        .from(MESSAGES)
        .and_where(Expr::col("room_id").eq(room))
        .and_where(Expr::col("deleted_at").is_null())
        .and_where(Expr::col("sender_id").ne(user));
    if let Some(after) = after {
        ids.and_where(Expr::col("id").gt(after));
    }
    if let Some(since) = since {
        ids.and_where(Expr::col("created_at").gte(since));
    }
    ids.limit(cap);
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from_subquery(ids, "unread");
    select
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{render_statement, Dialect};

    #[test]
    fn statements_render() {
        let page = render_statement(&dms_of(4, Some((10, 7)), 21), Dialect::Postgres);
        assert!(page.contains("ORDER BY \"last_activity_at\" DESC, \"id\" DESC") && page.contains("\"dm_a\" = 4 OR \"dm_b\" = 4"), "{page}");
        let expired = render_statement(&expired(77, 1000), Dialect::Postgres);
        assert!(expired.contains("\"created_at\" < 77") && expired.ends_with("LIMIT 1000"), "{expired}");
        let history = render_statement(&history(1, Some(50), Some(9), 11), Dialect::MySql);
        assert!(history.contains("`deleted_at` IS NULL") && history.contains("`id` < 50") && history.contains("`created_at` >= 9"), "{history}");
        for dialect in Dialect::ALL {
            let unread = render_statement(&unread_count(3, 4, Some(10), Some(5), 1000), *dialect);
            assert!(unread.contains("COUNT(") && unread.contains("LIMIT 1000") && unread.contains("unread"), "{unread}");
            let ownerless = render_statement(&ownerless_player_rooms(50), *dialect);
            assert!(ownerless.contains("NOT EXISTS") && ownerless.contains("'owner'"), "{ownerless}");
            let mine = render_statement(&memberships(4, &PLACES, Some(9), 21), *dialect);
            assert!(mine.contains("EXISTS") && mine.contains("'player'"), "{mine}");
        }
        assert!(render_statement(&lock_room(1, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(render_statement(&lock_room(1, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
    }
}
