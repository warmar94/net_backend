//! The friends module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the friends suite on SQLite locally and on MySQL / PostgreSQL (env-gated).
//!
//! **Writes between two players** take both account locks first, the lower id first (the
//! `auth_users` rows: MySQL `FOR UPDATE`, PostgreSQL `FOR NO KEY UPDATE`, SQLite's write
//! transaction), then read the two rows and change them by primary key: the reads stay true until
//! the commit and no statement takes a gap lock.

use sea_query::{Cond, DeleteStatement, Expr, ExprTrait, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::{LINKS, PRESENCE, PROFILES, SETTINGS};
use crate::db::{DbError, Dialect};

pub(crate) const FRIEND: &str = "friend";
pub(crate) const SENT: &str = "sent";
pub(crate) const RECEIVED: &str = "received";
pub(crate) const BLOCKED: &str = "blocked";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

/// One relation row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct LinkRow {
    pub(crate) id: i64,
    pub(crate) other_id: i64,
    pub(crate) state: String,
    pub(crate) updated_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct OtherRow {
    pub(crate) other_id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UserRow {
    pub(crate) user_id: i64,
}

/// A player and one of its friends.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct FriendOfRow {
    pub(crate) user_id: i64,
    pub(crate) other_id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

/// A player's friend code and online times.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct ProfileRow {
    pub(crate) user_id: i64,
    pub(crate) code: String,
    pub(crate) online_until: Option<i64>,
    pub(crate) last_seen_at: Option<i64>,
}

const LINK_COLUMNS: [&str; 4] = ["id", "other_id", "state", "updated_at"];
const PROFILE_COLUMNS: [&str; 4] = ["user_id", "code", "online_until", "last_seen_at"];

/// The account row, locked for the transaction (`None` if the account does not exist).
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// `user`'s row about `other`.
pub(crate) fn link(user: i64, other: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LINK_COLUMNS).from(LINKS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("other_id").eq(other));
    select
}

