use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{Context, Task};
use mattermost::{ApiError, Client, Emoji};

use crate::{
    prepare::{emoji_for, emoji_name, is_emoji_name},
    retry::retry,
};

/// The server's custom emoji, by name, with the URL of each image.
///
/// A cheap-to-clone snapshot: the [`EmojiStore`] replaces it as emoji
/// load or arrive, and the [`crate::Session`] hands the new one to every
/// live timeline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomEmojis {
    urls: Arc<HashMap<String, Arc<str>>>,
}

impl CustomEmojis {
    /// A set of `(name, image URL)` pairs.
    pub fn new(emoji: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            urls: Arc::new(
                emoji
                    .into_iter()
                    .map(|(name, url)| (name, url.into()))
                    .collect(),
            ),
        }
    }

    /// The image of the custom emoji `name`. Accepts the `:name:` form some
    /// clients store in custom statuses.
    pub fn image_url(&self, name: &str) -> Option<&Arc<str>> {
        self.urls.get(emoji_name(name))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.urls.contains_key(name)
    }

    pub fn insert(&mut self, name: String, url: String) {
        Arc::make_mut(&mut self.urls).insert(name, url.into());
    }

    pub fn remove(&mut self, name: &str) {
        if self.urls.contains_key(name) {
            Arc::make_mut(&mut self.urls).remove(name);
        }
    }

    /// Every `(name, image URL)` pair, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.urls.iter().map(|(name, url)| (name.as_str(), &**url))
    }

    pub fn len(&self) -> usize {
        self.urls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }
}

/// Custom emoji requested per page of the full list; the server's maximum.
const PAGE_SIZE: usize = 200;
/// Names looked up per request; the server's maximum.
const NAMES_PER_REQUEST: usize = 200;
const BATCH_WINDOW: Duration = Duration::from_millis(16);

/// The server's custom emoji, loaded two ways at once.
///
/// What is on screen comes first: statuses, messages, and reactions ask
/// for the names they use with [`EmojiStore::ensure`], and names asked for
/// within one short window cost a single request. Meanwhile the full list
/// loads page by page; once it is complete, names it lacks are known not to
/// exist and cost nothing. Emoji remembered from an earlier run
/// ([`EmojiStore::remember`]) show before either answers.
///
/// Notifies whenever [`EmojiStore::custom`] changes.
pub struct EmojiStore {
    client: Client,
    custom: CustomEmojis,
    /// Names remembered from an earlier run that the server hasn't
    /// confirmed yet.
    remembered: HashSet<String>,
    /// Names to look up in the next batch.
    queued: HashSet<String>,
    /// Names looked up already, found or not, so each costs one request.
    requested: HashSet<String>,
    /// Whether the full list has loaded, so every custom emoji is known.
    complete: bool,
    /// The server can't look up names in a batch; look up one at a time.
    one_by_one: bool,
    flush_task: Option<Task<()>>,
}

