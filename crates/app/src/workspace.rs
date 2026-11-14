//! The signed-in window: sidebar, the open channel or the Threads list, and
//! beside it a thread or a search of the channel's messages.
//!
//! Recently opened channel views stay alive, so switching back is instant
//! and keeps scroll position, parsed Markdown, and the unsent draft.

use gpui_kit::component::button::Button;

use std::{collections::HashMap, ops::Range, rc::Rc, sync::Arc};

use gpui_kit::base::{ResizeHandleContext, ResizeHandleRenderer, ResizeHandleState};
use gpui_kit::component::{
    ActiveTheme as _, TitleBar, WindowExt as _, h_flex,
    kbd::Kbd,
    resizable::{ResizableState, h_resizable, resizable_panel},
    v_flex,
};
use gpui_kit::{
    Action, AnyElement, AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Task, Window, div, px,
};
use mattermost::{ChannelId, Client, Post, PostId, TeamId, User};
use session::{SearchScope, Session, SessionEvent, TimelineKind};

use crate::{
    actions::{
        CloseConversation, Dismiss, FocusComposer, LeaveChannel, NextChannel, NextUnreadChannel,
        PreviousChannel, PreviousUnreadChannel, QuickSwitch, RESET_ZOOM_KEY, SearchConversation,
        SearchTeam, ShowSettings, ShowShortcuts, ShowThreads, ToggleChannelMute, ToggleFavorite,
        WORKSPACE_CONTEXT, ZOOM_IN_KEY, ZOOM_OUT_KEY, shortcut_label,
    },
    cache,
    dither::DitherView,
    extensions, persist,
    search::{SearchEvent, SearchView},
    sidebar::{SidebarEvent, SidebarView, sections},
    skeleton, switcher,
    threads_view::{ThreadsView, ThreadsViewEvent},
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
/// The Threads list, beside the open thread.
const THREAD_LIST_WIDTH: f32 = 26.;
const THREAD_LIST_RANGE: Range<f32> = 20.5..40.;
/// The narrowest the open channel gets beside a thread or search.
const CHANNEL_MIN_WIDTH: f32 = 20.;
/// How far the sidebar gives way when the window can't fit it at its own
/// minimum beside the channel: large zoom in a small window.
const SIDEBAR_SQUEEZED_WIDTH: f32 = 12.;

pub enum WorkspaceEvent {
    SignedOut,
}

/// How the panes beside the sidebar are arranged. A change lays them out
/// afresh, since a pane at the same position may now be another kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// The channel, with its thread beside it at a fixed width.
    Channel,
    /// The Threads list at a fixed width, and the open thread beside it.
    ThreadsSplit,
    /// The Threads list or, in its place, the open thread: the window is
    /// too narrow for both.
    ThreadsSingle,
}

/// The pane preference a drag on a pane's edge changes.
#[derive(Clone, Copy)]
enum Drag {
    Sidebar,
    Thread,
    ThreadList,
}

/// One pane of the resizable group, as this frame lays it out.
struct Pane {
    content: AnyElement,
    /// A fixed pane keeps this width as the window resizes; the others
    /// share what is left.
    width: Option<Pixels>,
    range: Range<Pixels>,
    drag: Option<Drag>,
}

