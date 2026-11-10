//! One-time preparation of a post's message for display.
//!
//! Runs once when a post enters a timeline, never during render: emoji
//! shortcodes become Unicode (or, for the server's custom emoji, an inline
//! image), emphasis written with a space inside its markers is tightened,
//! `@username`s become links to the person, bare URLs become
//! CommonMark autolinks, and the message is classified as plain or rich. Plain messages (the majority in
//! chat) render as ordinary text, which is far cheaper than a Markdown
//! document with its own parser state.

use std::{borrow::Cow, ops::Range, sync::Arc};

use markdown::mdast;

use crate::CustomEmojis;

/// A message ready for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedMessage {
    text: Arc<str>,
    rich: bool,
}

impl PreparedMessage {
    pub fn new(message: &str, custom: &CustomEmojis) -> Self {
        let text = tighten_emphasis(&emojify(message, custom));
        Self::from_text(autolink(&link_mentions(&text)))
    }

    /// A system message, such as "@ann joined the channel." These are drawn
    /// as plain text, so their mentions stay as written rather than turning
    /// into link syntax.
    pub fn system(message: &str, custom: &CustomEmojis) -> Self {
        Self::from_text(autolink(&emojify(message, custom)))
    }

    fn from_text(text: String) -> Self {
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
    scan(text, |piece| match piece {
        Piece::Text(text) => {
            out.push_str(text);
            true
        }
        Piece::Shortcode(name) => {
            // Standard emoji win: the server refuses custom names that clash.
            if let Some(emoji) = emoji_for(&name) {
                out.push_str(emoji);
                true
            } else if let Some(url) = custom.image_url(&name) {
                out.push_str(&format!("![:{name}:]({url})"));
                true
            } else {
                false
            }
        }
    });
    out
}

/// The `:shortcode:` names outside code in `text` that could be custom
/// emoji: well formed and not a standard emoji. Some may name nothing.
pub fn custom_emoji_names(text: &str) -> Vec<Cow<'_, str>> {
    let mut names = Vec::new();
    if text.contains(':') {
        scan(text, |piece| match piece {
            Piece::Text(_) => true,
            Piece::Shortcode(name) => {
                if emoji_for(&name).is_none() {
                    names.push(name);
                }
                false
            }
        });
    }
    names
}

/// The name in an emoji field such as a custom status, which some clients
/// fill with the shortcode (`:tada:`) rather than the name (`tada`).
pub fn emoji_name(field: &str) -> &str {
    let field = field.trim();
    field
        .strip_prefix(':')
        .and_then(|f| f.strip_suffix(':'))
        .filter(|name| !name.is_empty())
        .unwrap_or(field)
}

/// Whether `name` can name an emoji: letters, digits, `_`, `+`, and `-`.
pub fn is_emoji_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(is_name_char)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

enum Piece<'a> {
    /// Text to keep as it is.
    Text(&'a str),
    /// The name in a `:name:` outside code.
    Shortcode(Cow<'a, str>),
}

/// Split `text` into plain text and `:shortcode:`s outside code. `piece`
/// returns whether it consumed a shortcode; when it didn't, the opening
/// colon is passed on as text and scanning resumes after it, so the
/// closing colon can open the next shortcode.
fn scan<'a>(text: &'a str, mut piece: impl FnMut(Piece<'a>) -> bool) {
    let mut in_fence = false;
    for (line_ix, line) in text.split('\n').enumerate() {
        if line_ix > 0 {
            piece(Piece::Text("\n"));
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            piece(Piece::Text(line));
            continue;
        }
        if in_fence {
            piece(Piece::Text(line));
            continue;
        }
        scan_line(line, &mut piece);
    }
}

fn scan_line<'a>(line: &'a str, piece: &mut impl FnMut(Piece<'a>) -> bool) {
    let mut in_code = false;
    let mut rest = line;
    while let Some(ix) = rest.find([':', '`']) {
        let (before, after) = rest.split_at(ix);
        piece(Piece::Text(before));
        if let Some(code) = after.strip_prefix('`') {
            in_code = !in_code;
            piece(Piece::Text("`"));
            rest = code;
            continue;
        }
        // `after` starts with ':'.
        let candidate = &after[1..];
        let end = name_end(candidate);
        let name = &candidate[..end];
        if !in_code
            && !name.is_empty()
            && candidate[end..].starts_with(':')
            && piece(Piece::Shortcode(unescape_name(name)))
        {
            rest = &candidate[end + 1..];
            continue;
        }
        piece(Piece::Text(":"));
        rest = candidate;
    }
    piece(Piece::Text(rest));
}

/// Where the emoji name that starts `text` ends. An underscore may be
/// written `\_`: editors that escape Markdown do that to every one, and the
/// text would show the same name either way.
fn name_end(text: &str) -> usize {
    let mut end = 0;
    loop {
        let rest = &text[end..];
        if rest.starts_with(r"\_") {
            end += 2;
        } else if rest.starts_with(is_name_char) {
            end += 1;
        } else {
            return end;
        }
    }
}

/// An emoji name without the backslashes [`name_end`] let through.
fn unescape_name(name: &str) -> Cow<'_, str> {
    if name.contains('\\') {
        name.replace(r"\_", "_").into()
    } else {
        name.into()
    }
}

