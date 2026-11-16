//! Pictures attached to a message, shown large over the window.
//!
//! Clicking an attached image opens it here instead of in another app. ←
//! and → step through the message's other pictures, Escape or a click
//! outside closes the viewer, and Download saves the original.

use std::{cell::Cell, rc::Rc};

use gpui_kit::assets::IconName;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogEntrance,
    h_flex,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, InteractiveElement as _, IntoElement, KeyDownEvent, ObjectFit,
    ParentElement as _, Pixels, SharedString, Size, Styled as _, StyledImage as _, WeakEntity,
    Window, div, img, px, size,
};
use mattermost::{Client, FileInfo};

use crate::{composer::format_size, download};

/// The narrowest the viewer gets, so its title and buttons fit beside a
/// small picture.
const MIN_WIDTH: f32 = 26.;
/// Room the window keeps around the picture, in rems: the dialog's margins
/// and padding across, and also its title and footer down.
const CHROME: Size<f32> = Size {
    width: 6.,
    height: 11.,
};

/// Show `images` over the window, starting with the one at `index`.
pub fn open(
    client: Client,
    images: Vec<FileInfo>,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let images = images
        .into_iter()
        .map(|file| {
            let url = client.file_image_url(&file).into();
            ViewerImage { file, url }
        })
        .collect();
    open_images(Some(client), images, index, window, cx);
}

struct ViewerImage {
    file: FileInfo,
    url: SharedString,
}

/// URL images share the file viewer's sizing, cache, focus and dismissal. They
/// have no Mattermost file ID, so do not offer a file-API download command.
pub(crate) fn open_remote(
    url: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> Option<WeakEntity<crate::image_scope::ImageStore>> {
    let mut file = FileInfo::default();
    file.name = "Attachment image".into();
    open_images(None, vec![ViewerImage { file, url }], 0, window, cx)
}

/// Returns the store of the pictures shown, which lasts as long as the
/// viewer, if it opened.
fn open_images(
    client: Option<Client>,
    images: Vec<ViewerImage>,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) -> Option<WeakEntity<crate::image_scope::ImageStore>> {
    if images.is_empty() || window.has_active_dialog(cx) {
        return None;
    }
    let images: Rc<[ViewerImage]> = images.into();
    // Full-size pictures are the largest images drawn. Held by the dialog,
    // not GPUI's app-wide store, they are freed when the viewer closes.
    let pictures = crate::image_scope::ImageStore::new(cx);
    let shown_pictures = pictures.downgrade();
    let current = cx.new(|_| index.min(images.len() - 1));
    let focus = cx.focus_handle();
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            focus.focus(window, cx);
        }
        let ix = *current.read(cx);
        let file = images[ix].file.clone();
        let rem = window.rem_size();
        let viewport = window.viewport_size();
        let bounds = size(
            (viewport.width - rem * CHROME.width).max(rem * 4.),
            (viewport.height - rem * CHROME.height).max(rem * 4.),
        );
        let shown = fit(&file, bounds);
        let theme = cx.theme();
        let count = images.len();
        let step = {
            let current = current.clone();
            move |delta: isize, cx: &mut App| {
                current.update(cx, |ix, cx| {
                    *ix = (*ix as isize + delta).rem_euclid(count as isize) as usize;
                    cx.notify();
                })
            }
        };
        let on_key = {
            let step = step.clone();
            move |event: &KeyDownEvent, _: &mut Window, cx: &mut App| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "left" => step(-1, cx),
                    "right" => step(1, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }
        };
        let picture = img(images[ix].url.clone())
            .image_cache(&pictures)
            .w(shown.width)
            .h(shown.height)
            .object_fit(ObjectFit::Contain)
            .with_loading(|| {
                h_flex()
                    .size_full()
                    .justify_center()
                    .child(Spinner::new())
                    .into_any_element()
            })
            .with_fallback({
                let muted = theme.muted_foreground;
                move || {
                    h_flex()
                        .size_full()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child("This picture can’t be shown here.")
                        .into_any_element()
                }
            });
        let download = client.clone().map(|client| {
            let file = file.clone();
            Button::new("download-image")
                .small()
                .outline()
                .icon(IconName::Download)
                .label("Download")
                .on_click(move |_, window, cx| {
                    download::save(client.clone(), file.clone(), window, cx, |path, _, cx| {
                        cx.reveal_path(path)
                    })
                })
        });
        let navigation = (count > 1).then(|| {
            let (back, forward) = (step.clone(), step.clone());
            h_flex()
                .gap_1()
                .child(
                    Button::new("previous-image")
                        .small()
                        .ghost()
                        .icon(IconName::ChevronLeft)
                        .accessibility_label("Previous picture")
                        .tooltip("Previous picture (←)")
                        .on_click(move |_, _, cx| back(-1, cx)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} of {count}", ix + 1)),
                )
                .child(
                    Button::new("next-image")
                        .small()
                        .ghost()
                        .icon(IconName::ChevronRight)
                        .accessibility_label("Next picture")
                        .tooltip("Next picture (→)")
                        .on_click(move |_, _, cx| forward(1, cx)),
                )
        });
        dialog
            .title(div().pr_8().truncate().child(file.name.clone()))
            .w((shown.width + rem * 2.).max(rem * MIN_WIDTH))
            .centered()
            .entrance(DialogEntrance::Fade)
            .child(
                v_flex()
                    .id("image-viewer")
                    .track_focus(&focus)
                    .on_key_down(on_key)
                    .gap_3()
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("picture-{}", file.id)))
                            .test_support()
                            .justify_center()
                            .child(picture),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(if client.is_some() {
                                        details(&file)
                                    } else {
                                        String::new()
                                    }),
                            )
                            .children(navigation)
                            .children(download),
                    ),
            )
    });
    Some(shown_pictures)
}