pub(crate) fn insert_link(user: i64, other: i64, state: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(LINKS)
        .columns(["user_id", "other_id", "state", "created_at", "updated_at"])
        .values([user.into(), other.into(), state.into(), now.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

pub(crate) fn set_state(id: i64, state: &str, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(LINKS).value("state", state).value("updated_at", now).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn delete_link(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(LINKS).and_where(Expr::col("id").eq(id));
    delete
}

/// How many rows of this state a player has.
pub(crate) fn count(user: i64, state: &str) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(LINKS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("state").eq(state));
    select
}

/// A page of a player's rows of one state, newest first, before the row id `before`.
pub(crate) fn page(user: i64, state: &str, before: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(LINK_COLUMNS).from(LINKS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("state").eq(state));
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

/// Up to `limit` friends of a player.
pub(crate) fn friend_ids(user: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("other_id").from(LINKS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("state").eq(FRIEND)).limit(limit);
    select
}

/// The display names of accounts.
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

/// Up to two accounts with exactly this display name.
pub(crate) fn by_name(name: &str) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("display_name").eq(name)).order_by("id", Order::Asc).limit(2);
    select
}

pub(crate) fn profile(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(PROFILE_COLUMNS).from(PROFILES).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn profiles(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(PROFILE_COLUMNS).from(PROFILES).and_where(Expr::col("user_id").is_in(users.iter().copied()));
    select
}

/// The player with this friend code.
pub(crate) fn by_code(code: &str) -> SelectStatement {
    let mut select = Query::select();
    select.column("user_id").from(PROFILES).and_where(Expr::col("code").eq(code));
    select
}

pub(crate) fn insert_profile(user: i64, code: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(PROFILES).columns(["user_id", "code", "created_at"]).values([user.into(), code.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn set_code(user: i64, code: &str) -> UpdateStatement {
    let mut update = Query::update();
    update.table(PROFILES).value("code", code).and_where(Expr::col("user_id").eq(user));
    update
}

// ---- Steam IDs and settings ---------------------------------------------------------------------

const IDENTITIES: &str = "auth_identities";
const USERS: &str = "auth_users";

/// An account found by its linked Steam account.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SteamRow {
    pub(crate) user_id: i64,
    pub(crate) subject: String,
    pub(crate) display_name: Option<String>,
}

/// A relation row between the caller and a found account (either direction).
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct PairRow {
    pub(crate) user_id: i64,
    pub(crate) other_id: i64,
    pub(crate) state: String,
}

/// A player's settings row.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SettingsRow {
    pub(crate) steam_hidden_at: Option<i64>,
}

/// The SteamID64 (`subject`) of a player's linked Steam account.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SubjectRow {
    pub(crate) subject: String,
}

/// The player's linked Steam account, if any.
pub(crate) fn own_steam(user: i64, provider: &str) -> SelectStatement {
    let mut select = Query::select();
    select.column("subject").from(IDENTITIES).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("provider").eq(provider)).limit(1);
    select
}

/// The accounts other than `caller` whose linked Steam account is one of `subjects`: not banned at
/// `now`, not hidden from Steam ID lookups (unique `(provider, subject)` index).
pub(crate) fn steam_accounts(provider: &str, subjects: &[String], caller: i64, now: i64) -> SelectStatement {
    let mut select = Query::select();
    select
        .column((IDENTITIES, "user_id"))
        .column((IDENTITIES, "subject"))
        .column((USERS, "display_name"))
        .from(IDENTITIES)
        .inner_join(USERS, Expr::col((USERS, "id")).equals((IDENTITIES, "user_id")))
        .left_join(SETTINGS, Expr::col((SETTINGS, "user_id")).equals((IDENTITIES, "user_id")))
        .and_where(Expr::col((IDENTITIES, "provider")).eq(provider))
        .and_where(Expr::col((IDENTITIES, "subject")).is_in(subjects.iter().cloned()))
        .and_where(Expr::col((IDENTITIES, "user_id")).ne(caller))
        .and_where(
            Expr::col((USERS, "banned_at")).is_null().or(Expr::col((USERS, "banned_until")).is_not_null().and(Expr::col((USERS, "banned_until")).lte(now))),
        )
        .and_where(Expr::col((SETTINGS, "steam_hidden_at")).is_null());
    select
}

/// The relation rows between `user` and any of `others`, in both directions.
pub(crate) fn links_between(user: i64, others: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["user_id", "other_id", "state"]).from(LINKS).cond_where(
        Cond::any()
            .add(Cond::all().add(Expr::col("user_id").eq(user)).add(Expr::col("other_id").is_in(others.iter().copied())))
            .add(Cond::all().add(Expr::col("other_id").eq(user)).add(Expr::col("user_id").is_in(others.iter().copied()))),
    );
    select
}

pub(crate) fn settings(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.column("steam_hidden_at").from(SETTINGS).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn insert_settings(user: i64, hidden_at: Option<i64>, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(SETTINGS).columns(["user_id", "steam_hidden_at", "updated_at"]).values([user.into(), hidden_at.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn update_settings(user: i64, hidden_at: Option<i64>, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(SETTINGS).value("steam_hidden_at", hidden_at).value("updated_at", now).and_where(Expr::col("user_id").eq(user));
    update
}

/// This instance holds a connection of `user` until `until` (the row exists).
pub(crate) fn touch_presence(user: i64, instance: i64, until: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(PRESENCE).value("online_until", until).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("instance_id").eq(instance));
    update
}

/// This instance holds a connection of `user` until `until` (a new row).
pub(crate) fn insert_presence(user: i64, instance: i64, until: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(PRESENCE).columns(["user_id", "instance_id", "online_until"]).values([user.into(), instance.into(), until.into()]).map_err(build)?;
    Ok(insert)
}

/// This instance still holds connections of these players: their rows count until `until`.
pub(crate) fn refresh_presence(users: &[i64], instance: i64, until: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(PRESENCE)
        .value("online_until", until)
        .and_where(Expr::col("instance_id").eq(instance))
        .and_where(Expr::col("user_id").is_in(users.iter().copied()));
    update
}

/// This instance holds a connection of each of these players until `until` (new rows; one statement).
pub(crate) fn insert_presences(users: &[i64], instance: i64, until: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(PRESENCE).columns(["user_id", "instance_id", "online_until"]);
    for user in users {
        insert.values([(*user).into(), instance.into(), until.into()]).map_err(build)?;
    }
    Ok(insert)
}

/// This instance no longer holds a connection of these players.
pub(crate) fn delete_presences(users: &[i64], instance: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(PRESENCE).and_where(Expr::col("instance_id").eq(instance)).and_where(Expr::col("user_id").is_in(users.iter().copied()));
    delete
}

/// Which of these players another instance holds a connection of right now (rows still counting).
pub(crate) fn held_elsewhere(users: &[i64], instance: i64, now: i64) -> SelectStatement {
    let mut select = Query::select();
    select
        .distinct()
        .column("user_id")
        .from(PRESENCE)
        .and_where(Expr::col("user_id").is_in(users.iter().copied()))
        .and_where(Expr::col("instance_id").ne(instance))
        .and_where(Expr::col("online_until").gt(now));
    select
}

/// The friends of these players (`user_id` = the player, `other_id` = a friend).
pub(crate) fn friends_of_many(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["user_id", "other_id"]).from(LINKS).and_where(Expr::col("user_id").is_in(users.iter().copied())).and_where(Expr::col("state").eq(FRIEND));
    select
}

/// Profiles (with friend codes) for these players, in one statement.
pub(crate) fn insert_profiles(rows: &[(i64, String)], now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(PROFILES).columns(["user_id", "code", "created_at"]);
    for (user, code) in rows {
        insert.values([(*user).into(), code.as_str().into(), now.into()]).map_err(build)?;
    }
    Ok(insert)
}

/// Rows of instances that stopped without removing them (no longer counting since `before`).
pub(crate) fn purge_presence(before: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(PRESENCE).and_where(Expr::col("online_until").lt(before));
    delete
}

/// These players are seen now and count as online until `until` (`until` = now: offline).
pub(crate) fn seen(users: &[i64], until: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(PROFILES).value("online_until", until).value("last_seen_at", now).and_where(Expr::col("user_id").is_in(users.iter().copied()));
    update
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::render_statement;

    #[test]
    fn statements_render() {
        let page = render_statement(&page(4, FRIEND, Some(90), 21), Dialect::Postgres);
        assert!(page.contains("\"state\" = 'friend'") && page.contains("\"id\" < 90") && page.contains("ORDER BY \"id\" DESC"), "{page}");
        assert!(render_statement(&lock_user(1, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(render_statement(&lock_user(1, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
        let name = render_statement(&by_name("Ada"), Dialect::MySql);
        assert!(name.contains("`display_name` = 'Ada'") && name.ends_with("LIMIT 2"), "{name}");
        let seen = render_statement(&seen(&[1, 2], 10, 5), Dialect::Sqlite);
        assert!(seen.contains("\"online_until\" = 10") && seen.contains("IN (1, 2)"), "{seen}");
        let other = render_statement(&held_elsewhere(&[3], 77, 5), Dialect::MySql);
        assert!(other.contains("`instance_id` <> 77") && other.contains("`online_until` > 5") && other.contains("`user_id` IN (3)"), "{other}");
        let held = render_statement(&held_elsewhere(&[3, 4], 77, 5), Dialect::Postgres);
        assert!(held.starts_with("SELECT DISTINCT \"user_id\"") && held.contains("\"instance_id\" <> 77") && held.contains("IN (3, 4)"), "{held}");
        let rows = render_statement(&insert_presences(&[3, 4], 77, 9).expect("insert"), Dialect::MySql);
        assert!(rows.contains("VALUES (3, 77, 9), (4, 77, 9)"), "{rows}");
        let gone = render_statement(&delete_presences(&[3, 4], 77), Dialect::Sqlite);
        assert!(gone.contains("\"instance_id\" = 77") && gone.contains("IN (3, 4)"), "{gone}");
        let friends = render_statement(&friends_of_many(&[3, 4]), Dialect::Postgres);
        assert!(friends.contains("\"user_id\" IN (3, 4)") && friends.contains("\"state\" = 'friend'"), "{friends}");
        let profiles = render_statement(&insert_profiles(&[(3, "AAAA".into()), (4, "BBBB".into())], 9).expect("insert"), Dialect::Postgres);
        assert!(profiles.contains("(3, 'AAAA', 9), (4, 'BBBB', 9)"), "{profiles}");
        let refresh = render_statement(&refresh_presence(&[1, 2], 77, 9), Dialect::Postgres);
        assert!(refresh.contains("\"instance_id\" = 77") && refresh.contains("IN (1, 2)"), "{refresh}");
        let steam = render_statement(&steam_accounts("steam", &["76561201960265729".into()], 4, 9), Dialect::Postgres);
        for part in [
            "INNER JOIN \"auth_users\"",
            "LEFT JOIN \"friends_settings\"",
            "\"subject\" IN ('76561201960265729')",
            "\"auth_identities\".\"user_id\" <> 4",
            "\"banned_until\" <= 9",
            "\"steam_hidden_at\" IS NULL",
        ] {
            assert!(steam.contains(part), "{part}: {steam}");
        }
        let pairs = render_statement(&links_between(4, &[7, 8]), Dialect::MySql);
        assert!(pairs.contains("`user_id` = 4 AND `other_id` IN (7, 8)") && pairs.contains("`other_id` = 4 AND `user_id` IN (7, 8)"), "{pairs}");
    }
}
