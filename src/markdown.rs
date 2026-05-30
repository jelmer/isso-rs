//! Markdown rendering + HTML sanitization.
//!
//! Mirrors the two-stage pipeline from isso/html/__init__.py:
//!
//! 1. **Render Markdown** with pulldown-cmark (raw HTML is not escaped at this
//!    step — ammonia handles it below, and escaping twice corrupts the output).
//!    We enable strikethrough and tables because the Python defaults enable
//!    `strikethrough, subscript, superscript` mistune plugins.
//! 2. **Sanitize** with ammonia using the Python allowlist:
//!    - Tags: `a, p, hr, br, ol, ul, li, pre, code, blockquote, del, ins,
//!      strong, em, h1..h6, sub, sup, table, thead, tbody, th, td`, plus
//!      any `[markup] allowed-elements`.
//!    - Attributes: `a: href` and `a: rel`, `table: align`, `code: class` (iff
//!      matches `^language-[a-zA-Z0-9]{1,20}$`), plus any
//!      `[markup] allowed-attributes` on all tags.
//!    - All `<a href="...">` links except `mailto:` get `nofollow` and
//!      `noopener` appended to their `rel` (case-insensitively deduplicated),
//!      mirroring the `set_links` linkifier callback in isso/html/__init__.py.
//!      Any author-supplied `rel` that survived sanitisation is preserved.
//!
//! The rendered string is wrapped in `<p>...</p>` if it isn't already — the
//! JS frontend relies on this to detect "empty" renderings.

use std::collections::{HashMap, HashSet};

use ammonia::{Builder, UrlRelative};
use pulldown_cmark::{html, Options, Parser};
use regex::Regex;

/// Tags the Python reference implementation allows unconditionally.
const BASE_ALLOWED_TAGS: &[&str] = &[
    "a",
    "p",
    "hr",
    "br",
    "ol",
    "ul",
    "li",
    "pre",
    "code",
    "blockquote",
    "del",
    "ins",
    "strong",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "sub",
    "sup",
    "table",
    "thead",
    "tbody",
    "th",
    "td",
];

pub struct Renderer {
    extra_tags: HashSet<String>,
    extra_attrs: HashSet<String>,
    code_class_regex: Regex,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::with_allowlist(&[], &[])
    }

    /// Build a renderer that honours `[markup] allowed-elements` and
    /// `[markup] allowed-attributes`. Empty strings are ignored (matches
    /// the Python behaviour of `getlist` returning `['']` for blank config).
    pub fn with_allowlist(extra_tags: &[String], extra_attrs: &[String]) -> Self {
        Self {
            extra_tags: extra_tags
                .iter()
                .filter(|t| !t.is_empty())
                .cloned()
                .collect(),
            extra_attrs: extra_attrs
                .iter()
                .filter(|a| !a.is_empty())
                .cloned()
                .collect(),
            code_class_regex: Regex::new("^language-[a-zA-Z0-9]{1,20}$")
                .expect("static regex compiles"),
        }
    }

    pub fn render(&self, text: &str) -> String {
        // Step 1: Markdown -> HTML. pulldown-cmark by default passes raw HTML
        // through; ammonia is our XSS defence, so we don't double-escape here.
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_SMART_PUNCTUATION);
        let parser = Parser::new_ext(text, opts);
        let mut rendered = String::new();
        html::push_html(&mut rendered, parser);

        // Step 2: sanitise, then add rel=nofollow noopener on links.
        let cleaned = apply_link_rel(&self.sanitize(&rendered));

        // Step 3: wrap in <p>…</p> if it isn't already (frontend invariant).
        wrap_paragraph(cleaned)
    }

    fn sanitize(&self, html: &str) -> String {
        let mut tags: HashSet<&str> = BASE_ALLOWED_TAGS.iter().copied().collect();
        for t in &self.extra_tags {
            tags.insert(t.as_str());
        }

        let mut tag_attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
        // `rel` is allowed through so a caller-supplied value survives to the
        // set_links post-pass; bleach likewise keeps it only when the operator
        // adds it to `allowed-attributes`, but passing it always is harmless
        // since the renderer never emits `rel` on its own.
        tag_attrs.insert("a", ["href", "rel"].into_iter().collect());
        tag_attrs.insert("table", ["align"].into_iter().collect());
        // `<code class="language-…">` is allowed, but the attribute_filter
        // below rejects any value that doesn't match language-<alnum>.
        tag_attrs.insert("code", ["class"].into_iter().collect());

        // Global attributes from `[markup] allowed-attributes` apply to all tags.
        let generic_attrs: HashSet<&str> = self.extra_attrs.iter().map(|s| s.as_str()).collect();

        let regex = self.code_class_regex.clone();
        let mut builder = Builder::default();
        builder
            .tags(tags)
            .tag_attributes(tag_attrs)
            .generic_attributes(generic_attrs)
            // `rel` is added by the set_links post-pass (see `apply_link_rel`),
            // not ammonia's `link_rel`, because ammonia replaces the whole
            // attribute and would also touch `mailto:` links — neither of which
            // matches isso/html/__init__.py.
            .link_rel(None)
            .url_relative(UrlRelative::PassThrough)
            // `code class="language-xxx"` is allowed only when the value
            // matches bleach's language-<alnum> pattern.
            .attribute_filter(move |element, attribute, value| {
                if element == "code" && attribute == "class" {
                    if regex.is_match(value) {
                        Some(value.into())
                    } else {
                        None
                    }
                } else {
                    Some(value.into())
                }
            });
        builder.clean(html).to_string()
    }
}

