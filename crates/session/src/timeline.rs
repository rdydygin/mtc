use std::{borrow::Cow, collections::HashSet, ops::Range, sync::Arc, time::SystemTime};

use gpui_kit::{Context, Entity, EventEmitter, SharedString, Task};
use mattermost::{ChannelId, Client, Millis, Post, PostId, PostList, Reaction, UserId};

use crate::{
    CustomEmojis, Directory, EmojiStore, PreparedMessage, prepare::custom_emoji_names, retry::retry,
};

/// Posts requested per page. Large enough to fill a tall window, small
/// enough to decode in well under a frame.
pub const PAGE_SIZE: usize = 60;

/// Which conversation a [`Timeline`] shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TimelineKind {
    /// A channel's root posts. Replies live in threads (collapsed reply
    /// threads), so the channel stays scannable.
    Channel(ChannelId),
    /// A thread: its root followed by every reply.
    Thread { channel: ChannelId, root: PostId },
}

impl TimelineKind {
    pub fn channel(&self) -> &ChannelId {
        match self {
            Self::Channel(channel) | Self::Thread { channel, .. } => channel,
        }
    }

    pub fn root(&self) -> Option<&PostId> {
        match self {
            Self::Channel(_) => None,
            Self::Thread { root, .. } => Some(root),
        }
    }

    /// Whether `post` belongs in this conversation.
    fn accepts(&self, post: &Post) -> bool {
        match self {
            Self::Channel(channel) => &post.channel_id == channel && !post.is_reply(),
            Self::Thread { root, .. } => &post.id == root || &post.root_id == root,
        }
    }
}

/// Posts of one page, filtered, ordered, and prepared for display.
struct Prepared {
    entries: Vec<Entry>,
    /// The custom emoji the entries were prepared with.
    custom_emoji: CustomEmojis,
}

/// Order, filter, and prepare a page of posts on a background thread, so a
/// page of long messages never stalls a frame. Returns how many posts the
/// server sent, before filtering, for paging decisions.
async fn prepare_off_thread(
    executor: &gpui_kit::BackgroundExecutor,
    list: PostList,
    kind: &TimelineKind,
    custom_emoji: CustomEmojis,
) -> (usize, Prepared) {
    let kind = kind.clone();
    executor
        .spawn(async move {
            let posts = list.into_chronological();
            let fetched = posts.len();
            let entries = posts
                .into_iter()
                .filter(|p| kind.accepts(p))
                .map(|p| Entry::new(p, Delivery::Sent, &custom_emoji))
                .collect();
            (
                fetched,
                Prepared {
                    entries,
                    custom_emoji,
                },
            )
        })
        .await
}

/// Whether the server has accepted a post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// Shown optimistically while the request is in flight.
    Pending,
    /// The request failed; the post can be retried or discarded.
    Failed,
}

fn prepare(post: &Post, custom: &CustomEmojis) -> PreparedMessage {
    if post.is_system() {
        PreparedMessage::system(&post.message, custom)
    } else {
        PreparedMessage::new(&post.message, custom)
    }
}

/// Apply a reaction once per user and emoji, including removal echoes.
fn set_reaction(post: &mut Post, reaction: &Reaction, added: bool) -> bool {
    let reactions = &mut post.metadata.reactions;
    let existing = reactions
        .iter()
        .position(|r| r.user_id == reaction.user_id && r.emoji_name == reaction.emoji_name);
    match (added, existing) {
        (true, None) => reactions.push(reaction.clone()),
        (false, Some(ix)) => {
            reactions.remove(ix);
        }
        _ => return false,
    }
    true
}

/// One post as the UI shows it.
#[derive(Clone, Debug)]
pub struct Entry {
    post: Arc<Post>,
    message: PreparedMessage,
    delivery: Delivery,
    revision: u64,
}

impl Entry {
    fn new(post: Post, delivery: Delivery, custom: &CustomEmojis) -> Self {
        let message = prepare(&post, custom);
        Self {
            post: Arc::new(post),
            message,
            delivery,
            revision: 0,
        }
    }

    pub fn post(&self) -> &Arc<Post> {
        &self.post
    }

    pub fn id(&self) -> &PostId {
        &self.post.id
    }

    pub fn message(&self) -> &PreparedMessage {
        &self.message
    }

    pub fn delivery(&self) -> Delivery {
        self.delivery
    }

    /// Prepare the message again for a new set of custom emoji. Returns
    /// whether the display text changed.
    fn reprepare(&mut self, custom: &CustomEmojis) -> bool {
        if !self.post.message.contains(':') {
            return false;
        }
        let message = prepare(&self.post, custom);
        let changed = message != self.message;
        self.message = message;
        changed
    }

    /// The newest moment this post changed, for incremental refresh.
    fn changed_at(&self) -> Millis {
        self.post
            .update_at
            .max(self.post.create_at)
            .max(self.post.edit_at)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(SharedString),
}

/// A change to [`Timeline::entries`], expressed so that a virtual list can
/// mirror it without remeasuring unaffected rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineEvent {
    /// Entries in `old` were replaced by `new_count` entries starting at
    /// `old.start`.
    Spliced { old: Range<usize>, new_count: usize },
    /// The entry at this index changed in place.
    Updated(usize),
    /// Everything changed; rebuild from scratch.
    Reset,
}

