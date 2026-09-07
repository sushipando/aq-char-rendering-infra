//! Charpage presentation: original static artwork, drawn once per job.
use crate::store::Store;
use anyhow::{ensure, Context, Result};
use resvg::{tiny_skia, usvg};
use serde_json::{json, Value};

pub const POLICY: &str = "charpage-v1";
// characterB stage is 550x350; pMC is registered at (338.05, 304.2).
pub const VIEWBOX: [f64; 4] = [-338.05, -304.2, 550.0, 350.0];
const BACKGROUNDS: &[&[u8]] = &[
    include_bytes!("../assets/charpage/background.svgz"),
    include_bytes!("../assets/charpage/background-1.svgz"),
    include_bytes!("../assets/charpage/background-2.svgz"),
    include_bytes!("../assets/charpage/background-3.svgz"),
    include_bytes!("../assets/charpage/background-4.svgz"),
    include_bytes!("../assets/charpage/background-5.svgz"),
    include_bytes!("../assets/charpage/background-6.svgz"),
    include_bytes!("../assets/charpage/background-7.svgz"),
    include_bytes!("../assets/charpage/background-8.svgz"),
    include_bytes!("../assets/charpage/background-9.svgz"),
    include_bytes!("../assets/charpage/background-10.svgz"),
    include_bytes!("../assets/charpage/background-11.svgz"),
    include_bytes!("../assets/charpage/background-12.svgz"),
    include_bytes!("../assets/charpage/background-13.svgz"),
    include_bytes!("../assets/charpage/background-14.svgz"),
    include_bytes!("../assets/charpage/background-15.svgz"),
    include_bytes!("../assets/charpage/background-16.svgz"),
    include_bytes!("../assets/charpage/background-17.svgz"),
    include_bytes!("../assets/charpage/background-18.svgz"),
    include_bytes!("../assets/charpage/background-19.svgz"),
    include_bytes!("../assets/charpage/background-20.svgz"),
    include_bytes!("../assets/charpage/background-21.svgz"),
    include_bytes!("../assets/charpage/background-22.svgz"),
    include_bytes!("../assets/charpage/background-23.svgz"),
    include_bytes!("../assets/charpage/background-24.svgz"),
    include_bytes!("../assets/charpage/background-25.svgz"),
    include_bytes!("../assets/charpage/background-26.svgz"),
    include_bytes!("../assets/charpage/background-27.svgz"),
    include_bytes!("../assets/charpage/background-28.svgz"),
    include_bytes!("../assets/charpage/background-29.svgz"),
    include_bytes!("../assets/charpage/background-30.svgz"),
    include_bytes!("../assets/charpage/background-31.svgz"),
    include_bytes!("../assets/charpage/background-32.svgz"),
    include_bytes!("../assets/charpage/background-33.svgz"),
    include_bytes!("../assets/charpage/background-34.svgz"),
    include_bytes!("../assets/charpage/background-35.svgz"),
];

pub fn background_index(fields: &Value) -> usize {
    fields["bgindex"]
        .as_str()
        .and_then(|s| usize::from_str_radix(s, 36).ok())
        .filter(|n| *n < BACKGROUNDS.len())
        .unwrap_or(0)
}

fn options() -> usvg::Options<'static> {
    let mut options = usvg::Options::default();
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../assets/charpage/name.ttf").to_vec());
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../assets/charpage/body.ttf").to_vec());
    options
}

fn draw(
    bytes: &[u8],
    options: &usvg::Options,
    canvas: &mut tiny_skia::Pixmap,
    x: f32,
    y: f32,
) -> Result<()> {
    let tree = usvg::Tree::from_data(bytes, options)?;
    let scale = (canvas.width() as f32 / 550.0).min(canvas.height() as f32 / 350.0);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x * scale, y * scale),
        &mut canvas.as_mut(),
    );
    Ok(())
}

fn escaped(value: &str) -> String {
    html_escape::encode_text(value).into_owned()
}

