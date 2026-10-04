//! The lobbies module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the lobbies suite on SQLite locally and on MySQL / PostgreSQL (env-gated).
//!
//! **Writes to a lobby** take its row lock first (MySQL `FOR UPDATE`, PostgreSQL
//! `FOR NO KEY UPDATE`, SQLite's write transaction); a write that also counts a player's lobbies
//! (create, join) takes the player's account lock BEFORE the lobby's. Rows change by primary key
//! (or by the lobby's id under its lock) after a plain read, and the member count (counted, never
//! stored) stays exact under concurrent joins.

use sea_query::{DeleteStatement, Expr, ExprTrait, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::{LOBBIES, MEMBERS, METADATA};
use crate::db::{DbError, Dialect};

pub(crate) const OPEN: &str = "open";
pub(crate) const IN_GAME: &str = "in_game";
pub(crate) const PUBLIC: &str = "public";
pub(crate) const PRIVATE: &str = "private";
pub(crate) const FRIENDS: &str = "friends";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const LOBBY_COLUMNS: [&str; 8] = ["id", "code", "visibility", "state", "host_id", "max_players", "chat_room", "created_at"];
const MEMBER_COLUMNS: [&str; 5] = ["id", "lobby_id", "user_id", "ready", "joined_at"];

/// One lobby.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct LobbyRow {
    pub(crate) id: i64,
    pub(crate) code: String,
    pub(crate) visibility: String,
    pub(crate) state: String,
    pub(crate) host_id: Option<i64>,
    pub(crate) max_players: i64,
    pub(crate) chat_room: Option<i64>,
    pub(crate) created_at: i64,
}

/// One membership.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct MemberRow {
    pub(crate) id: i64,
    pub(crate) lobby_id: i64,
    pub(crate) user_id: i64,
    pub(crate) ready: i64,
    pub(crate) joined_at: i64,
}

/// One metadata entry.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct MetaRow {
    pub(crate) lobby_id: i64,
    pub(crate) meta_key: String,
    pub(crate) meta_value: String,
}

/// One metadata entry with its row id (a change by primary key).
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MetaIdRow {
    pub(crate) id: i64,
    pub(crate) meta_key: String,
    pub(crate) meta_value: String,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

/// A lobby's id and its chat room.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RoomRefRow {
    pub(crate) id: i64,
    pub(crate) chat_room: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct LobbyCountRow {
    pub(crate) lobby_id: i64,
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

fn locked(mut select: SelectStatement, dialect: Dialect) -> SelectStatement {
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// The account row, locked for the transaction (`None` if the account does not exist).
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    locked(select, dialect)
}

/// The lobby row, locked for the transaction.
pub(crate) fn lock_lobby(lobby: i64, dialect: Dialect) -> SelectStatement {
    locked(lobby_by_id(lobby), dialect)
}

pub(crate) fn lobby_by_id(lobby: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LOBBY_COLUMNS).from(LOBBIES).and_where(Expr::col("id").eq(lobby));
    select
}

pub(crate) fn lobbies_by_id(lobbies: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LOBBY_COLUMNS).from(LOBBIES).and_where(Expr::col("id").is_in(lobbies.iter().copied()));
    select
}

pub(crate) fn lobby_by_code(code: &str) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LOBBY_COLUMNS).from(LOBBIES).and_where(Expr::col("code").eq(code));
    select
}

/// What a search looks for.
pub(crate) struct Search<'a> {
    pub(crate) filters: &'a [(String, String)],
    /// Only lobbies these accounts host (the caller's friends), public or friends-only; `None`:
    /// every public lobby.
    pub(crate) hosts: Option<&'a [i64]>,
    pub(crate) include_full: bool,
    /// Lobbies with a smaller id than this.
    pub(crate) before: Option<i64>,
    pub(crate) limit: u64,
}

/// A page of open lobbies, newest first.
pub(crate) fn search(s: Search<'_>) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LOBBY_COLUMNS.map(|c| (LOBBIES, c))).from(LOBBIES).and_where(Expr::col((LOBBIES, "state")).eq(OPEN));
    match s.hosts {
        Some(hosts) => {
            select
                .and_where(Expr::col((LOBBIES, "visibility")).is_in([PUBLIC, FRIENDS]))
                .and_where(Expr::col((LOBBIES, "host_id")).is_in(hosts.iter().copied()));
        }
        None => {
            select.and_where(Expr::col((LOBBIES, "visibility")).eq(PUBLIC));
        }
    }
    for (key, value) in s.filters {
        let mut meta = Query::select();
        meta.column((METADATA, "id"))
            .from(METADATA)
            .and_where(Expr::col((METADATA, "lobby_id")).equals((LOBBIES, "id")))
            .and_where(Expr::col((METADATA, "meta_key")).eq(key.as_str()))
            .and_where(Expr::col((METADATA, "meta_value")).eq(value.as_str()));
        select.and_where(Expr::exists(meta));
    }
    if !s.include_full {
        let mut count = Query::select();
        count.expr(Expr::col((MEMBERS, "id")).count()).from(MEMBERS).and_where(Expr::col((MEMBERS, "lobby_id")).equals((LOBBIES, "id")));
        select.and_where(Expr::SubQuery(None, Box::new(count.into())).lt(Expr::col((LOBBIES, "max_players"))));
    }
    if let Some(before) = s.before {
        select.and_where(Expr::col((LOBBIES, "id")).lt(before));
    }
    select.order_by((LOBBIES, "id"), Order::Desc).limit(s.limit);
    select
}

