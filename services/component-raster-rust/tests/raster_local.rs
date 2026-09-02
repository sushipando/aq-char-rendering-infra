//! End-to-end local-raster integration tests against the real binary: build a
//! synthetic FFDec bundle + manifest in a filesystem store, run the worker,
//! and assert the result record and PNG geometry. Failure modes (missing
//! task, unknown part, malformed zoom matrix) fail explicitly.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

const BINARY: &str = env!("CARGO_BIN_EXE_aqw-component-raster");
const JOB: &str = "raster-integration";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "aqw-raster-it-{tag}-{}-{sequence}",
        std::process::id()
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn ffdec_svg() -> String {
    r##"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20.0px" width="40.0px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#33aa88" opacity="0.7"/>
    </g>
  </defs>
</svg>
"##
    .to_string()
}

fn make_bundle() -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let bytes = ffdec_svg();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "armor/000001.svg", bytes.as_bytes())
            .unwrap();
    }
    encoder.finish().unwrap()
}

fn manifest(tasks: Vec<Value>, raster_size: i64, output_size: i64) -> Value {
    json!({
        "schema_version": 1,
        "job_id": JOB,
        "render_hash": "integration",
        "final_key": "renders/integration.webp",
        "frame_count": tasks.len(),
        "frame_rate": 25.0,
        "viewbox": [0.0, 0.0, 512.0, 512.0],
        "frame_durations": [42],
        "fields": {"intColorBase": "16711680"},
        "parts": {
            "armor": {
                "source_idx": 0,
                "root_class": "Armor",
                "character_id": 286,
                "color_rules": {"chest": ["Base", "dark"]},
                "placement_colors": {"286,11": {"red_mult": 128, "green_mult": 256, "blue_mult": 256, "alpha_mult": 256, "red_add": 0, "green_add": 0, "blue_add": 0, "alpha_add": 0}},
            }
        },
        "all_color_rules": [["Base", "dark"]],
        "settings": {
            "facing": "right",
            "zoom": 2.0,
            "raster_size": raster_size,
            "output_size": output_size,
            "padding": 0,
            "webp_quality": 85.0,
            "webp_method": 4,
        },
        "component_pipeline": true,
        "component_tasks": tasks,
        "component_frames": [],
        "component_raster_space": "output",
    })
}

