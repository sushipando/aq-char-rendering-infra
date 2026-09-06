//! One export invocation per independently placed symbol. Keep the complete
//! source's normalization context so shared nested timelines resolve identically
//! to the previous batched export.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub fn partition(sources: Vec<Value>) -> Result<Vec<Value>> {
    let mut units = Vec::new();
    let mut keys = BTreeSet::new();
    for (group, source) in sources.into_iter().enumerate() {
        let requests = source["requests"].as_array().context("missing export requests")?;
        ensure!(!requests.is_empty(), "empty source export");
        for request in requests {
            let key = request["key"].as_str().context("missing symbol key")?;
            ensure!(keys.insert(key.to_owned()), "duplicate export symbol {key}");
            let mut unit = source.clone();
            unit["idx"] = units.len().into();
            unit["source_group_idx"] = group.into();
            unit["requests"] = json!([request]);
            if requests.len() > 1 {
                unit["normalization_requests"] = json!(requests);
            }
            units.push(unit);
        }
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armor_and_helm_parts_are_independent_with_shared_timeline_context() {
        let armor = json!({"idx":0,"key":"armor.swf","sha256":"armor","requests":[{"key":"armor_chest"},{"key":"armor_head"}]});
        let helm = json!({"idx":1,"key":"helm.swf","sha256":"helm","requests":[{"key":"helm"},{"key":"backhair"}]});
        let cape = json!({"idx":2,"key":"cape.swf","sha256":"cape","requests":[{"key":"cape"}]});
        let units = partition(vec![armor.clone(), helm.clone(), cape.clone()]).unwrap();
        assert_eq!(units.len(), 5);
        for (index, unit) in units.iter().enumerate() {
            assert_eq!(unit["idx"], index);
            assert_eq!(unit["requests"].as_array().unwrap().len(), 1);
        }
        assert_eq!(units[0]["normalization_requests"], armor["requests"]);
        assert_eq!(units[1]["normalization_requests"], armor["requests"]);
        assert_eq!(units[2]["normalization_requests"], helm["requests"]);
        assert_eq!(units[3]["normalization_requests"], helm["requests"]);
        assert_eq!(units[4]["requests"], cape["requests"]);
        assert!(units[4].get("normalization_requests").is_none());
        assert_eq!(units[0]["sha256"], units[1]["sha256"]);
        assert_ne!(units[0]["requests"], units[1]["requests"]);
    }

    #[test]
    fn duplicate_symbols_and_empty_sources_are_rejected() {
        assert!(partition(vec![json!({"requests":[]})]).is_err());
        let source = json!({"requests":[{"key":"head"},{"key":"head"}]});
        assert!(partition(vec![source]).is_err());
    }
}
