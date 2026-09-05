//! Local SVG benchmark/ablation harness for isolating resvg bottlenecks.
//!
//! Loads one SVG (usually the *built component SVG* our worker rasterizes),
//! applies optional ablations to the source text, renders with the same
//! in-process usvg/resvg path the worker uses, and reports parse vs render
//! timings. Run several ablations over the same file to find which feature
//! dominates (see docs/resvg_bottleneck_ablation_plan.md).

use std::path::PathBuf;
use std::time::Instant;

use crate::error::RasterError;
use crate::raster::render_svg;

pub struct BenchOptions {
    pub svg: PathBuf,
    /// Override output size. If None, the SVG intrinsic px size is used.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Remove `mix-blend-mode` style declarations (plan option B).
    pub no_blend: bool,
    /// Remove `clip-path` attributes (plan option C).
    pub no_clip: bool,
    /// Strip `stroke`/`stroke-width` attributes (plan option F).
    pub no_stroke: bool,
}

fn usage() -> &'static str {
    "\
usage: aqw-component-raster bench-svg FILE [--width N] [--height N] \
[--no-blend] [--no-clip] [--no-stroke]"
}

pub fn parse_cli(args: &[String]) -> Result<BenchOptions, RasterError> {
    if args.is_empty() || args[0] != "bench-svg" {
        return Err(RasterError::invalid(usage().to_string()));
    }
    let mut svg: Option<PathBuf> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut no_blend = false;
    let mut no_clip = false;
    let mut no_stroke = false;

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--width" | "--height" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| {
                        RasterError::invalid(format!("{flag} requires a value\n{}", usage()))
                    })
                    .and_then(|v| {
                        v.parse::<u32>()
                            .map_err(|_| RasterError::invalid(format!("{flag} must be an integer")))
                    })?;
                if flag == "--width" {
                    width = Some(value);
                } else {
                    height = Some(value);
                }
                index += 2;
            }
            "--no-blend" => {
                no_blend = true;
                index += 1;
            }
            "--no-clip" => {
                no_clip = true;
                index += 1;
            }
            "--no-stroke" => {
                no_stroke = true;
                index += 1;
            }
            "--help" | "-h" => return Err(RasterError::invalid(usage().to_string())),
            _ if flag.starts_with('-') => {
                return Err(RasterError::invalid(format!(
                    "unknown argument {flag}\n{}",
                    usage()
                )));
            }
            _ => {
                if svg.is_some() {
                    return Err(RasterError::invalid(format!(
                        "unexpected extra argument {flag}\n{}",
                        usage()
                    )));
                }
                svg = Some(PathBuf::from(flag));
                index += 1;
            }
        }
    }

    for arg in args {
        match arg.as_str() {
            "--no-blend" => no_blend = true,
            "--no-clip" => no_clip = true,
            "--no-stroke" => no_stroke = true,
            _ => {}
        }
    }

    let svg = svg.ok_or_else(|| RasterError::invalid(format!("missing SVG path\n{}", usage())))?;
    Ok(BenchOptions {
        svg,
        width,
        height,
        no_blend,
        no_clip,
        no_stroke,
    })
}

/// Remove all `attr="..."` (or `attr='...'`) from the text. The match must
/// be preceded by whitespace (attribute separator) so we never touch the
/// middle of another token.
fn strip_attr(text: &str, attr: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let needle = format!(" {attr}=\"");
    loop {
        match rest.find(&needle) {
            None => {
                out.push_str(rest);
                break;
            }
            Some(pos) => {
                out.push_str(&rest[..pos]);
                let after = &rest[pos + needle.len()..];
                let close = after.find('"').unwrap_or(after.len());
                rest = &after[close + 1.min(after.len() - close)..];
            }
        }
    }
    out
}

/// Remove every `mix-blend-mode: X;` (and trailing `;`) inside a style attr,
/// then collapse any empty `style=""` attributes.
fn strip_blend_modes(text: &str) -> String {
    deep_strip_blend(text)
}

fn deep_strip_blend(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find("mix-blend-mode:") {
            None => {
                out.push_str(rest);
                break;
            }
            Some(pos) => {
                out.push_str(&rest[..pos]);
                let rest_str = &rest[pos + "mix-blend-mode:".len()..];
                let end = rest_str
                    .find(';')
                    .or_else(|| rest_str.find('"'))
                    .unwrap_or(rest_str.len());
                // Drop the value + the ';' if we stopped at one.
                let stop_at = if end < rest_str.len() && rest_str.as_bytes()[end] == b';' {
                    end + 1
                } else {
                    end
                };
                rest = &rest_str[stop_at..];
            }
        }
    }
    // Remove `style="..."` that have become empty or start with ';'
    // (the original may be `style="mix-blend-mode: multiply"` -> now `style=""`).
    let mut cleaned = out;
    // After removal, style attrs that contain only spaces/empties:
    while cleaned.contains("style=\"\"") {
        cleaned = cleaned.replace("style=\"\"", "");
    }
    while cleaned.contains("style=\" \"") {
        cleaned = cleaned.replace("style=\" \"", "style=\"\"");
    }
    cleaned
}

