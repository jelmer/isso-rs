//! Comments table access, mirroring isso/db/comments.py.
//!
//! Wire-compat notes:
//! - `mode` bits: 1=accepted, 2=pending, 4=soft-deleted. Python's fetch uses
//!   the expression `(? | comments.mode) = ?` so e.g. mode-mask 5 yields
//!   accepted+soft-deleted comments (keeps replies visible above a tombstone).
//! - The `voters` BLOB is a 256-byte bloomfilter; vote() re-serialises the
//!   whole array on every vote.
//! - delete() is a soft-delete (mode=4, clear text/author/website) when the
//!   comment has children; otherwise a hard DELETE that the
//!   `remove_stale_threads` trigger may propagate up to threads.
//! - Votes are capped at MAX_LIKES_AND_DISLIKES (142) — the same limit the
//!   Python docstring derives from the bloomfilter false-positive rate.

use serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::bloomfilter::{Bloomfilter, ARRAY_LEN};

pub const MAX_LIKES_AND_DISLIKES: i64 = 142;

pub const MODE_ACCEPTED: i64 = 1;
pub const MODE_PENDING: i64 = 2;
pub const MODE_DELETED: i64 = 4;

/// The 15-column row as stored on disk. Matches Python's
/// `Comments.fields` exactly so callers can rely on 1:1 names.
#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub tid: i64,
    pub id: i64,
    pub parent: Option<i64>,
    pub created: f64,
    pub modified: Option<f64>,
    pub mode: i64,
    pub remote_addr: Option<String>,
    pub text: String,
    pub author: Option<String>,
    pub email: Option<String>,
    pub website: Option<String>,
    pub likes: i64,
    pub dislikes: i64,
    #[serde(skip)]
    pub voters: Vec<u8>,
    pub notification: i64,
}

/// Fields a caller supplies when inserting a new comment. Corresponds to
/// the dict `c` passed to Python's `Comments.add()`.
#[derive(Debug, Clone)]
pub struct NewComment<'a> {
    pub parent: Option<i64>,
    /// Seconds since the Unix epoch. When `None`, the DB layer stamps `now`.
    pub created: Option<f64>,
    pub mode: i64,
    pub remote_addr: &'a str,
    pub text: &'a str,
    pub author: Option<&'a str>,
    pub email: Option<&'a str>,
    pub website: Option<&'a str>,
    pub notification: i64,
}

fn row_to_comment(row: &SqliteRow) -> sqlx::Result<Comment> {
    Ok(Comment {
        tid: row.try_get("tid")?,
        id: row.try_get("id")?,
        parent: row.try_get("parent")?,
        created: row.try_get("created")?,
        modified: row.try_get("modified")?,
        mode: row.try_get("mode")?,
        remote_addr: row.try_get("remote_addr")?,
        text: row.try_get("text")?,
        author: row.try_get("author")?,
        email: row.try_get("email")?,
        website: row.try_get("website")?,
        likes: row.try_get("likes")?,
        dislikes: row.try_get("dislikes")?,
        voters: row.try_get("voters")?,
        notification: row.try_get("notification")?,
    })
}

