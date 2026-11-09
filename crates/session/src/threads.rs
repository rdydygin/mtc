use std::{collections::HashMap, sync::Arc, time::Duration};

use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, SharedString, Task};
use mattermost::{
    ChannelId, Client, Millis, Post, PostId, TeamId, UserId, UserThread, UserThreads,
};

use crate::{CustomEmojis, Directory, LoadState, emojify, retry::retry, timeline::now_millis};

/// Threads requested per page.
const PAGE_SIZE: usize = 30;
/// How long to wait before re-reading the unread totals after a change they
/// can't be worked out from, so a burst of changes costs one request.
const TOTALS_DELAY: Duration = Duration::from_millis(500);

/// Which of the Threads pane's lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ThreadFilter {
    #[default]
    All,
    Unread,
}

/// A thread the user follows, as the Threads pane lists it.
#[derive(Clone, Debug)]
pub struct FollowedThread {
    root: Arc<Post>,
    reply_count: i64,
    last_reply_at: Millis,
    participants: Vec<UserId>,
    unread_replies: i64,
    unread_mentions: i64,
    preview: SharedString,
}

impl FollowedThread {
    fn new(thread: UserThread) -> Self {
        let mut root = thread.post;
        if root.id.is_empty() {
            root.id = thread.id;
        }
        Self {
            preview: preview(&root),
            root: Arc::new(root),
            reply_count: thread.reply_count,
            last_reply_at: thread.last_reply_at,
            participants: thread
                .participants
                .into_iter()
                .map(|user| user.id)
                .filter(|id| !id.is_empty())
                .collect(),
            unread_replies: thread.unread_replies,
            unread_mentions: thread.unread_mentions,
        }
    }

    /// The root post's identity, which is also the thread's.
    pub fn id(&self) -> &PostId {
        &self.root.id
    }

    pub fn root(&self) -> &Arc<Post> {
        &self.root
    }

    pub fn channel(&self) -> &ChannelId {
        &self.root.channel_id
    }

    pub fn reply_count(&self) -> i64 {
        self.reply_count
    }

    /// When the thread last changed: its newest reply, or the root.
    pub fn last_reply_at(&self) -> Millis {
        self.last_reply_at.max(self.root.create_at)
    }

    /// Everyone who has posted in the thread, earliest first.
    pub fn participants(&self) -> &[UserId] {
        &self.participants
    }

    pub fn unread_replies(&self) -> i64 {
        self.unread_replies
    }

    pub fn unread_mentions(&self) -> i64 {
        self.unread_mentions
    }

    pub fn is_unread(&self) -> bool {
        self.unread_replies > 0 || self.unread_mentions > 0
    }

    /// The root's message as one line of plain text.
    pub fn preview(&self) -> &SharedString {
        &self.preview
    }

    fn counts(&self) -> (i64, i64) {
        (self.unread_replies, self.unread_mentions)
    }

    fn set_unread(&mut self, replies: i64, mentions: i64) {
        self.unread_replies = replies;
        self.unread_mentions = mentions;
    }
}

#[derive(Clone, Debug)]
pub enum ThreadsEvent {
    /// A command failed; the lists were reloaded to undo it.
    Failed { message: SharedString },
}

/// One of the Threads pane's lists: threads in order of their newest reply,
/// paged from the server.
struct ThreadList {
    ids: Vec<PostId>,
    state: LoadState,
    has_more: bool,
    /// The load in flight, first page or next; a new first page cancels it.
    task: Option<Task<()>>,
}

impl Default for ThreadList {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            state: LoadState::NotLoaded,
            has_more: false,
            task: None,
        }
    }
}

