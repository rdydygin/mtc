//! The subset of the Mattermost v4 data model the client reads.
//!
//! Every struct tolerates missing fields (`#[serde(default)]`) because servers
//! omit empty values and older servers lack newer fields. Unknown fields are
//! ignored, so a server upgrade never breaks deserialization.

use std::{collections::HashMap, fmt, sync::Arc};

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Arc<str>);

        impl $name {
            pub fn new(id: impl Into<Arc<str>>) -> Self {
                Self(id.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The shared string, for cheap use as a UI identity.
            pub fn as_arc(&self) -> &Arc<str> {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.into())
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id.into())
            }
        }
    };
}

id_type!(
    /// A user's stable identity.
    UserId
);
id_type!(
    /// A team's stable identity.
    TeamId
);
id_type!(
    /// A channel's stable identity, including direct and group messages.
    ChannelId
);
id_type!(
    /// A post's stable identity.
    PostId
);
id_type!(
    /// A custom emoji's stable identity.
    EmojiId
);

/// Milliseconds since the Unix epoch, as Mattermost reports every timestamp.
pub type Millis = i64;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub first_name: String,
    pub last_name: String,
    pub nickname: String,
    pub email: String,
    pub position: String,
    pub is_bot: bool,
    pub delete_at: Millis,
    pub last_picture_update: Millis,
    /// Free-form attributes; the custom status lives here as JSON.
    pub props: serde_json::Map<String, serde_json::Value>,
}

/// People in the channel first, followed by matching people elsewhere in the team.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserAutocomplete {
    // Some servers serialize an empty Go slice as null.
    users: Option<Vec<User>>,
    out_of_channel: Option<Vec<User>>,
}

impl UserAutocomplete {
    pub fn users(&self) -> &[User] {
        self.users.as_deref().unwrap_or_default()
    }

    pub fn out_of_channel(&self) -> &[User] {
        self.out_of_channel.as_deref().unwrap_or_default()
    }
}

/// A user-set status such as "🗓 In a meeting", shown beside their name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct CustomStatus {
    /// An emoji *name* (`calendar`), not the character.
    pub emoji: String,
    pub text: String,
    /// `thirty_minutes`, `one_hour`, `four_hours`, `today`, `this_week`,
    /// `date_and_time`, or empty for "don't clear".
    pub duration: String,
    /// RFC 3339; empty when the status doesn't expire.
    pub expires_at: String,
}

impl CustomStatus {
    pub fn new(emoji: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            emoji: emoji.into(),
            text: text.into(),
            ..Self::default()
        }
    }

    /// Clear automatically at `expires_at` (RFC 3339), described by `duration`.
    pub fn expiring(mut self, duration: impl Into<String>, expires_at: impl Into<String>) -> Self {
        self.duration = duration.into();
        self.expires_at = expires_at.into();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.emoji.is_empty() && self.text.is_empty()
    }

    /// Whether the status has passed its expiry. `now` is RFC 3339 in UTC;
    /// timestamps in that form compare correctly as strings.
    pub fn is_expired_at(&self, now_rfc3339: &str) -> bool {
        let expires = self.expires_at.trim();
        !expires.is_empty()
            && !expires.starts_with("0001")
            && normalize_rfc3339(expires).as_str() <= normalize_rfc3339(now_rfc3339).as_str()
    }
}

/// Reduce an RFC 3339 UTC timestamp to `YYYY-MM-DDTHH:MM:SS` for comparison.
fn normalize_rfc3339(s: &str) -> String {
    s.chars().take(19).collect()
}

impl User {
    /// The user's custom status, unless empty or expired at `now_rfc3339`.
    pub fn custom_status(&self, now_rfc3339: &str) -> Option<CustomStatus> {
        let raw = self.props.get("customStatus")?;
        let status: CustomStatus = match raw {
            serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok()?,
            serde_json::Value::Object(_) => serde_json::from_value(raw.clone()).ok()?,
            _ => return None,
        };
        (!status.is_empty() && !status.is_expired_at(now_rfc3339)).then_some(status)
    }