/// Insert a new comment tied to the thread identified by `uri`.
///
/// Returns the inserted row, after resolving any cross-reply parent relation
/// the same way Python's `_find` helper does: if the requested parent is
/// itself a reply, reparent to the root ancestor; if the parent does not
/// exist on this thread, drop the parent entirely.
pub async fn add(
    pool: &SqlitePool,
    uri: &str,
    now_unix: f64,
    comment: &NewComment<'_>,
) -> sqlx::Result<Comment> {
    let resolved_parent = match comment.parent {
        Some(parent) => resolve_parent(pool, uri, parent).await?,
        None => None,
    };

    // Initial voters filter contains the commenter's own (anonymised) IP so
    // they can't self-vote — Python does the same via
    // `Bloomfilter(iterable=[c["remote_addr"]])`.
    let mut bf = Bloomfilter::new();
    bf.add(comment.remote_addr);
    let voters = bf.array.to_vec();

    let created = comment.created.unwrap_or(now_unix);

    sqlx::query(
        "INSERT INTO comments (\
            tid, parent, created, modified, mode, remote_addr, \
            text, author, email, website, voters, notification) \
         SELECT threads.id, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
         FROM threads WHERE threads.uri = ?",
    )
    .bind(resolved_parent)
    .bind(created)
    .bind::<Option<f64>>(None) // modified
    .bind(comment.mode)
    .bind(comment.remote_addr)
    .bind(comment.text)
    .bind(comment.author)
    .bind(comment.email)
    .bind(comment.website)
    .bind(&voters[..])
    .bind(comment.notification)
    .bind(uri)
    .execute(pool)
    .await?;

    // Return the most-recently-inserted comment for this thread.
    let row = sqlx::query(
        "SELECT c.* FROM comments c \
         INNER JOIN threads t ON t.uri = ? AND c.tid = t.id \
         ORDER BY c.id DESC LIMIT 1",
    )
    .bind(uri)
    .fetch_one(pool)
    .await?;
    row_to_comment(&row)
}

async fn resolve_parent(pool: &SqlitePool, uri: &str, parent: i64) -> sqlx::Result<Option<i64>> {
    // Walk up the parent chain until we hit a comment that has no parent.
    // If we ever land on a comment that doesn't belong to this thread,
    // reject the parent entirely (matches Python's `_find`).
    let mut cur = parent;
    loop {
        let row: Option<(Option<i64>, i64)> = sqlx::query_as(
            "SELECT parent, tid FROM comments c \
             INNER JOIN threads t ON t.uri = ? \
             WHERE c.id = ? AND c.tid = t.id",
        )
        .bind(uri)
        .bind(cur)
        .fetch_optional(pool)
        .await?;
        let Some((parent_of_parent, _tid)) = row else {
            return Ok(None);
        };
        match parent_of_parent {
            Some(p) => cur = p,
            None => return Ok(Some(cur)),
        }
    }
}

