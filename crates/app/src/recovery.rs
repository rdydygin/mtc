//! Compact recovery feedback; callers retain content and own the retry operation.
use gpui_kit::component::{ActiveTheme as _, Sizable as _, button::Button, v_flex};
use gpui_kit::{App, IntoElement, ParentElement as _, Styled as _};

pub fn inline_failure(
    message: impl Into<gpui_kit::SharedString>,
    retry: Button,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .flex_none()
        .p_3()
        .gap_2()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(message.into())
        .child(retry.small().outline().label("Retry"))
}

#[cfg(test)]
pub(crate) mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowOptions};
    use mattermost::{
        Client, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;
    use session::{LoadState, Session, TimelineKind};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    pub(crate) fn route(request: &Request) -> Reply {
        let path = request.path().split('?').next().unwrap_or_default();
        match (request.method(), path) {
            ("GET", "/api/v4/users/me/teams") => {
                Reply::json(json!([{ "id": "team", "name": "team" }]))
            }
            ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(
                json!([{ "id": "town", "team_id": "team", "type": "O", "name": "town-square" }]),
            ),
            ("GET", "/api/v4/channels/town/posts") => {
                let posts: serde_json::Map<_, _> = (0..60).map(|i| {
                    let id = format!("p{i}");
                    (id.clone(), json!({"id": id, "channel_id": "town", "create_at": i + 1, "message": "Message"}))
                }).collect();
                Reply::json(json!({"order": posts.keys().collect::<Vec<_>>(), "posts": posts}))
            }
            (
                "GET",
                "/api/v4/users/me/teams/team/channels/members"
                | "/api/v4/users/me/preferences/favorite_channel"
                | "/api/v4/emoji",
            )
            | ("POST", "/api/v4/users/ids" | "/api/v4/users/status/ids") => Reply::json(json!([])),
            _ => Reply::not_found(),
        }
    }

    pub(crate) fn start(server: &SlowServer, cx: &mut TestAppContext) -> Entity<Session> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::threads_view::init(cx);
            crate::extensions::init(cx);
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        cx.new(|cx| Session::new(client, me, Some("team".into()), cx))
    }

    pub(crate) fn wait_until(
        cx: &mut TestAppContext,
        mut done: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for recovery");
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui_kit::test]
    fn failed_team_discovery_retries_to_a_loaded_empty_account(cx: &mut TestAppContext) {
        let fail = Arc::new(AtomicBool::new(true));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let fail = fail.clone();
            move |request| {
                if request.method() == "GET" && request.path() == "/api/v4/users/me/teams" {
                    return if fail.load(Ordering::SeqCst) {
                        Reply::not_found()
                    } else {
                        Reply::json(json!([]))
                    };
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                matches!(s.channels_state(), LoadState::Failed(_)) && s.is_loaded()
            })
        });
        // The remembered team's available navigation survives failed discovery.
        session.read_with(cx, |s, _| assert!(s.channel(&"town".into()).is_some()));
        fail.store(false, Ordering::SeqCst);
        session.update(cx, |s, cx| s.retry_channels(cx));
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                s.teams_loaded() && s.channels_state() == &LoadState::Loaded
            })
        });
        session.read_with(cx, |s, _| assert_eq!(s.channels().count(), 0));
    }

    #[gpui_kit::test]
    fn failed_channels_offer_retry_and_recover(cx: &mut TestAppContext) {
        let fail = Arc::new(AtomicBool::new(true));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let fail = fail.clone();
            move |request| {
                if request.method() == "GET"
                    && request.path() == "/api/v4/users/me/teams/team/channels"
                    && fail.load(Ordering::SeqCst)
                {
                    return Reply::not_found().status(503);
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| {
                matches!(s.channels_state(), LoadState::Failed(_))
            })
        });
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| crate::sidebar::SidebarView::new(session.clone(), cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("retry-conversations").label(), Some("Retry"));
            fail.store(false, Ordering::SeqCst);
            window.click("retry-conversations", cx);
            assert_eq!(session.read(cx).channels_state(), &LoadState::Loading);
        })
        .unwrap();
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.channels_state() == &LoadState::Loaded)
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("retry-conversations").is_none());
            assert!(session.read(cx).channel(&"town".into()).is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn older_failure_keeps_messages_and_retry_recovers(cx: &mut TestAppContext) {
        let fail = Arc::new(AtomicBool::new(true));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let fail = fail.clone();
            move |request| {
                if request.method() == "GET"
                    && request.path().starts_with("/api/v4/channels/town/posts?")
                    && request.path().contains("before=")
                {
                    return if fail.load(Ordering::SeqCst) {
                        Reply::not_found()
                    } else {
                        Reply::json(json!({"order": [], "posts": {}}))
                    };
                }
                route(request)
            }
        });
        let session = start(&server, cx);
        let kind = TimelineKind::Channel("town".into());
        let timeline = session.update(cx, |s, cx| s.timeline(kind.clone(), cx));
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| t.state() == &LoadState::Loaded)
        });
        timeline.update(cx, |t, cx| t.load_older(cx));
        wait_until(cx, |cx| timeline.read_with(cx, |t, _| t.older_failed()));
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| {
                    crate::timeline_view::TimelineView::new(kind, session.clone(), None, window, cx)
                })
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("retry-older-messages").label(), Some("Retry"));
            assert_eq!(timeline.read(cx).entries().len(), 60);
            fail.store(false, Ordering::SeqCst);
            window.click("retry-older-messages", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            timeline.read_with(cx, |t, _| !t.is_loading_older())
        });
        timeline.read_with(cx, |t, _| {
            assert!(!t.older_failed());
            assert_eq!(t.entries().len(), 60);
            assert!(!t.has_older());
        });
    }
}
