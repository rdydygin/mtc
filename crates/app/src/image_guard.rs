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

use std::collections::{HashMap, HashSet};

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
    // Parsing costs more than one pass over the body in places, here and
    // again in usvg, so a document that could cost too much there is
    // refused before either parses it.
    if parse_cost(text).is_some() {
        return false;
    }
    // usvg parses with exactly these options, so entities expand the same
    // way, and a document that fails here fails there before loading anything.
    let Ok(document) = Document::parse_with_options(text, parsing_options()) else {
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

fn parsing_options<'a>() -> ParsingOptions<'a> {
    ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    }
}

/// What would make a document too costly to read: work that grows faster
/// than its size, in roxmltree's parser or in usvg.
#[derive(Debug, PartialEq, Eq)]
enum Cost {
    /// An entity's value holds markup; see [`Entities::markup`].
    MarkupEntity,
    /// Resolving entity references; see [`Entities::work`].
    EntityReferences,
    /// A tag with more than [`MAX_ATTRIBUTES`] attributes, or tags whose
    /// attributes take more than [`MAX_ATTRIBUTE_PAIRS`] comparisons.
    CrowdedTag,
    /// Comparing the namespace names of prefixed attributes; see
    /// [`Markup::prefixed_pairs`].
    PrefixedAttributes,
    /// Copying and searching the namespaces in scope; see
    /// [`Markup::namespace_lookups`] and [`Markup::distinct_namespaces`].
    NamespaceScopes,
}

/// What would make parsing `text` too costly, read in linear time before
/// parsing it. The entities come first: the markup scan relies on them
/// holding none.
fn parse_cost(text: &str) -> Option<Cost> {
    let entities = Entities::scan(text);
    if entities.markup {
        return Some(Cost::MarkupEntity);
    }
    let (lookups, expanded) = entities.work(text);
    if entities.nested || lookups > MAX_ENTITY_WORK || expanded > MAX_ENTITY_TEXT {
        return Some(Cost::EntityReferences);
    }
    let markup = Markup::scan(text, entities.longest);
    if markup.widest_tag > MAX_ATTRIBUTES || markup.attribute_pairs > MAX_ATTRIBUTE_PAIRS {
        return Some(Cost::CrowdedTag);
    }
    // Attributes with the `xml` prefix are compared by its namespace name.
    let uri = markup.longest_uri.max(XML_NS.len());
    if markup.prefixed_pairs.saturating_mul(uri) > MAX_NAMESPACE_WORK {
        return Some(Cost::PrefixedAttributes);
    }
    // Each comparison reads at most one prefix as long as the longest.
    let lookups = markup.namespace_lookups;
    if lookups.saturating_mul(1 + markup.longest_prefix) > MAX_NAMESPACE_WORK
        || markup.distinct_namespaces > MAX_NAMESPACES
    {
        return Some(Cost::NamespaceScopes);
    }
    None
}

/// The most attributes, namespace declarations included, a start tag may
/// carry. roxmltree compares each attribute of a tag with every earlier one,
/// and each namespace declaration with every earlier one, so a tag with `n`
/// costs about `n²/2` comparisons. Real SVG elements carry a few dozen at
/// most: every presentation attribute written out, or an editor's root
/// element declaring its namespaces.
const MAX_ATTRIBUTES: usize = 128;

/// The most attribute comparisons a document may take, in the units of
/// [`Markup::attribute_pairs`]. A tag at [`MAX_ATTRIBUTES`] costs 16 384,
/// so 32 MB of such tags would cost about two seconds to parse; a typical
/// element has ten or fewer, so 100 000 of them cost ten million. At this
/// bound the comparisons take about a tenth of a second.
const MAX_ATTRIBUTE_PAIRS: usize = 1 << 26;

/// The most work namespace names may take roxmltree, in bytes compared:
/// see [`Markup::prefixed_pairs`] and [`Markup::namespace_lookups`].
/// Inkscape declares eight namespaces on the root and writes a few
/// prefixed attributes on most elements, which costs a few hundred bytes
/// per element: tens of millions for the largest drawings.
const MAX_NAMESPACE_WORK: usize = 1 << 28;

/// The most distinct namespaces, by prefix and value, a document may
/// declare; see [`Markup::distinct_namespaces`]. Editors declare a dozen at
/// most, and pasted icons repeat the same ones. At this bound roxmltree
/// shifts at most eight million entries keeping them sorted; 65 000, in
/// 1.6 MB, shifted two billion.
const MAX_NAMESPACES: usize = 4096;

/// What a document's entity declarations hold, read as roxmltree reads
/// them.
#[derive(Default)]
struct Entities {
    declarations: usize,
    /// The longest value.
    longest: usize,
    /// Whether a value holds markup. roxmltree parses an entity's value as
    /// content wherever it is referenced, so its tags aren't in the body as
    /// written, and a tag cut off by the end of the value leaves its
    /// attributes to the next tag: referencing many such entities builds
    /// one tag of any size. SVG uses entities for namespace names and the
    /// like, not for markup, so any `<` in a value refuses the document.
    markup: bool,
    /// Whether a value references another entity. That multiplies the work
    /// of [`Entities::work`] by up to 255 for every reference to it, without
    /// a single extra declaration; SVG editors never nest entities, so it
    /// refuses the document outright.
    nested: bool,
}

