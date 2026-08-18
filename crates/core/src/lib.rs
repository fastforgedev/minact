//! minact-core: The core workflow execution engine.
//!
//! Provides:
//! - Workflow YAML parsing and discovery
//! - Expression evaluation (`${{ }}`)
//! - Job DAG scheduling
//! - Built-in actions (checkout, cache, upload/download artifact)
//! - Actions fetched from GitHub: JavaScript, composite and container
//! - Full workflow execution engine
//! - Ready-made console reporters (pretty, plain, JSON)

pub mod actions;
pub mod commands;
pub mod config;
pub mod engine;
pub mod executor;
pub mod expr;
pub mod logging;
pub mod matrix;
pub mod parser;
pub mod reporters;
pub mod scheduler;
pub mod types;
pub mod workflow;

pub use actions::{ActionManifest, ActionRef, ActionRegistry, ActionRuns, ActionStore};
pub use commands::{parse_key_value_file, parse_path_file, parse_workflow_command};
pub use config::{Config, RunnerSpec, DEFAULT_CONFIG_FILES};
pub use engine::{Engine, EngineResult};
pub use executor::{Executor, OutputSink, StepOutcome, StepRequest};
pub use logging::*;
pub use matrix::{expand as expand_matrix, MatrixCombination};
pub use parser::{SearchPath, WorkflowParser};
pub use reporters::{
    conclusion_symbol, ordered_job_results, print_plain_summary, print_pretty_summary,
    JsonReporter, PlainReporter, PrettyReporter,
};
pub use scheduler::JobScheduler;

/// Re-exported so cancelling a run does not force callers to depend on
/// `tokio-util` themselves.
pub use tokio_util::sync::CancellationToken;
pub use types::*;
