//! One post in a timeline.
//!
//! A flat, team-chat layout: avatar and name start a group; consecutive posts
//! by the same author within a few minutes continue it without repeating
//! either. Day changes and the unread boundary are drawn as part of the
//! first row after them, so rows map one-to-one onto timeline entries and
//! the virtual list never has to translate indices.

use std::{rc::Rc, sync::Arc};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, GlobalState, Icon, Selectable as _, Sizable as _, StyledExt as _,
    attachment::{
        Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentTitle,
    },
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    clipboard::Clipboard,
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    popover::{Popover, PopoverState},
    separator::Separator,
    text::{TextView, TextViewState},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::{
    Anchor, AnyElement, App, ClickEvent, ElementId, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, RenderOnce, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use mattermost::{Client, FileInfo, Status, User, UserId};
use session::{CustomEmojis, Delivery, Entry};

use crate::{composer::format_size, extensions, time};

/// The largest an inline image is drawn, in logical pixels. Images keep
/// their aspect ratio; this bounds raster content, not interface layout.
const IMAGE_MAX_WIDTH: f32 = 360.;
const IMAGE_MAX_HEIGHT: f32 = 280.;

/// A press that moves further than this before release selected text
/// rather than clicking the post.
const CLICK_SLOP: f64 = 4.;

/// Reactions offered without opening a picker.
pub const QUICK_REACTIONS: [(&str, &str); 4] = [
    ("+1", "👍"),
    ("white_check_mark", "✅"),
    ("joy", "😂"),
    ("eyes", "👀"),
];

/// What the user asked to do with a post.
#[derive(Clone, Debug)]
pub enum PostAction {
    OpenThread,
    ToggleReaction(String),
    CopyText,
    Edit,
    Delete,
    Retry,
    Discard,
    /// Download an attached file and open it.
    OpenFile(FileInfo),
    /// Open the direct message with the post's author.
    MessageAuthor,
}

pub type PostActionHandler = Rc<dyn Fn(PostAction, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct PostRow {
    entry: Entry,
    author: Option<Arc<User>>,
    avatar_url: Option<SharedString>,
    author_status: Status,
    me: UserId,
    continuation: bool,
    day_label: Option<SharedString>,
    unread_divider: bool,
    rich_text: Option<Entity<TextViewState>>,
    thread_entry: bool,
    client: Option<Client>,
    custom_emoji: CustomEmojis,
    on_action: PostActionHandler,
}

impl PostRow {
    pub fn new(entry: Entry, me: UserId, on_action: PostActionHandler) -> Self {
        Self {
            entry,
            author: None,
            avatar_url: None,
            author_status: Status::default(),
            me,
            continuation: false,
            day_label: None,
            unread_divider: false,
            rich_text: None,
            thread_entry: false,
            client: None,
            custom_emoji: CustomEmojis::default(),
            on_action,
        }
    }

    pub fn author(mut self, author: Option<Arc<User>>, avatar_url: Option<SharedString>) -> Self {
        self.author = author;
        self.avatar_url = avatar_url;
        self
    }

    /// The author's presence, shown in their profile.
    pub fn author_status(mut self, status: Status) -> Self {
        self.author_status = status;
        self
    }

    /// Continue the previous author's group: no avatar or name.
    pub fn continuation(mut self, continuation: bool) -> Self {
        self.continuation = continuation;
        self
    }

    /// Draw a date divider above the post.
    pub fn day_label(mut self, label: Option<SharedString>) -> Self {
        self.day_label = label;
        self
    }

    /// Draw the "New" divider above the post.
    pub fn unread_divider(mut self, unread: bool) -> Self {
        self.unread_divider = unread;
        self
    }

    /// Parsed Markdown for a rich message, owned by the timeline view.
    pub fn rich_text(mut self, state: Option<Entity<TextViewState>>) -> Self {
        self.rich_text = state;
        self
    }

    /// The signed-in client, for attachment previews.
    pub fn client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// The server's custom emoji, for reactions and the author's status.
    pub fn custom_emoji(mut self, custom_emoji: CustomEmojis) -> Self {
        self.custom_emoji = custom_emoji;
        self
    }

    /// Lead into the post's thread: show its reply count, and open it when
    /// the post is clicked (channel timelines only).
    pub fn thread_entry(mut self, thread_entry: bool) -> Self {
        self.thread_entry = thread_entry;
        self
    }

    fn author_name(&self) -> SharedString {
        if let Some(name) = self.entry.post().override_username() {
            return name.to_string().into();
        }
        match &self.author {
            Some(user) => user.display_name().into(),
            None => "…".into(),
        }
    }

    fn render_body(&self, cx: &App) -> AnyElement {
        if let Some(body) = extensions::render_post_body(&self.entry, cx) {
            return body;
        }
        let message = self.entry.message();
        match &self.rich_text {
            Some(state) if message.is_rich() => TextView::new(state)
                .w_full()
                // Language and copy sit in the block's top-right corner,
                // over the code rather than on a line of their own.
                .code_block_actions(|block, _, cx| {
                    let code = block.code();
                    h_flex()
                        .gap_1()
                        .items_center()
                        .when_some(block.lang().filter(|l| !l.is_empty()), |this, lang| {
                            // The language as its logo; the name stays on hover.
                            this.child(
                                div()
                                    .id("code-language")
                                    .px_1()
                                    .child(
                                        crate::lang_icon::icon(&lang)
                                            .small()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(lang.clone()).build(window, cx)
                                    }),
                            )
                        })
                        .child(
                            Clipboard::new("copy-code")
                                .value(code)
                                .accessibility_label("Copy code")
                                .tooltip("Copy code"),
                        )
                })
                .into_any_element(),
            _ => div()
                .w_full()
                .child(SharedString::from(message.text().clone()))
                .into_any_element(),
        }
    }

    fn render_files(&self) -> Option<AnyElement> {
        let files = &self.entry.post().metadata.files;
        if files.is_empty() {
            return None;
        }
        let client = self.client.clone()?;
        Some(
            h_flex()
                .flex_wrap()
                .items_start()
                .gap_2()
                .children(files.iter().map(|file| {
                    let on_action = self.on_action.clone();
                    let open = file.clone();
                    let id = SharedString::from(format!("file-{}", file.id));
                    let open_file = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        on_action(PostAction::OpenFile(open.clone()), window, cx)
                    };
                    if file.is_image() && file.width > 0 && file.height > 0 {
                        // Size from metadata so the row's height is known
                        // before the image loads; virtual scrolling stays put.
                        let scale = (IMAGE_MAX_WIDTH / file.width as f32)
                            .min(IMAGE_MAX_HEIGHT / file.height as f32)
                            .min(1.);
                        let (w, h) = (file.width as f32 * scale, file.height as f32 * scale);
                        div()
                            .id(id)
                            .rounded_lg()
                            .overflow_hidden()
                            .child(img(client.file_image_url(file)).w(px(w)).h(px(h)))
                            .tooltip({
                                let name = SharedString::from(file.name.clone());
                                move |window, cx| Tooltip::new(name.clone()).build(window, cx)
                            })
                            .on_click(open_file)
                            .into_any_element()
                    } else {
                        let kind = if file.extension.is_empty() {
                            "File".to_string()
                        } else {
                            file.extension.to_uppercase()
                        };
                        Attachment::new()
                            .id(id)
                            .media(AttachmentMedia::new().child(Icon::new(IconName::File)))
                            .content(
                                AttachmentContent::new()
                                    .title(AttachmentTitle::new(file.name.clone()))
                                    .description(AttachmentDescription::new(format!(
                                        "{kind} · {}",
                                        format_size(file.size.max(0) as u64)
                                    ))),
                            )
                            .tooltip(format!("Open {}", file.name))
                            .on_click(open_file)
                            .into_any_element()
                    }
                }))
                .into_any_element(),
        )
    }

    /// The files `render_files` draws, which the actions menu can open.
    fn shown_files(&self) -> &[FileInfo] {
        if self.client.is_some() {
            &self.entry.post().metadata.files
        } else {
            &[]
        }
    }

    /// Where the link preview `render_link_preview` draws leads, if it
    /// draws one.
    fn link_preview_url(&self) -> Option<SharedString> {
        let metadata = &self.entry.post().metadata;
        let embed = metadata
            .embeds
            .iter()
            .find(|e| e.kind == "opengraph" || e.kind == "image")?;
        if embed.kind == "image" {
            let size = metadata.images.get(&embed.url)?;
            (size.width > 0 && size.height > 0).then(|| embed.url.clone().into())
        } else {
            embed.link_preview().map(|preview| preview.url.into())
        }
    }

    /// The first link's preview: an OpenGraph card, or the image itself
    /// for a direct image link.
    fn render_link_preview(&self, cx: &App) -> Option<AnyElement> {
        let metadata = &self.entry.post().metadata;
        let embed = metadata
            .embeds
            .iter()
            .find(|e| e.kind == "opengraph" || e.kind == "image")?;
        let theme = cx.theme();
        if embed.kind == "image" {
            let size = metadata.images.get(&embed.url)?;
            if size.width <= 0 || size.height <= 0 {
                return None;
            }
            let scale = (IMAGE_MAX_WIDTH / size.width as f32)
                .min(IMAGE_MAX_HEIGHT / size.height as f32)
                .min(1.);
            let url = embed.url.clone();
            return Some(
                div()
                    .id("link-image")
                    .rounded_lg()
                    .overflow_hidden()
                    .cursor_pointer()
                    .child(
                        img(embed.url.clone())
                            .w(px(size.width as f32 * scale))
                            .h(px(size.height as f32 * scale)),
                    )
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.open_url(&url)
                    })
                    .into_any_element(),
            );
        }
        let preview = embed.link_preview()?;
        let url = preview.url.clone();
        let site = if preview.site_name.is_empty() {
            url::Url::parse(&preview.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default()
        } else {
            preview.site_name.clone()
        };
        // Wide images (site banners) read best full-width under the text;
        // small ones as a thumbnail beside it.
        let banner = preview.image.as_ref().and_then(|image| {
            let size = metadata.images.get(image)?;
            (size.width >= 400 && size.height > 0)
                .then(|| (image.clone(), size.width as f32 / size.height as f32))
        });
        let thumbnail = preview.image.clone().filter(|_| banner.is_none());
        let text = v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .when(!site.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(site),
                )
            })
            .when(!preview.title.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.link)
                        .line_clamp(2)
                        .child(preview.title.clone()),
                )
            })
            .when(!preview.description.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .line_clamp(3)
                        .child(preview.description.clone()),
                )
            });
        Some(
            v_flex()
                .id("link-preview")
                .max_w(gpui_kit::rems(32.))
                .gap_3()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary.opacity(0.5))
                .cursor_pointer()
                .hover(|style| style.bg(theme.secondary))
                .child(h_flex().items_start().gap_3().child(text).when_some(
                    thumbnail,
                    |this, image| {
                        this.child(
                            img(image)
                                .flex_none()
                                .size_16()
                                .rounded(theme.radius)
                                .object_fit(gpui_kit::ObjectFit::Cover),
                        )
                    },
                ))
                // The banner spans the card's content width at its own
                // aspect ratio, so it lines up with the text above it.
                .when_some(banner, |this, (image, ratio)| {
                    // The wrapper owns the size; an image's intrinsic size
                    // would otherwise win over the aspect ratio.
                    this.child(
                        div()
                            .relative()
                            .w_full()
                            .aspect_ratio(ratio)
                            .rounded(theme.radius)
                            .overflow_hidden()
                            .child(
                                img(image)
                                    .absolute()
                                    .inset_0()
                                    .size_full()
                                    .object_fit(gpui_kit::ObjectFit::Cover),
                            ),
                    )
                })
                .tooltip(move |window, cx| Tooltip::new(preview.url.clone()).build(window, cx))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url)
                })
                .into_any_element(),
        )
    }

    fn render_reactions(&self) -> Option<AnyElement> {
        let reactions = &self.entry.post().metadata.reactions;
        if reactions.is_empty() {
            return None;
        }
        // Group by emoji, keeping first-use order.
        let mut groups: Vec<(&str, usize, bool)> = Vec::new();
        for reaction in reactions {
            let mine = reaction.user_id == self.me;
            match groups
                .iter_mut()
                .find(|(name, ..)| *name == reaction.emoji_name)
            {
                Some(group) => {
                    group.1 += 1;
                    group.2 |= mine;
                }
                None => groups.push((&reaction.emoji_name, 1, mine)),
            }
        }
        Some(
            h_flex()
                .flex_wrap()
                .gap_1()
                .children(groups.into_iter().map(|(name, count, mine)| {
                    let custom = session::emoji_for(name)
                        .is_none()
                        .then(|| self.custom_emoji.image_url(name).cloned())
                        .flatten();
                    let on_action = self.on_action.clone();
                    let name = name.to_string();
                    let button = Button::new(SharedString::from(format!("reaction-{name}")))
                        .xsmall()
                        .outline()
                        .selected(mine)
                        .toggled(mine);
                    match custom {
                        // The image stands in for the character; the
                        // shortcode names it for assistive technology.
                        Some(url) => button
                            .accessibility_label(format!(":{name}: {count}"))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(img(SharedString::from(url)).size_4())
                                    .child(count.to_string()),
                            ),
                        None => {
                            button.label(format!("{} {count}", crate::status::emoji_char(&name)))
                        }
                    }
                    .tooltip(format!(":{name}:"))
                    .on_click(move |_, window, cx| {
                        on_action(PostAction::ToggleReaction(name.clone()), window, cx)
                    })
                }))
                .into_any_element(),
        )
    }

    fn render_delivery(&self, cx: &App) -> Option<AnyElement> {
        if self.entry.delivery() != Delivery::Failed {
            return None;
        }
        let retry = self.on_action.clone();
        let discard = self.on_action.clone();
        Some(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child("Not sent")
                .child(
                    Button::new("retry")
                        .xsmall()
                        .ghost()
                        .label("Retry")
                        .on_click(move |_, window, cx| retry(PostAction::Retry, window, cx)),
                )
                .child(
                    Button::new("discard")
                        .xsmall()
                        .ghost()
                        .label("Discard")
                        .on_click(move |_, window, cx| discard(PostAction::Discard, window, cx)),
                )
                .into_any_element(),
        )
    }

    fn render_thread_summary(&self) -> Option<AnyElement> {
        let count = self.entry.post().reply_count;
        if !self.thread_entry || count == 0 {
            return None;
        }
        let on_action = self.on_action.clone();
        let label = if count == 1 {
            "1 reply".to_string()
        } else {
            format!("{count} replies")
        };
        Some(
            Button::new("thread")
                .xsmall()
                .ghost()
                .flex_none()
                .icon(IconName::MessageSquare)
                .label(label)
                .on_click(move |_, window, cx| on_action(PostAction::OpenThread, window, cx))
                .into_any_element(),
        )
    }

    /// Reactions. A continuation has no header line for the reply count,
    /// so it follows the reactions here instead.
    fn render_footer(&self) -> Option<AnyElement> {
        let reactions = self.render_reactions();
        let thread = self
            .continuation
            .then(|| self.render_thread_summary())
            .flatten();
        if reactions.is_none() && thread.is_none() {
            return None;
        }
        Some(
            h_flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .children(reactions)
                .children(thread)
                .into_any_element(),
        )
    }

    /// The author's avatar or name, which opens their profile when the post
    /// is by a known person. Webhook posts posing under another name don't.
    fn author_trigger(&self, id: &'static str, content: AnyElement, avatar: bool) -> AnyElement {
        let Some(user) = self
            .author
            .clone()
            .filter(|_| self.entry.post().override_username().is_none())
        else {
            return content;
        };
        let name = user.display_name();
        let trigger = Button::new(id)
            .accessibility_label(format!("Show profile of {name}"))
            .child(content);
        let trigger = if avatar {
            trigger.ghost().p_0().size_9().rounded_full()
        } else {
            trigger.text().h_auto()
        };
        let profile = Profile {
            is_me: user.id == self.me,
            user,
            avatar_url: self.avatar_url.clone(),
            status: self.author_status,
            custom_emoji: self.custom_emoji.clone(),
        };
        let on_action = self.on_action.clone();
        Popover::new(id)
            .anchor(Anchor::TopLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                let popover = cx.entity();
                render_profile(&profile, popover, &on_action, cx)
            })
            .into_any_element()
    }

    /// The row's action strip. The quick buttons are pointer shortcuts; the
    /// "More actions" menu is the row's one Tab stop and offers every
    /// command, so the strip stays reachable without a pointer.
    ///
    /// At rest the strip is transparent rather than invisible: GPUI skips an
    /// invisible element's paint, which would drop the trigger from the Tab
    /// order. It shows on hover, while the trigger has focus, and while its
    /// menu is open.
    fn render_hover_actions(
        &self,
        group: SharedString,
        actions: PostActions,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let react = self.on_action.clone();
        let reply = self.on_action.clone();
        let on_action = self.on_action.clone();
        let PostActions {
            entries,
            focus,
            open,
            revealed,
        } = actions;
        h_flex()
            .absolute()
            .top_0()
            .right_4()
            .gap_0p5()
            .p_0p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .when(!revealed, |this| {
                this.opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
            })
            .child(
                Button::new("quick-react")
                    .xsmall()
                    .ghost()
                    .icon(IconName::ThumbsUp)
                    .accessibility_label("React with thumbs up")
                    .tooltip("React with :+1:")
                    .tab_stop(false)
                    .on_click(move |_, window, cx| {
                        react(PostAction::ToggleReaction("+1".into()), window, cx)
                    }),
            )
            .when(!self.entry.post().is_system(), |this| {
                this.child(
                    Button::new("reply")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Reply)
                        .accessibility_label("Reply in thread")
                        .tooltip("Reply in thread")
                        .tab_stop(false)
                        .on_click(move |_, window, cx| reply(PostAction::OpenThread, window, cx)),
                )
            })
            .child(
                Button::new("more-actions")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Ellipsis)
                    .accessibility_label(format!("Actions for message from {}", self.author_name()))
                    .tooltip("More actions")
                    .track_focus(&focus)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        post_menu(menu, &entries, &on_action)
                    })
                    .on_open_change(move |is_open, _, cx| {
                        open.update(cx, |open, cx| {
                            *open = *is_open;
                            cx.notify();
                        })
                    }),
            )
            .into_any_element()
    }
}

