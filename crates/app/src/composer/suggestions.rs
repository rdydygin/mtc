//! Caret-aware completion tokens and the shared suggestion presentation model.
use std::{ops::Range, sync::Arc};

use mattermost::{Channel, TeamId, User};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Command,
    Person,
    Channel,
    Emoji,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub kind: Kind,
    pub range: Range<usize>,
    pub query: String,
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// UTF-8 byte offsets, like TextareaState::cursor(). Replace the entire token
/// even when the caret is in its middle, but search only its prefix.
pub(super) fn token_at(text: &str, caret: usize) -> Option<Token> {
    let before = text.get(..caret)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !name_char(*c))
        .map(|(ix, _)| ix)?;
    let marker = text[start..].chars().next()?;
    let kind = match marker {
        '/' if start == 0 => Kind::Command,
        '@' => Kind::Person,
        '~' => Kind::Channel,
        ':' if before[start + 1..]
            .chars()
            .filter(|c| c.is_alphabetic())
            .count()
            >= 2 =>
        {
            Kind::Emoji
        }
        _ => return None,
    };
    // Punctuation can start a token in prose, but a query/fragment marker
    // inside the same URL word must not turn Enter into completion.
    let word = before.split_whitespace().next_back()?;
    let autolink = word.trim_start_matches(['(', '[', '<', '"', '\'']);
    if word.contains("://") || autolink.to_ascii_lowercase().starts_with("www.") {
        return None;
    }
    if text[..start].chars().next_back().is_some_and(|c| {
        c.is_alphanumeric() || matches!(c, '_' | '/' | '\\' | '@' | '~' | '.' | ':')
    }) || in_code(&text[..start])
    {
        return None;
    }
    let mut end = caret
        + text[caret..]
            .find(|c: char| !name_char(c))
            .unwrap_or(text.len() - caret);
    if kind == Kind::Emoji && text[end..].starts_with(':') {
        end += 1;
    }
    Some(Token {
        kind,
        range: start..end,
        query: text[start + 1..caret].to_lowercase(),
    })
}

/// Lightweight Markdown guard: backtick spans (including multi-backtick
/// delimiters), backtick/tilde fences, and escaped delimiters.
fn in_code(text: &str) -> bool {
    let mut fence = None;
    let mut inline = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if indent <= 3 && matches!(marker, '`' | '~') && run >= 3 {
            match fence {
                Some((open, width))
                    if marker == open
                        && run >= width
                        && trimmed[run..].trim().is_empty()
                        // `text` stops just before the completion marker.
                        // An unterminated final line contains that marker in
                        // the full message, so it cannot be a closing fence.
                        && line.ends_with('\n') =>
                {
                    fence = None;
                }
                None => fence = Some((marker, run)),
                _ => {}
            }
            inline = 0;
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next();
            } else if c == '`' {
                let mut width = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    width += 1;
                }
                if inline == 0 {
                    inline = width;
                } else if inline == width {
                    inline = 0;
                }
            }
        }
    }
    fence.is_some()
        || inline != 0
        || text
            .rsplit('\n')
            .next()
            .is_some_and(|line| line.starts_with("    ") || line.starts_with('\t'))
}

#[derive(Clone)]
pub(super) struct Suggestion {
    pub id: String,
    pub completion: String,
    pub label: String,
    pub detail: String,
    pub user: Option<Arc<User>>,
}

impl Suggestion {
    pub fn person(user: Arc<User>) -> Self {
        let full_name = format!("{} {}", user.first_name, user.last_name);
        Self {
            id: format!("person-{}", user.id),
            completion: format!("@{}", user.username),
            label: if full_name.trim().is_empty() {
                user.display_name()
            } else {
                full_name.trim().to_string()
            },
            detail: format!("@{}", user.username),
            user: Some(user),
        }
    }
}

pub(super) fn user_matches(user: &User, query: &str) -> bool {
    user.delete_at == 0
        && [
            &user.username,
            &user.first_name,
            &user.last_name,
            &user.nickname,
        ]
        .iter()
        .any(|name| name.to_lowercase().starts_with(query))
}

