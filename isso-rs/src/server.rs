//! Axum app builder. MVP: minimal health/config endpoints wired so the binary
//! runs; full handler port is tracked as separate work.

use std::sync::Arc;

use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
}

pub async fn build_app(config: Config) -> anyhow::Result<Router> {
    let db = crate::db::open(&config.general.dbpath).await?;
    let state = AppState {
        config: Arc::new(config),
        db,
    };

    // TODO: port POST /new, GET /, GET/PUT/DELETE /id/:id, /like, /dislike,
    // /count, /preview, /feed, moderation, admin. Tracked by remaining tasks.
    Ok(Router::new()
        .route("/", get(root))
        .route("/config", get(config_endpoint))
        .with_state(state))
}

async fn root() -> &'static str {
    "isso-rs"
}

#[derive(Serialize)]
struct PublicConfig {
    #[serde(rename = "reply-to-self")]
    reply_to_self: bool,
    #[serde(rename = "require-email")]
    require_email: bool,
    #[serde(rename = "require-author")]
    require_author: bool,
    #[serde(rename = "reply-notifications")]
    reply_notifications: bool,
    gravatar: bool,
    avatar: bool,
    feed: bool,
}

async fn config_endpoint(State(state): State<AppState>) -> Json<PublicConfig> {
    Json(PublicConfig {
        reply_to_self: state.config.guard.reply_to_self,
        require_email: state.config.guard.require_email,
        require_author: state.config.guard.require_author,
        reply_notifications: state.config.general.reply_notifications,
        gravatar: state.config.general.gravatar,
        avatar: false,
        feed: !state.config.rss.base.is_empty(),
    })
}
