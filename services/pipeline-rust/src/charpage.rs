//! Charpage presentation: original static artwork, drawn once per job.
use crate::store::Store;
use anyhow::{ensure, Context, Result};
use resvg::{tiny_skia, usvg};
use serde_json::{json, Value};

pub const POLICY: &str = "charpage-v3-border-options";
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

fn border_color(layout: &Value) -> tiny_skia::Color {
    let rgb = u32::from_str_radix(&layout["border_color"].as_str().unwrap()[1..],16).unwrap();
    tiny_skia::Color::from_rgba8((rgb>>16) as u8,(rgb>>8) as u8,rgb as u8,255)
}

fn border_overlay(layout: &Value, canvas: [u32;2]) -> Result<Option<tiny_skia::Pixmap>> {
    if layout["border_fade"] != true { return Ok(None); }
    use std::io::Read;
    let mut svg = String::new();
    flate2::read::GzDecoder::new(include_bytes!("../assets/charpage/fade.svgz").as_slice()).read_to_string(&mut svg)?;
    // Preserve the authored opacity gradient and geometry, changing only RGB.
    let color = layout["border_color"].as_str().unwrap();
    let svg = svg.replace("#fef0c1",color).replace("#ffffff",color);
    let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default())?;
    let mut fade = tiny_skia::Pixmap::new(canvas[0],canvas[1]).context("fade canvas")?;
    resvg::render(&tree,tiny_skia::Transform::from_scale(canvas[0] as f32/550.0,canvas[1] as f32/350.0), &mut fade.as_mut());
    Ok(Some(fade))
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
    let layout = crate::presentation::normalize(settings["view"].as_str().unwrap_or("character"), &settings["presentation"])?;
    let background = if background_enabled {
        let mut background =
            tiny_skia::Pixmap::new(width, height).context("presentation allocation")?;
        if layout["border_fade"] == true { background.fill(border_color(&layout)); }
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
        if let Some(fade) = border_overlay(&layout,canvas)? {
            background.draw_pixmap(0,0,fade.as_ref(),&tiny_skia::PixmapPaint::default(),tiny_skia::Transform::identity(),None);
        }
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
    let mut effective_settings = settings.clone();
    effective_settings["presentation"] = layout.clone();
    let settings = &effective_settings;
    let dynamic = layout["background"] == true && background_index(fields) > 0;
    let (mut background, foreground) = render_layers(
        fields,
        settings,
        canvas,
        layout["background"] == true && !dynamic,
        layout["info"] == true,
    )?;
    let overlay = if dynamic && layout["border_fade"] == true {
        let mut base = tiny_skia::Pixmap::new(canvas[0],canvas[1]).context("background canvas")?;
        base.fill(border_color(layout));
        background = Some(base.encode_png()?);
        border_overlay(layout,canvas)?.map(|fade| fade.encode_png()).transpose()?
    } else { None };
    let mut record = json!({"policy":POLICY});
    for (name, bytes) in [("background", background), ("background_overlay", overlay), ("foreground", foreground)] {
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
    fn border_recolor_preserves_fade_alpha_and_can_be_disabled() {
        let red = crate::presentation::normalize("charpage",&json!({"border_color":"FF0000"})).unwrap();
        let blue = crate::presentation::normalize("charpage",&json!({"border_color":"0000FF"})).unwrap();
        let r = border_overlay(&red,[110,70]).unwrap().unwrap();
        let b = border_overlay(&blue,[110,70]).unwrap().unwrap();
        assert!(r.data().chunks_exact(4).any(|p|p[3]>0 && p[3]<255));
        for (r,b) in r.data().chunks_exact(4).zip(b.data().chunks_exact(4)) {
            assert_eq!(r[3],b[3]);
            assert_eq!(&r[..3], &[r[3],0,0]);
            assert_eq!(&b[..3], &[0,0,b[3]]);
        }
        let disabled = crate::presentation::normalize("charpage",&json!({"border_fade":false})).unwrap();
        assert!(border_overlay(&disabled,[110,70]).unwrap().is_none());
    }

    #[tokio::test]
    async fn animated_background_without_border_has_no_tinted_base_or_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::store::FsStore(temp.path().into());
        let layout = crate::presentation::normalize("charpage",&json!({"border_fade":false,"info":false})).unwrap();
        let layers = prepare(&store,"work","fixture",&json!({"bgindex":"W"}),&json!({}),&layout,[110,70]).await.unwrap();
        assert!(layers["background"].is_null());
        assert!(layers["background_overlay"].is_null());
        assert!(layers["foreground"].is_null());
        let mut enabled = layout;
        enabled["border_fade"] = true.into();
        enabled["border_color"] = "#123456".into();
        let layers = prepare(&store,"work","fixture",&json!({"bgindex":"W"}),&json!({}),&enabled,[110,70]).await.unwrap();
        let bytes = store.get("work",layers["background"]["key"].as_str().unwrap()).await.unwrap().unwrap();
        let base = tiny_skia::Pixmap::decode_png(&bytes).unwrap();
        assert_eq!(&base.data()[..4],&[0x12,0x34,0x56,255]);
        assert!(layers["background_overlay"]["key"].is_string());
    }
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
