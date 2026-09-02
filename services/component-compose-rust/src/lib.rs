//! aqw-component-compose library facade.
//!
//! The binary target (`main.rs`) selects the Lambda runtime or the local
//! `local-compose` CLI; the library modules are also exposed so integration
//! tests can reuse the worker's PNG/contract helpers.

pub mod compositor;
pub mod contract;
pub mod encode;
pub mod error;
pub mod local;
pub mod png;
pub mod storage;
pub mod telemetry;
pub mod worker;
