//! ⌘K: jump to any channel or person, or run an app command, by typing.

use std::{cell::Cell, collections::HashSet, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Sizable as _, WindowExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    Action, App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window, div,
};
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _};

gpui_kit::actions!(switcher, [Dismiss]);

use mattermost::{ApiError, Channel, ChannelId, ChannelType, TeamId, User};
use session::Session;

use crate::{actions, workspace::Workspace};

enum Target {
    Channel(ChannelId),
    Action(Box<dyn Action>),
    Mode(Mode),
    Person(User, u64),
    Join(Channel, u64),
    More(u64),
}

struct Group {
    label: &'static str,
    items: Vec<(CommandItem, Target)>,
}

fn channel_item(session: &Session, id: &ChannelId, cx: &App) -> Option<(CommandItem, Target)> {
    let entry = session.channel(id)?;
    let label = session.channel_label(entry, cx);
    let icon = match entry.kind() {
        ChannelType::Open => IconName::Hash,
        ChannelType::Private => IconName::Lock,
        ChannelType::Direct => IconName::AtSign,
        ChannelType::Group => IconName::Users,
    };
    let channel = entry.channel();
    let mut keywords = vec![SharedString::from(channel.name.clone())];
    // A group's server name lists its members' usernames.
    if entry.kind() == ChannelType::Group {
        keywords.push(channel.display_name.clone().into());
    }
    // Initials let "ts" find "Town Square".
    let initials: String = label
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| w.chars().next())
        .collect();
    if initials.len() > 1 {
        keywords.push(initials.into());
    }
    if let Some(partner) = channel.direct_partner(&session.me().id)
        && let Some(user) = session.directory().read(cx).user(&partner)
    {
        keywords.push(user.username.clone().into());
    }
    let item = CommandItem::new()
        .label(label)
        .icon(icon)
        .keywords(keywords);
    Some((item, Target::Channel(id.clone())))
}

fn action_item(label: &'static str, icon: IconName, action: impl Action) -> (CommandItem, Target) {
    let action: Box<dyn Action> = Box::new(action);
    let hint = action.boxed_clone();
    let item = CommandItem::new().label(label).child(move |window, cx| {
        h_flex()
            .w_full()
            .gap_2()
            .child(
                Icon::new(icon)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().flex_1().child(label))
            .children(Kbd::binding_for_action(
                &*hint,
                Some(actions::WORKSPACE_CONTEXT),
                window,
            ))
    });
    (item, Target::Action(action))
}

