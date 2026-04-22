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
use isso_rs::notify::Notifier;
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
async fn moderate_activate_flips_mode_to_accepted() {
    // Emulate the moderation email click flow: insert a pending comment
    // then hit /id/:id/activate/:key with a key signed for that id.
    let state = test_state().await;
    let key = state.signer.sign(&1_i64).unwrap();
    let signer_key = key.clone();
    let app = router(state.clone());

    // Moderation is only enabled when [moderation] enabled=true — without it
    // POST /new returns mode=1. For the test, insert a pending comment directly.
    sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/m', 'M')")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, \
         author, email, website, voters, notification) \
         VALUES (1, NULL, 1000.0, 2, '127.0.0.0', 'pending', NULL, NULL, NULL, zeroblob(256), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/id/1/activate/{signer_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert_eq!(body, "Comment has been activated");

    // Mode must have moved 2 -> 1.
    let mode: i64 = sqlx::query_scalar("SELECT mode FROM comments WHERE id = 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(mode, 1);
}

#[tokio::test]
async fn moderate_rejects_wrong_key() {
    let state = test_state().await;
    let app = router(state.clone());
    sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/m', 'M')")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, voters, notification) \
         VALUES (1, NULL, 1.0, 2, '127.0.0.0', 'x', zeroblob(256), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    // Key is signed for id=999, not the comment we're trying to activate.
    let bad_key = state.signer.sign(&999_i64).unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/id/1/activate/{bad_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unsubscribe_turns_off_notifications() {
    let state = test_state().await;
    sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/t', 'T')")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, email, voters, notification) \
         VALUES (1, NULL, 1.0, 1, '127.0.0.0', 'hi', 'jane@example.com', zeroblob(256), 1)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    let email = "jane@example.com";
    let key = state.signer.sign(&("unsubscribe", email)).unwrap();
    let app = router(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/id/1/unsubscribe/{email}/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // notification flag now 0.
    let n: i64 = sqlx::query_scalar("SELECT notification FROM comments WHERE id = 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn latest_requires_feature_flag_and_limit() {
    // Without latest-enabled = true we 404.
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/latest?limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // With the flag on but missing/invalid limit → 400.
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.general.latest_enabled = true;
        c
    });
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/latest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn feed_disabled_unless_rss_base_set() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/feed?uri=/any")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn feed_returns_atom_xml_when_enabled() {
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.rss.base = "https://comments.example.com".into();
        c
    });
    let state_db = state.db.clone();
    let app = router(state.clone());
    sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/p', 'P')")
        .execute(&state_db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, author, voters, notification) \
         VALUES (1, NULL, 1000.0, 1, '127.0.0.0', 'hello', 'jane', zeroblob(256), 0)",
    )
    .execute(&state_db)
    .await
    .unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/feed?uri=/p")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/atom+xml; charset=utf-8"
    );
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.contains("<feed"), "got: {body}");
    assert!(body.contains("<title>Comments for comments.example.com/p</title>"));
    assert!(body.contains("jane"));
}

#[tokio::test]
async fn admin_disabled_renders_disabled_html() {
    let app = router(test_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(
        body.contains("Administration is disabled on this instance"),
        "got: {body}"
    );
}

#[tokio::test]
async fn admin_login_required_without_cookie() {
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.admin.enabled = true;
        c.admin.password = "hunter2".into();
        c
    });
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(
        body.contains("<form method=\"POST\"") && body.contains("/login/"),
        "expected login.html, got: {body}"
    );
}

#[tokio::test]
async fn admin_login_wrong_password_reshows_form() {
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.admin.enabled = true;
        c.admin.password = "hunter2".into();
        c
    });
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login/")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("password=wrong"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.contains("<form method=\"POST\""), "got: {body}");
}

#[tokio::test]
async fn admin_login_right_password_sets_session_cookie_and_redirects() {
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.admin.enabled = true;
        c.admin.password = "hunter2".into();
        c
    });
    let signer = state.signer.clone();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login/")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("password=hunter2"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/admin/");
    // Set-Cookie was stamped and verifies.
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("admin-session cookie present");
    let value = set_cookie
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string();
    let payload: serde_json::Value = signer.unsign(&value, Some(86400), 0).unwrap();
    assert_eq!(payload, serde_json::json!({"logged": true}));
}

#[tokio::test]
async fn admin_lists_comments_with_valid_session() {
    let mut state = test_state().await;
    state.config = Arc::new({
        let mut c = (*state.config).clone();
        c.admin.enabled = true;
        c.admin.password = "hunter2".into();
        c
    });
    // Seed some comments so the admin listing has something to show.
    sqlx::query("INSERT INTO threads (id, uri, title) VALUES (1, '/adminp', 'AdminPost')")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO comments (tid, parent, created, mode, remote_addr, text, author, voters, notification) \
         VALUES (1, NULL, 1000.0, 2, '127.0.0.0', 'pending one', 'alice', zeroblob(256), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();

    // Forge the session cookie by signing {logged:true}.
    let session = state
        .signer
        .sign(&serde_json::json!({"logged": true}))
        .unwrap();
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/?mode=2")
                .header(header::COOKIE, format!("admin-session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("pending one"), "missing comment text: {body}");
    assert!(body.contains("alice"), "missing author: {body}");
    assert!(
        body.contains("class=\"label label-pending active\""),
        "pending tab not active: {body}"
    );
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
