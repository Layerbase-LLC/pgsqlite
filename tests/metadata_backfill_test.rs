//! Tests for the startup self-heal of missing `__pgsqlite_schema` rows.
//!
//! The startup drift validator hard-exits on either direction of drift. Only
//! one of them is safe to heal:
//!
//! * SQLite has a column metadata lacks -> the table is intact and complete,
//!   and a table with no metadata at all is already served by decltype
//!   inference. Healed.
//! * Metadata has a column SQLite lacks -> something claimed a column that does
//!   not exist. Still a hard error.
//!
//! Databases written by v0.0.22-layerbase-7 and earlier carry the first shape
//! whenever a column was added by `ALTER TABLE ... ADD COLUMN` or was named
//! after a constraint keyword, and both bricked customer databases on wake.

use pgsqlite::migration::backfill_missing_column_metadata;
use pgsqlite::schema_drift::SchemaDriftDetector;
use pgsqlite::session::DbHandler;
use rusqlite::Connection;

const ATTENDANCE_LOGS_DDL: &str = "CREATE TABLE attendance_logs (id TEXT PRIMARY KEY, org_id TEXT NOT NULL, employee_id TEXT NOT NULL, attendance_date INTEGER NOT NULL, status TEXT, check_in INTEGER, check_out INTEGER, remarks TEXT, created_at INTEGER NOT NULL)";

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

fn pg_type_of(conn: &Connection, table: &str, column: &str) -> String {
    conn.query_row(
        "SELECT pg_type FROM __pgsqlite_schema WHERE table_name = ?1 AND column_name = ?2",
        [table, column],
        |row| row.get::<_, String>(0),
    )
    .unwrap()
}

/// Reproduce a database written by an older binary: the columns exist, their
/// metadata rows do not.
fn delete_metadata_rows(conn: &Connection, table: &str, columns: &[&str]) {
    for column in columns {
        conn.execute(
            "DELETE FROM __pgsqlite_schema WHERE table_name = ?1 AND column_name = ?2",
            [table, column],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn database_missing_column_metadata_self_heals_and_boots() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("missing_metadata.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(ATTENDANCE_LOGS_DDL).await.unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        delete_metadata_rows(&conn, "attendance_logs", &["check_in", "check_out"]);

        // Sanity check: this is genuinely the bricking condition.
        let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
        assert!(
            !drift.is_empty(),
            "the test fixture must actually be drifted"
        );
        assert!(drift.format_report().contains("missing from metadata"));
    }

    // The wake path must heal the database instead of refusing to open it.
    DbHandler::new(path).expect("a database missing column metadata must self-heal at startup");

    let conn = Connection::open(&db_path).unwrap();
    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
    assert!(metadata_columns(&conn, "attendance_logs").contains(&"check_in".to_string()));
    assert!(metadata_columns(&conn, "attendance_logs").contains(&"check_out".to_string()));
}

#[tokio::test]
async fn healed_rows_match_the_declared_sqlite_type_and_never_drift() {
    // What the backfill can and cannot recover. `PRAGMA table_info` reports the
    // SQLite declared type, and several PostgreSQL types collapse onto the same
    // one (BOOL and TIMESTAMP both store as INTEGER, BYTEA as TEXT), so the
    // ORIGINAL pg_type is not recoverable from a healed column. That is not a
    // regression: a column with no metadata row is already served by exactly
    // this inference at runtime, so healing it is never worse than the gap it
    // replaces, and it is enormously better than refusing to open the database.
    //
    // What IS guaranteed is that a healed row can never itself read as drift:
    // sqlite_type is copied verbatim from PRAGMA, and pg_type is whatever a
    // fresh CREATE TABLE of that declared type would have recorded.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("healed_types.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT, flag BOOL, ratio REAL, amt NUMERIC(10,2))")
            .await
            .unwrap();
    }

    let declared: Vec<(String, String)> = {
        let conn = Connection::open(&db_path).unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(t)").unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };

    {
        let conn = Connection::open(&db_path).unwrap();
        delete_metadata_rows(&conn, "t", &["note", "flag", "ratio", "amt"]);
        assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 4);
    }

    let conn = Connection::open(&db_path).unwrap();
    for (column, declared_type) in &declared {
        let stored: String = conn
            .query_row(
                "SELECT sqlite_type FROM __pgsqlite_schema WHERE table_name = 't' AND column_name = ?1",
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            &stored, declared_type,
            "healed sqlite_type for {column} must be the declared type verbatim"
        );
    }

    // The declared SQLite type round-trips: `flag BOOL` stored as INTEGER heals
    // to pg_type INTEGER, which is what a client declaring INTEGER would get.
    assert_eq!(pg_type_of(&conn, "t", "flag"), "INTEGER");
    assert_eq!(pg_type_of(&conn, "t", "note"), "TEXT");

    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
}

#[tokio::test]
async fn backfill_is_idempotent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("idempotent.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(ATTENDANCE_LOGS_DDL).await.unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 0);

    delete_metadata_rows(&conn, "attendance_logs", &["check_in"]);
    assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 1);
    assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 0);
}

#[tokio::test]
async fn untracked_tables_are_left_alone() {
    // A table with no metadata at all is served entirely by decltype inference
    // and is not something the drift check looks at. Backfilling it would start
    // tracking it, which is a behavior change, not a repair.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("untracked.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE tracked (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    conn.execute("CREATE TABLE untracked (id INTEGER, name TEXT)", [])
        .unwrap();

    assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 0);
    assert!(metadata_columns(&conn, "untracked").is_empty());
}

#[tokio::test]
async fn internal_metadata_tables_are_never_tracked() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("internal.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    // A stray row naming a pgsqlite bookkeeping table must not cause the
    // backfill to start registering that table's own columns.
    conn.execute(
        "INSERT OR IGNORE INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
         VALUES ('__pgsqlite_schema', 'table_name', 'text', 'TEXT')",
        [],
    )
    .unwrap();

    assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 0);
}

#[tokio::test]
async fn orphaned_metadata_still_hard_fails() {
    // The opposite direction: metadata claims a column SQLite does not have.
    // That is a real inconsistency and must keep failing closed.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("orphan.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
             VALUES ('t', 'ghost', 'text', 'TEXT')",
            [],
        )
        .unwrap();
        assert_eq!(
            backfill_missing_column_metadata(&conn).unwrap(),
            0,
            "the backfill must not touch the orphaned direction"
        );
    }

    let message = match DbHandler::new(path) {
        Ok(_) => panic!("metadata claiming a column SQLite lacks must still refuse to open"),
        Err(error) => format!("{error}"),
    };
    assert!(
        message.contains("Schema drift detected"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("missing from SQLite"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn type_mismatches_still_hard_fail() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("mismatch.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE __pgsqlite_schema SET sqlite_type = 'BLOB' WHERE table_name = 't' AND column_name = 'name'",
            [],
        )
        .unwrap();
    }

    let message = match DbHandler::new(path) {
        Ok(_) => panic!("a type mismatch must still refuse to open"),
        Err(error) => format!("{error}"),
    };
    assert!(
        message.contains("Schema drift detected"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn a_table_dropped_behind_pgsqlites_back_still_hard_fails() {
    // Orphaned metadata for a table that no longer exists is C-076 territory
    // and must stay loud; the backfill skips it rather than inventing rows.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("dropped.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE t", []).unwrap();
        assert_eq!(backfill_missing_column_metadata(&conn).unwrap(), 0);
    }

    assert!(
        DbHandler::new(path).is_err(),
        "orphaned metadata for a vanished table must still refuse to open"
    );
}
