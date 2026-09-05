//! resvg or ThorVG in-process rasterization, alpha cropping, output-grid
//! downsampling via fast_image_resize, and PNG encoding.

use fast_image_resize as fir;

use crate::compositor::RgbaImage;
use crate::error::RasterError;

/// The SVG rasterizer that renders the tight-page component SVGs.
///
/// `resvg` is the pinned 0.48.1 upstream (with the repo's libblur SIMD blur
/// patch vendored under `vendor/resvg-upstream`); `thorvg` is the pinned
/// 1.1.1 upstream built through `vendor/thorvg-sys-upstream`. Both render the
/// same assembled SVG; the outputs are intentionally not pixel-identical, and
/// the either/or is chosen per render job via `render.raster_backend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderBackend {
    Resvg,
    Thorvg,
}

pub const DEFAULT_RENDER_BACKEND: RenderBackend = RenderBackend::Resvg;

impl RenderBackend {
    /// Parse a manifest `raster_backend` value; unknown values are rejected
    /// so a typo can never silently fall back to the wrong engine.
    pub fn parse(value: &str) -> Option<RenderBackend> {
        match value {
            "resvg" => Some(RenderBackend::Resvg),
            "thorvg" => Some(RenderBackend::Thorvg),
            _ => None,
        }
    }

    pub fn to_str(&self) -> &'static str {
        match self {
            RenderBackend::Resvg => "resvg",
            RenderBackend::Thorvg => "thorvg",
        }
    }
}

/// Rasterize a serialized component SVG at its natural px size with the
/// selected backend. The page is built with integer px width/height matching
/// the job's shared pixel scale, so both engines see the same expected size.
pub fn render_svg(
    svg_bytes: &[u8],
    expected: (u32, u32),
    backend: RenderBackend,
) -> Result<RgbaImage, RasterError> {
    match backend {
        RenderBackend::Resvg => render_svg_resvg(svg_bytes, expected),
        RenderBackend::Thorvg => crate::thorvg::render_svg(svg_bytes, expected),
    }
}

/// The resvg path: usvg parse -> tiny_skia premultiplied pixmap -> exact
/// CLI demultiply (kept bit-identical to the shared image's `/opt/resvg`).
pub fn render_svg_resvg(svg_bytes: &[u8], expected: (u32, u32)) -> Result<RgbaImage, RasterError> {
    let mut options = resvg::usvg::Options {
        resources_dir: None,
        ..resvg::usvg::Options::default()
    };
    options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(svg_bytes, &options)
        .map_err(|error| RasterError::Raster(format!("usvg parse failed: {error}")))?;
    let size = tree.size().to_int_size();
    if (size.width(), size.height()) != expected {
        return Err(RasterError::Raster(format!(
            "component rendered {}x{}, expected {}x{} (shared pixel scale mismatch)",
            size.width(),
            size.height(),
            expected.0,
            expected.1
        )));
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| RasterError::Raster("cannot allocate pixmap".to_string()))?;
    resvg::render(
        &tree,
        resvg::usvg::Transform::default(),
        &mut pixmap.as_mut(),
    );
    let mut pixels = pixmap.data().to_vec();
    // tiny_skia Pixmap stores premultiplied RGBA. The resvg CLI demultiplies
    // through `PremultipliedColorU8::demultiply` before writing PNG; replicate
    // that exact rounding so straight-alpha parity holds for every pixel.
    demultiply_u8(&mut pixels);
    Ok(RgbaImage::new(size.width(), size.height(), pixels))
}

fn demultiply_u8(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = pixel[3];
        if alpha == 255 {
            continue;
        }
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }
        let scale = 255.0 / alpha as f64;
        pixel[0] = (pixel[0] as f64 * scale + 0.5) as u8;
        pixel[1] = (pixel[1] as f64 * scale + 0.5) as u8;
        pixel[2] = (pixel[2] as f64 * scale + 0.5) as u8;
    }
}

pub const DOWNSAMPLE_HALO_RASTER_PIXELS: f64 = 12.0;

/// The active downsample backend. `fast_image_resize` (FIR) is the default;
/// `exact` selects the Pillow-verbatim `resample.rs` port used during parity
/// validation and as a fallback.
fn use_fast_image_resize() -> bool {
    // FIR is the default; `AQW_DOWNSAMPLER=exact` selects the Pillow-verbatim
    // port used for parity validation and rollback.
    !matches!(std::env::var("AQW_DOWNSAMPLER").as_deref(), Ok("exact"))
}

/// The active downsampler for output-grid component rasterization.
///
/// Default: fast_image_resize (FIR) — separable Lanczos3 with SIMD, ~3x
/// faster than the exact port, with a measured <=9/255 premultiplied diff on
/// real AQW content (0.0014% of pixels > 5/255) at the 2x component shrink.
/// Set `AQW_DOWNSAMPLER=exact` to force the Pillow-verbatim resampler
/// (bit-identical to the Python worker, used by the parity harness).
pub fn downsample_component_to_output_grid(
    image: &RgbaImage,
    x: i64,
    y: i64,
    raster_canvas: (i64, i64),
    output_canvas: (i64, i64),
) -> Option<(RgbaImage, i64, i64)> {
    downsample_component_impl(
        image,
        x,
        y,
        raster_canvas,
        output_canvas,
        use_fast_image_resize(),
    )
}

