//! Port of `_build_component_svg` + `add_color_filters`: assemble the final
//! tight-page component SVG from the imported symbol and one task placement.

use std::collections::HashMap;

use crate::import::{
    calibrate_minimum_strokes, clone_imported_symbol, prepare_minimum_strokes, tint_filter_key,
    tint_rgb, transformed_bounds, ImportedSymbol,
};
use crate::svg::{fmt_g, matrix_text, Node, FFDEC_NS, SVG_NS, XLINK_NS};

pub const PAGE_MARGIN_PIXELS: f64 = 24.0;

/// Port of `add_color_filters`: tint filters for every (location, shade) rule
/// plus the back-part darken filter used by rear layers.
pub fn add_color_filters(
    defs: &mut Node,
    rules: &[(String, String)],
    fields: &HashMap<String, String>,
) {
    let mut sorted: Vec<(String, String)> = rules.to_vec();
    sorted.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.1.to_lowercase().cmp(&right.1.to_lowercase()))
    });
    sorted.dedup();
    for (location, shade) in sorted {
        let raw_color = fields.get(&format!("intColor{location}"));
        let color: Option<i64> = raw_color.and_then(|raw| raw.trim().parse().ok());
        let mut filter_element = Node::elem("filter");
        filter_element.set("id", tint_filter_key(&location, &shade));
        filter_element.set("x", "-100%");
        filter_element.set("y", "-100%");
        filter_element.set("width", "300%");
        filter_element.set("height", "300%");
        filter_element.set("color-interpolation-filters", "sRGB");
        let mut matrix = Node::elem("feColorMatrix");
        matrix.set("type", "matrix");
        let values = match color {
            None => "1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 1 0".to_string(),
            Some(color) => {
                let (red, green, blue) = tint_rgb(color, &shade);
                format!(
                    "0 0 0 0 {} 0 0 0 0 {} 0 0 0 0 {} 0 0 0 1 0",
                    fmt_g(red as f64 / 255.0),
                    fmt_g(green as f64 / 255.0),
                    fmt_g(blue as f64 / 255.0)
                )
            }
        };
        matrix.set("values", values);
        filter_element.append(matrix);
        defs.append(filter_element);
    }

    let mut dark = Node::elem("filter");
    dark.set("id", "aqw_back_part_dark");
    dark.set("x", "-100%");
    dark.set("y", "-100%");
    dark.set("width", "300%");
    dark.set("height", "300%");
    dark.set("color-interpolation-filters", "sRGB");
    let mut dark_matrix = Node::elem("feColorMatrix");
    dark_matrix.set("type", "matrix");
    dark_matrix.set("values", "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0");
    dark.append(dark_matrix);
    defs.append(dark);
}

