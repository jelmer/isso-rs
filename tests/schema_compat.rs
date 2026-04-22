//! Schema-equivalence tests against the Python reference.
//!
//! We don't invoke Python at test time — that couples the tests to whether
//! `python3` + the `isso` package are importable on the builder. Instead,
//! we capture the exact `PRAGMA table_info` output a fresh Python-written
//! DB produces (see `docs/porting-reference.md` §1) and assert our
//! Rust-created schema matches row-for-row. If these ever drift, wire-
//! compat with deployed Python DBs is broken.
//!
//! Python reference captured with:
//!
//! ```text
//! python3 -c "
//! import tempfile, sqlite3
//! from isso import config, db
//! cfg = config.load('isso/isso.cfg')
//! with tempfile.NamedTemporaryFile(suffix='.db', delete=False) as tf:
//!     path = tf.name
//! cfg.set('general', 'dbpath', path)
//! db.SQLite3(path, cfg)
//! con = sqlite3.connect(path)
//! for tbl in ('threads','comments','preferences'):
//!     print(tbl, list(con.execute(f'PRAGMA table_info({tbl})')))
//! print('user_version:', con.execute('PRAGMA user_version').fetchone()[0])
//! "
//! ```

use sqlx::Row;
use tempfile::NamedTempFile;

use isso_rs::db;

/// A single row from `PRAGMA table_info(<table>)`.
///
/// Fields mirror the SQLite API: `(cid, name, declared_type, notnull, default, pk)`.
/// We store `declared_type` as-is — SQLite doesn't enforce types, so the
/// literal text matters for schema-equivalence comparison.
#[derive(Debug, PartialEq, Eq)]
struct ColumnInfo {
    cid: i64,
    name: String,
    declared_type: String,
    notnull: i64,
    default: Option<String>,
    pk: i64,
}

async fn table_info(pool: &sqlx::SqlitePool, table: &str) -> Vec<ColumnInfo> {
    let sql = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(&sql).fetch_all(pool).await.unwrap();
    rows.iter()
        .map(|r| ColumnInfo {
            cid: r.get::<i64, _>("cid"),
            name: r.get::<String, _>("name"),
            declared_type: r.get::<String, _>("type"),
            notnull: r.get::<i64, _>("notnull"),
            default: r.get::<Option<String>, _>("dflt_value"),
            pk: r.get::<i64, _>("pk"),
        })
        .collect()
}

fn col(
    cid: i64,
    name: &str,
    declared_type: &str,
    notnull: i64,
    default: Option<&str>,
    pk: i64,
) -> ColumnInfo {
    ColumnInfo {
        cid,
        name: name.into(),
        declared_type: declared_type.into(),
        notnull,
        default: default.map(String::from),
        pk,
    }
}

#[tokio::test]
async fn threads_schema_matches_python() {
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let got = table_info(&pool, "threads").await;
    // Captured from Python's isso db.SQLite3 on a fresh DB (see module docs).
    let expected = vec![
        col(0, "id", "INTEGER", 0, None, 1),
        col(1, "uri", "VARCHAR(256)", 0, None, 0),
        col(2, "title", "VARCHAR(256)", 0, None, 0),
    ];
    assert_eq!(got, expected);
}

#[tokio::test]
async fn comments_schema_matches_python() {
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let got = table_info(&pool, "comments").await;
    // Note the empty declared_type on `tid`: Python writes
    //   `tid REFERENCES threads(id)`
    // which SQLite stores with no explicit type. We match that exactly.
    let expected = vec![
        col(0, "tid", "", 0, None, 0),
        col(1, "id", "INTEGER", 0, None, 1),
        col(2, "parent", "INTEGER", 0, None, 0),
        col(3, "created", "FLOAT", 1, None, 0),
        col(4, "modified", "FLOAT", 0, None, 0),
        col(5, "mode", "INTEGER", 0, None, 0),
        col(6, "remote_addr", "VARCHAR", 0, None, 0),
        col(7, "text", "VARCHAR", 1, None, 0),
        col(8, "author", "VARCHAR", 0, None, 0),
        col(9, "email", "VARCHAR", 0, None, 0),
        col(10, "website", "VARCHAR", 0, None, 0),
        col(11, "likes", "INTEGER", 0, Some("0"), 0),
        col(12, "dislikes", "INTEGER", 0, Some("0"), 0),
        col(13, "voters", "BLOB", 1, None, 0),
        col(14, "notification", "INTEGER", 0, Some("0"), 0),
    ];
    assert_eq!(got, expected);
}

#[tokio::test]
async fn preferences_schema_matches_python() {
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let got = table_info(&pool, "preferences").await;
    let expected = vec![
        col(0, "key", "VARCHAR", 0, None, 1),
        col(1, "value", "VARCHAR", 0, None, 0),
    ];
    assert_eq!(got, expected);
}

#[tokio::test]
async fn user_version_matches_python_max() {
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, 5);
}

#[tokio::test]
async fn session_key_is_seeded_as_48_char_hex() {
    // Python stores session-key as os.urandom(24).hex() → 48 hex chars.
    // We mirror that so that tokens signed by either side decode on the other.
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let key: String = sqlx::query_scalar("SELECT value FROM preferences WHERE key = 'session-key'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(key.len(), 48);
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "session-key must be lowercase hex, got {key}"
    );
}

#[tokio::test]
async fn remove_stale_threads_trigger_exists() {
    // Python installs this trigger on every open; we must too, because
    // DELETE FROM comments relies on it to prune empty threads.
    let tmp = NamedTempFile::new().unwrap();
    let pool = db::open(&tmp.path().display().to_string()).await.unwrap();
    let name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name='remove_stale_threads'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(name.as_deref(), Some("remove_stale_threads"));
}