/// What the action strip needs from the row's retained state.
struct PostActions {
    entries: Rc<[PostMenuEntry]>,
    focus: FocusHandle,
    /// Whether the "More actions" menu is open.
    open: Entity<bool>,
    /// Show the strip without hover: its trigger has focus or its menu is open.
    revealed: bool,
}

impl RenderOnce for PostRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let post = self.entry.post().clone();
        // Retained per post, so focus and the open menu survive re-renders.
        let actions_focus = window
            .use_keyed_state(
                SharedString::from(format!("post-actions-focus-{}", post.id)),
                cx,
                |_, cx| cx.focus_handle(),
            )
            .read(cx)
            .clone();
        let actions_open = window.use_keyed_state(
            SharedString::from(format!("post-actions-open-{}", post.id)),
            cx,
            |_, _| false,
        );
        let actions_revealed = *actions_open.read(cx) || actions_focus.is_focused(window);
        // Whether the current press began on a control inside the row, which
        // handles it. Controls claim their presses by suppressing text
        // selection; `prevent_default` alone can't tell, because the message
        // text is focusable and focusing it on press prevents default too.
        let pressed_control = window.use_keyed_state(
            SharedString::from(format!("post-pressed-control-{}", post.id)),
            cx,
            |_, _| false,
        );
        let theme = cx.theme();
        let id: ElementId = post.id.as_arc().clone().into();
        let group: SharedString = format!("post-{}", post.id).into();
        let is_mine = post.user_id == self.me;
        let is_system = post.is_system();
        let sent = self.entry.delivery() == Delivery::Sent;
        let edited = post.edit_at > 0;
        let has_text = !post.message.trim().is_empty() || post.metadata.files.is_empty();
        let time_label = time::clock(post.create_at);
        let full_time: SharedString = time::full(post.create_at).into();

        let leading: AnyElement = if self.continuation {
            // Keep the time reachable without repeating the header.
            div()
                .id("time")
                .w_9()
                .flex_none()
                .pt_0p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .invisible()
                .group_hover(group.clone(), |style| style.visible())
                .child(time_label.clone())
                .tooltip({
                    let full_time = full_time.clone();
                    move |window, cx| Tooltip::new(full_time.clone()).build(window, cx)
                })
                .into_any_element()
        } else {
            let name = self.author_name();
            let mut avatar = Avatar::new().name(name).small().size_9();
            if let Some(url) = self.avatar_url.clone() {
                avatar = avatar.src(url.to_string());
            }
            div()
                .w_9()
                .flex_none()
                .child(self.author_trigger("author-avatar", avatar.into_any_element(), true))
                .into_any_element()
        };

        let header = (!self.continuation).then(|| {
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    self.author_trigger(
                        "author-name",
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(self.author_name())
                            .into_any_element(),
                        false,
                    ),
                )
                .when_some(
                    self.author
                        .as_ref()
                        .and_then(|u| u.custom_status(&time::now_rfc3339())),
                    |this, status| {
                        let tooltip: SharedString = if status.text.is_empty() {
                            format!(":{}:", status.emoji).into()
                        } else {
                            status.text.clone().into()
                        };
                        this.child(
                            div()
                                .id("custom-status")
                                .text_sm()
                                .child(crate::status::emoji_element(
                                    &status.emoji,
                                    &self.custom_emoji,
                                ))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tooltip.clone()).build(window, cx)
                                }),
                        )
                    },
                )
                .when(self.author.as_ref().is_some_and(|u| u.is_bot), |this| {
                    this.child(
                        div()
                            .px_1()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("BOT"),
                    )
                })
                .child(
                    div()
                        .id("time")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(time_label)
                        .tooltip(move |window, cx| {
                            Tooltip::new(full_time.clone()).build(window, cx)
                        }),
                )
                // The reply count rides on the header rather than taking a
                // line of its own under the message.
                .children(self.render_thread_summary())
        });

        let on_action = self.on_action.clone();
        let menu_entries: Rc<[PostMenuEntry]> = post_menu_entries(
            is_mine,
            is_system,
            self.shown_files(),
            self.link_preview_url(),
        )
        .into();
        let row = h_flex()
            .id(id)
            .group(group.clone())
            .relative()
            .w_full()
            .items_start()
            .gap_3()
            .px_4()
            .when(self.continuation, |this| this.py_0p5())
            .when(!self.continuation, |this| this.pt_3().pb_1())
            .hover(|style| style.bg(theme.accent.opacity(0.5)))
            // Mark the post the focused actions trigger acts on.
            .when(actions_revealed, |this| this.bg(theme.accent.opacity(0.5)))
            // Clicking anywhere else on a post opens its thread. Selecting
            // text and pressing the row's own controls do not.
            .when(self.thread_entry && sent && !is_system, |this| {
                let on_action = self.on_action.clone();
                let pressed = pressed_control.clone();
                // Start each press unclaimed, before any control below sees it.
                this.capture_any_mouse_down(|_, _, cx| {
                    GlobalState::init(cx);
                    GlobalState::reset_text_selection_suppression(cx);
                })
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    let on_control = GlobalState::is_text_selection_suppressed(cx);
                    pressed.update(cx, |pressed, _| *pressed = on_control);
                })
                .on_click(move |event, window, cx| {
                    if *pressed_control.read(cx) {
                        return;
                    }
                    if let ClickEvent::Mouse(click) = event
                        && (click.down.click_count > 1
                            || (click.up.position - click.down.position).magnitude() > CLICK_SLOP)
                    {
                        return;
                    }
                    on_action(PostAction::OpenThread, window, cx);
                })
            })
            .child(leading)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .children(header)
                    .when(has_text || edited, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .when(!sent, |this| this.opacity(0.6))
                                .child(self.render_body(cx))
                                .when(edited, |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("(edited)"),
                                    )
                                }),
                        )
                    })
                    .children(self.render_files())
                    .children(self.render_link_preview(cx))
                    .children(self.render_delivery(cx))
                    .children(self.render_footer()),
            )
            .when(sent, |this| {
                this.child(self.render_hover_actions(
                    group,
                    PostActions {
                        entries: menu_entries.clone(),
                        focus: actions_focus,
                        open: actions_open,
                        revealed: actions_revealed,
                    },
                    cx,
                ))
            });
        let row: AnyElement = if sent {
            row.context_menu(move |menu, _, _| post_menu(menu, &menu_entries, &on_action))
                .into_any_element()
        } else {
            row.into_any_element()
        };

        // A new author's group starts below a faint rule, unless a day or
        // unread divider already marks the break.
        let group_rule = !self.continuation && self.day_label.is_none() && !self.unread_divider;
        v_flex()
            .w_full()
            .when(group_rule, |this| {
                this.child(div().mx_4().mt_1().h_px().bg(theme.border.opacity(0.7)))
            })
            .when_some(self.day_label, |this, label| {
                this.child(
                    div()
                        .px_4()
                        .pt_3()
                        .pb_1()
                        .child(Separator::horizontal().label(label)),
                )
            })
            .when(self.unread_divider, |this| {
                this.child(
                    div().px_4().py_1().child(
                        Separator::horizontal()
                            .label("New messages")
                            // Emphasis, not an error: `primary` is the theme's emphasis role.
                            .color(theme.primary),
                    ),
                )
            })
            .child(row)
    }
}

