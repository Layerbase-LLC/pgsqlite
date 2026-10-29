//! Startup self-heal for tracked tables that are missing `__pgsqlite_schema`
//! rows for columns SQLite actually has.
//!
//! The startup drift validator reports two opposite conditions and hard-exits
//! on either. They are not equally severe:
//!
//! * **Metadata has a column SQLite lacks.** Something claimed a column exists
//!   that does not. That is a real inconsistency (an interrupted DDL, a
//!   half-applied rebuild) and refusing to open is the right answer. This
//!   module never touches that direction.
//! * **SQLite has a column metadata lacks.** The table is intact and complete;
//!   only pgsqlite's own bookkeeping is short a row. The runtime already copes
//!   with this: a table with NO metadata at all is served entirely by decltype
//!   inference (`PRAGMA table_info`). Refusing to open is therefore strictly
//!   worse than inferring the missing rows, because it fails closed on a
//!   database that would have worked.
//!
//! Two shipped defects produced exactly the second shape:
//!
//! * `ALTER TABLE ... ADD COLUMN` wrote no metadata at all before
//!   `v0.0.22-layerbase-7`.
//! * `CREATE TABLE` skipped any column whose name began with a constraint
//!   keyword (`check_in`, `checked_out`, `unique_id`) before
//!   `v0.0.22-layerbase-8`.
//!
//! Both bricked customer databases on their next wake. The write paths are
//! fixed, but databases created by an older binary still carry the gaps, so
//! this repair runs at startup before the drift check. It is idempotent, only
//! ever ADDS rows, and only for a column that demonstrably exists in the table
//! right now.
//!
//! The inferred row is deliberately conservative:
//!
//! * `sqlite_type` is copied verbatim from `PRAGMA table_info`, so the row can
//!   never itself register as a type mismatch.
//! * `pg_type` is derived by running the declared type back through the regular
//!   `CREATE TABLE` translator, so a healed column is typed exactly as the same
//!   column would have been had the writer not skipped it, with a SQLite-type
//!   mapping as the last resort.

use rusqlite::Connection;
use tracing::warn;

use crate::translator::CreateTableTranslator;
use crate::utils::{normalize_identifier, quote_identifier};

/// Insert `__pgsqlite_schema` rows for columns that exist in SQLite but have no
/// metadata, for tables that are already tracked. Returns the number of rows
/// written.
///
/// Only tracked tables are considered, because those are the only tables the
/// drift check looks at; a table with no metadata row at all is served by
/// inference and must stay that way.
pub fn backfill_missing_column_metadata(conn: &Connection) -> rusqlite::Result<usize> {
    if !table_exists(conn, "__pgsqlite_schema") {
        return Ok(0);
    }

    let mut healed = 0usize;
    for table in tracked_tables(conn)? {
        if is_internal_table(&table) || !table_exists(conn, &table) {
            // A missing table is the other direction (orphaned metadata) and is
            // deliberately left to fail loudly.
            continue;
        }

        let known = metadata_columns(conn, &table)?;
        for (column, sqlite_type) in sqlite_columns(conn, &table)? {
            if known.iter().any(|k| k == &column) {
                continue;
            }

            let pg_type = infer_pg_type(conn, &table, &column, &sqlite_type);
            conn.execute(
                "INSERT OR IGNORE INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![&table, &column, &pg_type, &sqlite_type],
            )?;
            warn!(
                "Backfilled missing pgsqlite metadata for {table}.{column} \
                 (inferred pg_type {pg_type} from declared SQLite type '{sqlite_type}'). \
                 This column was created by a build that failed to record it."
            );
            healed += 1;
        }
    }

    Ok(healed)
}

fn tracked_tables(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT table_name FROM __pgsqlite_schema")?;
    let raw = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut tables: Vec<String> = raw.iter().map(|name| normalize_identifier(name)).collect();
    tables.sort();
    tables.dedup();
    Ok(tables)
}

fn metadata_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT column_name FROM __pgsqlite_schema WHERE table_name = ?1 OR table_name = ?2",
    )?;
    let rows = stmt.query_map([table, &quote_identifier(table)], |row| {
        Ok(normalize_identifier(&row.get::<_, String>(0)?))
    })?;
    rows.collect()
}

fn sqlite_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_identifier(table)))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    rows.collect()
}

/// Derive the PostgreSQL type for a column from the type SQLite declared for
/// it, preferring the regular `CREATE TABLE` translator so a healed column is
/// typed the same way a freshly created one would be.
fn infer_pg_type(conn: &Connection, table: &str, column: &str, sqlite_type: &str) -> String {
    if sqlite_type.trim().is_empty() {
        return "text".to_string();
    }

    let synthetic = format!(
        "CREATE TABLE {} ({} {})",
        quote_identifier(table),
        quote_identifier(column),
        sqlite_type
    );
    if let Ok((_, mappings)) =
        CreateTableTranslator::translate_with_connection(&synthetic, Some(conn))
        && let Some(mapping) = mappings.get(&format!("{table}.{column}"))
    {
        return mapping.pg_type.clone();
    }

    fallback_pg_type(sqlite_type)
}

/// Last-resort mapping, mirroring the inference the runtime already applies to
/// a table with no metadata at all.
fn fallback_pg_type(sqlite_type: &str) -> String {
    let upper = sqlite_type.to_uppercase();
    if upper.contains("INT") {
        "int4"
    } else if upper.contains("BOOL") {
        "bool"
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "float8"
    } else if upper.contains("NUMERIC") || upper.contains("DECIMAL") {
        "numeric"
    } else if upper.contains("BLOB") {
        "bytea"
    } else {
        "text"
    }
    .to_string()
}

fn is_internal_table(table: &str) -> bool {
    table.starts_with("__pgsqlite_")
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}