/// Look up a Mattermost emoji name, which mostly follows GitHub shortcodes.
pub fn emoji_for(name: &str) -> Option<&'static str> {
    let name = match name {
        "+1" | "thumbsup" => "+1",
        "-1" | "thumbsdown" => "-1",
        other => other,
    };
    emojis::get_by_shortcode(name).map(|e| e.as_str())
}

/// What a mention links to: this, then the username.
const MENTION_SCHEME: &str = "mention:";

/// Mentions that address everyone in the channel, not one person.
const GROUP_MENTIONS: [&str; 3] = ["all", "channel", "here"];

/// The username a mention link leads to, for a link [`PreparedMessage::new`]
/// made from `@username`.
pub fn mention_username(url: &str) -> Option<&str> {
    url.strip_prefix(MENTION_SCHEME)
        .filter(|name| !name.is_empty())
}

/// Turn `@username` in a message's text into a link that [`mention_username`]
/// reads back. As in Mattermost, an `@` inside a word (an email address)
/// starts no mention, and a trailing `.`, `-`, or `_` ends the sentence, not
/// the name. `@channel`, `@all`, and `@here` stay text: they name no one
/// person.
///
/// Only text counts: code in all its forms, math, HTML, and links (whose text
/// can't hold another link) are left as written. Telling them apart takes the
/// Markdown parser the text view uses, so it runs only on messages with an
/// `@` that could start a mention.
fn link_mentions(text: &str) -> String {
    let maybe_mention = text
        .match_indices('@')
        .any(|(ix, _)| mention_at(&text[ix + 1..]).is_some());
    if !maybe_mention {
        return text.to_string();
    }
    let Ok(tree) = markdown::to_mdast(text, &parse_options()) else {
        return text.to_string();
    };
    let mut runs: Vec<Range<usize>> = Vec::new();
    text_runs(&tree, &mut runs);
    let mut out = String::with_capacity(text.len() + 32);
    let mut copied = 0;
    for run in runs {
        let mut prev = text[..run.start].chars().next_back();
        for (ix, c) in text[run.clone()].char_indices() {
            let at = run.start + ix;
            if c == '@'
                && starts_mention(prev)
                && let Some(name) = mention_at(&text[at + 1..run.end])
            {
                out.push_str(&text[copied..at]);
                out.push_str(&format!(
                    "[@{name}]({MENTION_SCHEME}{})",
                    name.to_ascii_lowercase()
                ));
                copied = at + 1 + name.len();
            }
            prev = Some(c);
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The parser options `TextView` renders Markdown with: GitHub's dialect,
/// with math.
fn parse_options() -> markdown::ParseOptions {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// Collect where `node`'s text lies in the source, in order, outside links.
/// Runs that touch are joined, so a name split across two text nodes reads
/// as one.
fn text_runs(node: &mdast::Node, runs: &mut Vec<Range<usize>>) {
    match node {
        mdast::Node::Text(text) => {
            if let Some(position) = &text.position {
                let run = position.start.offset..position.end.offset;
                match runs.last_mut() {
                    Some(last) if last.end == run.start => last.end = run.end,
                    _ => runs.push(run),
                }
            }
        }
        mdast::Node::Link(_) | mdast::Node::LinkReference(_) => {}
        _ => {
            for child in node.children().into_iter().flatten() {
                text_runs(child, runs);
            }
        }
    }
}

/// Whether an `@` after `prev` can start a mention: not inside a word or a
/// path, and not escaped.
fn starts_mention(prev: Option<char>) -> bool {
    !prev.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '/' | '\\' | '@'))
}

/// The username at the start of `rest`, the text after an `@`: a letter,
/// then letters, digits, `.`, `-`, and `_`, without trailing punctuation.
fn mention_at(rest: &str) -> Option<&str> {
    if !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .unwrap_or(rest.len());
    let name = rest[..end].trim_end_matches(['.', '-', '_']);
    (!GROUP_MENTIONS
        .iter()
        .any(|group| name.eq_ignore_ascii_case(group)))
    .then_some(name)
}

/// Move whitespace from just inside an emphasis marker to just outside it,
/// so `**bold **` and `* italic*` read as emphasis. CommonMark, which the
/// text view follows, takes such asterisks literally, yet they are what
/// bolding a selection that ends in a space leaves behind.
///
/// Only asterisks the parser left as text count, and only a pair that is
/// emphasis once tightened: anything else stays as written.
fn tighten_emphasis(text: &str) -> String {
    if !text.contains('*') {
        return text.to_string();
    }
    let Ok(tree) = markdown::to_mdast(text, &parse_options()) else {
        return text.to_string();
    };
    let mut existing = Vec::new();
    emphasis_spans(&tree, &mut existing);
    let mut pairs = Vec::new();
    loose_pairs(&tree, text, &mut pairs);
    // In source order, and never a pair inside another.
    pairs.sort_by_key(|(open, _)| open.start);
    let mut end = 0;
    pairs.retain(|(open, close)| {
        let outside = open.start >= end;
        if outside {
            end = close.end;
        }
        outside
    });
    while !pairs.is_empty() {
        let (tightened, spans) = tighten(text, &pairs);
        let mut emphasized = Vec::new();
        if let Ok(tree) = markdown::to_mdast(&tightened, &parse_options()) {
            emphasis_spans(&tree, &mut emphasized);
        }
        let proposed = pairs.len();
        let mut spans = spans.iter();
        pairs.retain(|_| spans.next().is_some_and(|span| emphasized.contains(span)));
        if pairs.len() == proposed {
            // Whitespace moved out of one marker can land inside another,
            // costing emphasis the message had.
            let kept = emphasized.len() >= existing.len() + proposed;
            return if kept { tightened } else { text.to_string() };
        }
    }
    text.to_string()
}

/// An opening and a closing emphasis marker: where each lies in the text.
type MarkerPair = (Range<usize>, Range<usize>);

/// Collect the marker pairs under `node` that whitespace keeps from being
/// emphasis: two runs of `*` of one length, left as text by the parser under
/// one parent and on one line, with whitespace inside exactly one of them.
/// Whitespace inside both is more likely arithmetic, as in `2 * 3 * 4`.
fn loose_pairs(node: &mdast::Node, text: &str, pairs: &mut Vec<MarkerPair>) {
    let mut markers = Vec::new();
    for child in node.children().into_iter().flatten() {
        match child {
            mdast::Node::Text(child) => {
                if let Some(position) = &child.position {
                    let source = position.start.offset..position.end.offset;
                    literal_asterisks(text, source, &mut markers);
                }
            }
            _ => loose_pairs(child, text, pairs),
        }
    }
    let mut markers = markers.as_slice();
    while let [open, rest @ ..] = markers {
        markers = rest;
        let close = rest
            .iter()
            .take_while(|close| !text[open.end..close.start].contains('\n'))
            .position(|close| close.len() == open.len());
        let Some(close) = close else {
            continue;
        };
        let inside = &text[open.end..rest[close].start];
        if inside.starts_with(char::is_whitespace) != inside.ends_with(char::is_whitespace) {
            pairs.push((open.clone(), rest[close].clone()));
            markers = &rest[close + 1..];
        }
    }
}

/// Collect the runs of `*` in `source`, the source of a text node.
fn literal_asterisks(text: &str, source: Range<usize>, markers: &mut Vec<Range<usize>>) {
    let bytes = text.as_bytes();
    let mut ix = source.start;
    while ix < source.end {
        match bytes[ix] {
            // An escaped asterisk is text, not a marker.
            b'\\' => ix += 2,
            b'*' => {
                let start = ix;
                while ix < source.end && bytes[ix] == b'*' {
                    ix += 1;
                }
                // The rest of a longer run, part of which is emphasis
                // already, is no marker of its own.
                if !text[..start].ends_with('*') && !text[ix..].starts_with('*') {
                    markers.push(start..ix);
                }
            }
            _ => ix += 1,
        }
    }
}

/// `text` with the whitespace inside each of `pairs` moved outside it, and
/// where each pair then lies, markers included.
fn tighten(text: &str, pairs: &[MarkerPair]) -> (String, Vec<Range<usize>>) {
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(pairs.len());
    let mut copied = 0;
    for (open, close) in pairs {
        let inside = &text[open.end..close.start];
        out.push_str(&text[copied..open.start]);
        if out.ends_with(|c: char| !sheds_space(c)) {
            out.push_str(&inside[..inside.len() - inside.trim_start().len()]);
        }
        let start = out.len();
        out.push_str(&text[open.clone()]);
        out.push_str(inside.trim());
        out.push_str(&text[close.clone()]);
        spans.push(start..out.len());
        if text[close.end..].starts_with(|c: char| !sheds_space(c)) {
            out.push_str(&inside[inside.trim_end().len()..]);
        }
        copied = close.end;
    }
    out.push_str(&text[copied..]);
    (out, spans)
}

/// Whether whitespace moved next to `c` is dropped instead: `c` is
/// whitespace already, or a marker that the whitespace would loosen in turn.
fn sheds_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '*' | '_' | '~')
}

