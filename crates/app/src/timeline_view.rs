//! A channel or thread: header, virtualized posts, typing line, composer.

use std::{collections::HashMap, rc::Rc, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    popover::Popover,
    spinner::Spinner,
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, ChannelType, FileInfo, Millis, Post, PostId};
use session::{Delivery, LoadState, Session, Timeline, TimelineEvent, TimelineKind};

use crate::{
    composer::{Composer, ComposerEvent, UploadTarget},
    download,
    extensions::{self, CommandContext, CommandOutcome},
    http::avatar_url,
    image_viewer,
    post_row::{PostAction, PostActionHandler, PostRow},
    post_scroller::{PostScroller, PostScrollerState},
    time,
};

/// Posts by one author closer together than this form one group.
const GROUP_WINDOW: Millis = 5 * 60 * 1000;
/// Start loading older posts when the top of the viewport is this close to
/// the oldest loaded post.
const PREFETCH_ROWS: usize = 8;
/// How many older pages to load looking for a post to reveal before
/// showing it in its thread instead.
const REVEAL_PAGES: usize = 20;
/// How long a revealed post stays marked.
const FLASH: Duration = Duration::from_millis(2500);

pub enum TimelineViewEvent {
    OpenThread(PostId),
    OpenChannel(ChannelId),
    Close,
}

pub struct TimelineView {
    kind: TimelineKind,
    session: Entity<Session>,
    timeline: Entity<Timeline>,
    scroller: Entity<PostScrollerState>,
    composer: Entity<Composer>,
    /// Parsed Markdown for live rich posts, kept so
    /// scrolling back never re-parses.
    rich_text: HashMap<PostId, Entity<TextViewState>>,
    /// The first post the user hasn't seen.
    unread_ix: Option<usize>,
    editing: Option<PostId>,
    /// A post to scroll to once it's loaded.
    revealing: Option<Reveal>,
    /// The post just revealed, marked for a moment so the eye finds it.
    flashed: Option<PostId>,
    _flash: Option<Task<()>>,
    /// A thread opened over search results offers to go back to them.
    back_to_results: bool,
    _subscriptions: Vec<Subscription>,
}

/// A post [`TimelineView::reveal`] is looking for.
struct Reveal {
    post: PostId,
    created: Millis,
    /// Older pages loaded so far while looking.
    pages: usize,
}

impl EventEmitter<TimelineViewEvent> for TimelineView {}

