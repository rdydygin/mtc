//! The real-time event stream (`/api/v4/websocket`).
//!
//! [`EventStream::connect`] keeps one WebSocket alive for the lifetime of the
//! returned handle: it authenticates, answers keep-alives, detects silent
//! connection loss, and reconnects with exponential backoff. Consumers see a
//! typed [`Event`] sequence in which every reconnection is announced by
//! [`Event::Connected`], so they can refetch whatever they might have missed.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc as fmpsc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::Instant};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _, http::HeaderValue};

use crate::{
    Client, runtime,
    types::{
        ChannelId, Emoji, Millis, Post, PostId, Preference, Reaction, Status, TeamId, User, UserId,
        UserThread,
    },
};

const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A connection that has not delivered anything, not even a pong, for this
/// long is considered dead even if the OS has not noticed yet.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(75);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// A server event the client understands.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// The socket is (re)connected and authenticated. Anything that happened
    /// while disconnected was not delivered; refetch visible state.
    Connected,
    /// The socket dropped; a reconnect is scheduled.
    Disconnected,
    Posted {
        post: Post,
        /// Users mentioned by the post, when the server includes them.
        mentions: Vec<UserId>,
        team_id: TeamId,
    },
    PostEdited(Post),
    PostDeleted(Post),
    ReactionAdded(Reaction),
    ReactionRemoved(Reaction),
    Typing {
        channel_id: ChannelId,
        parent_id: PostId,
        user_id: UserId,
    },
    /// The current user read a channel, possibly on another device.
    ChannelViewed(ChannelId),
    /// The current user read several channels at once.
    ChannelsViewed(Vec<(ChannelId, Millis)>),
    StatusChanged {
        user_id: UserId,
        status: Status,
    },
    /// A user's profile changed, including their custom status.
    UserUpdated(User),
    /// Channel membership or metadata changed (joined, left, created,
    /// renamed, a new direct message, …). Reload the channel list.
    ChannelsChanged,
    /// Someone added a custom emoji to the server.
    EmojiAdded(Emoji),
    /// The current user saved preferences, possibly on another device.
    PreferencesChanged(Vec<Preference>),
    /// The current user deleted preferences, possibly on another device.
    PreferencesDeleted(Vec<Preference>),
    /// The current user rearranged their sidebar categories, which include
    /// favorites. Newer web clients change favorites this way.
    SidebarCategoriesChanged,
    /// A thread the user follows changed, usually with a new reply.
    ThreadUpdated {
        thread: UserThread,
        /// Empty for threads in direct and group messages.
        team_id: TeamId,
    },
    /// The user read followed threads, possibly on another device.
    ThreadRead {
        /// The thread read, or `None` when every thread in `channel_id`
        /// was (or, with no channel either, every thread in `team_id`).
        thread_id: Option<PostId>,
        channel_id: Option<ChannelId>,
        team_id: TeamId,
        /// When the threads were read.
        timestamp: Millis,
        unread_replies: i64,
        unread_mentions: i64,
    },
    /// The user started or stopped following a thread.
    ThreadFollowChanged {
        thread_id: PostId,
        team_id: TeamId,
        following: bool,
    },
    /// Any other event, passed through unparsed so extensions can react to
    /// events this crate does not model.
    Other {
        name: String,
        data: Value,
        broadcast: Value,
    },
}

#[derive(Deserialize)]
struct RawEvent {
    #[serde(default)]
    event: String,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    broadcast: Value,
}

/// Owns the background connection. Dropping it closes the socket.
pub struct EventStream {
    outgoing: mpsc::UnboundedSender<Value>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl EventStream {
    /// Start the connection. Events arrive on the returned receiver, which
    /// can be polled from any executor.
    pub fn connect(client: Client) -> (Self, fmpsc::UnboundedReceiver<Event>) {
        let (events_tx, events_rx) = fmpsc::unbounded();
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let task = runtime::spawn(run(client, events_tx, outgoing_rx));
        (Self { outgoing, task }, events_rx)
    }

    /// Tell the channel's members that the user is typing.
    pub fn send_typing(&self, channel: &ChannelId, parent: Option<&PostId>) {
        let _ = self.outgoing.send(json!({
            "action": "user_typing",
            "data": {
                "channel_id": channel,
                "parent_id": parent.map(|p| p.as_str()).unwrap_or(""),
            },
        }));
    }
}

async fn run(
    client: Client,
    events: fmpsc::UnboundedSender<Event>,
    mut outgoing: mpsc::UnboundedReceiver<Value>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match connect_once(&client, &events, &mut outgoing).await {
            Ok(()) => {}
            Err(err) => log::warn!("websocket: {err:#}"),
        }
        if events.is_closed() {
            return;
        }
        let _ = events.unbounded_send(Event::Disconnected);
        // A connection that stayed up for a while was healthy; start over.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_once(
    client: &Client,
    events: &fmpsc::UnboundedSender<Event>,
    outgoing: &mut mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    let mut request = client.websocket_url()?.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", client.token()))?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connect")?;
    let (mut sink, mut stream) = socket.split();

