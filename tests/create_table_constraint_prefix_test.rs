//! Regression tests for the 2026-08-24 production brick.
//!
//! `CreateTableTranslator::translate_column_definition` decided whether a
//! comma-separated `CREATE TABLE` element was a table-level constraint with a
//! bare prefix test:
//!
//! ```text
//! column_def.to_uppercase().starts_with("CHECK")
//! ```
//!
//! There is no token boundary in that test, so a perfectly ordinary column
//! whose NAME merely begins with a constraint keyword (`check_in`,
//! `checked_out`, `checked_in_at`, `unique_id`, `constraint_name`) was treated
//! as a constraint: the raw text was passed through to SQLite, which created
//! the column, while the `__pgsqlite_schema` row was never written.
//!
//! Runtime never noticed (metadata misses fall back to decltype inference), but
//! the startup drift validator reports "Columns in SQLite but missing from
//! metadata" and hard-exits, so the database was wired to fail on its next
//! wake. Three customer databases hit this on prod.
//!
//! The DDL in these tests is copied verbatim from the affected databases.

use pgsqlite::schema_drift::SchemaDriftDetector;
use pgsqlite::session::DbHandler;
use pgsqlite::translator::CreateTableTranslator;
use rusqlite::Connection;
use uuid::Uuid;

/// The real `attendance_logs` DDL. 9 columns; `check_in` and `check_out` sit in
/// the middle of the list, so this is provably the CREATE path and not ALTER.
const ATTENDANCE_LOGS_DDL: &str = "CREATE TABLE attendance_logs (id TEXT PRIMARY KEY, org_id TEXT NOT NULL, employee_id TEXT NOT NULL, attendance_date INTEGER NOT NULL, status TEXT, check_in INTEGER, check_out INTEGER, remarks TEXT, created_at INTEGER NOT NULL)";

/// The real `checkins` DDL. 7 columns; `checked_in_at` was the missing one.
const CHECKINS_DDL: &str = "CREATE TABLE checkins (id INTEGER PRIMARY KEY AUTOINCREMENT, schedule_id INTEGER DEFAULT 0, student_id INTEGER DEFAULT 0, status TEXT DEFAULT 'present', checked_in_at TEXT DEFAULT '', notes TEXT DEFAULT '', created_at TEXT DEFAULT '')";

/// The real `keys` DDL. 10 columns; `checked_out` was the missing one. Note the
/// run of spaces before `BOOL`, and the genuine table-level PRIMARY KEY and
/// FOREIGN KEY clauses that must still be recognised as constraints.
const KEYS_DDL: &str = "CREATE TABLE keys (keynumber TEXT, keyname TEXT, quantity TEXT, drawer TEXT, drawer_row TEXT, drawer_column TEXT, checked_out     BOOL, building_to TEXT, holder TEXT, university TEXT, PRIMARY KEY (keynumber, keyname), FOREIGN KEY (holder) REFERENCES keyholder(umid), FOREIGN KEY (university) REFERENCES university(name))";

fn mapping_columns(sql: &str, table: &str) -> Vec<String> {
    let (_sqlite_sql, mappings) = CreateTableTranslator::translate(sql).unwrap();
    let prefix = format!("{table}.");
    let mut columns: Vec<String> = mappings
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
        .collect();
    columns.sort();
    columns
}

