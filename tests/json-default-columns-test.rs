use pgsqlite::translator::CreateTableTranslator;
use rusqlite::Connection;

#[test]
fn quoted_defaults_preserve_columns_and_values() {
    for default in [
        r#"'{"enabled":true,"appName":"Example (PG)"}'"#,
        "'comma, unmatched ) text'",
        "'comma, unmatched ( text'",
        "'it''s a comma, and )'",
    ] {
        let sql = format!("CREATE TABLE sample (id INTEGER, settings TEXT DEFAULT {default}, tail TEXT)");
        let (translated, mappings) = CreateTableTranslator::translate(&sql).unwrap();
        let mut columns: Vec<_> = mappings.keys().map(String::as_str).collect();
        columns.sort();
        assert_eq!(columns, ["sample.id", "sample.settings", "sample.tail"], "{sql}");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&translated).unwrap();
        conn.execute("INSERT INTO sample (id) VALUES (1)", []).unwrap();
        let actual: String = conn.query_row("SELECT settings FROM sample", [], |r| r.get(0)).unwrap();
        let expected: String = conn.query_row(&format!("SELECT {default}"), [], |r| r.get(0)).unwrap();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn json_default_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("defaults.sqlite");
    let path_str = path.to_str().unwrap();
    {
        let db = pgsqlite::session::DbHandler::new(path_str).unwrap();
        db.execute(r#"CREATE TABLE sample (id INTEGER, settings TEXT DEFAULT '{"enabled":true,"appName":"Example (PG)"}', tail TEXT)"#).await.unwrap();
        db.execute("INSERT INTO sample (id) VALUES (1)").await.unwrap();
    }
    let reopened = pgsqlite::session::DbHandler::new(path_str).expect("restart must succeed");
    drop(reopened);
    let conn = Connection::open(path).unwrap();
    let count: i64 = conn.query_row("SELECT count(*) FROM __pgsqlite_schema WHERE table_name='sample'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 3);
    let value: String = conn.query_row("SELECT settings FROM sample", [], |r| r.get(0)).unwrap();
    assert_eq!(value, r#"{"enabled":true,"appName":"Example (PG)"}"#);
    assert!(pgsqlite::schema_drift::SchemaDriftDetector::detect_drift(&conn).unwrap().is_empty());
}
