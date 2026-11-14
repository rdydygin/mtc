//! Retained text documents and disclosure state for integration cards.
use super::*;
use gpui_kit::base::{Link, TestSupportExt as _};
use gpui_kit::{AppContext as _, Hsla, ObjectFit, Role, rems};
use session::{AttachmentColor, AttachmentText, MessageAttachment};

pub(crate) struct AttachmentCards {
    source: Arc<[MessageAttachment]>,
    cards: Vec<Card>,
}
struct Card {
    id: SharedString,
    pretext: Text,
    body: Text,
    fields: Vec<Text>,
}
struct Text {
    id: SharedString,
    source: AttachmentText,
    full: Option<Entity<TextViewState>>,
    preview: Option<Entity<TextViewState>>,
    expanded: Entity<bool>,
}
impl Text {
    fn new(source: &AttachmentText, cx: &mut App) -> Self {
        let document = |message: &session::PreparedMessage, cx: &mut App| {
            message
                .is_rich()
                .then(|| cx.new(|cx| TextViewState::markdown(message.text(), cx).selectable(true)))
        };
        Self {
            id: SharedString::default(),
            source: source.clone(),
            full: document(source.full(), cx),
            preview: source.preview().and_then(|p| document(p, cx)),
            expanded: cx.new(|_| false),
        }
    }
    fn render(
        &self,
        id: &'static str,
        row: &PostRow,
        mention: Option<&Entity<MentionCard>>,
        action: &Entity<PostActionHandler>,
        cx: &App,
    ) -> AnyElement {
        let expanded = *self.expanded.read(cx);
        let collapsed = !expanded && self.source.preview().is_some();
        let message = if collapsed {
            self.source.preview().unwrap()
        } else {
            self.source.full()
        };
        let state = if collapsed { &self.preview } else { &self.full };
        let body = row.render_prepared(message, state.as_ref(), mention, action, cx);
        v_flex()
            .id(id)
            .w_full()
            .min_w_0()
            .gap_1()
            .child(body)
            .when(self.source.preview().is_some(), |this| {
                let state = self.expanded.clone();
                this.child(
                    Button::new("attachment-disclosure")
                        .small()
                        .ghost()
                        .label(if expanded { "Show less" } else { "Show more" })
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            state.update(cx, |expanded, cx| {
                                *expanded = !*expanded;
                                cx.notify();
                            });
                        }),
                )
            })
            .into_any_element()
    }
}
impl AttachmentCards {
    pub(crate) fn new(source: Arc<[MessageAttachment]>, cx: &mut App) -> Self {
        let mut identities = HashMap::new();
        let cards = source
            .iter()
            .map(|a| {
                let mut parts = vec![
                    a.pretext().full().text().as_ref(),
                    a.text().full().text().as_ref(),
                    a.author_name(),
                    a.title(),
                    a.title_link(),
                    a.image_url(),
                    a.footer(),
                ];
                parts.extend(a.fields().iter().map(|f| f.label().as_ref()));
                let id = identity(&parts, &mut identities);
                let mut field_ids = HashMap::new();
                let fields = a
                    .fields()
                    .iter()
                    .map(|f| {
                        let mut text = Text::new(f.value(), cx);
                        text.id = identity(&[f.label()], &mut field_ids);
                        text
                    })
                    .collect();
                Card {
                    id,
                    pretext: Text::new(a.pretext(), cx),
                    body: Text::new(a.text(), cx),
                    fields,
                }
            })
            .collect();
        Self { source, cards }
    }
    pub(crate) fn matches(&self, source: &[MessageAttachment]) -> bool {
        self.source.as_ref() == source
    }
    pub(super) fn render(
        &self,
        row: &PostRow,
        mention: Option<&Entity<MentionCard>>,
        action: &Entity<PostActionHandler>,
        cx: &App,
    ) -> AnyElement {
        v_flex().gap_2().text_sm().children(self.source.iter().zip(&self.cards).map(|(a,card)| {
            let id = SharedString::from(format!("attachment-{}-{}",row.entry.id(),card.id));
            div().id(id).child(v_flex().id("attachment-card").test_support().role(Role::Group).aria_label(format!("Message attachment: {}",a.summary())).gap_1().w_full().min_w_0()
                .when(!a.pretext().full().text().is_empty(), |this| this.child(card.pretext.render("pretext",row,mention,action,cx)))
                .child(v_flex().pl_3().gap_1().border_l_2().border_color(color(a.color(),cx)).w_full().min_w_0()
                    .when(!a.author_name().is_empty(), |this| this.child(h_flex().gap_1().text_xs()
                        .children(icon(a.author_icon())).child(link("attachment-author",a.author_name(),a.author_link(),row,cx))))
                    .when(!a.title().is_empty(), |this| this.child(div().font_semibold().child(link("attachment-title",a.title(),a.title_link(),row,cx))))
                    .child(h_flex().items_start().gap_2().child(div().flex_1().min_w_0().child(card.body.render("text",row,mention,action,cx)))
                        .when(!a.thumb_url().is_empty(), |this| this.child(picture("attachment-thumb",a.thumb_url(),true,cx))))
                    .child(h_flex().items_start().flex_wrap().w_full().children(a.fields().iter().zip(&card.fields).map(|(field,text)| {
                        div().id(text.id.clone()).when(field.is_short(), |this| this.flex_basis(relative(0.5)).min_w(rems(12.)).flex_grow(1.)).when(!field.is_short(), |this| this.w_full()).max_w_full().child(div().id("attachment-field").test_support().role(Role::Group).aria_label(SharedString::from(field.label().clone()))
                            .max_w_full().py_1().pr_2()
                            .child(div().font_semibold().child(SharedString::from(field.title().clone())))
                            .child(text.render("value",row,mention,action,cx)))
                    })))
                    .when(!a.image_url().is_empty(), |this| this.child(picture("attachment-image",a.image_url(),false,cx)))
                    .children(a.image_links().iter().map(|url| div().id(SharedString::from(url.clone())).child(link("attachment-image-link", &Arc::from("Attachment image"), url, row, cx))))
                    .when(!a.footer().is_empty(), |this| this.child(h_flex().gap_1().text_xs().text_color(cx.theme().muted_foreground).children(icon(a.footer_icon())).child(SharedString::from(a.footer().clone()))))
                    .when(a.has_actions(), |this| {
                        let on_action = row.on_action.clone();
                        this.child(v_flex().gap_1().text_xs().text_color(cx.theme().muted_foreground)
                            .child("This message has actions. Open it in Mattermost to use them.")
                            .child(Button::new("attachment-open-browser").small().ghost().label("Open in browser").on_click(move |_,window,cx| {
                                cx.stop_propagation(); on_action(PostAction::OpenInBrowser,window,cx);
                            })))
                    })))
        })).into_any_element()
    }
}
fn icon(url: &Arc<str>) -> Option<AnyElement> {
    (!url.is_empty()).then(|| {
        img(SharedString::from(url.clone()))
            .size_4()
            .object_fit(ObjectFit::Contain)
            .into_any_element()
    })
}
fn link(id: &'static str, label: &Arc<str>, url: &Arc<str>, row: &PostRow, cx: &App) -> AnyElement {
    if url.is_empty() {
        return div()
            .child(SharedString::from(label.clone()))
            .into_any_element();
    }
    let action = row.on_action.clone();
    let theme = cx.theme();
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(label.clone()))
        .child(
            Link::new("attachment-link")
                .href(SharedString::from(url.clone()))
                .accessibility_label(SharedString::from(label.clone()))
                .text_color(theme.link)
                .underline()
                .cursor_pointer()
                .hover(|s| s.text_color(theme.link.opacity(0.8)))
                .border_1()
                .border_color(theme.border.opacity(0.))
                .focus_visible(|s| s.border_color(theme.ring))
                .open_with(move |url, _, window, cx| {
                    cx.stop_propagation();
                    action(PostAction::OpenLink(url.into()), window, cx);
                })
                .child(SharedString::from(label.clone())),
        )
        .into_any_element()
}
fn picture(id: &'static str, url: &Arc<str>, thumb: bool, cx: &App) -> AnyElement {
    let source = SharedString::from(url.clone());
    let open = source.clone();
    Button::new(id)
        .ghost()
        .accessibility_label("Open attachment image")
        .child(
            img(source)
                .w(if thumb { px(64.) } else { px(IMAGE_MAX_WIDTH) })
                .h(if thumb { px(64.) } else { px(IMAGE_MAX_HEIGHT) })
                .max_w_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback({
                    let color = cx.theme().muted_foreground;
                    move || {
                        div()
                            .text_color(color)
                            .child("Image unavailable")
                            .into_any_element()
                    }
                }),
        )
        .max_w_full()
        .h_auto()
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            crate::image_viewer::open_remote(open.clone(), window, cx);
        })
        .into_any_element()
}
fn color(value: &AttachmentColor, cx: &App) -> Hsla {
    let theme = cx.theme();
    match value {
        AttachmentColor::Default => theme.border,
        AttachmentColor::Good => theme.success,
        AttachmentColor::Warning => theme.warning,
        AttachmentColor::Danger => theme.danger,
        AttachmentColor::Hex(value) => clamp_color(*value, theme.background),
    }
}

