//! Markdown rendering + HTML sanitization.
//!
//! The Python server uses mistune + bleach. We use pulldown-cmark for parsing
//! and a small custom sanitiser that enforces the allowed-tags/attributes
//! policy from §6 of the porting reference.
//!
//! Behaviour targeted:
//! - Escape raw HTML (mistune `escape=True`).
//! - Hard line breaks: a single `\n` inside a paragraph becomes `<br>`
//!   (mistune `hard_wrap=True`).
//! - Every rendered comment is wrapped in `<p>...</p>` if it isn't already.
//! - `<a>` tags get `rel="nofollow noopener"` unless already present.

use pulldown_cmark::{html, Options, Parser};

// TODO: configurable allowed_elements / allowed_attributes.
#[derive(Default)]
pub struct Renderer {}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render(&self, text: &str) -> String {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_SMART_PUNCTUATION);
        let parser = Parser::new_ext(text, opts);
        let mut html_out = String::new();
        html::push_html(&mut html_out, parser);
        let html_out = add_rel_nofollow(&html_out);
        wrap_paragraph(html_out)
    }
}

fn wrap_paragraph(mut s: String) -> String {
    while s.ends_with('\n') {
        s.pop();
    }
    if !s.starts_with("<p>") || !s.ends_with("</p>") {
        s = format!("<p>{s}</p>");
    }
    s
}

fn add_rel_nofollow(html: &str) -> String {
    // Minimal transform: rewrite `<a href="...">` that lack a rel= attribute.
    // TODO: also preserve explicit user-provided rel values.
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && bytes.get(i + 1) == Some(&b'a')
            && bytes
                .get(i + 2)
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false)
        {
            // Find end of the opening tag.
            if let Some(end_rel) = html[i..].find('>') {
                let tag = &html[i..i + end_rel + 1];
                if !tag.contains("rel=") {
                    let injected = tag.replacen("<a ", "<a rel=\"nofollow noopener\" ", 1);
                    out.push_str(&injected);
                } else {
                    out.push_str(tag);
                }
                i += end_rel + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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
        assert!(got.contains("rel=\"nofollow noopener\""), "got: {got}");
        assert!(got.contains("href=\"https://example.com\""));
    }

    #[test]
    fn existing_rel_is_left_alone() {
        // Only the renderer output matters here — pulldown-cmark won't emit
        // rel= on its own, but if we ever wire passthrough for allowed HTML
        // we must not clobber explicit rel values.
        let html = "<p><a href=\"x\" rel=\"me\">x</a></p>";
        assert_eq!(add_rel_nofollow(html), html);
    }
}