/// The ordered posts of one channel or thread, with paging and live updates.
///
/// Owns ordering, de-duplication, optimistic delivery state, and the
/// unread boundary. Views mirror it through [`TimelineEvent`]s and never
/// mutate it directly.
pub struct Timeline {
    kind: TimelineKind,
    client: Client,
    directory: Entity<Directory>,
    /// Where custom emoji in messages and reactions are looked up; the
    /// session hands their images back through [`Self::set_custom_emoji`].
    emoji: Entity<EmojiStore>,
    custom_emoji: CustomEmojis,
    entries: Vec<Entry>,
    revision: u64,
    /// Confirmed deletes stay hidden until outstanding requests settle.
    deleted: HashSet<PostId>,
    requests_in_flight: usize,
    state: LoadState,
    has_older: bool,
    loading_older: bool,
    older_failed: bool,
    /// Invalidates pagination responses when their snapshot or cursor disappears.
    pagination_generation: u64,
    unread_after: Option<Millis>,
    unread_post: Option<PostId>,
    _load_task: Option<Task<()>>,
}

impl EventEmitter<TimelineEvent> for Timeline {}

pub(crate) fn now_millis() -> Millis {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or_default()
}

impl Timeline {
    pub fn new(
        kind: TimelineKind,
        client: Client,
        directory: Entity<Directory>,
        emoji: Entity<EmojiStore>,
        custom_emoji: CustomEmojis,
    ) -> Self {
        Self {
            kind,
            client,
            directory,
            emoji,
            custom_emoji,
            entries: Vec::new(),
            revision: 0,
            deleted: HashSet::new(),
            requests_in_flight: 0,
            state: LoadState::NotLoaded,
            has_older: true,
            loading_older: false,
            older_failed: false,
            pagination_generation: 0,
            unread_after: None,
            unread_post: None,
            _load_task: None,
        }
    }

