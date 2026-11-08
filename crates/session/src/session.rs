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
    CustomEmojis, Delivery, Directory, EmojiStore, Timeline, TimelineKind,
    retry::{is_transient, retry},
    timeline::{LoadState, now_millis},
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

    /// The most recent activity, for recency ordering.
    pub fn last_activity(&self) -> Millis {
        self.channel.last_post_at
    }
}

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
    /// A server event this crate does not model, for extensions.
    Raw {
        name: SharedString,
        data: serde_json::Value,
    },
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
    channels_loaded: bool,
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
    window_active: bool,
    /// Every live timeline, for routing real-time events.
    timelines: HashMap<TimelineKind, WeakEntity<Timeline>>,
    /// Recently used timelines, kept alive for instant switching.
    timeline_cache: Vec<Entity<Timeline>>,
    typing: HashMap<(ChannelId, PostId), Vec<(UserId, Instant)>>,
    last_typing_sent: HashMap<(ChannelId, PostId), Instant>,
    connection: Connection,
    has_connected: bool,
    /// A load gave up while the network was down; repeat it on reconnect.
    load_failed: bool,
    stream: EventStream,
    _event_task: Task<()>,
    _channels_task: Option<Task<()>>,
    _typing_expiry: Option<Task<()>>,
    _emoji_subscription: Subscription,
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
            channels_loaded: false,
            teams_loaded: false,
            favorites: HashSet::new(),
            group_members: HashMap::new(),
            emoji,
            custom_emoji: CustomEmojis::default(),
            active_channel: None,
            window_active: true,
            timelines: HashMap::new(),
            timeline_cache: Vec::new(),
            typing: HashMap::new(),
            last_typing_sent: HashMap::new(),
            connection: Connection::Connecting,
            has_connected: false,
            load_failed: false,
            stream,
            _event_task,
            _channels_task: None,
            _typing_expiry: None,
            _emoji_subscription,
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
        self.channels_loaded
    }

    /// Whether the team list has arrived. At startup the remembered team's
    /// channels can arrive first.
    pub fn teams_loaded(&self) -> bool {
        self.teams_loaded
    }

    pub fn active_channel(&self) -> Option<&ChannelId> {
        self.active_channel.as_ref()
    }

    pub fn is_favorite(&self, channel: &ChannelId) -> bool {
        self.favorites.contains(channel)
    }

    /// The server's custom emoji, for drawing reactions and statuses.
    pub fn custom_emoji(&self) -> &CustomEmojis {
        &self.custom_emoji
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

    fn load_teams(&mut self, cx: &mut Context<Self>) {
        // The remembered team is almost always still ours, so its channels
        // load alongside the team list rather than after it: the sidebar
        // fills one round trip sooner. The list corrects a stale guess.
        let guess = self.team.clone();
        if guess.is_some() {
            self.load_channels(cx);
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || client.my_teams()).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(teams) => {
                    this.teams_loaded = true;
                    this.teams = teams.into_iter().filter(|t| t.delete_at == 0).collect();
                    this.teams.sort_by_key(|t| t.display_name.to_lowercase());
                    let keep = this
                        .team
                        .as_ref()
                        .is_some_and(|id| this.teams.iter().any(|t| &t.id == id));
                    if !keep {
                        this.team = this.teams.first().map(|t| t.id.clone());
                    }
                    if this.team != guess || guess.is_none() {
                        this.channels.clear();
                        this.channels_loaded = false;
                        this.load_channels(cx);
                    }
                    cx.notify();
                }
                Err(err) => this.handle_error(err, cx),
            });
        })
        .detach();
    }

    /// Switch the sidebar to another team.
    pub fn select_team(&mut self, team: TeamId, cx: &mut Context<Self>) {
        if self.team.as_ref() == Some(&team) {
            return;
        }
        self.team = Some(team);
        self.channels.clear();
        self.channels_loaded = false;
        self.load_channels(cx);
        cx.notify();
    }

    fn load_channels(&mut self, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            self.channels_loaded = true;
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
                    this.channels = channels
                        .into_iter()
                        .filter(|c| c.delete_at == 0)
                        .map(|channel| {
                            let member = members.remove(&channel.id);
                            (channel.id.clone(), ChannelEntry { channel, member })
                        })
                        .collect();
                    this.channels_loaded = true;
                    let partners: Vec<UserId> = this
                        .channels
                        .values()
                        .filter_map(|e| e.channel.direct_partner(&this.me.id))
                        .collect();
                    this.directory.update(cx, |d, cx| d.ensure(partners, cx));
                    this.load_group_members(cx);
                    cx.notify();
                }
                (Err(err), _) | (_, Err(err)) => this.handle_error(err, cx),
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
                self.timelines.insert(kind, timeline.downgrade());
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

    /// Show `channel` and mark it read. Returns its timeline.
    pub fn open_channel(
        &mut self,
        channel: &ChannelId,
        cx: &mut Context<Self>,
    ) -> Entity<Timeline> {
        self.active_channel = Some(channel.clone());
        let timeline = self.timeline(TimelineKind::Channel(channel.clone()), cx);
        let unread_after = self
            .channels
            .get(channel)
            .filter(|e| e.is_unread())
            .map(ChannelEntry::last_viewed_at);
        timeline.update(cx, |t, cx| t.set_unread_after(unread_after, cx));
        self.mark_viewed(channel, cx);
        cx.notify();
        timeline
    }

    /// Stop tracking the closed conversation as visible.
    pub fn close_channel(&mut self, cx: &mut Context<Self>) {
        self.active_channel = None;
        cx.notify();
    }

    /// Record that the user has seen everything in `channel`.
    pub fn mark_viewed(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        let Some(entry) = self.channels.get_mut(channel) else {
            return;
        };
        let was_unread = entry.is_unread() || entry.mention_count() > 0;
        if let Some(member) = entry.member.as_mut() {
            member.last_viewed_at = now_millis().max(entry.channel.last_post_at);
            member.mention_count = 0;
            member.mention_count_root = 0;
            member.msg_count = entry.channel.total_msg_count;
            member.msg_count_root = entry.channel.total_msg_count_root;
        }
        if was_unread {
            cx.notify();
        }
        let client = self.client.clone();
        let channel = channel.clone();
        cx.background_spawn(async move {
            if let Err(err) = client.view_channel(&channel).await {
                log::warn!("marking channel read: {err:#}");
            }
        })
        .detach();
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        if active && let Some(channel) = self.active_channel.clone() {
            let unread = self
                .channels
                .get(&channel)
                .is_some_and(ChannelEntry::is_unread);
            if unread {
                self.mark_viewed(&channel, cx);
            }
        }
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
        let now = now_millis();
        let pending_id = format!("{}:{}", self.me.id, now);
        let mut post = Post::default();
        post.id = PostId::from(pending_id.as_str());
        post.pending_post_id = pending_id.clone();
        post.create_at = now;
        post.user_id = self.me.id.clone();
        post.channel_id = kind.channel().clone();
        post.root_id = kind.root().cloned().unwrap_or_default();
        post.message = message.clone();
        let file_ids: Vec<String> = files.iter().map(|f| f.id.clone()).collect();
        post.metadata.files = files;

        let timeline = self.timeline(kind.clone(), cx);
        timeline.update(cx, |t, cx| t.insert_pending(post, cx));
        self.last_typing_sent.remove(&(
            kind.channel().clone(),
            kind.root().cloned().unwrap_or_default(),
        ));

        let client = self.client.clone();
        let channel = kind.channel().clone();
        let root = kind.root().cloned();
        let pending = PostId::from(pending_id.as_str());
        cx.spawn(async move |this, cx| {
            let result = client
                .create_post(&channel, &message, root.as_ref(), &pending_id, &file_ids)
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(post) => this.route_post(post, false, cx),
                Err(err) => {
                    timeline.update(cx, |t, cx| t.set_delivery(&pending, Delivery::Failed, cx));
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t send your message. {err:#}").into(),
                    });
                    this.handle_error(err, cx);
                }
            });
        })
        .detach();
    }

    /// Send a failed post again.
    pub fn retry_post(&mut self, kind: &TimelineKind, pending: &PostId, cx: &mut Context<Self>) {
        let Some(timeline) = self.live_timeline(kind) else {
            return;
        };
        let failed = timeline.update(cx, |t, cx| {
            let failed = t.position(pending).map(|ix| {
                let post = t.entries()[ix].post();
                (post.message.clone(), post.metadata.files.clone())
            });
            t.remove(pending, cx);
            failed
        });
        if let Some((message, files)) = failed {
            self.send_post(kind, message, files, cx);
        }
    }

    /// Discard a failed post without sending it.
    pub fn discard_post(&mut self, kind: &TimelineKind, pending: &PostId, cx: &mut Context<Self>) {
        if let Some(timeline) = self.live_timeline(kind) {
            timeline.update(cx, |t, cx| t.remove(pending, cx));
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
        // Removing at once feels instant; the server event is idempotent.
        self.route_deleted(post, cx);
        let client = self.client.clone();
        let id = post.id.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = client.delete_post(&id).await {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(SessionEvent::SendFailed {
                        message: format!("Couldn’t delete the message. {err:#}").into(),
                    })
                });
            }
        })
        .detach();
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
                this.directory.update(cx, |d, cx| d.ensure([user], cx));
                cx.notify();
            })?;
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
                    if self.teams.is_empty() {
                        self.load_teams(cx);
                    } else {
                        self.load_channels(cx);
                    }
                    self.load_favorites(cx);
                    for timeline in self.live_timelines() {
                        timeline.update(cx, |t, cx| t.catch_up(cx));
                    }
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
            Event::ChannelViewed(channel) => self.note_viewed_elsewhere(&[channel], cx),
            Event::ChannelsViewed(channels) => self.note_viewed_elsewhere(&channels, cx),
            Event::StatusChanged { user_id, status } => {
                self.directory
                    .update(cx, |d, cx| d.set_status(user_id, status, cx));
            }
            Event::UserUpdated(user) => {
                self.directory.update(cx, |d, cx| d.insert([user], cx));
            }
            Event::ChannelsChanged => self.load_channels(cx),
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
    fn note_viewed_elsewhere(&mut self, channels: &[ChannelId], cx: &mut Context<Self>) {
        let now = now_millis();
        let mut changed = false;
        for channel in channels {
            if let Some(member) = self
                .channels
                .get_mut(channel)
                .and_then(|e| e.member.as_mut())
            {
                changed |= member.mention_count != 0 || member.last_viewed_at < now;
                member.last_viewed_at = now;
                member.mention_count = 0;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Update unread state for a new post.
    fn note_activity(&mut self, post: &Post, mentions_me: bool, cx: &mut Context<Self>) {
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
        if from_me || is_visible {
            let channel = post.channel_id.clone();
            self.mark_viewed(&channel, cx);
        } else if (mentions_me || entry.channel.kind == ChannelType::Direct)
            && let Some(member) = entry.member.as_mut()
        {
            member.mention_count += 1;
        }
        cx.notify();
    }

    fn route_post(&mut self, post: Post, from_event: bool, cx: &mut Context<Self>) {
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
        for timeline in self.live_timelines() {
            timeline.update(cx, |t, cx| t.remove(&post.id, cx));
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
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use mattermost::{
        Client, TeamId, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::Session;
    use crate::{EmojiStore, LoadState, TimelineKind};

    /// One round trip on a poor mobile or satellite link.
    const ROUND_TRIP: Duration = Duration::from_millis(400);

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

    fn arrival(server: &SlowServer, path: &str) -> Duration {
        server
            .requests()
            .iter()
            .find(|r| r.path().split('?').next() == Some(path))
            .unwrap_or_else(|| panic!("{path} was never requested"))
            .at()
    }

    // These tests check when the server saw each request, not how long the
    // whole startup took: a request sent less than a round trip after
    // another cannot have waited for its answer. Wall-clock totals would
    // also count client setup (reading the system's certificates is slow on
    // macOS) and the test machine's load, and so flake.

    #[gpui_kit::test]
    fn the_sidebar_fills_in_one_round_trip(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(ROUND_TRIP), route);
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_loaded() && s.teams_loaded())
        });

        // The remembered team's channels and read state were asked for
        // before the team list came back, not after it.
        let teams = arrival(&server, "/api/v4/users/me/teams");
        for path in [
            "/api/v4/users/me/teams/team/channels",
            "/api/v4/users/me/teams/team/channels/members",
        ] {
            let at = arrival(&server, path);
            assert!(
                at < teams + ROUND_TRIP / 2,
                "{path} at {at:?}, teams at {teams:?}"
            );
        }
        session.read_with(cx, |s, _| {
            assert!(s.channel(&"town".into()).is_some());
            assert_eq!(s.team().map(|t| t.display_name.as_str()), Some("Team"));
        });
    }

    #[gpui_kit::test]
    fn the_last_conversation_loads_alongside_the_sidebar(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(ROUND_TRIP), route);
        let session = start(&server, cx);
        // What the workspace does at startup for the conversation open last.
        let timeline = session.update(cx, |s, cx| {
            s.timeline(TimelineKind::Channel("town".into()), cx)
        });
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_loaded())
                && timeline.read_with(cx, |t, _| t.state() == &LoadState::Loaded)
        });
        // Its posts were asked for alongside the channel list, not once the
        // list had confirmed the channel.
        let channels = arrival(&server, "/api/v4/users/me/teams/team/channels");
        let posts = arrival(&server, "/api/v4/channels/town/posts");
        assert!(
            posts < channels + ROUND_TRIP / 2,
            "posts at {posts:?}, channels at {channels:?}"
        );
        timeline.read_with(cx, |t, _| assert_eq!(t.entries().len(), 1));

        // Then the authors: their profiles and presence in one round trip.
        let directory = session.read_with(cx, |s, _| s.directory().clone());
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| d.user(&"ann".into()).is_some())
        });
        let users = arrival(&server, "/api/v4/users/ids");
        // The first presence request is the user's own, at startup.
        let statuses = server
            .requests()
            .iter()
            .filter(|r| r.path() == "/api/v4/users/status/ids")
            .map(|r| r.at())
            .max()
            .unwrap();
        assert!(
            statuses.abs_diff(users) < ROUND_TRIP / 2,
            "{users:?} vs {statuses:?}"
        );
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
}
