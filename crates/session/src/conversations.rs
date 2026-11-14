//! Retained conversation presentation. Session mutations invalidate order;
//! directory name revisions invalidate participant labels without presence work.
use std::{collections::HashMap, rc::Rc};

use gpui_kit::{App, SharedString};
use mattermost::ChannelId;

use super::{ChannelEntry, Session};

const DIRECT_MESSAGE_LIMIT: usize = 20;

/// Conversation IDs in navigation order. Favorites appear in only one section.
#[derive(Clone, Default)]
pub struct ChannelSections {
    favorites: Rc<[ChannelId]>,
    channels: Rc<[ChannelId]>,
    direct: Rc<[ChannelId]>,
}

impl ChannelSections {
    pub fn favorites(&self) -> &[ChannelId] {
        &self.favorites
    }
    pub fn channels(&self) -> &[ChannelId] {
        &self.channels
    }
    pub fn direct(&self) -> &[ChannelId] {
        &self.direct
    }
    pub fn ordered(&self) -> impl Iterator<Item = &ChannelId> {
        self.favorites
            .iter()
            .chain(self.channels.iter())
            .chain(self.direct.iter())
    }
}

struct Label {
    kind: mattermost::ChannelType,
    name: String,
    display_name: String,
    names_revision: u64,
    text: SharedString,
    key: SharedString,
}

#[derive(Default)]
pub(super) struct ConversationCache {
    labels: HashMap<ChannelId, Label>,
    all: Option<Rc<ChannelSections>>,
    visible: Option<(Option<ChannelId>, Rc<ChannelSections>)>,
    names_revision: u64,
    #[cfg(test)]
    builds: usize,
    #[cfg(test)]
    label_computations: usize,
}

impl Session {
    pub(super) fn set_group_members(
        &mut self,
        channel: ChannelId,
        members: Vec<mattermost::UserId>,
    ) {
        if self.group_members.get(&channel) != Some(&members) {
            self.conversations.borrow_mut().labels.remove(&channel);
            self.group_members.insert(channel, members);
            self.invalidate_conversations();
        }
    }

    pub(super) fn invalidate_conversations(&self) {
        let mut cache = self.conversations.borrow_mut();
        cache.all = None;
        cache.visible = None;
    }

    pub(super) fn invalidate_conversation_visibility(&self) {
        self.conversations.borrow_mut().visible = None;
    }

    /// Shared across sidebar frames, keyboard navigation and the switcher.
    /// Presence, typing, avatars and custom statuses do not rebuild this snapshot.
    pub fn conversation_sections(&self, cx: &App) -> Rc<ChannelSections> {
        let names_revision = self.directory.read(cx).names_revision();
        {
            let cache = self.conversations.borrow();
            if cache.names_revision == names_revision
                && let Some(all) = &cache.all
            {
                return all.clone();
            }
        }
        let mut favorites = Vec::new();
        let mut channels = Vec::new();
        let mut direct = Vec::new();
        for entry in self.channels() {
            if self.is_favorite(entry.id()) {
                favorites.push((entry.id().clone(), self.channel_sort_key(entry, cx)));
            } else if entry.kind().is_direct_or_group() {
                direct.push(entry);
            } else {
                channels.push((entry.id().clone(), self.channel_sort_key(entry, cx)));
            }
        }
        // Stable sorts deliberately retain the previous tie behavior.
        favorites.sort_by(|a, b| a.1.cmp(&b.1));
        channels.sort_by(|a, b| a.1.cmp(&b.1));
        direct.sort_by_key(|e| std::cmp::Reverse(e.last_activity()));
        let all = Rc::new(ChannelSections {
            favorites: favorites
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
                .into(),
            channels: channels
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
                .into(),
            direct: direct
                .into_iter()
                .map(|e| e.id().clone())
                .collect::<Vec<_>>()
                .into(),
        });
        let mut cache = self.conversations.borrow_mut();
        cache.labels.retain(|id, _| self.channels.contains_key(id));
        cache.names_revision = names_revision;
        cache.all = Some(all.clone());
        cache.visible = None;
        #[cfg(test)]
        {
            cache.builds += 1;
        }
        all
    }

