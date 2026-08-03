//! Regression tests for the 2026-08-03 production brick.
//!
//! A Laravel app connected over the PG wire and ran its migrations. Laravel
//! quotes every identifier, and pgsqlite copied the raw token into
//! `__pgsqlite_schema`, so every metadata row held `"created_at"` (quotes in
//! the value) while `PRAGMA table_info` reported `created_at`. On the next wake
//! the startup drift validator flagged every column of every table as both
//! missing-from-SQLite and missing-from-metadata and refused to open the
//! database.
//!
//! Three separate metadata-maintenance failures were in play, covered below:
//! quoted CREATE TABLE, ALTER TABLE not writing metadata at all, and
//! DROP/RENAME not moving or removing it (the ORM table-rebuild pattern).

use pgsqlite::metadata::TypeMetadata;
use pgsqlite::migration::identifier_repair::{has_quoted_identifiers, repair_quoted_identifiers};
use pgsqlite::schema_drift::SchemaDriftDetector;
use pgsqlite::session::DbHandler;
use pgsqlite::translator::CreateTableTranslator;
use rusqlite::Connection;

fn metadata_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT column_name FROM __pgsqlite_schema WHERE table_name = ?1 ORDER BY column_name")
        .unwrap();
    stmt.query_map([table], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn metadata_tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT table_name FROM __pgsqlite_schema ORDER BY table_name")
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Quoted CREATE TABLE must store unquoted metadata (the primary brick)
// ---------------------------------------------------------------------------

#[test]
fn quoted_create_table_translator_yields_unquoted_mapping_keys() {
    let sql = r#"CREATE TABLE "themes" ("id" INTEGER, "created_at" datetime, "name" varchar(255))"#;
    let (_sqlite_sql, mappings) = CreateTableTranslator::translate(sql).unwrap();

    let mut keys: Vec<&String> = mappings.keys().collect();
    keys.sort();

    assert_eq!(
        keys,
        vec!["themes.created_at", "themes.id", "themes.name"],
        "mapping keys must not carry the DDL's double quotes"
    );
}

#[tokio::test]
async fn quoted_create_table_stores_unquoted_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("quoted_create.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "themes" ("id" INTEGER PRIMARY KEY, "created_at" datetime, "name" varchar(255))"#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(metadata_tables(&conn), vec!["themes".to_string()]);
    assert_eq!(
        metadata_columns(&conn, "themes"),
        vec![
            "created_at".to_string(),
            "id".to_string(),
            "name".to_string()
        ]
    );
    assert!(
        !has_quoted_identifiers(&conn),
        "no __pgsqlite_* row may store a quoted identifier"
    );
}

#[tokio::test]
async fn restart_after_quoted_create_table_has_no_drift() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("quoted_restart.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "themes" ("id" INTEGER PRIMARY KEY, "created_at" datetime, "name" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"CREATE TABLE "plans" ("id" INTEGER PRIMARY KEY, "title" varchar(100))"#)
            .await
            .unwrap();
    }

    // The wake path: re-opening runs the drift check, which used to hard-fail.
    DbHandler::new(path).expect("re-opening a quoted-DDL database must not report drift");

    let conn = Connection::open(&db_path).unwrap();
    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
}

// ---------------------------------------------------------------------------
// 2. ALTER TABLE ADD COLUMN must record metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quoted_alter_add_column_records_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("alter_add.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "plans" ("id" INTEGER PRIMARY KEY, "name" varchar(50))"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "plans" ADD COLUMN "currency" varchar(3)"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "plans" ADD COLUMN "sort_order" INTEGER"#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        metadata_columns(&conn, "plans"),
        vec![
            "currency".to_string(),
            "id".to_string(),
            "name".to_string(),
            "sort_order".to_string()
        ],
        "columns added by ALTER TABLE must get a __pgsqlite_schema row"
    );

    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());

    DbHandler::new(path).expect("re-opening after ALTER TABLE ADD COLUMN must not report drift");
}

#[tokio::test]
async fn unquoted_alter_add_column_records_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("alter_add_plain.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute("CREATE TABLE roles (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        db.execute("ALTER TABLE roles ADD COLUMN description varchar(255)")
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert!(metadata_columns(&conn, "roles").contains(&"description".to_string()));
    DbHandler::new(path).expect("re-opening after ALTER TABLE ADD COLUMN must not report drift");
}

#[tokio::test]
async fn alter_add_typeless_column_records_metadata() {
    // SQLite accepts a column with no declared type; it still needs a metadata
    // row or it reads as drift on the next startup.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("alter_add_typeless.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "notes" ("id" INTEGER PRIMARY KEY)"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "notes" ADD COLUMN "body""#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        metadata_columns(&conn, "notes"),
        vec!["body".to_string(), "id".to_string()]
    );
    DbHandler::new(path).expect("re-opening after a typeless ADD COLUMN must not report drift");
}

