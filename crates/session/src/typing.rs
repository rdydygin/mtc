use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use gpui_kit::{Context, EventEmitter, Task};
use mattermost::{ChannelId, PostId, UserId};

use crate::TimelineKind;

/// How long a "typing" signal stays visible without being repeated.
const TYPING_TTL: Duration = Duration::from_secs(6);

/// The users typing in one conversation changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypingChanged(pub TimelineKind);

/// Who is typing where. Kept apart from the session so a signal reaches
/// only the typing line showing that conversation, not every view that
/// watches the session.
///
/// It never notifies; it emits [`TypingChanged`] for the one conversation
/// whose typing users changed.
#[derive(Default)]
pub struct Typing {
    /// When each user's latest signal arrived, on the executor's clock.
    users: HashMap<TimelineKind, Vec<(UserId, Instant)>>,
    _expiry: Option<Task<()>>,
}

impl EventEmitter<TypingChanged> for Typing {}

impl Typing {
    /// Users currently typing in a channel or thread.
    pub fn users(&self, kind: &TimelineKind) -> impl Iterator<Item = &UserId> {
        self.users
            .get(kind)
            .into_iter()
            .flatten()
            .map(|(user, _)| user)
    }

    /// `user` is typing in `channel`, in the thread `parent` unless it is
    /// empty, until a few seconds pass without another signal.
    pub fn start(
        &mut self,
        channel: ChannelId,
        parent: PostId,
        user: UserId,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let kind = kind(channel, parent);
        let users = self.users.entry(kind.clone()).or_default();
        let known = users.iter().any(|(u, _)| u == &user);
        users.retain(|(u, _)| u != &user);
        users.push((user, now));
        if self._expiry.is_none() {
            self.schedule_expiry(cx);
        }
        // A repeated signal only extends the time; the line stays the same.
        if !known {
            cx.emit(TypingChanged(kind));
        }
    }

    /// `user` stopped typing, as when their message arrives.
    pub fn stop(
        &mut self,
        channel: ChannelId,
        parent: PostId,
        user: &UserId,
        cx: &mut Context<Self>,
    ) {
        let kind = kind(channel, parent);
        let Some(users) = self.users.get_mut(&kind) else {
            return;
        };
        let before = users.len();
        users.retain(|(u, _)| u != user);
        if users.len() == before {
            return;
        }
        if users.is_empty() {
            self.users.remove(&kind);
        }
        cx.emit(TypingChanged(kind));
    }

    /// Wake when the oldest signal runs out.
    fn schedule_expiry(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self
            .users
            .values()
            .flatten()
            .map(|(_, at)| *at + TYPING_TTL)
            .min()
        else {
            self._expiry = None;
            return;
        };
        let delay = deadline.saturating_duration_since(cx.background_executor().now());
        self._expiry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| this.expire(cx));
        }));
    }

    fn expire(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let mut changed = Vec::new();
        self.users.retain(|kind, users| {
            let before = users.len();
            users.retain(|(_, at)| now.saturating_duration_since(*at) < TYPING_TTL);
            if users.len() != before {
                changed.push(kind.clone());
            }
            !users.is_empty()
        });
        self.schedule_expiry(cx);
        for kind in changed {
            cx.emit(TypingChanged(kind));
        }
    }
}

fn kind(channel: ChannelId, parent: PostId) -> TimelineKind {
    if parent.is_empty() {
        TimelineKind::Channel(channel)
    } else {
        TimelineKind::Thread {
            channel,
            root: parent,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::{AppContext as _, Entity, Subscription, TestAppContext};

    use super::*;

    fn watch(
        typing: &Entity<Typing>,
        cx: &mut TestAppContext,
    ) -> (Rc<RefCell<Vec<TimelineKind>>>, Subscription) {
        let changed = Rc::new(RefCell::new(Vec::new()));
        let log = changed.clone();
        let subscription = cx.update(|cx| {
            cx.subscribe(typing, move |_, TypingChanged(kind), _| {
                log.borrow_mut().push(kind.clone())
            })
        });
        (changed, subscription)
    }

    fn users(typing: &Entity<Typing>, kind: &TimelineKind, cx: &TestAppContext) -> Vec<UserId> {
        typing.read_with(cx, |t, _| t.users(kind).cloned().collect())
    }

    #[gpui_kit::test]
    fn each_user_clears_when_their_own_signal_runs_out(cx: &mut TestAppContext) {
        let typing = cx.new(|_| Typing::default());
        let (changed, _subscription) = watch(&typing, cx);
        let town = TimelineKind::Channel("town".into());
        let start = |user: &str, cx: &mut TestAppContext| {
            typing.update(cx, |t, cx| {
                t.start("town".into(), PostId::default(), user.into(), cx)
            })
        };

        start("ann", cx);
        cx.executor().advance_clock(Duration::from_secs(2));
        start("ben", cx);
        cx.executor().advance_clock(Duration::from_secs(1));
        // Ann again: more time, but the line reads the same.
        start("ann", cx);
        assert_eq!(*changed.borrow(), [town.clone(), town.clone()]);
        assert_eq!(users(&typing, &town, cx), ["ben".into(), "ann".into()]);

        // Ann's first signal ran out at 6s, but her repeat keeps her; Ben's
        // runs out at 8s.
        cx.executor().advance_clock(Duration::from_secs(5));
        assert_eq!(users(&typing, &town, cx), ["ann".into()]);
        assert_eq!(changed.borrow().len(), 3);

        cx.executor().advance_clock(Duration::from_secs(1));
        assert!(users(&typing, &town, cx).is_empty());
        assert_eq!(changed.borrow().len(), 4);
        typing.read_with(cx, |t, _| {
            assert!(t._expiry.is_none() && t.users.is_empty())
        });
    }

    #[gpui_kit::test]
    fn a_message_ends_its_authors_typing_in_that_conversation_only(cx: &mut TestAppContext) {
        let typing = cx.new(|_| Typing::default());
        let (changed, _subscription) = watch(&typing, cx);
        let thread = TimelineKind::Thread {
            channel: "town".into(),
            root: "root".into(),
        };
        let ann = UserId::from("ann");
        typing.update(cx, |t, cx| {
            t.start("town".into(), "root".into(), ann.clone(), cx);
            t.start("town".into(), PostId::default(), ann.clone(), cx);
        });
        changed.borrow_mut().clear();

        typing.update(cx, |t, cx| t.stop("town".into(), "root".into(), &ann, cx));
        // Someone who wasn't typing changes nothing.
        typing.update(cx, |t, cx| {
            t.stop("town".into(), "root".into(), &"ben".into(), cx)
        });

        assert_eq!(*changed.borrow(), std::slice::from_ref(&thread));
        assert!(users(&typing, &thread, cx).is_empty());
        assert_eq!(
            users(&typing, &TimelineKind::Channel("town".into()), cx),
            [ann]
        );
    }
}