fn text(
    options: &usvg::Options,
    canvas: &mut tiny_skia::Pixmap,
    value: &str,
    x: f32,
    y: f32,
    size: f32,
    name: bool,
    gold: bool,
) -> Result<()> {
    let family = if name { "BD Merced" } else { "Arial Black" };
    let color = if gold { "#ffcc66" } else { "white" };
    let element = format!(
        r#"<text x="0" y="0" font-family="{family}" font-size="{size}" fill="{color}" stroke="black" stroke-width="1.3" stroke-linejoin="round" paint-order="stroke fill">{}</text>"#,
        escaped(value)
    );
    let measure = usvg::Tree::from_data(
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="2000" height="200">{element}</svg>"#
        )
        .as_bytes(),
        options,
    )?;
    let width = measure.root().abs_bounding_box().width();
    let fit = ((530.0 - x) / width.max(1.0)).min(1.0);
    draw(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="550" height="350"><g transform="translate({x} {y}) scale({fit})">{element}</g></svg>"#).as_bytes(), options, canvas, 0.0, 0.0)
}

pub fn render(fields: &Value, settings: &Value, width: u32) -> Result<(Vec<u8>, Vec<u8>)> {
    ensure!((64..=4096).contains(&width), "invalid charpage size");
    // Match the composer's two-stage canvas rounding, including odd raster sizes.
    let raster = settings["raster_size"].as_u64().unwrap_or(width as u64) as f64;
    let raster_height = (raster * 350.0 / 550.0).round_ties_even();
    let height = (raster_height * width as f64 / raster).round_ties_even() as u32;
    let (back, front) = render_layers(fields, settings, [width, height], true, true)?;
    Ok((back.unwrap(), front.unwrap()))
}

pub fn render_layers(
    fields: &Value,
    settings: &Value,
    canvas: [u32; 2],
    background_enabled: bool,
    info_enabled: bool,
) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    let [width, height] = canvas;
    ensure!(
        width > 0 && height > 0 && width <= 4096 && height <= 4096,
        "invalid presentation canvas"
    );
    let options = if info_enabled {
        options()
    } else {
        usvg::Options::default()
    };
    let background = if background_enabled {
        let mut background =
            tiny_skia::Pixmap::new(width, height).context("presentation allocation")?;
        background.fill(tiny_skia::Color::from_rgba8(254, 241, 197, 255));
        let index = background_index(fields);
        let tree = usvg::Tree::from_data(BACKGROUNDS[index], &options)?;
        // Cover arbitrary content/fixed viewports without distorting the artwork.
        let scale = (width as f32 / 550.0).max(height as f32 / 350.0);
        let x = (width as f32 - 550.0 * scale) / 2.0 + if index == 0 { 0.0 } else { 5.0 * scale };
        let y = (height as f32 - 350.0 * scale) / 2.0;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x, y),
            &mut background.as_mut(),
        );
        // Fade belongs to the viewport edges, independently of background crop.
        let fade = usvg::Tree::from_data(include_bytes!("../assets/charpage/fade.svgz"), &options)?;
        resvg::render(
            &fade,
            tiny_skia::Transform::from_scale(width as f32 / 550.0, height as f32 / 350.0),
            &mut background.as_mut(),
        );
        Some(background.encode_png()?)
    } else {
        None
    };
    if !info_enabled {
        return Ok((background, None));
    }
    let mut foreground = tiny_skia::Pixmap::new(width, height).context("charpage allocation")?;
    draw(
        include_bytes!("../assets/charpage/chrome.svgz"),
        &options,
        &mut foreground,
        0.0,
        0.0,
    )?;
    let faction = fields["strFaction"].as_str().unwrap_or("Neutral");
    let icon: &[u8] = match faction.to_ascii_lowercase().as_str() {
        "evil" => include_bytes!("../assets/charpage/faction-evil.svgz"),
        "good" => include_bytes!("../assets/charpage/faction-good.svgz"),
        "chaos" => include_bytes!("../assets/charpage/faction-chaos.svgz"),
        _ => include_bytes!("../assets/charpage/faction-neutral.svgz"),
    };
    draw(icon, &options, &mut foreground, 11.95, 275.3)?;
    let display = crate::metadata::display(fields, settings)?;
    let field = |key: &str| display[key].as_str().unwrap_or("");
    text(
        &options,
        &mut foreground,
        field("name"),
        20.0,
        51.0,
        50.0,
        true,
        false,
    )?;
    text(
        &options,
        &mut foreground,
        field("class"),
        20.0,
        79.0,
        14.0,
        false,
        true,
    )?;
    text(
        &options,
        &mut foreground,
        &format!("Level {}", field("level")),
        20.0,
        98.0,
        12.0,
        false,
        true,
    )?;
    if !field("guild").is_empty() {
        draw(
            include_bytes!("../assets/charpage/guild.svgz"),
            &options,
            &mut foreground,
            0.0,
            0.0,
        )?;
        text(
            &options,
            &mut foreground,
            &format!("{} Guild", field("guild")),
            43.0,
            121.0,
            12.0,
            false,
            false,
        )?;
    }
    for (slot, y) in [
        ("Weapon", 147.0),
        ("Armor", 171.0),
        ("Helm", 195.0),
        ("Cape", 217.0),
        ("Pet", 240.0),
        ("Rune", 265.0),
    ] {
        let value = display["shown_items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["slot"] == slot)
            .and_then(|v| v["name"].as_str())
            .unwrap_or("None");
        text(
            &options,
            &mut foreground,
            value,
            46.0,
            y,
            12.0,
            false,
            false,
        )?;
    }
    text(
        &options,
        &mut foreground,
        &format!("{faction} Hero"),
        46.0,
        293.0,
        12.0,
        false,
        false,
    )?;
    Ok((background, Some(foreground.encode_png()?)))
}

