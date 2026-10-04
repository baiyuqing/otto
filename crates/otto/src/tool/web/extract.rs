//! Turns a fetched body into the text the model reads.
//!
//! HTML becomes Markdown with scripts, styles and other non-content elements
//! removed. When the page has a `<main>` or `<article>` element only that
//! region is converted. Other textual types are returned as decoded text.

use std::borrow::Cow;

/// Elements that never carry readable content.
const SKIPPED_TAGS: &[&str] = &[
    "script", "style", "noscript", "svg", "template", "iframe", "canvas", "head", "nav", "footer",
    "aside", "form", "button", "select", "option",
];

/// How a response body is read, from its `Content-Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Html,
    Text,
    Unsupported,
}

pub(super) fn classify(content_type: &str) -> Kind {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "" => Kind::Text,
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "application/json" | "application/xml" | "application/javascript"
        | "application/x-ndjson" | "application/yaml" | "application/x-yaml"
        | "application/toml" | "application/rss+xml" | "application/atom+xml" => Kind::Text,
        other if other.starts_with("text/") => Kind::Text,
        other if other.ends_with("+json") || other.ends_with("+xml") => Kind::Text,
        _ => Kind::Unsupported,
    }
}

/// The `charset` parameter of a content type, if any.
fn header_charset(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"'))
    })
}

/// A charset named by a `<meta>` tag in the first kilobytes of an HTML body.
fn meta_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(4096)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let at = text.find("charset")?;
    let rest = text[at + "charset".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))
        .unwrap_or(rest.len());
    let label = &rest[..end];
    (!label.is_empty()).then(|| label.to_string())
}

/// Decodes `body` using the header charset, then a `<meta>` charset for HTML,
/// then UTF-8. Invalid sequences become U+FFFD.
pub(super) fn decode(body: &[u8], content_type: &str, html: bool) -> String {
    let label = header_charset(content_type)
        .map(str::to_owned)
        .or_else(|| html.then(|| meta_charset(body)).flatten());
    let encoding = label
        .as_deref()
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    match text {
        Cow::Borrowed(text) => text.to_owned(),
        Cow::Owned(text) => text,
    }
}

/// The region of `html` to convert: the first `<main>` or `<article>` element
/// when one exists, else the whole document.
fn content_region(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    for tag in ["main", "article"] {
        let Some(start) = find_open_tag(&lower, tag) else {
            continue;
        };
        let close = format!("</{tag}>");
        if let Some(end) = lower[start..].rfind(&close) {
            return &html[start..start + end + close.len()];
        }
    }
    html
}

/// The byte offset of the first `<tag` that is a real open tag (`<main>` or
/// `<main ...>`, not `<mainframe>`).
fn find_open_tag(lower: &str, tag: &str) -> Option<usize> {
    let needle = format!("<{tag}");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        match lower.as_bytes().get(at + needle.len()) {
            Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/') => return Some(at),
            _ => from = at + needle.len(),
        }
    }
    None
}

/// Converts HTML to Markdown. Never panics on malformed markup.
pub(super) fn html_to_markdown(html: &str) -> String {
    let region = content_region(html);
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .build();
    let markdown = converter
        .convert(region)
        .unwrap_or_else(|_| region.to_owned());
    collapse_blank_lines(&markdown)
}

