//! ⌘F: find messages in the open conversation.
//!
//! The panel sits beside the conversation, where a thread opens. Results
//! follow the typing. Choosing one scrolls the conversation to it, or opens
//! its thread for a reply, and the results stay for the next choice.

use std::{collections::HashMap, ops::Range, sync::Arc, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledText, Subscription, Task, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, Post, PostId, PostSearch};
use session::{CustomEmojis, Session};

use crate::{http::avatar_url, time};

/// How long typing pauses before the search runs.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// A match further into a message than this many bytes is shown from a
/// little before it, so it isn't cut off below the visible lines.
const EXCERPT_AFTER: usize = 120;

pub enum SearchEvent {
    /// Show this post where it was written.
    Open(Arc<Post>),
    Close,
}

enum Status {
    /// Nothing typed yet.
    Idle,
    Searching,
    Done,
    Failed(SharedString),
}

pub struct SearchView {
    session: Entity<Session>,
    channel: ChannelId,
    input: Entity<InputState>,
    /// The query the results answer.
    query: String,
    results: Vec<Arc<Post>>,
    /// The words that matched in each result, as the server reports them.
    matches: HashMap<PostId, Vec<String>>,
    status: Status,
    /// The page to ask for next, while the server has more.
    next_page: Option<usize>,
    loading_more: bool,
    selected: Option<usize>,
    scroll: ScrollHandle,
    _search: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchEvent> for SearchView {}

impl SearchView {
    pub fn new(
        session: Entity<Session>,
        channel: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = scope_label(&session, &channel, cx)
            .map(|scope| format!("Search {scope}"))
            .unwrap_or_else(|| "Search messages".into());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let directory = session.read(cx).directory().clone();
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                InputEvent::Change => this.search(Some(DEBOUNCE), cx),
                InputEvent::PressEnter { .. } => match this.selected {
                    Some(ix) => this.open(ix, cx),
                    None => this.search(None, cx),
                },
                _ => {}
            }),
            // Authors' names and pictures arrive after the results.
            cx.observe(&directory, |_, _, cx| cx.notify()),
        ];
        Self {
            session,
            channel,
            input,
            query: String::new(),
            results: Vec::new(),
            matches: HashMap::new(),
            status: Status::Idle,
            next_page: None,
            loading_more: false,
            selected: None,
            scroll: ScrollHandle::new(),
            _search: None,
            _subscriptions: subscriptions,
        }
    }

    /// The conversation searched.
    pub fn channel(&self) -> &ChannelId {
        &self.channel
    }

    /// Put the cursor in the search field with its text selected, so typing
    /// starts a new search.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.focus_handle(cx).focus(window, cx);
        self.input
            .update(cx, |input, cx| input.select_all(window, cx));
    }

    /// Search for what's typed, after `delay` so a burst of typing makes
    /// one request. A newer search replaces one still waiting or running.
    fn search(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim().to_string();
        let failed = matches!(self.status, Status::Failed(_));
        if !query.is_empty() && query == self.query && !failed && delay.is_some() {
            // Typed back to what the results answer: drop the search for
            // what was typed in between, but not a page still loading.
            if !self.loading_more {
                self._search = None;
            }
            self.status = Status::Done;
            cx.notify();
            return;
        }
        if query.is_empty() {
            self._search = None;
            self.query.clear();
            self.results.clear();
            self.matches.clear();
            self.next_page = None;
            self.loading_more = false;
            self.selected = None;
            self.status = Status::Idle;
            cx.notify();
            return;
        }
        let channel = self.channel.clone();
        self._search = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.status = Status::Searching;
                this.loading_more = false;
                cx.notify();
                this.session
                    .update(cx, |s, cx| s.search_channel(&channel, &query, 0, cx))
            }) else {
                return;
            };
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.finish(query, 0, result, cx));
        }));
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.next_page else {
            return;
        };
        if self.loading_more {
            return;
        }
        self.loading_more = true;
        cx.notify();
        let query = self.query.clone();
        let task = self.session.update(cx, |s, cx| {
            s.search_channel(&self.channel, &query, page, cx)
        });
        self._search = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.finish(query, page, result, cx));
        }));
    }

    fn finish(
        &mut self,
        query: String,
        page: usize,
        result: anyhow::Result<PostSearch>,
        cx: &mut Context<Self>,
    ) {
        self.loading_more = false;
        self.query = query;
        match result {
            Ok(found) => {
                if page == 0 {
                    self.results.clear();
                    self.matches.clear();
                    self.selected = None;
                    self.scroll.scroll_to_item(0);
                }
                for post in found.posts {
                    if !self.results.iter().any(|r| r.id == post.id) {
                        self.results.push(Arc::new(post));
                    }
                }
                self.matches.extend(found.matches);
                self.next_page = found.has_next.then_some(page + 1);
                self.status = Status::Done;
            }
            // A later page that fails leaves the results there, and the
            // button to try it again.
            Err(_) if page > 0 => {}
            Err(err) => {
                self.status = Status::Failed(format!("Couldn’t search. {err:#}").into());
            }
        }
        cx.notify();
    }

    fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(post) = self.results.get(ix).cloned() else {
            return;
        };
        self.selected = Some(ix);
        cx.emit(SearchEvent::Open(post));
        cx.notify();
    }

    fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() as isize - 1;
        let ix = match self.selected {
            Some(ix) => (ix as isize + delta).clamp(0, last),
            None if delta > 0 => 0,
            None => last,
        } as usize;
        self.selected = Some(ix);
        self.scroll.scroll_to_item(ix);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.select(1, cx),
            "up" => self.select(-1, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ----- Rendering --------------------------------------------------------

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let searching = matches!(self.status, Status::Searching) && !self.results.is_empty();
        h_flex()
            .flex_none()
            .h_16()
            .px_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Input::new(&self.input)
                    .flex_1()
                    .aria_label("Search messages")
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    // The previous results stay while new ones load.
                    .when(searching, |this| this.suffix(Spinner::new().xsmall()))
                    .cleanable(true),
            )
            .child(
                Button::new("close-search")
                    .small()
                    .ghost()
                    .icon(IconName::X)
                    .accessibility_label("Close search")
                    .tooltip("Close search")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Close))),
            )
    }

    fn render_body(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let message = |text: SharedString| {
            v_flex()
                .flex_1()
                .px_6()
                .py_8()
                .gap_3()
                .items_center()
                .text_sm()
                .text_center()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        match &self.status {
            Status::Idle => message(
                "Use quotes for an exact phrase, from:username for someone’s messages, \
                 and -word to leave a word out."
                    .into(),
            )
            .into_any_element(),
            Status::Searching if self.results.is_empty() => message(SharedString::default())
                .child(Spinner::new())
                .into_any_element(),
            Status::Failed(error) => message(error.clone())
                .child(
                    Button::new("retry-search")
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.search(None, cx))),
                )
                .into_any_element(),
            _ if self.results.is_empty() => {
                message(format!("No messages match “{}”.", self.query).into()).into_any_element()
            }
            _ => self.render_results(cx),
        }
    }

    fn render_results(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let count = self.results.len();
        let more = if self.next_page.is_some() { "+" } else { "" };
        let summary = if count == 1 && more.is_empty() {
            "1 message".to_string()
        } else {
            format!("{count}{more} messages")
        };
        let words = query_words(&self.query);
        let rows: Vec<AnyElement> = self
            .results
            .iter()
            .enumerate()
            .map(|(ix, post)| {
                let terms = self.matches.get(&post.id).unwrap_or(&words);
                self.render_result(ix, post, terms, cx)
            })
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(summary),
            )
            .child(
                v_flex()
                    .id("search-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    .when(self.next_page.is_some(), |this| {
                        this.child(
                            div().flex_none().p_3().child(
                                Button::new("more-results")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .label("Show more results")
                                    .loading(self.loading_more)
                                    .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_result(
        &self,
        ix: usize,
        post: &Arc<Post>,
        terms: &[String],
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let session = self.session.read(cx);
        let author = session.directory().read(cx).user(&post.user_id).cloned();
        let name: SharedString = match (post.override_username(), &author) {
            (Some(name), _) => name.to_string().into(),
            (None, Some(user)) => user.display_name().into(),
            (None, None) => "…".into(),
        };
        let mut avatar = Avatar::new().name(name.clone()).xsmall();
        if let Some(user) = &author {
            avatar = avatar.src(avatar_url(
                session.client(),
                user.id.as_str(),
                user.last_picture_update,
            ));
        }
        let when = format!(
            "{} at {}",
            time::day_label(post.create_at),
            time::clock(post.create_at)
        );
        let text = session::emojify(&post.message, &CustomEmojis::default());
        let (text, ranges) = excerpt(&text, terms);
        let marked = HighlightStyle {
            background_color: Some(theme.warning.opacity(0.3)),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let files = post
            .metadata
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let selected = self.selected == Some(ix);
        let id: ElementId = post.id.as_arc().clone().into();
        v_flex()
            .id(id)
            .flex_none()
            .gap_1()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border.opacity(0.7))
            .hover(|style| style.bg(theme.accent.opacity(0.5)))
            .when(selected, |this| this.bg(theme.accent))
            .on_click(cx.listener(move |this, _, _, cx| this.open(ix, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(div().flex_none().child(avatar))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_semibold()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(when),
                    )
                    .when(post.is_reply(), |this| {
                        this.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("· in a thread"),
                        )
                    }),
            )
            .when(!text.is_empty(), |this| {
                this.child(
                    div().text_sm().line_clamp(4).child(
                        StyledText::new(text)
                            .with_highlights(ranges.into_iter().map(|range| (range, marked))),
                    ),
                )
            })
            .when(!files.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(IconName::Paperclip).xsmall())
                        .child(div().min_w_0().truncate().child(files)),
                )
            })
            .into_any_element()
    }
}

