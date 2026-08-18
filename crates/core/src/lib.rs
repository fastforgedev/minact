//! minact-core: The core workflow execution engine.
//!
//! Provides:
//! - Workflow YAML parsing and discovery
//! - Expression evaluation (`${{ }}`)
//! - Job DAG scheduling
//! - Built-in actions (checkout, cache, upload/download artifact)
//! - Full workflow execution engine

pub mod actions;
pub mod commands;
pub mod engine;
pub mod executor;
pub mod expr;
pub mod logging;
pub mod parser;
pub mod scheduler;
pub mod types;
pub mod workflow;

pub use actions::ActionRegistry;
pub use commands::{parse_key_value_file, parse_path_file, parse_workflow_command};
pub use engine::{Engine, EngineResult};
pub use executor::{Executor, OutputSink, StepOutcome, StepRequest};
pub use logging::*;
pub use parser::WorkflowParser;
pub use scheduler::JobScheduler;
pub use types::*;
