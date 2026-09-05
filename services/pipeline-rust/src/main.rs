use anyhow::{Context, Result};
use aqw_render_pipeline::{
    self as pipeline,
    config::Config,
    contract::string,
    control::Control,
    model::{ProbeConfig, ProbeTask},
    store::{self, Store},
};
use lambda_runtime::{service_fn, LambdaEvent};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

async fn prepare(
    store: &dyn Store,
    config: &Config,
    event: &Value,
    budget: Duration,
) -> Result<Value> {
    match event["phase"].as_str().unwrap_or("resolve") {
        "resolve" => {
            pipeline::resolve::resolve(
                store,
                config,
                &pipeline::contract::request(event["request"].clone(), None)?,
            )
            .await
        }
        "export" => {
            pipeline::export::export_source(
                store,
                &config.work_bucket,
                &config.source_bucket,
                event,
                config.ffdec.clone(),
                budget,
            )
            .await
        }
        "plan_bounds" => {
            let prepared: Value =
                store::read(store, &config.work_bucket, string(event, "input_key")?).await?;
            let mut sources = BTreeMap::new();
            for source in event["export_results"]
                .as_array()
                .context("missing export results")?
            {
                anyhow::ensure!(
                    source["job_id"] == event["job_id"],
                    "export result belongs to another job"
                );
                let index = source["source_idx"]
                    .as_u64()
                    .context("invalid source index")? as usize;
                anyhow::ensure!(
                    sources
                        .insert(index, string(source, "manifest_key")?.to_string())
                        .is_none(),
                    "duplicate source result"
                );
            }
            let mut config_probe = ProbeConfig::new(
                prepared["settings"]["zoom"]
                    .as_f64()
                    .context("missing zoom")?,
            );
            config_probe.padding_pixels =
                pipeline::config::number("CHAR_RENDER_BOUNDS_PADDING_PIXELS", 1, 1, 8)? as u32;
            config_probe.resolution =
                pipeline::config::number("CHAR_RENDER_BOUNDS_RESOLUTION", 256, 64, 1024)? as u32;
            pipeline::bounds::plan(
                store,
                &config.work_bucket,
                string(event, "job_id")?,
                string(event, "input_key")?,
                sources,
                config_probe,
                pipeline::bounds::PlanOptions::new(
                    prepared["cache"]["bounds"].as_bool().unwrap_or(true),
                    pipeline::bounds::BoundsMode::parse(
                        prepared["bounds_mode"]
                            .as_str()
                            .context("missing bounds_mode")?,
                    )?,
                ),
            )
            .await
        }
        "finish" => pipeline::finish::finish(store, config, event).await,
        phase => anyhow::bail!("unknown prepare phase {phase}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "probe-svg") {
        let bytes = tokio::fs::read(args.get(2).context("usage: probe-svg SVG [zoom]")?).await?;
        let zoom = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(1.0);
        let task = pipeline::model::ProbeTask::new(
            pipeline::model::StateRef::new(&bytes),
            ProbeConfig::new(zoom),
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&pipeline::bounds::probe(&bytes, &task)?)?
        );
        return Ok(());
    }
    let config = Config::from_env()?;
    if args.get(1).is_some_and(|s| s == "local") {
        let phase = args
            .get(2)
            .context("usage: local PHASE EVENT.json OBJECT_STORE_ROOT")?;
        let event: Value = serde_json::from_slice(
            &tokio::fs::read(args.get(3).context("missing event path")?).await?,
        )?;
        let store = store::FsStore(args.get(4).context("missing object store root")?.into());
        let result = match phase.as_str() {
            "finalize" => pipeline::finalize::finalize(&store, &config, &event).await?,
            "probe" => serde_json::to_value(
                pipeline::bounds::run_probe(
                    &store,
                    &config.work_bucket,
                    &serde_json::from_value(event)?,
                )
                .await?,
            )?,
            _ => {
                let mut event = event;
                event["phase"] = phase.clone().into();
                prepare(&store, &config, &event, Duration::from_secs(280)).await?
            }
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let store = Arc::new(store::S3Store(aws_sdk_s3::Client::new(&aws)));
    let control = Arc::new(Control::new(config, &aws));
    let handler = pipeline::config::env("CHAR_RENDER_HANDLER", "prepare");
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| {
        let store = store.clone();
        let control = control.clone();
        let handler = handler.clone();
        async move {
            let budget = Duration::from_millis(
                event
                    .context
                    .deadline
                    .saturating_sub(chrono::Utc::now().timestamp_millis() as u64)
                    .saturating_sub(15_000),
            );
            let payload = event.payload;
            let result: Result<Value> = match handler.as_str() {
                "launcher" => control.launcher(&payload).await,
                "complete" => control.complete(&payload).await,
                "cleanup" => control.cleanup(&payload).await,
                "shutdown" => control.shutdown().await,
                "bounds" => {
                    if payload.get("Records").is_some() {
                        pipeline::queue::handle(
                            store.as_ref(),
                            &control.config.work_bucket,
                            &pipeline::queue::SfnCallback(control.sfn.clone()),
                            &payload,
                            3,
                        )
                        .await
                    } else {
                        async {
                            anyhow::ensure!(
                                payload["phase"] == "probe",
                                "invalid direct bounds event"
                            );
                            let task: ProbeTask = serde_json::from_value(payload["task"].clone())?;
                            Ok(serde_json::to_value(
                                pipeline::bounds::run_probe(
                                    store.as_ref(),
                                    &control.config.work_bucket,
                                    &task,
                                )
                                .await?,
                            )?)
                        }
                        .await
                    }
                }
                "prepare" | "export" => {
                    prepare(store.as_ref(), &control.config, &payload, budget).await
                }
                "finalize" => {
                    async {
                        control
                            .jobs
                            .update(
                                string(&payload, "job_id")?,
                                json!({"status":"FINALIZING"}),
                                true,
                            )
                            .await?;
                        let result =
                            pipeline::finalize::finalize(store.as_ref(), &control.config, &payload)
                                .await?;
                        control
                            .complete(&json!({"request":payload["request"],"result":result}))
                            .await?;
                        Ok(result)
                    }
                    .await
                }
                _ => Err(anyhow::anyhow!("unknown Rust handler")),
            };
            result.map_err(|error| lambda_runtime::Error::from(format!("{error:#}")))
        }
    }))
    .await?;
    Ok(())
}