pub struct Workspace {
    session: Entity<Session>,
    sidebar: Entity<SidebarView>,
    empty: Entity<crate::dither::DitherView>,
    /// Open channel views, least recently used first.
    views: Vec<(ChannelId, Entity<TimelineView>, Subscription)>,
    active: Option<ChannelId>,
    thread: Option<(Entity<TimelineView>, Subscription)>,
    /// The message search, kept while hidden so its results are
    /// still there when it comes back.
    search: Option<(Entity<SearchView>, Subscription)>,
    /// Whether the search shows. A thread opened over it covers it until
    /// the thread closes.
    search_open: bool,
    /// The Threads list, made the first time it is shown.
    threads: Option<(Entity<ThreadsView>, Subscription)>,
    /// The Threads pane is shown in the channel's place.
    showing_threads: bool,
    /// Beside the Threads list before a thread is open.
    no_thread: Entity<DitherView>,
    drafts: HashMap<TimelineKind, String>,
    recent: Vec<ChannelId>,
    focus_handle: FocusHandle,
    /// Pane widths, and the rem size, sidebar cap, and arrangement they
    /// were laid out at.
    panes: Entity<ResizableState>,
    panes_basis: (Pixels, bool, Layout),
    /// The thread width the user dragged to, kept for the session.
    thread_width_rem: Option<f32>,
    /// The Threads list width the user dragged to, kept for the session.
    thread_list_width_rem: Option<f32>,
    /// Pane widths, as laid out, when the mouse last went down on the panes:
    /// a drag is told apart by which pane it changed.
    drag_origin: Vec<Pixels>,
    /// The window is too narrow for both, so the thread or search is shown
    /// in the place of the channel or the Threads list.
    side_replaces_content: bool,
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
        let (directory, emoji, server) = {
            let session = session.read(cx);
            (
                session.directory().clone(),
                session.emoji_store().clone(),
                session.client().server().to_string(),
            )
        };
        let remembered = [
            cache::remember_users(&directory, &server, cx),
            cache::remember_emoji(&emoji, &server, cx),
        ];
        let mut subscriptions = vec![
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| match event {
                SidebarEvent::OpenChannel(id) => this.open_channel(id, window, cx),
                SidebarEvent::OpenThreads => this.show_threads(&ShowThreads, window, cx),
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
                match (&this.thread, &this.search, this.side_replaces_content) {
                    (Some((thread, _)), _, true) => thread.focus_handle(cx).focus(window, cx),
                    (None, Some((search, _)), true) if this.search_open => {
                        search.update(cx, |search, cx| search.focus(window, cx))
                    }
                    _ => this.focus_active(window, cx),
                }
            }),
        ];
        subscriptions.extend(remembered.into_iter().flatten());
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            session,
            sidebar,
            empty: cx.new(|_| crate::dither::DitherView::new().conversation_hint()),
            views: Vec::new(),
            active: None,
            thread: None,
            search: None,
            search_open: false,
            threads: None,
            showing_threads: false,
            no_thread: cx.new(|_| DitherView::new().thread_hint()),
            drafts: HashMap::new(),
            recent: Vec::new(),
            focus_handle,
            panes: cx.new(|_| ResizableState::default()),
            panes_basis: (Pixels::ZERO, false, Layout::Channel),
            thread_width_rem: None,
            thread_list_width_rem: None,
            drag_origin: Vec::new(),
            side_replaces_content: false,
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
        if self.search.as_ref().is_some_and(|(search, _)| {
            search.read(cx).team() != session_ref.team().map(|team| &team.id)
        }) {
            self.search = None;
            self.search_open = false;
        }
        if !session_ref.is_loaded() {
            return;
        }
        if self.showing_threads {
            // Another team's thread leaves with its channel.
            let gone = self.thread.as_ref().is_some_and(|(thread, _)| {
                session_ref
                    .channel(thread.read(cx).kind().channel())
                    .is_none()
            });
            if gone {
                self.close_thread(window, cx);
            }
        } else if self
            .active
            .as_ref()
            .is_some_and(|id| session_ref.channel(id).is_some())
        {
            return;
        }
        let session_ref = session.read(cx);
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
        // The Threads pane stays up; there is no channel to recover.
        if self.showing_threads {
            return;
        }
        if let Some(id) = initial {
            self.open_channel(&id, window, cx);
        } else if self.active.is_some() {
            self.close_conversation(&CloseConversation, window, cx);
        }
    }

    pub fn open_channel(&mut self, id: &ChannelId, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_threads(cx);
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

        // A conversation search follows its channel; a team search stays open.
        if self
            .search
            .as_ref()
            .is_some_and(|(search, _)| matches!(search.read(cx).scope(), SearchScope::Conversation(channel) if channel != id))
        {
            self.search = None;
            self.search_open = false;
        }
        if let Some((search, _)) = &self.search {
            search.update(cx, |search, _| search.set_conversation(id.clone()));
        }
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

    fn toggle_channel_mute(
        &mut self,
        _: &ToggleChannelMute,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.active.clone() {
            self.session.update(cx, |s, cx| {
                if let Some(channel) = s.channel(&active) {
                    s.set_channel_muted(&active, !channel.is_muted(), cx);
                }
            });
        }
    }

    fn leave_channel(&mut self, _: &LeaveChannel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            crate::sidebar::confirm_leave(&self.session, active, window, cx);
        }
    }

    fn close_conversation(
        &mut self,
        _: &CloseConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search = None;
        self.search_open = false;
        self.close_thread(window, cx);
        self.leave_threads(cx);
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

    /// Show the threads the user follows in the channel's place. A thread
    /// already open stays open beside the list.
    fn show_threads(&mut self, _: &ShowThreads, window: &mut Window, cx: &mut Context<Self>) {
        let view = match &self.threads {
            Some((view, _)) => view.clone(),
            None => {
                let session = self.session.clone();
                let view = cx.new(|cx| ThreadsView::new(session, window, cx));
                let subscription = cx.subscribe_in(&view, window, Self::on_threads_view_event);
                self.threads = Some((view.clone(), subscription));
                view
            }
        };
        if !self.showing_threads {
            self.showing_threads = true;
            view.update(cx, |view, cx| view.shown(cx));
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.set_threads_selected(true, cx));
            // The channel is no longer in view, so its new posts are unread.
            if self.active.take().is_some() {
                self.session.update(cx, |s, cx| s.close_channel(cx));
            }
            // The channel's search goes with it, keeping its results for ⌘F.
            self.search_open = false;
            if let Some((thread, _)) = &self.thread {
                thread.update(cx, |thread, cx| thread.set_back_to_results(false, cx));
            }
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn leave_threads(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.showing_threads) {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.set_threads_selected(false, cx));
            cx.notify();
        }
    }

    fn on_threads_view_event(
        &mut self,
        _: &Entity<ThreadsView>,
        event: &ThreadsViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ThreadsViewEvent::OpenThread { channel, root } => {
                self.open_thread(channel.clone(), root.clone(), window, cx)
            }
            ThreadsViewEvent::OpenInChannel { channel, root } => {
                self.open_channel(channel, window, cx);
                self.open_thread(channel.clone(), root.clone(), window, cx);
            }
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
        self.session
            .update(cx, |s, cx| s.set_open_thread(kind.root().cloned(), cx));
        let session = self.session.clone();
        let view = cx.new(|cx| TimelineView::new(kind, session, draft, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_timeline_view_event);
        let back_to_results = self.search_open;
        view.update(cx, |view, cx| view.set_back_to_results(back_to_results, cx));
        view.focus_handle(cx).focus(window, cx);
        self.thread = Some((view, subscription));
        cx.notify();
    }

    /// Close the thread, uncovering the search it was opened over, if any.
    fn close_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((thread, _)) = self.thread.take() {
            self.save_draft(&thread, cx);
            self.session.update(cx, |s, cx| s.set_open_thread(None, cx));
            match &self.search {
                _ if self.showing_threads => {
                    if let Some((list, _)) = &self.threads {
                        list.update(cx, |list, cx| list.focus_selected(window, cx));
                    }
                }
                Some((search, _)) if self.search_open => {
                    search.update(cx, |search, cx| search.focus(window, cx))
                }
                _ => self.focus_active(window, cx),
            }
            cx.notify();
        }
    }

    /// Show the search of the open channel, keeping its last results.
    fn search_conversation(
        &mut self,
        _: &SearchConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.clone() else {
            return;
        };
        self.open_search(SearchScope::Conversation(active), window, cx);
    }

    fn search_team(&mut self, _: &SearchTeam, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.read(cx).team().is_some() {
            self.open_search(SearchScope::Team, window, cx);
        }
    }

    fn open_search(&mut self, scope: SearchScope, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_threads(cx);
        let search = match &self.search {
            Some((search, _)) => {
                search.update(cx, |search, cx| search.set_scope(scope, window, cx));
                search.clone()
            }
            None => {
                let session = self.session.clone();
                let search = cx.new(|cx| SearchView::new(session, scope, window, cx));
                let subscription = cx.subscribe_in(&search, window, Self::on_search_event);
                self.search = Some((search.clone(), subscription));
                search
            }
        };
        // The search takes the thread's place.
        self.search_open = true;
        self.close_thread(window, cx);
        search.update(cx, |search, cx| search.focus(window, cx));
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        _: &Entity<SearchView>,
        event: &SearchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchEvent::Open(post) => self.show_result(post, window, cx),
            SearchEvent::Close => self.close_search(window, cx),
        }
    }

    /// Show a search result where it was written: a reply in its thread,
    /// over the results, and any other post in the channel beside them.
    fn show_result(&mut self, post: &Post, window: &mut Window, cx: &mut Context<Self>) {
        let channel = post.channel_id.clone();
        if self.session.read(cx).channel(&channel).is_none() {
            return;
        }
        if self.active.as_ref() != Some(&channel) {
            self.open_channel(&channel, window, cx);
        }
        let target = if post.is_reply() {
            self.open_thread(channel, post.root_id.clone(), window, cx);
            self.thread.as_ref().map(|(thread, _)| thread.clone())
        } else {
            // A narrow window can't show the results beside the channel.
            if self.side_replaces_content {
                self.close_search(window, cx);
            }
            self.active_view().cloned()
        };
        if let Some(view) = target {
            let (id, created) = (post.id.clone(), post.create_at);
            view.update(cx, |view, cx| view.reveal(id, created, cx));
        }
        cx.notify();
    }

    fn save_draft(&mut self, view: &Entity<TimelineView>, cx: &mut Context<Self>) {
        let view = view.read(cx);
        // Closing or evicting the view cancels its debounce. Capture the
        // composer directly (including programmatic changes) and queue now.
        view.stage_draft(cx);
        persist::save_staged();
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

    /// Focus the visible conversation, revealing the channel if search covers it.
    /// In the Threads pane, focus its open thread or else the list.
    fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.showing_threads && self.side_replaces_content {
            if let Some((thread, _)) = &self.thread {
                thread.focus_handle(cx).focus(window, cx);
                return;
            }
            if self.search_open {
                self.search_open = false;
                cx.notify();
            }
        }
        let focus = if self.showing_threads {
            match (&self.thread, &self.threads) {
                (Some((thread, _)), _) => Some(thread.focus_handle(cx)),
                (None, Some((list, _))) => Some(list.focus_handle(cx)),
                (None, None) => None,
            }
        } else {
            self.active_view().map(|view| view.focus_handle(cx))
        };
        if let Some(focus) = focus {
            focus.focus(window, cx);
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
        if self.search_open {
            self.close_search(window, cx);
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
            let rows: [(&str, Box<dyn Action>); 12] = [
                ("Jump to a channel or person", Box::new(QuickSwitch)),
                (
                    "Search messages in the conversation",
                    Box::new(SearchConversation),
                ),
                ("Search all channels in this team", Box::new(SearchTeam)),
                ("Threads", Box::new(ShowThreads)),
                ("Next channel", Box::new(NextChannel)),
                ("Previous channel", Box::new(PreviousChannel)),
                ("Next unread channel", Box::new(NextUnreadChannel)),
                ("Previous unread channel", Box::new(PreviousUnreadChannel)),
                ("Focus the message field", Box::new(FocusComposer)),
                ("Close conversation", Box::new(CloseConversation)),
                ("Close panel or mark channel read", Box::new(Dismiss)),
                ("Settings", Box::new(ShowSettings)),
            ];
            let theme = cx.theme();
            let zoom = [ZOOM_IN_KEY, ZOOM_OUT_KEY, RESET_ZOOM_KEY].map(shortcut_label);
            let fixed = [
                ("Select a thread", "↑ / ↓ / Home / End".to_string()),
                ("Open selected thread", "Enter".to_string()),
                ("Thread actions", "Tab to More actions, Enter".to_string()),
                ("Send", "Enter".to_string()),
                ("New line", "Shift-Enter".to_string()),
                ("Edit your last message (empty field)", "Up".to_string()),
                ("Select a slash suggestion", "Up / Down".to_string()),
                ("Dismiss suggestions or cancel edit", "Escape".to_string()),
                ("Complete the selected slash command", "Tab".to_string()),
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
        let title = if self.showing_threads {
            "Threads".into()
        } else {
            self.active
                .as_ref()
                .and_then(|id| session.channel(id))
                .map(|e| session.channel_label(e, cx))
                .unwrap_or_else(|| "Conversations".into())
        };
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
        let (background, border, radius, backdrop) = {
            let theme = cx.theme();
            // Every content surface is an inset card on one tinted backdrop,
            // with the same gutter around and between them as the sidebar's.
            (
                theme.background,
                theme.border,
                theme.radius_lg,
                theme.title_bar,
            )
        };
        let panel = move |child: AnyElement| {
            div()
                .size_full()
                .min_w_0()
                .py_2()
                .pr_2()
                .child(
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
                .into_any_element()
        };
        let sidebar_range = (rem * SIDEBAR_RANGE.start).min(sidebar_fit)
            ..(rem * SIDEBAR_RANGE.end).min(sidebar_fit);
        let sidebar_width = preferred_sidebar.min(sidebar_fit);
        let thread = self.thread.as_ref().map(|(t, _)| t.clone());
        // What the window leaves beside the sidebar and a usable channel or
        // thread.
        let room = viewport - sidebar_width - channel_min;
        let thread_list = self
            .threads
            .as_ref()
            .filter(|_| self.showing_threads)
            .map(|(list, _)| list.clone());
        let content_pane = |content: AnyElement| Pane {
            content: panel(content),
            width: None,
            range: channel_min..Pixels::MAX,
            drag: None,
        };

        // Only the window and the sidebar decide whether a thread replaces
        // what it opens beside, never the thread's current width: a test on
        // that width changes the width it tests, so the layout would flip
        // back and forth at the switch point.
        let (layout, panes) = if let Some(list) = thread_list {
            // The list keeps its width and the thread takes the rest, unless
            // the window can't fit the list at its minimum beside a usable
            // thread. Then one shows at a time.
            let list_range = rem * THREAD_LIST_RANGE.start..rem * THREAD_LIST_RANGE.end;
            self.side_replaces_content = thread.is_some() && room < list_range.start;
            if room < list_range.start {
                let content = match thread {
                    Some(thread) => thread.into_any_element(),
                    None => list.into_any_element(),
                };
                (Layout::ThreadsSingle, vec![content_pane(content)])
            } else {
                let preferred = (rem * self.thread_list_width_rem.unwrap_or(THREAD_LIST_WIDTH))
                    .clamp(list_range.start, list_range.end);
                let detail = match thread {
                    Some(thread) => thread.into_any_element(),
                    None => self.no_thread.clone().into_any_element(),
                };
                let list = Pane {
                    content: panel(list.into_any_element()),
                    width: Some(preferred.min(room)),
                    range: list_range.start..list_range.end.min(room),
                    drag: Some(Drag::ThreadList),
                };
                (Layout::ThreadsSplit, vec![list, content_pane(detail)])
            }
        } else {
            // A thread or search opens beside the channel at its default
            // width, narrowed to what the window can spare, and replaces the
            // channel when the window can no longer fit it beside a usable
            // channel.
            let thread_range = rem * THREAD_RANGE.start..rem * THREAD_RANGE.end;
            let preferred = (rem * self.thread_width_rem.unwrap_or(THREAD_WIDTH))
                .clamp(thread_range.start, thread_range.end);
            let thread_width = room.clamp(thread_range.start, preferred);
            // A thread dragged wide gives way when the window narrows, down to
            // its minimum, rather than pushing the channel under its own.
            let thread_fit = thread_range.start..thread_range.end.min(room).max(thread_range.start);
            let side: Option<AnyView> = match (thread, &self.search) {
                (Some(thread), _) => Some(thread.into()),
                (None, Some((search, _))) if self.search_open => Some(search.clone().into()),
                _ => None,
            };
            self.side_replaces_content = side.is_some() && room < thread_range.start;
            let (content, side) = match side {
                Some(side) if self.side_replaces_content => (Some(side), None),
                side => (self.active_view().map(|view| view.clone().into()), side),
            };
            let content = match content {
                Some(view) => view.into_any_element(),
                // A conversation is about to reopen: show its shape, not the
                // empty state.
                None if matches!(
                    self.session.read(cx).channels_state(),
                    session::LoadState::Failed(_)
                ) =>
                {
                    crate::recovery::inline_failure(
                        "Couldn’t load conversations. Check your connection and try again.",
                        Button::new("retry-workspace-conversations").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.session.update(cx, |s, cx| s.retry_channels(cx));
                            },
                        )),
                        cx,
                    )
                    .into_any_element()
                }
                None if matches!(
                    self.session.read(cx).channels_state(),
                    session::LoadState::NotLoaded | session::LoadState::Loading
                ) && persist::read_with(|s| s.last_channel.is_some()) =>
                {
                    skeleton::conversation(cx).into_any_element()
                }
                None => self.empty.clone().into_any_element(),
            };
            let mut panes = vec![content_pane(content)];
            panes.extend(side.map(|side| Pane {
                content: panel(side.into_any_element()),
                width: Some(thread_width),
                range: thread_fit,
                drag: Some(Drag::Thread),
            }));
            (Layout::Channel, panes)
        };

        // Pane widths are pixels laid out from these preferences. After a
        // zoom change, when the window starts or stops capping the sidebar,
        // or when the panes are rearranged, lay them out afresh so they
        // return to the preferences.
        if (rem, sidebar_capped, layout) != self.panes_basis {
            self.panes_basis = (rem, sidebar_capped, layout);
            self.panes = cx.new(|_| ResizableState::default());
        }
        let sidebar = Pane {
            content: self.sidebar.clone().into_any_element(),
            width: Some(sidebar_width),
            range: sidebar_range,
            drag: Some(Drag::Sidebar),
        };
        let panes: Vec<Pane> = std::iter::once(sidebar).chain(panes).collect();
        let ranges: Vec<Range<Pixels>> = panes.iter().map(|pane| pane.range.clone()).collect();
        let drags: Vec<Option<Drag>> = panes.iter().map(|pane| pane.drag).collect();
        let workspace = cx.entity().downgrade();
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
        let group = h_resizable("workspace-panes")
            .with_handle_appearance(pane_divider())
            .with_state(&self.panes)
            .on_resize(move |state, _, cx| {
                let sizes = laid_out(state.read(cx).sizes());
                let _ = workspace.update(cx, |this, _| {
                    let origin = std::mem::take(&mut this.drag_origin);
                    // A fixed pane changes only for a drag on its own divider.
                    // Widths are kept in rems so a zoom change doesn't undo
                    // them.
                    for (ix, drag) in drags.iter().enumerate() {
                        let (Some(drag), Some(&now), Some(&was)) =
                            (drag, sizes.get(ix), origin.get(ix))
                        else {
                            continue;
                        };
                        if (now - was).abs() <= px(0.5) {
                            continue;
                        }
                        match drag {
                            Drag::Sidebar => {
                                persist::update(|s| s.sidebar_width_rem = Some(now / rem))
                            }
                            Drag::Thread => this.thread_width_rem = Some(now / rem),
                            Drag::ThreadList => this.thread_list_width_rem = Some(now / rem),
                        }
                    }
                });
            });
        let group = panes.into_iter().fold(group, |group, pane| {
            let panel = resizable_panel().size_range(pane.range);
            let panel = match pane.width {
                Some(width) => panel.size(width).fixed(),
                None => panel,
            };
            group.child(panel.child(pane.content))
        });

        v_flex()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(backdrop)
            .on_action(cx.listener(Self::quick_switch))
            .on_action(cx.listener(Self::show_threads))
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
            .on_action(cx.listener(Self::toggle_channel_mute))
            .on_action(cx.listener(Self::search_conversation))
            .on_action(cx.listener(Self::search_team))
            .on_action(cx.listener(Self::leave_channel))
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
                    .child(group),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, Bounds, TestAppContext, WindowBounds, WindowOptions, size};
    use mattermost::testing::{Network, Reply, Request, SlowServer};
    use serde_json::json;

    use super::*;

    /// A team with one channel and one followed thread in it, with two
    /// unread replies, one of them a mention.
    fn route(request: &Request) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        let thread = json!({
            "id": "root", "reply_count": 2, "last_reply_at": 30,
            "unread_replies": 2, "unread_mentions": 1,
            "participants": [{ "id": "ann" }],
            "post": { "id": "root", "channel_id": "town", "user_id": "ann",
                      "message": "Release **notes** are up", "create_at": 10 },
        });
        match path {
            "/api/v4/users/me/teams" => {
                Reply::json(json!([{ "id": "team", "name": "team", "display_name": "Team" }]))
            }
            "/api/v4/users/me/teams/team/channels" => Reply::json(json!([
                { "id": "town", "team_id": "team", "type": "O", "name": "town-square",
                  "display_name": "Town Square" },
            ])),
            "/api/v4/users/me/teams/team/channels/members" => {
                Reply::json(json!([{ "channel_id": "town", "user_id": "me" }]))
            }
            "/api/v4/users/me/teams/team/threads" if request.path().contains("totalsOnly") => {
                Reply::json(json!({ "total_unread_threads": 1, "total_unread_mentions": 1 }))
            }
            "/api/v4/users/me/teams/team/threads" => Reply::json(json!({
                "total": 1, "total_unread_threads": 1, "total_unread_mentions": 1,
                "threads": [thread],
            })),
            "/api/v4/posts/root/thread" => Reply::json(json!({
                "order": ["root", "reply"],
                "posts": {
                    "root": { "id": "root", "channel_id": "town", "user_id": "ann",
                              "message": "Release notes are up", "create_at": 10 },
                    "reply": { "id": "reply", "channel_id": "town", "user_id": "ann",
                               "root_id": "root", "message": "@me have a look", "create_at": 30 },
                },
            })),
            "/api/v4/users/ids" => Reply::json(json!([{ "id": "ann", "username": "ann" }])),
            "/api/v4/users/status/ids" => {
                Reply::json(json!([{ "user_id": "ann", "status": "online" }]))
            }
            "/api/v4/users/me/preferences/favorite_channel" | "/api/v4/emoji" => {
                Reply::json(json!([]))
            }
            _ if path.starts_with("/api/v4/users/me/teams/team/threads/root/read/") => {
                Reply::json(json!({}))
            }
            _ => Reply::not_found(),
        }
    }

    fn open_workspace(
        server: &SlowServer,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        open_workspace_as(server, "me", cx)
    }

    fn open_workspace_as(
        server: &SlowServer,
        user: &str,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            update::init(cx);
            crate::actions::init(cx);
            extensions::init(cx);
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = user.into();
        let workspace = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (handle, _) = cx.update(|cx| {
            let workspace = workspace.clone();
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1200.), px(800.)),
                    ))),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    let view = cx.new(|cx| {
                        Workspace::new(client, me, Some(TeamId::from("team")), window, cx)
                    });
                    *workspace.borrow_mut() = Some(view.clone());
                    view
                },
            )
            .expect("open test window")
        });
        let workspace = workspace.borrow_mut().take().unwrap();
        (handle, workspace)
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

    #[gpui_kit::test]
    fn text_drafts_survive_restart_and_clear_on_send_and_signout(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.method() == "POST" && request.path() == "/api/v4/posts" {
                Reply::json(
                    json!({"id":"sent", "channel_id":"town", "user_id":"me", "message":"channel draft", "create_at":50}),
                )
            } else {
                route(request)
            }
        });
        persist::update(|state| {
            let mut me = User::default();
            me.id = "me".into();
            state.session = Some(persist::SavedSession {
                server: Client::new_unverified(&server.url(), "token")
                    .unwrap()
                    .server()
                    .to_string(),
                token: "token".into(),
                user: Some(me),
                ..Default::default()
            });
        });
        let (handle, workspace) = open_workspace(&server, cx);
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_channel(&"town".into(), window, cx));
            window.render_frame(cx);
            window.input("channel draft", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_thread("town".into(), "root".into(), window, cx)
            });
            window.render_frame(cx);
            window.input("thread draft", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // Closing before the 500 ms debounce must queue the edit itself:
        // inspecting the writer does not save staged changes as flush would.
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.close_thread(window, cx));
        })
        .unwrap();
        let kind = TimelineKind::Thread {
            channel: "town".into(),
            root: "root".into(),
        };
        let written = persist::written_for_test();
        let owner_server = written.session.as_ref().unwrap().server.clone();
        assert_eq!(
            written.drafts.get(&owner_server, "me", &kind).as_deref(),
            Some("thread draft")
        );

        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_thread("town".into(), "root".into(), window, cx)
            });
            window.render_frame(cx);
            window.press("secondary-a", cx);
            window.input("/shr", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.press("tab", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // set_value used by slash completion doesn't emit InputEvent::Change.
        assert_eq!(
            persist::load()
                .drafts
                .get(&owner_server, "me", &kind)
                .as_deref(),
            Some("/shrug ")
        );
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.close_thread(window, cx));
        })
        .unwrap();
        let written = persist::written_for_test();
        assert_eq!(
            written.drafts.get(&owner_server, "me", &kind).as_deref(),
            Some("/shrug ")
        );
        // Restart from the queued snapshot, without a quit/flush rescuing it.
        let snapshot = persist::round_trip_for_test(&written);
        persist::update(|state| *state = snapshot);
        let (second, restored) = open_workspace(&server, cx);
        cx.update_window(second, |_, window, cx| {
            restored.update(cx, |w, cx| {
                w.open_channel(&"town".into(), window, cx);
                assert_eq!(
                    w.active_view().unwrap().read(cx).draft(cx).as_deref(),
                    Some("channel draft")
                );
                w.open_thread("town".into(), "root".into(), window, cx);
                assert_eq!(
                    w.thread.as_ref().unwrap().0.read(cx).draft(cx).as_deref(),
                    Some("/shrug ")
                );
            });
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(second, |_, window, cx| {
            restored.update(cx, |w, cx| w.close_thread(window, cx));
        })
        .unwrap();
        let sent = persist::written_for_test();
        assert!(
            sent.drafts.get(&owner_server, "me", &kind).is_none(),
            "closing before the debounce must queue the send-clear"
        );
        // Leave another draft for the sign-out assertion below.
        cx.update_window(second, |_, window, cx| {
            restored.update(cx, |w, cx| {
                w.open_thread("town".into(), "root".into(), window, cx)
            });
            window.render_frame(cx);
            window.input("discard on sign-out", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(second, |_, window, cx| {
            restored.update(cx, |w, cx| w.close_thread(window, cx));
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |_| {
            server.requests().iter().any(|r| {
                r.method() == "POST"
                    && r.path() == "/api/v4/posts"
                    && serde_json::from_slice::<serde_json::Value>(r.body()).unwrap()["message"]
                        == "channel draft"
            })
        });
        let channel = TimelineKind::Channel("town".into());
        let saved = persist::load();
        let owner = saved.session.as_ref().unwrap();
        assert!(saved.drafts.get(&owner.server, "me", &channel).is_none());
        crate::app_root::end_session();
        let signed_out = persist::load();
        assert!(
            signed_out
                .drafts
                .get(
                    &owner.server,
                    "me",
                    &TimelineKind::Thread {
                        channel: "town".into(),
                        root: "root".into()
                    }
                )
                .is_none()
        );
        // A stale view cannot recreate a draft after sign-out.
        persist::stage_draft(
            &owner.server,
            "me",
            saved.draft_generation,
            &channel,
            "stale",
        );
        assert!(
            persist::load()
                .drafts
                .get(&owner.server, "me", &channel)
                .is_none()
        );
        persist::update(|state| {
            let mut other = owner.clone();
            other.user.as_mut().unwrap().id = "other".into();
            state.session = Some(other);
        });
        let (third, other) = open_workspace_as(&server, "other", cx);
        cx.update_window(third, |_, window, cx| {
            other.update(cx, |w, cx| {
                w.open_channel(&"town".into(), window, cx);
                w.open_thread("town".into(), "root".into(), window, cx);
                assert!(w.active_view().unwrap().read(cx).draft(cx).is_none());
                assert!(w.thread.as_ref().unwrap().0.read(cx).draft(cx).is_none());
            });
        })
        .unwrap();
        persist::update(|state| state.session = None);
    }

    #[gpui_kit::test]
    fn team_search_shortcut_opens_other_channels_and_their_threads(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("GET", "/api/v4/users/me/teams") => Reply::json(json!([
                    { "id": "team", "name": "team" }, { "id": "next", "name": "next" }
                ])),
                ("GET", "/api/v4/users/me/teams/next/channels")
                | ("GET", "/api/v4/users/me/teams/next/channels/members") => Reply::json(json!([])),
                ("POST", "/api/v4/teams/team/posts/search") => Reply::json(json!({
                    "has_next": true, "order": ["remote", "reply"], "posts": {
                        "remote": { "id": "remote", "channel_id": "other", "user_id": "ann",
                                    "message": "hello", "create_at": 40 },
                        "reply": { "id": "reply", "channel_id": "other", "user_id": "ann",
                                   "root_id": "root", "message": "hello reply", "create_at": 30 }
                    }
                })),
                ("GET", "/api/v4/channels/other") => Reply::json(json!({
                    "id": "other", "team_id": "team", "type": "O", "name": "other", "display_name": "Other"
                })),
                ("GET", "/api/v4/channels/other/members/me") => Reply::json(json!({
                    "channel_id": "other", "user_id": "me"
                })),
                ("GET", "/api/v4/channels/other/posts") => Reply::json(json!({
                    "order": ["remote"], "posts": {"remote": {
                        "id": "remote", "channel_id": "other", "user_id": "ann",
                        "message": "hello", "create_at": 40
                    }}
                })),
                _ => route(request),
            }
        });
        let (handle, workspace) = open_workspace(&server, cx);
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| {
                w.session.read(cx).team().is_some() && w.session.read(cx).is_loaded()
            })
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_channel(&"town".into(), window, cx));
            window.render_frame(cx);
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-shift-f"
                } else {
                    "ctrl-shift-f"
                },
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
        let search = workspace.read_with(cx, |w, _| w.search.as_ref().unwrap().0.clone());
        search.read_with(cx, |s, _| assert_eq!(s.scope(), &SearchScope::Team));
        cx.update_window(handle, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "GET" && r.path() == "/api/v4/channels/other")
        });
        // Wait for the result rows to arrive, then use the keyboard to open one.
        wait_until(cx, |cx| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("more-results").is_some()
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, _| w.active == Some("other".into()))
        });
        workspace.read_with(cx, |w, cx| {
            assert!(w.search_open);
            assert_eq!(w.search.as_ref().unwrap().0, search);
            assert_eq!(
                w.session
                    .read(cx)
                    .channel(&"other".into())
                    .unwrap()
                    .channel()
                    .display_name,
                "Other"
            );
        });
        cx.update_window(handle, |_, window, cx| {
            search.update(cx, |s, cx| s.focus(window, cx));
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| workspace.read_with(cx, |w, _| w.thread.is_some()));
        workspace.read_with(cx, |w, cx| {
            let thread = &w.thread.as_ref().unwrap().0;
            assert_eq!(thread.read(cx).kind().channel(), &ChannelId::from("other"));
            assert!(w.search_open);
        });
        let session = workspace.read_with(cx, |w, _| w.session.clone());
        session.update(cx, |s, cx| s.select_team("next".into(), cx));
        wait_until(cx, |cx| workspace.read_with(cx, |w, _| w.search.is_none()));
        workspace.read_with(cx, |w, _| assert!(!w.search_open));
    }

    #[gpui_kit::test]
    fn an_unavailable_search_result_does_not_change_the_conversation(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, workspace) = open_workspace(&server, cx);
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| w.session.read(cx).is_loaded())
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_channel(&"town".into(), window, cx);
                for root in ["", "root"] {
                    let mut post = Post::default();
                    post.id = "unavailable".into();
                    post.channel_id = "gone".into();
                    post.root_id = root.into();
                    w.show_result(&post, window, cx);
                    assert_eq!(w.active, Some("town".into()));
                    assert!(w.thread.is_none());
                }
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn desktop_preferences_only_suppress_baseline_notifications(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (request.method(), request.path()) {
                ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                    {"id":"town","team_id":"team","name":"town-square","type":"O"},
                    {"id":"gm","type":"G","display_name":"Group"}
                ])),
                ("GET", "/api/v4/users/me/teams/team/channels/members") => Reply::json(json!([
                    {"channel_id":"town","user_id":"me","notify_props":{"desktop":"all"}},
                    {"channel_id":"gm","user_id":"me","notify_props":{"desktop":"mention"}}
                ])),
                _ => route(request),
            }
        });
        let (handle, workspace) = open_workspace(&server, cx);
        let session = workspace.read_with(cx, |w, _| w.session.clone());
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            session.update(cx, |s, cx| s.close_channel(cx));
            session.read(cx).directory().clone().update(cx, |d, cx| {
                d.set_status("me".into(), mattermost::Status::Online, cx)
            });
            for (ix, (channel, root, mentioned, visible, expected)) in [
                ("town", "", false, false, false), // all never expands the baseline
                ("town", "root", false, false, false),
                ("town", "root", true, false, true),
                ("town", "root", true, true, false),
                ("gm", "", true, false, false), // implicit GM mention
            ]
            .into_iter()
            .enumerate()
            {
                session.update(cx, |s, cx| {
                    s.set_open_thread(visible.then(|| "root".into()), cx)
                });
                let mut post = Post::default();
                post.id = format!("notify-{ix}").into();
                post.channel_id = channel.into();
                post.root_id = root.into();
                post.user_id = "ann".into();
                post.message = "ordinary message".into();
                let before = window.notifications(cx).len();
                extensions::dispatch_session_event(
                    &SessionEvent::Posted {
                        post: std::sync::Arc::new(post),
                        mentions_me: mentioned,
                    },
                    &session,
                    window,
                    cx,
                );
                assert_eq!(
                    window.notifications(cx).len(),
                    before + usize::from(expected)
                );
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn do_not_disturb_silences_mentions_and_direct_messages(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.method() == "GET" && request.path() == "/api/v4/users/me/teams/team/channels"
            {
                Reply::json(json!([
                    { "id": "town", "team_id": "team", "type": "O", "name": "town-square" },
                    { "id": "dm", "type": "D", "name": "me__ann" }
                ]))
            } else {
                route(request)
            }
        });
        let (handle, workspace) = open_workspace(&server, cx);
        let session = workspace.read_with(cx, |w, _| w.session.clone());
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.channel(&"dm".into()).is_some())
        });
        cx.update(|cx| {
            extensions::init(cx);
            cx.set_app_identity("com.mtc.test", "mtc");
        });
        cx.update_window(handle, |_, window, cx| {
            session.update(cx, |s, cx| s.close_channel(cx));
            let directory = session.read(cx).directory().clone();
            directory.update(cx, |d, cx| {
                d.set_status("me".into(), mattermost::Status::DoNotDisturb, cx);
                d.set_status("ann".into(), mattermost::Status::Online, cx);
            });
            for status in [mattermost::Status::DoNotDisturb, mattermost::Status::Online] {
                directory.update(cx, |d, cx| {
                    d.set_status("me".into(), status, cx);
                    d.set_status("ann".into(), mattermost::Status::DoNotDisturb, cx);
                });
                let before = window.notifications(cx).len();
                for (channel, mentions_me) in [("town", true), ("dm", false)] {
                    let mut post = Post::default();
                    post.id = format!("{channel}-{}", status.as_str()).into();
                    post.channel_id = channel.into();
                    post.user_id = "ann".into();
                    post.message = "hello".into();
                    extensions::dispatch_session_event(
                        &SessionEvent::Posted {
                            post: std::sync::Arc::new(post),
                            mentions_me,
                        },
                        &session,
                        window,
                        cx,
                    );
                }
                let added = if status == mattermost::Status::DoNotDisturb {
                    0
                } else {
                    2
                };
                assert_eq!(window.notifications(cx).len(), before + added);
            }
        })
        .unwrap();
        assert_eq!(cx.shown_system_notifications().len(), 2);
    }

    #[gpui_kit::test]
    fn the_threads_pane_lists_followed_threads_and_opens_one_beside_the_list(
        cx: &mut TestAppContext,
    ) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, workspace) = open_workspace(&server, cx);
        let session = workspace.read_with(cx, |w, _| w.session.clone());
        let threads = session.read_with(cx, |s, _| s.threads().clone());
        // The sidebar counts unread threads before the pane is ever shown.
        wait_until(cx, |cx| {
            threads.read_with(cx, |t, _| t.unread_mentions() == 1)
        });

        cx.update_window(handle, |_, window, cx| {
            window.dispatch_action(Box::new(ShowThreads), cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("thread-root").is_some()
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.click("thread-root", cx);
        })
        .unwrap();

        // The thread opens beside the list and is marked read.
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "PUT" && r.path().contains("/threads/root/read/"))
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, cx| {
            assert!(w.showing_threads);
            let (thread, _) = w.thread.as_ref().expect("a thread is open");
            assert_eq!(thread.read(cx).kind().root(), Some(&PostId::from("root")));
        });
        session.read_with(cx, |s, _| {
            assert_eq!(s.open_thread(), Some(&PostId::from("root")))
        });
        threads.read_with(cx, |t, _| {
            assert_eq!((t.unread_threads(), t.unread_mentions()), (0, 0));
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let row = window.find("thread-root").bounds();
            let thread = window.find("close-thread").bounds();
            assert!(thread.left() > row.right(), "{row:?} {thread:?}");
        })
        .unwrap();

        cx.update_window(handle, |_, window, cx| window.click("close-thread", cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("thread-root").focused(), Some(true));
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();

        // A channel takes the pane's place; the thread belongs to it, so it stays.
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_channel(&"town".into(), window, cx));
        })
        .unwrap();
        workspace.read_with(cx, |w, _| {
            assert!(!w.showing_threads);
            assert!(w.thread.is_some());
        });
    }
    #[gpui_kit::test]
    fn composer_focus_follows_the_visible_pane(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, workspace) = open_workspace(&server, cx);
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| w.session.read(cx).is_loaded())
        });
        cx.simulate_window_resize(handle, size(px(650.), px(800.)));
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_channel(&"town".into(), window, cx);
                w.open_thread("town".into(), "root".into(), window, cx);
            });
            window.render_frame(cx);
            window.press("secondary-l", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            let w = workspace.read(cx);
            assert!(w.side_replaces_content);
            assert!(
                w.thread
                    .as_ref()
                    .unwrap()
                    .0
                    .focus_handle(cx)
                    .is_focused(window)
            );
            assert!(!w.active_view().unwrap().focus_handle(cx).is_focused(window));
            window.input("/", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .thread
                    .as_ref()
                    .unwrap()
                    .0
                    .read(cx)
                    .draft(cx),
                Some("/".into())
            );
            window.render_frame(cx);
            assert!(
                window.try_find("command-me").is_some(),
                "suggestions must be visible"
            );
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        workspace.read_with(cx, |w, cx| {
            let thread = &w
                .thread
                .as_ref()
                .expect("Escape only dismisses suggestions")
                .0;
            assert_eq!(thread.read(cx).draft(cx), Some("/".into()));
        });

        cx.simulate_window_resize(handle, size(px(1400.), px(800.)));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("secondary-l", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert!(
                workspace
                    .read(cx)
                    .active_view()
                    .unwrap()
                    .focus_handle(cx)
                    .is_focused(window)
            );
        })
        .unwrap();

        cx.simulate_window_resize(handle, size(px(650.), px(800.)));
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.search_conversation(&SearchConversation, window, cx)
            });
            window.render_frame(cx);
            window.press("secondary-l", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let w = workspace.read(cx);
            assert!(!w.search_open);
            assert!(w.active_view().unwrap().focus_handle(cx).is_focused(window));
            workspace.update(cx, |w, cx| {
                w.open_thread("town".into(), "root".into(), window, cx)
            });
            window.render_frame(cx);
            window.click("close-thread", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let w = workspace.read(cx);
            assert!(w.thread.is_none());
            assert!(w.active_view().unwrap().focus_handle(cx).is_focused(window));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_switcher_finds_read_conversations_beyond_the_sidebar_limit(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.path() == "/api/v4/users/me/teams/team/channels" {
                let channels: Vec<_> = (0..25).map(|ix| json!({
                    "id": format!("dm-{ix}"), "type": "D", "name": format!("me__person-{ix}"),
                    "display_name": format!("Conversation {ix}"), "last_post_at": ix,
                })).collect();
                Reply::json(json!(channels))
            } else {
                route(request)
            }
        });
        let (handle, workspace) = open_workspace(&server, cx);
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| w.session.read(cx).channels().count() == 25)
        });
        workspace.read_with(cx, |w, cx| {
            let session = w.session.read(cx);
            assert!(
                !crate::sidebar::sections(session, cx)
                    .direct
                    .contains(&"dm-0".into())
            );
            let all = crate::sidebar::conversation_sections(session, cx);
            assert_eq!(all.direct.len(), 25);
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.quick_switch(&QuickSwitch, window, cx));
            window.render_frame(cx);
            window.input("me__person-0", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.press("enter", cx))
            .unwrap();
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| assert_eq!(w.active, Some("dm-0".into())));
    }

    #[gpui_kit::test]
    fn sidebar_and_command_palette_mute_the_channel(cx: &mut TestAppContext) {
        use gpui_kit::test::{TestEventExt as _, TestQueryExt as _};
        let muted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), {
            let muted = muted.clone();
            move |request| {
                if request.method() == "PUT"
                    && request.path() == "/api/v4/channels/town/members/me/notify_props"
                {
                    let props: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    muted.store(
                        props["mark_unread"] == "mention",
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    return Reply::json(
                        json!({ "channel_id":"town", "user_id":"me", "notify_props":props }),
                    );
                }
                if request.method() == "GET" && request.path() == "/api/v4/channels/town/members/me"
                {
                    return Reply::json(
                        json!({"channel_id":"town","user_id":"me","notify_props":{"mark_unread":if muted.load(std::sync::atomic::Ordering::SeqCst) {"mention"} else {"all"}}}),
                    );
                }
                route(request)
            }
        });
        let (handle, workspace) = open_workspace(&server, cx);
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| w.session.read(cx).is_loaded())
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_channel(&"town".into(), window, cx));
            window.render_frame(cx);
            window.right_click("channel-town", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let at = window.find_by_label("Mute channel").bounds().center();
            window.pointer_down(at, gpui_kit::MouseButton::Left, cx);
            window.pointer_up(at, gpui_kit::MouseButton::Left, cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| {
                let s = w.session.read(cx);
                s.channel(&"town".into()).unwrap().is_muted() && !s.is_mute_pending(&"town".into())
            })
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("channel-town").label(),
                Some("Town Square, muted")
            );
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-k"
                } else {
                    "ctrl-k"
                },
                cx,
            );
            window.input("Unmute channel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.find_by_label("Unmute channel");
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            workspace.read_with(cx, |w, cx| {
                let s = w.session.read(cx);
                !s.channel(&"town".into()).unwrap().is_muted() && !s.is_mute_pending(&"town".into())
            })
        });
        let props: Vec<serde_json::Value> = server
            .requests()
            .iter()
            .filter(|r| {
                r.method() == "PUT" && r.path() == "/api/v4/channels/town/members/me/notify_props"
            })
            .map(|r| serde_json::from_slice(r.body()).unwrap())
            .collect();
        assert_eq!(
            props,
            vec![
                json!({"mark_unread":"mention"}),
                json!({"mark_unread":"all"})
            ]
        );
    }
}