/// What an author's profile card shows.
struct Profile {
    user: Arc<User>,
    avatar_url: Option<SharedString>,
    status: Status,
    custom_emoji: CustomEmojis,
    is_me: bool,
}

fn render_profile(
    profile: &Profile,
    popover: Entity<PopoverState>,
    on_action: &PostActionHandler,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let user = &profile.user;
    let name = user.display_name();
    let deactivated = user.delete_at > 0;
    let custom = user.custom_status(&time::now_rfc3339());
    let mut avatar = Avatar::new().name(name.clone()).size_12();
    if let Some(url) = profile.avatar_url.clone() {
        avatar = avatar.src(url.to_string());
    }
    let muted_line = |text: String| {
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .truncate()
            .child(text)
    };
    let on_action = on_action.clone();
    v_flex()
        .w_72()
        .gap_3()
        .text_sm()
        .child(
            h_flex()
                .gap_3()
                .child(div().flex_none().child(avatar))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().min_w_0().font_semibold().truncate().child(name))
                                .when(user.is_bot, |this| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .px_1()
                                            .rounded(theme.radius)
                                            .bg(theme.muted)
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("BOT"),
                                    )
                                }),
                        )
                        .child(muted_line(format!("@{}", user.username)))
                        .when(!user.position.trim().is_empty(), |this| {
                            this.child(muted_line(user.position.trim().to_string()))
                        }),
                ),
        )
        .map(|this| {
            if deactivated {
                this.child(muted_line("Deactivated".into()))
            } else if user.is_bot {
                this
            } else {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .size_2()
                                .rounded_full()
                                .bg(crate::status::presence_color(profile.status, cx)),
                        )
                        .child(crate::status::presence_label(profile.status)),
                )
            }
        })
        .when_some(custom, |this, status| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(crate::status::emoji_element(
                        &status.emoji,
                        &profile.custom_emoji,
                    ))
                    .child(div().min_w_0().truncate().child(status.text)),
            )
        })
        .when(!user.email.is_empty(), |this| {
            this.child(muted_line(user.email.clone()))
        })
        .when(!profile.is_me && !deactivated, |this| {
            this.child(
                Button::new("profile-message")
                    .small()
                    .outline()
                    .w_full()
                    .icon(IconName::MessageSquare)
                    .label("Message")
                    .on_click(move |_, window, cx| {
                        popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                        on_action(PostAction::MessageAuthor, window, cx);
                    }),
            )
        })
        .into_any_element()
}

