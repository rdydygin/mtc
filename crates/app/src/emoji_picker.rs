//! Shared bounded emoji search and the reaction popover.
use crate::{
    persist,
    post_row::{PostAction, PostActionHandler},
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Enter, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::ListItem,
    popover::Popover,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div,
};
use mattermost::{Client, Emoji};
use session::{CustomEmojis, EmojiStore};
use std::{rc::Rc, time::Duration};

pub struct EmojiSearch {
    store: Entity<EmojiStore>,
    client: Client,
    query: String,
    generation: u64,
    rows: Vec<String>,
    recent: Vec<String>,
    loading: bool,
    failed: bool,
    task: Option<Task<()>>,
    _store_subscription: Subscription,
}

impl EmojiSearch {
    pub fn new(client: Client, store: Entity<EmojiStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |this, _, cx| {
            this.refresh(cx);
        });
        let mut recent = persist::read_with(|s| s.recent_emoji.clone());
        for name in [
            "+1",
            "heart",
            "joy",
            "tada",
            "eyes",
            "smile",
            "white_check_mark",
            "rocket",
        ] {
            if !recent.iter().any(|r| r == name) {
                recent.push(name.to_owned());
            }
        }
        let rows = session::emoji_matches(store.read(cx).custom(), "", &recent, 8);
        Self {
            client,
            store,
            query: String::new(),
            generation: 0,
            rows,
            recent,
            loading: false,
            failed: false,
            task: None,
            _store_subscription: subscription,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.task = None;
        self.loading = false;
        cx.notify();
    }

    pub fn custom<'a>(&self, cx: &'a App) -> &'a CustomEmojis {
        self.store.read(cx).custom()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.rows = session::emoji_matches(self.custom(cx), &self.query, &self.recent, 8);
        cx.notify();
    }

    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.generation += 1;
        self.task = None;
        self.query = query.to_lowercase();
        self.loading = !query.is_empty();
        self.failed = false;
        self.refresh(cx);
        if query.is_empty() {
            return;
        }
        let generation = self.generation;
        let query = self.query.clone();
        let client = self.client.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(180))
                .await;
            let result = client.search_emoji(&query).await;
            let _ = this.update(cx, |this, cx| this.receive(generation, result, cx));
        }));
    }

    fn receive(
        &mut self,
        generation: u64,
        result: anyhow::Result<Vec<Emoji>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        self.failed = result.is_err();
        if let Ok(emoji) = result {
            self.store.update(cx, |s, cx| s.add(emoji, cx));
        }
        self.refresh(cx);
    }
}

type Choose = Rc<dyn Fn(String, &mut Window, &mut App)>;

struct EmojiPicker {
    input: Entity<InputState>,
    search: Entity<EmojiSearch>,
    selected: Option<String>,
    choose: Choose,
    _subscriptions: Vec<Subscription>,
}

impl EmojiPicker {
    fn new(
        client: Client,
        store: Entity<EmojiStore>,
        choose: Choose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| EmojiSearch::new(client, store, cx));
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search emoji"));
        let change = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.sync_query(cx);
            }
        });
        let results = cx.observe(&search, |this, _, cx| {
            let rows = &this.search.read(cx).rows;
            if !this
                .selected
                .as_ref()
                .is_some_and(|name| rows.contains(name))
            {
                this.selected = rows.first().cloned();
            }
            cx.notify();
        });
        let selected = search.read(cx).rows.first().cloned();
        Self {
            input,
            search,
            selected,
            choose,
            _subscriptions: vec![change, results],
        }
    }

    fn sync_query(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim_matches(':').to_lowercase();
        if self.search.read(cx).query != query {
            self.search.update(cx, |s, cx| s.set_query(&query, cx));
            self.selected = self.search.read(cx).rows.first().cloned();
        }
    }

    fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_query(cx);
        if let Some(name) = self
            .search
            .read(cx)
            .rows
            .get(self.selected_index(cx))
            .cloned()
        {
            persist::update(|s| s.remember_emoji(&name));
            (self.choose)(name, window, cx);
        }
    }

    fn selected_index(&self, cx: &App) -> usize {
        self.selected
            .as_ref()
            .and_then(|name| self.search.read(cx).rows.iter().position(|row| row == name))
            .unwrap_or(0)
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        self.sync_query(cx);
        let count = self.search.read(cx).rows.len();
        if count != 0 {
            let ix = (self.selected_index(cx) + if down { 1 } else { count - 1 }) % count;
            self.selected = Some(self.search.read(cx).rows[ix].clone());
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for EmojiPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_index(cx);
        let search = self.search.read(cx);
        v_flex()
            .w_64()
            .gap_1()
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| this.step(false, cx)))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| this.step(true, cx)))
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                this.choose(window, cx);
                cx.stop_propagation();
            }))
            .child(
                Input::new(&self.input)
                    .id("emoji-search")
                    .aria_label("Search emoji")
                    .small(),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if search.loading {
                        "Searching custom emoji…"
                    } else if search.failed {
                        "Custom search unavailable; showing known emoji"
                    } else if search.rows.is_empty() {
                        "No matching emoji"
                    } else if search.query.is_empty() {
                        "Recent and suggested emoji"
                    } else {
                        "Emoji"
                    }),
            )
            .children(search.rows.iter().enumerate().map(|(ix, name)| {
                ListItem::new(SharedString::from(format!("emoji-{name}")))
                    .selected(ix == selected)
                    .aria_selected(ix == selected)
                    .accessibility_label(format!("React with :{name}:"))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .text_sm()
                            .child(
                                div()
                                    .w_6()
                                    .flex_none()
                                    .child(crate::status::emoji_element(name, search.custom(cx))),
                            )
                            .child(div().min_w_0().truncate().child(format!(":{name}:"))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = this.search.read(cx).rows.get(ix).cloned();
                        this.choose(window, cx);
                    }))
            }))
    }
}

