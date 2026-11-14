//! Exercise GPUI's real Markdown conversion, including flatten_unclaimed_math.
//! The recording resolver uses the production admission decision, then returns
//! a local failure so these tests neither access files nor issue HTTP requests.
use super::*;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Bounds, Context, Render, TestAppContext, WindowBounds, WindowOptions, size,
};
use std::{collections::BTreeMap, sync::Mutex};

type Requests = Arc<Mutex<BTreeMap<String, bool>>>;
struct Document {
    state: Entity<TextViewState>,
    prepared: session::PreparedMessage,
    requests: Requests,
}
impl Render for Document {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let prepared = self.prepared.clone();
        let requests = self.requests.clone();
        TextView::new(&self.state)
            .w_full()
            .image_source(move |url| {
                let source = prepared_image_source(&prepared, url);
                let allowed = matches!(
                    source,
                    gpui_kit::ImageSource::Resource(gpui_kit::Resource::Uri(_))
                );
                requests.lock().unwrap().insert(url.to_string(), allowed);
                gpui_kit::ImageSource::Custom(Arc::new(|_, _| {
                    Some(Err(gpui_kit::ImageCacheError::Asset(
                        "Test image unavailable".into(),
                    )))
                }))
            })
    }
}
fn rendered_requests(
    source: &str,
    prepared: &session::PreparedMessage,
    cx: &mut TestAppContext,
) -> BTreeMap<String, bool> {
    let requests = Requests::default();
    let (handle, _) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(1400.)),
                ))),
                ..Default::default()
            },
            cx,
            |_, cx| {
                let state = cx.new(|cx| TextViewState::markdown(source, cx));
                cx.new(|_| Document {
                    state,
                    prepared: prepared.clone(),
                    requests: requests.clone(),
                })
            },
        )
        .unwrap()
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
    requests.lock().unwrap().clone()
}

#[gpui_kit::test]
fn dollar_wrapped_unsafe_images_cannot_load_in_the_actual_renderer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for source in [
        "$![bad](data:image/png;base64,AAAA)$",
        "$![bad](file:///tmp/private.png)$",
        "$<img src=\"data:image/png;base64,AAAA\">$",
        "$<img src=\"file:///tmp/private.png\">$",
        "$![bad][image]$\n\n[image]: data:image/png;base64,AAAA",
        "$![bad][image]$\n\n[image]: file:///tmp/private.png",
        "$![bad](data&colon;image/png;base64,AAAA)$",
        "$$\n![bad](data:image/png;base64,AAAA)\n$$",
    ] {
        let prepared = session::PreparedMessage::new(source, &CustomEmojis::default());
        let filtered = rendered_requests(prepared.text(), &prepared, cx);
        assert!(filtered.is_empty(), "filtered {source}: {filtered:?}");
        // Even if renderer parsing changes or unsanitized source reaches a
        // document, the authoritative resolver cannot decode/load its images.
        let raw = rendered_requests(source, &prepared, cx);
        assert!(
            raw.values().all(|allowed| !allowed),
            "raw {source}: {raw:?}"
        );
        if source == "$![bad](data:image/png;base64,AAAA)$" {
            assert!(
                raw.contains_key("data:image/png;base64,AAAA"),
                "exercise math fallback, not an inert test"
            );
        }
    }
}

#[gpui_kit::test]
fn dollar_wrapped_external_images_keep_the_budget_in_the_actual_renderer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let source = (0..20)
        .map(|i| format!("$![:image:](https://example.com/{i})$ "))
        .collect::<String>();
    let prepared = session::PreparedMessage::new(&source, &CustomEmojis::default());
    let filtered = rendered_requests(prepared.text(), &prepared, cx);
    assert_eq!(filtered.len(), 8);
    assert!(filtered.values().all(|allowed| *allowed));
    let raw = rendered_requests(&source, &prepared, cx);
    assert_eq!(raw.len(), 20);
    assert_eq!(raw.values().filter(|allowed| **allowed).count(), 8);

    let references = format!(
        "{}\n\n{}",
        (0..20)
            .map(|i| format!("$![:image:][image{i}]$ "))
            .collect::<String>(),
        (0..20)
            .map(|i| format!("[image{i}]: https://example.com/{i}\n"))
            .collect::<String>()
    );
    let prepared = session::PreparedMessage::new(&references, &CustomEmojis::default());
    let filtered = rendered_requests(prepared.text(), &prepared, cx);
    assert_eq!(filtered.len(), 8);
    assert!(filtered.values().all(|allowed| *allowed));
}

#[gpui_kit::test]
fn dollar_wrapped_custom_emoji_keep_their_separate_budget_in_the_actual_renderer(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let client = Client::new_unverified("https://chat.example.com", "token").unwrap();
    let store = cx.new(|_| session::EmojiStore::new(client));
    store.update(cx, |store, cx| {
        store.add(
            (0..205)
                .map(|i| {
                    let mut emoji = mattermost::Emoji::default();
                    emoji.id = format!("id{i}").into();
                    emoji.name = format!("custom_{i}");
                    emoji
                })
                .collect(),
            cx,
        )
    });
    let custom = store.read_with(cx, |store, _| store.custom().clone());
    let source = (0..205)
        .map(|i| format!("$:custom_{i}:$ "))
        .collect::<String>();
    let raw_source = session::emojify(&source, &custom);
    let prepared = session::PreparedMessage::new(&source, &custom);
    let filtered = rendered_requests(prepared.text(), &prepared, cx);
    assert_eq!(filtered.len(), 200);
    assert!(filtered.values().all(|allowed| *allowed));
    let raw = rendered_requests(&raw_source, &prepared, cx);
    assert_eq!(raw.len(), 205);
    assert_eq!(raw.values().filter(|allowed| **allowed).count(), 200);
}
