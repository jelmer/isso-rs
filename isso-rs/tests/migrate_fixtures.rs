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
    )
    .await
    .unwrap();

    let pool2 = open_mem().await;
    let with = migrate::dispatch(
        "disqus",
        std::path::Path::new("tests/fixtures/disqus.xml"),
        &pool2,
        true,
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
