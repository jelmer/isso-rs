//! HTTP handlers. Names mirror `isso/views/comments.py` where possible:
//!
//! - [`new_comment`] → `POST /new`
//! - [`fetch`]       → `GET /`
//! - [`view`]        → `GET /id/:id`
//! - [`edit`]        → `PUT /id/:id`
//! - [`delete_comment`] → `DELETE /id/:id`
//! - [`like`] / [`dislike`] → `POST /id/:id/(dis)like`
//! - [`counts`]      → `POST /count`
//! - [`preview`]     → `POST /preview`
//! - [`config_endpoint`] → `GET /config`
//!
//! Wire-compat notes:
//! - JSON field names match Python exactly (`reply-to-self`, etc. use
//!   kebab-case via `#[serde(rename)]`).
//! - Cookie names are the comment id as a bare decimal string (the same
//!   trick Python uses, e.g. `1=...`). We also emit `X-Set-Cookie: isso-{id}=...`
//!   for frontends that can't read Set-Cookie cross-origin.
//! - Status 201 for accepted comments, 202 for pending-moderation ones.

use std::collections::HashMap;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{build_cookie, extract_remote_addr, ApiError, AppState};
use crate::db::comments::{self as cmt, Comment, CommentUpdate, FetchParams, NewComment, OrderBy};
use crate::db::threads;
use crate::guard::{CommentInput, Guard, GuardError};

/// Public config block returned by `GET /config` and embedded in every
/// `GET /` response under the `config` key.
#[derive(Serialize, Debug, Clone)]
pub struct PublicConfig {
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

impl PublicConfig {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            reply_to_self: state.config.guard.reply_to_self,
            require_email: state.config.guard.require_email,
            require_author: state.config.guard.require_author,
            reply_notifications: state.config.general.reply_notifications,
            gravatar: state.config.general.gravatar,
            avatar: false,
            feed: !state.config.rss.base.is_empty(),
        }
    }
}

pub async fn config_endpoint(State(state): State<AppState>) -> Json<PublicConfig> {
    Json(PublicConfig::from_state(&state))
}

#[derive(Debug, Deserialize)]
pub struct NewQuery {
    uri: String,
}

#[derive(Debug, Deserialize)]
pub struct NewBody {
    text: Option<String>,
    author: Option<String>,
    email: Option<String>,
    website: Option<String>,
    parent: Option<i64>,
    title: Option<String>,
    #[serde(default)]
    notification: i64,
}

/// Projection of [`Comment`] onto the public API shape. Python's handler
/// strips anything not in `API.FIELDS`; we materialise that explicitly.
#[derive(Debug, Serialize)]
pub struct CommentJson {
    pub id: i64,
    pub parent: Option<i64>,
    pub text: String,
    pub author: Option<String>,
    pub website: Option<String>,
    pub mode: i64,
    pub created: f64,
    pub modified: Option<f64>,
    pub likes: i64,
    pub dislikes: i64,
    pub hash: String,
    pub notification: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gravatar_image: Option<String>,
    // These two only populate on `GET /` / nested-reply responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_replies: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_replies: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replies: Option<Vec<CommentJson>>,
}

fn render_comment(c: Comment, state: &AppState, render_html: bool) -> CommentJson {
    let text = if render_html {
        state.renderer.render(&c.text)
    } else {
        c.text.clone()
    };
    let hash_input = c
        .email
        .clone()
        .or_else(|| c.remote_addr.clone())
        .unwrap_or_default();
    let hash = state.hasher.uhash(&hash_input);
    let gravatar_image = if state.config.general.gravatar {
        // Python `gravatar-url` contains `{}` where the MD5 of the email (or
        // author name, whichever is set first) goes. Gravatar requires MD5
        // specifically — not the configured `[hash] algorithm`.
        use md5::Digest as _;
        let email_or_author = c.email.clone().or(c.author.clone()).unwrap_or_default();
        let md5_hex = hex::encode(md5::Md5::digest(email_or_author.as_bytes()));
        Some(state.config.general.gravatar_url.replace("{}", &md5_hex))
    } else {
        None
    };
    CommentJson {
        id: c.id,
        parent: c.parent,
        text,
        author: c.author,
        website: c.website,
        mode: c.mode,
        created: c.created,
        modified: c.modified,
        likes: c.likes,
        dislikes: c.dislikes,
        hash,
        notification: c.notification,
        gravatar_image,
        total_replies: None,
        hidden_replies: None,
        replies: None,
    }
}

