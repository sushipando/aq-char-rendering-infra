//! A small mutable SVG DOM with namespace-preserving parse/serialize.
//!
//! The Python component raster worker manipulates FFDec SVG exports with
//! ``xml.etree.ElementTree`` (match tags by local name, read/set attributes in
//! Clark notation, wrap children in groups, rename ids and rewrite
//! ``url(#id)`` references). This module provides the same operations on a
//! tiny owned tree parsed with `roxmltree` (read-only, attribute-order
//! preserving) and serialized back to XML text that `usvg` consumes.
//!
//! Attribute namespace semantics follow the FFDec exports: unprefixed SVG
//! attributes carry no namespace, while ``ffdec:*`` and ``xlink:href`` carry
//! their declared namespace URIs.

use roxmltree::Node as RNode;

use crate::error::RasterError;

pub const SVG_NS: &str = "http://www.w3.org/2000/svg";
pub const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
pub const FFDEC_NS: &str = "https://www.free-decompiler.com/flash";

/// One attribute: the authored qualified name (e.g. ``xlink:href``), its
/// namespace URI when it has one, and its string value.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub qname: String,
    pub ns: Option<String>,
    pub value: String,
}

impl Attr {
    pub fn local(&self) -> &str {
        local_name(&self.qname)
    }

    pub fn matches(&self, local: &str, ns: Option<&str>) -> bool {
        self.local() == local
            && match (ns, self.ns.as_deref()) {
                (None, None) => true,
                (Some(expected), Some(actual)) => expected == actual,
                _ => false,
            }
    }
}

/// One element. Children are elements or plain text; comments and CDATA are
/// dropped (FFDec exports do not need them).
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub qname: String,
    pub ns: Option<String>,
    pub attrs: Vec<Attr>,
    pub children: Vec<Node>,
    pub text: Option<String>,
}

impl Node {
    pub fn new(qname: &str, ns: Option<&str>) -> Self {
        Node {
            qname: qname.to_string(),
            ns: ns.map(str::to_string),
            attrs: Vec::new(),
            children: Vec::new(),
            text: None,
        }
    }

    /// A plain (non-namespaced) element in the SVG namespace.
    pub fn elem(local: &str) -> Self {
        Node::new(local, Some(SVG_NS))
    }

    pub fn local(&self) -> &str {
        local_name(&self.qname)
    }

    pub fn set_attr(&mut self, qname: &str, ns: Option<&str>, value: impl Into<String>) {
        let value = value.into();
        if let Some(existing) = self
            .attrs
            .iter_mut()
            .find(|attr| attr.matches(local_name(qname), ns))
        {
            existing.value = value;
            return;
        }
        let qualified = match ns {
            Some(namespace) => {
                let local = local_name(qname);
                match prefix_for(namespace) {
                    Some(prefix) => format!("{prefix}:{local}"),
                    None => local.to_string(),
                }
            }
            None => qname.to_string(),
        };
        if let Some(existing) = self
            .attrs
            .iter_mut()
            .find(|attr| attr.local() == local_name(&qualified))
        {
            if existing.ns.is_none() && ns.is_none() {
                existing.value = value;
                return;
            }
        }
        self.attrs.push(Attr {
            qname: qualified,
            ns: ns.map(str::to_string),
            value,
        });
    }

    /// Shortcut for a plain attribute.
    pub fn set(&mut self, local: &str, value: impl Into<String>) {
        self.set_attr(local, None, value);
    }

    pub fn get(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|attr| attr.matches(local, None))
            .map(|attr| attr.value.as_str())
    }

    pub fn get_in(&self, local: &str, ns: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|attr| attr.matches(local, Some(ns)))
            .map(|attr| attr.value.as_str())
    }

    pub fn remove_attr(&mut self, local: &str) {
        self.attrs
            .retain(|attr| !(attr.ns.is_none() && attr.local() == local));
    }

    pub fn append(&mut self, child: Node) {
        self.children.push(child);
    }

    pub fn insert_at(&mut self, index: usize, child: Node) {
        let index = index.min(self.children.len());
        self.children.insert(index, child);
    }

    pub fn remove_at(&mut self, index: usize) -> Option<Node> {
        if index < self.children.len() {
            Some(self.children.remove(index))
        } else {
            None
        }
    }

    pub fn replace_attr_value(&mut self, local: &str, value: impl Into<String>) {
        self.set_attr(local, None, value);
    }

    /// Depth-first iteration including self.
    pub fn iter_mut(&mut self) -> ChildIterMut<'_> {
        ChildIterMut::new(self)
    }

    /// Depth-first iteration including self (read-only).
    pub fn iter(&self) -> ChildIter<'_> {
        ChildIter::new(self)
    }
}

