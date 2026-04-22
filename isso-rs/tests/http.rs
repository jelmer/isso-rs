//! End-to-end HTTP tests against the axum router.
//!
//! We stand up the full stack (in-memory SQLite + handler chain) and drive
//! it with raw Request/Response objects. These tests exist to catch
//! integration-level regressions the unit tests can't — cookie signing,
//! CSRF rejection, JSON shape, status codes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

use isso_rs::config::Config;
use isso_rs::hash::Hasher;
use isso_rs::markdown::Renderer;
use isso_rs::server::{router, AppState};
use isso_rs::signer::Signer;

async fn test_state() -> AppState {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    // Mirror the DB schema from src/db.rs — we can't call db::open against
    // ":memory:" because sqlite's URL handling differs there. Run the
    // CREATE statements directly.
    sqlx::query("CREATE TABLE threads (id INTEGER PRIMARY KEY, uri VARCHAR UNIQUE, title VARCHAR)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE preferences (key VARCHAR PRIMARY KEY, value VARCHAR)")
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
    sqlx::query("INSERT INTO preferences (key, value) VALUES ('session-key', 'test-session-key')")
        .execute(&pool)
        .await
        .unwrap();

    let mut config = Config::default();
    config.general.dbpath = ":memory:".into();
    AppState {
        config: Arc::new(config),
        db: pool,
        hasher: Arc::new(Hasher::from_config("pbkdf2", "Eech7co8Ohloopo9Ol6baimi").unwrap()),
        signer: Arc::new(Signer::new(b"test-session-key")),
        renderer: Arc::new(Renderer::new()),
    }
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn cors_echoes_matching_origin() {
    // Two configured hosts; the caller's Origin matches the second, so we
    // expect it back verbatim in Access-Control-Allow-Origin.
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.general.hosts = vec!["https://example.tld/".into(), "http://example.tld/".into()];
        c
    });
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .header(header::ORIGIN, "http://example.tld")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "http://example.tld"
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-credentials")
            .unwrap(),
        "true"
    );
    assert_eq!(
        resp.headers().get("access-control-allow-methods").unwrap(),
        "HEAD, GET, POST, PUT, DELETE"
    );
}

#[tokio::test]
async fn cors_preflight_short_circuits() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/new?uri=/x")
                .header(header::ORIGIN, "http://localhost:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Every CORS header is present even though no handler matched OPTIONS.
    for hdr in [
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-allow-methods",
    ] {
        assert!(resp.headers().contains_key(hdr), "missing header: {hdr}");
    }
}

#[tokio::test]
async fn get_config_returns_public_knobs() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(
        j,
        json!({
            "reply-to-self": false,
            "require-email": false,
            "require-author": false,
            "reply-notifications": false,
            "gravatar": false,
            "avatar": false,
            "feed": false,
        })
    );
}

#[tokio::test]
async fn post_new_requires_json_content_type() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/post-1")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("text=hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_new_rejects_short_text() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/post-1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"text": "hi", "title": "P"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = String::from_utf8(body_bytes(resp).await).unwrap();
    assert_eq!(msg, "text is too short (minimum length: 3)");
}

#[tokio::test]
async fn post_new_creates_thread_and_returns_cookie() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/post-2")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "text": "hello world",
                        "author": "jane",
                        "email": "jane@example.com",
                        "title": "Post 2",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookies: Vec<_> = resp.headers().get_all(header::SET_COOKIE).iter().collect();
    assert_eq!(cookies.len(), 1, "expected one Set-Cookie header");
    let x_set: Vec<_> = resp.headers().get_all("x-set-cookie").iter().collect();
    assert_eq!(x_set.len(), 1, "expected one X-Set-Cookie header");

    let j = body_json(resp).await;
    assert_eq!(j["id"], json!(1));
    assert_eq!(j["parent"], Value::Null);
    assert_eq!(j["author"], json!("jane"));
    assert_eq!(j["mode"], json!(1));
    assert_eq!(j["text"], json!("<p>hello world</p>"));
    assert!(j["hash"].is_string());
}

#[tokio::test]
async fn get_empty_thread_returns_empty_replies() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/?uri=/never-posted")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["id"], Value::Null);
    assert_eq!(j["total_replies"], json!(0));
    assert_eq!(j["replies"], json!([]));
}

#[tokio::test]
async fn post_then_get_roundtrip() {
    // Two comments on the same thread — second should be a top-level reply
    // (no explicit parent). Fetch the thread and assert on the shape.
    let app = router(test_state().await);

    for (n, text) in [(1, "first"), (2, "second")] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/new?uri=/roundtrip")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "text": format!("{text} body"),
                            "title": "Roundtrip",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "insert {n} failed");
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/?uri=/roundtrip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["total_replies"], json!(2));
    let replies = j["replies"].as_array().unwrap();
    let texts: Vec<&str> = replies
        .iter()
        .map(|r| r["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, vec!["<p>first body</p>", "<p>second body</p>"]);
}

#[tokio::test]
async fn vote_updates_like_count_and_rejects_duplicate() {
    let app = router(test_state().await);
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/voted")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"text": "vote me", "title": "V"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // First like from a fresh IP.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/id/1/like")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "10.0.0.2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j, json!({"likes": 1, "dislikes": 0}));

    // Same IP — bloomfilter rejects, count stays at 1.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/id/1/like")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", "10.0.0.2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j, json!({"likes": 1, "dislikes": 0}));
}

#[tokio::test]
async fn preview_renders_markdown() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/preview")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"text": "hi **world**"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j, json!({"text": "<p>hi <strong>world</strong></p>"}));
}

#[tokio::test]
async fn post_count_returns_parallel_list() {
    let app = router(test_state().await);
    // Post one comment on /a, zero on /b.
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/a")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"text": "one comment", "title": "A"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/count")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!(["/a", "/b"])).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j, json!([1, 0]));
}
