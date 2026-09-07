//! Version-one public request contract, shared by every Rust handler.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

pub fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .with_context(|| format!("missing string {name}"))
}
pub fn integer(value: &Value, name: &str, min: u64, max: u64) -> Result<u64> {
    let n = value[name]
        .as_u64()
        .with_context(|| format!("invalid integer {name}"))?;
    ensure!((min..=max).contains(&n), "{name} out of range");
    Ok(n)
}
pub(crate) fn keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value.as_object().context("expected object")?;
    ensure!(
        object.keys().all(|key| allowed.contains(&key.as_str())),
        "unsupported request field"
    );
    Ok(())
}

pub fn request(mut value: Value, defaults: Option<&Value>) -> Result<Value> {
    keys(
        &value,
        &[
            "schema_version",
            "job_id",
            "created_at",
            "discord",
            "render",
            "bounds_mode",
            "component_raster_mode",
            "cache",
            "appearance",
        ],
    )?;
    ensure!(value["schema_version"] == 1, "unsupported request schema");
    let raw = string(&value, "job_id")?;
    let id = uuid::Uuid::parse_str(raw)?.to_string();
    ensure!(raw.to_lowercase() == id, "job_id must be a canonical UUID");
    value["job_id"] = id.into();
    let time = DateTime::parse_from_rfc3339(string(&value, "created_at")?)?;
    value["created_at"] = time
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        .into();
    keys(&value["discord"], &["user_id", "channel_id", "guild_id"])?;
    for name in ["user_id", "channel_id", "guild_id"] {
        if name == "guild_id" && value["discord"][name].is_null() {
            value["discord"][name] = Value::Null;
            continue;
        }
        let raw = value["discord"][name]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value["discord"][name].to_string());
        ensure!(
            raw.len() <= 20
                && !raw.starts_with('0')
                && !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit()),
            "invalid Discord snowflake"
        );
        value["discord"][name] = raw.into();
    }
    let render = &mut value["render"];
    keys(
        render,
        &[
            "username",
            "base_items",
            "show_hidden",
            "facing",
            "override",
            "complete_loop",
            "max_frames",
            "subframe_start",
            "zoom",
            "raster_size",
            "output_size",
            "max_size",
            "padding",
            "view",
            "presentation",
            "output_format",
            "rgba_compression",
            "avif_quality",
            "avif_speed",
            "webp_quality",
            "webp_method",
            "webp_lossless",
            "raster_backend",
        ],
    )?;
    if !render["max_size"].is_null() {
        ensure!(
            render["raster_size"].is_null() && render["output_size"].is_null(),
            "max_size cannot be combined with raster_size/output_size"
        );
        render["raster_size"] = render["max_size"].clone();
        render["output_size"] = render["max_size"].clone();
        render.as_object_mut().unwrap().remove("max_size");
    }
    if let Some(defaults) = defaults {
        for (key, default) in defaults.as_object().context("invalid render defaults")? {
            render
                .as_object_mut()
                .unwrap()
                .entry(key.clone())
                .or_insert(default.clone());
        }
    }
    let base = json!({"base_items":false,"show_hidden":false,"facing":"right","override":null,"complete_loop":true,"max_frames":360,"subframe_start":1,"zoom":2.0,"raster_size":2048,"output_size":render["raster_size"].as_u64().unwrap_or(2048),"padding":0,"output_format":"webp","rgba_compression":"zstd","avif_quality":70,"avif_speed":8,"webp_quality":85.0,"webp_method":4,"webp_lossless":null,"raster_backend":"resvg"});
    for (key, default) in base.as_object().unwrap() {
        render
            .as_object_mut()
            .unwrap()
            .entry(key.clone())
            .or_insert(default.clone());
    }
    let username = string(render, "username")?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    ensure!(
        (1..=25).contains(&username.len())
            && username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b" _-".contains(&c)),
        "invalid username"
    );
    render["username"] = username.clone().into();
    if render["view"].is_null() { render["view"] = "character".into(); }
    ensure!(matches!(render["view"].as_str(), Some("character" | "charpage")), "invalid render view");
    render["presentation"] = crate::presentation::normalize(render["view"].as_str().unwrap(), &render["presentation"])?;
    let facing = string(render, "facing")?.to_lowercase();
    ensure!(
        matches!(facing.as_str(), "left" | "right"),
        "invalid facing"
    );
    render["facing"] = facing.into();
    // This migration deliberately selects the patched resvg pipeline.
    ensure!(
        render["raster_backend"] == "resvg",
        "this pipeline supports patched resvg only"
    );
    for name in ["base_items", "show_hidden", "complete_loop"] {
        ensure!(render[name].is_boolean(), "invalid boolean {name}");
    }
    ensure!(
        render["webp_lossless"].is_null() || render["webp_lossless"].is_boolean(),
        "invalid webp_lossless"
    );
    let raster = integer(render, "raster_size", 64, 4096)?;
    let output = integer(render, "output_size", 64, 2048)?;
    ensure!(
        output <= raster && integer(render, "padding", 0, 1023)? * 2 < output,
        "invalid output size/padding"
    );
    integer(render, "max_frames", 1, 2000)?;
    integer(render, "subframe_start", 1, 10000)?;
    ensure!(matches!(render["output_format"].as_str(), Some("webp" | "avif")), "invalid output_format");
    ensure!(matches!(render["rgba_compression"].as_str(), Some("none" | "zstd")), "invalid rgba_compression");
    integer(render, "avif_quality", 0, 100)?;
    integer(render, "avif_speed", 0, 10)?;
    integer(render, "webp_method", 0, 6)?;
    for (name, min, max) in [("zoom", 0.25, 8.0), ("webp_quality", 0.0, 100.0)] {
        let n = render[name].as_f64().context("invalid numeric setting")?;
        ensure!(
            n.is_finite() && (min..=max).contains(&n),
            "{name} out of range"
        );
        render[name] = json!(n);
    }
    if !render["override"].is_null() {
        let override_ = &mut render["override"];
        keys(override_, &["item_id", "slot"])?;
        integer(override_, "item_id", 1, 10_000_000)?;
        if !override_["slot"].is_null() {
            let slot = string(override_, "slot")?.to_lowercase();
            ensure!(
                ["armor", "weapon", "helm", "cape", "ground"].contains(&slot.as_str()),
                "invalid override slot"
            );
            override_["slot"] = slot.into();
        } else {
            override_["slot"] = Value::Null;
        }
    }
    for name in ["bounds_mode", "component_raster_mode"] {
        if value[name].is_null() {
            value[name] = "inline".into();
        } else {
            ensure!(
                matches!(value[name].as_str(), Some("inline" | "distributed")),
                "invalid {name}"
            );
        }
    }
    if value["cache"].is_null() {
        value["cache"] =
            json!({"render":true,"animation":true,"vectors":true,"bounds":true,"components":true});
    } else {
        keys(
            &value["cache"],
            &["render", "animation", "vectors", "bounds", "components"],
        )?;
        let cache = value["cache"]
            .as_object_mut()
            .context("cache must be an object")?;
        for name in ["render", "animation", "vectors", "bounds", "components"] {
            cache.entry(name).or_insert(Value::Bool(true));
            ensure!(cache[name].is_boolean(), "invalid cache.{name}");
        }
    }
    if value["appearance"].is_null() {
        value["appearance"] = Value::Null;
    } else {
        let fields = value["appearance"]
            .as_object()
            .context("appearance must be an object")?;
        ensure!(fields.len() <= 128, "too many appearance fields");
        let mut total = 0;
        for (key, val) in fields {
            ensure!(
                !key.is_empty()
                    && key.len() <= 64
                    && key.as_bytes()[0].is_ascii_alphabetic()
                    && key.bytes().all(|b| b.is_ascii_alphanumeric()),
                "invalid appearance field"
            );
            let text = val.as_str().context("appearance value must be a string")?;
            ensure!(text.len() <= 2048, "appearance value too large");
            total += key.len() + text.len();
        }
        ensure!(total <= 32768, "appearance too large");
        ensure!(
            string(&value["appearance"], "strName")?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .eq_ignore_ascii_case(&username),
            "appearance username mismatch"
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Value {
        json!({"schema_version":1,"job_id":"45cfafbd-5089-4f6d-850a-caa798ec1fcb","created_at":"2026-09-05T01:02:03Z","discord":{"user_id":"1","channel_id":"2"},"render":{"username":"Test"}})
    }
    #[test]
    fn username_accepts_leading_underscores_and_hyphens() {
        for name in ["___cj", "-cj", "  ___cj  "] {
            let mut value = sample();
            value["render"]["username"] = name.into();
            assert_eq!(request(value, None).unwrap()["render"]["username"], name.trim());
        }
        for name in ["", "   ", "cj/name", "cj@example", "abcdefghijklmnopqrstuvwxyz"] {
            let mut value = sample();
            value["render"]["username"] = name.into();
            assert!(request(value, None).is_err());
        }
    }
    #[test]
    fn normalizes_defaults_and_rejects_unknown_fields() {
        let parsed = request(sample(), None).unwrap();
        assert_eq!(parsed["render"]["zoom"], 2.0);
        assert_eq!(parsed["bounds_mode"], "inline");
        assert_eq!(parsed["component_raster_mode"], "inline");
        assert_eq!(
            parsed["cache"],
            json!({"render":true,"animation":true,"vectors":true,"bounds":true,"components":true})
        );
        let mut invalid = sample();
        invalid["render"]["surprise"] = true.into();
        assert!(request(invalid, None).is_err());
    }
    #[test]
    fn validates_avif_format_quality_speed_and_reused_lossless_toggle() {
        let defaults = request(sample(), None).unwrap();
        assert_eq!(defaults["render"]["output_format"], "webp");
        for lossless in [false, true] {
            let mut value = sample();
            value["render"]["output_format"] = "avif".into();
            value["render"]["avif_quality"] = 63.into();
            value["render"]["webp_lossless"] = lossless.into();
            let parsed = request(value, None).unwrap();
            assert_eq!(parsed["render"]["avif_quality"], 63);
            assert_eq!(parsed["render"]["avif_speed"], 8);
            assert_eq!(parsed["render"]["webp_lossless"], lossless);
        }
        for (field, bad) in [("output_format",json!("png")), ("rgba_compression",json!("png")), ("avif_quality",json!(101)),
            ("avif_quality",json!(1.5)), ("avif_speed",json!(11))] {
            let mut value = sample(); value["render"][field] = bad;
            assert!(request(value, None).is_err());
        }
    }

    #[test]
    fn size_alias_precedes_defaults() {
        let mut value = sample();
        value["render"]["max_size"] = 512.into();
        let parsed = request(value, Some(&json!({"raster_size":4096,"output_size":2048}))).unwrap();
        assert_eq!(parsed["render"]["output_size"], 512);
    }

    #[test]
    fn sparse_admission_check_is_independent_of_fleet_defaults() {
        let sparse = json!({"schema_version":1,"job_id":"45cfafbd-5089-4f6d-850a-caa798ec1fcb","created_at":"2026-09-05T00:00:00Z","discord":{"user_id":"1","channel_id":"2"},"render":{"username":"alina","max_frames":8,"raster_size":512,"output_size":256,"output_format":"webp","rgba_compression":"zstd","avif_quality":70,"avif_speed":8,"webp_quality":85.0,"webp_method":4,"webp_lossless":null,"raster_backend":"resvg"},"appearance":null});
        let admitted = request(sparse.clone(), None).unwrap();
        let fleet = json!({"complete_loop":true,"max_frames":120,"subframe_start":1,"zoom":1.0,"raster_size":2048,"output_size":2048,"padding":0,"output_format":"webp","rgba_compression":"zstd","avif_quality":70,"avif_speed":8,"webp_quality":85.0,"webp_method":4,"webp_lossless":false,"raster_backend":"resvg"});
        let execution = request(sparse.clone(), Some(&fleet)).unwrap();
        assert_eq!(
            request(admitted.clone(), None).unwrap(),
            request(sparse, None).unwrap()
        );
        assert_eq!(execution["render"]["zoom"], 1.0);
        assert_eq!(admitted["render"]["zoom"], 2.0);
    }

    #[test]
    fn validates_individual_cache_controls() {
        let mut value = sample();
        value["cache"] = json!({"render":false,"bounds":false});
        let parsed = request(value, None).unwrap();
        assert_eq!(
            parsed["cache"],
            json!({"render":false,"animation":true,"vectors":true,"bounds":false,"components":true})
        );
        let mut invalid = sample();
        invalid["cache"] = json!({"vectors":"no"});
        assert!(request(invalid, None).is_err());
    }

    #[test]
    fn validates_explicit_bounds_mode() {
        let mut value = sample();
        value["bounds_mode"] = "distributed".into();
        assert_eq!(request(value, None).unwrap()["bounds_mode"], "distributed");

        let mut invalid = sample();
        invalid["bounds_mode"] = "automatic".into();
        assert!(request(invalid, None).is_err());
    }

    #[test]
    fn validates_explicit_component_raster_mode() {
        let mut value = sample();
        value["component_raster_mode"] = "distributed".into();
        assert_eq!(
            request(value, None).unwrap()["component_raster_mode"],
            "distributed"
        );

        let mut invalid = sample();
        invalid["component_raster_mode"] = "automatic".into();
        assert!(request(invalid, None).is_err());
    }
}
