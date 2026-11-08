//! The signed-in window: sidebar, the open channel, and an optional thread.
//!
//! Recently opened channel views stay alive, so switching back is instant
//! and keeps scroll position, parsed Markdown, and the unsent draft.

use std::{collections::HashMap, ops::Range, rc::Rc, sync::Arc};

use gpui_kit::base::{ResizeHandleContext, ResizeHandleRenderer, ResizeHandleState};
use gpui_kit::component::{
    ActiveTheme as _, TitleBar, WindowExt as _, h_flex,
    kbd::Kbd,
    resizable::{ResizableState, h_resizable, resizable_panel},
    v_flex,
};
use gpui_kit::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use mattermost::{ChannelId, Client, PostId, TeamId, User};
use session::{Session, SessionEvent, TimelineKind};

use crate::{
    actions::{
        CloseConversation, Dismiss, FocusComposer, NextChannel, NextUnreadChannel, PreviousChannel,
        PreviousUnreadChannel, QuickSwitch, RESET_ZOOM_KEY, ShowSettings, ShowShortcuts,
        ToggleFavorite, WORKSPACE_CONTEXT, ZOOM_IN_KEY, ZOOM_OUT_KEY, shortcut_label,
    },
    extensions, persist,
    sidebar::{SidebarEvent, SidebarView, sections},
    skeleton, switcher,
    timeline_view::{TimelineView, TimelineViewEvent},
    update,
};

/// Channel views kept alive for instant switching.
const VIEW_CACHE: usize = 8;
const RECENT_LIMIT: usize = 20;

/// Pane widths in rems, so they follow the interface zoom. At the default
/// 14px base font these are within a few pixels of the earlier fixed widths.
const SIDEBAR_WIDTH: f32 = 21.;
const SIDEBAR_RANGE: Range<f32> = 18.5..33.;
const THREAD_WIDTH: f32 = 28.5;
const THREAD_RANGE: Range<f32> = 21.5..51.;
/// The narrowest the open channel gets beside a thread.
const CHANNEL_MIN_WIDTH: f32 = 20.;
/// How far the sidebar gives way when the window can't fit it at its own
/// minimum beside the channel: large zoom in a small window.
const SIDEBAR_SQUEEZED_WIDTH: f32 = 12.;

pub enum WorkspaceEvent {
    SignedOut,
}

pub struct Workspace {
    session: Entity<Session>,
    sidebar: Entity<SidebarView>,
    empty: Entity<crate::dither::DitherView>,
    /// Open channel views, least recently used first.
    views: Vec<(ChannelId, Entity<TimelineView>, Subscription)>,
    active: Option<ChannelId>,
    thread: Option<(Entity<TimelineView>, Subscription)>,
    drafts: HashMap<TimelineKind, String>,
    recent: Vec<ChannelId>,
    focus_handle: FocusHandle,
    /// Pane widths, and the rem size and sidebar cap they were laid out at.
    panes: Entity<ResizableState>,
    panes_basis: (Pixels, bool),
    /// The thread width the user dragged to, kept for the session.
    thread_width_rem: Option<f32>,
    /// Pane widths, as laid out, when the mouse last went down on the panes:
    /// a drag is told apart by which pane it changed.
    drag_origin: Vec<Pixels>,
    /// The window is too narrow for both, so the thread is shown in the
    /// channel's place.
    thread_replaces_channel: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(
        client: Client,
        me: User,
        team: Option<TeamId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = cx.new(|cx| Session::new(client, me, team, cx));
        // The conversation open last time reopens once the channel list
        // confirms it still exists. Fetch its posts now, alongside that
        // list, so they are there when it does.
        let last_channel = persist::read_with(|state| state.last_channel.clone());
        if let Some(id) = &last_channel {
            session.update(cx, |s, cx| {
                s.timeline(TimelineKind::Channel(ChannelId::from(id.as_str())), cx);
            });
        }
        let sidebar = cx.new(|cx| SidebarView::new(session.clone(), cx));
        let subscriptions = vec![
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| match event {
                SidebarEvent::OpenChannel(id) => this.open_channel(id, window, cx),
            }),
            cx.subscribe_in(&session, window, |this, session, event, window, cx| {
                if let SessionEvent::SignedOut = event {
                    cx.emit(WorkspaceEvent::SignedOut);
                    return;
                }
                let _ = this;
                extensions::dispatch_session_event(event, session, window, cx);
            }),
            cx.observe_in(&session, window, Self::on_session_changed),
            // The title bar offers a restart once an update is installed.
            cx.observe(&update::updater(cx), |_, _, cx| cx.notify()),
            // A focused control can leave the tree: a row scrolled out of the
            // virtual list, or the channel replaced by its thread in a narrow
            // window. Typing should still land in the visible conversation.
            cx.on_focus_lost(window, |this, window, cx| {
                match (&this.thread, this.thread_replaces_channel) {
                    (Some((thread, _)), true) => thread.focus_handle(cx).focus(window, cx),
                    _ => this.focus_active(window, cx),
                }
            }),
        ];
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            session,
            sidebar,
            empty: cx.new(|_| crate::dither::DitherView::new().conversation_hint()),
            views: Vec::new(),
            active: None,
            thread: None,
            drafts: HashMap::new(),
            recent: Vec::new(),
            focus_handle,
            panes: cx.new(|_| ResizableState::default()),
            panes_basis: (Pixels::ZERO, false),
            thread_width_rem: None,
            drag_origin: Vec::new(),
            thread_replaces_channel: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn client(&self, cx: &gpui_kit::App) -> Client {
        self.session.read(cx).client().clone()
    }

