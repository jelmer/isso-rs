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
use crate::notify::Notifier;
use crate::signer::Signer;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub hasher: Arc<Hasher>,
    pub signer: Arc<Signer>,
    pub renderer: Arc<Renderer>,
    pub notifier: Arc<Notifier>,
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
        let config = Arc::new(config);
        let signer = Arc::new(Signer::new(session_key.as_bytes()));
        let notifier = Arc::new(Notifier::new(Arc::clone(&config), Arc::clone(&signer)));
        Ok(Self {
            signer,
            hasher: Arc::new(hasher),
            renderer: Arc::new(renderer),
            notifier,
            db,
            config,
        })
    }
}

/// Build the axum router on top of the given state.
///
/// Also spawns the background purge task if `[moderation] enabled` — mirrors
/// isso/core.py's ThreadedMixin, which purges stale pending comments once per
/// `purge-after` interval. The task runs for the lifetime of the process.
pub async fn build_app(config: Config) -> anyhow::Result<Router> {
    let state = AppState::from_config(config).await?;
    maybe_spawn_purge(&state);
    Ok(router(state))
}

fn maybe_spawn_purge(state: &AppState) {
    if !state.config.moderation.enabled {
        return;
    }
    let pool = state.db.clone();
    let interval = state.config.moderation.purge_after;
    let delta_secs = interval.as_secs() as f64;
    tokio::spawn(async move {
        // Run one purge immediately, then every `purge-after` — matches the
        // Python uWSGIMixin which also does an initial purge.
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if let Err(e) = crate::db::comments::purge(&pool, now, delta_secs).await {
                tracing::warn!("periodic purge failed: {e}");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Router assembly extracted so tests can build a router against an
/// arbitrary AppState (e.g. with an in-memory SQLite DB).
pub fn router(state: AppState) -> Router {
    let mut r = Router::new();

    // Static assets (js/css/img/demo) are mounted when [server] static-dir
    // points at a readable directory. Operators running behind a reverse
    // proxy that serves these themselves can leave static-dir empty.
    let static_dir = state.config.server.static_dir.clone();
    if !static_dir.is_empty() {
        let base = std::path::Path::new(&static_dir);
        for sub in ["js", "css", "img", "demo"] {
            let path = base.join(sub);
            if path.is_dir() {
                r = r.nest_service(
                    &format!("/{sub}"),
                    tower_http::services::ServeDir::new(&path),
                );
            }
        }
    }

    r.route("/", get(handlers::fetch))
        .route("/new", post(handlers::new_comment))
        .route("/config", get(handlers::config_endpoint))
        .route("/count", post(handlers::counts))
        .route("/preview", post(handlers::preview))
        .route("/info", get(handlers::info))
        .route("/feed", get(handlers::feed))
        .route("/latest", get(handlers::latest))
        .route("/id/:id", get(handlers::view))
        .route("/id/:id", put(handlers::edit))
        .route("/id/:id", delete(handlers::delete_comment))
        .route("/id/:id/like", post(handlers::like))
        .route("/id/:id/dislike", post(handlers::dislike))
        .route(
            "/id/:id/unsubscribe/:email/:key",
            get(handlers::unsubscribe),
        )
        .route("/id/:id/:action/:key", get(handlers::moderate_get))
        .route("/id/:id/:action/:key", post(handlers::moderate_post))
        .route("/login/", get(handlers::login_get))
        .route("/login/", post(handlers::login_post))
        .route("/admin/", get(handlers::admin))
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
/// the check entirely (they can't trigger simple-form CSRF). `/login/`
/// is also exempt — it's a regular `<form>` POST by design (the Python
/// version doesn't wrap login in `@xhr` either).
async fn csrf_guard(req: Request<Body>, next: Next) -> Response {
    let method = req.method();
    if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return next.run(req).await;
    }
    if req.uri().path() == "/login/" {
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
                // Include the debug string in the body — the 5xx path is
                // only taken on genuine bugs / config issues, so the
                // information is actionable for operators.
                let msg = format!("internal error: {e:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
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

/// Extract the anonymised remote address from request headers.
///
/// Mirrors the Python `_remote_addr` logic from isso/views/comments.py:
///
/// ```text
/// route = access_route + [peer]           # XFF (left→right) + socket peer
/// remote_addr = next(
///     addr for addr in reversed(route) if addr not in trusted_proxies,
///     default=peer,
/// )
/// ```
///
/// In practice that means: when `trusted_proxies` is empty, we always use
/// the TCP peer. When the peer *is* listed as a trusted proxy we believe
/// its `X-Forwarded-For` and walk right-to-left, stripping any hop that
/// also appears in the trusted set, until we find a client address.
///
/// Anonymisation always runs on the winning address before it leaves here.
pub fn extract_remote_addr(headers: &HeaderMap, peer: Option<&str>, trusted: &[String]) -> String {
    let peer_str = peer.unwrap_or("0.0.0.0").to_string();

    // Fast path: when there are no trusted proxies, Python doesn't consult
    // XFF at all. Use the TCP peer.
    if trusted.is_empty() {
        return crate::ip::anonymize(&peer_str);
    }

    // Build the full `route = [XFF..., peer]`. Hop-by-hop ordering matches
    // access_route: the leftmost entry is the original client.
    let xff: Vec<String> = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut route = xff;
    route.push(peer_str.clone());

    // Walk right-to-left, skipping trusted-proxy hops.
    let resolved = route
        .iter()
        .rev()
        .find(|addr| !trusted.iter().any(|t| t == *addr))
        .cloned()
        .unwrap_or(peer_str);
    crate::ip::anonymize(&resolved)
}

/// Resolve the *external* URL prefix a response should use for self-
/// referencing links (admin UI, moderation emails, CORS fallback).
///
/// Priority, matching Python's werkzeug ProxyFix(x_prefix=1) + host detection:
///   1. `[server] public-endpoint` if configured  (highest)
///   2. `<scheme>://<host><prefix>` reconstructed from X-Forwarded-Proto,
///      X-Forwarded-Host, X-Forwarded-Prefix / X-Script-Name headers
///   3. The request's Host header + `http` scheme  (fallback)
///   4. The first configured `[general] host`
///
/// Returns the URL *without* a trailing slash.
pub fn external_url_prefix(headers: &HeaderMap, config: &Config) -> String {
    if !config.server.public_endpoint.is_empty() {
        return config
            .server
            .public_endpoint
            .trim_end_matches('/')
            .to_string();
    }

    let hdr = |name: &str| -> Option<&str> { headers.get(name).and_then(|v| v.to_str().ok()) };

    let scheme = hdr("x-forwarded-proto")
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    let host = hdr("x-forwarded-host")
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| hdr("host"))
        .unwrap_or("");
    let prefix = hdr("x-forwarded-prefix")
        .or_else(|| hdr("x-script-name"))
        .map(str::trim)
        .unwrap_or("");

    if !host.is_empty() {
        let mut out = format!("{scheme}://{host}{prefix}");
        while out.ends_with('/') {
            out.pop();
        }
        return out;
    }
    config
        .general
        .hosts
        .first()
        .cloned()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
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
mod proxy_tests {
    use super::*;

    fn hdr_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn xff_ignored_when_no_trusted_proxies_configured() {
        // Default posture: trust only the TCP peer. XFF is attacker-controlled.
        let headers = hdr_map(&[("x-forwarded-for", "evil.attacker")]);
        let got = extract_remote_addr(&headers, Some("203.0.113.7"), &[]);
        assert_eq!(got, "203.0.113.0"); // anonymised /24
    }

    #[test]
    fn xff_right_to_left_stripping_trusted_hops() {
        // Route: [client, trusted-proxy-A, trusted-proxy-B], peer = trusted-proxy-B.
        // Reversed: trusted-proxy-B → trusted-proxy-A → client. First untrusted
        // entry (right-to-left) is the client.
        let headers = hdr_map(&[("x-forwarded-for", "198.51.100.5, 10.0.0.1")]);
        let trusted = vec!["10.0.0.1".into(), "10.0.0.2".into()];
        let got = extract_remote_addr(&headers, Some("10.0.0.2"), &trusted);
        assert_eq!(got, "198.51.100.0");
    }

    #[test]
    fn xff_falls_back_to_peer_when_every_hop_is_trusted() {
        // If every hop in the route is trusted, the Python default=peer wins
        // (before anonymisation).
        let headers = hdr_map(&[("x-forwarded-for", "10.0.0.2")]);
        let trusted = vec!["10.0.0.1".into(), "10.0.0.2".into()];
        let got = extract_remote_addr(&headers, Some("10.0.0.1"), &trusted);
        assert_eq!(got, "10.0.0.0");
    }

    #[test]
    fn xff_respected_only_when_peer_is_trusted() {
        // If the TCP peer isn't listed as trusted, the XFF header is still
        // consulted but only to the extent the walk reaches the peer itself
        // (which is untrusted and terminates the walk).
        let headers = hdr_map(&[("x-forwarded-for", "198.51.100.5")]);
        let trusted = vec!["10.0.0.1".into()];
        let got = extract_remote_addr(&headers, Some("203.0.113.9"), &trusted);
        assert_eq!(got, "203.0.113.0");
    }

    #[test]
    fn external_url_prefix_prefers_public_endpoint() {
        let mut cfg = Config::default();
        cfg.server.public_endpoint = "https://comments.example.com/".into();
        // Even with XFH saying something different, public-endpoint wins.
        let headers = hdr_map(&[("x-forwarded-host", "liar.example.net")]);
        assert_eq!(
            external_url_prefix(&headers, &cfg),
            "https://comments.example.com"
        );
    }

    #[test]
    fn external_url_prefix_reconstructs_from_forwarded_headers() {
        let cfg = Config::default();
        let headers = hdr_map(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "comments.example.com"),
            ("x-forwarded-prefix", "/isso"),
        ]);
        assert_eq!(
            external_url_prefix(&headers, &cfg),
            "https://comments.example.com/isso"
        );
    }

    #[test]
    fn external_url_prefix_falls_back_to_host_header() {
        let cfg = Config::default();
        let headers = hdr_map(&[("host", "localhost:8080")]);
        assert_eq!(external_url_prefix(&headers, &cfg), "http://localhost:8080");
    }

    #[test]
    fn external_url_prefix_final_fallback_is_configured_host() {
        let mut cfg = Config::default();
        cfg.general.hosts = vec!["https://fallback.example/".into()];
        let headers = HeaderMap::new();
        assert_eq!(
            external_url_prefix(&headers, &cfg),
            "https://fallback.example"
        );
    }
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
