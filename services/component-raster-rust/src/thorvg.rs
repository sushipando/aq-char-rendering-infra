//! ThorVG (v1.1.1) in-process SVG rasterization — the alternate backend.
//!
//! Every job can choose which SVG rasterizer renders its tight-page component
//! SVGs (`render.raster_backend`, see `raster.rs::RenderBackend`). This module
//! drives ThorVG's C API (`thorvg_sys`, vendored C++ engine built from
//! `vendor/thorvg-sys-upstream` with cc-rs; bindings are frozen at
//! `vendor/thorvg-sys-upstream/bindings.rs` so the Docker build needs no
//! libclang). The flow mirrors upstream's `tools/svg2png` but renders into a
//! caller-owned memory buffer instead of a PNG file:
//!
//!   1. `tvg_engine_init(0)` — engine up, zero worker threads (single-threaded);
//!   2. `tvg_swcanvas_create(TVG_ENGINE_OPTION_DEFAULT)` — software canvas;
//!   3. `tvg_picture_new()` + `tvg_picture_load_data(..., "svg", ...)` — parse;
//!   4. `tvg_picture_set_size(picture, w, h)` — exact page px (preserves the
//!      1:1 meet fit resvg applies through `--width`/`--height`);
//!   5. `tvg_swcanvas_set_target(..., TVG_COLORSPACE_ARGB8888S)` — the buffer
//!      is **straight** (un-premultiplied) alpha;
//!   6. `tvg_canvas_add` / `tvg_canvas_draw(canvas, true)` / `tvg_canvas_sync`.
//!
//! Buffer byte order: ThorVG packs each pixel as the 32-bit word
//! `a<<24 | r<<16 | g<<8 | b` (`_argbJoin` in `tvgSwRaster.cpp`), so
//! little-endian memory holds B,G,R,A. Each word is widened here into the
//! R,G,B,A rows `RgbaImage` expects — and unlike the resvg backend's
//! tiny_skia buffer, no demultiply pass is needed.
//!
//! Reference counting: the C API starts a fresh paint at `refCnt == 0`, so
//! `tvg_canvas_add` is what adopts it (`ref()`), and `tvg_canvas_destroy`
//! releases and **frees** it via `clearPaints()`/`unref()`. `tvg_paint_rel`
//! must be called BEFORE the canvas destroy (no-op for adopted paints, delete
//! for unadopted ones) — see `release()`.

use crate::compositor::RgbaImage;
use crate::error::RasterError;

use thorvg_sys::{Tvg_Canvas, Tvg_Paint};

/// Zero worker threads keeps ThorVG's task scheduler fully synchronous —
/// appropriate for the chunk-of-one worker where each Lambda invocation
/// rasterizes exactly one component.
const ENGINE_THREADS: u32 = 0;

/// ThorVG renders with the `-S` (straight alpha) colorspace so the pixels
/// land unpremultiplied, exactly like the resvg backend's demultiplied
/// output.
const TARGET_COLORSPACE: thorvg_sys::Tvg_Colorspace =
    thorvg_sys::Tvg_Colorspace::TVG_COLORSPACE_ARGB8888S;

/// Release a canvas and its picture in ThorVG's reference-count order.
///
/// In ThorVG 1.1 the C API starts a fresh paint at `refCnt == 0` (see
/// `Paint::Impl` in `tvgPaint.h`). `tvg_canvas_add` adopts the paint via
/// `ref()` (0 -> 1), and `tvg_canvas_destroy` -> `Scene::clearPaints` ->
/// `unref()` **frees it** when the count reaches 0. `Paint::rel` in the C API
/// frees a paint with `refCnt <= 0` (i.e. one never adopted, or already
/// freed by a canvas destroy). Therefore `tvg_paint_rel` MUST be called
/// *before* the canvas is destroyed: for an adopted paint it is a no-op
/// (count 1), for an unadopted one it performs the delete; either way the
/// subsequent `tvg_canvas_destroy` never double-frees.
fn release(canvas: Tvg_Canvas, picture: Tvg_Paint) {
    if !picture.is_null() {
        let _ = unsafe { thorvg_sys::tvg_paint_rel(picture) };
    }
    if !canvas.is_null() {
        let _ = unsafe { thorvg_sys::tvg_canvas_destroy(canvas) };
    }
}