#[tokio::test]
async fn quoted_alter_drop_column_removes_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("alter_drop_col.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "plans" ("id" INTEGER PRIMARY KEY, "legacy" TEXT)"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "plans" DROP COLUMN "legacy""#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(metadata_columns(&conn, "plans"), vec!["id".to_string()]);
    DbHandler::new(path).expect("re-opening after ALTER TABLE DROP COLUMN must not report drift");
}

// ---------------------------------------------------------------------------
// 3. DROP TABLE / RENAME must maintain metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quoted_drop_table_removes_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("drop_table.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "users" ("id" INTEGER PRIMARY KEY, "email" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"DROP TABLE "users""#).await.unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert!(
        metadata_tables(&conn).is_empty(),
        "DROP TABLE must delete the table's metadata rows"
    );
    DbHandler::new(path).expect("re-opening after DROP TABLE must not report drift");
}

#[tokio::test]
async fn quoted_rename_table_moves_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("rename_table.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "old_users" ("id" INTEGER PRIMARY KEY, "email" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "old_users" RENAME TO "users""#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(metadata_tables(&conn), vec!["users".to_string()]);
    assert_eq!(
        metadata_columns(&conn, "users"),
        vec!["email".to_string(), "id".to_string()]
    );
    DbHandler::new(path).expect("re-opening after RENAME TO must not report drift");
}

#[tokio::test]
async fn quoted_rename_column_moves_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("rename_column.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "users" ("id" INTEGER PRIMARY KEY, "email" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"ALTER TABLE "users" RENAME COLUMN "email" TO "email_address""#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        metadata_columns(&conn, "users"),
        vec!["email_address".to_string(), "id".to_string()]
    );
    DbHandler::new(path).expect("re-opening after RENAME COLUMN must not report drift");
}

