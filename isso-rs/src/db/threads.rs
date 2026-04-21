//! Threads table access, mirroring isso/db/threads.py.

use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: i64,
    pub uri: String,
    pub title: Option<String>,
}

pub async fn contains(pool: &SqlitePool, uri: &str) -> sqlx::Result<bool> {
    let found: Option<i64> = sqlx::query_scalar("SELECT id FROM threads WHERE uri = ?")
        .bind(uri)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

pub async fn get_by_uri(pool: &SqlitePool, uri: &str) -> sqlx::Result<Option<Thread>> {
    let row: Option<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, uri, title FROM threads WHERE uri = ?")
            .bind(uri)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, uri, title)| Thread { id, uri, title }))
}

pub async fn get_by_id(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Thread>> {
    let row: Option<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, uri, title FROM threads WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, uri, title)| Thread { id, uri, title }))
}

/// Create a new thread. Returns the inserted row.
///
/// If a row with the same URI already exists the SQL layer raises a
/// UNIQUE constraint error — callers should `contains()` first, matching
/// the Python flow in views/comments.py::new.
pub async fn new_thread(pool: &SqlitePool, uri: &str, title: Option<&str>) -> sqlx::Result<Thread> {
    sqlx::query("INSERT INTO threads (uri, title) VALUES (?, ?)")
        .bind(uri)
        .bind(title)
        .execute(pool)
        .await?;
    // Round-trip so we return the DB-assigned id.
    get_by_uri(pool, uri)
        .await?
        .ok_or_else(|| sqlx::Error::Protocol("thread missing after insert".into()))
}