/// How the search names its conversation: `#general`, or a person's or
/// group's name.
fn scope_label(session: &Entity<Session>, channel: &ChannelId, cx: &App) -> Option<String> {
    let session = session.read(cx);
    let entry = session.channel(channel)?;
    let label = session.channel_label(entry, cx);
    Some(if entry.kind().is_direct_or_group() {
        format!("messages with {label}")
    } else {
        format!("#{label}")
    })
}

/// The words of `query` worth marking in results when the server doesn't
/// say what matched: no modifiers, exclusions, quotes, or wildcards.
fn query_words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|word| !word.starts_with('-') && !word.contains(':'))
        .map(|word| {
            word.trim_matches(|c: char| c == '"' || c == '*')
                .to_string()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Where `terms` start words in `text`, ignoring case, as sorted byte
/// ranges that don't overlap.
fn find_terms(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut previous: Option<char> = None;
    for (start, c) in text.char_indices() {
        let word_start = previous.is_none_or(|p| !p.is_alphanumeric());
        previous = Some(c);
        if !word_start || !c.is_alphanumeric() {
            continue;
        }
        let longest = terms
            .iter()
            .filter_map(|term| starts_with_ignoring_case(&text[start..], term))
            .max();
        if let Some(len) = longest {
            ranges.push(start..start + len);
        }
    }
    // Drop matches inside an earlier, longer one.
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start < last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// The byte length of `term` at the start of `text`, comparing letters
/// without regard to case.
fn starts_with_ignoring_case(text: &str, term: &str) -> Option<usize> {
    if term.is_empty() {
        return None;
    }
    let mut len = 0;
    let mut chars = text.chars();
    for expected in term.chars() {
        let c = chars.next()?;
        if !c.to_lowercase().eq(expected.to_lowercase()) {
            return None;
        }
        len += c.len_utf8();
    }
    Some(len)
}

/// `text` with `terms` marked. When the first match is far in, the text
/// starts shortly before it, after an ellipsis.
fn excerpt(text: &str, terms: &[String]) -> (SharedString, Vec<Range<usize>>) {
    let ranges = find_terms(text, terms);
    let Some(first) = ranges
        .first()
        .map(|r| r.start)
        .filter(|&at| at > EXCERPT_AFTER)
    else {
        return (text.to_string().into(), ranges);
    };
    // Start at the beginning of a word a few dozen bytes before the match.
    let mut cut = first.saturating_sub(EXCERPT_AFTER / 2);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    if let Some(space) = text[cut..first].find(char::is_whitespace) {
        cut += space + 1;
    }
    let ellipsis = "…";
    let shift = |at: usize| at - cut + ellipsis.len();
    let ranges = ranges
        .into_iter()
        .map(|r| shift(r.start)..shift(r.end))
        .collect();
    (format!("{ellipsis}{}", &text[cut..]).into(), ranges)
}

impl Focusable for SearchView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for SearchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("search-view")
            .size_full()
            .min_w_0()
            .capture_key_down(cx.listener(Self::on_key_down))
            .child(self.render_header(cx))
            .child(self.render_body(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, TestAppContext, WindowBounds, WindowOptions, point, px, size};
    use mattermost::{
        Client, TeamId, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::*;

    fn route(request: &Request) -> Reply {
        match request.path().split('?').next().unwrap_or_default() {
            "/api/v4/users/me/teams" => Reply::json(json!([{ "id": "team", "name": "team" }])),
            "/api/v4/users/me/teams/team/channels" => Reply::json(json!([
                { "id": "town", "team_id": "team", "type": "O", "name": "town-square",
                  "display_name": "Town Square" },
            ])),
            "/api/v4/teams/team/posts/search" => Reply::json(json!({
                "order": ["new", "old"],
                "posts": {
                    "new": { "id": "new", "channel_id": "town", "user_id": "ann",
                             "message": "hello again", "create_at": 2 },
                    "old": { "id": "old", "channel_id": "town", "user_id": "ann",
                             "message": "hello", "create_at": 1 },
                },
                "matches": { "new": ["hello"], "old": ["hello"] },
            })),
            "/api/v4/users/me/teams/team/channels/members"
            | "/api/v4/users/me/preferences/favorite_channel"
            | "/api/v4/emoji"
            | "/api/v4/users/ids"
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
    fn typing_searches_and_the_keyboard_opens_a_result(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let session = cx.new(|cx| Session::new(client, me, Some(TeamId::from("team")), cx));
        wait_until(cx, |cx| session.read_with(cx, |s, _| s.is_loaded()));
        let (window, view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(480.), px(640.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| SearchView::new(session, "town".into(), window, cx))
            })
            .unwrap()
        });
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = opened.clone();
        cx.update(|cx| {
            cx.subscribe(&view, move |_, event: &SearchEvent, _| {
                if let SearchEvent::Open(post) = event {
                    sink.borrow_mut().push(post.id.to_string());
                }
            })
            .detach()
        });
        let searches = |server: &SlowServer| {
            server
                .requests()
                .iter()
                .filter(|r| r.path().ends_with("/posts/search"))
                .count()
        };

        // Each keystroke is its own turn of the event loop, as when typing,
        // but the pause between them is shorter than the debounce.
        let type_keys = |cx: &mut TestAppContext, keys: fn(&mut Window, &mut App)| {
            cx.update_window(window, |_, window, cx| keys(window, cx))
                .unwrap();
            cx.run_until_parked();
        };
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| v.focus(window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        type_keys(cx, |window, cx| window.input("hel", cx));
        type_keys(cx, |window, cx| window.input("lo", cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
        // The pause after typing made one request, for the whole word.
        assert_eq!(searches(&server), 1);
        view.read_with(cx, |v, _| {
            assert_eq!(v.query, "hello");
            assert_eq!(v.results[0].id.as_str(), "new");
        });

        // Typing back to the same words cancels the search for the text
        // in between.
        type_keys(cx, |window, cx| window.input("!", cx));
        type_keys(cx, |window, cx| window.press("backspace", cx));
        for _ in 0..10 {
            cx.executor().advance_clock(DEBOUNCE);
            std::thread::sleep(Duration::from_millis(5));
            cx.run_until_parked();
        }
        assert_eq!(searches(&server), 1);
        view.read_with(cx, |v, _| assert!(matches!(v.status, Status::Done)));

        cx.update_window(window, |_, window, cx| {
            window.press("down", cx);
            window.press("down", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // Down stops at the last result, and Enter opens the chosen one.
        assert_eq!(*opened.borrow(), ["old"]);
        assert_eq!(searches(&server), 1);
    }

    fn words(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn marks_terms_at_word_starts_ignoring_case() {
        let text = "Deploy failed; redeploy after the DEPLOYMENT fix";
        let ranges = find_terms(text, &words(&["deploy"]));
        let marked: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(marked, ["Deploy", "DEPLOY"]);
    }

    #[test]
    fn marks_non_latin_text_on_char_boundaries() {
        let text = "Привет, мир! ПРИВЕТСТВИЕ";
        let ranges = find_terms(text, &words(&["привет"]));
        let marked: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(marked, ["Привет", "ПРИВЕТ"]);
    }

    #[test]
    fn query_words_leave_out_modifiers_and_exclusions() {
        assert_eq!(
            query_words("\"release notes\" from:alice -draft deplo*"),
            ["release", "notes", "deplo"]
        );
    }

    #[test]
    fn a_late_match_is_shown_from_shortly_before_it() {
        let text = format!("{} the needle is here", "word ".repeat(60));
        let (shown, ranges) = excerpt(&text, &words(&["needle"]));
        assert!(shown.starts_with('…'));
        assert!(shown.len() < text.len());
        assert_eq!(&shown[ranges[0].clone()], "needle");
    }

    #[test]
    fn an_early_match_keeps_the_whole_text() {
        let (shown, ranges) = excerpt("the needle", &words(&["needle"]));
        assert_eq!(shown.as_ref(), "the needle");
        assert_eq!(ranges, vec![4..10]);
    }
}
