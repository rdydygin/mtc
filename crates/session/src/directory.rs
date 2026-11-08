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
/// posts by forty authors costs one request, not forty.
pub struct Directory {
    client: Client,
    /// Where the custom emoji in users' statuses are looked up.
    emoji: Entity<EmojiStore>,
    users: HashMap<UserId, Arc<User>>,
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

    /// Fetch any of `ids` not already known or requested.
    pub fn ensure(&mut self, ids: impl IntoIterator<Item = UserId>, cx: &mut Context<Self>) {
        let mut added = false;
        for id in ids {
            if id.is_empty()
                || self.users.contains_key(&id)
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
                        for user in users {
                            this.users.insert(user.id.clone(), Arc::new(user));
                        }
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
            cx.notify();
        }
    }

    /// Insert users learned elsewhere, such as from a search.
    pub fn insert(&mut self, users: impl IntoIterator<Item = User>, cx: &mut Context<Self>) {
        let users: Vec<User> = users.into_iter().collect();
        self.request_status_emoji(&users, cx);
        for user in users {
            self.users.insert(user.id.clone(), Arc::new(user));
        }
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
