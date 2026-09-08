//! End-to-end local-mode integration tests against the real binary and (when
//! available) the pinned cwebp. These reproduce the worker's whole chunk
//! path: manifest + results + rasters in, lossless PNGs + WebPs + a
//! compose-batch manifest out, with all the failure modes explicit.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_aqw-component-compose");

fn cwebp() -> Option<String> {
    match std::env::var("CHAR_RENDER_CWEBP") {
        Ok(path) if Path::new(&path).is_file() => Some(path),
        _ => which("cwebp"),
    }
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

struct Fixture {
    root: PathBuf,
    results: PathBuf,
    rasters: PathBuf,
    records: Vec<Value>,
}

impl Fixture {
    fn new(root: PathBuf) -> Self {
        let results = root.join("component").join("results");
        let rasters = root.join("component").join("rasters");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::create_dir_all(&rasters).unwrap();
        Fixture {
            root,
            results,
            rasters,
            records: Vec::new(),
        }
    }

    fn solid(task_id: &str, width: u32, height: u32, pixel: [u8; 4], x: i64, y: i64) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&pixel);
        }
        let png = aqw_component_compose::png::encode_rgba8(width, height, &pixels).unwrap();
        let _ = task_id;
        let _ = x;
        let _ = y;
        png
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        task_id: &str,
        width: u32,
        height: u32,
        pixel: [u8; 4],
        x: i64,
        y: i64,
        empty: bool,
    ) {
        let mut record = serde_json::json!({
            "task_id": task_id,
            "empty": empty,
            "component_raster_space": "output",
        });
        if empty {
            record["x"] = serde_json::json!(0);
            record["y"] = serde_json::json!(0);
        } else {
            let png = Self::solid(task_id, width, height, pixel, x, y);
            let raster = self.rasters.join(format!("{task_id}.png"));
            std::fs::write(&raster, &png).unwrap();
            record["x"] = serde_json::json!(x);
            record["y"] = serde_json::json!(y);
            record["sha256"] = serde_json::json!(sha256_hex(&png));
            record["png_key"] = serde_json::json!(format!("local://{task_id}"));
        }
        std::fs::write(
            self.results.join(format!("{task_id}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        self.records.push(record);
    }

    fn write_manifest(&self, frames: &[Value]) {
        let manifest = serde_json::json!({
            "schema_version": 1,
            "job_id": "rust-integration",
            "render_hash": "integration",
            "final_key": "renders/rust-integration.webp",
            "frame_count": frames.len(),
            "frame_rate": 25.0,
            "viewbox": [0.0, 0.0, 256.0, 256.0],
            "frame_durations": vec![40u64; frames.len()],
            "fields": {},
            "aliases": {},
            "weapon_type": "Sword",
            "parts": {},
            "static_keys": [],
            "ground_animate": {},
            "all_color_rules": [],
            "settings": {
                "facing": "right",
                "zoom": 1.0,
                "raster_size": 256,
                "output_size": 256,
                "padding": 0,
                "webp_quality": 80.0,
                "webp_method": 4,
            },
            "component_pipeline": true,
            "component_tasks": [],
            "component_frames": frames,
            "component_raster_space": "output",
            "component_batches": [],
        });
        std::fs::write(
            self.root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, output_dir: &Path, frame_end: u32, extra_args: &[&str]) -> std::process::Output {
        let mut command = Command::new(BINARY);
        command
            .arg("local-compose")
            .arg("--artifact-dir")
            .arg(&self.root)
            .arg("--output-dir")
            .arg(output_dir)
            .arg("--frame-start")
            .arg("1")
            .arg("--frame-end")
            .arg(frame_end.to_string());
        for arg in extra_args {
            command.arg(arg);
        }
        command.output().unwrap()
    }
}

fn unique_dir(tag: &str) -> PathBuf {
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "aqw-rust-it-{tag}-{}-{sequence}",
        std::process::id()
    ))
}

fn sample_frame_root() -> (PathBuf, Vec<serde_json::Value>, Vec<PathBuf>) {
    let temp = unique_dir("chunk");
    let _ = std::fs::remove_dir_all(&temp);
    let mut fixture = Fixture::new(temp.clone());
    // Order-sensitive translucent overlap + negative/overflow placement.
    fixture.add("red", 40, 40, [255, 0, 0, 128], 10, 10, false);
    fixture.add("blue", 40, 40, [0, 0, 255, 128], 30, 20, false);
    fixture.add("edge", 40, 40, [0, 255, 0, 255], -25, 240, false);
    fixture.add("hidden", 1, 1, [0, 0, 0, 0], 0, 0, true);
    let frames = vec![
        serde_json::json!({"number": 1, "layers": ["red", "blue", "hidden"], "duration_ms": 40}),
        serde_json::json!({"number": 2, "layers": ["blue", "red", "edge"], "duration_ms": 55}),
        serde_json::json!({"number": 3, "layers": ["red"], "duration_ms": 40}),
    ];
    fixture.write_manifest(&frames);
    let raster_paths = vec![
        fixture.rasters.join("red.png"),
        fixture.rasters.join("blue.png"),
        fixture.rasters.join("edge.png"),
    ];
    (temp, frames, raster_paths)
}

#[test]
fn local_mode_composes_every_frame_and_writes_the_batch_contract() {
    let (temp, frames, _) = sample_frame_root();
    let output = temp.join("out");
    let result = Fixture::new(temp.clone()).run(&output, 3, &[]);
    assert!(
        result.status.success(),
        "local-compose failed: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );

    for frame in &frames {
        let number = frame["number"].as_u64().unwrap();
        let png = output.join("frames").join(format!("{number:06}.png"));
        let webp = output.join("frames").join(format!("{number:06}.webp"));
        assert!(png.is_file(), "missing lossless frame PNG {png:?}");
        assert!(webp.is_file(), "missing frame WebP {webp:?}");
        // Composed canvas must be 256x256 RGBA with real content.
        let (width, height, pixels) = read_png(&png);
        assert_eq!((width, height), (256, 256));
        assert!(
            pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0),
            "frame is empty"
        );
    }

    let batch: Value =
        serde_json::from_slice(&std::fs::read(output.join("batch-0000.json")).unwrap()).unwrap();
    assert_eq!(batch["schema_version"], 1);
    assert_eq!(batch["job_id"], "rust-integration");
    assert_eq!(batch["batch"], 0);
    let frames_json = batch["frames"].as_array().unwrap();
    assert_eq!(frames_json.len(), 3);
    assert_eq!(
        frames_json
            .iter()
            .map(|f| f["frame"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        frames_json
            .iter()
            .map(|f| f["duration"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![40, 55, 40]
    );
    for frame in frames_json {
        assert_eq!(frame["canvas_width"], 256);
        assert_eq!(frame["canvas_height"], 256);
        assert_eq!(frame["width"], 256);
        assert_eq!(frame["height"], 256);
        assert_eq!(frame["x"], 0);
        assert_eq!(frame["y"], 0);
        let webp = std::fs::read(
            output
                .join("frames")
                .join(format!("{:06}.webp", frame["frame"].as_i64().unwrap())),
        )
        .unwrap();
        assert_eq!(frame["sha256"].as_str().unwrap(), sha256_hex(&webp));
        assert_eq!(frame["bytes"].as_u64().unwrap(), webp.len() as u64);
    }
    let _ = std::fs::remove_dir_all(&temp);
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap();
    let image = aqw_component_compose::png::decode_rgba8(&bytes).unwrap();
    (image.width, image.height, image.pixels)
}

#[test]
fn local_mode_encodes_webp_byte_identical_to_pillow_convention() {
    let Some(cwebp) = cwebp() else {
        eprintln!("skipping webp test: cwebp not found");
        return;
    };
    let (temp, frames, _) = sample_frame_root();
    let output = temp.join("out");
    let fixture = Fixture::new(temp.clone());
    let result = fixture.run(&output, 3, &["--cwebp", &cwebp]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for frame in &frames {
        let number = frame["number"].as_u64().unwrap();
        let webp_path = output.join("frames").join(format!("{number:06}.webp"));
        assert!(webp_path.is_file(), "missing webp for frame {number}");
        assert!(webp_path.metadata().unwrap().len() > 0);
    }
    let _ = std::fs::remove_dir_all(&temp);
}

#[test]
fn local_mode_rejects_missing_task_results() {
    let temp = unique_dir("missing-task");
    let mut fixture = Fixture::new(temp.clone());
    fixture.add("red", 10, 10, [255, 0, 0, 255], 0, 0, false);
    let frames =
        vec![serde_json::json!({"number": 1, "layers": ["red", "ghost"], "duration_ms": 40})];
    fixture.write_manifest(&frames);
    let output = temp.join("out");
    let fixture = Fixture::new(temp.clone());
    let result = fixture.run(&output, 1, &[]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(stderr.contains("missing results"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&temp);
}

#[test]
fn local_mode_rejects_missing_png() {
    let temp = unique_dir("missing-png");
    let _ = std::fs::remove_dir_all(&temp);
    let mut fixture = Fixture::new(temp.clone());
    fixture.add("red", 40, 40, [255, 0, 0, 255], 0, 0, false);
    // Delete the raster so the decode/fetch path fails.
    std::fs::remove_file(fixture.rasters.join("red.png")).unwrap();
    fixture.write_manifest(&[serde_json::json!({"number": 1, "layers": ["red"]})]);
    let output = temp.join("out");
    let run = Fixture::new(temp.clone());
    let result = run.run(&output, 1, &[]);
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("raster") || stderr.contains("png") || stderr.contains("component"),
        "stderr: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

#[tokio::test]
async fn unique_composition_encodes_once_and_emits_every_logical_frame() {
    let Some(cwebp) = cwebp() else {
        eprintln!("skipping dedup encode test: cwebp not found");
        return;
    };
    let temp = unique_dir("dedup");
    let _ = std::fs::remove_dir_all(&temp);
    let mut fixture = Fixture::new(temp.clone());
    fixture.add("red", 40, 40, [255, 0, 0, 255], 10, 10, false);
    let frames = vec![
        serde_json::json!({"number":1,"layers":["red"],"duration_ms":40}),
        serde_json::json!({"number":2,"layers":["red"],"duration_ms":55}),
    ];
    fixture.write_manifest(&frames);
    let manifest_path = fixture.root.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["frame_durations"] = serde_json::json!([40, 55]);
    manifest["component_compositions"] = serde_json::json!([{
        "canonical_frame": 1,
        "layers": ["red"],
        "logical_frames": [1, 2]
    }]);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let results = fixture
        .records
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let event = aqw_component_compose::contract::ComposeEvent {
        job_id: "rust-integration".to_string(),
        manifest_key: "local://manifest".to_string(),
        component_results: results,
        component_results_key: None,
        batch: aqw_component_compose::contract::BatchIndex {
            index: 0,
            frame_start: None,
            frame_end: None,
            composition_start: Some(0),
            composition_end: Some(0),
        },
        benchmark_output_prefix: None,
    };
    let output = temp.join("out");
    let scratch = temp.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let (result, stats) = aqw_component_compose::worker::run_chunk(
        &event,
        &aqw_component_compose::local::FsSource::new(temp.clone()),
        &aqw_component_compose::local::FsSink::new(output.clone(), 0),
        &aqw_component_compose::worker::ComposeOptions {
            download_concurrency: 4,
            cwebp: cwebp.into(),
            scratch_dir: scratch,
            retain_png_dir: Some(output.join("frames")),
        },
    )
    .await
    .unwrap();

    assert_eq!(stats.unique_frames_encoded, 1);
    assert_eq!(stats.logical_frames_emitted, 2);
    assert_eq!(stats.deduplicated_frames, 1);
    assert!(output.join("frames/000001.webp").is_file());
    assert!(!output.join("frames/000002.webp").exists());
    let batch: Value =
        serde_json::from_slice(&std::fs::read(output.join("batch-0000.json")).unwrap()).unwrap();
    assert_eq!(result.batch, 0);
    assert_eq!(batch["frames"].as_array().unwrap().len(), 2);
    assert_eq!(
        batch["frames"][0]["webp_key"],
        batch["frames"][1]["webp_key"]
    );
    assert_eq!(batch["frames"][0]["sha256"], batch["frames"][1]["sha256"]);
    assert_eq!(batch["frames"][0]["duration"], 40);
    assert_eq!(batch["frames"][1]["duration"], 55);
    let _ = std::fs::remove_dir_all(&temp);
}

#[tokio::test]
async fn charpage_layers_surround_character_for_webp_and_zstd_avif() {
    let Some(cwebp) = cwebp() else { return; };
    for (format, background_on, foreground_on) in ["webp", "avif"].into_iter().flat_map(|f| [(f,true,true),(f,true,false),(f,false,true),(f,false,false)]) {
        let temp = unique_dir("charpage");
        let mut fixture = Fixture::new(temp.clone());
        fixture.add("red", 40, 40, [255,0,0,255], 10, 10, false);
        if background_on { fixture.add("scene",256,256,[255,255,255,255],0,0,false); }
        fixture.write_manifest(&[serde_json::json!({"number":1,"layers":if background_on {vec!["scene","red"]} else {vec!["red"]},"duration_ms":40})]);
        let path = temp.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["settings"]["output_format"] = format.into();
        manifest["settings"]["rgba_compression"] = "zstd".into();
        manifest["settings"]["webp_lossless"] = true.into();
        let background = Fixture::solid("background",256,256,[255,255,255,255],0,0);
        let mut pixels = vec![0;256*256*4];
        let offset = (20*256+20)*4;
        pixels[offset..offset+4].copy_from_slice(&[0,0,255,255]);
        let foreground = aqw_component_compose::png::encode_rgba8(256,256,&pixels).unwrap();
        for (name, bytes) in [("background",background),("foreground",foreground)] {
            if (name == "background" && !background_on) || (name == "foreground" && !foreground_on) { continue; }
            std::fs::write(fixture.rasters.join(format!("{name}.png")),&bytes).unwrap();
            manifest["presentation_layers"][name] = serde_json::json!({"key":format!("local://{name}"),"sha256":sha256_hex(&bytes)});
        }
        if background_on {
            let mut pixels = vec![0;256*256*4];
            for (x,y) in [(15,15),(60,60)] { pixels[(y*256+x)*4..(y*256+x)*4+4].copy_from_slice(&[0,255,0,255]); }
            let overlay = aqw_component_compose::png::encode_rgba8(256,256,&pixels).unwrap();
            std::fs::write(fixture.rasters.join("overlay.png"),&overlay).unwrap();
            manifest["presentation_layers"]["background_overlay"] = serde_json::json!({"key":"local://overlay","sha256":sha256_hex(&overlay)});
        }
        std::fs::write(path,serde_json::to_vec(&manifest).unwrap()).unwrap();
        let event = aqw_component_compose::contract::ComposeEvent {
            job_id:"rust-integration".into(),manifest_key:"local://manifest".into(),
            component_results:fixture.records.iter().cloned().map(serde_json::from_value).collect::<Result<Vec<_>,_>>().unwrap(),
            component_results_key:None,benchmark_output_prefix:None,
            batch:aqw_component_compose::contract::BatchIndex{index:0,frame_start:Some(1),frame_end:Some(1),composition_start:None,composition_end:None},
        };
        let output=temp.join("out"); let scratch=temp.join("scratch");std::fs::create_dir_all(&scratch).unwrap();
        aqw_component_compose::worker::run_chunk(&event,
            &aqw_component_compose::local::FsSource::new(temp.clone()),
            &aqw_component_compose::local::FsSink::new(output.clone(),0),
            &aqw_component_compose::worker::ComposeOptions{download_concurrency:2,cwebp:cwebp.clone().into(),scratch_dir:scratch,retain_png_dir:Some(output.join("frames"))}
        ).await.unwrap();
        let pixels = if format == "webp" {
            aqw_component_compose::png::decode_rgba8(&std::fs::read(output.join("frames/000001.png")).unwrap()).unwrap().pixels
        } else {
            zstd::stream::decode_all(std::io::Cursor::new(std::fs::read(output.join("frames/000001.rgba")).unwrap())).unwrap()
        };
        assert_eq!(&pixels[0..4], if background_on { &[255,255,255,255] } else { &[0,0,0,0] });
        assert_eq!(&pixels[(15*256+15)*4..(15*256+15)*4+4], &[255,0,0,255]);
        assert_eq!(&pixels[offset..offset+4], if foreground_on { &[0,0,255,255] } else { &[255,0,0,255] });
        if background_on { assert_eq!(&pixels[(60*256+60)*4..(60*256+60)*4+4], &[0,255,0,255]); }
        std::fs::remove_dir_all(temp).unwrap();
    }
}