/// The threads the user follows in the current team (collapsed reply
/// threads), with the unread totals the sidebar shows beside them.
///
/// The unread list is a separate query rather than a filter of the full
/// one, so an old unread thread shows without paging through everything
/// newer. A thread read while that list is up stays in it; views decide
/// whether to show it.
pub struct FollowedThreads {
    client: Client,
    me: UserId,
    directory: Entity<Directory>,
    team: Option<TeamId>,
    /// Every thread known, whether or not a list shows it, so changes to it
    /// can be applied to the totals.
    threads: HashMap<PostId, FollowedThread>,
    all: ThreadList,
    unread: ThreadList,
    unread_threads: i64,
    unread_mentions: i64,
    _totals_task: Option<Task<()>>,
}

impl EventEmitter<ThreadsEvent> for FollowedThreads {}

impl FollowedThreads {
    pub fn new(client: Client, me: UserId, directory: Entity<Directory>) -> Self {
        Self {
            client,
            me,
            directory,
            team: None,
            threads: HashMap::new(),
            all: ThreadList::default(),
            unread: ThreadList::default(),
            unread_threads: 0,
            unread_mentions: 0,
            _totals_task: None,
        }
    }

    /// Show `team`'s threads, forgetting another team's.
    pub fn set_team(&mut self, team: Option<TeamId>, cx: &mut Context<Self>) {
        if self.team == team {
            return;
        }
        self.team = team;
        self.threads.clear();
        self.all = ThreadList::default();
        self.unread = ThreadList::default();
        self.unread_threads = 0;
        self.unread_mentions = 0;
        self.load_totals(Duration::ZERO, cx);
        cx.notify();
    }

    /// Threads with replies the user hasn't read.
    pub fn unread_threads(&self) -> i64 {
        self.unread_threads
    }

    /// Unread replies that mention the user, across every thread.
    pub fn unread_mentions(&self) -> i64 {
        self.unread_mentions
    }

    pub fn thread(&self, id: &PostId) -> Option<&FollowedThread> {
        self.threads.get(id)
    }

    /// The threads of a list, newest reply first.
    pub fn threads(&self, filter: ThreadFilter) -> impl Iterator<Item = &FollowedThread> {
        self.list(filter)
            .ids
            .iter()
            .filter_map(|id| self.threads.get(id))
    }

    pub fn state(&self, filter: ThreadFilter) -> &LoadState {
        &self.list(filter).state
    }

    /// Whether older threads remain on the server.
    pub fn has_more(&self, filter: ThreadFilter) -> bool {
        self.list(filter).has_more
    }

    pub fn is_loading_more(&self, filter: ThreadFilter) -> bool {
        let list = self.list(filter);
        list.state == LoadState::Loaded && list.task.is_some()
    }

    fn list(&self, filter: ThreadFilter) -> &ThreadList {
        match filter {
            ThreadFilter::All => &self.all,
            ThreadFilter::Unread => &self.unread,
        }
    }

    fn list_mut(&mut self, filter: ThreadFilter) -> &mut ThreadList {
        match filter {
            ThreadFilter::All => &mut self.all,
            ThreadFilter::Unread => &mut self.unread,
        }
    }

    // ----- Loading ---------------------------------------------------------

    /// Load a list's first page, unless it is loaded or loading.
    pub fn load(&mut self, filter: ThreadFilter, cx: &mut Context<Self>) {
        if matches!(
            self.list(filter).state,
            LoadState::NotLoaded | LoadState::Failed(_)
        ) {
            self.reload(filter, cx);
        }
    }

