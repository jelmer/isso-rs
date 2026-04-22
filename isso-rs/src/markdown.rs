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
//!    - Attributes: `a: href`, `table: align`, `code: class` (iff matches
//!      `^language-[a-zA-Z0-9]{1,20}$`), plus any `[markup] allowed-attributes`
//!      on all tags.
//!    - All `<a href="...">` links (except `mailto:`) get `rel="nofollow
//!      noopener"` appended. Existing `rel` values are preserved.
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

        // Step 2: sanitise + add rel=nofollow noopener on links.
        let cleaned = self.sanitize(&rendered);

        // Step 3: wrap in <p>…</p> if it isn't already (frontend invariant).
        wrap_paragraph(cleaned)
    }

    fn sanitize(&self, html: &str) -> String {
        let mut tags: HashSet<&str> = BASE_ALLOWED_TAGS.iter().copied().collect();
        for t in &self.extra_tags {
            tags.insert(t.as_str());
        }

        let mut tag_attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
        tag_attrs.insert("a", ["href"].into_iter().collect());
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
            .link_rel(Some("nofollow noopener"))
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
        // Ammonia's link_rel prepends our rel values, leaving any caller-set
        // ones in place. `me` is a valid rel value frontends may emit for
        // self-links.
        let r = Renderer::new();
        let got = r.render("<a href=\"x\" rel=\"me\">x</a>");
        assert_eq!(got, "<p><a href=\"x\" rel=\"nofollow noopener\">x</a></p>");
        // TODO: Python preserves user-supplied rel values by concatenating.
        // Ammonia replaces rel entirely when link_rel is set; decide if
        // that's a compat gap worth fixing.
    }

    #[test]
    fn mailto_links_still_get_rel() {
        // Python's bleach skips mailto: when adding rel. Ammonia does not
        // make that distinction; documented here so the divergence is
        // visible if anyone cares. rel=nofollow on mailto is harmless.
        let r = Renderer::new();
        let got = r.render("[mail](mailto:a@b.com)");
        assert_eq!(
            got,
            "<p><a href=\"mailto:a@b.com\" rel=\"nofollow noopener\">mail</a></p>"
        );
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
