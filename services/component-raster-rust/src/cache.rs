//! Content-addressed component-raster cache for appearance-independent parts.
//!
//! A component raster is **appearance-independent** when the part has no
//! color customization: no `color_rules` (character tint mapping) and no
//! `placement_colors` (authored color transforms). For such parts the final
//! output PNG is a pure function of the placed state geometry — the state
//! SVG content, the characterB matrix, darkening, layer placement, the shared
//! viewbox/raster scale, the output grid, and zoom — and is byte-identical
//! across characters and jobs. The cache stores that final PNG plus its
//! placement so a later job can skip the bundle download, import, SVG build,
//! resvg rasterize, downsample, and encode entirely.
//!
//! The cache key is computed from manifest/task fields only (no bundle
//! download needed), so a hit is a cheap HEAD + two GETs. Color-customizable
//! parts are never cached: their output depends on the character's colors, so
//! a shared entry would be wrong.
//!
//! Entries live in the work bucket under `component-rasters/{schema}/{key}`
//! (the worker already has read/write there; the prefix is outside the 2-day
//! `jobs/` lifecycle rule, so entries persist for reuse).

use serde::{Deserialize, Serialize};

use crate::contract::Part;
use crate::error::RasterError;
use crate::storage::{Sink, Source};
use crate::telemetry::sha256_hex;

/// Bump when the raster pipeline changes in a way that can alter output
/// bytes for identical inputs (resvg / fast_image_resize / downsample
/// settings). Stale entries are simply never hit after a bump.
pub const CACHE_SCHEMA: &str = "1";

/// A part is appearance-independent iff it has no color customization.
pub fn is_no_cc(part: &Part) -> bool {
    part.color_rules.is_empty() && part.placement_colors.is_empty()
}

/// Canonical, deterministic inputs that fully determine the output PNG for a
/// no-CC part. Serialized with serde_json (stable field order, shortest float
/// repr) and hashed.
#[derive(Serialize)]
struct CacheKeyInput {
    schema: &'static str,
    state_signature: String,
    matrix: [f64; 6],
    darken: bool,
    layer_index: i64,
    layer_name: String,
    viewbox: [f64; 4],
    raster_size: i64,
    output_size: i64,
    component_raster_space: String,
    zoom: f64,
    root_class: String,
    character_id: Option<i64>,
}

/// Compute the content-addressed cache key for one no-CC task.
///
/// All inputs come from the manifest + task, so the key is available before
/// the source bundle is downloaded.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    state_signature: &str,
    matrix: [f64; 6],
    darken: bool,
    layer_index: i64,
    layer_name: &str,
    viewbox: [f64; 4],
    raster_size: i64,
    output_size: i64,
    component_raster_space: &str,
    zoom: f64,
    part: &Part,
) -> Result<String, RasterError> {
    let input = CacheKeyInput {
        schema: CACHE_SCHEMA,
        state_signature: state_signature.to_string(),
        matrix,
        darken,
        layer_index,
        layer_name: layer_name.to_string(),
        viewbox,
        raster_size,
        output_size,
        component_raster_space: component_raster_space.to_string(),
        zoom,
        root_class: part.root_class.clone(),
        character_id: part.character_id,
    };
    let bytes = serde_json::to_vec(&input)
        .map_err(|error| RasterError::Json(format!("cannot serialize cache key: {error}")))?;
    Ok(sha256_hex(&bytes))
}

fn png_key(key: &str) -> String {
    format!("component-rasters/{CACHE_SCHEMA}/{key}.png")
}

fn meta_key(key: &str) -> String {
    format!("component-rasters/{CACHE_SCHEMA}/{key}.json")
}

/// Placement + diagnostics stored alongside the cached PNG.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheMeta {
    pub empty: bool,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub input_bytes: u64,
    pub svg_bytes: u64,
    pub filter_count: usize,
}

