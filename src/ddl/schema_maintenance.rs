//! Keeps the `__pgsqlite_*` identifier metadata in step with DDL that changes
//! the shape of a table after `CREATE TABLE`.
//!
//! Before this module existed, only `CREATE TABLE` ever wrote metadata:
//!
//! * `ALTER TABLE ... ADD COLUMN` added a real SQLite column with no matching
//!   `__pgsqlite_schema` row, so the next startup saw a column "missing from
//!   metadata".
//! * `DROP TABLE` left every metadata row of the dropped table behind, so the
//!   next startup saw a whole table's worth of columns "missing from SQLite".
//! * `ALTER TABLE ... RENAME TO` left the metadata parked under the old name,
//!   producing both failures at once. This is the shape ORMs use to emulate an
//!   unsupported `ALTER` (`CREATE TABLE "__temp__x"; INSERT ... SELECT;
//!   DROP TABLE "x"; ALTER TABLE "__temp__x" RENAME TO "x"`).
//!
//! All of the above are drift, and the startup drift validator refuses to open
//! a drifted database, so each of them was a latent brick. Every entry point
//! normalizes identifiers via [`crate::utils::normalize_identifier`], so quoted
//! DDL (`DROP TABLE "users"`) is handled identically to unquoted DDL.

use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::Connection;
use tracing::debug;

use crate::utils::{normalize_identifier, quote_identifier};

/// Every `__pgsqlite_*` table that keys rows by a user identifier. Each of them
/// has a `table_name` column; all of them currently also have a `column_name`
/// column, but presence is probed at runtime so the list stays forgiving of
/// older databases created by earlier migration versions.
pub const IDENTIFIER_METADATA_TABLES: &[&str] = &[
    "__pgsqlite_schema",
    "__pgsqlite_string_constraints",
    "__pgsqlite_numeric_constraints",
    "__pgsqlite_array_types",
    "__pgsqlite_enum_usage",
    "__pgsqlite_fts_metadata",
    "__pgsqlite_datetime_cache",
];

/// A SQL identifier: either a double-quoted name (with `""` escapes) or a bare
/// word.
const IDENT: &str = r#"(?:"(?:[^"]|"")*"|[A-Za-z_][A-Za-z0-9_$]*)"#;

static DROP_TABLE_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(r#"(?is)^\s*DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?(.+?)(?:\s+(?:CASCADE|RESTRICT))?\s*;?\s*$"#).ok()
});

static ALTER_RENAME_TABLE_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?is)^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?({IDENT})\s+RENAME\s+TO\s+({IDENT})\s*;?\s*$"#
    ))
    .ok()
});

static ALTER_RENAME_COLUMN_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?is)^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?({IDENT})\s+RENAME\s+(?:COLUMN\s+)?({IDENT})\s+TO\s+({IDENT})\s*;?\s*$"#
    ))
    .ok()
});

static ALTER_ADD_COLUMN_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(&format!(
        // The type is optional: SQLite accepts `ALTER TABLE t ADD COLUMN c`
        // with no declared type, and that column still needs a metadata row or
        // it reads as drift on the next startup.
        r#"(?is)^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?({IDENT})\s+ADD\s+(?:COLUMN\s+)?(?:IF\s+NOT\s+EXISTS\s+)?({IDENT})(?:\s+(.+?))?\s*;?\s*$"#
    ))
    .ok()
});

static ALTER_DROP_COLUMN_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?is)^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?({IDENT})\s+DROP\s+(?:COLUMN\s+)?(?:IF\s+EXISTS\s+)?({IDENT})(?:\s+(?:CASCADE|RESTRICT))?\s*;?\s*$"#
    ))
    .ok()
});

/// Maintain `__pgsqlite_*` metadata after a DDL statement has been executed
/// successfully against SQLite.
///
/// Best-effort by design: metadata bookkeeping must never turn a DDL statement
/// the user already saw succeed into an error, so failures are logged and
/// swallowed. Non-DDL statements are rejected by a cheap prefix check before
/// any regex runs, because this is on the hot path for every executed
/// statement.
pub fn maintain_metadata_after_ddl(conn: &Connection, query: &str) {
    if !is_maintained_ddl(query) {
        return;
    }

    if let Err(e) = maintain_metadata_after_ddl_inner(conn, query) {
        debug!("Metadata maintenance failed for DDL statement: {e}");
    }
}

