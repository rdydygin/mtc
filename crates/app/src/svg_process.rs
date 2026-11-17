//! Rasterizing untrusted SVG in a short-lived helper process: the app's side.
//!
//! GPUI renders every image body it can't decode as a raster image as SVG,
//! with usvg, on a background thread. usvg has paths that cost far more than
//! the document's size, and some that never end, and a thread can't be
//! stopped. So the app never renders an untrusted SVG itself: it starts its
//! own executable again in helper mode (see [`crate::svg_helper`]), writes
//! the SVG to the helper's stdin, and reads back a PNG rendered exactly as
//! GPUI would have rendered the SVG. A helper that takes longer than
//! [`TIMEOUT`] is killed, as is one whose load is dropped or that is still
//! running when the app quits.
//!
//! GPUI draws an SVG at twice its size and marks the result as 2×, which a
//! PNG can't say; a bare PNG would show at twice the size wherever a picture
//! takes its size from the image. So GPUI gets [`wrap`]: an SVG this module
//! writes, holding nothing but the PNG at half its pixel size, which GPUI
//! draws back to the same pixels at the same size.
//!
//! This file must not name anything else in the crate: the integration tests
//! compile it on their own to drive the real executable in helper mode.

use std::{
    path::PathBuf,
    sync::{
        LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::Engine as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, Command},
    sync::{Semaphore, watch},
};

/// The argument that starts the executable as a helper. The next argument is
/// how long, in milliseconds, the helper may live.
pub const HELPER_ARG: &str = "--rasterize-svg";

/// How long a helper may take. Real images, an emoji or a diagram, render in
/// well under a tenth of this, start-up included; a document that takes
/// longer is very likely built to stall the renderer, and is shown as an
/// image that can't be shown.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// How much longer than its timeout a helper lets itself live. The app kills
/// it at the timeout; this bound holds even if the app is gone.
pub const GRACE: Duration = Duration::from_secs(1);

/// How many helpers run at once. Each may keep a core busy for [`TIMEOUT`],
/// so a conversation full of hostile SVGs takes at most this many cores for
/// that long at a time; the rest wait their turn without holding a thread.
pub const MAX_RUNNING: usize = 3;

/// The most pixels a helper returns: 4096 × 4096, enough to fill a 5K
/// screen. A larger rendering is drawn again smaller, as GPUI already does
/// past 8192 pixels a side.
pub const MAX_PIXELS: u64 = 4096 * 4096;

/// The most bytes a helper may write: a PNG of [`MAX_PIXELS`] stored without
/// compression is just over four bytes a pixel.
pub const MAX_OUTPUT: usize = MAX_PIXELS as usize * 4 + (1 << 20);

/// The most bytes a helper reads: a server image is never larger.
pub const MAX_INPUT: usize = 32 * 1024 * 1024;

/// The stack a helper renders on. usvg recurses as it follows references,
/// and a main thread's stack differs by platform: 8 MiB on macOS and Linux,
/// 1 MiB on Windows. The helper gives its renderer this one everywhere, so
/// how deep a document may go can be bounded once.
pub const RENDER_STACK: usize = 16 * 1024 * 1024;

/// Whether GPUI would render `body` as SVG: its raster decoders don't
/// recognize it. GPUI asks exactly this.
pub fn needs_helper(body: &[u8]) -> bool {
    image::guess_format(body).is_err()
}

/// The helpers this app runs: those started from its own executable.
pub fn helpers() -> Option<&'static Helpers> {
    static HELPERS: LazyLock<Option<Helpers>> = LazyLock::new(|| {
        std::env::current_exe()
            .inspect_err(|err| {
                log::error!("cannot find the app's executable for SVG helpers: {err}")
            })
            .ok()
            .map(|program| Helpers::new(program, TIMEOUT))
    });
    HELPERS.as_ref()
}

/// Starts helpers from one executable, at most [`MAX_RUNNING`] at a time.
pub struct Helpers {
    program: PathBuf,
    timeout: Duration,
    slots: Semaphore,
    /// Whether the app is quitting; then helpers are killed and none start.
    closing: watch::Sender<bool>,
    /// Helpers started and not yet reaped.
    live: AtomicUsize,
    /// The most helpers ever live at once.
    peak: AtomicUsize,
}

impl Helpers {
    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self {
            program,
            timeout,
            slots: Semaphore::new(MAX_RUNNING),
            closing: watch::Sender::new(false),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// `svg` drawn as GPUI would draw it, as an SVG that holds only that
    /// raster image (see [`wrap`]); `None` if the helper refused it, failed,
    /// or ran out of time. Dropping the future kills the helper.
    ///
    /// The work runs on the Mattermost I/O runtime, so waiting for a slot or
    /// for the helper holds no thread.
    pub async fn rasterize(&'static self, svg: Vec<u8>) -> Option<Vec<u8>> {
        struct AbortOnDrop(tokio::task::AbortHandle);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }

        let task = mattermost::runtime::spawn(self.run(svg));
        let _abort = AbortOnDrop(task.abort_handle());
        task.await.ok().flatten()
    }