/// The most work looking up entities may take, in name bytes compared; see
/// [`Entities::work`]. Illustrator declares a dozen namespace names as
/// entities, and its older files one per distinct style, referenced from
/// every element: a few thousand references among fifty entities cost a
/// few hundred thousand. At this bound the lookups take a few hundredths
/// of a second.
const MAX_ENTITY_WORK: usize = 1 << 26;

/// The most text entity references may expand to, in bytes; see
/// [`Entities::work`]. Illustrator's style entities, a hundred bytes each
/// on 50 000 elements, expand to 5 MB. Expanded text costs usvg about what
/// text in the body does: at this bound, a few hundredths of a second.
const MAX_ENTITY_TEXT: usize = 1 << 23;

impl Entities {
    fn scan(text: &str) -> Self {
        let mut entities = Self::default();
        for value in entity_values(text) {
            entities.declarations += 1;
            entities.longest = entities.longest.max(value.len());
            entities.markup |= value.contains('<');
            entities.nested |= entity_references(value).0 > 0;
        }
        entities
    }

    /// What resolving the references in `text` costs: the lookups, then the
    /// text they expand to. roxmltree finds a referenced entity by comparing
    /// its name with each declaration's in turn, at most its length plus one
    /// per comparison, then reads the value in its place. So `r` references
    /// with names of `b` bytes in all, among `d` declarations whose longest
    /// value is `l`, cost `d·(r + b)` and `r·l` at most, unless entities
    /// nest.
    fn work(&self, text: &str) -> (usize, usize) {
        let (references, names) = entity_references(text);
        let lookups = self
            .declarations
            .saturating_mul(references.saturating_add(names));
        (lookups, references.saturating_mul(self.longest))
    }
}

/// The value of each `<!ENTITY` declaration: the first quoted string after
/// it, as the name and the optional `%` before it hold no quote. The search
/// for the opening quote stops at the next `<`, before the next
/// declaration, and no value starts inside another that uses the same
/// quote, so each byte is read at most three times.
fn entity_values(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices("<!ENTITY").filter_map(|(start, _)| {
        let declaration = &text[start + "<!ENTITY".len()..];
        let open = declaration.find(['<', '"', '\''])?;
        let quote = declaration[open..].chars().next()?;
        if quote == '<' {
            return None;
        }
        let value = &declaration[open + 1..];
        Some(&value[..value.find(quote).unwrap_or(value.len())])
    })
}

/// How many entity references `text` holds, and their names' length in all.
/// Every `&` starts one in a valid document, outside comments and CDATA,
/// but character references and the five names XML predefines resolve
/// without a lookup. Each piece between two `&` is read once.
fn entity_references(text: &str) -> (usize, usize) {
    const PREDEFINED: [&str; 5] = ["lt;", "gt;", "amp;", "apos;", "quot;"];
    text.split('&')
        .skip(1)
        .filter(|after| {
            !after.starts_with('#') && !PREDEFINED.iter().any(|name| after.starts_with(name))
        })
        .fold((0, 0), |(count, names), after| {
            (count + 1, names + after.find(';').unwrap_or(after.len()))
        })
}

/// What a document's tags cost roxmltree beyond reading them once,
/// measured by [`Markup::scan`]. Every sum only grows as the scan goes on,
/// so it bounds the cost of any part of the document roxmltree reads before
/// it fails.
#[derive(Debug, Default, PartialEq, Eq)]
struct Markup {
    /// The most attributes, namespace declarations included, on one tag.
    widest_tag: usize,
    /// The sum over tags of the square of their attributes, namespace
    /// declarations included: each is compared with every earlier one.
    attribute_pairs: usize,
    /// The sum over tags of the square of their prefixed attributes.
    /// roxmltree checks each attribute of a tag against every earlier one
    /// by namespace name and then local name, and compares the namespace
    /// names in full when both are prefixed: up to the longest namespace
    /// name per pair, even for two in the same namespace.
    prefixed_pairs: usize,
    /// The longest namespace name a declaration gives, an entity reference
    /// in it counted as the longest entity value.
    longest_uri: usize,
    /// The longest prefix a declaration binds.
    longest_prefix: usize,
    /// Prefix comparisons. Each element that declares a namespace gets a
    /// copy of every one in scope, each checked against those it has so far;
    /// each of its declarations is checked against the earlier ones; and
    /// its own prefix and each prefixed attribute's are looked up among all
    /// in scope. Counts at most `inherited × in scope + declared²` for an
    /// element that declares and `(1 + prefixed) × in scope` for every one.
    /// Repeating a declaration on siblings costs little; nesting new ones
    /// makes every element below pay for all.
    namespace_lookups: usize,
    /// Distinct namespaces declared, by prefix and value. roxmltree keeps
    /// each distinct one in a vector sorted for its whole document, so each
    /// new one shifts every later one: their square. Repeating one costs
    /// nothing. Two values written differently may name the same namespace,
    /// so this only ever counts too many.
    distinct_namespaces: usize,
}

