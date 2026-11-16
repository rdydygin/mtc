//! Rasterizing untrusted SVG in a short-lived helper process: the helper.
//!
//! The app starts its own executable with [`HELPER_ARG`], before anything
//! else in `main` runs, writes one SVG to its stdin and reads one PNG from
//! its stdout (see [`crate::svg_process`]). The helper renders with GPUI's
//! own renderer, so with the options, fonts and size GPUI uses in the app.
//! It checks the image again as the app does, and ends itself once its time
//! is up, even if the app that started it is gone.

use std::{
    ffi::OsStr,
    io::{Read as _, Write as _},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{DevicePixels, SvgRenderer, SvgSize, size};
use image::ImageEncoder as _;

use crate::{
    assets::AppAssets,
    image_guard::is_safe_image,
    svg_process::{GRACE, HELPER_ARG, MAX_INPUT, MAX_PIXELS, TIMEOUT, needs_helper},
};

/// Runs the helper and exits, if this process was started as one.
pub fn run_if_requested() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(HELPER_ARG)) {
        return;
    }
    let lifetime = args
        .next()
        .and_then(|arg| arg.to_str()?.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(TIMEOUT + GRACE);
    // The watchdog ends the process however the renderer is stuck.
    std::thread::spawn(move || {
        std::thread::sleep(lifetime);
        std::process::exit(2);
    });
    limit_memory();
    std::process::exit(if serve().is_some() { 0 } else { 1 });
}

fn serve() -> Option<()> {
    let mut svg = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut svg)
        .ok()?;
    if svg.len() > MAX_INPUT {
        return None;
    }
    let png = rasterize(&svg)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&png).ok()?;
    stdout.flush().ok()
}

/// `svg` as a PNG, drawn as GPUI draws an SVG image and at most
/// [`MAX_PIXELS`]; `None` if it isn't SVG GPUI could render, or reads
/// what it mustn't.
pub fn rasterize(svg: &[u8]) -> Option<Vec<u8>> {
    if !needs_helper(svg) || !is_safe_image(svg) {
        return None;
    }
    let renderer = SvgRenderer::new(Arc::new(AppAssets));
    let parsed = renderer.parse_svg(svg).ok()?;
    // What GPUI's image loader calls, which draws at twice the size.
    let mut image = renderer.render_parsed(&parsed, 1.0).ok()?;
    let drawn = image.size(0);
    let (width, height) = (
        i32::from(drawn.width) as u64,
        i32::from(drawn.height) as u64,
    );
    if width * height > MAX_PIXELS {
        let scale = (MAX_PIXELS as f64 / (width * height) as f64).sqrt();
        let width = DevicePixels((width as f64 * scale) as i32);
        image = renderer
            .render_parsed(&parsed, SvgSize::Size(size(width, width)))
            .ok()?;
    }
    let drawn = image.size(0);
    let (width, height) = (
        i32::from(drawn.width) as u32,
        i32::from(drawn.height) as u32,
    );
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    // GPUI keeps BGRA with straight alpha.
    let mut pixels = image.as_bytes(0)?.to_vec();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
    .ok()?;
    Some(png)
}

/// Bounds the helper's memory where the system enforces it. macOS accepts
/// but ignores an address-space limit, and Windows has none per process;
/// there, the time limit bounds what a helper can allocate.
fn limit_memory() {
    #[cfg(target_os = "linux")]
    {
        /// Room for the executable, a full-size 8192² rendering and its PNG.
        const MAX_ADDRESS_SPACE: libc::rlim_t = 2 << 30;
        let limit = libc::rlimit {
            rlim_cur: MAX_ADDRESS_SPACE,
            rlim_max: MAX_ADDRESS_SPACE,
        };
        // SAFETY: `limit` is a valid rlimit that outlives the call.
        unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="6"><rect width="10" height="6" fill="#f00"/></svg>"##;

    fn png_size(png: &[u8]) -> (usize, usize) {
        let size = imagesize::blob_size(png).unwrap();
        (size.width, size.height)
    }

    #[test]
    fn draws_svg_at_twice_its_size_as_gpui_does() {
        let png = rasterize(SVG).unwrap();
        assert_eq!(png_size(&png), (20, 12));
    }

    #[test]
    fn caps_the_pixels_of_a_huge_svg() {
        let huge = br#"<svg xmlns="http://www.w3.org/2000/svg" width="6000" height="3000"><rect width="6000" height="3000"/></svg>"#;
        let png = rasterize(huge).unwrap();
        let (width, height) = png_size(&png);
        assert!((width * height) as u64 <= MAX_PIXELS);
        // The shape survives the cap.
        assert!(width.abs_diff(height * 2) <= 2, "{width}×{height}");
    }

    #[test]
    fn refuses_what_the_app_refuses() {
        let reads_files = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><image width="4" height="4" xlink:href="/etc/passwd"/></svg>"#;
        assert!(rasterize(reads_files).is_none());
        // Raster images never reach the helper, and it won't draw them.
        let png = rasterize(SVG).unwrap();
        assert!(rasterize(&png).is_none());
        assert!(rasterize(b"not an image").is_none());
    }
}
