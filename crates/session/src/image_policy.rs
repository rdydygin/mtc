//! One image admission policy for a post's Markdown and integration image fields.
use crate::CustomEmojis;
use markdown::mdast::Node;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub(crate) const MAX_POST_IMAGES: usize = 8;
const MAX_POST_EMOJI: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageBudget {
    accepted: Arc<HashSet<String>>,
    limit: usize,
    emoji: Arc<HashSet<String>>,
    emoji_limit: usize,
}
impl Default for ImageBudget {
    fn default() -> Self {
        Self {
            accepted: Arc::default(),
            limit: MAX_POST_IMAGES,
            emoji: Arc::default(),
            emoji_limit: MAX_POST_EMOJI,
        }
    }
}
impl ImageBudget {
    pub(crate) fn admit(&mut self, url: &str) -> bool {
        if !is_web_image(url) {
            return false;
        }
        if self.accepted.contains(url) {
            return true;
        }
        if self.accepted.len() >= self.limit {
            return false;
        }
        Arc::make_mut(&mut self.accepted).insert(url.to_string());
        true
    }
    fn admit_markdown(&mut self, alt: &str, url: &str, custom: &CustomEmojis) -> bool {
        if !is_web_image(url) {
            return false;
        }
        if !custom.is_trusted_image(alt, url) {
            return self.admit(url);
        }
        if self.emoji.contains(url) {
            return true;
        }
        if self.emoji.len() >= self.emoji_limit {
            return false;
        }
        Arc::make_mut(&mut self.emoji).insert(url.to_string());
        true
    }
    pub(crate) fn allows(&self, url: &str) -> bool {
        is_web_image(url) && (self.accepted.contains(url) || self.emoji.contains(url))
    }
    /// Preview text cannot introduce a new image or spend another budget.
    pub(crate) fn restricted(&self) -> Self {
        Self {
            accepted: self.accepted.clone(),
            limit: self.accepted.len(),
            emoji: self.emoji.clone(),
            emoji_limit: self.emoji.len(),
        }
    }
}
pub(crate) fn is_web_image(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
            && !rest.is_empty()
            && !url.chars().any(char::is_control)
    })
}

