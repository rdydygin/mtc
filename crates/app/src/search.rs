//! Find messages in a conversation or the current team.
//!
//! The panel sits beside the conversation, where a thread opens. Results
//! follow the typing. Choosing one scrolls the conversation to it, or opens
//! its thread for a reply, and the results stay for the next choice.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
    time::Duration,
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
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
    ParentElement as _, Render, Resource, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, Post, PostId, PostSearch, TeamId};
use session::{CustomEmojis, SearchScope, Session};

use crate::{
    http::avatar_url,
    image_scope::{ImageStore, image_scope, release_replaced_avatars},
    time,
};

/// How long typing pauses before the search runs.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// A match further into a message than this many bytes is shown from a
/// little before it, so it isn't cut off below the visible lines.
const EXCERPT_AFTER: usize = 120;

mod collections;
pub use collections::Collection;

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
    collection: Option<Collection>,
    collection_pages: usize,
    collection_unavailable: bool,
    collection_focus: FocusHandle,
    collection_scroll: gpui_kit::ListState,
    session: Entity<Session>,
    team: Option<TeamId>,
    channel: Option<ChannelId>,
    scope: SearchScope,
    results_scope: Option<SearchScope>,
    /// Changes whenever input or scope invalidates an outstanding request.
    revision: u64,
    input: Entity<InputState>,
    /// The query the results answer.
    query: String,
    results: Vec<Arc<Post>>,
    result_text: HashMap<PostId, String>,
    /// The words that matched in each result, as the server reports them.
    matches: HashMap<PostId, Vec<String>>,
    status: Status,
    /// The page to ask for next, while the server has more.
    next_page: Option<usize>,
    loading_more: bool,
    more_failed: bool,
    selected: Option<usize>,
    scroll: ScrollHandle,
    /// The authors' pictures in the results. The view stays for the
    /// workspace's life, kept while hidden, so each new set of results
    /// frees the pictures the last one showed that it doesn't (see
    /// [`Self::release_pictures`]), a replaced profile picture frees its
    /// old version, and the rest go with the view.
    images: Entity<ImageStore>,
    _search: Option<Task<()>>,
    /// The next page loading. Kept apart from `_search` so asking for more
    /// of the old results never cancels a search for newer words.
    _more: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchEvent> for SearchView {}

