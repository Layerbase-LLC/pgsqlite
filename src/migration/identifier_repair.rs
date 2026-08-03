//! Startup self-heal for `__pgsqlite_*` metadata poisoned with quoted
//! identifiers.
//!
//! pgsqlite releases up to and including `v0.0.22-layerbase-6` copied the raw
//! SQL token into the metadata tables, so a client that quotes its identifiers
//! (`CREATE TABLE "themes" ("created_at" datetime)`) produced rows whose
//! `column_name` literally contained the double quotes. `PRAGMA table_info`
//! reports the unquoted name, so the startup drift validator saw every such
//! column as simultaneously missing from SQLite and missing from metadata and
//! refused to open the database - a permanent brick on the next wake.
//!
//! The write paths are fixed, but databases created by an older binary still
//! carry the poisoned rows, and more can be created in the window before the
//! fixed binary is everywhere. This repair runs unconditionally at startup
//! before the drift check, is idempotent, and touches only rows that are
//! actually quoted.

use rusqlite::Connection;
use tracing::info;

use crate::ddl::schema_maintenance::IDENTIFIER_METADATA_TABLES;
use crate::utils::{is_quoted_identifier, normalize_identifier};

/// Columns that hold a user identifier and therefore may have been poisoned.
const IDENTIFIER_COLUMNS: &[&str] = &["table_name", "column_name"];

/// Rewrite quoted identifier values in the `__pgsqlite_*` metadata tables to
/// their normalized form. Returns the number of rows changed (repaired plus
/// discarded duplicates).
///
/// When the normalized row already exists - which happens if a table was
/// written once by a quoting client and once by a non-quoting one - the clean
/// row wins and the poisoned duplicate is deleted.
pub fn repair_quoted_identifiers(conn: &Connection) -> rusqlite::Result<usize> {
    let mut total = 0usize;

    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) {
            continue;
        }

        let columns: Vec<&str> = IDENTIFIER_COLUMNS
            .iter()
            .copied()
            .filter(|col| has_column(conn, meta, col))
            .collect();

        if columns.is_empty() {
            continue;
        }

        total += repair_table(conn, meta, &columns)?;
    }

    if total > 0 {
        info!(
            "Repaired {total} pgsqlite metadata row(s) that stored quoted identifiers; \
             the schema drift check would otherwise refuse to open this database"
        );
    }

    Ok(total)
}

fn repair_table(conn: &Connection, meta: &str, columns: &[&str]) -> rusqlite::Result<usize> {
    let selected = columns.join(", ");
    let predicate = columns
        .iter()
        .map(|col| format!("{col} LIKE '\"%\"'"))
        .collect::<Vec<_>>()
        .join(" OR ");

    let candidates: Vec<(i64, Vec<String>)> = {
        let mut stmt =
            conn.prepare(&format!("SELECT rowid, {selected} FROM {meta} WHERE {predicate}"))?;
        let rows = stmt.query_map([], |row| {
            let rowid: i64 = row.get(0)?;
            let mut values = Vec::with_capacity(columns.len());
            for idx in 0..columns.len() {
                values.push(row.get::<_, String>(idx + 1)?);
            }
            Ok((rowid, values))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut repaired = 0usize;

    for (rowid, values) in candidates {
        let normalized: Vec<String> = values.iter().map(|v| normalize_identifier(v)).collect();

        if normalized == values {
            // LIKE matched but the value was not actually a quoted identifier.
            continue;
        }

        let assignments = columns
            .iter()
            .enumerate()
            .map(|(idx, col)| format!("{col} = ?{}", idx + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let mut params: Vec<&dyn rusqlite::ToSql> =
            normalized.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        params.push(&rowid);

        let update = format!(
            "UPDATE {meta} SET {assignments} WHERE rowid = ?{}",
            columns.len() + 1
        );

        match conn.execute(&update, params.as_slice()) {
            Ok(_) => repaired += 1,
            Err(_) => {
                // A clean row for this identifier already exists and owns the
                // primary key. Keep it and discard the poisoned duplicate.
                conn.execute(&format!("DELETE FROM {meta} WHERE rowid = ?1"), [rowid])?;
                repaired += 1;
            }
        }
    }

    Ok(repaired)
}

/// True when any `__pgsqlite_*` metadata row still stores a quoted identifier.
/// Exposed for tests and diagnostics.
pub fn has_quoted_identifiers(conn: &Connection) -> bool {
    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) {
            continue;
        }
        for col in IDENTIFIER_COLUMNS {
            if !has_column(conn, meta, col) {
                continue;
            }
            let found: Vec<String> = conn
                .prepare(&format!("SELECT {col} FROM {meta}"))
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap_or_default();

            if found.iter().any(|value| is_quoted_identifier(value)) {
                return true;
            }
        }
    }
    false
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

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        [column],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}