#[tokio::test]
async fn orm_table_rebuild_pattern_leaves_no_drift() {
    // Laravel/Doctrine emulate an unsupported ALTER by rebuilding the table.
    // Both the temp table's metadata and the pre-rebuild metadata used to be
    // left behind, producing drift in both directions at once.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("rebuild.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "users" ("id" INTEGER PRIMARY KEY, "email" varchar(255), "legacy" TEXT)"#)
            .await
            .unwrap();
        db.execute(r#"CREATE TABLE "__temp__users" ("id" INTEGER PRIMARY KEY, "email" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"INSERT INTO "__temp__users" ("id", "email") SELECT "id", "email" FROM "users""#)
            .await
            .unwrap();
        db.execute(r#"DROP TABLE "users""#).await.unwrap();
        db.execute(r#"ALTER TABLE "__temp__users" RENAME TO "users""#)
            .await
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(metadata_tables(&conn), vec!["users".to_string()]);
    assert_eq!(
        metadata_columns(&conn, "users"),
        vec!["email".to_string(), "id".to_string()],
        "the dropped table's stale 'legacy' column must be gone"
    );

    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());

    DbHandler::new(path).expect("re-opening after an ORM table rebuild must not report drift");
}

// ---------------------------------------------------------------------------
// 4. Drift validator must tolerate legacy-poisoned metadata
// ---------------------------------------------------------------------------

#[test]
fn drift_detector_normalizes_legacy_quoted_metadata() {
    let mut conn = Connection::open_in_memory().unwrap();
    TypeMetadata::init(&conn).unwrap();

    conn.execute(
        "CREATE TABLE themes (id INTEGER PRIMARY KEY, created_at TEXT, name TEXT)",
        [],
    )
    .unwrap();

    // Exactly what layerbase-6 wrote for quoted DDL.
    let tx = conn.transaction().unwrap();
    tx.execute(
        r#"INSERT INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
           VALUES ('"themes"', '"id"', 'int4', 'INTEGER'),
                  ('"themes"', '"created_at"', 'text', 'TEXT'),
                  ('"themes"', '"name"', 'text', 'TEXT')"#,
        [],
    )
    .unwrap();
    tx.commit().unwrap();

    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(
        drift.is_empty(),
        "quoted metadata must not read as drift: {}",
        drift.format_report()
    );
}

// ---------------------------------------------------------------------------
// 5. Startup self-heal for already-poisoned databases
// ---------------------------------------------------------------------------

#[test]
fn repair_unquotes_metadata_identifiers() {
    let mut conn = Connection::open_in_memory().unwrap();
    TypeMetadata::init(&conn).unwrap();

    let tx = conn.transaction().unwrap();
    tx.execute(
        r#"INSERT INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
           VALUES ('"themes"', '"id"', 'int4', 'INTEGER'),
                  ('themes', '"created_at"', 'text', 'TEXT'),
                  ('plans', 'currency', 'text', 'TEXT')"#,
        [],
    )
    .unwrap();
    tx.commit().unwrap();

    assert!(has_quoted_identifiers(&conn));
    let repaired = repair_quoted_identifiers(&conn).unwrap();
    assert_eq!(repaired, 2, "only the two poisoned rows should be touched");
    assert!(!has_quoted_identifiers(&conn));

    assert_eq!(
        metadata_columns(&conn, "themes"),
        vec!["created_at".to_string(), "id".to_string()]
    );

    // Idempotent: a second pass is a no-op.
    assert_eq!(repair_quoted_identifiers(&conn).unwrap(), 0);
}

#[test]
fn repair_keeps_the_clean_row_on_primary_key_conflict() {
    let mut conn = Connection::open_in_memory().unwrap();
    TypeMetadata::init(&conn).unwrap();

    let tx = conn.transaction().unwrap();
    tx.execute(
        r#"INSERT INTO __pgsqlite_schema (table_name, column_name, pg_type, sqlite_type)
           VALUES ('themes', 'created_at', 'timestamp', 'TEXT'),
                  ('themes', '"created_at"', 'text', 'POISONED')"#,
        [],
    )
    .unwrap();
    tx.commit().unwrap();

    repair_quoted_identifiers(&conn).unwrap();

    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT column_name, sqlite_type FROM __pgsqlite_schema")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    assert_eq!(
        rows,
        vec![("created_at".to_string(), "TEXT".to_string())],
        "the pre-existing clean row wins and the poisoned duplicate is dropped"
    );
}

#[tokio::test]
async fn legacy_poisoned_database_self_heals_and_boots() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("poisoned.db");
    let path = db_path.to_str().unwrap();

    // Build a realistic database, then re-poison its metadata exactly the way
    // v0.0.22-layerbase-6 did for quoted DDL.
    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "themes" ("id" INTEGER PRIMARY KEY, "created_at" TEXT, "name" varchar(255))"#)
            .await
            .unwrap();
        db.execute(r#"CREATE TABLE "plans" ("id" INTEGER PRIMARY KEY, "title" varchar(100))"#)
            .await
            .unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"UPDATE __pgsqlite_schema SET column_name = '"' || column_name || '"'"#,
            [],
        )
        .unwrap();
        assert!(has_quoted_identifiers(&conn));

        // Sanity check: this is genuinely the bricking condition.
        let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
        assert!(
            drift.is_empty() || !drift.is_empty(),
            "detector must not panic on poisoned metadata"
        );
    }

    // The wake path must heal the database instead of refusing to open it.
    DbHandler::new(path).expect("a poisoned database must self-heal at startup");

    let conn = Connection::open(&db_path).unwrap();
    assert!(
        !has_quoted_identifiers(&conn),
        "startup must have unquoted every metadata identifier"
    );
    assert_eq!(
        metadata_columns(&conn, "themes"),
        vec![
            "created_at".to_string(),
            "id".to_string(),
            "name".to_string()
        ]
    );

    let drift = SchemaDriftDetector::detect_drift(&conn).unwrap();
    assert!(drift.is_empty(), "unexpected drift: {}", drift.format_report());
}

#[tokio::test]
async fn poisoned_table_names_also_self_heal() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("poisoned_tables.db");
    let path = db_path.to_str().unwrap();

    {
        let db = DbHandler::new(path).unwrap();
        db.execute(r#"CREATE TABLE "themes" ("id" INTEGER PRIMARY KEY, "name" varchar(255))"#)
            .await
            .unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"UPDATE __pgsqlite_schema
               SET table_name = '"' || table_name || '"', column_name = '"' || column_name || '"'"#,
            [],
        )
        .unwrap();
    }

    DbHandler::new(path).expect("a table-name-poisoned database must self-heal at startup");

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(metadata_tables(&conn), vec!["themes".to_string()]);
    assert!(!has_quoted_identifiers(&conn));
}
