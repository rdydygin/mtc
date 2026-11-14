use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::StreamExt as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    WeakEntity,
};
use mattermost::{
    ApiError, Channel, ChannelId, ChannelMember, ChannelType, Client, CustomStatus, Event,
    EventStream, FAVORITE_CHANNEL, FileInfo, Millis, Post, PostId, Preference, Reaction, Status,
    Team, TeamId, User, UserId,
};

use crate::{
    CustomEmojis, Delivery, Directory, EmojiStore, FollowedThreads, ThreadsEvent, Timeline,
    TimelineKind,
    retry::{is_transient, retry},
    timeline::{CLIENT_NONCE, LoadState, now_millis, send_ids},
};

/// How long a "typing" signal stays visible without being repeated.
const TYPING_TTL: Duration = Duration::from_secs(6);
/// Minimum gap between our own typing signals for one conversation.
const TYPING_THROTTLE: Duration = Duration::from_secs(3);
/// Timelines kept alive after their view closes, for instant switching.
const TIMELINE_CACHE: usize = 24;

/// A channel together with the current user's read state in it.
#[derive(Clone, Debug)]
pub struct ChannelEntry {
    channel: Channel,
    member: Option<ChannelMember>,
}

impl ChannelEntry {
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    pub fn id(&self) -> &ChannelId {
        &self.channel.id
    }

    pub fn kind(&self) -> ChannelType {
        self.channel.kind
    }

    pub fn is_muted(&self) -> bool {
        self.member.as_ref().is_some_and(ChannelMember::is_muted)
    }

    /// Whether a root post arrived after the user last read the channel.
    pub fn is_unread(&self) -> bool {
        let last_viewed = self.member.as_ref().map_or(0, |m| m.last_viewed_at);
        self.channel
            .last_root_post_at
            .max(self.channel.last_post_at)
            > last_viewed
            && self.member.is_some()
    }

    pub fn mention_count(&self) -> i64 {
        self.member.as_ref().map_or(0, |m| m.mention_count)
    }

    pub fn last_viewed_at(&self) -> Millis {
        self.member.as_ref().map_or(0, |m| m.last_viewed_at)
    }

    /// Channel override; `default` inherits the account's desktop setting.
    pub fn desktop_notifications(&self) -> &str {
        self.member
            .as_ref()
            .and_then(|m| m.notify_props.get("desktop"))
            .map(String::as_str)
            .unwrap_or("default")
    }

    pub fn ignore_channel_mentions(&self) -> Option<&str> {
        self.member
            .as_ref()
            .and_then(|m| m.notify_props.get("ignore_channel_mentions"))
            .map(String::as_str)
    }

    /// The most recent activity, for recency ordering.
    pub fn last_activity(&self) -> Millis {
        self.channel.last_post_at
    }

    /// Whether the user can leave: a public or private channel other than
    /// the team's default one, which everyone stays in.
    pub fn can_leave(&self) -> bool {
        matches!(self.kind(), ChannelType::Open | ChannelType::Private)
            && self.channel.name != DEFAULT_CHANNEL
    }
}

/// The channel every team member belongs to and can't leave.
const DEFAULT_CHANNEL: &str = "town-square";

/// The real-time connection as the user should understand it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Online,
    /// Disconnected; reconnecting automatically.
    Offline,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// The server rejected the session; the user must sign in again.
    SignedOut,
    /// A post arrived from someone else.
    Posted { post: Arc<Post>, mentions_me: bool },
    /// A send failed; the message is kept so the user can retry.
    SendFailed { message: SharedString },
    /// Channel discovery may have changed on the server.
    ChannelsChanged,
    /// A server event this crate does not model, for extensions.
    Raw {
        name: SharedString,
        data: serde_json::Value,
    },
}

enum ReadIntent {
    Viewed { explicit: bool },
    Unread { post: PostId, at: Millis },
}

/// One in-flight operation and one replaceable latest intent per channel.
#[derive(Default)]
struct ChannelSync {
    pending: Option<ReadIntent>,
    refresh: bool,
    fetching: bool,
    writing: bool,
    unread_hint: bool,
    running: bool,
    _task: Option<Task<()>>,
}

impl ChannelSync {
    fn has_deferred_passive_view(&self) -> bool {
        self.unread_hint && matches!(self.pending, Some(ReadIntent::Viewed { explicit: false }))
    }
}

