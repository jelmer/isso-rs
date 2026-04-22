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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            cors_middleware,
        ))
        .layer(middleware::from_fn(csrf_guard))
        .with_state(state)
}

/// CORS middleware mirroring isso/wsgi.py::CORSMiddleware.
///
/// - Echoes the caller's `Origin` header back in `Access-Control-Allow-Origin`
///   *if* it's in the configured `[general] host` list; otherwise reports
///   the first configured host.
/// - Always emits `Access-Control-Allow-Credentials: true` (cookies cross
///   origins in the JS frontend's typical setup).
/// - For preflight `OPTIONS`, short-circuits with 200 plus the headers —
///   matches Python which bypasses the inner app on OPTIONS.
async fn cors_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    const ALLOW_METHODS: &str = "HEAD, GET, POST, PUT, DELETE";

    let origin_hdr = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // Pick the Allow-Origin value: echo the caller's Origin if it matches
    // one of the configured hosts (after normalising to scheme://netloc),
    // else fall back to the first configured host.
    let allow_origin = resolve_allow_origin(origin_hdr.as_deref(), &state.config.general.hosts);

    let is_preflight = req.method() == Method::OPTIONS;
    let mut resp = if is_preflight {
        (StatusCode::OK, "").into_response()
    } else {
        next.run(req).await
    };

    let headers = resp.headers_mut();
    if let Some(origin) = allow_origin {
        if let Ok(v) = HeaderValue::from_str(&origin) {
            headers.insert("access-control-allow-origin", v);
        }
    }
    headers.insert(
        "access-control-allow-credentials",
        HeaderValue::from_static("true"),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static(ALLOW_METHODS),
    );
    // Mirror Python's allowed/exposed config: Isso core doesn't set these by
    // default, so we leave them unset unless a future config knob arrives.
    resp
}

fn resolve_allow_origin(origin: Option<&str>, hosts: &[String]) -> Option<String> {
    let configured: Vec<(String, Option<u16>, bool)> =
        hosts.iter().filter_map(|h| split_origin(h)).collect();
    if configured.is_empty() {
        return origin.map(String::from);
    }
    if let Some(origin) = origin {
        if let Some(o) = split_origin(origin) {
            if configured.iter().any(|c| c == &o) {
                return Some(join_origin(&o));
            }
        }
    }
    configured.first().map(join_origin)
}

fn split_origin(s: &str) -> Option<(String, Option<u16>, bool)> {
    let url = if s.starts_with("http://") || s.starts_with("https://") {
        url::Url::parse(s).ok()?
    } else {
        url::Url::parse(&format!("http://{s}")).ok()?
    };
    Some((
        url.host_str()?.to_string(),
        url.port(),
        url.scheme() == "https",
    ))
}

fn join_origin(parts: &(String, Option<u16>, bool)) -> String {
    let scheme = if parts.2 { "https" } else { "http" };
    match parts.1 {
        Some(port) if (parts.2 && port != 443) || (!parts.2 && port != 80) => {
            format!("{scheme}://{}:{port}", parts.0)
        }
        _ => format!("{scheme}://{}", parts.0),
    }
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

#[cfg(test)]
mod cors_tests {
    use super::*;

    #[test]
    fn echoes_origin_when_in_configured_hosts() {
        let hosts = vec!["https://example.tld/".into(), "http://example.tld/".into()];
        assert_eq!(
            resolve_allow_origin(Some("https://example.tld"), &hosts),
            Some("https://example.tld".to_string())
        );
        assert_eq!(
            resolve_allow_origin(Some("http://example.tld"), &hosts),
            Some("http://example.tld".to_string())
        );
    }

    #[test]
    fn falls_back_to_first_host_on_mismatch() {
        // Python's CORSMiddleware returns hosts[0] when the caller's Origin
        // isn't whitelisted — tested by test_cors.py::test_simple case `c`.
        let hosts = vec!["https://example.tld/".into(), "http://example.tld/".into()];
        assert_eq!(
            resolve_allow_origin(Some("http://foo.other"), &hosts),
            Some("https://example.tld".to_string())
        );
    }

    #[test]
    fn non_default_port_is_preserved() {
        let hosts = vec!["http://localhost:8080/".into()];
        assert_eq!(
            resolve_allow_origin(Some("http://localhost:8080"), &hosts),
            Some("http://localhost:8080".to_string())
        );
    }

    #[test]
    fn missing_origin_falls_back_to_first() {
        let hosts = vec!["https://example.tld/".into()];
        assert_eq!(
            resolve_allow_origin(None, &hosts),
            Some("https://example.tld".to_string())
        );
    }
}
