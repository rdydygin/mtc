//! GPUI Session entities against real Mattermost HTTP and WebSocket traffic.
#[path = "../../mattermost/tests/support/mod.rs"]
mod support;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use mattermost::Post;
use session::{Connection, Delivery, LoadState, SearchScope, Session, SessionEvent, TimelineKind};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use support::{Live, io};

fn wait(cx: &mut TestAppContext, label: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "LIVE session timeout: {label}; see wire transcript"
        );
        cx.executor().advance_clock(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish<T: 'static>(cx: &mut TestAppContext, task: gpui_kit::Task<T>) -> T {
    let result = Rc::new(RefCell::new(None));
    let slot = result.clone();
    cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
        .detach();
    wait(cx, "GPUI task result", |_| result.borrow().is_some());
    result.borrow_mut().take().unwrap()
}

fn start(f: &Live, cx: &mut TestAppContext) -> Entity<Session> {
    cx.executor().allow_parking();
    let s = cx.new(|cx| Session::new(f.a.clone(), f.alice.clone(), Some(f.team.id.clone()), cx));
    wait(cx, "startup teams/channels and authenticated hello", |cx| {
        s.read_with(cx, |s, _| {
            s.is_loaded() && s.teams_loaded() && s.connection() == Connection::Online
        })
    });
    s.read_with(cx, |s, _| {
        assert_eq!(s.me().id, f.alice.id);
        assert_eq!(s.team().unwrap().id, f.team.id);
        assert!(s.channel(&f.public.id).is_some());
        assert!(s.channel(&f.private.id).is_some());
    });
    s
}

#[gpui_kit::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
fn startup_send_receive_reconnect_mark_unread_and_mute(cx: &mut TestAppContext) {
    let Some(f) = io(Live::provision()) else {
        return;
    };
    let s = start(&f, cx);
    let failures = Rc::new(RefCell::new(Vec::new()));
    let failed = failures.clone();
    let received_mention = Rc::new(RefCell::new(None));
    let mentioned = received_mention.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&s, move |_, event, _| {
            if let SessionEvent::SendFailed { message } = event {
                failed.borrow_mut().push(message.to_string());
            }
            if let SessionEvent::Posted { post, mentions_me } = event
                && post.message.starts_with("bob live receive")
            {
                *mentioned.borrow_mut() = Some(*mentions_me);
            }
        })
    });
    let timeline = s.update(cx, |s, cx| s.open_channel(&f.public.id, cx));
    wait(cx, "initial timeline", |cx| {
        timeline.read_with(cx, |t, _| matches!(t.state(), LoadState::Loaded))
    });
    let kind = TimelineKind::Channel(f.public.id.clone());
    let message = format!("session send {}", f.suffix);
    s.update(cx, |s, cx| {
        s.send_post(&kind, message.clone(), Vec::new(), cx)
    });
    wait(cx, "send reconciled to a single delivered post", |cx| {
        assert!(
            failures.borrow().is_empty(),
            "session send failures: {:?}",
            failures.borrow()
        );
        timeline.read_with(cx, |t, _| {
            let entries: Vec<_> = t
                .entries()
                .iter()
                .filter(|e| e.post().message == message)
                .collect();
            entries.len() == 1 && entries[0].delivery() == Delivery::Sent
        })
    });
    let sent = timeline.read_with(cx, |t, _| {
        t.entries()
            .iter()
            .find(|e| e.post().message == message)
            .unwrap()
            .post()
            .as_ref()
            .clone()
    });
    assert!(
        !sent.id.as_str().contains(':'),
        "pending post must be replaced by real server id: {sent:?}"
    );
    let bob = f.b.clone();
    let channel = f.public.id.clone();
    let alice_username = f.alice.username.clone();
    let received = io(async move {
        bob.create_post(
            &channel,
            &format!("bob live receive @{alice_username}"),
            None,
            "",
            &[],
            &Default::default(),
        )
        .await
    });
    wait(cx, "receive another user's WebSocket post", |cx| {
        timeline.read_with(cx, |t, _| t.position(&received.id).is_some())
            && received_mention.borrow().is_some()
    });
    assert_eq!(
        *received_mention.borrow(),
        Some(true),
        "recipient Session must flag mentions for notifications"
    );

    f.wire.disconnect();
    wait(cx, "Session observes real socket loss", |cx| {
        s.read_with(cx, |s, _| s.connection() == Connection::Offline)
    });
    let bob = f.b.clone();
    let channel = f.public.id.clone();
    let (edited, missed) = io(async move {
        let edited = bob
            .edit_post(&received.id, "bob edited during outage")
            .await?;
        let missed = bob
            .create_post(
                &channel,
                "bob missed during outage",
                None,
                "",
                &[],
                &Default::default(),
            )
            .await?;
        Ok((edited, missed))
    });
    assert!(timeline.read_with(cx, |t, _| t.position(&missed.id).is_none()));
    f.wire.reconnect();
    wait(
        cx,
        "reconnect catch-up includes new and edited posts",
        |cx| {
            s.read_with(cx, |s, _| s.connection() == Connection::Online)
                && timeline.read_with(cx, |t, _| {
                    t.position(&missed.id).is_some()
                        && t.entries()
                            .iter()
                            .any(|e| e.id() == &edited.id && e.post().message == edited.message)
                })
        },
    );

    // Establish a confirmed read before testing explicit unread suppression.
    // is_settled() covers timelines only, so check server-backed member time.
    s.update(cx, |s, cx| s.mark_viewed(&f.public.id, cx));
    wait(cx, "confirmed read before explicit unread", |cx| {
        s.read_with(cx, |s, _| {
            let member = s.channel(&f.public.id).unwrap();
            !member.is_unread() && member.last_viewed_at() >= missed.create_at
        })
    });
    s.update(cx, |s, cx| s.mark_post_unread(&missed, cx));
    wait(cx, "unread persisted and timeline boundary", |cx| {
        s.read_with(cx, |s, _| {
            let member = s.channel(&f.public.id).unwrap();
            member.is_unread() && member.last_viewed_at() == missed.create_at - 1
        }) && timeline.read_with(cx, |t, _| t.unread_after().is_some())
    });
    let last_viewed = s.read_with(cx, |s, _| s.channel(&f.public.id).unwrap().last_viewed_at());
    let passive_views = f.wire.requests("POST", "/api/v4/channels/members/me/view");
    // Simulate focus updates and another arrival in the already visible channel.
    s.update(cx, |s, cx| {
        s.set_window_active(false, cx);
        s.set_window_active(true, cx);
        s.open_channel(&f.public.id, cx);
    });
    let bob = f.b.clone();
    let channel = f.public.id.clone();
    let newer = io(async move {
        bob.create_post(
            &channel,
            "suppression must survive arrival",
            None,
            "",
            &[],
            &Default::default(),
        )
        .await
    });
    wait(cx, "new post while locally suppressed", |cx| {
        timeline.read_with(cx, |t, _| t.position(&newer.id).is_some())
    });
    // Negative assertion: pump timers/work long enough to dispatch a passive
    // view if suppression is broken; count requests at dispatch, not response.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        f.wire.requests("POST", "/api/v4/channels/members/me/view"),
        passive_views,
        "focus/arrival issued a view while explicitly unread"
    );
    let a = f.a.clone();
    let channel = f.public.id.clone();
    let member = io(async move { a.my_channel_member(&channel).await });
    assert_eq!(
        member.last_viewed_at, last_viewed,
        "passive view erased explicit unread"
    );
    for muted in [true, false] {
        s.update(cx, |s, cx| s.set_channel_muted(&f.public.id, muted, cx));
        wait(cx, "mute notification properties persisted", |cx| {
            s.read_with(cx, |s, _| {
                !s.is_mute_pending(&f.public.id)
                    && s.channel(&f.public.id).unwrap().is_muted() == muted
            })
        });
        let a = f.a.clone();
        let channel = f.public.id.clone();
        assert_eq!(
            io(async move { a.my_channel_member(&channel).await }).is_muted(),
            muted
        );
    }
    s.update(cx, |s, cx| s.mark_viewed(&f.public.id, cx));
    wait(cx, "explicit read releases suppression", |cx| {
        s.read_with(cx, |s, _| {
            let member = s.channel(&f.public.id).unwrap();
            !member.is_unread() && member.last_viewed_at() >= newer.create_at
        })
    });
    // Another user's real post_deleted event can carry delete_at=0. Session
    // must route the event as a deletion, rather than treating it as an edit.
    let bob = f.b.clone();
    let id = edited.id.clone();
    io(async move { bob.delete_post(&id).await });
    wait(
        cx,
        "recipient Session removes a real post_deleted snapshot",
        |cx| timeline.read_with(cx, |t, _| t.position(&edited.id).is_none()),
    );
    assert!(
        failures.borrow().is_empty(),
        "session failures: {:?}",
        failures.borrow()
    );
}