    /// Confirm a restored session with the server; see [`Session::refresh_me`].
    pub fn refresh_me(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<Arc<User>>> {
        self.session.update(cx, |s, cx| s.refresh_me(cx))
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, cx| s.set_window_active(active, cx));
    }

    /// Pick a channel once the channel list arrives, and recover when the
    /// open channel disappears (team switch, removed from channel).
    fn on_session_changed(
        &mut self,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_ref = session.read(cx);
        if !session_ref.is_loaded() {
            return;
        }
        let still_exists = self
            .active
            .as_ref()
            .is_some_and(|id| session_ref.channel(id).is_some());
        if still_exists {
            return;
        }
        let initial = persist::read_with(|state| state.last_channel.clone())
            .map(ChannelId::from)
            .filter(|id| session_ref.channel(id).is_some());
        if let Some(team) = session_ref.team() {
            let team = team.id.to_string();
            let changed = persist::read_with(|state| {
                state
                    .session
                    .as_ref()
                    .is_some_and(|s| s.team_id.as_deref() != Some(team.as_str()))
            });
            if changed {
                persist::update(|state| {
                    if let Some(saved) = state.session.as_mut() {
                        saved.team_id = Some(team);
                    }
                });
            }
        }
        if let Some(id) = initial {
            self.open_channel(&id, window, cx);
        } else if self.active.is_some() {
            self.close_conversation(&CloseConversation, window, cx);
        }
    }

    pub fn open_channel(&mut self, id: &ChannelId, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_ref() == Some(id) {
            self.focus_active(window, cx);
            return;
        }
        let view = match self.views.iter().position(|(c, ..)| c == id) {
            Some(ix) => {
                let entry = self.views.remove(ix);
                let view = entry.1.clone();
                self.views.push(entry);
                view
            }
            None => {
                let kind = TimelineKind::Channel(id.clone());
                let draft = self.drafts.remove(&kind);
                let session = self.session.clone();
                let view = cx.new(|cx| TimelineView::new(kind, session, draft, window, cx));
                let subscription = cx.subscribe_in(&view, window, Self::on_timeline_view_event);
                self.views.push((id.clone(), view.clone(), subscription));
                view
            }
        };
        while self.views.len() > VIEW_CACHE {
            let (_, evicted, _) = self.views.remove(0);
            self.save_draft(&evicted, cx);
        }

        self.session.update(cx, |s, cx| {
            s.open_channel(id, cx);
        });
        self.active = Some(id.clone());
        self.recent.retain(|r| r != id);
        self.recent.insert(0, id.clone());
        self.recent.truncate(RECENT_LIMIT);
        persist::update(|state| state.last_channel = Some(id.to_string()));

        // A thread belongs to its channel.
        if let Some((thread, _)) = &self.thread
            && thread.read(cx).kind().channel() != id
        {
            self.close_thread(window, cx);
        }
        view.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = self.active.clone() {
            self.session.update(cx, |s, cx| {
                let favorite = s.is_favorite(&active);
                s.set_favorite(&active, !favorite, cx);
            });
        }
    }