    /// The sidebar keeps the most recent DMs plus unread and active ones.
    pub fn sidebar_sections(&self, cx: &App) -> Rc<ChannelSections> {
        let all = self.conversation_sections(cx);
        let mut cache = self.conversations.borrow_mut();
        if let Some((active, visible)) = &cache.visible
            && active.as_ref() == self.active_channel()
        {
            return visible.clone();
        }
        let mut visible = (*all).clone();
        visible.direct = all
            .direct
            .iter()
            .enumerate()
            .filter(|(ix, id)| {
                *ix < DIRECT_MESSAGE_LIMIT
                    || Some(*id) == self.active_channel()
                    || self
                        .channel(id)
                        .is_some_and(|e| e.is_unread() || e.mention_count() > 0)
            })
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>()
            .into();
        let visible = Rc::new(visible);
        cache.visible = Some((self.active_channel.clone(), visible.clone()));
        visible
    }

    /// The retained label, including sorted group participant names.
    pub fn channel_label(&self, entry: &ChannelEntry, cx: &App) -> SharedString {
        self.ensure_channel_label(entry, cx);
        self.conversations.borrow().labels[entry.id()].text.clone()
    }

    fn channel_sort_key(&self, entry: &ChannelEntry, cx: &App) -> SharedString {
        self.ensure_channel_label(entry, cx);
        self.conversations.borrow().labels[entry.id()].key.clone()
    }

