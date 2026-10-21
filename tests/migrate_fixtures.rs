//! Integration tests for the migrators against the real-world fixture dumps
//! (originally from isso's Python test suite). These are end-to-end checks
//! that the parsers survive the weirdness of actual Disqus/WordPress export
//! files — empty threads, duplicate ids, CDATA blocks, etc.

use sqlx::SqlitePool;

use isso_rs::db;
use isso_rs::migrate;

async fn open_mem() -> SqlitePool {
    db::open(":memory:").await.unwrap()
}

#[tokio::test]
async fn disqus_fixture_imports_without_error() {
    let pool = open_mem().await;
    let report = migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool,
        false,
        false,
    )
    .await
    .unwrap();

    // Fixture has several threads, some empty. At least one real thread
    // must insert successfully, and the orphan count should be low.
    assert!(
        report.threads_inserted >= 1,
        "expected ≥1 thread, got {}: {report:?}",
        report.threads_inserted
    );
    assert!(
        report.comments_inserted >= 1,
        "expected ≥1 comment, got {}: {report:?}",
        report.comments_inserted
    );
}

#[tokio::test]
async fn disqus_fixture_with_empty_id_flag_imports_more() {
    // --empty-id keeps threads with empty <id/> elements (Python workaround
    // for weird exports). The count with the flag should be ≥ count without.
    let pool1 = open_mem().await;
    let without = migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool1,
        false,
        false,
    )
    .await
    .unwrap();

    let pool2 = open_mem().await;
    let with = migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool2,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(
        with.threads_inserted >= without.threads_inserted,
        "with empty-id: {with:?} vs without: {without:?}"
    );
}

#[tokio::test]
async fn wordpress_fixture_imports_without_error() {
    let pool = open_mem().await;
    let report = migrate::dispatch(
        "wordpress",
        std::path::Path::new("tests/fixtures/wordpress.xml"),
        &pool,
        false,
        false,
    )
    .await
    .unwrap();
    assert!(
        report.threads_inserted >= 1,
        "expected ≥1 thread, got {report:?}"
    );
    assert!(
        report.comments_inserted >= 1,
        "expected ≥1 comment, got {report:?}"
    );
}

#[tokio::test]
async fn autodetect_recognizes_all_fixtures() {
    use migrate::Kind;
    let disqus = std::fs::read_to_string("tests/fixtures/disqus.xml").unwrap();
    assert!(matches!(migrate::autodetect(&disqus), Some(Kind::Disqus)));

    let wp = std::fs::read_to_string("tests/fixtures/wordpress.xml").unwrap();
    assert!(matches!(migrate::autodetect(&wp), Some(Kind::WordPress)));
}

async fn thread_titles(pool: &SqlitePool) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT uri, title FROM threads ORDER BY uri")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn comment_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM comments")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn disqus_skips_threads_without_id_by_default() {
    // Only dsq:id="2" (<id>some-id</id>) has an identifier; it is keyed by
    // its URL path.
    let pool = open_mem().await;
    migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(comment_count(&pool).await, 4);
    assert_eq!(
        thread_titles(&pool).await,
        vec![("/".to_string(), Some("Hello, World!".to_string()))]
    );
}

#[tokio::test]
async fn disqus_thread_id_keys_threads_by_identifier() {
    let pool = open_mem().await;
    migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool,
        true,
        true,
    )
    .await
    .unwrap();
    assert_eq!(comment_count(&pool).await, 5);
    // dsq:id="2" is keyed by its <id>; dsq:id="7" has an empty <id /> and
    // falls back to its URL path.
    assert_eq!(
        thread_titles(&pool).await,
        vec![
            (
                "/blog/2013/02/12/redacted/index.html".to_string(),
                Some(String::new())
            ),
            ("some-id".to_string(), Some("Hello, World!".to_string())),
        ]
    );
}
