//! Offline static-card preview. No AWS calls, deployment, or live rendering.
use anyhow::{Context, Result};
use resvg::tiny_skia;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let fields: serde_json::Value = serde_json::from_slice(&std::fs::read(
        args.get(1)
            .context("usage: charpage_preview FIELDS.json OUTPUT_DIR [WIDTH]")?,
    )?)?;
    let out = std::path::Path::new(args.get(2).context("missing output directory")?);
    std::fs::create_dir_all(out)?;
    let width = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(1024);
    let (background, foreground) =
        aqw_render_pipeline::charpage::render(&fields, &serde_json::json!({}), width)?;
    std::fs::write(out.join("background.png"), &background)?;
    std::fs::write(out.join("foreground.png"), &foreground)?;
    let mut preview = tiny_skia::Pixmap::decode_png(&background)?;
    let overlay = tiny_skia::Pixmap::decode_png(&foreground)?;
    preview.draw_pixmap(
        0,
        0,
        overlay.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    preview.save_png(out.join("static-card.png"))?;
    Ok(())
}
