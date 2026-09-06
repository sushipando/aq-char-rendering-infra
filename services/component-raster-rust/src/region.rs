//! Safe allocation region for an already prepared resvg tree. The original
//! viewport, stroke calibration, transforms and filter definitions stay intact.
use resvg::{
    tiny_skia::{IntRect, Transform},
    usvg,
};

pub const POLICY: &str = "prepared-tree-region-v1";

fn union(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}

fn extent(group: &usvg::Group) -> Option<[f64; 4]> {
    if group.opacity().get() == 0.0 {
        return None;
    }
    if !group.filters().is_empty() {
        // Include the ENTIRE effect region, even for alpha-generating filters.
        let r = group.abs_layer_bounding_box();
        return Some([
            r.left() as f64,
            r.top() as f64,
            r.right() as f64,
            r.bottom() as f64,
        ]);
    }
    group
        .children()
        .iter()
        .filter_map(|node| match node {
            usvg::Node::Group(g) => extent(g),
            usvg::Node::Text(t) => extent(t.flattened()),
            usvg::Node::Path(p) if !p.is_visible() => None,
            usvg::Node::Image(i) if !i.is_visible() => None,
            _ => {
                let r = node.abs_stroke_bounding_box();
                Some([
                    r.left() as f64,
                    r.top() as f64,
                    r.right() as f64,
                    r.bottom() as f64,
                ])
            }
        })
        .reduce(union)
}

fn max_bbox(size: [u32; 2]) -> Option<IntRect> {
    IntRect::from_xywh(
        i32::try_from(size[0]).ok()?.checked_mul(-2)?,
        i32::try_from(size[1]).ok()?.checked_mul(-2)?,
        size[0].checked_mul(5)?,
        size[1].checked_mul(5)?,
    )
}

// Mirrors the pinned resvg render_group allocation math, including the
// non-filter antialias margin and its canvas-dependent 5x allocation cap.
fn layer(group: &usvg::Group, ts: Transform, cap: IntRect) -> Option<(IntRect, Transform)> {
    let b = group.layer_bounding_box().transform(ts)?;
    let filtered = !group.filters().is_empty();
    let pad = if filtered { 0 } else { 2 };
    let rect = IntRect::from_xywh(
        (b.x().floor() as i32).checked_sub(pad)?,
        (b.y().floor() as i32).checked_sub(pad)?,
        (if filtered {
            b.width().ceil().max(1.0)
        } else {
            b.width().ceil()
        } as u32)
            .checked_add((2 * pad) as u32)?,
        (if filtered {
            b.height().ceil().max(1.0)
        } else {
            b.height().ceil()
        } as u32)
            .checked_add((2 * pad) as u32)?,
    )?;
    let rect = IntRect::from_ltrb(
        rect.left().max(cap.left()),
        rect.top().max(cap.top()),
        rect.right().min(cap.right()),
        rect.bottom().min(cap.bottom()),
    )?;
    // Keep the same floating-point operation order as resvg.
    let dx = b.x() - (b.x() - rect.x() as f32);
    let dy = b.y() - (b.y() - rect.y() as f32);
    Some((rect, Transform::from_translate(-dx, -dy).pre_concat(ts)))
}

fn integer_delta(old: Transform, new: Transform) -> Option<[i32; 2]> {
    if [old.sx, old.sy, old.kx, old.ky] != [new.sx, new.sy, new.kx, new.ky] {
        return None;
    }
    let dx = old.tx as f64 - new.tx as f64;
    let dy = old.ty as f64 - new.ty as f64;
    if !dx.is_finite()
        || !dy.is_finite()
        || dx != dx.round()
        || dy != dy.round()
        || dx.abs() > i32::MAX as f64
        || dy.abs() > i32::MAX as f64
    {
        return None;
    }
    Some([dx as i32, dy as i32])
}