    pub fn kind(&self) -> &TimelineKind {
        &self.kind
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn state(&self) -> &LoadState {
        &self.state
    }

    pub fn has_older(&self) -> bool {
        self.has_older
    }

    pub fn older_failed(&self) -> bool {
        self.older_failed
    }

    pub fn is_loading_older(&self) -> bool {
        self.loading_older
    }

    /// Posts created after this moment by someone else are new to the user.
    pub fn unread_after(&self) -> Option<Millis> {
        self.unread_after
    }

    pub fn set_unread_after(&mut self, at: Option<Millis>, cx: &mut Context<Self>) {
        if self.unread_after != at {
            self.unread_after = at;
            self.unread_post = None;
            cx.emit(TimelineEvent::Reset);
            cx.notify();
        }
    }

    /// The explicitly marked post, including a message authored by the user.
    pub fn unread_post(&self) -> Option<&PostId> {
        self.unread_post.as_ref()
    }

    pub(crate) fn set_unread_post(&mut self, post: PostId, at: Millis, cx: &mut Context<Self>) {
        self.unread_after = Some(at);
        self.unread_post = Some(post);
        cx.emit(TimelineEvent::Reset);
        cx.notify();
    }

    pub fn position(&self, id: &PostId) -> Option<usize> {
        self.entries.iter().rposition(|e| e.id() == id)
    }

    /// Load the newest page, unless already loaded or loading.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, LoadState::Loading | LoadState::Loaded) {
            return;
        }
        self.reload(cx);
    }

    /// Replace the contents with the newest page from the server.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.state == LoadState::Loading {
            return;
        }
        self.reset_pagination();
        self.begin_request();
        self.state = LoadState::Loading;
        cx.notify();
        let client = self.client.clone();
        let kind = self.kind.clone();
        let custom = self.custom_emoji.clone();
        let revision = self.revision;
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || async {
                match &kind {
                    TimelineKind::Channel(channel) => client.latest_posts(channel, PAGE_SIZE).await,
                    TimelineKind::Thread { root, .. } => client.thread(root).await,
                }
            })
            .await;
            let result = match result {
                Ok(list) => Ok(prepare_off_thread(&executor, list, &kind, custom).await),
                Err(err) => Err(err),
            };
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((count, entries)) => this.replace_all(count, entries, revision, cx),
                    Err(err) => {
                        // Keep showing what we had; surface the failure.
                        this.state = LoadState::Failed(format!("{err:#}").into());
                        cx.notify();
                    }
                }
                this.finish_request();
            });
        }));
    }

    /// `fetched` is how many posts the server returned, before filtering.
    fn replace_all(
        &mut self,
        fetched: usize,
        prepared: Prepared,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        self.reset_pagination();
        self.has_older = matches!(self.kind, TimelineKind::Channel(_)) && fetched >= PAGE_SIZE;
        let entries = self.adopt(prepared);
        let ids: HashSet<_> = entries.iter().map(|e| e.id().clone()).collect();
        // Keep optimistic posts and changes made while the request was in flight.
        self.entries.retain(|e| {
            e.delivery != Delivery::Sent || e.revision > revision || ids.contains(e.id())
        });
        for entry in entries {
            self.merge(entry, Some(revision));
        }
        self.state = LoadState::Loaded;
        self.request_missing(0..self.entries.len(), cx);
        cx.emit(TimelineEvent::Reset);
        cx.notify();
    }

    /// Entries prepared off the UI thread, brought up to date with custom
    /// emoji that arrived while they were being prepared.
    fn adopt(&self, prepared: Prepared) -> Vec<Entry> {
        let mut entries = prepared.entries;
        if prepared.custom_emoji != self.custom_emoji {
            for entry in &mut entries {
                entry.reprepare(&self.custom_emoji);
            }
        }
        entries
    }

    fn reset_pagination(&mut self) {
        self.pagination_generation += 1;
        self.loading_older = false;
        self.older_failed = false;
    }

    /// Fetch the page before the oldest loaded post.
    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        let TimelineKind::Channel(channel) = &self.kind else {
            return;
        };
        if self.loading_older || !self.has_older || self.state != LoadState::Loaded {
            return;
        }
        let Some(oldest) = self.entries.iter().find(|e| e.delivery == Delivery::Sent) else {
            self.reset_pagination();
            self.has_older = false;
            cx.notify();
            return;
        };
        let before = oldest.id().clone();
        let channel = channel.clone();
        self.begin_request();
        self.loading_older = true;
        self.older_failed = false;
        let generation = self.pagination_generation;
        cx.notify();
        let client = self.client.clone();
        let kind = self.kind.clone();
        let custom = self.custom_emoji.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = retry(&executor, || {
                client.posts_before(&channel, &before, PAGE_SIZE)
            })
            .await;
            let result = match result {
                Ok(list) => Ok(prepare_off_thread(&executor, list, &kind, custom).await),
                Err(err) => Err(err),
            };
            let _ = this.update(cx, |this, cx| {
                if generation != this.pagination_generation {
                    this.finish_request();
                    return;
                }
                this.loading_older = false;
                match result {
                    Ok((fetched, prepared)) => {
                        let posts: Vec<Entry> = this
                            .adopt(prepared)
                            .into_iter()
                            .filter(|e| {
                                this.position(e.id()).is_none() && !this.deleted.contains(e.id())
                            })
                            .collect();
                        this.has_older = fetched >= PAGE_SIZE;
                        let count = posts.len();
                        this.entries.splice(0..0, posts);
                        this.request_missing(0..count, cx);
                        cx.emit(TimelineEvent::Spliced {
                            old: 0..0,
                            new_count: count,
                        });
                    }
                    Err(err) => {
                        this.older_failed = true;
                        log::warn!("loading older posts: {err:#}");
                    }
                }
                this.finish_request();
                cx.notify();
            });
        })
        .detach();
    }

    /// Catch up after a reconnect: apply everything that changed while the
    /// socket was down.
    pub fn catch_up(&mut self, cx: &mut Context<Self>) {
        match self.state {
            LoadState::Loaded => {}
            // The first load gave up while the network was down.
            LoadState::Failed(_) => return self.reload(cx),
            LoadState::NotLoaded | LoadState::Loading => return,
        }
        let TimelineKind::Channel(channel) = &self.kind else {
            // A thread is small; reloading it is simpler than merging.
            self.reload(cx);
            return;
        };
        let Some(since) = self.confirmed_since() else {
            self.reload(cx);
            return;
        };
        let client = self.client.clone();
        let channel = channel.clone();
        let revision = self.revision;
        self.begin_request();
        cx.spawn(async move |this, cx| {
            let result = client.posts_since(&channel, since).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(list) = result {
                    let mut events = Vec::new();
                    for post in list.into_chronological() {
                        if this.kind.accepts(&post) {
                            let entry = Entry::new(post, Delivery::Sent, &this.custom_emoji);
                            if let Some(event) = this.merge(entry, Some(revision)) {
                                events.push(event);
                            }
                        }
                    }
                    if !events.is_empty() {
                        this.request_missing(0..this.entries.len(), cx);
                        // A reset reads the final length, so it must stand alone.
                        if events.contains(&TimelineEvent::Reset) {
                            cx.emit(TimelineEvent::Reset);
                        } else {
                            for event in events {
                                cx.emit(event);
                            }
                        }
                        cx.notify();
                    }
                }
                this.finish_request();
            });
        })
        .detach();
    }

    /// Overlap the last confirmed change so timestamp boundaries are harmless.
    fn confirmed_since(&self) -> Option<Millis> {
        self.entries
            .iter()
            .filter(|e| e.delivery == Delivery::Sent)
            .map(Entry::changed_at)
            .max()
            .map(|at| at.saturating_sub(1_000).max(0))
    }

    /// Insert a new post or replace a known one, keeping chronological order.
    /// A post echoing an optimistic entry's `pending_post_id` replaces it.
    pub fn upsert(&mut self, post: Post, cx: &mut Context<Self>) {
        self.upsert_from(post, None, cx);
    }

    fn upsert_from(&mut self, post: Post, revision: Option<u64>, cx: &mut Context<Self>) {
        if !self.kind.accepts(&post) {
            return;
        }
        let entry = Entry::new(post, Delivery::Sent, &self.custom_emoji);
        if let Some(event) = self.merge(entry, revision) {
            let range = match &event {
                TimelineEvent::Updated(ix) => *ix..ix + 1,
                TimelineEvent::Spliced { old, new_count } => old.start..old.start + new_count,
                TimelineEvent::Reset => 0..self.entries.len(),
            };
            self.request_missing(range, cx);
            cx.emit(event);
            cx.notify();
        }
    }

    /// Merge server state without undoing newer posts.
    fn merge(&mut self, mut entry: Entry, revision: Option<u64>) -> Option<TimelineEvent> {
        if entry.post.delete_at > 0 {
            if self.requests_in_flight > 0 {
                self.deleted.insert(entry.id().clone());
            }
            return self.position(entry.id()).map(|ix| {
                self.entries.remove(ix);
                TimelineEvent::Spliced {
                    old: ix..ix + 1,
                    new_count: 0,
                }
            });
        }
        if self.deleted.contains(entry.id()) {
            return None;
        }
        let pending = PostId::from(entry.post.pending_post_id.as_str());
        let ix = self.position(entry.id()).or_else(|| {
            (!pending.is_empty())
                .then(|| self.position(&pending))
                .flatten()
        });
        if let Some(ix) = ix {
            let current = &self.entries[ix];
            if current.delivery == Delivery::Sent {
                if current.changed_at() > entry.changed_at() {
                    return None;
                }
                if current.changed_at() == entry.changed_at()
                    && revision.is_some_and(|at| current.revision > at)
                {
                    let reactions = entry.post.metadata.reactions.clone();
                    entry.post = current.post.clone();
                    entry.message = current.message.clone();
                    Arc::make_mut(&mut entry.post).metadata.reactions = reactions;
                }
            }
            if current.delivery == entry.delivery && current.post == entry.post {
                return None;
            }
            let was_pending = current.delivery != Delivery::Sent;
            let id = entry.id().clone();
            self.revision += 1;
            entry.revision = self.revision;
            self.entries[ix] = entry;
            if was_pending {
                self.entries
                    .sort_by_key(|e| (e.delivery != Delivery::Sent, e.post.create_at));
                if self.position(&id) != Some(ix) {
                    return Some(TimelineEvent::Reset);
                }
            }
            return Some(TimelineEvent::Updated(ix));
        }
        // Pending entries stay at the tail, after every confirmed post.
        let ix = self
            .entries
            .iter()
            .rposition(|e| e.delivery == Delivery::Sent && e.post.create_at <= entry.post.create_at)
            .map_or(0, |ix| ix + 1);
        self.revision += 1;
        entry.revision = self.revision;
        self.entries.insert(ix, entry);
        Some(TimelineEvent::Spliced {
            old: ix..ix,
            new_count: 1,
        })
    }

    /// Remove a confirmed deletion from its conversation.
    pub fn delete(&mut self, post: &Post, cx: &mut Context<Self>) {
        if self.kind.accepts(post) {
            if self.requests_in_flight > 0 {
                self.deleted.insert(post.id.clone());
            }
            self.remove(&post.id, cx);
        }
    }

    fn begin_request(&mut self) {
        self.requests_in_flight += 1;
    }

    fn finish_request(&mut self) {
        self.requests_in_flight -= 1;
        if self.requests_in_flight == 0 {
            self.deleted.clear();
        }
    }

    /// Remove a row optimistically, allowing a later reload to restore it.
    pub fn remove(&mut self, id: &PostId, cx: &mut Context<Self>) {
        if let Some(ix) = self.position(id) {
            self.entries.remove(ix);
            if !self
                .entries
                .iter()
                .any(|entry| entry.delivery == Delivery::Sent)
            {
                self.reset_pagination();
                self.has_older = false;
            }
            cx.emit(TimelineEvent::Spliced {
                old: ix..ix + 1,
                new_count: 0,
            });
            cx.notify();
        }
    }

    /// Show a post immediately while it is being sent.
    pub fn insert_pending(&mut self, post: Post, cx: &mut Context<Self>) {
        let ix = self.entries.len();
        self.entries
            .push(Entry::new(post, Delivery::Pending, &self.custom_emoji));
        cx.emit(TimelineEvent::Spliced {
            old: ix..ix,
            new_count: 1,
        });
        cx.notify();
    }

    pub fn set_delivery(&mut self, id: &PostId, delivery: Delivery, cx: &mut Context<Self>) {
        if let Some(ix) = self.position(id)
            && self.entries[ix].delivery != delivery
        {
            self.entries[ix].delivery = delivery;
            cx.emit(TimelineEvent::Updated(ix));
            cx.notify();
        }
    }

    fn update_post(
        &mut self,
        id: &PostId,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Post) -> bool,
    ) {
        let Some(ix) = self.position(id) else {
            return;
        };
        let entry = &mut self.entries[ix];
        let post = Arc::make_mut(&mut entry.post);
        if change(post) {
            self.revision += 1;
            entry.revision = self.revision;
            cx.emit(TimelineEvent::Updated(ix));
            cx.notify();
        }
    }

    pub fn apply_reaction(&mut self, reaction: &Reaction, added: bool, cx: &mut Context<Self>) {
        if added {
            let name = reaction.emoji_name.as_str();
            self.emoji.update(cx, |emoji, cx| emoji.ensure([name], cx));
        }
        self.update_post(&reaction.post_id, cx, |post| {
            set_reaction(post, reaction, added)
        });
    }

    /// Show messages with a new set of custom emoji. Only posts whose
    /// display text changes are reported.
    pub fn set_custom_emoji(&mut self, custom: CustomEmojis, cx: &mut Context<Self>) {
        if custom == self.custom_emoji {
            return;
        }
        self.custom_emoji = custom;
        let mut changed = false;
        for (ix, entry) in self.entries.iter_mut().enumerate() {
            if entry.reprepare(&self.custom_emoji) {
                cx.emit(TimelineEvent::Updated(ix));
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Reflect a new reply on its root without reloading the channel.
    pub fn note_reply(&mut self, root: &PostId, cx: &mut Context<Self>) {
        self.update_post(root, cx, |post| {
            post.reply_count += 1;
            true
        });
    }

    /// Look up the authors and custom emoji of the entries in `range`.
    fn request_missing(&self, range: Range<usize>, cx: &mut Context<Self>) {
        let entries = &self.entries[range];
        let ids: Vec<UserId> = entries.iter().map(|e| e.post.user_id.clone()).collect();
        self.directory.update(cx, |d, cx| d.ensure(ids, cx));
        self.request_emoji(entries.iter().map(|e| e.post.as_ref()), cx);
    }

    /// Look up the custom emoji that `posts` use in messages and reactions.
    fn request_emoji<'a>(&self, posts: impl IntoIterator<Item = &'a Post>, cx: &mut Context<Self>) {
        let mut names = Vec::new();
        for post in posts {
            names.extend(custom_emoji_names(&post.message));
            names.extend(
                post.metadata
                    .reactions
                    .iter()
                    .map(|r| Cow::from(r.emoji_name.as_str())),
            );
        }
        if !names.is_empty() {
            let names = names.iter().map(|name| name.as_ref());
            self.emoji.update(cx, |emoji, cx| emoji.ensure(names, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::{AppContext as _, TestAppContext};
    use mattermost::User;

    use super::*;

    fn post(id: &str, at: Millis) -> Post {
        let mut post = Post::default();
        post.id = id.into();
        post.channel_id = "c".into();
        post.user_id = "u".into();
        post.create_at = at;
        post.message = format!("message {id}");
        post
    }

    fn timeline(cx: &mut TestAppContext) -> (Entity<Timeline>, Rc<RefCell<Vec<TimelineEvent>>>) {
        let client = Client::new_unverified("http://localhost:1", "token").unwrap();
        timeline_with_client(client, cx)
    }

    fn timeline_with_client(
        client: Client,
        cx: &mut TestAppContext,
    ) -> (Entity<Timeline>, Rc<RefCell<Vec<TimelineEvent>>>) {
        let mut me = User::default();
        me.id = "me".into();
        let emoji = cx.new(|_| EmojiStore::new(client.clone()));
        let directory = cx.new(|_| Directory::new(client.clone(), me, emoji.clone()));
        let timeline = cx.new(|_| {
            Timeline::new(
                TimelineKind::Channel("c".into()),
                client,
                directory,
                emoji,
                CustomEmojis::default(),
            )
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        cx.update(|cx| {
            cx.subscribe(&timeline, move |_, event: &TimelineEvent, _| {
                sink.borrow_mut().push(event.clone())
            })
            .detach()
        });
        (timeline, events)
    }

    fn ids(timeline: &Entity<Timeline>, cx: &mut TestAppContext) -> Vec<String> {
        timeline.read_with(cx, |t, _| {
            t.entries().iter().map(|e| e.id().to_string()).collect()
        })
    }

    fn snapshot(posts: Vec<Post>) -> Prepared {
        Prepared {
            entries: posts
                .into_iter()
                .map(|p| Entry::new(p, Delivery::Sent, &CustomEmojis::default()))
                .collect(),
            custom_emoji: CustomEmojis::default(),
        }
    }

    fn wait_for_pagination(cx: &mut TestAppContext, timeline: &Entity<Timeline>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if timeline.read_with(cx, |t, _| t.requests_in_flight == 0) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pagination did not finish"
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(50));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[gpui_kit::test]
    fn reload_and_cursor_removal_clear_obsolete_pagination_failures(cx: &mut TestAppContext) {
        use mattermost::testing::{Network, Reply, SlowServer};
        use serde_json::json;
        cx.executor().allow_parking();
        for count in [0, 1] {
            let server = SlowServer::start(
                Network::new(std::time::Duration::ZERO),
                move |request| {
                    if request.method() == "GET"
                        && request.path().starts_with("/api/v4/channels/c/posts?")
                    {
                        if request.path().contains("before=") {
                            return Reply::not_found();
                        }
                        return Reply::json(if count == 0 {
                            json!({"order": [], "posts": {}})
                        } else {
                            json!({"order": ["new"], "posts": {"new": {"id": "new", "channel_id": "c", "create_at": 200}}})
                        });
                    }
                    Reply::not_found()
                },
            );
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let (timeline, _) = timeline_with_client(client, cx);
            timeline.update(cx, |t, cx| {
                t.replace_all(PAGE_SIZE, snapshot(vec![post("seed", 100)]), t.revision, cx);
                t.load_older(cx);
            });
            wait_for_pagination(cx, &timeline);
            timeline.update(cx, |t, cx| {
                assert!(t.older_failed());
                t.reload(cx);
            });
            wait_for_pagination(cx, &timeline);
            timeline.read_with(cx, |t, _| {
                assert_eq!(t.state(), &LoadState::Loaded);
                assert_eq!(t.entries().len(), count);
                assert!(!t.has_older());
                assert!(!t.older_failed());
                assert!(!t.is_loading_older());
            });
            timeline.update(cx, |t, cx| {
                t.replace_all(PAGE_SIZE, snapshot(vec![post("seed", 100)]), t.revision, cx);
                t.load_older(cx);
            });
            wait_for_pagination(cx, &timeline);
            timeline.update(cx, |t, cx| {
                assert!(t.older_failed());
                t.insert_pending(post("draft", 300), cx);
                t.delete(&post("seed", 100), cx);
                assert!(!t.older_failed());
                assert!(!t.has_older());
                t.load_older(cx);
                assert!(!t.is_loading_older());
                // A full server page can also leave no confirmed cursor after filtering.
                t.replace_all(PAGE_SIZE, snapshot(vec![]), t.revision, cx);
                t.load_older(cx);
                assert!(!t.has_older());
                assert!(!t.older_failed());
            });
        }
    }

    #[gpui_kit::test]
    fn superseded_pagination_failure_cannot_change_a_new_snapshot(cx: &mut TestAppContext) {
        use mattermost::testing::{Network, Reply, SlowServer};
        cx.executor().allow_parking();
        let server = SlowServer::start(Network::new(std::time::Duration::ZERO), |request| {
            if request.method() == "GET"
                && request.path().starts_with("/api/v4/channels/c/posts?")
                && request.path().contains("before=")
            {
                return Reply::not_found();
            }
            Reply::json(serde_json::json!([]))
        });
        let (timeline, _) =
            timeline_with_client(Client::new_unverified(&server.url(), "token").unwrap(), cx);
        timeline.update(cx, |t, cx| {
            t.replace_all(PAGE_SIZE, snapshot(vec![post("seed", 100)]), t.revision, cx);
            t.load_older(cx);
            // The replacement completes before the old page's response is applied.
            t.replace_all(0, snapshot(vec![]), t.revision, cx);
        });
        wait_for_pagination(cx, &timeline);
        timeline.read_with(cx, |t, _| {
            assert!(!t.older_failed());
            assert!(!t.is_loading_older());
            assert!(!t.has_older());
            assert!(t.entries().is_empty());
            assert!(t.deleted.is_empty());
        });
    }

    #[gpui_kit::test]
    fn tombstones_only_cover_matching_in_flight_requests(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("a", 10), cx);
            t.delete(&post("a", 10), cx);
            assert!(t.deleted.is_empty());
            t.begin_request();
            t.begin_request();
            let mut unrelated = post("elsewhere", 20);
            unrelated.channel_id = "other".into();
            t.delete(&unrelated, cx);
            assert!(t.deleted.is_empty());
            // A root not loaded yet may still be present in an outstanding page.
            t.delete(&post("b", 20), cx);
            assert_eq!(t.deleted.len(), 1);
            t.finish_request();
            t.upsert(post("b", 20), cx);
            assert!(t.entries().is_empty());
            t.finish_request();
            assert!(t.deleted.is_empty());
            assert_eq!(t.requests_in_flight, 0);
        });
    }

    #[gpui_kit::test]
    fn failed_requests_release_tombstones(cx: &mut TestAppContext) {
        use mattermost::testing::{Network, Reply, SlowServer};
        use std::time::{Duration, Instant};

        cx.executor().allow_parking();
        let server = SlowServer::start(Network::new(Duration::from_millis(20)), |_| {
            Reply::json(serde_json::json!({ "message": "Forbidden" })).status(403)
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let (timeline, _) = timeline_with_client(client, cx);
        timeline.update(cx, |t, cx| {
            t.reload(cx);
            t.reload(cx);
            assert_eq!(t.requests_in_flight, 1);
            t.delete(&post("a", 10), cx);
            assert_eq!(t.deleted.len(), 1);
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if timeline.read_with(cx, |t, _| matches!(t.state(), LoadState::Failed(_))) {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for refusal");
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
        timeline.read_with(cx, |t, _| {
            assert!(t.deleted.is_empty());
            assert_eq!(t.requests_in_flight, 0);
        });
    }

    #[gpui_kit::test]
    fn a_full_page_of_deleted_posts_does_not_end_pagination(cx: &mut TestAppContext) {
        use mattermost::testing::{Network, Reply, SlowServer};
        use std::time::{Duration, Instant};

        cx.executor().allow_parking();
        let server = SlowServer::start(Network::new(Duration::from_millis(100)), |request| {
            if request.path().contains("before=") {
                let posts: serde_json::Map<_, _> = (0..PAGE_SIZE)
                    .map(|ix| {
                        let id = format!("old{ix}");
                        (
                            id.clone(),
                            serde_json::json!({ "id": id, "channel_id": "c", "create_at": ix + 1 }),
                        )
                    })
                    .collect();
                let order: Vec<_> = posts.keys().cloned().collect();
                return Reply::json(serde_json::json!({ "order": order, "posts": posts }));
            }
            Reply::not_found()
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let (timeline, _) = timeline_with_client(client, cx);
        timeline.update(cx, |t, cx| {
            t.replace_all(PAGE_SIZE, snapshot(vec![post("seed", 100)]), t.revision, cx);
            t.load_older(cx);
            for ix in 0..PAGE_SIZE {
                t.delete(&post(&format!("old{ix}"), ix as Millis + 1), cx);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if !timeline.read_with(cx, |t, _| t.is_loading_older()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for older posts"
            );
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
        timeline.read_with(cx, |t, _| {
            assert!(t.has_older());
            assert_eq!(t.entries().len(), 1);
            assert!(t.deleted.is_empty());
            assert_eq!(t.requests_in_flight, 0);
        });
    }

    #[gpui_kit::test]
    fn catch_up_uses_only_confirmed_times_with_an_overlap(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.insert_pending(post("failed", 900_000), cx);
            t.set_delivery(&"failed".into(), Delivery::Failed, cx);
            t.insert_pending(post("pending", 1_000_000), cx);
            assert_eq!(t.confirmed_since(), None);
            let mut confirmed = post("a", 2_000);
            confirmed.update_at = 3_000;
            confirmed.edit_at = 4_000;
            t.upsert(confirmed, cx);
            assert_eq!(t.confirmed_since(), Some(3_000));
            t.upsert(post("b", 100), cx);
            assert_eq!(t.confirmed_since(), Some(3_000));
        });
    }

    #[gpui_kit::test]
    fn stale_posts_cannot_undo_edits_or_deletes(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            let mut edited = post("a", 10);
            edited.edit_at = 30;
            edited.message = "new edit".into();
            t.upsert(edited, cx);
            t.upsert(post("a", 10), cx);
            assert_eq!(t.entries()[0].post().message, "new edit");
            let mut stale = post("a", 10);
            stale.update_at = 20;
            t.upsert(stale, cx);
            assert_eq!(t.entries()[0].post().message, "new edit");
            t.begin_request();
            t.delete(&post("a", 10), cx);
            t.upsert(post("a", 10), cx);
            // Deletes that arrive before a post is loaded also leave a tombstone.
            t.delete(&post("b", 20), cx);
            t.upsert(post("b", 20), cx);
            assert!(t.entries().is_empty());
            t.finish_request();
        });
    }

    #[gpui_kit::test]
    fn reload_keeps_changes_made_while_the_snapshot_was_in_flight(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("old-page", 1), cx);
            t.upsert(post("edited", 10), cx);
            t.upsert(post("deleted", 20), cx);
            let revision = t.revision;
            t.begin_request();
            let mut edited = post("edited", 10);
            edited.update_at = 40;
            edited.message = "live edit".into();
            t.upsert(edited, cx);
            t.upsert(post("live", 30), cx);
            t.delete(&post("deleted", 20), cx);
            t.insert_pending(post("pending", 100), cx);
            t.replace_all(
                2,
                snapshot(vec![post("edited", 10), post("deleted", 20)]),
                revision,
                cx,
            );
            assert_eq!(
                t.entries()
                    .iter()
                    .map(|e| e.id().as_str())
                    .collect::<Vec<_>>(),
                ["edited", "live", "pending"]
            );
            assert_eq!(t.entries()[0].post().message, "live edit");
            assert_eq!(t.entries()[2].delivery(), Delivery::Pending);
            t.finish_request();
        });
    }

    #[gpui_kit::test]
    fn newer_rest_snapshots_replace_reactions_without_replaying_events(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("a", 10), cx);
            let revision = t.revision;
            let mut reaction = Reaction::default();
            reaction.post_id = "a".into();
            reaction.user_id = "u".into();
            reaction.emoji_name = "+1".into();
            reaction.create_at = 20;
            t.apply_reaction(&reaction, true, cx);
            t.apply_reaction(&reaction, true, cx);
            // The snapshot includes a later removal, even though its request began first.
            let mut removed = post("a", 10);
            removed.update_at = 30;
            t.upsert_from(removed, Some(revision), cx);
            assert!(t.entries()[0].post().metadata.reactions.is_empty());
            // A reaction for an unloaded post is supplied by the authoritative snapshot.
            reaction.post_id = "b".into();
            t.apply_reaction(&reaction, true, cx);
            let mut loaded = post("b", 15);
            loaded.update_at = 40;
            loaded.metadata.reactions.push(reaction);
            let revision = t.revision;
            t.replace_all(2, snapshot(vec![post("a", 10), loaded]), revision, cx);
            assert_eq!(t.entries()[1].post().metadata.reactions.len(), 1);
        });
    }

    #[gpui_kit::test]
    fn reload_reconciles_a_failed_post_with_its_server_echo(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            let mut pending = post("me:99", 99);
            pending.pending_post_id = "me:99".into();
            t.insert_pending(pending, cx);
            t.set_delivery(&"me:99".into(), Delivery::Failed, cx);
            t.upsert(post("later", 75), cx);
            let revision = t.revision;
            let mut confirmed = post("real", 50);
            confirmed.pending_post_id = "me:99".into();
            t.replace_all(
                2,
                snapshot(vec![confirmed.clone(), post("later", 75)]),
                revision,
                cx,
            );
            t.upsert(confirmed, cx);
            assert_eq!(t.entries().len(), 2);
            assert_eq!(t.entries()[0].id().as_str(), "real");
            assert_eq!(t.entries()[1].id().as_str(), "later");
            assert_eq!(t.entries()[0].delivery(), Delivery::Sent);
        });
    }

    #[gpui_kit::test]
    fn identical_posts_do_not_emit_updates_or_advance_the_revision(cx: &mut TestAppContext) {
        let (timeline, events) = timeline(cx);
        let mut original = post("a", 10);
        original
            .props
            .insert("override_username".into(), serde_json::json!("bot"));
        original
            .metadata
            .embeds
            .push(mattermost::PostEmbed::default());
        original
            .metadata
            .files
            .push(mattermost::FileInfo::default());
        original.metadata.reactions.push(Reaction::default());
        original
            .metadata
            .images
            .insert("image".into(), mattermost::ImageSize::default());
        timeline.update(cx, |t, cx| t.upsert(original.clone(), cx));
        events.borrow_mut().clear();
        timeline.update(cx, |t, cx| {
            let revision = t.revision;
            t.upsert_from(original.clone(), Some(revision), cx);
            t.upsert(original.clone(), cx);
            assert_eq!(t.revision, revision);
        });
        assert!(events.borrow().is_empty());
        timeline.update(cx, |t, cx| {
            let mut changed = original.clone();
            changed.metadata.reactions.clear();
            t.upsert(changed, cx);
        });
        assert_eq!(*events.borrow(), [TimelineEvent::Updated(0)]);
    }

    #[gpui_kit::test]
    fn keeps_chronological_order_and_reports_splices(cx: &mut TestAppContext) {
        let (timeline, events) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("b", 20), cx);
            t.upsert(post("a", 10), cx);
            t.upsert(post("c", 30), cx);
        });
        assert_eq!(ids(&timeline, cx), ["a", "b", "c"]);
        assert_eq!(
            *events.borrow(),
            [
                TimelineEvent::Spliced {
                    old: 0..0,
                    new_count: 1
                },
                TimelineEvent::Spliced {
                    old: 0..0,
                    new_count: 1
                },
                TimelineEvent::Spliced {
                    old: 2..2,
                    new_count: 1
                },
            ]
        );
    }

    #[gpui_kit::test]
    fn edits_update_in_place_and_ignore_replies(cx: &mut TestAppContext) {
        let (timeline, events) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("a", 10), cx);
            let mut edited = post("a", 10);
            edited.message = "edited".into();
            t.upsert(edited, cx);
            let mut reply = post("r", 11);
            reply.root_id = "a".into();
            t.upsert(reply, cx);
        });
        assert_eq!(ids(&timeline, cx), ["a"]);
        assert_eq!(events.borrow().last(), Some(&TimelineEvent::Updated(0)));
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries()[0].post().message, "edited");
        });
    }

    #[gpui_kit::test]
    fn confirmed_post_replaces_its_optimistic_entry(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        timeline.update(cx, |t, cx| {
            t.upsert(post("a", 10), cx);
            let mut pending = post("me:99", 99);
            pending.pending_post_id = "me:99".into();
            t.insert_pending(pending, cx);
            // Someone else's post arriving meanwhile stays before ours.
            t.upsert(post("b", 50), cx);
        });
        assert_eq!(ids(&timeline, cx), ["a", "b", "me:99"]);
        timeline.update(cx, |t, cx| {
            let mut confirmed = post("real", 100);
            confirmed.pending_post_id = "me:99".into();
            t.upsert(confirmed, cx);
        });
        assert_eq!(ids(&timeline, cx), ["a", "b", "real"]);
        timeline.read_with(cx, |t, _| {
            assert_eq!(t.entries()[2].delivery(), Delivery::Sent);
        });
    }

    #[gpui_kit::test]
    fn new_custom_emoji_update_only_affected_posts(cx: &mut TestAppContext) {
        let (timeline, events) = timeline(cx);
        timeline.update(cx, |t, cx| {
            let mut parrot = post("a", 10);
            parrot.message = "nice :partyparrot:".into();
            t.upsert(parrot, cx);
            t.upsert(post("b", 20), cx);
        });
        events.borrow_mut().clear();
        let custom = CustomEmojis::new([("partyparrot".into(), "http://localhost:1/e.png".into())]);
        timeline.update(cx, |t, cx| t.set_custom_emoji(custom.clone(), cx));
        assert_eq!(*events.borrow(), [TimelineEvent::Updated(0)]);
        timeline.read_with(cx, |t, _| {
            assert!(t.entries()[0].message().is_rich());
            assert!(!t.entries()[1].message().is_rich());
        });
        // The same set again changes nothing.
        timeline.update(cx, |t, cx| t.set_custom_emoji(custom, cx));
        assert_eq!(events.borrow().len(), 1);
    }

    #[gpui_kit::test]
    fn reactions_toggle_once_per_user(cx: &mut TestAppContext) {
        let (timeline, _) = timeline(cx);
        let mut reaction = Reaction::default();
        reaction.post_id = "a".into();
        reaction.user_id = "u".into();
        reaction.emoji_name = "+1".into();
        timeline.update(cx, |t, cx| {
            t.upsert(post("a", 10), cx);
            t.apply_reaction(&reaction, true, cx);
            t.apply_reaction(&reaction, true, cx);
        });
        let count = |cx: &mut TestAppContext| {
            timeline.read_with(cx, |t, _| t.entries()[0].post().metadata.reactions.len())
        };
        assert_eq!(count(cx), 1);
        timeline.update(cx, |t, cx| t.apply_reaction(&reaction, false, cx));
        assert_eq!(count(cx), 0);
    }
}