/// A command a post's menus offer.
#[derive(Clone, Debug)]
enum PostCommand {
    Post(PostAction),
    /// Open the link preview's target in the browser.
    OpenLink(SharedString),
}

#[derive(Clone, Debug)]
enum PostMenuEntry {
    Command(SharedString, PostCommand),
    Separator,
}

/// The commands for a post, shared by the "More actions" menu and the
/// context menu so the two entry points cannot disagree. Files and the link
/// preview are listed because their cards only respond to the pointer.
fn post_menu_entries(
    is_mine: bool,
    is_system: bool,
    files: &[FileInfo],
    link: Option<SharedString>,
) -> Vec<PostMenuEntry> {
    let item = |label: String, action: PostAction| {
        PostMenuEntry::Command(label.into(), PostCommand::Post(action))
    };
    let mut entries = Vec::new();
    if !is_system {
        entries.push(item("Reply".into(), PostAction::OpenThread));
    }
    for (name, emoji) in QUICK_REACTIONS {
        entries.push(item(
            format!("React {emoji}"),
            PostAction::ToggleReaction(name.to_string()),
        ));
    }
    entries.push(PostMenuEntry::Separator);
    entries.push(item("Copy text".into(), PostAction::CopyText));
    if !files.is_empty() || link.is_some() {
        entries.push(PostMenuEntry::Separator);
        entries.extend(files.iter().map(|file| {
            item(
                format!("Open {}", file.name),
                PostAction::OpenFile(file.clone()),
            )
        }));
        entries.extend(
            link.map(|url| PostMenuEntry::Command("Open link".into(), PostCommand::OpenLink(url))),
        );
    }
    if is_mine && !is_system {
        entries.push(PostMenuEntry::Separator);
        entries.push(item("Edit".into(), PostAction::Edit));
        entries.push(item("Delete…".into(), PostAction::Delete));
    }
    entries
}