pub async fn get(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Comment>> {
    let row = sqlx::query("SELECT * FROM comments WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| row_to_comment(&r)).transpose()
}

pub async fn activate(pool: &SqlitePool, id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE comments SET mode = 1 WHERE id = ? AND mode = 2")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn unsubscribe(pool: &SqlitePool, email: &str, id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE comments SET notification = 0 WHERE email = ? AND (id = ? OR parent = ?)")
        .bind(email)
        .bind(id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Apply partial updates to a comment. Only the fields in `update` are
/// written, so callers can change `text` without clobbering `author` and
/// vice-versa.
#[derive(Debug, Default)]
pub struct CommentUpdate<'a> {
    pub text: Option<&'a str>,
    pub author: Option<Option<&'a str>>,
    pub website: Option<Option<&'a str>>,
    pub email: Option<Option<&'a str>>,
    pub modified: Option<f64>,
    pub mode: Option<i64>,
    pub notification: Option<i64>,
}

pub async fn update(
    pool: &SqlitePool,
    id: i64,
    patch: &CommentUpdate<'_>,
) -> sqlx::Result<Option<Comment>> {
    // Build SET clauses dynamically so we don't overwrite columns the caller
    // didn't touch. Binding order mirrors the column order below.
    let mut assignments: Vec<&'static str> = Vec::new();
    if patch.text.is_some() {
        assignments.push("text = ?");
    }
    if patch.author.is_some() {
        assignments.push("author = ?");
    }
    if patch.website.is_some() {
        assignments.push("website = ?");
    }
    if patch.email.is_some() {
        assignments.push("email = ?");
    }
    if patch.modified.is_some() {
        assignments.push("modified = ?");
    }
    if patch.mode.is_some() {
        assignments.push("mode = ?");
    }
    if patch.notification.is_some() {
        assignments.push("notification = ?");
    }
    if assignments.is_empty() {
        return get(pool, id).await;
    }
    let sql = format!(
        "UPDATE comments SET {} WHERE id = ?",
        assignments.join(", ")
    );
    let mut q = sqlx::query(&sql);
    if let Some(text) = patch.text {
        q = q.bind(text);
    }
    if let Some(author) = patch.author {
        q = q.bind(author);
    }
    if let Some(website) = patch.website {
        q = q.bind(website);
    }
    if let Some(email) = patch.email {
        q = q.bind(email);
    }
    if let Some(modified) = patch.modified {
        q = q.bind(modified);
    }
    if let Some(mode) = patch.mode {
        q = q.bind(mode);
    }
    if let Some(notification) = patch.notification {
        q = q.bind(notification);
    }
    q.bind(id).execute(pool).await?;
    get(pool, id).await
}

/// Delete a comment, either softly (mode=4 + scrub text/author/website) if
/// it has replies, or hard-delete if it is a leaf. Returns `None` for a
/// hard delete, or the soft-deleted comment otherwise.
///
/// Iteratively prunes any soft-deleted comments that are no longer
/// referenced — matching Python's `_remove_stale` loop.
pub async fn delete(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Comment>> {
    let has_children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE parent = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    if has_children == 0 {
        sqlx::query("DELETE FROM comments WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        remove_stale(pool).await?;
        return Ok(None);
    }
    sqlx::query(
        "UPDATE comments SET text = '', mode = 4, author = NULL, website = NULL WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    remove_stale(pool).await?;
    get(pool, id).await
}

async fn remove_stale(pool: &SqlitePool) -> sqlx::Result<()> {
    // Repeatedly delete soft-deleted comments whose replies have also been
    // deleted, which may in turn make their own parents collectible.
    loop {
        let affected = sqlx::query(
            "DELETE FROM comments WHERE mode = 4 AND id NOT IN \
             (SELECT parent FROM comments WHERE parent IS NOT NULL)",
        )
        .execute(pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Ok(());
        }
    }
}

/// Outcome of a vote attempt. `changed` is false when the vote was rejected
/// (cap reached or IP already voted); `message` is only populated in that case.
#[derive(Debug, Serialize, Clone)]
pub struct VoteResult {
    pub likes: i64,
    pub dislikes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip)]
    pub changed: bool,
}

pub async fn vote(
    pool: &SqlitePool,
    upvote: bool,
    id: i64,
    remote_addr: &str,
) -> sqlx::Result<Option<VoteResult>> {
    let row: Option<(i64, i64, Vec<u8>)> =
        sqlx::query_as("SELECT likes, dislikes, voters FROM comments WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((likes, dislikes, voters_bytes)) = row else {
        return Ok(None);
    };

    if likes + dislikes >= MAX_LIKES_AND_DISLIKES {
        return Ok(Some(VoteResult {
            likes,
            dislikes,
            message: Some(format!(
                "{} denied due to a \"likes + dislikes\" total too high ({} >= {})",
                if upvote { "Upvote" } else { "Downvote" },
                likes + dislikes,
                MAX_LIKES_AND_DISLIKES,
            )),
            changed: false,
        }));
    }

    let voter_count = (likes + dislikes).max(0) as u32;
    let mut bf = Bloomfilter::from_bytes(&voters_bytes, voter_count);
    if bf.contains(remote_addr) {
        return Ok(Some(VoteResult {
            likes,
            dislikes,
            message: Some(format!(
                "{} denied because a vote has already been registered for this remote address: {}",
                if upvote { "Upvote" } else { "Downvote" },
                remote_addr,
            )),
            changed: false,
        }));
    }
    bf.add(remote_addr);

    // Keep the exact SQL shape Python uses (likes+=1 XOR dislikes+=1, voters=?).
    let sql = if upvote {
        "UPDATE comments SET likes = likes + 1, voters = ? WHERE id = ?"
    } else {
        "UPDATE comments SET dislikes = dislikes + 1, voters = ? WHERE id = ?"
    };
    sqlx::query(sql)
        .bind(&bf.array[..ARRAY_LEN])
        .bind(id)
        .execute(pool)
        .await?;

    let (new_likes, new_dislikes) = if upvote {
        (likes + 1, dislikes)
    } else {
        (likes, dislikes + 1)
    };
    Ok(Some(VoteResult {
        likes: new_likes,
        dislikes: new_dislikes,
        message: None,
        changed: true,
    }))
}

/// Comment count per URI. Only accepted (mode=1) comments contribute.
///
/// Matches the `POST /count` endpoint: takes a list of URIs, returns a
/// parallel list of counts (missing threads report 0).
pub async fn count(pool: &SqlitePool, uris: &[&str]) -> sqlx::Result<Vec<i64>> {
    // Python issues one grouped SELECT and then looks up each url. Do the
    // same so per-URI cost stays O(1) in the HTTP handler.
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT threads.uri, COUNT(comments.id) FROM comments \
         LEFT OUTER JOIN threads ON threads.id = tid AND comments.mode = 1 \
         GROUP BY threads.uri",
    )
    .fetch_all(pool)
    .await?;
    let map: std::collections::HashMap<&str, i64> =
        rows.iter().map(|(u, c)| (u.as_str(), *c)).collect();
    Ok(uris
        .iter()
        .map(|u| map.get(u).copied().unwrap_or(0))
        .collect())
}

/// Whether the given email has posted any approved comment in the last 6 months.
/// Used by `[moderation] approve-if-email-previously-approved`.
pub async fn is_previously_approved_author(
    pool: &SqlitePool,
    email: Option<&str>,
) -> sqlx::Result<bool> {
    let Some(email) = email else {
        return Ok(false);
    };
    let found: i64 = sqlx::query_scalar(
        "SELECT CASE WHEN EXISTS(\
            SELECT 1 FROM comments WHERE email = ? AND mode = 1 \
            AND created > strftime('%s', DATETIME('now', '-6 month'))\
         ) THEN 1 ELSE 0 END",
    )
    .bind(email)
    .fetch_one(pool)
    .await?;
    Ok(found == 1)
}

/// Comment count grouped by mode, returned as `(mode, count)` pairs.
/// Used by the admin UI to label tabs with item counts.
pub async fn count_by_mode(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, i64)>> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT mode, COUNT(id) FROM comments GROUP BY mode")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

/// Parameters for the admin-UI list query (Python's `Comments.fetchall`).
/// Unlike `FetchParams` this joins threads and returns the URI + title
/// alongside each comment, supports per-thread grouping (`order_by = "tid"`),
/// and paginates by page number rather than offset.
#[derive(Debug, Clone)]
pub struct AdminFetchParams<'a> {
    /// Bitmask filter: 1, 2, 4, or 5 in the public handler; 1/2/4 used by admin tabs.
    pub mode: i64,
    pub order_by: AdminOrderBy,
    pub asc: bool,
    pub limit: i64,
    pub page: i64,
    /// Optional: narrow to a single comment id (admin search by URL).
    pub comment_id: Option<i64>,
    /// Optional: narrow to a thread URI (admin search by thread URL).
    pub thread_uri: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub enum AdminOrderBy {
    Id,
    Created,
    Modified,
    Likes,
    Dislikes,
    Tid,
}

impl AdminOrderBy {
    fn as_sql(self) -> &'static str {
        match self {
            AdminOrderBy::Id => "comments.id",
            AdminOrderBy::Created => "comments.created",
            AdminOrderBy::Modified => "comments.modified",
            AdminOrderBy::Likes => "comments.likes",
            AdminOrderBy::Dislikes => "comments.dislikes",
            AdminOrderBy::Tid => "comments.tid",
        }
    }
}

/// Row returned by the admin listing — a Comment plus the joined thread fields.
#[derive(Debug, Clone)]
pub struct AdminCommentRow {
    pub comment: Comment,
    pub uri: String,
    pub title: Option<String>,
}

/// Admin-UI list endpoint. Mirrors `Comments.fetchall` in isso/db/comments.py.
pub async fn fetch_admin(
    pool: &SqlitePool,
    params: &AdminFetchParams<'_>,
) -> sqlx::Result<Vec<AdminCommentRow>> {
    let mut sql = String::from(
        "SELECT comments.*, threads.uri AS thread_uri, threads.title AS thread_title \
         FROM comments INNER JOIN threads ON comments.tid = threads.id WHERE ",
    );
    if params.comment_id.is_some() {
        sql.push_str("comments.id = ?");
    } else if params.thread_uri.is_some() {
        sql.push_str("threads.uri = ?");
    } else {
        sql.push_str("comments.mode = ?");
    }
    // Match Python's ORDER BY: group replies under their parent's `created`,
    // then the requested column, then `created` as a tie-breaker.
    sql.push_str(" ORDER BY CASE WHEN comments.parent IS NOT NULL THEN comments.created END, ");
    sql.push_str(params.order_by.as_sql());
    if !params.asc {
        sql.push_str(" DESC");
    }
    sql.push_str(", comments.created");
    sql.push_str(" LIMIT ?,?");

    let mut q = sqlx::query(&sql);
    if let Some(cid) = params.comment_id {
        q = q.bind(cid);
    } else if let Some(uri) = params.thread_uri {
        q = q.bind(uri);
    } else {
        q = q.bind(params.mode);
    }
    q = q.bind(params.page * params.limit).bind(params.limit);

    let rows = q.fetch_all(pool).await?;
    rows.iter()
        .map(|r| {
            Ok(AdminCommentRow {
                comment: row_to_comment(r)?,
                uri: r.try_get::<String, _>("thread_uri")?,
                title: r.try_get::<Option<String>, _>("thread_title")?,
            })
        })
        .collect()
}

/// Fetch the N most recently created accepted comments across all threads,
/// enriched with their thread's URI. Backs the `/latest` endpoint, which is
/// the only cross-thread public listing.
pub async fn fetch_latest(pool: &SqlitePool, limit: i64) -> sqlx::Result<Vec<(Comment, String)>> {
    let rows = sqlx::query(
        "SELECT comments.*, threads.uri AS thread_uri FROM comments \
         INNER JOIN threads ON threads.id = comments.tid \
         WHERE comments.mode = 1 \
         ORDER BY comments.created DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| Ok((row_to_comment(r)?, r.try_get::<String, _>("thread_uri")?)))
        .collect()
}

/// Delete stale pending comments older than `delta` seconds.
pub async fn purge(pool: &SqlitePool, now_unix: f64, delta_secs: f64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM comments WHERE mode = 2 AND ? - created > ?")
        .bind(now_unix)
        .bind(delta_secs)
        .execute(pool)
        .await?;
    remove_stale(pool).await?;
    Ok(())
}

/// Parameters for the public `GET /` fetch. Mirrors the Python `fetch()` kwargs.
#[derive(Debug, Clone)]
pub struct FetchParams<'a> {
    pub uri: &'a str,
    /// Bitmask: 1=accepted, 2=pending, 4=soft-deleted. Default 5
    /// (accepted + soft-deleted) keeps deleted tombstones visible so
    /// replies under them still render.
    pub mode: i64,
    /// Return only comments created after this unix timestamp. 0 = no filter.
    pub after: f64,
    /// Filter by parent:
    /// - `None` means "no filter" (match Python's sentinel `"any"`).
    /// - `Some(None)` means "top-level only" (parent IS NULL).
    /// - `Some(Some(id))` means "replies to `id`".
    pub parent: Option<Option<i64>>,
    pub order_by: OrderBy,
    pub asc: bool,
    pub limit: Option<i64>,
    pub offset: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum OrderBy {
    Id,
    Created,
    Modified,
    Likes,
    Dislikes,
    Karma,
}

impl OrderBy {
    fn as_sql(self) -> &'static str {
        match self {
            OrderBy::Id => "id",
            OrderBy::Created => "created",
            OrderBy::Modified => "modified",
            OrderBy::Likes => "likes",
            OrderBy::Dislikes => "dislikes",
            OrderBy::Karma => "karma",
        }
    }
}

impl Default for FetchParams<'_> {
    fn default() -> Self {
        Self {
            uri: "",
            mode: 5,
            after: 0.0,
            parent: None,
            order_by: OrderBy::Id,
            asc: true,
            limit: None,
            offset: 0,
        }
    }
}

