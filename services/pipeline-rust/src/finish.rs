use crate::{
    bounds,
    config::Config,
    contract::string,
    geometry,
    model::*,
    store::{self, Store},
};
use anyhow::{ensure, Context, Result};
use aqw_component_raster::import::transformed_bounds;
use futures::{stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

pub fn batches(count: usize, size: usize) -> Vec<Value> {
    (1..=count)
        .step_by(size)
        .enumerate()
        .map(
            |(i, start)| json!({"index":i,"frame_start":start,"frame_end":count.min(start+size-1)}),
        )
        .collect()
}

/// Positive authored alpha offsets can turn transparent input into paint.
/// Those appearances must retain the declared page for framing and allocation.
fn state_bounds(result: &BoundsResult, part: &Value) -> Option<[f64; 4]> {
    let creates_alpha = part["placement_colors"].as_object().is_some_and(|colors| {
        colors
            .values()
            .any(|c| c["alpha_add"].as_i64().unwrap_or(0) > 0)
    });
    if creates_alpha {
        match (result.declared_bounds, result.bounds) {
            (Some(declared), Some(measured)) => Some(bounds::union(declared, measured)),
            (declared, measured) => declared.or(measured),
        }
    } else {
        result.bounds
    }
}

/// Group logical animation frames by their exact ordered component recipe.
/// Duration is intentionally not part of the key: it changes playback timing,
/// not the pixels produced by the component compositor.
pub fn component_compositions(frames: &[Value]) -> Result<Vec<Value>> {
    let mut by_layers: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    let mut compositions: Vec<Value> = Vec::new();
    for frame in frames {
        let number = frame["number"]
            .as_u64()
            .context("component frame has no number")?;
        let layers = frame["layers"]
            .as_array()
            .context("component frame has no layers")?
            .iter()
            .map(|layer| {
                layer
                    .as_str()
                    .context("component frame layer is not a string")
                    .map(str::to_string)
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(index) = by_layers.get(&layers).copied() {
            compositions[index]["logical_frames"]
                .as_array_mut()
                .context("invalid component composition")?
                .push(number.into());
        } else {
            by_layers.insert(layers.clone(), compositions.len());
            compositions.push(json!({
                "canonical_frame": number,
                "layers": layers,
                "logical_frames": [number],
            }));
        }
    }
    Ok(compositions)
}

/// Split first-seen unique compositions into contiguous batches. The adaptive
/// size keeps the number of Inline Map iterations at or below its configured
/// concurrency while minimizing the longest batch.
pub fn composition_batches(count: usize, concurrency: usize) -> Vec<Value> {
    if count == 0 {
        return Vec::new();
    }
    let size = count.div_ceil(concurrency.max(1));
    (0..count)
        .step_by(size)
        .enumerate()
        .map(|(index, start)| {
            json!({
                "index": index,
                "composition_start": start,
                "composition_end": count.min(start + size) - 1,
            })
        })
        .collect()
}

pub async fn finish(store: &dyn Store, config: &Config, event: &Value) -> Result<Value> {
    let started = Instant::now();
    let request = crate::contract::request(event["request"].clone(), None)?;
    let job = string(&request, "job_id")?;
    let prepared: Value =
        store::read(store, &config.work_bucket, string(event, "input_key")?).await?;
    ensure!(
        prepared["job_id"] == job && prepared["settings"] == request["render"],
        "prepare input/request mismatch"
    );
    let plan: BoundsPlan = store::read(
        store,
        &config.work_bucket,
        string(event, "bounds_plan_key")?,
    )
    .await?;
    ensure!(
        plan.schema_version == 1
            && plan.job_id == job
            && plan.input_key == string(event, "input_key")?,
        "bounds plan mismatch"
    );
    let results: BTreeMap<String, BoundsResult> = stream::iter(plan.states.iter())
        .map(|(hash, task)| async move {
            let result: BoundsResult =
                store::read(store, &config.work_bucket, &task.result_key).await?;
            result.validate(task)?;
            Ok::<_, anyhow::Error>((hash.clone(), result))
        })
        .buffered(16)
        .try_collect()
        .await?;
    // A callback succeeds only after the immutable result exists, and we
    // verify every result again here before exposing a completed bounds set.
    store::write(
        store,
        &config.work_bucket,
        &format!("jobs/{job}/prepare/bounds-complete.json"),
        &json!({"schema_version":1,"job_id":job,"states":results}),
        false,
    )
    .await?;
    let mut symbols = BTreeMap::new();
    let mut parts = BTreeMap::new();
    let mut all_rules = BTreeSet::new();
    for (index, key) in &plan.source_manifests {
        let source: SourceManifest = store::read(store, &config.work_bucket, key).await?;
        source.validate()?;
        all_rules.extend(source.color_rules.values().cloned());
        for (key, symbol) in source.symbols {
            ensure!(!symbols.contains_key(&key), "duplicate symbol export");
            ensure!(
                symbol
                    .schedule
                    .iter()
                    .all(|hash| results.contains_key(hash)),
                "missing symbol bounds"
            );
            parts.insert(key.clone(),json!({"source_idx":index,"root_class":symbol.request.class_name,"character_id":symbol.request.character_id,"frame_count":symbol.schedule.len(),"root_timeline_frames":symbol.request.root_timeline_frames,"settled_stop_frame":symbol.settled_stop_frame,"color_rules":source.color_rules,"hand_visibility":source.hand_visibility,"placement_colors":source.placement_colors}));
            symbols.insert(key, symbol);
        }
    }
    let settings = &prepared["settings"];
    let complete = settings["complete_loop"]
        .as_bool()
        .context("invalid complete_loop")?;
    let max = settings["max_frames"]
        .as_u64()
        .context("missing frame cap")? as usize;
    let mut ignored = BTreeSet::new();
    if let Some(head) = symbols.get("armor_head") {
        ignored.insert("armor_head".to_string());
        for key in ["helm", "hair", "backhair"] {
            if symbols.get(key).is_some_and(|symbol| {
                geometry::pattern(&symbol.schedule) == geometry::pattern(&head.schedule)
            }) {
                ignored.insert(key.into());
            }
        }
    }
    let mut static_keys = BTreeSet::new();
    let mut ground = BTreeMap::new();
    for (key, symbol) in &symbols {
        if !matches!(key.as_str(), "ground" | "pet") {
            continue;
        }
        if symbol.animated_span >= 2 {
            ground.insert(key.clone(), symbol.animated_span);
        } else if symbol.random_pose_as3 || symbol.mirror_flip_frame > 0 {
            static_keys.insert(key.clone());
        }
    }
    let mut item_loop = Some(1usize);
    for (key, symbol) in &symbols {
        if key != crate::background::KEY && !ignored.contains(key) && !static_keys.contains(key) {
            item_loop = item_loop
                .zip(geometry::period(&symbol.schedule, max))
                .and_then(|(a, b)| geometry::lcm(a, b));
        }
    }
    let mut blink = symbols
        .get("armor_head")
        .and_then(|s| geometry::period(&s.schedule, max));
    let precomputed = &prepared["precomputed_loop"];
    if precomputed["frame_count"].as_u64().is_some() {
        item_loop = precomputed["detected_item_loop"]
            .as_u64()
            .map(|n| n as usize);
        blink = precomputed["detected_blink_frames"]
            .as_u64()
            .map(|n| n as usize);
        if let Some(keys) = precomputed["static_keys"].as_array() {
            for key in keys {
                static_keys.insert(key.as_str().context("invalid static key")?.into());
            }
        }
        if let Some(spans) = precomputed["ground_animate"].as_object() {
            for (key, span) in spans {
                if let Some(n) = span.as_u64().filter(|n| *n >= 2) {
                    ground.insert(key.clone(), n as usize);
                }
            }
        }
    }
    if symbols.contains_key(crate::background::KEY) {
        item_loop = item_loop.zip(prepared["background_period"].as_u64().map(|n| n as usize))
            .and_then(|(a,b)| geometry::lcm(a,b));
    }
    let detected = item_loop.zip(blink).and_then(|(a, b)| {
        if a > 0 && b > 0 {
            b.div_ceil(a).checked_mul(a)
        } else {
            None
        }
    });
    let natural_count = if complete {
        precomputed["frame_count"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or_else(|| detected.unwrap_or(max).min(max))
    } else {
        1
    };
    let count = natural_count.min(config.component_frame_cap);
    let loop_status = crate::metadata::loop_status(count, item_loop, blink, ground.values().copied());
    ensure!(count > 0, "empty animation");
    let select = |key: &str, index: usize| -> Result<usize> {
        let source = if let Some(span) = ground.get(key) {
            geometry::pingpong(index, *span)
        } else if static_keys.contains(key) {
            0
        } else if ignored.contains(key) && blink.is_some_and(|n| n > 0) {
            index.min(blink.unwrap() - 1)
        } else {
            index
        };
        ensure!(
            symbols.get(key).is_some_and(|s| source < s.schedule.len()),
            "source frame outside exported schedule for {key}"
        );
        Ok(source)
    };
    let aliases: BTreeMap<String, String> = serde_json::from_value(prepared["aliases"].clone())?;
    let mut layers = geometry::layers(
        &aliases,
        string(&prepared, "weapon_type")?,
        string(settings, "facing")?,
    );
    let mut tight = None;
    for layer in &layers {
        for index in 0..count {
            let source = select(&layer.symbol_key, index)?;
            let hash = &symbols[&layer.symbol_key].schedule[source];
            if let Some(b) = state_bounds(&results[hash], &parts[&layer.symbol_key]) {
                let b = transformed_bounds(b, layer.matrix);
                tight = Some(tight.map(|a| bounds::union(a, b)).unwrap_or(b));
            }
        }
    }
    let [x, y, w, h] = tight.context("character composition produced no visible layers")?;
    let output = settings["output_size"]
        .as_u64()
        .context("missing output size")?;
    let padding = settings["padding"].as_u64().context("missing padding")?;
    let units = w.max(h) / (output - 2 * padding) as f64;
    let margin = padding as f64 * units;
    let layout = crate::presentation::normalize(settings["view"].as_str().unwrap_or("character"), &settings["presentation"])?;
    let viewbox = crate::presentation::viewbox(&layout, [x - margin, y - margin, w + 2.0 * margin, h + 2.0 * margin]);
    if symbols.contains_key(crate::background::KEY) {
        let scale = (viewbox[2] / 550.0).max(viewbox[3] / 350.0);
        layers.insert(0, geometry::Layer { name:crate::background::KEY.into(), symbol_key:crate::background::KEY.into(), darken:false,
            matrix:[scale,0.0,0.0,scale,viewbox[0]+(viewbox[2]-550.0*scale)/2.0+5.0*scale,viewbox[1]+(viewbox[3]-350.0*scale)/2.0] });
    }
    let frame_rate = if let Some(rate) = prepared["frame_rate"].as_f64() {
        rate
    } else {
        let record = &prepared["character_renderer"];
        let bytes = store
            .get(&config.source_bucket, string(record, "key")?)
            .await?
            .context("missing characterB")?;
        ensure!(
            crate::sha256(&bytes) == string(record, "sha256")?,
            "characterB checksum mismatch"
        );
        crate::swf::Swf::parse(&bytes)?.frame_rate
    };
    ensure!(
        frame_rate > 0.0 && frame_rate <= 1000.0,
        "invalid frame rate"
    );
    let timestamps: Vec<_> = (0..=count)
        .map(|i| (i as f64 * 1000.0 / frame_rate).round_ties_even() as u64)
        .collect();
    let durations: Vec<_> = timestamps.windows(2).map(|w| w[1] - w[0]).collect();
    ensure!(durations.iter().all(|n| *n > 0), "invalid frame duration");
    let fields = &prepared["fields"];
    let colors: BTreeMap<_, _> = fields
        .as_object()
        .context("missing fields")?
        .iter()
        .filter(|(k, _)| k.starts_with("intColor"))
        .collect();
    let mut tasks = Vec::new();
    let mut seen = BTreeSet::new();
    let mut frames = Vec::new();
    for (index, duration) in durations.iter().enumerate() {
        let mut ids = Vec::new();
        for (layer_index, layer) in layers.iter().enumerate() {
            let source = select(&layer.symbol_key, index)?;
            let hash = &symbols[&layer.symbol_key].schedule[source];
            let raster_bounds = json!({"policy":aqw_component_raster::region::POLICY,"bounds":state_bounds(&results[hash], &parts[&layer.symbol_key])});
            let identity = crate::digest(
                &json!({"renderer_version":config.renderer_version,"raster_size":settings["raster_size"],"output_size":output,"viewbox":viewbox,"facing":settings["facing"],"weapon_type":prepared["weapon_type"],"colors":colors,"symbol_key":layer.symbol_key,"layer_name":layer.name,"layer_index":layer_index,"matrix":layer.matrix,"darken":layer.darken,"state_signature":hash,"part":crate::digest(&parts[&layer.symbol_key])?}),
            )?;
            let identity = crate::digest(&(&identity, &raster_bounds))?;
            if seen.insert(identity.clone()) {
                tasks.push(json!({"task_id":identity,"symbol_key":layer.symbol_key,"layer_name":layer.name,"layer_index":layer_index,"matrix":layer.matrix,"darken":layer.darken,"svg_key":plan.states[hash].state.svg_key,"source_frame":source+1,"state_signature":hash,"raster_bounds":raster_bounds}));
            }
            ids.push(identity);
        }
        frames.push(json!({"number":index+1,"duration_ms":duration,"layers":ids}));
    }
    let compositions = component_compositions(&frames)?;
    let mut warnings = prepared["warnings"].as_array().cloned().unwrap_or_default();
    if count < natural_count {
        warnings
            .push(format!("Component raster cap limits this job to {count} output frames").into());
    }
    if !static_keys.is_empty() {
        warnings.push(
            format!(
                "Froze random-pose layers: {}",
                static_keys.iter().cloned().collect::<Vec<_>>().join(", ")
            )
            .into(),
        );
    }
    if !ground.is_empty() {
        warnings.push(
            format!(
                "Ping-pong random-pose layers: {}",
                ground.keys().cloned().collect::<Vec<_>>().join(", ")
            )
            .into(),
        );
    }
    if complete && item_loop.is_none() {
        warnings.push("At least one item timeline did not repeat within the frame cap".into());
    }
    let presentation_layers = if layout["background"] == true || layout["info"] == true {
        let raster = settings["raster_size"].as_u64().unwrap();
        let canvas = crate::presentation::canvas(viewbox, raster as u32, output as u32, raster <= output * 2);
        Some(crate::charpage::prepare(store, &config.work_bucket, job, &prepared["fields"], settings, &layout, canvas).await?)
    } else { None };
    let manifest_key = format!("jobs/{job}/prepare/manifest.json");
    let batches = batches(count, config.frames_per_lambda);
    let compositions_per_batch = compositions
        .len()
        .div_ceil(config.compose_concurrency.max(1));
    let component_batches = composition_batches(compositions.len(), config.compose_concurrency);
    let mut manifest = prepared.clone();
    for (key,value) in json!({"presentation_layers":presentation_layers,"frame_count":count,"loop_status":loop_status,"frame_rate":frame_rate,"viewbox":viewbox,"frame_durations":durations,"parts":parts,"static_keys":static_keys,"ground_animate":ground,"all_color_rules":all_rules,"batches":batches,"component_batches":component_batches,"component_pipeline":true,"component_compose_schema":2,"component_raster_space":if settings["raster_size"].as_u64().unwrap()<=output*2 {"output"} else {"raster"},"component_tasks":tasks,"component_frames":frames,"component_compositions":compositions,"warnings":warnings,"detected_loop":detected,"detected_blink_frames":blink,"ignored_loop_keys":ignored,"source_bundles":{},"sources":[]}).as_object().unwrap() {manifest[key]=value.clone();}
    store::write(store, &config.work_bucket, &manifest_key, &manifest, false).await?;
    crate::log(
        "prepare_profile",
        json!({"job_id":job,"frame_count":count,"unique_compositions":compositions.len(),"deduplicated_frames":count-compositions.len(),"component_batch_count":component_batches.len(),"compositions_per_batch":compositions_per_batch,"compose_concurrency":config.compose_concurrency,"unique_component_tasks":tasks.len(),"unique_bounds_states":results.len(),"detected_item_loop":item_loop,"detected_blink_frames":blink,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(
        json!({"schema_version":1,"job_id":job,"cache_hit":false,"render_hash":prepared["render_hash"],"final_key":prepared["final_key"],"manifest_key":manifest_key,"batches":batches,"component_batches":component_batches,"component_pipeline":true,"component_task_indices":(0..tasks.len()).collect::<Vec<_>>(),"frame_count":count,"unique_composition_count":compositions.len(),"compositions_per_batch":compositions_per_batch}),
    )
}

#[cfg(test)]
mod tests {
    use super::{component_compositions, composition_batches};
    use serde_json::json;

    #[test]
    fn authored_alpha_offsets_keep_declared_framing() {
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="1000"><g transform="matrix(1 0 0 1 0 0)" opacity="0"><rect width="10" height="10"/></g></svg>"#;
        let task = crate::model::ProbeTask::new(
            crate::model::StateRef::new(bytes),
            crate::model::ProbeConfig::new(1.0),
        )
        .unwrap();
        let mut bounds = crate::bounds::probe(bytes, &task).unwrap();
        assert_eq!(super::state_bounds(&bounds, &json!({})), None);
        assert_eq!(
            super::state_bounds(
                &bounds,
                &json!({"placement_colors":{"1,2":{"alpha_add":1}}})
            ),
            Some([0.0, 0.0, 1000.0, 1000.0])
        );
        bounds.bounds = Some([-5.0, -5.0, 1010.0, 1010.0]);
        assert_eq!(super::state_bounds(&bounds, &json!({"placement_colors":{"1,2":{"alpha_add":1}}})), bounds.bounds,
            "retain measured filter extent and probe padding outside the declared page");
    }

    #[test]
    fn groups_exact_recipes_globally_without_using_duration() {
        let frames = vec![
            json!({"number":1,"duration_ms":40,"layers":["a","b"]}),
            json!({"number":2,"duration_ms":80,"layers":["b","a"]}),
            json!({"number":11,"duration_ms":120,"layers":["a","b"]}),
        ];
        let groups = component_compositions(&frames).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["canonical_frame"], 1);
        assert_eq!(groups[0]["logical_frames"], json!([1, 11]));
        assert_eq!(groups[1]["canonical_frame"], 2);
        assert_eq!(groups[1]["logical_frames"], json!([2]));
    }

    #[test]
    fn adaptively_batches_consecutive_unique_compositions() {
        assert_eq!(
            composition_batches(5, 2),
            vec![
                json!({"index":0,"composition_start":0,"composition_end":2}),
                json!({"index":1,"composition_start":3,"composition_end":4}),
            ]
        );
        assert_eq!(composition_batches(40, 40).len(), 40);
        assert_eq!(composition_batches(41, 40).len(), 21);
        assert_eq!(composition_batches(120, 40).len(), 40);
        assert_eq!(composition_batches(400, 40).len(), 40);
    }
}
