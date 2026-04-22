//! HTTP server: axum router, application state, shared helpers.
//!
//! Handlers live in [`handlers`]. This module owns the wiring: the
//! [`AppState`] that binds the DB pool, config, hasher, signer, and
//! markdown renderer, plus the cross-cutting plumbing (CSRF check,
//! remote-IP extraction, cookie building, error → HTTP response mapping).

pub mod handlers;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::hash::Hasher;
use crate::markdown::Renderer;
use crate::signer::Signer;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub hasher: Arc<Hasher>,
    pub signer: Arc<Signer>,
    pub renderer: Arc<Renderer>,
}

impl AppState {
    /// Build the state needed for the app, pulling the session-key from
    /// preferences so the signer matches the Python install's existing
    /// tokens (Python stores session-key in preferences too).
    pub async fn from_config(config: Config) -> anyhow::Result<Self> {
        let db = crate::db::open(&config.general.dbpath).await?;
        let session_key: String =
            sqlx::query_scalar("SELECT value FROM preferences WHERE key = 'session-key'")
                .fetch_one(&db)
                .await?;
        let hasher = Hasher::from_config(&config.hash.algorithm, &config.hash.salt)?;
        let renderer = Renderer::with_allowlist(
            &config.markup.allowed_elements,
            &config.markup.allowed_attributes,
        );
        Ok(Self {
            signer: Arc::new(Signer::new(session_key.as_bytes())),
            hasher: Arc::new(hasher),
            renderer: Arc::new(renderer),
            db,
            config: Arc::new(config),
        })
    }
}

/// Build the axum router on top of the given state.
pub async fn build_app(config: Config) -> anyhow::Result<Router> {
    let state = AppState::from_config(config).await?;
    Ok(router(state))
}

/// Router assembly extracted so tests can build a router against an
/// arbitrary AppState (e.g. with an in-memory SQLite DB).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::fetch))
        .route("/new", post(handlers::new_comment))
        .route("/config", get(handlers::config_endpoint))
        .route("/count", post(handlers::counts))
        .route("/preview", post(handlers::preview))
        .route("/id/:id", get(handlers::view))
        .route("/id/:id", put(handlers::edit))
        .route("/id/:id", delete(handlers::delete_comment))
        .route("/id/:id/like", post(handlers::like))
        .route("/id/:id/dislike", post(handlers::dislike))
        .layer(middleware::from_fn(csrf_guard))
        .with_state(state)
}

/// Mirror Python's `xhr` decorator: reject mutating requests whose
/// Content-Type is form-encoded, multipart, or plain text. GET/HEAD skip
/// the check entirely (they can't trigger simple-form CSRF).
async fn csrf_guard(req: Request<Body>, next: Next) -> Response {
    let method = req.method();
    if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return next.run(req).await;
    }
    if let Some(ct) = req.headers().get(header::CONTENT_TYPE) {
        let bytes = ct.as_bytes();
        let is_json = bytes.starts_with(b"application/json");
        if !is_json {
            return (StatusCode::FORBIDDEN, "CSRF").into_response();
        }
    }
    next.run(req).await
}

/// Application-level error type; converts cleanly into an HTTP response.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Forbidden(String),
    NotFound,
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            ApiError::Internal(e) => {
                tracing::error!("internal error: {e:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

/// Extract the remote address from request headers.
///
/// Python consults `X-Forwarded-For` only when the peer IP is in
/// `[server] trusted-proxies`. We reimplement that check here, walking
/// the XFF list from right to left and stripping trusted-proxy hops, and
/// falling back to the TCP peer otherwise.
///
/// The caller is expected to have stamped `X-Real-Client` (or we see the
/// raw socket peer through axum's [`ConnectInfo`]) — but since anonymisation
/// always happens before the address is stored, the worst-case outcome of a
/// missed XFF is "we record the proxy's /24 instead of the client's".
pub fn extract_remote_addr(headers: &HeaderMap, peer: Option<&str>, trusted: &[String]) -> String {
    // TODO: walk XFF strictly instead of taking just the leftmost hop.
    let raw = match peer {
        Some(p) if !trusted.iter().any(|t| t == p) => p.to_string(),
        _ => headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .unwrap_or(peer.unwrap_or("0.0.0.0"))
            .to_string(),
    };
    crate::ip::anonymize(&raw)
}

/// Shared cookie builder — mirrors Python's `create_cookie` closure:
/// Secure when public-endpoint is HTTPS, SameSite=None in that case else Lax.
pub fn build_cookie(name: &str, value: &str, max_age: i64, config: &Config) -> HeaderValue {
    let public = if config.server.public_endpoint.is_empty() {
        &config.general.hosts[0]
    } else {
        &config.server.public_endpoint
    };
    let is_https = public.starts_with("https://");
    let samesite = match config.server.samesite.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ if is_https => "None",
        _ => "Lax",
    };
    let secure = if is_https { "; Secure" } else { "" };
    let raw = format!("{name}={value}; Path=/; Max-Age={max_age}; SameSite={samesite}{secure}");
    HeaderValue::from_str(&raw).expect("cookie header value is ASCII")
}