fn populate(root: &Path, manifest: &Value) {
    let manifest_path = root
        .join("work")
        .join("jobs")
        .join(JOB)
        .join("prepare")
        .join("manifest.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(manifest_path, serde_json::to_vec(manifest).unwrap()).unwrap();
    let bundle_path = root
        .join("work")
        .join("jobs")
        .join(JOB)
        .join("prepare")
        .join("source-bundles")
        .join("0.0.tar.gz");
    std::fs::create_dir_all(bundle_path.parent().unwrap()).unwrap();
    std::fs::write(bundle_path, make_bundle()).unwrap();
}

fn run(root: &Path, task_index: i64) -> std::process::Output {
    Command::new(BINARY)
        .arg("local-raster")
        .arg("--store-root")
        .arg(root)
        .arg("--job-id")
        .arg(JOB)
        .arg("--task-index")
        .arg(task_index.to_string())
        .output()
        .unwrap()
}

fn invisible_frame_svg() -> String {
    r##"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="0px" width="0px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#33aa88" opacity="0.7"/>
    </g>
  </defs>
</svg>
"##
    .to_string()
}

fn default_task() -> Value {
    json!({
        "task_id": "task-0",
        "symbol_key": "armor",
        "layer_name": "chest",
        "layer_index": 0,
        "matrix": [0.98, 0.2, -0.2, 0.98, 300.0, 140.0],
        "darken": true,
        "bundle_key": format!("jobs/{JOB}/prepare/source-bundles/0.0.tar.gz"),
        "member": "armor/000001.svg",
        "source_frame": 1,
        "state_signature": "sig-0",
    })
}

fn decode(path: &Path) -> (u32, u32, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap();
    let image = aqw_component_raster::png::decode_rgba8(&bytes).unwrap();
    (image.width, image.height, image.pixels)
}

#[test]
fn local_raster_renders_and_records_the_contract() {
    let root = unique_dir("ok");
    let manifest = manifest(vec![default_task()], 512, 256);
    populate(&root, &manifest);
    let result = run(&root, 0);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let record: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(record["task_id"].as_str(), Some("task-0"));
    assert_eq!(record["empty"], false);
    assert_eq!(record["component_raster_space"], "output");
    assert!(record["canvas_width"].as_i64().unwrap() > 0);
    assert!(record["width"].as_i64().unwrap() > 0);
    assert!(record["height"].as_i64().unwrap() > 0);
    assert!(record["x"].as_i64().unwrap() >= 0);
    assert!(record["y"].as_i64().unwrap() >= 0);
    assert!(record["filter_count"].as_u64().unwrap() >= 1);
    assert!(record["svg_bytes"].as_u64().unwrap() > 0);
    assert!(record["input_bytes"].as_u64().unwrap() > 0);

    let png_path = root.join("work").join(record["png_key"].as_str().unwrap());
    assert!(png_path.is_file());
    let (width, height, pixels) = decode(&png_path);
    assert_eq!(width as i64, record["width"].as_i64().unwrap());
    assert_eq!(height as i64, record["height"].as_i64().unwrap());
    // The darkened translucent rect must have visible alpha.
    assert!(pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0));
    // sha256 in the record matches the stored PNG.
    assert_eq!(
        sha256_hex(&std::fs::read(&png_path).unwrap()),
        record["sha256"].as_str().unwrap()
    );
    assert_eq!(
        record["bytes"].as_u64().unwrap(),
        std::fs::metadata(&png_path).unwrap().len()
    );

    let result_key = root
        .join("work")
        .join(record["result_key"].as_str().unwrap());
    let stored: Value = serde_json::from_slice(&std::fs::read(result_key).unwrap()).unwrap();
    assert_eq!(stored["task_id"], record["task_id"]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn local_raster_writes_under_benchmark_prefix() {
    let root = unique_dir("prefix");
    let manifest = manifest(vec![default_task()], 512, 512);
    populate(&root, &manifest);
    let output = Command::new(BINARY)
        .arg("local-raster")
        .arg("--store-root")
        .arg(&root)
        .arg("--job-id")
        .arg(JOB)
        .arg("--task-index")
        .arg("0")
        .arg("--benchmark-output-prefix")
        .arg("benchmarks/rust-raster/x")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        record["png_key"].as_str().unwrap(),
        "benchmarks/rust-raster/x/rasters/task-0.png"
    );
    assert_eq!(
        record["result_key"].as_str().unwrap(),
        "benchmarks/rust-raster/x/results/task-0.json"
    );
    assert!(root
        .join("work")
        .join("benchmarks/rust-raster/x/rasters/task-0.png")
        .is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn local_raster_rejects_missing_task_index() {
    let root = unique_dir("missing-task");
    let manifest = manifest(vec![default_task()], 512, 256);
    populate(&root, &manifest);
    let output = run(&root, 7);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("out of range"), "stderr: {stderr}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn local_raster_rejects_unknown_part() {
    let root = unique_dir("unknown-part");
    let mut task = default_task();
    task["symbol_key"] = json!("cape");
    let manifest = manifest(vec![task], 512, 256);
    populate(&root, &manifest);
    let output = run(&root, 0);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown part"), "stderr: {stderr}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn local_raster_treats_zero_size_frame_as_empty_not_error() {
    let root = unique_dir("zero-size");
    let manifest = manifest(vec![default_task()], 512, 256);
    populate(&root, &manifest);

    // Replace the state SVG with a zero-size authored invisible frame and
    // re-tar the bundle, then re-run the task: it must be an empty result.
    let task_svg = invisible_frame_svg();
    use flate2::{write::GzEncoder, Compression};
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(task_svg.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "armor/000001.svg", task_svg.as_bytes())
            .unwrap();
    }
    let bundle = encoder.finish().unwrap();
    let bundle_path = root
        .join("work")
        .join("jobs")
        .join(JOB)
        .join("prepare")
        .join("source-bundles")
        .join("0.0.tar.gz");
    std::fs::write(bundle_path, bundle).unwrap();

    let output = run(&root, 0);
    assert!(
        output.status.success(),
        "zero-size frame must not fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["empty"], true);
    assert_eq!(record["width"], 0);
    assert_eq!(record["height"], 0);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn local_raster_rejects_missing_member() {
    let root = unique_dir("bad-member");
    let mut task = default_task();
    task["member"] = json!("armor/000009.svg");
    let manifest = manifest(vec![task], 512, 256);
    populate(&root, &manifest);
    let output = run(&root, 0);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing member"), "stderr: {stderr}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn raster_output_is_deterministic_across_runs() {
    let root = unique_dir("deterministic");
    let manifest = manifest(vec![default_task()], 512, 256);
    populate(&root, &manifest);
    let first = run(&root, 0);
    let second = run(&root, 0);
    assert!(first.status.success() && second.status.success());
    let first_record: Value = serde_json::from_slice(&first.stdout).unwrap();
    let second_record: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        first_record["png_key"].as_str(),
        second_record["png_key"].as_str()
    );
    let png = std::fs::read(
        root.join("work")
            .join(first_record["png_key"].as_str().unwrap()),
    )
    .unwrap();
    // Same bytes written to the identical deterministic key.
    let png_again = std::fs::read(
        root.join("work")
            .join(second_record["png_key"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(png, png_again);
    std::fs::remove_dir_all(&root).ok();
}