impl Markup {
    /// Reads `text` as roxmltree's tokenizer does: comments, CDATA,
    /// processing instructions and the doctype end where roxmltree ends
    /// them, so the tags found are the ones roxmltree reads, and open
    /// elements are tracked to know how many namespaces are in scope.
    /// Where roxmltree would fail, the scan stops, since it parses nothing
    /// after. `longest_entity` is the longest entity value, which an entity
    /// reference in a namespace name can stand for. One pass: every read
    /// ends where the next begins.
    fn scan(text: &str, longest_entity: usize) -> Self {
        let mut markup = Self::default();
        // For each open element, how many namespaces are in scope at most.
        let mut scopes = Vec::new();
        let mut namespaces = HashSet::new();
        let mut at = 0;
        while let Some(offset) = text[at..].find('<') {
            let rest = &text[at + offset..];
            let read = if rest.starts_with("<!--") {
                length_through(rest, "<!--", "-->")
            } else if rest.starts_with("<![CDATA[") {
                length_through(rest, "<![CDATA[", "]]>")
            } else if rest.starts_with("<!DOCTYPE") {
                doctype_length(rest)
            } else if rest.starts_with("<?") {
                length_through(rest, "<?", "?>")
            } else if rest.starts_with("<!") {
                None
            } else if rest.starts_with("</") {
                scopes.pop();
                rest.find('>').map(|end| end + 1)
            } else {
                markup.start_tag(rest, longest_entity, &mut scopes, &mut namespaces)
            };
            let Some(read) = read else {
                break;
            };
            at += offset + read;
        }
        markup
    }

    /// Reads the start tag `text` begins with, as roxmltree does: a name,
    /// then attributes, each a name, `=` and a quoted value that holds no
    /// `<`, then `>` or `/>`. Returns its length, or `None` where roxmltree
    /// would fail, with what roxmltree does before failing charged.
    fn start_tag<'a>(
        &mut self,
        text: &'a str,
        longest_entity: usize,
        scopes: &mut Vec<usize>,
        namespaces: &mut HashSet<(&'a str, &'a str)>,
    ) -> Option<usize> {
        let bytes = text.as_bytes();
        let space = |at: usize| bytes.get(at).is_some_and(|byte| XML_SPACE.contains(byte));
        let name_byte = |at: usize| {
            bytes
                .get(at)
                .is_some_and(|byte| !XML_SPACE.contains(byte) && !b"=/<>\"'".contains(byte))
        };
        let mut at = 1;
        while name_byte(at) {
            at += 1;
        }
        let (mut attributes, mut prefixed, mut declarations) = (0, 0, 0);
        let open = loop {
            while space(at) {
                at += 1;
            }
            match *bytes.get(at)? {
                b'>' => break true,
                b'/' if bytes.get(at + 1) == Some(&b'>') => {
                    at += 1;
                    break false;
                }
                _ => {}
            }
            let name_start = at;
            while name_byte(at) {
                at += 1;
            }
            let name = &text[name_start..at];
            while space(at) {
                at += 1;
            }
            if name.is_empty() || bytes.get(at) != Some(&b'=') {
                return None;
            }
            at += 1;
            while space(at) {
                at += 1;
            }
            let quote = *bytes.get(at).filter(|byte| matches!(byte, b'"' | b'\''))?;
            at += 1;
            let length = bytes[at..]
                .iter()
                .position(|&byte| byte == quote || byte == b'<')?;
            if bytes[at + length] == b'<' {
                return None;
            }
            let value = &text[at..at + length];
            at += length + 1;
            // roxmltree handles each attribute as it completes, checking a
            // declaration against the tag's earlier ones then, so its share
            // is charged now, kept if the tag later fails: adding `2n - 1`
            // for the `n`th keeps each sum at the square of the count.
            attributes += 1;
            self.widest_tag = self.widest_tag.max(attributes);
            self.attribute_pairs = self.attribute_pairs.saturating_add(2 * attributes - 1);
            if let Some(prefix) = declared_prefix(name) {
                declarations += 1;
                self.namespace_lookups =
                    self.namespace_lookups.saturating_add(2 * declarations - 1);
                self.longest_prefix = self.longest_prefix.max(prefix.len());
                if namespaces.insert((prefix, value)) {
                    self.distinct_namespaces += 1;
                }
                let references = entity_references(value).0;
                let uri = value
                    .len()
                    .saturating_add(references.saturating_mul(longest_entity));
                self.longest_uri = self.longest_uri.max(uri);
            } else if name.contains(':') {
                prefixed += 1;
                self.prefixed_pairs = self.prefixed_pairs.saturating_add(2 * prefixed - 1);
            }
        };
        // The rest roxmltree does once the tag is complete: copy the
        // namespaces in scope, if it declares any, and look up its prefix
        // and those of its attributes.
        let inherited = scopes.last().copied().unwrap_or(0);
        let scope = inherited.saturating_add(declarations);
        let mut lookups = (1 + prefixed).saturating_mul(scope);
        if declarations > 0 {
            lookups = lookups.saturating_add(inherited.saturating_mul(scope));
        }
        self.namespace_lookups = self.namespace_lookups.saturating_add(lookups);
        if open {
            scopes.push(scope);
        }
        Some(at + 1)
    }
}