/// Validation shared between `POST /new` and `PUT /id/:id`. Returns the
/// same error strings Python emits so frontends with pre-existing error
/// handling keep working.
fn verify_comment(
    text: Option<&str>,
    author: Option<&str>,
    website: Option<&str>,
    email: Option<&str>,
) -> Result<(), String> {
    let text = text.ok_or_else(|| "text is missing".to_string())?;
    if text.trim_end().len() < 3 {
        return Err("text is too short (minimum length: 3)".into());
    }
    if text.len() > 65535 {
        return Err("text is too long (maximum length: 65535)".into());
    }
    if let Some(email) = email {
        if email.len() > 254 {
            return Err("http://tools.ietf.org/html/rfc5321#section-4.5.3".into());
        }
    }
    if let Some(website) = website {
        if website.len() > 254 {
            return Err("arbitrary length limit".into());
        }
        // TODO: port the Django URL regex from views/comments.py for strict match.
    }
    let _ = author;
    Ok(())
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn html_escape(s: &str, quote: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote => out.push_str("&#34;"),
            '\'' if quote => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn text_sha1_hex(text: &str) -> String {
    hex::encode(Sha1::digest(text.as_bytes()))
}

fn cookie_headers(id: i64, value: &str, max_age: i64, state: &AppState) -> Vec<(String, String)> {
    vec![
        (
            header::SET_COOKIE.to_string(),
            build_cookie(&id.to_string(), value, max_age, &state.config)
                .to_str()
                .expect("ASCII cookie")
                .to_string(),
        ),
        (
            "X-Set-Cookie".to_string(),
            build_cookie(&format!("isso-{id}"), value, max_age, &state.config)
                .to_str()
                .expect("ASCII cookie")
                .to_string(),
        ),
    ]
}

fn with_cookies(status: StatusCode, body: Value, cookies: Vec<(String, String)>) -> Response {
    let mut resp = (status, Json(body)).into_response();
    for (name, v) in cookies {
        if let (Ok(n), Ok(val)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_str(&v),
        ) {
            resp.headers_mut().append(n, val);
        }
    }
    resp
}

pub async fn new_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    Query(q): Query<NewQuery>,
    Json(body): Json<NewBody>,
) -> Result<Response, ApiError> {
    let peer = connect.as_ref().map(|ci| ci.0.ip().to_string());
    let remote_addr = extract_remote_addr(
        &headers,
        peer.as_deref(),
        &state.config.server.trusted_proxies,
    );

    verify_comment(
        body.text.as_deref(),
        body.author.as_deref(),
        body.website.as_deref(),
        body.email.as_deref(),
    )
    .map_err(ApiError::BadRequest)?;

    let text = body.text.expect("verified above");
    let author = body.author.map(|a| html_escape(&a, false));
    let email = body.email;
    let website = body.website.map(|w| {
        let escaped = html_escape(&w, true);
        if escaped.starts_with("http://") || escaped.starts_with("https://") {
            escaped
        } else {
            format!("http://{escaped}")
        }
    });

    // Ensure a thread row exists. Python fetches the page title via HTTP
    // when no title is provided — that's network I/O we don't want in MVP,
    // so we require `title` or reject with the same error Python would.
    let thread = match threads::get_by_uri(&state.db, &q.uri).await? {
        Some(t) => t,
        None => {
            let title = body.title.as_deref().ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "Cannot create new thread: URI {} has no title. Provide `title` in the request body.",
                    q.uri
                ))
            })?;
            threads::new_thread(&state.db, &q.uri, Some(title)).await?
        }
    };

    let mode = if state.config.moderation.enabled {
        2
    } else {
        1
    };
    let now = now_unix();

    let guard = Guard {
        cfg: &state.config.guard,
        max_age_secs: state.config.general.max_age.as_secs(),
    };
    let input = CommentInput {
        remote_addr: &remote_addr,
        parent: body.parent,
        author: author.as_deref(),
        email: email.as_deref(),
    };
    match guard.validate(&state.db, thread.id, now, &input).await {
        Ok(()) => {}
        Err(GuardError::Db(e)) => return Err(ApiError::Internal(e.into())),
        Err(other) => return Err(ApiError::Forbidden(other.to_string())),
    }

    let new_c = NewComment {
        parent: body.parent,
        created: None,
        mode,
        remote_addr: &remote_addr,
        text: &text,
        author: author.as_deref(),
        email: email.as_deref(),
        website: website.as_deref(),
        notification: body.notification,
    };
    let inserted = cmt::add(&state.db, &q.uri, now, &new_c).await?;

    // Fire notification hooks (stdout log, SMTP admin email). The notifier
    // does its own work on a tokio task so this doesn't block the response.
    state.notifier.comment_created(&thread, &inserted);

    let token = state
        .signer
        .sign(&json!([inserted.id, text_sha1_hex(&inserted.text)]))
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("sign: {e}")))?;
    let max_age = state.config.general.max_age.as_secs() as i64;
    let cookies = cookie_headers(inserted.id, &token, max_age, &state);
    let json_body = serde_json::to_value(render_comment(inserted.clone(), &state, true))
        .map_err(|e| ApiError::Internal(e.into()))?;
    let status = if inserted.mode == 2 {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CREATED
    };
    Ok(with_cookies(status, json_body, cookies))
}

#[derive(Debug, Deserialize)]
pub struct FetchQuery {
    uri: String,
    #[serde(default)]
    plain: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    nested_limit: Option<String>,
    #[serde(default)]
    offset: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    sort: Option<String>,
}