pub(crate) struct NewLobby<'a> {
    pub(crate) code: &'a str,
    pub(crate) visibility: &'a str,
    pub(crate) host: i64,
    pub(crate) max_players: i64,
    pub(crate) now: i64,
}

pub(crate) fn insert_lobby(l: NewLobby<'_>) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(LOBBIES)
        .columns(["code", "visibility", "state", "host_id", "max_players", "created_at", "updated_at"])
        .values([l.code.into(), l.visibility.into(), OPEN.into(), l.host.into(), l.max_players.into(), l.now.into(), l.now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// What an update sets (absent: unchanged).
#[derive(Default)]
pub(crate) struct LobbyUpdate<'a> {
    pub(crate) visibility: Option<&'a str>,
    pub(crate) state: Option<&'a str>,
    pub(crate) max_players: Option<i64>,
    pub(crate) host: Option<i64>,
    pub(crate) code: Option<&'a str>,
}

pub(crate) fn update_lobby(lobby: i64, u: LobbyUpdate<'_>, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(LOBBIES).value("updated_at", now).and_where(Expr::col("id").eq(lobby));
    if let Some(visibility) = u.visibility {
        update.value("visibility", visibility);
    }
    if let Some(state) = u.state {
        update.value("state", state);
    }
    if let Some(max) = u.max_players {
        update.value("max_players", max);
    }
    if let Some(host) = u.host {
        update.value("host_id", host);
    }
    if let Some(code) = u.code {
        update.value("code", code);
    }
    update
}

pub(crate) fn set_chat_room(lobby: i64, room: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(LOBBIES).value("chat_room", room).and_where(Expr::col("id").eq(lobby));
    update
}

pub(crate) fn delete_lobby(lobby: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(LOBBIES).and_where(Expr::col("id").eq(lobby));
    delete
}

fn no_member_of_lobby() -> SelectStatement {
    let mut members = Query::select();
    members.column((MEMBERS, "id")).from(MEMBERS).and_where(Expr::col((MEMBERS, "lobby_id")).equals((LOBBIES, "id")));
    members
}

/// Lobbies without a member (left behind when the last member's account was deleted), at most
/// `limit`, with their chat rooms.
pub(crate) fn empty_lobbies(limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns([(LOBBIES, "id"), (LOBBIES, "chat_room")]).from(LOBBIES).and_where(Expr::not_exists(no_member_of_lobby())).limit(limit);
    select
}

/// Delete a lobby only while it has no member (a player who joined since the purge read it keeps
/// it).
pub(crate) fn delete_empty_lobby(lobby: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(LOBBIES).and_where(Expr::col((LOBBIES, "id")).eq(lobby)).and_where(Expr::not_exists(no_member_of_lobby()));
    delete
}

/// The lobbies that name one of these chat rooms.
#[cfg(feature = "chat")]
pub(crate) fn lobbies_with_rooms(rooms: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "chat_room"]).from(LOBBIES).and_where(Expr::col("chat_room").is_in(rooms.iter().copied()));
    select
}

/// Lobbies without a host (its account was deleted), at most `limit`.
pub(crate) fn hostless_lobbies(limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(LOBBIES).and_where(Expr::col("host_id").is_null()).limit(limit);
    select
}

// ---- members ------------------------------------------------------------------------------------

pub(crate) fn member(lobby: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("lobby_id").eq(lobby)).and_where(Expr::col("user_id").eq(user));
    select
}

/// A lobby's members in the order they joined.
pub(crate) fn members(lobby: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("lobby_id").eq(lobby)).order_by("id", Order::Asc);
    select
}

/// A player's memberships, oldest first.
pub(crate) fn memberships(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("user_id").eq(user)).order_by("id", Order::Asc);
    select
}