/// Force the fast_image_resize path (debug/benchmark comparisons only).
pub fn downsample_component_fir(
    image: &RgbaImage,
    x: i64,
    y: i64,
    raster_canvas: (i64, i64),
    output_canvas: (i64, i64),
) -> Option<(RgbaImage, i64, i64)> {
    downsample_component_impl(image, x, y, raster_canvas, output_canvas, true)
}

fn downsample_component_impl(
    image: &RgbaImage,
    x: i64,
    y: i64,
    raster_canvas: (i64, i64),
    output_canvas: (i64, i64),
    use_fir: bool,
) -> Option<(RgbaImage, i64, i64)> {
    let (source_width, source_height) = raster_canvas;
    let (target_width, target_height) = output_canvas;
    if source_width
        .min(source_height)
        .min(target_width)
        .min(target_height)
        <= 0
    {
        return None;
    }
    if target_width > source_width || target_height > source_height {
        return None;
    }

    let visible_left = 0.max(x);
    let visible_top = 0.max(y);
    let visible_right = source_width.min(x + image.width as i64);
    let visible_bottom = source_height.min(y + image.height as i64);
    if visible_left >= visible_right || visible_top >= visible_bottom {
        return None;
    }

    let halo = DOWNSAMPLE_HALO_RASTER_PIXELS;
    let output_left = 0.max(
        ((visible_left as f64 - halo) * target_width as f64 / source_width as f64).floor() as i64,
    );
    let output_top = 0.max(
        ((visible_top as f64 - halo) * target_height as f64 / source_height as f64).floor() as i64,
    );
    let output_right = target_width.min(
        ((visible_right as f64 + halo) * target_width as f64 / source_width as f64).ceil() as i64,
    );
    let output_bottom = target_height.min(
        ((visible_bottom as f64 + halo) * target_height as f64 / source_height as f64).ceil()
            as i64,
    );
    if output_left >= output_right || output_top >= output_bottom {
        return None;
    }

    let box_left = output_left as f64 * source_width as f64 / target_width as f64;
    let box_top = output_top as f64 * source_height as f64 / target_height as f64;
    let box_right = output_right as f64 * source_width as f64 / target_width as f64;
    let box_bottom = output_bottom as f64 * source_height as f64 / target_height as f64;
    let patch_left = 0.max(box_left.floor() as i64);
    let patch_top = 0.max(box_top.floor() as i64);
    let patch_right = source_width.min(box_right.ceil() as i64);
    let patch_bottom = source_height.min(box_bottom.ceil() as i64);

    // Place the layer into a transparent patch at its full-canvas offset.
    let mut patch = RgbaImage::new(
        (patch_right - patch_left) as u32,
        (patch_bottom - patch_top) as u32,
        vec![0u8; ((patch_right - patch_left) * (patch_bottom - patch_top) * 4) as usize],
    );
    patch.composite(image, x - patch_left, y - patch_top);

    // Pillow-exact resampling: premultiply -> fixed-point Lanczos two-pass ->
    // unpremultiply, matching `patch.convert("RGBa").resize(..., LANCZOS,
    // box=...).convert("RGBA")`. fast_image_resize remains available behind
    // AQW_DOWNSAMPLER=fast_image_resize for benchmarks.
    let resized = if use_fir {
        let source = fir::images::Image::from_vec_u8(
            patch.width,
            patch.height,
            patch.pixels,
            fir::PixelType::U8x4,
        )
        .expect("patch is aligned");
        let mut destination = fir::images::Image::new(
            (output_right - output_left) as u32,
            (output_bottom - output_top) as u32,
            source.pixel_type(),
        );
        let options = fir::ResizeOptions::new()
            .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3))
            .crop(
                box_left - patch_left as f64,
                box_top - patch_top as f64,
                box_right - box_left,
                box_bottom - box_top,
            )
            .use_alpha(true);
        fir::Resizer::new()
            .resize(&source, &mut destination, &options)
            .map_err(|error| RasterError::Raster(format!("downsample: {error}")))
            .ok()?;
        RgbaImage::new(
            destination.width(),
            destination.height(),
            destination.buffer().to_vec(),
        )
    } else {
        let out_w = (output_right - output_left) as usize;
        let out_h = (output_bottom - output_top) as usize;
        crate::resample::resize_lanczos(
            &patch,
            out_w,
            out_h,
            box_left - patch_left as f64,
            box_top - patch_top as f64,
            box_right - patch_left as f64,
            box_bottom - patch_top as f64,
        )
        .ok()?
    };
    let bbox = resized.alpha_bbox(resized.width, resized.height)?;
    let (crop_left, crop_top, _, _) = bbox;
    let cropped = resized.crop(bbox);
    Some((
        cropped,
        output_left + crop_left as i64,
        output_top + crop_top as i64,
    ))
}

