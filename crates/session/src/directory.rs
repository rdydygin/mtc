use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{Context, Entity, SharedString, Task};
use mattermost::{Client, Status, User, UserId};

use crate::EmojiStore;

/// Users and their presence, fetched on demand.
///
/// Callers ask for users with [`Directory::ensure`]; requests made within one
/// short window are coalesced into a single `users/ids` call, so a page of
/// posts by forty authors costs one request, not forty. Users remembered
/// from an earlier run ([`Directory::remember`]) show at once and are asked
/// for again the same way.
pub struct Directory {
    client: Client,
    /// Where the custom emoji in users' statuses are looked up.
    emoji: Entity<EmojiStore>,
    users: HashMap<UserId, Arc<User>>,
    /// Users remembered from an earlier run that the server hasn't
    /// confirmed yet.
    remembered: HashSet<UserId>,
    /// Counts changes to `users` (not presence), so a copy kept elsewhere
    /// can tell when it is out of date.
    revision: u64,
    statuses: HashMap<UserId, Status>,
    queued: HashSet<UserId>,
    in_flight: HashSet<UserId>,
    flush_task: Option<Task<()>>,
}

const BATCH_WINDOW: Duration = Duration::from_millis(16);

impl Directory {
    /// `emoji` should already be asked for `me`'s status emoji.
    pub fn new(client: Client, me: User, emoji: Entity<EmojiStore>) -> Self {
        let mut users = HashMap::new();
        users.insert(me.id.clone(), Arc::new(me));
        Self {
            client,
            emoji,
            users,
            remembered: HashSet::new(),
            revision: 0,
            statuses: HashMap::new(),
            queued: HashSet::new(),
            in_flight: HashSet::new(),
            flush_task: None,
        }
    }

    pub fn user(&self, id: &UserId) -> Option<&Arc<User>> {
        self.users.get(id)
    }

    pub fn users(&self) -> impl Iterator<Item = &Arc<User>> {
        self.users.values()
    }