/// Open the switcher over `window`.
pub fn open(
    workspace: WeakEntity<Workspace>,
    session: &Entity<Session>,
    recent: &[ChannelId],
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let session_ref = session.read(cx);
    let sections = crate::sidebar::conversation_sections(session_ref, cx);
    let active = session_ref.active_channel().cloned();

    // The open conversation is left out of Recent but stays findable below.
    let mut seen = HashSet::new();
    let recent_ids: Vec<&ChannelId> = recent
        .iter()
        .filter(|id| Some(*id) != active.as_ref() && seen.insert(*id))
        .take(5)
        .collect();
    let recent_items: Vec<_> = recent_ids
        .iter()
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let in_recent = |id: &ChannelId| recent_ids.contains(&id);
    let favorite_items: Vec<_> = sections
        .favorites
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let channel_items: Vec<_> = sections
        .channels
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let direct_items: Vec<_> = sections
        .direct
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let mut commands = vec![
        (
            CommandItem::new()
                .label("New group message")
                .icon(IconName::Users),
            Target::Mode(Mode::Group),
        ),
        (
            CommandItem::new()
                .label("Browse channels")
                .icon(IconName::Hash),
            Target::Mode(Mode::Browse),
        ),
    ];
    if let Some(active) = &active {
        let favorite = session_ref.is_favorite(active);
        commands.push(action_item(
            crate::sidebar::favorite_label(favorite),
            if favorite {
                IconName::StarOff
            } else {
                IconName::Star
            },
            actions::ToggleFavorite,
        ));
        if !session_ref.is_mute_pending(active)
            && let Some(channel) = session_ref.channel(active)
        {
            let muted = channel.is_muted();
            commands.push(action_item(
                crate::sidebar::mute_label(muted),
                if muted {
                    IconName::Bell
                } else {
                    IconName::BellOff
                },
                actions::ToggleChannelMute,
            ));
        }
        if session_ref.channel(active).is_some_and(|e| e.can_leave()) {
            commands.push(action_item(
                "Leave channel",
                IconName::LogOut,
                actions::LeaveChannel,
            ));
        }
    }
    if active.is_some() {
        commands.push(action_item(
            "Search messages",
            IconName::Search,
            actions::SearchConversation,
        ));
    }
    commands.extend([
        action_item("Threads", IconName::MessagesSquare, actions::ShowThreads),
        action_item(
            "Close conversation",
            IconName::X,
            actions::CloseConversation,
        ),
        action_item(
            "Next unread channel",
            IconName::ArrowDown,
            actions::NextUnreadChannel,
        ),
        action_item("Zoom in", IconName::ZoomIn, actions::ZoomIn),
        action_item("Zoom out", IconName::ZoomOut, actions::ZoomOut),
        action_item("Actual size", IconName::Scan, actions::ResetZoom),
        action_item(
            "Light appearance",
            IconName::Sun,
            actions::UseLightAppearance,
        ),
        action_item(
            "Dark appearance",
            IconName::Moon,
            actions::UseDarkAppearance,
        ),
        action_item(
            "Match system appearance",
            IconName::SunMoon,
            actions::UseSystemAppearance,
        ),
        action_item("Settings", IconName::Settings, actions::ShowSettings),
        action_item(
            "Keyboard shortcuts",
            IconName::Keyboard,
            actions::ShowShortcuts,
        ),
    ]);
    commands.push(match crate::update::status(cx) {
        crate::update::UpdateStatus::Ready { .. } => action_item(
            "Restart to update",
            IconName::CircleArrowUp,
            actions::RestartToUpdate,
        ),
        _ => action_item(
            "Check for updates",
            IconName::RefreshCw,
            actions::CheckForUpdates,
        ),
    });
    commands.push(action_item(
        if crate::update::is_automatic() {
            "Turn off automatic updates"
        } else {
            "Turn on automatic updates"
        },
        IconName::Settings,
        actions::ToggleAutomaticUpdates,
    ));
    commands.push(action_item("Sign out", IconName::LogOut, actions::SignOut));

    let groups: Vec<Group> = [
        Group {
            label: "Recent",
            items: recent_items,
        },
        Group {
            label: "Favorites",
            items: favorite_items,
        },
        Group {
            label: "Channels",
            items: channel_items,
        },
        Group {
            label: "Direct messages",
            items: direct_items,
        },
        Group {
            label: "Commands",
            items: commands,
        },
    ]
    .into_iter()
    .filter(|g| !g.items.is_empty())
    .collect();

    let palette = cx.new(|cx| Switcher::new(workspace, session.clone(), groups, window, cx));
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            palette.read(cx).state.focus_handle(cx).focus(window, cx);
        }
        let palette = palette.clone();
        let closing = palette.downgrade();
        dialog
            .close_button(false)
            // Let native buttons receive Enter rather than the dialog's
            // blanket confirm binding. Command owns Enter inside its list.
            .keyboard(false)
            .on_close(move |_, _, cx| {
                let _ = closing.update(cx, |this, _| this.cancel());
            })
            .p_2()
            .w(window.rem_size() * 44.)
            .content(move |content, _, _| content.child(palette.clone()))
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Jump,
    Group,
    Browse,
}

/// Retained for the lifetime of one palette. Search tasks hold only a weak
/// reference, so closing the dialog cancels pending work and navigation.
struct Switcher {
    workspace: WeakEntity<Workspace>,
    session: Entity<Session>,
    state: Entity<CommandState>,
    local: Vec<Group>,
    mode: Mode,
    team: Option<TeamId>,
    query: String,
    revision: u64,
    people: Vec<User>,
    channels: Vec<Channel>,
    selected: Vec<User>,
    next_page: Option<usize>,
    loaded_pages: usize,
    refresh_pending: bool,
    refresh_running: bool,
    restore_selection: Option<(u64, ChannelId)>,
    selection_revision: u64,
    loading: bool,
    busy: bool,
    closed: bool,
    error: Option<SharedString>,
    _search: Option<Task<()>>,
    _refresh: Option<Task<()>>,
    _operation: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Switcher {
    fn new(
        workspace: WeakEntity<Workspace>,
        session: Entity<Session>,
        local: Vec<Group>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        let team = session.read(cx).team().map(|team| team.id.clone());
        let subscriptions = vec![
            cx.subscribe_in(&session, window, |this, _, event, window, cx| {
                if matches!(event, session::SessionEvent::ChannelsChanged)
                    && this.mode == Mode::Browse
                {
                    this.invalidate_channels(window, cx);
                }
            }),
            cx.observe(&session, |this, session, cx| {
                let team = session.read(cx).team().map(|team| team.id.clone());
                if this.team != team {
                    this.team = team;
                    this.search(false, cx);
                }
                cx.notify();
            }),
        ];
        Self {
            workspace,
            session,
            state,
            local,
            mode: Mode::Jump,
            team,
            query: String::new(),
            revision: 0,
            people: Vec::new(),
            channels: Vec::new(),
            selected: Vec::new(),
            next_page: None,
            loaded_pages: 0,
            refresh_pending: false,
            refresh_running: false,
            restore_selection: None,
            selection_revision: 0,
            loading: false,
            busy: false,
            closed: false,
            error: None,
            _search: None,
            _refresh: None,
            _operation: None,
            _subscriptions: subscriptions,
        }
    }

    fn cancel(&mut self) {
        self.closed = true;
        self.revision += 1;
        self._search = None;
        self._refresh = None;
        self._operation = None;
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.busy {
            return;
        }
        self.mode = mode;
        self.selected.clear();
        self.query.clear();
        self.state
            .update(cx, |state, cx| state.set_query("", window, cx));
        self.search(false, cx);
        self.state.focus_handle(cx).focus(window, cx);
    }

    fn query_changed(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = query.trim().to_owned();
        if query == self.query {
            return;
        }
        self.query = query;
        self.search(true, cx);
    }

    fn search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.revision += 1;
        self._search = None;
        self._refresh = None;
        self.refresh_pending = false;
        self.refresh_running = false;
        self.restore_selection = None;
        self.loaded_pages = 0;
        self.people.clear();
        self.channels.clear();
        self.next_page = None;
        self.error = None;
        self.loading = self.mode == Mode::Browse || !self.query.is_empty();
        if !self.loading {
            cx.notify();
            return;
        }
        self.fetch(0, debounce, cx);
    }