/// Collect where `node`'s emphasis lies in the source, markers included.
fn emphasis_spans(node: &mdast::Node, spans: &mut Vec<Range<usize>>) {
    if let mdast::Node::Emphasis(_) | mdast::Node::Strong(_) = node
        && let Some(position) = node.position()
    {
        spans.push(position.start.offset..position.end.offset);
    }
    for child in node.children().into_iter().flatten() {
        emphasis_spans(child, spans);
    }
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
            let url = trim_url_end(&candidate[..end]);
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

/// Drop trailing punctuation, which usually ends the sentence rather than the
/// URL. A closing parenthesis stays when it balances one inside the URL, as in
/// `https://en.wikipedia.org/wiki/Rust_(programming_language)`.
fn trim_url_end(url: &str) -> &str {
    let mut url = url;
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let trimmed = match trimmed.strip_suffix(')') {
            Some(inner) if inner.matches('(').count() <= inner.matches(')').count() => inner,
            _ => trimmed,
        };
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
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
    fn finds_names_that_may_be_custom_emoji() {
        assert_eq!(
            custom_emoji_names(":happy_cat: :rocket: `:in_code:` :pepe-privet:"),
            ["happy_cat", "pepe-privet"]
        );
        assert_eq!(
            custom_emoji_names("```\n:in_fence:\n```"),
            Vec::<&str>::new()
        );
        assert_eq!(custom_emoji_names("no colons"), Vec::<&str>::new());
    }

    #[test]
    fn emoji_fields_may_hold_shortcodes() {
        assert_eq!(emoji_name(":pepe-privet:"), "pepe-privet");
        assert_eq!(emoji_name("calendar"), "calendar");
        assert_eq!(emoji_name("::"), "::");
        assert!(is_emoji_name("party_parrot+1-2"));
        assert!(!is_emoji_name("../image"));
        assert!(!is_emoji_name(""));
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
    fn shortcodes_may_escape_their_underscores() {
        let custom = CustomEmojis::new([(
            "party_parrot".to_string(),
            "https://chat.example.com/api/v4/emoji/e1/image".to_string(),
        )]);
        assert_eq!(
            emojify(r":party\_parrot: :white\_check\_mark:", &custom),
            "![:party_parrot:](https://chat.example.com/api/v4/emoji/e1/image) ✅"
        );
        for text in [
            r":not\_an\_emoji:",
            r"`:party\_parrot:`",
            r":party\parrot: :party\\_parrot:",
        ] {
            assert_eq!(emojify(text, &custom), text);
        }
        assert_eq!(
            custom_emoji_names(r":party\_parrot: :white\_check\_mark: :a\b:"),
            ["party_parrot"]
        );
    }

    #[test]
    fn emphasis_may_hold_whitespace_inside_its_markers() {
        assert_eq!(tighten_emphasis("**bold **"), "**bold**");
        assert_eq!(tighten_emphasis("**Note: **the rest"), "**Note:** the rest");
        assert_eq!(tighten_emphasis("so** bold** it is"), "so **bold** it is");
        assert_eq!(tighten_emphasis("a ** bold** b"), "a **bold** b");
        // A space moved next to the closing underscore would undo the italics.
        assert_eq!(tighten_emphasis("_a **b **_"), "_a **b**_");
        assert_eq!(
            tighten_emphasis("*italic * and ***both ***"),
            "*italic* and ***both***"
        );
        assert_eq!(
            tighten_emphasis(
                "**a `b` [c](https://x.y) *d* ![e](f) **\n> - **quoted ** x\n\n| **cell ** |\n| - |"
            ),
            "**a `b` [c](https://x.y) *d* ![e](f)**\n> - **quoted** x\n\n| **cell** |\n| - |"
        );
    }

    #[test]
    fn emphasis_is_tightened_only_where_it_then_reads_as_emphasis() {
        for text in [
            "2 * 3 * 4 and x ** 2 + y ** 2",
            "**fine** and *fine* and ***fine***",
            "`**code **` and\n```\n**code **\n```",
            r"\*\*escaped \*\*",
            "**over\nlines **",
            "* item *\n* **item**",
            // The second marker already opens emphasis.
            "**a **b**",
            // Tightening the single asterisks would undo the bold.
            "*note: **2 * 3**",
            // The first asterisk belongs to a marker that is half emphasis.
            "**a* *b",
            "| **a | b ** |\n| - | - |",
            // No emphasis opens between a letter and punctuation.
            "foo**«bar» **",
        ] {
            assert_eq!(tighten_emphasis(text), text);
        }
    }

    #[test]
    fn tightened_emphasis_keeps_its_emoji_and_mentions() {
        let custom = CustomEmojis::new([(
            "party-parrot".to_string(),
            "https://chat.example.com/api/v4/emoji/e1/image".to_string(),
        )]);
        let message = PreparedMessage::new("**:party-parrot: Привет, @ann! **", &custom);
        assert_eq!(
            message.text().as_ref(),
            "**![:party-parrot:](https://chat.example.com/api/v4/emoji/e1/image) \
             Привет, [@ann](mention:ann)!**"
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
        assert_eq!(
            autolink("see https://en.wikipedia.org/wiki/Rust_(language)."),
            "see <https://en.wikipedia.org/wiki/Rust_(language)>."
        );
        assert_eq!(
            autolink("(see https://example.com/a)."),
            "(see <https://example.com/a>)."
        );
    }

    #[test]
    fn mentions_become_links_to_the_person() {
        assert_eq!(link_mentions("hi @ann!"), "hi [@ann](mention:ann)!");
        assert_eq!(
            link_mentions("(cc @Bob.Smith, @c-d_e.)"),
            "(cc [@Bob.Smith](mention:bob.smith), [@c-d_e](mention:c-d_e).)"
        );
        assert_eq!(link_mentions("**@ann**"), "**[@ann](mention:ann)**");
        assert_eq!(
            link_mentions("> @ann\n- @bob `@x`\n\n| a |\n| - |\n| @cy |"),
            "> [@ann](mention:ann)\n- [@bob](mention:bob) `@x`\n\n| a |\n| - |\n| [@cy](mention:cy) |"
        );
        assert_eq!(mention_username("mention:ann"), Some("ann"));
        assert_eq!(mention_username("https://example.com"), None);
    }

    #[test]
    fn mentions_skip_addresses_code_links_and_groups() {
        for text in [
            "mail bob@example.com",
            "`@ann` and\n```\n@ann\n```",
            "https://medium.com/@ann and https://x.y/?to=@ann",
            "[@ann](https://example.com) [x](mailto:@ann) [cc @ann](https://x.y)",
            "@channel @Here @all: lunch",
            "@1password \\@ann @ @_x",
            "``@ann`` and `` a ` @ann ``",
            "~~~\n@ann\n~~~\n\n````\n```\n@ann\n````",
            "    @ann\n\npara\n\n    @ann",
            "$@ann$\n\n<div>\n@ann\n</div>",
        ] {
            assert_eq!(link_mentions(text), text);
        }
    }

    #[test]
    fn mentions_make_a_message_rich_except_in_system_messages() {
        let none = CustomEmojis::default();
        assert!(PreparedMessage::new("hi @ann", &none).is_rich());
        let system = PreparedMessage::system("@ann joined the channel.", &none);
        assert_eq!(system.text().as_ref(), "@ann joined the channel.");
        assert!(!system.is_rich());
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