/// The whitespace roxmltree skips.
const XML_SPACE: [u8; 4] = *b" \t\n\r";

/// `text` without the whitespace it begins with.
fn skip_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r'])
}

/// The prefix an attribute named `name` declares a namespace for, empty for
/// the default one, as roxmltree reads it: `xmlns`, `xmlns:p`, and also
/// `p:xmlns`, which declares the default.
fn declared_prefix(name: &str) -> Option<&str> {
    match name.split_once(':') {
        None => (name == "xmlns").then_some(""),
        Some(("xmlns", prefix)) => Some(prefix),
        Some((_, "xmlns")) => Some(""),
        Some(_) => None,
    }
}

/// The length of `text`, which begins with `start`, through the first `end`
/// after it.
fn length_through(text: &str, start: &str, end: &str) -> Option<usize> {
    let found = text[start.len()..].find(end)?;
    Some(start.len() + found + end.len())
}

/// Where the first of `targets` outside quotes is in `text`.
fn find_unquoted(text: &str, targets: &[u8]) -> Option<usize> {
    let mut quote = None;
    text.bytes().position(|byte| match quote {
        Some(open) => {
            if byte == open {
                quote = None;
            }
            false
        }
        None if matches!(byte, b'"' | b'\'') => {
            quote = Some(byte);
            false
        }
        None => targets.contains(&byte),
    })
}