/// Retained by the post's keyed state; search/input entities exist only while open.
pub struct ReactionPicker {
    client: Client,
    store: Entity<EmojiStore>,
    picker: Option<Entity<EmojiPicker>>,
    trigger_focus: gpui_kit::FocusHandle,
    on_action: PostActionHandler,
    fallback_focus: Option<gpui_kit::FocusHandle>,
    _release: Subscription,
}

impl ReactionPicker {
    pub fn new(
        client: Client,
        store: Entity<EmojiStore>,
        on_action: PostActionHandler,
        fallback_focus: Option<gpui_kit::FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let release = cx.on_release_in(window, |this, window, cx| {
            if let (Some(picker), Some(fallback)) = (&this.picker, &this.fallback_focus) {
                let input = picker.read(cx).input.focus_handle(cx);
                if window.focused(cx).is_none() || input.is_focused(window) {
                    fallback.focus(window, cx);
                }
            }
        });
        Self {
            client,
            store,
            picker: None,
            trigger_focus: cx.focus_handle().tab_stop(true),
            on_action,
            fallback_focus,
            _release: release,
        }
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let action = self.on_action.clone();
        let choose: Choose = Rc::new(move |name, window, cx| {
            action(PostAction::ToggleReaction(name), window, cx);
            let _ = weak.update(cx, |this, cx| {
                this.picker = None;
                this.trigger_focus.focus(window, cx);
                cx.notify();
            });
        });
        self.picker = Some(cx.new(|cx| {
            EmojiPicker::new(self.client.clone(), self.store.clone(), choose, window, cx)
        }));
        cx.notify();
    }
}