#[gpui_kit::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
fn team_search_hydrates_new_channel_and_permalink_lookup(cx: &mut TestAppContext) {
    let Some(f) = io(Live::provision()) else {
        return;
    };
    let s = start(&f, cx);
    // Hold WS offline so a new membership can't refresh the startup list before
    // search exercises ensure_channel against the real server.
    f.wire.disconnect();
    wait(cx, "socket offline before new membership", |cx| {
        s.read_with(cx, |s, _| s.connection() == Connection::Offline)
    });
    let a = f.a.clone();
    let b = f.b.clone();
    let team = f.team.clone();
    let suffix = f.suffix.clone();
    let bob = f.bob.id.clone();
    let (channel, post) = io(async move {
        let channel = Live::create_channel(&a, &team, "hydrate", &suffix, "O").await?;
        a.join_channel(&channel.id, &bob).await?;
        let post = b
            .create_post(
                &channel.id,
                &format!("mtchydrate{suffix}"),
                None,
                "",
                &[],
                &Default::default(),
            )
            .await?;
        Ok((channel, post))
    });
    assert!(s.read_with(cx, |s, _| s.channel(&channel.id).is_none()));
    let task = s.update(cx, |s, cx| {
        s.search_messages(
            &SearchScope::Team,
            &format!("mtchydrate{}", f.suffix),
            0,
            cx,
        )
    });
    let found = finish(cx, task).unwrap();
    assert!(
        found.posts.iter().any(|p| p.id == post.id),
        "team search/hydration lost real hit: {found:?}"
    );
    s.read_with(cx, |s, _| {
        assert_eq!(s.channel(&channel.id).unwrap().channel().name, channel.name)
    });
    // Permalinks use Client::post_by_id then Session::ensure_channel/open_channel.
    // There is no separate Session permalink method; exercise that production path.
    let a = f.a.clone();
    let id = post.id.clone();
    let linked: Post = io(async move { a.post_by_id(&id).await });
    let task = s.update(cx, |s, cx| s.ensure_channel(&linked.channel_id, cx));
    finish(cx, task).unwrap();
    let timeline = s.update(cx, |s, cx| s.open_channel(&linked.channel_id, cx));
    wait(cx, "permalink post in hydrated timeline", |cx| {
        timeline.read_with(cx, |t, _| t.position(&linked.id).is_some())
    });
    f.wire.reconnect();
}
