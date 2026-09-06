use aqw_component_raster::{
    raster::{render_svg_bounded, render_svg_resvg, RenderBackend},
    region,
};

#[test]
fn cropped_allocations_preserve_pixels_and_cover_missed_marks() {
    let cases = [
        r#"<rect x="400" y="300" width="80" height="100" fill="red"/>"#,
        r#"<g transform="matrix(.97 .23 -.23 .97 401.25 302.5)"><path d="M0 0 Q35 10 64 80 L2 71 Z" fill="red" stroke="black" stroke-width="1.3"/></g>"#,
        r#"<g transform="translate(600.25 400.5) scale(-1 1)"><circle cx="30" cy="40" r="30" fill="blue"/></g>"#,
        r#"<rect x="400" y="300" width="60" height="40"/><rect x="800" y="700" width=".7" height=".9" opacity=".04"/>"#,
        r##"<defs><filter id="f" x="-100%" y="-100%" width="300%" height="300%"><feGaussianBlur stdDeviation="5"/></filter></defs><rect x="400" y="300" width="80" height="100" filter="url(#f)"/>"##,
        r##"<defs><filter id="f"><feColorMatrix values="0 0 0 0 1 0 0 0 0 .2 0 0 0 0 .7 0 0 0 1 0"/></filter></defs><g opacity=".75"><g filter="url(#f)"><rect x="400" y="300" width="80" height="100"/></g></g>"##,
        r##"<defs><filter id="f" x="-100%" y="-100%" width="300%" height="300%"><feFlood flood-color="red"/></filter></defs><rect x="400" y="300" width="80" height="100" opacity=".1" filter="url(#f)"/>"##,
        r##"<defs><linearGradient id="g"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient></defs><rect x="400" y="300" width="80" height="100" fill="url(#g)"/>"##,
    ];
    let mut cropped = 0;
    for body in cases {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024">{body}</svg>"#
        );
        let original = render_svg_resvg(svg.as_bytes(), (1024, 1024)).unwrap();
        // An intentionally incomplete hint must not lose the remote faint
        // mark or the full prepared filter/alpha extent.
        let (small, offset, reason) = render_svg_bounded(
            svg.as_bytes(),
            (1024, 1024),
            RenderBackend::Resvg,
            Some([420.0, 320.0, 1.0, 1.0]),
        )
        .unwrap();
        let mut restored = vec![0; original.pixels.len()];
        for y in 0..small.height as usize {
            let dst = ((y + offset[1] as usize) * 1024 + offset[0] as usize) * 4;
            restored[dst..dst + small.width as usize * 4].copy_from_slice(
                &small.pixels[y * small.width as usize * 4..(y + 1) * small.width as usize * 4],
            );
        }
        let changed = original
            .pixels
            .iter()
            .zip(&restored)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 0, "{reason}: {body}");
        cropped += usize::from(small.width * small.height < 1024 * 1024);
    }
    assert!(
        cropped >= 5,
        "expected meaningful optimization, got {cropped} cropped cases"
    );
}

#[test]
fn unsupported_subroots_keep_original_page() {
    for body in [
        r##"<defs><clipPath id="c"><rect x="400" y="300" width="80" height="100"/></clipPath></defs><rect x="400" y="300" width="80" height="100" clip-path="url(#c)"/>"##,
        r##"<defs><mask id="m"><rect width="1024" height="1024" fill="white"/></mask></defs><rect x="400" y="300" width="80" height="100" mask="url(#m)"/>"##,
        r##"<defs><pattern id="p" width="10" height="10" patternUnits="userSpaceOnUse"><rect width="5" height="5"/></pattern></defs><rect x="400" y="300" width="80" height="100" fill="url(#p)"/>"##,
    ] {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024">{body}</svg>"#
        );
        let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).unwrap();
        assert_eq!(
            region::select(&tree, [1024, 1024], [400.0, 300.0, 80.0, 100.0]),
            ([0, 0, 1024, 1024], "layer_or_sampling_guard")
        );
    }
}

#[test]
fn nested_filter_allocation_cap_changes_reject_the_crop() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024"><defs>
        <filter id="outer" filterUnits="userSpaceOnUse" x="400" y="300" width="80" height="100"><feColorMatrix/></filter>
        <filter id="inner" filterUnits="userSpaceOnUse" x="-5000" y="-5000" width="10000" height="10000"><feGaussianBlur stdDeviation="3"/></filter>
        </defs><g filter="url(#outer)"><rect x="400" y="300" width="80" height="100" filter="url(#inner)"/></g></svg>"##;
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).unwrap();
    assert_eq!(region::select(&tree, [1024,1024], [400.0,300.0,80.0,100.0]), ([0,0,1024,1024], "layer_or_sampling_guard"));
}
