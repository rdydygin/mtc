//! The Threads pane's list: the threads the user follows, newest reply
//! first, with All and Unread tabs. The workspace shows the open thread
//! beside it.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, ScrollStrategy,
    SharedString, Styled as _, Subscription, UniformListScrollHandle, WeakEntity, Window, div,
    prelude::FluentBuilder as _, rems, uniform_list,
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

gpui_kit::actions!(
    threads,
    [
        SelectPrevious,
        SelectNext,
        SelectFirst,
        SelectLast,
        OpenSelected
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some("ThreadsList")),
        KeyBinding::new("down", SelectNext, Some("ThreadsList")),
        KeyBinding::new("home", SelectFirst, Some("ThreadsList")),
        KeyBinding::new("end", SelectLast, Some("ThreadsList")),
        KeyBinding::new("enter", OpenSelected, Some("ThreadsList")),
    ]);
}

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
    selected: Option<PostId>,
    row_focus: FocusHandle,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ThreadsViewEvent> for ThreadsView {}

impl ThreadsView {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let threads = session.read(cx).threads().clone();
        let directory = session.read(cx).directory().clone();
        let subscriptions = vec![
            cx.observe_in(&threads, window, |this, threads, window, cx| {
                // A team switch empties the lists; fill the current one again.
                let filter = this.filter;
                if threads.read(cx).state(filter) == &LoadState::NotLoaded {
                    threads.update(cx, |t, cx| t.load(filter, cx));
                }
                this.sync_rows(window, cx);
                cx.notify()
            }),
            cx.observe(&directory, |_, _, cx| cx.notify()),
            // Channel names, and which thread is open.
            cx.observe(&session, |_, _, cx| cx.notify()),
        ];
        let rows: Vec<_> = threads
            .read(cx)
            .threads(ThreadFilter::All)
            .map(|t| t.id().clone())
            .collect();
        let selected = rows.first().cloned();
        Self {
            session,
            filter: ThreadFilter::All,
            rows,
            selected,
            row_focus: cx.focus_handle().tab_stop(true),
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owns_focus = self.focus_handle.contains_focused(window, cx);
        let previous = std::mem::take(&mut self.rows);
        self.rows = self
            .threads(cx)
            .read(cx)
            .threads(self.filter)
            .map(|t| t.id().clone())
            .collect();
        if self
            .selected
            .as_ref()
            .is_none_or(|id| !self.rows.contains(id))
        {
            self.selected = self.rows.first().cloned();
        }
        // Reconcile before the next frame can virtualize the focused row away.
        // A background refresh must never take focus from the detail composer.
        if owns_focus && previous != self.rows {
            self.focus_selected(window, cx);
        }
    }