    /// Replace a list with the newest page from the server.
    pub fn reload(&mut self, filter: ThreadFilter, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            // Without a team there is nothing to follow.
            self.list_mut(filter).state = LoadState::Loaded;
            cx.notify();
            return;
        };
        let task = self.fetch(team, filter, None, cx);
        let list = self.list_mut(filter);
        list.state = LoadState::Loading;
        list.task = Some(task);
        cx.notify();
    }

    /// Fetch the page after the oldest thread of a list.
    pub fn load_more(&mut self, filter: ThreadFilter, cx: &mut Context<Self>) {
        let list = self.list(filter);
        if list.state != LoadState::Loaded || list.task.is_some() || !list.has_more {
            return;
        }
        let (Some(team), Some(oldest)) = (self.team.clone(), list.ids.last().cloned()) else {
            return;
        };
        let task = self.fetch(team, filter, Some(oldest), cx);
        self.list_mut(filter).task = Some(task);
        cx.notify();
    }

    fn fetch(
        &self,
        team: TeamId,
        filter: ThreadFilter,
        before: Option<PostId>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let client = self.client.clone();
        let me = self.me.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let unread_only = filter == ThreadFilter::Unread;
            let result = retry(&executor, || {
                client.followed_threads(&me, &team, before.as_ref(), unread_only, PAGE_SIZE)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.list_mut(filter).task = None;
                match result {
                    Ok(page) => this.adopt_page(filter, page, before.is_none(), cx),
                    Err(err) if before.is_none() => {
                        this.list_mut(filter).state = LoadState::Failed(format!("{err:#}").into());
                    }
                    Err(err) => log::warn!("loading more threads: {err:#}"),
                }
                cx.notify();
            });
        })
    }

    /// Take in a page of a list: the first replaces it, later ones extend it.
    fn adopt_page(
        &mut self,
        filter: ThreadFilter,
        page: UserThreads,
        first: bool,
        cx: &mut Context<Self>,
    ) {
        // Each page carries fresh totals.
        self.unread_threads = page.total_unread_threads;
        self.unread_mentions = page.total_unread_mentions;
        let has_more = page.threads.len() >= PAGE_SIZE;
        let mut ids = Vec::with_capacity(page.threads.len());
        let mut people = Vec::new();
        for thread in page.threads {
            let thread = FollowedThread::new(thread);
            people.extend(thread.people());
            ids.push(thread.id().clone());
            self.threads.insert(thread.id().clone(), thread);
        }
        self.directory.update(cx, |d, cx| d.ensure(people, cx));
        let list = self.list_mut(filter);
        list.has_more = has_more;
        list.state = LoadState::Loaded;
        if first {
            list.ids = ids;
            self.forget_unlisted();
        } else {
            ids.retain(|id| !list.ids.contains(id));
            list.ids.extend(ids);
        }
    }

    /// Drop threads neither list shows. Changes to them are no longer
    /// followed, so what is known about them may be out of date.
    fn forget_unlisted(&mut self) {
        let (all, unread) = (&self.all.ids, &self.unread.ids);
        self.threads
            .retain(|id, _| all.contains(id) || unread.contains(id));
    }

    /// Bring everything up to date after a reconnect.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_totals(Duration::ZERO, cx);
        for filter in [ThreadFilter::All, ThreadFilter::Unread] {
            if self.list(filter).state != LoadState::NotLoaded {
                self.reload(filter, cx);
            }
        }
    }

    /// Read the unread totals after `delay`, replacing a read already
    /// waiting.
    fn load_totals(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            return;
        };
        let client = self.client.clone();
        let me = self.me.clone();
        self._totals_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            if !delay.is_zero() {
                executor.timer(delay).await;
            }
            let result = retry(&executor, || client.followed_thread_totals(&me, &team)).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(totals) if this.team.as_ref() == Some(&team) => {
                    this.unread_threads = totals.total_unread_threads;
                    this.unread_mentions = totals.total_unread_mentions;
                    cx.notify();
                }
                Ok(_) => {}
                Err(err) => log::warn!("loading thread totals: {err:#}"),
            });
        }));
    }

    // ----- Changes from the server -------------------------------------------

    /// Totals follow a known thread's change exactly. An unknown thread's
    /// earlier state isn't known, so the totals are read again instead.
    fn adjust_totals(
        &mut self,
        before: Option<(i64, i64)>,
        after: (i64, i64),
        cx: &mut Context<Self>,
    ) {
        let Some((replies, mentions)) = before else {
            self.load_totals(TOTALS_DELAY, cx);
            return;
        };
        let unread = |replies: i64| i64::from(replies > 0);
        self.unread_threads = (self.unread_threads + unread(after.0) - unread(replies)).max(0);
        self.unread_mentions = (self.unread_mentions + after.1 - mentions).max(0);
    }

    /// A followed thread changed, usually with a new reply.
    pub fn apply_update(&mut self, thread: UserThread, cx: &mut Context<Self>) {
        let thread = FollowedThread::new(thread);
        let id = thread.id().clone();
        let before = self.threads.get(&id).map(FollowedThread::counts);
        self.adjust_totals(before, thread.counts(), cx);
        self.directory
            .update(cx, |d, cx| d.ensure(thread.people(), cx));
        let unread = thread.is_unread();
        self.threads.insert(id.clone(), thread);
        self.place(ThreadFilter::All, &id, true);
        self.place(ThreadFilter::Unread, &id, unread);
        cx.notify();
    }

    /// Move `id` to its place in a list by its newest reply. A thread not in
    /// the list joins it when `join`, unless it is older than every loaded
    /// thread while more remain on the server.
    fn place(&mut self, filter: ThreadFilter, id: &PostId, join: bool) {
        let Some(at) = self.threads.get(id).map(FollowedThread::last_reply_at) else {
            return;
        };
        let threads = &self.threads;
        let list = match filter {
            ThreadFilter::All => &mut self.all,
            ThreadFilter::Unread => &mut self.unread,
        };
        if list.state != LoadState::Loaded {
            return;
        }
        let listed = match list.ids.iter().position(|other| other == id) {
            Some(ix) => {
                list.ids.remove(ix);
                true
            }
            None => false,
        };
        if !listed && !join {
            return;
        }
        let ix = list.ids.partition_point(|other| {
            threads
                .get(other)
                .is_some_and(|other| other.last_reply_at() >= at)
        });
        if listed || ix < list.ids.len() || !list.has_more {
            list.ids.insert(ix, id.clone());
        }
    }

    /// The user read a thread, here or on another device; what remains
    /// unread is what arrived since.
    pub fn note_read(
        &mut self,
        id: &PostId,
        unread_replies: i64,
        unread_mentions: i64,
        cx: &mut Context<Self>,
    ) {
        let before = self.threads.get(id).map(FollowedThread::counts);
        self.adjust_totals(before, (unread_replies, unread_mentions), cx);
        if let Some(thread) = self.threads.get_mut(id) {
            thread.set_unread(unread_replies, unread_mentions);
        }
        cx.notify();
    }

    /// The user read every thread in `channel`, or with `None`, in the team.
    pub fn note_all_read(&mut self, channel: Option<&ChannelId>, cx: &mut Context<Self>) {
        for thread in self.threads.values_mut() {
            if channel.is_none_or(|channel| thread.channel() == channel) {
                thread.set_unread(0, 0);
            }
        }
        match channel {
            Some(_) => self.load_totals(TOTALS_DELAY, cx),
            None => {
                self.unread_threads = 0;
                self.unread_mentions = 0;
            }
        }
        cx.notify();
    }

    /// The user started or stopped following a thread, possibly elsewhere.
    pub fn note_following(&mut self, id: &PostId, following: bool, cx: &mut Context<Self>) {
        if !following {
            // One not loaded may still count in the totals.
            if self.thread(id).is_none() {
                self.load_totals(TOTALS_DELAY, cx);
            }
            self.forget(id, cx);
            return;
        }
        let Some(team) = self.team.clone() else {
            return;
        };
        let client = self.client.clone();
        let me = self.me.clone();
        let id = id.clone();
        cx.spawn(
            async move |this, cx| match client.followed_thread(&me, &team, &id).await {
                Ok(thread) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.team.as_ref() == Some(&team) {
                            this.apply_update(thread, cx);
                        }
                    });
                }
                Err(err) => log::warn!("loading a followed thread: {err:#}"),
            },
        )
        .detach();
    }

    /// The root of a thread was edited.
    pub fn note_root_edited(&mut self, post: &Post, cx: &mut Context<Self>) {
        if let Some(thread) = self.threads.get_mut(&post.id) {
            thread.preview = preview(post);
            thread.root = Arc::new(post.clone());
            cx.notify();
        }
    }

    /// A thread is no longer followed, or its root was deleted.
    pub fn forget(&mut self, id: &PostId, cx: &mut Context<Self>) {
        let before = self.threads.get(id).map(FollowedThread::counts);
        if before.is_none() && !self.all.ids.contains(id) && !self.unread.ids.contains(id) {
            return;
        }
        self.adjust_totals(before, (0, 0), cx);
        self.threads.remove(id);
        self.all.ids.retain(|other| other != id);
        self.unread.ids.retain(|other| other != id);
        cx.notify();
    }

    /// Forget the threads of channels the user is no longer in, such as one
    /// they left. The server stops counting them too, including any on pages
    /// not loaded yet, so its totals are read again.
    pub fn keep_channels(&mut self, member: impl Fn(&ChannelId) -> bool, cx: &mut Context<Self>) {
        let gone: Vec<PostId> = self
            .threads
            .values()
            .filter(|thread| !member(thread.channel()))
            .map(|thread| thread.id().clone())
            .collect();
        if gone.is_empty() {
            return;
        }
        for id in &gone {
            self.forget(id, cx);
        }
        self.load_totals(Duration::ZERO, cx);
    }

    // ----- Commands ----------------------------------------------------------

    /// Mark a thread read now, unless it is known to be read already.
    pub fn mark_read(&mut self, id: &PostId, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            return;
        };
        let now = now_millis();
        let at = match self.threads.get_mut(id) {
            Some(thread) if !thread.is_unread() => return,
            Some(thread) => {
                let before = thread.counts();
                let at = now.max(thread.last_reply_at());
                thread.set_unread(0, 0);
                self.adjust_totals(Some(before), (0, 0), cx);
                cx.notify();
                at
            }
            None => now,
        };
        let client = self.client.clone();
        let me = self.me.clone();
        let id = id.clone();
        cx.background_spawn(async move {
            if let Err(err) = client.mark_thread_read(&me, &team, &id, at).await {
                log::warn!("marking a thread read: {err:#}");
            }
        })
        .detach();
    }

    /// Mark every followed thread in the team read.
    pub fn mark_all_read(&mut self, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            return;
        };
        self.note_all_read(None, cx);
        let client = self.client.clone();
        let me = self.me.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = client.mark_all_threads_read(&me, &team).await {
                let _ = this.update(cx, |this, cx| {
                    this.refresh(cx);
                    cx.emit(ThreadsEvent::Failed {
                        message: format!("Couldn’t mark your threads read. {err:#}").into(),
                    });
                });
            }
        })
        .detach();
    }

    /// Stop following a thread: it leaves the lists, and new replies no
    /// longer count as unread.
    pub fn unfollow(&mut self, id: &PostId, cx: &mut Context<Self>) {
        let Some(team) = self.team.clone() else {
            return;
        };
        self.forget(id, cx);
        let client = self.client.clone();
        let me = self.me.clone();
        let id = id.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = client.set_thread_following(&me, &team, &id, false).await {
                let _ = this.update(cx, |this, cx| {
                    this.refresh(cx);
                    cx.emit(ThreadsEvent::Failed {
                        message: format!("Couldn’t unfollow the thread. {err:#}").into(),
                    });
                });
            }
        })
        .detach();
    }
}