    fn close_conversation(
        &mut self,
        _: &CloseConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_thread(window, cx);
        self.active = None;
        persist::update(|state| state.last_channel = None);
        self.session.update(cx, |s, cx| s.close_channel(cx));
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn on_timeline_view_event(
        &mut self,
        view: &Entity<TimelineView>,
        event: &TimelineViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TimelineViewEvent::OpenThread(root) => {
                let channel = view.read(cx).kind().channel().clone();
                self.open_thread(channel, root.clone(), window, cx);
            }
            TimelineViewEvent::OpenChannel(id) => self.open_channel(id, window, cx),
            TimelineViewEvent::Close => self.close_thread(window, cx),
        }
    }

    fn open_thread(
        &mut self,
        channel: ChannelId,
        root: PostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = TimelineKind::Thread { channel, root };
        if let Some((thread, _)) = &self.thread {
            if thread.read(cx).kind() == &kind {
                thread.focus_handle(cx).focus(window, cx);
                return;
            }
            let thread = thread.clone();
            self.save_draft(&thread, cx);
        }
        let draft = self.drafts.remove(&kind);
        let session = self.session.clone();
        let view = cx.new(|cx| TimelineView::new(kind, session, draft, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_timeline_view_event);
        view.focus_handle(cx).focus(window, cx);
        self.thread = Some((view, subscription));
        cx.notify();
    }

    fn close_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((thread, _)) = self.thread.take() {
            self.save_draft(&thread, cx);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn save_draft(&mut self, view: &Entity<TimelineView>, cx: &mut Context<Self>) {
        let view = view.read(cx);
        match view.draft(cx) {
            Some(draft) => self.drafts.insert(view.kind().clone(), draft),
            None => self.drafts.remove(view.kind()),
        };
    }

    fn active_view(&self) -> Option<&Entity<TimelineView>> {
        let active = self.active.as_ref()?;
        self.views
            .iter()
            .find(|(c, ..)| c == active)
            .map(|(_, v, _)| v)
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_view() {
            view.focus_handle(cx).focus(window, cx);
        }
    }

    // ----- Actions ----------------------------------------------------------

    fn quick_switch(&mut self, _: &QuickSwitch, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let recent = self.recent.clone();
        switcher::open(workspace, &self.session, &recent, window, cx);
    }

    fn step_channel(
        &mut self,
        delta: isize,
        unread_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.read(cx);
        let order: Vec<ChannelId> = sections(session, cx).ordered().cloned().collect();
        if order.is_empty() {
            return;
        }
        let len = order.len() as isize;
        let start = self
            .active
            .as_ref()
            .and_then(|a| order.iter().position(|c| c == a))
            .map_or(-1, |ix| ix as isize);
        let target = (1..=len)
            .map(|step| (start + delta * step).rem_euclid(len) as usize)
            .find(|&ix| {
                !unread_only
                    || session
                        .channel(&order[ix])
                        .is_some_and(|e| (e.is_unread() && !e.is_muted()) || e.mention_count() > 0)
            });
        if let Some(ix) = target {
            let id = order[ix].clone();
            self.open_channel(&id, window, cx);
        }
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread.is_some() {
            self.close_thread(window, cx);
            return;
        }
        if let Some(active) = self.active.clone() {
            self.session.update(cx, |s, cx| {
                s.mark_viewed(&active, cx);
                let timeline = s.timeline(TimelineKind::Channel(active), cx);
                timeline.update(cx, |t, cx| t.set_unread_after(None, cx));
            });
        }
    }

    fn show_shortcuts(&mut self, _: &ShowShortcuts, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, window, cx| {
            let rows: [(&str, Box<dyn Action>); 9] = [
                ("Jump to a channel or person", Box::new(QuickSwitch)),
                ("Next channel", Box::new(NextChannel)),
                ("Previous channel", Box::new(PreviousChannel)),
                ("Next unread channel", Box::new(NextUnreadChannel)),
                ("Previous unread channel", Box::new(PreviousUnreadChannel)),
                ("Focus the message field", Box::new(FocusComposer)),
                ("Close conversation", Box::new(CloseConversation)),
                ("Close thread or mark channel read", Box::new(Dismiss)),
                ("Settings", Box::new(ShowSettings)),
            ];
            let theme = cx.theme();
            let zoom = [ZOOM_IN_KEY, ZOOM_OUT_KEY, RESET_ZOOM_KEY].map(shortcut_label);
            let fixed = [
                ("Send", "Enter".to_string()),
                ("New line", "Shift-Enter".to_string()),
                ("Edit your last message", "Up".to_string()),
                ("Complete a slash command", "Tab".to_string()),
                ("Zoom in, out, actual size", zoom.join(" ")),
            ];
            // Dialog accepts pixels and clamps this rem-derived width to the viewport.
            dialog
                .title("Keyboard shortcuts")
                .w(window.rem_size() * 34.)
                .child(
                    v_flex()
                        .gap_2()
                        .text_sm()
                        .children(rows.into_iter().map(|(label, action)| {
                            h_flex().justify_between().child(label).children(
                                Kbd::binding_for_action(&*action, Some(WORKSPACE_CONTEXT), window),
                            )
                        }))
                        .children(fixed.into_iter().map(|(label, keys)| {
                            h_flex()
                                .justify_between()
                                .child(label)
                                .child(div().text_color(theme.muted_foreground).child(keys))
                        })),
                )
        });
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let session = self.session.read(cx);
        let title = self
            .active
            .as_ref()
            .and_then(|id| session.channel(id))
            .map(|e| session.channel_label(e, cx))
            .unwrap_or_else(|| "Conversations".into());
        TitleBar::new()
            .child(
                h_flex()
                    .flex_1()
                    .justify_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("mtc  /  {title}")),
            )
            .children(update::title_bar_button(cx).map(|button| div().pr_2().child(button)))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The dividers between the inset panels. The gutter already separates them,
/// so a divider draws nothing at rest (the stock hairline would run down the
/// next panel's edge and across the gutters) and shows a grab pill, centred
/// in the 8px gutter left of the boundary, while the pointer is on it.
fn pane_divider() -> ResizeHandleRenderer {
    Rc::new(
        |handle: &ResizeHandleContext, _: &mut Window, cx: &mut App| {
            let (length, opacity) = match handle.state() {
                ResizeHandleState::Idle => (px(0.), 0.),
                ResizeHandleState::Hovered => (px(20.), 0.35),
                ResizeHandleState::Pressed => (px(28.), 0.6),
                ResizeHandleState::Dragging => (px(44.), 0.9),
            };
            let pill = (length > px(0.)).then(|| {
                div()
                    .flex_none()
                    .w(px(3.))
                    .h(length)
                    // From the boundary to the gutter's centre, less half the pill.
                    .ml(px(-4. - 1.5))
                    .rounded_full()
                    .bg(cx.theme().muted_foreground)
                    .opacity(opacity)
            });
            Some(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .w(px(1.))
                    .h_full()
                    .children(pill)
                    .into_any_element(),
            )
        },
    )
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        let viewport = window.viewport_size().width;
        let channel_min = rem * CHANNEL_MIN_WIDTH;
        let (sidebar_width_rem, sidebar_width_px) =
            persist::read_with(|state| (state.sidebar_width_rem, state.sidebar_width));
        let preferred_sidebar = sidebar_width_rem
            .map(|width| rem * width)
            .or(sidebar_width_px.map(px))
            .unwrap_or(rem * SIDEBAR_WIDTH)
            .clamp(rem * SIDEBAR_RANGE.start, rem * SIDEBAR_RANGE.end);
        // What the window leaves the sidebar beside the channel. Below the
        // sidebar's own minimum, it holds that width and can't be dragged.
        let sidebar_fit = (viewport - channel_min).max(rem * SIDEBAR_SQUEEZED_WIDTH);
        let sidebar_capped = sidebar_fit < preferred_sidebar;
        // Pane widths are pixels laid out from these preferences. After a
        // zoom change, or when the window starts or stops capping the
        // sidebar, lay them out afresh so they return to the preferences.
        if (rem, sidebar_capped) != self.panes_basis {
            self.panes_basis = (rem, sidebar_capped);
            self.panes = cx.new(|_| ResizableState::default());
        }
        let theme = cx.theme();
        let (background, border, radius) = (theme.background, theme.border, theme.radius_lg);
        // Every content surface is an inset card on one tinted backdrop, with
        // the same gutter around and between them as the sidebar's.
        let backdrop = theme.title_bar;
        let panel = move |child: AnyElement| {
            div().size_full().min_w_0().py_2().pr_2().child(
                div()
                    .size_full()
                    .min_w_0()
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .overflow_hidden()
                    .child(child),
            )
        };
        let sidebar_range = (rem * SIDEBAR_RANGE.start).min(sidebar_fit)
            ..(rem * SIDEBAR_RANGE.end).min(sidebar_fit);
        let sidebar_width = preferred_sidebar.min(sidebar_fit);
        let thread_range = rem * THREAD_RANGE.start..rem * THREAD_RANGE.end;

        // A thread opens beside the channel at its default width, narrowed
        // to what the window can spare, and replaces the channel when the
        // window can no longer fit it beside a usable channel.
        let room = viewport - sidebar_width - channel_min;
        let preferred = (rem * self.thread_width_rem.unwrap_or(THREAD_WIDTH))
            .clamp(thread_range.start, thread_range.end);
        let thread_width = room.clamp(thread_range.start, preferred);
        // A thread dragged wide gives way when the window narrows, down to
        // its minimum, rather than pushing the channel under its own.
        let thread_fit = thread_range.start..thread_range.end.min(room).max(thread_range.start);
        let thread = self.thread.as_ref().map(|(t, _)| t.clone());
        // Only the window and the sidebar decide this, never the thread's
        // current width: a test on that width changes the width it tests,
        // so the layout would flip back and forth at the switch point.
        self.thread_replaces_channel = thread.is_some() && room < thread_range.start;
        let (content, side_thread) = match thread {
            Some(thread) if self.thread_replaces_channel => (Some(thread), None),
            thread => (self.active_view().cloned(), thread),
        };
        let workspace = cx.entity().downgrade();
        let ranges = [
            sidebar_range.clone(),
            channel_min..Pixels::MAX,
            thread_fit.clone(),
        ];
        let laid_out = move |sizes: &[Pixels]| -> Vec<Pixels> {
            // The group keeps the width a pane was dragged to even where the
            // pane's range no longer allows it, and lays it out clamped.
            sizes
                .iter()
                .zip(&ranges)
                .map(|(size, range)| (*size).clamp(range.start, range.end))
                .collect()
        };
        let drag_start = laid_out.clone();

        v_flex()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(backdrop)
            .on_action(cx.listener(Self::quick_switch))
            .on_action(cx.listener(|this, _: &NextChannel, window, cx| {
                this.step_channel(1, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousChannel, window, cx| {
                this.step_channel(-1, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextUnreadChannel, window, cx| {
                this.step_channel(1, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousUnreadChannel, window, cx| {
                this.step_channel(-1, true, window, cx)
            }))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::close_conversation))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(
                cx.listener(|this, _: &FocusComposer, window, cx| this.focus_active(window, cx)),
            )
            .on_action(cx.listener(Self::show_shortcuts))
            .child(self.render_title_bar(cx))
            .child(
                // Pane sizes are layout geometry owned by the resizable group.
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(backdrop)
                    .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
                        this.drag_origin = drag_start(this.panes.read(cx).sizes());
                    }))
                    .child(
                        h_resizable("workspace-panes")
                            .with_handle_appearance(pane_divider())
                            .with_state(&self.panes)
                            .on_resize(move |state, _, cx| {
                                let sizes = laid_out(state.read(cx).sizes());
                                let _ = workspace.update(cx, |this, _| {
                                    let origin = std::mem::take(&mut this.drag_origin);
                                    let moved = |ix: usize| match (sizes.get(ix), origin.get(ix)) {
                                        (Some(&now), Some(&was)) if (now - was).abs() > px(0.5) => {
                                            Some(now)
                                        }
                                        _ => None,
                                    };
                                    // A fixed pane changes only for a drag on its
                                    // own divider: the sidebar's, or the thread's.
                                    // Widths are kept in rems so a zoom change
                                    // doesn't undo them.
                                    if let Some(sidebar) = moved(0) {
                                        persist::update(|s| {
                                            s.sidebar_width_rem = Some(sidebar / rem)
                                        });
                                    }
                                    if let Some(thread) = moved(2) {
                                        this.thread_width_rem = Some(thread / rem);
                                    }
                                });
                            })
                            .child(
                                resizable_panel()
                                    .size(sidebar_width)
                                    .size_range(sidebar_range)
                                    .fixed()
                                    .child(self.sidebar.clone()),
                            )
                            .child(
                                resizable_panel()
                                    .size_range(channel_min..Pixels::MAX)
                                    .child(panel(match content {
                                        Some(view) => view.into_any_element(),
                                        // A conversation is about to reopen:
                                        // show its shape, not the empty state.
                                        None if !self.session.read(cx).is_loaded()
                                            && persist::read_with(|s| s.last_channel.is_some()) =>
                                        {
                                            skeleton::conversation(cx).into_any_element()
                                        }
                                        None => self.empty.clone().into_any_element(),
                                    })),
                            )
                            .when_some(side_thread, |this, thread| {
                                this.child(
                                    resizable_panel()
                                        .size(thread_width)
                                        .size_range(thread_fit)
                                        .fixed()
                                        .child(panel(thread.into_any_element())),
                                )
                            }),
                    ),
            )
    }
}