/// Cheap gate so the regexes never run for the overwhelming majority of
/// statements. Only `DROP TABLE` and `ALTER TABLE` change metadata-bearing
/// shape after creation.
fn is_maintained_ddl(query: &str) -> bool {
    starts_with_keyword(query, "DROP") || starts_with_keyword(query, "ALTER")
}

/// Case-insensitive check that the statement's first word is `keyword`.
fn starts_with_keyword(query: &str, keyword: &str) -> bool {
    query
        .trim_start()
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
}

fn maintain_metadata_after_ddl_inner(conn: &Connection, query: &str) -> rusqlite::Result<()> {
    if starts_with_keyword(query, "DROP") {
        if let Some(tables) = parse_dropped_tables(query) {
            for table in tables {
                delete_table_metadata(conn, &table)?;
            }
        }
        return Ok(());
    }

    // ALTER TABLE. Order matters only in that RENAME TO must be tried before
    // RENAME <column> TO, which it is by construction of the patterns.
    if let Some(re) = ALTER_RENAME_TABLE_RE.as_ref()
        && let Some(caps) = re.captures(query)
    {
        let old = normalize_identifier(caps.get(1).map_or("", |m| m.as_str()));
        let new = normalize_identifier(caps.get(2).map_or("", |m| m.as_str()));
        return rename_table_metadata(conn, &old, &new);
    }

    if let Some(re) = ALTER_RENAME_COLUMN_RE.as_ref()
        && let Some(caps) = re.captures(query)
    {
        let table = normalize_identifier(caps.get(1).map_or("", |m| m.as_str()));
        let old = normalize_identifier(caps.get(2).map_or("", |m| m.as_str()));
        let new = normalize_identifier(caps.get(3).map_or("", |m| m.as_str()));
        return rename_column_metadata(conn, &table, &old, &new);
    }

    if let Some(re) = ALTER_DROP_COLUMN_RE.as_ref()
        && let Some(caps) = re.captures(query)
    {
        let table = normalize_identifier(caps.get(1).map_or("", |m| m.as_str()));
        let column = normalize_identifier(caps.get(2).map_or("", |m| m.as_str()));
        return delete_column_metadata(conn, &table, &column);
    }

    if let Some(re) = ALTER_ADD_COLUMN_RE.as_ref()
        && let Some(caps) = re.captures(query)
    {
        let table_token = caps.get(1).map_or("", |m| m.as_str());
        let column_token = caps.get(2).map_or("", |m| m.as_str());
        let rest = caps.get(3).map_or("", |m| m.as_str());
        return add_column_metadata(conn, table_token, column_token, rest);
    }

    Ok(())
}

/// Split the table list of a `DROP TABLE a, "b"` statement into normalized
/// names. Returns `None` when the statement does not parse as DROP TABLE.
fn parse_dropped_tables(query: &str) -> Option<Vec<String>> {
    let re = DROP_TABLE_RE.as_ref()?;
    let caps = re.captures(query)?;
    let list = caps.get(1)?.as_str();

    let names: Vec<String> = list
        .split(',')
        .map(|name| normalize_identifier(name))
        .filter(|name| !name.is_empty())
        .collect();

    if names.is_empty() { None } else { Some(names) }
}

/// pgsqlite's own bookkeeping tables, which are never tracked as user tables.
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

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        [column],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Drop every metadata row belonging to `table`.
pub fn delete_table_metadata(conn: &Connection, table: &str) -> rusqlite::Result<()> {
    let table = normalize_identifier(table);

    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) {
            continue;
        }
        let removed = conn.execute(
            &format!("DELETE FROM {meta} WHERE table_name = ?1"),
            [&table],
        )?;
        if removed > 0 {
            debug!("Removed {removed} {meta} row(s) for dropped table '{table}'");
        }
    }

    Ok(())
}