pub struct ChildIter<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> ChildIter<'a> {
    fn new(root: &'a Node) -> Self {
        ChildIter { stack: vec![root] }
    }
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        for child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

pub struct ChildIterMut<'a> {
    stack: Vec<&'a mut Node>,
}

impl<'a> ChildIterMut<'a> {
    fn new(root: &'a mut Node) -> Self {
        ChildIterMut { stack: vec![root] }
    }
}

impl<'a> Iterator for ChildIterMut<'a> {
    type Item = &'a mut Node;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        // SAFETY: every entry in `self.stack` descends from the original
        // `&'a mut` root and remains valid for `'a`; we re-derive ``&'a mut``
        // references to its children so the iterator can yield them. Each
        // node is pushed exactly once (parents before children, DFS), so the
        // yielded references never alias.
        let children_ptr = unsafe { &mut *(&mut *node as *mut Node) };
        let len = children_ptr.children.len();
        let mut pointers: Vec<&'a mut Node> = Vec::new();
        for index in (0..len).rev() {
            let child = unsafe { &mut *(&mut children_ptr.children[index] as *mut Node) };
            pointers.push(child);
        }
        // Push in reverse so the stack pops in document order.
        for child_ref in pointers.into_iter().rev() {
            self.stack.push(child_ref);
        }
        Some(node)
    }
}

pub fn local_name(qname: &str) -> &str {
    qname.rsplit(':').next().unwrap_or(qname)
}

/// The prefix we emit for attributes created under a known namespace.
pub fn prefix_for(namespace: &str) -> Option<&'static str> {
    match namespace {
        XLINK_NS => Some("xlink"),
        FFDEC_NS => Some("ffdec"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub root: Node,
    /// Namespace declarations from the source document: ``(prefix, uri)``
    /// with `None` for the default namespace.
    pub namespaces: Vec<(Option<String>, String)>,
}

fn parse_qname_attr(
    node: &RNode,
    prefixes: &std::collections::HashMap<String, String>,
) -> Option<Node> {
    if !node.is_element() {
        return None;
    }
    let qname = node.tag_name();
    let qualified = match qname.namespace() {
        Some(namespace) => match prefixes.get(namespace) {
            Some(prefix) => format!("{prefix}:{}", qname.name()),
            None => qname.name().to_string(),
        },
        None => qname.name().to_string(),
    };
    let mut element = Node {
        qname: qualified,
        ns: qname.namespace().map(str::to_string),
        attrs: Vec::new(),
        children: Vec::new(),
        text: None,
    };
    for attribute in node.attributes() {
        let attr_qualified = match attribute.namespace() {
            Some(namespace) => match prefixes.get(namespace) {
                Some(prefix) => format!("{prefix}:{}", attribute.name()),
                None => attribute.name().to_string(),
            },
            None => attribute.name().to_string(),
        };
        // Skip xmlns declarations; roxmltree exposes them separately.
        if attr_qualified == "xmlns" || attr_qualified.starts_with("xmlns:") {
            continue;
        }
        element.attrs.push(Attr {
            qname: attr_qualified,
            ns: attribute.namespace().map(str::to_string),
            value: attribute.value().to_string(),
        });
    }
    for child in node.children() {
        if child.is_element() {
            if let Some(child_node) = parse_qname_attr(&child, prefixes) {
                element.children.push(child_node);
            }
        } else if child.is_text() {
            if let Some(text) = child.text() {
                if !text.trim().is_empty() {
                    element.text = Some(text.to_string());
                }
            }
        }
    }
    Some(element)
}

pub fn parse(bytes: &[u8]) -> Result<Document, RasterError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| RasterError::Svg(format!("source svg is not utf-8: {error}")))?;
    let document = roxmltree::Document::parse(text)
        .map_err(|error| RasterError::Svg(format!("invalid xml: {error}")))?;
    let root = document.root_element();
    let root_node = parse_qname_attr(&root, &prefix_index(&document))
        .ok_or_else(|| RasterError::Svg("source svg has no root element".to_string()))?;

    Ok(Document {
        root: root_node,
        namespaces: collect_namespaces(&document),
    })
}

/// uri -> prefix map used to rebuild prefixed element names.
fn prefix_index(document: &roxmltree::Document<'_>) -> std::collections::HashMap<String, String> {
    let mut index = std::collections::HashMap::new();
    for namespace in collect_namespaces(document) {
        if let (Some(prefix), uri) = namespace {
            index.entry(uri).or_insert(prefix);
        }
    }
    index
}