    fn ensure_channel_label(&self, entry: &ChannelEntry, cx: &App) {
        let channel = entry.channel();
        let names_revision = if entry.kind().is_direct_or_group() {
            self.directory.read(cx).names_revision()
        } else {
            0
        };
        if self
            .conversations
            .borrow()
            .labels
            .get(entry.id())
            .is_some_and(|label| {
                label.kind == channel.kind
                    && label.name == channel.name
                    && label.display_name == channel.display_name
                    && label.names_revision == names_revision
            })
        {
            return;
        }
        let text = self.compute_channel_label(entry, cx);
        let key = text.to_lowercase().into();
        let mut cache = self.conversations.borrow_mut();
        cache.labels.insert(
            entry.id().clone(),
            Label {
                kind: channel.kind,
                name: channel.name.clone(),
                display_name: channel.display_name.clone(),
                names_revision,
                text,
                key,
            },
        );
        #[cfg(test)]
        {
            cache.label_computations += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext as _, TestAppContext};
    use mattermost::{Channel, ChannelMember, Client, Event, Post, Status, User};
    use serde_json::json;

    fn entry(ix: usize, direct: bool) -> ChannelEntry {
        ChannelEntry {
            channel: serde_json::from_value(json!({
                "id": format!("c-{ix}"), "name": if direct { format!("me__u-{ix}") } else { format!("channel-{ix:04}") },
                "display_name": format!("Channel {ix:04}"), "type": if direct { "D" } else { "O" },
                "last_post_at": ix + 1,
            })).unwrap(),
            member: Some(serde_json::from_value(json!({
                "channel_id": format!("c-{ix}"), "user_id": "me", "last_viewed_at": 10000
            })).unwrap()),
        }
    }

    // No executor turn is needed: exercise the presentation through session commands
    // before any startup response can replace the fixture.
    #[gpui_kit::test]
    fn five_thousand_conversations_reuse_labels_and_order_across_presence_bursts(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let client = Client::new_unverified("http://127.0.0.1:1", "token").unwrap();
        let me: User = serde_json::from_value(json!({"id":"me", "username":"me"})).unwrap();
        let session = cx.new(|cx| Session::new(client, me, None, cx));
        session.update(cx, |s, cx| {
            s.channels = (0..5000)
                .map(|ix| {
                    let e = entry(ix, ix >= 4970);
                    (e.id().clone(), e)
                })
                .collect();
            let all = s.conversation_sections(cx);
            let visible = s.sidebar_sections(cx);
            assert_eq!(all.channels().len(), 4970);
            assert_eq!(all.direct().len(), 30);
            assert_eq!(visible.direct().len(), 20);
            assert_eq!(all.channels().first(), Some(&"c-0".into()));
            assert_eq!(all.direct().first(), Some(&"c-4999".into()));
            for entry in s.channels() {
                s.channel_label(entry, cx);
            }
            assert_eq!(s.conversations.borrow().label_computations, 5000);
            let builds = s.conversations.borrow().builds;
            for _ in 0..100 {
                s.handle_event(
                    Event::StatusChanged {
                        user_id: "u-4999".into(),
                        status: Status::Away,
                    },
                    cx,
                );
                assert!(Rc::ptr_eq(&all, &s.conversation_sections(cx)));
                assert!(Rc::ptr_eq(&visible, &s.sidebar_sections(cx)));
                s.channel_label(s.channel(&"c-4999".into()).unwrap(), cx);
            }
            assert_eq!(s.conversations.borrow().builds, builds);
            assert_eq!(s.conversations.borrow().label_computations, 5000);
            s.set_favorite(&"c-4999".into(), true, cx);
            assert_eq!(
                s.conversation_sections(cx).favorites(),
                &[ChannelId::from("c-4999")]
            );
            assert!(!s.sidebar_sections(cx).direct().contains(&"c-4999".into()));
            s.note_activity(
                &serde_json::from_value::<Post>(json!({
                    "id":"new", "channel_id":"c-4970", "user_id":"other", "create_at":20000
                }))
                .unwrap(),
                false,
                cx,
            );
            assert_eq!(
                s.conversation_sections(cx).direct().first(),
                Some(&"c-4970".into())
            );
            assert_eq!(
                s.conversations.borrow().label_computations,
                5000,
                "activity and favorites reuse sort keys"
            );
            // Active and unread DMs remain visible beyond the recent limit.
            s.active_channel = Some("c-4971".into());
            assert!(s.sidebar_sections(cx).direct().contains(&"c-4971".into()));
            let member: ChannelMember = serde_json::from_value(json!({
                "channel_id":"c-4972", "user_id":"me", "last_viewed_at":0, "mention_count":1
            }))
            .unwrap();
            s.apply_channel_member(member.clone(), None, cx);
            assert!(s.sidebar_sections(cx).direct().contains(&member.channel_id));
            let mut read = member.clone();
            read.last_viewed_at = 10000;
            read.mention_count = 0;
            s.apply_channel_member(read, None, cx);
            assert!(!s.sidebar_sections(cx).direct().contains(&member.channel_id));
            s.close_channel(cx);
            assert!(!s.sidebar_sections(cx).direct().contains(&"c-4971".into()));
        });
    }

    #[gpui_kit::test]
    fn participant_names_and_membership_invalidate_group_labels_but_status_does_not(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let client = Client::new_unverified("http://127.0.0.1:1", "token").unwrap();
        let me: User = serde_json::from_value(json!({"id":"me"})).unwrap();
        let session = cx.new(|cx| Session::new(client, me, None, cx));
        session.update(cx, |s, cx| {
            let channel: Channel =
                serde_json::from_value(json!({"id":"group", "name":"group", "type":"G"})).unwrap();
            let id = channel.id.clone();
            s.channels.insert(
                id.clone(),
                ChannelEntry {
                    channel,
                    member: None,
                },
            );
            s.set_group_members(id.clone(), vec!["ann".into(), "ben".into()]);
            s.favorites.insert(id.clone());
            let users: Vec<User> = serde_json::from_value(json!([
                {"id":"ann", "username":"ann", "first_name":"Ann"},
                {"id":"ben", "username":"ben", "first_name":"Ben"}
            ]))
            .unwrap();
            s.directory.update(cx, |d, cx| d.insert(users, cx));
            let all = s.conversation_sections(cx);
            assert_eq!(s.channel_label(s.channel(&id).unwrap(), cx), "Ann, Ben");
            let work = s.conversations.borrow().label_computations;
            s.directory.update(cx, |d, cx| {
                d.update_user(
                    &"ann".into(),
                    |u| {
                        u.last_picture_update = 5;
                    },
                    cx,
                )
            });
            assert!(Rc::ptr_eq(&all, &s.conversation_sections(cx)));
            assert_eq!(s.conversations.borrow().label_computations, work);
            s.directory.update(cx, |d, cx| {
                d.update_user(
                    &"ann".into(),
                    |u| {
                        u.first_name = "Zoe".into();
                    },
                    cx,
                )
            });
            assert!(!Rc::ptr_eq(&all, &s.conversation_sections(cx)));
            assert_eq!(s.channel_label(s.channel(&id).unwrap(), cx), "Ben, Zoe");
            let all = s.conversation_sections(cx);
            s.set_group_members(id.clone(), vec!["ann".into()]);
            assert!(!Rc::ptr_eq(&all, &s.conversation_sections(cx)));
            assert_eq!(s.channel_label(s.channel(&id).unwrap(), cx), "Zoe");
        });
    }
}