impl EmojiStore {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            custom: CustomEmojis::default(),
            remembered: HashSet::new(),
            queued: HashSet::new(),
            requested: HashSet::new(),
            complete: false,
            one_by_one: false,
            flush_task: None,
        }
    }

    pub fn custom(&self) -> &CustomEmojis {
        &self.custom
    }

    /// Look up any custom emoji among `names` not already known or asked
    /// for. Standard emoji names and malformed names are skipped.
    /// Remembered emoji count as unknown, so each is confirmed the first
    /// time it is needed.
    pub fn ensure<'a>(&mut self, names: impl IntoIterator<Item = &'a str>, cx: &mut Context<Self>) {
        if self.complete {
            return;
        }
        let mut added = false;
        for name in names {
            let name = emoji_name(name);
            if !is_emoji_name(name)
                || emoji_for(name).is_some()
                || (self.custom.contains(name) && !self.remembered.contains(name))
                || self.requested.contains(name)
                || self.queued.contains(name)
            {
                continue;
            }
            self.queued.insert(name.to_string());
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
        let names: Vec<String> = self.queued.drain().collect();
        if names.is_empty() || self.complete {
            return;
        }
        self.requested.extend(names.iter().cloned());
        let client = self.client.clone();
        let one_by_one = self.one_by_one;
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut found = Vec::new();
            // Names whose lookup failed, rather than found missing, so a
            // later `ensure` can try them again.
            let mut failed = Vec::new();
            let mut batch_missing = false;
            if !one_by_one {
                for chunk in names.chunks(NAMES_PER_REQUEST) {
                    let chunk = chunk.to_vec();
                    match retry(&executor, || client.custom_emoji_by_names(chunk.clone())).await {
                        Ok(emoji) => found.extend(emoji),
                        // Servers before the batch lookup don't route it.
                        Err(err) if is_status(&err, &[404, 405]) => {
                            batch_missing = true;
                            break;
                        }
                        Err(err) => {
                            log::warn!("looking up custom emoji: {err:#}");
                            failed.extend(chunk);
                        }
                    }
                }
            }
            if one_by_one || batch_missing {
                let lookups = names.iter().map(|name| {
                    let executor = &executor;
                    let client = &client;
                    async move {
                        let result = retry(executor, || client.custom_emoji_by_name(name)).await;
                        (name, result)
                    }
                });
                for (name, result) in futures::future::join_all(lookups).await {
                    match result {
                        Ok(emoji) => found.push(emoji),
                        Err(err) if is_status(&err, &[404]) => {}
                        Err(err) => {
                            log::warn!("looking up custom emoji: {err:#}");
                            failed.push(name.clone());
                        }
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.one_by_one |= batch_missing;
                for name in &failed {
                    this.requested.remove(name);
                }
                this.add(found, cx);
                let missing = names.into_iter().filter(|name| !failed.contains(name));
                this.forget_missing(missing, cx);
            });
        })
        .detach();
    }

    /// Load every custom emoji, page by page, then add them all at once so
    /// open timelines re-prepare their messages only once.
    pub fn load_all(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut emoji = Vec::new();
            let mut complete = false;
            for page in 0.. {
                match retry(&executor, || client.custom_emoji(page, PAGE_SIZE)).await {
                    Ok(batch) => {
                        let done = batch.len() < PAGE_SIZE;
                        emoji.extend(batch);
                        if done {
                            complete = true;
                            break;
                        }
                    }
                    // The server has custom emoji turned off: there are none.
                    Err(err) if is_status(&err, &[501]) => {
                        complete = true;
                        break;
                    }
                    // Names on screen are still looked up as they appear.
                    Err(err) => {
                        log::warn!("loading custom emoji: {err:#}");
                        break;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.add(emoji, cx);
                this.complete = complete;
                if complete {
                    this.queued.clear();
                    let missing: Vec<String> = this.remembered.iter().cloned().collect();
                    this.forget_missing(missing, cx);
                }
            });
        })
        .detach();
    }

    /// Add emoji found by a lookup or announced by the server. An emoji
    /// recreated under a known name replaces the old one's image.
    pub fn add(&mut self, emoji: Vec<Emoji>, cx: &mut Context<Self>) {
        let client = &self.client;
        let found: Vec<_> = emoji
            .into_iter()
            .filter(|e| e.delete_at == 0 && !e.name.is_empty())
            .map(|e| (e.name.clone(), client.custom_emoji_url(&e.id)))
            .collect();
        for (name, _) in &found {
            self.remembered.remove(name);
        }
        let new: Vec<_> = found
            .into_iter()
            .filter(|(name, url)| self.custom.image_url(name).map(|u| &**u) != Some(url.as_str()))
            .collect();
        if new.is_empty() {
            return;
        }
        for (name, url) in new {
            self.custom.insert(name, url);
        }
        cx.notify();
    }

    /// Show `emoji`, `(name, image URL)` pairs saved by an earlier run,
    /// until the server confirms them, so messages don't wait on a round
    /// trip for their images. Names already known keep their newer image;
    /// once the full list loads, remembered names it lacks are dropped.
    pub fn remember(
        &mut self,
        emoji: impl IntoIterator<Item = (String, String)>,
        cx: &mut Context<Self>,
    ) {
        // A complete list is newer than anything remembered.
        if self.complete {
            return;
        }
        let mut added = false;
        for (name, url) in emoji {
            // A name already asked about is answered by that lookup.
            if is_emoji_name(&name)
                && !self.custom.contains(&name)
                && !self.requested.contains(&name)
            {
                self.remembered.insert(name.clone());
                self.custom.insert(name, url);
                added = true;
            }
        }
        if added {
            cx.notify();
        }
    }

    /// Drop the remembered emoji among `names`, which the server was asked
    /// about and didn't have: deleted since the earlier run. Call after
    /// [`Self::add`], which confirms the ones it did have.
    fn forget_missing(&mut self, names: impl IntoIterator<Item = String>, cx: &mut Context<Self>) {
        let mut changed = false;
        for name in names {
            if self.remembered.remove(&name) {
                self.custom.remove(&name);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }
}

fn is_status(err: &anyhow::Error, codes: &[u16]) -> bool {
    matches!(err.downcast_ref::<ApiError>(), Some(ApiError::Status(code)) if codes.contains(code))
}
