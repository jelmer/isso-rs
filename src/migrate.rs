//! Importers for Disqus/WordPress/Generic comment dumps, mirroring
//! isso/migrate.py.
//!
//! Each importer parses its source format, resolves parent references (which
//! need to be rewritten because Isso assigns its own comment ids on insert),
//! and feeds rows into [`crate::db::comments::add`]. The flow is:
//!
//! 1. Parse the dump (XML for Disqus/WordPress, JSON for Generic).
//! 2. Group comments by thread.
//! 3. For each thread, insert its comments in id order and keep a remap from
//!    source id → inserted id so later parent references resolve correctly.
//!
//! Called from the CLI via `isso import --type=<kind> <file>`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use serde::Deserialize;
use sqlx::SqlitePool;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::PrimitiveDateTime;

use crate::db::comments::{self as cmt, NewComment};
use crate::db::threads;
use crate::ip::anonymize;

/// Dispatch a migration run. Matches the Python `dispatch()` function.
/// `kind` can be `"disqus"`, `"wordpress"`, `"generic"`, or `"auto"`.
pub async fn dispatch(
    kind: &str,
    dump: &Path,
    pool: &SqlitePool,
    empty_id: bool,
) -> anyhow::Result<ImportReport> {
    let payload =
        fs::read_to_string(dump).map_err(|e| anyhow::anyhow!("reading {}: {e}", dump.display()))?;
    let resolved = match kind {
        "disqus" => Kind::Disqus,
        "wordpress" => Kind::WordPress,
        "generic" => Kind::Generic,
        "auto" | "" => autodetect(&payload[..payload.len().min(8192)])
            .ok_or_else(|| anyhow::anyhow!("Unknown format, abort."))?,
        other => anyhow::bail!("unknown import kind: {other}"),
    };
    match resolved {
        Kind::Disqus => Disqus::new(&payload, empty_id).migrate(pool).await,
        Kind::WordPress => WordPress::new(&payload).migrate(pool).await,
        Kind::Generic => Generic::new(&payload)?.migrate(pool).await,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Disqus,
    WordPress,
    Generic,
}

/// Autodetect the dump format by peeking at the first few KB.
pub fn autodetect(peek: &str) -> Option<Kind> {
    if peek.contains("xmlns=\"http://disqus.com") {
        return Some(Kind::Disqus);
    }
    if WordPress::detect(peek).is_some() {
        return Some(Kind::WordPress);
    }
    if peek.trim_start().starts_with("[{") {
        return Some(Kind::Generic);
    }
    None
}

#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    pub threads_inserted: usize,
    pub comments_inserted: usize,
    pub orphan_count: usize,
}

// ---------------------------------------------------------------------------
// Disqus
// ---------------------------------------------------------------------------

const DISQUS_NS: &str = "http://disqus.com";
const DISQUS_INTERNALS: &str = "http://disqus.com/disqus-internals";

/// Disqus XML importer.
///
/// The Disqus format (see
/// https://help.disqus.com/en/articles/1717164-comments-export) interleaves
/// `<thread>` and `<post>` elements at the document root. Every `<post>` has
/// a `dsq:id` attribute (internals) and a `<thread dsq:id="..."/>` reference,
/// letting us bucket posts per thread for ordered insertion.
pub struct Disqus<'a> {
    xml: &'a str,
    empty_id: bool,
}

impl<'a> Disqus<'a> {
    pub fn new(xml: &'a str, empty_id: bool) -> Self {
        Self { xml, empty_id }
    }

