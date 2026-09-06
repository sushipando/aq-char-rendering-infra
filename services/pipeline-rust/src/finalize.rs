use crate::{
    config::Config,
    contract::{integer, string},
    store::{self, Store},
};
use anyhow::{ensure, Context, Result};
use futures::{stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

fn unique_webps(frames: &BTreeMap<u64, Value>) -> Result<BTreeMap<String, (String, usize)>> {
    let mut unique = BTreeMap::new();
    for frame in frames.values() {
        let key = string(frame, "webp_key")?.to_string();
        let expected = (
            string(frame, "sha256")?.to_string(),
            integer(frame, "bytes", 1, usize::MAX as u64)? as usize,
        );
        if let Some(existing) = unique.insert(key.clone(), expected.clone()) {
            ensure!(
                existing == expected,
                "encoded WebP key has conflicting checksum or size: {key}"
            );
        }
    }
    Ok(unique)
}

fn uint24(data: &[u8]) -> Result<u32> {
    ensure!(data.len() >= 3, "truncated WebP integer");
    Ok(data[0] as u32 | (data[1] as u32) << 8 | (data[2] as u32) << 16)
}

/// Validate the container after libwebp muxing, including exact frame timing
/// and full-canvas placement. Encoded frame checksums are verified separately.
pub fn validate_webp(
    bytes: &[u8],
    count: usize,
    canvas: [u32; 2],
    durations: &[u32],
) -> Result<()> {
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "not a WebP container"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into()?) as usize + 8 == bytes.len(),
        "WebP RIFF size mismatch"
    );
    let mut offset = 12;
    let mut frame = 0;
    let mut size = None;
    let mut loop_count = None;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 8)
            .context("truncated WebP chunk")?;
        let length = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        let payload = bytes
            .get(offset + 8..offset + 8 + length)
            .context("truncated WebP payload")?;
        match &header[..4] {
            b"VP8X" => {
                ensure!(payload.len() == 10, "invalid VP8X");
                size = Some([uint24(&payload[4..])? + 1, uint24(&payload[7..])? + 1]);
            }
            b"ANIM" => {
                ensure!(payload.len() == 6, "invalid ANIM");
                loop_count = Some(u16::from_le_bytes(payload[4..6].try_into()?));
            }
            b"ANMF" => {
                ensure!(
                    payload.len() >= 16 && frame < durations.len(),
                    "invalid ANMF"
                );
                let x = uint24(payload)? * 2;
                let y = uint24(&payload[3..])? * 2;
                let w = uint24(&payload[6..])? + 1;
                let h = uint24(&payload[9..])? + 1;
                ensure!(
                    x + w <= canvas[0] && y + h <= canvas[1],
                    "frame exceeds canvas"
                );
                ensure!(
                    uint24(&payload[12..])? == durations[frame],
                    "WebP frame duration mismatch"
                );
                frame += 1;
            }
            b"VP8L" if count == 1 => {
                ensure!(payload.len() >= 5 && payload[0] == 0x2f, "invalid VP8L");
                let bits = u32::from_le_bytes(payload[1..5].try_into()?);
                size.get_or_insert([(bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1]);
            }
            b"VP8 " if count == 1 => {
                ensure!(
                    payload.len() >= 10 && payload[3..6] == [0x9d, 0x01, 0x2a],
                    "invalid VP8 frame"
                );
                size.get_or_insert([
                    (u16::from_le_bytes(payload[6..8].try_into()?) & 0x3fff) as u32,
                    (u16::from_le_bytes(payload[8..10].try_into()?) & 0x3fff) as u32,
                ]);
            }
            _ => (),
        }
        offset += 8 + length + (length & 1);
    }
    ensure!(
        offset == bytes.len() && size == Some(canvas),
        "WebP canvas mismatch"
    );
    ensure!(
        (count == 1 && frame == 0) || (frame == count && loop_count == Some(0)),
        "WebP frame count/loop mismatch"
    );
    Ok(())
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
            let number = integer(frame, "frame", 1, count as u64)?;
            ensure!(
                frames.insert(number, frame.clone()).is_none(),
                "duplicate encoded frame"
            );
        }
    }
    ensure!(frames.len() == count, "encoded frame set is incomplete");
    let first = frames.values().next().context("no frames")?;
    let canvas = [
        integer(first, "canvas_width", 1, 4096)? as u32,
        integer(first, "canvas_height", 1, 4096)? as u32,
    ];
    let mut durations = Vec::new();
    for (number, frame) in &frames {
        ensure!(
            frame["canvas_width"] == canvas[0] && frame["canvas_height"] == canvas[1],
            "frames do not share one canvas"
        );
        let duration = integer(frame, "duration", 1, 0xffffff)? as u32;
        ensure!(
            prepared["frame_durations"][*number as usize - 1] == duration,
            "encoded duration differs from prepare"
        );
        durations.push(duration);
    }
    let temporary = tempfile::tempdir()?;
    let unique = unique_webps(&frames)?;
    let downloaded: Vec<_> = stream::iter(unique.into_iter().enumerate())
        .map(|(index, (key, (sha256, expected_bytes)))| {
            let root = temporary.path();
            async move {
                let bytes = store
                    .get(&config.work_bucket, &key)
                    .await?
                    .context("missing encoded WebP")?;
                ensure!(
                    crate::sha256(&bytes) == sha256,
                    "encoded WebP checksum mismatch"
                );
                ensure!(
                    bytes.len() == expected_bytes,
                    "encoded WebP byte length mismatch"
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
    let paths: Vec<_> = frames
        .values()
        .map(|frame| {
            downloaded
                .get(string(frame, "webp_key")?)
                .cloned()
                .context("encoded WebP was not downloaded")
        })
        .collect::<Result<_>>()?;
    let output = temporary.path().join("result.webp");
    let mut command = tokio::process::Command::new(crate::config::env(
        "CHAR_RENDER_WEBPMUX",
        "/usr/bin/webpmux",
    ));
    for (path, frame) in paths.iter().zip(frames.values()) {
        command.arg("-frame").arg(path).arg(format!(
            "+{}+{}+{}+0-b",
            integer(frame, "duration", 1, 0xffffff)?,
            integer(frame, "x", 0, 4096)?,
            integer(frame, "y", 0, 4096)?
        ));
    }
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
    let bytes = tokio::fs::read(&output).await?;
    validate_webp(&bytes, count, canvas, &durations)?;
    let final_key = string(&prepared, "final_key")?;
    let result = json!({"url":format!("{}/{final_key}",config.public_base_url),"frame_count":count,"width":canvas[0],"height":canvas[1],"duration_ms":durations.iter().map(|d|*d as u64).sum::<u64>(),"bytes":bytes.len(),"cache_hit":false,"render_hash":prepared["render_hash"],"final_key":final_key});
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
        json!({"job_id":job,"frame_count":count,"output_bytes":result["bytes"],"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::unique_webps;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    #[test]
    fn collapses_alias_records_to_one_download() {
        let frames: BTreeMap<u64, Value> = BTreeMap::from([
            (1, json!({"webp_key":"same.webp","sha256":"abc","bytes":12})),
            (
                11,
                json!({"webp_key":"same.webp","sha256":"abc","bytes":12}),
            ),
            (
                12,
                json!({"webp_key":"other.webp","sha256":"def","bytes":9}),
            ),
        ]);
        assert_eq!(unique_webps(&frames).unwrap().len(), 2);
    }

    #[test]
    fn rejects_conflicting_metadata_for_one_key() {
        let frames: BTreeMap<u64, Value> = BTreeMap::from([
            (1, json!({"webp_key":"same.webp","sha256":"abc","bytes":12})),
            (
                2,
                json!({"webp_key":"same.webp","sha256":"different","bytes":12}),
            ),
        ]);
        assert!(unique_webps(&frames).is_err());
    }
}
