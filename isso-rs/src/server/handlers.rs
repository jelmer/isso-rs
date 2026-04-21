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
        let email_or_author = c.email.clone().or(c.author.clone()).unwrap_or_default();
        // TODO: gravatar uses MD5 specifically; our Hasher doesn't implement md5.
        // Leave as None for now — wiring md5 is cheap but not load-bearing for MVP.
        let _ = email_or_author;
        None
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