/// Fetch comments for a thread. Python builds an SQL expression whose
/// truthiness is `(? | comments.mode) = ?` to filter by mode bitmask; we
/// replicate it so mode=5 behaves identically.
pub async fn fetch(pool: &SqlitePool, params: &FetchParams<'_>) -> sqlx::Result<Vec<Comment>> {
    let mut sql = String::from(
        "SELECT comments.*, likes - dislikes AS karma FROM comments \
         INNER JOIN threads ON threads.uri = ? AND comments.tid = threads.id \
         AND (? | comments.mode) = ? AND comments.created > ?",
    );
    // Build WHERE + ORDER BY in one pass.
    match params.parent {
        Some(None) => sql.push_str(" AND comments.parent IS NULL"),
        Some(Some(_)) => sql.push_str(" AND comments.parent = ?"),
        None => {}
    }
    sql.push_str(" ORDER BY CASE WHEN comments.parent IS NOT NULL THEN comments.created END, ");
    sql.push_str(params.order_by.as_sql());
    if !params.asc {
        sql.push_str(" DESC");
    }
    if params.limit.is_some() && params.offset > 0 {
        sql.push_str(" LIMIT ?,?");
    } else if params.limit.is_some() {
        sql.push_str(" LIMIT ?");
    }

    let mut q = sqlx::query(&sql)
        .bind(params.uri)
        .bind(params.mode)
        .bind(params.mode)
        .bind(params.after);
    if let Some(Some(parent_id)) = params.parent {
        q = q.bind(parent_id);
    }
    if let Some(limit) = params.limit {
        if params.offset > 0 {
            q = q.bind(params.offset).bind(limit);
        } else {
            q = q.bind(limit);
        }
    }
    let rows = q.fetch_all(pool).await?;
    rows.iter().map(row_to_comment).collect()
}