    pub fn focus_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| row == id))
        {
            self.select_row(ix, window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
    }

    fn select_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.rows.get(ix).cloned();
        self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        self.row_focus.focus(window, cx);
        cx.notify();
    }

    fn open_thread(&mut self, root: PostId, in_channel: bool, cx: &mut Context<Self>) {
        let Some(channel) = self
            .threads(cx)
            .read(cx)
            .thread(&root)
            .map(|t| t.channel().clone())
        else {
            return;
        };
        self.selected = Some(root.clone());
        cx.emit(if in_channel {
            ThreadsViewEvent::OpenInChannel { channel, root }
        } else {
            ThreadsViewEvent::OpenThread { channel, root }
        });
        cx.notify();
    }

    fn navigate(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let ix = self
            .selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| row == id))
            .unwrap_or(0);
        let ix = ix.saturating_add_signed(offset).min(self.rows.len() - 1);
        self.select_row(ix, window, cx);
    }

    fn thread_menu(
        &self,
        menu: PopupMenu,
        root: PostId,
        view: WeakEntity<Self>,
        cx: &App,
    ) -> PopupMenu {
        let threads = self.threads(cx);
        let unread = threads
            .read(cx)
            .thread(&root)
            .is_some_and(|t| t.is_unread());
        let open_root = root.clone();
        let read_root = root.clone();
        let read_threads = threads.clone();
        menu.item(
            PopupMenuItem::new("Open in channel").on_click(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.open_thread(open_root.clone(), true, cx));
            }),
        )
        .when(unread, |menu| {
            menu.item(
                PopupMenuItem::new("Mark as read").on_click(move |_, _, cx| {
                    read_threads.update(cx, |t, cx| t.mark_read(&read_root, cx));
                }),
            )
        })
        .separator()
        .item(
            PopupMenuItem::new("Unfollow thread").on_click(move |_, _, cx| {
                threads.update(cx, |t, cx| t.unfollow(&root, cx));
            }),
        )
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

    fn set_filter(&mut self, filter: ThreadFilter, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        // Each tab starts at its newest thread.
        self.scroll = UniformListScrollHandle::new();
        self.shown(cx);
        self.sync_rows(window, cx);
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
                .on_click(cx.listener(|this, ix: &usize, window, cx| {
                    let filter = if *ix == 0 {
                        ThreadFilter::All
                    } else {
                        ThreadFilter::Unread
                    };
                    this.set_filter(filter, window, cx);
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
        let selected = self.selected.as_ref() == Some(thread.id());
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

        let root = thread.id().clone();
        let open_view = view.clone();
        let menu_view = view.clone();
        let more_view = view.clone();
        let menu_root = root.clone();
        let more_root = root.clone();
        h_flex()
            .h(rems(ROW_HEIGHT))
            .px_2()
            .py_0p5()
            .child(
                Button::new(SharedString::from(format!("thread-{root}")))
                    .ghost()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_2()
                    .selected(selected)
                    .tab_stop(selected)
                    .when(selected, |button| button.track_focus(&self.row_focus))
                    .accessibility_label(label)
                    .child(content)
                    .on_click(move |_, _, cx| {
                        let _ = open_view
                            .update(cx, |this, cx| this.open_thread(root.clone(), false, cx));
                    })
                    .context_menu(move |menu, _, cx| match menu_view.upgrade() {
                        Some(view) => {
                            view.read(cx)
                                .thread_menu(menu, menu_root.clone(), view.downgrade(), cx)
                        }
                        None => menu,
                    }),
            )
            .child(div().w_8().flex_none().when(selected, |this| {
                this.child(
                    Button::new("thread-more-actions")
                        .small()
                        .ghost()
                        .icon(IconName::Ellipsis)
                        .accessibility_label("More actions")
                        .tooltip("More actions")
                        .dropdown_menu(move |menu, _, cx| match more_view.upgrade() {
                            Some(view) => view.read(cx).thread_menu(
                                menu,
                                more_root.clone(),
                                view.downgrade(),
                                cx,
                            ),
                            None => menu,
                        }),
                )
            }))
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let threads = self.threads(cx);
        let (state, has_more, loading_more) = {
            let threads = threads.read(cx);
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
        if self.selected.is_some() {
            self.row_focus.clone()
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Render for ThreadsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("threads-view")
            .track_focus(&self.focus_handle)
            .key_context("ThreadsList")
            .on_action(
                cx.listener(|this, _: &SelectPrevious, window, cx| this.navigate(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| this.navigate(1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, window, cx| {
                this.navigate(isize::MIN, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectLast, window, cx| {
                    this.navigate(isize::MAX, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| {
                if let Some(root) = this.selected.clone() {
                    this.open_thread(root, false, cx);
                }
            }))
            .size_full()
            .min_w_0()
            .child(self.render_header(cx))
            .child(self.render_tabs(cx))
            .child(self.render_body(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::tests::{route, start, wait_until};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use mattermost::testing::{Network, Reply, SlowServer};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, time::Duration};

    #[gpui_kit::test]
    fn refreshed_rows_keep_keyboard_focus_and_selection_visible(cx: &mut TestAppContext) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let reordered = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let reordered = reordered.clone();
            move |request| {
                if request.method() == "GET"
                    && request
                        .path()
                        .starts_with("/api/v4/users/me/teams/team/threads?")
                {
                    let mut threads: Vec<_> = (0..20).map(|i| json!({
                        "id": format!("t{i}"), "reply_count": 1,
                        "last_reply_at": if i == 18 && reordered.load(Ordering::SeqCst) { 1000 } else { 100 - i },
                        "post": {"id": format!("t{i}"), "channel_id": "town", "message": "Thread"}
                    })).collect();
                    if reordered.load(Ordering::SeqCst) {
                        let moved = threads.remove(18);
                        threads.insert(0, moved);
                    }
                    return Reply::json(json!({"threads": threads, "total": 20}));
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let threads = session.read_with(cx, |s, _| s.threads().clone());
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ThreadsView::new(session.clone(), window, cx))
            })
            .unwrap()
        });
        view.update(cx, |v, cx| v.shown(cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.rows.len() == 20));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let opened = opened.clone();
            cx.subscribe(&view, move |_, event, _| {
                if let ThreadsViewEvent::OpenThread { root, .. } = event {
                    opened.borrow_mut().push(root.clone());
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            view.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("end", cx);
            assert_eq!(view.read(cx).selected, Some("t19".into()));
        })
        .unwrap();
        threads.update(cx, |t, cx| t.forget(&"t19".into(), cx));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("thread-t0").focused(), Some(true));
            assert!(window.find("thread-t0").visible());
            window.press("down", cx);
            window.press("enter", cx);
            window.press("end", cx);
            assert_eq!(view.read(cx).selected, Some("t18".into()));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), vec![PostId::from("t1")]);
        reordered.store(true, Ordering::SeqCst);
        threads.update(cx, |t, cx| t.reload(ThreadFilter::All, cx));
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| v.rows.first() == Some(&PostId::from("t18")))
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("thread-t18").focused(), Some(true));
            assert!(window.find("thread-t18").visible());
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(opened.borrow().last(), Some(&PostId::from("t0")));
        threads.update(cx, |t, cx| {
            for i in 0..20 {
                t.forget(&format!("t{i}").into(), cx);
            }
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(view.read(cx).selected.is_none());
            assert!(view.read(cx).focus_handle.is_focused(window));
        })
        .unwrap();
        threads.update(cx, |t, cx| t.reload(ThreadFilter::All, cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.rows.len() == 20));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("up", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(opened.borrow().last(), Some(&PostId::from("t18")));
    }

    #[gpui_kit::test]
    fn keyboard_selection_and_shared_actions(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.method() == "GET"
                && request
                    .path()
                    .starts_with("/api/v4/users/me/teams/team/threads?")
            {
                let threads: Vec<_> = (0..20).map(|i| json!({
                    "id": format!("t{i}"), "reply_count": 1, "last_reply_at": 100 - i,
                    "unread_replies": 1, "unread_mentions": 0, "participants": [],
                    "post": { "id": format!("t{i}"), "channel_id": "town", "message": format!("Thread {i}") }
                })).collect();
                return Reply::json(
                    json!({"threads": threads, "total": 20, "total_unread_threads": 20}),
                );
            }
            if matches!(request.method(), "PUT" | "DELETE")
                && request
                    .path()
                    .starts_with("/api/v4/users/me/teams/team/threads/")
            {
                return Reply::json(json!({}));
            }
            route(request)
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ThreadsView::new(session.clone(), window, cx))
            })
            .unwrap()
        });
        view.update(cx, |v, cx| v.shown(cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.rows.len() == 20));
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&view, move |_, event, _| match event {
                ThreadsViewEvent::OpenThread { root, .. } => {
                    events.borrow_mut().push((false, root.clone()))
                }
                ThreadsViewEvent::OpenInChannel { root, .. } => {
                    events.borrow_mut().push((true, root.clone()))
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            view.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("down", cx);
            assert_eq!(view.read(cx).selected, Some("t1".into()));
            window.press("end", cx);
            assert_eq!(view.read(cx).selected, Some("t19".into()));
            assert!(window.find("thread-t19").visible());
            window.press("up", cx);
            assert_eq!(view.read(cx).selected, Some("t18".into()));
            window.press("home", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(*events.borrow(), vec![(false, "t0".into())]);
        cx.update_window(handle, |_, window, cx| {
            // The workspace restores this selection when the detail closes.
            view.update(cx, |view, cx| view.focus_selected(window, cx));
            window.render_frame(cx);
            assert_eq!(window.find("thread-t0").focused(), Some(true));
            window.press("tab", cx);
            assert_eq!(window.find("thread-more-actions").focused(), Some(true));
            window.press("enter", cx);
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("thread-more-actions").focused(), Some(true));
            window.press("enter", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(events.borrow().last(), Some(&(true, "t0".into())));
        // Read and unfollow are reached through the same dropdown, using only keys.
        cx.update_window(handle, |_, window, cx| {
            view.focus_handle(cx).focus(window, cx);
            window.press("tab", cx);
            window.press("enter", cx);
            window.press("down", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            session.read_with(cx, |s, cx| {
                !s.threads()
                    .read(cx)
                    .thread(&"t0".into())
                    .unwrap()
                    .is_unread()
            })
        });
        cx.update_window(handle, |_, window, cx| {
            view.focus_handle(cx).focus(window, cx);
            window.press("tab", cx);
            window.press("enter", cx);
            window.press("up", cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| !v.rows.contains(&"t0".into()))
        });
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "DELETE" && r.path().ends_with("/t0/following"))
        });
    }
}