/// Rewrite denied images to literal alt text before TextView sees the source.
/// The app installs no math plugin, so GPUI reparses unclaimed inline math
/// as prose. Inspect GFM prose here too: dollar signs cannot hide images.
/// The view also enforces the admitted URL snapshot when resolving images,
/// independently of any later renderer parsing. Raw HTML nodes
/// become literal text too: TextView otherwise converts HTML image tags outside
/// this Markdown image admission path. Reference images are resolved here too, so definition ordering cannot change
/// which URL the renderer loads after admission.
pub(crate) fn filter_images(
    text: String,
    budget: &mut ImageBudget,
    custom: &CustomEmojis,
) -> String {
    if !text.contains("![") && !text.contains('<') {
        return text;
    }
    let options = markdown::ParseOptions::gfm();
    let Ok(tree) = markdown::to_mdast(&text, &options) else {
        // No unparsed source may bypass admission on a later renderer parse.
        return literal(&text);
    };
    let mut definitions = HashMap::new();
    visit(&tree, &mut |node| {
        if let Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.as_str())
                .or_insert(definition.url.as_str());
        }
    });
    let mut replacements = Vec::new();
    visit(&tree, &mut |node| {
        if let Node::Html(html) = node {
            if let Some(position) = node.position() {
                replacements.push((
                    position.start.offset..position.end.offset,
                    literal(&html.value),
                ));
            }
            return;
        }
        let (alt, url, reference) = match node {
            Node::Image(image) => (image.alt.as_str(), image.url.as_str(), false),
            Node::ImageReference(image) => (
                image.alt.as_str(),
                definitions
                    .get(image.identifier.as_str())
                    .copied()
                    .unwrap_or_default(),
                true,
            ),
            _ => return,
        };
        // Admit the exact destination emitted below, not a pre-encoding alias:
        // direct and reference images must not turn one slot into two URLs.
        let destination = reference.then(|| {
            url.replace('\\', "%5C")
                .replace('<', "%3C")
                .replace('>', "%3E")
        });
        let url = destination.as_deref().unwrap_or(url);
        let accepted = budget.admit_markdown(alt, url, custom);
        if (!accepted || reference)
            && let Some(position) = node.position()
        {
            let replacement = if accepted {
                format!("![{}](<{url}>)", literal(alt))
            } else {
                literal(alt)
            };
            replacements.push((position.start.offset..position.end.offset, replacement));
        }
    });
    let mut result = text;
    for (range, replacement) in replacements.into_iter().rev() {
        result.replace_range(range, &replacement);
    }
    result
}
fn visit<'a>(node: &'a Node, f: &mut impl FnMut(&'a Node)) {
    f(node);
    for child in node.children().into_iter().flatten() {
        visit(child, f);
    }
}
fn literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    fn filter_images(text: String, budget: &mut ImageBudget) -> String {
        super::filter_images(text, budget, &CustomEmojis::default())
    }
    #[test]
    fn trusted_emoji_have_a_separate_cap_and_cannot_exempt_other_sources() {
        let client =
            mattermost::Client::new_unverified("https://chat.example.com", "token").unwrap();
        let mut custom = CustomEmojis::for_server(&client);
        let mut budget = ImageBudget::default();
        for i in 0..=MAX_POST_EMOJI {
            let name = format!("team_emoji_{i}");
            let url = client.custom_emoji_url(&format!("id{i}").into());
            custom.insert(name.clone(), url.clone());
            assert_eq!(
                budget.admit_markdown(&format!(":{name}:"), &url, &custom),
                i < MAX_POST_EMOJI
            );
        }
        assert!(budget.accepted.is_empty());
        // Explicit integration image/icon URLs still spend ordinary image slots.
        assert!(budget.admit(custom.image_url("team_emoji_0").unwrap()));
        assert_eq!(budget.accepted.len(), 1);
        for url in [
            "https://evil.example/api/v4/emoji/id/image",
            "https://chat.example.com/api/v4/files/id/image",
            "https://chat.example.com/api/v4/emoji/id/image?redirect=evil",
        ] {
            custom.insert("spoof".into(), url.into());
            assert!(!custom.is_trusted_image(":spoof:", url));
        }
        assert!(!custom.is_trusted_image(
            ":unknown:",
            "https://chat.example.com/api/v4/emoji/id/image"
        ));
        assert!(
            !custom.is_trusted_image("ordinary image", custom.image_url("team_emoji_0").unwrap())
        );
    }

    #[test]
    fn schemes_references_entities_and_html_are_filtered_but_code_is_literal() {
        let text = "![inline](data:image/png;base64,AAAA) ![local](file:///tmp/a) ![relative](/a) ![entity](data&colon;image/png;base64,AAAA)\n\n![ref][a]\n\n[a]: data:image/png;base64,AAAA\n\n<div>\n![nested](data:image/png;base64,AAAA)\n</div>\n\n`![code](data:image/png;base64,AAAA)`\n\n![web][b]\n\n[b]: https://example.com/image.png";
        let filtered = filter_images(text.into(), &mut ImageBudget::default());
        for alt in ["inline", "local", "relative", "entity", "ref", "nested"] {
            assert!(!filtered.contains(&format!("![{alt}]")), "{filtered}");
        }
        assert!(filtered.contains("`![code](data:image/png;base64,AAAA)`"));
        assert!(filtered.contains("![web](<https://example.com/image.png>)"));
        let html = filter_images(
            "<img src=\"data:image/png;base64,AAAA\">".into(),
            &mut ImageBudget::default(),
        );
        assert!(html.starts_with(r"\<img"));
        assert!(!html.contains("<img src=\"data:"));
    }
    #[test]
    fn distinct_urls_share_a_budget_and_alt_text_cannot_inject_markdown() {
        let mut budget = ImageBudget::default();
        let text = (0..100)
            .map(|i| format!("![image {i}](https://example.com/{i})\n"))
            .collect();
        let filtered = filter_images(text, &mut budget);
        assert_eq!(filtered.matches("![").count(), MAX_POST_IMAGES);
        assert!(filtered.contains("image 99"));
        assert!(budget.admit("https://example.com/0"));
        assert!(!budget.admit("https://example.com/new"));
        let nested = filter_images(
            "![\\!\\[nested\\]\\(https://example.com/x\\)](data:image/png;base64,AAAA)".into(),
            &mut ImageBudget::default(),
        );
        assert!(nested.starts_with(r"\!\[nested\]"));
    }
}
