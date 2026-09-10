//! AQW: preserve backdrop-dependent Add operations across cached components.
use crate::{tiny_skia, usvg};

/// One ordered cached image, with its final compositing operation.
pub struct Layer<'a> {
    /// Apply RGB addition rather than source-over at final composition.
    pub additive: bool,
    nodes: Vec<(&'a usvg::Node, tiny_skia::Transform)>,
}

/// Flatten only non-isolating groups. Filters, masks and group opacity retain
/// their own compositing scope. Consecutive source-over drawing shares a layer.
pub fn plan(tree: &usvg::Tree) -> Vec<Layer<'_>> {
    fn visit<'a>(nodes: &'a [usvg::Node], ts: tiny_skia::Transform, out: &mut Vec<Layer<'a>>) {
        for node in nodes {
            let additive = if let usvg::Node::Group(g) = node {
                if !g.should_isolate() {
                    visit(g.children(), ts.pre_concat(g.transform()), out);
                    continue;
                }
                g.blend_mode() == usvg::BlendMode::AqwAdd
            } else {
                false
            };
            if additive || out.last().is_none_or(|l| l.additive) {
                out.push(Layer {
                    additive,
                    nodes: Vec::new(),
                });
            }
            out.last_mut().unwrap().nodes.push((node, ts));
        }
    }
    let mut out = Vec::new();
    visit(
        tree.root().children(),
        tiny_skia::Transform::identity(),
        &mut out,
    );
    out
}

/// Render one layer onto transparency with its original transforms and effects.
pub fn render(layer: &Layer<'_>, ts: tiny_skia::Transform, pixmap: &mut tiny_skia::PixmapMut) {
    let ctx = crate::render::Context {
        max_bbox: crate::max_filter_bbox(pixmap.width(), pixmap.height()),
    };
    for (node, transform) in &layer.nodes {
        crate::render::render_node(node, &ctx, ts.pre_concat(*transform), pixmap);
    }
}
