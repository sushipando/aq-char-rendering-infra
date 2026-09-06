//! Rust orchestration, immutable SVG export, and asynchronous resvg bounds.
pub mod bounds;
pub mod components;
pub mod config;
pub mod contract;
pub mod control;
pub mod export;
pub mod finalize;
pub mod finish;
pub mod geometry;
pub mod jobs;
pub mod model;
pub mod queue;
pub mod resolve;
pub mod script;
pub mod store;
pub mod swf;
pub mod timeline;
pub mod webp;

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

pub fn digest<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    // Value's default BTreeMap gives stable recursive object ordering.
    Ok(sha256(&serde_json::to_vec(&serde_json::to_value(value)?)?))
}

pub fn log(event: &str, fields: serde_json::Value) {
    let mut value = fields;
    value["event"] = event.into();
    println!("{value}");
}
