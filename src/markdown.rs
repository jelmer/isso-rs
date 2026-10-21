//! Markdown rendering + HTML sanitization.
//!
//! Mirrors the two-stage pipeline from isso/html/__init__.py:
//!
//! 1. **Render Markdown** with pulldown-cmark, configured like isso's mistune
//!    renderer: raw HTML in the input is always escaped, `[markup.mistune]
//!    plugins` select the syntax extensions, and the `hard_wrap` parameter
//!    turns single newlines into `<br>`.
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
use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};
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
    options: Options,
    hard_wrap: bool,
    /// mistune's subscript/superscript patterns, which unlike pulldown-cmark
    /// also match mid-word (`H~2~O`, `x^2^`).
    script_regex: Option<Regex>,
    extra_tags: HashSet<String>,
    extra_attrs: HashSet<String>,
    code_class_regex: Regex,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a mistune plugin name onto the pulldown-cmark option providing the
/// same syntax.
fn plugin_option(name: &str) -> Option<Options> {
    match name {
        // pulldown-cmark reads a single `~x~` as strikethrough unless its
        // subscript extension is on, which mistune doesn't do. Its subscript
        // markers are turned back into text in `markdown_events`, since
        // sub/superscript are done with mistune's own patterns there.
        "strikethrough" => Some(Options::ENABLE_STRIKETHROUGH | Options::ENABLE_SUBSCRIPT),
        "subscript" | "superscript" => Some(Options::empty()),
        "table" => Some(Options::ENABLE_TABLES),
        "footnotes" => Some(Options::ENABLE_FOOTNOTES),
        "task_lists" => Some(Options::ENABLE_TASKLISTS),
        "def_list" => Some(Options::ENABLE_DEFINITION_LIST),
        _ => None,
    }
}

impl Renderer {
    /// Renderer with isso's default `[markup]` and `[markup.mistune]`
    /// settings.
    pub fn new() -> Self {
        Self::from_config(&crate::config::Config::default().markup)
            .expect("default markup config is supported")
    }

    /// Build a renderer from the `[markup]` and `[markup.mistune]` config.
    ///
    /// Fails for the `misaka` renderer and for mistune plugins we have no
    /// equivalent for, rather than rendering comments differently from
    /// isso.
    pub fn from_config(markup: &crate::config::Markup) -> anyhow::Result<Self> {
        match markup.renderer.as_str() {
            "mistune" => {}
            "misaka" => anyhow::bail!("[markup] renderer = misaka is not supported; use mistune"),
            other => anyhow::bail!(
                "Invalid renderer value: {other}. Set to either `mistune` or `misaka`."
            ),
        }
        let mut options = Options::empty();
        for plugin in &markup.mistune_plugins {
            options |= plugin_option(plugin)
                .ok_or_else(|| anyhow::anyhow!("unsupported mistune plugin: {plugin}"))?;
        }
        let has_plugin = |name: &str| markup.mistune_plugins.iter().any(|p| p == name);
        let mut script_patterns = Vec::new();
        if has_plugin("superscript") {
            script_patterns.push(r"\^(?P<sup>\S+?)\^");
        }
        if has_plugin("subscript") {
            script_patterns.push(r"~(?P<sub>\S+?)~");
        }
        let script_regex = (!script_patterns.is_empty())
            .then(|| Regex::new(&script_patterns.join("|")).expect("static regex compiles"));
        let mut hard_wrap = false;
        for parameter in &markup.mistune_parameters {
            match parameter.as_str() {
                "hard_wrap" => hard_wrap = true,
                // isso always escapes raw HTML, whether or not this is listed.
                "escape" => {}
                // isso passes nothing but hard_wrap on to mistune either.
                other => tracing::warn!("ignoring unknown mistune parameter: {other}"),
            }
        }

        let extra_tags: HashSet<String> = markup
            .allowed_elements
            .iter()
            .filter(|t| !t.is_empty())
            .cloned()
            .collect();
        let mut extra_attrs: HashSet<String> = markup
            .allowed_attributes
            .iter()
            .filter(|a| !a.is_empty())
            .cloned()
            .collect();
        // If images are allowed, their source has to be as well.
        if extra_tags.contains("img") {
            extra_attrs.insert("src".into());
        }
        Ok(Self {
            options,
            hard_wrap,
            script_regex,
            extra_tags,
            extra_attrs,
            code_class_regex: Regex::new("^language-[a-zA-Z0-9]{1,20}$")
                .expect("static regex compiles"),
        })
    }

