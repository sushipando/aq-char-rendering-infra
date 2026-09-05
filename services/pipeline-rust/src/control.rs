use crate::{
    config::Config,
    contract::{self, string},
    jobs::{self, Jobs},
};
use anyhow::{ensure, Context, Result};
use aws_sdk_sfn::error::ProvideErrorMetadata;
use serde_json::{json, Value};

pub struct Control {
    pub config: Config,
    pub jobs: Jobs,
    pub sfn: aws_sdk_sfn::Client,
    pub sqs: aws_sdk_sqs::Client,
    pub lambda: aws_sdk_lambda::Client,
    pub ssm: aws_sdk_ssm::Client,
}
impl Control {
    pub fn new(config: Config, aws: &aws_config::SdkConfig) -> Self {
        Self {
            jobs: Jobs {
                client: aws_sdk_dynamodb::Client::new(aws),
                table: config.job_table.clone(),
            },
            config,
            sfn: aws_sdk_sfn::Client::new(aws),
            sqs: aws_sdk_sqs::Client::new(aws),
            lambda: aws_sdk_lambda::Client::new(aws),
            ssm: aws_sdk_ssm::Client::new(aws),
        }
    }

    pub async fn publish_pending(&self, job: &str) -> Result<()> {
        let current = self.jobs.get(job).await?.context("missing completed job")?;
        if current["result_enqueued_at"].is_null() && !current["result_payload"].is_null() {
            // Always publish the terminal transaction's winning payload, not
            // a losing completion attempt's transient local result.
            self.sqs
                .send_message()
                .queue_url(&self.config.result_queue_url)
                .message_body(serde_json::to_string(&current["result_payload"])?)
                .send()
                .await?;
            self.jobs
                .update(job, json!({"result_enqueued_at":jobs::now()}), false)
                .await?;
        }
        Ok(())
    }

    pub async fn complete(&self, event: &Value) -> Result<Value> {
        let request = contract::request(event["request"].clone(), None)?;
        let job = string(&request, "job_id")?;
        let (status, payload, attributes) = if event.get("failure").is_some() {
            let payload = json!({"schema_version":1,"job_id":job,"status":"FAILED","discord":request["discord"],"error":{"code":"RENDER_FAILED","message":"The character could not be rendered. Please try again later."}});
            ("FAILED", payload, json!({"error_code":"RENDER_FAILED"}))
        } else {
            let result = &event["result"];
            for name in [
                "url",
                "frame_count",
                "width",
                "height",
                "duration_ms",
                "bytes",
                "cache_hit",
            ] {
                ensure!(!result[name].is_null(), "incomplete success result");
            }
            let status = if result["cache_hit"] == true {
                "CACHE_HIT"
            } else {
                "SUCCEEDED"
            };
            let compact: serde_json::Map<_, _> = [
                "url",
                "frame_count",
                "width",
                "height",
                "duration_ms",
                "bytes",
                "cache_hit",
            ]
            .iter()
            .map(|k| ((*k).to_string(), result[*k].clone()))
            .collect();
            (
                status,
                json!({"schema_version":1,"job_id":job,"status":"SUCCEEDED","discord":request["discord"],"result":compact}),
                json!({"render_hash":result.get("render_hash").or_else(||event.get("render_hash")).unwrap_or(&Value::Null),"result_url":result["url"]}),
            )
        };
        let mut attributes = attributes;
        attributes["result_payload"] = payload;
        let released = self.jobs.release(job, status, attributes).await?;
        self.publish_pending(job).await?;
        let current = self.jobs.get(job).await?.context("completed job missing")?;
        crate::log(
            "job_complete",
            json!({"job_id":job,"status":current["status"],"released":released}),
        );
        Ok(
            json!({"job_id":job,"status":if current["status"]=="CACHE_HIT" {json!("SUCCEEDED")} else {current["status"].clone()},"released":released}),
        )
    }

