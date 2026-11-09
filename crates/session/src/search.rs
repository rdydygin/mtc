//! Message search within one conversation.

use anyhow::{Result, anyhow};
use gpui_kit::{Context, Task};
use mattermost::{ChannelId, PostSearch, UserId};

use crate::Session;

/// Results requested per page.
pub const SEARCH_PAGE_SIZE: usize = 30;

impl Session {
    /// Search the messages of `channel`, replies included, newest first.
    /// `query` is what the user typed (see [`search_terms`]); `page` counts
    /// from 0. The results' authors are looked up as they arrive.
    pub fn search_channel(
        &mut self,
        channel: &ChannelId,
        query: &str,
        page: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<PostSearch>> {
        let Some(entry) = self.channel(channel) else {
            return Task::ready(Err(anyhow!("This conversation is no longer available.")));
        };
        // Direct and group messages belong to no team, but the search
        // endpoint needs one; any of the user's teams finds them.
        let Some(team) = self.team().map(|t| t.id.clone()) else {
            return Task::ready(Err(anyhow!("Search needs a team to search in.")));
        };
        let terms = search_terms(query, &entry.channel().name);
        let client = self.client().clone();
        let channel = channel.clone();
        cx.spawn(async move |this, cx| {
            let mut found = client
                .search_posts(&team, &terms, page, SEARCH_PAGE_SIZE)
                .await?;
            // `in:` narrows the search on the server; this keeps one that
            // couldn't resolve the channel from listing other conversations.
            found.posts.retain(|post| post.channel_id == channel);
            let authors: Vec<UserId> = found.posts.iter().map(|p| p.user_id.clone()).collect();
            this.update(cx, |this, cx| {
                this.directory().update(cx, |d, cx| d.ensure(authors, cx));
            })?;
            Ok(found)
        })
    }
}

/// The server terms for `query` typed in the channel named `channel_name`:
/// the query limited to that channel with `in:`, and its last word made a
/// prefix (`deplo` → `deplo*`) so results keep up with typing.
///
/// The last word stays as typed when it is a modifier (`from:alice`), an
/// exclusion (`-draft`), inside an unclosed quote, already a prefix, or a
/// single character, which would match too much to be useful.
pub fn search_terms(query: &str, channel_name: &str) -> String {
    let query = query.trim();
    let open_quote = query.matches('"').count() % 2 == 1;
    let widen = !open_quote
        && query.split_whitespace().last().is_some_and(|word| {
            word.chars().count() > 1 && word.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
    let star = if widen { "*" } else { "" };
    format!("{query}{star} in:{channel_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_the_last_plain_word() {
        assert_eq!(search_terms("deplo", "town"), "deplo* in:town");
        assert_eq!(
            search_terms("  release notes ", "town"),
            "release notes* in:town"
        );
        assert_eq!(search_terms("привет", "a__b"), "привет* in:a__b");
    }

    #[test]
    fn keeps_modifiers_quotes_and_short_words_as_typed() {
        assert_eq!(search_terms("from:alice", "town"), "from:alice in:town");
        assert_eq!(search_terms("ship -draft", "town"), "ship -draft in:town");
        assert_eq!(
            search_terms("\"deploy fail", "town"),
            "\"deploy fail in:town"
        );
        assert_eq!(
            search_terms("\"deploy failed\"", "town"),
            "\"deploy failed\" in:town"
        );
        assert_eq!(search_terms("deploy*", "town"), "deploy* in:town");
        assert_eq!(search_terms("a", "town"), "a in:town");
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
            testing::{Network, Reply, SlowServer},
        };
        use serde_json::json;

        use crate::Session;

        /// A team with one channel, where a search also turns up a post
        /// from another conversation, as a server that ignores `in:` would.
        fn server() -> SlowServer {
            SlowServer::start(Network::new(Duration::ZERO), |request| {
                match request.path().split('?').next().unwrap_or_default() {
                    "/api/v4/users/me/teams" => {
                        Reply::json(json!([{ "id": "team", "name": "team" }]))
                    }
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
                    "/api/v4/users/ids" => Reply::json(json!([{ "id": "ann", "username": "ann" }])),
                    "/api/v4/users/me/teams/team/channels/members"
                    | "/api/v4/users/me/preferences/favorite_channel"
                    | "/api/v4/emoji"
                    | "/api/v4/users/status/ids" => Reply::json(json!([])),
                    _ => Reply::not_found(),
                }
            })
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
            let task = session.update(cx, |s, cx| s.search_channel(&"town".into(), "hel", 0, cx));
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
        fn a_channel_that_is_gone_fails_without_asking_the_server(cx: &mut TestAppContext) {
            let server = server();
            cx.executor().allow_parking();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let session = cx.new(|cx| Session::new(client, User::default(), None, cx));
            let task = session.update(cx, |s, cx| s.search_channel(&"gone".into(), "x", 0, cx));
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
