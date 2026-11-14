//! Message search in a conversation or the current team.

use anyhow::{Result, anyhow};
use gpui_kit::{Context, Task};
use mattermost::{ChannelId, PostSearch, UserId};

use crate::Session;

/// Results requested per page.
pub const SEARCH_PAGE_SIZE: usize = 30;

/// Where to search for messages in the current team.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Conversation(ChannelId),
    Team,
}

impl Session {
    /// Search messages in `scope`, replies included, newest first.
    /// `query` is what the user typed (see [`search_terms`]); `page` counts
    /// from 0. The results' authors are looked up as they arrive.
    pub fn search_messages(
        &mut self,
        scope: &SearchScope,
        query: &str,
        page: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<PostSearch>> {
        let channel_name = match scope {
            SearchScope::Conversation(channel) => match self.channel(channel) {
                Some(entry) => Some(entry.channel().name.as_str()),
                None => {
                    return Task::ready(Err(anyhow!("This conversation is no longer available.")));
                }
            },
            SearchScope::Team => None,
        };
        // Direct and group messages belong to no team, but the search
        // endpoint needs one; any of the user's teams finds them.
        let Some(team) = self.team().map(|t| t.id.clone()) else {
            return Task::ready(Err(anyhow!("Search needs a team to search in.")));
        };
        let terms = search_terms(query, channel_name);
        let client = self.client().clone();
        let scope = scope.clone();
        cx.spawn(async move |this, cx| {
            let mut found = client
                .search_posts(&team, &terms, page, SEARCH_PAGE_SIZE)
                .await?;
            // `in:` narrows the search on the server; this keeps one that
            // couldn't resolve the channel from listing other conversations.
            if let SearchScope::Conversation(channel) = &scope {
                found.posts.retain(|post| &post.channel_id == channel);
            }
            // Team results can name a channel that wasn't in the startup list.
            let channels = this.update(cx, |this, cx| -> Result<Vec<Task<Result<()>>>> {
                if this.team().map(|current| &current.id) != Some(&team) {
                    return Err(anyhow!("The current team changed."));
                }
                let authors: Vec<UserId> = found.posts.iter().map(|p| p.user_id.clone()).collect();
                this.directory().update(cx, |d, cx| d.ensure(authors, cx));
                let ids: std::collections::HashSet<_> = found
                    .posts
                    .iter()
                    .map(|post| post.channel_id.clone())
                    .collect();
                Ok(ids
                    .into_iter()
                    .map(|id| this.ensure_channel(&id, cx))
                    .collect())
            })??;
            for result in futures::future::join_all(channels).await {
                if let Err(err) = result {
                    log::warn!("loading search channel: {err:#}");
                }
            }
            this.update(cx, |this, _| -> Result<()> {
                if this.team().map(|current| &current.id) != Some(&team) {
                    return Err(anyhow!("The current team changed."));
                }
                found
                    .posts
                    .retain(|post| this.channel(&post.channel_id).is_some());
                found
                    .matches
                    .retain(|id, _| found.posts.iter().any(|post| &post.id == id));
                Ok(())
            })??;
            Ok(found)
        })
    }
}

/// The server terms for `query`, limited to `channel_name` when given,
/// with its last word made a prefix (`deplo` → `deplo*`) so results keep up with typing.
///
/// The last word stays as typed when it is a modifier (`from:alice`), an
/// exclusion (`-draft`), inside an unclosed quote, already a prefix, or a
/// single character, which would match too much to be useful.
pub fn search_terms(query: &str, channel_name: Option<&str>) -> String {
    let query = query.trim();
    let open_quote = query.matches('"').count() % 2 == 1;
    let widen = !open_quote
        && query.split_whitespace().last().is_some_and(|word| {
            word.chars().count() > 1 && word.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
    let star = if widen { "*" } else { "" };
    let channel = channel_name
        .map(|name| format!(" in:{name}"))
        .unwrap_or_default();
    format!("{query}{star}{channel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_the_last_plain_word() {
        assert_eq!(search_terms("deplo", Some("town")), "deplo* in:town");
        assert_eq!(
            search_terms("  release notes ", Some("town")),
            "release notes* in:town"
        );
        assert_eq!(search_terms("привет", Some("a__b")), "привет* in:a__b");
    }

    #[test]
    fn keeps_modifiers_quotes_and_short_words_as_typed() {
        assert_eq!(
            search_terms("from:alice", Some("town")),
            "from:alice in:town"
        );
        assert_eq!(
            search_terms("ship -draft", Some("town")),
            "ship -draft in:town"
        );
        assert_eq!(
            search_terms("\"deploy fail", Some("town")),
            "\"deploy fail in:town"
        );
        assert_eq!(
            search_terms("\"deploy failed\"", Some("town")),
            "\"deploy failed\" in:town"
        );
        assert_eq!(search_terms("deploy*", Some("town")), "deploy* in:town");
        assert_eq!(search_terms("a", Some("town")), "a in:town");
    }

    #[test]
    fn team_terms_keep_explicit_filters_without_adding_a_channel() {
        assert_eq!(search_terms("deplo", None), "deplo*");
        assert_eq!(search_terms("hello in:other", None), "hello in:other");
        assert_eq!(search_terms("from:ann -draft", None), "from:ann -draft");
    }

    mod with_server {
        use std::{
            cell::RefCell,
            rc::Rc,
            time::{Duration, Instant},
        };

        use gpui_kit::{AppContext as _, TestAppContext};
        use mattermost::{
            Client, TeamId, User,
            testing::{Network, Reply, Request, SlowServer},
        };
        use serde_json::json;

        use crate::{SearchScope, Session};

        /// A team with one channel, where a search also turns up a post
        /// from another conversation, as a server that ignores `in:` would.
        fn server() -> SlowServer {
            SlowServer::start(Network::new(Duration::ZERO), route)
        }

        fn route(request: &Request) -> Reply {
            match request.path().split('?').next().unwrap_or_default() {
                "/api/v4/users/me/teams" => Reply::json(json!([{ "id": "team", "name": "team" }])),
                "/api/v4/users/me/teams/team/channels" => Reply::json(json!([
                    { "id": "town", "team_id": "team", "type": "O", "name": "town-square" },
                ])),
                "/api/v4/teams/team/posts/search" => Reply::json(json!({
                    "order": ["here", "elsewhere"],
                    "posts": {
                        "here": { "id": "here", "channel_id": "town", "user_id": "ann",
                                  "message": "hello", "create_at": 2 },
                        "elsewhere": { "id": "elsewhere", "channel_id": "other",
                                       "user_id": "bob", "message": "hello", "create_at": 1 },
                    },
                })),
                "/api/v4/channels/other" => Reply::json(json!({
                    "id": "other", "team_id": "team", "name": "other", "type": "O", "last_post_at": 20
                })),
                "/api/v4/channels/other/members/me" => Reply::json(json!({
                    "channel_id": "other", "user_id": "me", "last_viewed_at": 10,
                    "mention_count": 2, "notify_props": { "mark_unread": "mention" }
                })),
                "/api/v4/users/ids" => Reply::json(json!([{ "id": "ann", "username": "ann" }])),
                "/api/v4/users/me/teams/team/channels/members"
                | "/api/v4/users/me/preferences/favorite_channel"
                | "/api/v4/emoji"
                | "/api/v4/users/status/ids" => Reply::json(json!([])),
                _ => Reply::not_found(),
            }
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

        #[gpui_kit::test]
        fn results_keep_to_the_channel_and_bring_their_authors(cx: &mut TestAppContext) {
            let server = server();
            cx.executor().allow_parking();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let mut me = User::default();
            me.id = "me".into();
            let session = cx.new(|cx| Session::new(client, me, Some(TeamId::from("team")), cx));
            wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));

            let found = Rc::new(RefCell::new(None));
            let task = session.update(cx, |s, cx| {
                s.search_messages(
                    &super::SearchScope::Conversation("town".into()),
                    "hel",
                    0,
                    cx,
                )
            });
            let slot = found.clone();
            cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
                .detach();
            wait_until(cx, |_| found.borrow().is_some());
            let found = found.borrow_mut().take().unwrap().unwrap();
            let ids: Vec<&str> = found.posts.iter().map(|p| p.id.as_str()).collect();
            assert_eq!(ids, ["here"]);

            wait_until(cx, |cx| {
                session.read_with(cx, |s, cx| {
                    s.directory().read(cx).user(&"ann".into()).is_some()
                })
            });
        }

        #[gpui_kit::test]
        fn team_results_include_other_channels_and_load_their_names(cx: &mut TestAppContext) {
            let server = server();
            cx.executor().allow_parking();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let session =
                cx.new(|cx| Session::new(client, User::default(), Some("team".into()), cx));
            wait_until(cx, |cx| {
                session.read_with(cx, |s, _| s.team().is_some() && s.is_loaded())
            });
            let task = session.update(cx, |s, cx| {
                s.search_messages(&SearchScope::Team, "hel", 0, cx)
            });
            let found = Rc::new(RefCell::new(None));
            let slot = found.clone();
            cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
                .detach();
            wait_until(cx, |_| found.borrow().is_some());
            let found = found.borrow_mut().take().unwrap().unwrap();
            assert_eq!(
                found
                    .posts
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>(),
                ["here", "elsewhere"]
            );
            session.read_with(cx, |s, _| {
                let channel = s.channel(&"other".into()).unwrap();
                assert_eq!(channel.channel().name, "other");
                assert!(channel.is_muted());
                assert!(channel.is_unread());
                assert_eq!(channel.last_viewed_at(), 10);
                assert_eq!(channel.mention_count(), 2);
            });
        }

        #[gpui_kit::test]
        fn team_search_omits_hits_when_channel_or_membership_loading_fails(
            cx: &mut TestAppContext,
        ) {
            for (path, status) in [
                ("/api/v4/channels/other", 403),
                ("/api/v4/channels/other", 503),
                ("/api/v4/channels/other/members/me", 403),
                ("/api/v4/channels/other/members/me", 503),
            ] {
                let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
                    if request.path() == path {
                        Reply::json(json!({ "message": "Unavailable" })).status(status)
                    } else {
                        route(request)
                    }
                });
                cx.executor().allow_parking();
                let client = Client::new_unverified(&server.url(), "token").unwrap();
                let session =
                    cx.new(|cx| Session::new(client, User::default(), Some("team".into()), cx));
                wait_until(cx, |cx| {
                    session.read_with(cx, |s, _| s.team().is_some() && s.is_loaded())
                });
                let task = session.update(cx, |s, cx| {
                    s.search_messages(&SearchScope::Team, "hel", 0, cx)
                });
                let found = Rc::new(RefCell::new(None));
                let slot = found.clone();
                cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
                    .detach();
                wait_until(cx, |_| found.borrow().is_some());
                let found = found.borrow_mut().take().unwrap().unwrap();
                assert_eq!(
                    found
                        .posts
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>(),
                    ["here"],
                    "{path}: {status}"
                );
                session.read_with(cx, |s, _| assert!(s.channel(&"other".into()).is_none()));
            }
        }

        #[gpui_kit::test]
        fn team_search_without_a_team_fails_locally(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let server = server();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let session = cx.new(|cx| Session::new(client, User::default(), None, cx));
            let task = session.update(cx, |s, cx| {
                s.search_messages(&SearchScope::Team, "x", 0, cx)
            });
            let err = cx.foreground_executor().block_test(task).unwrap_err();
            assert_eq!(err.to_string(), "Search needs a team to search in.");
        }

        #[gpui_kit::test]
        fn a_channel_that_is_gone_fails_without_asking_the_server(cx: &mut TestAppContext) {
            let server = server();
            cx.executor().allow_parking();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let session = cx.new(|cx| Session::new(client, User::default(), None, cx));
            let task = session.update(cx, |s, cx| {
                s.search_messages(&super::SearchScope::Conversation("gone".into()), "x", 0, cx)
            });
            let err = cx.foreground_executor().block_test(task).unwrap_err();
            assert_eq!(err.to_string(), "This conversation is no longer available.");
            assert!(
                server
                    .requests()
                    .iter()
                    .all(|r| !r.path().contains("/posts/search"))
            );
        }
    }
}
