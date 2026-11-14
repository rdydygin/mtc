//! Bounded integration content. Parse and prepare at ingest, never in a row renderer.
use crate::{
    CustomEmojis, PreparedMessage,
    image_policy::{ImageBudget, is_web_image},
};
use mattermost::Post;
use serde_json::Value;
use std::sync::Arc;

const MAX_ATTACHMENTS: usize = 10;
const MAX_FIELDS: usize = 20;
const MAX_TEXT: usize = 8192;
const MAX_TOTAL: usize = 32768;
const MAX_LABEL: usize = 512;
const MAX_URL: usize = 2048;
const COLLAPSE_AT: usize = 700;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentColor {
    Default,
    Good,
    Warning,
    Danger,
    Hex(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentText {
    full: PreparedMessage,
    preview: Option<PreparedMessage>,
}
impl AttachmentText {
    fn new(source: &str, custom: &CustomEmojis, images: &mut ImageBudget) -> Self {
        let full = PreparedMessage::with_images(source, custom, images);
        let preview = full.truncated(COLLAPSE_AT, images, custom);
        Self { full, preview }
    }
    pub fn full(&self) -> &PreparedMessage {
        &self.full
    }
    pub fn preview(&self) -> Option<&PreparedMessage> {
        self.preview.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentField {
    title: Arc<str>,
    value: AttachmentText,
    short: bool,
    label: Arc<str>,
}
impl AttachmentField {
    pub fn title(&self) -> &Arc<str> {
        &self.title
    }
    pub fn value(&self) -> &AttachmentText {
        &self.value
    }
    pub fn is_short(&self) -> bool {
        self.short
    }
    pub fn label(&self) -> &Arc<str> {
        &self.label
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageAttachment {
    color: AttachmentColor,
    pretext: AttachmentText,
    author_name: Arc<str>,
    author_link: Arc<str>,
    author_icon: Arc<str>,
    title: Arc<str>,
    title_link: Arc<str>,
    text: AttachmentText,
    fields: Vec<AttachmentField>,
    image_url: Arc<str>,
    image_links: Vec<Arc<str>>,
    thumb_url: Arc<str>,
    footer: Arc<str>,
    footer_icon: Arc<str>,
    actions: bool,
    summary: Arc<str>,
    emoji_source: Arc<str>,
}
impl MessageAttachment {
    pub fn parse(post: &Post, custom: &CustomEmojis) -> Arc<[Self]> {
        let mut images = ImageBudget::default();
        PreparedMessage::with_images(&post.message, custom, &mut images);
        Self::parse_with_images(post, custom, &mut images)
    }
    pub(crate) fn parse_with_images(
        post: &Post,
        custom: &CustomEmojis,
        images: &mut ImageBudget,
    ) -> Arc<[Self]> {
        let mut budget = MAX_TOTAL;
        attachment_values(post)
            .filter_map(|value| {
                value.as_object()?;
                let mut read = |key: &str, cap: usize| {
                    let source = value.get(key).and_then(Value::as_str).unwrap_or_default();
                    let text = bounded(source, cap.min(budget)).to_string();
                    budget -= text.len();
                    text
                };
                let mut image_links = Vec::new();
                let fallback = read("fallback", MAX_TEXT);
                let pretext = read("pretext", MAX_TEXT);
                let title = read("title", MAX_LABEL);
                let text = read("text", MAX_TEXT);
                let author_name = read("author_name", MAX_LABEL);
                let author_link = read("author_link", MAX_URL);
                let author_icon =
                    image_url(&read("author_icon", MAX_URL), images, &mut image_links);
                let title_link = read("title_link", MAX_URL);
                let image = image_url(&read("image_url", MAX_URL), images, &mut image_links);
                let thumb = image_url(&read("thumb_url", MAX_URL), images, &mut image_links);
                let footer = read("footer", MAX_LABEL);
                let footer_icon =
                    image_url(&read("footer_icon", MAX_URL), images, &mut image_links);
                let mut emoji_source = format!("{pretext}\n{text}\n{fallback}");
                let fields: Vec<AttachmentField> = value
                    .get("fields")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(MAX_FIELDS)
                    .filter_map(|field| {
                        field.as_object()?;
                        let title = bounded(
                            field
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            MAX_LABEL.min(budget),
                        )
                        .to_string();
                        budget -= title.len();
                        let value = bounded(
                            field
                                .get("value")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            MAX_TEXT.min(budget),
                        )
                        .to_string();
                        budget -= value.len();
                        emoji_source.push('\n');
                        emoji_source.push_str(&value);
                        Some(AttachmentField {
                            label: format!("{title}: {value}").into(),
                            title: title.into(),
                            value: AttachmentText::new(&value, custom, images),
                            short: field.get("short").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect();
                let mut summary = if !fallback.trim().is_empty() {
                    fallback.clone()
                } else {
                    [&pretext, &title, &text]
                        .into_iter()
                        .filter(|s| !s.trim().is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                if summary.is_empty() {
                    summary = fields
                        .iter()
                        .filter(|f| !f.title().is_empty() || !f.value().full().text().is_empty())
                        .map(|f| f.label().as_ref())
                        .collect::<Vec<_>>()
                        .join("\n");
                }
                // Fallback-only webhooks must still be visible.
                let body = if pretext.is_empty() && title.is_empty() && text.is_empty() {
                    &fallback
                } else {
                    &text
                };
                Some(Self {
                    color: parse_color(
                        value
                            .get("color")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    pretext: AttachmentText::new(&pretext, custom, images),
                    author_name: author_name.into(),
                    author_link: author_link.into(),
                    author_icon,
                    title: title.into(),
                    title_link: title_link.into(),
                    text: AttachmentText::new(body, custom, images),
                    fields,
                    image_url: image,
                    image_links,
                    thumb_url: thumb,
                    footer: footer.into(),
                    footer_icon,
                    actions: value
                        .get("actions")
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty()),
                    summary: summary.into(),
                    emoji_source: emoji_source.into(),
                })
            })
            .collect()
    }
    pub fn color(&self) -> &AttachmentColor {
        &self.color
    }
    pub fn pretext(&self) -> &AttachmentText {
        &self.pretext
    }
    pub fn text(&self) -> &AttachmentText {
        &self.text
    }
    pub fn fields(&self) -> &[AttachmentField] {
        &self.fields
    }
    pub fn has_actions(&self) -> bool {
        self.actions
    }
    pub fn emoji_source(&self) -> &str {
        &self.emoji_source
    }
    pub fn author_name(&self) -> &Arc<str> {
        &self.author_name
    }
    pub fn author_link(&self) -> &Arc<str> {
        &self.author_link
    }
    pub fn author_icon(&self) -> &Arc<str> {
        &self.author_icon
    }
    pub fn title(&self) -> &Arc<str> {
        &self.title
    }
    pub fn title_link(&self) -> &Arc<str> {
        &self.title_link
    }
    pub fn image_links(&self) -> &[Arc<str>] {
        &self.image_links
    }
    pub fn image_url(&self) -> &Arc<str> {
        &self.image_url
    }
    pub fn thumb_url(&self) -> &Arc<str> {
        &self.thumb_url
    }
    pub fn footer(&self) -> &Arc<str> {
        &self.footer
    }
    pub fn footer_icon(&self) -> &Arc<str> {
        &self.footer_icon
    }
    pub fn summary(&self) -> &Arc<str> {
        &self.summary
    }
}
fn bounded(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(s.len().min(max))]
}
fn attachment_values(post: &Post) -> impl Iterator<Item = &Value> {
    post.props
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ATTACHMENTS)
}
fn image_url(s: &str, images: &mut ImageBudget, links: &mut Vec<Arc<str>>) -> Arc<str> {
    if images.admit(s) {
        s.into()
    } else {
        if is_web_image(s) && !links.iter().any(|url| url.as_ref() == s) {
            links.push(s.into());
        }
        "".into()
    }
}
fn parse_color(s: &str) -> AttachmentColor {
    match s {
        "good" => AttachmentColor::Good,
        "warning" => AttachmentColor::Warning,
        "danger" => AttachmentColor::Danger,
        _ => {
            let hex = s.strip_prefix('#').unwrap_or(s);
            if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                AttachmentColor::Hex(u32::from_str_radix(hex, 16).unwrap())
            } else if hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                let expanded: String = hex.chars().flat_map(|c| [c, c]).collect();
                AttachmentColor::Hex(u32::from_str_radix(&expanded, 16).unwrap())
            } else {
                AttachmentColor::Default
            }
        }
    }
}
/// Text for copy, search, notifications, and thread labels. No Markdown preparation needed.
pub fn post_text(post: &Post) -> String {
    if !post.message.trim().is_empty() {
        return post.message.clone();
    }
    let mut remaining = MAX_TOTAL;
    let mut pieces = Vec::new();
    for value in attachment_values(post) {
        let before = pieces.len();
        let fallback = value
            .get("fallback")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let keys: &[&str] = if fallback.trim().is_empty() {
            &["pretext", "title", "text"]
        } else {
            &["fallback"]
        };
        for key in keys {
            let text = bounded(
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim(),
                MAX_TEXT.min(remaining),
            );
            remaining -= text.len();
            if !text.is_empty() {
                pieces.push(text.to_string());
            }
        }
        if pieces.len() == before {
            for field in value
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_FIELDS)
            {
                let title = bounded(
                    field
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim(),
                    MAX_LABEL.min(remaining),
                );
                remaining -= title.len();
                let value = bounded(
                    field
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim(),
                    MAX_TEXT.min(remaining),
                );
                remaining -= value.len();
                if !title.is_empty() || !value.is_empty() {
                    pieces.push(if title.is_empty() {
                        value.to_string()
                    } else if value.is_empty() {
                        title.to_string()
                    } else {
                        format!("{title}: {value}")
                    });
                }
            }
        }
    }
    bounded(&pieces.join("\n"), MAX_TOTAL).to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn every_attachment_markdown_surface_rejects_non_web_images() {
        let mut post = Post::default();
        post.props.insert("attachments".into(),json!([{"pretext":"![pre](data:image/png;base64,AAAA)","text":"![body](file:///tmp/a)","fields":[{"value":"![field][ref]\n\n[ref]: data:image/png;base64,AAAA"}]}]));
        let parsed = MessageAttachment::parse(&post, &CustomEmojis::default());
        assert_eq!(parsed[0].pretext().full().text().as_ref(), "pre");
        assert_eq!(parsed[0].text().full().text().as_ref(), "body");
        assert!(!parsed[0].fields()[0].value().full().text().contains("!["));
    }

    #[test]
    fn fields_only_summaries_are_bounded_and_ignore_odd_values() {
        let mut post = Post::default();
        post.props.insert("attachments".into(),json!([{"fields":[null,{"title":"Build","value":"Passed"},{"title":false,"value":"Ready"},{"title":"Owner","value":42}]}]));
        assert_eq!(post_text(&post), "Build: Passed\nReady\nOwner");
        assert!(
            MessageAttachment::parse(&post, &CustomEmojis::default())[0]
                .summary()
                .contains("Build: Passed")
        );
        post.props.insert("attachments".into(),json!([{"fields":vec![json!({"title":"🦀".repeat(900),"value":"界".repeat(9000)});100]}]));
        let text = post_text(&post);
        assert!(text.len() <= MAX_TOTAL);
        assert!(text.lines().count() <= MAX_FIELDS);
    }

    #[test]
    fn attachment_markdown_and_explicit_images_share_the_body_budget() {
        let mut post = Post::default();
        post.message = (0..6)
            .map(|i| format!("![body](https://example.com/{i})\n"))
            .collect();
        post.props.insert("attachments".into(),json!([
            {"image_url":"https://example.com/main", "thumb_url":"https://example.com/thumb", "pretext":"![pre](https://example.com/pre)", "text":"![data](data:image/png;base64,AAAA) ![text](https://example.com/text)", "fields":[{"value":"![field](https://example.com/field)"}]},
            {"image_url":"https://example.com/excess"}
        ]));
        let parsed = MessageAttachment::parse(&post, &CustomEmojis::default());
        assert_eq!(parsed[0].image_url().as_ref(), "https://example.com/main");
        assert_eq!(parsed[0].thumb_url().as_ref(), "https://example.com/thumb");
        assert_eq!(parsed[0].pretext().full().text().as_ref(), "pre");
        assert_eq!(parsed[0].text().full().text().as_ref(), "data text");
        assert_eq!(
            parsed[0].fields()[0].value().full().text().as_ref(),
            "field"
        );
        assert!(parsed[1].image_url().is_empty());
        assert_eq!(
            parsed[1].image_links()[0].as_ref(),
            "https://example.com/excess"
        );
    }

    #[test]
    fn collapsed_text_never_gets_a_second_image_budget() {
        let mut post = Post::default();
        post.props.insert("attachments".into(),json!([{"text":(0..100).map(|i| format!("![image {i}](https://example.com/{i})\n")).collect::<String>()}]));
        let parsed = MessageAttachment::parse(&post, &CustomEmojis::default());
        let text = parsed[0].text();
        assert_eq!(
            text.full().text().matches("![").count(),
            crate::image_policy::MAX_POST_IMAGES
        );
        assert!(
            text.preview().unwrap().text().matches("![").count()
                <= crate::image_policy::MAX_POST_IMAGES
        );
        assert!(
            !text
                .preview()
                .unwrap()
                .text()
                .contains("https://example.com/8")
        );
    }

    #[test]
    fn tolerant_bounded_and_prepared() {
        let mut post = Post::default();
        post.props.insert("attachments".into(), json!([null, 4, {"title":false,"author_icon":"file:///secret", "text":"@ann :rocket: **shipped**", "color":"#abc", "fields":[null, {"title":"Build", "value":42,"short":"yes"}], "actions":[{}]}]));
        let parsed = MessageAttachment::parse(&post, &CustomEmojis::default());
        assert_eq!(parsed.len(), 1);
        assert!(
            parsed[0]
                .text()
                .full()
                .text()
                .contains("[@ann](mention:ann) 🚀")
        );
        assert!(parsed[0].text().full().is_rich());
        assert_eq!(parsed[0].color(), &AttachmentColor::Hex(0xaabbcc));
        assert!(parsed[0].author_icon().is_empty());
        assert!(!parsed[0].fields()[0].is_short());
        assert!(parsed[0].has_actions());
        let huge = "🦀".repeat(MAX_TEXT);
        post.props.insert(
            "attachments".into(),
            json!(vec![
                json!({"text":huge,"fields":vec![json!({"value":huge});40]});
                30
            ]),
        );
        let parsed = MessageAttachment::parse(&post, &CustomEmojis::default());
        assert_eq!(parsed.len(), MAX_ATTACHMENTS);
        assert_eq!(parsed[0].text().full().text().len(), MAX_TEXT);
        assert!(parsed[0].text().preview().is_some());
        assert_eq!(parsed[0].fields().len(), MAX_FIELDS);
        assert!(post_text(&post).len() <= MAX_TOTAL + MAX_ATTACHMENTS);
    }
    #[test]
    fn empty_messages_have_shared_summary() {
        let mut post = Post::default();
        post.props.insert("attachments".into(), json!([{"fallback":"Build failed","text":"details"},{"pretext":"Deploy", "title":"Production","text":"Ready"}]));
        assert_eq!(post_text(&post), "Build failed\nDeploy\nProduction\nReady");
        post.message = "Message wins".into();
        assert_eq!(post_text(&post), post.message);
    }
}