/// The signed-in session: teams, channels, read state, live timelines, and
/// the real-time connection that keeps them current.
///
/// This is the one owner of server-backed state. Views read it, call its
/// named commands, and react to its notifications and [`SessionEvent`]s.
pub struct Session {
    client: Client,
    me: Arc<User>,
    directory: Entity<Directory>,
    teams: Vec<Team>,
    team: Option<TeamId>,
    channels: HashMap<ChannelId, ChannelEntry>,
    /// Completed lists invalidate older channel hydration.
    channel_list_generation: u64,
    /// Monotonic team selection revision, including round trips to the same team.
    team_revision: u64,
    /// Channels hydrated since the pending list request started.
    hydrated_channels: HashSet<ChannelId>,
    channels_state: LoadState,
    teams_state: LoadState,
    teams_loaded: bool,
    /// Favorite channels in every team; the server keeps them as
    /// preferences, synced across the user's devices.
    favorites: HashSet<ChannelId>,
    /// The other members of each group message, for naming it.
    group_members: HashMap<ChannelId, Vec<UserId>>,
    emoji: Entity<EmojiStore>,
    /// The latest snapshot of [`Self::emoji`], shared with timelines.
    custom_emoji: CustomEmojis,
    active_channel: Option<ChannelId>,
    /// Explicitly unread active channel, protected from passive read updates.
    unread_channel: Option<ChannelId>,
    channel_sync: HashMap<ChannelId, ChannelSync>,
    mute_pending: HashSet<ChannelId>,
    /// The thread on screen, kept read while the window is active.
    open_thread: Option<PostId>,
    /// The threads the user follows in the current team.
    threads: Entity<FollowedThreads>,
    window_active: bool,
    /// Every live timeline, for routing real-time events.
    timelines: HashMap<TimelineKind, WeakEntity<Timeline>>,
    /// Recently used timelines, kept alive for instant switching.
    timeline_cache: Vec<Entity<Timeline>>,
    timeline_subscriptions: HashMap<TimelineKind, Subscription>,
    /// Unresolved sends outlive the bounded timeline cache, but not the session.
    /// Kept in memory only; composer text drafts have separate persistence.
    outbox: Vec<Post>,
    sending: HashSet<PostId>,
    /// Confirmations received while a local delete request is outstanding.
    deleting: HashMap<PostId, bool>,
    typing: HashMap<(ChannelId, PostId), Vec<(UserId, Instant)>>,
    last_typing_sent: HashMap<(ChannelId, PostId), Instant>,
    connection: Connection,
    has_connected: bool,
    /// A load gave up while the network was down; repeat it on reconnect.
    load_failed: bool,
    stream: EventStream,
    _event_task: Task<()>,
    _channels_task: Option<Task<()>>,
    _teams_task: Option<Task<()>>,
    _typing_expiry: Option<Task<()>>,
    _emoji_subscription: Subscription,
    _threads_subscription: Subscription,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    pub fn new(
        client: Client,
        me: User,
        preferred_team: Option<TeamId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let me_arc = Arc::new(me.clone());
        let emoji = cx.new(|cx| {
            let mut emoji = EmojiStore::new(client.clone());
            if let Some(status) = me.custom_status("") {
                emoji.ensure([status.emoji.as_str()], cx);
            }
            emoji.load_all(cx);
            emoji
        });
        let _emoji_subscription = cx.observe(&emoji, |this, emoji, cx| {
            let custom = emoji.read(cx).custom().clone();
            this.set_custom_emoji(custom, cx);
        });
        let directory = cx.new(|_| Directory::new(client.clone(), me, emoji.clone()));
        let threads =
            cx.new(|_| FollowedThreads::new(client.clone(), me_arc.id.clone(), directory.clone()));
        let _threads_subscription = cx.subscribe(&threads, |_, _, event, cx| match event {
            ThreadsEvent::Failed { message } => cx.emit(SessionEvent::SendFailed {
                message: message.clone(),
            }),
        });
        let (stream, mut events) = EventStream::connect(client.clone());
        let _event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut this = Self {
            client,
            me: me_arc,
            directory,
            teams: Vec::new(),
            team: preferred_team,
            channels: HashMap::new(),
            channel_list_generation: 0,
            team_revision: 0,
            hydrated_channels: HashSet::new(),
            channels_state: LoadState::NotLoaded,
            teams_state: LoadState::NotLoaded,
            teams_loaded: false,
            favorites: HashSet::new(),
            group_members: HashMap::new(),
            emoji,
            custom_emoji: CustomEmojis::default(),
            active_channel: None,
            open_thread: None,
            unread_channel: None,
            channel_sync: HashMap::new(),
            mute_pending: HashSet::new(),
            threads,
            window_active: true,
            timelines: HashMap::new(),
            timeline_cache: Vec::new(),
            timeline_subscriptions: HashMap::new(),
            outbox: Vec::new(),
            sending: HashSet::new(),
            deleting: HashMap::new(),
            typing: HashMap::new(),
            last_typing_sent: HashMap::new(),
            connection: Connection::Connecting,
            has_connected: false,
            load_failed: false,
            stream,
            _event_task,
            _channels_task: None,
            _teams_task: None,
            _typing_expiry: None,
            _emoji_subscription,
            _threads_subscription,
        };
        this.load_teams(cx);
        this.load_favorites(cx);
        let me_id = this.me.id.clone();
        this.directory
            .update(cx, |d, cx| d.refresh_statuses(vec![me_id], cx));
        this
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn me(&self) -> &Arc<User> {
        &self.me
    }

    pub fn directory(&self) -> &Entity<Directory> {
        &self.directory
    }

    pub fn teams(&self) -> &[Team] {
        &self.teams
    }

    pub fn team(&self) -> Option<&Team> {
        let id = self.team.as_ref()?;
        self.teams.iter().find(|t| &t.id == id)
    }

    pub fn connection(&self) -> Connection {
        self.connection
    }

    pub fn channels(&self) -> impl Iterator<Item = &ChannelEntry> {
        self.channels.values()
    }

    pub fn channel(&self, id: &ChannelId) -> Option<&ChannelEntry> {
        self.channels.get(id)
    }

    /// Whether the current team's channels have arrived.
    pub fn is_loaded(&self) -> bool {
        self.channels_state == LoadState::Loaded
    }

    /// Whether the team list has arrived. At startup the remembered team's
    /// channels can arrive first.
    pub fn teams_loaded(&self) -> bool {
        self.teams_loaded
    }

    pub fn active_channel(&self) -> Option<&ChannelId> {
        self.active_channel.as_ref()
    }

    /// The threads the user follows in the current team.
    pub fn threads(&self) -> &Entity<FollowedThreads> {
        &self.threads
    }

    /// The root of the thread on screen.
    pub fn open_thread(&self) -> Option<&PostId> {
        self.open_thread.as_ref()
    }

    pub fn is_favorite(&self, channel: &ChannelId) -> bool {
        self.favorites.contains(channel)
    }

    /// The server's custom emoji, for drawing reactions and statuses.
    pub fn custom_emoji(&self) -> &CustomEmojis {
        &self.custom_emoji
    }

    /// Where custom emoji are looked up; [`Self::custom_emoji`] is its
    /// latest snapshot.
    pub fn emoji_store(&self) -> &Entity<EmojiStore> {
        &self.emoji
    }

    /// The name people know a channel by. Direct and group messages are
    /// named after the other participants.
    pub fn channel_label(&self, entry: &ChannelEntry, cx: &App) -> SharedString {
        let channel = &entry.channel;
        if let Some(partner) = channel.direct_partner(&self.me.id) {
            if partner == self.me.id {
                return format!("{} (you)", self.me.display_name()).into();
            }
            return self.directory.read(cx).display_name(&partner);
        }
        // The server's own name for a group lists usernames.
        if let Some(members) = self.group_members.get(&channel.id) {
            let directory = self.directory.read(cx);
            let mut names: Vec<SharedString> = members
                .iter()
                .map(|id| directory.display_name(id))
                .collect();
            names.sort_by_key(|name| name.to_lowercase());
            return names.join(", ").into();
        }
        if channel.display_name.is_empty() {
            channel.name.clone().into()
        } else {
            channel.display_name.clone().into()
        }
    }

    // ----- Loading ---------------------------------------------------------

    /// Includes startup team discovery, which can fail before channels are known.
    pub fn channels_state(&self) -> &LoadState {
        if matches!(self.teams_state, LoadState::Failed(_)) {
            &self.teams_state
        } else {
            &self.channels_state
        }
    }

    pub fn retry_channels(&mut self, cx: &mut Context<Self>) {
        if !self.teams_loaded || matches!(self.teams_state, LoadState::Failed(_)) {
            if self.teams_state != LoadState::Loading {
                self.load_teams(cx);
            }
        } else if self.channels_state != LoadState::Loading {
            self.load_channels(cx);
        }
    }

    fn load_teams(&mut self, cx: &mut Context<Self>) {
        if self.teams_state == LoadState::Loading {
            return;
        }
        self.teams_state = LoadState::Loading;
        self.channels_state = LoadState::Loading;
        cx.notify();
        // The remembered team is almost always still ours, so its channels
        // load alongside the team list rather than after it: the sidebar
        // fills one round trip sooner. The list corrects a stale guess.
        let guess = self.team.clone();
        if guess.is_some() {
            self.load_channels(cx);
        }
        let client = self.client.clone();
        self._teams_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || client.my_teams()).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(teams) => {
                    this.teams_loaded = true;
                    this.teams_state = LoadState::Loaded;
                    this.teams = teams.into_iter().filter(|t| t.delete_at == 0).collect();
                    this.teams.sort_by_key(|t| t.display_name.to_lowercase());
                    let keep = this
                        .team
                        .as_ref()
                        .is_some_and(|id| this.teams.iter().any(|t| &t.id == id));
                    if !keep {
                        let team = this.teams.first().map(|t| t.id.clone());
                        if this.team != team {
                            this.team_revision += 1;
                            this.team = team;
                        }
                    }
                    if this.team != guess || guess.is_none() {
                        this.channels.clear();
                        this.channels_state = LoadState::NotLoaded;
                        this.load_channels(cx);
                    }
                    cx.notify();
                }
                Err(err) => {
                    this.teams_state = LoadState::Failed(
                        "Couldn’t load conversations. Check your connection and try again.".into(),
                    );
                    this.handle_error(err, cx);
                    cx.notify();
                }
            });
        }));
    }

    /// Switch the sidebar to another team.
    /// Changes whenever a different team is selected.
    pub fn team_revision(&self) -> u64 {
        self.team_revision
    }

    pub fn select_team(&mut self, team: TeamId, cx: &mut Context<Self>) {
        if self.team.as_ref() == Some(&team) {
            return;
        }
        self.team_revision += 1;
        self.team = Some(team);
        self.channels.clear();
        self.channel_list_generation += 1;
        self.channels_state = LoadState::NotLoaded;
        self.load_channels(cx);
        cx.notify();
    }

    fn load_channels(&mut self, cx: &mut Context<Self>) {
        self._channels_task = None;
        self.channels_state = LoadState::Loading;
        cx.notify();
        self.hydrated_channels.clear();
        let team = self.team.clone();
        self.threads.update(cx, |t, cx| t.set_team(team, cx));
        let Some(team) = self.team.clone() else {
            self.channels_state = LoadState::Loaded;
            cx.notify();
            return;
        };
        let client = self.client.clone();
        self._channels_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let (channels, members) = futures::join!(
                retry(&executor, || client.my_channels(&team)),
                retry(&executor, || client.my_channel_members(&team))
            );
            let _ = this.update(cx, |this, cx| match (channels, members) {
                (Ok(channels), Ok(members)) => {
                    let mut members: HashMap<ChannelId, ChannelMember> = members
                        .into_iter()
                        .map(|m| (m.channel_id.clone(), m))
                        .collect();
                    let mut channels: HashMap<ChannelId, ChannelEntry> = channels
                        .into_iter()
                        .filter(|c| c.delete_at == 0)
                        .map(|channel| {
                            let member = members.remove(&channel.id);
                            (channel.id.clone(), ChannelEntry { channel, member })
                        })
                        .collect();
                    // These channels were loaded after this snapshot began.
                    for id in this.hydrated_channels.drain() {
                        if let Some(entry) = this.channels.get(&id) {
                            channels.insert(id, entry.clone());
                        }
                    }
                    this.channel_list_generation += 1;
                    // A channel left elsewhere, or one the user was removed
                    // from, takes its threads with it.
                    let lost = this.channels.keys().any(|id| !channels.contains_key(id));
                    this.channels = channels;
                    this.channels_state = LoadState::Loaded;
                    if let Some(id) = this.unread_channel.as_ref()
                        && !this.channel_sync.get(id).is_some_and(|sync| sync.running)
                        && this
                            .channel(id)
                            .is_some_and(|entry| !entry.is_unread() && entry.mention_count() == 0)
                    {
                        let id = this.unread_channel.take().unwrap();
                        if let Some(timeline) = this.live_timeline(&TimelineKind::Channel(id)) {
                            timeline.update(cx, |t, cx| t.set_unread_after(None, cx));
                        }
                    }
                    if lost {
                        let channels = &this.channels;
                        this.threads.update(cx, |t, cx| {
                            t.keep_channels(|id| channels.contains_key(id), cx)
                        });
                    }
                    let partners: Vec<UserId> = this
                        .channels
                        .values()
                        .filter_map(|e| e.channel.direct_partner(&this.me.id))
                        .collect();
                    this.directory.update(cx, |d, cx| d.ensure(partners, cx));
                    this.load_group_members(cx);
                    cx.notify();
                }
                (Err(err), _) | (_, Err(err)) => {
                    this.channels_state = LoadState::Failed(
                        "Couldn’t load conversations. Check your connection and try again.".into(),
                    );
                    this.handle_error(err, cx);
                    cx.notify();
                }
            });
        }));
    }

    /// Learn who is in each group message not yet known. Membership of a
    /// group message never changes, so each is asked about once.
    fn load_group_members(&mut self, cx: &mut Context<Self>) {
        let unknown: Vec<ChannelId> = self
            .channels
            .values()
            .filter(|e| e.kind() == ChannelType::Group && !self.group_members.contains_key(e.id()))
            .map(|e| e.id().clone())
            .collect();
        if unknown.is_empty() {
            return;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.group_channel_members(unknown).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(groups) => {
                    let mut users = Vec::new();
                    for (channel, members) in groups {
                        let ids = members
                            .iter()
                            .map(|u| u.id.clone())
                            .filter(|id| id != &this.me.id)
                            .collect();
                        this.group_members.insert(channel, ids);
                        users.extend(members);
                    }
                    this.directory.update(cx, |d, cx| d.insert(users, cx));
                    cx.notify();
                }
                Err(err) => log::warn!("loading group members: {err:#}"),
            });
        })
        .detach();
    }

    fn load_favorites(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || client.my_preferences(FAVORITE_CHANNEL)).await;
            // The server answers 404 for a category the user has never used.
            let result = match result {
                Err(err) if err.downcast_ref::<ApiError>() == Some(&ApiError::Status(404)) => {
                    Ok(Vec::new())
                }
                result => result,
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(preferences) => {
                    this.favorites = preferences
                        .iter()
                        .filter_map(Preference::favorite_channel)
                        .collect();
                    cx.notify();
                }
                Err(err) => this.handle_error(err, cx),
            });
        })
        .detach();
    }

    fn set_custom_emoji(&mut self, custom: CustomEmojis, cx: &mut Context<Self>) {
        self.custom_emoji = custom;
        for timeline in self.live_timelines() {
            let custom = self.custom_emoji.clone();
            timeline.update(cx, |t, cx| t.set_custom_emoji(custom, cx));
        }
        cx.notify();
    }

    fn handle_error(&mut self, err: anyhow::Error, cx: &mut Context<Self>) {
        if err.downcast_ref::<ApiError>() == Some(&ApiError::Unauthorized) {
            cx.emit(SessionEvent::SignedOut);
        } else {
            log::warn!("{err:#}");
            // Only a failure that may pass is worth repeating on reconnect.
            if is_transient(&err) {
                self.load_failed = true;
            }
        }
    }

    /// Read the signed-in user again. A session restored from disk opens
    /// with the profile saved last time, so the window doesn't wait on the
    /// network; this confirms the token still works (signing out if not)
    /// and picks up changes made elsewhere.
    pub fn refresh_me(&mut self, cx: &mut Context<Self>) -> Task<Result<Arc<User>>> {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || client.me()).await;
            this.update(cx, |this, cx| match result {
                Ok(me) => {
                    let me = Arc::new(me);
                    this.directory
                        .update(cx, |d, cx| d.insert([(*me).clone()], cx));
                    this.me = me.clone();
                    cx.notify();
                    Ok(me)
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    this.handle_error(err, cx);
                    Err(anyhow::anyhow!(message))
                }
            })?
        })
    }

    // ----- Timelines -------------------------------------------------------

    /// The timeline for `kind`, created and loaded on first use.
    pub fn timeline(&mut self, kind: TimelineKind, cx: &mut Context<Self>) -> Entity<Timeline> {
        let timeline = match self.timelines.get(&kind).and_then(WeakEntity::upgrade) {
            Some(timeline) => timeline,
            None => {
                let client = self.client.clone();
                let directory = self.directory.clone();
                let emoji = self.emoji.clone();
                let custom_emoji = self.custom_emoji.clone();
                let timeline =
                    cx.new(|_| Timeline::new(kind.clone(), client, directory, emoji, custom_emoji));
                let subscription = cx.observe(&timeline, |this, timeline, cx| {
                    if this.outbox.is_empty() {
                        return;
                    }
                    let timeline = timeline.read(cx);
                    let confirmed: Vec<_> = timeline
                        .entries()
                        .iter()
                        .filter(|e| {
                            e.delivery() == Delivery::Sent && e.post().user_id == this.me.id
                        })
                        .flat_map(|e| send_ids(e.post()).map(str::to_owned))
                        .collect();
                    this.confirm_sends(&confirmed);
                });
                self.timeline_subscriptions
                    .insert(kind.clone(), subscription);
                self.timelines.insert(kind.clone(), timeline.downgrade());
                timeline.update(cx, |t, cx| {
                    for post in self.outbox.iter().filter(|p| kind.accepts(p)) {
                        t.insert_pending(post.clone(), cx);
                        if !self.sending.contains(&post.id) {
                            t.set_delivery(&post.id, Delivery::Failed, cx);
                        }
                    }
                });
                timeline
            }
        };
        // Most recently used last; evict from the front.
        self.timeline_cache.retain(|t| t != &timeline);
        self.timeline_cache.push(timeline.clone());
        if self.timeline_cache.len() > TIMELINE_CACHE {
            self.timeline_cache.remove(0);
        }
        self.timelines.retain(|_, weak| weak.upgrade().is_some());
        self.timeline_subscriptions
            .retain(|kind, _| self.timelines.contains_key(kind));
        timeline.update(cx, |t, cx| t.load(cx));
        timeline
    }

    fn live_timeline(&self, kind: &TimelineKind) -> Option<Entity<Timeline>> {
        self.timelines.get(kind).and_then(WeakEntity::upgrade)
    }

    fn live_timelines(&self) -> Vec<Entity<Timeline>> {
        self.timelines
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect()
    }

    fn confirm_sends(&mut self, ids: &[String]) {
        self.outbox.retain(|p| !ids.contains(&p.pending_post_id));
        self.sending
            .retain(|id| !ids.iter().any(|p| p == id.as_str()));
    }

    /// Load a channel found outside the startup list before showing it.
    pub fn ensure_channel(&mut self, id: &ChannelId, cx: &mut Context<Self>) -> Task<Result<()>> {
        if self.channels.contains_key(id) {
            return Task::ready(Ok(()));
        }
        let client = self.client.clone();
        let id = id.clone();
        let team = self.team.clone();
        let generation = self.channel_list_generation;
        cx.spawn(async move |this, cx| {
            let channel = client.channel(&id).await?;
            let member = Some(client.my_channel_member(&id).await?);
            this.update(cx, |this, cx| -> Result<()> {
                if this.team != team || this.channel_list_generation != generation {
                    return Err(anyhow::anyhow!("The channel list changed."));
                }
                let partner = channel.direct_partner(&this.me.id);
                this.channels
                    .entry(id.clone())
                    .or_insert(ChannelEntry { channel, member });
                this.hydrated_channels.insert(id);
                this.directory.update(cx, |d, cx| d.ensure(partner, cx));
                this.load_group_members(cx);
                cx.notify();
                Ok(())
            })??;
            Ok(())
        })
    }

    /// Show `channel` and mark it read. Returns its timeline.
    pub fn open_channel(
        &mut self,
        channel: &ChannelId,
        cx: &mut Context<Self>,
    ) -> Entity<Timeline> {
        let entered = self.active_channel.as_ref() != Some(channel);
        if entered {
            self.unread_channel = None;
        }
        self.active_channel = Some(channel.clone());
        let timeline = self.timeline(TimelineKind::Channel(channel.clone()), cx);
        let unread_after = self
            .channels
            .get(channel)
            .filter(|e| e.is_unread())
            .map(ChannelEntry::last_viewed_at);
        timeline.update(cx, |t, cx| t.set_unread_after(unread_after, cx));
        if entered {
            self.request_view(channel, true, cx);
        } else {
            self.auto_mark_viewed(channel, cx);
        }
        cx.notify();
        timeline
    }

    /// Stop tracking the closed conversation as visible.
    pub fn close_channel(&mut self, cx: &mut Context<Self>) {
        self.active_channel = None;
        self.unread_channel = None;
        cx.notify();
    }

    /// Explicit read commands (Esc, /read, sending) release local suppression.
    pub fn mark_viewed(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.unread_channel.as_ref() == Some(channel) {
            self.unread_channel = None;
            if let Some(timeline) = self.live_timeline(&TimelineKind::Channel(channel.clone())) {
                timeline.update(cx, |t, cx| t.set_unread_after(None, cx));
            }
        }
        self.request_view(channel, true, cx);
    }

    fn auto_mark_viewed(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.unread_channel.as_ref() != Some(channel) {
            self.request_view(channel, false, cx);
        }
    }

    fn request_view(&mut self, channel: &ChannelId, explicit: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.channels.get_mut(channel) else {
            return;
        };
        if let Some(member) = entry.member.as_mut() {
            member.last_viewed_at = now_millis().max(entry.channel.last_post_at);
            member.mention_count = 0;
            member.mention_count_root = 0;
            member.msg_count = entry.channel.total_msg_count;
            member.msg_count_root = entry.channel.total_msg_count_root;
        }
        self.hydrated_channels.insert(channel.clone());
        cx.notify();
        let sync = self.channel_sync.entry(channel.clone()).or_default();
        if explicit
            || sync.pending.is_none()
            || matches!(sync.pending, Some(ReadIntent::Viewed { explicit: false }))
        {
            sync.pending = Some(ReadIntent::Viewed { explicit });
        }
        self.sync_channel(channel, cx);
    }

    /// Only sent root posts have channel-level unread state.
    pub fn mark_post_unread(&mut self, post: &Post, cx: &mut Context<Self>) {
        if post.is_reply()
            || (!post.pending_post_id.is_empty() && post.id.as_str() == post.pending_post_id)
            || !self.channels.contains_key(&post.channel_id)
        {
            return;
        }
        if self.active_channel.as_ref() == Some(&post.channel_id) {
            self.unread_channel = Some(post.channel_id.clone());
        }
        self.channel_sync
            .entry(post.channel_id.clone())
            .or_default()
            .pending = Some(ReadIntent::Unread {
            post: post.id.clone(),
            at: post.create_at - 1,
        });
        self.sync_channel(&post.channel_id, cx);
    }

    fn refresh_channel_member(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        self.channel_sync
            .entry(channel.clone())
            .or_default()
            .refresh = true;
        self.sync_channel(channel, cx);
    }

    fn dirty_member_snapshot(&mut self, channel: &ChannelId) {
        if let Some(sync) = self.channel_sync.get_mut(channel)
            && sync.fetching
        {
            sync.refresh = true;
        }
    }

    /// Writes are serialized, passive views collapse to one pending intent, and
    /// every settled write (including errors) is reconciled with server truth.
    fn sync_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        let sync = self.channel_sync.entry(channel.clone()).or_default();
        if sync.running {
            return;
        }
        sync.running = true;
        let channel = channel.clone();
        let client = self.client.clone();
        let user = self.me.id.clone();
        let id = channel.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut marker = None;
            loop {
                let step = this.update(cx, |this, _| {
                    let sync = this.channel_sync.get_mut(&id)?;
                    if sync.pending.is_none() && !sync.refresh {
                        sync.running = false;
                        return None;
                    }
                    // Confirm a remote unread hint before allowing a passive
                    // view to erase it. Explicit intents retain priority.
                    let intent = if sync.has_deferred_passive_view() {
                        None
                    } else {
                        sync.pending.take()
                    };
                    sync.writing = intent.is_some();
                    Some(intent)
                });
                let Ok(Some(intent)) = step else {
                    break;
                };
                let result = match intent {
                    Some(ReadIntent::Viewed { .. }) => {
                        marker = None;
                        client.view_channel(&id).await
                    }
                    Some(ReadIntent::Unread { post, at }) => {
                        let result = client.mark_post_unread(&user, &post).await.map(|_| ());
                        marker = Some((post, at));
                        result
                    }
                    None => {
                        // Coalesce WS bursts before starting a standalone refetch.
                        cx.background_executor()
                            .timer(Duration::from_millis(25))
                            .await;
                        Ok(())
                    }
                };
                if let Err(err) = result {
                    let _ = this.update(cx, |this, cx| {
                        if marker.is_some() {
                            cx.emit(SessionEvent::SendFailed {
                                message: format!("Couldn’t mark the message unread. {err:#}")
                                    .into(),
                            });
                        }
                        this.handle_error(err, cx);
                    });
                }
                // Events received while writing/debouncing are covered by this GET;
                // events received during the GET request one further refetch.
                if this
                    .update(cx, |this, _| {
                        let sync = this.channel_sync.get_mut(&id).unwrap();
                        sync.writing = false;
                        sync.refresh = false;
                        sync.fetching = true;
                    })
                    .is_err()
                {
                    break;
                }
                let result = client.my_channel_member(&id).await;
                let _ = this.update(cx, |this, cx| {
                    let sync = this.channel_sync.get_mut(&id).unwrap();
                    sync.fetching = false;
                    // Events received after this GET started make the snapshot
                    // stale. Keep their state and let the worker refetch it.
                    if sync.refresh {
                        return;
                    }
                    match result {
                        Ok(member) => this.apply_channel_member(member, marker.take(), cx),
                        Err(err) => {
                            // A failed confirmation cannot authorize a passive
                            // read. Explicit intents remain queued for execution.
                            let sync = this.channel_sync.get_mut(&id).unwrap();
                            if sync.has_deferred_passive_view() {
                                sync.pending = None;
                            }
                            this.handle_error(err, cx);
                        }
                    }
                });
            }
        });
        self.channel_sync.get_mut(&channel).unwrap()._task = Some(task);
    }

    fn apply_channel_member(
        &mut self,
        member: ChannelMember,
        marker: Option<(PostId, Millis)>,
        cx: &mut Context<Self>,
    ) {
        if member.user_id != self.me.id {
            return;
        }
        let channel = member.channel_id.clone();
        let queued = self.channel_sync.get(&channel).is_some_and(|sync| {
            sync.writing || (sync.pending.is_some() && !sync.has_deferred_passive_view())
        });
        let Some(entry) = self.channels.get_mut(&channel) else {
            return;
        };
        // Preserve the latest optimistic local intent until its write settles.
        if queued {
            if let Some(current) = entry.member.as_mut() {
                current.notify_props = member.notify_props;
            }
        } else {
            entry.member = Some(member);
            let unread = entry.is_unread() || entry.mention_count() > 0;
            let at = unread.then(|| entry.last_viewed_at());
            if let Some(sync) = self.channel_sync.get_mut(&channel) {
                if unread && sync.has_deferred_passive_view() {
                    sync.pending = None;
                }
                if unread && sync.unread_hint && self.active_channel.as_ref() == Some(&channel) {
                    self.unread_channel = Some(channel.clone());
                }
                sync.unread_hint = false;
            }
            let clear_mark = !unread && self.unread_channel.as_ref() == Some(&channel);
            if clear_mark {
                self.unread_channel = None;
            }
            // Ordinary reads retain the opening boundary; an authoritative read
            // that releases explicit suppression clears its marked boundary.
            if (unread || clear_mark)
                && let Some(timeline) = self.live_timeline(&TimelineKind::Channel(channel.clone()))
            {
                timeline.update(cx, |t, cx| match marker {
                    Some((post, marked_at)) if at == Some(marked_at) => {
                        t.set_unread_post(post, marked_at, cx)
                    }
                    _ => t.set_unread_after(at, cx),
                });
            }
        }
        self.hydrated_channels.insert(channel);
        cx.notify();
    }

    pub fn is_mute_pending(&self, channel: &ChannelId) -> bool {
        self.mute_pending.contains(channel)
    }

    /// Optimistically update only mark_unread, then refetch after every outcome.
    pub fn set_channel_muted(&mut self, channel: &ChannelId, muted: bool, cx: &mut Context<Self>) {
        if self.mute_pending.contains(channel) {
            return;
        }
        self.dirty_member_snapshot(channel);
        let Some(member) = self
            .channels
            .get_mut(channel)
            .and_then(|e| e.member.as_mut())
        else {
            return;
        };
        let previous = member.notify_props.get("mark_unread").cloned();
        member.notify_props.insert(
            "mark_unread".into(),
            if muted { "mention" } else { "all" }.into(),
        );
        self.mute_pending.insert(channel.clone());
        self.hydrated_channels.insert(channel.clone());
        cx.notify();
        let client = self.client.clone();
        let user = self.me.id.clone();
        let channel = channel.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = client.set_channel_muted(&channel, &user, muted).await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(member) = this
                        .channels
                        .get_mut(&channel)
                        .and_then(|e| e.member.as_mut())
                    {
                        match previous {
                            Some(value) => {
                                member.notify_props.insert("mark_unread".into(), value);
                            }
                            None => {
                                member.notify_props.remove("mark_unread");
                            }
                        }
                    }
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t change channel mute state. {err:#}").into(),
                    });
                    this.handle_error(err, cx);
                    cx.notify();
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.mute_pending.remove(&channel);
                this.refresh_channel_member(&channel, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Membership broadcasts refresh preferences without overwriting read counters.
    fn apply_notify_props(&mut self, member: ChannelMember, cx: &mut Context<Self>) {
        if member.user_id == self.me.id {
            self.dirty_member_snapshot(&member.channel_id);
        }
        if member.user_id == self.me.id
            && let Some(current) = self
                .channels
                .get_mut(&member.channel_id)
                .and_then(|e| e.member.as_mut())
        {
            self.hydrated_channels.insert(member.channel_id);
            current.notify_props = member.notify_props;
            cx.notify();
        }
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        if active && let Some(channel) = self.active_channel.clone() {
            let unread = self
                .channels
                .get(&channel)
                .is_some_and(ChannelEntry::is_unread);
            if unread {
                self.auto_mark_viewed(&channel, cx);
            }
        }
        if active && let Some(root) = self.open_thread.clone() {
            let unread = self
                .threads
                .read(cx)
                .thread(&root)
                .is_some_and(|t| t.is_unread());
            if unread {
                self.threads.update(cx, |t, cx| t.mark_read(&root, cx));
            }
        }
    }

    /// Show the thread under `root`, or with `None`, no thread, and mark it
    /// read.
    pub fn set_open_thread(&mut self, root: Option<PostId>, cx: &mut Context<Self>) {
        if self.open_thread == root {
            return;
        }
        self.open_thread = root.clone();
        if let Some(root) = root
            && self.window_active
        {
            self.threads.update(cx, |t, cx| t.mark_read(&root, cx));
        }
        cx.notify();
    }

    /// Whether a server event about followed threads is about this team's.
    /// Threads in direct and group messages belong to no team.
    fn is_current_team(&self, team: &TeamId) -> bool {
        team.is_empty() || self.team.as_ref() == Some(team)
    }

    // ----- Commands --------------------------------------------------------

    /// Send `message` to a channel or thread, showing it immediately.
    /// `files` are already uploaded (see [`mattermost::Client::upload_file`]);
    /// a post may be files alone.
    pub fn send_post(
        &mut self,
        kind: &TimelineKind,
        message: String,
        files: Vec<FileInfo>,
        cx: &mut Context<Self>,
    ) {
        let message = message.trim_end().to_string();
        if message.trim().is_empty() && files.is_empty() {
            return;
        }
        self.mark_viewed(kind.channel(), cx);
        let mut now = now_millis();
        let pending_id = loop {
            let id = format!("{}:{}", self.me.id, now);
            if !self.outbox.iter().any(|p| p.pending_post_id == id) {
                break id;
            }
            now += 1;
        };
        let mut post = Post::default();
        post.id = PostId::from(pending_id.as_str());
        post.pending_post_id = pending_id.clone();
        post.props.insert(CLIENT_NONCE.into(), pending_id.into());
        post.create_at = now;
        post.user_id = self.me.id.clone();
        post.channel_id = kind.channel().clone();
        post.root_id = kind.root().cloned().unwrap_or_default();
        post.message = message.clone();
        post.metadata.files = files;

        let timeline = self.timeline(kind.clone(), cx);
        self.outbox.push(post.clone());
        self.sending.insert(post.id.clone());
        timeline.update(cx, |t, cx| t.insert_pending(post.clone(), cx));
        self.last_typing_sent.remove(&(
            kind.channel().clone(),
            kind.root().cloned().unwrap_or_default(),
        ));

        self.submit_post(post, cx);
    }

    /// Submit an optimistic post with its original deduplication id.
    fn submit_post(&mut self, post: Post, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let pending = post.id.clone();
        let author = post.user_id.clone();
        cx.spawn(async move |this, cx| {
            let file_ids: Vec<String> = post.metadata.files.iter().map(|f| f.id.clone()).collect();
            let root = (!post.root_id.is_empty()).then_some(&post.root_id);
            let result = client
                .create_post(
                    &post.channel_id,
                    &post.message,
                    root,
                    &post.pending_post_id,
                    &file_ids,
                    &post.props,
                )
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(mut post) => {
                    if post.user_id.is_empty() {
                        post.user_id = author;
                    }
                    // Some servers omit the echo field in the REST response;
                    // this request still confirms the exact submitted send.
                    if post.pending_post_id.is_empty() {
                        post.pending_post_id = pending.to_string();
                    }
                    this.route_post(post, false, cx);
                }
                Err(err) => {
                    // A late echo can confirm the post before this failure arrives.
                    this.sending.remove(&pending);
                    let failed = this.outbox.iter().any(|p| p.id == pending);
                    if failed {
                        for timeline in this.live_timelines() {
                            timeline
                                .update(cx, |t, cx| t.set_delivery(&pending, Delivery::Failed, cx));
                        }
                    }
                    if failed {
                        cx.emit(SessionEvent::SendFailed {
                            message: format!("Couldn’t send your message. {err:#}").into(),
                        });
                    }
                    this.handle_error(err, cx);
                }
            });
        })
        .detach();
    }

    /// Send a failed post again.
    pub fn retry_post(&mut self, kind: &TimelineKind, pending: &PostId, cx: &mut Context<Self>) {
        let Some(post) = self
            .outbox
            .iter()
            .find(|p| &p.id == pending && kind.accepts(p))
            .cloned()
        else {
            return;
        };
        if !self.sending.insert(pending.clone()) {
            return;
        }
        let timeline = self.timeline(kind.clone(), cx);
        timeline.update(cx, |t, cx| t.set_delivery(pending, Delivery::Pending, cx));
        self.submit_post(post, cx);
    }

    /// Discard a failed post without sending it.
    pub fn discard_post(&mut self, kind: &TimelineKind, pending: &PostId, cx: &mut Context<Self>) {
        if self.sending.contains(pending) {
            return;
        }
        let before = self.outbox.len();
        self.outbox.retain(|p| &p.id != pending || !kind.accepts(p));
        if self.outbox.len() != before {
            for timeline in self.live_timelines() {
                timeline.update(cx, |t, cx| t.remove(pending, cx));
            }
        }
    }

    pub fn edit_post(&mut self, post: &PostId, message: String, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let post = post.clone();
        cx.spawn(async move |this, cx| {
            let result = client.edit_post(&post, &message).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(post) => this.route_edited(post, cx),
                Err(err) => {
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t save your edit. {err:#}").into(),
                    });
                }
            });
        })
        .detach();
    }

    pub fn delete_post(&mut self, post: &Post, cx: &mut Context<Self>) {
        if self.deleting.contains_key(&post.id) {
            return;
        }
        self.deleting.insert(post.id.clone(), false);
        // Keep followed threads until confirmation, so a rejection cannot lose
        // their unread state or list membership.
        let removed: Vec<WeakEntity<Timeline>> = self
            .live_timelines()
            .into_iter()
            .filter_map(|timeline| {
                timeline.update(cx, |t, cx| {
                    t.position(&post.id)?;
                    t.remove(&post.id, cx);
                    Some(timeline.downgrade())
                })
            })
            .collect();
        let client = self.client.clone();
        let post = post.clone();
        cx.spawn(async move |this, cx| {
            let result = client.delete_post(&post.id).await;
            let fetched = if let Err(err) = &result {
                let rejected = matches!(
                    err.downcast_ref::<ApiError>(),
                    Some(ApiError::Unauthorized | ApiError::Status(400..=499))
                );
                if rejected {
                    None
                } else {
                    Some(client.get_post(&post.id).await)
                }
            } else {
                None
            };
            let _ = this.update(cx, |this, cx| {
                let confirmed = this.deleting.remove(&post.id).unwrap_or(false);
                match result {
                    Ok(()) => this.route_deleted(&post, cx),
                    Err(err) => {
                        if !confirmed {
                            match fetched {
                                Some(Ok(post)) if post.delete_at > 0 => {
                                    this.route_deleted(&post, cx)
                                }
                                Some(Ok(post)) => this.restore_deleted(post, &removed, cx),
                                Some(Err(fetch_err)) => {
                                    if matches!(
                                        fetch_err.downcast_ref::<ApiError>(),
                                        Some(ApiError::Status(404))
                                    ) {
                                        this.route_deleted(&post, cx);
                                    } else {
                                        log::warn!("reconciling delete: {fetch_err:#}");
                                    }
                                }
                                None => this.restore_deleted(post, &removed, cx),
                            }
                        }
                        cx.emit(SessionEvent::SendFailed {
                            message: format!("Couldn’t delete the message. {err:#}").into(),
                        });
                    }
                }
            });
        })
        .detach();
    }

    fn restore_deleted(
        &mut self,
        post: Post,
        removed: &[WeakEntity<Timeline>],
        cx: &mut Context<Self>,
    ) {
        for timeline in removed.iter().filter_map(WeakEntity::upgrade) {
            timeline.update(cx, |t, cx| t.upsert(post.clone(), cx));
        }
        if !post.is_reply() {
            self.threads
                .update(cx, |t, cx| t.note_root_edited(&post, cx));
        }
    }

    /// Add the user's reaction, or remove it if already present.
    pub fn toggle_reaction(&mut self, post: &Post, emoji_name: &str, cx: &mut Context<Self>) {
        let mine = post
            .metadata
            .reactions
            .iter()
            .any(|r| r.user_id == self.me.id && r.emoji_name == emoji_name);
        let mut reaction = Reaction::default();
        reaction.user_id = self.me.id.clone();
        reaction.post_id = post.id.clone();
        reaction.emoji_name = emoji_name.to_string();
        reaction.create_at = now_millis();
        self.route_reaction(&reaction, !mine, cx);

        let client = self.client.clone();
        cx.background_spawn(async move {
            let result = if mine {
                client
                    .remove_reaction(&reaction.user_id, &reaction.post_id, &reaction.emoji_name)
                    .await
            } else {
                client
                    .add_reaction(&reaction.user_id, &reaction.post_id, &reaction.emoji_name)
                    .await
            };
            if let Err(err) = result {
                log::warn!("reaction: {err:#}");
            }
        })
        .detach();
    }

    /// Set the user's presence; others see it beside their name.
    pub fn set_presence(&mut self, status: Status, cx: &mut Context<Self>) {
        let me = self.me.id.clone();
        self.directory
            .update(cx, |d, cx| d.set_status(me.clone(), status, cx));
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = client.set_status(&me, status).await {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t change your status. {err:#}").into(),
                    })
                });
            }
        })
        .detach();
    }

    /// Set or, with `None`, clear the user's custom status.
    pub fn set_custom_status(&mut self, status: Option<CustomStatus>, cx: &mut Context<Self>) {
        let me = self.me.id.clone();
        let stored = status
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok())
            .unwrap_or_default();
        self.directory.update(cx, |d, cx| {
            d.update_user(
                &me,
                |user| {
                    user.props
                        .insert("customStatus".into(), serde_json::Value::String(stored));
                },
                cx,
            )
        });
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = match status {
                Some(status) => client.set_custom_status(status).await,
                None => client.clear_custom_status().await,
            };
            if let Err(err) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t update your status. {err:#}").into(),
                    })
                });
            }
        })
        .detach();
    }

    /// Add `channel` to the user's favorites, or remove it.
    pub fn set_favorite(&mut self, channel: &ChannelId, favorite: bool, cx: &mut Context<Self>) {
        let changed = if favorite {
            self.favorites.insert(channel.clone())
        } else {
            self.favorites.remove(channel)
        };
        if !changed {
            return;
        }
        cx.notify();
        let client = self.client.clone();
        let me = self.me.id.clone();
        let channel = channel.clone();
        cx.spawn(async move |this, cx| {
            let preference =
                Preference::new(me.clone(), FAVORITE_CHANNEL, channel.as_str(), "true");
            // Removing deletes the preference, as the server's own clients do.
            let result = if favorite {
                client.save_preferences(&me, vec![preference]).await
            } else {
                client.delete_preferences(&me, vec![preference]).await
            };
            if let Err(err) = result {
                let _ = this.update(cx, |this, cx| {
                    if favorite {
                        this.favorites.remove(&channel);
                    } else {
                        this.favorites.insert(channel);
                    }
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t update your favorites. {err:#}").into(),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Leave `channel`. It leaves the sidebar at once and comes back if the
    /// server refuses.
    pub fn leave_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        let Some(entry) = self.channels.remove(channel) else {
            return;
        };
        cx.notify();
        let client = self.client.clone();
        let me = self.me.id.clone();
        let team = self.team.clone();
        let channel = channel.clone();
        cx.spawn(async move |this, cx| {
            let result = client.leave_channel(&channel, &me).await;
            let _ = this.update(cx, |this, cx| match result {
                // Its threads go once the server agrees, so a refused leave
                // has none to bring back.
                Ok(()) => this
                    .threads
                    .update(cx, |t, cx| t.keep_channels(|id| id != &channel, cx)),
                Err(err) => {
                    let label = this.channel_label(&entry, cx);
                    // Another team's channels have replaced its own since;
                    // they'll include it again when it is next shown.
                    if this.team == team {
                        this.channels.entry(channel).or_insert(entry);
                    }
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t leave {label}. {err:#}").into(),
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Run a server-side slash command in the active channel.
    pub fn execute_command(
        &self,
        channel: &ChannelId,
        command: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = self.client.clone();
        let channel = channel.clone();
        let team = self.team.clone().unwrap_or_default();
        cx.background_spawn(async move { client.execute_command(&channel, &team, &command).await })
    }

    /// Open the direct message channel with `user`, creating it if needed.
    pub fn open_direct_channel(
        &mut self,
        user: UserId,
        cx: &mut Context<Self>,
    ) -> Task<Result<ChannelId>> {
        if let Some(entry) = self
            .channels
            .values()
            .find(|entry| entry.channel.direct_partner(&self.me.id).as_ref() == Some(&user))
        {
            return Task::ready(Ok(entry.id().clone()));
        }
        let client = self.client.clone();
        let me = self.me.id.clone();
        cx.spawn(async move |this, cx| {
            let channel = client.direct_channel(&me, &user).await?;
            let member = client.my_channel_member(&channel.id).await.ok();
            let id = channel.id.clone();
            this.update(cx, |this, cx| {
                this.channels
                    .entry(id.clone())
                    .or_insert(ChannelEntry { channel, member });
                this.hydrated_channels.insert(id.clone());
                this.directory.update(cx, |d, cx| d.ensure([user], cx));
                cx.notify();
            })?;
            Ok(id)
        })
    }

    /// Find active people and cache their identity for opening conversations.
    pub fn search_people(&self, term: &str, cx: &mut Context<Self>) -> Task<Result<Vec<User>>> {
        let client = self.client.clone();
        let term = term.to_owned();
        let directory = self.directory.clone();
        cx.spawn(async move |_, cx| {
            let users: Vec<User> = client
                .search_users(&term, 50)
                .await?
                .into_iter()
                .filter(|user| user.delete_at == 0)
                .collect();
            directory.update(cx, |d, cx| d.insert(users.clone(), cx));
            Ok(users)
        })
    }

    /// A page of public channels available to join. The view also checks
    /// membership when displaying results because a live event can join one.
    pub fn discover_channels(
        &self,
        term: &str,
        page: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Vec<Channel>, bool)>> {
        let Some(team) = self.team.clone() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Choose a team to browse its channels."
            )));
        };
        let client = self.client.clone();
        let term = term.to_owned();
        cx.background_spawn(async move {
            let channels = if term.is_empty() {
                client.public_channels(&team, page, 100).await?
            } else {
                client.search_public_channels(&team, &term).await?
            };
            let more = term.is_empty() && channels.len() == 100;
            Ok((
                channels
                    .into_iter()
                    .filter(|channel| {
                        channel.team_id == team
                            && channel.kind == ChannelType::Open
                            && channel.delete_at == 0
                    })
                    .collect(),
                more,
            ))
        })
    }

    /// Create or reuse a group with 2–7 other people. The server is the
    /// authority for deduplication of an existing member set.
    pub fn open_group_channel(
        &mut self,
        mut users: Vec<UserId>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ChannelId>> {
        users.retain(|id| id != &self.me.id);
        users.sort();
        users.dedup();
        if !(2..=7).contains(&users.len()) {
            return Task::ready(Err(anyhow::anyhow!(
                "Choose 2–7 people for a group message."
            )));
        }
        let client = self.client.clone();
        let mut members = users.clone();
        members.push(self.me.id.clone());
        cx.spawn(async move |this, cx| {
            let channel = client.group_channel(&members).await?;
            let member = client.my_channel_member(&channel.id).await.ok();
            let id = channel.id.clone();
            this.update(cx, |this, cx| {
                this.channels
                    .entry(id.clone())
                    .or_insert(ChannelEntry { channel, member });
                this.hydrated_channels.insert(id.clone());
                this.group_members.insert(id.clone(), users.clone());
                this.directory.update(cx, |d, cx| d.ensure(users, cx));
                cx.notify();
            })?;
            Ok(id)
        })
    }

    /// Join before publishing the channel to the sidebar. A denied join
    /// leaves session membership unchanged.
    pub fn join_public_channel(
        &mut self,
        channel: Channel,
        cx: &mut Context<Self>,
    ) -> Task<Result<ChannelId>> {
        if channel.kind != ChannelType::Open
            || channel.delete_at != 0
            || Some(&channel.team_id) != self.team.as_ref()
        {
            return Task::ready(Err(anyhow::anyhow!(
                "Choose a public channel in the current team."
            )));
        }
        if self.channels.contains_key(&channel.id) {
            return Task::ready(Ok(channel.id));
        }
        let client = self.client.clone();
        let me = self.me.id.clone();
        cx.spawn(async move |this, cx| {
            let member = client.join_channel(&channel.id, &me).await?;
            let id = channel.id.clone();
            this.update(cx, |this, cx| -> Result<()> {
                if Some(&channel.team_id) != this.team.as_ref() {
                    anyhow::bail!("The team changed. Browse channels again.");
                }
                this.channels.insert(
                    id.clone(),
                    ChannelEntry {
                        channel,
                        member: Some(member),
                    },
                );
                this.hydrated_channels.insert(id.clone());
                cx.notify();
                Ok(())
            })??;
            Ok(id)
        })
    }

    /// Tell others the user is typing, at most once every few seconds.
    pub fn notify_typing(&mut self, kind: &TimelineKind) {
        let key = (
            kind.channel().clone(),
            kind.root().cloned().unwrap_or_default(),
        );
        let now = Instant::now();
        if self
            .last_typing_sent
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < TYPING_THROTTLE)
        {
            return;
        }
        self.last_typing_sent.insert(key, now);
        self.stream.send_typing(kind.channel(), kind.root());
    }

    /// Users currently typing in a channel or thread, excluding the user.
    pub fn typing_users(&self, kind: &TimelineKind) -> Vec<UserId> {
        let key = (
            kind.channel().clone(),
            kind.root().cloned().unwrap_or_default(),
        );
        let now = Instant::now();
        self.typing
            .get(&key)
            .map(|users| {
                users
                    .iter()
                    .filter(|(_, at)| now.duration_since(*at) < TYPING_TTL)
                    .map(|(user, _)| user.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ----- Real-time routing -----------------------------------------------

    fn handle_event(&mut self, event: Event, cx: &mut Context<Self>) {
        log::debug!("event: {event:?}");
        match event {
            Event::Connected => {
                self.connection = Connection::Online;
                // Anything could have happened while we were away, and a
                // load that gave up while the network was down can now work.
                if self.has_connected || std::mem::take(&mut self.load_failed) {
                    let me = self.me.id.clone();
                    self.directory
                        .update(cx, |d, cx| d.refresh_statuses(vec![me], cx));
                    if self.teams.is_empty() {
                        self.load_teams(cx);
                    } else {
                        self.load_channels(cx);
                    }
                    self.load_favorites(cx);
                    self.threads.update(cx, |t, cx| t.refresh(cx));
                }
                for timeline in self.live_timelines() {
                    timeline.update(cx, |t, cx| t.catch_up(cx));
                }
                self.has_connected = true;
                cx.notify();
            }
            Event::Disconnected => {
                self.connection = Connection::Offline;
                cx.notify();
            }
            Event::Posted { post, mentions, .. } => {
                let mentions_me = mentions.contains(&self.me.id);
                self.note_activity(&post, mentions_me, cx);
                let from_me = post.user_id == self.me.id;
                let post = Arc::new(post);
                self.route_post((*post).clone(), true, cx);
                if !from_me {
                    // A reply stops the author's typing indicator.
                    let key = (post.channel_id.clone(), post.root_id.clone());
                    if let Some(users) = self.typing.get_mut(&key) {
                        users.retain(|(u, _)| u != &post.user_id);
                    }
                    cx.emit(SessionEvent::Posted { post, mentions_me });
                }
            }
            Event::PostEdited(post) => self.route_edited(post, cx),
            Event::PostDeleted(post) => self.route_deleted(&post, cx),
            Event::ReactionAdded(reaction) => self.route_reaction(&reaction, true, cx),
            Event::ReactionRemoved(reaction) => self.route_reaction(&reaction, false, cx),
            Event::Typing {
                channel_id,
                parent_id,
                user_id,
            } => {
                if user_id == self.me.id {
                    return;
                }
                let users = self.typing.entry((channel_id, parent_id)).or_default();
                users.retain(|(u, _)| u != &user_id);
                users.push((user_id.clone(), Instant::now()));
                self.directory.update(cx, |d, cx| d.ensure([user_id], cx));
                self.schedule_typing_expiry(cx);
                cx.notify();
            }
            Event::PostUnread { state, .. } => {
                if state.user_id.is_empty() || state.user_id == self.me.id {
                    self.channel_sync
                        .entry(state.channel_id.clone())
                        .or_default()
                        .unread_hint = true;
                    self.refresh_channel_member(&state.channel_id, cx);
                }
            }
            Event::ChannelMemberUpdated(member) => self.apply_notify_props(member, cx),
            Event::ChannelViewed(channel) => {
                self.note_viewed_elsewhere(&[(channel, now_millis())], cx)
            }
            Event::ChannelsViewed(channels) => self.note_viewed_elsewhere(&channels, cx),
            Event::StatusChanged { user_id, status } => {
                self.directory
                    .update(cx, |d, cx| d.set_status(user_id, status, cx));
            }
            Event::UserUpdated(mut user) => {
                if user.id == self.me.id {
                    // Broadcast profiles are sanitized; users/me owns account defaults.
                    if user.notify_props.is_empty() {
                        user.notify_props = self.me.notify_props.clone();
                    }
                    self.me = Arc::new(user.clone());
                    cx.notify();
                }
                self.directory.update(cx, |d, cx| d.insert([user], cx));
            }
            Event::ChannelsChanged {
                name,
                team_id,
                user_id,
            } => {
                self.load_channels(cx);
                let in_team = team_id.is_empty() || self.team.as_ref() == Some(&team_id);
                let discovery_changed = matches!(
                    name.as_str(),
                    "channel_created"
                        | "channel_restored"
                        | "channel_deleted"
                        | "channel_converted"
                ) || (user_id == self.me.id
                    && matches!(
                        name.as_str(),
                        "user_added" | "user_removed" | "added_to_team" | "leave_team"
                    ));
                if in_team && discovery_changed {
                    cx.emit(SessionEvent::ChannelsChanged);
                }
            }
            Event::EmojiAdded(emoji) => {
                self.emoji
                    .update(cx, |store, cx| store.add(vec![emoji], cx));
            }
            Event::PreferencesChanged(preferences) => {
                let mut changed = false;
                for preference in preferences
                    .iter()
                    .filter(|p| p.category == FAVORITE_CHANNEL)
                {
                    let channel = ChannelId::from(preference.name.as_str());
                    changed |= match preference.favorite_channel() {
                        Some(channel) => self.favorites.insert(channel),
                        None => self.favorites.remove(&channel),
                    };
                }
                if changed {
                    cx.notify();
                }
            }
            Event::PreferencesDeleted(preferences) => {
                let mut changed = false;
                for preference in preferences
                    .iter()
                    .filter(|p| p.category == FAVORITE_CHANNEL)
                {
                    changed |= self
                        .favorites
                        .remove(&ChannelId::from(preference.name.as_str()));
                }
                if changed {
                    cx.notify();
                }
            }
            // The server mirrors the Favorites category into preferences.
            Event::SidebarCategoriesChanged => self.load_favorites(cx),
            Event::ThreadUpdated { thread, team_id } if self.is_current_team(&team_id) => {
                let root = thread.id.clone();
                self.threads.update(cx, |t, cx| t.apply_update(thread, cx));
                // A reply to the thread on screen is read as it arrives.
                if self.window_active && self.open_thread.as_ref() == Some(&root) {
                    self.threads.update(cx, |t, cx| t.mark_read(&root, cx));
                }
            }
            Event::ThreadRead {
                thread_id,
                channel_id,
                team_id,
                unread_replies,
                unread_mentions,
                ..
            } if self.is_current_team(&team_id) => {
                self.threads.update(cx, |t, cx| match thread_id {
                    Some(root) => t.note_read(&root, unread_replies, unread_mentions, cx),
                    None => t.note_all_read(channel_id.as_ref(), cx),
                });
            }
            Event::ThreadFollowChanged {
                thread_id,
                team_id,
                following,
            } if self.is_current_team(&team_id) => {
                self.threads
                    .update(cx, |t, cx| t.note_following(&thread_id, following, cx));
            }
            Event::Other { name, data, .. } => {
                cx.emit(SessionEvent::Raw {
                    name: name.into(),
                    data,
                });
            }
            _ => {}
        }
    }

    /// Clear unread state for channels read on this or another device.
    fn note_viewed_elsewhere(&mut self, channels: &[(ChannelId, Millis)], cx: &mut Context<Self>) {
        let mut changed = false;
        for (channel, at) in channels {
            // A read elsewhere may release local suppression, but an old view
            // echo cannot decide that: refetch after our queued writes settle.
            if self.unread_channel.as_ref() == Some(channel) {
                self.refresh_channel_member(channel, cx);
                continue;
            }
            if let Some(entry) = self.channels.get_mut(channel)
                && let Some(member) = entry.member.as_mut()
                && *at > member.last_viewed_at
            {
                changed = true;
                member.last_viewed_at = *at;
                if *at >= entry.channel.last_post_at {
                    member.mention_count = 0;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Update unread state for a new post.
    fn note_activity(&mut self, post: &Post, mentions_me: bool, cx: &mut Context<Self>) {
        self.dirty_member_snapshot(&post.channel_id);
        let Some(entry) = self.channels.get_mut(&post.channel_id) else {
            // A channel we don't know yet (new DM, just added).
            self.load_channels(cx);
            return;
        };
        entry.channel.last_post_at = entry.channel.last_post_at.max(post.create_at);
        entry.channel.total_msg_count += 1;
        if !post.is_reply() {
            entry.channel.last_root_post_at = entry.channel.last_root_post_at.max(post.create_at);
            entry.channel.total_msg_count_root += 1;
        }
        let from_me = post.user_id == self.me.id;
        let is_visible =
            self.window_active && self.active_channel.as_ref() == Some(&post.channel_id);
        if from_me || (is_visible && self.unread_channel.as_ref() != Some(&post.channel_id)) {
            let channel = post.channel_id.clone();
            if from_me {
                self.mark_viewed(&channel, cx);
            } else {
                self.auto_mark_viewed(&channel, cx);
            }
        } else if (mentions_me || entry.channel.kind == ChannelType::Direct)
            && let Some(member) = entry.member.as_mut()
        {
            member.mention_count += 1;
        }
        cx.notify();
    }

    fn route_post(&mut self, post: Post, from_event: bool, cx: &mut Context<Self>) {
        if post.user_id == self.me.id {
            let confirmed: Vec<String> = send_ids(&post).map(str::to_owned).collect();
            self.confirm_sends(&confirmed);
        }
        if post.is_reply() {
            if from_event
                && let Some(channel) =
                    self.live_timeline(&TimelineKind::Channel(post.channel_id.clone()))
            {
                let root = post.root_id.clone();
                channel.update(cx, |t, cx| t.note_reply(&root, cx));
            }
            let thread = TimelineKind::Thread {
                channel: post.channel_id.clone(),
                root: post.root_id.clone(),
            };
            if let Some(thread) = self.live_timeline(&thread) {
                thread.update(cx, |t, cx| t.upsert(post, cx));
            }
        } else {
            if let Some(channel) =
                self.live_timeline(&TimelineKind::Channel(post.channel_id.clone()))
            {
                channel.update(cx, |t, cx| t.upsert(post, cx));
            }
        }
    }

    fn route_edited(&mut self, post: Post, cx: &mut Context<Self>) {
        if !post.is_reply() {
            self.threads
                .update(cx, |t, cx| t.note_root_edited(&post, cx));
        }
        let channel = TimelineKind::Channel(post.channel_id.clone());
        let thread = TimelineKind::Thread {
            channel: post.channel_id.clone(),
            root: if post.is_reply() {
                post.root_id.clone()
            } else {
                post.id.clone()
            },
        };
        for kind in [channel, thread] {
            if let Some(timeline) = self.live_timeline(&kind) {
                let post = post.clone();
                timeline.update(cx, |t, cx| {
                    if t.position(&post.id).is_some() {
                        t.upsert(post, cx);
                    }
                });
            }
        }
    }

    fn route_deleted(&mut self, post: &Post, cx: &mut Context<Self>) {
        if let Some(confirmed) = self.deleting.get_mut(&post.id) {
            *confirmed = true;
        }
        if !post.is_reply() {
            self.threads.update(cx, |t, cx| t.forget(&post.id, cx));
        }
        for timeline in self.live_timelines() {
            timeline.update(cx, |t, cx| t.delete(post, cx));
        }
    }

    fn route_reaction(&mut self, reaction: &Reaction, added: bool, cx: &mut Context<Self>) {
        for timeline in self.live_timelines() {
            timeline.update(cx, |t, cx| t.apply_reaction(reaction, added, cx));
        }
    }

    fn schedule_typing_expiry(&mut self, cx: &mut Context<Self>) {
        self._typing_expiry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_TTL).await;
            let _ = this.update(cx, |this, cx| {
                let now = Instant::now();
                for users in this.typing.values_mut() {
                    users.retain(|(_, at)| now.duration_since(*at) < TYPING_TTL);
                }
                this.typing.retain(|_, users| !users.is_empty());
                cx.notify();
            });
        }));
    }

    /// Whether every live timeline is loaded, for tests and diagnostics.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.live_timelines()
            .iter()
            .all(|t| matches!(t.read(cx).state(), LoadState::Loaded | LoadState::Failed(_)))
    }
}

/// Startup against a server behind a slow link: every response takes one
/// round trip, and what can overlap must.
#[cfg(test)]
mod slow_network_tests {
    use mattermost::{ApiError, ChannelId, ChannelType};
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use gpui_kit::{AppContext as _, Entity, SharedString, TestAppContext};
    use mattermost::{
        Client, PostId, TeamId, User,
        testing::{Gate, Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::{Session, SessionEvent};
    use crate::{EmojiStore, LoadState, ThreadFilter, TimelineKind};

    fn route(request: &Request) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        match path {
            "/api/v4/users/me" => Reply::json(json!({ "id": "me", "username": "me" })),
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
            "/api/v4/channels/town/posts" => Reply::json(json!({
                "order": ["p1"],
                "posts": { "p1": { "id": "p1", "channel_id": "town", "user_id": "ann",
                                   "message": "hello", "create_at": 1 } },
            })),
            "/api/v4/users/ids" => Reply::json(json!([{ "id": "ann", "username": "ann" }])),
            "/api/v4/users/status/ids" => {
                Reply::json(json!([{ "user_id": "ann", "status": "online" }]))
            }
            "/api/v4/users/me/preferences/favorite_channel" | "/api/v4/emoji" => {
                Reply::json(json!([]))
            }
            _ => Reply::not_found(),
        }
    }

    fn me() -> User {
        let mut me = User::default();
        me.id = "me".into();
        me
    }

    fn start(server: &SlowServer, cx: &mut TestAppContext) -> Entity<Session> {
        cx.executor().allow_parking();
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        cx.new(|cx| Session::new(client, me(), Some(TeamId::from("team")), cx))
    }

    /// Run GPUI's work and let real network I/O happen until `done`, or
    /// fail after a real-time deadline. GPUI's clock (batching windows,
    /// retry backoff) moves forward while waiting.
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

    fn requested(server: &SlowServer, method: &str, path: &str) -> bool {
        server
            .requests()
            .iter()
            .any(|r| r.method() == method && r.path().split('?').next() == Some(path))
    }

    fn finish<T: 'static>(task: gpui_kit::Task<T>, cx: &mut TestAppContext) -> T {
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = result.clone();
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
            .detach();
        wait_until(cx, |_| result.borrow().is_some());
        result.borrow_mut().take().unwrap()
    }

    #[gpui_kit::test]
    fn discovery_invalidations_ignore_unrelated_channel_events(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let invalidations = Rc::new(std::cell::Cell::new(0));
        let seen = invalidations.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&session, move |_, event, _| {
                if matches!(event, SessionEvent::ChannelsChanged) {
                    seen.set(seen.get() + 1);
                }
            })
        });
        for (name, team, user, expected) in [
            ("channel_updated", "team", "", 0),
            ("user_added", "team", "ann", 0),
            ("user_removed", "team", "ben", 0),
            ("channel_created", "other", "", 0),
            ("user_added", "other", "me", 0),
            ("direct_added", "", "me", 0),
            ("channel_created", "team", "", 1),
            ("user_added", "team", "me", 2),
            ("user_removed", "team", "me", 3),
            ("channel_deleted", "team", "", 4),
        ] {
            session.update(cx, |s, cx| {
                s.handle_event(
                    mattermost::Event::ChannelsChanged {
                        name: name.into(),
                        team_id: team.into(),
                        user_id: user.into(),
                    },
                    cx,
                )
            });
            cx.run_until_parked();
            assert_eq!(invalidations.get(), expected, "{name} in {team} for {user}");
        }
    }

    #[gpui_kit::test]
    fn discovery_opens_searched_people_and_reuses_group_channels(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(10)), |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("POST", "/api/v4/users/search") => Reply::json(json!([
                    { "id": "ann", "username": "ann", "first_name": "Ann" },
                    { "id": "gone", "username": "gone", "delete_at": 1 }
                ])),
                ("POST", "/api/v4/channels/direct") => {
                    Reply::json(json!({ "id": "dm", "type": "D", "name": "me__ann" }))
                }
                ("POST", "/api/v4/channels/group") => Reply::json(
                    json!({ "id": "group", "type": "G", "display_name": "ann, ben, me" }),
                ),
                ("GET", "/api/v4/channels/dm/members/me" | "/api/v4/channels/group/members/me") => {
                    Reply::json(json!({ "user_id": "me" }))
                }
                _ => route(request),
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let users = session.update(cx, |s, cx| s.search_people("ann", cx));
        let users = finish(users, cx).unwrap();
        assert_eq!(users.len(), 1);
        session.read_with(cx, |s, cx| {
            assert!(s.directory.read(cx).user(&users[0].id).is_some())
        });
        for _ in 0..2 {
            let task = session.update(cx, |s, cx| s.open_direct_channel(users[0].id.clone(), cx));
            assert_eq!(finish(task, cx).unwrap(), ChannelId::from("dm"));
            let task = session.update(cx, |s, cx| {
                s.open_group_channel(
                    vec!["ann".into(), "ben".into(), "ann".into(), "me".into()],
                    cx,
                )
            });
            assert_eq!(finish(task, cx).unwrap(), ChannelId::from("group"));
        }
        session.read_with(cx, |s, _| {
            assert_eq!(
                s.channels()
                    .filter(|c| c.kind() == ChannelType::Direct)
                    .count(),
                1
            );
            assert_eq!(
                s.channels()
                    .filter(|c| c.kind() == ChannelType::Group)
                    .count(),
                1
            );
            assert_eq!(s.group_members[&ChannelId::from("group")].len(), 2);
        });
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "POST" && r.path() == "/api/v4/channels/direct")
                .count(),
            1
        );
        let invalid = session.update(cx, |s, cx| s.open_group_channel(vec!["ann".into()], cx));
        assert!(finish(invalid, cx).is_err());
    }

    #[gpui_kit::test]
    fn discovery_lists_joins_and_keeps_denied_channels_out_of_membership(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(10)), |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("GET", "/api/v4/teams/team/channels")
                | ("POST", "/api/v4/teams/team/channels/search") => Reply::json(json!([
                    { "id": "public", "team_id": "team", "type": "O", "name": "design" },
                    { "id": "denied", "team_id": "team", "type": "O", "name": "restricted" },
                    { "id": "archived", "team_id": "team", "type": "O", "delete_at": 1 },
                    { "id": "private", "team_id": "team", "type": "P" },
                    { "id": "other", "team_id": "other", "type": "O" }
                ])),
                ("POST", "/api/v4/channels/public/members") => {
                    Reply::json(json!({ "channel_id": "public", "user_id": "me" }))
                }
                ("POST", "/api/v4/channels/denied/members") => {
                    Reply::json(json!({ "message": "No permission" })).status(403)
                }
                _ => route(request),
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        for term in ["", "design"] {
            let task = session.update(cx, |s, cx| s.discover_channels(term, 0, cx));
            let (channels, more) = finish(task, cx).unwrap();
            assert!(!more);
            assert_eq!(channels.len(), 2);
            let join = session.update(cx, |s, cx| s.join_public_channel(channels[0].clone(), cx));
            assert_eq!(finish(join, cx).unwrap(), ChannelId::from("public"));
            let denied = session.update(cx, |s, cx| s.join_public_channel(channels[1].clone(), cx));
            let error = finish(denied, cx).unwrap_err();
            assert_eq!(
                error.downcast_ref::<ApiError>(),
                Some(&ApiError::Status(403))
            );
        }
        session.read_with(cx, |s, _| {
            assert!(s.channel(&"public".into()).is_some());
            assert!(s.channel(&"denied".into()).is_none());
        });
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "POST" && r.path() == "/api/v4/channels/public/members")
                .count(),
            1
        );
    }

    #[gpui_kit::test]
    fn reconnect_merges_missed_posts_without_undoing_live_changes(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(100)), |request| {
            if request.path().starts_with("/api/v4/channels/town/posts") {
                let catching_up = request.path().contains("since=");
                let mut posts = json!({
                    "a": { "id": "a", "channel_id": "town", "create_at": 1000,
                           "update_at": if catching_up { 3000 } else { 1000 }, "message": "snapshot" },
                    "d": { "id": "d", "channel_id": "town", "create_at": 1500 },
                    "p": { "id": "p", "channel_id": "town", "create_at": 2000,
                           "update_at": if catching_up { 4000 } else { 2000 } },
                });
                let mut order = vec!["a", "d", "p"];
                if catching_up {
                    posts["missed"] =
                        json!({ "id": "missed", "channel_id": "town", "create_at": 2500 });
                    order.push("missed");
                }
                return Reply::json(json!({ "order": order, "posts": posts }));
            }
            route(request)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        timeline.update(cx, |t, cx| {
            let mut pending = mattermost::Post::default();
            pending.id = "pending".into();
            pending.channel_id = "town".into();
            pending.create_at = 9_000_000;
            t.insert_pending(pending, cx);
            t.set_delivery(&"pending".into(), crate::Delivery::Failed, cx);
            t.catch_up(cx);
        });
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.path().contains("since="))
        });
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.path().contains("since=1000"))
        );
        timeline.update(cx, |t, cx| {
            let mut edited = mattermost::Post::default();
            edited.id = "a".into();
            edited.channel_id = "town".into();
            edited.create_at = 1000;
            edited.edit_at = 5000;
            edited.message = "live edit".into();
            t.upsert(edited, cx);
            let mut deleted = mattermost::Post::default();
            deleted.id = "d".into();
            deleted.channel_id = "town".into();
            t.delete(&deleted, cx);
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.position(&"missed".into()).is_some())
        });
        timeline.read_with(cx, |t, _| {
            assert_eq!(
                t.entries()
                    .iter()
                    .map(|e| e.id().as_str())
                    .collect::<Vec<_>>(),
                ["a", "p", "missed", "pending"]
            );
            assert_eq!(t.entries()[0].post().message, "live edit");
            assert_eq!(t.entries()[3].delivery(), crate::Delivery::Failed);
        });
    }

    #[gpui_kit::test]
    fn retry_reuses_the_pending_id_and_reconciles_a_late_echo(cx: &mut TestAppContext) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::from_millis(100)), {
            let attempts = attempts.clone();
            move |request| {
                if request.path() == "/api/v4/posts" && request.method() == "POST" {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        // The write committed, but its response was lost.
                        return Reply::json(json!({ "message": "Lost response" })).status(500);
                    }
                    let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    return Reply::json(json!({
                        "id": "committed", "channel_id": "town", "root_id": "root",
                        "user_id": "me", "create_at": 10, "update_at": 11, "message": body["message"],
                        "pending_post_id": body["pending_post_id"],
                    }));
                }
                if request.path().starts_with("/api/v4/posts/root/thread") {
                    return Reply::json(json!({ "order": [], "posts": {} }));
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let kind = TimelineKind::Thread {
            channel: "town".into(),
            root: "root".into(),
        };
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        session.update(cx, |s, cx| {
            let mut file = mattermost::FileInfo::default();
            file.id = "file".into();
            s.send_post(&kind, "Retry me".into(), vec![file], cx);
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| {
                t.entries()
                    .iter()
                    .any(|e| e.delivery() == crate::Delivery::Failed)
            })
        });
        let pending = timeline.read_with(cx, |t, _| t.entries()[0].id().clone());
        session.update(cx, |s, cx| {
            s.retry_post(&kind, &pending, cx);
            // A second click must not submit the pending retry again.
            s.retry_post(&kind, &pending, cx);
        });
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries()[0].id(), &pending);
            assert_eq!(t.entries()[0].delivery(), crate::Delivery::Pending);
        });
        wait_until(cx, |_| attempts.load(Ordering::SeqCst) == 2);
        session.update(cx, |s, cx| {
            let mut echo = mattermost::Post::default();
            echo.id = "committed".into();
            echo.channel_id = "town".into();
            echo.root_id = "root".into();
            echo.user_id = "me".into();
            echo.create_at = 10;
            echo.message = "Retry me".into();
            echo.pending_post_id = pending.to_string();
            s.route_post(echo, true, cx);
        });
        // Wait for the retry's REST response too; both confirmations are idempotent.
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.entries()[0].post().update_at == 11)
        });
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries().len(), 1);
            assert_eq!(t.entries()[0].id().as_str(), "committed");
            assert_eq!(t.entries()[0].delivery(), crate::Delivery::Sent);
        });
        let requests = server.requests();
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .filter(|r| r.path() == "/api/v4/posts" && r.method() == "POST")
            .map(|r| serde_json::from_slice(r.body()).unwrap())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[1]["pending_post_id"], pending.as_str());
        assert_eq!(bodies[1]["root_id"], "root");
        assert_eq!(bodies[1]["file_ids"], json!(["file"]));
    }

    #[gpui_kit::test]
    fn a_rejected_optimistic_delete_is_restored_immediately(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), |request| {
            if request.method() == "DELETE" && request.path() == "/api/v4/posts/p1" {
                return Reply::json(json!({ "message": "Forbidden" })).status(403);
            }
            route(request)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        session
            .read_with(cx, |s, _| s.threads().clone())
            .update(cx, |t, cx| {
                t.apply_update(
                    serde_json::from_value(json!({
                        "id": "p1", "reply_count": 2, "last_reply_at": 20,
                        "unread_replies": 2, "unread_mentions": 1,
                        "post": {"id": "p1", "channel_id": "town", "create_at": 1}
                    }))
                    .unwrap(),
                    cx,
                );
            });
        let post = timeline.read_with(cx, |t, _| t.entries()[0].post().as_ref().clone());
        let failures = failures(&session, cx);
        session.update(cx, |s, cx| s.delete_post(&post, cx));
        timeline.read_with(cx, |t, _| assert!(t.entries().is_empty()));
        wait_until(cx, |_| !failures.borrow().is_empty());
        timeline.read_with(cx, |t, _| assert!(t.position(&post.id).is_some()));
        session.read_with(cx, |s, cx| {
            let thread = s.threads().read(cx).thread(&post.id).unwrap();
            assert_eq!(thread.unread_replies(), 2);
            assert_eq!(thread.unread_mentions(), 1);
        });
        assert!(
            !server
                .requests()
                .iter()
                .any(|r| r.method() == "GET" && r.path() == "/api/v4/posts/p1")
        );
    }

    #[gpui_kit::test]
    fn outbox_survives_eviction_retry_and_discard(cx: &mut TestAppContext) {
        let succeeds = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::from_millis(30)), {
            let succeeds = succeeds.clone();
            move |request| {
                if request.method() == "POST" && request.path() == "/api/v4/posts" {
                    if !succeeds.load(Ordering::SeqCst) {
                        return Reply::json(json!({"message": "Forbidden"})).status(403);
                    }
                    let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    return Reply::json(
                        json!({"id": "sent", "user_id": "me", "channel_id": "town",
                        "root_id": "root", "pending_post_id": body["pending_post_id"], "create_at": 10}),
                    );
                }
                if request.method() == "GET"
                    && request.path().starts_with("/api/v4/posts/root/thread")
                {
                    return Reply::json(json!({}));
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        let kind = TimelineKind::Thread {
            channel: "town".into(),
            root: "root".into(),
        };
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        let weak = timeline.downgrade();
        let pending = session.update(cx, |s, cx| {
            let mut file = mattermost::FileInfo::default();
            file.id = "file".into();
            s.send_post(&kind, "Keep me".into(), vec![file], cx);
            s.outbox[0].id.clone()
        });
        // The in-flight send must not keep the timeline alive either.
        drop(timeline);
        session.update(cx, |s, cx| {
            for ix in 0..super::TIMELINE_CACHE {
                s.timeline(TimelineKind::Channel(format!("other-{ix}").into()), cx);
            }
        });
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "timeline was evicted");
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| !s.sending.contains(&pending))
        });
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        timeline.read_with(cx, |t, _| {
            let entry = &t.entries()[t.position(&pending).unwrap()];
            assert_eq!(entry.delivery(), crate::Delivery::Failed);
            assert_eq!(entry.post().message, "Keep me");
            assert_eq!(entry.post().metadata.files[0].id, "file");
            assert_eq!(entry.post().root_id.as_str(), "root");
        });
        succeeds.store(true, Ordering::SeqCst);
        session.update(cx, |s, cx| s.retry_post(&kind, &pending, cx));
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.outbox.is_empty()));
        timeline.read_with(cx, |t, _| {
            assert!(t.position(&pending).is_none());
            assert!(t.position(&"sent".into()).is_some());
        });
        let bodies: Vec<serde_json::Value> = server
            .requests()
            .iter()
            .filter(|r| r.method() == "POST" && r.path() == "/api/v4/posts")
            .map(|r| serde_json::from_slice(r.body()).unwrap())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);

        succeeds.store(false, Ordering::SeqCst);
        session.update(cx, |s, cx| {
            s.send_post(&kind, "Discard me".into(), vec![], cx)
        });
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.sending.is_empty()));
        let weak = timeline.downgrade();
        drop(timeline);
        session.update(cx, |s, cx| {
            for ix in 0..super::TIMELINE_CACHE {
                s.timeline(TimelineKind::Channel(format!("other-{ix}").into()), cx);
            }
        });
        cx.run_until_parked();
        assert!(
            weak.upgrade().is_none(),
            "failed entry's timeline was evicted"
        );
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        let pending = session.read_with(cx, |s, _| s.outbox[0].id.clone());
        session.update(cx, |s, cx| s.discard_post(&kind, &pending, cx));
        assert!(session.read_with(cx, |s, _| s.outbox.is_empty()));
        timeline.read_with(cx, |t, _| assert!(t.position(&pending).is_none()));
    }

    #[gpui_kit::test]
    fn a_rest_catch_up_confirms_the_session_outbox(cx: &mut TestAppContext) {
        let committed = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), {
            let committed = committed.clone();
            move |request| {
                if request.method() == "POST" && request.path() == "/api/v4/posts" {
                    let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    *committed.lock().unwrap() = Some(json!({
                        "id": "confirmed", "channel_id": "town", "user_id": "me", "create_at": 2,
                        "props": body["props"], "message": body["message"]
                    }));
                    return Reply::json(json!({"message": "Response lost after commit"}))
                        .status(500);
                }
                if request.method() == "GET"
                    && request.path().starts_with("/api/v4/channels/town/posts?")
                    && request.path().contains("since=")
                {
                    return Reply::json(
                        json!({"posts": {"confirmed": committed.lock().unwrap().clone().unwrap()}}),
                    );
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        let kind = TimelineKind::Channel("town".into());
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        session.update(cx, |s, cx| {
            s.send_post(
                &kind,
                "Committed despite the lost response".into(),
                vec![],
                cx,
            )
        });
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.sending.is_empty()));
        let pending = session.read_with(cx, |s, _| s.outbox[0].id.clone());
        assert!(
            committed
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .get("pending_post_id")
                .is_none()
        );
        timeline.update(cx, |t, cx| t.catch_up(cx));
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.outbox.is_empty()));
        timeline.read_with(cx, |t, _| {
            assert!(t.position(&pending).is_none());
            assert!(t.position(&"confirmed".into()).is_some());
        });
    }

    #[gpui_kit::test]
    fn deduplicated_retry_removes_its_optimistic_twin(cx: &mut TestAppContext) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::from_millis(30)), {
            let attempts = attempts.clone();
            move |request| {
                if request.method() == "POST" && request.path() == "/api/v4/posts" {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Reply::json(json!({"message": "Lost response"})).status(500);
                    }
                    return Reply::json(
                        json!({"id": "S", "channel_id": "town", "user_id": "me", "create_at": 2}),
                    );
                }
                if request.method() == "GET"
                    && request.path().split('?').next() == Some("/api/v4/channels/town/posts")
                    && attempts.load(Ordering::SeqCst) > 0
                {
                    return Reply::json(
                        json!({"posts": {"S": {"id": "S", "channel_id": "town", "user_id": "me", "create_at": 2}}}),
                    );
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        let kind = TimelineKind::Channel("town".into());
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        session.update(cx, |s, cx| s.send_post(&kind, "Once".into(), vec![], cx));
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.sending.is_empty()));
        let pending = session.read_with(cx, |s, _| s.outbox[0].id.clone());
        timeline.update(cx, |t, cx| t.reload(cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.position(&"S".into()), Some(0));
            assert_eq!(t.position(&pending), Some(1));
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&timeline, move |_, event: &crate::TimelineEvent, _| {
                events.borrow_mut().push(event.clone())
            })
            .detach();
        });
        session.update(cx, |s, cx| s.retry_post(&kind, &pending, cx));
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.outbox.is_empty()));
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries().len(), 1);
            assert_eq!(t.entries()[0].id().as_str(), "S");
        });
        assert!(events.borrow().contains(&crate::TimelineEvent::Spliced {
            old: 1..2,
            new_count: 0
        }));
        assert!(!events.borrow().contains(&crate::TimelineEvent::Reset));
    }

    fn rollback_stays_in_original_timelines(status: u16, cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(40)), move |request| {
            let path = request.path().split('?').next().unwrap_or_default();
            match (request.method(), path) {
                ("GET", "/api/v4/posts/old/thread") => Reply::json(json!({"posts": {"old": {
                    "id": "old", "channel_id": "town", "create_at": 1, "message": "Old root"
                }}})),
                ("GET", "/api/v4/channels/town/posts") if request.path().contains("before=") => {
                    Reply::json(json!({}))
                }
                ("GET", "/api/v4/channels/town/posts") => {
                    let posts: serde_json::Map<String, serde_json::Value> = (60..120).map(|i| {
                        (format!("n{i}"), json!({"id": format!("n{i}"), "channel_id": "town", "create_at": i}))
                    }).collect();
                    Reply::json(json!({"posts": posts}))
                }
                ("DELETE", "/api/v4/posts/old") => {
                    Reply::json(json!({"message": "Delete failed"})).status(status)
                }
                ("GET", "/api/v4/posts/old") => {
                    Reply::json(json!({"id": "old", "channel_id": "town", "create_at": 1}))
                }
                _ => route(request),
            }
        });
        let session = start(&server, cx);
        let (channel, thread) = session.update(cx, |s, cx| {
            (
                s.timeline(TimelineKind::Channel("town".into()), cx),
                s.timeline(
                    TimelineKind::Thread {
                        channel: "town".into(),
                        root: "old".into(),
                    },
                    cx,
                ),
            )
        });
        wait_until(cx, |cx| {
            channel.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
                && thread.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        let post = thread.read_with(cx, |t, _| t.entries()[0].post().as_ref().clone());
        let failures = failures(&session, cx);
        session.update(cx, |s, cx| s.delete_post(&post, cx));
        wait_until(cx, |_| !failures.borrow().is_empty());
        thread.read_with(cx, |t, _| assert!(t.position(&post.id).is_some()));
        channel.read_with(cx, |t, _| {
            assert_eq!(t.entries().len(), 60);
            assert_eq!(t.entries()[0].id().as_str(), "n60");
            assert!(t.position(&post.id).is_none());
        });
        channel.update(cx, |t, cx| t.load_older(cx));
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "GET" && r.path().contains("before=n60"))
        });
        assert!(
            !server
                .requests()
                .iter()
                .any(|r| r.method() == "GET" && r.path().contains("before=old"))
        );
    }

    #[gpui_kit::test]
    fn rejected_delete_restores_only_timelines_that_removed_it(cx: &mut TestAppContext) {
        rollback_stays_in_original_timelines(403, cx);
    }

    #[gpui_kit::test]
    fn ambiguous_delete_restores_only_timelines_that_removed_it(cx: &mut TestAppContext) {
        rollback_stays_in_original_timelines(500, cx);
    }

    fn first_hello(load_finished: bool, live_during_load: bool, cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(100)), |request| {
            if request.method() == "GET"
                && request.path().starts_with("/api/v4/channels/town/posts?")
                && request.path().contains("since=")
            {
                assert!(
                    request.path().contains("since=0"),
                    "socket traffic skipped the gap: {}",
                    request.path()
                );
                return Reply::json(json!({"order": ["missed"], "posts": {
                    "missed": {"id": "missed", "channel_id": "town", "create_at": 2}
                }}));
            }
            route(request)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        if load_finished {
            wait_until(cx, |cx| {
                timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
            });
        } else {
            wait_until(cx, |_| {
                server.requests().iter().any(|r| {
                    r.method() == "GET" && r.path().starts_with("/api/v4/channels/town/posts?")
                })
            });
            assert!(timeline.read_with(cx, |t, _| *t.state() == LoadState::Loading));
        }
        session.update(cx, |s, cx| {
            assert!(!s.has_connected);
            s.handle_event(mattermost::Event::Connected, cx);
            if live_during_load {
                let mut post = mattermost::Post::default();
                post.id = "live".into();
                post.channel_id = "town".into();
                post.create_at = 100_000;
                s.route_post(post, true, cx);
            }
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.position(&"missed".into()).is_some())
        });
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries().len(), if live_during_load { 3 } else { 2 })
        });
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "GET"
                    && r.path().starts_with("/api/v4/channels/town/posts?")
                    && r.path().contains("since="))
                .count(),
            1
        );
    }

    #[gpui_kit::test]
    fn first_hello_catches_up_loaded_timelines(cx: &mut TestAppContext) {
        first_hello(true, false, cx);
    }

    #[gpui_kit::test]
    fn first_hello_waits_for_initial_loads(cx: &mut TestAppContext) {
        first_hello(false, false, cx);
    }

    #[gpui_kit::test]
    fn first_hello_does_not_skip_the_gap_when_live_posts_arrive_during_load(
        cx: &mut TestAppContext,
    ) {
        first_hello(false, true, cx);
    }

    #[gpui_kit::test]
    fn first_hello_queues_a_thread_refresh_after_its_initial_load(cx: &mut TestAppContext) {
        let reads = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::from_millis(100)), {
            let reads = reads.clone();
            move |request| {
                if request.method() == "GET"
                    && request.path().split('?').next() == Some("/api/v4/posts/root/thread")
                {
                    let refreshed = reads.fetch_add(1, Ordering::SeqCst) > 0;
                    let mut posts =
                        json!({"root": {"id": "root", "channel_id": "town", "create_at": 1}});
                    if refreshed {
                        posts["missed"] = json!({"id": "missed", "channel_id": "town", "root_id": "root", "create_at": 2});
                    }
                    return Reply::json(json!({"posts": posts}));
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(
                TimelineKind::Thread {
                    channel: "town".into(),
                    root: "root".into(),
                },
                cx,
            )
        });
        wait_until(cx, |_| reads.load(Ordering::SeqCst) == 1);
        assert!(timeline.read_with(cx, |t, _| *t.state() == LoadState::Loading));
        session.update(cx, |s, cx| s.handle_event(mattermost::Event::Connected, cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.position(&"missed".into()).is_some())
        });
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[gpui_kit::test]
    fn successful_rest_send_without_echo_id_clears_the_outbox(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), |request| {
            if request.method() == "POST" && request.path() == "/api/v4/posts" {
                return Reply::json(json!({"id": "sent", "channel_id": "town", "create_at": 5}));
            }
            route(request)
        });
        let session = start(&server, cx);
        let kind = TimelineKind::Channel("town".into());
        let timeline = session.update(cx, |s, cx| {
            s.send_post(&kind, "Hello".into(), vec![], cx);
            s.timeline(kind, cx)
        });
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.outbox.is_empty()));
        timeline.read_with(cx, |t, _| {
            assert!(t.position(&"sent".into()).is_some());
            assert!(
                t.entries()
                    .iter()
                    .all(|e| e.delivery() == crate::Delivery::Sent)
            );
        });
    }

    #[gpui_kit::test]
    fn a_confirmed_socket_delete_wins_over_a_late_rejection(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(50)), |request| {
            if request.method() == "DELETE" && request.path() == "/api/v4/posts/p1" {
                return Reply::json(json!({"message": "Already gone"})).status(403);
            }
            route(request)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        let post = timeline.read_with(cx, |t, _| t.entries()[0].post().as_ref().clone());
        let failures = failures(&session, cx);
        session.update(cx, |s, cx| {
            s.delete_post(&post, cx);
            s.handle_event(mattermost::Event::PostDeleted(post.clone()), cx);
            // A second click must not overwrite the socket confirmation.
            s.delete_post(&post, cx);
        });
        wait_until(cx, |_| !failures.borrow().is_empty());
        timeline.read_with(cx, |t, _| assert!(t.entries().is_empty()));
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "DELETE" && r.path() == "/api/v4/posts/p1")
                .count(),
            1
        );
    }

    fn ambiguous_delete(deleted: bool, cx: &mut TestAppContext) {
        let server =
            SlowServer::start(
                Network::new(Duration::from_millis(20)),
                move |request| match (request.method(), request.path()) {
                    ("DELETE", "/api/v4/posts/p1") => {
                        Reply::json(json!({"message": "Lost response"})).status(500)
                    }
                    ("GET", "/api/v4/posts/p1") if deleted => Reply::not_found(),
                    ("GET", "/api/v4/posts/p1") => {
                        Reply::json(json!({"id": "p1", "channel_id": "town",
                    "message": "fresh after ambiguous delete", "create_at": 1, "update_at": 2}))
                    }
                    _ => route(request),
                },
            );
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| *t.state() == LoadState::Loaded)
        });
        let post = timeline.read_with(cx, |t, _| t.entries()[0].post().as_ref().clone());
        let failures = failures(&session, cx);
        session.update(cx, |s, cx| s.delete_post(&post, cx));
        wait_until(cx, |_| !failures.borrow().is_empty());
        timeline.read_with(cx, |t, _| {
            if deleted {
                assert!(t.entries().is_empty());
            } else {
                assert_eq!(
                    t.entries()[0].post().message,
                    "fresh after ambiguous delete"
                );
            }
        });
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "GET" && r.path() == "/api/v4/posts/p1")
                .count(),
            1
        );
    }

    #[gpui_kit::test]
    fn ambiguous_delete_refetches_current_post(cx: &mut TestAppContext) {
        ambiguous_delete(false, cx);
    }

    #[gpui_kit::test]
    fn ambiguous_committed_delete_stays_deleted(cx: &mut TestAppContext) {
        ambiguous_delete(true, cx);
    }

    #[gpui_kit::test]
    fn sanitized_self_updates_preserve_account_notification_defaults(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let session = start(&server, cx);
        session.update(cx, |s, cx| {
            let mut user = s.me().as_ref().clone();
            user.notify_props.insert("desktop".into(), "none".into());
            s.me = Arc::new(user.clone());
            user.notify_props.clear();
            user.last_picture_update = 12;
            s.handle_event(mattermost::Event::UserUpdated(user), cx);
            assert_eq!(s.me().notify_props["desktop"], "none");
            assert_eq!(s.me().last_picture_update, 12);
            let directory_me = s.directory.read(cx).user(&s.me().id).unwrap();
            assert_eq!(directory_me.notify_props["desktop"], "none");
        });
    }

    fn state_member() -> Arc<std::sync::Mutex<mattermost::ChannelMember>> {
        Arc::new(std::sync::Mutex::new(
            serde_json::from_value(json!({
                "channel_id":"town", "user_id":"me", "last_viewed_at":1000
            }))
            .unwrap(),
        ))
    }

    fn state_route(
        request: &Request,
        state: &std::sync::Mutex<mattermost::ChannelMember>,
    ) -> Reply {
        let mut member = state.lock().unwrap();
        match (request.method(), request.path()) {
            ("POST", "/api/v4/channels/members/me/view") => {
                member.last_viewed_at = 1000;
                member.mention_count = 0;
                member.mention_count_root = 0;
                Reply::json(json!({}))
            }
            ("POST", "/api/v4/users/me/posts/p1/set_unread") => {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(request.body()).unwrap(),
                    json!({"collapsed_threads_supported":true})
                );
                member.last_viewed_at = 0;
                member.msg_count = 7;
                member.msg_count_root = 4;
                member.mention_count = 2;
                member.mention_count_root = 1;
                Reply::json(serde_json::to_value(&*member).unwrap())
            }
            ("GET", "/api/v4/channels/town/members/me") => {
                Reply::json(serde_json::to_value(&*member).unwrap())
            }
            ("GET", "/api/v4/users/me/teams/team/channels/members") => {
                Reply::json(json!([&*member]))
            }
            ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                {"id":"town", "team_id":"team", "name":"town-square", "type":"O", "last_post_at":1}
            ])),
            ("PUT", "/api/v4/channels/town/members/me/notify_props") => {
                let props: std::collections::HashMap<String, String> =
                    serde_json::from_slice(request.body()).unwrap();
                member.notify_props.extend(props);
                Reply::json(serde_json::to_value(&*member).unwrap())
            }
            _ => route(request),
        }
    }

    fn unread_post() -> mattermost::Post {
        let mut post = mattermost::Post::default();
        post.id = "p1".into();
        post.channel_id = "town".into();
        post.user_id = "ann".into();
        post.create_at = 1;
        post
    }

    fn unread_event() -> mattermost::Event {
        // Deliberately stale: the payload must never overwrite a fetched member.
        let state = serde_json::from_value(
            json!({"channel_id":"town", "user_id":"me", "last_viewed_at":-10, "mention_count":99}),
        )
        .unwrap();
        mattermost::Event::PostUnread {
            state,
            post_id: "p1".into(),
        }
    }

    fn finish_sync(session: &Entity<Session>, cx: &mut TestAppContext) {
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                s.channel_sync.values().all(|sync| !sync.running) && s.mute_pending.is_empty()
            })
        });
    }

    #[gpui_kit::test]
    fn delayed_member_get_retries_instead_of_overwriting_newer_events(cx: &mut TestAppContext) {
        for activity in [true, false] {
            let state = state_member();
            {
                let mut member = state.lock().unwrap();
                member.last_viewed_at = 0;
                member.mention_count = 2;
                member.notify_props.insert("desktop".into(), "all".into());
            }
            let gets = Arc::new(AtomicUsize::new(0));
            let server = SlowServer::start(Network::new(Duration::ZERO), {
                let state = state.clone();
                let gets = gets.clone();
                move |request| {
                    let reply = state_route(request, &state);
                    if request.method() == "GET"
                        && request.path() == "/api/v4/channels/town/members/me"
                    {
                        // Count only after the response snapshot has been captured.
                        gets.fetch_add(1, Ordering::SeqCst);
                        reply.delay(Duration::from_millis(100))
                    } else {
                        reply
                    }
                }
            });
            let session = start(&server, cx);
            wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
            session.update(cx, |s, cx| {
                s.active_channel = Some("town".into());
                s.unread_channel = Some("town".into());
                s.refresh_channel_member(&"town".into(), cx);
            });
            wait_until(cx, |_| gets.load(Ordering::SeqCst) == 1);
            if activity {
                state.lock().unwrap().mention_count = 3;
                session.update(cx, |s, cx| {
                    let mut post = unread_post();
                    post.id = "new-mention".into();
                    post.create_at = 2;
                    s.handle_event(
                        mattermost::Event::Posted {
                            post,
                            mentions: vec!["me".into()],
                            team_id: "team".into(),
                        },
                        cx,
                    );
                });
            } else {
                let member = {
                    let mut member = state.lock().unwrap();
                    member.notify_props.insert("desktop".into(), "none".into());
                    member
                        .notify_props
                        .insert("mark_unread".into(), "mention".into());
                    member.clone()
                };
                session.update(cx, |s, cx| {
                    s.handle_event(mattermost::Event::ChannelMemberUpdated(member), cx)
                });
            }
            // The old response has completed, while the replacement GET waits.
            wait_until(cx, |_| gets.load(Ordering::SeqCst) == 2);
            session.read_with(cx, |s, _| {
                let entry = s.channel(&"town".into()).unwrap();
                if activity {
                    assert_eq!(entry.mention_count(), 3);
                } else {
                    assert_eq!(entry.desktop_notifications(), "none");
                    assert!(entry.is_muted());
                }
            });
            finish_sync(&session, cx);
            assert_eq!(gets.load(Ordering::SeqCst), 2);
            session.read_with(cx, |s, _| {
                let entry = s.channel(&"town".into()).unwrap();
                if activity {
                    assert_eq!(entry.mention_count(), 3);
                } else {
                    assert_eq!(entry.desktop_notifications(), "none");
                    assert!(entry.is_muted());
                }
            });
        }
    }

    #[gpui_kit::test]
    fn mute_reconciliation_cannot_clear_an_unread_intent_or_schedule_a_passive_read(
        cx: &mut TestAppContext,
    ) {
        let state = state_member();
        let gets = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let state = state.clone();
            let gets = gets.clone();
            move |request| {
                let reply = state_route(request, &state);
                if request.method() == "GET" && request.path() == "/api/v4/channels/town/members/me"
                {
                    gets.fetch_add(1, Ordering::SeqCst);
                    reply.delay(Duration::from_millis(100))
                } else if request.method() == "POST"
                    && request.path() == "/api/v4/users/me/posts/p1/set_unread"
                {
                    reply.delay(Duration::from_millis(100))
                } else {
                    reply
                }
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        session.update(cx, |s, cx| {
            s.active_channel = Some("town".into());
            s.set_channel_muted(&"town".into(), true, cx);
        });
        wait_until(cx, |_| gets.load(Ordering::SeqCst) == 1);
        session.update(cx, |s, cx| s.mark_post_unread(&unread_post(), cx));
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/users/me/posts/p1/set_unread")
        });
        // The old mute snapshot returned before the unread write could start.
        session.update(cx, |s, cx| {
            assert!(s.unread_channel.is_some());
            let mut post = unread_post();
            post.id = "visible".into();
            post.create_at = 2;
            state.lock().unwrap().mention_count = 3;
            s.handle_event(
                mattermost::Event::Posted {
                    post,
                    mentions: vec!["me".into()],
                    team_id: "team".into(),
                },
                cx,
            );
            assert!(s.unread_channel.is_some());
        });
        finish_sync(&session, cx);
        session.read_with(cx, |s, _| {
            assert!(s.unread_channel.is_some());
            assert!(s.channel(&"town".into()).unwrap().is_unread());
            assert_eq!(s.channel(&"town".into()).unwrap().mention_count(), 3);
        });
        let requests = server.requests();
        assert!(
            !requests
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/channels/members/me/view")
        );
        let mute_get = requests
            .iter()
            .find(|r| r.method() == "GET" && r.path() == "/api/v4/channels/town/members/me")
            .unwrap();
        let unread = requests
            .iter()
            .find(|r| r.method() == "POST" && r.path() == "/api/v4/users/me/posts/p1/set_unread")
            .unwrap();
        assert!(unread.at() >= mute_get.at() + Duration::from_millis(100));
    }

    #[gpui_kit::test]
    fn remote_unread_is_preserved_but_a_stale_hint_cannot_undo_esc(cx: &mut TestAppContext) {
        let state = state_member();
        let gets = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let state = state.clone();
            let gets = gets.clone();
            move |request| {
                let reply = state_route(request, &state);
                if request.method() == "GET" && request.path() == "/api/v4/channels/town/members/me"
                {
                    gets.fetch_add(1, Ordering::SeqCst);
                    reply.delay(Duration::from_millis(100))
                } else {
                    reply
                }
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        {
            let mut member = state.lock().unwrap();
            member.last_viewed_at = 0;
            member.mention_count = 2;
        }
        session.update(cx, |s, cx| {
            s.active_channel = Some("town".into());
            s.handle_event(unread_event(), cx);
        });
        // The post lands after the first GET captured unread, before it returns.
        // Its passive view must wait for a clean confirmation of the hint.
        wait_until(cx, |_| gets.load(Ordering::SeqCst) == 1);
        session.update(cx, |s, cx| {
            assert!(s.unread_channel.is_none());
            state.lock().unwrap().mention_count = 3;
            let mut post = unread_post();
            post.id = "while-unread".into();
            post.create_at = 2;
            s.handle_event(
                mattermost::Event::Posted {
                    post,
                    mentions: vec!["me".into()],
                    team_id: "team".into(),
                },
                cx,
            );
        });
        finish_sync(&session, cx);
        session.update(cx, |s, cx| {
            assert!(s.unread_channel.is_some());
            assert_eq!(s.channel(&"town".into()).unwrap().mention_count(), 3);
            s.set_window_active(false, cx);
            s.set_window_active(true, cx);
            state.lock().unwrap().mention_count = 4;
            let mut post = unread_post();
            post.id = "after-confirmation".into();
            post.create_at = 3;
            s.note_activity(&post, true, cx);
            assert_eq!(s.channel(&"town".into()).unwrap().mention_count(), 4);
        });
        finish_sync(&session, cx);
        assert!(
            !server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/channels/members/me/view")
        );
        let next_get = gets.load(Ordering::SeqCst) + 1;
        session.update(cx, |s, cx| s.handle_event(unread_event(), cx));
        wait_until(cx, |_| gets.load(Ordering::SeqCst) == next_get);
        // This GET captured unread before Esc. It must not re-arm suppression.
        session.update(cx, |s, cx| s.mark_viewed(&"town".into(), cx));
        finish_sync(&session, cx);
        session.read_with(cx, |s, _| {
            assert!(s.unread_channel.is_none());
            assert!(!s.channel(&"town".into()).unwrap().is_unread());
        });
        // Even a hint received after Esc is confirmed against read server state.
        session.update(cx, |s, cx| s.handle_event(unread_event(), cx));
        finish_sync(&session, cx);
        session.read_with(cx, |s, _| assert!(s.unread_channel.is_none()));
    }

    #[gpui_kit::test]
    fn reading_a_channel_retains_its_opening_unread_divider(cx: &mut TestAppContext) {
        let state = state_member();
        state.lock().unwrap().last_viewed_at = 0;
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            state_route(request, &state)
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let timeline = session.update(cx, |s, cx| s.open_channel(&"town".into(), cx));
        finish_sync(&session, cx);
        session.read_with(cx, |s, cx| {
            assert!(!s.channel(&"town".into()).unwrap().is_unread());
            assert_eq!(timeline.read(cx).unread_after(), Some(0));
        });
    }

    #[gpui_kit::test]
    fn http_only_unread_and_reconnect_follow_authoritative_members(cx: &mut TestAppContext) {
        let state = state_member();
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let state = state.clone();
            move |request| state_route(request, &state)
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let timeline = session.update(cx, |s, cx| {
            s.active_channel = Some("town".into());
            let timeline = s.timeline(TimelineKind::Channel("town".into()), cx);
            s.mark_post_unread(&unread_post(), cx);
            timeline
        });
        finish_sync(&session, cx);
        session.read_with(cx, |s, cx| {
            let entry = s.channel(&"town".into()).unwrap();
            assert_eq!(entry.mention_count(), 2);
            let member = entry.member.as_ref().unwrap();
            assert_eq!(
                (
                    member.msg_count,
                    member.msg_count_root,
                    member.mention_count_root
                ),
                (7, 4, 1)
            );
            assert!(s.unread_channel.is_some());
            assert_eq!(timeline.read(cx).unread_post(), Some(&PostId::from("p1")));
        });
        {
            let mut member = state.lock().unwrap();
            member.last_viewed_at = 1000;
            member.mention_count = 0;
            member
                .notify_props
                .insert("mark_unread".into(), "mention".into());
        }
        session.update(cx, |s, cx| {
            s.has_connected = true;
            s.handle_event(mattermost::Event::Connected, cx);
        });
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                s.channel(&"town".into()).unwrap().is_muted() && s.unread_channel.is_none()
            })
        });
        // A later remote unread of the same post is not swallowed by an echo record.
        state.lock().unwrap().last_viewed_at = 0;
        session.update(cx, |s, cx| s.handle_event(unread_event(), cx));
        finish_sync(&session, cx);
        session.read_with(cx, |s, _| {
            assert_eq!(s.channel(&"town".into()).unwrap().last_viewed_at(), 0)
        });
    }

    #[gpui_kit::test]
    fn websocket_success_and_http_error_refetch_truth_in_both_orders(cx: &mut TestAppContext) {
        for (ws_first, committed) in [(true, true), (false, true), (true, false), (false, false)] {
            let state = state_member();
            let server = SlowServer::start(Network::new(Duration::ZERO), {
                let state = state.clone();
                move |request| {
                    let unread = request.method() == "POST"
                        && request.path() == "/api/v4/users/me/posts/p1/set_unread";
                    let reply = if unread && !committed {
                        Reply::json(json!({}))
                    } else {
                        state_route(request, &state)
                    };
                    if request.method() == "POST"
                        && request.path() == "/api/v4/users/me/posts/p1/set_unread"
                    {
                        // The write took effect, but its response was lost/rejected.
                        return reply.status(500).delay(if ws_first {
                            Duration::from_millis(100)
                        } else {
                            Duration::ZERO
                        });
                    }
                    reply
                }
            });
            let session = start(&server, cx);
            wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
            session.update(cx, |s, cx| {
                s.active_channel = Some("town".into());
                s.mark_post_unread(&unread_post(), cx);
            });
            wait_until(cx, |_| {
                server.requests().iter().any(|r| {
                    r.method() == "POST" && r.path() == "/api/v4/users/me/posts/p1/set_unread"
                })
            });
            if !ws_first {
                finish_sync(&session, cx);
            }
            session.update(cx, |s, cx| s.handle_event(unread_event(), cx));
            finish_sync(&session, cx);
            session.read_with(cx, |s, _| {
                assert_eq!(
                    s.channel(&"town".into()).unwrap().mention_count(),
                    if committed { 2 } else { 0 }
                );
                assert_eq!(s.unread_channel.is_some(), committed);
            });
            // Esc/send/navigation issue a read; even a late unread echo refetches read truth.
            session.update(cx, |s, cx| {
                s.mark_viewed(&"town".into(), cx);
                s.handle_event(unread_event(), cx);
            });
            finish_sync(&session, cx);
            session.read_with(cx, |s, _| {
                assert!(!s.channel(&"town".into()).unwrap().is_unread());
                assert!(s.unread_channel.is_none());
            });
        }
    }

    #[gpui_kit::test]
    fn passive_views_and_unread_events_are_bounded_for_a_hundred_posts(cx: &mut TestAppContext) {
        let state = state_member();
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            let reply = state_route(request, &state);
            if request.method() == "POST" && request.path() == "/api/v4/channels/members/me/view" {
                reply.delay(Duration::from_millis(100))
            } else {
                reply
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        session.update(cx, |s, cx| {
            s.open_channel(&"town".into(), cx);
        });
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/channels/members/me/view")
        });
        session.update(cx, |s, cx| {
            for ix in 0..100 {
                let mut post = unread_post();
                post.id = format!("burst-{ix}").into();
                post.create_at = ix + 2;
                s.note_activity(&post, true, cx);
                s.handle_event(unread_event(), cx);
            }
        });
        finish_sync(&session, cx);
        let requests = server.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method() == "POST" && r.path() == "/api/v4/channels/members/me/view")
                .count(),
            2
        );
        assert!(
            requests
                .iter()
                .filter(|r| r.method() == "GET" && r.path() == "/api/v4/channels/town/members/me")
                .count()
                <= 3
        );
    }

    #[gpui_kit::test]
    fn read_unread_barriers_serialize_and_replace_pending_passive_views(cx: &mut TestAppContext) {
        for read_first in [true, false] {
            let state = state_member();
            let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
                let reply = state_route(request, &state);
                if request.method() == "POST" {
                    reply.delay(Duration::from_millis(80))
                } else {
                    reply
                }
            });
            let session = start(&server, cx);
            wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
            session.update(cx, |s, cx| {
                s.active_channel = Some("town".into());
                if read_first {
                    s.auto_mark_viewed(&"town".into(), cx);
                } else {
                    s.mark_post_unread(&unread_post(), cx);
                }
            });
            let first = if read_first {
                "/api/v4/channels/members/me/view"
            } else {
                "/api/v4/users/me/posts/p1/set_unread"
            };
            wait_until(cx, |_| {
                server
                    .requests()
                    .iter()
                    .any(|r| r.method() == "POST" && r.path() == first)
            });
            session.update(cx, |s, cx| {
                if read_first {
                    for _ in 0..10 {
                        s.auto_mark_viewed(&"town".into(), cx);
                    }
                    s.mark_post_unread(&unread_post(), cx);
                } else {
                    s.mark_viewed(&"town".into(), cx);
                }
            });
            finish_sync(&session, cx);
            session.read_with(cx, |s, _| {
                assert_eq!(s.channel(&"town".into()).unwrap().is_unread(), read_first)
            });
            let writes: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| {
                    r.method() == "POST"
                        && (r.path() == first
                            || r.path() == "/api/v4/channels/members/me/view"
                            || r.path() == "/api/v4/users/me/posts/p1/set_unread")
                })
                .collect();
            assert_eq!(writes.len(), 2);
            let read = writes
                .iter()
                .find(|r| r.path() == "/api/v4/channels/members/me/view")
                .unwrap();
            let unread = writes
                .iter()
                .find(|r| r.path() == "/api/v4/users/me/posts/p1/set_unread")
                .unwrap();
            assert!(read.at().abs_diff(unread.at()) >= Duration::from_millis(80));
        }
    }

    #[gpui_kit::test]
    fn mute_ignores_stale_put_results_and_refetches_after_success_or_error(
        cx: &mut TestAppContext,
    ) {
        for reject in [true, false] {
            let state = state_member();
            let server = SlowServer::start(Network::new(Duration::ZERO), {
                let state = state.clone();
                move |request| {
                    let reply = state_route(request, &state);
                    if request.method() == "PUT"
                        && request.path() == "/api/v4/channels/town/members/me/notify_props"
                    {
                        reply
                            .status(if reject { 500 } else { 200 })
                            .delay(Duration::from_millis(100))
                    } else {
                        reply
                    }
                }
            });
            let session = start(&server, cx);
            wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
            session.update(cx, |s, cx| {
                s.set_channel_muted(&"town".into(), true, cx);
                assert!(s.channel(&"town".into()).unwrap().is_muted());
            });
            wait_until(cx, |_| {
                server.requests().iter().any(|r| {
                    r.method() == "PUT"
                        && r.path() == "/api/v4/channels/town/members/me/notify_props"
                })
            });
            {
                let mut member = state.lock().unwrap();
                member
                    .notify_props
                    .insert("mark_unread".into(), "all".into());
                member.notify_props.insert("desktop".into(), "none".into());
            }
            finish_sync(&session, cx);
            session.read_with(cx, |s, _| {
                let entry = s.channel(&"town".into()).unwrap();
                assert!(!entry.is_muted());
                assert_eq!(entry.desktop_notifications(), "none");
            });
        }
    }

    #[gpui_kit::test]
    fn delayed_read_events_use_server_time_and_never_move_backwards(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        session.update(cx, |s, cx| {
            s.window_active = false;
            s.handle_event(
                mattermost::Event::ChannelsViewed(vec![("town".into(), 100)]),
                cx,
            );
            let mut post = mattermost::Post::default();
            post.id = "queued".into();
            post.channel_id = "town".into();
            post.user_id = "someone-else".into();
            post.create_at = 110;
            s.handle_event(
                mattermost::Event::Posted {
                    post,
                    mentions: vec!["me".into()],
                    team_id: "team".into(),
                },
                cx,
            );
            let channel = s.channel(&"town".into()).unwrap();
            assert_eq!(channel.last_viewed_at(), 100);
            assert!(channel.is_unread());
            assert_eq!(channel.mention_count(), 1);
            for at in [90, 100] {
                s.handle_event(
                    mattermost::Event::ChannelsViewed(vec![("town".into(), at)]),
                    cx,
                );
                let channel = s.channel(&"town".into()).unwrap();
                assert_eq!(channel.last_viewed_at(), 100);
                assert_eq!(channel.mention_count(), 1);
            }
            s.handle_event(
                mattermost::Event::ChannelsViewed(vec![("town".into(), 105)]),
                cx,
            );
            let channel = s.channel(&"town".into()).unwrap();
            assert_eq!(channel.last_viewed_at(), 105);
            assert!(channel.is_unread());
            assert_eq!(channel.mention_count(), 1);
            s.handle_event(
                mattermost::Event::ChannelsViewed(vec![("town".into(), 120)]),
                cx,
            );
            let channel = s.channel(&"town".into()).unwrap();
            assert!(!channel.is_unread());
            assert_eq!(channel.mention_count(), 0);
            // Older servers send channel_viewed without a timestamp.
            s.handle_event(mattermost::Event::ChannelViewed("town".into()), cx);
            assert!(s.channel(&"town".into()).unwrap().last_viewed_at() > 120);
        });
    }

    // These tests check when the server saw each request, not how long the
    // whole startup took: a request sent less than a round trip after
    // another cannot have waited for its answer. Wall-clock totals would
    // also count client setup (reading the system's certificates is slow on
    // macOS) and the test machine's load, and so flake.

    #[gpui_kit::test]
    fn the_sidebar_fills_in_one_round_trip(cx: &mut TestAppContext) {
        let teams = Gate::default();
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let teams = teams.clone();
            move |request| {
                let reply = route(request);
                if request.method() == "GET" && request.path() == "/api/v4/users/me/teams" {
                    reply.wait_for(teams.clone())
                } else {
                    reply
                }
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            requested(&server, "GET", "/api/v4/users/me/teams")
                && session.read_with(cx, |s, _| s.is_loaded())
        });
        // The remembered team's channels and read state arrived while the
        // team list was still held back.
        session.read_with(cx, |s, _| {
            assert!(!s.teams_loaded());
            assert!(s.channel(&"town".into()).is_some());
        });
        teams.open();
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.teams_loaded()));
        session.read_with(cx, |s, _| {
            assert_eq!(s.team().map(|t| t.display_name.as_str()), Some("Team"));
        });
    }

    #[gpui_kit::test]
    fn the_last_conversation_loads_alongside_the_sidebar(cx: &mut TestAppContext) {
        let channels = Gate::default();
        let authors = Gate::default();
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let channels = channels.clone();
            let authors = authors.clone();
            move |request| {
                let reply = route(request);
                match (
                    request.method(),
                    request.path().split('?').next().unwrap_or_default(),
                ) {
                    (
                        "GET",
                        "/api/v4/users/me/teams/team/channels"
                        | "/api/v4/users/me/teams/team/channels/members",
                    ) => reply.wait_for(channels.clone()),
                    ("POST", "/api/v4/users/ids" | "/api/v4/users/status/ids") => {
                        reply.wait_for(authors.clone())
                    }
                    _ => reply,
                }
            }
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.state() == &LoadState::Loaded)
        });
        // Posts do not wait for the channel lists to confirm the channel.
        session.read_with(cx, |s, _| assert!(!s.is_loaded()));
        timeline.read_with(cx, |t, _| assert_eq!(t.entries().len(), 1));
        channels.open();
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));

        // Both author requests must start before either response is released.
        wait_until(cx, |_| {
            requested(&server, "POST", "/api/v4/users/ids")
                && server
                    .requests()
                    .iter()
                    .filter(|r| r.method() == "POST" && r.path() == "/api/v4/users/status/ids")
                    .count()
                    == 2
        });
        let directory = session.read_with(cx, |s, _| s.directory().clone());
        directory.read_with(cx, |d, _| assert!(d.user(&"ann".into()).is_none()));
        authors.open();
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| {
                d.user(&"ann".into()).is_some()
                    && d.status(&"ann".into()) == mattermost::Status::Online
            })
        });
    }

    #[gpui_kit::test]
    fn startup_survives_requests_that_fail_once(cx: &mut TestAppContext) {
        // Every endpoint fails its first request, as on a flaky link.
        let seen = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let failures = Arc::new(AtomicUsize::new(0));
        let failed = failures.clone();
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), move |request| {
            let path = request
                .path()
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            if seen.lock().unwrap().insert(path) {
                failed.fetch_add(1, Ordering::SeqCst);
                return Reply::json(json!({ "message": "Bad gateway" })).status(502);
            }
            route(request)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_loaded())
                && timeline.read_with(cx, |t, _| t.state() == &LoadState::Loaded)
        });
        assert!(failures.load(Ordering::SeqCst) >= 3);
        session.read_with(cx, |s, _| assert!(s.channel(&"town".into()).is_some()));
    }

    #[gpui_kit::test]
    fn channel_hydration_and_list_refresh_keep_completion_order(cx: &mut TestAppContext) {
        for hydrate_first in [true, false] {
            let refreshing = Arc::new(AtomicBool::new(false));
            let list = Gate::default();
            let channel = Gate::default();
            let member = Gate::default();
            let server = SlowServer::start(Network::new(Duration::ZERO), {
                let refreshing = refreshing.clone();
                let list = list.clone();
                let channel = channel.clone();
                let member = member.clone();
                let lists = AtomicUsize::new(0);
                move |request| match (
                    request.method(),
                    request.path().split('?').next().unwrap_or_default(),
                ) {
                    ("GET", "/api/v4/users/me/teams/team/channels") => {
                        let snapshot = lists.fetch_add(1, Ordering::SeqCst);
                        let reply = Reply::json(json!([{
                            "id": "town", "team_id": "team", "name": "town-square", "type": "O",
                            "display_name": format!("Snapshot {snapshot}")
                        }]));
                        if refreshing.load(Ordering::SeqCst) {
                            reply.wait_for(list.clone())
                        } else {
                            reply
                        }
                    }
                    ("GET", "/api/v4/users/me/teams/team/channels/members")
                        if refreshing.load(Ordering::SeqCst) =>
                    {
                        route(request).wait_for(list.clone())
                    }
                    ("GET", "/api/v4/channels/design") => Reply::json(json!({
                        "id": "design", "team_id": "team", "name": "design", "type": "P"
                    }))
                    .wait_for(channel.clone()),
                    ("GET", "/api/v4/channels/design/members/me") => Reply::json(json!({
                        "channel_id": "design", "user_id": "me", "last_viewed_at": 10,
                        "notify_props": { "mark_unread": "mention" }
                    }))
                    .wait_for(member.clone()),
                    _ => route(request),
                }
            });
            let session = start(&server, cx);
            wait_until(cx, |cx| {
                session.read_with(cx, |s, _| s.is_loaded() && s.team().is_some())
            });
            refreshing.store(true, Ordering::SeqCst);
            session.update(cx, |s, cx| s.load_channels(cx));
            let task = session.update(cx, |s, cx| s.ensure_channel(&"design".into(), cx));
            let result = Rc::new(RefCell::new(None));
            let slot = result.clone();
            cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
                .detach();
            // Both operations have started, but neither can complete yet.
            wait_until(cx, |_| {
                let requests = server.requests();
                [
                    "/api/v4/users/me/teams/team/channels",
                    "/api/v4/users/me/teams/team/channels/members",
                ]
                .iter()
                .all(|path| {
                    requests
                        .iter()
                        .filter(|r| r.method() == "GET" && r.path() == *path)
                        .count()
                        == 2
                }) && requested(&server, "GET", "/api/v4/channels/design")
            });
            assert!(result.borrow().is_none());
            session.read_with(cx, |s, _| {
                assert_eq!(
                    s.channel(&"town".into()).unwrap().channel().display_name,
                    "Snapshot 0"
                );
                assert!(s.channel(&"design".into()).is_none());
            });
            let mut viewed_at = None;
            if hydrate_first {
                channel.open();
                wait_until(cx, |_| {
                    requested(&server, "GET", "/api/v4/channels/design/members/me")
                });
                assert!(result.borrow().is_none());
                member.open();
                wait_until(cx, |_| result.borrow().is_some());
                assert!(result.borrow().as_ref().unwrap().is_ok());
                viewed_at = Some(session.update(cx, |s, cx| {
                    s.open_channel(&"design".into(), cx);
                    s.channel(&"design".into()).unwrap().last_viewed_at()
                }));
            }
            list.open();
            wait_until(cx, |cx| {
                session.read_with(cx, |s, _| {
                    s.channel(&"town".into())
                        .is_some_and(|c| c.channel().display_name == "Snapshot 1")
                })
            });
            if hydrate_first {
                session.read_with(cx, |s, _| {
                    let channel = s
                        .channel(&"design".into())
                        .expect("keep the freshly hydrated channel");
                    assert!(channel.is_muted());
                    assert_eq!(channel.last_viewed_at(), viewed_at.unwrap());
                    assert_eq!(s.active_channel(), Some(&"design".into()));
                });
                // A later authoritative snapshot can still remove the channel.
                session.update(cx, |s, cx| s.load_channels(cx));
                wait_until(cx, |cx| {
                    session.read_with(cx, |s, _| {
                        s.channel(&"town".into())
                            .is_some_and(|c| c.channel().display_name == "Snapshot 2")
                    })
                });
            } else {
                assert!(result.borrow().is_none());
                channel.open();
                wait_until(cx, |_| {
                    requested(&server, "GET", "/api/v4/channels/design/members/me")
                });
                assert!(result.borrow().is_none());
                member.open();
                wait_until(cx, |_| result.borrow().is_some());
                assert!(result.borrow().as_ref().unwrap().is_err());
            }
            session.read_with(cx, |s, _| assert!(s.channel(&"design".into()).is_none()));
        }
    }

    #[gpui_kit::test]
    fn reconnect_refreshes_our_presence_in_both_directions(cx: &mut TestAppContext) {
        let dnd = Arc::new(AtomicBool::new(false));
        let remote = dnd.clone();
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            if request.method() == "POST" && request.path() == "/api/v4/users/status/ids" {
                let status = if remote.load(Ordering::SeqCst) {
                    "dnd"
                } else {
                    "online"
                };
                Reply::json(json!([{ "user_id": "me", "status": status }]))
            } else {
                route(request)
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, cx| {
                s.directory.read(cx).status(&s.me.id) == mattermost::Status::Online
            })
        });
        session.update(cx, |s, cx| s.handle_event(mattermost::Event::Connected, cx));
        for enabled in [true, false] {
            session.update(cx, |s, cx| {
                s.handle_event(mattermost::Event::Disconnected, cx)
            });
            dnd.store(enabled, Ordering::SeqCst);
            session.update(cx, |s, cx| s.handle_event(mattermost::Event::Connected, cx));
            let expected = if enabled {
                mattermost::Status::DoNotDisturb
            } else {
                mattermost::Status::Online
            };
            wait_until(cx, |cx| {
                session.read_with(cx, |s, cx| {
                    s.directory.read(cx).status(&s.me.id) == expected
                })
            });
        }
    }

    /// A team with a private channel besides Town Square. The server
    /// answers a request to leave it with `status`.
    fn leave_route(request: &Request, status: u16) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        match (request.method(), path) {
            ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                { "id": "town", "team_id": "team", "type": "O", "name": "town-square",
                  "display_name": "Town Square" },
                { "id": "design", "team_id": "team", "type": "P", "name": "design",
                  "display_name": "Design" },
            ])),
            // A followed thread in each channel.
            ("GET", "/api/v4/users/me/teams/team/threads") => Reply::json(json!({
                "total": 2,
                "threads": [
                    { "id": "in-design", "last_reply_at": 20,
                      "post": { "id": "in-design", "channel_id": "design", "user_id": "ann" } },
                    { "id": "in-town", "last_reply_at": 10,
                      "post": { "id": "in-town", "channel_id": "town", "user_id": "ann" } },
                ],
            })),
            ("DELETE", "/api/v4/channels/design/members/me") => {
                Reply::json(json!({ "status": "OK" })).status(status)
            }
            _ => route(request),
        }
    }

    fn leave_design(status: u16, cx: &mut TestAppContext) -> (SlowServer, Entity<Session>, Gate) {
        let leave = Gate::default();
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let leave = leave.clone();
            move |request| {
                let reply = leave_route(request, status);
                if request.method() == "DELETE"
                    && request.path() == "/api/v4/channels/design/members/me"
                {
                    reply.wait_for(leave.clone())
                } else {
                    reply
                }
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        session.read_with(cx, |s, _| {
            assert!(s.channel(&"design".into()).unwrap().can_leave());
            assert!(!s.channel(&"town".into()).unwrap().can_leave());
        });
        let threads = session.read_with(cx, |s, _| s.threads().clone());
        threads.update(cx, |t, cx| t.load(ThreadFilter::All, cx));
        wait_until(cx, |cx| {
            threads.read_with(cx, |t, _| t.threads(ThreadFilter::All).count() == 2)
        });
        session.update(cx, |s, cx| s.leave_channel(&"design".into(), cx));
        // Gone from the sidebar before the server answers.
        session.read_with(cx, |s, _| assert!(s.channel(&"design".into()).is_none()));
        (server, session, leave)
    }

    #[gpui_kit::test]
    fn leaving_a_channel_takes_it_out_of_the_sidebar(cx: &mut TestAppContext) {
        let (server, session, leave) = leave_design(200, cx);
        leave.open();
        // Its thread leaves the Threads pane once the server agrees.
        let threads = session.read_with(cx, |s, _| s.threads().clone());
        wait_until(cx, |cx| {
            threads.read_with(cx, |t, _| t.thread(&"in-design".into()).is_none())
        });
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.method() == "DELETE" && r.path() == "/api/v4/channels/design/members/me")
        );
        session.read_with(cx, |s, cx| {
            assert!(s.channel(&"design".into()).is_none());
            let threads = s.threads().read(cx);
            let left: Vec<_> = threads.threads(ThreadFilter::All).map(|t| t.id()).collect();
            assert_eq!(left, [&PostId::from("in-town")]);
        });
    }

    /// Design's threads count in the totals until the user is out of it:
    /// they leave, or `out` is set as if they were removed elsewhere.
    fn design_threads_route(request: &Request, out: &AtomicBool) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        match (request.method(), path) {
            ("GET", "/api/v4/users/me/teams/team/threads") => {
                let unread = i64::from(!out.load(Ordering::SeqCst));
                Reply::json(json!({
                    "total_unread_threads": unread,
                    "total_unread_mentions": unread,
                }))
            }
            // Town Square alone.
            ("GET", "/api/v4/users/me/teams/team/channels") if out.load(Ordering::SeqCst) => {
                route(request)
            }
            ("DELETE", "/api/v4/channels/design/members/me") => {
                out.store(true, Ordering::SeqCst);
                leave_route(request, 200)
            }
            _ => leave_route(request, 200),
        }
    }

    /// A session in Town Square and Design, whose totals count a thread in
    /// Design that no list here has loaded.
    fn in_design(cx: &mut TestAppContext) -> (SlowServer, Arc<AtomicBool>, Entity<Session>) {
        let out = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let out = out.clone();
            move |request| design_threads_route(request, &out)
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, cx| {
                s.channel(&"design".into()).is_some() && s.threads().read(cx).unread_mentions() == 1
            })
        });
        (server, out, session)
    }

    #[gpui_kit::test]
    fn leaving_a_channel_reads_the_thread_totals_again(cx: &mut TestAppContext) {
        let (_server, _, session) = in_design(cx);
        session.update(cx, |s, cx| s.leave_channel(&"design".into(), cx));
        wait_until(cx, |cx| {
            session.read_with(cx, |s, cx| s.threads().read(cx).unread_mentions() == 0)
        });
    }

    #[gpui_kit::test]
    fn removal_from_a_channel_elsewhere_reads_the_thread_totals_again(cx: &mut TestAppContext) {
        let (_server, out, session) = in_design(cx);
        out.store(true, Ordering::SeqCst);
        // As a membership event does.
        session.update(cx, |s, cx| s.load_channels(cx));
        wait_until(cx, |cx| {
            session.read_with(cx, |s, cx| s.threads().read(cx).unread_mentions() == 0)
        });
        session.read_with(cx, |s, _| assert!(s.channel(&"design".into()).is_none()));
    }

    #[gpui_kit::test]
    fn a_refused_leave_puts_the_channel_back(cx: &mut TestAppContext) {
        let (_server, session, leave) = leave_design(403, cx);
        let failures = failures(&session, cx);
        leave.open();
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.channel(&"design".into()).is_some())
        });
        let failures = failures.borrow();
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].starts_with("Couldn’t leave Design."),
            "{failures:?}"
        );
    }

    #[gpui_kit::test]
    fn a_refused_leave_after_a_team_switch_stays_with_its_team(cx: &mut TestAppContext) {
        let leave = Gate::default();
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let leave = leave.clone();
            move |request| match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("GET", "/api/v4/users/me/teams") => Reply::json(json!([
                    { "id": "team", "name": "team" },
                    { "id": "other", "name": "other" },
                ])),
                (
                    "GET",
                    "/api/v4/users/me/teams/other/channels"
                    | "/api/v4/users/me/teams/other/channels/members",
                ) => Reply::json(json!([])),
                ("DELETE", "/api/v4/channels/design/members/me") => Reply::json(json!({
                    "message": "You can't leave this channel.",
                }))
                .status(403)
                .wait_for(leave.clone()),
                _ => leave_route(request, 403),
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_loaded() && s.teams_loaded())
        });
        session.read_with(cx, |s, _| assert!(s.channel(&"design".into()).is_some()));
        let failures = failures(&session, cx);
        session.update(cx, |s, cx| s.leave_channel(&"design".into(), cx));
        wait_until(cx, |_| {
            requested(&server, "DELETE", "/api/v4/channels/design/members/me")
        });
        session.update(cx, |s, cx| s.select_team("other".into(), cx));
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                s.is_loaded()
                    && s.team()
                        .is_some_and(|team| team.id == TeamId::from("other"))
            })
        });
        assert!(requested(
            &server,
            "GET",
            "/api/v4/users/me/teams/other/channels"
        ));
        assert!(requested(
            &server,
            "GET",
            "/api/v4/users/me/teams/other/channels/members"
        ));
        assert!(failures.borrow().is_empty(), "refused before the switch");
        leave.open();
        wait_until(cx, |_| !failures.borrow().is_empty());
        assert_eq!(failures.borrow().len(), 1);
        session.read_with(cx, |s, _| {
            assert_eq!(s.team().map(|team| team.id.as_str()), Some("other"));
            assert!(s.channel(&"design".into()).is_none());
        });
    }

    #[gpui_kit::test]
    fn unfollowing_a_thread_not_loaded_reads_the_totals_again(cx: &mut TestAppContext) {
        let unfollowed = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let unfollowed = unfollowed.clone();
            move |request| match request.path().split('?').next().unwrap_or_default() {
                "/api/v4/users/me/teams/team/threads" => {
                    let unread = i64::from(!unfollowed.load(Ordering::SeqCst));
                    Reply::json(json!({
                        "total_unread_threads": unread,
                        "total_unread_mentions": unread,
                    }))
                }
                _ => route(request),
            }
        });
        let session = start(&server, cx);
        let threads = session.read_with(cx, |s, _| s.threads().clone());
        wait_until(cx, |cx| {
            threads.read_with(cx, |t, _| t.unread_mentions() == 1)
        });
        // Unfollowed on another device, with no list loaded here.
        unfollowed.store(true, Ordering::SeqCst);
        threads.update(cx, |t, cx| t.note_following(&"elsewhere".into(), false, cx));
        wait_until(cx, |cx| {
            threads.read_with(cx, |t, _| {
                (t.unread_threads(), t.unread_mentions()) == (0, 0)
            })
        });
    }

    /// The messages of the failures `session` reports, as they arrive.
    fn failures(
        session: &Entity<Session>,
        cx: &mut TestAppContext,
    ) -> Rc<RefCell<Vec<SharedString>>> {
        let failures = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let failures = failures.clone();
            cx.subscribe(session, move |_, event, _| {
                if let SessionEvent::SendFailed { message } = event {
                    failures.borrow_mut().push(message.clone());
                }
            })
            .detach();
        });
        failures
    }

    /// The server of a company with custom emoji, where the full list can't
    /// be read (as when requests are rate-limited) and only `lookup` finds
    /// emoji by name.
    fn emoji_route(request: &Request, lookup: &str) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        let happy_cat = json!({ "id": "e1", "name": "happy_cat" });
        let pepe = json!({ "id": "e2", "name": "pepe-privet" });
        match path {
            "/api/v4/emoji" => Reply::json(json!({ "message": "Forbidden" })).status(403),
            "/api/v4/emoji/names" if lookup == "batch" => Reply::json(json!([happy_cat, pepe])),
            "/api/v4/emoji/name/happy_cat" if lookup == "one" => Reply::json(happy_cat),
            "/api/v4/emoji/name/pepe-privet" if lookup == "one" => Reply::json(pepe),
            "/api/v4/channels/town/posts" => Reply::json(json!({
                "order": ["p1"],
                "posts": { "p1": { "id": "p1", "channel_id": "town", "user_id": "ann",
                                   "message": "meow :happy_cat:", "create_at": 1 } },
            })),
            // A status set by a client that stores the shortcode.
            "/api/v4/users/ids" => Reply::json(json!([{
                "id": "ann", "username": "ann",
                "props": { "customStatus": r#"{"emoji":":pepe-privet:","text":""}"# },
            }])),
            _ => route(request),
        }
    }

    fn emoji_on_screen_load(lookup: &'static str, cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), move |request| {
            emoji_route(request, lookup)
        });
        let session = start(&server, cx);
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.custom_emoji().len() == 2)
                && timeline.read_with(cx, |t, _| {
                    t.entries()
                        .first()
                        .is_some_and(|e| e.message().text().contains("/api/v4/emoji/e1/image"))
                })
        });
        session.read_with(cx, |s, _| {
            let status = s.custom_emoji().image_url(":pepe-privet:").unwrap();
            assert!(status.ends_with("/api/v4/emoji/e2/image"), "{status}");
        });
    }

    #[gpui_kit::test]
    fn custom_emoji_on_screen_load_without_the_full_list(cx: &mut TestAppContext) {
        emoji_on_screen_load("batch", cx);
    }

    #[gpui_kit::test]
    fn older_servers_look_up_custom_emoji_one_by_one(cx: &mut TestAppContext) {
        emoji_on_screen_load("one", cx);
    }

    #[gpui_kit::test]
    fn failed_emoji_lookups_are_tried_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        // The first lookup fails through all of its retries.
        let lookups = Arc::new(AtomicUsize::new(0));
        let seen = lookups.clone();
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), move |request| {
            if request.method() != "POST" || request.path() != "/api/v4/emoji/names" {
                return Reply::not_found();
            }
            if seen.fetch_add(1, Ordering::SeqCst) < 4 {
                return Reply::json(json!({ "message": "Bad gateway" })).status(502);
            }
            Reply::json(json!([{ "id": "e1", "name": "happy_cat" }]))
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client));
        store.update(cx, |s, cx| s.ensure(["happy_cat"], cx));
        wait_until(cx, |_| lookups.load(Ordering::SeqCst) >= 4);
        store.read_with(cx, |s, _| assert!(s.custom().is_empty()));

        // Asking again once the failure is known looks the name up anew.
        wait_until(cx, |cx| {
            store.update(cx, |s, cx| s.ensure(["happy_cat"], cx));
            store.read_with(cx, |s, _| s.custom().len() == 1)
        });
        assert_eq!(lookups.load(Ordering::SeqCst), 5);
    }

    #[gpui_kit::test]
    fn a_recreated_emoji_replaces_the_old_image(cx: &mut TestAppContext) {
        let client = Client::new_unverified("http://localhost:1", "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client));
        let emoji = |id: &str| {
            serde_json::from_value::<mattermost::Emoji>(json!({ "id": id, "name": "happy_cat" }))
                .unwrap()
        };
        store.update(cx, |s, cx| s.add(vec![emoji("e1")], cx));
        store.update(cx, |s, cx| s.add(vec![emoji("e2")], cx));
        store.read_with(cx, |s, _| {
            let url = s.custom().image_url("happy_cat").unwrap();
            assert!(url.ends_with("/emoji/e2/image"), "{url}");
        });
    }

    /// Emoji saved by an earlier run: one recreated since, one deleted.
    fn remembered_emoji() -> Vec<(String, String)> {
        vec![
            (
                "happy_cat".into(),
                "http://old/api/v4/emoji/e0/image".into(),
            ),
            ("gone".into(), "http://old/api/v4/emoji/e9/image".into()),
        ]
    }

    fn image(store: &Entity<EmojiStore>, name: &str, cx: &mut TestAppContext) -> Option<String> {
        store.read_with(cx, |s, _| s.custom().image_url(name).map(|u| u.to_string()))
    }

    #[gpui_kit::test]
    fn remembered_emoji_show_until_the_full_list_corrects_them(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let server =
            SlowServer::start(
                Network::new(Duration::from_millis(20)),
                |request| match request.path().split('?').next().unwrap_or_default() {
                    "/api/v4/emoji" => Reply::json(json!([{ "id": "e1", "name": "happy_cat" }])),
                    _ => Reply::not_found(),
                },
            );
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client));
        store.update(cx, |s, cx| s.remember(remembered_emoji(), cx));
        assert!(image(&store, "gone", cx).is_some(), "shown at once");

        store.update(cx, |s, cx| s.load_all(cx));
        wait_until(cx, |cx| image(&store, "gone", cx).is_none());
        let url = image(&store, "happy_cat", cx).unwrap();
        assert!(url.ends_with("/emoji/e1/image"), "{url}");

        // The complete list is newer than anything remembered.
        store.update(cx, |s, cx| s.remember(remembered_emoji(), cx));
        assert!(image(&store, "gone", cx).is_none());
    }

    #[gpui_kit::test]
    fn remembered_emoji_are_confirmed_by_name_without_the_full_list(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let server =
            SlowServer::start(
                Network::new(Duration::from_millis(20)),
                |request| match request.path().split('?').next().unwrap_or_default() {
                    "/api/v4/emoji" => Reply::json(json!({ "message": "Forbidden" })).status(403),
                    "/api/v4/emoji/names" => {
                        Reply::json(json!([{ "id": "e1", "name": "happy_cat" }]))
                    }
                    _ => Reply::not_found(),
                },
            );
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client));
        store.update(cx, |s, cx| {
            s.remember(remembered_emoji(), cx);
            s.load_all(cx);
            s.ensure(["happy_cat", "gone"], cx);
        });
        assert!(image(&store, "gone", cx).is_some(), "shown at once");

        wait_until(cx, |cx| image(&store, "gone", cx).is_none());
        let url = image(&store, "happy_cat", cx).unwrap();
        assert!(url.ends_with("/emoji/e1/image"), "{url}");
        let lookups = server
            .requests()
            .iter()
            .filter(|r| r.method() == "POST" && r.path() == "/api/v4/emoji/names")
            .count();
        assert_eq!(lookups, 1);
    }
}
