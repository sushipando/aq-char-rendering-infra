//! SQS delivery is at least once. Result publication precedes the callback;
//! retries reuse that result. Tokens never appear in telemetry or errors.
use crate::{model::ProbeTask, store::Store};
use anyhow::{Context, Result};
use aws_sdk_sfn::error::ProvideErrorMetadata;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Message {
    task: ProbeTask,
    #[serde(default)]
    task_token: Option<String>,
}

/// The export worker uses the same bounds queue as the callback workflow, but
/// its messages intentionally omit a task token. They are speculative work;
/// PlanBounds remains the completion barrier.
#[async_trait::async_trait]
pub trait BoundsPublisher: Send + Sync {
    async fn publish(&self, task: &ProbeTask) -> Result<()>;
}

pub struct SqsBoundsPublisher {
    client: aws_sdk_sqs::Client,
    queue_url: String,
}

impl SqsBoundsPublisher {
    pub fn new(client: aws_sdk_sqs::Client, queue_url: impl Into<String>) -> Self {
        Self {
            client,
            queue_url: queue_url.into(),
        }
    }
}

#[async_trait::async_trait]
impl BoundsPublisher for SqsBoundsPublisher {
    async fn publish(&self, task: &ProbeTask) -> Result<()> {
        task.validate()?;
        self.client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(json!({"task":task}).to_string())
            .send()
            .await
            .context("bounds prefetch queue unavailable")?;
        Ok(())
    }
}

pub fn stale_callback(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("TaskTimedOut" | "TaskDoesNotExist" | "InvalidToken")
    )
}

#[async_trait::async_trait]
pub trait Callback: Send + Sync {
    async fn success(&self, token: &str, key: &str) -> Result<()>;
    async fn failure(&self, token: &str) -> Result<()>;
}

pub struct SfnCallback(pub aws_sdk_sfn::Client);
#[async_trait::async_trait]
impl Callback for SfnCallback {
    async fn success(&self, token: &str, key: &str) -> Result<()> {
        match self
            .0
            .send_task_success()
            .task_token(token)
            .output(json!({"result_key":key}).to_string())
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if stale_callback(error.as_service_error().and_then(|e| e.code())) => {
                crate::log("bounds_callback_stale", json!({"result_key":key}));
                Ok(())
            }
            Err(_) => anyhow::bail!("bounds success callback unavailable"),
        }
    }
    async fn failure(&self, token: &str) -> Result<()> {
        match self.0.send_task_failure().task_token(token).error("BoundsProbeFailed").cause("The SVG bounds worker exhausted its delivery attempts; inspect probe logs by state hash.").send().await {
            Ok(_)=>Ok(()),Err(error) if stale_callback(error.as_service_error().and_then(|e|e.code()))=>Ok(()),Err(_)=>anyhow::bail!("bounds failure callback unavailable"),
        }
    }
}

async fn record(
    store: &dyn Store,
    bucket: &str,
    callback: &dyn Callback,
    record: &Value,
    max_attempts: u64,
) -> Result<()> {
    let message: Message = serde_json::from_str(
        record["body"]
            .as_str()
            .context("missing SQS message body")?,
    )
    .context("invalid bounds queue message")?;
    let result = crate::bounds::run_probe(store, bucket, &message.task).await;
    match result {
        Ok(_) => match message.task_token.as_deref() {
            Some(token) => callback.success(token, &message.task.result_key).await,
            None => {
                crate::log(
                    "bounds_prefetch_complete",
                    json!({"state_sha256":message.task.state.sha256,"result_key":message.task.result_key}),
                );
                Ok(())
            }
        },
        Err(error) => {
            crate::log(
                "bounds_probe_failed",
                json!({"state_sha256":message.task.state.sha256,"prefetch":message.task_token.is_none(),"error":error.to_string()}),
            );
            let Some(token) = message.task_token.as_deref() else {
                // Fire-and-forget probes have no workflow callback to fail.
                // Keep returning the record so SQS retries and eventually
                // redrives poison input instead of silently acknowledging it.
                anyhow::bail!("bounds prefetch will retry");
            };
            let attempts = record["attributes"]["ApproximateReceiveCount"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1);
            if attempts >= max_attempts {
                callback.failure(token).await
            } else {
                anyhow::bail!("bounds probe will retry")
            }
        }
    }
}