/// The length of the doctype `text` begins with, as roxmltree reads it: to
/// the first `>` or `[` outside the quoted external id, then through the
/// internal subset, whose entity declarations end at their first `>`
/// outside quotes and whose other declarations at their first `>`, to `]`
/// and `>`. `None` where roxmltree would fail.
fn doctype_length(text: &str) -> Option<usize> {
    let mut at = find_unquoted(text, b"[>")?;
    if text.as_bytes()[at] == b'>' {
        return Some(at + 1);
    }
    at += 1;
    loop {
        let rest = skip_space(&text[at..]);
        at = text.len() - rest.len();
        at += if let Some(after) = rest.strip_prefix(']') {
            let after = skip_space(after);
            return after.starts_with('>').then(|| text.len() - after.len() + 1);
        } else if rest.starts_with("<!--") {
            length_through(rest, "<!--", "-->")?
        } else if rest.starts_with("<?") {
            length_through(rest, "<?", "?>")?
        } else if rest.starts_with("<!ENTITY") {
            find_unquoted(rest, b">")? + 1
        } else if ["<!ELEMENT", "<!ATTLIST", "<!NOTATION"]
            .iter()
            .any(|declaration| rest.starts_with(declaration))
        {
            rest.find('>')? + 1
        } else {
            return None;
        };
    }
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

    /// Large legal documents, and whether each is safe: every link is
    /// looked up in one index, every ancestor chain is walked once, and each
    /// `url(` reads only its own prefix. Done per reference, these took
    /// billions of steps.
    fn large_documents() -> Vec<(String, bool)> {
        const REFERENCES: usize = 50_000;
        const DEPTH: usize = 5_000;

        // Many feImages naming one rect, inside unused <defs>.
        let mut many = String::from(r#"<defs><rect id="r" width="1" height="1"/><filter id="f">"#);
        for _ in 0..REFERENCES {
            many.push_str(r##"<feImage href="#r"/>"##);
        }
        many.push_str("</filter></defs>");
        // One reference that names nothing still fails the whole document.
        let broken = many.replacen(r##"href="#r""##, r##"href="#missing""##, 1);

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

        // A megabyte of `url(` without a closing parenthesis, every one
        // naming a fragment; then the same with one external target last.
        let urls = "url(#".repeat(200_000);
        let local = format!(r#"<rect width="1" height="1" fill="{urls}"/>"#);
        let external = format!(r#"<rect width="1" height="1" fill="{urls}url(/x"/>"#);

        [
            (many, true),
            (broken, false),
            (deep, true),
            (local, true),
            (external, false),
        ]
        .into_iter()
        .map(|(inner, safe)| (svg(&inner), safe))
        .collect()
    }

    #[test]
    fn large_documents_are_checked() {
        for (document, safe) in large_documents() {
            assert_eq!(is_safe_image(document.as_bytes()), safe);
        }
    }

    /// Documents each check refuses, with the one that refuses them: every
    /// one took seconds or more to parse unguarded.
    fn attacks() -> Vec<(&'static str, String, Cost)> {
        // One tag of 50 000 attributes: half a megabyte, two seconds.
        let attributes: String = (0..50_000).map(|i| format!(" a{i}=''")).collect();
        let crowded = svg(&format!("<rect{attributes}/>"));

        // Every pair of prefixed attributes compares 100 KB namespace
        // names: 445 KB, six seconds.
        let uri = "x".repeat(100_000);
        let attributes: String = (0..MAX_ATTRIBUTES - 2)
            .map(|i| format!(" {}:a{i}=''", if i % 2 == 0 { 'p' } else { 'q' }))
            .collect();
        let long_names = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' xmlns:p='urn:{uri}p' xmlns:q='urn:{uri}q'>{}</svg>",
            format!("<g{attributes}/>").repeat(100)
        );

        // 3 000 elements each declaring one more: 80 KB, twelve seconds.
        let mut nested = String::new();
        for level in 0..3_000 {
            nested.push_str(&format!("<g xmlns:p{level}='urn:{level}'>"));
        }
        nested.push_str(&"</g>".repeat(3_000));
        let nested = svg(&nested);

        // 60 000 declarations, each checked against the earlier ones as it
        // is read, on a tag that never ends: 1 MB, 2.6 seconds.
        let declarations: String = (0..60_000).map(|i| format!(" xmlns:p{i}='u'")).collect();
        let unfinished = format!("<svg xmlns='http://www.w3.org/2000/svg'><g{declarations} bad");

        // 65 000 distinct namespaces in descending order, each inserted at
        // the front of roxmltree's sorted vector: 1.6 MB, two billion
        // entries shifted.
        let descending: String = (1..=65_000)
            .rev()
            .map(|i| format!("<g xmlns:p='urn:{i:06}'/>"))
            .collect();
        let distinct = svg(&descending);

        // 500 entities each holding a tag cut off after 200 attributes,
        // which roxmltree gathers into one: five seconds.
        let (mut dtd, mut references) = (String::new(), String::new());
        for entity in 0..500 {
            dtd.push_str(&format!(r#"<!ENTITY e{entity} "<x"#));
            for attribute in 0..200 {
                dtd.push_str(&format!(" a{entity}_{attribute}=''"));
            }
            dtd.push_str(r#"">"#);
            references.push_str(&format!("&e{entity};"));
        }
        let built = with_dtd(&dtd, &format!("{references}<g/>"));

        // Each reference compared with 30 000 declarations: 830 KB, over a
        // second.
        let dtd: String = (0..30_000).map(|i| format!("<!ENTITY e{i} 'x'>")).collect();
        let lookups = with_dtd(&dtd, &"&e29999;".repeat(30_000));

        vec![
            ("one crowded tag", crowded, Cost::CrowdedTag),
            ("long namespace names", long_names, Cost::PrefixedAttributes),
            ("nested declarations", nested, Cost::NamespaceScopes),
            ("distinct namespaces", distinct, Cost::NamespaceScopes),
            (
                "an unfinished tag of declarations",
                unfinished,
                Cost::CrowdedTag,
            ),
            ("entities building one tag", built, Cost::MarkupEntity),
            ("entity lookups", lookups, Cost::EntityReferences),
        ]
    }

    #[test]
    fn each_attack_is_refused_by_its_check() {
        for (name, document, refused_by) in attacks() {
            assert_eq!(cost(&document), Some(refused_by), "{name}");
        }
    }

    /// How long the guard takes on each attack and large document. Run with
    /// `cargo test --release -p mtc image_guard -- --ignored --nocapture`.
    #[test]
    #[ignore = "a benchmark: timing depends on the machine"]
    fn benchmark() {
        let documents = attacks()
            .into_iter()
            .map(|(name, document, _)| (name.to_string(), document))
            .chain(
                large_documents()
                    .into_iter()
                    .enumerate()
                    .map(|(i, (document, _))| (format!("large document {i}"), document)),
            );
        for (name, document) in documents {
            let started = std::time::Instant::now();
            is_safe_image(document.as_bytes());
            let elapsed = started.elapsed();
            println!("{name}: {} KB, {elapsed:?}", document.len() / 1024);
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "{name}: {elapsed:?}"
            );
        }
    }

    /// Why `text` is refused for what it costs to read, if it is.
    fn cost(text: &str) -> Option<Cost> {
        parse_cost(text)
    }

    /// A document with an internal DTD.
    fn with_dtd(dtd: &str, inner: &str) -> String {
        format!(
            r#"<!DOCTYPE svg [{dtd}]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{inner}</svg>"#
        )
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
    fn the_markup_scan_reads_tags_as_roxmltree_does() {
        let scan = |text: &str| Markup::scan(text, 0);
        let one = scan("<a x='1' p:y='2' xmlns:p='u'/>");
        assert_eq!(
            one,
            Markup {
                widest_tag: 3,
                attribute_pairs: 9,
                prefixed_pairs: 1,
                longest_uri: 1,
                longest_prefix: 1,
                // Its own prefix and `p:y`'s looked up among one namespace,
                // and its one declaration checked.
                namespace_lookups: 3,
                distinct_namespaces: 1,
            }
        );
        // `p:xmlns` declares the default namespace, so binds no prefix.
        assert_eq!(scan("<a p:xmlns='uri'/>").longest_prefix, 0);
        assert_eq!(scan("<a p:xmlns='uri'/>").longest_uri, 3);
        // An entity reference in a namespace name counts as the longest
        // entity value.
        assert_eq!(Markup::scan("<a xmlns:p='&e;'/>", 1000).longest_uri, 1003);

        // Each nested declaration copies all those above: element `i`
        // costs `i² + 1`.
        let nested = "<a xmlns:a='u'><b xmlns:b='u'><c xmlns:c='u'/></b></a>";
        assert_eq!(scan(nested).namespace_lookups, 2 + 5 + 10);
        // Siblings repeating one cost the same each.
        let siblings =
            |count: usize| format!("<r xmlns:a='u'>{}</r>", "<g xmlns:a='u'/>".repeat(count));
        assert_eq!(scan(&siblings(1_000)).namespace_lookups, 2 + 5 * 1_000);

        // An end tag takes its element's namespaces out of scope.
        let closed = "<r><a xmlns:a='u'></a><b xmlns:b='u'/></r>";
        assert_eq!(scan(closed).namespace_lookups, 2 + 2);

        // Nothing inside these is a tag, and an end tag in them closes
        // nothing: the second `g` still has the first's namespace in scope.
        for hidden in [
            "<!-- </g> <x a='1' b='2' c='3'/> -->",
            "<![CDATA[ </g> <x a='1' b='2' c='3'/> ]]>",
            "<?pi </g> <x a='1' b='2' c='3'/> ?>",
        ] {
            let text = format!("<g xmlns:a='u'>{hidden}<g xmlns:b='v'/></g>");
            let markup = scan(&text);
            assert_eq!(markup.namespace_lookups, 2 + 5, "{text}");
            assert_eq!(markup.widest_tag, 1, "{text}");
        }
        let dtd = r#"<!DOCTYPE svg PUBLIC "a>b" "[c]" [
            <!ENTITY e "</g> <x a='1' b='2' c='3'/>">
            <!ATTLIST svg a CDATA "x">
            <!-- </g> --> <?pi </g> ?>
        ]>"#;
        let markup = scan(&format!("{dtd}<g xmlns:a='u'><g xmlns:b='v'/></g>"));
        assert_eq!((markup.widest_tag, markup.namespace_lookups), (1, 2 + 5));
        // Values may hold `>`, `=` and the other quote.
        assert_eq!(scan(&rect_with_attributes(5)).widest_tag, 5);

        // roxmltree fails at a `<` in a value, and parses nothing after.
        assert_eq!(scan("<a b='<'/><c d='1' e='2'/>"), Markup::default());
        // But what it read before failing still costs: each declaration is
        // checked against the tag's earlier ones as it completes, before the
        // tag turns out to be malformed, or unfinished.
        for tail in [" bad", " bad='", " / >", ""] {
            let text = format!("<r><g a='1' p:b='2' xmlns:x='u' xmlns:y='u'{tail}");
            let markup = scan(&text);
            assert_eq!(
                (
                    markup.widest_tag,
                    markup.attribute_pairs,
                    markup.prefixed_pairs
                ),
                (4, 16, 1),
                "{text}"
            );
            // Two declarations: two squared.
            assert_eq!(markup.namespace_lookups, 2 * 2, "{text}");
        }
        // Distinct namespaces, by prefix and value, counted as each
        // declaration completes, whether or not its tag does.
        let distinct = "<r xmlns:p='u'><a xmlns:p='u'/><b xmlns:p='v'/><c xmlns:q='u'/><d xmlns='u' xmlns:x='w' bad";
        assert_eq!(scan(distinct).distinct_namespaces, 5);
        let declarations: String = (0..60_000).map(|i| format!(" xmlns:p{i}='u'")).collect();
        let unfinished = scan(&format!("<g{declarations} bad"));
        assert_eq!(unfinished.namespace_lookups, 60_000 * 60_000);
        assert_eq!(unfinished.widest_tag, 60_000);
    }

    #[test]
    fn crowded_tags_are_refused() {
        let full = rect_with_attributes(MAX_ATTRIBUTES);
        let over = rect_with_attributes(MAX_ATTRIBUTES + 1);
        for before in [
            "",
            r#"<!-- it's "quoted -->"#,
            r#"<text><![CDATA[ ' " ]]></text>"#,
            r#"<?pi " ?>"#,
            r#"<text>"it's" = 'x' > y</text>"#,
            &format!("<!-- {} -->", "=".repeat(1_000)),
            &rect_with_attributes(MAX_ATTRIBUTES),
        ] {
            let fits = svg(&format!("{before}{full}"));
            assert_eq!(cost(&fits), None, "{fits}");
            let crowded = svg(&format!("{before}{over}"));
            assert_eq!(cost(&crowded), Some(Cost::CrowdedTag), "{crowded}");
        }
        let dtd = r#"<!ENTITY q '"'><!-- ' --><!ENTITY a "x>y">"#;
        assert_eq!(cost(&with_dtd(dtd, &full)), None);
        assert_eq!(cost(&with_dtd(dtd, &over)), Some(Cost::CrowdedTag));

        // Tags each within the bound, but too many of them: the root's four
        // and 4 096 of 128 are just over.
        let attributes: String = (0..MAX_ATTRIBUTES).map(|i| format!(" a{i}=''")).collect();
        let tags = |count: usize| svg(&format!("<g{attributes}/>").repeat(count));
        let tag_pairs = MAX_ATTRIBUTES * MAX_ATTRIBUTES;
        assert_eq!(MAX_ATTRIBUTE_PAIRS / tag_pairs, 4_096);
        assert_eq!(
            Markup::scan(&tags(4_096), 0).attribute_pairs,
            4_096 * tag_pairs + 16
        );
        assert_eq!(cost(&tags(4_096)), Some(Cost::CrowdedTag));
        assert_eq!(cost(&tags(4_095)), None);
    }

    #[test]
    fn long_namespace_names_on_prefixed_attributes_are_refused() {
        // Every pair of prefixed attributes compares 100 KB namespace
        // names: 445 KB that took six seconds to parse.
        let uri = "x".repeat(100_000);
        let attributes: String = (0..MAX_ATTRIBUTES - 2)
            .map(|i| format!(" {}:a{i}=''", if i % 2 == 0 { 'p' } else { 'q' }))
            .collect();
        let reviewed = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' xmlns:p='urn:{uri}p' xmlns:q='urn:{uri}q'>{}</svg>",
            format!("<g{attributes}/>").repeat(100)
        );
        assert_eq!(cost(&reviewed), Some(Cost::PrefixedAttributes));
        // The same through an entity.
        let entity = with_dtd(
            &format!("<!ENTITY u '{uri}'>"),
            &format!(
                "<g xmlns:p='&u;'>{}</g>",
                format!("<g{attributes}/>").repeat(100)
            ),
        );
        let entity = entity.replace(" q:", " p:");
        assert_eq!(cost(&entity), Some(Cost::PrefixedAttributes));
        // Short ones cost little.
        let short = reviewed.replace(&uri, "");
        assert_eq!(cost(&short), None);
    }

    #[test]
    fn nested_namespace_declarations_are_refused() {
        // Many prefixes in scope, then many elements looking theirs up.
        let declarations: String = (0..126).map(|i| format!(" xmlns:p{i}='u{i}'")).collect();
        let lookups = svg(&format!(
            "<g{declarations}>{}</g>",
            "<a/>".repeat(1_000_000)
        ));
        assert_eq!(cost(&lookups), Some(Cost::NamespaceScopes));

        // Distinct namespaces, each other check passed: the root's
        // `xmlns:xlink` and `xmlns` and siblings declaring one each.
        let siblings = |count: usize| {
            svg(&(0..count)
                .map(|i| format!("<g xmlns:p='urn:{i}'/>"))
                .collect::<String>())
        };
        let full = siblings(MAX_NAMESPACES - 2);
        assert_eq!(Markup::scan(&full, 0).distinct_namespaces, MAX_NAMESPACES);
        assert_eq!(cost(&full), None);
        assert_eq!(
            cost(&siblings(MAX_NAMESPACES - 1)),
            Some(Cost::NamespaceScopes)
        );
        let (_, reproducer, _) = attacks()
            .into_iter()
            .find(|(name, ..)| *name == "distinct namespaces")
            .unwrap();
        assert_eq!(Markup::scan(&reproducer, 0).distinct_namespaces, 65_002);
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
        // A large drawing of it: every path with its own prefixed attributes.
        let path =
            r#"<path inkscape:connector-curvature="0" sodipodi:nodetypes="ccc" d="M0 0h1v1z"/>"#;
        let drawing = inkscape.replace("</g>", &format!("{}</g>", path.repeat(100_000)));
        assert_eq!(cost(&drawing), None);
        // Icons pasted in with their own declarations, however many: the
        // same two namespaces each time.
        let icon = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1" height="1"><rect width="1" height="1"/></svg>"#;
        let sprites = svg(&icon.repeat(5_000));
        assert_eq!(Markup::scan(&sprites, 0).distinct_namespaces, 2);
        assert!(is_safe_image(sprites.as_bytes()));
        // Text that names them is no declaration.
        let prose = svg(&format!(
            "<!-- xmlns xmlns --><style>/* xmlns */</style><text>{}</text>",
            "xmlns ".repeat(1_000)
        ));
        assert!(is_safe_image(prose.as_bytes()));
    }

    #[test]
    fn entities_holding_markup_are_refused() {
        // roxmltree leaves the attributes of a tag cut off by the end of an
        // entity to the next tag, so tags no bound sees can grow without one.
        let pieces = with_dtd(
            r#"<!ENTITY a "<x a1='1' a2='2'"><!ENTITY b "<y b1='1'">"#,
            "&a;&b;<g c='3'/>",
        );
        let parsed = Document::parse_with_options(&pieces, parsing_options()).unwrap();
        let g = parsed
            .descendants()
            .find(|node| node.has_tag_name("g"))
            .unwrap();
        let names: Vec<_> = g.attributes().map(|attribute| attribute.name()).collect();
        assert_eq!(names, ["a1", "a2", "b1", "c"]);
        assert_eq!(cost(&pieces), Some(Cost::MarkupEntity));

        // Entities as SVG editors use them, for names, still render.
        let names = r##"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd" [
            <!ENTITY ns_svg "http://www.w3.org/2000/svg">
            <!ENTITY ns_xlink 'http://www.w3.org/1999/xlink'>
            <!ENTITY red "#f00">
        ]><svg xmlns="&ns_svg;" xmlns:xlink="&ns_xlink;" width="1" height="1"><rect width="1" height="1" fill="&red;"/></svg>"##;
        assert!(is_safe_image(names.as_bytes()));
    }

    #[test]
    fn costly_entity_references_are_refused() {
        // An entity referencing another 255 times, which roxmltree allows
        // under each of 2 000 references, every one compared with 10 000
        // declarations: within the bound if each counted once.
        let unused: String = (0..10_000).map(|i| format!("<!ENTITY d{i} ''>")).collect();
        let nested = with_dtd(
            &format!("{unused}<!ENTITY b ''><!ENTITY a '{}'>", "&b;".repeat(255)),
            &"&a;".repeat(2_000),
        );
        assert!(!Entities::scan(&nested).markup && Entities::scan(&nested).nested);
        let unnested = Entities::scan(&nested.replace("&b;", "bbb"));
        assert!(unnested.work(&nested).0 <= MAX_ENTITY_WORK);
        assert_eq!(cost(&nested), Some(Cost::EntityReferences));
        // Lookups and expanded text, as counted: two declarations, three
        // references with four bytes of names, the longest value three bytes.
        let counted = "<!ENTITY a 'xyz'><!ENTITY bb 'q'>&a;&bb;&a;&lt;&#65;";
        assert_eq!(Entities::scan(counted).work(counted), (2 * (3 + 4), 3 * 3));
        // 64 KB copied in 1 024 times: 64 MB of text, which took usvg over
        // half a second more than the body alone.
        let sixty_four = with_dtd(
            &format!("<!ENTITY x '{}'>", "x".repeat(1 << 16)),
            &format!("<text>{}</text>", "&x;".repeat(1_024)),
        );
        assert_eq!(cost(&sixty_four), Some(Cost::EntityReferences));
        // 100 KB copied in 10 000 times: a gigabyte of text from 130 KB.
        let expanding = with_dtd(
            &format!("<!ENTITY x '{}'>", "x".repeat(100_000)),
            &format!("<text>{}</text>", "&x;".repeat(10_000)),
        );
        assert_eq!(cost(&expanding), Some(Cost::EntityReferences));
    }

    #[test]
    fn entities_as_editors_write_them_still_render() {
        // Illustrator's namespace names, and older files' style per
        // entity, referenced from every element.
        let mut dtd = String::from(
            r#"<!ENTITY ns_extend "http://ns.adobe.com/Extensibility/1.0/">
            <!ENTITY ns_ai "http://ns.adobe.com/AdobeIllustrator/10.0/">
            <!ENTITY ns_graphs "http://ns.adobe.com/Graphs/1.0/">
            <!ENTITY ns_svg "http://www.w3.org/2000/svg">
            <!ENTITY ns_xlink "http://www.w3.org/1999/xlink">"#,
        );
        for style in 0..50 {
            dtd.push_str(&format!(
                r#"<!ENTITY st{style} "fill:none;stroke:#{style:06};stroke-width:2;stroke-miterlimit:10;">"#
            ));
        }
        let shapes: String = (0..5_000)
            .map(|i| format!(r#"<rect width="1" height="1" style="&st{};"/>"#, i % 50))
            .collect();
        let illustrator = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd" [{dtd}]><svg xmlns:x="&ns_extend;" xmlns:i="&ns_ai;" xmlns:graph="&ns_graphs;" xmlns="&ns_svg;" xmlns:xlink="&ns_xlink;" width="10" height="10">{shapes}<text>Fish &amp; chips &lt;3 &#x41;&#65;</text></svg>"#
        );
        assert!(is_safe_image(illustrator.as_bytes()));
        // Escapes alone need no lookup, however many.
        let escapes = svg(&format!(
            "<text>{}</text>",
            "&amp;&lt;&#65;".repeat(100_000)
        ));
        assert!(is_safe_image(escapes.as_bytes()));
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