/// Append `nofollow` and `noopener` to the `rel` of every non-`mailto:` link,
/// mirroring the `set_links` linkifier callback in isso/html/__init__.py:
///
/// - links without an `href` are left untouched,
/// - `mailto:` links are skipped entirely (no `rel` added),
/// - existing `rel` tokens are kept; `nofollow`/`noopener` are only appended
///   when not already present (case-insensitive).
///
/// The input is trusted ammonia output, so `<a>` open tags are well-formed:
/// lowercase tag name, attributes in `name="value"` form with double quotes.
fn apply_link_rel(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < html.len() {
        // Look for the start of an `<a` open tag (followed by whitespace or
        // `>`, so we don't match `<abbr>` etc.).
        if bytes[i] == b'<'
            && html[i + 1..].starts_with('a')
            && matches!(bytes.get(i + 2), Some(b' ' | b'\t' | b'\n' | b'\r' | b'>'))
        {
            if let Some(end_rel) = html[i..].find('>') {
                let tag = &html[i..i + end_rel + 1];
                out.push_str(&rewrite_anchor_tag(tag));
                i += end_rel + 1;
                continue;
            }
        }
        let ch = html[i..].chars().next().expect("valid char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrite a single `<a ...>` open tag (including the angle brackets) per the
/// set_links rules. Returns the tag unchanged when it has no `href` or the
/// `href` is a `mailto:` link.
fn rewrite_anchor_tag(tag: &str) -> String {
    let inner = &tag[1..tag.len() - 1]; // strip < and >
    let mut href: Option<String> = None;
    let mut rel: Option<String> = None;
    for (name, value) in attributes(inner) {
        match name.to_ascii_lowercase().as_str() {
            "href" => href = Some(value),
            "rel" => rel = Some(value),
            _ => {}
        }
    }

    let href = match href {
        Some(h) => h,
        None => return tag.to_string(),
    };
    if href.starts_with("mailto:") {
        return tag.to_string();
    }

    let mut rel_values: Vec<String> = rel
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    for token in ["nofollow", "noopener"] {
        if !rel_values.iter().any(|v| v.eq_ignore_ascii_case(token)) {
            rel_values.push(token.to_string());
        }
    }
    let rel_attr = format!(" rel=\"{}\"", rel_values.join(" "));

    // Rebuild the tag, dropping any existing rel (we re-emit it) and appending
    // the computed rel just before the closing `>`.
    let mut rebuilt = String::from("<a");
    for (name, value) in attributes(inner) {
        if name.eq_ignore_ascii_case("rel") {
            continue;
        }
        rebuilt.push_str(&format!(" {name}=\"{value}\""));
    }
    rebuilt.push_str(&rel_attr);
    rebuilt.push('>');
    rebuilt
}

/// Parse `name="value"` attribute pairs out of an open-tag body (the text
/// between `<a` and `>`). Only double-quoted values are produced, which is
/// all ammonia ever emits.
fn attributes(inner: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < inner.len() {
        // Skip the leading tag name token and any whitespace between attrs.
        while i < inner.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < inner.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &inner[name_start..i];
        while i < inner.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if name.is_empty() || i >= inner.len() || bytes[i] != b'=' {
            // No value (e.g. the `a` tag-name token itself, or a bare attr);
            // skip it and continue.
            continue;
        }
        i += 1; // consume '='
        while i < inner.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= inner.len() || bytes[i] != b'"' {
            continue;
        }
        i += 1; // opening quote
        let val_start = i;
        while i < inner.len() && bytes[i] != b'"' {
            i += 1;
        }
        let value = &inner[val_start..i];
        i += 1; // closing quote
        attrs.push((name.to_string(), value.to_string()));
    }
    attrs
}

fn wrap_paragraph(mut s: String) -> String {
    while s.ends_with('\n') {
        s.pop();
    }
    if !(s.starts_with("<p>") && s.ends_with("</p>")) {
        s = format!("<p>{s}</p>");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_gets_p_wrapper() {
        let r = Renderer::new();
        assert_eq!(r.render("hello"), "<p>hello</p>");
    }

    #[test]
    fn links_get_nofollow_noopener() {
        let r = Renderer::new();
        let got = r.render("see [here](https://example.com)");
        assert_eq!(
            got,
            "<p>see <a href=\"https://example.com\" rel=\"nofollow noopener\">here</a></p>"
        );
    }

    #[test]
    fn raw_script_tags_are_stripped() {
        // The critical XSS invariant. If this test ever regresses, a
        // comment containing <script> would execute in the reader's browser.
        let r = Renderer::new();
        let got = r.render("<script>alert(1)</script>hello");
        assert_eq!(got, "<p>hello</p>");
    }

    #[test]
    fn raw_onload_handler_is_stripped() {
        let r = Renderer::new();
        let got = r.render("<p onclick=\"alert(1)\">x</p>");
        assert_eq!(got, "<p>x</p>");
    }

    #[test]
    fn img_tag_is_stripped_by_default() {
        let r = Renderer::new();
        let got = r.render("<img src=\"bad\">hello");
        assert_eq!(got, "<p>hello</p>");
    }

    #[test]
    fn img_tag_allowed_when_configured() {
        // The operator opts into `img` + `src` via `[markup] allowed-elements`
        // and `allowed-attributes`. Python adds `src` automatically if `img`
        // is allowed without `src`; we leave that to the caller for now.
        let r = Renderer::with_allowlist(&["img".into()], &["src".into()]);
        let got = r.render("<img src=\"cat.jpg\">hello");
        // Note: pulldown-cmark wraps inline HTML, then ammonia allows it.
        assert_eq!(got, "<p><img src=\"cat.jpg\">hello</p>");
    }

    #[test]
    fn code_class_language_marker_is_preserved() {
        // Fenced code block with language info string survives sanitisation
        // because bleach/ammonia's regex accepts `language-<alnum>`.
        let r = Renderer::new();
        let got = r.render("```rust\nfn main() {}\n```");
        assert_eq!(
            got,
            "<p><pre><code class=\"language-rust\">fn main() {}\n</code></pre></p>"
        );
    }

    #[test]
    fn code_class_arbitrary_value_is_dropped() {
        // Anything that doesn't match ^language-[a-zA-Z0-9]{1,20}$ should be
        // dropped, per isso/html/__init__.py::allow_attribute_class.
        let r = Renderer::new();
        let got = r.render("<code class=\"evil attr\">x</code>");
        assert_eq!(got, "<p><code>x</code></p>");
    }

    #[test]
    fn existing_rel_values_are_preserved() {
        // A caller-supplied rel (allowed via `allowed-attributes`) is kept and
        // our tokens are appended, matching the set_links callback. `me` is a
        // valid rel value frontends may emit for self-links.
        let r = Renderer::with_allowlist(&[], &["rel".into()]);
        let got = r.render("<a href=\"x\" rel=\"me\">x</a>");
        assert_eq!(
            got,
            "<p><a href=\"x\" rel=\"me nofollow noopener\">x</a></p>"
        );
    }

    #[test]
    fn existing_rel_tokens_are_not_duplicated() {
        // If the author already set nofollow, we must not add it twice
        // (case-insensitive), per set_links.
        let r = Renderer::with_allowlist(&[], &["rel".into()]);
        let got = r.render("<a href=\"x\" rel=\"NoFollow\">x</a>");
        assert_eq!(got, "<p><a href=\"x\" rel=\"NoFollow noopener\">x</a></p>");
    }

    #[test]
    fn plain_link_gets_nofollow_noopener() {
        let r = Renderer::new();
        let got = r.render("<a href=\"https://example.com\">x</a>");
        assert_eq!(
            got,
            "<p><a href=\"https://example.com\" rel=\"nofollow noopener\">x</a></p>"
        );
    }

    #[test]
    fn mailto_links_do_not_get_rel() {
        // bleach's set_links skips mailto: links entirely; we match that.
        let r = Renderer::new();
        let got = r.render("[mail](mailto:a@b.com)");
        assert_eq!(got, "<p><a href=\"mailto:a@b.com\">mail</a></p>");
    }

    #[test]
    fn strong_and_em_survive() {
        let r = Renderer::new();
        assert_eq!(
            r.render("**bold** and *italic*"),
            "<p><strong>bold</strong> and <em>italic</em></p>"
        );
    }

    #[test]
    fn pulldown_strikethrough_renders() {
        let r = Renderer::new();
        assert_eq!(r.render("~~gone~~"), "<p><del>gone</del></p>");
    }
}