// Integration color is audited data. Retain hue, limit saturation, and ensure
// a 3:1 boundary contrast even for yellow, white, black, and custom themes.
fn clamp_color(value: u32, background: Hsla) -> Hsla {
    let mut color: Hsla = gpui_kit::rgb(value).into();
    color.s = color.s.min(0.65);
    let background = luminance(background);
    let target = if background > 0.18 { 0. } else { 1. };
    for _ in 0..100 {
        let foreground = luminance(color);
        if (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05) >= 3. {
            break;
        }
        color.l = (color.l + (target - color.l).signum() * 0.01).clamp(0., 1.);
    }
    color
}
fn luminance(color: Hsla) -> f32 {
    let color = color.to_rgb();
    let linear = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}
// Integrations provide no IDs. Content-derived identity survives reordering;
// the occurrence suffix only separates indistinguishable duplicate payloads.
fn identity(parts: &[&str], seen: &mut HashMap<u64, usize>) -> SharedString {
    use std::hash::{Hash as _, Hasher as _};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hash);
    let hash = hash.finish();
    let occurrence = seen.entry(hash).or_default();
    let id = format!("{hash}-{occurrence}");
    *occurrence += 1;
    id.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn integration_colors_keep_boundary_contrast() {
        for background in [0xffffff, 0x000000, 0x808080, 0xf0eee0, 0x173022] {
            let background: Hsla = gpui_kit::rgb(background).into();
            for supplied in [0xffffff, 0x000000, 0xffff00, 0xff0000, 0x00ffff, 0x808080] {
                let color = clamp_color(supplied, background);
                let a = luminance(color);
                let b = luminance(background);
                assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 3.);
            }
        }
    }
}
