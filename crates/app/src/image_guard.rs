//! Which image bodies GPUI may render.
//!
//! GPUI decodes the raster formats it recognizes and renders everything else
//! as SVG, with usvg's default options. Those resolve an `<image>` or
//! `<feImage>` link as a local path and read the whole file: `/dev/zero`
//! exhausts memory, a FIFO hangs the loader, and on Windows a UNC path
//! connects to another host. Compressed SVG, at the top level or nested in a
//! `data:` link, is inflated without a limit. Any message or attachment can
//! carry such an image.
//!
//! Which links usvg follows depends on how it renders: a filter applied only
//! to text, for one, is resolved only when fonts are loaded. So the check
//! doesn't ask the renderer. It reads the document the way usvg does and
//! refuses every reference that could leave it, wherever it appears.

use roxmltree::{Document, Node, ParsingOptions};

const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// Whether GPUI may render `body`: a raster image, bytes no renderer
/// accepts, or SVG that refers only to itself and to embedded raster images.
pub fn is_safe_image(body: &[u8]) -> bool {
    const GZIP: [u8; 2] = [0x1f, 0x8b];
    if body.starts_with(&GZIP) {
        return false;
    }
    // Other than gzip, usvg reads only UTF-8 text. Raster formats aren't.
    let Ok(text) = std::str::from_utf8(body) else {
        return true;
    };
    // usvg parses with exactly these options, so entities expand the same
    // way, and a document that fails here fails there before loading anything.
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let Ok(document) = Document::parse_with_options(text, options) else {
        return true;
    };
    document.descendants().all(|node| is_self_contained(&node))
}

fn is_self_contained(node: &Node) -> bool {
    if node.is_text() {
        // CSS can't add links, but its `url(…)` values must stay local too.
        let in_style = node
            .parent_element()
            .is_some_and(|parent| parent.tag_name().name().eq_ignore_ascii_case("style"));
        return !in_style || node.text().is_none_or(urls_are_local);
    }
    node.attributes().all(|attribute| {
        // `href` in any namespace, under any prefix: usvg reads both the
        // SVG 2 form and `xlink:href`.
        let link_ok = attribute.name() != "href"
            || is_local_reference(attribute.value())
            || is_embedded_raster(attribute.value())
            // A hyperlink: usvg draws its content and never follows it.
            || (node.tag_name().name() == "a" && node.tag_name().namespace() == Some(SVG_NS));
        link_ok && urls_are_local(attribute.value())
    })
}

/// A reference to an element of the same document.
fn is_local_reference(value: &str) -> bool {
    value.trim_start().starts_with('#')
}

/// Every CSS `url(…)` in `value` names an element of the same document.
fn urls_are_local(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.match_indices("url(").all(|(start, _)| {
        let target = &lower[start + "url(".len()..];
        let target = target[..target.find(')').unwrap_or(target.len())]
            .trim()
            .trim_matches(['"', '\'']);
        is_local_reference(target)
    })
}