    /// Coalesce a burst without cancelling typing or paging. A second burst
    /// during I/O schedules one follow-up; unrelated server events never get here.
    fn invalidate_channels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.mode != Mode::Browse {
            return;
        }
        self.refresh_pending = true;
        if self.refresh_running {
            return;
        }
        self.refresh_running = true;
        self._refresh = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let Ok(snapshot) = this.update(cx, |this, cx| {
                    if this.closed || this.mode != Mode::Browse {
                        return None;
                    }
                    // Query callbacks are deferred; also wait for a typed value
                    // that has not reached the search owner yet.
                    if this.loading
                        || this.busy
                        || this.state.read(cx).query(cx).trim() != this.query
                    {
                        return Some(None);
                    }
                    this.refresh_pending = false;
                    let pages = this.loaded_pages.max(1);
                    let tasks: Vec<_> = (0..pages)
                        .map(|page| {
                            this.session.update(cx, |session, cx| {
                                session.discover_channels(&this.query, page, cx)
                            })
                        })
                        .collect();
                    Some(Some((this.revision, pages, tasks)))
                }) else {
                    return;
                };
                let Some(snapshot) = snapshot else {
                    return;
                };
                let Some((revision, pages, tasks)) = snapshot else {
                    continue;
                };
                let mut result = futures::future::try_join_all(tasks).await;
                // In a non-exhaustive browse, an insertion can push a displayed
                // channel beyond the refreshed page boundary. Verify only those
                // missing identities before treating them as removed.
                if let Ok(results) = &mut result
                    && results.last().is_some_and(|(_, more)| *more)
                {
                    let checks = this
                        .update(cx, |this, cx| {
                            if !this.closed
                                && this.mode == Mode::Browse
                                && this.query.is_empty()
                                && revision == this.revision
                            {
                                let client = this.session.read(cx).client().clone();
                                this.channels
                                    .iter()
                                    .filter(|known| {
                                        !results.iter().any(|(channels, _)| {
                                            channels.iter().any(|found| found.id == known.id)
                                        })
                                    })
                                    .map(|known| {
                                        let client = client.clone();
                                        let known = known.clone();
                                        cx.background_spawn(async move {
                                            match client.channel(&known.id).await {
                                                Ok(channel) => Some(channel),
                                                Err(error)
                                                    if matches!(
                                                        error.downcast_ref::<ApiError>(),
                                                        Some(ApiError::Status(403 | 404))
                                                    ) =>
                                                {
                                                    None
                                                }
                                                Err(_) => Some(known),
                                            }
                                        })
                                    })
                                    .collect::<Vec<_>>()
                            } else {
                                Vec::new()
                            }
                        })
                        .unwrap_or_default();
                    let verified = futures::future::join_all(checks).await;
                    if let Some((channels, _)) = results.last_mut() {
                        channels.extend(verified.into_iter().flatten().filter(|channel| {
                            channel.kind == ChannelType::Open && channel.delete_at == 0
                        }));
                    }
                }
                let Ok(again) = this.update(cx, |this, cx| {
                    if this.closed || this.mode != Mode::Browse || revision != this.revision {
                        return false;
                    }
                    // Paging may have completed while these snapshots were in
                    // flight. Refresh the larger range rather than replacing it.
                    if this.loading
                        || this.busy
                        || this.loaded_pages > pages
                        || this.state.read(cx).query(cx).trim() != this.query
                    {
                        this.refresh_pending = true;
                        return true;
                    }
                    match result {
                        Ok(results) => {
                            let selected = this.state.read(cx).selected_index().and_then(|path| {
                                let group = this.remote_group(cx);
                                match group.items.get(path.row).map(|(_, target)| target) {
                                    Some(Target::Join(channel, _)) => Some(channel.id.clone()),
                                    _ => None,
                                }
                            });
                            let more = results.last().is_some_and(|(_, more)| *more);
                            let mut fresh: Vec<Channel> = results
                                .into_iter()
                                .flat_map(|(channels, _)| channels)
                                .collect();
                            // Preserve the existing order, updating by identity;
                            // new channels append rather than moving the highlight.
                            this.channels.retain_mut(|channel| {
                                if let Some(ix) =
                                    fresh.iter().position(|found| found.id == channel.id)
                                {
                                    *channel = fresh.remove(ix);
                                    true
                                } else {
                                    false
                                }
                            });
                            for channel in fresh {
                                if !this.channels.iter().any(|known| known.id == channel.id) {
                                    this.channels.push(channel);
                                }
                            }
                            this.loaded_pages = pages;
                            this.next_page = more.then_some(pages);
                            this.restore_selection = selected.map(|id| (revision, id));
                            this.error = None;
                        }
                        Err(error) => {
                            this.error = Some(discovery_error(&error, Mode::Browse, false))
                        }
                    }
                    let again = this.refresh_pending;
                    if !again {
                        this.refresh_running = false;
                    }
                    cx.notify();
                    again
                }) else {
                    return;
                };
                if !again {
                    return;
                }
            }
        }));
    }

    fn fetch(&mut self, page: usize, debounce: bool, cx: &mut Context<Self>) {
        let revision = self.revision;
        let query = self.query.clone();
        let mode = self.mode;
        self.loading = true;
        self.error = None;
        self._search = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                if mode == Mode::Browse {
                    let task = this
                        .session
                        .update(cx, |s, cx| s.discover_channels(&query, page, cx));
                    cx.spawn(async move |_, _| {
                        task.await
                            .map(|(channels, more)| Found::Channels(channels, more))
                    })
                } else {
                    let task = this.session.update(cx, |s, cx| s.search_people(&query, cx));
                    cx.spawn(async move |_, _| task.await.map(Found::People))
                }
            }) else {
                return;
            };
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.apply_results(revision, mode, &query, page, result, cx);
            });
        }));
        cx.notify();
    }

    fn apply_results(
        &mut self,
        revision: u64,
        mode: Mode,
        query: &str,
        page: usize,
        result: anyhow::Result<Found>,
        cx: &mut Context<Self>,
    ) {
        if self.closed || revision != self.revision || mode != self.mode || query != self.query {
            return;
        }
        self.loading = false;
        match result {
            Ok(Found::People(people)) => {
                let me = &self.session.read(cx).me().id;
                self.people = people.into_iter().filter(|user| &user.id != me).collect();
            }
            Ok(Found::Channels(channels, more)) => {
                for channel in channels {
                    if !self.channels.iter().any(|known| known.id == channel.id) {
                        self.channels.push(channel);
                    }
                }
                self.loaded_pages = page + 1;
                self.next_page = more.then_some(page + 1);
            }
            Err(err) => self.error = Some(discovery_error(&err, mode, false)),
        }
        cx.notify();
    }

    fn confirm(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.busy {
            return;
        }
        let task = match target {
            Target::Channel(id) => {
                let id = id.clone();
                self.cancel();
                window.close_dialog(cx);
                let _ = self
                    .workspace
                    .update(cx, |w, cx| w.open_channel(&id, window, cx));
                return;
            }
            Target::Action(action) => {
                let action = action.boxed_clone();
                self.cancel();
                window.close_dialog(cx);
                window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
                return;
            }
            Target::Mode(mode) => {
                self.set_mode(*mode, window, cx);
                return;
            }
            Target::More(revision) => {
                if *revision == self.revision
                    && !self.loading
                    && let Some(page) = self.next_page
                {
                    self.fetch(page, false, cx);
                }
                return;
            }
            Target::Person(user, revision) => {
                if *revision != self.revision || self.state.read(cx).query(cx).trim() != self.query
                {
                    return;
                }
                if self.mode == Mode::Group {
                    if let Some(ix) = self
                        .selected
                        .iter()
                        .position(|selected| selected.id == user.id)
                    {
                        self.selected.remove(ix);
                    } else if self.selected.len() < 7 {
                        self.selected.push(user.clone());
                    }
                    self.state
                        .update(cx, |state, cx| state.set_query("", window, cx));
                    self.query.clear();
                    self.search(false, cx);
                    return;
                }
                self.session
                    .update(cx, |s, cx| s.open_direct_channel(user.id.clone(), cx))
            }
            Target::Join(channel, revision) => {
                if *revision != self.revision || self.state.read(cx).query(cx).trim() != self.query
                {
                    return;
                }
                self.session
                    .update(cx, |s, cx| s.join_public_channel(channel.clone(), cx))
            }
        };
        self.open_result(task, window, cx);
    }

    fn open_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.busy || !(2..=7).contains(&self.selected.len()) {
            return;
        }
        let users = self.selected.iter().map(|user| user.id.clone()).collect();
        let task = self
            .session
            .update(cx, |s, cx| s.open_group_channel(users, cx));
        self.open_result(task, window, cx);
    }

    fn open_result(
        &mut self,
        task: Task<anyhow::Result<ChannelId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        self._operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.closed {
                    return;
                }
                this.busy = false;
                match result {
                    Ok(id) => {
                        this.closed = true;
                        this.revision += 1;
                        this._search = None;
                        window.close_dialog(cx);
                        let _ = this
                            .workspace
                            .update(cx, |w, cx| w.open_channel(&id, window, cx));
                    }
                    Err(err) => {
                        this.error = Some(discovery_error(&err, this.mode, true));
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    fn remote_group(&self, cx: &App) -> Group {
        let session = self.session.read(cx);
        let items = if self.mode == Mode::Browse {
            let mut items: Vec<_> = self
                .channels
                .iter()
                .filter(|channel| session.channel(&channel.id).is_none())
                .map(|channel| {
                    let name = if channel.display_name.is_empty() {
                        &channel.name
                    } else {
                        &channel.display_name
                    };
                    let label = format!("{name} · #{}", channel.name);
                    let purpose = channel.purpose.clone();
                    let name = label.clone();
                    (
                        CommandItem::new().label(label).child(move |_, cx| {
                            v_flex().flex_1().min_w_0().child(name.clone()).child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(purpose.clone()),
                            )
                        }),
                        Target::Join(channel.clone(), self.revision),
                    )
                })
                .collect();
            if self.next_page.is_some() {
                items.push((
                    CommandItem::new()
                        .label("Load more channels")
                        .icon(IconName::ArrowDown)
                        .disabled(self.loading),
                    Target::More(self.revision),
                ));
            }
            items
        } else {
            self.people
                .iter()
                .map(|user| {
                    let selected = self.selected.iter().any(|selected| selected.id == user.id);
                    let full_name = format!("{} {}", user.first_name, user.last_name)
                        .trim()
                        .to_owned();
                    let name = if full_name.is_empty() {
                        user.display_name()
                    } else {
                        full_name
                    };
                    let username = format!("@{}", user.username);
                    let label = format!("{name} {username}");
                    let avatar = crate::http::avatar_url(
                        session.client(),
                        user.id.as_str(),
                        user.last_picture_update,
                    );
                    (
                        CommandItem::new()
                            .label(label)
                            .keywords([self.query.clone()])
                            .disabled(
                                self.mode == Mode::Group && self.selected.len() == 7 && !selected,
                            )
                            .child(move |_, cx| {
                                h_flex()
                                    .flex_1()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        Avatar::new()
                                            .name(name.clone())
                                            .src(avatar.clone())
                                            .xsmall(),
                                    )
                                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                                    .child(
                                        div()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(username.clone()),
                                    )
                                    .children(selected.then(|| Icon::new(IconName::Check).small()))
                            }),
                        Target::Person(user.clone(), self.revision),
                    )
                })
                .collect()
        };
        Group {
            label: if self.mode == Mode::Browse {
                "Public channels"
            } else {
                "People"
            },
            items,
        }
    }
}

