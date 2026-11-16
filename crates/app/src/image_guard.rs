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

use std::collections::HashMap;

use roxmltree::{Document, Node, NodeId, ParsingOptions};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Attributes usvg resolves a `url(…)` from: paint, clipping, masking,
/// markers and filters. All name elements of the document; `marker` is the
/// CSS shorthand for the three marker properties.
const URL_ATTRIBUTES: [&str; 10] = [
    "fill",
    "stroke",
    "filter",
    "clip-path",
    "mask",
    "marker",
    "marker-start",
    "marker-mid",
    "marker-end",
    "style",
];

/// Elements whose content usvg keeps as written, so an `id` under them is
/// one usvg can link to.
const CONTAINERS: [&str; 10] = [
    "svg", "g", "a", "defs", "switch", "symbol", "marker", "pattern", "mask", "clipPath",
];

/// Elements an `<feImage>` can draw.
const DRAWABLE: [&str; 13] = [
    "svg", "g", "a", "switch", "use", "rect", "circle", "ellipse", "line", "path", "polygon",
    "polyline", "text",
];

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
    // Parsing itself is quadratic in the attributes of one tag and worse in
    // namespace declarations, here and again in usvg, so documents that
    // could cost that much are refused before either parses them.
    if has_crowded_tag(text) || declares_markup_entity(text) || declares_many_namespaces(text) {
        return false;
    }
    // usvg parses with exactly these options, so entities expand the same
    // way, and a document that fails here fails there before loading anything.
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let Ok(document) = Document::parse_with_options(text, options) else {
        return true;
    };
    // Every check below is linear in what it reads, and links are looked up
    // in an index built once, so a large document can't make the check
    // itself the denial of service.
    let ids = Ids::new(&document);
    document
        .descendants()
        .all(|node| is_self_contained(&ids, &node))
}

/// The most attributes, namespace declarations included, a start tag may
/// carry. roxmltree compares each attribute of a tag with every earlier one,
/// and each namespace declaration with every earlier one, so a tag with `n`
/// costs about `n²/2` comparisons. Real SVG elements carry a few dozen at
/// most: every presentation attribute written out, or an editor's root
/// element declaring its namespaces. At this bound the largest body we
/// download, 32 MB of such tags, parses in a second or two, a few times as
/// long as ordinary tags take; one tag of 50 000 took two seconds alone.
const MAX_ATTRIBUTES: usize = 256;