/// Trims trailing whitespace and limits runs of blank lines to one.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Resolves `[text](relative)` links against `base`, so a model reading the
/// Markdown can pass the target to `web_fetch` unchanged.
pub(super) fn absolutize_links(markdown: &str, base: &url::Url) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(open) = rest.find("](") {
        let (head, tail) = rest.split_at(open + 2);
        out.push_str(head);
        let Some(close) = tail.find(')') else {
            rest = tail;
            break;
        };
        let target = &tail[..close];
        let bare = target.split_once(' ').map_or(target, |(url, _)| url);
        let title = &target[bare.len()..];
        match base.join(bare) {
            Ok(joined) if !bare.starts_with('#') && !bare.is_empty() => {
                out.push_str(joined.as_str());
                out.push_str(title);
            }
            _ => out.push_str(target),
        }
        out.push(')');
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

/// One window of `text` by character offset: at most `max_chars` characters
/// and `max_bytes` bytes. Returns the slice, the offset to continue from when
/// more follows, and the total character count.
pub(super) fn window(
    text: &str,
    offset: usize,
    max_chars: usize,
    max_bytes: usize,
) -> (String, Option<usize>, usize) {
    let total = text.chars().count();
    if offset >= total {
        return (String::new(), None, total);
    }
    let mut slice = String::new();
    let mut taken = 0;
    for c in text.chars().skip(offset) {
        if taken == max_chars || slice.len() + c.len_utf8() > max_bytes {
            break;
        }
        slice.push(c);
        taken += 1;
    }
    let end = offset + taken;
    (slice, (end < total).then_some(end), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_content_types() {
        assert_eq!(classify("text/html; charset=utf-8"), Kind::Html);
        assert_eq!(classify("Application/XHTML+xml"), Kind::Html);
        assert_eq!(classify("text/plain"), Kind::Text);
        assert_eq!(classify("application/json"), Kind::Text);
        assert_eq!(classify("application/ld+json"), Kind::Text);
        assert_eq!(classify(""), Kind::Text);
        for binary in ["image/png", "application/pdf", "application/octet-stream", "application/zip"] {
            assert_eq!(classify(binary), Kind::Unsupported, "{binary}");
        }
    }

    #[test]
    fn removes_scripts_styles_and_navigation() {
        let html = r#"<html><head><title>T</title><style>.a{color:red}</style></head>
<body><nav>menu menu</nav><script>var secret = 1;</script>
<h1>Title</h1><p>Hello <b>world</b> and <a href="/x">a link</a>.</p>
<footer>copyright</footer></body></html>"#;
        let text = html_to_markdown(html);
        assert!(text.contains("# Title"), "{text}");
        assert!(text.contains("Hello **world**"), "{text}");
        assert!(text.contains("[a link](/x)"), "{text}");
        for gone in ["secret", "color:red", "menu", "copyright"] {
            assert!(!text.contains(gone), "{gone} survived: {text}");
        }
    }

    #[test]
    fn prefers_the_main_region() {
        let html = "<body><div>sidebar noise</div><main><h2>Real</h2><p>content</p></main><div>more noise</div></body>";
        let text = html_to_markdown(html);
        assert!(text.contains("Real") && text.contains("content"), "{text}");
        assert!(!text.contains("noise"), "{text}");
        let article = "<body><p>outside</p><article><p>inside</p></article></body>";
        let text = html_to_markdown(article);
        assert!(text.contains("inside") && !text.contains("outside"), "{text}");
    }

    #[test]
    fn a_lookalike_tag_is_not_a_main_region() {
        let html = "<body><p>keep me</p><mainframe>x</mainframe></body>";
        assert!(html_to_markdown(html).contains("keep me"));
    }

    #[test]
    fn keeps_tables_and_code_blocks() {
        let html = "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table><pre><code>fn main() {}</code></pre>";
        let text = html_to_markdown(html);
        assert!(text.contains("fn main() {}"), "{text}");
        assert!(text.contains('1') && text.contains('2'), "{text}");
    }

    #[test]
    fn malformed_markup_does_not_panic() {
        let text = html_to_markdown("<div><p>unclosed <b>bold <i>x</div></p>");
        assert!(text.contains("unclosed"), "{text}");
        assert_eq!(html_to_markdown(""), "");
    }

    #[test]
    fn decodes_header_and_meta_charsets() {
        let gbk = [0xC4u8, 0xE3, 0xBA, 0xC3]; // 你好
        assert_eq!(decode(&gbk, "text/plain; charset=gbk", false), "你好");
        let mut html = b"<meta charset=\"gb2312\"><p>".to_vec();
        html.extend_from_slice(&gbk);
        assert!(decode(&html, "text/html", true).contains("你好"));
        assert_eq!(decode("héllo".as_bytes(), "text/plain", false), "héllo");
        assert!(decode(&[0xff, 0xfe, b'a'], "text/plain", false).contains('\u{fffd}'));
        // The header wins over the page.
        let latin = [0xE9u8];
        assert_eq!(
            decode(&latin, "text/html; charset=iso-8859-1", true),
            "\u{e9}"
        );
    }

    #[test]
    fn makes_links_absolute() {
        let base = url::Url::parse("https://example.com/docs/page").unwrap();
        let md = "see [a](/x) and [b](rel.html \"t\") and [c](https://o.example/y) and [d](#top)";
        assert_eq!(
            absolutize_links(md, &base),
            "see [a](https://example.com/x) and [b](https://example.com/docs/rel.html \"t\") and [c](https://o.example/y) and [d](#top)"
        );
    }

    #[test]
    fn windows_by_characters_and_reports_the_next_offset() {
        let text = "你好世界abc";
        assert_eq!(window(text, 0, 2, 100), ("你好".into(), Some(2), 7));
        assert_eq!(window(text, 2, 10, 100), ("世界abc".into(), None, 7));
        assert_eq!(window(text, 7, 5, 100), (String::new(), None, 7));
        assert_eq!(window(text, 99, 5, 100), (String::new(), None, 7));
        // A byte budget never splits a character: 3 bytes fits one CJK char.
        assert_eq!(window(text, 0, 10, 5), ("你".into(), Some(1), 7));
    }

    #[test]
    fn collapses_blank_runs() {
        assert_eq!(collapse_blank_lines("a  \n\n\n\nb\n"), "a\n\nb");
    }
}
