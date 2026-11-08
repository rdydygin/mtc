use std::{ops::Range, sync::Arc, time::SystemTime};

use gpui_kit::{Context, Entity, EventEmitter, SharedString, Task};
use mattermost::{ChannelId, Client, Millis, Post, PostId, PostList, Reaction, UserId};

use crate::{CustomEmojis, Directory, PreparedMessage};

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

/// One post as the UI shows it.
#[derive(Clone, Debug)]
pub struct Entry {
    post: Arc<Post>,
    message: PreparedMessage,
    delivery: Delivery,
}

impl Entry {
    fn new(post: Post, delivery: Delivery, custom: &CustomEmojis) -> Self {
        let message = PreparedMessage::new(&post.message, custom);
        Self {
            post: Arc::new(post),
            message,
            delivery,
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
        let message = PreparedMessage::new(&self.post.message, custom);
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
    custom_emoji: CustomEmojis,
    entries: Vec<Entry>,
    state: LoadState,
    has_older: bool,
    loading_older: bool,
    unread_after: Option<Millis>,
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
        custom_emoji: CustomEmojis,
    ) -> Self {
        Self {
            kind,
            client,
            directory,
            custom_emoji,
            entries: Vec::new(),
            state: LoadState::NotLoaded,
            has_older: true,
            loading_older: false,
            unread_after: None,
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
            cx.emit(TimelineEvent::Reset);
            cx.notify();
        }
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
        self.state = LoadState::Loading;
        cx.notify();
        let client = self.client.clone();
        let kind = self.kind.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = match &kind {
                TimelineKind::Channel(channel) => client.latest_posts(channel, PAGE_SIZE).await,
                TimelineKind::Thread { root, .. } => client.thread(root).await,
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(list) => this.replace_all(list, cx),
                Err(err) => {
                    // Keep showing what we had; surface the failure.
                    this.state = LoadState::Failed(format!("{err:#}").into());
                    cx.notify();
                }
            });
        }));
    }

    fn replace_all(&mut self, list: PostList, cx: &mut Context<Self>) {
        let posts = list.into_chronological();
        self.has_older = matches!(self.kind, TimelineKind::Channel(_)) && posts.len() >= PAGE_SIZE;
        // Optimistic posts survive a reload until the server answers.
        let pending: Vec<Entry> = self
            .entries
            .drain(..)
            .filter(|e| e.delivery != Delivery::Sent)
            .collect();
        self.entries = posts
            .into_iter()
            .filter(|p| self.accepts(p))
            .map(|p| Entry::new(p, Delivery::Sent, &self.custom_emoji))
            .collect();
        self.entries.extend(pending);
        self.state = LoadState::Loaded;
        self.request_authors(0..self.entries.len(), cx);
        cx.emit(TimelineEvent::Reset);
        cx.notify();
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
            return;
        };
        self.loading_older = true;
        cx.notify();
        let client = self.client.clone();
        let channel = channel.clone();
        let before = oldest.id().clone();
        cx.spawn(async move |this, cx| {
            let result = client.posts_before(&channel, &before, PAGE_SIZE).await;
            let _ = this.update(cx, |this, cx| {
                this.loading_older = false;
                match result {
                    Ok(list) => {
                        let posts: Vec<Post> = list
                            .into_chronological()
                            .into_iter()
                            .filter(|p| this.accepts(p) && this.position(&p.id).is_none())
                            .collect();
                        this.has_older = posts.len() >= PAGE_SIZE / 2;
                        let count = posts.len();
                        this.entries.splice(
                            0..0,
                            posts
                                .into_iter()
                                .map(|p| Entry::new(p, Delivery::Sent, &this.custom_emoji)),
                        );
                        this.request_authors(0..count, cx);
                        cx.emit(TimelineEvent::Spliced {
                            old: 0..0,
                            new_count: count,
                        });
                    }
                    Err(err) => log::warn!("loading older posts: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Catch up after a reconnect: apply everything that changed while the
    /// socket was down.
    pub fn catch_up(&mut self, cx: &mut Context<Self>) {
        if self.state != LoadState::Loaded {
            return;
        }
        let TimelineKind::Channel(channel) = &self.kind else {
            // A thread is small; reloading it is simpler than merging.
            self.reload(cx);
            return;
        };
        let Some(since) = self.entries.iter().map(Entry::changed_at).max() else {
            self.reload(cx);
            return;
        };
        let client = self.client.clone();
        let channel = channel.clone();
        cx.spawn(async move |this, cx| {
            let Ok(list) = client.posts_since(&channel, since).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                for post in list.into_chronological() {
                    if post.delete_at > 0 {
                        this.remove(&post.id, cx);
                    } else {
                        this.upsert(post, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn accepts(&self, post: &Post) -> bool {
        match &self.kind {
            TimelineKind::Channel(channel) => &post.channel_id == channel && !post.is_reply(),
            TimelineKind::Thread { root, .. } => &post.id == root || &post.root_id == root,
        }
    }

    /// Insert a new post or replace a known one, keeping chronological order.
    /// A post echoing an optimistic entry's `pending_post_id` replaces it.
    pub fn upsert(&mut self, post: Post, cx: &mut Context<Self>) {
        if !self.accepts(&post) {
            return;
        }
        if let Some(ix) = self.position(&post.id) {
            self.entries[ix] = Entry::new(post, Delivery::Sent, &self.custom_emoji);
            cx.emit(TimelineEvent::Updated(ix));
            cx.notify();
            return;
        }
        if !post.pending_post_id.is_empty() {
            let pending = PostId::from(post.pending_post_id.as_str());
            if let Some(ix) = self.position(&pending) {
                self.entries[ix] = Entry::new(post, Delivery::Sent, &self.custom_emoji);
                cx.emit(TimelineEvent::Updated(ix));
                cx.notify();
                return;
            }
        }
        // Pending entries stay at the tail, after every confirmed post.
        let ix = self
            .entries
            .iter()
            .rposition(|e| e.delivery == Delivery::Sent && e.post.create_at <= post.create_at)
            .map_or(0, |ix| ix + 1);
        self.directory
            .update(cx, |d, cx| d.ensure([post.user_id.clone()], cx));
        self.entries
            .insert(ix, Entry::new(post, Delivery::Sent, &self.custom_emoji));
        cx.emit(TimelineEvent::Spliced {
            old: ix..ix,
            new_count: 1,
        });
        cx.notify();
    }

    pub fn remove(&mut self, id: &PostId, cx: &mut Context<Self>) {
        if let Some(ix) = self.position(id) {
            self.entries.remove(ix);
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
            cx.emit(TimelineEvent::Updated(ix));
            cx.notify();
        }
    }

    pub fn apply_reaction(&mut self, reaction: &Reaction, added: bool, cx: &mut Context<Self>) {
        self.update_post(&reaction.post_id, cx, |post| {
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

    fn request_authors(&self, range: Range<usize>, cx: &mut Context<Self>) {
        let ids: Vec<UserId> = self.entries[range]
            .iter()
            .map(|e| e.post.user_id.clone())
            .collect();
        self.directory.update(cx, |d, cx| d.ensure(ids, cx));
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