/// Move every metadata row from `old` to `new`, discarding any rows already
/// parked under `new` (they describe a table that no longer exists - the ORM
/// table-rebuild pattern drops the original before renaming the replacement
/// into its place).
pub fn rename_table_metadata(conn: &Connection, old: &str, new: &str) -> rusqlite::Result<()> {
    if old == new {
        return Ok(());
    }

    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) {
            continue;
        }
        conn.execute(&format!("DELETE FROM {meta} WHERE table_name = ?1"), [new])?;
        let moved = conn.execute(
            &format!("UPDATE {meta} SET table_name = ?1 WHERE table_name = ?2"),
            [new, old],
        )?;
        if moved > 0 {
            debug!("Moved {moved} {meta} row(s) from table '{old}' to '{new}'");
        }
    }

    Ok(())
}

fn rename_column_metadata(
    conn: &Connection,
    table: &str,
    old: &str,
    new: &str,
) -> rusqlite::Result<()> {
    if old == new {
        return Ok(());
    }

    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) || !has_column(conn, meta, "column_name") {
            continue;
        }
        conn.execute(
            &format!("DELETE FROM {meta} WHERE table_name = ?1 AND column_name = ?2"),
            [table, new],
        )?;
        conn.execute(
            &format!("UPDATE {meta} SET column_name = ?1 WHERE table_name = ?2 AND column_name = ?3"),
            [new, table, old],
        )?;
    }

    Ok(())
}

