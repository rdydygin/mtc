//! One-time preparation of a post's message for display.
//!
//! Runs once when a post enters a timeline, never during render: emoji
//! shortcodes become Unicode (or, for the server's custom emoji, an inline
//! image), bare URLs become CommonMark autolinks, and the message is
//! classified as plain or rich. Plain messages (the majority in
//! chat) render as ordinary text, which is far cheaper than a Markdown
//! document with its own parser state.

use std::sync::Arc;

use crate::CustomEmojis;

/// A message ready for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedMessage {
    text: Arc<str>,
    rich: bool,
}

impl PreparedMessage {
    pub fn new(message: &str, custom: &CustomEmojis) -> Self {
        let text = autolink(&emojify(message, custom));
        let rich = needs_rich_text(&text);
        Self {
            text: text.into(),
            rich,
        }
    }

    /// The display text: Markdown source when [`Self::is_rich`], plain text
    /// otherwise.
    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    /// Whether the text needs Markdown rendering.
    pub fn is_rich(&self) -> bool {
        self.rich
    }
}

/// Replace `:shortcode:` with the emoji it names, outside code. A custom
/// emoji becomes a Markdown image, which renders inline at text height.
pub fn emojify(text: &str, custom: &CustomEmojis) -> String {
    if !text.contains(':') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for (line_ix, line) in text.split('\n').enumerate() {
        if line_ix > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        emojify_line(line, custom, &mut out);
    }
    out
}

fn emojify_line(line: &str, custom: &CustomEmojis, out: &mut String) {
    let mut in_code = false;
    let mut rest = line;
    while let Some(ix) = rest.find([':', '`']) {
        let (before, after) = rest.split_at(ix);
        out.push_str(before);
        if let Some(code) = after.strip_prefix('`') {
            in_code = !in_code;
            out.push('`');
            rest = code;
            continue;
        }
        // `after` starts with ':'.
        let candidate = &after[1..];
        let end = candidate
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')))
            .unwrap_or(candidate.len());
        let name = &candidate[..end];
        if !in_code && !name.is_empty() && candidate[end..].starts_with(':') {
            // Standard emoji win: the server refuses custom names that clash.
            if let Some(emoji) = emoji_for(name) {
                out.push_str(emoji);
                rest = &candidate[end + 1..];
                continue;
            }
            if let Some(url) = custom.image_url(name) {
                out.push_str(&format!("![:{name}:]({url})"));
                rest = &candidate[end + 1..];
                continue;
            }
        }
        out.push(':');
        rest = candidate;
    }
    out.push_str(rest);
}

/// Look up a Mattermost emoji name, which mostly follows GitHub shortcodes.
pub fn emoji_for(name: &str) -> Option<&'static str> {
    let name = match name {
        "+1" | "thumbsup" => "+1",
        "-1" | "thumbsdown" => "-1",
        "white_check_mark" | "heavy_check_mark" => name,
        other => other,
    };
    emojis::get_by_shortcode(name).map(|e| e.as_str())
}

/// Wrap bare `http(s)://` URLs in `<…>` so Markdown renders them as links.
fn autolink(text: &str) -> String {
    if !text.contains("://") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_fence = false;
    for (line_ix, line) in text.split('\n').enumerate() {
        if line_ix > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence || line.contains('`') {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(ix) = rest.find("http") {
            let (before, candidate) = rest.split_at(ix);
            let is_url = candidate.starts_with("http://") || candidate.starts_with("https://");
            // Leave URLs that are already part of Markdown link syntax alone.
            let prev = before.chars().last();
            if !is_url || matches!(prev, Some('(' | '<' | '[' | '"' | '\'')) {
                out.push_str(before);
                out.push_str("http");
                rest = &candidate[4..];
                continue;
            }
            let end = candidate
                .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
                .unwrap_or(candidate.len());
            // Trailing punctuation usually ends the sentence, not the URL.
            let url = candidate[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
            out.push_str(before);
            out.push('<');
            out.push_str(url);
            out.push('>');
            rest = &candidate[url.len()..];
        }
        out.push_str(rest);
    }
    out
}

/// A conservative test for Markdown syntax that would render differently
/// from the literal text.
pub fn needs_rich_text(text: &str) -> bool {
    if text.contains(['*', '_', '`', '[', '<', '|', '~', '\\']) {
        return true;
    }
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#')
            || line.starts_with('>')
            || line.starts_with("- ")
            || line.starts_with("+ ")
            || line
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_shortcodes_outside_code() {
        let none = CustomEmojis::default();
        assert_eq!(emojify("ship it :rocket:", &none), "ship it 🚀");
        assert_eq!(emojify("`:rocket:` :+1:", &none), "`:rocket:` 👍");
        assert_eq!(emojify("time 10:30:45", &none), "time 10:30:45");
        assert_eq!(emojify("```\n:rocket:\n```", &none), "```\n:rocket:\n```");
        assert_eq!(emojify(":not_an_emoji_xyz:", &none), ":not_an_emoji_xyz:");
    }

    #[test]
    fn custom_emoji_become_inline_images() {
        let custom = CustomEmojis::new([(
            "party-parrot".to_string(),
            "https://chat.example.com/api/v4/emoji/e1/image".to_string(),
        )]);
        let message = PreparedMessage::new("ship it :party-parrot: :rocket:", &custom);
        assert_eq!(
            message.text().as_ref(),
            "ship it ![:party-parrot:](https://chat.example.com/api/v4/emoji/e1/image) 🚀"
        );
        assert!(message.is_rich());
        assert_eq!(
            emojify("`:party-parrot:`", &custom),
            "`:party-parrot:`",
            "code stays literal"
        );
        assert!(
            !PreparedMessage::new("no :party-parrot: here", &CustomEmojis::default()).is_rich()
        );
    }

    #[test]
    fn autolinks_bare_urls_only() {
        assert_eq!(
            autolink("see https://example.com/a."),
            "see <https://example.com/a>."
        );
        assert_eq!(
            autolink("[x](https://example.com)"),
            "[x](https://example.com)"
        );
        assert_eq!(autolink("httpd is fine"), "httpd is fine");
    }

    #[test]
    fn classifies_plain_and_rich() {
        let prepare = |text| PreparedMessage::new(text, &CustomEmojis::default());
        assert!(!prepare("hello there\nsecond line").is_rich());
        assert!(prepare("**bold**").is_rich());
        assert!(prepare("1. first").is_rich());
        assert!(prepare("go to https://x.y").is_rich());
        assert!(!prepare("version 1.2 is out").is_rich());
    }
}