/// Best-effort cache read. Any error (missing object, corrupt JSON, sha
/// mismatch) is a miss; the caller falls through to a normal render.
pub async fn read_cache(
    source: &dyn Source,
    key: &str,
) -> Result<Option<(CacheMeta, Option<Vec<u8>>)>, RasterError> {
    let meta_bytes = match source.fetch(&meta_key(key)).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let meta: CacheMeta = match serde_json::from_slice(&meta_bytes) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.empty {
        return Ok(Some((meta, None)));
    }
    let png = match source.fetch(&png_key(key)).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if let Some(expected) = &meta.sha256 {
        if sha256_hex(&png) != *expected {
            return Ok(None);
        }
    }
    Ok(Some((meta, Some(png))))
}

/// Best-effort cache write. Failures are logged and ignored: the cache is
/// optional acceleration, never a correctness dependency.
pub async fn write_cache(
    sink: &dyn Sink,
    key: &str,
    meta: &CacheMeta,
    png: Option<&[u8]>,
) -> Result<(), RasterError> {
    let meta_bytes = serde_json::to_vec(meta)
        .map_err(|error| RasterError::Json(format!("cannot serialize cache meta: {error}")))?;
    sink.put(&meta_key(key), "application/json", &meta_bytes)
        .await?;
    if let Some(png) = png {
        sink.put(&png_key(key), "image/png", png).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cc_part() -> Part {
        Part {
            root_class: "Armor".to_string(),
            character_id: Some(286),
            color_rules: Default::default(),
            placement_colors: Default::default(),
        }
    }

    fn cc_part() -> Part {
        let mut part = no_cc_part();
        part.color_rules.insert(
            "Base".to_string(),
            vec!["Base".to_string(), "dark".to_string()],
        );
        part
    }

    #[test]
    fn no_cc_detection() {
        assert!(is_no_cc(&no_cc_part()));
        assert!(!is_no_cc(&cc_part()));
    }

    #[test]
    fn key_is_deterministic_and_sensitive() {
        let base = cache_key(
            "sig-1",
            [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
            false,
            3,
            "armor_chest",
            [0.0, 0.0, 100.0, 200.0],
            512,
            256,
            "output",
            2.0,
            &no_cc_part(),
        )
        .unwrap();
        let same = cache_key(
            "sig-1",
            [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
            false,
            3,
            "armor_chest",
            [0.0, 0.0, 100.0, 200.0],
            512,
            256,
            "output",
            2.0,
            &no_cc_part(),
        )
        .unwrap();
        assert_eq!(base, same);

        // Every input that changes the output must change the key.
        let variants = [
            cache_key(
                "sig-2",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 7.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                true,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                4,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_head",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 99.0, 200.0],
                512,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                1024,
                256,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                128,
                "output",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "raster",
                2.0,
                &no_cc_part(),
            )
            .unwrap(),
            cache_key(
                "sig-1",
                [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
                false,
                3,
                "armor_chest",
                [0.0, 0.0, 100.0, 200.0],
                512,
                256,
                "output",
                3.0,
                &no_cc_part(),
            )
            .unwrap(),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn key_ignores_character_colors_for_no_cc_parts() {
        // A no-CC part's key must not depend on the character's color rules
        // (they are empty for the part), so the same item placed the same way
        // reuses across characters.
        let mut other = no_cc_part();
        other.root_class = "Armor".to_string();
        let a = cache_key(
            "sig-1",
            [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
            false,
            3,
            "armor_chest",
            [0.0, 0.0, 100.0, 200.0],
            512,
            256,
            "output",
            2.0,
            &no_cc_part(),
        )
        .unwrap();
        let b = cache_key(
            "sig-1",
            [1.0, 0.0, 0.0, 1.0, 5.0, 6.0],
            false,
            3,
            "armor_chest",
            [0.0, 0.0, 100.0, 200.0],
            512,
            256,
            "output",
            2.0,
            &other,
        )
        .unwrap();
        assert_eq!(a, b);
    }
}