pub async fn fetch(
    State(state): State<AppState>,
    Query(q): Query<FetchQuery>,
) -> Result<Response, ApiError> {
    // Sort → (order_by, asc).
    let (order_by, asc) = match q.sort.as_deref().unwrap_or("oldest") {
        "newest" => (OrderBy::Created, false),
        "oldest" => (OrderBy::Created, true),
        "upvotes" => (OrderBy::Karma, false),
        other => {
            return Err(ApiError::BadRequest(format!(
                "Invalid sort option '{other}'. Must be one of: 'newest', 'oldest', 'upvotes'"
            )));
        }
    };

    let limit: Option<i64> = match q.limit {
        Some(v) => Some(
            v.parse()
                .map_err(|_| ApiError::BadRequest("limit should be integer".into()))?,
        ),
        None => None,
    };
    let offset: i64 = match q.offset {
        Some(v) => v
            .parse()
            .map_err(|_| ApiError::BadRequest("offset should be integer".into()))?,
        None => 0,
    };
    if offset < 0 {
        return Err(ApiError::BadRequest("offset should not be negative".into()));
    }
    let after: f64 = match q.after {
        Some(v) => v
            .parse()
            .map_err(|_| ApiError::BadRequest("after should be a number".into()))?,
        None => 0.0,
    };
    let nested_limit: Option<i64> = match q.nested_limit {
        Some(v) => Some(
            v.parse()
                .map_err(|_| ApiError::BadRequest("nested_limit should be integer".into()))?,
        ),
        None => None,
    };
    let root_id: Option<i64> = match q.parent.as_deref() {
        Some(v) => Some(
            v.parse()
                .map_err(|_| ApiError::BadRequest("parent should be integer".into()))?,
        ),
        None => None,
    };

    // Python name `plain` is inverted: plain="0" (default) → render HTML.
    let render_html = q.plain.as_deref().unwrap_or("0") == "0";

    // Count replies per parent, then fetch the top-level list.
    let reply_counts: HashMap<Option<i64>, i64> = cmt::reply_count(&state.db, &q.uri, 5)
        .await?
        .into_iter()
        .collect();

    let top_list = if limit == Some(0) {
        Vec::new()
    } else {
        let parent = match root_id {
            Some(id) => Some(Some(id)),
            None => Some(None),
        };
        let params = FetchParams {
            uri: &q.uri,
            mode: 5,
            after,
            parent,
            order_by,
            asc,
            limit,
            offset,
        };
        cmt::fetch(&state.db, &params).await?
    };

    let total_replies: i64 = match root_id {
        None => reply_counts.values().sum(),
        Some(id) => reply_counts.get(&Some(id)).copied().unwrap_or(0),
    };

    let root_count = reply_counts.get(&root_id).copied().unwrap_or(0);
    let hidden_replies = root_count - top_list.len() as i64 - offset;

    let mut replies: Vec<CommentJson> = top_list
        .into_iter()
        .map(|c| render_comment(c, &state, render_html))
        .collect();

    if root_id.is_none() {
        // Nested: one level of replies, per Python's hard-coded depth=1.
        for reply in replies.iter_mut() {
            let total = reply_counts.get(&Some(reply.id)).copied().unwrap_or(0);
            let nested = match nested_limit {
                Some(n) if n <= 0 => Vec::new(),
                _ => {
                    let params = FetchParams {
                        uri: &q.uri,
                        mode: 5,
                        after,
                        parent: Some(Some(reply.id)),
                        order_by,
                        asc,
                        limit: nested_limit,
                        offset: 0,
                    };
                    cmt::fetch(&state.db, &params).await?
                }
            };
            let nested_len = nested.len() as i64;
            reply.total_replies = Some(total);
            reply.hidden_replies = Some(total - nested_len);
            reply.replies = Some(
                nested
                    .into_iter()
                    .map(|c| render_comment(c, &state, render_html))
                    .collect(),
            );
        }
    }

    let body = json!({
        "id": root_id,
        "total_replies": total_replies,
        "hidden_replies": hidden_replies,
        "replies": replies,
        "config": PublicConfig::from_state(&state),
    });
    Ok((StatusCode::OK, Json(body)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ViewQuery {
    #[serde(default)]
    plain: Option<String>,
}

fn cookie_for(id: i64, headers: &HeaderMap) -> Option<String> {
    let name = id.to_string();
    let cookie_hdr = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_hdr.split(';') {
        let (k, v) = part.trim().split_once('=')?;
        if k == name {
            return Some(v.to_string());
        }
    }
    None
}

pub async fn view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<ViewQuery>,
) -> Result<Response, ApiError> {
    let comment = cmt::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    let cookie = cookie_for(id, &headers).ok_or_else(|| ApiError::Forbidden("no cookie".into()))?;
    let _claim: (i64, String) = state
        .signer
        .unsign(
            &cookie,
            Some(state.config.general.max_age.as_secs()),
            now_unix() as u64,
        )
        .map_err(|e| ApiError::Forbidden(format!("invalid cookie: {e}")))?;
    let render_html = q.plain.as_deref().unwrap_or("0") == "0";
    let body = serde_json::to_value(render_comment(comment, &state, render_html))
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((StatusCode::OK, Json(body)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct EditBody {
    text: Option<String>,
    author: Option<String>,
    website: Option<String>,
}

pub async fn edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<EditBody>,
) -> Result<Response, ApiError> {
    let cookie = cookie_for(id, &headers).ok_or_else(|| ApiError::Forbidden("no cookie".into()))?;
    let (cookie_id, expected_text_sha1): (i64, String) = state
        .signer
        .unsign(
            &cookie,
            Some(state.config.general.max_age.as_secs()),
            now_unix() as u64,
        )
        .map_err(|e| ApiError::Forbidden(format!("invalid cookie: {e}")))?;
    if cookie_id != id {
        return Err(ApiError::Forbidden("cookie id mismatch".into()));
    }
    let existing = cmt::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    if text_sha1_hex(&existing.text) != expected_text_sha1 {
        return Err(ApiError::Forbidden("text checksum mismatch".into()));
    }

    verify_comment(
        body.text.as_deref(),
        body.author.as_deref(),
        body.website.as_deref(),
        None,
    )
    .map_err(ApiError::BadRequest)?;

    let text = body.text.expect("verified above");
    let author = body.author.map(|a| html_escape(&a, false));
    let website = body.website.map(|w| html_escape(&w, true));

    let patch = CommentUpdate {
        text: Some(&text),
        author: Some(author.as_deref()),
        website: Some(website.as_deref()),
        modified: Some(now_unix()),
        ..Default::default()
    };
    let updated = cmt::update(&state.db, id, &patch)
        .await?
        .ok_or(ApiError::NotFound)?;

    let token = state
        .signer
        .sign(&json!([updated.id, text_sha1_hex(&updated.text)]))
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("sign: {e}")))?;
    let max_age = state.config.general.max_age.as_secs() as i64;
    let cookies = cookie_headers(updated.id, &token, max_age, &state);
    let json_body = serde_json::to_value(render_comment(updated, &state, true))
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(with_cookies(StatusCode::OK, json_body, cookies))
}

pub async fn delete_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let cookie = cookie_for(id, &headers).ok_or_else(|| ApiError::Forbidden("no cookie".into()))?;
    let (cookie_id, expected_text_sha1): (i64, String) = state
        .signer
        .unsign(
            &cookie,
            Some(state.config.general.max_age.as_secs()),
            now_unix() as u64,
        )
        .map_err(|e| ApiError::Forbidden(format!("invalid cookie: {e}")))?;
    if cookie_id != id {
        return Err(ApiError::Forbidden("cookie id mismatch".into()));
    }
    let existing = cmt::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    if text_sha1_hex(&existing.text) != expected_text_sha1 {
        return Err(ApiError::Forbidden("text checksum mismatch".into()));
    }
    let deleted = cmt::delete(&state.db, id).await?;
    let body = match deleted {
        Some(c) => serde_json::to_value(render_comment(c, &state, true))
            .map_err(|e| ApiError::Internal(e.into()))?,
        None => Value::Null,
    };
    // Tell the browser to drop the edit cookie by setting Max-Age=0.
    let cookies = cookie_headers(id, "", 0, &state);
    Ok(with_cookies(StatusCode::OK, body, cookies))
}

pub async fn like(
    State(state): State<AppState>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    vote_impl(state, headers, connect, id, true).await
}

pub async fn dislike(
    State(state): State<AppState>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    vote_impl(state, headers, connect, id, false).await
}

async fn vote_impl(
    state: AppState,
    headers: HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    id: i64,
    upvote: bool,
) -> Result<Response, ApiError> {
    let peer = connect.as_ref().map(|ci| ci.0.ip().to_string());
    let remote_addr = extract_remote_addr(
        &headers,
        peer.as_deref(),
        &state.config.server.trusted_proxies,
    );
    let result = cmt::vote(&state.db, upvote, id, &remote_addr).await?;
    let body = match result {
        Some(vr) => json!({"likes": vr.likes, "dislikes": vr.dislikes}),
        None => Value::Null,
    };
    Ok((StatusCode::OK, Json(body)).into_response())
}

pub async fn counts(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Vec<i64>>, ApiError> {
    let arr = body
        .as_array()
        .ok_or_else(|| ApiError::BadRequest("JSON must be a list of URLs".into()))?;
    if !arr.iter().all(|v| v.is_string()) {
        return Err(ApiError::BadRequest("JSON must be a list of URLs".into()));
    }
    let uris: Vec<&str> = arr
        .iter()
        .map(|v| v.as_str().expect("verified above"))
        .collect();
    let counts = cmt::count(&state.db, &uris).await?;
    Ok(Json(counts))
}

#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    text: Option<String>,
}

pub async fn preview(
    State(state): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<Value>, ApiError> {
    let text = body
        .text
        .ok_or_else(|| ApiError::BadRequest("no text given".into()))?;
    Ok(Json(json!({"text": state.renderer.render(&text)})))
}

/// `GET /id/:id/unsubscribe/:email/:key` — turn off reply notifications for
/// a specific commenter on this thread. Key is `Signer::sign(("unsubscribe", email))`
/// with effectively unlimited max_age (Python uses 2**32 seconds).
pub async fn unsubscribe(
    State(state): State<AppState>,
    Path((id, email, key)): Path<(i64, String, String)>,
) -> Result<Response, ApiError> {
    let email = urlencoding::decode(&email)
        .map(|s| s.into_owned())
        .unwrap_or(email);
    let payload: (String, String) = state
        .signer
        .unsign(&key, None, now_unix() as u64)
        .map_err(|e| ApiError::Forbidden(format!("invalid key: {e}")))?;
    if payload.0 != "unsubscribe" || payload.1 != email {
        return Err(ApiError::Forbidden("key / email mismatch".into()));
    }
    if cmt::get(&state.db, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    cmt::unsubscribe(&state.db, &email, id).await?;

    let html = "<!DOCTYPE html><html><head><title>Successfully unsubscribed</title></head>\
        <body><p>You have been unsubscribed from replies in the given conversation.</p></body></html>";
    let mut resp = (StatusCode::OK, html).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

/// `GET/POST /id/:id/:action/:key` — moderation.
///
/// The `key` is `Signer::sign(comment_id)` with a long max_age; it's the
/// same signature admin emails include in their Delete/Activate links.
///
/// GET returns an HTML confirmation page that POSTs the same URL from JS.
/// POST performs the action (activate / edit / delete) and returns plain text
/// or JSON depending on the action.
#[derive(Debug, Deserialize)]
pub struct ModerateEditBody {
    text: Option<String>,
    author: Option<String>,
    website: Option<String>,
}

pub async fn moderate_get(
    State(state): State<AppState>,
    Path((id, action, key)): Path<(i64, String, String)>,
) -> Result<Response, ApiError> {
    let _signed_id = moderate_verify(&state, id, &key)?;
    let item = cmt::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    let thread = crate::db::threads::get_by_id(&state.db, item.tid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let link = format!("{}{}#isso-{}", public_endpoint(&state), thread.uri, item.id);
    // Build an HTML page that POSTs back to the same URL after user
    // confirmation, matching isso/views/comments.py::moderate's GET modal.
    let action_cap = capitalize(&action);
    let link_json = serde_json::to_string(&link).unwrap_or_else(|_| "\"\"".to_string());
    let html = format!(
        "<!DOCTYPE html>\
         <html>\
         <head><title>{action_cap}</title></head>\
         <body>\
         <script>\
           if (confirm('{action_cap}: Are you sure?')) {{\
             var xhr = new XMLHttpRequest();\
             xhr.open('POST', window.location.href);\
             xhr.send(null);\
             xhr.onload = function() {{ window.location.href = {link_json}; }};\
           }}\
         </script>\
         </body>\
         </html>"
    );
    let mut resp = (StatusCode::OK, html).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

pub async fn moderate_post(
    State(state): State<AppState>,
    Path((id, action, key)): Path<(i64, String, String)>,
    body: Option<Json<ModerateEditBody>>,
) -> Result<Response, ApiError> {
    let signed_id = moderate_verify(&state, id, &key)?;
    let item = cmt::get(&state.db, signed_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let thread = crate::db::threads::get_by_id(&state.db, item.tid)
        .await?
        .ok_or(ApiError::NotFound)?;

    match action.as_str() {
        "activate" => {
            if item.mode == 1 {
                return Ok((StatusCode::OK, "Already activated").into_response());
            }
            cmt::activate(&state.db, signed_id).await?;
            // Refresh after the update and fire the activate notification.
            if let Some(activated) = cmt::get(&state.db, signed_id).await? {
                state.notifier.comment_activated(&thread, &activated);
            }
            Ok((StatusCode::OK, "Comment has been activated").into_response())
        }
        "delete" => {
            cmt::delete(&state.db, signed_id).await?;
            Ok((StatusCode::OK, "Comment has been deleted").into_response())
        }
        "edit" => {
            let body = body.ok_or_else(|| {
                ApiError::BadRequest("edit requires a JSON body with text/author/website".into())
            })?;
            verify_comment(
                body.0.text.as_deref(),
                body.0.author.as_deref(),
                body.0.website.as_deref(),
                None,
            )
            .map_err(ApiError::BadRequest)?;
            let text = body.0.text.clone().expect("verified above");
            let author = body.0.author.clone().map(|a| html_escape(&a, false));
            let website = body.0.website.clone().map(|w| html_escape(&w, true));
            let patch = CommentUpdate {
                text: Some(&text),
                author: Some(author.as_deref()),
                website: Some(website.as_deref()),
                modified: Some(now_unix()),
                ..Default::default()
            };
            let updated = cmt::update(&state.db, signed_id, &patch)
                .await?
                .ok_or(ApiError::NotFound)?;
            let json_body = serde_json::to_value(render_comment(updated, &state, true))
                .map_err(|e| ApiError::Internal(e.into()))?;
            Ok((StatusCode::OK, Json(json_body)).into_response())
        }
        other => Err(ApiError::BadRequest(format!("unknown action: {other}"))),
    }
}

fn moderate_verify(state: &AppState, id: i64, key: &str) -> Result<i64, ApiError> {
    let signed: i64 = state
        .signer
        .unsign(key, None, now_unix() as u64)
        .map_err(|e| ApiError::Forbidden(format!("invalid key: {e}")))?;
    if signed != id {
        return Err(ApiError::Forbidden("key / id mismatch".into()));
    }
    Ok(signed)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn public_endpoint(state: &AppState) -> String {
    if state.config.server.public_endpoint.is_empty() {
        state
            .config
            .general
            .hosts
            .first()
            .cloned()
            .unwrap_or_default()
    } else {
        state.config.server.public_endpoint.clone()
    }
    .trim_end_matches('/')
    .to_string()
}

/// `GET /latest?limit=N` — cross-thread list of the N newest accepted
/// comments. Disabled unless `[general] latest-enabled = true`.
#[derive(Debug, Deserialize)]
pub struct LatestQuery {
    limit: Option<String>,
}

pub async fn latest(
    State(state): State<AppState>,
    Query(q): Query<LatestQuery>,
) -> Result<Response, ApiError> {
    if !state.config.general.latest_enabled {
        return Err(ApiError::NotFound);
    }
    let limit: i64 = q
        .limit
        .ok_or_else(|| {
            ApiError::BadRequest("Query parameter 'limit' is mandatory (integer, >0)".into())
        })?
        .parse()
        .map_err(|_| {
            ApiError::BadRequest("Query parameter 'limit' is mandatory (integer, >0)".into())
        })?;
    if limit <= 0 {
        return Err(ApiError::BadRequest(
            "Query parameter 'limit' is mandatory (integer, >0)".into(),
        ));
    }
    let rows = cmt::fetch_latest(&state.db, limit).await?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(c, uri)| {
            let mut rendered = serde_json::to_value(render_comment(c, &state, true))
                .expect("render_comment produces JSON-safe output");
            if let Some(obj) = rendered.as_object_mut() {
                obj.insert("uri".to_string(), Value::String(uri));
            }
            rendered
        })
        .collect();
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// `GET /feed?uri=...` — Atom feed for a thread's accepted comments.
/// Disabled unless `[rss] base` is set.
pub async fn feed(
    State(state): State<AppState>,
    Query(q): Query<FetchQuery>,
) -> Result<Response, ApiError> {
    let base = state.config.rss.base.trim_end_matches('/');
    if base.is_empty() {
        return Err(ApiError::NotFound);
    }
    let hostname = url::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();

    let limit: i64 = state.config.rss.limit as i64;
    let params = cmt::FetchParams {
        uri: &q.uri,
        mode: 1, // Atom feed shows only accepted comments.
        after: 0.0,
        parent: None,
        order_by: cmt::OrderBy::Id,
        asc: false,
        limit: Some(limit),
        offset: 0,
    };
    let comments = cmt::fetch(&state.db, &params).await?;

    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\" xmlns:thr=\"http://purl.org/syndication/thread/1.0\">\n");
    xml.push_str(&format!(
        "  <id>tag:{hostname},2018:/isso/thread{uri}</id>\n",
        uri = xml_escape(&q.uri)
    ));
    xml.push_str(&format!(
        "  <title>Comments for {hostname}{uri}</title>\n",
        uri = xml_escape(&q.uri)
    ));
    let newest_ts = comments
        .first()
        .map(|c| c.modified.unwrap_or(c.created))
        .unwrap_or(0.0);
    xml.push_str(&format!(
        "  <updated>{}</updated>\n",
        iso8601_utc(newest_ts)
    ));
    for c in &comments {
        xml.push_str("  <entry>\n");
        xml.push_str(&format!(
            "    <id>tag:{hostname},2018:/isso/{tid}/{cid}</id>\n",
            tid = c.tid,
            cid = c.id
        ));
        xml.push_str(&format!("    <title>Comment #{}</title>\n", c.id));
        xml.push_str(&format!(
            "    <updated>{}</updated>\n",
            iso8601_utc(c.modified.unwrap_or(c.created))
        ));
        if let Some(author) = c.author.as_deref() {
            xml.push_str("    <author><name>");
            xml.push_str(&xml_escape(author));
            xml.push_str("</name></author>\n");
        }
        xml.push_str(&format!(
            "    <link href=\"{base}{uri}#isso-{cid}\"/>\n",
            base = xml_escape(base),
            uri = xml_escape(&q.uri),
            cid = c.id
        ));
        xml.push_str("    <content type=\"html\">");
        xml.push_str(&xml_escape(&state.renderer.render(&c.text)));
        xml.push_str("</content>\n");
        if let Some(parent_id) = c.parent {
            xml.push_str(&format!(
                "    <thr:in-reply-to ref=\"tag:{hostname},2018:/isso/{tid}/{pid}\" \
                 href=\"{base}{uri}#isso-{pid}\"/>\n",
                tid = c.tid,
                pid = parent_id,
                base = xml_escape(base),
                uri = xml_escape(&q.uri),
            ));
        }
        xml.push_str("  </entry>\n");
    }
    xml.push_str("</feed>\n");

    let mut resp = (StatusCode::OK, xml).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/atom+xml; charset=utf-8"),
    );
    Ok(resp)
}

/// Minimal XML text escape. We only emit ASCII-safe fields (URIs, authors,
/// rendered HTML that's already sanitised), so this is enough.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Admin UI & login.
// ---------------------------------------------------------------------------

use axum::extract::Form;
use minijinja::context;

/// Serve the login form (GET /login/ renders, POST validates password and
/// sets the admin-session cookie that /admin/ requires).
///
/// If `[admin] enabled = false` the endpoint instead renders the `disabled`
/// template so the operator sees a useful message rather than a 404.
pub async fn login_get(State(state): State<AppState>) -> Response {
    render_login_or_disabled(&state)
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    password: Option<String>,
}

pub async fn login_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !state.config.admin.enabled {
        return render_login_or_disabled(&state);
    }
    let supplied = form.password.unwrap_or_default();
    if supplied.is_empty() || supplied != state.config.admin.password {
        return render_login_or_disabled(&state);
    }
    // Sign an admin-session payload; the admin endpoint accepts tokens
    // valid for 24 hours (Python's max_age=60*60*24).
    let token = match state.signer.sign(&json!({"logged": true})) {
        Ok(t) => t,
        Err(e) => {
            return ApiError::Internal(anyhow::anyhow!("sign: {e}")).into_response();
        }
    };
    // Compute the redirect target: current URL with "/login/" → "/admin/".
    let location = redirect_to_admin(&headers);
    let cookie = super::build_cookie("admin-session", &token, 60 * 60 * 24, &state.config);
    let x_cookie = super::build_cookie("isso-admin-session", &token, 60 * 60 * 24, &state.config);

    let mut resp = (StatusCode::SEE_OTHER, "").into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        location
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("/admin/")),
    );
    resp.headers_mut().append(header::SET_COOKIE, cookie);
    resp.headers_mut().append("X-Set-Cookie", x_cookie);
    resp
}

fn redirect_to_admin(headers: &HeaderMap) -> String {
    // Use the Host + protocol the request came in on; fall back to "/admin/".
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // We always redirect to /admin/ at the same host.
    if host.is_empty() {
        "/admin/".into()
    } else {
        // Browsers accept a relative URL here; keep it path-only so behind a
        // reverse proxy with path rewriting we don't strand the user.
        "/admin/".into()
    }
}

fn render_login_or_disabled(state: &AppState) -> Response {
    let template = if state.config.admin.enabled {
        "login.html"
    } else {
        "disabled.html"
    };
    render_admin_template(
        state,
        template,
        context! { isso_host_script => isso_host_script(state) },
    )
}

fn isso_host_script(state: &AppState) -> String {
    if state.config.server.public_endpoint.is_empty() {
        state
            .config
            .general
            .hosts
            .first()
            .cloned()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string()
    } else {
        state
            .config
            .server
            .public_endpoint
            .trim_end_matches('/')
            .to_string()
    }
}

fn render_admin_template(
    _state: &AppState,
    template: &str,
    ctx: impl serde::Serialize,
) -> Response {
    let env = crate::templates::env();
    let tmpl = match env.get_template(template) {
        Ok(t) => t,
        Err(e) => return ApiError::Internal(anyhow::anyhow!("template load: {e}")).into_response(),
    };
    let rendered = match tmpl.render(ctx) {
        Ok(s) => s,
        Err(e) => {
            return ApiError::Internal(anyhow::anyhow!("template render: {e}")).into_response()
        }
    };
    let mut resp = (StatusCode::OK, rendered).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

#[derive(Debug, Deserialize)]
pub struct AdminQuery {
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    order_by: Option<String>,
    #[serde(default)]
    asc: Option<i64>,
    #[serde(default)]
    mode: Option<i64>,
    #[serde(default)]
    comment_search_url: Option<String>,
}

/// `GET /admin/` — HTML admin dashboard. Validates the admin-session cookie
/// and renders admin.html with the comment listing. The template relies on
/// `/js/admin.js` at the same host for client-side actions (edit, delete,
/// validate). We don't bundle static assets in isso-rs — operators serve the
/// isso JS from the Python package or their own static tree.
pub async fn admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AdminQuery>,
) -> Response {
    let isso_host = isso_host_script(&state);
    if !state.config.admin.enabled {
        return render_admin_template(
            &state,
            "disabled.html",
            context! { isso_host_script => isso_host },
        );
    }

    // Validate the admin-session cookie.
    let cookie = cookie_for_name("admin-session", &headers);
    let logged = cookie
        .and_then(|tok| {
            state
                .signer
                .unsign::<Value>(&tok, Some(60 * 60 * 24), now_unix() as u64)
                .ok()
        })
        .and_then(|v| v.get("logged").and_then(|l| l.as_bool()))
        .unwrap_or(false);
    if !logged {
        return render_admin_template(
            &state,
            "login.html",
            context! { isso_host_script => isso_host },
        );
    }

    // Query parameters.
    let page = q.page.unwrap_or(0).max(0);
    let order_by_str = q.order_by.unwrap_or_else(|| "created".into());
    let order_by = match order_by_str.as_str() {
        "id" => cmt::AdminOrderBy::Id,
        "created" => cmt::AdminOrderBy::Created,
        "modified" => cmt::AdminOrderBy::Modified,
        "likes" => cmt::AdminOrderBy::Likes,
        "dislikes" => cmt::AdminOrderBy::Dislikes,
        "tid" => cmt::AdminOrderBy::Tid,
        _ => cmt::AdminOrderBy::Created,
    };
    let asc = q.asc.unwrap_or(0) != 0;
    let mode = q.mode.unwrap_or(2);
    let comment_search_url = q.comment_search_url.clone().unwrap_or_default();

    let (search_comment_id, search_uri) = if comment_search_url.is_empty() {
        (None, None)
    } else {
        parse_search_url(&comment_search_url)
    };
    let params = cmt::AdminFetchParams {
        mode,
        order_by,
        asc,
        limit: 100,
        page,
        comment_id: search_comment_id,
        thread_uri: search_uri.as_deref(),
    };

    let rows = match cmt::fetch_admin(&state.db, &params).await {
        Ok(r) => r,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let counts = match cmt::count_by_mode(&state.db).await {
        Ok(c) => c,
        Err(e) => return ApiError::from(e).into_response(),
    };

    let comments_ctx: Vec<Value> = rows
        .iter()
        .map(|row| {
            let c = &row.comment;
            let hash = state
                .signer
                .sign(&c.id)
                .unwrap_or_else(|_| String::from("<sign-failed>"));
            json!({
                "id": c.id,
                "tid": c.tid,
                "title": row.title,
                "uri": row.uri,
                "parent": c.parent,
                "created": c.created,
                "modified": c.modified,
                "mode": c.mode,
                "author": c.author,
                "email": c.email,
                "website": c.website,
                "text": c.text,
                "likes": c.likes,
                "dislikes": c.dislikes,
                "hash": hash,
            })
        })
        .collect();
    // The template uses {{counts.valid}} / {{counts.pending}} / {{counts.staled}}
    // — named fields keyed by mode (1/2/4). Materialize that here so the
    // template doesn't need a `dict.get(key, default)` lookup.
    let map: std::collections::HashMap<i64, i64> = counts.iter().copied().collect();
    let counts_ctx = json!({
        "valid": map.get(&1).copied().unwrap_or(0),
        "pending": map.get(&2).copied().unwrap_or(0),
        "staled": map.get(&4).copied().unwrap_or(0),
    });
    let max_page = counts.iter().map(|(_, n)| n).sum::<i64>() / 100;

    let conf_public = json!({
        "avatar": false,
        "votes": true,
    });

    render_admin_template(
        &state,
        "admin.html",
        context! {
            isso_host_script => isso_host,
            comments => comments_ctx,
            counts => counts_ctx,
            page => page,
            mode => mode,
            max_page => max_page,
            order_by => order_by_str,
            asc => asc as i64,
            comment_search_url => comment_search_url,
            conf => conf_public,
        },
    )
}

fn cookie_for_name(name: &str, headers: &HeaderMap) -> Option<String> {
    let hdr = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in hdr.split(';') {
        let (k, v) = part.trim().split_once('=')?;
        if k == name {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse a comment URL `http://site/path#isso-<id>` into `(comment_id, thread_uri)`.
/// If the fragment is missing, only `thread_uri` is populated.
fn parse_search_url(url: &str) -> (Option<i64>, Option<String>) {
    let parsed = match url::Url::parse(url) {
        Ok(p) => p,
        Err(_) => return (None, None),
    };
    let path = if parsed.path().is_empty() {
        None
    } else {
        Some(parsed.path().to_string())
    };
    let fragment = parsed.fragment().unwrap_or("");
    let id = fragment
        .rsplit('-')
        .next()
        .and_then(|s| s.parse::<i64>().ok());
    (id, path)
}

/// Render a unix timestamp as `YYYY-MM-DDTHH:MM:SSZ` (UTC, second-resolution).
/// Matches the Python `datetime.fromtimestamp(ts).isoformat() + "Z"` output.
fn iso8601_utc(ts: f64) -> String {
    if ts <= 0.0 {
        return "1970-01-01T01:00:00Z".to_string();
    }
    use time::OffsetDateTime;
    let secs = ts as i64;
    let odt = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        odt.year(),
        odt.month() as u8,
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
}
