//! SQLite schema + migrations, mirroring isso/db/__init__.py.
//!
//! Schema version: MAX_VERSION = 5. A fresh DB is stamped directly at v5;
//! existing DBs from the Python server run the v0→v5 migration chain.

pub mod comments;
pub mod threads;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite, SqlitePool};
use std::str::FromStr;

pub const MAX_VERSION: u32 = 5;

pub async fn open(path: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?.create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;
    initialize(&pool).await?;
    Ok(pool)
}

async fn initialize(pool: &SqlitePool) -> anyhow::Result<()> {
    // Detect whether any isso tables already exist.
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('threads','comments','preferences')",
    )
    .fetch_one(pool)
    .await?;

    // threads + preferences first (comments references threads).
    sqlx::query(THREADS_SQL).execute(pool).await?;
    sqlx::query(PREFERENCES_SQL).execute(pool).await?;
    sqlx::query(COMMENTS_SQL).execute(pool).await?;
    sqlx::query(TRIGGER_SQL).execute(pool).await?;

    // Seed the session key if missing. isso uses os.urandom(24).hex() so we do
    // the same — 24 random bytes, hex encoded.
    let have_key: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM preferences WHERE key = 'session-key'")
            .fetch_one(pool)
            .await?;
    if have_key == 0 {
        let mut bytes = [0u8; 24];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut bytes);
        let session_key = hex::encode(bytes);
        sqlx::query("INSERT INTO preferences (key, value) VALUES ('session-key', ?)")
            .bind(&session_key)
            .execute(pool)
            .await?;
    }

    if existing == 0 {
        // Fresh DB — stamp directly at MAX_VERSION.
        sqlx::query(&format!("PRAGMA user_version = {MAX_VERSION}"))
            .execute(pool)
            .await?;
    } else {
        migrate(pool).await?;
    }
    Ok(())
}

async fn migrate(pool: &Pool<Sqlite>) -> anyhow::Result<()> {
    loop {
        let version: u32 = sqlx::query("PRAGMA user_version")
            .fetch_one(pool)
            .await?
            .try_get::<i64, _>(0)? as u32;
        if version >= MAX_VERSION {
            return Ok(());
        }
        tracing::info!("running migration {} -> {}", version, version + 1);
        match version {
            0 => migrate_0_to_1(pool).await?,
            1 => migrate_1_to_2(pool).await?,
            2 => migrate_2_to_3(pool).await?,
            3 => migrate_3_to_4(pool).await?,
            4 => migrate_4_to_5(pool).await?,
            other => anyhow::bail!("no migration path from version {other}"),
        }
    }
}

/// v0 → v1: re-initialize the voters bloom filter on every comment because
/// the old Python signature had a bug that leaked other commenters' IPs.
async fn migrate_0_to_1(pool: &SqlitePool) -> anyhow::Result<()> {
    use crate::bloomfilter::Bloomfilter;
    let mut bf = Bloomfilter::new();
    bf.add("127.0.0.0");
    let blob = bf.array.to_vec();
    sqlx::query("UPDATE comments SET voters = ?")
        .bind(&blob)
        .execute(pool)
        .await?;
    sqlx::query("PRAGMA user_version = 1").execute(pool).await?;
    Ok(())
}

/// v1 → v2: no-op for us. The Python version moves `[general] session-key` from
/// the config file into the preferences table. We already seed preferences from
/// a random key on first open, and we don't read session-key from the config,
/// so we only bump the version.
async fn migrate_1_to_2(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("PRAGMA user_version = 2").execute(pool).await?;
    Ok(())
}

