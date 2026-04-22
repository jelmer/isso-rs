//! End-to-end tests for the thread-title fetch path used by POST /new.
//!
//! These spin up a tiny axum server on an ephemeral port to stand in for
//! the author's blog, and then exercise `thread_title::fetch` + the
//! `new_comment` HTTP handler against it. They cover three scenarios:
//!
//! 1. `fetch()` against a live server that serves a page with `#isso-thread
//!    data-title="X"` returns `X`.
//! 2. `POST /new` with no `title` in the body correctly resolves the
//!    thread title from the live server and inserts a thread with that title.
//! 3. `POST /new` with no `title` and no reachable host replies with a 400,
//!    matching the Python implementation's behaviour.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tower::ServiceExt;

use isso_rs::config::Config;
use isso_rs::hash::Hasher;
use isso_rs::markdown::Renderer;
use isso_rs::notify::Notifier;
use isso_rs::server::{router, AppState};
use isso_rs::signer::Signer;
use isso_rs::thread_title;

/// Spawn an axum server that serves a single `/article.html` with the
/// given body. Returns `http://<addr>` so callers can configure it as
/// the isso `[general] host`.
async fn spawn_blog(body_html: &'static str) -> String {
    let app = axum::Router::new().route(
        "/article.html",
        get(move || async move {
            let mut resp = body_html.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// Re-export axum's IntoResponse for the closure above.
use axum::response::IntoResponse;

#[tokio::test]
async fn fetch_reads_data_title_from_live_server() {
    let blog = spawn_blog(
        r#"<!doctype html>
<html><body>
  <h1>Unused</h1>
  <section id="isso-thread" data-title="Thread from data-title"></section>
</body></html>"#,
    )
    .await;
    let hosts = vec![blog];
    let got = thread_title::fetch(&hosts, "/article.html").await.unwrap();
    let expected = thread_title::ResolvedThread {
        uri: "/article.html".into(),
        title: "Thread from data-title".into(),
    };
    assert_eq!(got, expected);
}

#[tokio::test]
async fn fetch_falls_back_to_nearest_h1_when_no_data_title() {
    let blog = spawn_blog(
        r#"<!doctype html>
<html><body>
  <article>
    <h1>The article's own heading</h1>
    <div><div id="isso-thread"></div></div>
  </article>
</body></html>"#,
    )
    .await;
    let hosts = vec![blog];
    let got = thread_title::fetch(&hosts, "/article.html").await.unwrap();
    let expected = thread_title::ResolvedThread {
        uri: "/article.html".into(),
        title: "The article's own heading".into(),
    };
    assert_eq!(got, expected);
}

#[tokio::test]
async fn fetch_returns_err_when_no_host_answers() {
    // 127.0.0.1:1 — reserved low port, nothing ever listens there.
    let hosts = vec!["http://127.0.0.1:1".into()];
    let err = thread_title::fetch(&hosts, "/article.html")
        .await
        .expect_err("expected every-host-failed error");
    // The error string should carry the URL so operators can tell which
    // host path was attempted.
    let msg = err.to_string();
    assert!(
        msg.contains("127.0.0.1:1/article.html"),
        "expected URL in error message, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Handler-level round-trips
// ---------------------------------------------------------------------------

async fn handler_state(hosts: Vec<String>) -> AppState {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
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
    sqlx::query("INSERT INTO preferences (key, value) VALUES ('session-key', 'test-session-key')")
        .execute(&pool)
        .await
        .unwrap();

    let mut config = Config::default();
    config.general.dbpath = ":memory:".into();
    config.general.hosts = hosts;
    let config = Arc::new(config);
    let signer = Arc::new(Signer::new(b"test-session-key"));
    let notifier = Arc::new(Notifier::new(Arc::clone(&config), Arc::clone(&signer)));
    AppState {
        config,
        db: pool,
        hasher: Arc::new(Hasher::from_config("pbkdf2", "Eech7co8Ohloopo9Ol6baimi").unwrap()),
        signer,
        renderer: Arc::new(Renderer::new()),
        notifier,
    }
}

#[tokio::test]
async fn post_new_without_title_fetches_from_configured_host() {
    let blog = spawn_blog(
        r#"<!doctype html>
<html><body>
  <h1>Resolved by fetch</h1>
  <section id="isso-thread"></section>
</body></html>"#,
    )
    .await;
    let state = handler_state(vec![blog]).await;
    let pool = state.db.clone();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/article.html")
                .header(header::CONTENT_TYPE, "application/json")
                // Deliberately no `title` field.
                .body(Body::from(
                    serde_json::to_vec(&json!({ "text": "first!" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The thread row should exist with the title scraped from the blog's <h1>.
    let (uri, title): (String, Option<String>) = sqlx::query_as("SELECT uri, title FROM threads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(uri, "/article.html");
    assert_eq!(title.as_deref(), Some("Resolved by fetch"));
}

#[tokio::test]
async fn post_new_without_title_and_no_reachable_host_returns_400() {
    let state = handler_state(vec!["http://127.0.0.1:1".into()]).await;
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/new?uri=/article.html")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "text": "first!" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        msg.contains("not accessible") && msg.contains("/article.html"),
        "got: {msg}"
    );
}