/// Whether some start tag roxmltree reads could carry more than
/// [`MAX_ATTRIBUTES`] attributes. Counts the quoted strings from each `<` to
/// the next `>` outside quotes: every attribute and namespace declaration
/// has exactly one. roxmltree's tags contain no other `<`, not even in a
/// value, so every tag it reads restarts the count, whatever the scan took
/// the text before it for: a comment, CDATA, the DTD. There, the count is
/// only ever too high. One pass over the bytes.
fn has_crowded_tag(text: &str) -> bool {
    let mut in_markup = false;
    let mut quote = None;
    let mut strings = 0;
    for byte in text.bytes() {
        match (byte, quote) {
            (b'<', _) => {
                in_markup = true;
                quote = None;
                strings = 0;
            }
            (_, Some(open)) if byte == open => quote = None,
            (_, Some(_)) => {}
            (b'>', None) => in_markup = false,
            (b'"' | b'\'', None) if in_markup => {
                quote = Some(byte);
                strings += 1;
                if strings > MAX_ATTRIBUTES {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether an `<!ENTITY` declaration's value holds markup. roxmltree parses
/// an entity's value as content wherever it is referenced, so its tags
/// aren't in the body as written, and a tag cut off by the end of the value
/// leaves its attributes to the next tag: referencing many such entities
/// builds one tag of any size. SVG uses entities for namespace names and
/// the like, not for markup, so any `<` in a value refuses the document.
///
/// The value is the first quoted string after `<!ENTITY`: the name and the
/// optional `%` before it hold no quote. Both searches stop at the next `<`,
/// before the next declaration, so each byte is read at most twice.
fn declares_markup_entity(text: &str) -> bool {
    text.match_indices("<!ENTITY").any(|(start, _)| {
        let declaration = &text[start + "<!ENTITY".len()..];
        let Some(open) = declaration.find(['<', '"', '\'']) else {
            return false;
        };
        let quote = declaration.as_bytes()[open];
        quote != b'<'
            && declaration[open + 1..]
                .bytes()
                .find(|&byte| byte == quote || byte == b'<')
                == Some(b'<')
    })
}

/// The most namespace declarations a document may make. roxmltree gives
/// each element that declares one a copy of every namespace in scope,
/// checking each copy against those already made, and finds the namespace
/// of every element and prefixed attribute by walking all in scope. With
/// `n` declarations in all, at most `n` elements declare, at most `n + 1`
/// namespaces are ever in scope, and parsing costs about `n³` steps plus
/// `n` per element and attribute. Editors declare a handful, mostly on the
/// root. At this bound the worst body we download parses in under half a
/// second, twice as long as with none; 3 000 nested elements declaring one
/// each, 80 KB, took twelve seconds.
const MAX_NAMESPACE_DECLARATIONS: usize = 64;

/// Whether the document might make more than [`MAX_NAMESPACE_DECLARATIONS`]
/// namespace declarations. Counts `xmlns` wherever it appears: every
/// declaration is an attribute named `xmlns` or `xmlns:…`, written out in
/// the body because entities hold no markup.
fn declares_many_namespaces(text: &str) -> bool {
    text.matches("xmlns")
        .nth(MAX_NAMESPACE_DECLARATIONS)
        .is_some()
}

/// The elements usvg can link to by `id`, found in one pass.
struct Ids<'a> {
    targets: HashMap<&'a str, Target>,
}

struct Target {
    /// How many elements carry the id; usvg's choice among several is its
    /// implementation's.
    elements: usize,
    /// Whether the (first) element is one an `<feImage>` can draw: usvg
    /// keeps it, it can be drawn, and it has a single id-like attribute.
    drawable: bool,
}

impl<'a> Ids<'a> {
    fn new(document: &'a Document) -> Self {
        let svg_element = |node: &Node, names: &[&str]| {
            node.is_element()
                && matches!(node.tag_name().namespace(), None | Some(SVG_NS))
                && names.contains(&node.tag_name().name())
        };
        // Whether every element above a node is a container usvg keeps as
        // written. Descendants come after their parent, so each node's
        // answer extends its parent's.
        let mut kept: HashMap<NodeId, bool> = HashMap::new();
        let mut targets: HashMap<&'a str, Target> = HashMap::new();
        for node in document.descendants().filter(Node::is_element) {
            let parent_kept = node.parent().is_none_or(|parent| {
                !parent.is_element()
                    || (kept.get(&parent.id()).copied().unwrap_or(false)
                        && svg_element(&parent, &CONTAINERS))
            });
            kept.insert(node.id(), parent_kept);
            // usvg indexes only an element's first id-like attribute; with
            // one, in any namespace, there is no choice to get wrong.
            let single_id = node
                .attributes()
                .filter(|attribute| attribute.name() == "id")
                .count()
                == 1;
            let drawable = single_id && parent_kept && svg_element(&node, &DRAWABLE);
            for attribute in node.attributes() {
                let recognized = attribute.name() == "id"
                    && matches!(
                        attribute.namespace(),
                        None | Some(SVG_NS) | Some(XLINK_NS) | Some(XML_NS)
                    );
                if recognized {
                    let target = targets.entry(attribute.value()).or_insert(Target {
                        elements: 0,
                        drawable,
                    });
                    target.elements += 1;
                }
            }
        }
        Self { targets }
    }

    /// Whether `id` names exactly one element, one an `<feImage>` can draw.
    fn is_drawable(&self, id: &str) -> bool {
        self.targets
            .get(id)
            .is_some_and(|target| target.elements == 1 && target.drawable)
    }
}

fn is_self_contained(ids: &Ids, node: &Node) -> bool {
    if node.is_text() {
        // CSS can only set presentation attributes, never a link, but its
        // `url(…)` values must stay local too.
        let in_style = node
            .parent_element()
            .is_some_and(|parent| parent.tag_name().name().eq_ignore_ascii_case("style"));
        return !in_style || node.text().is_none_or(urls_are_local);
    }
    let element = node.tag_name().name();
    node.attributes().all(|attribute| {
        let name = attribute.name();
        let value = attribute.value();
        if name == "href" {
            // In any namespace, under any prefix: usvg reads both the SVG 2
            // form and `xlink:href`. Only `<image>` and `<feImage>` load
            // what their link names, and usvg reads anything that isn't a
            // `data:` link as a file path: for `<image>` even a fragment,
            // for `<feImage>` a fragment naming no element usvg keeps.
            let link_ok = match element {
                "image" => is_embedded_raster(value),
                "feImage" => is_embedded_raster(value) || links_to_drawable(ids, value),
                // A hyperlink: usvg draws its content and never follows it.
                "a" if node.tag_name().namespace() == Some(SVG_NS) => true,
                _ => is_local_reference(value) || is_embedded_raster(value),
            };
            if !link_ok {
                return false;
            }
        }
        !URL_ATTRIBUTES.contains(&name) || urls_are_local(value)
    })
}

/// A reference to an element of the same document.
fn is_local_reference(value: &str) -> bool {
    value.trim_start().starts_with('#')
}

/// Whether `value` links to one element that usvg keeps and can draw, the
/// only fragment `<feImage>` doesn't treat as a path. Parses the link as
/// svgtypes' IRI does; `ids` records ids as usvg's tree does.
fn links_to_drawable(ids: &Ids, value: &str) -> bool {
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
    let Some(rest) = value.trim_start_matches(is_space).strip_prefix('#') else {
        return false;
    };
    let (id, tail) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
    !id.is_empty() && tail.trim_start_matches(is_space).is_empty() && ids.is_drawable(id)
}

/// Every CSS `url(…)` in `value` names an element of the same document:
/// after optional whitespace, quotes and whitespace, a `#`. One pass; the
/// prefix read for each `url(` ends before the next one begins.
fn urls_are_local(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.match_indices("url(").all(|(start, _)| {
        let target = lower[start + "url(".len()..]
            .trim_start()
            .trim_start_matches(['"', '\''])
            .trim_start();
        target.starts_with('#')
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

    /// SVGs GPUI may render; none of them loads anything.
    fn self_contained() -> Vec<String> {
        [
            r#"<rect width="10" height="10" fill="red"/>"#.to_string(),
            r##"<defs><linearGradient id="g"/><filter id="f"><feGaussianBlur/></filter></defs><rect width="10" height="10" fill="url(#g)" style="filter: url( '#f' )"/>"##.into(),
            r##"<defs><path id="p" d="M0 0h1"/></defs><use href="#p"/><use xlink:href="#p"/>"##.into(),
            r##"<use href="#missing"/>"##.into(),
            r#"<a href="https://example.com/"><text>Link</text></a>"#.into(),
            r#"<a xlink:href="https://example.com/"><rect width="1" height="1"/></a>"#.into(),
            r#"<text>url(file:///etc/passwd) is only words here</text>"#.into(),
            r##"<style>rect { fill: url(#g) }</style>"##.into(),
            // Metadata that only looks like a link.
            r#"<rect width="1" height="1" aria-label="url(https://example.com)"/>"#.into(),
            r#"<rect width="1" height="1" data-source="url(file:///tmp/x)"/>"#.into(),
            // feImage drawing an element of the document.
            r##"<defs><rect id="r" width="1" height="1"/></defs><filter id="f"><feImage href="#r"/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            r##"<g><rect id="r" width="1" height="1"/></g><filter id="f"><feImage xlink:href=" #r "/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            // Embedded raster images, declared or sniffed as usvg does.
            format!(r#"<image width="1" height="1" href="data:image/png;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:text/plain;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:;base64,{PNG}"/>"#),
            format!(r#"<image width="1" height="1" xlink:href="data:image/jpeg;base64,{PNG}"/>"#),
            format!(r#"<filter id="f"><feImage href="data:image/png;base64,{PNG}"/></filter><rect width="1" height="1" filter="url(#f)"/>"#),
        ]
        .into_iter()
        .map(|inner| svg(&inner))
        .collect()
    }

    /// SVGs that would make GPUI read a file or parse nested SVG.
    fn loading() -> Vec<String> {
        [
            r#"<image width="1" height="1" href="/dev/zero"/>"#.to_string(),
            r#"<image width="1" height="1" xlink:href="/dev/zero"/>"#.into(),
            r#"<image width="1" height="1" href="secret.png"/>"#.into(),
            r#"<image width="1" height="1" href="file:///dev/zero"/>"#.into(),
            r#"<image width="1" height="1" href="\\host\share\a.png"/>"#.into(),
            // `<image>` reads its link as a path even when it is a fragment.
            r##"<image width="1" height="1" href="#missing.svgz"/>"##.into(),
            r##"<defs><rect id="r" width="1" height="1"/></defs><image width="1" height="1" href="#r"/>"##.into(),
            // An feImage fragment that names nothing usvg keeps is read as a path.
            r##"<filter id="f"><feImage href="#missing"/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            r##"<filter id="f"><feImage href="# r"/></filter><rect id="r" width="1" height="1" filter="url(#f)"/>"##.into(),
            r##"<unknown><rect id="r" width="1" height="1"/></unknown><filter id="f"><feImage href="#r"/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            r##"<x:rect xmlns:x="urn:x" id="r"/><filter id="f"><feImage href="#r"/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            r##"<style id="r"/><filter id="f"><feImage href="#r"/></filter><rect width="1" height="1" filter="url(#f)"/>"##.into(),
            // Only resolved when the text is laid out, so only with fonts.
            r#"<defs><filter id="f"><feImage href="/dev/zero"/></filter></defs><text x="10" y="30" font-family="MtcTest" filter="url(#f)">A</text>"#.into(),
            // usvg indexes only an element's first id-like attribute, so a
            // fragment naming a later one names nothing usvg keeps.
            r##"<rect id="visible" xml:id="r" width="1" height="1"/><filter id="f"><feImage href="#r"/></filter><rect width="10" height="10" filter="url(#f)"/>"##.into(),
            r##"<rect xml:id="visible" id="r" width="1" height="1"/><filter id="f"><feImage href="#r"/></filter><rect width="10" height="10" filter="url(#f)"/>"##.into(),
            r##"<rect xlink:id="visible" xml:id="r" width="1" height="1"/><filter id="f"><feImage href="#r"/></filter><rect width="10" height="10" filter="url(#f)"/>"##.into(),
            r##"<rect svg:id="visible" id="r" xmlns:svg="http://www.w3.org/2000/svg" width="1" height="1"/><filter id="f"><feImage href="#r"/></filter><rect width="10" height="10" filter="url(#f)"/>"##.into(),
            r#"<filter id="f"><feImage xlink:href="/dev/zero"/></filter><rect width="1" height="1" filter="url(#f)"/>"#.into(),
            r#"<image width="1" height="1" odd:href="/dev/zero" xmlns:odd="http://www.w3.org/1999/xlink"/>"#.into(),
            // Nested SVG: declared, or plain text that isn't a raster image.
            format!(r#"<image width="1" height="1" href="data:image/svg+xml;base64,{SVG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:text/plain;base64,{SVG}"/>"#),
            format!(r#"<image width="1" height="1" href="data:;base64,{SVG}"/>"#),
            r#"<image width="1" height="1" href="data:,%1f%8b%08"/>"#.into(),
        ]
        .into_iter()
        .map(|inner| svg(&inner))
        .collect()
    }

    /// Refused although usvg would load nothing: references that leave the
    /// document where no renderer should need them.
    fn needlessly_external() -> Vec<String> {
        [
            r#"<use href="other.svg#p"/>"#.to_string(),
            r#"<image width="1" height="1" href="data:image/png;base64,%%%"/>"#.into(),
            r#"<image width="1" height="1" odd:href="/dev/zero" xmlns:odd="urn:anything"/>"#.into(),
            r#"<style>rect { fill: url(file:///dev/zero) }</style>"#.into(),
            r#"<style><![CDATA[rect { filter: URL( "/dev/zero" ) }]]></style>"#.into(),
            r#"<rect width="1" height="1" style="fill: url(/dev/zero)"/>"#.into(),
            r#"<rect width="1" height="1" fill="url(other.svg#g)"/>"#.into(),
            r#"<path d="M0 0h1" marker-end="url(file:///m.svg#m)"/>"#.into(),
            r#"<rect width="1" height="1" clip-path="url(/c.svg#c)"/>"#.into(),
        ]
        .into_iter()
        .map(|inner| svg(&inner))
        .collect()
    }

    #[test]
    fn self_contained_svgs_render() {
        for svg in self_contained() {
            assert!(is_safe_image(svg.as_bytes()), "{svg}");
        }
    }

    #[test]
    fn svgs_that_could_read_files_or_inflate_data_are_refused() {
        for svg in loading().into_iter().chain(needlessly_external()) {
            assert!(!is_safe_image(svg.as_bytes()), "{svg}");
        }
    }

    /// Only the bundled test font (family `MtcTest`, one glyph: `A`), so
    /// text is laid out the same way on every machine, with or without
    /// system fonts. See `testdata/README.md`.
    static FONTS: std::sync::LazyLock<std::sync::Arc<usvg::fontdb::Database>> =
        std::sync::LazyLock::new(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_font_data(include_bytes!("testdata/mtc-test-font.ttf").to_vec());
            std::sync::Arc::new(fonts)
        });

    /// What GPUI's renderer, usvg with fonts loaded so text is laid out,
    /// would load for `svg`: file paths, and nested SVG from `data:` links.
    fn renderer_loads(svg: &str) -> Vec<String> {
        use std::sync::{Arc, Mutex};
        let loads = Arc::new(Mutex::new(Vec::new()));
        let (paths, data) = (loads.clone(), loads.clone());
        let default_data = usvg::ImageHrefResolver::default_data_resolver();
        let options = usvg::Options {
            fontdb: FONTS.clone(),
            image_href_resolver: usvg::ImageHrefResolver {
                resolve_string: Box::new(move |href, _| {
                    paths.lock().unwrap().push(format!("path {href}"));
                    None
                }),
                resolve_data: Box::new(move |mime, bytes, options| {
                    let kind = default_data(mime, bytes, options);
                    // usvg parses these as SVG unless they turn out raster,
                    // whether or not parsing then succeeds.
                    let raster = matches!(
                        kind,
                        Some(
                            usvg::ImageKind::JPEG(_)
                                | usvg::ImageKind::PNG(_)
                                | usvg::ImageKind::GIF(_)
                                | usvg::ImageKind::WEBP(_)
                        )
                    );
                    if matches!(mime, "image/svg+xml" | "text/plain") && !raster {
                        data.lock().unwrap().push(format!("nested {mime}"));
                    }
                    kind
                }),
            },
            ..usvg::Options::default()
        };
        let _ = usvg::Tree::from_str(svg, &options);
        loads.lock().unwrap().clone()
    }

    /// The guard against the renderer itself: nothing it lets through loads
    /// anything, and the attacks it refuses really would.
    #[test]
    fn the_guard_agrees_with_the_renderer() {
        let every = self_contained()
            .into_iter()
            .chain(loading())
            .chain(needlessly_external());
        for svg in every {
            if is_safe_image(svg.as_bytes()) {
                assert_eq!(renderer_loads(&svg), Vec::<String>::new(), "{svg}");
            }
        }
        for svg in loading() {
            assert!(!renderer_loads(&svg).is_empty(), "{svg}");
        }
    }

    #[test]
    fn the_test_font_lays_out_text() {
        let face = FONTS.faces().next().expect("the test font loads");
        assert_eq!(face.families[0].0, "MtcTest");
        let text = svg(r#"<text x="10" y="30" font-family="MtcTest" font-size="20">A</text>"#);
        let tree = usvg::Tree::from_str(
            &text,
            &usvg::Options {
                fontdb: FONTS.clone(),
                ..usvg::Options::default()
            },
        )
        .unwrap();
        assert!(tree.root().has_children(), "the text was laid out");
    }

    /// Large legal documents: every link is looked up in one index, every
    /// ancestor chain is walked once, and each `url(` reads only its own
    /// prefix. Done per reference, these took billions of steps.
    #[test]
    fn large_documents_are_checked_in_linear_time() {
        const REFERENCES: usize = 50_000;
        const DEPTH: usize = 5_000;
        let started = std::time::Instant::now();

        // Many feImages naming one rect, inside unused <defs>.
        let mut many = String::from(r#"<defs><rect id="r" width="1" height="1"/><filter id="f">"#);
        for _ in 0..REFERENCES {
            many.push_str(r##"<feImage href="#r"/>"##);
        }
        many.push_str("</filter></defs>");
        assert!(is_safe_image(svg(&many).as_bytes()));
        // One reference that names nothing still fails the whole document.
        let broken = many.replacen(r##"href="#r""##, r##"href="#missing""##, 1);
        assert!(!is_safe_image(svg(&broken).as_bytes()));

        // A deep chain of groups, each named by its own feImage.
        let mut deep = String::new();
        for level in 0..DEPTH {
            deep.push_str(&format!(r#"<g id="g{level}">"#));
        }
        deep.push_str(&"</g>".repeat(DEPTH));
        deep.push_str(r#"<filter id="f">"#);
        for level in 0..DEPTH {
            deep.push_str(&format!(r##"<feImage href="#g{level}"/>"##));
        }
        deep.push_str("</filter>");
        assert!(is_safe_image(svg(&deep).as_bytes()));

        // A megabyte of `url(` without a closing parenthesis, every one
        // naming a fragment; then the same with one external target last.
        let urls = "url(#".repeat(200_000);
        let local = format!(r#"<rect width="1" height="1" fill="{urls}"/>"#);
        assert!(is_safe_image(svg(&local).as_bytes()));
        let external = format!(r#"<rect width="1" height="1" fill="{urls}url(/x"/>"#);
        assert!(!is_safe_image(svg(&external).as_bytes()));

        // Generous for an unoptimized build on a slow machine; the quadratic
        // check took minutes.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    /// A rect with `count` attributes in all, each value holding what could
    /// throw a count off: `>`, `=` and the other quote.
    fn rect_with_attributes(count: usize) -> String {
        let attributes: String = (2..count)
            .map(|i| format!(r#" data-{i}='a>b="c"=d'"#))
            .collect();
        format!(r#"<rect width="1" height="1"{attributes}/>"#)
    }

    #[test]
    fn crowded_tags_are_refused_before_parsing() {
        let attributes: String = (0..50_000).map(|i| format!(" a{i}=''")).collect();
        let crowded = svg(&format!("<rect{attributes}/>"));
        let declarations: String = (0..50_000)
            .map(|i| format!(" xmlns:p{i}='urn:{i}'"))
            .collect();
        let declaring = svg(&format!("<g{declarations}/>"));
        let started = std::time::Instant::now();
        assert!(!is_safe_image(crowded.as_bytes()));
        assert!(!is_safe_image(declaring.as_bytes()));
        // Half a megabyte each; parsing either took two seconds, growing
        // with the square of the size.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn attributes_are_counted_per_tag_whatever_comes_before() {
        let full = rect_with_attributes(MAX_ATTRIBUTES);
        let over = rect_with_attributes(MAX_ATTRIBUTES + 1);
        // Each leaves the count thinking it is inside quotes or outside
        // markup; neither may hide the tag after it.
        for before in [
            "",
            r#"<!-- it's "quoted -->"#,
            r#"<text><![CDATA[ ' " ]]></text>"#,
            r#"<?pi " ?>"#,
            r#"<text>"it's" = 'x' > y</text>"#,
            &format!("<!-- {} -->", "=".repeat(1_000)),
            &format!(
                "<style>{}</style>",
                r#"rect { font-family: "A" } "#.repeat(1_000)
            ),
            &rect_with_attributes(MAX_ATTRIBUTES),
        ] {
            let fits = svg(&format!("{before}{full}"));
            assert!(is_safe_image(fits.as_bytes()), "{fits}");
            let crowded = svg(&format!("{before}{over}"));
            assert!(!is_safe_image(crowded.as_bytes()), "{crowded}");
        }

        let doc = |dtd: &str, inner: &str| {
            format!(
                r#"<!DOCTYPE svg [{dtd}]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{inner}</svg>"#
            )
        };
        let dtd = r#"<!ENTITY q '"'><!-- ' --><!ENTITY a "x>y">"#;
        assert!(is_safe_image(doc(dtd, &full).as_bytes()));
        assert!(!is_safe_image(doc(dtd, &over).as_bytes()));
    }

    #[test]
    fn many_namespace_declarations_are_refused_before_parsing() {
        const DEPTH: usize = 3_000;
        let mut nested = String::new();
        for level in 0..DEPTH {
            nested.push_str(&format!("<g xmlns:p{level}='urn:{level}'>"));
        }
        nested.push_str(&"</g>".repeat(DEPTH));
        let nested = svg(&nested);
        // A few declarations on the root, then many children each copying
        // them all to declare one more.
        let siblings = svg(&r#"<g xmlns:x="urn:x"/>"#.repeat(100_000));
        let started = std::time::Instant::now();
        assert!(!is_safe_image(nested.as_bytes()));
        assert!(!is_safe_image(siblings.as_bytes()));
        // Parsing the nested one, 80 KB, took twelve seconds.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );

        // `svg` declares two on the root.
        let declaring = |count: usize| {
            let declarations: String = (2..count)
                .map(|i| format!(" xmlns:p{i}='urn:{i}'"))
                .collect();
            svg(&format!("<g{declarations}/>"))
        };
        assert!(is_safe_image(
            declaring(MAX_NAMESPACE_DECLARATIONS).as_bytes()
        ));
        assert!(!is_safe_image(
            declaring(MAX_NAMESPACE_DECLARATIONS + 1).as_bytes()
        ));
    }

    #[test]
    fn editor_namespaces_still_render() {
        // As Inkscape writes them, metadata and all.
        let inkscape = r##"<svg xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:cc="http://creativecommons.org/ns#" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:svg="http://www.w3.org/2000/svg" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" width="10" height="10" inkscape:version="1.3" sodipodi:docname="a.svg">
            <sodipodi:namedview id="base" pagecolor="#ffffff" inkscape:zoom="1"/>
            <metadata><rdf:RDF><cc:Work rdf:about=""><dc:format>image/svg+xml</dc:format></cc:Work></rdf:RDF></metadata>
            <g inkscape:label="Layer 1" inkscape:groupmode="layer"><rect width="1" height="1"/></g>
        </svg>"##;
        assert!(is_safe_image(inkscape.as_bytes()));
        // Icons pasted in with their own declarations.
        let icons = svg(&r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1"/></svg>"#.repeat(20));
        assert!(is_safe_image(icons.as_bytes()));
    }

    #[test]
    fn entities_holding_markup_are_refused() {
        let doc = |dtd: &str, inner: &str| {
            format!(
                r#"<!DOCTYPE svg [{dtd}]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{inner}</svg>"#
            )
        };
        // roxmltree leaves the attributes of a tag cut off by the end of an
        // entity to the next tag, so tags no bound sees can grow without one.
        let pieces = doc(
            r#"<!ENTITY a "<x a1='1' a2='2'"><!ENTITY b "<y b1='1'">"#,
            "&a;&b;<g c='3'/>",
        );
        let options = ParsingOptions {
            allow_dtd: true,
            ..ParsingOptions::default()
        };
        let parsed = Document::parse_with_options(&pieces, options).unwrap();
        let g = parsed
            .descendants()
            .find(|node| node.has_tag_name("g"))
            .unwrap();
        let names: Vec<_> = g.attributes().map(|attribute| attribute.name()).collect();
        assert_eq!(names, ["a1", "a2", "b1", "c"]);
        assert!(!is_safe_image(pieces.as_bytes()));

        // Many such entities: one tag of 100 000 attributes, every entity
        // within the bound on its own. Parsing it took five seconds.
        let (mut dtd, mut references) = (String::new(), String::new());
        for entity in 0..500 {
            dtd.push_str(&format!(r#"<!ENTITY e{entity} "<x"#));
            for attribute in 0..200 {
                dtd.push_str(&format!(" a{entity}_{attribute}=''"));
            }
            dtd.push_str(r#"">"#);
            references.push_str(&format!("&e{entity};"));
        }
        let built = doc(&dtd, &format!("{references}<g/>"));
        let started = std::time::Instant::now();
        assert!(!is_safe_image(built.as_bytes()));
        assert!(started.elapsed() < std::time::Duration::from_millis(500));

        // Entities as SVG editors use them, for names, still render.
        let names = r##"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd" [
            <!ENTITY ns_svg "http://www.w3.org/2000/svg">
            <!ENTITY ns_xlink 'http://www.w3.org/1999/xlink'>
            <!ENTITY red "#f00">
        ]><svg xmlns="&ns_svg;" xmlns:xlink="&ns_xlink;" width="1" height="1"><rect width="1" height="1" fill="&red;"/></svg>"##;
        assert!(is_safe_image(names.as_bytes()));
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