/// A `data:` link usvg turns into a raster image, never into nested SVG.
/// Mirrors usvg's own resolver: the declared type decides, and plain text
/// (also what a link without a type declares) is sniffed.
fn is_embedded_raster(value: &str) -> bool {
    let Ok(url) = data_url::DataUrl::process(value) else {
        // usvg would treat it as a file path.
        return false;
    };
    let Ok((data, _)) = url.decode_to_vec() else {
        return false;
    };
    let mime = format!(
        "{}/{}",
        url.mime_type().type_.as_str(),
        url.mime_type().subtype.as_str()
    );
    match mime.as_str() {
        "image/jpg" | "image/jpeg" | "image/png" | "image/gif" | "image/webp" => true,
        "text/plain" => matches!(
            imagesize::image_type(&data),
            Ok(imagesize::ImageType::Gif
                | imagesize::ImageType::Jpeg
                | imagesize::ImageType::Png
                | imagesize::ImageType::Webp)
        ),
        "image/svg+xml" => false,
        // usvg loads nothing for other types.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    /// `<svg/>`, as base64.
    const SVG: &str = "PHN2Zy8+";

    fn svg(inner: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100">{inner}</svg>"#
        )
    }

    #[test]
    fn self_contained_svgs_render() {
        for inner in [
            r#"<rect width="10" height="10" fill="red"/>"#.to_string(),
            r##"<defs><linearGradient id="g"/><filter id="f"><feGaussianBlur/></filter></defs><rect width="10" height="10" fill="url(#g)" style="filter: url( '#f' )"/>"##.into(),
            r##"<defs><path id="p" d="M0 0h1"/></defs><use href="#p"/><use xlink:href="#p"/>"##.into(),
            r#"<a href="https://example.com/"><text>Link</text></a>"#.into(),
            r#"<a xlink:href="https://example.com/"><rect width="1" height="1"/></a>"#.into(),
            r#"<text>url(file:///etc/passwd) is only words here</text>"#.into(),
            r##"<style>rect { fill: url(#g) }</style>"##.into(),
            // Embedded raster images, declared or sniffed as usvg does.
            format!(r#"<image width="1" height="1" href="data:image/png;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:text/plain;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" xlink:href="data:image/jpeg;base64,{PNG}"/>"#),
        ] {
            assert!(is_safe_image(svg(&inner).as_bytes()), "{inner}");
        }
    }

    #[test]
    fn svgs_that_could_read_files_or_inflate_data_are_refused() {
        for inner in [
            r#"<image width="1" height="1" href="/dev/zero"/>"#.to_string(),
            r#"<image width="1" height="1" xlink:href="/dev/zero"/>"#.into(),
            r#"<image width="1" height="1" href="secret.png"/>"#.into(),
            r#"<image width="1" height="1" href="file:///dev/zero"/>"#.into(),
            r#"<image width="1" height="1" href="\\host\share\a.png"/>"#.into(),
            r#"<image width="1" height="1" href="data:image/png;base64,%%%"/>"#.into(),
            // Only resolved when the text is laid out, so only with fonts.
            r#"<defs><filter id="f"><feImage href="/dev/zero"/></filter></defs><text x="10" y="30" filter="url(#f)">Hello</text>"#.into(),
            r#"<filter id="f"><feImage xlink:href="/dev/zero"/></filter><rect width="1" height="1" filter="url(#f)"/>"#.into(),
            r#"<use href="other.svg#p"/>"#.into(),
            r#"<image width="1" height="1" odd:href="/dev/zero" xmlns:odd="http://www.w3.org/1999/xlink"/>"#.into(),
            r#"<image width="1" height="1" odd:href="/dev/zero" xmlns:odd="urn:anything"/>"#.into(),
            r#"<style>rect { fill: url(file:///dev/zero) }</style>"#.into(),
            r#"<style><![CDATA[rect { filter: URL( "/dev/zero" ) }]]></style>"#.into(),
            r#"<rect width="1" height="1" style="fill: url(/dev/zero)"/>"#.into(),
            r#"<rect width="1" height="1" fill="url(other.svg#g)"/>"#.into(),
            // Nested SVG: declared, or plain text that isn't a raster image.
            format!(r#"<image width="1" height="1" href="data:image/svg+xml;base64,{SVG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:text/plain;base64,{SVG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:;base64,{SVG}"/>"#),
            r#"<image width="1" height="1" href="data:,%1f%8b%08"/>"#.into(),
        ] {
            assert!(!is_safe_image(svg(&inner).as_bytes()), "{inner}");
        }
    }

    #[test]
    fn entities_expand_before_the_check() {
        let doc = |dtd: &str, inner: &str| {
            format!(
                r#"<?xml version="1.0"?><!DOCTYPE svg [{dtd}]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{inner}</svg>"#
            )
        };
        for (dtd, inner) in [
            (
                r#"<!ENTITY p "/dev/zero">"#,
                r#"<image width="1" height="1" href="&p;"/>"#,
            ),
            (
                r#"<!ENTITY i "<image width='1' height='1' href='/dev/zero'/>">"#,
                "&i;",
            ),
            (
                r#"<!ENTITY u "url(/dev/zero)">"#,
                r#"<rect width="1" height="1" fill="&u;"/>"#,
            ),
        ] {
            let text = doc(dtd, inner);
            // The document is valid, so usvg would render it.
            assert!(
                Document::parse_with_options(
                    &text,
                    ParsingOptions {
                        allow_dtd: true,
                        ..ParsingOptions::default()
                    }
                )
                .is_ok()
            );
            assert!(!is_safe_image(text.as_bytes()), "{text}");
        }
    }

    #[test]
    fn other_bodies_pass_through() {
        assert!(!is_safe_image(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]));
        assert!(is_safe_image(&[
            0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'
        ]));
        assert!(is_safe_image(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(is_safe_image(b"GIF89a"));
        assert!(is_safe_image(b"not an image"));
        assert!(is_safe_image(b""));
    }
}
