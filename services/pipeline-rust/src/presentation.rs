//! Layout policy independent of character rasterization and presentation artwork.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub const POLICY: &str = "presentation-v1";

/// Resolve presets into explicit settings; callers may independently override
/// framing, the fixed viewport, character registration, background and info.
pub fn normalize(view: &str, overrides: &Value) -> Result<Value> {
    ensure!(
        matches!(view, "character" | "charpage"),
        "invalid render view"
    );
    let mut result = if view == "charpage" {
        json!({"framing":"fixed","viewport":[0.0,0.0,550.0,350.0],"character_position":[338.05,304.2],"background":true,"info":true})
    } else {
        json!({"framing":"content","viewport":[0.0,0.0,550.0,350.0],"character_position":[0.0,0.0],"background":false,"info":false})
    };
    if !overrides.is_null() {
        for (key, value) in overrides
            .as_object()
            .context("presentation must be an object")?
        {
            ensure!(
                result.get(key).is_some(),
                "unknown presentation setting {key}"
            );
            result[key] = value.clone();
        }
    }
    ensure!(
        matches!(result["framing"].as_str(), Some("content" | "fixed")),
        "invalid presentation framing"
    );
    for key in ["background", "info"] {
        ensure!(result[key].is_boolean(), "invalid presentation {key}");
    }
    for (key, len) in [("viewport", 4), ("character_position", 2)] {
        let values = result[key]
            .as_array()
            .context("layout coordinates must be arrays")?;
        ensure!(
            values.len() == len
                && values.iter().all(|v| v
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && n.abs() <= 10000.0)),
            "invalid presentation {key}"
        );
        // DynamoDB normalizes 550.0 to 550. Canonicalize both representations
        // before launcher admission comparisons and render cache hashing.
        result[key] = Value::Array(values.iter().map(|v| json!(v.as_f64().unwrap())).collect());
    }
    ensure!(
        result["viewport"][2].as_f64().unwrap() >= 1.0
            && result["viewport"][3].as_f64().unwrap() >= 1.0,
        "viewport width/height must be positive"
    );
    Ok(result)
}

pub fn viewbox(layout: &Value, content: [f64; 4]) -> [f64; 4] {
    if layout["framing"] == "content" {
        return content;
    }
    // Shift the viewport instead of rewriting every layer's transform. This is
    // equivalent to translating the character into the specified fixed viewport.
    [
        layout["viewport"][0].as_f64().unwrap() - layout["character_position"][0].as_f64().unwrap(),
        layout["viewport"][1].as_f64().unwrap() - layout["character_position"][1].as_f64().unwrap(),
        layout["viewport"][2].as_f64().unwrap(),
        layout["viewport"][3].as_f64().unwrap(),
    ]
}

pub fn canvas(viewbox: [f64; 4], raster: u32, output: u32, output_space: bool) -> [u32; 2] {
    let scale = raster as f64 / viewbox[2].max(viewbox[3]);
    let raster_canvas = [
        (viewbox[2] * scale).round_ties_even().max(1.0),
        (viewbox[3] * scale).round_ties_even().max(1.0),
    ];
    let scale = if output_space {
        output as f64 / raster_canvas[0].max(raster_canvas[1])
    } else {
        1.0
    };
    raster_canvas.map(|n| (n * scale).round_ties_even().max(1.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_and_independent_overrides() {
        let content = [-20.0, -80.0, 100.0, 200.0];
        let normal = normalize("character", &json!({"background":true})).unwrap();
        assert_eq!(viewbox(&normal, content), content);
        assert_eq!(normal["background"], true);
        assert_eq!(normal["info"], false);
        let card = normalize(
            "charpage",
            &json!({"info":false,"character_position":[400.0,300.0]}),
        )
        .unwrap();
        assert_eq!(viewbox(&card, content), [-400.0, -300.0, 550.0, 350.0]);
        assert_eq!(card["background"], true);
        assert_eq!(card["info"], false);
        for value in [
            json!({"typo":true}),
            json!({"info":"yes"}),
            json!({"viewport":[0,0,0,350]}),
            json!({"character_position":[0]}),
            json!([]),
        ] {
            assert!(normalize("character", &value).is_err());
        }
    }
    #[test]
    fn dynamodb_number_normalization_preserves_admission_equality() {
        let queued = json!({"viewport":[0.0,0.0,550.0,350.0],"character_position":[338.05,304.2]});
        let stored = json!({"viewport":[0,0,550,350],"character_position":[338.05,304.2]});
        assert_eq!(normalize("charpage", &queued).unwrap(), normalize("charpage", &stored).unwrap());
        let integral_position = json!({"character_position":[400,300]});
        let float_position = json!({"character_position":[400.0,300.0]});
        assert_eq!(normalize("charpage", &integral_position).unwrap(), normalize("charpage", &float_position).unwrap());
        assert_ne!(normalize("charpage", &queued).unwrap(), normalize("charpage", &integral_position).unwrap());
    }
    #[test]
    fn canvas_matches_composer_at_odd_sizes() {
        for raster in [65, 129, 257, 1024, 2048, 4096] {
            for viewbox in [[0.0, 0.0, 550.0, 350.0], [0.0, 0.0, 160.0, 700.0]] {
                let (r, o) =
                    aqw_component_compose::compositor::frame_canvas_sizes(&viewbox, raster, 64)
                        .unwrap();
                assert_eq!(
                    canvas(viewbox, raster as u32, 64, false),
                    r.map(|n| n as u32)
                );
                assert_eq!(
                    canvas(viewbox, raster as u32, 64, true),
                    o.map(|n| n as u32)
                );
            }
        }
    }
}
