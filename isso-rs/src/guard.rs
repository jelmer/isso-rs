//! Rate-limiting and anti-spam checks, mirroring isso/db/spam.py::Guard.
//!
//! Checks run before a comment is inserted:
//! 1. No more than `ratelimit` comments from a given remote_addr in the last 60s.
//! 2. If top-level (parent is None), no more than `direct-reply` comments
//!    from that IP on the same thread.
//! 3. Unless `reply-to-self` is enabled, the parent comment (if any) must not
//!    belong to the same remote_addr within the edit window (max_age).
//! 4. require-email / require-author field presence.

use sqlx::SqlitePool;

use crate::config::Guard as GuardConfig;

#[derive(Debug)]
pub enum GuardError {
    Ratelimit,
    DirectReplyLimit,
    ReplyToSelfBlocked,
    AuthorRequired,
    EmailRequired,
    Db(sqlx::Error),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Ratelimit => f.write_str("ratelimit exceeded"),
            GuardError::DirectReplyLimit => f.write_str("direct reply limit exceeded"),
            GuardError::ReplyToSelfBlocked => f.write_str("edit time frame is still open"),
            GuardError::AuthorRequired => f.write_str("author required"),
            GuardError::EmailRequired => f.write_str("email address required"),
            GuardError::Db(e) => write!(f, "db error: {e}"),
        }
    }
}

impl std::error::Error for GuardError {}

impl From<sqlx::Error> for GuardError {
    fn from(e: sqlx::Error) -> Self {
        GuardError::Db(e)
    }
}

pub struct Guard<'a> {
    pub cfg: &'a GuardConfig,
    pub max_age_secs: u64,
}

pub struct CommentInput<'a> {
    pub remote_addr: &'a str,
    pub parent: Option<i64>,
    pub author: Option<&'a str>,
    pub email: Option<&'a str>,
}

impl<'a> Guard<'a> {
    pub async fn validate(
        &self,
        pool: &SqlitePool,
        thread_id: i64,
        now_unix: f64,
        comment: &CommentInput<'_>,
    ) -> Result<(), GuardError> {
        if !self.cfg.enabled {
            return Ok(());
        }

        // 1. Rate limit within 60 seconds.
        let recent: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM comments WHERE remote_addr = ? AND ? - created < 60",
        )
        .bind(comment.remote_addr)
        .bind(now_unix)
        .fetch_one(pool)
        .await?;
        if recent >= self.cfg.ratelimit as i64 {
            return Err(GuardError::Ratelimit);
        }

        // 2. Direct-reply limit for top-level comments.
        if comment.parent.is_none() {
            let top_level: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM comments WHERE tid = ? AND remote_addr = ? AND parent IS NULL",
            )
            .bind(thread_id)
            .bind(comment.remote_addr)
            .fetch_one(pool)
            .await?;
            if top_level >= self.cfg.direct_reply as i64 {
                return Err(GuardError::DirectReplyLimit);
            }
        }

        // 3. Reply-to-self guard.
        if !self.cfg.reply_to_self {
            if let Some(parent_id) = comment.parent {
                let max_age = self.max_age_secs as f64;
                let same_author: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM comments WHERE id = ? AND remote_addr = ? AND ? - created < ?",
                )
                .bind(parent_id)
                .bind(comment.remote_addr)
                .bind(now_unix)
                .bind(max_age)
                .fetch_one(pool)
                .await?;
                if same_author > 0 {
                    return Err(GuardError::ReplyToSelfBlocked);
                }
            }
        }

        // 4. require-author / require-email.
        if self.cfg.require_email && comment.email.is_none_or(|e| e.is_empty()) {
            return Err(GuardError::EmailRequired);
        }
        if self.cfg.require_author && comment.author.is_none_or(|a| a.is_empty()) {
            return Err(GuardError::AuthorRequired);
        }
        Ok(())
    }
}