impl Render for ReactionPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.picker.clone();
        let mut popover = Popover::new("reaction-picker")
            .open(content.is_some())
            .trigger(
                Button::new("add-reaction")
                    .track_focus(&self.trigger_focus)
                    .xsmall()
                    .ghost()
                    .icon(gpui_kit::assets::IconName::Plus)
                    .accessibility_label("Add reaction…")
                    .tooltip("Add reaction…"),
            )
            .on_open_change(cx.listener(|this, open, window, cx| {
                if *open {
                    this.open(window, cx);
                    this.picker
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .input
                        .focus_handle(cx)
                        .focus(window, cx);
                } else {
                    this.picker = None;
                    this.trigger_focus.focus(window, cx);
                    cx.notify();
                }
            }));
        if let Some(picker) = &content {
            popover = popover.track_focus(&picker.read(cx).input.focus_handle(cx));
        }
        popover.content(move |_, _, _| div().children(content.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, WindowOptions};
    use mattermost::testing::{Network, Reply, SlowServer};
    use serde_json::json;

    fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(cx) {
            assert!(std::time::Instant::now() < deadline, "request timed out");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui_kit::test]
    fn picker_keyboard_toggles_reaction_and_escape_restores_focus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let server = SlowServer::start(Network::new(Duration::ZERO), |r| {
            match (r.method(), r.path().split('?').next().unwrap()) {
                ("POST", "/api/v4/reactions") => Reply::json(json!({})),
                (_, "/api/v4/users/me") => Reply::json(json!({"id":"me"})),
                _ => Reply::json(json!([])),
            }
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let me = serde_json::from_value(json!({"id":"me"})).unwrap();
        let session = cx.new(|cx| session::Session::new(client.clone(), me, None, cx));
        let store = session.read_with(cx, |s, _| s.emoji_store().clone());
        let post = serde_json::from_value(json!({"id":"post", "channel_id":"channel"})).unwrap();
        let on_action: PostActionHandler = Rc::new(move |action, _, cx| {
            if let PostAction::ToggleReaction(name) = action {
                session.update(cx, |s, cx| s.toggle_reaction(&post, &name, cx));
            }
        });
        let (handle, picker) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ReactionPicker::new(client, store, on_action, None, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("add-reaction", cx);
            window.render_frame(cx);
            assert_eq!(window.find("emoji-search").focused(), Some(true));
            window.input("smile", cx);
            window.press("down", cx);
            window.press("up", cx);
            window.press("enter", cx);
            assert!(picker.read(cx).picker.is_none());
            window.render_frame(cx);
            window.click("add-reaction", cx);
            window.render_frame(cx);
            window.press("escape", cx);
            assert!(picker.read(cx).picker.is_none());
            assert_eq!(window.find("add-reaction").focused(), Some(true));
        })
        .unwrap();
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/reactions")
        });
        let request = server
            .requests()
            .into_iter()
            .find(|r| r.method() == "POST" && r.path() == "/api/v4/reactions")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body["emoji_name"], "smile");
        assert_eq!(body["post_id"], "post");
    }

    #[gpui_kit::test]
    fn delayed_custom_emoji_keeps_the_picker_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let server = SlowServer::start(Network::new(Duration::ZERO), move |r| {
            if r.method() == "POST" && r.path() == "/api/v4/emoji/search" {
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                Reply::json(json!([{"id":"remote", "name":"party_parrot"}]))
            } else {
                Reply::not_found()
            }
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client.clone()));
        store.update(cx, |s, cx| {
            s.add(
                vec![serde_json::from_value(json!({"id":"local", "name":"party_popper"})).unwrap()],
                cx,
            )
        });
        let chosen = Rc::new(std::cell::RefCell::new(None));
        let action: PostActionHandler = {
            let chosen = chosen.clone();
            Rc::new(move |action, _, _| {
                if let PostAction::ToggleReaction(name) = action {
                    *chosen.borrow_mut() = Some(name);
                }
            })
        };
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ReactionPicker::new(client, store.clone(), action, None, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("add-reaction", cx);
            window.input("party", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(Duration::from_millis(200));
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/emoji/search")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("emoji-party_popper").selected(), Some(true));
        })
        .unwrap();
        release.send(()).unwrap();
        wait_until(cx, |cx| {
            store.read_with(cx, |s, _| s.custom().contains("party_parrot"))
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("emoji-party_popper").selected(), Some(true));
            assert_eq!(window.find("emoji-party_parrot").selected(), Some(false));
            window.press("enter", cx);
        })
        .unwrap();
        assert_eq!(chosen.borrow().as_deref(), Some("party_popper"));
    }

    #[gpui_kit::test]
    fn custom_search_is_debounced_cached_and_rejects_stale_responses(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let server = SlowServer::start(Network::new(Duration::ZERO), |r| {
            if r.method() == "POST" && r.path() == "/api/v4/emoji/search" {
                Reply::json(json!([{"id":"party", "name":"party_parrot"}]))
            } else {
                Reply::not_found()
            }
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let store = cx.new(|_| EmojiStore::new(client.clone()));
        let search = cx.new(|cx| EmojiSearch::new(client, store.clone(), cx));
        search.update(cx, |s, cx| {
            s.set_query("discard", cx);
            s.set_query("party", cx);
        });
        cx.background_executor
            .advance_clock(Duration::from_millis(200));
        wait_until(cx, |cx| search.read_with(cx, |s, _| !s.loading));
        search.read_with(cx, |s, _| {
            assert!(s.rows.iter().any(|name| name == "party_parrot"))
        });
        let requests: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.method() == "POST" && r.path() == "/api/v4/emoji/search")
            .collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(requests[0].body()).unwrap()["term"],
            "party"
        );
        search.update(cx, |s, cx| {
            let stale = s.generation;
            s.set_query("new", cx);
            s.receive(
                stale,
                Ok(vec![
                    serde_json::from_value(json!({"id":"old", "name":"old_emoji"})).unwrap(),
                ]),
                cx,
            );
            assert!(!s.custom(cx).contains("old_emoji"));
            assert!(s.loading);
            s.set_query("", cx);
        });
    }

    #[test]
    fn mru_moves_duplicates_to_front_bounds_and_round_trips() {
        let mut state = persist::State::default();
        for n in 0..20 {
            state.remember_emoji(&format!("emoji_{n}"));
        }
        state.remember_emoji("emoji_10");
        assert_eq!(state.recent_emoji.len(), 16);
        assert_eq!(state.recent_emoji[0], "emoji_10");
        assert_eq!(state.recent_emoji[1], "emoji_19");
        let restored: persist::State =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.recent_emoji, state.recent_emoji);
        let matches = session::emoji_matches(
            &CustomEmojis::default(),
            "",
            &["heart".into(), "smile".into()],
            8,
        );
        assert_eq!(&matches[..2], &["heart", "smile"]);
    }
}