impl TimelineView {
    pub fn new(
        kind: TimelineKind,
        session: Entity<Session>,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        let count = timeline.read(cx).entries().len();
        let scroller = cx.new(|cx| PostScrollerState::new(count, cx));
        let placeholder = Self::placeholder(&kind, &session, cx);
        let target = UploadTarget {
            client: session.read(cx).client().clone(),
            channel: kind.channel().clone(),
        };
        let composer = cx.new(|cx| Composer::new(placeholder, draft, target, window, cx));
        let directory = session.read(cx).directory().clone();

        let subscriptions = vec![
            cx.subscribe(&timeline, Self::on_timeline_event),
            cx.observe(&timeline, |this, _, cx| {
                this.continue_reveal(cx);
                cx.notify();
            }),
            cx.observe(&directory, |_, _, cx| cx.notify()),
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.observe(&scroller, Self::on_scroll),
            cx.subscribe_in(&composer, window, Self::on_composer_event),
        ];

        let mut this = Self {
            kind,
            session,
            timeline,
            scroller,
            composer,
            rich_text: HashMap::new(),
            unread_ix: None,
            editing: None,
            revealing: None,
            flashed: None,
            _flash: None,
            back_to_results: false,
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    fn placeholder(kind: &TimelineKind, session: &Entity<Session>, cx: &App) -> SharedString {
        if kind.root().is_some() {
            return "Reply…".into();
        }
        let session = session.read(cx);
        match session.channel(kind.channel()) {
            Some(entry) if entry.kind().is_direct_or_group() => {
                format!("Message {}", session.channel_label(entry, cx)).into()
            }
            Some(entry) => format!("Message #{}", session.channel_label(entry, cx)).into(),
            None => "Message".into(),
        }
    }

    pub fn kind(&self) -> &TimelineKind {
        &self.kind
    }

    pub fn draft(&self, cx: &App) -> Option<String> {
        let text = self.composer.read(cx).text(cx);
        (!text.trim().is_empty() && !self.composer.read(cx).is_editing()).then_some(text)
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.focus_handle(cx).focus(window, cx);
    }

    /// Offer "Back to search results" in place of closing the thread.
    pub fn set_back_to_results(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.back_to_results != back {
            self.back_to_results = back;
            cx.notify();
        }
    }

    /// Scroll to `post`, created at `created`, and mark it for a moment.
    /// A channel loads older pages until it reaches the post; one too far
    /// back opens in its thread instead.
    pub fn reveal(&mut self, post: PostId, created: Millis, cx: &mut Context<Self>) {
        self.revealing = Some(Reveal {
            post,
            created,
            pages: 0,
        });
        self.continue_reveal(cx);
    }

    fn continue_reveal(&mut self, cx: &mut Context<Self>) {
        // A channel no longer on screen gives up, rather than load on and
        // then open the post's thread beside another conversation.
        if self.kind.root().is_none()
            && self.session.read(cx).active_channel() != Some(self.kind.channel())
        {
            self.revealing = None;
        }
        let Some(reveal) = self.revealing.as_mut() else {
            return;
        };
        let timeline = self.timeline.read(cx);
        if let Some(ix) = timeline.position(&reveal.post) {
            let post = reveal.post.clone();
            self.revealing = None;
            self.scroller.update(cx, |s, cx| {
                s.scroll_to_item(ix, cx);
            });
            self.flash(post, cx);
            return;
        }
        match timeline.state() {
            LoadState::Loaded => {}
            LoadState::NotLoaded | LoadState::Loading => return,
            LoadState::Failed(_) => {
                self.revealing = None;
                return;
            }
        }
        if timeline.is_loading_older() {
            return;
        }
        let passed = timeline
            .entries()
            .first()
            .is_none_or(|e| e.post().create_at <= reveal.created);
        if passed || !timeline.has_older() || reveal.pages >= REVEAL_PAGES {
            // Not among the channel's posts as far as they go: show it in
            // its thread, which loads it by itself.
            let post = reveal.post.clone();
            self.revealing = None;
            if self.kind.root().is_none() {
                cx.emit(TimelineViewEvent::OpenThread(post));
            }
            return;
        }
        reveal.pages += 1;
        self.timeline.update(cx, |t, cx| t.load_older(cx));
    }

    fn flash(&mut self, post: PostId, cx: &mut Context<Self>) {
        self.flashed = Some(post);
        self._flash = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH).await;
            let _ = this.update(cx, |this, cx| {
                this.flashed = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ----- Mirroring the timeline -------------------------------------------

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let entries = self.timeline.read(cx).entries().to_vec();
        let live: std::collections::HashSet<&PostId> = entries.iter().map(|e| e.id()).collect();
        self.rich_text.retain(|id, _| live.contains(id));
        for ix in 0..entries.len() {
            self.sync_rich_text(ix, cx);
        }
        self.unread_ix = self.find_unread(cx);
        let count = entries.len();
        self.scroller.update(cx, |s, cx| s.reset(count, cx));
    }

    fn on_timeline_event(
        &mut self,
        _: Entity<Timeline>,
        event: &TimelineEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TimelineEvent::Reset => self.rebuild(cx),
            TimelineEvent::Spliced { old, new_count } => {
                if !old.is_empty() {
                    let timeline = self.timeline.read(cx);
                    let live: std::collections::HashSet<&PostId> =
                        timeline.entries().iter().map(|e| e.id()).collect();
                    self.rich_text.retain(|id, _| live.contains(id));
                }
                for ix in old.start..old.start + new_count {
                    self.sync_rich_text(ix, cx);
                }
                let (old, new_count) = (old.clone(), *new_count);
                let count = self.timeline.read(cx).entries().len();
                self.scroller.update(cx, |s, cx| {
                    s.splice(old.clone(), new_count, cx);
                    // The next row may gain or lose its author header or
                    // day divider.
                    let next = old.start + new_count;
                    if next < count {
                        s.remeasure_items(next..next + 1, cx);
                    }
                });
                self.unread_ix = self.find_unread(cx);
                self.continue_reveal(cx);
            }
            TimelineEvent::Updated(ix) => {
                let ix = *ix;
                self.sync_rich_text(ix, cx);
                self.scroller.update(cx, |s, cx| {
                    s.remeasure_items(ix..ix + 1, cx);
                });
            }
        }
        cx.notify();
    }

    fn sync_rich_text(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.timeline.read(cx).entries().get(ix).cloned() else {
            return;
        };
        let message = entry.message();
        if !message.is_rich() {
            self.rich_text.remove(entry.id());
            return;
        }
        let text = message.text().clone();
        match self.rich_text.get(entry.id()) {
            Some(state) => state.update(cx, |state, cx| state.set_text(&text, cx)),
            None => {
                let state = cx.new(|cx| TextViewState::markdown(&text, cx).selectable(true));
                self.rich_text.insert(entry.id().clone(), state);
            }
        }
    }

    fn find_unread(&self, cx: &App) -> Option<usize> {
        let timeline = self.timeline.read(cx);
        let after = timeline.unread_after()?;
        let me = &self.session.read(cx).me().id;
        let entries = timeline.entries();
        let ix = entries
            .iter()
            .position(|e| e.post().create_at > after && &e.post().user_id != me)?;
        Some(ix)
    }

    fn on_scroll(&mut self, scroller: Entity<PostScrollerState>, cx: &mut Context<Self>) {
        let scroller = scroller.read(cx);
        // Posts that don't fill the viewport leave the oldest of them on
        // screen with no way to scroll up to it.
        let near_top = scroller.fits() || scroller.logical_scroll_top().item_ix < PREFETCH_ROWS;
        let timeline = self.timeline.read(cx);
        if near_top && timeline.has_older() && !timeline.is_loading_older() {
            self.timeline.update(cx, |t, cx| t.load_older(cx));
        }
    }

    // ----- Commands ---------------------------------------------------------

    fn on_composer_event(
        &mut self,
        _: &Entity<Composer>,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Submit { text, files } => {
                self.submit(text.clone(), files.clone(), window, cx)
            }
            ComposerEvent::EditLast => self.edit_last(window, cx),
            ComposerEvent::CancelEdit => self.editing = None,
            ComposerEvent::Typing => {
                let kind = self.kind.clone();
                self.session.update(cx, |s, _| s.notify_typing(&kind));
            }
        }
    }

    fn submit(
        &mut self,
        text: String,
        files: Vec<FileInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(post) = self.editing.take() {
            self.session
                .update(cx, |s, cx| s.edit_post(&post, text, cx));
            return;
        }
        // Attachments always go out as a post; a slash command can't carry them.
        if !files.is_empty() {
            self.send(text, files, cx);
            return;
        }
        let context = CommandContext::new(self.session.clone(), self.kind.clone());
        match extensions::run_slash_command(&text, &context, window, cx) {
            CommandOutcome::Send(message) => self.send(message, Vec::new(), cx),
            CommandOutcome::Handled => {}
            CommandOutcome::Pass if text.starts_with('/') => {
                let channel = self.kind.channel().clone();
                let task = self
                    .session
                    .update(cx, |s, cx| s.execute_command(&channel, text.clone(), cx));
                cx.spawn_in(window, async move |_, cx| {
                    if let Err(err) = task.await {
                        let _ = cx.update(|window, cx| {
                            window.push_notification(
                                gpui_kit::component::notification::Notification::error(format!(
                                    "Couldn’t run {}. {err:#}",
                                    text.split_whitespace().next().unwrap_or_default()
                                )),
                                cx,
                            );
                        });
                    }
                })
                .detach();
            }
            CommandOutcome::Pass => self.send(text, Vec::new(), cx),
        }
    }

    fn send(&mut self, message: String, files: Vec<FileInfo>, cx: &mut Context<Self>) {
        let kind = self.kind.clone();
        self.session
            .update(cx, |s, cx| s.send_post(&kind, message, files, cx));
        self.scroller.update(cx, |s, cx| s.scroll_to_end(cx));
    }

    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me().id.clone();
        let last = self
            .timeline
            .read(cx)
            .entries()
            .iter()
            .rev()
            .find(|e| {
                e.post().user_id == me && e.delivery() == Delivery::Sent && !e.post().is_system()
            })
            .map(|e| e.post().clone());
        if let Some(post) = last {
            self.start_editing(&post, window, cx);
        }
    }