impl SearchView {
    pub fn new(
        session: Entity<Session>,
        scope: SearchScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let channel = match &scope {
            SearchScope::Conversation(channel) => Some(channel.clone()),
            SearchScope::Team => session.read(cx).active_channel().cloned(),
        };
        let team = session.read(cx).team().map(|team| team.id.clone());
        let placeholder = search_placeholder(&session, &scope, cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let directory = session.read(cx).directory().clone();
        let images = ImageStore::new(cx);
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                InputEvent::Change => this.search(Some(DEBOUNCE), cx),
                InputEvent::PressEnter { .. } => match this.selected {
                    // A choice among results for other words searches the
                    // words typed now.
                    Some(ix) if this.answers_input(cx) => this.open(ix, cx),
                    _ => this.search(None, cx),
                },
                _ => {}
            }),
            // Authors' names and pictures arrive after the results.
            cx.observe(&directory, |_, _, cx| cx.notify()),
            release_replaced_avatars(&images, &session, cx),
        ];
        Self {
            collection: None,
            collection_pages: 0,
            collection_unavailable: false,
            collection_focus: cx.focus_handle().tab_stop(true),
            collection_scroll: gpui_kit::ListState::new(
                0,
                gpui_kit::ListAlignment::Top,
                gpui_kit::Pixels::ZERO,
            ),
            session,
            team,
            channel,
            scope,
            results_scope: None,
            revision: 0,
            input,
            query: String::new(),
            results: Vec::new(),
            result_text: HashMap::new(),
            matches: HashMap::new(),
            status: Status::Idle,
            next_page: None,
            loading_more: false,
            more_failed: false,
            selected: None,
            scroll: ScrollHandle::new(),
            images,
            _search: None,
            _more: None,
            _subscriptions: subscriptions,
        }
    }

    /// The team this search was opened in.
    pub fn team(&self) -> Option<&TeamId> {
        self.team.as_ref()
    }

    /// Where the messages are searched.
    pub fn scope(&self) -> &SearchScope {
        &self.scope
    }

    /// Keep the conversation choice pointed at the channel now on screen.
    pub fn set_conversation(&mut self, channel: ChannelId) {
        self.channel = Some(channel);
    }

    pub fn set_scope(&mut self, scope: SearchScope, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        let placeholder = search_placeholder(&self.session, &self.scope, cx);
        self.input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
        self.results_scope = None;
        self.results.clear();
        self.result_text.clear();
        self.matches.clear();
        self.selected = None;
        self.next_page = None;
        self._more = None;
        self.loading_more = false;
        self.release_pictures(cx);
        self.search(None, cx);
    }

    /// Whether the results answer what's typed now.
    fn answers_input(&self, cx: &App) -> bool {
        self.input.read(cx).value().trim() == self.query
            && self.results_scope.as_ref() == Some(&self.scope)
    }

    /// Put the cursor in the search field with its text selected, so typing
    /// starts a new search.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.collection.is_some() {
            self.collection_focus.focus(window, cx);
            return;
        }
        self.input.focus_handle(cx).focus(window, cx);
        self.input
            .update(cx, |input, cx| input.select_all(window, cx));
    }

    /// Search for what's typed, after `delay` so a burst of typing makes
    /// one request. A newer search replaces one still waiting or running.
    fn search(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        if self.collection.is_some() {
            self.load_collection(0, cx);
            return;
        }
        self.revision += 1;
        self._more = None;
        self.loading_more = false;
        let revision = self.revision;
        let query = self.input.read(cx).value().trim().to_string();
        let failed = matches!(self.status, Status::Failed(_));
        if !query.is_empty() && self.answers_input(cx) && !failed && delay.is_some() {
            // Typed back to what the results answer: drop the search for
            // what was typed in between.
            self._search = None;
            self.status = Status::Done;
            cx.notify();
            return;
        }
        if query.is_empty() {
            self._search = None;
            self._more = None;
            self.query.clear();
            self.results.clear();
            self.result_text.clear();
            self.matches.clear();
            self.next_page = None;
            self.loading_more = false;
            self.selected = None;
            self.status = Status::Idle;
            self.release_pictures(cx);
            cx.notify();
            return;
        }
        let scope = self.scope.clone();
        self._search = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.status = Status::Searching;
                this._more = None;
                this.loading_more = false;
                this.more_failed = false;
                cx.notify();
                this.session
                    .update(cx, |s, cx| s.search_messages(&scope, &query, 0, cx))
            }) else {
                return;
            };
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.finish(revision, scope, query, 0, result, cx)
            });
        }));
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.collection.is_some() {
            if !self.loading_more
                && !matches!(self.status, Status::Searching)
                && let Some(page) = self.next_page
            {
                self.load_collection(page, cx);
            }
            return;
        }
        let Some(page) = self.next_page else {
            return;
        };
        if self.loading_more || !self.answers_input(cx) {
            return;
        }
        self.loading_more = true;
        self.more_failed = false;
        cx.notify();
        let query = self.query.clone();
        let scope = self.scope.clone();
        let revision = self.revision;
        let task = self
            .session
            .update(cx, |s, cx| s.search_messages(&scope, &query, page, cx));
        self._more = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.finish(revision, scope, query, page, result, cx)
            });
        }));
    }

    fn finish(
        &mut self,
        revision: u64,
        scope: SearchScope,
        query: String,
        page: usize,
        result: anyhow::Result<PostSearch>,
        cx: &mut Context<Self>,
    ) {
        // A page of older words, answered after newer ones were searched.
        if revision != self.revision || scope != self.scope {
            return;
        }
        self.loading_more = false;
        self.query = query;
        self.results_scope = Some(scope);
        match result {
            Ok(found) => {
                if page == 0 {
                    self.results.clear();
                    self.result_text.clear();
                    self.matches.clear();
                    self.selected = None;
                    self.scroll.scroll_to_item(0);
                }
                for post in found.posts {
                    if !self.results.iter().any(|r| r.id == post.id) {
                        self.result_text.insert(
                            post.id.clone(),
                            session::emojify(&session::post_text(&post), &CustomEmojis::default()),
                        );
                        self.results.push(Arc::new(post));
                    }
                }
                self.matches.extend(found.matches);
                self.next_page = found.has_next.then_some(page + 1);
                self.status = Status::Done;
                if page == 0 {
                    self.release_pictures(cx);
                }
            }
            // A later page that fails leaves the results there, and the
            // button to try it again.
            Err(_) if page > 0 => self.more_failed = true,
            Err(err) => {
                log::warn!("searching messages: {err:#}");
                self.status = Status::Failed(
                    "Couldn’t search messages. Check your connection and try again.".into(),
                );
            }
        }
        cx.notify();
    }

    /// The picture of `post`'s author, once the directory knows them.
    fn author_picture(&self, post: &Post, cx: &App) -> Option<String> {
        let session = self.session.read(cx);
        let author = session.directory().read(cx).user(&post.user_id)?;
        Some(avatar_url(
            session.client(),
            author.id.as_str(),
            author.last_picture_update,
        ))
    }

    /// Free the pictures of results no longer shown. Authors of the results
    /// now shown keep theirs, so a new search doesn't redraw them.
    fn release_pictures(&mut self, cx: &mut Context<Self>) {
        let shown: HashSet<String> = self
            .results
            .iter()
            .filter_map(|post| self.author_picture(post, cx))
            .collect();
        self.images.update(cx, |images, cx| {
            images.retain(
                |picture| matches!(picture, Resource::Uri(uri) if shown.contains(uri.as_ref())),
                cx,
            )
        });
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
        if self.collection.is_some() {
            self.collection_scroll.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.collection.is_some() && !self.collection_focus.is_focused(window) {
            return;
        }
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.select(1, cx),
            "up" => self.select(-1, cx),
            "home" if self.collection.is_some() => self.select(-(self.results.len() as isize), cx),
            "end" if self.collection.is_some() => self.select(self.results.len() as isize, cx),
            "enter" if self.collection.is_some() => {
                if let Some(ix) = self.selected {
                    self.open(ix, cx);
                }
            }
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
            _ if self.results.is_empty() && self.next_page.is_none() => {
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
                    .when(self.more_failed, |this| {
                        this.child(crate::recovery::inline_failure(
                            "Couldn’t load more search results.",
                            Button::new("retry-search-results")
                                .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                            cx,
                        ))
                    })
                    .when(self.next_page.is_some() && !self.more_failed, |this| {
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
        let author = session.directory().read(cx).user(&post.user_id);
        let name: SharedString = match (post.override_username(), author) {
            (Some(name), _) => name.to_string().into(),
            (None, Some(user)) => user.display_name().into(),
            (None, None) => "…".into(),
        };
        let avatar = Avatar::new()
            .name(name.clone())
            .xsmall()
            .when_some(self.author_picture(post, cx), Avatar::src);
        let when = format!(
            "{} at {}",
            time::day_label(post.create_at),
            time::clock(post.create_at)
        );
        let text = self
            .result_text
            .get(&post.id)
            .map(String::as_str)
            .unwrap_or(&post.message);
        let (text, ranges) = excerpt(text, terms);
        let marked = HighlightStyle {
            background_color: Some(theme.accent),
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
        let row = v_flex()
            .id(id)
            .role(gpui_kit::Role::ListItem)
            .aria_label(format!(
                "{}: {}",
                name,
                self.result_text
                    .get(&post.id)
                    .map(String::as_str)
                    .unwrap_or(&post.message)
            ))
            .test_support()
            .when(self.collection.is_some(), |this| {
                this.h_40().overflow_hidden()
            })
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
            .when(self.results_scope == Some(SearchScope::Team), |this| {
                let label = scope_label(&self.session, &post.channel_id, cx)
                    .unwrap_or_else(|| format!("Channel {}", post.channel_id));
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
            })
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
            });
        // Each row draws inside the store: a collection's list lays rows out
        // while it prepaints, and their pictures must not reach the
        // app-wide store.
        image_scope(&self.images, row).into_any_element()
    }
}

fn search_placeholder(session: &Entity<Session>, scope: &SearchScope, cx: &App) -> String {
    match scope {
        SearchScope::Team => "Search all channels in this team".into(),
        SearchScope::Conversation(channel) => scope_label(session, channel, cx)
            .map(|label| format!("Search {label}"))
            .unwrap_or_else(|| "Search messages".into()),
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
    if let Some((space, ch)) = text[cut..first]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
    {
        cut += space + ch.len_utf8();
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
        if self.collection.is_some() {
            self.collection_focus.clone()
        } else {
            self.input.focus_handle(cx)
        }
    }
}

impl Render for SearchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.collection.is_some() {
            return self.render_collection(cx);
        }
        v_flex()
            .id("search-view")
            .size_full()
            .min_w_0()
            .capture_key_down(cx.listener(Self::on_key_down))
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .child(
                        Button::new("search-conversation")
                            .small()
                            .ghost()
                            .label("This conversation")
                            .selected(matches!(self.scope, SearchScope::Conversation(_)))
                            .disabled(self.channel.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(channel) = this.channel.clone() {
                                    this.set_scope(SearchScope::Conversation(channel), window, cx);
                                    this.input.focus_handle(cx).focus(window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("search-team")
                            .small()
                            .ghost()
                            .label("All channels")
                            .selected(self.scope == SearchScope::Team)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_scope(SearchScope::Team, window, cx);
                                this.input.focus_handle(cx).focus(window, cx);
                            })),
                    ),
            )
            .child(self.render_body(cx))
            .into_any_element()
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
    use gpui_kit::{
        AnyWindowHandle, Bounds, TestAppContext, WindowBounds, WindowOptions, point, px, size,
    };
    use mattermost::{
        Client, TeamId, User,
        testing::{Network, Reply, Request, SlowServer},
    };
    use serde_json::json;

    use super::*;

    fn open_collection(
        server: &SlowServer,
        collection: Collection,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<SearchView>) {
        let (window, view) = open_search(server, cx);
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| {
                *v = SearchView::new_collection(v.session.clone(), collection, window, cx);
                v.focus(window, cx);
            })
        })
        .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| matches!(v.status, Status::Done))
        });
        (window, view)
    }

    #[gpui_kit::test]
    fn attachment_search_results_prepare_text_on_arrival(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (request.method(), request.path()) {
                ("POST", "/api/v4/teams/team/posts/search") => Reply::json(
                    json!({"order":["bot"],"posts":{"bot":{"id":"bot","channel_id":"town","props":{"attachments":[{"fields":[{"title":"Build","value":":rocket:"}]}]}}}}),
                ),
                _ => route(request),
            }
        });
        let (handle, view) = open_search(&server, cx);
        cx.update_window(handle, |_, window, cx| window.input("Build", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 1));
        view.read_with(cx, |v, _| {
            assert_eq!(v.result_text[&PostId::from("bot")], "Build: 🚀")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("bot").label().unwrap().contains("Build: 🚀"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn non_active_channel_removal_prunes_saved_and_closes_pinned(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let removed = AtomicBool::new(false);
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                    {"id":"town", "team_id":"team", "name":"town-square", "type":"O"},
                    {"id":"other", "team_id":"team", "name":"other", "type":"O"}
                ])),
                ("GET", "/api/v4/users/me/posts/flagged" | "/api/v4/channels/other/pinned") => {
                    Reply::json(json!({"posts":{
                        "a":{"id":"a", "channel_id":"other", "create_at":3},
                        "b":{"id":"b", "channel_id":"town", "create_at":2},
                        "c":{"id":"c", "channel_id":"town", "create_at":1}
                    }}))
                }
                ("DELETE", "/api/v4/channels/other/members/me") => {
                    removed.store(true, Ordering::SeqCst);
                    Reply::json(json!({}))
                }
                ("GET", "/api/v4/channels/other/members/me") if removed.load(Ordering::SeqCst) => {
                    Reply::json(json!({})).status(403)
                }
                _ => route(request),
            }
        });
        let (window, saved) = open_collection(&server, Collection::Saved, cx);
        let session = saved.read_with(cx, |v, _| v.session.clone());
        let pinned = cx
            .update_window(window, |_, window, cx| {
                cx.new(|cx| {
                    SearchView::new_collection(
                        session.clone(),
                        Collection::Pinned("other".into()),
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        wait_until(cx, |cx| {
            pinned.read_with(cx, |v, _| matches!(v.status, Status::Done))
        });
        let closed = Rc::new(RefCell::new(0));
        let sink = closed.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&pinned, move |_, event, _| {
                if matches!(event, SearchEvent::Close) {
                    *sink.borrow_mut() += 1;
                }
            })
        });
        saved.update(cx, |v, _| v.selected = Some(1));
        session.update(cx, |s, cx| {
            s.open_channel(&"town".into(), cx);
            s.leave_channel(&"other".into(), cx);
        });
        wait_until(cx, |cx| saved.read_with(cx, |v, _| v.results.len() == 2));
        saved.read_with(cx, |v, _| {
            assert_eq!(v.results[v.selected.unwrap()].id.as_str(), "b")
        });
        assert_eq!(*closed.borrow(), 1);
        assert!(pinned.read_with(cx, |v, _| v.collection_unavailable && v.results.is_empty()));
    }

    #[gpui_kit::test]
    fn collection_events_refresh_loaded_depth_without_moving_selection_or_anchor(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let version = Arc::new(AtomicUsize::new(0));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let version = version.clone();
            move |request| {
                let current = version.load(Ordering::SeqCst);
                match (request.method(), request.path().split('?').next().unwrap_or_default()) {
                    ("GET", "/api/v4/users/me/preferences/flagged_post") => Reply::json(json!((0..120).map(|i| json!({"user_id":"me", "category":"flagged_post", "name":format!("p{i}"), "value":"true"})).collect::<Vec<_>>())),
                    ("PUT", "/api/v4/users/me/preferences") => { version.store(1, Ordering::SeqCst); Reply::json(json!({})) },
                    ("GET", "/api/v4/users/me/posts/flagged") => {
                        let mut rows: Vec<_> = (0..120).rev().map(|i| json!({"id":format!("p{i}"), "channel_id":"town", "create_at":i+1, "message":format!("version {current}")})).collect();
                        if current > 0 { rows.insert(0, json!({"id":"newer", "channel_id":"town", "create_at":200})); }
                        let page = if request.path().contains("page=1&") { 1 } else { 0 };
                        if current == 3 && page == 1 { return Reply::json(json!({})).status(500); }
                        let posts: serde_json::Map<_,_> = rows.into_iter().skip(page*60).take(60).map(|p| (p["id"].as_str().unwrap().to_string(), p)).collect();
                        Reply::json(json!({"posts":posts})).delay(if current > 0 && page == 1 { Duration::from_millis(80) } else { Duration::ZERO })
                    }
                    _ => route(request),
                }
            }
        });
        let (window, view) = open_collection(&server, Collection::Saved, cx);
        let session = view.read_with(cx, |v, _| v.session.clone());
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_saved(&"p0".into()))
        });
        view.update(cx, |v, cx| v.load_more(cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 120));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, _| {
                v.selected = Some(90);
                v.collection_scroll.scroll_to(gpui_kit::ListOffset {
                    item_ix: 80,
                    offset_in_item: px(3.),
                });
            });
            window.render_frame(cx);
        })
        .unwrap();
        let assert_position = |cx: &mut TestAppContext, top: usize| {
            view.read_with(cx, |v, _| {
                assert_eq!(v.results.len(), 120);
                assert_eq!(v.results[v.selected.unwrap()].id.as_str(), "p29");
                let offset = v.collection_scroll.logical_scroll_top();
                assert_eq!(offset.item_ix, top);
                assert_eq!(offset.offset_in_item, px(3.));
                assert_eq!(v.results[top].id.as_str(), "p39");
            });
        };
        assert_position(cx, 80);
        session.update(cx, |s, cx| s.set_saved(&"newer".into(), true, cx));
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| matches!(v.status, Status::Searching))
        });
        assert_position(cx, 80);
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| matches!(v.status, Status::Done))
        });
        assert_position(cx, 81);
        version.store(2, Ordering::SeqCst);
        session.update(cx, |_, cx| {
            let mut post = Post::default();
            post.id = "p29".into();
            post.channel_id = "town".into();
            cx.emit(session::SessionEvent::PostChanged(Arc::new(post)));
        });
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| {
                matches!(v.status, Status::Done)
                    && v.results[v.selected.unwrap()].message == "version 2"
            })
        });
        assert_position(cx, 81);
        version.store(3, Ordering::SeqCst);
        session.update(cx, |_, cx| cx.emit(session::SessionEvent::SavedChanged));
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| matches!(v.status, Status::Failed(_)))
        });
        assert_position(cx, 81);
        view.read_with(cx, |v, _| {
            assert_eq!(v.results[v.selected.unwrap()].message, "version 2")
        });
    }

    #[gpui_kit::test]
    fn saved_collection_refreshes_after_save_and_remove(cx: &mut TestAppContext) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let saved = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let saved = saved.clone();
            move |request| match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("PUT", "/api/v4/users/me/preferences") => {
                    saved.store(true, Ordering::SeqCst);
                    Reply::json(json!({}))
                }
                ("POST", "/api/v4/users/me/preferences/delete") => {
                    saved.store(false, Ordering::SeqCst);
                    Reply::json(json!({}))
                }
                ("GET", "/api/v4/users/me/preferences/flagged_post") => {
                    Reply::json(if saved.load(Ordering::SeqCst) {
                        json!([{"user_id":"me", "category":"flagged_post", "name":"new", "value":"true"}])
                    } else {
                        json!([])
                    })
                }
                ("GET", "/api/v4/users/me/posts/flagged") => {
                    Reply::json(if saved.load(Ordering::SeqCst) {
                        json!({"posts":{"new":{"id":"new", "channel_id":"town", "create_at":1}}})
                    } else {
                        json!({})
                    })
                }
                _ => route(request),
            }
        });
        let (window, search) = open_search(&server, cx);
        let session = search.read_with(cx, |v, _| v.session.clone());
        let view = cx
            .update_window(window, |_, window, cx| {
                cx.new(|cx| {
                    SearchView::new_collection(session.clone(), Collection::Saved, window, cx)
                })
            })
            .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| matches!(v.status, Status::Done))
        });
        session.update(cx, |s, cx| s.set_saved(&"new".into(), true, cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 1));
        session.update(cx, |s, cx| s.set_saved(&"new".into(), false, cx));
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| {
                v.results.is_empty() && matches!(v.status, Status::Done)
            })
        });
    }

    #[gpui_kit::test]
    fn collections_open_newest_first_with_arrows_and_enter(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("GET", "/api/v4/users/me/posts/flagged" | "/api/v4/channels/town/pinned") => {
                    Reply::json(json!({
                        "order": ["old", "new"], "posts": {
                            "old": {"id":"old", "channel_id":"town", "message":"old", "create_at":1},
                            "new": {"id":"new", "channel_id":"town", "message":"new", "create_at":2}
                        }
                    }))
                }
                _ => route(request),
            }
        });
        let (window, view) = open_search(&server, cx);
        let opened = opened(&view, cx);
        for collection in [Collection::Saved, Collection::Pinned("town".into())] {
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.collection = Some(collection);
                    v.load_collection(0, cx);
                    v.focus(window, cx);
                })
            })
            .unwrap();
            wait_until(cx, |cx| {
                view.read_with(cx, |v, _| matches!(v.status, Status::Done))
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.press("home", cx);
                window.press("enter", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
        }
        assert_eq!(*opened.borrow(), ["new", "old", "new", "old"]);
    }

    #[gpui_kit::test]
    fn saved_pagination_keeps_rows_on_failure_and_retries(cx: &mut TestAppContext) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let fail = Arc::new(AtomicBool::new(true));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let fail = fail.clone();
            move |request| {
                if request.method() == "GET"
                    && request
                        .path()
                        .starts_with("/api/v4/users/me/posts/flagged?")
                {
                    if request.path().contains("page=1&") {
                        return if fail.load(Ordering::SeqCst) {
                            Reply::not_found()
                        } else {
                            Reply::json(
                                json!({"posts":{"older":{"id":"older", "channel_id":"town"}}}),
                            )
                        };
                    }
                    let posts: serde_json::Map<String, serde_json::Value> = (1..=60)
                        .map(|i| {
                            (
                                format!("p{i}"),
                                json!({"id":format!("p{i}"), "channel_id":"town", "create_at":i}),
                            )
                        })
                        .collect();
                    return Reply::json(json!({"posts":posts}));
                }
                route(request)
            }
        });
        let (window, view) = open_search(&server, cx);
        view.update(cx, |v, cx| {
            v.collection = Some(Collection::Saved);
            v.load_collection(0, cx);
        });
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 60));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("more-collection", cx);
        })
        .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.more_failed));
        assert_eq!(view.read_with(cx, |v, _| v.results.len()), 60);
        fail.store(false, Ordering::SeqCst);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("retry-collection-page", cx);
        })
        .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 61));
        assert!(view.read_with(cx, |v, _| v.next_page.is_none()));
    }

    #[test]
    fn excerpts_advance_past_multibyte_whitespace() {
        for prefix in ["x".repeat(130), "界".repeat(44)] {
            for whitespace in ['\u{3000}', '\u{2003}'] {
                let text = format!("{prefix}{whitespace}needle");
                let (shown, ranges) = excerpt(&text, &["needle".into()]);
                assert_eq!(shown.as_ref(), "…needle");
                assert_eq!(ranges, vec![3..9]);
                assert_eq!(&shown[ranges[0].clone()], "needle");
            }
        }
    }

    fn route(request: &Request) -> Reply {
        match (
            request.method(),
            request.path().split('?').next().unwrap_or_default(),
        ) {
            ("GET", "/api/v4/users/me/teams") => {
                Reply::json(json!([{ "id": "team", "name": "team" }]))
            }
            ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                { "id": "town", "team_id": "team", "type": "O", "name": "town-square",
                  "display_name": "Town Square" },
            ])),
            ("POST", "/api/v4/teams/team/posts/search") => Reply::json(json!({
                "order": ["new", "old"],
                "posts": {
                    "new": { "id": "new", "channel_id": "town", "user_id": "ann",
                             "message": "hello again", "create_at": 2 },
                    "old": { "id": "old", "channel_id": "town", "user_id": "ann",
                             "message": "hello", "create_at": 1 },
                },
                "matches": { "new": ["hello"], "old": ["hello"] },
            })),
            ("GET", "/api/v4/users/me/teams/team/channels/members")
            | ("GET", "/api/v4/users/me/preferences/favorite_channel")
            | ("GET", "/api/v4/emoji")
            | ("POST", "/api/v4/users/ids")
            | ("POST", "/api/v4/users/status/ids") => Reply::json(json!([])),
            _ => Reply::not_found(),
        }
    }

    /// The same, with another page of results always waiting.
    fn paged_route(request: &Request) -> Reply {
        match (
            request.method(),
            request.path().split('?').next().unwrap_or_default(),
        ) {
            ("POST", "/api/v4/teams/team/posts/search") => Reply::json(json!({
                "order": ["new"],
                "posts": {
                    "new": { "id": "new", "channel_id": "town", "user_id": "ann",
                             "message": "hello again", "create_at": 2 },
                },
                "has_next": true,
            })),
            _ => route(request),
        }
    }

    /// A search of Town Square in its own window, on a signed-in session.
    fn open_search(
        server: &SlowServer,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<SearchView>) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let mut me = User::default();
        me.id = "me".into();
        let session = cx.new(|cx| Session::new(client, me, Some(TeamId::from("team")), cx));
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.is_loaded() && s.team().is_some())
        });
        let (window, view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(480.), px(640.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    SearchView::new(
                        session,
                        SearchScope::Conversation("town".into()),
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| v.focus(window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        (window, view)
    }

    /// The posts `view` asks to open, as it asks.
    fn opened(view: &Entity<SearchView>, cx: &mut TestAppContext) -> Rc<RefCell<Vec<String>>> {
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = opened.clone();
        cx.update(|cx| {
            cx.subscribe(view, move |_, event: &SearchEvent, _| {
                if let SearchEvent::Open(post) = event {
                    sink.borrow_mut().push(post.id.to_string());
                }
            })
            .detach()
        });
        opened
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

    /// Results hold their authors' pictures in the view's own store, not
    /// GPUI's app-wide one. The view outlives many searches, so a new set
    /// of results frees the pictures only the last one showed, and closing
    /// the view frees the rest.
    #[gpui_kit::test]
    fn result_pictures_go_with_their_results_and_the_view(cx: &mut TestAppContext) {
        use crate::image_scope::testing::{draw_until_painted, in_atlas};
        use gpui_kit::ImgResourceLoader;
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            // Stray local connections reach test ports too: answer by
            // method and path.
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("POST", "/api/v4/users/ids") => Reply::json(json!([
                    { "id": "ann", "username": "ann", "last_picture_update": 1 },
                    { "id": "bob", "username": "bob", "last_picture_update": 2 },
                ])),
                ("POST", "/api/v4/teams/team/posts/search") => {
                    let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    let author = if body["terms"].as_str().unwrap_or_default().contains("bye") {
                        "bob"
                    } else {
                        "ann"
                    };
                    Reply::json(json!({
                        "order": [author],
                        "posts": { author: { "id": author, "channel_id": "town",
                                             "user_id": author, "message": "hello bye" } },
                    }))
                }
                _ => route(request),
            }
        });
        let (window, view) = open_search(&server, cx);
        cx.update(crate::http::PictureClient::install);
        let picture = |user: &str, cx: &mut TestAppContext| {
            let version = if user == "ann" { 1 } else { 2 };
            view.read_with(cx, |v, cx| {
                let client = v.session.read(cx).client().clone();
                Resource::Uri(avatar_url(&client, user, version).into())
            })
        };
        let (ann, bob) = (picture("ann", cx), picture("bob", cx));
        let images = view.read_with(cx, |v, _| v.images.clone());
        let held = |picture: &Resource| {
            let images = images.clone();
            let picture = picture.clone();
            move |cx: &App| vec![images.read(cx).get(&picture)]
        };

        cx.update_window(window, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, cx| {
                v.results.len() == 1 && v.author_picture(&v.results[0], cx).is_some()
            })
        });
        let anns = draw_until_painted(cx, window, held(&ann)).remove(0);
        assert!(
            !cx.update(|cx| cx.has_asset::<ImgResourceLoader>(&ann)),
            "the picture stays out of the app-wide store"
        );

        cx.update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| v.focus(window, cx));
            window.input("bye", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| {
                v.results.first().is_some_and(|p| p.id.as_str() == "bob")
            })
        });
        draw_until_painted(cx, window, held(&bob));
        assert!(
            images.read_with(cx, |images, _| images.get(&ann).is_none()),
            "results for other words free the last ones' pictures"
        );
        assert!(!in_atlas(cx, window, &anns), "from the GPU atlas too");

        drop(images);
        let images = view.read_with(cx, |v, _| v.images.downgrade());
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        drop(view);
        // GPUI releases dropped entities when it next flushes effects.
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(images.upgrade().is_none(), "closing frees the pictures");
        assert!(!cx.update(|cx| cx.has_asset::<ImgResourceLoader>(&bob)));
    }

    /// Ann's and Bob's pictures, as the directory now has them, once it
    /// knows both.
    fn author_pictures(view: &Entity<SearchView>, cx: &mut TestAppContext) -> (Resource, Resource) {
        let picture = |user: &str, cx: &mut TestAppContext| {
            view.read_with(cx, |v, cx| {
                let session = v.session.read(cx);
                let version = session
                    .directory()
                    .read(cx)
                    .user(&user.into())?
                    .last_picture_update;
                Some(Resource::Uri(
                    avatar_url(session.client(), user, version).into(),
                ))
            })
        };
        wait_until(cx, |cx| {
            picture("ann", cx).is_some() && picture("bob", cx).is_some()
        });
        (picture("ann", cx).unwrap(), picture("bob", cx).unwrap())
    }

    /// Unpinning, unsaving or refreshing replaces a collection's rows
    /// without a new search; pictures of authors no row shows go too.
    #[gpui_kit::test]
    fn collection_rows_free_their_authors_pictures_when_they_go(cx: &mut TestAppContext) {
        use crate::image_scope::testing::{draw_until_painted, in_atlas};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let unpinned = Arc::new(AtomicBool::new(false));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let unpinned = unpinned.clone();
            move |request| match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("POST", "/api/v4/users/ids") => Reply::json(json!([
                    { "id": "ann", "username": "ann", "last_picture_update": 1 },
                    { "id": "bob", "username": "bob", "last_picture_update": 2 },
                ])),
                ("GET", "/api/v4/channels/town/pinned") => {
                    let mut posts = json!({
                        "bob": { "id": "bob", "channel_id": "town", "user_id": "bob",
                                 "message": "kept", "create_at": 2 },
                    });
                    if !unpinned.load(Ordering::SeqCst) {
                        posts["ann"] = json!({ "id": "ann", "channel_id": "town",
                                               "user_id": "ann", "message": "pinned",
                                               "create_at": 1 });
                    }
                    Reply::json(json!({ "posts": posts }))
                }
                _ => route(request),
            }
        });
        let (window, view) = open_collection(&server, Collection::Pinned("town".into()), cx);
        cx.update(crate::http::PictureClient::install);
        let (ann, bob) = author_pictures(&view, cx);
        let images = view.read_with(cx, |v, _| v.images.clone());
        let painted = draw_until_painted(cx, window, {
            let (images, ann, bob) = (images.clone(), ann.clone(), bob.clone());
            move |cx| vec![images.read(cx).get(&ann), images.read(cx).get(&bob)]
        });

        unpinned.store(true, Ordering::SeqCst);
        view.update(cx, |v, cx| v.load_collection(0, cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 1));
        cx.run_until_parked();
        assert!(
            images.read_with(cx, |images, _| images.get(&ann).is_none()),
            "the unpinned post's author's picture goes"
        );
        assert!(!in_atlas(cx, window, &painted[0]), "from the GPU atlas too");
        assert!(images.read_with(cx, |images, _| images.get(&bob).is_some()));
        assert!(in_atlas(cx, window, &painted[1]), "the kept row's stays");
    }

    /// A new profile picture for someone in the results frees the old one,
    /// though the results stay the same.
    #[gpui_kit::test]
    fn a_replaced_profile_picture_frees_its_old_version(cx: &mut TestAppContext) {
        use crate::image_scope::testing::{draw_until_painted, in_atlas};
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (
                request.method(),
                request.path().split('?').next().unwrap_or_default(),
            ) {
                ("POST", "/api/v4/users/ids") => Reply::json(json!([
                    { "id": "ann", "username": "ann", "last_picture_update": 1 },
                    { "id": "bob", "username": "bob", "last_picture_update": 2 },
                ])),
                ("POST", "/api/v4/teams/team/posts/search") => Reply::json(json!({
                    "order": ["ann", "bob"],
                    "posts": {
                        "ann": { "id": "ann", "channel_id": "town", "user_id": "ann",
                                 "message": "hello", "create_at": 2 },
                        "bob": { "id": "bob", "channel_id": "town", "user_id": "bob",
                                 "message": "hello", "create_at": 1 },
                    },
                })),
                _ => route(request),
            }
        });
        let (window, view) = open_search(&server, cx);
        cx.update(crate::http::PictureClient::install);
        cx.update_window(window, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
        let (old, bob) = author_pictures(&view, cx);
        let images = view.read_with(cx, |v, _| v.images.clone());
        let lookup = |pictures: Vec<Resource>| {
            let images = images.clone();
            move |cx: &App| {
                pictures
                    .iter()
                    .map(|picture| images.read(cx).get(picture))
                    .collect()
            }
        };
        let painted = draw_until_painted(cx, window, lookup(vec![old.clone(), bob.clone()]));

        let directory = view.read_with(cx, |v, cx| v.session.read(cx).directory().clone());
        directory.update(cx, |d, cx| {
            d.update_user(&"ann".into(), |ann| ann.last_picture_update = 5, cx)
        });
        let (new, _) = author_pictures(&view, cx);
        assert_ne!(new, old);
        draw_until_painted(cx, window, lookup(vec![new]));
        assert!(
            images.read_with(cx, |images, _| images.get(&old).is_none()),
            "the old version goes"
        );
        assert!(!in_atlas(cx, window, &painted[0]), "from the GPU atlas too");
        assert!(in_atlas(cx, window, &painted[1]), "others' pictures stay");
    }

    #[gpui_kit::test]
    fn failed_pagination_retains_results_and_retries_the_same_page(cx: &mut TestAppContext) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let fail = Arc::new(AtomicBool::new(true));
        let server = SlowServer::start(Network::new(Duration::ZERO), {
            let fail = fail.clone();
            move |request| {
                if request.method() == "POST" && request.path() == "/api/v4/teams/team/posts/search"
                {
                    let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                    if body["page"] == 1 {
                        return if fail.load(Ordering::SeqCst) {
                            Reply::not_found()
                        } else {
                            route(request)
                        };
                    }
                }
                paged_route(request)
            }
        });
        let (handle, view) = open_search(&server, cx);
        cx.update_window(handle, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 1));
        cx.update_window(handle, |_, window, cx| window.click("more-results", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.more_failed));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(view.read(cx).results.len(), 1);
            assert_eq!(view.read(cx).next_page, Some(1));
            assert_eq!(window.find("retry-search-results").label(), Some("Retry"));
            fail.store(false, Ordering::SeqCst);
            window.click("retry-search-results", cx);
        })
        .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
        view.read_with(cx, |v, _| {
            assert!(!v.more_failed);
            assert!(v.next_page.is_none());
        });
    }

    #[gpui_kit::test]
    fn typing_searches_and_the_keyboard_opens_a_result(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (window, view) = open_search(&server, cx);
        let opened = opened(&view, cx);
        let searches = |server: &SlowServer| {
            server
                .requests()
                .iter()
                .filter(|r| r.method() == "POST" && r.path() == "/api/v4/teams/team/posts/search")
                .count()
        };

        // Each keystroke is its own turn of the event loop, as when typing,
        // but the pause between them is shorter than the debounce.
        let type_keys = |cx: &mut TestAppContext, keys: fn(&mut Window, &mut App)| {
            cx.update_window(window, |_, window, cx| keys(window, cx))
                .unwrap();
            cx.run_until_parked();
        };
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
        view.read_with(cx, |v, _| assert!(v._search.is_none()));
        cx.executor().advance_clock(DEBOUNCE);
        cx.run_until_parked();
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

    #[gpui_kit::test]
    fn enter_searches_new_words_rather_than_opening_the_old_choice(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (window, view) = open_search(&server, cx);
        let opened = opened(&view, cx);
        cx.update_window(window, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
        cx.update_window(window, |_, window, cx| {
            window.press("down", cx);
            window.input("!", cx);
            window.press("enter", cx);
        })
        .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.query == "hello!"));
        assert!(opened.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn showing_more_results_doesnt_cancel_a_newer_search(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), paged_route);
        let (window, view) = open_search(&server, cx);
        cx.update_window(window, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.next_page.is_some()));

        // "Show more results" for the old words, pressed while the new
        // ones wait out the debounce.
        cx.update_window(window, |_, window, cx| window.input("!", cx))
            .unwrap();
        cx.run_until_parked();
        view.update(cx, |v, cx| v.load_more(cx));
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.query == "hello!"));
        view.read_with(cx, |v, _| assert!(matches!(v.status, Status::Done)));
    }

    #[gpui_kit::test]
    fn an_unavailable_page_still_allows_loading_more_results(cx: &mut TestAppContext) {
        let searches = std::sync::atomic::AtomicUsize::new(0);
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            if request.method() == "POST"
                && request.path() == "/api/v4/teams/team/posts/search"
                && searches.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                Reply::json(json!({
                    "has_next": true, "order": ["gone"], "posts": {
                        "gone": { "id": "gone", "channel_id": "unavailable", "message": "hello" }
                    }
                }))
            } else {
                route(request)
            }
        });
        let (window, view) = open_search(&server, cx);
        cx.update_window(window, |_, window, cx| {
            window.click("search-team", cx);
            window.input("hello", cx);
        })
        .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.next_page.is_some()));
        view.read_with(cx, |v, _| assert!(v.results.is_empty()));
        cx.update_window(window, |_, window, cx| window.click("more-results", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
    }

    #[gpui_kit::test]
    fn scope_buttons_work_from_the_keyboard_and_discard_old_answers(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (window, view) = open_search(&server, cx);
        cx.update_window(window, |_, window, cx| window.input("hello", cx))
            .unwrap();
        wait_until(cx, |cx| view.read_with(cx, |v, _| v.results.len() == 2));
        let revision = view.read_with(cx, |v, _| v.revision);
        cx.update_window(window, |_, window, cx| {
            // Tab leaves the input for Close, then the two scope buttons.
            window.press("tab", cx);
            window.press("tab", cx);
            window.press("tab", cx);
            assert_eq!(window.find("search-team").focused(), Some(true));
            window.press("space", cx);
        })
        .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| v.results_scope == Some(SearchScope::Team))
        });
        view.update(cx, |v, cx| {
            v.finish(
                revision,
                SearchScope::Conversation("town".into()),
                "hello".into(),
                0,
                Ok(PostSearch::default()),
                cx,
            );
            assert_eq!(v.results.len(), 2);
            assert_eq!(v.results_scope, Some(SearchScope::Team));
            v.finish(
                revision,
                SearchScope::Team,
                "old query".into(),
                1,
                Ok(PostSearch::default()),
                cx,
            );
            assert_eq!(v.query, "hello");
        });
        cx.update_window(window, |_, window, cx| {
            window.click("search-conversation", cx)
        })
        .unwrap();
        wait_until(cx, |cx| {
            view.read_with(cx, |v, _| {
                v.results_scope == Some(SearchScope::Conversation("town".into()))
            })
        });
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
