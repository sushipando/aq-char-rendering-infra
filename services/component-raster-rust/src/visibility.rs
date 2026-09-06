//! Conservative structural invisibility proof for FFDec's reachable artwork.
//! Missing thumbnail pixels, tiny geometry, and transparent paint are not proofs.
use std::collections::{HashMap, HashSet};

use crate::svg::{Node, XLINK_NS};

pub fn proven_invisible(root: &Node) -> bool {
    // The FFDec importer unwraps the outer document. Do not use outer SVG
    // presentation effects as a proof about the later imported symbol.
    if root.local() == "svg"
        && ["opacity", "display", "filter"]
            .iter()
            .any(|a| root.get(a).is_some())
    {
        return false;
    }
    // CSS can override presentation attributes. Unsupported styling retains the
    // conservative probe fallback rather than approximating the CSS cascade.
    if root
        .iter()
        .any(|n| n.local() == "style" || n.get("style").is_some())
    {
        return false;
    }
    let mut ids = HashMap::new();
    for node in root.iter() {
        if let Some(id) = node.get("id") {
            if ids.insert(id, node).is_some() {
                return false;
            }
        }
    }
    empty(root, &ids, &mut HashSet::new(), 0)
}

fn empty<'a>(
    node: &'a Node,
    ids: &HashMap<&str, &'a Node>,
    visiting: &mut HashSet<String>,
    depth: usize,
) -> bool {
    if depth > 128 {
        return false;
    }
    if matches!(node.local(), "defs" | "metadata" | "title" | "desc") {
        return true;
    }
    if node.get("display").is_some_and(|v| v.trim() == "none")
        || node
            .get("opacity")
            .and_then(|v| v.trim().parse::<f64>().ok())
            == Some(0.0)
    {
        // Group opacity is applied AFTER that group's filters.
        return true;
    }
    // An ancestor filter can create alpha from an empty child (flood, alpha
    // offset, arithmetic composite, feImage, etc.). Do not guess its behavior.
    if node.get("filter").is_some_and(|v| v.trim() != "none") {
        return false;
    }
    match node.local() {
        "svg" | "g" | "symbol" => node
            .children
            .iter()
            .all(|n| empty(n, ids, visiting, depth + 1)),
        "use" => {
            let Some(id) = node
                .get("href")
                .or_else(|| node.get_in("href", XLINK_NS))
                .and_then(|s| s.strip_prefix('#'))
            else {
                return false;
            };
            let Some(target) = ids.get(id) else {
                return false;
            };
            if !visiting.insert(id.into()) {
                return false;
            }
            let result = empty(target, ids, visiting, depth + 1);
            visiting.remove(id);
            result
        }
        // Even opacity on fill/stroke is left unresolved: minimum-stroke and
        // authored alpha/color corrections happen later in the component pass.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invisible(body: &str) -> bool {
        let doc = crate::svg::parse(
            format!(r#"<svg xmlns="http://www.w3.org/2000/svg">{body}</svg>"#).as_bytes(),
        )
        .unwrap();
        proven_invisible(&doc.root)
    }
    #[test]
    fn follows_only_reachable_artwork_and_respects_filter_order() {
        assert!(invisible(
            r##"<defs><g id="unused"><rect width="9000" height="9000"/></g><g id="off" opacity="0"><rect width="10" height="10"/></g></defs><g><use href="#off"/></g>"##
        ));
        assert!(invisible(r#"<g opacity="0" filter="url(#f)"><rect/></g>"#));
        assert!(!invisible(
            r#"<g filter="url(#f)"><g opacity="0"><rect/></g></g>"#
        ));
        assert!(!invisible(
            r#"<g opacity="0"><rect/></g><rect width=".00001" height=".00001"/>"#
        ));
        assert!(!invisible(
            r#"<rect opacity="0.000001" width=".00001" height=".00001"/>"#
        ));
        assert!(!invisible(
            r#"<style>g {opacity: 1}</style><g opacity="0"><rect/></g>"#
        ));
        assert!(!invisible(
            r#"<g opacity="0" style="opacity:1"><rect/></g>"#
        ));
        assert!(!invisible(
            r##"<defs><g id="cycle"><use href="#cycle"/></g></defs><use href="#cycle"/>"##
        ));
        assert!(!invisible(r##"<use href="#missing"/>"##));
        assert!(!invisible(
            r#"<g visibility="hidden"><rect visibility="visible"/></g>"#
        ));
    }
}
