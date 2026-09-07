//! Bounded original-RGBA handoff into a single temporal AV1 encoder process.
use crate::{
    config::Config,
    contract::{integer, string},
    store::{self, Store},
};
use anyhow::{ensure, Context, Result};
use futures::{stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::{
    collections::BTreeMap,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

pub const POLICY: &str = "avif-rgba-v1-libavif1.4.2-aom3.14.1-444-alpha100-lag0-j2";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    frame: usize,
    rgba_key: String,
    #[serde(default)]
    rgba_compression: Option<String>,
    #[serde(default)]
    raw_sha256: Option<String>,
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

fn schedule(batches: &[Value], count: usize, durations: &[u32]) -> Result<Vec<Frame>> {
    ensure!(
        count > 0 && count <= 2000 && durations.len() == count,
        "invalid RGBA schedule"
    );
    let mut frames = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for batch in batches {
        ensure!(
            batch["schema_version"] == 2,
            "AVIF requires original RGBA batch schema 2"
        );
        for value in batch["frames"].as_array().context("missing RGBA frames")? {
            let frame: Frame =
                serde_json::from_value(value.clone()).context("invalid RGBA frame record")?;
            ensure!(
                (1..=count).contains(&frame.frame),
                "RGBA frame out of range"
            );
            let identity = (
                frame.sha256.clone(),
                frame.bytes,
                frame.width,
                frame.height,
                frame.rgba_compression.clone(),
                frame.raw_sha256.clone(),
            );
            if let Some(previous) = identities.insert(frame.rgba_key.clone(), identity.clone()) {
                ensure!(previous == identity, "conflicting RGBA object metadata");
            }
            ensure!(
                frames.insert(frame.frame, frame).is_none(),
                "duplicate RGBA frame"
            );
        }
    }
    ensure!(frames.len() == count, "incomplete RGBA frame set");
    let first = &frames[&1];
    let canvas = [first.width, first.height];
    ensure!(
        canvas.iter().all(|v| (1..=4096).contains(v)),
        "invalid RGBA canvas"
    );
    let size = canvas[0] as usize * canvas[1] as usize * 4;
    let mut runs: Vec<Frame> = Vec::new();
    for (number, frame) in frames {
        ensure!(
            frame.x == 0
                && frame.y == 0
                && [frame.width, frame.height] == canvas
                && [frame.canvas_width, frame.canvas_height] == canvas
                && (1..=size).contains(&frame.bytes),
            "RGBA must be tightly packed full-canvas straight-alpha RGBA8"
        );
        ensure!(
            frame.duration > 0 && frame.duration == durations[number - 1],
            "RGBA duration differs from prepare"
        );
        ensure!(
            !frame.rgba_key.is_empty()
                && frame.sha256.len() == 64
                && frame.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid RGBA identity"
        );
        match frame.rgba_compression.as_deref().unwrap_or("none") {
            "none" => ensure!(
                frame.bytes == size && frame.raw_sha256.is_none(),
                "invalid uncompressed RGBA identity"
            ),
            "zstd" => ensure!(
                frame
                    .raw_sha256
                    .as_ref()
                    .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
                "missing original RGBA checksum"
            ),
            _ => anyhow::bail!("unsupported RGBA compression"),
        }
        // Merge only the same verified object; distinct alias keys are still
        // fetched and checked. Keep tiny durations separate for player behavior.
        if let Some(previous) = runs.last_mut() {
            if previous.rgba_key == frame.rgba_key && previous.duration > 10 && frame.duration > 10
            {
                if let Some(duration) = previous.duration.checked_add(frame.duration) {
                    previous.duration = duration;
                    continue;
                }
            }
        }
        runs.push(frame);
    }
    // libavif emits a still container for one physical image, even without
    // FLAG_SINGLE. Keep two samples when a logical animation is all identical.
    if count > 1 && runs.len() == 1 {
        let mut tail = runs[0].clone();
        tail.duration = durations[count - 1];
        runs[0].duration -= tail.duration;
        runs.push(tail);
    }
    Ok(runs)
}

/// Stream decompression straight into the encoder pipe. Keep downloaded frames
/// compressed; no additional full RGBA frame is allocated in Rust for zstd.
async fn write_pixels(
    input: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: &Frame,
    bytes: &[u8],
) -> Result<f64> {
    if frame.rgba_compression.as_deref() != Some("zstd") {
        input.write_all(bytes).await?;
        return Ok(0.0);
    }
    let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
    // Level 1 uses a much smaller window. Bound even malformed object headers.
    decoder.window_log_max(23)?;
    let expected = frame.width as usize * frame.height as usize * 4;
    let mut buffer = [0u8; 65536];
    let mut total = 0;
    let mut hash = Sha256::new();
    let mut elapsed = 0.0;
    loop {
        let started = Instant::now();
        let count = decoder.read(&mut buffer)?;
        total += count;
        ensure!(total <= expected, "decompressed RGBA exceeds canvas size");
        hash.update(&buffer[..count]);
        elapsed += started.elapsed().as_secs_f64() * 1000.0;
        if count == 0 {
            break;
        }
        input.write_all(&buffer[..count]).await?;
    }
    ensure!(
        total == expected && Some(hex::encode(hash.finalize())) == frame.raw_sha256,
        "decompressed RGBA length/checksum mismatch"
    );
    Ok(elapsed)
}

pub async fn finalize(
    store: &dyn Store,
    config: &Config,
    event: &Value,
    prepared: &Value,
) -> Result<Value> {
    let started = Instant::now();
    let job = string(event, "job_id")?;
    ensure!(prepared["job_id"] == job, "prepare belongs to another job");
    let count = integer(prepared, "frame_count", 1, 2000)? as usize;
    let durations: Vec<u32> = serde_json::from_value(prepared["frame_durations"].clone())?;
    let mut batches = Vec::new();
    for result in event["render_results"]
        .as_array()
        .context("missing rendered batches")?
    {
        let batch: Value = store::read(
            store,
            &config.work_bucket,
            string(result, "batch_manifest_key")?,
        )
        .await?;
        ensure!(batch["job_id"] == job, "RGBA batch belongs to another job");
        batches.push(batch);
    }
    let runs = schedule(&batches, count, &durations)?;
    let width = runs[0].width;
    let height = runs[0].height;
    let settings = &prepared["settings"];
    let quality = integer(settings, "avif_quality", 0, 100)? as u32;
    let speed = integer(settings, "avif_speed", 0, 10)? as u32;
    let final_key = string(prepared, "final_key")?;
    ensure!(
        final_key.ends_with(".avif"),
        "AVIF final key has wrong extension"
    );
    let temporary = tempfile::tempdir()?;
    let output = temporary.path().join("result.avif");
    let xmp = temporary.path().join("render.xmp");
    let packet = crate::metadata::packet(prepared)?;
    tokio::fs::write(&xmp, &packet).await?;
    let stderr = std::fs::File::create(temporary.path().join("encoder.stderr"))?;
    let stdout = std::fs::File::create(temporary.path().join("encoder.json"))?;
    let mut child = tokio::process::Command::new(crate::config::env(
        "CHAR_RENDER_AVIF_RGBA",
        "/opt/avif/avif-rgba",
    ))
    .arg(&output)
    .arg(&xmp)
    .stdin(Stdio::piped())
    .stderr(stderr)
    .stdout(stdout)
    .kill_on_drop(true)
    .spawn()
    .context("start AVIF sequence encoder")?;
    let mut input = child.stdin.take().context("missing encoder stdin")?;
    let mut download_ms = 0.0;
    let mut decompression_ms = 0.0;
    let mut download_bytes = 0usize;
    let encoding = async {
        for n in [
            width,
            height,
            runs.len() as u32,
            quality,
            speed,
            u32::from(settings["webp_lossless"] == true),
            2,
            u32::from(count > 1),
        ] {
            input.write_all(&n.to_le_bytes()).await?;
        }
        // At most two downloads in flight. Never retain the full raw animation
        // in RAM or /tmp. Pipe backpressure bounds pending encoder input too.
        let mut downloads = stream::iter(runs.iter())
            .map(|frame| async move {
                let start = Instant::now();
                let bytes = store
                    .get(&config.work_bucket, &frame.rgba_key)
                    .await?
                    .context("missing original RGBA")?;
                ensure!(
                    bytes.len() == frame.bytes && crate::sha256(&bytes) == frame.sha256,
                    "RGBA checksum/length mismatch"
                );
                Ok::<_, anyhow::Error>((frame, bytes, start.elapsed().as_secs_f64() * 1000.0))
            })
            .buffered(2);
        while let Some(frame) = downloads.next().await {
            let (frame, bytes, elapsed) = frame?;
            download_ms += elapsed;
            download_bytes += bytes.len();
            input.write_all(&frame.duration.to_le_bytes()).await?;
            decompression_ms += write_pixels(&mut input, frame, &bytes).await?;
        }
        input.shutdown().await?;
        drop(input);
        ensure!(
            child.wait().await?.success(),
            "AVIF sequence encoder failed"
        );
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(840), encoding).await;
    if !matches!(&result, Ok(Ok(()))) {
        let _ = child.kill().await;
        let error = tokio::fs::read(temporary.path().join("encoder.stderr"))
            .await
            .unwrap_or_default();
        let details = String::from_utf8_lossy(&error[error.len().saturating_sub(2000)..]);
        return match result {
            Ok(Err(error)) => Err(error.context(format!("AVIF encoder: {details}"))),
            _ => anyhow::bail!("AVIF encoding timed out: {details}"),
        };
    }
    let mut bytes = tokio::fs::read(output).await?;
    crate::metadata::complete_stats(&mut bytes, &packet, prepared)?;
    let report: Value =
        serde_json::from_slice(&tokio::fs::read(temporary.path().join("encoder.json")).await?)?;
    let duration: u64 = durations.iter().map(|n| *n as u64).sum();
    ensure!(
        report["width"] == width
            && report["height"] == height
            && report["physical_frame_count"] == runs.len()
            && report["bytes"] == bytes.len()
            && report["duration_ms"] == duration,
        "AVIF encoder report mismatch"
    );
    let result = json!({"url":format!("{}/{final_key}",config.public_base_url),"output_format":"avif",
        "frame_count":count,"logical_frame_count":count,"physical_frame_count":runs.len(),"merged_frame_count":count-runs.len(),
        "finalize_policy":POLICY,"metadata_job_id":job,"metadata_policy":crate::metadata::POLICY,"width":width,"height":height,"duration_ms":duration,"bytes":bytes.len(),
        "cache_hit":false,"render_hash":prepared["render_hash"],"final_key":final_key});
    store
        .put(&config.work_bucket, final_key, bytes, "image/avif", false)
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
        json!({"job_id":job,"output_format":"avif","finalize_policy":POLICY,
        "frame_count":count,"physical_frame_count":runs.len(),"rgba_download_sum_ms":download_ms,
        "rgba_download_bytes":download_bytes,"rgba_decompression_ms":decompression_ms,"output_bytes":result["bytes"],"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch() -> Value {
        json!({"schema_version":2,"frames":(1..=3).map(|frame| json!({"frame":frame,"rgba_key":"a.rgba","x":0,"y":0,
            "width":8,"height":6,"canvas_width":8,"canvas_height":6,"duration":42,"sha256":"a".repeat(64),"bytes":192})).collect::<Vec<_>>()})
    }
    #[tokio::test]
    async fn streaming_zstd_checks_size_hash_and_corruption() {
        let raw = vec![37u8; 192];
        let compressed = zstd::bulk::compress(&raw, 1).unwrap();
        let mut value = batch()["frames"][0].clone();
        value["rgba_compression"] = "zstd".into();
        value["raw_sha256"] = crate::sha256(&raw).into();
        value["sha256"] = crate::sha256(&compressed).into();
        value["bytes"] = compressed.len().into();
        let frame: Frame = serde_json::from_value(value.clone()).unwrap();
        write_pixels(&mut tokio::io::sink(), &frame, &compressed)
            .await
            .unwrap();
        assert!(write_pixels(&mut tokio::io::sink(), &frame, b"corrupt")
            .await
            .is_err());
        for size in [191, 193, 100000] {
            let bad = zstd::bulk::compress(&vec![37; size], 1).unwrap();
            assert!(write_pixels(&mut tokio::io::sink(), &frame, &bad)
                .await
                .is_err());
        }
        let wrong = zstd::bulk::compress(&vec![38; 192], 1).unwrap();
        assert!(write_pixels(&mut tokio::io::sink(), &frame, &wrong)
            .await
            .is_err());
        assert!(schedule(
            &[json!({"schema_version":2,"frames":[value.clone()]})],
            1,
            &[42]
        )
        .is_ok());
        value["raw_sha256"] = Value::Null;
        assert!(schedule(&[json!({"schema_version":2,"frames":[value]})], 1, &[42]).is_err());
    }

    #[test]
    fn validates_raw_schedule_before_merging() {
        let b = batch();
        let runs = schedule(&[b.clone()], 3, &[42; 3]).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].duration, 84);
        assert_eq!(runs[1].duration, 42);
        for field in ["bytes", "width", "x", "duration", "frame"] {
            let mut bad = b.clone();
            bad["frames"][1][field] = 999.into();
            assert!(schedule(&[bad], 3, &[42; 3]).is_err(), "{field}");
        }
        let mut bad = b.clone();
        bad["frames"][1]["sha256"] = "b".repeat(64).into();
        assert!(schedule(&[bad], 3, &[42; 3]).is_err());
        let mut bad = b.clone();
        bad["frames"][1]["webp_key"] = "lossy.webp".into();
        assert!(schedule(&[bad], 3, &[42; 3]).is_err());
        assert!(schedule(&[b.clone(), b], 3, &[42; 3]).is_err());
        assert!(schedule(&[], 3, &[42; 3]).is_err());
    }
}