    /// The name people know the user by: the full name, then the nickname,
    /// then the username. The full name comes first because servers that
    /// sync accounts from a directory often fill the nickname with the login.
    pub fn display_name(&self) -> String {
        let full = format!("{} {}", self.first_name.trim(), self.last_name.trim());
        let full = full.trim();
        let nickname = self.nickname.trim();
        if !full.is_empty() {
            full.to_string()
        } else if !nickname.is_empty() && !nickname.eq_ignore_ascii_case(&self.username) {
            nickname.to_string()
        } else {
            self.username.clone()
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub display_name: String,
    pub delete_at: Millis,
}

/// What kind of conversation a channel is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelType {
    #[default]
    #[serde(rename = "O")]
    Open,
    #[serde(rename = "P")]
    Private,
    #[serde(rename = "D")]
    Direct,
    #[serde(rename = "G")]
    Group,
}

impl ChannelType {
    pub fn is_direct_or_group(self) -> bool {
        matches!(self, Self::Direct | Self::Group)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Channel {
    pub id: ChannelId,
    /// Empty for direct and group messages, which belong to no team.
    pub team_id: TeamId,
    #[serde(rename = "type")]
    pub kind: ChannelType,
    pub name: String,
    pub display_name: String,
    pub header: String,
    pub purpose: String,
    pub last_post_at: Millis,
    pub last_root_post_at: Millis,
    pub total_msg_count: i64,
    pub total_msg_count_root: i64,
    pub delete_at: Millis,
}

impl Channel {
    /// For a direct message, the other participant. Direct channel names are
    /// `<user_id>__<user_id>`.
    pub fn direct_partner(&self, me: &UserId) -> Option<UserId> {
        if self.kind != ChannelType::Direct {
            return None;
        }
        let (a, b) = self.name.split_once("__")?;
        Some(if a == me.as_str() { b } else { a }.into())
    }
}

/// The current user's read state in one channel.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ChannelMember {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub last_viewed_at: Millis,
    pub msg_count: i64,
    pub msg_count_root: i64,
    pub mention_count: i64,
    pub mention_count_root: i64,
    pub notify_props: HashMap<String, String>,
}

impl ChannelMember {
    pub fn is_muted(&self) -> bool {
        self.notify_props.get("mark_unread").map(String::as_str) == Some("mention")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Reaction {
    pub user_id: UserId,
    pub post_id: PostId,
    pub emoji_name: String,
    pub create_at: Millis,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct FileInfo {
    pub id: String,
    pub name: String,
    pub extension: String,
    pub size: i64,
    pub mime_type: String,
    pub width: i64,
    pub height: i64,
    pub has_preview_image: bool,
}

impl FileInfo {
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct PostMetadata {
    pub reactions: Vec<Reaction>,
    pub files: Vec<FileInfo>,
    /// Previews the server generated for links in the message.
    pub embeds: Vec<PostEmbed>,
    /// Dimensions of images referenced by the post, keyed by URL.
    pub images: HashMap<String, ImageSize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct PostEmbed {
    /// `opengraph`, `image`, `message_attachment`, `permalink`, …
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ImageSize {
    pub width: i64,
    pub height: i64,
}

/// A website's OpenGraph summary, as embedded by the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LinkPreview {
    pub url: String,
    pub site_name: String,
    pub title: String,
    pub description: String,
    pub image: Option<String>,
}

impl PostEmbed {
    pub fn link_preview(&self) -> Option<LinkPreview> {
        if self.kind != "opengraph" {
            return None;
        }
        let text = |key: &str| {
            self.data
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let image = self
            .data
            .get("images")
            .and_then(serde_json::Value::as_array)
            .and_then(|images| images.first())
            .and_then(|image| {
                ["secure_url", "url"].iter().find_map(|k| {
                    image
                        .get(*k)
                        .and_then(serde_json::Value::as_str)
                        .filter(|u| !u.is_empty())
                        .map(str::to_string)
                })
            });
        let preview = LinkPreview {
            url: self.url.clone(),
            site_name: text("site_name"),
            title: text("title"),
            description: text("description"),
            image,
        };
        (!preview.title.is_empty() || !preview.description.is_empty()).then_some(preview)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Post {
    pub id: PostId,
    pub create_at: Millis,
    pub update_at: Millis,
    pub edit_at: Millis,
    pub delete_at: Millis,
    pub user_id: UserId,
    pub channel_id: ChannelId,
    /// The thread root, empty for a root post.
    pub root_id: PostId,
    pub message: String,
    /// System posts (`system_join_channel`, …) carry a non-empty type.
    #[serde(rename = "type")]
    pub kind: String,
    /// Client-generated identity echoed back by the server; used to reconcile
    /// optimistic sends.
    pub pending_post_id: String,
    pub reply_count: i64,
    pub props: serde_json::Map<String, serde_json::Value>,
    pub metadata: PostMetadata,
}

impl Post {
    pub fn is_system(&self) -> bool {
        self.kind.starts_with("system_")
    }

    pub fn is_reply(&self) -> bool {
        !self.root_id.is_empty()
    }

    /// The name a webhook or bot asked to be shown instead of the account's.
    pub fn override_username(&self) -> Option<&str> {
        let from_webhook = self.props.get("from_webhook").and_then(|v| v.as_str()) == Some("true");
        if !from_webhook {
            return None;
        }
        self.props
            .get("override_username")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// A page of posts as returned by the posts endpoints.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct PostList {
    /// Post identities, newest first.
    pub order: Vec<PostId>,
    pub posts: HashMap<PostId, Post>,
    pub next_post_id: PostId,
    pub prev_post_id: PostId,
    pub has_next: bool,
}

impl PostList {
    /// The posts in chronological order (oldest first).
    ///
    /// Sorted by creation time rather than trusting `order`: the channel
    /// endpoints list newest first, but the thread endpoint does not.
    pub fn into_chronological(mut self) -> Vec<Post> {
        let mut out = Vec::with_capacity(self.order.len());
        for id in self.order.iter().rev() {
            if let Some(post) = self.posts.remove(id) {
                out.push(post);
            }
        }
        // Posts missing from `order` (some endpoints omit it) still count.
        out.extend(self.posts.into_values());
        out.sort_by_key(|p| p.create_at);
        out
    }
}

/// One page of posts found by a search.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct PostSearch {
    /// The matching posts, newest first.
    pub posts: Vec<Post>,
    /// The words that matched in each post, as the server stemmed them.
    pub matches: HashMap<PostId, Vec<String>>,
    /// Whether another page may follow.
    pub has_next: bool,
}

impl PostSearch {
    pub(crate) fn from_page(
        list: PostList,
        matches: Option<HashMap<PostId, Vec<String>>>,
        per_page: usize,
    ) -> Self {
        let fetched = list.order.len().max(list.posts.len());
        let has_next = list.has_next || fetched >= per_page;
        let mut posts = list.into_chronological();
        posts.reverse();
        Self {
            posts,
            matches: matches.unwrap_or_default(),
            has_next,
        }
    }
}

/// Go encodes an empty list as `null`, which `#[serde(default)]` alone
/// doesn't accept.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// A thread the user follows (collapsed reply threads), with their read
/// state in it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct UserThread {
    /// The root post's identity.
    pub id: PostId,
    pub reply_count: i64,
    pub last_reply_at: Millis,
    pub last_viewed_at: Millis,
    /// Everyone who has posted in the thread, earliest first. Only `id` is
    /// filled in unless full profiles were asked for.
    #[serde(deserialize_with = "null_as_default")]
    pub participants: Vec<User>,
    /// The root post.
    #[serde(deserialize_with = "null_as_default")]
    pub post: Post,
    pub unread_replies: i64,
    pub unread_mentions: i64,
}

/// One page of the threads the user follows in a team, and their unread
/// totals across all of them.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct UserThreads {
    pub total: i64,
    pub total_unread_threads: i64,
    pub total_unread_mentions: i64,
    #[serde(deserialize_with = "null_as_default")]
    pub threads: Vec<UserThread>,
}

/// An emoji uploaded to the server. Messages and reactions name it like a
/// standard emoji (`:partyparrot:`); standard emoji are never listed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Emoji {
    pub id: EmojiId,
    pub creator_id: UserId,
    pub name: String,
    pub delete_at: Millis,
}

/// The preference category that marks a channel as a favorite: the
/// preference is named after the channel, with the value `"true"`.
pub const FAVORITE_CHANNEL: &str = "favorite_channel";

/// One of the user's stored settings, synced across their devices.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Preference {
    pub user_id: UserId,
    pub category: String,
    pub name: String,
    pub value: String,
}

impl Preference {
    pub fn new(
        user_id: UserId,
        category: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            user_id,
            category: category.into(),
            name: name.into(),
            value: value.into(),
        }
    }

    /// The channel this preference marks as a favorite, if it is one.
    pub fn favorite_channel(&self) -> Option<ChannelId> {
        (self.category == FAVORITE_CHANNEL && self.value == "true")
            .then(|| self.name.as_str().into())
    }
}

/// Presence as reported by the status endpoints and `status_change` events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    Online,
    Away,
    DoNotDisturb,
    #[default]
    Offline,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::DoNotDisturb => "dnd",
            Self::Offline => "offline",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "online" => Self::Online,
            "away" => Self::Away,
            "dnd" => Self::DoNotDisturb,
            _ => Self::Offline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_status_reads_props_and_expires() {
        let mut user = User::default();
        user.props.insert(
            "customStatus".into(),
            serde_json::Value::String(
                r#"{"emoji":"calendar","text":"In a meeting","duration":"one_hour","expires_at":"2026-11-07T18:00:00Z"}"#.into(),
            ),
        );
        let status = user.custom_status("2026-11-07T17:30:00Z").unwrap();
        assert_eq!(status.emoji, "calendar");
        assert!(user.custom_status("2026-11-07T18:00:01Z").is_none());
        user.props.insert(
            "customStatus".into(),
            serde_json::Value::String(String::new()),
        );
        assert!(user.custom_status("2026-11-07T17:30:00Z").is_none());
    }

    #[test]
    fn display_name_prefers_full_name() {
        let user = |first: &str, last: &str, nickname: &str| User {
            username: "efkovalev".into(),
            first_name: first.into(),
            last_name: last.into(),
            nickname: nickname.into(),
            ..User::default()
        };
        assert_eq!(
            user("Evgeniy", "Kovalev", "efkovalev").display_name(),
            "Evgeniy Kovalev"
        );
        assert_eq!(user("Avito.People", "", "").display_name(), "Avito.People");
        assert_eq!(user("", "", "Zhenya").display_name(), "Zhenya");
        assert_eq!(user("", "", "EFKovalev").display_name(), "efkovalev");
        assert_eq!(user(" ", "", "").display_name(), "efkovalev");
    }

    #[test]
    fn chronological_ignores_endpoint_order() {
        let post = |id: &str, at: Millis| Post {
            id: id.into(),
            create_at: at,
            ..Post::default()
        };
        let mut list = PostList {
            order: vec!["root".into(), "reply".into()],
            ..PostList::default()
        };
        list.posts.insert("root".into(), post("root", 1));
        list.posts.insert("reply".into(), post("reply", 2));
        let ids: Vec<_> = list
            .into_chronological()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec![PostId::from("root"), PostId::from("reply")]);
    }

    #[test]
    fn followed_threads_accept_null_lists() {
        let page: UserThreads = serde_json::from_str(
            r#"{"total":1,"total_unread_threads":1,"total_unread_mentions":2,"threads":[
                {"id":"root","reply_count":3,"last_reply_at":30,"last_viewed_at":10,
                 "participants":null,"unread_replies":2,"unread_mentions":2,
                 "post":{"id":"root","channel_id":"c1","message":"hi"}}]}"#,
        )
        .unwrap();
        let thread = &page.threads[0];
        assert_eq!(thread.id.as_str(), "root");
        assert_eq!(thread.post.channel_id.as_str(), "c1");
        assert!(thread.participants.is_empty());
        let totals: UserThreads =
            serde_json::from_str(r#"{"total":0,"total_unread_threads":0,"threads":null}"#).unwrap();
        assert!(totals.threads.is_empty());
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct UserStatus {
    pub user_id: UserId,
    pub status: String,
}
