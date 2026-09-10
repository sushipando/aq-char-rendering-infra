use aqw_component_raster::{import, raster, svg};
use std::collections::HashMap;

fn pixel(body: &str) -> Vec<u8> {
    let source =
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">{body}</svg>"#);
    raster::render_svg_resvg(source.as_bytes(), (4, 4))
        .unwrap()
        .pixels[..4]
        .to_vec()
}

#[test]
fn additive_rgb_preserves_backdrop_and_uses_source_over_alpha() {
    assert_eq!(
        pixel(
            r##"<rect width="4" height="4" fill="#804020"/><g style="mix-blend-mode: aqw-add"><rect width="4" height="4" fill="#208040"/></g>"##
        ),
        [160, 192, 96, 255]
    );
    let translucent = pixel(
        r##"<rect width="4" height="4" fill="#200000" opacity="0.5"/><g opacity="0.5" style="mix-blend-mode: aqw-add"><rect width="4" height="4" fill="#002000"/></g>"##,
    );
    assert_eq!(translucent[3], 192); // Plus would incorrectly produce 255.
    assert!((20..=23).contains(&translucent[0]) && (20..=23).contains(&translucent[1]));
    assert_eq!(
        pixel(
            r##"<rect width="4" height="4" fill="#f00000"/><g style="mix-blend-mode: aqw-add"><rect width="4" height="4" fill="#800000"/></g>"##
        ),
        [255, 0, 0, 255]
    );
    // Negative placement, opacity, and the filtered path share the same blend.
    assert_eq!(
        pixel(
            r##"<filter id="f" color-interpolation-filters="sRGB"><feColorMatrix/></filter><rect width="4" height="4" fill="#804020"/><g transform="translate(-2 -2)" filter="url(#f)" opacity="0.5" style="mix-blend-mode: aqw-add"><rect width="8" height="8" fill="#208040"/></g>"##
        ),
        [144, 128, 64, 255]
    );
}

#[test]
fn authored_color_wrapper_keeps_blend_against_backdrop() {
    let source = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:f="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><g transform="matrix(1 0 0 1 0 0)"><rect width="4" height="4" fill="#800000"/><use f:characterId="2" xlink:href="#effect" style="mix-blend-mode: aqw-add; image-rendering: auto"/></g><defs><g id="effect"><rect width="4" height="4" fill="#004000"/></g></defs></svg>"##;
    let imported = import::import_ffdec_symbol(
        "test",
        source,
        1.0,
        &HashMap::new(),
        "Test",
        &HashMap::from([(
            (1, 2),
            import::AuthoredColorTransform {
                green_mult: 128,
                ..Default::default()
            },
        )]),
        Some(1),
    )
    .unwrap();
    let mut root = svg::Node::elem("svg");
    root.set("width", "4");
    root.set("height", "4");
    for def in imported.definitions {
        root.append(def);
    }
    root.append(imported.definition);
    let data = svg::serialize(&svg::Document {
        root,
        namespaces: imported.namespaces,
    });
    let image = raster::render_svg_resvg(data.as_bytes(), (4, 4)).unwrap();
    assert_eq!(&image.pixels[..4], &[128, 32, 0, 255]);
}