pub async fn prepare(
    store: &dyn Store,
    bucket: &str,
    job: &str,
    fields: &Value,
    settings: &Value,
    layout: &Value,
    canvas: [u32; 2],
) -> Result<Value> {
    let (background, foreground) = render_layers(
        fields,
        settings,
        canvas,
        layout["background"] == true,
        layout["info"] == true,
    )?;
    let mut record = json!({"policy":POLICY});
    for (name, bytes) in [("background", background), ("foreground", foreground)] {
        if let Some(bytes) = bytes {
            let key = format!("jobs/{job}/prepare/presentation-{name}.png");
            record[name] = json!({"key":key,"sha256":crate::sha256(&bytes)});
            store.put(bucket, &key, bytes, "image/png", false).await?;
        }
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_ids_are_base36_and_bounded() {
        assert_eq!(background_index(&json!({"bgindex":"z"})), 35);
        assert_eq!(background_index(&json!({"bgindex":"8"})), 8);
        assert_eq!(background_index(&json!({"bgindex":"../../x"})), 0);
    }
    #[test]
    fn optional_artwork_layers_are_independent() {
        for (background, info) in [(false, false), (true, false), (false, true)] {
            let (back, front) =
                render_layers(&json!({}), &json!({}), [64, 100], background, info).unwrap();
            assert_eq!(back.is_some(), background);
            assert_eq!(front.is_some(), info);
        }
    }
    #[test]
    fn renders_static_layers_without_system_fonts() {
        let (back, front) = render(
            &json!({"strName":"___cj & <test>","strFaction":"Evil","bgindex":"8"}),
            &json!({}),
            550,
        )
        .unwrap();
        let back = tiny_skia::Pixmap::decode_png(&back).unwrap();
        let front = tiny_skia::Pixmap::decode_png(&front).unwrap();
        assert_eq!((back.width(), back.height()), (550, 350));
        assert!(back.pixels().iter().all(|p| p.alpha() == 255));
        assert!(front.pixels().iter().any(|p| p.alpha() > 0));
    }
}