    pub fn render(&self, text: &str) -> String {
        let mut rendered = String::new();
        html::push_html(&mut rendered, self.markdown_events(text).into_iter());

        // Like isso, wrap in <p>...</p> before sanitising; the sanitiser's
        // HTML parser then splits paragraphs around block elements.
        let wrapped = wrap_paragraph(rendered);
        escape_text_quotes(&apply_link_rel(&self.sanitize(&wrapped)))
    }

    /// Parse `text` into the event stream mistune would have rendered.
    fn markdown_events<'a>(&self, text: &'a str) -> Vec<Event<'a>> {
        let mut out = Vec::new();
        // Adjacent text events are merged so the sub/superscript patterns
        // can see a whole run of text.
        let mut pending = String::new();
        let mut in_code_block = false;
        for (event, range) in Parser::new_ext(text, self.options).into_offset_iter() {
            let source = &text[range];
            match &event {
                // mistune doesn't decode entity references; with escaping on,
                // `&amp;` shows up literally.
                Event::Text(t) if !in_code_block && source.starts_with('&') && **t != *source => {
                    pending.push_str(source);
                    continue;
                }
                Event::Text(t) if !in_code_block => {
                    pending.push_str(t);
                    continue;
                }
                // With hard_wrap, mistune's line break rule wins over the
                // backslash one, keeping the backslash.
                Event::HardBreak if self.hard_wrap && source.starts_with('\\') => {
                    pending.push('\\');
                }
                Event::Start(Tag::Subscript) | Event::End(TagEnd::Subscript) => {
                    pending.push('~');
                    continue;
                }
                _ => {}
            }
            self.flush_text(&mut pending, &mut out);
            out.push(match event {
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code_block = true;
                    Event::Start(Tag::CodeBlock(kind))
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    Event::End(TagEnd::CodeBlock)
                }
                // Raw HTML becomes text so that it gets escaped, like
                // mistune's `escape=True`.
                Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
                Event::SoftBreak if self.hard_wrap => Event::HardBreak,
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => Event::Start(Tag::Link {
                    link_type,
                    dest_url: sanitize_url(dest_url),
                    title,
                    id,
                }),
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => Event::Start(Tag::Image {
                    link_type,
                    dest_url: sanitize_url(dest_url),
                    title,
                    id,
                }),
                other => other,
            });
        }
        self.flush_text(&mut pending, &mut out);
        out
    }

    /// Emit the merged text run, with mistune's sub/superscript applied.
    fn flush_text<'a>(&self, pending: &mut String, out: &mut Vec<Event<'a>>) {
        if pending.is_empty() {
            return;
        }
        let text = std::mem::take(pending);
        let Some(regex) = &self.script_regex else {
            out.push(Event::Text(text.into()));
            return;
        };
        let mut last = 0;
        for caps in regex.captures_iter(&text) {
            let whole = caps.get(0).expect("group 0 always matches");
            let (tag, inner) = match (caps.name("sup"), caps.name("sub")) {
                (Some(inner), _) => ("sup", inner),
                (None, Some(inner)) => ("sub", inner),
                (None, None) => unreachable!("one alternative always matches"),
            };
            if whole.start() > last {
                out.push(Event::Text(text[last..whole.start()].to_string().into()));
            }
            out.push(Event::Html(CowStr::from(format!("<{tag}>"))));
            out.push(Event::Text(inner.as_str().to_string().into()));
            out.push(Event::Html(CowStr::from(format!("</{tag}>"))));
            last = whole.end();
        }
        if last < text.len() {
            out.push(Event::Text(text[last..].to_string().into()));
        }
    }

    fn sanitize(&self, html: &str) -> String {
        let mut tags: HashSet<&str> = BASE_ALLOWED_TAGS.iter().copied().collect();
        for t in &self.extra_tags {
            tags.insert(t.as_str());
        }
        // Not in isso's allowlist, but html5lib re-inserts the `<tr>` it
        // implies around table cells before bleach sees them, so isso's
        // tables keep their rows.
        tags.insert("tr");

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
            // bleach's default protocol allowlist.
            .url_schemes(["http", "https", "mailto"].into_iter().collect())
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

/// Write `"` in text as `&quot;`, as mistune's escaping does and bleach
/// preserves. The input is trusted ammonia output, where attribute values
/// are always double-quoted and `<` in text is always escaped.
fn escape_text_quotes(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let (mut in_tag, mut in_value) = (false, false);
    for ch in html.chars() {
        match ch {
            '"' if !in_tag => {
                out.push_str("&quot;");
                continue;
            }
            '"' => in_value = !in_value,
            '<' if !in_tag => in_tag = true,
            '>' if in_tag && !in_value => in_tag = false,
            _ => {}
        }
        out.push(ch);
    }
    out
}

/// mistune's `sanitize_url`: links and images pointing anywhere but a safe
/// protocol, a few image `data:` types or a relative URL are replaced by
/// `#harmful-link`.
fn sanitize_url(url: CowStr<'_>) -> CowStr<'_> {
    const SAFE_PROTOCOLS: &[&str] = &[
        "http:", "https:", "mailto:", "tel:", "ftp:", "ftps:", "irc:", "ircs:",
    ];
    const GOOD_DATA_PROTOCOLS: &[&str] = &[
        "data:image/gif;",
        "data:image/png;",
        "data:image/jpeg;",
        "data:image/webp;",
    ];
    // mistune percent-decodes up to three times to catch `javascript%3A`.
    let mut decoded = url.to_string();
    for _ in 0..3 {
        let next = urlencoding::decode(&decoded)
            .map(|d| d.into_owned())
            .unwrap_or_else(|_| decoded.clone());
        if next == decoded {
            break;
        }
        decoded = next;
    }
    let normalized = decoded.to_lowercase();
    let normalized = normalized.trim_start();
    let safe = SAFE_PROTOCOLS.iter().any(|p| normalized.starts_with(p))
        || GOOD_DATA_PROTOCOLS
            .iter()
            .any(|p| normalized.starts_with(p))
        || normalized.starts_with(['/', '#', '?'])
        || !normalized.split('/').next().unwrap_or("").contains(':');
    if safe {
        url
    } else {
        "#harmful-link".into()
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
    if !(s.starts_with("<p>") || s.ends_with("</p>")) {
        s = format!("<p>{s}</p>");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected outputs below were produced by isso's Python renderer
    /// (mistune + bleach) with the same configuration.
    fn renderer(ini: &str) -> Renderer {
        Renderer::from_config(&crate::config::Config::parse(ini).unwrap().markup).unwrap()
    }

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
    fn raw_html_is_escaped() {
        // The critical XSS invariant. If this test ever regresses, a
        // comment containing <script> would execute in the reader's browser.
        let r = Renderer::new();
        assert_eq!(
            r.render("<script>alert(1)</script>hello"),
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;hello</p>"
        );
        assert_eq!(
            r.render("<p onclick=\"alert(1)\">x</p>"),
            "<p>&lt;p onclick=&quot;alert(1)&quot;&gt;x&lt;/p&gt;</p>"
        );
        assert_eq!(
            r.render("<div>\nblock\n</div>"),
            "<p>&lt;div&gt;\nblock\n&lt;/div&gt;</p>"
        );
    }

    #[test]
    fn img_is_stripped_by_default() {
        let r = Renderer::new();
        assert_eq!(r.render("![cat](cat.jpg)"), "<p></p>");
    }

    #[test]
    fn img_allowed_when_configured_brings_src() {
        let r = renderer("[markup]\nallowed-elements = img\n");
        assert_eq!(r.render("![cat](cat.jpg)"), "<p><img src=\"cat.jpg\"></p>");
    }

    #[test]
    fn block_elements_split_the_paragraph_wrapper() {
        let r = Renderer::new();
        assert_eq!(
            r.render("```rust\nfn main() {}\n```"),
            "<p></p><pre><code class=\"language-rust\">fn main() {}\n</code></pre><p></p>"
        );
        assert_eq!(r.render("---"), "<p></p><hr><p></p>");
    }

    #[test]
    fn code_class_arbitrary_value_is_dropped() {
        // Anything that doesn't match ^language-[a-zA-Z0-9]{1,20}$ should be
        // dropped, per isso/html/__init__.py::allow_attribute_class.
        let r = Renderer::new();
        assert_eq!(
            r.sanitize("<code class=\"evil attr\">x</code>"),
            "<code>x</code>"
        );
    }

    #[test]
    fn hard_wrap_turns_newlines_into_breaks() {
        let r = Renderer::new();
        assert_eq!(
            r.render("line one\nline two"),
            "<p>line one<br>\nline two</p>"
        );
        assert_eq!(r.render("a\\\nb"), "<p>a\\<br>\nb</p>");

        let r = renderer("[markup.mistune]\nparameters = escape\n");
        assert_eq!(r.render("line one\nline two"), "<p>line one\nline two</p>");
        assert_eq!(r.render("a\\\nb"), "<p>a<br>\nb</p>");
    }

    #[test]
    fn subscript_and_superscript_match_mid_word() {
        let r = Renderer::new();
        assert_eq!(
            r.render("H~2~O and x^2^"),
            "<p>H<sub>2</sub>O and x<sup>2</sup></p>"
        );
        assert_eq!(r.render("a ^b c^ d"), "<p>a ^b c^ d</p>");
        assert_eq!(r.render("`x^2^`"), "<p><code>x^2^</code></p>");
        assert_eq!(
            r.render("```\nH~2~O\n```"),
            "<p></p><pre><code>H~2~O\n</code></pre><p></p>"
        );
    }

    #[test]
    fn strikethrough_needs_double_tilde() {
        let r = renderer("[markup.mistune]\nplugins = strikethrough\n");
        assert_eq!(r.render("~~gone~~"), "<p><del>gone</del></p>");
        assert_eq!(r.render("~kept~"), "<p>~kept~</p>");
    }

    #[test]
    fn no_smart_punctuation() {
        let r = Renderer::new();
        assert_eq!(
            r.render("\"quoted\" -- dash... & < >"),
            "<p>&quot;quoted&quot; -- dash... &amp; &lt; &gt;</p>"
        );
    }

    #[test]
    fn entities_are_not_decoded() {
        let r = Renderer::new();
        assert_eq!(
            r.render("&amp; &copy; &#169;"),
            "<p>&amp;amp; &amp;copy; &amp;#169;</p>"
        );
    }

    #[test]
    fn tables_need_the_table_plugin() {
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(
            Renderer::new().render(table),
            "<p>| a | b |<br>\n|---|---|<br>\n| 1 | 2 |</p>"
        );
        let r = renderer("[markup.mistune]\nplugins = table\n");
        assert_eq!(
            r.render(table),
            "<p></p><table><thead><tr><th>a</th><th>b</th></tr></thead><tbody>\n\
             <tr><td>1</td><td>2</td></tr>\n</tbody></table><p></p>"
        );
    }

    #[test]
    fn harmful_links_are_neutralised() {
        let r = Renderer::new();
        for link in [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt%3Aalert(1))",
            "[x](file:///etc/passwd)",
            "[x](data:text/html,x)",
            "[x](ssh://a)",
        ] {
            assert_eq!(
                r.render(link),
                "<p><a href=\"#harmful-link\" rel=\"nofollow noopener\">x</a></p>",
                "{link}"
            );
        }
        assert_eq!(
            r.render("[x](/rel)"),
            "<p><a href=\"/rel\" rel=\"nofollow noopener\">x</a></p>"
        );
    }

    #[test]
    fn unsupported_renderer_or_plugin_is_an_error() {
        let parse = |ini| Renderer::from_config(&crate::config::Config::parse(ini).unwrap().markup);
        assert_eq!(
            parse("[markup]\nrenderer = misaka\n")
                .err()
                .unwrap()
                .to_string(),
            "[markup] renderer = misaka is not supported; use mistune"
        );
        assert_eq!(
            parse("[markup.mistune]\nplugins = math\n")
                .err()
                .unwrap()
                .to_string(),
            "unsupported mistune plugin: math"
        );
    }

    #[test]
    fn existing_rel_values_are_preserved() {
        assert_eq!(
            apply_link_rel("<a href=\"x\" rel=\"me\">x</a>"),
            "<a href=\"x\" rel=\"me nofollow noopener\">x</a>"
        );
    }

    #[test]
    fn existing_rel_tokens_are_not_duplicated() {
        // If nofollow is already set, it must not be added twice
        // (case-insensitive), per set_links.
        assert_eq!(
            apply_link_rel("<a href=\"x\" rel=\"NoFollow\">x</a>"),
            "<a href=\"x\" rel=\"NoFollow noopener\">x</a>"
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
    fn quotes_in_attributes_are_left_alone() {
        assert_eq!(
            escape_text_quotes("<a href=\"x>y\" title=\"t\">\"q\"</a>"),
            "<a href=\"x>y\" title=\"t\">&quot;q&quot;</a>"
        );
    }
}
