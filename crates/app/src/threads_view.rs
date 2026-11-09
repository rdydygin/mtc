//! The Threads pane's list: the threads the user follows, newest reply
//! first, with All and Unread tabs. The workspace shows the open thread
//! beside it.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, UniformListScrollHandle, WeakEntity, Window, div, prelude::FluentBuilder as _,
    rems, uniform_list,
};
use mattermost::{ChannelId, PostId};
use session::{FollowedThreads, LoadState, Session, ThreadFilter};

use crate::{http::avatar_url, time};

/// A row's height in rems: the author line, two lines of preview, and the
/// participants. Rows share one height so the list can be virtualized.
pub const ROW_HEIGHT: f32 = 7.;
/// Participants shown in a row before a count of the rest.
const PARTICIPANTS_SHOWN: usize = 3;
/// Load the next page when the last visible row is this close to the end.
const PREFETCH_ROWS: usize = 5;

pub enum ThreadsViewEvent {
    OpenThread {
        channel: ChannelId,
        root: PostId,
    },
    /// Show the thread's channel, with the thread beside it.
    OpenInChannel {
        channel: ChannelId,
        root: PostId,
    },
}

pub struct ThreadsView {
    session: Entity<Session>,
    filter: ThreadFilter,
    /// The threads of the current tab, as last rendered.
    rows: Vec<PostId>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ThreadsViewEvent> for ThreadsView {}

impl ThreadsView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let threads = session.read(cx).threads().clone();
        let directory = session.read(cx).directory().clone();
        let subscriptions = vec![
            cx.observe(&threads, |this, threads, cx| {
                // A team switch empties the lists; fill the current one again.
                let filter = this.filter;
                if threads.read(cx).state(filter) == &LoadState::NotLoaded {
                    threads.update(cx, |t, cx| t.load(filter, cx));
                }
                cx.notify()
            }),
            cx.observe(&directory, |_, _, cx| cx.notify()),
            // Channel names, and which thread is open.
            cx.observe(&session, |_, _, cx| cx.notify()),
        ];
        Self {
            session,
            filter: ThreadFilter::All,
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn threads(&self, cx: &App) -> Entity<FollowedThreads> {
        self.session.read(cx).threads().clone()
    }

    /// Bring the current tab up to date as the pane comes into view.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter;
        self.threads(cx).update(cx, |t, cx| match filter {
            ThreadFilter::All => t.load(filter, cx),
            // Threads read since the tab was last up leave it now.
            ThreadFilter::Unread => t.reload(filter, cx),
        });
    }

    fn set_filter(&mut self, filter: ThreadFilter, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        // Each tab starts at its newest thread.
        self.scroll = UniformListScrollHandle::new();
        self.shown(cx);
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let threads = self.threads(cx);
        let threads = threads.read(cx);
        let (unread, mentions) = (threads.unread_threads(), threads.unread_mentions());
        let subtitle = match (unread, mentions) {
            (0, 0) => "No unread replies".to_string(),
            (unread, 0) => format!("{unread} unread"),
            (unread, 1) => format!("{unread} unread, 1 mention"),
            (unread, mentions) => format!("{unread} unread, {mentions} mentions"),
        };
        h_flex()
            .flex_none()
            .h_16()
            .pl_4()
            .pr_4()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Icon::new(IconName::MessagesSquare)
                    .small()
                    .text_color(theme.muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(theme.foreground)
                            .truncate()
                            .child("Threads"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .child(
                Button::new("mark-all-threads-read")
                    .small()
                    .ghost()
                    .icon(IconName::CheckCheck)
                    .disabled(unread == 0 && mentions == 0)
                    .accessibility_label("Mark all threads as read")
                    .tooltip("Mark all as read")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.threads(cx).update(cx, |t, cx| t.mark_all_read(cx));
                    })),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().flex_none().px_3().py_2().child(
            TabBar::new("thread-filter")
                .segmented()
                .small()
                .selected_index(match self.filter {
                    ThreadFilter::All => 0,
                    ThreadFilter::Unread => 1,
                })
                .child(Tab::new().label("All"))
                .child(Tab::new().label("Unread"))
                .on_click(cx.listener(|this, ix: &usize, _, cx| {
                    let filter = if *ix == 0 {
                        ThreadFilter::All
                    } else {
                        ThreadFilter::Unread
                    };
                    this.set_filter(filter, cx);
                })),
        )
    }