    pub async fn launcher(&self, event: &Value) -> Result<Value> {
        let records = event["Records"].as_array().context("missing SQS records")?;
        ensure!(records.len() == 1, "launcher requires batch size one");
        let queued: Value = serde_json::from_str(string(&records[0], "body")?)?;
        // Admission stores the client request before the launcher applies
        // fleet defaults. Compare both with contract defaults so omitted
        // fields do not make a legitimate sparse request look tampered with.
        let admitted_request = contract::request(queued.clone(), None)?;
        let request = contract::request(queued, Some(&self.config.defaults))?;
        let job = string(&request, "job_id")?;
        let admitted = self
            .jobs
            .get(job)
            .await?
            .context("job was not admitted through DynamoDB")?;
        ensure!(
            admitted["user_id"] == request["discord"]["user_id"]
                && admitted["channel_id"] == request["discord"]["channel_id"],
            "admitted job ownership mismatch"
        );
        if !admitted["request"].is_null() {
            ensure!(
                contract::request(admitted["request"].clone(), None)? == admitted_request,
                "queued request differs from admission"
            );
        }
        if admitted["slot_released"] == true
            || admitted["status"].as_str().is_some_and(jobs::terminal)
        {
            return Ok(json!({"job_id":job,"status":admitted["status"],"skipped":true}));
        }
        let machine = std::env::var("CHAR_RENDER_STATE_MACHINE_ARN")?;
        let input = serde_json::to_string(&json!({"request":request}))?;
        let (arn, duplicate) = match self
            .sfn
            .start_execution()
            .state_machine_arn(&machine)
            .name(job)
            .input(&input)
            .send()
            .await
        {
            Ok(result) => (result.execution_arn, false),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|e| e.code() == Some("ExecutionAlreadyExists")) =>
            {
                let arn = format!("{}:{job}", machine.replace(":stateMachine:", ":execution:"));
                let existing = self
                    .sfn
                    .describe_execution()
                    .execution_arn(&arn)
                    .send()
                    .await?;
                let value: Value =
                    serde_json::from_str(existing.input().context("missing existing input")?)?;
                ensure!(
                    value["request"] == request,
                    "existing execution name belongs to a different request"
                );
                (arn, true)
            }
            Err(error) => return Err(error.into()),
        };
        // An execution can finish before this write; don't resurrect it.
        let current = self
            .jobs
            .get(job)
            .await?
            .context("admitted job disappeared")?;
        if current["slot_released"] != true {
            self.jobs
                .update(job, json!({"execution_arn":arn,"status":"PREPARING"}), true)
                .await?;
        }
        crate::log(
            "execution_started",
            json!({"job_id":job,"execution_arn":arn,"duplicate":duplicate}),
        );
        Ok(json!({"job_id":job,"execution_arn":arn,"duplicate":duplicate}))
    }

    async fn release_failure(&self, record: &Value, status: &str) -> Result<bool> {
        let job = string(record, "job_id")?;
        let code = format!("WORKFLOW_{status}");
        let payload = json!({"schema_version":1,"job_id":job,"status":"FAILED","discord":{"user_id":record["user_id"],"channel_id":record["channel_id"],"guild_id":record["guild_id"].as_str().filter(|s|!s.is_empty())},"error":{"code":code,"message":"The character render stopped before it completed. Please try again."}});
        let released = self
            .jobs
            .release(
                job,
                status,
                json!({"error_code":code,"result_payload":payload}),
            )
            .await?;
        self.publish_pending(job).await?;
        Ok(released)
    }

    pub async fn cleanup(&self, event: &Value) -> Result<Value> {
        if event["source"] == "aws.events" && event["detail-type"] == "Scheduled Event" {
            let mut repaired = Vec::new();
            let mut requeued = Vec::new();
            for record in self.jobs.scan(200).await? {
                let job = string(&record, "job_id")?;
                if !record["result_payload"].is_null() && record["result_enqueued_at"].is_null() {
                    self.publish_pending(job).await?;
                    requeued.push(job.to_string());
                }
                if record["slot_released"] == true {
                    continue;
                }
                let terminal =
                    if let Some(arn) = record["execution_arn"].as_str().filter(|s| !s.is_empty()) {
                        match self
                            .sfn
                            .describe_execution()
                            .execution_arn(arn)
                            .send()
                            .await
                        {
                            Ok(execution) => match execution.status.as_str() {
                                "FAILED" | "TIMED_OUT" | "ABORTED" => {
                                    Some(execution.status.as_str().to_string())
                                }
                                _ => None,
                            },
                            Err(error)
                                if error
                                    .as_service_error()
                                    .is_some_and(|e| e.code() == Some("ExecutionDoesNotExist")) =>
                            {
                                Some("FAILED".into())
                            }
                            Err(error) => return Err(error.into()),
                        }
                    } else if record["status"] == "QUEUED"
                        && record["updated_at"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .is_none_or(|t| t < chrono::Utc::now() - chrono::Duration::hours(1))
                    {
                        Some("FAILED".into())
                    } else {
                        None
                    };
                if let Some(status) = terminal {
                    if self.release_failure(&record, &status).await? {
                        repaired.push(job.to_string());
                    }
                }
            }
            return Ok(json!({"scheduled":true,"repaired":repaired,"requeued":requeued}));
        }
        let detail = &event["detail"];
        let status = detail["status"].as_str().unwrap_or("");
        if !matches!(status, "FAILED" | "TIMED_OUT" | "ABORTED") {
            return Ok(json!({"ignored":true}));
        }
        let job = detail["name"]
            .as_str()
            .or_else(|| {
                detail["executionArn"]
                    .as_str()
                    .and_then(|s| s.rsplit(':').next())
            })
            .context("missing workflow identity")?;
        let Some(record) = self.jobs.get(job).await? else {
            return Ok(json!({"job_id":job,"missing":true}));
        };
        let released = self.release_failure(&record, status).await?;
        Ok(json!({"job_id":job,"status":status,"released":released}))
    }

    pub async fn shutdown(&self) -> Result<Value> {
        let parameter = std::env::var("CHAR_RENDER_ENABLED_PARAMETER")?;
        let machine = std::env::var("CHAR_RENDER_STATE_MACHINE_ARN")?;
        let names: Vec<String> =
            serde_json::from_str(&crate::config::env("CHAR_RENDER_STOP_FUNCTIONS", "[]"))?;
        let mappings: Vec<String> = serde_json::from_str(&crate::config::env(
            "CHAR_RENDER_LAUNCHER_EVENT_SOURCE_UUIDS",
            "[]",
        ))?;
        self.ssm
            .put_parameter()
            .name(parameter)
            .value("false")
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .overwrite(true)
            .send()
            .await?;
        let mut disabled = Vec::new();
        for uuid in mappings {
            match self
                .lambda
                .update_event_source_mapping()
                .uuid(&uuid)
                .enabled(false)
                .send()
                .await
            {
                Ok(_) => disabled.push(uuid),
                Err(error)
                    if error
                        .as_service_error()
                        .is_some_and(|e| e.code() == Some("ResourceInUseException")) => {}
                Err(error) => return Err(error.into()),
            }
        }
        for name in &names {
            self.lambda
                .put_function_concurrency()
                .function_name(name)
                .reserved_concurrent_executions(0)
                .send()
                .await?;
        }
        let mut stopped = Vec::new();
        let mut next = None;
        loop {
            let page = self
                .sfn
                .list_executions()
                .state_machine_arn(&machine)
                .status_filter(aws_sdk_sfn::types::ExecutionStatus::Running)
                .set_next_token(next)
                .send()
                .await?;
            for execution in page.executions {
                self.sfn
                    .stop_execution()
                    .execution_arn(&execution.execution_arn)
                    .error("BudgetShutdown")
                    .cause("Automatic render-compute shutdown triggered by AWS Budget")
                    .send()
                    .await?;
                stopped.push(execution.execution_arn);
            }
            next = page.next_token;
            if next.is_none() {
                break;
            }
        }
        Ok(
            json!({"render_enabled":false,"disabled_event_source_mappings":disabled,"throttled_functions":names,"stopped_executions":stopped}),
        )
    }
}