pub async fn handle(
    store: &dyn Store,
    bucket: &str,
    callback: &dyn Callback,
    event: &Value,
    max_attempts: u64,
) -> Result<Value> {
    let records = event["Records"].as_array().context("missing SQS records")?;
    let mut failures = Vec::new();
    for item in records {
        let id = item["messageId"]
            .as_str()
            .context("missing SQS message ID")?;
        if record(store, bucket, callback, item, max_attempts)
            .await
            .is_err()
        {
            failures.push(json!({"itemIdentifier":id}));
        }
    }
    Ok(json!({"batchItemFailures":failures}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{ProbeConfig, StateRef},
        store::FsStore,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fake {
        successes: AtomicUsize,
        failures: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Callback for Fake {
        async fn success(&self, _: &str, _: &str) -> Result<()> {
            self.successes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn failure(&self, _: &str) -> Result<()> {
            self.failures.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn duplicate_delivery_reuses_result_and_poison_reports_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = FsStore(root.path().into());
        let callback = Fake {
            successes: 0.into(),
            failures: 0.into(),
        };
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#;
        let task = ProbeTask::new(StateRef::new(bytes), ProbeConfig::new(1.0)).unwrap();
        store
            .put(
                "work",
                &task.state.svg_key,
                bytes.to_vec(),
                "image/svg+xml",
                true,
            )
            .await
            .unwrap();
        let message = json!({"task":task,"task_token":"secret-token"}).to_string();
        let event = json!({"Records":[{"messageId":"first","body":message,"attributes":{"ApproximateReceiveCount":"1"}}]});
        for _ in 0..2 {
            assert_eq!(
                handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
                json!([])
            );
        }
        assert_eq!(callback.successes.load(Ordering::SeqCst), 2);
        let poison = ProbeTask::new(StateRef::new(b"broken"), ProbeConfig::new(1.0)).unwrap();
        store
            .put(
                "work",
                &poison.state.svg_key,
                b"broken".to_vec(),
                "image/svg+xml",
                true,
            )
            .await
            .unwrap();
        let mut event = json!({"Records":[{"messageId":"bad","body":json!({"task":poison,"task_token":"secret"}).to_string(),"attributes":{"ApproximateReceiveCount":"1"}}]});
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([{"itemIdentifier":"bad"}])
        );
        event["Records"][0]["attributes"]["ApproximateReceiveCount"] = "3".into();
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([])
        );
        assert_eq!(callback.failures.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prefetch_messages_need_no_callback_and_poison_still_redrives() {
        let root = tempfile::tempdir().unwrap();
        let store = FsStore(root.path().into());
        let callback = Fake {
            successes: 0.into(),
            failures: 0.into(),
        };
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#;
        let task = ProbeTask::new(StateRef::new(bytes), ProbeConfig::new(1.0)).unwrap();
        store
            .put(
                "work",
                &task.state.svg_key,
                bytes.to_vec(),
                "image/svg+xml",
                true,
            )
            .await
            .unwrap();
        let event = json!({"Records":[{"messageId":"prefetch","body":json!({"task":task}).to_string(),"attributes":{"ApproximateReceiveCount":"1"}}]});
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([])
        );
        assert_eq!(callback.successes.load(Ordering::SeqCst), 0);
        assert_eq!(callback.failures.load(Ordering::SeqCst), 0);

        let poison = ProbeTask::new(StateRef::new(b"broken"), ProbeConfig::new(1.0)).unwrap();
        store
            .put(
                "work",
                &poison.state.svg_key,
                b"broken".to_vec(),
                "image/svg+xml",
                true,
            )
            .await
            .unwrap();
        let event = json!({"Records":[{"messageId":"bad-prefetch","body":json!({"task":poison}).to_string(),"attributes":{"ApproximateReceiveCount":"3"}}]});
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([{"itemIdentifier":"bad-prefetch"}])
        );
        assert_eq!(callback.failures.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn only_terminal_token_errors_are_acknowledged() {
        assert!(stale_callback(Some("TaskTimedOut")));
        assert!(!stale_callback(Some("ThrottlingException")));
    }

    struct Flaky {
        attempts: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Callback for Flaky {
        async fn success(&self, _: &str, _: &str) -> Result<()> {
            anyhow::ensure!(
                self.attempts.fetch_add(1, Ordering::SeqCst) > 0,
                "transient network failure"
            );
            Ok(())
        }
        async fn failure(&self, _: &str) -> Result<()> {
            anyhow::bail!("unexpected failure callback")
        }
    }

    #[tokio::test]
    async fn callback_retry_uses_persisted_result_without_svg() {
        let root = tempfile::tempdir().unwrap();
        let store = FsStore(root.path().into());
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#;
        let task = ProbeTask::new(StateRef::new(bytes), ProbeConfig::new(1.0)).unwrap();
        store
            .put(
                "work",
                &task.state.svg_key,
                bytes.to_vec(),
                "image/svg+xml",
                true,
            )
            .await
            .unwrap();
        let callback = Flaky { attempts: 0.into() };
        let event = json!({"Records":[{"messageId":"retry","body":json!({"task":task,"task_token":"secret"}).to_string()}]});
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([{"itemIdentifier":"retry"}])
        );
        assert!(store.exists("work", &task.result_key).await.unwrap());
        // Corrupt only the temporary fixture SVG. A callback retry must not read it.
        store
            .put(
                "work",
                &task.state.svg_key,
                b"corrupt".to_vec(),
                "image/svg+xml",
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            handle(&store, "work", &callback, &event, 3).await.unwrap()["batchItemFailures"],
            json!([])
        );
    }
}
