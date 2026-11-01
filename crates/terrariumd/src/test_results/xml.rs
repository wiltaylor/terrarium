//! Safe XML for the JUnit and TRX parsers.
//!
//! roxmltree with `allow_dtd: false`: a DTD with declarations is an
//! error, so no entity is declared and none expands; no external
//! reference is read. An empty `DOCTYPE` is refused here too, so the rule
//! is simple: no `DOCTYPE`.

use roxmltree::{Document, Node, ParsingOptions};

use super::{ParseError, invalid};

/// The most XML nodes of one file. A 20 MiB file of small elements stays
/// under it.
const MAX_NODES: u32 = 10_000_000;

const DOCTYPE: &str =
    "the XML has a DOCTYPE; DTDs and entities are not allowed in test result files";

/// The name of the root element (with no prefix), from a scan of the
/// prolog: no parse.
pub fn root_name(text: &str) -> Result<String, ParseError> {
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("<?") {
            let end = r
                .find("?>")
                .ok_or_else(|| invalid("bad XML: an unclosed `<?`"))?;
            rest = r[end + 2..].trim_start();
        } else if let Some(r) = rest.strip_prefix("<!--") {
            let end = r
                .find("-->")
                .ok_or_else(|| invalid("bad XML: an unclosed comment"))?;
            rest = r[end + 3..].trim_start();
        } else if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("<!DOCTYPE") {
            return Err(invalid(DOCTYPE));
        } else if let Some(r) = rest.strip_prefix('<') {
            let name: String = r
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
                .collect();
            if name.is_empty() {
                return Err(invalid("bad XML: no root element"));
            }
            let local = name.rsplit(':').next().unwrap_or(&name).to_owned();
            return Ok(local);
        } else {
            return Err(invalid("bad XML: no root element"));
        }
    }
}

/// Parse a document with no DTD.
pub fn parse(text: &str) -> Result<Document<'_>, ParseError> {
    // The root scan refuses a DOCTYPE in the prolog, also an empty one.
    root_name(text)?;
    let opt = ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_NODES,
        ..ParsingOptions::default()
    };
    Document::parse_with_options(text, opt).map_err(|e| match e {
        roxmltree::Error::DtdDetected => invalid(DOCTYPE),
        roxmltree::Error::NodesLimitReached => invalid("the XML has too many nodes; split the run"),
        e => invalid(format!("bad XML: {e}")),
    })
}

/// The local name of an element.
pub fn name<'a>(n: &Node<'a, '_>) -> &'a str {
    n.tag_name().name()
}

/// The child elements with this local name.
pub fn children<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && name(c) == tag)
}

/// The first child element with this local name.
pub fn child<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> Option<Node<'a, 'i>> {
    children(n, tag).next()
}

/// All the text inside an element (text and CDATA), in order.
pub fn text(n: Node<'_, '_>) -> String {
    n.descendants()
        .filter(|d| d.is_text())
        .filter_map(|d| d.text())
        .collect()
}

/// The text of an element, or `None` when it is empty.
pub fn text_opt(n: Option<Node<'_, '_>>) -> Option<String> {
    let t = text(n?);
    (!t.trim().is_empty()).then_some(t)
}

/// An attribute by its local name.
pub fn attr<'a>(n: &Node<'a, '_>, key: &str) -> Option<&'a str> {
    n.attributes().find(|a| a.name() == key).map(|a| a.value())
}