/// The size to draw `file` at: its own, scaled down to fit `bounds`. A
/// picture whose size the server doesn't know fills `bounds`, and its
/// contents keep their proportions inside.
fn fit(file: &FileInfo, bounds: Size<Pixels>) -> Size<Pixels> {
    if file.width <= 0 || file.height <= 0 {
        return bounds;
    }
    let natural = size(px(file.width as f32), px(file.height as f32));
    let scale = (bounds.width / natural.width)
        .min(bounds.height / natural.height)
        .min(1.);
    size(natural.width * scale, natural.height * scale)
}

/// `PNG · 1.2 MB · 1920 × 1080`.
fn details(file: &FileInfo) -> String {
    let mut parts = Vec::new();
    if !file.extension.is_empty() {
        parts.push(file.extension.to_uppercase());
    }
    parts.push(format_size(file.size.max(0) as u64));
    if file.width > 0 && file.height > 0 {
        parts.push(format!("{} × {}", file.width, file.height));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, Context, Render, TestAppContext, WindowBounds, WindowOptions, point};

    use super::*;

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn named(id: &str) -> FileInfo {
        let mut file = picture(640, 480);
        file.id = id.into();
        file.name = format!("{id}.png");
        file.mime_type = "image/png".into();
        file
    }

    /// The viewer holds the full-size pictures it shows, and frees them
    /// when it closes; GPUI's app-wide store would keep them for the run.
    #[gpui_kit::test]
    fn the_viewer_frees_its_pictures_when_it_closes(cx: &mut TestAppContext) {
        use gpui_kit::{ImgResourceLoader, Resource};
        cx.update(gpui_kit::init);
        let window = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank))
                .unwrap()
                .0
        });
        cx.update(crate::http::PictureClient::install);
        let url = "https://example.com/full.png";
        let shared = Resource::Uri(url.into());
        let pictures = cx
            .update_window(window, |_, window, cx| {
                let pictures = open_remote(url.into(), window, cx).unwrap();
                window.render_frame(cx);
                pictures
            })
            .unwrap();
        // The picture is fetched and decoded on the test executor, then
        // painted from the viewer's store.
        let mut painted = None;
        for _ in 0..20 {
            cx.run_until_parked();
            painted = cx
                .update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    match pictures.upgrade()?.read(cx).get(&shared) {
                        Some(Ok(image)) if window.has_image_atlas_entry(&image) => Some(image),
                        _ => None,
                    }
                })
                .unwrap();
            if painted.is_some() {
                break;
            }
        }
        let painted = painted.expect("the viewer paints its picture");
        assert!(!cx.update(|cx| cx.has_asset::<ImgResourceLoader>(&shared)));
        cx.update_window(window, |_, window, cx| {
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert!(pictures.upgrade().is_none(), "closing frees the pictures");
        cx.update_window(window, |_, window, _| {
            assert!(
                !window.has_image_atlas_entry(&painted),
                "and takes them out of the GPU atlas"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn arrows_step_through_the_pictures_and_escape_closes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let client = Client::new_unverified("http://localhost:1", "token").unwrap();
        let window = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(1000.), px(800.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Blank))
                .unwrap()
                .0
        });
        cx.update_window(window, |_, window, cx| {
            open(client, vec![named("a"), named("b")], 1, window, cx);
            // The first frame builds the dialog and focuses it.
            window.render_frame(cx);
            window.render_frame(cx);
            let showing =
                |window: &Window, id: &str| window.try_find(format!("picture-{id}")).is_some();
            assert!(showing(window, "b"));
            window.press("right", cx);
            window.render_frame(cx);
            assert!(showing(window, "a"), "→ wraps around to the first picture");
            window.press("left", cx);
            window.render_frame(cx);
            assert!(showing(window, "b"));
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
    }

    fn picture(width: i64, height: i64) -> FileInfo {
        let mut file = FileInfo::default();
        file.width = width;
        file.height = height;
        file
    }

    #[test]
    fn small_pictures_keep_their_size_and_large_ones_shrink_to_fit() {
        let bounds = size(px(800.), px(600.));
        assert_eq!(fit(&picture(400, 300), bounds), size(px(400.), px(300.)));
        assert_eq!(fit(&picture(1600, 600), bounds), size(px(800.), px(300.)));
        assert_eq!(fit(&picture(1000, 3000), bounds), size(px(200.), px(600.)));
        assert_eq!(fit(&picture(0, 0), bounds), bounds);
    }

    #[test]
    fn details_name_the_kind_size_and_dimensions() {
        let mut file = picture(1920, 1080);
        file.extension = "png".into();
        file.size = 2048;
        assert_eq!(
            details(&file),
            format!("PNG · {} · 1920 × 1080", format_size(2048))
        );
    }
}
