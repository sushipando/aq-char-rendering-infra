use crate::{
    config::Config,
    contract::{integer, string},
    model::FINALIZE_POLICY,
    store::{self, Store},
    webp::{self, FrameInfo, MAX_DURATION, REPLACE_NO_DISPOSE},
};
use anyhow::{ensure, Context, Result};
use futures::{stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// The compose contract has fixed replacement/no-disposal semantics. Reject
/// unknown fields so a future blend/disposal extension cannot be silently merged.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalFrame {
    frame: u64,
    webp_key: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    canvas_width: u32,
    canvas_height: u32,
    duration: u32,
    sha256: String,
    bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EncodedImage {
    sha256: String,
    bytes: usize,
    size: [u32; 2],
}

impl LogicalFrame {
    fn image(&self) -> EncodedImage {
        EncodedImage {
            sha256: self.sha256.clone(),
            bytes: self.bytes,
            size: [self.width, self.height],
        }
    }
}

#[derive(Clone, Debug)]
struct EncodedRun {
    webp_key: String,
    image: EncodedImage,
    frame: FrameInfo,
}

fn unique_webps(frames: &BTreeMap<u64, LogicalFrame>) -> Result<BTreeMap<String, EncodedImage>> {
    let mut unique = BTreeMap::new();
    for frame in frames.values() {
        let key = frame.webp_key.clone();
        let expected = frame.image();
        if let Some(existing) = unique.insert(key.clone(), expected.clone()) {
            ensure!(
                existing == expected,
                "encoded WebP key has conflicting checksum, size, or dimensions: {key}"
            );
        }
    }
    Ok(unique)
}

/// Validate every logical record before grouping. Equality is encoded-content
/// identity, not S3 key or recipe identity; all payloads are checked on download.
fn encoded_runs(
    frames: &BTreeMap<u64, LogicalFrame>,
    canvas: [u32; 2],
    durations: &[u32],
) -> Result<Vec<EncodedRun>> {
    ensure!(
        !frames.is_empty() && frames.len() == durations.len(),
        "encoded frame set is incomplete"
    );
    let mut runs: Vec<EncodedRun> = Vec::new();
    for (index, (number, logical)) in frames.iter().enumerate() {
        ensure!(
            *number == index as u64 + 1 && logical.frame == *number,
            "encoded frame sequence is incomplete"
        );
        ensure!(
            [logical.canvas_width, logical.canvas_height] == canvas,
            "frames do not share one canvas"
        );
        ensure!(
            (1..=MAX_DURATION).contains(&logical.duration) && logical.duration == durations[index],
            "encoded duration differs from prepare"
        );
        ensure!(
            (1..=canvas[0]).contains(&logical.width) && (1..=canvas[1]).contains(&logical.height),
            "invalid encoded frame dimensions"
        );
        ensure!(
            logical.x <= canvas[0] - logical.width
                && logical.y <= canvas[1] - logical.height
                && logical.x % 2 == 0
                && logical.y % 2 == 0,
            "invalid encoded frame placement"
        );
        ensure!(
            !logical.webp_key.is_empty()
                && logical.bytes > 0
                && logical.sha256.len() == 64
                && logical.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid encoded frame identity"
        );
        let image = logical.image();
        let mut remaining = logical.duration;
        if let Some(previous) = runs.last_mut() {
            // Decoder-specific minimum-duration clamping makes <=10ms frames
            // unsafe to combine. Do not create a tiny overflow fragment either.
            if logical.duration > 10
                && previous.frame.duration > 10
                && previous.image == image
                && previous.frame.x == logical.x
                && previous.frame.y == logical.y
                && previous.frame.flags == REPLACE_NO_DISPOSE
            {
                let added = remaining.min(MAX_DURATION - previous.frame.duration);
                previous.frame.duration += added;
                remaining -= added;
                if (1..=10).contains(&remaining) {
                    previous.frame.duration -= 11 - remaining;
                    remaining = 11;
                }
            }
        }
        if remaining > 0 {
            runs.push(EncodedRun {
                webp_key: logical.webp_key.clone(),
                image,
                frame: FrameInfo {
                    x: logical.x,
                    y: logical.y,
                    width: logical.width,
                    height: logical.height,
                    duration: remaining,
                    flags: REPLACE_NO_DISPOSE,
                },
            });
        }
    }
    ensure!(
        runs.iter().map(|r| r.frame.duration as u64).sum::<u64>()
            == durations.iter().map(|d| *d as u64).sum::<u64>(),
        "encoded run duration mismatch"
    );
    Ok(runs)
}

fn validate_schedule(
    bytes: &[u8],
    canvas: [u32; 2],
    frames: &[FrameInfo],
    allow_still: bool,
) -> Result<()> {
    let actual = webp::inspect(bytes)?;
    ensure!(actual.canvas == canvas, "WebP canvas mismatch");
    if actual.loop_count.is_none() {
        ensure!(
            allow_still
                && frames.len() == 1
                && frames[0].x == 0
                && frames[0].y == 0
                && [frames[0].width, frames[0].height] == canvas,
            "WebP animation timing was lost"
        );
    } else {
        ensure!(
            actual.loop_count == Some(0) && actual.background == Some(0),
            "WebP loop/background mismatch"
        );
        ensure!(
            actual.frames == frames,
            "WebP physical frame schedule/placement/flags mismatch"
        );
    }
    Ok(())
}

/// Compatibility helper for full-canvas schedules; count is the physical count.
pub fn validate_webp(
    bytes: &[u8],
    count: usize,
    canvas: [u32; 2],
    durations: &[u32],
) -> Result<()> {
    ensure!(
        count > 0
            && count == durations.len()
            && durations.iter().all(|d| (1..=MAX_DURATION).contains(d)),
        "invalid WebP duration schedule"
    );
    let frames: Vec<_> = durations
        .iter()
        .map(|duration| FrameInfo {
            x: 0,
            y: 0,
            width: canvas[0],
            height: canvas[1],
            duration: *duration,
            flags: REPLACE_NO_DISPOSE,
        })
        .collect();
    validate_schedule(bytes, canvas, &frames, count == 1)
}

pub async fn finalize(store: &dyn Store, config: &Config, event: &Value) -> Result<Value> {
    let started = Instant::now();
    let job = string(event, "job_id")?;
    let prepared: Value =
        store::read(store, &config.work_bucket, string(event, "manifest_key")?).await?;
    ensure!(
        prepared["job_id"] == job,
        "prepare manifest belongs to another job"
    );
    if prepared["settings"]["output_format"] == "avif" {
        return crate::avif::finalize(store, config, event, &prepared).await;
    }
    ensure!(prepared["settings"]["output_format"].is_null() || prepared["settings"]["output_format"] == "webp", "invalid output format");
    let count = integer(&prepared, "frame_count", 1, 2000)? as usize;
    let batches: Vec<Value> = stream::iter(
        event["render_results"]
            .as_array()
            .context("missing rendered batches")?,
    )
    .map(|result| async move {
        let batch: Value = store::read(
            store,
            &config.work_bucket,
            string(result, "batch_manifest_key")?,
        )
        .await?;
        ensure!(batch["job_id"] == job, "batch belongs to another job");
        Ok::<_, anyhow::Error>(batch)
    })
    .buffered(config.download_concurrency)
    .try_collect()
    .await?;
    let mut frames = BTreeMap::new();
    for batch in batches {
        for frame in batch["frames"].as_array().context("missing frames")? {
            let frame: LogicalFrame =
                serde_json::from_value(frame.clone()).context("invalid encoded frame record")?;
            let number = frame.frame;
            ensure!(
                (1..=count as u64).contains(&number),
                "encoded frame number out of range"
            );
            ensure!(
                frames.insert(number, frame).is_none(),
                "duplicate encoded frame"
            );
        }
    }
    ensure!(frames.len() == count, "encoded frame set is incomplete");
    let first = frames.values().next().context("no frames")?;
    let canvas = [first.canvas_width, first.canvas_height];
    ensure!(
        canvas.iter().all(|n| (1..=4096).contains(n)),
        "invalid WebP canvas"
    );
    let durations: Vec<u32> = serde_json::from_value(prepared["frame_durations"].clone())
        .context("invalid prepared frame durations")?;
    let runs = encoded_runs(&frames, canvas, &durations)?;
    let temporary = tempfile::tempdir()?;
    let unique = unique_webps(&frames)?;
    let unique_count = unique.len();
    let download_started = Instant::now();
    let downloaded: Vec<_> = stream::iter(unique.into_iter().enumerate())
        .map(|(index, (key, expected))| {
            let root = temporary.path();
            async move {
                let bytes = store
                    .get(&config.work_bucket, &key)
                    .await?
                    .context("missing encoded WebP")?;
                ensure!(
                    crate::sha256(&bytes) == expected.sha256,
                    "encoded WebP checksum mismatch"
                );
                ensure!(
                    bytes.len() == expected.bytes,
                    "encoded WebP byte length mismatch"
                );
                let image = webp::inspect(&bytes)?;
                // Current cwebp output is metadata-free. ICC/orientation metadata
                // needs an explicit global policy, not different behavior between
                // webpmux's multi-frame path and the single-run container path.
                ensure!(
                    !image.has_metadata,
                    "encoded-frame color/orientation metadata is unsupported"
                );
                ensure!(
                    image.loop_count.is_none() && image.canvas == expected.size,
                    "encoded WebP is animated or has unexpected dimensions"
                );
                let path = root.join(format!("unique-{index:06}.webp"));
                tokio::fs::write(&path, bytes).await?;
                Ok::<_, anyhow::Error>((key, path))
            }
        })
        .buffered(config.download_concurrency)
        .try_collect()
        .await?;
    let downloaded: BTreeMap<_, _> = downloaded.into_iter().collect();
    let download_ms = download_started.elapsed().as_secs_f64() * 1000.0;
    let mux_started = Instant::now();
    let output = temporary.path().join("result.webp");
    let mut command = tokio::process::Command::new(crate::config::env(
        "CHAR_RENDER_WEBPMUX",
        "/usr/bin/webpmux",
    ));
    for run in &runs {
        let path = downloaded
            .get(&run.webp_key)
            .context("encoded WebP was not downloaded")?;
        command.arg("-frame").arg(path).arg(format!(
            "+{}+{}+{}+0-b",
            run.frame.duration, run.frame.x, run.frame.y
        ));
    }
    let bytes = if count > 1 && runs.len() == 1 {
        let still = tokio::fs::read(&downloaded[&runs[0].webp_key]).await?;
        webp::single_frame_animation(&still, canvas, runs[0].frame)?
    } else {
        let result = tokio::time::timeout(
            Duration::from_secs(240),
            command
                .args(["-loop", "0", "-bgcolor", "0,0,0,0", "-o"])
                .arg(&output)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("WebP mux timed out")??;
        ensure!(
            result.status.success(),
            "WebP mux failed: {}",
            String::from_utf8_lossy(&result.stderr[result.stderr.len().saturating_sub(2000)..])
        );
        tokio::fs::read(&output).await?
    };
    let xmp = crate::metadata::packet(&prepared)?;
    let mut bytes = webp::with_xmp(&bytes, &xmp)?;
    crate::metadata::complete_stats(&mut bytes, &xmp, &prepared)?;
    let mux_ms = mux_started.elapsed().as_secs_f64() * 1000.0;
    validate_schedule(
        &bytes,
        canvas,
        &runs.iter().map(|r| r.frame).collect::<Vec<_>>(),
        count == 1,
    )?;
    let final_key = string(&prepared, "final_key")?;
    let result = json!({"url":format!("{}/{final_key}",config.public_base_url),"frame_count":count,"logical_frame_count":count,"physical_frame_count":runs.len(),"merged_frame_count":count-runs.len(),"finalize_policy":FINALIZE_POLICY,"metadata_job_id":job,"metadata_policy":crate::metadata::POLICY,"width":canvas[0],"height":canvas[1],"duration_ms":durations.iter().map(|d|*d as u64).sum::<u64>(),"bytes":bytes.len(),"cache_hit":false,"render_hash":prepared["render_hash"],"final_key":final_key});
    // Single PutObject is atomic; publish cache metadata only after validation
    // and a complete successful image write. No temporary S3 copy required.
    store
        .put(&config.work_bucket, final_key, bytes, "image/webp", false)
        .await?;
    store::write(
        store,
        &config.work_bucket,
        &format!("{final_key}.json"),
        &result,
        false,
    )
    .await?;
    crate::log(
        "finalize_profile",
        json!({"job_id":job,"frame_count":count,"logical_frame_count":count,"physical_frame_count":runs.len(),"merged_frame_count":count-runs.len(),"unique_webp_count":unique_count,"finalize_policy":FINALIZE_POLICY,"animation_duration_ms":result["duration_ms"],"download_ms":download_ms,"mux_ms":mux_ms,"output_bytes":result["bytes"],"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(number: u64, content: &str, duration: u32) -> LogicalFrame {
        LogicalFrame {
            frame: number,
            webp_key: format!("{content}.webp"),
            sha256: crate::sha256(content.as_bytes()),
            bytes: 123,
            x: 0,
            y: 0,
            width: 8,
            height: 6,
            canvas_width: 8,
            canvas_height: 6,
            duration,
        }
    }
    fn run(values: Vec<LogicalFrame>) -> Result<Vec<EncodedRun>> {
        let durations: Vec<_> = values.iter().map(|f| f.duration).collect();
        encoded_runs(
            &values.into_iter().map(|f| (f.frame, f)).collect(),
            [8, 6],
            &durations,
        )
    }

    #[test]
    fn collapses_alias_records_to_one_download_and_merges_only_adjacent_runs() {
        let frames = vec![
            frame(1, "a", 41),
            frame(2, "a", 42),
            frame(3, "b", 41),
            frame(4, "a", 42),
        ];
        assert_eq!(
            unique_webps(&frames.iter().map(|f| (f.frame, f.clone())).collect())
                .unwrap()
                .len(),
            2
        );
        let runs = run(frames).unwrap();
        assert_eq!(
            runs.iter().map(|r| r.frame.duration).collect::<Vec<_>>(),
            [83, 41, 42]
        );
        assert_eq!(
            runs.iter().map(|r| r.webp_key.as_str()).collect::<Vec<_>>(),
            ["a.webp", "b.webp", "a.webp"]
        );
    }

    #[test]
    fn same_payload_different_keys_merges_but_placement_or_dimensions_do_not() {
        let a = frame(1, "a", 40);
        let mut b = frame(2, "a", 50);
        b.webp_key = "another-key.webp".into();
        assert_eq!(run(vec![a.clone(), b.clone()]).unwrap().len(), 1);
        let mut a = a;
        a.width = 6;
        b.width = 6;
        b.x = 2;
        assert_eq!(run(vec![a.clone(), b.clone()]).unwrap().len(), 2);
        b.x = 0;
        b.width = 8;
        assert_eq!(run(vec![a, b]).unwrap().len(), 2);
    }

    #[test]
    fn split_duration_limits_without_overflow_or_new_tiny_fragments() {
        let runs = run(vec![
            frame(1, "a", MAX_DURATION - 5),
            frame(2, "a", 20),
            frame(3, "a", MAX_DURATION),
        ])
        .unwrap();
        assert_eq!(
            runs.iter().map(|r| r.frame.duration).collect::<Vec<_>>(),
            [MAX_DURATION, MAX_DURATION, 15]
        );
        let runs = run(vec![frame(1, "a", MAX_DURATION - 19), frame(2, "a", 20)]).unwrap();
        assert_eq!(
            runs.iter().map(|r| r.frame.duration).collect::<Vec<_>>(),
            [MAX_DURATION - 10, 11]
        );
        assert!(run(vec![frame(1, "a", MAX_DURATION + 1)]).is_err());
        assert_eq!(
            run(vec![frame(1, "a", 5), frame(2, "a", 5)]).unwrap().len(),
            2
        );
    }

    #[test]
    fn still_all_identical_and_all_unique_schedules() {
        assert_eq!(run(vec![frame(1, "a", 42)]).unwrap()[0].frame.duration, 42);
        let runs = run((1..=120)
            .map(|n| frame(n, "a", if n % 3 == 0 { 41 } else { 42 }))
            .collect())
        .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].frame.duration, 5000);
        assert_eq!(
            run((1..=120).map(|n| frame(n, &n.to_string(), 42)).collect())
                .unwrap()
                .len(),
            120
        );
    }

    #[test]
    fn validates_every_logical_frame_before_merging() {
        assert!(run(vec![frame(1, "a", 42), frame(3, "a", 42)]).is_err());
        let mut b = frame(2, "a", 42);
        b.canvas_height = 5;
        assert!(run(vec![frame(1, "a", 42), b]).is_err());
        let mut b = frame(2, "a", 42);
        b.width = 7;
        b.x = 1;
        assert!(run(vec![frame(1, "a", 42), b]).is_err());
        let frames = BTreeMap::from([(1, frame(1, "a", 42)), (2, frame(2, "a", 42))]);
        assert!(encoded_runs(&frames, [8, 6], &[42, 41]).is_err());
        assert!(encoded_runs(&frames, [8, 6], &[42]).is_err());
        assert!(encoded_runs(&frames, [8, 6], &[42, 42, 42]).is_err());
    }

    #[test]
    fn rejects_conflicting_metadata_for_one_key_and_unknown_frame_semantics() {
        for field in ["hash", "size", "dimensions"] {
            let a = frame(1, "a", 42);
            let mut b = frame(2, "a", 42);
            match field {
                "hash" => b.sha256 = "b".repeat(64),
                "size" => b.bytes += 1,
                _ => b.width -= 1,
            }
            assert!(unique_webps(&BTreeMap::from([(1, a), (2, b)])).is_err());
        }
        let original = json!({"frame":1,"webp_key":"a.webp","sha256":"a".repeat(64),"bytes":123,"x":0,"y":0,"width":8,"height":6,"canvas_width":8,"canvas_height":6,"duration":42});
        for field in ["blend", "dispose", "flags"] {
            let mut value = original.clone();
            value[field] = 1.into();
            assert!(serde_json::from_value::<LogicalFrame>(value).is_err());
        }
    }
}