pub(crate) fn insert_member(lobby: i64, user: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(MEMBERS)
        .columns(["lobby_id", "user_id", "ready", "joined_at"])
        .values([lobby.into(), user.into(), 0i64.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

pub(crate) fn delete_member(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MEMBERS).and_where(Expr::col("id").eq(id));
    delete
}

pub(crate) fn set_ready(id: i64, ready: bool) -> UpdateStatement {
    let mut update = Query::update();
    update.table(MEMBERS).value("ready", i64::from(ready)).and_where(Expr::col("id").eq(id));
    update
}

/// Every member of a lobby not ready (a lobby back to `open`); runs under the lobby's lock.
pub(crate) fn reset_ready(lobby: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(MEMBERS).value("ready", 0i64).and_where(Expr::col("lobby_id").eq(lobby));
    update
}

/// How many members a lobby has.
pub(crate) fn count_members(lobby: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(MEMBERS).and_where(Expr::col("lobby_id").eq(lobby));
    select
}

/// The member counts of these lobbies (lobbies without members are absent).
pub(crate) fn member_counts(lobbies: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("lobby_id")
        .expr_as(Expr::col("id").count(), "n")
        .from(MEMBERS)
        .and_where(Expr::col("lobby_id").is_in(lobbies.iter().copied()))
        .group_by_col("lobby_id");
    select
}

/// How many lobbies a player is in.
pub(crate) fn count_memberships(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(MEMBERS).and_where(Expr::col("user_id").eq(user));
    select
}

// ---- metadata -----------------------------------------------------------------------------------

/// The metadata of these lobbies.
pub(crate) fn metadata_of(lobbies: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["lobby_id", "meta_key", "meta_value"]).from(METADATA).and_where(Expr::col("lobby_id").is_in(lobbies.iter().copied()));
    select
}

pub(crate) fn insert_meta(lobby: i64, key: &str, value: &str) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(METADATA).columns(["lobby_id", "meta_key", "meta_value"]).values([lobby.into(), key.into(), value.into()]).map_err(build)?;
    Ok(insert)
}

/// One lobby's metadata rows (with their ids, for changes by primary key).
pub(crate) fn meta_rows(lobby: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "meta_key", "meta_value"]).from(METADATA).and_where(Expr::col("lobby_id").eq(lobby));
    select
}

pub(crate) fn set_meta(id: i64, value: &str) -> UpdateStatement {
    let mut update = Query::update();
    update.table(METADATA).value("meta_value", value).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn delete_meta(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(METADATA).and_where(Expr::col("id").eq(id));
    delete
}

// ---- accounts -----------------------------------------------------------------------------------

/// The display names of accounts.
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::render_statement;

    #[test]
    fn statements_render() {
        let filters = vec![("mode".to_string(), "ranked".to_string())];
        let sql = render_statement(&search(Search { filters: &filters, hosts: None, include_full: false, before: Some(9), limit: 21 }), Dialect::Postgres);
        assert!(sql.contains(r#"EXISTS(SELECT "lobby_metadata"."id" FROM "lobby_metadata" WHERE "lobby_metadata"."lobby_id" = "lobbies"."id""#), "{sql}");
        assert!(
            sql.contains(
                r#"(SELECT COUNT("lobby_members"."id") FROM "lobby_members" WHERE "lobby_members"."lobby_id" = "lobbies"."id") < "lobbies"."max_players""#
            ),
            "{sql}"
        );
        assert!(
            sql.contains(r#""lobbies"."visibility" = 'public'"#)
                && sql.contains(r#""lobbies"."id" < 9"#)
                && sql.ends_with("ORDER BY \"lobbies\".\"id\" DESC LIMIT 21"),
            "{sql}"
        );
        let friends = render_statement(&search(Search { filters: &[], hosts: Some(&[4, 5]), include_full: true, before: None, limit: 5 }), Dialect::MySql);
        assert!(
            friends.contains("`lobbies`.`visibility` IN ('public', 'friends')")
                && friends.contains("`lobbies`.`host_id` IN (4, 5)")
                && !friends.contains("COUNT"),
            "{friends}"
        );
        assert!(render_statement(&lock_lobby(1, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(render_statement(&lock_user(1, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
        assert!(render_statement(&empty_lobbies(10), Dialect::Sqlite).contains("NOT EXISTS"));
        for dialect in [Dialect::MySql, Dialect::Postgres, Dialect::Sqlite] {
            let delete = render_statement(&delete_empty_lobby(4), dialect);
            assert!(delete.starts_with("DELETE FROM") && delete.contains("NOT EXISTS") && delete.contains("lobby_members"), "{delete}");
        }
        let update = render_statement(&update_lobby(3, LobbyUpdate { state: Some(IN_GAME), ..LobbyUpdate::default() }, 9), Dialect::Sqlite);
        assert!(update.contains("\"state\" = 'in_game'") && !update.contains("\"code\""), "{update}");
    }
}