enum Found {
    People(Vec<User>),
    Channels(Vec<Channel>, bool),
}

fn discovery_error(error: &anyhow::Error, mode: Mode, opening: bool) -> SharedString {
    if matches!(
        error.downcast_ref::<ApiError>(),
        Some(ApiError::Status(403))
    ) {
        return match (mode, opening) {
            (Mode::Browse, true) => {
                "You don’t have permission to join this channel. Ask a team admin for access."
            }
            (Mode::Browse, false) => "You don’t have permission to browse this team’s channels.",
            (_, true) => "You don’t have permission to open this conversation.",
            _ => "You don’t have permission to search people on this server.",
        }
        .into();
    }
    format!(
        "Couldn’t {}. {error}",
        if opening && mode == Mode::Browse {
            "join the channel"
        } else if opening {
            "open the conversation"
        } else if mode == Mode::Browse {
            "load channels"
        } else {
            "search people"
        }
    )
    .into()
}

impl Render for Switcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((revision, channel)) = self.restore_selection.take() {
            let view = cx.entity().downgrade();
            let selection_revision = self.selection_revision;
            // Command installs this render's model before the next frame.
            window.on_next_frame(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.closed
                        || revision != this.revision
                        || selection_revision != this.selection_revision
                    {
                        return;
                    }
                    let group = this.remote_group(cx);
                    let row = group.items.iter().position(|(_, target)| {
                        matches!(target, Target::Join(found, _) if found.id == channel)
                    });
                    if let Some(row) = row {
                        this.state.update(cx, |state, cx| {
                            state.set_selected_index(Some(IndexPath::new(row)), window, cx)
                        });
                    }
                });
            });
        }
        let remote = self.remote_group(cx);
        let groups: Vec<&Group> = if self.mode == Mode::Jump {
            self.local.iter().chain(std::iter::once(&remote)).collect()
        } else {
            vec![&remote]
        };
        // The mapping belongs to this installed Command model. Filtering keeps
        // original IndexPaths, so appending server results preserves local selection.
        let targets: Vec<Vec<Target>> = groups
            .iter()
            .map(|group| {
                group
                    .items
                    .iter()
                    .map(|(_, target)| match target {
                        Target::Channel(id) => Target::Channel(id.clone()),
                        Target::Action(action) => Target::Action(action.boxed_clone()),
                        Target::Mode(mode) => Target::Mode(*mode),
                        Target::Person(user, revision) => Target::Person(user.clone(), *revision),
                        Target::Join(channel, revision) => Target::Join(channel.clone(), *revision),
                        Target::More(revision) => Target::More(*revision),
                    })
                    .collect()
            })
            .collect();
        let mut command = Command::new(&self.state)
            .bordered(false)
            .max_h(gpui_kit::rems(32.))
            .filterable(self.mode == Mode::Jump)
            .placeholder(match self.mode {
                Mode::Jump => "Find a conversation, person, or command…",
                Mode::Group => "Find people to add…",
                Mode::Browse => "Search public channels…",
            });
        for group in groups {
            command = command.group(CommandGroup::new().label(group.label).items(
                group.items.iter().map(|(item, _)| {
                    if self.busy {
                        item.clone().disabled(true)
                    } else {
                        item.clone()
                    }
                }),
            ));
        }
        let status: SharedString = if self.busy {
            if self.mode == Mode::Browse {
                "Joining channel…"
            } else {
                "Opening conversation…"
            }
            .into()
        } else if let Some(error) = &self.error {
            error.clone()
        } else if self.loading {
            if self.mode == Mode::Browse {
                "Loading channels…"
            } else {
                "Searching people…"
            }
            .into()
        } else {
            match self.mode {
                Mode::Jump => "↑ ↓ to select · Enter to open · Esc to close",
                Mode::Group => "Choose 2–7 people · Enter to add or remove",
                Mode::Browse => "Enter to join and open · Esc clears search, then closes",
            }
            .into()
        };
        let waiting = self.loading || self.busy;
        let empty = if waiting {
            "Searching…"
        } else if self.error.is_some() {
            "Try the search again when you’re ready."
        } else {
            match self.mode {
                Mode::Browse => "No public channels to join. Try another search.",
                Mode::Group if self.query.is_empty() => "Type a name or username to find people.",
                _ => "No matches. Try another name or username.",
            }
        };
        let mut view = v_flex()
            .key_context("Switcher")
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                this.cancel();
                window.close_dialog(cx);
            }))
            .gap_2();
        if self.mode != Mode::Jump {
            view = view.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("back")
                            .ghost()
                            .small()
                            .label("Back")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_mode(Mode::Jump, window, cx)
                            })),
                    )
                    .child(if self.mode == Mode::Group {
                        "New group message"
                    } else {
                        "Browse channels"
                    }),
            );
        }
        if self.mode == Mode::Group {
            let mut selection = h_flex().flex_wrap().gap_2();
            for user in &self.selected {
                let id = user.id.clone();
                selection = selection.child(
                    Button::new(SharedString::from(format!("selected-person-{id}")))
                        .small()
                        .label(format!("Remove @{}", user.username))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.selected.retain(|user| user.id != id);
                            this.state.focus_handle(cx).focus(window, cx);
                            cx.notify();
                        })),
                );
            }
            view = view.child(selection);
        }
        let query_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();
        let selection_view = cx.entity().downgrade();
        view = view.child(
            command
                .on_cancel({
                    let view = cx.entity().downgrade();
                    move |_, cx| {
                        let _ = view.update(cx, |this, _| this.cancel());
                    }
                })
                .empty(move |_, _, _| div().p_4().child(empty))
                .on_query(move |query, _, cx| {
                    let _ = query_view.update(cx, |this, cx| this.query_changed(query, cx));
                })
                .on_select(move |_, _, cx| {
                    let _ = selection_view.update(cx, |this, _| this.selection_revision += 1);
                })
                .on_confirm(move |path: IndexPath, window, cx| {
                    if let Some(target) = targets
                        .get(path.section)
                        .and_then(|group| group.get(path.row))
                    {
                        let _ =
                            confirm_view.update(cx, |this, cx| this.confirm(target, window, cx));
                    }
                }),
        );
        view = view.child(
            h_flex()
                .gap_2()
                .px_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .children(waiting.then(|| Spinner::new().small()))
                .child(
                    div()
                        .id("discovery-status")
                        .flex_1()
                        .role(gpui_kit::Role::Status)
                        .aria_label(status.clone())
                        .child(status),
                ),
        );
        if self.error.is_some() && !self.busy && !self.loading {
            view = view.child(
                Button::new("retry-discovery")
                    .ghost()
                    .small()
                    .label("Retry search")
                    .on_click(cx.listener(|this, _, _, cx| this.search(false, cx))),
            );
        }
        if self.mode == Mode::Group {
            view = view.child(
                Button::new("open-group")
                    .label(format!("Open group message ({}/7)", self.selected.len()))
                    .disabled(self.busy || self.selected.len() < 2)
                    .on_click(cx.listener(|this, _, window, cx| this.open_group(window, cx))),
            );
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, TestAppContext, WindowOptions};
    use mattermost::{
        Client,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;
    use std::time::Instant;

    fn route(request: &Request) -> Reply {
        match (
            request.method(),
            request.path().split('?').next().unwrap_or_default(),
        ) {
            ("GET", "/api/v4/users/me/teams") => {
                Reply::json(json!([{ "id": "team", "name": "team" }]))
            }
            ("GET", "/api/v4/teams/team/channels") => {
                if request.path().contains("page=1&") {
                    Reply::json(
                        json!([{ "id": "last", "team_id": "team", "type": "O", "name": "last" }]),
                    )
                } else {
                    Reply::json(json!((0..100).map(|ix| json!({
                        "id": format!("channel-{ix}"), "team_id": "team", "type": "O", "name": format!("channel-{ix}")
                    })).collect::<Vec<_>>()))
                }
            }
            ("POST", "/api/v4/users/status/ids") => Reply::json(json!([])),
            _ => Reply::json(json!([])),
        }
    }

    fn palette(
        server: &SlowServer,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Switcher>) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::update::init(cx);
            crate::actions::init(cx);
            crate::extensions::init(cx);
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let session =
            cx.new(|cx| Session::new(client.clone(), me.clone(), Some("team".into()), cx));
        let (window, palette) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let workspace =
                    cx.new(|cx| Workspace::new(client, me, Some("team".into()), window, cx));
                cx.new(|cx| Switcher::new(workspace.downgrade(), session, Vec::new(), window, cx))
            })
            .unwrap()
        });
        (window, palette)
    }

    fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for discovery");
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui_kit::test]
    fn stale_search_results_cannot_replace_current_people_or_errors(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (_, palette) = palette(&server, cx);
        palette.update(cx, |view, cx| {
            view.query = "ann".into();
            view.revision = 3;
            let mut ann = User::default();
            ann.id = "ann".into();
            view.apply_results(3, Mode::Jump, "ann", 0, Ok(Found::People(vec![ann])), cx);
            // ABA: the query text is the same again, but the earlier request
            // must still be rejected, as must a different query or mode.
            for (revision, mode, query) in [
                (1, Mode::Jump, "ann"),
                (3, Mode::Jump, "ben"),
                (3, Mode::Group, "ann"),
            ] {
                view.apply_results(revision, mode, query, 0, Ok(Found::People(Vec::new())), cx);
                view.apply_results(
                    revision,
                    mode,
                    query,
                    0,
                    Err(anyhow::anyhow!("stale failure")),
                    cx,
                );
                assert_eq!(view.people[0].id.as_str(), "ann");
                assert!(view.error.is_none());
            }
        });
    }

    #[gpui_kit::test]
    fn browse_refresh_coalesces_events_and_keeps_pages_and_selected_channel(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let changed = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::from_millis(10)), {
            let changed = changed.clone();
            move |request| {
                if changed.load(Ordering::SeqCst)
                    && request.method() == "GET"
                    && request.path() == "/api/v4/teams/team/channels?page=0&per_page=100"
                {
                    return Reply::json(json!((1..101).map(|ix| json!({
                        "id": format!("channel-{ix}"), "team_id": "team", "type": "O", "name": format!("channel-{ix}")
                    })).collect::<Vec<_>>()));
                }
                route(request)
            }
        });
        let (window, palette) = palette(&server, cx);
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, cx| view.session.read(cx).team().is_some())
        });
        cx.update_window(window, |_, window, cx| {
            palette.update(cx, |view, cx| view.set_mode(Mode::Browse, window, cx))
        })
        .unwrap();
        wait_until(cx, |cx| palette.read_with(cx, |view, _| !view.loading));
        palette.update(cx, |view, cx| view.fetch(1, false, cx));
        wait_until(cx, |cx| palette.read_with(cx, |view, _| !view.loading));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            palette.update(cx, |view, cx| {
                view.state.update(cx, |state, cx| {
                    state.set_selected_index(Some(IndexPath::new(100)), window, cx)
                })
            });
        })
        .unwrap();
        cx.run_until_parked();
        changed.store(true, Ordering::SeqCst);
        let session = palette.read_with(cx, |view, _| view.session.clone());
        for _ in 0..20 {
            session.update(cx, |_, cx| cx.emit(session::SessionEvent::ChannelsChanged));
        }
        cx.run_until_parked();
        palette.read_with(cx, |view, cx| {
            assert_eq!(
                view.channels.len(),
                101,
                "invalidation must retain the displayed pages"
            );
            assert_eq!(view.state.read(cx).selected_index().unwrap().row, 100);
        });
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, _| !view.refresh_running)
        });
        // Render the reconciled model, then its one-shot selection restoration.
        for _ in 0..2 {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.simulate_next_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        palette.read_with(cx, |view, cx| {
            assert_eq!(view.channels.len(), 101);
            assert_eq!(view.loaded_pages, 2);
            assert!(view.next_page.is_none());
            let path = view.state.read(cx).selected_index().unwrap();
            assert_eq!(path.row, 99, "the archived preceding row was removed");
            assert!(matches!(&view.remote_group(cx).items[path.row].1, Target::Join(channel, _) if channel.id.as_str() == "last"));
        });
        for page in [0, 1] {
            let path = format!("/api/v4/teams/team/channels?page={page}&per_page=100");
            assert_eq!(
                server
                    .requests()
                    .iter()
                    .filter(|r| r.method() == "GET" && r.path() == path)
                    .count(),
                2,
                "one coalesced refresh per loaded page"
            );
        }
    }

    #[gpui_kit::test]
    fn browse_refresh_keeps_a_selected_channel_pushed_past_the_page_boundary(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let changed = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::from_millis(10)), {
            let changed = changed.clone();
            move |request| {
                if request.method() == "GET" && request.path() == "/api/v4/channels/channel-99" {
                    return Reply::json(
                        json!({ "id": "channel-99", "team_id": "team", "type": "O", "name": "channel-99" }),
                    );
                }
                if changed.load(Ordering::SeqCst)
                    && request.method() == "GET"
                    && request.path() == "/api/v4/teams/team/channels?page=0&per_page=100"
                {
                    let mut channels: Vec<_> = (0..99).map(|ix| json!({ "id": format!("channel-{ix}"), "team_id": "team", "type": "O", "name": format!("channel-{ix}") })).collect();
                    channels.insert(
                        0,
                        json!({ "id": "new", "team_id": "team", "type": "O", "name": "new" }),
                    );
                    return Reply::json(json!(channels));
                }
                route(request)
            }
        });
        let (window, palette) = palette(&server, cx);
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, cx| view.session.read(cx).team().is_some())
        });
        cx.update_window(window, |_, window, cx| {
            palette.update(cx, |view, cx| view.set_mode(Mode::Browse, window, cx))
        })
        .unwrap();
        wait_until(cx, |cx| palette.read_with(cx, |view, _| !view.loading));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            palette.update(cx, |view, cx| {
                view.state.update(cx, |state, cx| {
                    state.set_selected_index(Some(IndexPath::new(99)), window, cx)
                })
            });
        })
        .unwrap();
        cx.run_until_parked();
        changed.store(true, Ordering::SeqCst);
        let session = palette.read_with(cx, |view, _| view.session.clone());
        session.update(cx, |_, cx| cx.emit(session::SessionEvent::ChannelsChanged));
        cx.run_until_parked();
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, _| !view.refresh_running)
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.simulate_next_frame(cx);
        })
        .unwrap();
        palette.read_with(cx, |view, cx| {
            assert_eq!(view.channels.len(), 101);
            assert_eq!(view.next_page, Some(1));
            let path = view.state.read(cx).selected_index().unwrap();
            assert!(matches!(&view.remote_group(cx).items[path.row].1, Target::Join(channel, _) if channel.id.as_str() == "channel-99"));
        });
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "GET" && r.path() == "/api/v4/channels/channel-99")
                .count(),
            1
        );
    }

    #[gpui_kit::test]
    fn browse_live_events_do_not_bypass_the_query_debounce(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(10)), route);
        let (window, palette) = palette(&server, cx);
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, cx| view.session.read(cx).team().is_some())
        });
        cx.update_window(window, |_, window, cx| {
            palette.update(cx, |view, cx| view.set_mode(Mode::Browse, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        wait_until(cx, |cx| palette.read_with(cx, |view, _| !view.loading));
        cx.update_window(window, |_, window, cx| {
            palette.update(cx, |view, cx| {
                view.state
                    .update(cx, |state, cx| state.set_query("design", window, cx))
            });
        })
        .unwrap();
        cx.run_until_parked();
        let session = palette.read_with(cx, |view, _| view.session.clone());
        for _ in 0..20 {
            session.update(cx, |_, cx| cx.emit(session::SessionEvent::ChannelsChanged));
        }
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(249));
        cx.run_until_parked();
        let searches = || {
            server
                .requests()
                .into_iter()
                .filter(|r| {
                    r.method() == "POST" && r.path() == "/api/v4/teams/team/channels/search"
                })
                .count()
        };
        assert_eq!(
            searches(),
            0,
            "live events must not start the typed search early"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if palette.read_with(cx, |view, _| !view.loading) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(searches(), 1);
        wait_until(cx, |cx| {
            palette.read_with(cx, |view, _| !view.refresh_running)
        });
        assert_eq!(
            searches(),
            2,
            "one follow-up refresh for the coalesced burst"
        );
    }
}
