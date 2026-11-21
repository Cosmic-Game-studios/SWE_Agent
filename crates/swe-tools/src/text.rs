//! Text helpers shared by the tools: truncation, diffs, HTML to text.

use similar::{ChangeTag, TextDiff};

/// Keep the head and the tail of `text` when it is longer than `max_bytes`,
/// cutting on character boundaries and saying how much was dropped.
pub fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let half = max_bytes / 2;
    let mut head_end = half;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - half;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let dropped = tail_start - head_end;
    format!(
        "{}\n\n[... {dropped} bytes truncated ...]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Cut a single line to `max_chars` characters.
pub fn truncate_line(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}… [line truncated]", &line[..index]),
        None => line.to_string(),
    }
}

pub struct DiffSummary {
    pub unified: String,
    pub added: u32,
    pub removed: u32,
}

/// Unified diff between two versions of a file.
pub fn unified_diff(path: &str, before: &str, after: &str) -> DiffSummary {
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    DiffSummary { unified, added, removed }
}

/// Reduce an HTML document to readable plain text.
///
/// This is intentionally small: drop non-content elements, turn block-level
/// tags into line breaks, keep link targets, decode common entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        push_text(&mut out, &rest[..open]);
        let after = &rest[open..];
        if let Some(stripped) = after.strip_prefix("<!--") {
            rest = stripped.find("-->").map(|end| &stripped[end + 3..]).unwrap_or("");
            continue;
        }
        let Some(close) = after.find('>') else {
            rest = "";
            break;
        };
        let tag = &after[1..close];
        rest = &after[close + 1..];
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');
        match name.as_str() {
            "script" | "style" | "noscript" | "svg" | "head" | "template" if !closing => {
                // Skip everything up to the matching end tag.
                let end_tag = format!("</{name}");
                let lower = rest.to_ascii_lowercase();
                rest = match lower.find(&end_tag) {
                    Some(end) => {
                        let tail = &rest[end..];
                        tail.find('>').map(|gt| &tail[gt + 1..]).unwrap_or("")
                    }
                    None => "",
                };
            }
            "br" => out.push('\n'),
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav" | "ul" | "ol"
            | "table" | "tr" | "blockquote" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "dl" | "dt" | "dd"
            | "figure" | "form" => out.push_str("\n\n"),
            "li" if !closing => out.push_str("\n- "),
            "td" | "th" if closing => out.push('\t'),
            "a" if !closing => {
                if let Some(href) = attribute(tag, "href") {
                    if href.starts_with("http") {
                        out.push_str(&format!("[{href}] "));
                    }
                }
            }
            _ => {}
        }
    }
    push_text(&mut out, rest);
    collapse_whitespace(&out)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find(&format!("{name}="))? + name.len() + 1;
    let value = &tag[start..];
    let quote = value.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &value[1..];
        inner.find(quote).map(|end| decode_entities(&inner[..end]))
    } else {
        Some(value.split_whitespace().next().unwrap_or("").to_string())
    }
}

fn push_text(out: &mut String, raw: &str) {
    out.push_str(&decode_entities(raw));
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let end = after.find(';').filter(|&end| end <= 10);
        let decoded = end.and_then(|end| {
            let entity = &after[1..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "copy" => Some('©'),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|ch| (ch, end))
        });
        match decoded {
            Some((ch, end)) => {
                out.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse runs of spaces and limit consecutive blank lines to one.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run == 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_keeps_both_ends_on_char_boundaries() {
        let text = format!("{}{}{}", "a".repeat(50), "é".repeat(100), "z".repeat(50));
        let cut = truncate_middle(&text, 120);
        assert!(cut.starts_with("aaaa"));
        assert!(cut.ends_with("zzzz"));
        assert!(cut.contains("bytes truncated"));
        assert_eq!(truncate_middle("short", 120), "short");
    }

    #[test]
    fn diff_counts_changed_lines() {
        let summary = unified_diff("src/a.rs", "one\ntwo\nthree\n", "one\n2\nthree\nfour\n");
        assert_eq!((summary.added, summary.removed), (2, 1));
        assert!(summary.unified.contains("--- a/src/a.rs"));
        assert!(summary.unified.contains("+2"));
        assert!(summary.unified.contains("-two"));
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = r#"<html><head><title>T</title><style>p{color:red}</style></head>
            <body><h1>Docs &amp; Guides</h1><script>alert("x < y")</script>
            <p>Use <a href="https://example.com/api">the API</a> &mdash; it&#39;s fast.</p>
            <ul><li>One</li><li>Two</li></ul><!-- hidden --></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Docs & Guides"));
        assert!(text.contains("[https://example.com/api] the API — it's fast."));
        assert!(text.contains("- One\n- Two"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("hidden"));
    }
}
