use super::*;

fn canvas(width: u32) -> tiny_skia::Pixmap {
    tiny_skia::Pixmap::new(width, (width as f64 * 350.0 / 550.0).round() as u32).unwrap()
}

fn scale(canvas: &tiny_skia::Pixmap) -> f32 {
    (canvas.width() as f32 / 550.0).min(canvas.height() as f32 / 350.0)
}

fn original(bytes: &[u8], options: &usvg::Options, width: u32) -> tiny_skia::Pixmap {
    let mut image = canvas(width);
    let tree = usvg::Tree::from_data(bytes, options).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale(&image), scale(&image)),
        &mut image.as_mut(),
    );
    image
}

// Summing alpha across a horizontal line's cross-section measures its width
// independently of whether its center happens to lie on a pixel boundary.
fn coverage(image: &tiny_skia::Pixmap, stage_y: f32) -> f32 {
    let s = scale(image);
    let x = (40.0 * s) as usize;
    let top = ((stage_y - 10.0) * s).floor() as usize;
    let bottom = ((stage_y + 10.0) * s).ceil() as usize;
    (top..bottom)
        .map(|y| image.pixels()[y * image.width() as usize + x].alpha() as f32 / 255.0)
        .sum()
}

#[test]
fn minimum_strokes_use_output_pixels_and_preserve_authored_widths() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"
        xmlns:ffdec="https://www.free-decompiler.com/flash" width="550" height="350">
      <defs>
        <g id="hairline"><path d="M0 0H100" fill="none" stroke="black" stroke-width="2"
          ffdec:has-small-stroke="true" ffdec:original-stroke-width="0.05"/></g>
        <g id="authored"><path d="M0 0H100" fill="none" stroke="black" stroke-width="2"
          ffdec:has-small-stroke="true" ffdec:original-stroke-width="1.5"/></g>
        <g id="ordinary"><path d="M0 0H100" fill="none" stroke="black" stroke-width="4"/></g>
      </defs>
      <use href="#hairline" transform="matrix(0.5 0 0 0.5 20 40.5)"/>
      <use href="#authored" transform="matrix(0.5 0 0 0.5 20 100.5)"/>
      <use href="#ordinary" transform="matrix(0.5 0 0 0.5 20 160.5)"/>
      <rect x="20" y="220.5" width="50" height="1" fill="black"/>
    </svg>"##;
    for width in [275, 550, 1100, 2048, 4096] {
        let mut image = canvas(width);
        draw(svg, &usvg::Options::default(), &mut image, 0.0, 0.0).unwrap();
        let s = scale(&image);
        for (y, expected) in [(40.5, 1.0), (100.5, (0.75 * s).max(1.0)), (160.5, 2.0 * s)] {
            let actual = coverage(&image, y);
            // Wide stroke coverage is quantized by the rasterizer's AA scan
            // conversion. Keep the one-pixel hairline check tighter.
            let tolerance = if expected <= 1.0 { 0.08 } else { 0.26 };
            assert!(
                (actual - expected).abs() < tolerance,
                "output {width}, row {y}: width {actual}, expected {expected}"
            );
        }
        // Ordinary strokes and filled geometry remain pixel-identical,
        // including the rasterizer's own fractional-pixel coverage rounding.
        let before = original(svg, &usvg::Options::default(), width);
        for (top, bottom) in [(150.0, 170.0), (210.0, 230.0)] {
            let start = (top * s).floor() as usize * width as usize * 4;
            let end = (bottom * s).ceil() as usize * width as usize * 4;
            assert!(before.data()[start..end] == image.data()[start..end]);
        }
    }
}

#[test]
fn bundled_icons_change_only_when_their_minimum_strokes_need_scaling() {
    let assets: &[(&str, &[u8], bool)] = &[
        (
            "equipment",
            include_bytes!("../../assets/charpage/chrome.svgz"),
            true,
        ),
        (
            "good",
            include_bytes!("../../assets/charpage/faction-good.svgz"),
            true,
        ),
        (
            "chaos",
            include_bytes!("../../assets/charpage/faction-chaos.svgz"),
            true,
        ),
        (
            "evil",
            include_bytes!("../../assets/charpage/faction-evil.svgz"),
            false,
        ),
        (
            "neutral",
            include_bytes!("../../assets/charpage/faction-neutral.svgz"),
            true,
        ),
        (
            "guild",
            include_bytes!("../../assets/charpage/guild.svgz"),
            false,
        ),
    ];
    let options = usvg::Options::default();
    let output = std::env::var_os("AQW_TEST_CHARPAGE_STROKE_OUTPUT").map(std::path::PathBuf::from);
    if let Some(path) = &output {
        std::fs::create_dir_all(path).unwrap();
    }
    for &(name, bytes, affected) in assets {
        for width in [550, 1100, 2048] {
            let before = original(bytes, &options, width);
            let mut after = canvas(width);
            draw(bytes, &options, &mut after, 0.0, 0.0).unwrap();
            if !affected || width == 550 {
                assert!(before.data() == after.data(), "{name} at {width} changed");
            } else {
                let changed = before
                    .pixels()
                    .iter()
                    .zip(after.pixels())
                    .filter(|(a, b)| a != b)
                    .count();
                assert!(
                    changed > 20,
                    "{name} at {width}: only {changed} pixels changed"
                );
                let alpha = |image: &tiny_skia::Pixmap| -> u64 {
                    image.pixels().iter().map(|p| p.alpha() as u64).sum()
                };
                assert!(
                    alpha(&after) < alpha(&before),
                    "{name} at {width} didn't get thinner"
                );
            }
            if let Some(path) = &output {
                let s = scale(&after);
                let rect = if name == "equipment" {
                    tiny_skia::IntRect::from_xywh(
                        (8.0 * s) as i32,
                        (125.0 * s) as i32,
                        (40.0 * s).ceil() as u32,
                        (150.0 * s).ceil() as u32,
                    )
                    .unwrap()
                } else if name == "guild" {
                    tiny_skia::IntRect::from_xywh(
                        (10.0 * s) as i32,
                        (95.0 * s) as i32,
                        (40.0 * s).ceil() as u32,
                        (40.0 * s).ceil() as u32,
                    )
                    .unwrap()
                } else {
                    tiny_skia::IntRect::from_xywh(
                        0,
                        0,
                        (33.0 * s).ceil() as u32,
                        (38.0 * s).ceil() as u32,
                    )
                    .unwrap()
                };
                before
                    .clone_rect(rect)
                    .unwrap()
                    .save_png(path.join(format!("{name}-{width}-before.png")))
                    .unwrap();
                after
                    .clone_rect(rect)
                    .unwrap()
                    .save_png(path.join(format!("{name}-{width}-after.png")))
                    .unwrap();
            }
        }
    }
}