    fn start_editing(&mut self, post: &Post, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(post.id.clone());
        let message = post.message.clone();
        self.composer
            .update(cx, |c, cx| c.set_editing(Some(&message), window, cx));
    }

    fn handle_post_action(
        &mut self,
        id: &PostId,
        action: PostAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(post) = self
            .timeline
            .read(cx)
            .position(id)
            .map(|ix| self.timeline.read(cx).entries()[ix].post().clone())
        else {
            return;
        };
        match action {
            PostAction::OpenThread => {
                if self.kind.root().is_none() {
                    cx.emit(TimelineViewEvent::OpenThread(post.id.clone()));
                } else {
                    self.focus_composer(window, cx);
                }
            }
            PostAction::ToggleReaction(name) => {
                self.session
                    .update(cx, |s, cx| s.toggle_reaction(&post, &name, cx));
            }
            PostAction::CopyText => {
                cx.write_to_clipboard(ClipboardItem::new_string(post.message.clone()));
            }
            PostAction::Edit => self.start_editing(&post, window, cx),
            PostAction::Delete => {
                let session = self.session.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let session = session.clone();
                    let post = post.clone();
                    alert
                        .title("Delete this message?")
                        .description("It will be removed for everyone. This can’t be undone.")
                        .confirm()
                        .ok_text("Delete")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, _, cx| {
                            session.update(cx, |s, cx| s.delete_post(&post, cx));
                            true
                        })
                });
            }
            PostAction::Retry => {
                let kind = self.kind.clone();
                self.session
                    .update(cx, |s, cx| s.retry_post(&kind, &post.id, cx));
            }
            PostAction::Discard => {
                let kind = self.kind.clone();
                self.session
                    .update(cx, |s, cx| s.discard_post(&kind, &post.id, cx));
            }
            PostAction::OpenFile(file) => {
                let client = self.session.read(cx).client().clone();
                if file.is_image() {
                    // Step through the message's pictures, not just this one.
                    let images: Vec<FileInfo> = post
                        .metadata
                        .files
                        .iter()
                        .filter(|f| f.is_image())
                        .cloned()
                        .collect();
                    let index = images.iter().position(|f| f.id == file.id).unwrap_or(0);
                    image_viewer::open(client, images, index, window, cx);
                } else {
                    download::save(client, file, window, cx, download::open);
                }
            }
            PostAction::MessageUser(user) => {
                let task = self
                    .session
                    .update(cx, |s, cx| s.open_direct_channel(user, cx));
                cx.spawn_in(window, async move |this, cx| match task.await {
                    Ok(channel) => {
                        let _ = this
                            .update(cx, |_, cx| cx.emit(TimelineViewEvent::OpenChannel(channel)));
                    }
                    Err(err) => {
                        let _ = cx.update(|window, cx| {
                            window.push_notification(
                                gpui_kit::component::notification::Notification::error(format!(
                                    "Couldn’t open the conversation. {err:#}"
                                )),
                                cx,
                            )
                        });
                    }
                })
                .detach();
            }
        }
    }

    // ----- Rendering --------------------------------------------------------

    fn render_row(&self, ix: usize, view: &WeakEntity<Self>, cx: &App) -> AnyElement {
        let timeline = self.timeline.read(cx);
        let entries = timeline.entries();
        let Some(entry) = entries.get(ix) else {
            return div().into_any_element();
        };
        let session = self.session.read(cx);
        let directory = session.directory().read(cx);
        let post = entry.post();
        let previous = ix
            .checked_sub(1)
            .and_then(|p| entries.get(p))
            .map(|e| e.post());

        let same_day =
            previous.is_some_and(|p| time::day(p.create_at) == time::day(post.create_at));
        let unread_divider = self.unread_ix == Some(ix);
        let continuation = !unread_divider
            && same_day
            && previous.is_some_and(|p| {
                p.user_id == post.user_id
                    && post.create_at - p.create_at < GROUP_WINDOW
                    && !p.is_system()
                    && !post.is_system()
                    && p.override_username() == post.override_username()
            });
        let day_label = (!same_day).then(|| SharedString::from(time::day_label(post.create_at)));

        let author = directory.user(&post.user_id).cloned();
        let avatar = author.as_ref().map(|user| {
            SharedString::from(avatar_url(
                session.client(),
                user.id.as_str(),
                user.last_picture_update,
            ))
        });

        let id = post.id.clone();
        let view = view.clone();
        let on_action: PostActionHandler = Rc::new(move |action, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.handle_post_action(&id, action, window, cx)
            });
        });

        PostRow::new(entry.clone(), session.me().id.clone(), on_action)
            .author(author, avatar)
            .author_status(directory.status(&post.user_id))
            .continuation(continuation)
            .day_label(day_label)
            .unread_divider(unread_divider)
            .rich_text(self.rich_text.get(entry.id()).cloned())
            .thread_entry(self.kind.root().is_none())
            .highlighted(self.flashed.as_ref() == Some(entry.id()))
            .client(session.client().clone())
            .directory(session.directory().clone())
            .custom_emoji(session.custom_emoji().clone())
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let session = self.session.read(cx);
        // The bar shows one truncated line of each; `details` keeps the full
        // text for the details popover.
        let mut details: Vec<HeaderDetail> = Vec::new();
        let (icon, title, subtitle, details_label) = match &self.kind {
            TimelineKind::Thread { channel, .. } => {
                let channel_name = session
                    .channel(channel)
                    .map(|e| session.channel_label(e, cx))
                    .unwrap_or_default();
                if !channel_name.is_empty() {
                    details.push(HeaderDetail::plain("Channel", channel_name.clone()));
                }
                (
                    IconName::MessageSquare,
                    SharedString::from("Thread"),
                    channel_name,
                    "Thread details",
                )
            }
            TimelineKind::Channel(channel) => match session.channel(channel) {
                Some(entry) => {
                    let icon = match entry.kind() {
                        ChannelType::Open => IconName::Hash,
                        ChannelType::Private => IconName::Lock,
                        ChannelType::Direct => IconName::AtSign,
                        ChannelType::Group => IconName::Users,
                    };
                    let details_label = match entry.kind() {
                        ChannelType::Open | ChannelType::Private => "Channel details",
                        ChannelType::Direct | ChannelType::Group => "Conversation details",
                    };
                    let header = entry.channel().header.trim();
                    let purpose = entry.channel().purpose.trim();
                    let subtitle = if header.is_empty() { purpose } else { header };
                    let mut subtitle = subtitle.lines().next().unwrap_or_default().to_string();
                    // A direct message shows the other person's status.
                    if let Some(status) = entry
                        .channel()
                        .direct_partner(&session.me().id)
                        .and_then(|partner| session.directory().read(cx).user(&partner).cloned())
                        .and_then(|user| user.custom_status(&time::now_rfc3339()))
                    {
                        subtitle = format!(
                            "{} {}",
                            crate::status::emoji_char(&status.emoji),
                            status.text
                        );
                        details.push(HeaderDetail::plain("Status", subtitle.clone().into()));
                    }
                    if !header.is_empty() {
                        details.push(HeaderDetail::markdown("Header", header.to_string().into()));
                    }
                    if !purpose.is_empty() {
                        details.push(HeaderDetail::plain("Purpose", purpose.to_string().into()));
                    }
                    (
                        icon,
                        session.channel_label(entry, cx),
                        subtitle.into(),
                        details_label,
                    )
                }
                None => (
                    IconName::Hash,
                    SharedString::from(""),
                    SharedString::default(),
                    "Channel details",
                ),
            },
        };
        let is_thread = self.kind.root().is_some();
        let favorite = match &self.kind {
            TimelineKind::Channel(channel) if session.channel(channel).is_some() => {
                Some((channel.clone(), session.is_favorite(channel)))
            }
            _ => None,
        };
        // The title block is the trigger: a quiet Button keeps the compact bar
        // while giving the full text a keyboard-reachable home.
        let trigger = Button::new("channel-details-trigger")
            .ghost()
            .large()
            .w_full()
            .h_auto()
            .px_2()
            .py_1()
            .accessibility_label(if title.is_empty() {
                SharedString::from(details_label)
            } else {
                SharedString::from(format!("{details_label}: {title}"))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(
                        gpui_kit::component::Icon::new(icon)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .font_semibold()
                                            .text_color(theme.foreground)
                                            .truncate()
                                            .child(title.clone()),
                                    )
                                    .child(
                                        gpui_kit::component::Icon::new(IconName::ChevronDown)
                                            .xsmall()
                                            .flex_none()
                                            .text_color(theme.muted_foreground),
                                    ),
                            )
                            .when(!subtitle.is_empty(), |this| {
                                this.child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(subtitle),
                                )
                            }),
                    ),
            );
        let details = Rc::new(details);
        let details_popover = Popover::new("channel-details")
            .anchor(gpui_kit::Anchor::TopLeft)
            .trigger_style(StyleRefinement::default().flex_1().min_w_0())
            .trigger(trigger)
            .content(move |_, _, cx| render_header_details(&title, &details, cx));
        h_flex()
            .flex_none()
            .h_16()
            .pl_2()
            .pr_4()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(details_popover)
            .when_some(favorite, |this, (channel, favorite)| {
                // A toggle: the filled star and pressed state show it's on.
                this.child(
                    Button::new("favorite")
                        .small()
                        .ghost()
                        .icon(if favorite {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .toggled(favorite)
                        .accessibility_label("Favorite")
                        .tooltip(crate::sidebar::favorite_label(favorite))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.session
                                .update(cx, |s, cx| s.set_favorite(&channel, !favorite, cx));
                        })),
                )
            })
            .when(is_thread, |this| {
                let (icon, label) = if self.back_to_results {
                    (IconName::ArrowLeft, "Back to search results")
                } else {
                    (IconName::X, "Close thread")
                };
                this.child(
                    Button::new("close-thread")
                        .small()
                        .ghost()
                        .icon(icon)
                        .accessibility_label(label)
                        .tooltip(label)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(TimelineViewEvent::Close))),
                )
            })
    }

    fn render_typing(&self, cx: &App) -> impl IntoElement {
        let session = self.session.read(cx);
        let directory = session.directory().read(cx);
        let names: Vec<SharedString> = session
            .typing_users(&self.kind)
            .iter()
            .map(|id| directory.display_name(id))
            .collect();
        let text = match names.as_slice() {
            [] => String::new(),
            [one] => format!("{one} is typing…"),
            [a, b] => format!("{a} and {b} are typing…"),
            _ => "Several people are typing…".to_string(),
        };
        // Fixed height: the composer must not jump when someone types.
        div()
            .flex_none()
            .h_5()
            .px_5()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .truncate()
            .child(text)
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let timeline = self.timeline.read(cx);
        let is_empty = timeline.entries().is_empty();
        match timeline.state() {
            LoadState::NotLoaded | LoadState::Loading if is_empty => {
                crate::skeleton::posts(cx).into_any_element()
            }
            LoadState::Failed(error) if is_empty => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_sm()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(error.clone()),
                )
                .child(
                    Button::new("retry-load")
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.timeline.update(cx, |t, cx| t.reload(cx));
                        })),
                )
                .into_any_element(),
            _ if is_empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if self.kind.root().is_some() {
                    "No replies yet."
                } else {
                    "No messages yet. Start the conversation."
                })
                .into_any_element(),
            _ => {
                let view = cx.entity().downgrade();
                let loading_older = timeline.is_loading_older();
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(PostScroller::new(
                        "posts",
                        self.scroller.clone(),
                        move |ix, _, cx| {
                            view.upgrade()
                                .map(|v| v.read(cx).render_row(ix, &v.downgrade(), cx))
                                .unwrap_or_else(|| div().into_any_element())
                        },
                    ))
                    .when(loading_older, |this| {
                        this.child(
                            h_flex()
                                .absolute()
                                .top_2()
                                .left_0()
                                .right_0()
                                .justify_center()
                                .child(Spinner::new().small()),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

/// One labeled field of the header details popover.
struct HeaderDetail {
    label: &'static str,
    text: SharedString,
    /// Channel headers are Markdown; purposes and statuses are plain text.
    markdown: bool,
}

impl HeaderDetail {
    fn plain(label: &'static str, text: SharedString) -> Self {
        Self {
            label,
            text,
            markdown: false,
        }
    }

    fn markdown(label: &'static str, text: SharedString) -> Self {
        Self {
            label,
            text,
            markdown: true,
        }
    }
}

/// The full, wrapped title and fields the header bar truncates.
fn render_header_details(title: &SharedString, details: &[HeaderDetail], cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .id("channel-details-body")
        .w_80()
        .max_h_96()
        .overflow_y_scroll()
        .gap_3()
        .whitespace_normal()
        .text_sm()
        .when(!title.is_empty(), |this| {
            this.child(
                div()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(title.clone()),
            )
        })
        .children(details.iter().map(|detail| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_medium()
                        .text_color(theme.muted_foreground)
                        .child(detail.label),
                )
                .child(if detail.markdown {
                    TextView::markdown(
                        SharedString::from(format!("channel-details-{}", detail.label)),
                        detail.text.clone(),
                    )
                    .selectable(true)
                    .into_any_element()
                } else {
                    div()
                        .text_color(theme.foreground)
                        .child(detail.text.clone())
                        .into_any_element()
                })
        }))
        .into_any_element()
}

impl Focusable for TimelineView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl Render for TimelineView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let drop_tint = cx.theme().primary.opacity(0.06);
        v_flex()
            .id("timeline-view")
            .size_full()
            .min_w_0()
            // Drop files anywhere on the conversation to attach them.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_tint))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.composer.update(cx, |composer, cx| {
                    composer.attach_paths(paths.paths().to_vec(), cx)
                });
                this.focus_composer(window, cx);
            }))
            .child(self.render_header(cx))
            .child(self.render_body(cx))
            .child(self.render_typing(cx))
            .child(self.composer.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        ops::Range,
        time::{Duration, Instant},
    };

    use gpui_kit::{Bounds, Pixels, TestAppContext, WindowBounds, WindowOptions, point, px, size};
    use mattermost::{
        Client, TeamId, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::*;

    /// Posts `n{i}` of the channel `town`, created at `i + 1`.
    fn posts(range: Range<usize>) -> serde_json::Value {
        let order: Vec<String> = range.clone().rev().map(|i| format!("n{i}")).collect();
        let posts: serde_json::Map<String, serde_json::Value> = range
            .map(|i| {
                let id = format!("n{i}");
                let post = json!({ "id": id, "channel_id": "town", "user_id": "ann",
                                   "message": format!("message {i}"), "create_at": i + 1 });
                (id, post)
            })
            .collect();
        json!({ "order": order, "posts": posts })
    }

    /// A channel of 120 posts: the newest page holds the last 60, the
    /// page before it the first 60, and nothing comes before those.
    fn route(request: &Request) -> Reply {
        let path = request.path();
        match path.split('?').next().unwrap_or_default() {
            "/api/v4/users/me/teams" => Reply::json(json!([{ "id": "team", "name": "team" }])),
            "/api/v4/users/me/teams/team/channels" => Reply::json(json!([
                { "id": "town", "team_id": "team", "type": "O", "name": "town-square" },
            ])),
            "/api/v4/channels/town/posts" if path.contains("before=n60") => {
                Reply::json(posts(0..60))
            }
            "/api/v4/channels/town/posts" if path.contains("before=") => Reply::json(json!({})),
            "/api/v4/channels/town/posts" => Reply::json(posts(60..120)),
            "/api/v4/users/me/teams/team/channels/members"
            | "/api/v4/users/me/preferences/favorite_channel"
            | "/api/v4/emoji"
            | "/api/v4/users/ids"
            | "/api/v4/users/status/ids" => Reply::json(json!([])),
            _ => Reply::not_found(),
        }
    }

    /// Run GPUI's work and let real network I/O happen until `done`.
    fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting");
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The channel's view in a window once its newest page is shown, and
    /// the threads it asks to open.
    fn open_channel(
        server: &SlowServer,
        cx: &mut TestAppContext,
    ) -> (Entity<TimelineView>, Rc<RefCell<Vec<PostId>>>) {
        open_channel_in(px(600.), server, cx)
    }

    /// [`open_channel`] in a window of the given height.
    fn open_channel_in(
        height: Pixels,
        server: &SlowServer,
        cx: &mut TestAppContext,
    ) -> (Entity<TimelineView>, Rc<RefCell<Vec<PostId>>>) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let session = cx.new(|cx| Session::new(client, me, Some(TeamId::from("team")), cx));
        // Shown, as the workspace does.
        session.update(cx, |s, cx| {
            s.open_channel(&"town".into(), cx);
        });
        let view = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(800.), height),
                })),
                ..Default::default()
            };
            let kind = TimelineKind::Channel("town".into());
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| TimelineView::new(kind, session, None, window, cx))
            })
            .unwrap()
            .1
        });
        let threads = Rc::new(RefCell::new(Vec::new()));
        let opened = threads.clone();
        cx.update(|cx| {
            cx.subscribe(&view, move |_, event: &TimelineViewEvent, _| {
                if let TimelineViewEvent::OpenThread(root) = event {
                    opened.borrow_mut().push(root.clone());
                }
            })
            .detach()
        });
        wait_until(cx, |cx| {
            view.read_with(cx, |v, cx| v.timeline.read(cx).entries().len() >= 60)
        });
        (view, threads)
    }

    fn older_pages(server: &SlowServer) -> usize {
        server
            .requests()
            .iter()
            .filter(|r| r.path().contains("before="))
            .count()
    }

    #[gpui_kit::test]
    fn deleting_a_post_releases_its_parsed_rich_text(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.path().split('?').next() == Some("/api/v4/channels/town/posts") {
                let mut page = posts(60..120);
                page["posts"]["n70"]["message"] = json!("**rich post**");
                page["posts"]["n71"]["message"] = json!("**keep this post**");
                Reply::json(page)
            } else {
                route(request)
            }
        });
        let (view, _) = open_channel(&server, cx);
        let (timeline, parsed) = view.read_with(cx, |v, _| {
            (
                v.timeline.clone(),
                v.rich_text[&PostId::from("n70")].downgrade(),
            )
        });
        timeline.update(cx, |t, cx| t.remove(&"n70".into(), cx));
        cx.run_until_parked();
        view.read_with(cx, |v, _| {
            assert!(!v.rich_text.contains_key(&PostId::from("n70")));
            assert!(v.rich_text.contains_key(&PostId::from("n71")));
        });
        assert!(parsed.upgrade().is_none());
    }

    #[gpui_kit::test]
    fn posts_that_dont_fill_the_window_bring_older_ones(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        // Tall enough to show the newest page with room to spare.
        let (view, _) = open_channel_in(px(4000.), &server, cx);
        wait_until(cx, |cx| {
            view.read_with(cx, |v, cx| v.timeline.read(cx).entries().len() == 120)
        });
    }

    #[gpui_kit::test]
    fn revealing_a_loaded_post_scrolls_to_it_and_marks_it(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (view, threads) = open_channel(&server, cx);
        view.update(cx, |v, cx| v.reveal("n70".into(), 71, cx));
        cx.run_until_parked();
        view.read_with(cx, |v, cx| {
            assert_eq!(v.flashed, Some("n70".into()));
            let ix = v.timeline.read(cx).position(&"n70".into()).unwrap();
            assert_eq!(v.scroller.read(cx).logical_scroll_top().item_ix, ix);
        });
        assert_eq!(older_pages(&server), 0);
        assert!(threads.borrow().is_empty());
        // The mark fades on its own.
        cx.executor().advance_clock(FLASH);
        cx.run_until_parked();
        view.read_with(cx, |v, _| assert_eq!(v.flashed, None));
    }

    #[gpui_kit::test]
    fn revealing_an_older_post_loads_back_to_it(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (view, threads) = open_channel(&server, cx);
        view.update(cx, |v, cx| v.reveal("n5".into(), 6, cx));
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| v.flashed == Some("n5".into()))
        });
        view.read_with(cx, |v, cx| {
            let ix = v.timeline.read(cx).position(&"n5".into()).unwrap();
            assert_eq!(ix, 5);
            assert_eq!(v.scroller.read(cx).logical_scroll_top().item_ix, ix);
        });
        assert!(threads.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn a_post_the_channel_doesnt_reach_opens_in_its_thread(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (view, threads) = open_channel(&server, cx);
        view.update(cx, |v, cx| v.reveal("gone".into(), 0, cx));
        wait_until(cx, |_| !threads.borrow().is_empty());
        assert_eq!(*threads.borrow(), [PostId::from("gone")]);
        view.read_with(cx, |v, _| {
            assert!(v.revealing.is_none());
            assert_eq!(v.flashed, None);
        });
    }

    #[gpui_kit::test]
    fn a_reveal_stops_when_another_conversation_opens(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (view, threads) = open_channel(&server, cx);
        let session = view.read_with(cx, |v, _| v.session.clone());
        view.update(cx, |v, cx| v.reveal("gone".into(), 0, cx));
        // Another channel opens while the older page is on its way.
        session.update(cx, |s, cx| {
            s.open_channel(&"other".into(), cx);
        });
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.revealing.is_none()));
        assert!(threads.borrow().is_empty());
        assert_eq!(older_pages(&server), 1);
    }
}