/// Count of replies grouped by parent id. Returned as `(parent_or_null, n)`
/// pairs — matches Python's `reply_count()` which yields a dict keyed by
/// parent id (None for top-level).
pub async fn reply_count(
    pool: &SqlitePool,
    uri: &str,
    mode: i64,
) -> sqlx::Result<Vec<(Option<i64>, i64)>> {
    let rows: Vec<(Option<i64>, i64)> = sqlx::query_as(
        "SELECT comments.parent, COUNT(*) FROM comments \
         INNER JOIN threads ON threads.uri = ? AND comments.tid = threads.id \
         AND (? | comments.mode) = ? \
         GROUP BY comments.parent",
    )
    .bind(uri)
    .bind(mode)
    .bind(mode)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::threads;

    async fn setup() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        // run the schema ourselves — mirror db::initialize's essentials.
        sqlx::query(
            "CREATE TABLE threads (id INTEGER PRIMARY KEY, uri VARCHAR UNIQUE, title VARCHAR)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE comments (\
                tid REFERENCES threads(id), id INTEGER PRIMARY KEY, parent INTEGER, \
                created FLOAT NOT NULL, modified FLOAT, mode INTEGER, remote_addr VARCHAR, \
                text VARCHAR NOT NULL, author VARCHAR, email VARCHAR, website VARCHAR, \
                likes INTEGER DEFAULT 0, dislikes INTEGER DEFAULT 0, \
                voters BLOB NOT NULL, notification INTEGER DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER remove_stale_threads AFTER DELETE ON comments BEGIN \
             DELETE FROM threads WHERE id NOT IN (SELECT tid FROM comments); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn make_thread(pool: &sqlx::SqlitePool, uri: &str) {
        threads::new_thread(pool, uri, Some("Title")).await.unwrap();
    }

    fn sample_comment<'a>(text: &'a str, remote: &'a str) -> NewComment<'a> {
        NewComment {
            parent: None,
            created: Some(1_000.0),
            mode: MODE_ACCEPTED,
            remote_addr: remote,
            text,
            author: None,
            email: None,
            website: None,
            notification: 0,
        }
    }

    #[tokio::test]
    async fn add_and_get_roundtrip() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let c = add(&pool, "/a", 2000.0, &sample_comment("hello", "127.0.0.0"))
            .await
            .unwrap();
        assert_eq!(c.text, "hello");
        assert_eq!(c.mode, MODE_ACCEPTED);
        assert_eq!(c.parent, None);
        let loaded = get(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, c.id);
        assert_eq!(loaded.text, "hello");
    }

    #[tokio::test]
    async fn add_uses_now_when_created_absent() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let mut nc = sample_comment("hi", "127.0.0.0");
        nc.created = None;
        let c = add(&pool, "/a", 12345.0, &nc).await.unwrap();
        assert_eq!(c.created, 12345.0);
    }

    #[tokio::test]
    async fn nested_parent_is_flattened_to_root() {
        // Python enforces max nesting = 1: replies-to-replies get reparented
        // to the root. add() must uphold that via resolve_parent.
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let root = add(&pool, "/a", 1000.0, &sample_comment("root", "127.0.0.0"))
            .await
            .unwrap();
        let mut child = sample_comment("child", "127.0.0.0");
        child.parent = Some(root.id);
        let child = add(&pool, "/a", 1001.0, &child).await.unwrap();
        assert_eq!(child.parent, Some(root.id));

        // Reply to the child → should end up parented to root, not child.
        let mut grandchild = sample_comment("grandchild", "127.0.0.0");
        grandchild.parent = Some(child.id);
        let gc = add(&pool, "/a", 1002.0, &grandchild).await.unwrap();
        assert_eq!(gc.parent, Some(root.id));
    }

    #[tokio::test]
    async fn parent_from_wrong_thread_is_dropped() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        make_thread(&pool, "/b").await;
        let on_a = add(&pool, "/a", 1000.0, &sample_comment("on a", "127.0.0.0"))
            .await
            .unwrap();
        let mut nc = sample_comment("cross", "127.0.0.0");
        nc.parent = Some(on_a.id); // parent lives on /a, comment goes to /b
        let c = add(&pool, "/b", 1001.0, &nc).await.unwrap();
        assert_eq!(c.parent, None);
    }

    #[tokio::test]
    async fn soft_delete_keeps_row_when_there_are_replies() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let root = add(
            &pool,
            "/a",
            1000.0,
            &NewComment {
                author: Some("me"),
                website: Some("http://me.test"),
                ..sample_comment("root", "127.0.0.0")
            },
        )
        .await
        .unwrap();
        let mut child = sample_comment("child", "127.0.0.1");
        child.parent = Some(root.id);
        add(&pool, "/a", 1001.0, &child).await.unwrap();

        let deleted = delete(&pool, root.id).await.unwrap().unwrap();
        assert_eq!(deleted.mode, MODE_DELETED);
        assert_eq!(deleted.text, "");
        assert_eq!(deleted.author, None);
        assert_eq!(deleted.website, None);
    }

    #[tokio::test]
    async fn hard_delete_removes_leaf_and_cascades_stale_thread() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let c = add(&pool, "/a", 1000.0, &sample_comment("hi", "127.0.0.0"))
            .await
            .unwrap();
        assert!(delete(&pool, c.id).await.unwrap().is_none());
        // Trigger should have dropped the thread too.
        let threads_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(threads_left, 0);
    }

    #[tokio::test]
    async fn vote_rejects_duplicate_ip_and_self() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let c = add(&pool, "/a", 1000.0, &sample_comment("root", "10.0.0.1"))
            .await
            .unwrap();

        // Author's own IP is already in the bloomfilter → reject.
        let self_vote = vote(&pool, true, c.id, "10.0.0.1").await.unwrap().unwrap();
        assert!(!self_vote.changed);
        assert_eq!(self_vote.likes, 0);

        // Fresh IP → counts.
        let ok = vote(&pool, true, c.id, "10.0.0.2").await.unwrap().unwrap();
        assert!(ok.changed);
        assert_eq!(ok.likes, 1);

        // Same IP again → rejected.
        let dup = vote(&pool, true, c.id, "10.0.0.2").await.unwrap().unwrap();
        assert!(!dup.changed);
        assert_eq!(dup.likes, 1);
    }

    #[tokio::test]
    async fn vote_missing_comment_returns_none() {
        let pool = setup().await;
        assert!(vote(&pool, true, 9999, "1.2.3.4").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn count_reports_zero_for_missing_thread() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        add(&pool, "/a", 1000.0, &sample_comment("one", "127.0.0.0"))
            .await
            .unwrap();
        add(&pool, "/a", 1001.0, &sample_comment("two", "127.0.0.1"))
            .await
            .unwrap();
        let counts = count(&pool, &["/a", "/missing"]).await.unwrap();
        assert_eq!(counts, vec![2, 0]);
    }

    #[tokio::test]
    async fn count_ignores_pending_and_deleted_comments() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        add(&pool, "/a", 1000.0, &sample_comment("a", "127.0.0.0"))
            .await
            .unwrap();
        let mut pending = sample_comment("b", "127.0.0.1");
        pending.mode = MODE_PENDING;
        add(&pool, "/a", 1001.0, &pending).await.unwrap();
        assert_eq!(count(&pool, &["/a"]).await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn activate_moves_pending_to_accepted() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let mut pending = sample_comment("b", "127.0.0.1");
        pending.mode = MODE_PENDING;
        let c = add(&pool, "/a", 1001.0, &pending).await.unwrap();
        activate(&pool, c.id).await.unwrap();
        let after = get(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after.mode, MODE_ACCEPTED);
    }

    #[tokio::test]
    async fn update_patches_only_provided_fields() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let c = add(
            &pool,
            "/a",
            1000.0,
            &NewComment {
                author: Some("orig"),
                ..sample_comment("first", "127.0.0.0")
            },
        )
        .await
        .unwrap();
        let patch = CommentUpdate {
            text: Some("second"),
            modified: Some(2000.0),
            ..Default::default()
        };
        let updated = update(&pool, c.id, &patch).await.unwrap().unwrap();
        assert_eq!(updated.text, "second");
        assert_eq!(updated.modified, Some(2000.0));
        assert_eq!(updated.author, Some("orig".to_string())); // unchanged
    }

    #[tokio::test]
    async fn fetch_default_returns_accepted_and_soft_deleted() {
        let pool = setup().await;
        make_thread(&pool, "/a").await;
        let accepted = add(&pool, "/a", 1000.0, &sample_comment("a", "127.0.0.0"))
            .await
            .unwrap();
        let mut pending = sample_comment("p", "127.0.0.1");
        pending.mode = MODE_PENDING;
        add(&pool, "/a", 1001.0, &pending).await.unwrap();

        // Soft-delete via a reply → mode 4, but the original is still
        // referenced by the reply. Build the soft-delete scenario directly.
        let mut reply = sample_comment("r", "127.0.0.2");
        reply.parent = Some(accepted.id);
        add(&pool, "/a", 1002.0, &reply).await.unwrap();
        delete(&pool, accepted.id).await.unwrap();

        let params = FetchParams {
            uri: "/a",
            ..Default::default()
        };
        let rows = fetch(&pool, &params).await.unwrap();
        // Default mode=5 (accepted | soft-deleted): the deleted parent
        // stays as a mode-4 tombstone with empty text, the reply is
        // accepted, and the pending comment "p" is excluded.
        let summary: Vec<(i64, &str)> = rows.iter().map(|c| (c.mode, c.text.as_str())).collect();
        assert_eq!(summary, vec![(MODE_DELETED, ""), (MODE_ACCEPTED, "r")]);
    }
}
