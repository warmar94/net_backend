//! The storage module's table, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `storage_objects` | one row per object: owner (`auth_users`, cascade), collection, key, the value's JSON bytes, version, write access, size, times |
//!
//! The unique index `(user_id, collection, object_key)` is the object's identity and serves the
//! listings (ordered by key).

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The table.
pub(crate) const OBJECTS: &str = "storage_objects";

fn objects(dialect: Dialect) -> TableCreateStatement {
    let mut t = schema::create_table(OBJECTS);
    let mut fk = ForeignKey::create();
    fk.name("storage_objects_user_id_fk").from(OBJECTS, "user_id").to("auth_users", "id").on_delete(ForeignKeyAction::Cascade);
    let mut write = ColumnDef::new("write_access");
    write.string_len(16).not_null();
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("collection", 128))
        .col(schema::string("object_key", 128))
        .col(schema::bytes("value", dialect))
        .col(schema::big_int("version"))
        .col(write)
        .col(schema::big_int("size_bytes"))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk);
    t
}

/// The migrations of the storage module for a dialect (versions `2026100200nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let mut unique = Index::create();
    unique.name("storage_objects_uq").table(OBJECTS).col("user_id").col("collection").col("object_key").unique();
    let steps: Vec<(&str, String)> =
        vec![("create_storage_objects", render_table(&objects(dialect), dialect)), ("unique_storage_objects", render_index(&unique, dialect))];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1002_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 2);
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`value` LONGBLOB NOT NULL") && mysql[0].sql.contains("ON DELETE CASCADE"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[1].sql.starts_with("CREATE UNIQUE INDEX \"storage_objects_uq\""));
    }
}