/// v2 → v3: flatten nesting deeper than 1 level. For every top-level comment,
/// walk its descendant tree and reparent every node to the top-level.
async fn migrate_2_to_3(pool: &SqlitePool) -> anyhow::Result<()> {
    let top_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM comments WHERE parent IS NULL")
        .fetch_all(pool)
        .await?;

    let mut tx = pool.begin().await?;
    for top in top_ids {
        let mut queue: Vec<i64> = vec![top];
        while let Some(id) = queue.pop() {
            let children: Vec<i64> = sqlx::query_scalar("SELECT id FROM comments WHERE parent = ?")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
            for child in &children {
                sqlx::query("UPDATE comments SET parent = ? WHERE id = ?")
                    .bind(top)
                    .bind(*child)
                    .execute(&mut *tx)
                    .await?;
            }
            queue.extend(children);
        }
    }
    sqlx::query("PRAGMA user_version = 3")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// v3 → v4: add `notification INTEGER DEFAULT 0` to comments (idempotent).
async fn migrate_3_to_4(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA table_info(comments)")
        .fetch_all(pool)
        .await?;
    let has_notification = rows.iter().any(|r| {
        r.try_get::<String, _>("name")
            .map(|n| n == "notification")
            .unwrap_or(false)
    });
    if !has_notification {
        sqlx::query("ALTER TABLE comments ADD COLUMN notification INTEGER DEFAULT 0")
            .execute(pool)
            .await?;
    }
    sqlx::query("PRAGMA user_version = 4").execute(pool).await?;
    Ok(())
}

/// v4 → v5: make `comments.text` NOT NULL. Copy to a new table with the
/// constraint, replace empty/NULL text with ''.
async fn migrate_4_to_5(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE comments SET text = '' WHERE text IS NULL")
        .execute(&mut *tx)
        .await?;
    sqlx::query(&comments_create_sql("comments_new"))
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO comments_new (tid, id, parent, created, modified, mode, remote_addr, text, author, email, website, likes, dislikes, voters, notification) \
         SELECT tid, id, parent, created, modified, mode, remote_addr, text, author, email, website, likes, dislikes, voters, notification FROM comments",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("ALTER TABLE comments RENAME TO comments_backup_v4")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ALTER TABLE comments_new RENAME TO comments")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP TABLE comments_backup_v4")
        .execute(&mut *tx)
        .await?;
    sqlx::query("PRAGMA user_version = 5")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

const THREADS_SQL: &str = "CREATE TABLE IF NOT EXISTS threads (\
    id INTEGER PRIMARY KEY,\
    uri VARCHAR(256) UNIQUE,\
    title VARCHAR(256))";

const PREFERENCES_SQL: &str = "CREATE TABLE IF NOT EXISTS preferences (\
    key VARCHAR PRIMARY KEY,\
    value VARCHAR)";

const TRIGGER_SQL: &str = "CREATE TRIGGER IF NOT EXISTS remove_stale_threads \
    AFTER DELETE ON comments \
    BEGIN \
        DELETE FROM threads WHERE id NOT IN (SELECT tid FROM comments); \
    END";

const COMMENTS_SQL: &str = "CREATE TABLE IF NOT EXISTS comments (\
    tid REFERENCES threads(id),\
    id INTEGER PRIMARY KEY,\
    parent INTEGER,\
    created FLOAT NOT NULL,\
    modified FLOAT,\
    mode INTEGER,\
    remote_addr VARCHAR,\
    text VARCHAR NOT NULL,\
    author VARCHAR,\
    email VARCHAR,\
    website VARCHAR,\
    likes INTEGER DEFAULT 0,\
    dislikes INTEGER DEFAULT 0,\
    voters BLOB NOT NULL,\
    notification INTEGER DEFAULT 0)";

fn comments_create_sql(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
            tid REFERENCES threads(id),\
            id INTEGER PRIMARY KEY,\
            parent INTEGER,\
            created FLOAT NOT NULL,\
            modified FLOAT,\
            mode INTEGER,\
            remote_addr VARCHAR,\
            text VARCHAR NOT NULL,\
            author VARCHAR,\
            email VARCHAR,\
            website VARCHAR,\
            likes INTEGER DEFAULT 0,\
            dislikes INTEGER DEFAULT 0,\
            voters BLOB NOT NULL,\
            notification INTEGER DEFAULT 0)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn fresh_db_stamps_max_version_and_seeds_session_key() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let pool = open(&path).await.unwrap();

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version as u32, MAX_VERSION);

        let key: Option<String> =
            sqlx::query_scalar("SELECT value FROM preferences WHERE key = 'session-key'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        let key = key.expect("session-key should be seeded");
        assert_eq!(key.len(), 48, "24 random bytes hex-encoded");
    }

    #[tokio::test]
    async fn trigger_removes_stale_threads_after_comment_delete() {
        use crate::bloomfilter::Bloomfilter;
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let pool = open(&path).await.unwrap();

        sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/a', 't')")
            .execute(&pool)
            .await
            .unwrap();
        let bf = Bloomfilter::new();
        sqlx::query(
            "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, voters, notification) \
             VALUES (1, NULL, 1.0, 1, '127.0.0.0', 'hi', ?, 0)",
        )
        .bind(&bf.array[..])
        .execute(&pool)
        .await
        .unwrap();

        // Delete the comment → trigger should drop the thread.
        sqlx::query("DELETE FROM comments")
            .execute(&pool)
            .await
            .unwrap();
        let threads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(threads, 0);
    }
}