fn equivalent_layers(
    group: &usvg::Group,
    old: Transform,
    new: Transform,
    old_cap: IntRect,
    new_cap: IntRect,
) -> bool {
    group.children().iter().all(|node| match node {
        usvg::Node::Group(g) => {
            if g.mask().is_some() || g.clip_path().is_some() {
                return false;
            }
            // feImage can render a separate tree with viewport-dependent work.
            if g.filters().iter().any(|f| {
                f.primitives()
                    .iter()
                    .any(|p| matches!(p.kind(), usvg::filter::Kind::Image(_)))
            }) {
                return false;
            }
            let old = old.pre_concat(g.transform());
            let new = new.pre_concat(g.transform());
            let Some(delta) = integer_delta(old, new) else {
                return false;
            };
            if !g.should_isolate() {
                return equivalent_layers(g, old, new, old_cap, new_cap);
            }
            let (Some((a, at)), Some((b, bt))) = (layer(g, old, old_cap), layer(g, new, new_cap))
            else {
                return false;
            };
            // Every temporary surface must retain its size and local transform;
            // only its integer placement onto the destination may change.
            a.width() == b.width()
                && a.height() == b.height()
                && a.x() as i64 - b.x() as i64 == delta[0] as i64
                && a.y() as i64 - b.y() as i64 == delta[1] as i64
                && at == bt
                && equivalent_layers(g, at, bt, old_cap, new_cap)
        }
        usvg::Node::Path(p) => {
            let pattern = |paint: &usvg::Paint| matches!(paint, usvg::Paint::Pattern(_));
            !p.fill().is_some_and(|f| pattern(f.paint()))
                && !p.stroke().is_some_and(|s| pattern(s.paint()))
                // Keep path/gradient floating-point sampling identical. An
                // isolated ancestor can absorb the integer destination shift;
                // otherwise use an origin-preserving crop candidate.
                && old == new
        }
        // Retain the original page for other subroots until separately verified.
        usvg::Node::Text(_) | usvg::Node::Image(_) => false,
    })
}

/// Return an integer subrectangle [left, top, right, bottom] of the original
/// page. A thumbnail is a hint, never the sole authority to discard pixels.
pub fn select(tree: &usvg::Tree, expected: [u32; 2], hint: [f64; 4]) -> ([u32; 4], &'static str) {
    let full = [0, 0, expected[0], expected[1]];
    let Some(painted) = extent(tree.root()) else {
        return (full, "unresolved_tree_extent");
    };
    let combined = union(
        painted,
        [hint[0], hint[1], hint[0] + hint[2], hint[1] + hint[3]],
    );
    if !combined.iter().all(|n| n.is_finite()) {
        return (full, "invalid_tree_extent");
    }
    let margin = crate::component_svg::PAGE_MARGIN_PIXELS;
    let region = [
        (combined[0].floor() - margin).clamp(0.0, expected[0] as f64) as u32,
        (combined[1].floor() - margin).clamp(0.0, expected[1] as f64) as u32,
        (combined[2].ceil() + margin).clamp(0.0, expected[0] as f64) as u32,
        (combined[3].ceil() + margin).clamp(0.0, expected[1] as f64) as u32,
    ];
    if region[2] <= region[0] || region[3] <= region[1] {
        return (full, "unresolved_tree_extent");
    }
    // Retaining one/both old origins can still save substantial allocation
    // without introducing different subpixel rounding deep in a scaled tree.
    let mut candidates = [
        region,
        [0, region[1], region[2], region[3]],
        [region[0], 0, region[2], region[3]],
        [0, 0, region[2], region[3]],
    ];
    candidates.sort_by_key(|r| (r[2] - r[0]) as u64 * (r[3] - r[1]) as u64);
    let Some(old_cap) = max_bbox(expected) else {
        return (full, "invalid_layer_cap");
    };
    let mut reason = "no_material_reduction";
    for candidate in candidates {
        let size = [candidate[2] - candidate[0], candidate[3] - candidate[1]];
        if size[0] as u64 * size[1] as u64 * 10 >= expected[0] as u64 * expected[1] as u64 * 9 {
            continue;
        }
        reason = "layer_or_sampling_guard";
        let Some(new_cap) = max_bbox(size) else {
            continue;
        };
        if equivalent_layers(
            tree.root(),
            Transform::identity(),
            Transform::from_translate(-(candidate[0] as f32), -(candidate[1] as f32)),
            old_cap,
            new_cap,
        ) {
            return (
                candidate,
                if candidate == region {
                    "measured_and_prepared_bounds"
                } else {
                    "prepared_bounds_preserved_origin"
                },
            );
        }
    }
    (full, reason)
}