/// A no-op passthrough for text (the ablation already applied).
fn apply_ablations(source: &str, opts: &BenchOptions) -> String {
    let mut mutated = source.to_string();
    if opts.no_blend {
        mutated = strip_blend_modes(&mutated);
    }
    if opts.no_clip {
        mutated = strip_attr(&mutated, "clip-path");
    }
    if opts.no_stroke {
        mutated = strip_attr(&mutated, "stroke");
        mutated = strip_attr(&mutated, "stroke-width");
    }
    mutated
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

pub fn run_bench(opts: &BenchOptions) -> Result<(), RasterError> {
    let bytes = std::fs::read(&opts.svg).map_err(|error| {
        RasterError::invalid(format!("cannot read {}: {error}", opts.svg.display()))
    })?;
    let source = String::from_utf8(bytes)
        .map_err(|error| RasterError::invalid(format!("svg is not utf-8: {error}")))?;
    let mutated = apply_ablations(&source, opts);

    // Autodetect intrinsic px from <svg width/height>.
    let (w, h) = match (opts.width, opts.height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, w),
        (None, Some(h)) => (h, h),
        (None, None) => autodetect_size(&mutated),
    };

    // A --width/--height that differs from the SVG intrinsic size: resvg
    // honors the SVG's own pixel size, so to render at a different
    // resolution we must also scale the content. Wrap the whole document in
    // a root transform that scales coordinates by (req/intrinsic) in the
    // SVG user space and set width/height to the requested px. This is the
    // "resolution scaling test" from the ablation plan: it keeps the same
    // scene but changes the output pixel count ~4x per half step.
    let (intrinsic_w, intrinsic_h) = autodetect_size(&mutated);
    let mutated = if (opts.width.is_some() || opts.height.is_some())
        && (w != intrinsic_w || h != intrinsic_h)
    {
        scale_svg(&mutated, w, h, intrinsic_w, intrinsic_h)
    } else {
        mutated
    };

    let started = Instant::now();
    let image = render_svg(mutated.as_bytes(), (w, h))?;
    let total_ms = elapsed_ms(started);

    println!(
        "source={} bytes mutated={} rendered={}x{} total={:.1}ms",
        source.len(),
        mutated.len(),
        image.width,
        image.height,
        total_ms,
    );
    Ok(())
}

fn parse_px(value: &str) -> u32 {
    let v = value.trim();
    let v = v.trim_end_matches("px").trim();
    v.parse::<f64>().unwrap_or(0.0).round() as u32
}

fn autodetect_size(svg: &str) -> (u32, u32) {
    let w = extract_attr(svg, "width")
        .map(|v| parse_px(&v))
        .unwrap_or(0);
    let h = extract_attr(svg, "height")
        .map(|v| parse_px(&v))
        .unwrap_or(0);
    (w.max(1), h.max(1))
}

fn extract_attr(svg: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = svg.find(&needle)?;
    let after = &svg[start + needle.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// Scale the root <svg> so it renders at `(req_w, req_h)` pixels: update the
/// width/height attributes and add a root transform that scales the
/// coordinate system by (req_w/intrinsic_w, req_h/intrinsic_h).
fn scale_svg(svg: &str, req_w: u32, req_h: u32, src_w: u32, src_h: u32) -> String {
    let sw = req_w as f64 / src_w.max(1) as f64;
    let sh = req_h as f64 / src_h.max(1) as f64;

    let mut out = svg.to_string();
    out = replace_attr_px(&out, "width", req_w);
    out = replace_attr_px(&out, "height", req_h);

    // Insert `transform="scale(sx sy)"` into the root <svg> tag: find the
    // `<svg` opening tag, then its first '>'.
    let svg_open = out.find("<svg").unwrap_or(0);
    let tail = &out[svg_open..];
    let rel = tail.find('>').unwrap_or(tail.len());
    let insert_at = svg_open + rel;
    let mut buf = String::with_capacity(out.len() + 48);
    buf.push_str(&out[..insert_at]);
    let scale_attr = format!(" transform=\"scale({} {})\"", fmt_g(sw), fmt_g(sh));
    buf.push_str(&scale_attr);
    buf.push_str(&out[insert_at..]);
    buf
}

fn fmt_g(v: f64) -> String {
    // Shortest round-trip decimal like Rust's {}/f64 is close enough.
    format!("{}", v)
}

/// Replace width=.../height=... (with an optional trailing px) in the root
/// <svg> element with an unqualified px value.
fn replace_attr_px(svg: &str, attr: &str, value: u32) -> String {
    let needle = format!("{attr}=\"");
    if let Some(start) = svg.find(&needle) {
        let after = &svg[start + needle.len()..];
        let end = after.find('"').unwrap_or(after.len());
        let mut out = String::with_capacity(svg.len());
        out.push_str(&svg[..start + needle.len()]);
        let value_str = format!("{}", value);
        out.push_str(&value_str);
        out.push_str(&after[end..]);
        out
    } else {
        svg.to_string()
    }
}