impl FollowedThread {
    /// Who the list row names or shows: the root's author and participants.
    fn people(&self) -> Vec<UserId> {
        std::iter::once(self.root.user_id.clone())
            .chain(self.participants.iter().cloned())
            .collect()
    }
}

/// A root post's message as one line of plain text for a list row, worked
/// out once rather than on every frame: Markdown markers dropped, standard
/// emoji drawn, and for a post without text, what it carries.
fn preview(post: &Post) -> SharedString {
    let message = emojify(&post.message, &CustomEmojis::default());
    let mut text = String::with_capacity(message.len());
    for line in message.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let line = strip_line_marker(line);
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&unlink(line));
    }
    let text: String = text
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.is_empty() {
        return text.into();
    }
    let files: Vec<&str> = post
        .metadata
        .files
        .iter()
        .map(|file| file.name.as_str())
        .collect();
    if !files.is_empty() {
        return files.join(", ").into();
    }
    // Integrations often post attachments with no message of their own.
    post.props
        .get("attachments")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|attachment| ["fallback", "title", "text"].map(|key| attachment.get(key)))
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
        .into()
}

/// A line without its heading, quote, or list marker.
fn strip_line_marker(line: &str) -> &str {
    let unheaded = line.trim_start_matches('#');
    if unheaded.len() < line.len() && unheaded.starts_with(' ') {
        return unheaded.trim_start();
    }
    for marker in ["> ", "- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    line.strip_prefix('>').unwrap_or(line)
}

/// `[text](url)` and `![text](url)` as just `text`.
fn unlink(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|ix| open + ix) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|ix| close + ix) else {
            break;
        };
        let start = if rest[..open].ends_with('!') {
            open - 1
        } else {
            open
        };
        out.push_str(&rest[..start]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use mattermost::User;
    use serde_json::json;

    use super::*;
    use crate::EmojiStore;

    fn thread(id: &str, last_reply_at: Millis, unread_replies: i64, mentions: i64) -> UserThread {
        serde_json::from_value(json!({
            "id": id,
            "reply_count": 2,
            "last_reply_at": last_reply_at,
            "unread_replies": unread_replies,
            "unread_mentions": mentions,
            "participants": [{ "id": "ann" }],
            "post": { "id": id, "channel_id": "c", "user_id": "bob", "message": format!("about {id}") },
        }))
        .unwrap()
    }

    fn page(threads: Vec<UserThread>, unread_threads: i64, mentions: i64) -> UserThreads {
        let mut page = UserThreads::default();
        page.total_unread_threads = unread_threads;
        page.total_unread_mentions = mentions;
        page.threads = threads;
        page
    }

    /// A model with no team, so nothing reaches the network.
    fn threads(cx: &mut TestAppContext) -> Entity<FollowedThreads> {
        let client = Client::new_unverified("http://localhost:1", "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let emoji = cx.new(|_| EmojiStore::new(client.clone()));
        let directory = cx.new(|_| Directory::new(client.clone(), me, emoji));
        cx.new(|_| FollowedThreads::new(client, "me".into(), directory))
    }

    fn ids(
        threads: &Entity<FollowedThreads>,
        filter: ThreadFilter,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        threads.read_with(cx, |t, _| {
            t.threads(filter).map(|t| t.id().to_string()).collect()
        })
    }

    #[gpui_kit::test]
    fn a_new_reply_moves_its_thread_to_the_top(cx: &mut TestAppContext) {
        let threads = threads(cx);
        threads.update(cx, |t, cx| {
            let first = page(vec![thread("b", 20, 0, 0), thread("a", 10, 0, 0)], 0, 0);
            t.adopt_page(ThreadFilter::All, first, true, cx);
            t.adopt_page(ThreadFilter::Unread, page(Vec::new(), 0, 0), true, cx);
            t.apply_update(thread("a", 30, 1, 1), cx);
        });
        assert_eq!(ids(&threads, ThreadFilter::All, cx), ["a", "b"]);
        assert_eq!(ids(&threads, ThreadFilter::Unread, cx), ["a"]);
        threads.read_with(cx, |t, _| {
            assert_eq!((t.unread_threads(), t.unread_mentions()), (1, 1));
        });
    }

    #[gpui_kit::test]
    fn a_thread_older_than_the_loaded_pages_waits_for_its_page(cx: &mut TestAppContext) {
        let threads = threads(cx);
        threads.update(cx, |t, cx| {
            let full: Vec<_> = (0..PAGE_SIZE as i64)
                .map(|ix| thread(&format!("t{ix}"), 1000 - ix, 0, 0))
                .collect();
            t.adopt_page(ThreadFilter::All, page(full, 0, 0), true, cx);
            assert!(t.has_more(ThreadFilter::All));
            t.apply_update(thread("old", 1, 0, 0), cx);
        });
        let listed = ids(&threads, ThreadFilter::All, cx);
        assert_eq!(listed.len(), PAGE_SIZE);
        assert!(!listed.contains(&"old".to_string()));
    }

    #[gpui_kit::test]
    fn reading_a_thread_updates_the_totals_once(cx: &mut TestAppContext) {
        let threads = threads(cx);
        threads.update(cx, |t, cx| {
            let first = page(vec![thread("a", 20, 2, 1), thread("b", 10, 1, 0)], 2, 1);
            t.adopt_page(ThreadFilter::Unread, first, true, cx);
            // Read here, then the server's echo of the same read.
            t.note_read(&"a".into(), 0, 0, cx);
            t.note_read(&"a".into(), 0, 0, cx);
        });
        threads.read_with(cx, |t, _| {
            assert_eq!((t.unread_threads(), t.unread_mentions()), (1, 0));
            // Still listed, so it doesn't vanish while being read.
            assert_eq!(t.threads(ThreadFilter::Unread).count(), 2);
            assert!(!t.thread(&"a".into()).unwrap().is_unread());
        });
        threads.update(cx, |t, cx| t.note_all_read(None, cx));
        threads.read_with(cx, |t, _| {
            assert_eq!((t.unread_threads(), t.unread_mentions()), (0, 0));
            assert!(t.threads(ThreadFilter::Unread).all(|t| !t.is_unread()));
        });
    }

    #[gpui_kit::test]
    fn unfollowing_removes_a_thread_and_its_unread_count(cx: &mut TestAppContext) {
        let threads = threads(cx);
        threads.update(cx, |t, cx| {
            let first = page(vec![thread("a", 20, 1, 1), thread("b", 10, 0, 0)], 1, 1);
            t.adopt_page(ThreadFilter::All, first, true, cx);
            t.note_following(&"a".into(), false, cx);
        });
        assert_eq!(ids(&threads, ThreadFilter::All, cx), ["b"]);
        threads.read_with(cx, |t, _| {
            assert_eq!((t.unread_threads(), t.unread_mentions()), (0, 0));
        });
    }

    #[gpui_kit::test]
    fn leaving_a_channel_drops_its_threads_and_their_unread_counts(cx: &mut TestAppContext) {
        let threads = threads(cx);
        let mut elsewhere = thread("b", 10, 1, 0);
        elsewhere.post.channel_id = "left".into();
        threads.update(cx, |t, cx| {
            let first = page(vec![thread("a", 20, 1, 1), elsewhere], 2, 1);
            t.adopt_page(ThreadFilter::All, first, true, cx);
            t.keep_channels(|channel| channel.as_str() != "left", cx);
        });
        assert_eq!(ids(&threads, ThreadFilter::All, cx), ["a"]);
        threads.read_with(cx, |t, _| {
            assert!(t.thread(&"b".into()).is_none());
            assert_eq!((t.unread_threads(), t.unread_mentions()), (1, 1));
        });
    }

    #[test]
    fn previews_read_as_plain_text() {
        let post = |message: &str| {
            let mut post = Post::default();
            post.message = message.into();
            post
        };
        assert_eq!(
            preview(&post(
                "## Release\n> **Heads up** :tada:\n- see [the notes](https://x.y)"
            )),
            "Release Heads up 🎉 see the notes"
        );
        assert_eq!(
            preview(&post("```rust\nfn main() {}\n```\nrun `cargo test`")),
            "fn main() {} run cargo test"
        );
        assert_eq!(preview(&post("#general is quiet")), "#general is quiet");
        let mut files = post("");
        let mut file = mattermost::FileInfo::default();
        file.name = "report.pdf".into();
        files.metadata.files.push(file);
        assert_eq!(preview(&files), "report.pdf");
    }
}
