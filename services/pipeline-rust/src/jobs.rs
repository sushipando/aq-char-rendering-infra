use anyhow::{ensure, Context, Result};
use aws_sdk_dynamodb::{
    error::ProvideErrorMetadata,
    types::{AttributeValue as A, TransactWriteItem, Update},
};
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn terminal(status: &str) -> bool {
    matches!(
        status,
        "CACHE_HIT" | "SUCCEEDED" | "FAILED" | "TIMED_OUT" | "ABORTED"
    )
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
fn attribute(value: &Value) -> A {
    match value {
        Value::Null => A::Null(true),
        Value::Bool(b) => A::Bool(*b),
        Value::Number(n) => A::N(n.to_string()),
        Value::String(s) => A::S(s.clone()),
        Value::Array(a) => A::L(a.iter().map(attribute).collect()),
        Value::Object(o) => A::M(o.iter().map(|(k, v)| (k.clone(), attribute(v))).collect()),
    }
}
fn value(a: &A) -> Result<Value> {
    Ok(match a {
        A::Null(_) => Value::Null,
        A::Bool(b) => json!(b),
        A::N(n) => serde_json::from_str(n)?,
        A::S(s) => json!(s),
        A::L(a) => Value::Array(a.iter().map(value).collect::<Result<_>>()?),
        A::M(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), value(v)?)))
                .collect::<Result<_>>()?,
        ),
        _ => anyhow::bail!("unsupported job attribute"),
    })
}
fn attrs(value: &Value) -> HashMap<String, A> {
    value
        .as_object()
        .expect("internal object")
        .iter()
        .map(|(k, v)| (k.clone(), attribute(v)))
        .collect()
}
fn key(job: &str) -> HashMap<String, A> {
    attrs(&json!({"PK":format!("JOB#{job}"),"SK":"META"}))
}

pub struct Jobs {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}
impl Jobs {
    pub async fn get(&self, job: &str) -> Result<Option<Value>> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(key(job)))
            .consistent_read(true)
            .send()
            .await?;
        response.item.map(|item| value(&A::M(item))).transpose()
    }
    pub async fn update(&self, job: &str, attributes: Value, require_active: bool) -> Result<()> {
        let mut names = HashMap::new();
        let mut values = HashMap::new();
        let mut setters = Vec::new();
        for (i, (name, val)) in attributes
            .as_object()
            .context("invalid update attributes")?
            .iter()
            .enumerate()
        {
            names.insert(format!("#f{i}"), name.clone());
            values.insert(format!(":v{i}"), attribute(val));
            setters.push(format!("#f{i} = :v{i}"));
        }
        names.insert("#updated".into(), "updated_at".into());
        values.insert(":now".into(), A::S(now()));
        setters.push("#updated = :now".into());
        let mut call = self
            .client
            .update_item()
            .table_name(&self.table)
            .set_key(Some(key(job)))
            .update_expression(format!("SET {}", setters.join(", ")))
            .set_expression_attribute_names(Some(names));
        if require_active {
            values.insert(":false".into(), A::Bool(false));
            call=call.condition_expression("attribute_exists(PK) AND (attribute_not_exists(slot_released) OR slot_released = :false)");
        } else {
            call = call.condition_expression("attribute_exists(PK)");
        }
        call.set_expression_attribute_values(Some(values))
            .send()
            .await?;
        Ok(())
    }
    pub async fn release(&self, job: &str, status: &str, attributes: Value) -> Result<bool> {
        ensure!(terminal(status), "cannot release a nonterminal job");
        let Some(current) = self.get(job).await? else {
            return Ok(false);
        };
        if current["slot_released"] == true {
            return Ok(false);
        }
        let user = crate::contract::string(&current, "user_id")?;
        let timestamp = now();
        let mut names = HashMap::from([("#status".into(), "status".into())]);
        let mut values =
            attrs(&json!({":status":status,":true":true,":false":false,":now":timestamp}));
        let mut setters = vec![
            "#status = :status".to_string(),
            "slot_released = :true".into(),
            "updated_at = :now".into(),
        ];
        for (i, (name, val)) in attributes
            .as_object()
            .context("invalid release attributes")?
            .iter()
            .enumerate()
        {
            names.insert(format!("#f{i}"), name.clone());
            values.insert(format!(":v{i}"), attribute(val));
            setters.push(format!("#f{i} = :v{i}"));
        }
        let job_update=Update::builder().table_name(&self.table).set_key(Some(key(job))).update_expression(format!("SET {}",setters.join(", "))).condition_expression("attribute_exists(PK) AND (attribute_not_exists(slot_released) OR slot_released = :false)").set_expression_attribute_names(Some(names)).set_expression_attribute_values(Some(values)).build()?;
        let counter = Update::builder()
            .table_name(&self.table)
            .set_key(Some(attrs(
                &json!({"PK":format!("USER#{user}"),"SK":"COUNTER"}),
            )))
            .update_expression("SET active_count = active_count - :one, updated_at = :now")
            .condition_expression("active_count > :zero")
            .set_expression_attribute_values(Some(attrs(
                &json!({":one":1,":zero":0,":now":timestamp}),
            )))
            .build()?;
        match self
            .client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().update(job_update).build())
            .transact_items(TransactWriteItem::builder().update(counter).build())
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|e| e.code() == Some("TransactionCanceledException")) =>
            {
                if self
                    .get(job)
                    .await?
                    .is_some_and(|v| v["slot_released"] == true)
                {
                    Ok(false)
                } else {
                    Err(error.into())
                }
            }
            Err(error) => Err(error.into()),
        }
    }
    pub async fn scan(&self, maximum: usize) -> Result<Vec<Value>> {
        let mut records = Vec::new();
        let mut next = None;
        while records.len() < maximum {
            let response = self
                .client
                .scan()
                .table_name(&self.table)
                .filter_expression("begins_with(PK, :prefix)")
                .expression_attribute_values(":prefix", A::S("JOB#".into()))
                .limit((maximum - records.len()).min(100) as i32)
                .set_exclusive_start_key(next)
                .send()
                .await?;
            for item in response.items.unwrap_or_default() {
                records.push(value(&A::M(item))?);
            }
            next = response.last_evaluated_key;
            if next.as_ref().is_none_or(|k| k.is_empty()) {
                break;
            }
        }
        Ok(records)
    }
}