fn collect_namespaces(document: &roxmltree::Document<'_>) -> Vec<(Option<String>, String)> {
    let mut namespaces: Vec<(Option<String>, String)> = Vec::new();
    for node in document.descendants() {
        for namespace in node.namespaces() {
            let prefix = namespace.name().map(str::to_string);
            let uri = namespace.uri().to_string();
            if !namespaces
                .iter()
                .any(|(existing, existing_uri)| existing == &prefix && existing_uri == &uri)
            {
                namespaces.push((prefix, uri));
            }
        }
    }
    namespaces
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

fn escape_attribute(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(character),
        }
    }
    output
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn write_node(node: &Node, output: &mut String, indent: usize) {
    let padding = "  ".repeat(indent);
    output.push_str(&format!("{}<{}", padding, node.qname));
    for attr in &node.attrs {
        output.push_str(&format!(
            " {}=\"{}\"",
            attr.qname,
            escape_attribute(&attr.value)
        ));
    }
    let closing = node.children.is_empty() && node.text.is_none();
    if closing {
        output.push_str("/>\n");
        return;
    }
    output.push('>');
    if let Some(text) = &node.text {
        output.push_str(&escape_text(text));
    }
    if !node.children.is_empty() {
        output.push('\n');
        for child in &node.children {
            write_node(child, output, indent + 1);
        }
    }
    output.push_str(&format!("{}</{}>\n", padding, node.qname));
}

pub fn serialize(document: &Document) -> String {
    let mut output = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    output.push('<');
    output.push_str(&document.root.qname);
    // Ensure the SVG default namespace is present (FFDec exports declare it).
    let has_default = document
        .namespaces
        .iter()
        .any(|(prefix, _)| prefix.is_none());
    if !has_default {
        output.push_str(&format!(" xmlns=\"{}\"", SVG_NS));
    }
    for (prefix, uri) in &document.namespaces {
        match prefix {
            Some(prefix) => {
                output.push_str(&format!(" xmlns:{prefix}=\"{}\"", escape_attribute(uri)))
            }
            None => output.push_str(&format!(" xmlns=\"{}\"", escape_attribute(uri))),
        }
    }
    // xlink is required for our synthetic hrefs; FFDec exports declare it,
    // but add it defensively if it was not collected. Same for ffdec: so the
    // assembled component SVG always re-declares the prefixes the Python
    // ElementTree registry would.
    let has_xlink = document
        .namespaces
        .iter()
        .any(|(prefix, uri)| prefix.as_deref() == Some("xlink") || uri == XLINK_NS);
    if !has_xlink {
        output.push_str(&format!(" xmlns:xlink=\"{}\"", XLINK_NS));
    }
    let has_ffdec = document
        .namespaces
        .iter()
        .any(|(prefix, uri)| prefix.as_deref() == Some("ffdec") || uri == FFDEC_NS);
    if !has_ffdec {
        output.push_str(&format!(" xmlns:ffdec=\"{}\"", FFDEC_NS));
    }
    for attr in &document.root.attrs {
        output.push_str(&format!(
            " {}=\"{}\"",
            attr.qname,
            escape_attribute(&attr.value)
        ));
    }
    output.push_str(">\n");
    for child in &document.root.children {
        write_node(child, &mut output, 1);
    }
    output.push_str(&format!("</{}>\n", document.root.qname));
    output
}

// ---------------------------------------------------------------------------
// Shared SVG number/transform helpers (ported from character_svg.py)
// ---------------------------------------------------------------------------

/// Python's `%.12g` float formatting: up to 12 significant digits, decimal
/// notation for exponents in [-4, 12), else exponent notation with a sign.
pub fn fmt_g(value: f64) -> String {
    fmt_g_with_precision(value, 12)
}

fn fmt_g_with_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let abs = value.abs();
    let exponent = abs.log10().floor() as i32;
    if exponent < -4 || exponent >= precision as i32 {
        // Exponential form with (precision - 1) fraction digits.
        let mut mantissa = abs / 10f64.powi(exponent);
        let mut exp = exponent;
        if mantissa >= 10.0 {
            mantissa /= 10.0;
            exp += 1;
        }
        let decimals = precision.saturating_sub(1);
        let mut text = format!("{mantissa:.decimals$}");
        trim_zeros(&mut text);
        format!("{sign}{text}e{exp:+03}")
    } else {
        let decimals = (precision as i64 - exponent as i64 - 1).max(0) as usize;
        let mut text = format!("{abs:.decimals$}");
        trim_zeros(&mut text);
        format!("{sign}{text}")
    }
}

fn trim_zeros(text: &mut String) {
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if text == "-0" {
        text.clear();
        text.push('0');
    }
}

/// Format a matrix like Python's `matrix_text`: `matrix(a b c d e f)`.
pub fn matrix_text(matrix: [f64; 6]) -> String {
    let values = matrix.map(fmt_g).join(" ");
    format!("matrix({values})")
}