fn delete_column_metadata(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<()> {
    for meta in IDENTIFIER_METADATA_TABLES {
        if !table_exists(conn, meta) || !has_column(conn, meta, "column_name") {
            continue;
        }
        conn.execute(
            &format!("DELETE FROM {meta} WHERE table_name = ?1 AND column_name = ?2"),
            [table, column],
        )?;
    }

    Ok(())
}

/// Record a `__pgsqlite_schema` row for a freshly added column.
///
/// `column_token` and `table_token` are the raw SQL tokens (possibly quoted);
/// `rest` is everything after the column name, i.e. the declared type plus any
/// constraints.
///
/// The PostgreSQL type is derived by running the column definition through the
/// regular `CREATE TABLE` translator, so `ALTER` and `CREATE` agree on type
/// naming. The SQLite type, in contrast, is read back from `PRAGMA table_info`
/// rather than predicted: `ALTER TABLE` statements are passed through to SQLite
/// untranslated, so the declared type SQLite actually recorded is the only
/// value that can never register as drift.
fn add_column_metadata(
    conn: &Connection,
    table_token: &str,
    column_token: &str,
    rest: &str,
) -> rusqlite::Result<()> {
    let table = normalize_identifier(table_token);
    let column = normalize_identifier(column_token);

    if table.is_empty() || column.is_empty() || is_internal_table(&table) {
        // pgsqlite's own bookkeeping tables are not user tables; registering
        // one of their columns would make the drift check start tracking the
        // metadata table itself.
        return Ok(());
    }

    let Some(sqlite_type) = declared_sqlite_type(conn, &table, &column) else {
        // The column is not actually there (statement was a no-op, or the
        // regex matched something that is not an ADD COLUMN). Recording
        // metadata for it would itself be drift.
        return Ok(());
    };

    let mapping = derive_type_mapping(conn, &table, column_token, rest);
    let pg_type = mapping
        .as_ref()
        .map(|m| m.pg_type.clone())
        .unwrap_or_else(|| fallback_pg_type(rest));

    crate::metadata::TypeMetadata::init(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![&table, &column, &pg_type, &sqlite_type],
    )?;
    debug!("Recorded metadata for added column {table}.{column} -> {pg_type} ({sqlite_type})");

    if let Some(mapping) = mapping
        && let Some(modifier) = mapping.type_modifier
    {
        store_constraint_metadata(conn, &table, &column, &mapping.pg_type, modifier)?;
    }

    Ok(())
}

/// Read the declared type SQLite recorded for a column, or `None` if the column
/// does not exist.
fn declared_sqlite_type(conn: &Connection, table: &str, column: &str) -> Option<String> {
    let quoted = quote_identifier(table);
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({quoted})"))
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .ok()?;

    for row in rows.flatten() {
        if row.0 == column {
            return Some(row.1);
        }
    }
    None
}

fn derive_type_mapping(
    conn: &Connection,
    table: &str,
    column_token: &str,
    rest: &str,
) -> Option<crate::metadata::TypeMapping> {
    let synthetic = format!(
        "CREATE TABLE {} ({} {})",
        quote_identifier(table),
        column_token,
        rest
    );

    let (_, mappings) =
        crate::translator::CreateTableTranslator::translate_with_connection(&synthetic, Some(conn))
            .ok()?;

    let column = normalize_identifier(column_token);
    mappings.get(&format!("{table}.{column}")).cloned()
}

/// Last-resort PostgreSQL type when the definition could not be translated:
/// the first token of the declared type, lower-cased.
fn fallback_pg_type(rest: &str) -> String {
    rest.split_whitespace()
        .next()
        .unwrap_or("text")
        .split('(')
        .next()
        .unwrap_or("text")
        .to_lowercase()
}

fn store_constraint_metadata(
    conn: &Connection,
    table: &str,
    column: &str,
    pg_type: &str,
    modifier: i32,
) -> rusqlite::Result<()> {
    let mut base_type = pg_type.split('(').next().unwrap_or(pg_type).trim();
    if let Some(bracket) = base_type.find('[') {
        base_type = &base_type[..bracket];
    }
    let base_type = base_type.to_lowercase();

    match base_type.as_str() {
        "varchar" | "char" | "character varying" | "character" | "nvarchar" => {
            if !table_exists(conn, "__pgsqlite_string_constraints") {
                return Ok(());
            }
            let is_char = base_type == "char" || base_type == "character";
            conn.execute(
                "INSERT OR REPLACE INTO __pgsqlite_string_constraints
                 (table_name, column_name, max_length, is_char_type) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![table, column, modifier, if is_char { 1 } else { 0 }],
            )?;
        }
        "numeric" | "decimal" => {
            if !table_exists(conn, "__pgsqlite_numeric_constraints") {
                return Ok(());
            }
            let tmp_typmod = modifier - 4; // Remove VARHDRSZ
            let precision = (tmp_typmod >> 16) & 0xFFFF;
            let scale = tmp_typmod & 0xFFFF;
            conn.execute(
                "INSERT OR REPLACE INTO __pgsqlite_numeric_constraints
                 (table_name, column_name, precision, scale) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![table, column, precision, scale],
            )?;
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_only_matches_alter_and_drop() {
        assert!(is_maintained_ddl("DROP TABLE \"users\""));
        assert!(is_maintained_ddl("  alter table users add column x int"));
        assert!(is_maintained_ddl("ALTER  TABLE users RENAME TO u"));
        assert!(!is_maintained_ddl("SELECT 1"));
        assert!(!is_maintained_ddl("INSERT OR REPLACE INTO __pgsqlite_schema ..."));
        assert!(!is_maintained_ddl("CREATE TABLE users (id int)"));
    }

    #[test]
    fn non_table_drops_are_parsed_as_nothing() {
        assert!(parse_dropped_tables("DROP INDEX idx_users_email").is_none());
        assert!(parse_dropped_tables("DROP VIEW v").is_none());
    }

    #[test]
    fn parses_quoted_and_multi_table_drops() {
        assert_eq!(
            parse_dropped_tables("DROP TABLE \"users\"").unwrap(),
            vec!["users".to_string()]
        );
        assert_eq!(
            parse_dropped_tables("DROP TABLE IF EXISTS \"a\", b CASCADE").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn internal_tables_are_recognized() {
        assert!(is_internal_table("__pgsqlite_schema"));
        assert!(!is_internal_table("users"));
    }

    #[test]
    fn fallback_pg_type_takes_leading_token() {
        assert_eq!(fallback_pg_type("varchar(3) NOT NULL"), "varchar");
        assert_eq!(fallback_pg_type("INTEGER"), "integer");
    }
}