fn metadata_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT column_name FROM __pgsqlite_schema WHERE table_name = ?1 ORDER BY column_name",
        )
        .unwrap();
    stmt.query_map([table], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. The three production DDLs must map every column
// ---------------------------------------------------------------------------

#[test]
fn attendance_logs_maps_every_column() {
    assert_eq!(
        mapping_columns(ATTENDANCE_LOGS_DDL, "attendance_logs"),
        vec![
            "attendance_date",
            "check_in",
            "check_out",
            "created_at",
            "employee_id",
            "id",
            "org_id",
            "remarks",
            "status",
        ],
        "check_in / check_out must not be mistaken for a CHECK constraint"
    );
}

#[test]
fn checkins_maps_every_column() {
    assert_eq!(
        mapping_columns(CHECKINS_DDL, "checkins"),
        vec![
            "checked_in_at",
            "created_at",
            "id",
            "notes",
            "schedule_id",
            "status",
            "student_id",
        ],
        "checked_in_at must not be mistaken for a CHECK constraint"
    );
}

#[test]
fn keys_maps_every_column() {
    assert_eq!(
        mapping_columns(KEYS_DDL, "keys"),
        vec![
            "building_to",
            "checked_out",
            "drawer",
            "drawer_column",
            "drawer_row",
            "holder",
            "keyname",
            "keynumber",
            "quantity",
            "university",
        ],
        "checked_out must not be mistaken for a CHECK constraint"
    );
}

// ---------------------------------------------------------------------------
// 2. Every constraint keyword is affected, not just CHECK
// ---------------------------------------------------------------------------

#[test]
fn columns_named_after_constraint_keywords_are_still_columns() {
    let sql = "CREATE TABLE t (id INTEGER, check_in INTEGER, checked BOOL, checks TEXT, unique_id TEXT, uniques TEXT, constraint_name TEXT, primary_email TEXT, foreign_id INTEGER)";
    assert_eq!(
        mapping_columns(sql, "t"),
        vec![
            "check_in",
            "checked",
            "checks",
            "constraint_name",
            "foreign_id",
            "id",
            "primary_email",
            "unique_id",
            "uniques",
        ]
    );
}

#[test]
fn extra_whitespace_before_the_type_is_not_the_trigger() {
    // The prod `keys` DDL had a run of spaces before BOOL; a same-shaped column
    // with a name that does NOT start with a keyword always worked, which is
    // what rules the whitespace out as the cause.
    let sql = "CREATE TABLE t (id INTEGER, other_out     BOOL, checked_out     BOOL)";
    assert_eq!(mapping_columns(sql, "t"), vec!["checked_out", "id", "other_out"]);
}

#[test]
fn quoted_columns_were_never_affected() {
    // Quoting shifts the first character to `"`, so an ORM that quotes its
    // identifiers never hit this. Pin it so the fix does not regress the
    // quoted path either.
    let sql = r#"CREATE TABLE "t" ("id" INTEGER, "check_in" INTEGER)"#;
    assert_eq!(mapping_columns(sql, "t"), vec!["check_in", "id"]);
}

#[test]
fn if_not_exists_form_maps_every_column() {
    let sql = "CREATE TABLE IF NOT EXISTS t (id INTEGER, check_in INTEGER)";
    assert_eq!(mapping_columns(sql, "t"), vec!["check_in", "id"]);
}

// ---------------------------------------------------------------------------
// 3. Real table-level constraints must still be recognised
// ---------------------------------------------------------------------------

fn is_skipped_as_constraint(element: &str) -> bool {
    let sql = format!("CREATE TABLE t (id INTEGER, {element})");
    let (_sqlite_sql, mappings) = CreateTableTranslator::translate(&sql).unwrap();
    // Only `id` mapped means the second element was treated as a constraint.
    mappings.len() == 1 && mappings.contains_key("t.id")
}

#[test]
fn genuine_table_constraints_are_still_skipped() {
    for element in [
        "CHECK (id > 0)",
        "CHECK(id > 0)",
        "check (id > 0)",
        "UNIQUE (id)",
        "UNIQUE(id)",
        "CONSTRAINT chk_id CHECK (id > 0)",
        "PRIMARY KEY (id)",
        "PRIMARY KEY(id)",
        "FOREIGN KEY (id) REFERENCES other(id)",
        "FOREIGN KEY(id) REFERENCES other(id)",
    ] {
        assert!(
            is_skipped_as_constraint(element),
            "table constraint `{element}` must not be recorded as a column"
        );
    }
}

#[test]
fn translated_sql_still_carries_the_table_constraints() {
    let (sqlite_sql, _mappings) = CreateTableTranslator::translate(KEYS_DDL).unwrap();
    assert!(
        sqlite_sql.contains("PRIMARY KEY (keynumber, keyname)"),
        "table PRIMARY KEY lost: {sqlite_sql}"
    );
    assert!(
        sqlite_sql.contains("FOREIGN KEY (holder) REFERENCES keyholder(umid)"),
        "table FOREIGN KEY lost: {sqlite_sql}"
    );
}

// ---------------------------------------------------------------------------
// 4. End to end: create, then wake. This is the brick itself.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attendance_logs_survives_a_restart() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("attendance.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(ATTENDANCE_LOGS_DDL).await.unwrap();
    }

    {
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            metadata_columns(&conn, "attendance_logs").contains(&"check_in".to_string()),
            "check_in must have a metadata row"
        );
    }

    // The wake path: re-opening runs the drift check, which used to hard-exit.
    DbHandler::new(path).expect("re-opening must not report drift");

    let conn = Connection::open(&db_path).unwrap();
    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
}

#[tokio::test]
async fn all_three_production_tables_survive_a_restart() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("prod_shapes.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(ATTENDANCE_LOGS_DDL).await.unwrap();
        db.execute(CHECKINS_DDL).await.unwrap();
        db.execute("CREATE TABLE keyholder (umid TEXT PRIMARY KEY, last_name TEXT)")
            .await
            .unwrap();
        db.execute("CREATE TABLE university (name TEXT PRIMARY KEY, building TEXT)")
            .await
            .unwrap();
        db.execute(KEYS_DDL).await.unwrap();
    }

    DbHandler::new(path).expect("re-opening must not report drift");

    let conn = Connection::open(&db_path).unwrap();
    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());

    assert!(metadata_columns(&conn, "checkins").contains(&"checked_in_at".to_string()));
    assert!(metadata_columns(&conn, "keys").contains(&"checked_out".to_string()));
}

#[tokio::test]
async fn create_inside_a_transaction_records_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("txn.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        let session_id = Uuid::new_v4();
        db.create_session_connection(session_id).await.unwrap();
        db.begin_with_session(&session_id).await.unwrap();
        db.execute_with_session(ATTENDANCE_LOGS_DDL, &session_id)
            .await
            .unwrap();
        db.commit_with_session(&session_id).await.unwrap();
    }

    DbHandler::new(path).expect("re-opening must not report drift");

    let conn = Connection::open(&db_path).unwrap();
    assert!(metadata_columns(&conn, "attendance_logs").contains(&"check_out".to_string()));
}

#[tokio::test]
async fn multiline_lowercase_ddl_records_metadata() {
    // The shape a node 'pg' client sends from a template literal: newlines,
    // lower-cased keywords, trailing indentation.
    let ddl = "create table if not exists shifts (\n  id text primary key,\n  check_in integer,\n  check_out integer,\n  unique_id text,\n  constraint_name text\n)";

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("multiline.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(ddl).await.unwrap();
    }

    DbHandler::new(path).expect("re-opening must not report drift");

    let conn = Connection::open(&db_path).unwrap();
    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
    assert_eq!(
        metadata_columns(&conn, "shifts"),
        vec![
            "check_in".to_string(),
            "check_out".to_string(),
            "constraint_name".to_string(),
            "id".to_string(),
            "unique_id".to_string(),
        ]
    );
}