    fn render_row(&self, ix: usize, view: &WeakEntity<Self>, cx: &App) -> AnyElement {
        let session = self.session.read(cx);
        let threads = session.threads().read(cx);
        let Some(thread) = self.rows.get(ix).and_then(|id| threads.thread(id)) else {
            return div().h(rems(ROW_HEIGHT)).into_any_element();
        };
        let theme = cx.theme();
        let directory = session.directory().read(cx);
        let root = thread.root();
        let author: SharedString = match root.override_username() {
            Some(name) => name.to_string().into(),
            None => directory.display_name(&root.user_id),
        };
        let channel = session
            .channel(thread.channel())
            .map(|entry| session.channel_label(entry, cx));
        let unread = thread.is_unread();
        let mentions = thread.unread_mentions();
        let open = session.open_thread() == Some(thread.id());
        let replies = match thread.reply_count() {
            1 => "1 reply".to_string(),
            count => format!("{count} replies"),
        };
        let when = time::recent(thread.last_reply_at());
        // Whoever replied most recently comes first.
        let participants = thread.participants();
        let shown: Vec<AnyElement> = participants
            .iter()
            .rev()
            .take(PARTICIPANTS_SHOWN)
            .map(|id| {
                let mut avatar = Avatar::new().name(directory.display_name(id)).size_5();
                if let Some(user) = directory.user(id) {
                    avatar = avatar.src(avatar_url(
                        session.client(),
                        id.as_str(),
                        user.last_picture_update,
                    ));
                }
                avatar.into_any_element()
            })
            .collect();
        let others = participants.len().saturating_sub(PARTICIPANTS_SHOWN);

        let label = format!(
            "{author}{}: {}. {replies}{}{}",
            channel
                .as_ref()
                .map(|c| format!(" in {c}"))
                .unwrap_or_default(),
            thread.preview(),
            if unread { ", unread" } else { "" },
            match mentions {
                0 => String::new(),
                1 => ", 1 mention".to_string(),
                n => format!(", {n} mentions"),
            },
        );
        let content = v_flex()
            .size_full()
            .min_w_0()
            .justify_between()
            .whitespace_normal()
            .text_left()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .text_sm()
                    .child(
                        // The unread dot keeps its lane, so names align.
                        div()
                            .size_2()
                            .flex_none()
                            .rounded_full()
                            .when(unread, |this| this.bg(theme.primary)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.foreground)
                            .map(|this| {
                                if unread {
                                    this.font_semibold()
                                } else {
                                    this.font_medium()
                                }
                            })
                            .child(author),
                    )
                    .when_some(channel, |this, channel| {
                        this.child(
                            div()
                                .min_w_0()
                                .max_w(rems(10.))
                                .px_1()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(channel),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(when),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .pl_4()
                    .text_sm()
                    .line_clamp(2)
                    .text_ellipsis()
                    .text_color(if unread {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(thread.preview().clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .pl_4()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(h_flex().gap_0p5().children(shown))
                    .when(others > 0, |this| this.child(format!("+{others}")))
                    .child(replies)
                    .child(div().flex_1())
                    .when(mentions > 0, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .px_1p5()
                                .rounded_full()
                                .bg(theme.primary)
                                .text_color(theme.primary_foreground)
                                .font_semibold()
                                .child(format!("@{}", mentions.min(99))),
                        )
                    }),
            );

        let channel = thread.channel().clone();
        let root = thread.id().clone();
        let threads = session.threads().clone();
        let open_view = view.clone();
        let menu_view = view.clone();
        div()
            .h(rems(ROW_HEIGHT))
            .px_2()
            .py_0p5()
            .child(
                Button::new(SharedString::from(format!("thread-{root}")))
                    .ghost()
                    .size_full()
                    .px_2()
                    .py_2()
                    .selected(open)
                    .accessibility_label(label)
                    .child(content)
                    .on_click({
                        let (channel, root) = (channel.clone(), root.clone());
                        move |_, _, cx| {
                            let event = ThreadsViewEvent::OpenThread {
                                channel: channel.clone(),
                                root: root.clone(),
                            };
                            let _ = open_view.update(cx, |_, cx| cx.emit(event));
                        }
                    })
                    .context_menu(move |menu, _, _| {
                        let (channel, root) = (channel.clone(), root.clone());
                        let view = menu_view.clone();
                        let open = PopupMenuItem::new("Open in channel").on_click({
                            let (channel, root) = (channel.clone(), root.clone());
                            move |_, _, cx| {
                                let event = ThreadsViewEvent::OpenInChannel {
                                    channel: channel.clone(),
                                    root: root.clone(),
                                };
                                let _ = view.update(cx, |_, cx| cx.emit(event));
                            }
                        });
                        let read = unread.then(|| {
                            let (threads, root) = (threads.clone(), root.clone());
                            PopupMenuItem::new("Mark as read").on_click(move |_, _, cx| {
                                threads.update(cx, |t, cx| t.mark_read(&root, cx));
                            })
                        });
                        let unfollow = {
                            let threads = threads.clone();
                            PopupMenuItem::new("Unfollow thread").on_click(move |_, _, cx| {
                                threads.update(cx, |t, cx| t.unfollow(&root, cx));
                            })
                        };
                        menu.item(open)
                            .when_some(read, |menu, read| menu.item(read))
                            .separator()
                            .item(unfollow)
                    }),
            )
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let threads = self.threads(cx);
        let (state, has_more, loading_more) = {
            let threads = threads.read(cx);
            self.rows = threads
                .threads(self.filter)
                .map(|t| t.id().clone())
                .collect();
            (
                threads.state(self.filter).clone(),
                threads.has_more(self.filter),
                threads.is_loading_more(self.filter),
            )
        };
        if self.rows.is_empty() {
            return match state {
                LoadState::NotLoaded | LoadState::Loading => {
                    crate::skeleton::thread_rows(cx).into_any_element()
                }
                LoadState::Failed(error) => {
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .p_4()
                        .text_sm()
                        .child(
                            div()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child(error),
                        )
                        .child(Button::new("retry-threads").label("Try again").on_click(
                            cx.listener(|this, _, _, cx| {
                                let filter = this.filter;
                                this.threads(cx).update(cx, |t, cx| t.reload(filter, cx));
                            }),
                        ))
                        .into_any_element()
                }
                LoadState::Loaded => {
                    let (title, body) = match self.filter {
                        ThreadFilter::All => (
                            "No followed threads",
                            "Threads you start, reply to, or are mentioned in show up here.",
                        ),
                        ThreadFilter::Unread => ("No unread threads", "You’re all caught up."),
                    };
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .p_6()
                        .text_center()
                        .child(div().text_sm().font_medium().child(title))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(body),
                        )
                        .into_any_element()
                }
            };
        }
        let view = cx.entity().downgrade();
        let count = self.rows.len();
        let filter = self.filter;
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list("threads", count, move |range, _, cx| {
                    if has_more && !loading_more && range.end + PREFETCH_ROWS >= count {
                        let threads = threads.clone();
                        cx.defer(move |cx| threads.update(cx, |t, cx| t.load_more(filter, cx)));
                    }
                    let Some(this) = view.upgrade() else {
                        return Vec::new();
                    };
                    let this = this.read(cx);
                    range.map(|ix| this.render_row(ix, &view, cx)).collect()
                })
                .track_scroll(&self.scroll)
                .size_full()
                .pb_2(),
            )
            .vertical_scrollbar(&self.scroll)
            .when(loading_more, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .right_0()
                        .justify_center()
                        .child(Spinner::new().small()),
                )
            })
            .into_any_element()
    }
}

impl Focusable for ThreadsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("threads-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .child(self.render_header(cx))
            .child(self.render_tabs(cx))
            .child(self.render_body(cx))
    }
}