    /// Kills every helper and lets no more start, waiting briefly until all
    /// are reaped. For the app's quit.
    pub fn close(&self) {
        self.closing.send_replace(true);
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.live() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Helpers started and not yet reaped.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// The most helpers ever live at once.
    #[allow(dead_code, reason = "read by the integration tests")]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    async fn run(&'static self, svg: Vec<u8>) -> Option<Vec<u8>> {
        let mut closing = self.closing.subscribe();
        let work = async {
            let _slot = self.slots.acquire().await.ok()?;
            let mut helper = self.start()?;
            let png = tokio::time::timeout(self.timeout, helper.exchange(svg))
                .await
                .ok()??;
            wrap(&png)
        };
        tokio::select! {
            biased;
            _ = closing.wait_for(|closing| *closing) => None,
            svg = work => svg,
        }
    }

    fn start(&'static self) -> Option<Running> {
        let lifetime = (self.timeout + GRACE).as_millis().to_string();
        let mut command = Command::new(&self.program);
        command
            .arg(HELPER_ARG)
            .arg(lifetime)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let child = command
            .spawn()
            .inspect_err(|err| log::warn!("cannot start an SVG helper: {err}"))
            .ok()?;
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        Some(Running {
            child: Some(child),
            helpers: self,
        })
    }
}

/// A started helper, killed and reaped when dropped, however its work ends.
struct Running {
    child: Option<Child>,
    helpers: &'static Helpers,
}

impl Running {
    /// Writes `svg` and reads the helper's PNG, if it succeeds and writes
    /// no more than [`MAX_OUTPUT`].
    async fn exchange(&mut self, svg: Vec<u8>) -> Option<Vec<u8>> {
        let child = self.child.as_mut()?;
        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        // Written while the output is read, so neither side waits on a full
        // pipe; the input closes once written.
        let write = async move {
            let _ = stdin.write_all(&svg).await;
        };
        let read = async move {
            let mut png = Vec::new();
            let mut limited = stdout.take(MAX_OUTPUT as u64 + 1);
            limited.read_to_end(&mut png).await.ok().map(|_| png)
        };
        let ((), png) = tokio::join!(write, read);
        let png = png.filter(|png| png.len() <= MAX_OUTPUT)?;
        let status = child.wait().await.ok()?;
        status.success().then_some(png)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // A helper that already exited can't be killed; waiting reaps it.
        let _ = child.start_kill();
        let live = &self.helpers.live;
        mattermost::runtime::spawn(async move {
            let _ = child.wait().await;
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// An SVG that draws `png`, a helper's rendering, back to the same pixels
/// at the size GPUI would have given the original: GPUI renders SVG at
/// twice its size, so this one is half the PNG's. `None` unless `png` is a
/// PNG of at most [`MAX_PIXELS`].
///
/// usvg parses this document in the app, but it is one this module wrote:
/// one `<image>` with an embedded PNG, nothing to resolve and nothing to
/// style. Drawn at exactly its pixel size, without smoothing, the PNG is
/// copied as it is.
pub fn wrap(png: &[u8]) -> Option<Vec<u8>> {
    if imagesize::image_type(png).ok()? != imagesize::ImageType::Png {
        return None;
    }
    let size = imagesize::blob_size(png).ok()?;
    let (width, height) = (size.width as u64, size.height as u64);
    if width == 0 || height == 0 || width.saturating_mul(height) > MAX_PIXELS {
        return None;
    }
    let (width, height) = (width as f64 / 2., height as f64 / 2.);
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    Some(
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><image width="{width}" height="{height}" preserveAspectRatio="none" image-rendering="optimizeSpeed" xlink:href="data:image/png;base64,{data}"/></svg>"#
        )
        .into_bytes(),
    )
}

/// SVGs the helper's tests draw, here so that the integration tests, which
/// compile this file alone, draw the same ones.
#[cfg(test)]
pub mod test_svgs {
    /// 666 bytes usvg never finishes, though the image guard lets them
    /// through: a style rule whose selector it matches by trying every set
    /// of a shape's 60 ancestors, 2⁶⁰ of them. The guard leaves what usvg
    /// spends time on, rather than stack, to the helper's time limit.
    pub fn hang() -> Vec<u8> {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="9" height="9"><style>{}rect{{fill:red}}</style>{}<rect width="9" height="9"/>{}</svg>"#,
            "g ".repeat(61),
            "<g>".repeat(60),
            "</g>".repeat(60)
        )
        .into_bytes()
    }

    /// `links` patterns, each filling a shape `levels` groups deep with the
    /// next. usvg draws each one's content from where the last is drawn,
    /// recursively.
    pub fn pattern_chain(links: usize, levels: usize) -> Vec<u8> {
        let mut svg =
            String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">"#);
        for link in 0..links {
            let fill = if link + 1 == links {
                "#f00".to_string()
            } else {
                format!("url(#p{})", link + 1)
            };
            svg.push_str(&format!(
                r#"<pattern id="p{link}" width="4" height="4" patternUnits="userSpaceOnUse">{}<rect width="4" height="4" fill="{fill}"/>{}</pattern>"#,
                "<g>".repeat(levels),
                "</g>".repeat(levels)
            ));
        }
        svg.push_str(r##"<rect width="4" height="4" fill="url(#p0)"/></svg>"##);
        svg.into_bytes()
    }

    /// The deepest [`pattern_chain`] of 50-group links the image guard lets
    /// through, as the helper's tests use it.
    pub const DEEPEST_LINKS: usize = 18;
    pub const DEEPEST_LEVELS: usize = 50;
}