/// Port of `_build_component_svg`. Returns the root element, the integer page
/// rect on the shared raster canvas, and whether the state has drawable
/// extent.
pub struct ComponentSvg {
    pub root: Node,
    pub page: [i64; 4], // left, top, right, bottom
    pub visible: bool,
    pub warnings: Vec<String>,
    pub namespaces: Vec<(Option<String>, String)>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_component_svg(
    imported: &ImportedSymbol,
    matrix: [f64; 6],
    darken: bool,
    placed_key: &str,
    layer_name: &str,
    viewbox: [f64; 4],
    raster_size: i64,
    fields: &HashMap<String, String>,
    all_color_rules: &[(String, String)],
) -> ComponentSvg {
    let mut root = Node::new("svg", Some(SVG_NS));
    root.set("version", "1.1");
    root.set_attr("objectType", Some(FFDEC_NS), "aqw-component");
    root.set("data-renderer", "ffdec-component-raster-v1");

    let mut defs = Node::elem("defs");
    let _warnings: Vec<String> = Vec::new();
    add_color_filters(&mut defs, all_color_rules, fields);

    let symbol = clone_imported_symbol(imported, placed_key);
    let layer_scale = crate::svg::affine_geometric_scale(matrix);
    let mut symbol = symbol;
    let (prepared, _malformed) = prepare_minimum_strokes(&mut symbol, layer_scale);
    if prepared > 0 {
        root.set("data-aqw-calibrated-minimum-strokes", prepared.to_string());
    }
    for definition in &symbol.definitions {
        defs.append(definition.clone());
    }
    defs.append(symbol.definition.clone());
    root.append(defs);

    let mut group = Node::elem("g");
    group.set("id", "aqw-component");
    let mut use_element = Node::elem("use");
    use_element.set("id", format!("layer-{layer_name}"));
    use_element.set_attr("href", Some(XLINK_NS), format!("#symbol_{placed_key}"));
    use_element.set("href", format!("#symbol_{placed_key}"));
    use_element.set("transform", matrix_text(matrix));
    if darken {
        use_element.set("filter", "url(#aqw_back_part_dark)");
    }
    group.append(use_element);
    root.append(group);

    let bounds = transformed_bounds(imported.bounds, matrix);
    let [x, y, width, height] = bounds;
    if width <= 0.0 || height <= 0.0 {
        return ComponentSvg {
            root,
            page: [0, 0, 0, 0],
            visible: false,
            warnings: Vec::new(),
            namespaces: imported.namespaces.clone(),
        };
    }

    let [viewbox_x, viewbox_y, viewbox_width, viewbox_height] = viewbox;
    let pixel_scale = raster_size as f64 / viewbox_width.max(viewbox_height);
    let margin = PAGE_MARGIN_PIXELS;
    let mut left = (((x - viewbox_x) * pixel_scale).floor() - margin) as i64;
    let mut top = (((y - viewbox_y) * pixel_scale).floor() - margin) as i64;
    let mut right = (((x + width - viewbox_x) * pixel_scale).ceil() + margin) as i64;
    let mut bottom = (((y + height - viewbox_y) * pixel_scale).ceil() + margin) as i64;

    // Clamp the tight page to the shared raster canvas. A component can be far
    // larger than the shared viewbox (e.g. CosmiQ's pet is ~7x the character
    // viewbox), which would otherwise rasterize an off-canvas page several
    // times the canvas size and exhaust Lambda memory (~3.6GB for a 26k x 34k
    // page). Only the on-canvas part is ever visible (the composer clips the
    // same rectangle), so clipping here changes nothing for normal components
    // (pages are already within the canvas) and bounds pathological ones.
    let canvas_w = ((viewbox_width * pixel_scale).round() as i64).max(1);
    let canvas_h = ((viewbox_height * pixel_scale).round() as i64).max(1);
    left = left.clamp(0, canvas_w);
    top = top.clamp(0, canvas_h);
    right = right.clamp(0, canvas_w);
    bottom = bottom.clamp(0, canvas_h);
    if right <= left || bottom <= top {
        return ComponentSvg {
            root,
            page: [0, 0, 0, 0],
            visible: false,
            warnings: Vec::new(),
            namespaces: imported.namespaces.clone(),
        };
    }

    let page_width = (right - left).max(1);
    let page_height = (bottom - top).max(1);
    let page_x = viewbox_x + left as f64 / pixel_scale;
    let page_y = viewbox_y + top as f64 / pixel_scale;

    let viewbox_text = format!(
        "{} {} {} {}",
        fmt_g(page_x),
        fmt_g(page_y),
        fmt_g(page_width as f64 / pixel_scale),
        fmt_g(page_height as f64 / pixel_scale)
    );
    root.set("viewBox", viewbox_text);
    root.set("width", format!("{page_width}px"));
    root.set("height", format!("{page_height}px"));

    calibrate_minimum_strokes(&mut root, 1.0);

    ComponentSvg {
        root,
        page: [left, top, right, bottom],
        visible: true,
        warnings: Vec::new(),
        namespaces: imported.namespaces.clone(),
    }
}

/// Python's filter-count diagnostic: elements whose local tag is `filter` or
/// `feGaussianBlur`.
pub fn filter_count(root: &Node) -> usize {
    root.iter()
        .filter(|node| matches!(node.local(), "filter" | "feGaussianBlur"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::import_ffdec_symbol;

    fn imported() -> ImportedSymbol {
        let source = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20.0px" width="40.0px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#ff0000"/>
    </g>
  </defs>
</svg>"##;
        import_ffdec_symbol(
            "armor",
            source,
            2.0,
            &HashMap::new(),
            "Armor",
            &HashMap::new(),
            Some(286),
        )
        .unwrap()
    }

    #[test]
    fn builds_tight_page_with_expected_geometry() {
        // 40x20 SVG at zoom 2 exported with registration translate (12, 16):
        // user bounds are (-6, -8, 20, 10). Identity placement on a 512 viewbox
        // raster: pixel_scale = 512/512 = 1, page = floor(-6)-24 ..
        let fields = HashMap::new();
        let result = build_component_svg(
            &imported(),
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            true,
            "00_chest",
            "chest",
            [0.0, 0.0, 512.0, 512.0],
            512,
            &fields,
            &[],
        );
        assert!(result.visible);
        // Unclipped page would be left = floor(-6)-24 = -30; right = ceil(14)+24 = 38;
        // top = floor(-8)-24 = -32; bottom = ceil(2)+24 = 26. The page is now
        // clamped to the shared raster canvas [0..512], so negatives clip to 0.
        assert_eq!(result.page, [0, 0, 38, 26]);
        let root = result.root;
        let viewbox = root.get("viewBox").unwrap();
        let values: Vec<f64> = viewbox
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        // Page clamped to canvas origin: viewBox starts at the shared canvas
        // origin (0,0) and the page is 38x26 px.
        assert!((values[0] - 0.0).abs() < 1e-9);
        assert!((values[1] - 0.0).abs() < 1e-9);
        assert_eq!(root.get("width"), Some("38px"));
        assert_eq!(root.get("height"), Some("26px"));
        // Darkened use carries the back-part filter; no color rules so only
        // the dark filter exists.
        let use_element = root
            .iter()
            .find(|node| node.local() == "use" && node.get("id") == Some("layer-chest"))
            .unwrap();
        assert_eq!(use_element.get("filter"), Some("url(#aqw_back_part_dark)"));
        assert_eq!(filter_count(&root), 1);
    }

    #[test]
    fn add_color_filters_builds_tints_and_darken() {
        let mut defs = Node::elem("defs");
        let mut fields = HashMap::new();
        fields.insert("intColorBase".to_string(), "16711680".to_string());
        add_color_filters(
            &mut defs,
            &[("Base".to_string(), "dark".to_string())],
            &fields,
        );
        let ids: Vec<String> = defs
            .iter()
            .filter(|node| node.local() == "filter")
            .map(|node| node.get("id").unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["aqw_tint_base_dark", "aqw_back_part_dark"]);
        // dark tint of pure red: (-25, -50, -50) -> (230, 0, 0).
        let tint = defs
            .iter()
            .find(|node| node.get("id") == Some("aqw_tint_base_dark"))
            .unwrap();
        let matrix = tint
            .children
            .iter()
            .find(|node| node.local() == "feColorMatrix")
            .unwrap();
        let values = matrix.get("values").unwrap();
        assert!(
            values.starts_with("0 0 0 0 0.901960784"),
            "values: {values}"
        );
    }

    #[test]
    fn invisible_state_has_no_page() {
        let imported = imported();
        let mut zero = imported;
        zero.bounds = [0.0, 0.0, 0.0, 0.0];
        let result = build_component_svg(
            &zero,
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            false,
            "01_weapon",
            "weapon",
            [0.0, 0.0, 512.0, 512.0],
            512,
            &HashMap::new(),
            &[],
        );
        assert!(!result.visible);
        assert_eq!(result.page, [0, 0, 0, 0]);
    }
}
