//! One-file subprocess used by scripts/sanity_check_swfs.py. No AWS clients.
use anyhow::{Context, Result};
use aqw_render_pipeline::{
    background,
    export::{Ffdec, ScriptMetadata},
    model::SymbolRequest,
    swf::Swf,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

async fn check(path: &Path, jar: Option<PathBuf>) -> Result<Value> {
    let original = std::fs::read(path)?;
    let is_background = path
        .file_name()
        .and_then(|p| p.to_str())
        .is_some_and(|p| p.starts_with("cp-bg"));
    let (bytes, bg_root) = if is_background {
        let (b, id, _) = background::wrap(&original)?;
        (b, Some(id))
    } else {
        (original, None)
    };
    let swf = Swf::parse(&bytes)?;
    swf.placement_colors()?;
    // Inspect every sprite, including unexported nested clips.
    for id in swf.sprites.keys() {
        swf.timeline(*id).with_context(|| format!("sprite {id}"))?;
    }
    let mut result = json!({"status":"ok","sprites":swf.sprites.len(),"symbols":swf.symbols.len(),"scripts_checked":false});
    let Some(jar) = jar else {
        return Ok(result);
    };
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source.swf");
    std::fs::write(&source, aqw_render_pipeline::swf::repair_missing_end(&bytes)?)?;
    let output = temp.path().join("scripts");
    Ffdec {
        jar,
        deadline: Instant::now() + Duration::from_secs(90),
    }
    .run(
        &temp.path().join("home"),
        &[
            "-export".into(),
            "script".into(),
            output.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
        ],
    )
    .await?;
    let mut pending = vec![output];
    let mut metadata = ScriptMetadata::default();
    let mut errors = Vec::new();
    let mut scripts = 0;
    while let Some(dir) = pending.pop() {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else if entry.path().extension().is_some_and(|s| s == "as") {
                scripts += 1;
                if let Err(e) = metadata.inspect_file(&entry.path(), &swf) {
                    errors.push(json!({"script":entry.path().strip_prefix(temp.path()).unwrap().display().to_string(),"error":format!("{e:#}")}));
                }
            }
        }
    }
    let roots = if let Some(id) = bg_root {
        vec![(id, "CharpageBackgroundStage".to_string())]
    } else {
        swf.symbols
            .iter()
            .filter(|(id, _)| swf.sprites.contains_key(id))
            .cloned()
            .collect()
    };
    let mut timeline_errors = Vec::new();
    for (id, name) in &roots {
        let (frame, frames) = swf.timeline(*id)?;
        let request = SymbolRequest {
            key: if bg_root.is_some() {
                background::KEY.into()
            } else {
                "probe".into()
            },
            class_name: name.clone(),
            character_id: *id,
            frame,
            root_timeline_frames: frames,
        };
        if let Err(e) = metadata.normalize(&bytes, &swf, &[request]) {
            timeline_errors.push(json!({"id":id,"class":name,"error":format!("{e:#}")}));
        }
    }
    result["scripts_checked"] = json!(true);
    result["script_count"] = json!(scripts);
    result["parser_errors"] = json!(errors);
    result["timeline_roots_checked"] = json!(roots.len());
    result["timeline_errors"] = json!(timeline_errors);
    result["status"] = json!(if !errors.is_empty() {
        "parser_failed"
    } else if !timeline_errors.is_empty() {
        "timeline_review"
    } else if roots.is_empty() {
        "no_exported_roots"
    } else {
        "ok"
    });
    Ok(result)
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.first() {
        Some(path) => check(Path::new(path), args.get(1).map(PathBuf::from)).await,
        None => Err(anyhow::anyhow!("usage: swf_sanity SWF [FFDEC_JAR]")),
    };
    println!(
        "{}",
        result.unwrap_or_else(|e| json!({"status":"failed","error":format!("{e:#}")}))
    );
}
