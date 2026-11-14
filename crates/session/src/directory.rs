use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{Context, Entity, SharedString, Task};
use mattermost::{Client, Status, User, UserId};

use crate::EmojiStore;

/// Where looking up a username stands, short of having found the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsernameLookup {
    /// Asked for; the answer hasn't arrived.
    Pending,
    /// No one has the username.
    Missing,
    /// The lookup failed. Asking again tries again.
    Failed,
}

/// Users and their presence, fetched on demand.
///
/// Callers ask for users with [`Directory::ensure`]; requests made within one
/// short window are coalesced into a single `users/ids` call, so a page of
/// posts by forty authors costs one request, not forty. Users remembered
/// from an earlier run ([`Directory::remember`]) show at once and are asked
/// for again the same way. People mentioned by name are looked up one at a
/// time, as they're clicked ([`Directory::ensure_username`]).
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
    /// Only active status reads have tokens; status events invalidate them.
    status_requests: HashMap<UserId, u64>,
    next_status_request: u64,
    /// Usernames asked about that aren't known users.
    usernames: HashMap<String, UsernameLookup>,
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
            status_requests: HashMap::new(),
            next_status_request: 0,
            usernames: HashMap::new(),
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

    /// The known user with `username`. Usernames are lowercase.
    pub fn user_by_username(&self, username: &str) -> Option<&Arc<User>> {
        self.users.values().find(|user| user.username == username)
    }

    /// Where looking up `username` stands, if it was asked for and hasn't
    /// found the user.
    pub fn username_lookup(&self, username: &str) -> Option<UsernameLookup> {
        self.usernames.get(username).copied()
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

    /// Look up the user with `username`, such as someone a message mentions,
    /// unless known or already asked about. A user remembered from an earlier
    /// run is confirmed, as [`Self::ensure`] does.
    pub fn ensure_username(&mut self, username: &str, cx: &mut Context<Self>) {
        if let Some(user) = self.user_by_username(username) {
            let id = user.id.clone();
            self.ensure([id], cx);
            return;
        }
        if matches!(
            self.usernames.get(username),
            Some(UsernameLookup::Pending | UsernameLookup::Missing)
        ) {
            return;
        }
        let username = username.to_string();
        self.usernames
            .insert(username.clone(), UsernameLookup::Pending);
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let users = client.users_by_usernames(vec![username.clone()]).await;
            let (statuses, tokens) = match &users {
                Ok(users) if !users.is_empty() => {
                    let ids = users.iter().map(|user| user.id.clone()).collect::<Vec<_>>();
                    let Ok(tokens) = this.update(cx, |this, _| this.begin_status_request(&ids))
                    else {
                        return;
                    };
                    (
                        client.statuses_by_ids(ids).await.unwrap_or_default(),
                        tokens,
                    )
                }
                _ => (Vec::new(), HashMap::new()),
            };
            let _ = this.update(cx, |this, cx| match users {
                Ok(users) if users.is_empty() => {
                    this.usernames.insert(username, UsernameLookup::Missing);
                    cx.notify();
                }
                Ok(users) => {
                    this.usernames.remove(&username);
                    this.apply_statuses(statuses, &tokens);
                    this.insert(users, cx);
                }
                Err(err) => {
                    log::warn!("looking up @{username}: {err:#}");
                    this.usernames.insert(username, UsernameLookup::Failed);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.flush_task = None;
        let ids: Vec<UserId> = self.queued.drain().collect();
        if ids.is_empty() {
            return;
        }
        self.in_flight.extend(ids.iter().cloned());
        let tokens = self.begin_status_request(&ids);
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
                this.apply_statuses(statuses.unwrap_or_default(), &tokens);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn set_status(&mut self, id: UserId, status: Status, cx: &mut Context<Self>) {
        self.status_requests.remove(&id);
        if self.statuses.insert(id, status) != Some(status) {
            cx.notify();
        }
    }

    /// Re-read presence for `ids`, such as our own at startup or reconnect.
    pub fn refresh_statuses(&mut self, ids: Vec<UserId>, cx: &mut Context<Self>) {
        let tokens = self.begin_status_request(&ids);
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let statuses = client.statuses_by_ids(ids).await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.apply_statuses(statuses, &tokens);
                cx.notify();
            });
        })
        .detach();
    }

    fn begin_status_request(&mut self, ids: &[UserId]) -> HashMap<UserId, u64> {
        self.next_status_request = self
            .next_status_request
            .checked_add(1)
            .expect("status request counter overflow");
        ids.iter()
            .map(|id| {
                self.status_requests
                    .insert(id.clone(), self.next_status_request);
                (id.clone(), self.next_status_request)
            })
            .collect()
    }

    fn apply_statuses(&mut self, statuses: Vec<(UserId, Status)>, tokens: &HashMap<UserId, u64>) {
        for (id, status) in statuses {
            if tokens
                .get(&id)
                .is_some_and(|token| self.status_requests.get(&id) == Some(token))
            {
                self.statuses.insert(id, status);
            }
        }
        // Omitted users and failed requests must release their tokens too.
        for (id, token) in tokens {
            if self.status_requests.get(id) == Some(token) {
                self.status_requests.remove(id);
            }
        }
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
        Client, Status, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::{Directory, UsernameLookup};
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
    fn status_tokens_are_removed_without_reusing_them(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let directory = start(&server, cx);
        directory.update(cx, |d, cx| {
            let ann = mattermost::UserId::from("ann");
            let old = d.begin_status_request(&[ann.clone(), "omitted".into()]);
            d.set_status(ann.clone(), Status::DoNotDisturb, cx);
            assert!(!d.status_requests.contains_key(&ann));
            let current = d.begin_status_request(std::slice::from_ref(&ann));
            assert_ne!(old[&ann], current[&ann]);
            d.apply_statuses(vec![(ann.clone(), Status::Online)], &old);
            assert_eq!(d.status(&ann), Status::DoNotDisturb);
            assert_eq!(d.status_requests.get(&ann), current.get(&ann));
            assert!(!d.status_requests.contains_key(&"omitted".into()));
            d.apply_statuses(vec![(ann.clone(), Status::Away)], &current);
            assert!(d.status_requests.is_empty());
            let later = d.begin_status_request(std::slice::from_ref(&ann));
            assert_ne!(later[&ann], current[&ann]);
            d.apply_statuses(vec![(ann.clone(), Status::Online)], &current);
            assert_eq!(d.status(&ann), Status::Away);
            assert_eq!(d.status_requests.get(&ann), later.get(&ann));
            d.apply_statuses(Vec::new(), &later);
            d.set_status("unrequested".into(), Status::Online, cx);
            assert!(d.status_requests.is_empty());
        });
    }

    /// Omitted users and failed status reads release tokens on every path.
    fn check_status_cleanup(
        cx: &mut TestAppContext,
        lookup: fn(&mut Directory, &mut gpui_kit::Context<Directory>),
    ) {
        for status in [200, 503] {
            let server =
                SlowServer::start(Network::new(Duration::from_millis(50)), move |request| {
                    match request.path() {
                        "/api/v4/users/ids" | "/api/v4/users/usernames" => Reply::json(json!([
                            { "id": "ann", "username": "ann" }
                        ])),
                        "/api/v4/users/status/ids" => Reply::json(json!([])).status(status),
                        _ => Reply::not_found(),
                    }
                });
            let directory = start(&server, cx);
            directory.update(cx, lookup);
            wait_until(cx, |_| {
                server
                    .requests()
                    .iter()
                    .any(|r| r.path() == "/api/v4/users/status/ids")
            });
            wait_until(cx, |cx| {
                directory.read_with(cx, |d, _| d.status_requests.is_empty())
            });
        }
    }

    #[gpui_kit::test]
    fn refresh_releases_status_tokens_on_omissions_and_errors(cx: &mut TestAppContext) {
        check_status_cleanup(cx, |d, cx| d.refresh_statuses(vec!["me".into()], cx));
    }

    #[gpui_kit::test]
    fn batched_lookup_releases_status_tokens_on_omissions_and_errors(cx: &mut TestAppContext) {
        check_status_cleanup(cx, |d, cx| d.ensure(["ann".into()], cx));
    }

    #[gpui_kit::test]
    fn username_lookup_releases_status_tokens_on_omissions_and_errors(cx: &mut TestAppContext) {
        check_status_cleanup(cx, |d, cx| d.ensure_username("ann", cx));
    }

    #[gpui_kit::test]
    fn a_status_change_wins_over_an_older_refresh(cx: &mut TestAppContext) {
        for (old, new) in [
            (Status::Online, Status::DoNotDisturb),
            (Status::DoNotDisturb, Status::Online),
        ] {
            let server = SlowServer::start(Network::new(Duration::from_millis(50)), move |_| {
                Reply::json(json!([
                    { "user_id": "me", "status": old.as_str() },
                    { "user_id": "ann", "status": "away" }
                ]))
            });
            let directory = start(&server, cx);
            directory.update(cx, |d, cx| {
                d.set_status("me".into(), new, cx);
                d.refresh_statuses(vec!["me".into(), "ann".into()], cx);
            });
            wait_until(cx, |_| !server.requests().is_empty());
            // Even a repeated status event supersedes the pending REST read.
            directory.update(cx, |d, cx| d.set_status("me".into(), new, cx));
            wait_until(cx, |cx| {
                directory.read_with(cx, |d, _| d.status(&"ann".into()) == Status::Away)
            });
            directory.read_with(cx, |d, _| assert_eq!(d.status(&"me".into()), new));
        }
    }

    #[gpui_kit::test]
    fn user_lookups_keep_newer_status_changes(cx: &mut TestAppContext) {
        for by_username in [false, true] {
            let server =
                SlowServer::start(
                    Network::new(Duration::from_millis(50)),
                    |request| match request.path() {
                        "/api/v4/users/ids" | "/api/v4/users/usernames" => Reply::json(json!([
                            { "id": "ann", "username": "ann" }
                        ])),
                        "/api/v4/users/status/ids" => Reply::json(json!([
                            { "user_id": "ann", "status": "online" }
                        ])),
                        _ => Reply::not_found(),
                    },
                );
            let directory = start(&server, cx);
            directory.update(cx, |d, cx| {
                if by_username {
                    d.ensure_username("ann", cx);
                } else {
                    d.ensure(["ann".into()], cx);
                }
            });
            wait_until(cx, |_| {
                server
                    .requests()
                    .iter()
                    .any(|r| r.path() == "/api/v4/users/status/ids")
            });
            directory.update(cx, |d, cx| {
                d.set_status("ann".into(), Status::DoNotDisturb, cx)
            });
            wait_until(cx, |cx| {
                directory.read_with(cx, |d, _| d.user(&"ann".into()).is_some())
            });
            directory.read_with(cx, |d, _| {
                assert_eq!(d.status(&"ann".into()), Status::DoNotDisturb)
            });
        }
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

    /// A server whose username lookups all answer `users`.
    fn usernames_server(users: serde_json::Value) -> SlowServer {
        SlowServer::start(
            Network::new(Duration::from_millis(50)),
            move |request| match request.path() {
                "/api/v4/users/usernames" => Reply::json(users.clone()),
                "/api/v4/users/status/ids" => {
                    Reply::json(json!([{ "user_id": "ann", "status": "online" }]))
                }
                _ => Reply::not_found(),
            },
        )
    }

    fn username_lookups(server: &SlowServer) -> usize {
        server
            .requests()
            .iter()
            .filter(|r| r.path() == "/api/v4/users/usernames")
            .count()
    }

    #[gpui_kit::test]
    fn a_username_finds_the_user_and_their_presence(cx: &mut TestAppContext) {
        let server = usernames_server(json!([{ "id": "ann", "username": "ann" }]));
        let directory = start(&server, cx);
        directory.update(cx, |d, cx| {
            d.ensure_username("ann", cx);
            assert_eq!(d.username_lookup("ann"), Some(UsernameLookup::Pending));
        });
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| d.user_by_username("ann").is_some())
        });
        directory.read_with(cx, |d, _| {
            assert_eq!(d.username_lookup("ann"), None);
            assert_eq!(d.status(&"ann".into()), Status::Online);
        });

        directory.update(cx, |d, cx| d.ensure_username("ann", cx));
        cx.run_until_parked();
        assert_eq!(username_lookups(&server), 1, "a known user costs nothing");
    }

    #[gpui_kit::test]
    fn a_username_no_one_has_is_asked_about_once(cx: &mut TestAppContext) {
        let server = usernames_server(json!([]));
        let directory = start(&server, cx);
        directory.update(cx, |d, cx| d.ensure_username("nobody", cx));
        wait_until(cx, |cx| {
            directory.read_with(cx, |d, _| {
                d.username_lookup("nobody") == Some(UsernameLookup::Missing)
            })
        });

        directory.update(cx, |d, cx| d.ensure_username("nobody", cx));
        cx.run_until_parked();
        assert_eq!(username_lookups(&server), 1);
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