/// Encode straight-alpha RGBA8 as an 8-bit PNG.
pub fn encode_rgba8(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, RasterError> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| RasterError::Png(error.to_string()))?;
        writer
            .write_image_data(pixels)
            .map_err(|error| RasterError::Png(error.to_string()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_geometry_matches_python_intent() {
        // A simple fully-visible 100x100 layer on a 200x200 raster -> 100x100
        // output: output rect should cover the whole canvas (halo clamps).
        let image = RgbaImage::new(100, 100, vec![255u8; 100 * 100 * 4]);
        let scaled = downsample_component_to_output_grid(&image, 50, 50, (200, 200), (100, 100))
            .expect("fully visible layer must downsample");
        // The layer is placed mid-canvas; bbox after resize is non-empty.
        assert!(scaled.0.width > 0 && scaled.0.height > 0);
        assert!(scaled.1 >= 0 && scaled.2 >= 0);
    }

    #[test]
    fn fully_transparent_layer_is_skipped_by_downsample() {
        let image = RgbaImage::new(10, 10, vec![0u8; 10 * 10 * 4]);
        let scaled = downsample_component_to_output_grid(&image, 0, 0, (200, 200), (100, 100));
        assert!(scaled.is_none());
    }

    #[test]
    fn fir_stays_within_bounded_tolerance_of_exact() {
        // The two downsamplers share geometry and stay close on real content:
        // a translucent gradient patch downsampled 2x must keep the same
        // bbox and stay within a bounded premultiplied per-channel diff.
        let w = 240u32;
        let h = 180u32;
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let fx = x as f64 / w as f64;
                let fy = y as f64 / h as f64;
                px.extend_from_slice(&[
                    (80.0 + 150.0 * fx) as u8,
                    (60.0 + 120.0 * fy) as u8,
                    (200.0 - 100.0 * fy) as u8,
                    (200.0 + 55.0 * fx) as u8,
                ]);
            }
        }
        let image = RgbaImage::new(w, h, px);
        let exact = downsample_component_impl(&image, 40, 30, (4096, 3253), (2048, 1626), false)
            .expect("exact path produces output");
        let fir = downsample_component_impl(&image, 40, 30, (4096, 3253), (2048, 1626), true)
            .expect("FIR path produces output");
        assert_eq!((fir.0.width, fir.0.height), (exact.0.width, exact.0.height));
        // Bounded premultiplied-on-gray max diff (measured <=9/255 on real
        // Soltina cape; envelope 24 keeps CI robust across content). Compare
        // premultiplied-on-gray, not straight alpha: FIR and Pillow round
        // premultiply differently and straight-alpha blows up at zero-alpha
        // pixels.
        fn premult_gray(img: &RgbaImage, gray: u8) -> Vec<u8> {
            let mut out = Vec::with_capacity(img.pixels.len());
            let mut gy = gray as f32 / 255.0;
            let _ = &mut gy;
            for px in img.pixels.as_chunks::<4>().0 {
                let a = px[3] as f32 / 255.0;
                // composite straight color over gray
                let c = |src: u8| (src as f32 / 255.0) * a + gray as f32 / 255.0 * (1.0 - a);
                let (r, g, b) = (c(px[0]), c(px[1]), c(px[2]));
                // premultiplied rgb
                out.extend_from_slice(&[
                    (r * 255.0).round() as u8,
                    (g * 255.0).round() as u8,
                    (b * 255.0).round() as u8,
                    px[3],
                ]);
            }
            out
        }
        let mut max_diff = 0i32;
        let min_h = fir.0.height.min(exact.0.height) as usize;
        let min_w = fir.0.width.min(exact.0.width) as usize;
        let a = premult_gray(&fir.0, 96);
        let b = premult_gray(&exact.0, 96);
        for y in 0..min_h {
            for x in 0..min_w {
                let ia = (y * fir.0.width as usize + x) * 4;
                let ib = (y * exact.0.width as usize + x) * 4;
                for c in 0..4 {
                    let d = (a[ia + c] as i32 - b[ib + c] as i32).abs();
                    max_diff = max_diff.max(d);
                }
            }
        }
        assert!(max_diff <= 24, "FIR diverged from exact by {max_diff}/255");
        // And FIR itself is the default backend.
        assert!(use_fast_image_resize());
    }

    #[test]
    fn png_round_trip() {
        let pixels: Vec<u8> = (0..8 * 8 * 4)
            .map(|index| (index * 3 % 256) as u8)
            .collect();
        let encoded = encode_rgba8(8, 8, &pixels).unwrap();
        assert!(encoded.len() > 16);
        let decoded = crate::png::decode_rgba8(&encoded).unwrap();
        assert_eq!(decoded.pixels, pixels);
    }
}