fn post_menu(
    menu: PopupMenu,
    entries: &[PostMenuEntry],
    on_action: &PostActionHandler,
) -> PopupMenu {
    entries.iter().fold(menu, |menu, entry| match entry {
        PostMenuEntry::Separator => menu.separator(),
        PostMenuEntry::Command(label, command) => {
            let on_action = on_action.clone();
            let command = command.clone();
            menu.item(PopupMenuItem::new(label.clone()).on_click(
                move |_, window, cx| match &command {
                    PostCommand::Post(action) => on_action(action.clone(), window, cx),
                    PostCommand::OpenLink(url) => cx.open_url(url),
                },
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Bounds, Context, InputEvent as _, MouseDownEvent, MouseMoveEvent,
        MouseUpEvent, Pixels, Render, TestAppContext, WindowBounds, WindowOptions, point, size,
    };
    use mattermost::{Post, Reaction};
    use session::{Directory, Timeline, TimelineKind};

    use super::*;

    /// A thread root with a reply and a reaction, rendered as the channel
    /// timeline renders it.
    struct OneRow {
        entry: Entry,
        rich_text: Entity<TextViewState>,
        actions: Rc<RefCell<Vec<PostAction>>>,
    }

    impl Render for OneRow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let actions = self.actions.clone();
            div().w(px(600.)).child(
                PostRow::new(
                    self.entry.clone(),
                    "me".into(),
                    Rc::new(move |action, _, _| actions.borrow_mut().push(action)),
                )
                .rich_text(Some(self.rich_text.clone()))
                .thread_entry(true),
            )
        }
    }

    fn open_row(
        cx: &mut TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, Rc<RefCell<Vec<PostAction>>>) {
        cx.update(gpui_kit::init);
        let client = Client::new_unverified("http://localhost:1", "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let directory = cx.new(|_| Directory::new(client.clone(), me));
        let timeline = cx.new(|_| {
            Timeline::new(
                TimelineKind::Channel("c".into()),
                client,
                directory,
                CustomEmojis::default(),
            )
        });
        let mut post = Post::default();
        post.id = "root".into();
        post.channel_id = "c".into();
        post.user_id = "them".into();
        post.create_at = 1;
        post.message = "ku, **1x1**?".into();
        post.reply_count = 1;
        let mut reaction = Reaction::default();
        reaction.user_id = "them".into();
        reaction.post_id = "root".into();
        reaction.emoji_name = "+1".into();
        post.metadata.reactions.push(reaction);
        timeline.update(cx, |t, cx| t.upsert(post, cx));
        let entry = timeline.read_with(cx, |t, _| t.entries()[0].clone());
        assert!(entry.message().is_rich());
        let actions = Rc::new(RefCell::new(Vec::new()));
        let window = cx.update(|cx| {
            let text = entry.message().text().clone();
            let actions = actions.clone();
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(800.), px(400.)),
                    })),
                    ..Default::default()
                },
                cx,
                move |_, cx| {
                    let rich_text =
                        cx.new(|cx| TextViewState::markdown(&text, cx).selectable(true));
                    cx.new(|_| OneRow {
                        entry,
                        rich_text,
                        actions,
                    })
                },
            )
            .unwrap()
            .0
        });
        cx.run_until_parked();
        (window, actions)
    }

    fn opened_thread(actions: &RefCell<Vec<PostAction>>) -> usize {
        actions
            .borrow()
            .iter()
            .filter(|action| matches!(action, PostAction::OpenThread))
            .count()
    }

    /// A plain left click at a window position, through native dispatch.
    fn click_point(window: &mut Window, position: gpui_kit::Point<Pixels>, cx: &mut App) {
        window.dispatch_event(
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseDownEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    }

    #[gpui_kit::test]
    fn clicking_the_message_text_opens_its_thread(cx: &mut TestAppContext) {
        let (window, actions) = open_row(cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // The message sits between the header and the reactions.
            let header = window.find("thread").bounds();
            let reactions = window.find("reaction-+1").bounds();
            let y = (header.bottom() + reactions.top()) / 2.;
            click_point(window, point(reactions.left() + px(4.), y), cx);
        })
        .unwrap();
        assert_eq!(opened_thread(&actions), 1);
    }

    #[gpui_kit::test]
    fn the_reply_count_shares_the_header_and_opens_the_thread_once(cx: &mut TestAppContext) {
        let (window, actions) = open_row(cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let thread = window.find("thread").bounds();
            let reactions = window.find("reaction-+1").bounds();
            // Above the message on the author's line, not under it.
            assert!(thread.bottom() < reactions.top());
            assert!(thread.left() > reactions.left());
            window.click("thread", cx);
        })
        .unwrap();
        assert_eq!(opened_thread(&actions), 1);
    }

    #[gpui_kit::test]
    fn pressing_a_reaction_does_not_open_the_thread(cx: &mut TestAppContext) {
        let (window, actions) = open_row(cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("reaction-+1", cx);
        })
        .unwrap();
        assert_eq!(opened_thread(&actions), 0);
        assert!(matches!(
            actions.borrow().as_slice(),
            [PostAction::ToggleReaction(name)] if name == "+1"
        ));
    }

    fn labels(entries: &[PostMenuEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| match entry {
                PostMenuEntry::Command(label, _) => label.as_ref(),
                PostMenuEntry::Separator => "-",
            })
            .collect()
    }

    #[test]
    fn menu_reaches_files_and_link_preview() {
        let mut file = FileInfo::default();
        file.name = "report.pdf".into();
        let entries = post_menu_entries(false, false, &[file], Some("https://example.com".into()));
        let labels = labels(&entries);
        assert!(labels.contains(&"Open report.pdf"));
        assert!(labels.contains(&"Open link"));
        assert!(!labels.contains(&"Edit"));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            PostMenuEntry::Command(_, PostCommand::Post(PostAction::OpenFile(f))) if f.name == "report.pdf"
        )));
    }

    #[test]
    fn menu_without_attachments_has_no_open_section() {
        let entries = post_menu_entries(true, false, &[], None);
        let labels = labels(&entries);
        assert!(!labels.iter().any(|label| label.starts_with("Open")));
        assert_eq!(labels.last(), Some(&"Delete…"));
        assert_eq!(labels.first(), Some(&"Reply"));
    }
}