/// Tear everything down and return a `RasterError` describing `step`.
fn fail(canvas: Tvg_Canvas, picture: Tvg_Paint, step: &str) -> RasterError {
    release(canvas, picture);
    let _ = unsafe { thorvg_sys::tvg_engine_term() };
    RasterError::Raster(format!("thorvg {step}"))
}

/// A raw pointer to `text`'s bytes for ThorVG's `const char*` parameters.
///
/// The compiler NUL-terminates static string literals in the binary, so the
/// bytes behind `&str` literals are safe for `strcmp`-style consumers. This
/// is only used for fixed literals like the `"svg"` mimetype; streaming SVG
/// content goes through `tvg_picture_load_data(..., copy = true)` instead.
fn c_str(text: &str) -> *const ::core::ffi::c_char {
    &text.as_bytes()[0] as *const u8 as *const ::core::ffi::c_char
}

/// Rasterize a serialized component SVG at its natural px size.
///
/// `expected` mirrors `raster.rs::render_svg`: the page is built with integer
/// px width/height matching the job's shared pixel scale, so ThorVG's natural
/// size must equal it within a float epsilon (the SVG root width/height are
/// authored as whole pixels). A mismatch means the shared canvas and the SVG
/// disagree on scale and the downstream placement would be wrong.
pub fn render_svg(svg_bytes: &[u8], expected: (u32, u32)) -> Result<RgbaImage, RasterError> {
    let engine_result = unsafe { thorvg_sys::tvg_engine_init(ENGINE_THREADS) };
    if engine_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(RasterError::Raster("thorvg engine init failed".to_string()));
    }

    let canvas = unsafe {
        thorvg_sys::tvg_swcanvas_create(thorvg_sys::Tvg_Engine_Option::TVG_ENGINE_OPTION_DEFAULT)
    };
    if canvas.is_null() {
        let _ = unsafe { thorvg_sys::tvg_engine_term() };
        return Err(RasterError::Raster(
            "thorvg canvas creation failed".to_string(),
        ));
    }
    let picture = unsafe { thorvg_sys::tvg_picture_new() };
    if picture.is_null() {
        return Err(fail(canvas, picture, "picture creation failed"));
    }

    // ThorVG operates on a `const char*`; the component SVG is UTF-8 text.
    // `copy = true` makes ThorVG own a private byte-for-byte copy, so the
    // caller's buffer lifetime is irrelevant and the XML parser is safe to
    // read past the exact size (C-string semantics).
    let svg_bytes_len = svg_bytes.len();
    if svg_bytes_len == 0 || svg_bytes_len > u32::MAX as usize {
        return Err(fail(canvas, picture, "empty component svg"));
    }
    let data_ptr: *const ::core::ffi::c_char =
        &svg_bytes[0] as *const u8 as *const ::core::ffi::c_char;
    let loaded = unsafe {
        thorvg_sys::tvg_picture_load_data(
            picture,
            data_ptr,
            svg_bytes_len as u32,
            c_str("svg"),
            ::core::ptr::null(),
            true,
        )
    };
    if loaded != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "svg load failed"));
    }

    // ThorVG reports the SVG's natural size; the tight page is authored in
    // whole px so a mismatch of more than half a pixel means the two engines
    // disagree about the page scale.
    let mut natural_w = 0.0f32;
    let mut natural_h = 0.0f32;
    let size_result =
        unsafe { thorvg_sys::tvg_picture_get_size(picture, &mut natural_w, &mut natural_h) };
    if size_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "picture size query failed"));
    }
    if (natural_w - expected.0 as f32).abs() > 0.5 || (natural_h - expected.1 as f32).abs() > 0.5 {
        release(canvas, picture);
        let _ = unsafe { thorvg_sys::tvg_engine_term() };
        return Err(RasterError::Raster(format!(
            "thorvg component rendered {natural_w:.1}x{natural_h:.1}, expected {}x{} \
             (shared pixel scale mismatch)",
            expected.0, expected.1
        )));
    }

    let width = expected.0;
    let height = expected.1;
    // Pin the display size to the exact integer page (ThorVG fits the
    // viewBox with preserveAspectRatio meet + center, matching resvg).
    let size_result =
        unsafe { thorvg_sys::tvg_picture_set_size(picture, width as f32, height as f32) };
    if size_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "picture size assignment failed"));
    }
    let mut buffer = vec![0u32; (width as usize * height as usize).max(1)];
    let target_result = unsafe {
        thorvg_sys::tvg_swcanvas_set_target(
            canvas,
            buffer.as_mut_ptr(),
            width,
            width,
            height,
            TARGET_COLORSPACE,
        )
    };
    if target_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "canvas target failed"));
    }
    let add_result = unsafe { thorvg_sys::tvg_canvas_add(canvas, picture) };
    if add_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "canvas add failed"));
    }
    let draw_result = unsafe { thorvg_sys::tvg_canvas_draw(canvas, true) };
    if draw_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "canvas draw failed"));
    }
    let sync_result = unsafe { thorvg_sys::tvg_canvas_sync(canvas) };
    if sync_result != thorvg_sys::Tvg_Result::TVG_RESULT_SUCCESS {
        return Err(fail(canvas, picture, "canvas sync failed"));
    }

    // Widening pass: AARRGGBB word (little-endian memory B,G,R,A) -> RGBA.
    let mut pixels = Vec::with_capacity((width as usize * height as usize * 4).max(4));
    for word in &buffer {
        let value = *word;
        pixels.extend_from_slice(&[
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
            (value >> 24) as u8,
        ]);
    }

    release(canvas, picture);
    let _ = unsafe { thorvg_sys::tvg_engine_term() };
    Ok(RgbaImage::new(width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6">
  <rect x="1" y="1" width="4" height="3" fill="#ff0000"/>
  <circle cx="6" cy="4" r="1.5" fill="#00ff00" opacity="0.5"/>
</svg>"##;

    #[test]
    fn engine_round_trip() {
        // The ThorVG engine is process-global (its init/term refcount is not
        // thread-safe), so these checks MUST run in one serialized test:
        // production is one SVG per process, but the parallel test harness
        // would otherwise race init/term between threads. See
        // tvgInitializer.cpp::engineInit.

        // 1. Renders at the exact page size with straight alpha.
        let image = render_svg(SAMPLE.as_bytes(), (8, 6)).expect("thorvg must render the sample");
        assert_eq!((image.width, image.height), (8, 6));
        // Pixel (1,1) is inside the opaque red rect: row 1 * stride 8 + col 1.
        #[allow(clippy::identity_op)] // keep the (row * stride + col) form legible
        let opaque = (1 * 8 + 1) * 4;
        assert_eq!(image.pixels[opaque], 255);
        assert_eq!(image.pixels[opaque + 1], 0);
        assert_eq!(image.pixels[opaque + 2], 0);
        assert_eq!(image.pixels[opaque + 3], 255);
        // The translucent green circle at (6,4) must have alpha in (0, 255).
        let semi = (4 * 8 + 6) * 4;
        let alpha = image.pixels[semi + 3];
        assert!(
            alpha > 0 && alpha < 255,
            "translucent fill must be straight alpha, got {alpha}"
        );
        // Fully outside both shapes stays transparent (draw clear=true).
        assert_eq!(image.pixels[3 + 3 * 4], 0);
        assert!(image.alpha_bbox(8, 6).is_some());

        // 2. Rejects a page-size mismatch (the mismatch path must tear down
        //    cleanly so the engine survives for the next render).
        let mismatch = render_svg(SAMPLE.as_bytes(), (16, 12));
        assert!(mismatch.is_err(), "thorvg must reject a page-size mismatch");

        // 3. Rejects unparseable input, then keeps working afterwards.
        let garbage = render_svg("not an svg at all".as_bytes(), (8, 6));
        assert!(garbage.is_err(), "thorvg must reject unparseable input");
        let again =
            render_svg(SAMPLE.as_bytes(), (8, 6)).expect("thorvg must recover after errors");
        assert_eq!((again.width, again.height), (8, 6));
    }
}
