#![forbid(unsafe_code)]
//! Optional cooperating-process execution for original izu workspaces.
//!
//! Source history remains the engine's responsibility. This crate admits bounded
//! work, binds writers to an explicit engine workspace, owns their process groups,
//! and persists observations. It is not a security sandbox.

mod engine;
mod environment;
mod os;
mod registry;
mod runtime;
mod scheduler;
mod types;
mod worker;

pub use engine::{CheckExecution, CloseReceipt, run_check};
pub use runtime::Runtime;
pub use types::*;
pub use worker::worker_main;