/// Parse an SVG `matrix(a b c d e f)` attribute value.
pub fn parse_matrix(value: &str) -> Option<[f64; 6]> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix("matrix(")?.strip_suffix(')')?;
    let mut numbers = Vec::with_capacity(6);
    for part in inner.split([',', ' ']).filter(|part| !part.is_empty()) {
        let parsed: f64 = part.parse().ok()?;
        numbers.push(parsed);
    }
    if numbers.len() != 6 {
        return None;
    }
    Some([
        numbers[0], numbers[1], numbers[2], numbers[3], numbers[4], numbers[5],
    ])
}

/// `sqrt(abs(det))` of an SVG affine transform's linear part.
pub fn affine_geometric_scale(matrix: [f64; 6]) -> f64 {
    (matrix[0] * matrix[3] - matrix[1] * matrix[2]).abs().sqrt()
}

/// Python's `round()` (banker's rounding).
pub fn py_round(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction < 0.5 {
        floor as i64
    } else if fraction > 0.5 {
        floor as i64 + 1
    } else {
        let integer = floor as i64;
        if integer % 2 == 0 {
            integer
        } else {
            integer + 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" width="10" height="5" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(1, 0, 0, 1, 2, 3)">
    <use ffdec:characterId="7" xlink:href="#sprite0" filter="url(#f1)"/>
  </g>
  <defs>
    <g id="sprite0"><path id="p1" d="M0 0"/></g>
    <filter id="f1"><feGaussianBlur stdDeviation="2"/></filter>
  </defs>
</svg>"##;

    #[test]
    fn parses_and_preserves_attributes() {
        let document = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(document.root.local(), "svg");
        assert_eq!(document.root.get("width"), Some("10"));
        assert_eq!(document.root.get_in("objectType", FFDEC_NS), Some("frame"));
        let groups: Vec<&Node> = document
            .root
            .iter()
            .filter(|node| node.local() == "use")
            .collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].get_in("characterId", FFDEC_NS), Some("7"));
        assert_eq!(groups[0].get_in("href", XLINK_NS), Some("#sprite0"));
        assert_eq!(groups[0].get("filter"), Some("url(#f1)"));
    }

    #[test]
    fn serialize_round_trips_and_reparses() {
        let document = parse(SAMPLE.as_bytes()).unwrap();
        let text = serialize(&document);
        let reparsed = parse(text.as_bytes()).unwrap();
        eprintln!("SERIALIZED:\n{}", text);
        assert_eq!(reparsed.root.get("width"), Some("10"));
        assert_eq!(reparsed.root.get_in("objectType", FFDEC_NS), Some("frame"));
        assert_eq!(
            reparsed
                .root
                .iter()
                .find(|node| node.local() == "use")
                .unwrap(),
            document
                .root
                .iter()
                .find(|node| node.local() == "use")
                .unwrap()
        );
        assert!(text.contains("xmlns:ffdec=\"https://www.free-decompiler.com/flash\""));
        assert!(text.contains("xlink:href=\"#sprite0\""));
    }

    #[test]
    fn iter_mut_walks_depth_first() {
        let mut document = parse(SAMPLE.as_bytes()).unwrap();
        let mut count = 0;
        for node in document.root.iter_mut() {
            if node.local() == "path" {
                node.set("d", "M1 1");
            }
            count += 1;
        }
        assert!(count > 5);
        let path = document
            .root
            .iter()
            .find(|node| node.local() == "path")
            .unwrap();
        assert_eq!(path.get("d"), Some("M1 1"));
    }

    #[test]
    fn matrix_and_scale_helpers() {
        let matrix = parse_matrix("matrix(2, 0, 0, 2, -5, 7)").unwrap();
        assert_eq!(matrix, [2.0, 0.0, 0.0, 2.0, -5.0, 7.0]);
        assert!((affine_geometric_scale(matrix) - 2.0).abs() < 1e-12);
        assert_eq!(matrix_text(matrix), "matrix(2 0 0 2 -5 7)");
        assert_eq!(fmt_g(2.2940527227516303), "2.29405272275");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1.0 / 3.0), "0.333333333333");
        assert_eq!(fmt_g(0.0392156862745098), "0.0392156862745");
        assert_eq!(fmt_g(0.0980392156862745), "0.0980392156863");
        assert_eq!(fmt_g(12_345_678_901_234.0), "1.23456789012e+13");
        assert_eq!(fmt_g(0.0000123), "1.23e-05");
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(1.5), 2);
    }

    #[test]
    fn attr_set_and_remove() {
        let mut document = parse(SAMPLE.as_bytes()).unwrap();
        document.root.set("width", "20");
        document.root.set_attr("href", Some(XLINK_NS), "nok");
        assert_eq!(document.root.get("width"), Some("20"));
        document.root.remove_attr("width");
        assert_eq!(document.root.get("width"), None);
        let text = serialize(&document);
        assert!(!text.contains("width=\"20\""));
    }
}
