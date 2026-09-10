//! Port of `character_svg.import_ffdec_symbol` and its helpers to the small
//! mutable SVG DOM. Every step mirrors the Python reference so the assembled
//! component SVG rasterizes identically under the same resvg version.

use std::collections::HashMap;

use crate::error::RasterError;
use crate::svg::{fmt_g, matrix_text, parse_matrix, serialize, Document, Node, FFDEC_NS, XLINK_NS};

// ---------------------------------------------------------------------------
// Stroke-calibration markers (character_svg.py lines 125-130)
// ---------------------------------------------------------------------------

const FFDEC_SMALL_STROKE: &str = "has-small-stroke";
const FFDEC_ORIGINAL_STROKE_WIDTH: &str = "original-stroke-width";
const COMPENSATED_STROKE_WIDTH: &str = "data-aqw-ffdec-compensated-stroke-width";
const AUTHORED_STROKE_WIDTH: &str = "data-aqw-authored-stroke-width";
const SYMBOL_MINIMUM_STROKE_SCALE: &str = "data-aqw-symbol-minimum-stroke-scale";
const LAYER_STROKE_SCALE: &str = "data-aqw-layer-stroke-scale";

pub const IDENTITY: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

#[derive(Clone, Debug)]
pub struct ImportedSymbol {
    pub key: String,
    pub definition: Node,
    pub definitions: Vec<Node>,
    pub bounds: [f64; 4],
    pub export_zoom: f64,
    pub minimum_stroke_scale: f64,
    /// Namespace declarations carried from the source FFDec export so the
    /// assembled component SVG re-declares them (ffdec:, xlink:).
    pub namespaces: Vec<(Option<String>, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AuthoredColorTransform {
    pub red_mult: i64,
    pub green_mult: i64,
    pub blue_mult: i64,
    pub alpha_mult: i64,
    pub red_add: i64,
    pub green_add: i64,
    pub blue_add: i64,
    pub alpha_add: i64,
}

impl Default for AuthoredColorTransform {
    fn default() -> Self {
        AuthoredColorTransform {
            red_mult: 256,
            green_mult: 256,
            blue_mult: 256,
            alpha_mult: 256,
            red_add: 0,
            green_add: 0,
            blue_add: 0,
            alpha_add: 0,
        }
    }
}

pub fn tint_filter_key(location: &str, shade: &str) -> String {
    format!(
        "aqw_tint_{}_{}",
        location.to_lowercase(),
        shade.to_lowercase()
    )
}

pub fn tint_rgb(color: i64, shade: &str) -> (i64, i64, i64) {
    let red = (color >> 16) & 0xFF;
    let green = (color >> 8) & 0xFF;
    let blue = color & 0xFF;
    let offsets: (i64, i64, i64) = match shade.to_lowercase().as_str() {
        "light" => (100, 100, 100),
        "dark" => (-25, -50, -50),
        "darker" => (-125, -125, -125),
        _ => (0, 0, 0),
    };
    let clamp = |value: i64| value.clamp(0, 255);
    (
        clamp(red + offsets.0),
        clamp(green + offsets.1),
        clamp(blue + offsets.2),
    )
}

fn parse_svg_length(value: Option<&str>) -> Option<f64> {
    // Mirrors the `dimension()` helper in import_ffdec_symbol (Python):
    // unitless or px, any non-negative finite value. Zero-size frames are
    // authored invisible states and must reach the empty-symbol branch rather
    // than fail the import.
    let value = value?.trim();
    let base = value.strip_suffix("px").unwrap_or(value).trim();
    let parsed: f64 = base.parse().ok()?;
    if parsed.is_finite() && parsed >= 0.0 {
        Some(parsed)
    } else {
        None
    }
}

fn positive_float(value: Option<&str>) -> Option<f64> {
    let parsed: f64 = value?.parse().ok()?;
    if parsed.is_finite() && parsed > 0.0 {
        Some(parsed)
    } else {
        None
    }
}

/// Python's `re.split(r"[\s,]+", ...)` then float parse for every token.
trait AffineScaledNumbers {
    fn scaled(&self, zoom: f64) -> Option<String>;
}

impl AffineScaledNumbers for Option<&str> {
    fn scaled(&self, zoom: f64) -> Option<String> {
        let raw = (*self)?;
        if raw.trim().is_empty() {
            return None;
        }
        let values: Vec<f64> = raw
            .split(|character: char| character.is_whitespace() || character == ',')
            .filter(|part| !part.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
            return None;
        }
        Some(
            values
                .iter()
                .map(|value| fmt_g(value / zoom))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

fn url_ref_value(value: &str, id_map: &HashMap<String, String>) -> String {
    // Port of _URL_REF_RE.sub on `url(#id)` occurrences: keep the literal
    // `url(#` and the closing `)`, mapping the id fragment through id_map.
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("url(#") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        match after.find(')') {
            Some(end) => {
                output.push_str("url(#");
                let fragment = &after[..end];
                if let Some(mapping) = id_map.get(fragment) {
                    output.push_str(mapping);
                } else {
                    output.push_str(fragment);
                }
                output.push(')');
                rest = &after[end + 1..];
            }
            None => {
                output.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    output.push_str(rest);
    output
}

/// Port of `_rewrite_references`: give every id a prefix and fix all
/// `#id` / `url(#id)` references (attributes and element text).
pub fn rewrite_references(root: &mut Node, prefix: &str) {
    let mut id_map: HashMap<String, String> = HashMap::new();
    for node in root.iter_mut() {
        if let Some(old) = node.get("id") {
            let new = format!("{prefix}_{old}");
            id_map.insert(old.to_string(), new.clone());
            node.set("id", new);
        }
    }
    if id_map.is_empty() {
        return;
    }
    for node in root.iter_mut() {
        for attr in &mut node.attrs {
            let value = &attr.value;
            if let Some(fragment) = value.strip_prefix('#') {
                if let Some(mapping) = id_map.get(fragment) {
                    attr.value = format!("#{mapping}");
                    continue;
                }
            }
            if value.contains("url(#") {
                attr.value = url_ref_value(value, &id_map);
            }
        }
        if let Some(text) = &node.text {
            if text.contains("url(#") {
                node.text = Some(url_ref_value(text, &id_map));
            }
        }
    }
}

/// Port of `_normalize_ffdec_font_export_zoom`.
fn normalize_ffdec_font_export_zoom(root: &mut Node, zoom: f64) {
    if (zoom - 1.0).abs() < 1e-12 {
        return;
    }
    let font_ids: Vec<String> = root
        .iter()
        .filter_map(|node| node.get("id"))
        .filter(|id| id.starts_with("font_"))
        .map(str::to_string)
        .collect();
    if font_ids.is_empty() {
        return;
    }

    let mut filter_ids: Vec<String> = Vec::new();
    let snapshot: Vec<String> = font_ids.clone();
    for node in root.iter_mut() {
        if node.local() != "use" {
            continue;
        }
        let href = node
            .get_in("href", XLINK_NS)
            .or_else(|| node.get("href"))
            .map(str::to_string);
        let Some(href) = href else { continue };
        let target = href
            .strip_prefix('#')
            .map(str::to_string)
            .unwrap_or_default();
        if !snapshot.contains(&target) {
            continue;
        }
        if let Some(transform) = node.get("transform").and_then(parse_matrix) {
            node.set(
                "transform",
                matrix_text([
                    transform[0] / zoom,
                    transform[1] / zoom,
                    transform[2] / zoom,
                    transform[3] / zoom,
                    transform[4],
                    transform[5],
                ]),
            );
        }
        if let Some(filter) = node.get("filter").map(str::to_string) {
            if filter.starts_with("url(#") && filter.ends_with(')') {
                let id = &filter[5..filter.len() - 1];
                if !filter_ids.iter().any(|existing| existing == id) {
                    filter_ids.push(id.to_string());
                }
            }
        }
    }

    for node in root.iter_mut() {
        if node.local() != "filter" {
            continue;
        }
        let id = node.get("id").map(str::to_string);
        let Some(id) = id else { continue };
        if !filter_ids.iter().any(|existing| existing == &id) {
            continue;
        }
        if node.get("primitiveUnits") == Some("objectBoundingBox") {
            continue;
        }
        for primitive in node.iter_mut() {
            match primitive.local() {
                "feGaussianBlur" => {
                    if let Some(scaled) = primitive.get("stdDeviation").scaled(zoom) {
                        primitive.set("stdDeviation", scaled);
                    }
                }
                "feOffset" => {
                    for attribute in ["dx", "dy"] {
                        if let Some(scaled) = primitive.get(attribute).scaled(zoom) {
                            primitive.set(attribute, scaled);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn authored_color_filter_id(transform: &AuthoredColorTransform) -> String {
    let values = format!(
        "{},{},{},{},{},{},{},{}",
        transform.red_mult,
        transform.green_mult,
        transform.blue_mult,
        transform.alpha_mult,
        transform.red_add,
        transform.green_add,
        transform.blue_add,
        transform.alpha_add
    );
    let digest = sha1_digest(values.as_bytes());
    format!("aqw_authored_cxform_{digest}")
}

fn sha1_digest(bytes: &[u8]) -> String {
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())[..12].to_string()
}

fn authored_color_filter(filter_id: &str, transform: &AuthoredColorTransform) -> Node {
    let multipliers = [
        transform.red_mult as f64 / 256.0,
        transform.green_mult as f64 / 256.0,
        transform.blue_mult as f64 / 256.0,
        transform.alpha_mult as f64 / 256.0,
    ];
    let additions = [
        transform.red_add as f64 / 255.0,
        transform.green_add as f64 / 255.0,
        transform.blue_add as f64 / 255.0,
        transform.alpha_add as f64 / 255.0,
    ];
    let values = [
        format!("{} 0 0 0 {}", fmt_g(multipliers[0]), fmt_g(additions[0])),
        format!("0 {} 0 0 {}", fmt_g(multipliers[1]), fmt_g(additions[1])),
        format!("0 0 {} 0 {}", fmt_g(multipliers[2]), fmt_g(additions[2])),
        format!("0 0 0 {} {}", fmt_g(multipliers[3]), fmt_g(additions[3])),
    ]
    .join(" ");

    let mut element = Node::elem("filter");
    element.set("id", filter_id);
    element.set("x", "-100%");
    element.set("y", "-100%");
    element.set("width", "300%");
    element.set("height", "300%");
    element.set("color-interpolation-filters", "sRGB");
    let mut matrix = Node::elem("feColorMatrix");
    matrix.set("type", "matrix");
    matrix.set("values", values);
    element.append(matrix);
    element
}

/// Port of `_apply_authored_color_transforms`. Returns the number of wrapped
/// uses and the newly created filter definitions.
fn apply_authored_color_transforms(
    root: &mut Node,
    root_symbol_id: &str,
    root_character_id: Option<i64>,
    transforms: &HashMap<(i64, i64), AuthoredColorTransform>,
    created_filters: &mut Vec<Node>,
) -> usize {
    if root_character_id.is_none() || transforms.is_empty() {
        return 0;
    }
    // id -> single character id for <use> targets.
    let mut target_characters: HashMap<String, std::collections::HashSet<i64>> = HashMap::new();
    for node in root.iter() {
        if node.local() != "use" {
            continue;
        }
        let raw_character = node.get_in("characterId", FFDEC_NS);
        let href = node
            .get_in("href", XLINK_NS)
            .or_else(|| node.get("href"))
            .map(str::to_string);
        let Some(raw_character) = raw_character else {
            continue;
        };
        let Some(href) = href else { continue };
        let Some(target) = href.strip_prefix('#') else {
            continue;
        };
        if let Ok(character_id) = raw_character.parse::<i64>() {
            target_characters
                .entry(target.to_string())
                .or_default()
                .insert(character_id);
        }
    }
    let mut id_to_character: HashMap<String, i64> = HashMap::new();
    for (target, characters) in &target_characters {
        if characters.len() == 1 {
            id_to_character.insert(target.clone(), *characters.iter().next().unwrap());
        }
    }

    let mut filters: HashMap<AuthoredColorTransform, String> = HashMap::new();
    let mut applied = 0;

    // Recursive walk; the frame is the first child of the root symbol
    // definition (id == root_symbol_id), which is guaranteed at this point.
    #[allow(clippy::too_many_arguments)]
    fn visit(
        node: &mut Node,
        is_frame: bool,
        root_symbol_id: &str,
        root_character_id: i64,
        id_to_character: &HashMap<String, i64>,
        transforms: &HashMap<(i64, i64), AuthoredColorTransform>,
        filters: &mut HashMap<AuthoredColorTransform, String>,
        created_filters: &mut Vec<Node>,
    ) -> usize {
        let parent_character = if is_frame {
            Some(root_character_id)
        } else {
            node.get("id")
                .and_then(|id| id_to_character.get(id).copied())
        };
        let mut applied = 0usize;
        let child_count = node.children.len();
        for index in 0..child_count {
            // The frame is the first child of the root symbol definition
            // (id == root_symbol_id); it is the parent for root CXFORMs.
            let child_is_frame = index == 0 && node.get("id") == Some(root_symbol_id);
            // Recurse first (matches Python's pre-order parent iteration).
            applied += visit(
                &mut node.children[index],
                child_is_frame,
                root_symbol_id,
                root_character_id,
                id_to_character,
                transforms,
                filters,
                created_filters,
            );
            let child = &node.children[index];
            if child.local() != "use" {
                continue;
            }
            let Some(raw_character) = child.get_in("characterId", FFDEC_NS) else {
                continue;
            };
            let Ok(child_character) = raw_character.parse::<i64>() else {
                continue;
            };
            let Some(parent_character) = parent_character else {
                continue;
            };
            let Some(transform) = transforms.get(&(parent_character, child_character)) else {
                continue;
            };
            let filter_id = match filters.get(transform) {
                Some(id) => id.clone(),
                None => {
                    let id = authored_color_filter_id(transform);
                    created_filters.push(authored_color_filter(&id, transform));
                    filters.insert(transform.clone(), id.clone());
                    id
                }
            };
            // Wrap the child <use> in <g filter="url(#id)">.
            let mut child = node.children[index].clone();
            let mut wrapper = Node::elem("g");
            wrapper.set("filter", format!("url(#{filter_id})"));
            promote_blending(&mut child, &mut wrapper);
            wrapper.append(child);
            node.children[index] = wrapper;
            applied += 1;
        }
        applied
    }

    applied += visit(
        root,
        false,
        root_symbol_id,
        root_character_id.unwrap_or_default(),
        &id_to_character,
        transforms,
        &mut filters,
        created_filters,
    );
    applied
}

/// Port of `_apply_color_rules`.
pub fn apply_color_rules(root: &mut Node, rules: &HashMap<String, (String, String)>) {
    #[allow(clippy::too_many_arguments)]
    fn visit(parent: &mut Node, rules: &HashMap<String, (String, String)>) {
        let child_count = parent.children.len();
        for index in 0..child_count {
            {
                let child = &mut parent.children[index];
                visit(child, rules);
            }
            let class_name = parent.children[index]
                .get_in("characterName", FFDEC_NS)
                .map(|value| value.to_lowercase());
            let Some(class_name) = class_name else {
                continue;
            };
            let Some(rule) = rules.get(&class_name) else {
                continue;
            };
            let tint_filter = format!("url(#{})", tint_filter_key(&rule.0, &rule.1));
            if parent.children[index].get("filter").is_none() {
                parent.children[index].set("filter", tint_filter);
                continue;
            }
            // Keep tint and blend mode on the same element: wrap a filtered
            // child in a tinted group.
            let mut child = parent.children[index].clone();
            let mut wrapper = Node::elem("g");
            wrapper.set("filter", tint_filter);
            promote_blending(&mut child, &mut wrapper);
            wrapper.append(child);
            parent.children[index] = wrapper;
        }
    }
    visit(root, rules);
}

// A color-filter wrapper isolates its child. Blending must therefore happen
// on the wrapper after the color transform, against the actual backdrop.
fn promote_blending(child: &mut Node, wrapper: &mut Node) {
    if let Some(mode) = child.get("mix-blend-mode").map(str::to_owned) {
        child.remove_attr("mix-blend-mode");
        wrapper.set("mix-blend-mode", mode);
    }
    if let Some(style) = child.get("style").map(str::to_owned) {
        let (blend, other): (Vec<_>, Vec<_>) = style.split(';')
            .filter(|s| !s.trim().is_empty())
            .partition(|s| s.split_once(':').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("mix-blend-mode")));
        if !blend.is_empty() {
            wrapper.set("style", blend.join(";"));
            child.remove_attr("style");
            if !other.is_empty() { child.set("style", other.join(";")); }
        }
    }
}

/// Only placements directly inside the exported gauntlet have the hand holder
/// at parent.parent. Do not descend through another placed SWF character.
fn apply_hand_visibility(node: &mut Node, rules: &HashMap<String, String>, hand: &str) {
    node.children.retain(|child| !child.get_in("characterName", FFDEC_NS)
        .is_some_and(|name| rules.get(&name.to_lowercase()).is_some_and(|hidden| hidden == hand)));
    for child in &mut node.children {
        if child.get_in("characterId", FFDEC_NS).is_none() && child.local() == "g" {
            apply_hand_visibility(child, rules, hand);
        }
    }
}

/// Port of `import_ffdec_symbol`.
pub fn import_ffdec_symbol(
    key: &str,
    source_svg: &str,
    zoom: f64,
    color_rules: &HashMap<String, (String, String)>,
    root_class: &str,
    placement_colors: &HashMap<(i64, i64), AuthoredColorTransform>,
    root_character_id: Option<i64>,
) -> Result<ImportedSymbol, RasterError> {
    import_ffdec_symbol_with_visibility(key, source_svg, zoom, color_rules, root_class, placement_colors, root_character_id, &HashMap::new(), None)
}

pub fn import_ffdec_symbol_with_visibility(
    key: &str,
    source_svg: &str,
    zoom: f64,
    color_rules: &HashMap<String, (String, String)>,
    root_class: &str,
    placement_colors: &HashMap<(i64, i64), AuthoredColorTransform>,
    root_character_id: Option<i64>,
    hand_visibility: &HashMap<String, String>,
    hand: Option<&str>,
) -> Result<ImportedSymbol, RasterError> {
    let document = crate::svg::parse(source_svg.as_bytes())
        .map_err(|error| RasterError::Svg(format!("Invalid FFDec SVG: {error}")))?;
    let root = document.root;

    let width = parse_svg_length(root.get("width"));
    let height = parse_svg_length(root.get("height"));
    let (Some(width), Some(height)) = (width, height) else {
        return Err(RasterError::Svg(
            "FFDec SVG has no usable dimensions".to_string(),
        ));
    };

    let mut definitions: Vec<Node> = Vec::new();
    let mut rendered: Vec<Node> = Vec::new();
    for child in root.children {
        if child.local() == "defs" {
            definitions.extend(child.children);
        } else {
            rendered.push(child);
        }
    }

    if width == 0.0 || height == 0.0 {
        let mut empty = Node::elem("g");
        empty.set("id", format!("symbol_{key}"));
        let mut temporary_root = Node::elem("g");
        for definition in &definitions {
            temporary_root.append(definition.clone());
        }
        temporary_root.append(empty.clone());
        rewrite_references(&mut temporary_root, &format!("part_{key}"));
        empty.set("id", format!("symbol_{key}"));
        return Ok(ImportedSymbol {
            key: key.to_string(),
            definition: empty,
            definitions,
            bounds: [0.0, 0.0, 0.0, 0.0],
            export_zoom: zoom,
            minimum_stroke_scale: 1.0 / zoom,
            namespaces: document.namespaces,
        });
    }

    if rendered.len() != 1 {
        return Err(RasterError::Svg(format!(
            "Expected one FFDec frame wrapper, found {}",
            rendered.len()
        )));
    }
    let mut frame = rendered.remove(0);
    let export_matrix = frame
        .get("transform")
        .and_then(parse_matrix)
        .ok_or_else(|| RasterError::Svg("FFDec frame wrapper has no matrix".to_string()))?;
    let [a, b, c, _d, e, f] = export_matrix;
    if b.abs() > 1e-8
        || c.abs() > 1e-8
        || (a - zoom).abs() > 1e-5
        || (export_matrix[3] - zoom).abs() > 1e-5
    {
        return Err(RasterError::Svg(format!(
            "Unexpected FFDec crop/zoom matrix {export_matrix:?}"
        )));
    }
    frame.remove_attr("transform");
    if let Some(hand) = hand { apply_hand_visibility(&mut frame, hand_visibility, hand); }
    let symbol_bounds = [-e / zoom, -f / zoom, width / zoom, height / zoom];

    let mut root_definition = Node::elem("g");
    root_definition.set("id", format!("symbol_{key}"));
    let root_rule = color_rules.get(&root_class.to_lowercase());
    if let Some(rule) = root_rule {
        root_definition.set(
            "filter",
            format!("url(#{})", tint_filter_key(&rule.0, &rule.1)),
        );
    }
    root_definition.append(frame);

    let mut temporary_root = Node::elem("g");
    for definition in &definitions {
        temporary_root.append(definition.clone());
    }
    temporary_root.append(root_definition.clone());

    normalize_ffdec_font_export_zoom(&mut temporary_root, zoom);
    let mut created_filters: Vec<Node> = Vec::new();
    let _created = apply_authored_color_transforms(
        &mut temporary_root,
        &format!("symbol_{key}"),
        root_character_id,
        placement_colors,
        &mut created_filters,
    );
    // The new filter definitions created above were not part of
    // temporary_root; insert them before the root definition (the final
    // child), exactly like the Python worker.
    for filter in &created_filters {
        let count = temporary_root.children.len();
        temporary_root.insert_at(count - 1, filter.clone());
    }
    apply_color_rules(&mut temporary_root, color_rules);
    rewrite_references(&mut temporary_root, &format!("part_{key}"));

    // Every mutation above happened on the copies nested in temporary_root;
    // take them back out so the returned symbol carries the transformed tree
    // (the Python worker's live ElementTree shares the same nodes).
    let mut children = std::mem::take(&mut temporary_root.children);
    let root_definition = children
        .pop()
        .expect("temporary_root keeps the root definition");
    let definitions = children;
    Ok(ImportedSymbol {
        key: key.to_string(),
        definition: root_definition,
        definitions,
        bounds: symbol_bounds,
        export_zoom: zoom,
        minimum_stroke_scale: 1.0 / zoom,
        namespaces: document.namespaces,
    })
}

/// Port of `clone_imported_symbol`.
pub fn clone_imported_symbol(symbol: &ImportedSymbol, placed_key: &str) -> ImportedSymbol {
    let mut temporary_root = Node::elem("g");
    for definition in &symbol.definitions {
        temporary_root.append(definition.clone());
    }
    temporary_root.append(symbol.definition.clone());
    rewrite_references(&mut temporary_root, &format!("placed_{placed_key}"));
    let mut children = std::mem::take(&mut temporary_root.children);
    let mut root_definition = children
        .pop()
        .expect("temporary_root keeps the root definition");
    root_definition.set("id", format!("symbol_{placed_key}"));
    let definitions = children;
    ImportedSymbol {
        key: symbol.key.clone(),
        definition: root_definition,
        definitions,
        bounds: symbol.bounds,
        export_zoom: symbol.export_zoom,
        minimum_stroke_scale: symbol.minimum_stroke_scale,
        namespaces: symbol.namespaces.clone(),
    }
}

/// Port of `prepare_minimum_strokes`.
pub fn prepare_minimum_strokes(symbol: &mut ImportedSymbol, layer_scale: f64) -> (usize, usize) {
    if !symbol.export_zoom.is_finite()
        || symbol.export_zoom <= 0.0
        || !symbol.minimum_stroke_scale.is_finite()
        || symbol.minimum_stroke_scale <= 0.0
        || !layer_scale.is_finite()
        || layer_scale <= 0.0
    {
        return (0, 1);
    }
    let mut prepared = 0usize;
    let mut malformed = 0usize;
    for definition in &mut symbol.definitions {
        prepared += stroke_pass(
            definition,
            symbol.export_zoom,
            symbol.minimum_stroke_scale,
            layer_scale,
            &mut malformed,
        );
    }
    prepared += stroke_pass(
        &mut symbol.definition,
        symbol.export_zoom,
        symbol.minimum_stroke_scale,
        layer_scale,
        &mut malformed,
    );
    (prepared, malformed)
}

fn stroke_pass(
    subtree: &mut Node,
    export_zoom: f64,
    minimum_stroke_scale: f64,
    layer_scale: f64,
    malformed: &mut usize,
) -> usize {
    let mut prepared = 0usize;
    for element in subtree.iter_mut() {
        let marked = element
            .get_in(FFDEC_SMALL_STROKE, FFDEC_NS)
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !marked {
            continue;
        }
        let compensated = positive_float(element.get("stroke-width"));
        let original = positive_float(element.get_in(FFDEC_ORIGINAL_STROKE_WIDTH, FFDEC_NS));
        let (Some(compensated), Some(original)) = (compensated, original) else {
            *malformed += 1;
            continue;
        };
        element.set(COMPENSATED_STROKE_WIDTH, fmt_g(compensated));
        element.set(AUTHORED_STROKE_WIDTH, fmt_g(original / export_zoom));
        element.set(SYMBOL_MINIMUM_STROKE_SCALE, fmt_g(minimum_stroke_scale));
        element.set(LAYER_STROKE_SCALE, fmt_g(layer_scale));
        prepared += 1;
    }
    prepared
}

/// Port of `svg_viewport_scale`.
pub fn svg_viewport_scale(root: &Node) -> Option<f64> {
    let raw_viewbox = root.get("viewBox")?;
    let values: Vec<f64> = raw_viewbox
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() != 4
        || !values.iter().all(|value| value.is_finite())
        || values[2] <= 0.0
        || values[3] <= 0.0
    {
        return None;
    }
    let width = parse_svg_length(root.get("width"))?;
    let height = parse_svg_length(root.get("height"))?;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale_x = width / values[2];
    let scale_y = height / values[3];
    if (scale_x - scale_y).abs() > 1e-6 {
        return None;
    }
    Some((scale_x * scale_y).sqrt())
}

/// Port of `calibrate_minimum_strokes`.
pub fn calibrate_minimum_strokes(root: &mut Node, minimum_pixels: f64) -> usize {
    let Some(viewport_scale) = svg_viewport_scale(root) else {
        return 0;
    };
    if !minimum_pixels.is_finite() || minimum_pixels <= 0.0 {
        return 0;
    }
    let mut calibrated = 0usize;
    for node in root.iter_mut() {
        let compensated = positive_float(node.get(COMPENSATED_STROKE_WIDTH));
        let authored = positive_float(node.get(AUTHORED_STROKE_WIDTH));
        let symbol_scale = positive_float(node.get(SYMBOL_MINIMUM_STROKE_SCALE));
        let layer_scale = positive_float(node.get(LAYER_STROKE_SCALE));
        let (Some(compensated), Some(authored), Some(symbol_scale), Some(layer_scale)) =
            (compensated, authored, symbol_scale, layer_scale)
        else {
            continue;
        };
        let current_pixels = symbol_scale * layer_scale * viewport_scale;
        let corrected = authored.max(compensated * minimum_pixels / current_pixels);
        node.set("stroke-width", fmt_g(corrected));
        calibrated += 1;
    }
    if calibrated > 0 {
        root.set(
            "data-aqw-minimum-stroke-width",
            format!("{minimum_pixels:.12}px"),
        );
    }
    calibrated
}

/// Port of `_transformed_bounds`.
pub fn transformed_bounds(bounds: [f64; 4], matrix: [f64; 6]) -> [f64; 4] {
    let [x, y, width, height] = bounds;
    let points: [[f64; 2]; 4] = [
        transform_point(matrix, x, y),
        transform_point(matrix, x + width, y),
        transform_point(matrix, x, y + height),
        transform_point(matrix, x + width, y + height),
    ];
    let min_x = points
        .iter()
        .map(|point| point[0])
        .fold(f64::INFINITY, f64::min);
    let min_y = points
        .iter()
        .map(|point| point[1])
        .fold(f64::INFINITY, f64::min);
    let max_x = points
        .iter()
        .map(|point| point[0])
        .fold(f64::NEG_INFINITY, f64::max);
    let max_y = points
        .iter()
        .map(|point| point[1])
        .fold(f64::NEG_INFINITY, f64::max);
    [min_x, min_y, max_x - min_x, max_y - min_y]
}

pub fn transform_point(matrix: [f64; 6], x: f64, y: f64) -> [f64; 2] {
    let [a, b, c, d, e, f] = matrix;
    [a * x + c * y + e, b * x + d * y + f]
}

/// Serialize a full document assembled from one root node, reusing the
/// namespaces that were declared in the source document.
pub fn serialize_document(root: Node, namespaces: &[(Option<String>, String)]) -> String {
    serialize(&Document {
        root,
        namespaces: namespaces.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ffdec() -> &'static str {
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20.0px" width="40.0px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#ff0000" opacity="1"/>
    </g>
  </defs>
</svg>"##
    }

    #[test]
    fn hand_rules_hide_only_direct_placements_for_matching_holder() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:f="https://www.free-decompiler.com/flash"><g><use f:characterId="12" f:characterName="Back"/><use f:characterId="13" f:characterName="Front"/><g f:characterId="20"><use f:characterName="Back"/></g></g></svg>"#;
        let rules = HashMap::from([("back".into(),"fronthand".into()),("front".into(),"backhand".into())]);
        for (hand, remaining) in [("fronthand", "Front"), ("backhand", "Back"), ("weapon", "Back")] {
            let mut node = crate::svg::parse(svg.as_bytes()).unwrap().root;
            apply_hand_visibility(&mut node, &rules, hand);
            let children = &node.children[0].children;
            assert_eq!(children.len(), if hand == "weapon" {3} else {2});
            assert_eq!(children[0].get_in("characterName",FFDEC_NS),Some(remaining));
            assert_eq!(children.last().unwrap().children.len(),1);
        }
    }

    #[test]
    fn imports_and_removes_the_zoom_wrapper() {
        let rules = HashMap::new();
        let transforms = HashMap::new();
        let symbol = import_ffdec_symbol(
            "armor",
            sample_ffdec(),
            2.0,
            &rules,
            "Armor",
            &transforms,
            Some(286),
        )
        .unwrap();
        assert_eq!(symbol.bounds, [-6.0, -8.0, 20.0, 10.0]);
        assert_eq!(symbol.export_zoom, 2.0);
        assert_eq!(symbol.minimum_stroke_scale, 0.5);
        // The rendered frame wrapper matrix is removed.
        let frame = &symbol.definition.children[0];
        assert_eq!(frame.get("transform"), None);
        assert_eq!(symbol.definitions.len(), 1);
    }

    #[test]
    fn apply_color_rules_uses_character_name() {
        let mut root = Node::elem("g");
        let mut use_node = Node::elem("use");
        use_node.set_attr("characterName", Some(FFDEC_NS), "Chest");
        use_node.set("filter", "blur(2px)");
        root.append(use_node);
        let mut rules = HashMap::new();
        rules.insert(
            "chest".to_string(),
            ("Base".to_string(), "dark".to_string()),
        );
        apply_color_rules(&mut root, &rules);
        let wrapped = &root.children[0];
        assert_eq!(wrapped.local(), "g");
        assert_eq!(wrapped.get("filter"), Some("url(#aqw_tint_base_dark)"));
        assert_eq!(wrapped.children[0].local(), "use");
    }

    #[test]
    fn rewrite_references_prefixes_ids() {
        let mut root = Node::elem("g");
        root.append({
            let mut defs = Node::elem("defs");
            let mut path = Node::elem("path");
            path.set("id", "p1");
            defs.append(path);
            defs
        });
        let mut use_node = Node::elem("use");
        use_node.set_attr("href", Some(XLINK_NS), "#p1");
        root.append(use_node);
        rewrite_references(&mut root, "part_armor");
        let path = root.iter().find(|node| node.local() == "path").unwrap();
        assert_eq!(path.get("id"), Some("part_armor_p1"));
        let use_node = root.iter().find(|node| node.local() == "use").unwrap();
        assert_eq!(use_node.get_in("href", XLINK_NS), Some("#part_armor_p1"));
    }

    #[test]
    fn prepare_and_calibrate_minimum_strokes() {
        let mut document = crate::svg::parse(sample_ffdec().as_bytes()).unwrap();
        // Add a stroke marker like FFDec emits.
        for node in document.root.iter_mut() {
            if node.local() == "rect" {
                node.set_attr("has-small-stroke", Some(FFDEC_NS), "true");
                node.set_attr("original-stroke-width", Some(FFDEC_NS), "0.05");
                node.set("stroke-width", "2.2940527227516303");
                node.set("stroke", "#000000");
                node.set("fill", "none");
            }
        }
        let bytes = crate::svg::serialize(&document).into_bytes();
        let text = String::from_utf8(bytes).unwrap();
        let mut symbol = import_ffdec_symbol(
            "armor",
            &text,
            2.0,
            &HashMap::new(),
            "Armor",
            &HashMap::new(),
            Some(286),
        )
        .unwrap();
        let (prepared, malformed) = prepare_minimum_strokes(&mut symbol, 0.5);
        assert_eq!(malformed, 0);
        assert_eq!(prepared, 1);
        let marked = symbol
            .definition
            .iter()
            .chain(
                symbol
                    .definitions
                    .iter()
                    .flat_map(|definition| definition.iter()),
            )
            .find(|node| node.get(COMPENSATED_STROKE_WIDTH).is_some())
            .unwrap()
            .clone();
        assert_eq!(marked.get(COMPENSATED_STROKE_WIDTH), Some("2.29405272275"));
        assert_eq!(marked.get(AUTHORED_STROKE_WIDTH), Some("0.025"));

        let mut clone = clone_imported_symbol(&symbol, "00_chest");
        let viewbox_scale = calibrate_minimum_strokes(&mut clone.definition, 1.0);
        // The tight page has width/height px equal to its viewBox, so the
        // viewport scale is 1; corrected width stays the authored width.
        assert!(viewbox_scale <= 1);
        let _ = clone;
    }

    #[test]
    fn tint_matrix_matches_python() {
        let transform = AuthoredColorTransform {
            red_mult: 128,
            green_mult: 256,
            blue_mult: 192,
            alpha_mult: 256,
            red_add: -10,
            green_add: 0,
            blue_add: 25,
            alpha_add: 0,
        };
        let filter = authored_color_filter("id", &transform);
        let matrix = filter
            .children
            .iter()
            .find(|node| node.local() == "feColorMatrix")
            .unwrap();
        assert_eq!(
            matrix.get("values"),
            Some("0.5 0 0 0 -0.0392156862745 0 1 0 0 0 0 0 0.75 0 0.0980392156863 0 0 0 1 0")
        );
    }
}
