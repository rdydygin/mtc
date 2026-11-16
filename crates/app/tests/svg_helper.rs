//! The SVG helper as the app runs it: the real executable in helper mode,
//! driven by the app's own code for it. Helper mode exits before the app
//! starts, so these tests never open a window or touch the user's profile.

#[allow(dead_code)]
#[path = "../src/svg_process.rs"]
mod svg_process;

use std::{
    borrow::Cow,
    io::Write as _,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use futures::{
    FutureExt as _,
    future::{Either, join_all, select},
};
use gpui_kit::{AssetSource, RenderImage, SharedString, SvgRenderer};
use svg_process::{GRACE, HELPER_ARG, Helpers, MAX_RUNNING, TIMEOUT, wrap};

/// Gradients that inherit from each other in a cycle, `a → b → c → b`:
/// usvg follows the links forever.
const HANG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><linearGradient id="a" xlink:href="#b"/><linearGradient id="b" xlink:href="#c"/><linearGradient id="c" xlink:href="#b"/><rect width="9" height="9" fill="url(#a)"/></svg>"##;

/// Shapes with soft edges and translucent paint, so that alpha survives the
/// trip through the helper only if handled right.
const PICTURE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="24" viewBox="0 0 20 12">
<defs><linearGradient id="g"><stop offset="0" stop-color="#08f"/><stop offset="1" stop-color="#f80" stop-opacity="0.3"/></linearGradient></defs>
<rect x="0.5" y="0.5" width="19" height="11" rx="3" fill="url(#g)"/>
<circle cx="13" cy="6" r="4.3" fill="#2a2" fill-opacity="0.6" stroke="#000" stroke-width="0.7"/>
</svg>"##;

fn helpers(timeout: Duration) -> &'static Helpers {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_mtc"));
    Box::leak(Box::new(Helpers::new(program, timeout)))
}

/// Waits for `done`, failing only if it takes far longer than it ever
/// should.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never saw {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct NoAssets;

impl AssetSource for NoAssets {
    fn load(&self, _: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        Ok(None)
    }

    fn list(&self, _: &str) -> gpui_kit::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// What GPUI's image loader makes of `svg`.
fn gpui_renders(svg: &[u8]) -> Arc<RenderImage> {
    SvgRenderer::new(Arc::new(NoAssets))
        .render_single_frame(svg, 1.0)
        .unwrap()
}

#[test]
fn an_svg_drawn_by_the_helper_matches_gpui_drawing_it() {
    let helpers = helpers(TIMEOUT);
    let raster = futures::executor::block_on(helpers.rasterize(PICTURE.to_vec())).unwrap();
    let expected = gpui_renders(PICTURE);
    let actual = gpui_renders(&raster);
    assert_eq!(actual.size(0), expected.size(0));
    let (actual, expected) = (actual.as_bytes(0).unwrap(), expected.as_bytes(0).unwrap());
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, e)| a.abs_diff(*e))
        .max()
        .unwrap();
    // Straight alpha, premultiplied and back, rounds a little.
    assert!(worst <= 2, "pixels differ by up to {worst}");
    assert!(expected.iter().any(|&byte| byte != 0));
}

#[test]
fn a_hanging_svg_times_out_without_holding_a_thread() {
    let helpers = helpers(TIMEOUT);
    // Both are driven on this one thread. Were the hang to hold it, the
    // picture couldn't finish first.
    let hang = helpers.rasterize(HANG.to_vec()).boxed();
    let picture = helpers.rasterize(PICTURE.to_vec()).boxed();
    let started = Instant::now();
    let Either::Right((picture, hang)) = futures::executor::block_on(select(hang, picture)) else {
        panic!("the hanging SVG finished first");
    };
    assert!(picture.is_some());
    assert_eq!(futures::executor::block_on(hang), None);
    assert!(started.elapsed() >= TIMEOUT);
    // Killed and reaped.
    wait_until("the helper reaped", || helpers.live() == 0);
}

#[test]
fn dropping_loads_kills_helpers_and_a_flood_waits_its_turn() {
    // Far longer than the test waits: only dropping can end these.
    let helpers = helpers(Duration::from_secs(600));
    let mut flood = join_all((0..MAX_RUNNING * 3).map(|_| helpers.rasterize(HANG.to_vec())));
    // Start them; the runtime drives them from here on.
    futures::executor::block_on(futures::future::poll_fn(|cx| {
        assert!(flood.poll_unpin(cx).is_pending());
        Poll::Ready(())
    }));
    wait_until("helpers start", || helpers.live() == MAX_RUNNING);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(helpers.live(), MAX_RUNNING);
    drop(flood);
    wait_until("every helper killed and reaped", || helpers.live() == 0);
    assert_eq!(helpers.peak(), MAX_RUNNING);
}

#[test]
fn quitting_kills_helpers_and_starts_no_more() {
    let helpers = helpers(Duration::from_secs(600));
    let mut hang = helpers.rasterize(HANG.to_vec()).boxed();
    futures::executor::block_on(futures::future::poll_fn(|cx| {
        assert!(hang.poll_unpin(cx).is_pending());
        Poll::Ready(())
    }));
    wait_until("the helper starts", || helpers.live() == 1);
    helpers.close();
    assert_eq!(futures::executor::block_on(hang), None);
    wait_until("the helper killed and reaped", || helpers.live() == 0);
    assert_eq!(
        futures::executor::block_on(helpers.rasterize(PICTURE.to_vec())),
        None
    );
    assert_eq!(helpers.peak(), 1);
}

#[test]
fn a_helper_ends_itself_when_its_time_is_up() {
    // As if the app were gone and nobody killed it.
    let mut helper = Command::new(env!("CARGO_BIN_EXE_mtc"))
        .args([HELPER_ARG, "300"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    helper.stdin.take().unwrap().write_all(HANG).unwrap();
    let deadline = Instant::now() + TIMEOUT + GRACE + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = helper.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the helper outlived its time");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
}

#[test]
fn the_app_wraps_only_a_png_of_bounded_size() {
    // A PNG header that claims 100000 × 100000 pixels, far over the cap.
    let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    huge.extend_from_slice(&100_000u32.to_be_bytes());
    huge.extend_from_slice(&100_000u32.to_be_bytes());
    huge.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(wrap(&huge), None);
    assert_eq!(wrap(PICTURE), None);
    assert_eq!(wrap(b"GIF89a\x01\0\x01\0"), None);
}