    pub async fn migrate(&self, pool: &SqlitePool) -> anyhow::Result<ImportReport> {
        let parsed = parse_disqus(self.xml)?;

        // Bucket posts by their referenced thread internals-id.
        let mut by_thread: HashMap<String, Vec<DisqusPost>> = HashMap::new();
        for post in &parsed.posts {
            by_thread
                .entry(post.thread_ref.clone())
                .or_default()
                .push(post.clone());
        }
        let mut report = ImportReport::default();
        let mut inserted_ids: HashSet<String> = HashSet::new();

        for thread in &parsed.threads {
            if thread.link.is_empty() {
                continue;
            }
            // Python skips duplicate empty-id threads unless --empty-id is set.
            if thread.has_empty_id && !self.empty_id {
                continue;
            }
            let Some(posts) = by_thread.get(&thread.internals_id) else {
                continue;
            };
            let path = url::Url::parse(&thread.link)
                .ok()
                .map(|u| u.path().to_string())
                .unwrap_or_else(|| thread.link.clone());
            if path.is_empty() {
                continue;
            }

            // Ensure thread row exists.
            if threads::get_by_uri(pool, &path).await?.is_none() {
                threads::new_thread(pool, &path, Some(thread.title.trim())).await?;
            }
            report.threads_inserted += 1;

            // Sort by created timestamp so parent→child insertion order is
            // guaranteed; Disqus ids are not sequential.
            let mut posts = posts.clone();
            posts.sort_by(|a, b| a.created.total_cmp(&b.created));
            let mut remap: HashMap<String, i64> = HashMap::new();
            for post in &posts {
                let parent = post.parent_ref.as_ref().and_then(|r| remap.get(r).copied());
                let nc = NewComment {
                    parent,
                    created: Some(post.created),
                    mode: post.mode,
                    remote_addr: &post.remote_addr,
                    text: &post.text,
                    author: post.author.as_deref(),
                    email: post.email.as_deref(),
                    website: None,
                    notification: 0,
                };
                let inserted = cmt::add(pool, &path, post.created, &nc).await?;
                remap.insert(post.dsq_id.clone(), inserted.id);
                inserted_ids.insert(post.dsq_id.clone());
                report.comments_inserted += 1;
            }
        }

        // Count orphan posts — those that never got inserted.
        let all_post_ids: HashSet<String> = parsed.posts.iter().map(|p| p.dsq_id.clone()).collect();
        report.orphan_count = all_post_ids.difference(&inserted_ids).count();
        Ok(report)
    }
}

#[derive(Debug, Clone)]
struct DisqusThread {
    internals_id: String,
    link: String,
    title: String,
    has_empty_id: bool,
}

#[derive(Debug, Clone)]
struct DisqusPost {
    dsq_id: String,
    thread_ref: String,
    parent_ref: Option<String>,
    text: String,
    author: Option<String>,
    email: Option<String>,
    created: f64,
    remote_addr: String,
    mode: i64,
}

#[derive(Debug, Default)]
struct DisqusParsed {
    threads: Vec<DisqusThread>,
    posts: Vec<DisqusPost>,
}