pub(super) fn channel_matches<'a>(
    channels: impl Iterator<Item = &'a Channel>,
    team: &TeamId,
    query: &str,
) -> Vec<Suggestion> {
    let mut channels: Vec<_> = channels
        .filter(|c| {
            &c.team_id == team
                && !c.kind.is_direct_or_group()
                && c.delete_at == 0
                && (c.name.to_lowercase().starts_with(query)
                    || c.display_name.to_lowercase().starts_with(query))
        })
        .collect();
    channels.sort_by_key(|c| (c.display_name.to_lowercase(), &c.id));
    channels
        .into_iter()
        .take(6)
        .map(|c| Suggestion {
            id: format!("channel-{}", c.id),
            completion: format!("~{}", c.name),
            label: c.display_name.clone(),
            detail: format!("~{}", c.name),
            user: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_only_blocks_tokens_on_the_current_line() {
        for token in [":sm", "@ali", "~town"] {
            for indent in ["    ", "\t"] {
                let after = format!("{indent}code\n{token}");
                assert!(token_at(&after, after.len()).is_some(), "{after:?}");
                let inside = format!("previous line\n{indent}{token}");
                assert!(token_at(&inside, inside.len()).is_none(), "{inside:?}");
            }
        }
    }

    #[test]
    fn emoji_tokens_require_a_boundary_and_two_letters_outside_code_or_urls() {
        for text in [
            ":sm",
            "(:sm",
            "hi :sm",
            "🙂 :sm",
            "`code` :sm",
            "```\ncode\n```\n:sm",
        ] {
            let token = token_at(text, text.len()).unwrap();
            assert_eq!(token.kind, Kind::Emoji);
            assert_eq!(token.query, "sm");
        }
        for text in [
            ":",
            ":s",
            "10:30",
            "word:sm",
            "::sm",
            "\\:sm",
            "https://host/:sm",
            "www.host/:sm",
            "` :sm",
            "```\n:sm",
            "~~~\n:sm",
            "    :sm",
            "\t:sm",
            "mail:someone",
        ] {
            assert!(token_at(text, text.len()).is_none(), "{text}");
        }
        assert_eq!(token_at(":smile:", 3).unwrap().range, 0..7);
    }

    #[test]
    fn urls_do_not_offer_mentions_or_channels() {
        for marker in ['@', '~'] {
            for prefix in [
                "https://example.com/?",
                "https://example.com/#",
                "https://example.com/?x=",
                "www.example.com/#",
                "WWW.example.com/?",
                "(<www.example.com/?",
                "<https://example.com/#",
            ] {
                let text = format!("see {prefix}{marker}he");
                assert!(token_at(&text, text.len()).is_none(), "{text}");
            }
            for prefix in [
                "(",
                "[",
                "a sentence, ",
                "https://example.com/ ",
                "www.example.com/\n(",
            ] {
                let text = format!("{prefix}{marker}he");
                assert!(token_at(&text, text.len()).is_some(), "{text}");
            }
        }
        // The URL suffix after the caret is retained too.
        let text = "https://example.com/#@here/path";
        assert!(token_at(text, text.find("here").unwrap() + 2).is_none());
    }

    #[test]
    fn fences_only_close_with_a_matching_marker_and_whitespace_suffix() {
        for text in ["```\n``` @he", "~~~\n~~~ ~he"] {
            assert!(token_at(text, text.len()).is_none(), "{text}");
        }
        for (open, illegal_close) in [
            ("```rust", "```not-a-close"),
            ("```", "```not-a-close ```"),
            ("~~~~", "~~~~not-a-close"),
            ("````", "```"),
            ("~~~", "```"),
            ("```", "    ```"),
            ("```", "\t```"),
            ("```", "   ``` x"),
        ] {
            for marker in ['@', '~'] {
                let text = format!("{open}\n{illegal_close}\n{marker}he");
                assert!(token_at(&text, text.len()).is_none(), "{text}");
            }
        }
        for (open, close) in [
            ("```rust", "```"),
            ("```", "   ```` \t"),
            ("  ~~~rust", " ~~~~\t"),
        ] {
            let text = format!("{open}\ncode\n{close}\n@he");
            assert!(token_at(&text, text.len()).is_some(), "{text}");
        }
    }

    #[test]
    fn detects_tokens_at_the_caret() {
        for (text, caret, query, range) in [
            ("@an", 3, "an", 0..3),
            ("Hi (@anna), next", 7, "an", 4..9),
            ("@", 1, "", 0..1),
            ("привет @анна!", 18, "ан", 13..22),
            ("hello\n~town", 11, "town", 6..11),
        ] {
            let token =
                token_at(text, caret).unwrap_or_else(|| panic!("no token in {text:?} at {caret}"));
            assert_eq!(token.query, query, "{text}");
            assert_eq!(token.range, range, "{text}");
        }
        assert_eq!(token_at("/shrug", 4).unwrap().kind, Kind::Command);
        for text in [
            "a@b.c",
            "word~town",
            "`@an",
            "``@an",
            "```rust\n@an",
            "~~~\n@an",
            "path/@an",
            "\\@an",
            "@@an",
            "x /shrug",
            "hello world",
            "@an ",
        ] {
            assert!(token_at(text, text.len()).is_none(), "{text}");
        }
        for text in [
            "`code` @an",
            "```\ncode\n```\n@an",
            "``a`b`` @an",
            "~~~\ncode\n~~~\n@an",
        ] {
            assert!(token_at(text, text.len()).is_some(), "{text}");
        }
    }

    #[test]
    fn channels_are_joined_team_channels_in_stable_order() {
        let channels: Vec<Channel> = serde_json::from_value(serde_json::json!([
            {"id":"b", "team_id":"team", "name":"town-square", "display_name":"Town Square"},
            {"id":"a", "team_id":"team", "name":"town-hall", "display_name":"Town Hall"},
            {"id":"c", "team_id":"other", "name":"town-other"},
            {"id":"d", "team_id":"team", "name":"town-deleted", "delete_at":1},
            {"id":"e", "team_id":"team", "name":"town-dm", "type":"D"}
        ]))
        .unwrap();
        let matches = channel_matches(channels.iter(), &"team".into(), "town");
        assert_eq!(
            matches
                .iter()
                .map(|c| c.completion.as_str())
                .collect::<Vec<_>>(),
            ["~town-hall", "~town-square"]
        );
        assert!(channel_matches(channels.iter(), &"team".into(), "missing").is_empty());
    }
}
