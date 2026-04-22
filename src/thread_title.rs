//! Fetch a thread's page over HTTP and extract its title — mirrors
//! isso/utils/http.py + isso/utils/parse.py from the Python implementation.
//!
//! When a POST /new arrives with no `title` in the body and the thread
//! doesn't exist yet, we GET the page from one of the configured
//! `[general] host`s and look for:
//!
//! 1. A `<div>` or `<section>` with `id="isso-thread"` and a `data-title`
//!    attribute → use that.
//! 2. Otherwise, walk up from `isso-thread` looking for the nearest `<h1>`
//!    in an ancestor and use its text content.
//! 3. If nothing matches, fall back to `"Untitled."` — same default the
//!    Python parser used.
//!
//! The module also carries over the `data-isso-id` override: if the
//! `isso-thread` element has a `data-isso-id` attribute, that becomes the
//! canonical thread URI (authors can decouple their comment threads from
//! URL-path changes this way).

use scraper::{Html, Selector};

/// A resolved thread identifier: the canonical URI (possibly rewritten by
/// `data-isso-id`) and its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedThread {
    pub uri: String,
    pub title: String,
}

pub const DEFAULT_TITLE: &str = "Untitled.";

/// GET `{host}{uri}` and parse the response body for a thread title.
/// Tries each host in order; first successful response wins.
///
/// Returns `None` if every host errors out. Callers should fall back to the
/// URI itself or surface a 400 in that case — matching Python's behaviour,
/// which bubbled the curl error up to the handler.
pub async fn fetch(hosts: &[String], uri: &str) -> Option<ResolvedThread> {
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "Isso/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/jelmer/isso-rs)"
        ))
        .redirect(reqwest::redirect::Policy::limited(3))
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    for host in hosts {
        let url = match join_host_uri(host, uri) {
            Some(u) => u,
            None => continue,
        };
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = match resp.text().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::debug!("body read failed for {url}: {e}");
                        continue;
                    }
                };
                return Some(extract_title(&body, uri));
            }
            Ok(resp) => {
                tracing::debug!("GET {url}: status {}", resp.status());
            }
            Err(e) => {
                tracing::debug!("GET {url} failed: {e}");
            }
        }
    }
    None
}

/// Join a configured host (e.g. `https://example.com/` or
/// `http://blog.example.org`) with a thread URI (e.g. `/post-1`) into a
/// fully-qualified URL. Handles the slash book-keeping both sides sometimes
/// get wrong.
fn join_host_uri(host: &str, uri: &str) -> Option<String> {
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    if uri.starts_with('/') {
        Some(format!("{host}{uri}"))
    } else {
        Some(format!("{host}/{uri}"))
    }
}

/// Parse `body` as HTML and extract the thread title, following the Python
/// `parse.thread` logic. `uri` is the request's URI, used as the initial
/// `id` value and potentially rewritten by a `data-isso-id` attribute.
fn extract_title(body: &str, uri: &str) -> ResolvedThread {
    let doc = Html::parse_document(body);
    let sel =
        Selector::parse("div#isso-thread, section#isso-thread").expect("static selector compiles");
    let Some(el) = doc.select(&sel).next() else {
        return ResolvedThread {
            uri: uri.to_string(),
            title: DEFAULT_TITLE.to_string(),
        };
    };

    // data-isso-id rewrites the URI if present.
    let resolved_uri = el
        .value()
        .attr("data-isso-id")
        .map(percent_decode)
        .unwrap_or_else(|| uri.to_string());

    // data-title short-circuits the <h1> walk.
    if let Some(dt) = el.value().attr("data-title") {
        return ResolvedThread {
            uri: resolved_uri,
            title: percent_decode(dt),
        };
    }

    // Walk up from the isso-thread element, looking for the nearest <h1>
    // anywhere in the subtree of each ancestor (Python's recursive search).
    let h1 = Selector::parse("h1").expect("static selector compiles");
    let mut current = Some(el);
    while let Some(node) = current {
        if let Some(heading) = node.select(&h1).next() {
            let text: String = heading.text().collect::<Vec<_>>().join("");
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return ResolvedThread {
                    uri: resolved_uri,
                    title: trimmed.to_string(),
                };
            }
        }
        current = node.parent().and_then(scraper::ElementRef::wrap);
    }

    ResolvedThread {
        uri: resolved_uri,
        title: DEFAULT_TITLE.to_string(),
    }
}

/// Lightweight percent-decode matching Python's urllib.parse.unquote
/// behaviour for the attributes we consume. Good enough for the limited
/// ASCII + UTF-8 URL-safe set we see on isso-thread's data-* attributes.
fn percent_decode(s: &str) -> String {
    urlencoding::decode(s)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_isso_thread_element_falls_back_to_default() {
        let html = "<html><body><p>no comment widget here</p></body></html>";
        let got = extract_title(html, "/post");
        assert_eq!(
            got,
            ResolvedThread {
                uri: "/post".into(),
                title: DEFAULT_TITLE.into()
            }
        );
    }

    #[test]
    fn data_title_wins_over_h1() {
        // The isso-thread element's own data-title short-circuits the h1
        // walk — Python behaviour.
        let html = r#"
            <html><body>
              <h1>Some article heading</h1>
              <section id="isso-thread" data-title="Overridden"></section>
            </body></html>
        "#;
        let got = extract_title(html, "/post");
        assert_eq!(
            got,
            ResolvedThread {
                uri: "/post".into(),
                title: "Overridden".into()
            }
        );
    }

    #[test]
    fn falls_back_to_nearest_h1_in_an_ancestor() {
        // No data-title: walk up from isso-thread's parent, then grandparent,
        // looking for an h1. The <h1> in a sibling of the thread's parent
        // should still be found because we search within the ancestor.
        let html = r#"
            <html><body>
              <article>
                <h1>The article title</h1>
                <div>
                  <div id="isso-thread"></div>
                </div>
              </article>
            </body></html>
        "#;
        let got = extract_title(html, "/post");
        assert_eq!(
            got,
            ResolvedThread {
                uri: "/post".into(),
                title: "The article title".into()
            }
        );
    }

    #[test]
    fn data_isso_id_rewrites_the_uri() {
        let html = r#"
            <html><body>
              <h1>T</h1>
              <div id="isso-thread" data-isso-id="/canonical/uri"></div>
            </body></html>
        "#;
        let got = extract_title(html, "/original/request/uri");
        assert_eq!(
            got,
            ResolvedThread {
                uri: "/canonical/uri".into(),
                title: "T".into()
            }
        );
    }

    #[test]
    fn defaults_when_isso_thread_has_no_title_and_no_h1() {
        let html = r#"
            <html><body>
              <main>
                <p>Some body text without a heading.</p>
                <div id="isso-thread"></div>
              </main>
            </body></html>
        "#;
        let got = extract_title(html, "/post");
        assert_eq!(
            got,
            ResolvedThread {
                uri: "/post".into(),
                title: DEFAULT_TITLE.into()
            }
        );
    }

    #[test]
    fn join_host_uri_normalises_slashes() {
        assert_eq!(
            join_host_uri("https://example.com/", "/post"),
            Some("https://example.com/post".to_string())
        );
        assert_eq!(
            join_host_uri("https://example.com", "/post"),
            Some("https://example.com/post".to_string())
        );
        assert_eq!(
            join_host_uri("https://example.com", "post"),
            Some("https://example.com/post".to_string())
        );
        assert_eq!(join_host_uri("", "/post"), None);
    }
}