/// Minimal Disqus XML extractor built on quick-xml's event stream. We don't
/// need a full DOM — the element names we care about are well-known and
/// non-recursive.
fn parse_disqus(xml: &str) -> anyhow::Result<DisqusParsed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf: Vec<u8> = Vec::new();

    let mut out = DisqusParsed::default();
    // Stack of elements we're currently inside.
    let mut path_stack: Vec<String> = Vec::new();
    // Per-thread / per-post scratch.
    let mut cur_thread: Option<DisqusThread> = None;
    let mut cur_post: Option<DisqusPost> = None;
    let mut text_buf: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => anyhow::bail!("xml parse error: {e}"),
            Ok(Event::Eof) => break,
            Ok(Event::Start(start)) => {
                let name = strip_disqus_ns(start.name());
                path_stack.push(name.clone());
                match name.as_str() {
                    "thread" => {
                        let internals = read_internals_id(&start);
                        cur_thread = Some(DisqusThread {
                            internals_id: internals.clone(),
                            link: String::new(),
                            title: String::new(),
                            has_empty_id: false,
                        });
                    }
                    "post" => {
                        let internals = read_internals_id(&start);
                        cur_post = Some(DisqusPost {
                            dsq_id: internals,
                            thread_ref: String::new(),
                            parent_ref: None,
                            text: String::new(),
                            author: None,
                            email: None,
                            created: 0.0,
                            remote_addr: "0.0.0.0".into(),
                            mode: 1,
                        });
                    }
                    _ => {
                        text_buf = Some(String::new());
                    }
                }
                // Self-closing <thread dsq:id="..."/> inside a post refers to
                // the thread of that post.
                let local_path = path_stack.join("/");
                if local_path.ends_with("post/thread") {
                    if let Some(post) = cur_post.as_mut() {
                        post.thread_ref = read_internals_id(&start);
                    }
                } else if local_path.ends_with("post/parent") {
                    if let Some(post) = cur_post.as_mut() {
                        post.parent_ref = Some(read_internals_id(&start));
                    }
                }
            }
            Ok(Event::Empty(start)) => {
                // Handle self-closing <thread dsq:id="..."/> and <parent dsq:id="..."/>.
                let name = strip_disqus_ns(start.name());
                let full_path = format!("{}/{}", path_stack.join("/"), name);
                if full_path.ends_with("post/thread") {
                    if let Some(post) = cur_post.as_mut() {
                        post.thread_ref = read_internals_id(&start);
                    }
                } else if full_path.ends_with("post/parent") {
                    if let Some(post) = cur_post.as_mut() {
                        post.parent_ref = Some(read_internals_id(&start));
                    }
                }
            }
            Ok(Event::End(end)) => {
                let name = strip_disqus_ns(end.name());
                let local_path = path_stack.join("/");
                let text = text_buf.take().unwrap_or_default();
                if let Some(thread) = cur_thread.as_mut() {
                    match name.as_str() {
                        "link" if local_path.ends_with("thread/link") => {
                            thread.link = text.clone();
                        }
                        "title" if local_path.ends_with("thread/title") => {
                            thread.title = text.clone();
                        }
                        // An empty <id/> flags a "duplicate empty" thread.
                        "id" if local_path.ends_with("thread/id") && text.is_empty() => {
                            thread.has_empty_id = true;
                        }
                        _ => {}
                    }
                }
                if let Some(post) = cur_post.as_mut() {
                    match name.as_str() {
                        "message" if local_path.ends_with("post/message") => {
                            post.text = text.clone();
                        }
                        "name" if local_path.ends_with("post/author/name") => {
                            post.author = Some(text.clone());
                        }
                        "email" if local_path.ends_with("post/author/email") => {
                            post.email = Some(text.clone());
                        }
                        "createdAt" if local_path.ends_with("post/createdAt") => {
                            post.created = parse_iso_utc(&text).unwrap_or(0.0);
                        }
                        "ipAddress" if local_path.ends_with("post/ipAddress") => {
                            post.remote_addr = anonymize(&text);
                        }
                        "isDeleted" if local_path.ends_with("post/isDeleted") => {
                            post.mode = if text.trim() == "false" { 1 } else { 4 };
                        }
                        _ => {}
                    }
                }
                path_stack.pop();
                if name == "thread" {
                    if let Some(t) = cur_thread.take() {
                        out.threads.push(t);
                    }
                } else if name == "post" {
                    if let Some(p) = cur_post.take() {
                        out.posts.push(p);
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(buf) = text_buf.as_mut() {
                    buf.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(buf) = text_buf.as_mut() {
                    buf.push_str(std::str::from_utf8(t.as_ref()).unwrap_or(""));
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn strip_disqus_ns(q: QName) -> String {
    let raw = std::str::from_utf8(q.as_ref()).unwrap_or("");
    raw.rsplit_once(':').map(|(_, n)| n).unwrap_or(raw).into()
}

fn read_internals_id(start: &quick_xml::events::BytesStart) -> String {
    for attr in start.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
        if key == "dsq:id" || key.ends_with(":id") {
            return String::from_utf8_lossy(&attr.value).into_owned();
        }
    }
    let _ = DISQUS_NS;
    let _ = DISQUS_INTERNALS;
    String::new()
}

const DISQUS_TS: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

fn parse_iso_utc(s: &str) -> Option<f64> {
    let parsed = PrimitiveDateTime::parse(s.trim(), &DISQUS_TS).ok()?;
    Some(parsed.assume_utc().unix_timestamp() as f64)
}

// ---------------------------------------------------------------------------
// WordPress
// ---------------------------------------------------------------------------

/// WordPress WXR importer.
///
/// WordPress exports use the WXR schema with a versioned namespace
/// (`http://wordpress.org/export/1.0/` or 1.2 / 1.3). The schema differs only
/// in version; we detect the version from the header.
pub struct WordPress<'a> {
    xml: &'a str,
}

impl<'a> WordPress<'a> {
    pub fn new(xml: &'a str) -> Self {
        Self { xml }
    }

    pub fn detect(peek: &str) -> Option<String> {
        let re = regex::Regex::new(r"http://wordpress.org/export/(1\.\d)/").ok()?;
        re.captures(peek)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    }

    pub async fn migrate(&self, pool: &SqlitePool) -> anyhow::Result<ImportReport> {
        let parsed = parse_wordpress(self.xml)?;
        let mut report = ImportReport::default();

        for thread in &parsed.threads {
            if thread.title.is_empty() || thread.comments.is_empty() {
                continue;
            }
            let url = url::Url::parse(&thread.link).ok();
            let mut path = url
                .as_ref()
                .map(|u| u.path().to_string())
                .unwrap_or_else(|| thread.link.clone());
            if let Some(q) = url.as_ref().and_then(|u| u.query()) {
                path.push('?');
                path.push_str(q);
            }
            if threads::get_by_uri(pool, &path).await?.is_none() {
                threads::new_thread(pool, &path, Some(thread.title.trim())).await?;
            }
            report.threads_inserted += 1;

            // Topologically insert: a parent must exist before its child.
            // WordPress assigns sequential ids, so sort by id first and then
            // rewrite parent references against the remap table.
            let mut comments = thread.comments.clone();
            comments.sort_by_key(|c| c.id);
            let mut remap: HashMap<i64, i64> = HashMap::new();
            for c in &comments {
                let parent = c.parent.and_then(|p| remap.get(&p).copied());
                let nc = NewComment {
                    parent,
                    created: Some(c.created),
                    mode: c.mode,
                    remote_addr: &c.remote_addr,
                    text: &c.text,
                    author: c.author.as_deref(),
                    email: c.email.as_deref(),
                    website: c.website.as_deref(),
                    notification: 0,
                };
                let inserted = cmt::add(pool, &path, c.created, &nc).await?;
                remap.insert(c.id, inserted.id);
                report.comments_inserted += 1;
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone)]
struct WpThread {
    link: String,
    title: String,
    comments: Vec<WpComment>,
}

#[derive(Debug, Clone)]
struct WpComment {
    id: i64,
    parent: Option<i64>,
    text: String,
    author: Option<String>,
    email: Option<String>,
    website: Option<String>,
    remote_addr: String,
    created: f64,
    mode: i64,
}

#[derive(Debug, Default)]
struct WpParsed {
    threads: Vec<WpThread>,
}

const WORDPRESS_TS: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

fn parse_wordpress(xml: &str) -> anyhow::Result<WpParsed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut out = WpParsed::default();
    let mut cur_thread: Option<WpThread> = None;
    let mut cur_comment: Option<WpComment> = None;
    let mut text_buf: Option<String> = None;
    let mut path_stack: Vec<String> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => anyhow::bail!("xml parse error: {e}"),
            Ok(Event::Eof) => break,
            Ok(Event::Start(start)) => {
                let name = local_name(start.name());
                path_stack.push(name.clone());
                match name.as_str() {
                    "item" => {
                        cur_thread = Some(WpThread {
                            link: String::new(),
                            title: String::new(),
                            comments: Vec::new(),
                        });
                    }
                    "comment" => {
                        cur_comment = Some(WpComment {
                            id: 0,
                            parent: None,
                            text: String::new(),
                            author: None,
                            email: None,
                            website: None,
                            remote_addr: "0.0.0.0".into(),
                            created: 0.0,
                            mode: 1,
                        });
                    }
                    _ => {
                        text_buf = Some(String::new());
                    }
                }
            }
            Ok(Event::Empty(_)) => {
                // Self-closing elements don't carry data we consume.
            }
            Ok(Event::End(end)) => {
                let name = local_name(end.name());
                let text = text_buf.take().unwrap_or_default();
                let local_path = path_stack.join("/");
                if let Some(thread) = cur_thread.as_mut() {
                    if cur_comment.is_none() {
                        // Top-level thread fields.
                        match name.as_str() {
                            "title" if local_path.ends_with("item/title") => {
                                thread.title = text.clone();
                            }
                            "link" if local_path.ends_with("item/link") => {
                                thread.link = text.clone();
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(c) = cur_comment.as_mut() {
                    match name.as_str() {
                        "comment_id" => c.id = text.trim().parse().unwrap_or(0),
                        "comment_parent" => {
                            let p: i64 = text.trim().parse().unwrap_or(0);
                            c.parent = if p == 0 { None } else { Some(p) };
                        }
                        "comment_content" => c.text = wp_normalize_text(&text),
                        "comment_author" => c.author = trim_opt(&text),
                        "comment_author_email" => c.email = trim_opt(&text),
                        "comment_author_url" => c.website = trim_opt(&text),
                        "comment_author_IP" => c.remote_addr = anonymize(text.trim()),
                        "comment_date_gmt" => {
                            c.created = parse_wp_date(&text).unwrap_or(0.0);
                        }
                        "comment_approved" => {
                            c.mode = if text.trim() == "1" { 1 } else { 2 };
                        }
                        _ => {}
                    }
                }
                path_stack.pop();
                if name == "comment" {
                    if let (Some(thread), Some(c)) = (cur_thread.as_mut(), cur_comment.take()) {
                        thread.comments.push(c);
                    }
                } else if name == "item" {
                    if let Some(t) = cur_thread.take() {
                        out.threads.push(t);
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(buf) = text_buf.as_mut() {
                    buf.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(buf) = text_buf.as_mut() {
                    buf.push_str(std::str::from_utf8(t.as_ref()).unwrap_or(""));
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn parse_wp_date(s: &str) -> Option<f64> {
    let parsed = PrimitiveDateTime::parse(s.trim(), &WORDPRESS_TS).ok()?;
    Some(parsed.assume_utc().unix_timestamp() as f64)
}

/// WordPress renders a single `\n` inside a paragraph as `<br>`. Mirror the
/// Python importer by inserting two trailing spaces (Markdown hard-break) on
/// every lone newline — i.e. one that neither follows nor precedes another
/// newline. The Python regex uses look-around; Rust's default regex engine
/// doesn't support it, so we walk characters and inspect neighbors directly.
fn wp_normalize_text(text: &str) -> String {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    let mut out = String::with_capacity(trimmed.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let prev_is_nl = i > 0 && bytes[i - 1] == b'\n';
            let next_is_nl = i + 1 < bytes.len() && bytes[i + 1] == b'\n';
            if !prev_is_nl && !next_is_nl {
                out.push_str("  \n");
            } else {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        // Push a single char (text is UTF-8; we only peek at ASCII \n).
        let ch = trimmed[i..].chars().next().expect("in-bounds char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn trim_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn local_name(q: QName) -> String {
    let raw = std::str::from_utf8(q.as_ref()).unwrap_or("");
    raw.rsplit_once(':').map(|(_, n)| n).unwrap_or(raw).into()
}

// ---------------------------------------------------------------------------
// Generic (JSON)
// ---------------------------------------------------------------------------

/// Generic JSON importer — a list of threads, each with comments.
/// See isso/migrate.py::Generic for the schema.
pub struct Generic {
    threads: Vec<GenericThread>,
}

#[derive(Deserialize)]
struct GenericThread {
    id: String,
    title: String,
    comments: Vec<GenericCommentInput>,
}

#[derive(Deserialize)]
struct GenericCommentInput {
    id: i64,
    text: String,
    author: Option<String>,
    email: Option<String>,
    website: Option<String>,
    remote_addr: String,
    created: String,
}

impl Generic {
    pub fn new(json: &str) -> anyhow::Result<Self> {
        let threads: Vec<GenericThread> =
            serde_json::from_str(json).map_err(|e| anyhow::anyhow!("json parse: {e}"))?;
        Ok(Self { threads })
    }

    pub async fn migrate(&self, pool: &SqlitePool) -> anyhow::Result<ImportReport> {
        let mut report = ImportReport::default();
        for thread in &self.threads {
            if threads::get_by_uri(pool, &thread.id).await?.is_none() {
                threads::new_thread(pool, &thread.id, Some(&thread.title)).await?;
            }
            report.threads_inserted += 1;

            let mut comments: Vec<&GenericCommentInput> = thread.comments.iter().collect();
            comments.sort_by_key(|c| c.id);
            for c in comments {
                let created = parse_wp_date(&c.created).unwrap_or(0.0);
                let nc = NewComment {
                    parent: None,
                    created: Some(created),
                    mode: 1,
                    remote_addr: &c.remote_addr,
                    text: &c.text,
                    author: c.author.as_deref(),
                    email: c.email.as_deref(),
                    website: c.website.as_deref(),
                    notification: 0,
                };
                cmt::add(pool, &thread.id, created, &nc).await?;
                report.comments_inserted += 1;
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autodetect_picks_disqus_for_namespace() {
        let peek = r#"<?xml version="1.0"?><disqus xmlns="http://disqus.com" xmlns:dsq="http://disqus.com/disqus-internals">"#;
        assert!(matches!(autodetect(peek), Some(Kind::Disqus)));
    }

    #[test]
    fn autodetect_picks_wordpress_for_wxr_namespace() {
        let peek = r#"<?xml version="1.0"?><rss xmlns:wp="http://wordpress.org/export/1.2/">"#;
        assert!(matches!(autodetect(peek), Some(Kind::WordPress)));
    }

    #[test]
    fn autodetect_picks_generic_for_json_list() {
        let peek = r#"[{"id": "post-1", "title": "T", "comments": []}]"#;
        assert!(matches!(autodetect(peek), Some(Kind::Generic)));
    }

    #[test]
    fn autodetect_none_on_plain_text() {
        assert!(autodetect("hello world").is_none());
    }

    #[test]
    fn parse_iso_utc_round_trips_disqus_timestamp() {
        // Python: mktime(strptime("2024-04-15T10:30:00Z", "%Y-%m-%dT%H:%M:%SZ"))
        // In UTC this is the unix timestamp for 2024-04-15 10:30:00.
        let got = parse_iso_utc("2024-04-15T10:30:00Z").unwrap();
        assert_eq!(got, 1713177000.0);
    }

    #[test]
    fn wp_normalize_text_inserts_hard_break_on_single_newline() {
        // Single \n inside a paragraph → append two spaces before it (Markdown hard break).
        let got = wp_normalize_text("line one\nline two\n\nnew paragraph");
        assert_eq!(got, "line one  \nline two\n\nnew paragraph");
    }

    #[tokio::test]
    async fn generic_json_end_to_end() {
        let pool = crate::db::open(":memory:").await.unwrap();
        let json = r#"[
            {"id": "/post-a", "title": "Post A", "comments": [
                {"id": 1, "text": "first", "author": "a", "email": "a@e",
                 "website": null, "remote_addr": "1.2.3.0",
                 "created": "2024-01-01 00:00:00"}
            ]},
            {"id": "/post-b", "title": "Post B", "comments": []}
        ]"#;
        let report = Generic::new(json).unwrap().migrate(&pool).await.unwrap();
        assert_eq!(report.threads_inserted, 2);
        assert_eq!(report.comments_inserted, 1);

        // Post A has one comment, Post B has zero — matches count().
        let counts = cmt::count(&pool, &["/post-a", "/post-b"]).await.unwrap();
        assert_eq!(counts, vec![1, 0]);
    }

    #[tokio::test]
    async fn disqus_importer_end_to_end() {
        // Minimal Disqus dump: one thread + two posts, the second replying
        // to the first. After import we expect two comments on /t/ and the
        // reply's parent to point at the root's inserted id.
        let xml = r##"<?xml version="1.0"?>
<disqus xmlns="http://disqus.com" xmlns:dsq="http://disqus.com/disqus-internals">
  <thread dsq:id="100">
    <id>thread-one</id>
    <link>http://example.com/t/</link>
    <title>Thread One</title>
  </thread>
  <post dsq:id="10">
    <message>root comment</message>
    <createdAt>2024-01-01T00:00:00Z</createdAt>
    <isDeleted>false</isDeleted>
    <author>
      <name>Alice</name>
      <email>a@ex</email>
    </author>
    <ipAddress>1.2.3.4</ipAddress>
    <thread dsq:id="100"/>
  </post>
  <post dsq:id="11">
    <message>reply</message>
    <createdAt>2024-01-02T00:00:00Z</createdAt>
    <isDeleted>false</isDeleted>
    <author>
      <name>Bob</name>
      <email>b@ex</email>
    </author>
    <ipAddress>5.6.7.8</ipAddress>
    <thread dsq:id="100"/>
    <parent dsq:id="10"/>
  </post>
</disqus>"##;
        let pool = crate::db::open(":memory:").await.unwrap();
        let report = Disqus::new(xml, false).migrate(&pool).await.unwrap();
        assert_eq!(report.threads_inserted, 1);
        assert_eq!(report.comments_inserted, 2);
        assert_eq!(report.orphan_count, 0);

        let counts = cmt::count(&pool, &["/t/"]).await.unwrap();
        assert_eq!(counts, vec![2]);

        let reply_parent: Option<i64> =
            sqlx::query_scalar("SELECT parent FROM comments WHERE text = 'reply'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let root_id: i64 =
            sqlx::query_scalar("SELECT id FROM comments WHERE text = 'root comment'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reply_parent, Some(root_id));
    }

    #[tokio::test]
    async fn wordpress_importer_roundtrips_parent_refs() {
        let xml = r##"<?xml version="1.0"?>
<rss xmlns:wp="http://wordpress.org/export/1.2/">
<channel>
  <item>
    <title>Hello</title>
    <link>http://example.com/hello</link>
    <wp:comment>
      <wp:comment_id>1</wp:comment_id>
      <wp:comment_parent>0</wp:comment_parent>
      <wp:comment_content><![CDATA[first]]></wp:comment_content>
      <wp:comment_author>Alice</wp:comment_author>
      <wp:comment_author_email>a@ex</wp:comment_author_email>
      <wp:comment_author_url/>
      <wp:comment_author_IP>1.2.3.4</wp:comment_author_IP>
      <wp:comment_date_gmt>2024-01-01 00:00:00</wp:comment_date_gmt>
      <wp:comment_approved>1</wp:comment_approved>
    </wp:comment>
    <wp:comment>
      <wp:comment_id>2</wp:comment_id>
      <wp:comment_parent>1</wp:comment_parent>
      <wp:comment_content><![CDATA[reply to first]]></wp:comment_content>
      <wp:comment_author>Bob</wp:comment_author>
      <wp:comment_author_email>b@ex</wp:comment_author_email>
      <wp:comment_author_url/>
      <wp:comment_author_IP>5.6.7.8</wp:comment_author_IP>
      <wp:comment_date_gmt>2024-01-02 00:00:00</wp:comment_date_gmt>
      <wp:comment_approved>1</wp:comment_approved>
    </wp:comment>
  </item>
</channel>
</rss>"##;
        let pool = crate::db::open(":memory:").await.unwrap();
        let report = WordPress::new(xml).migrate(&pool).await.unwrap();
        assert_eq!(report.threads_inserted, 1);
        assert_eq!(report.comments_inserted, 2);

        // Confirm the reply's parent was rewritten to the inserted comment's id,
        // not the WordPress-side id 1.
        let reply: (i64, Option<i64>) =
            sqlx::query_as("SELECT id, parent FROM comments WHERE text = 'reply to first'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let first: i64 = sqlx::query_scalar("SELECT id FROM comments WHERE text = 'first'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(reply.1, Some(first));
    }
}