    /// Changes whenever a user is added or changed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn status(&self, id: &UserId) -> Status {
        self.statuses.get(id).copied().unwrap_or_default()
    }

    /// The best name for `id`, falling back to a placeholder while loading.
    pub fn display_name(&self, id: &UserId) -> SharedString {
        match self.users.get(id) {
            Some(user) => user.display_name().into(),
            None => "Someone".into(),
        }
    }

    /// Fetch any of `ids` not already known or requested. Remembered users
    /// count as unknown, so each is confirmed the first time it is needed.
    pub fn ensure(&mut self, ids: impl IntoIterator<Item = UserId>, cx: &mut Context<Self>) {
        let mut added = false;
        for id in ids {
            if id.is_empty()
                || (self.users.contains_key(&id) && !self.remembered.contains(&id))
                || self.in_flight.contains(&id)
                || self.queued.contains(&id)
            {
                continue;
            }
            self.queued.insert(id);
            added = true;
        }
        if added && self.flush_task.is_none() {
            self.flush_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(BATCH_WINDOW).await;
                let _ = this.update(cx, |this, cx| this.flush(cx));
            }));
        }
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.flush_task = None;
        let ids: Vec<UserId> = self.queued.drain().collect();
        if ids.is_empty() {
            return;
        }
        self.in_flight.extend(ids.iter().cloned());
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            // Independent requests: one round trip, not two.
            let (users, statuses) = futures::join!(
                client.users_by_ids(ids.clone()),
                client.statuses_by_ids(ids.clone())
            );
            let _ = this.update(cx, |this, cx| {
                for id in &ids {
                    this.in_flight.remove(id);
                }
                match users {
                    Ok(users) => {
                        this.request_status_emoji(&users, cx);
                        for id in &ids {
                            this.remembered.remove(id);
                        }
                        for user in users {
                            this.users.insert(user.id.clone(), Arc::new(user));
                        }
                        this.revision += 1;
                    }
                    Err(err) => log::warn!("loading users: {err:#}"),
                }
                if let Ok(statuses) = statuses {
                    this.statuses.extend(statuses);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn set_status(&mut self, id: UserId, status: Status, cx: &mut Context<Self>) {
        if self.statuses.insert(id, status) != Some(status) {
            cx.notify();
        }
    }

    /// Re-read presence for `ids`, such as the user's own at startup.
    pub fn refresh_statuses(&mut self, ids: Vec<UserId>, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(statuses) = client.statuses_by_ids(ids).await {
                let _ = this.update(cx, |this, cx| {
                    this.statuses.extend(statuses);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Change a known user in place, for optimistic updates.
    pub fn update_user(
        &mut self,
        id: &UserId,
        change: impl FnOnce(&mut User),
        cx: &mut Context<Self>,
    ) {
        if let Some(user) = self.users.get_mut(id) {
            change(Arc::make_mut(user));
            let user = user.clone();
            self.request_status_emoji([user.as_ref()], cx);
            self.revision += 1;
            cx.notify();
        }
    }

    /// Insert users learned elsewhere, such as from a search.
    pub fn insert(&mut self, users: impl IntoIterator<Item = User>, cx: &mut Context<Self>) {
        let users: Vec<User> = users.into_iter().collect();
        self.request_status_emoji(&users, cx);
        for user in users {
            self.remembered.remove(&user.id);
            self.users.insert(user.id.clone(), Arc::new(user));
        }
        self.revision += 1;
        cx.notify();
    }

    /// Show `users`, saved by an earlier run, until the server confirms
    /// them, so names and pictures don't wait on a round trip. Users
    /// already loaded keep their newer copy.
    pub fn remember(&mut self, users: impl IntoIterator<Item = User>, cx: &mut Context<Self>) {
        let users: Vec<User> = users
            .into_iter()
            .filter(|user| !user.id.is_empty() && !self.users.contains_key(&user.id))
            .collect();
        if users.is_empty() {
            return;
        }
        self.request_status_emoji(&users, cx);
        for user in users {
            self.remembered.insert(user.id.clone());
            self.users.insert(user.id.clone(), Arc::new(user));
        }
        self.revision += 1;
        cx.notify();
    }

    /// Look up the custom emoji in `users`' statuses, so they draw as
    /// images rather than `:name:`.
    fn request_status_emoji<'a>(
        &self,
        users: impl IntoIterator<Item = &'a User>,
        cx: &mut Context<Self>,
    ) {
        // Expired statuses are looked up too; a spare lookup is harmless.
        let names: Vec<String> = users
            .into_iter()
            .filter_map(|user| user.custom_status(""))
            .map(|status| status.emoji)
            .collect();
        if !names.is_empty() {
            self.emoji.update(cx, |emoji, cx| {
                emoji.ensure(names.iter().map(String::as_str), cx)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use mattermost::{
        Client, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::Directory;
    use crate::EmojiStore;

    fn route(request: &Request) -> Reply {
        match request.path().split('?').next().unwrap_or_default() {
            "/api/v4/users/ids" => Reply::json(json!([
                { "id": "ann", "username": "ann", "first_name": "Ann", "last_picture_update": 2 },
            ])),
            "/api/v4/users/status/ids" => Reply::json(json!([])),
            _ => Reply::not_found(),
        }
    }

    fn user(id: &str, first_name: &str) -> User {
        let mut user = User::default();
        user.id = id.into();
        user.username = id.into();
        user.first_name = first_name.into();
        user
    }

    fn start(server: &SlowServer, cx: &mut TestAppContext) -> Entity<Directory> {
        cx.executor().allow_parking();
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let emoji = cx.new(|_| EmojiStore::new(client.clone()));
        cx.new(|_| Directory::new(client, user("me", "Me"), emoji))
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

    fn requests(server: &SlowServer) -> usize {
        server
            .requests()
            .iter()
            .filter(|r| r.path() == "/api/v4/users/ids")
            .count()
    }

    #[gpui_kit::test]
    fn remembered_users_show_until_the_server_confirms_them(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(50)), route);
        let directory = start(&server, cx);
        directory.update(cx, |d, cx| d.remember([user("ann", "Annie")], cx));
        assert_eq!(
            directory.read_with(cx, |d, _| d.display_name(&"ann".into())),
            "Annie"
        );

        directory.update(cx, |d, cx| d.ensure(["ann".into()], cx));
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| d.display_name(&"ann".into()) == "Ann")
        });

        // Confirmed once; asking again costs nothing.
        directory.update(cx, |d, cx| d.ensure(["ann".into()], cx));
        cx.run_until_parked();
        assert_eq!(requests(&server), 1);
    }

    #[gpui_kit::test]
    fn a_remembered_copy_never_replaces_a_loaded_user(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::from_millis(50)), route);
        let directory = start(&server, cx);
        directory.update(cx, |d, cx| d.ensure(["ann".into()], cx));
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| d.user(&"ann".into()).is_some())
        });
        let revision = directory.read_with(cx, |d, _| d.revision());

        directory.update(cx, |d, cx| {
            d.remember([user("ann", "Annie"), user("me", "Old me")], cx)
        });

        directory.read_with(cx, |d, _| {
            assert_eq!(d.display_name(&"ann".into()), "Ann");
            assert_eq!(d.display_name(&"me".into()), "Me");
            assert_eq!(d.revision(), revision, "nothing changed");
        });
    }
}