    let mut seq: u64 = 1;
    sink.send(Message::text(
        json!({
            "seq": seq,
            "action": "authentication_challenge",
            "data": { "token": client.token() },
        })
        .to_string(),
    ))
    .await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            message = stream.next() => {
                let message = message.ok_or_else(|| anyhow!("closed by server"))??;
                last_heard = Instant::now();
                match message {
                    Message::Text(text) => {
                        if let Some(event) = parse_event(&text)
                            && events.unbounded_send(event).is_err() {
                                return Ok(());
                            }
                    }
                    Message::Close(_) => return Err(anyhow!("closed by server")),
                    _ => {}
                }
            }
            Some(mut action) = outgoing.recv() => {
                seq += 1;
                action["seq"] = json!(seq);
                sink.send(Message::text(action.to_string())).await?;
            }
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENCE_TIMEOUT {
                    return Err(anyhow!("connection went silent"));
                }
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

/// Several event payloads embed objects as JSON strings.
fn embedded<T: serde::de::DeserializeOwned>(data: &Value, key: &str) -> Option<T> {
    match data.get(key)? {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => serde_json::from_value(other.clone()).ok(),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub(crate) fn parse_event(text: &str) -> Option<Event> {
    let raw: RawEvent = serde_json::from_str(text).ok()?;
    // Replies to our own actions carry no event name.
    if raw.event.is_empty() {
        return None;
    }
    let data = &raw.data;
    let broadcast = &raw.broadcast;
    Some(match raw.event.as_str() {
        "hello" => Event::Connected,
        "posted" => Event::Posted {
            post: embedded(data, "post")?,
            mentions: embedded(data, "mentions").unwrap_or_default(),
            team_id: str_field(data, "team_id").into(),
        },
        "post_edited" => Event::PostEdited(embedded(data, "post")?),
        "post_deleted" => Event::PostDeleted(embedded(data, "post")?),
        "reaction_added" => Event::ReactionAdded(embedded(data, "reaction")?),
        "reaction_removed" => Event::ReactionRemoved(embedded(data, "reaction")?),
        "typing" => Event::Typing {
            channel_id: str_field(broadcast, "channel_id").into(),
            parent_id: str_field(data, "parent_id").into(),
            user_id: str_field(data, "user_id").into(),
        },
        "user_updated" => Event::UserUpdated(embedded(data, "user")?),
        "channel_viewed" => Event::ChannelViewed(str_field(data, "channel_id").into()),
        "multiple_channels_viewed" => Event::ChannelsViewed(
            data.get("channel_times")
                .and_then(Value::as_object)
                .map(|times| {
                    times
                        .iter()
                        .filter_map(|(id, at)| Some((ChannelId::from(id.as_str()), at.as_i64()?)))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "status_change" => Event::StatusChanged {
            user_id: str_field(data, "user_id").into(),
            status: Status::parse(str_field(data, "status")),
        },
        "channel_created" | "channel_deleted" | "channel_restored" | "channel_updated"
        | "channel_converted" | "direct_added" | "group_added" | "user_added" | "user_removed"
        | "added_to_team" | "leave_team" => Event::ChannelsChanged,
        "emoji_added" => Event::EmojiAdded(embedded(data, "emoji")?),
        "preferences_changed" => Event::PreferencesChanged(embedded(data, "preferences")?),
        "preference_changed" => Event::PreferencesChanged(vec![embedded(data, "preference")?]),
        "preferences_deleted" => Event::PreferencesDeleted(embedded(data, "preferences")?),
        "sidebar_category_updated" => Event::SidebarCategoriesChanged,
        "thread_updated" => Event::ThreadUpdated {
            thread: embedded(data, "thread")?,
            team_id: str_field(broadcast, "team_id").into(),
        },
        "thread_read_changed" => {
            let thread_id = str_field(data, "thread_id");
            // Every thread in a channel is read at once when the channel is.
            let channel_id = str_field(broadcast, "channel_id");
            let number = |key: &str| data.get(key).and_then(Value::as_i64).unwrap_or_default();
            Event::ThreadRead {
                thread_id: (!thread_id.is_empty()).then(|| thread_id.into()),
                channel_id: (thread_id.is_empty() && !channel_id.is_empty())
                    .then(|| channel_id.into()),
                team_id: str_field(broadcast, "team_id").into(),
                timestamp: number("timestamp"),
                unread_replies: number("unread_replies"),
                unread_mentions: number("unread_mentions"),
            }
        }
        "thread_follow_changed" => Event::ThreadFollowChanged {
            thread_id: str_field(data, "thread_id").into(),
            team_id: str_field(broadcast, "team_id").into(),
            following: data.get("state").and_then(Value::as_bool).unwrap_or(false),
        },
        name => Event::Other {
            name: name.to_string(),
            data: raw.data.clone(),
            broadcast: raw.broadcast.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_posted_with_embedded_post() {
        let text = r#"{"event":"posted","data":{"post":"{\"id\":\"p1\",\"channel_id\":\"c1\",\"user_id\":\"u1\",\"message\":\"hi\",\"create_at\":5}","mentions":"[\"u2\"]","team_id":"t1"},"broadcast":{"channel_id":"c1"},"seq":3}"#;
        match parse_event(text) {
            Some(Event::Posted {
                post,
                mentions,
                team_id,
            }) => {
                assert_eq!(post.id.as_str(), "p1");
                assert_eq!(post.message, "hi");
                assert_eq!(mentions, vec![UserId::from("u2")]);
                assert_eq!(team_id.as_str(), "t1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_typing_and_ignores_replies() {
        let text = r#"{"event":"typing","data":{"parent_id":"","user_id":"u1"},"broadcast":{"channel_id":"c1"}}"#;
        assert!(matches!(parse_event(text), Some(Event::Typing { .. })));
        assert!(parse_event(r#"{"status":"OK","seq_reply":1}"#).is_none());
    }

    #[test]
    fn parses_preferences_and_emoji() {
        let text = r#"{"event":"preferences_changed","data":{"preferences":"[{\"user_id\":\"u1\",\"category\":\"favorite_channel\",\"name\":\"c1\",\"value\":\"true\"}]"},"broadcast":{}}"#;
        match parse_event(text) {
            Some(Event::PreferencesChanged(preferences)) => {
                assert_eq!(preferences.len(), 1);
                assert_eq!(
                    preferences[0].favorite_channel(),
                    Some(ChannelId::from("c1"))
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        let text = r#"{"event":"emoji_added","data":{"emoji":"{\"id\":\"e1\",\"name\":\"parrot\"}"},"broadcast":{}}"#;
        assert!(
            matches!(parse_event(text), Some(Event::EmojiAdded(emoji)) if emoji.name == "parrot")
        );
    }

    #[test]
    fn keeps_server_times_for_viewed_channels() {
        let text = r#"{"event":"multiple_channels_viewed","data":{"channel_times":{"c1":120,"c2":240}},"broadcast":{}}"#;
        match parse_event(text) {
            Some(Event::ChannelsViewed(channels)) => {
                assert_eq!(channels, vec![("c1".into(), 120), ("c2".into(), 240)]);
            }
            other => panic!("unexpected {other:?}"),
        }
        let text = r#"{"event":"channel_viewed","data":{"channel_id":"c1"},"broadcast":{}}"#;
        assert!(matches!(parse_event(text), Some(Event::ChannelViewed(id)) if id.as_str() == "c1"));
    }

    #[test]
    fn parses_thread_events() {
        let text = r#"{"event":"thread_updated","data":{"thread":"{\"id\":\"root\",\"reply_count\":2,\"last_reply_at\":20,\"unread_replies\":1,\"unread_mentions\":0,\"participants\":[{\"id\":\"u1\"}],\"post\":{\"id\":\"root\",\"channel_id\":\"c1\"}}","previous_unread_replies":0},"broadcast":{"team_id":"t1","user_id":"me"}}"#;
        match parse_event(text) {
            Some(Event::ThreadUpdated { thread, team_id }) => {
                assert_eq!(thread.id.as_str(), "root");
                assert_eq!(thread.unread_replies, 1);
                assert_eq!(thread.participants[0].id.as_str(), "u1");
                assert_eq!(team_id.as_str(), "t1");
            }
            other => panic!("unexpected {other:?}"),
        }

        let one = r#"{"event":"thread_read_changed","data":{"thread_id":"root","timestamp":50,"unread_replies":0,"unread_mentions":0,"channel_id":"c1"},"broadcast":{"team_id":"t1"}}"#;
        assert!(matches!(
            parse_event(one),
            Some(Event::ThreadRead { thread_id: Some(id), channel_id: None, timestamp: 50, .. })
                if id.as_str() == "root"
        ));
        let channel = r#"{"event":"thread_read_changed","data":{"timestamp":60},"broadcast":{"channel_id":"c1","team_id":""}}"#;
        assert!(matches!(
            parse_event(channel),
            Some(Event::ThreadRead { thread_id: None, channel_id: Some(c), .. }) if c.as_str() == "c1"
        ));
        let team = r#"{"event":"thread_read_changed","data":{"timestamp":70},"broadcast":{"team_id":"t1"}}"#;
        assert!(matches!(
            parse_event(team),
            Some(Event::ThreadRead {
                thread_id: None,
                channel_id: None,
                ..
            })
        ));

        let unfollowed = r#"{"event":"thread_follow_changed","data":{"thread_id":"root","state":false,"reply_count":2},"broadcast":{"team_id":"t1"}}"#;
        assert!(matches!(
            parse_event(unfollowed),
            Some(Event::ThreadFollowChanged {
                following: false,
                ..
            })
        ));
    }

    #[test]
    fn passes_unknown_events_through() {
        let text = r#"{"event":"custom_plugin_event","data":{"x":1},"broadcast":{}}"#;
        assert!(
            matches!(parse_event(text), Some(Event::Other { name, .. }) if name == "custom_plugin_event")
        );
    }
}
