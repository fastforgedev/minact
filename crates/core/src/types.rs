//! Core type definitions for the minact workflow system.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Represents the execution context available during workflow runs.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub github: GithubContext,
    pub env: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
    pub job_outputs: HashMap<String, HashMap<String, String>>,
    /// Conclusion of each finished job, exposed as `needs.<job_id>.result`.
    pub job_results: HashMap<String, StepConclusion>,
    pub step_outputs: HashMap<String, HashMap<String, String>>,
    /// Outcome/conclusion of each finished step in the current job,
    /// exposed as `steps.<step_id>.outcome` / `steps.<step_id>.conclusion`.
    pub step_status: HashMap<String, StepStatus>,
    pub inputs: HashMap<String, String>,
    pub runner: RunnerContext,
    /// The current matrix combination, exposed as `${{ matrix.* }}`.
    /// Empty for jobs without a `strategy.matrix`.
    pub matrix: HashMap<String, Value>,
    /// Information about the current job's matrix run, exposed as
    /// `${{ strategy.* }}`.
    pub strategy: StrategyContext,
    /// Drives `success()`, `failure()` and `cancelled()` at the point the
    /// current `if:` condition is evaluated.
    pub status: RunStatus,
}

/// The `strategy` context of a matrix job.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyContext {
    /// Whether a failure cancels the remaining matrix instances.
    pub fail_fast: bool,
    /// Zero-based index of this instance within the matrix.
    pub job_index: usize,
    /// How many instances the matrix expanded to.
    pub job_total: usize,
    /// The configured `max-parallel`, if any.
    pub max_parallel: Option<u64>,
}

impl Default for StrategyContext {
    fn default() -> Self {
        Self {
            fail_fast: true,
            job_index: 0,
            job_total: 1,
            max_parallel: None,
        }
    }
}

/// The status the `success()` / `failure()` / `cancelled()` functions report.
///
/// These are not mutually exclusive in GitHub Actions semantics: when a job
/// depends on a *skipped* job, neither `success()` nor `failure()` holds, so a
/// job with no `if:` (implicitly `success()`) is skipped while one with
/// `if: always()` still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStatus {
    pub success: bool,
    pub failure: bool,
    pub cancelled: bool,
}

impl Default for RunStatus {
    fn default() -> Self {
        Self::success()
    }
}

impl RunStatus {
    /// Everything so far succeeded.
    pub const fn success() -> Self {
        Self {
            success: true,
            failure: false,
            cancelled: false,
        }
    }

    /// Something so far failed.
    pub const fn failure() -> Self {
        Self {
            success: false,
            failure: true,
            cancelled: false,
        }
    }

    /// Nothing failed, but not everything succeeded either (e.g. a skipped
    /// dependency). Only `always()` holds in this state.
    pub const fn neutral() -> Self {
        Self {
            success: false,
            failure: false,
            cancelled: false,
        }
    }

    /// Derive the status from the conclusions of a job's dependencies.
    pub fn from_conclusions<'a>(conclusions: impl IntoIterator<Item = &'a StepConclusion>) -> Self {
        let mut status = Self::success();
        for conclusion in conclusions {
            match conclusion {
                StepConclusion::Success => {}
                StepConclusion::Failure => {
                    status.success = false;
                    status.failure = true;
                }
                StepConclusion::Cancelled => {
                    status.success = false;
                    status.cancelled = true;
                }
                StepConclusion::Skipped => {
                    status.success = false;
                }
            }
        }
        status
    }
}

/// The outcome and conclusion of a finished step.
///
/// `outcome` is the raw result; `conclusion` is the result after
/// `continue-on-error` is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepStatus {
    pub outcome: StepConclusion,
    pub conclusion: StepConclusion,
}

/// GitHub-like event context.
#[derive(Debug, Clone, Default)]
pub struct GithubContext {
    pub event_name: String,
    pub event: HashMap<String, serde_json::Value>,
    pub repository: String,
    pub ref_name: String,
    pub sha: String,
    pub workspace: String,
    pub action: String,
    pub actor: String,
    /// Directory the running action was checked out to, empty outside one.
    /// Composite actions lean on it to reach files they ship with.
    pub action_path: String,
    /// `owner/repo` of the running action, empty outside one.
    pub action_repository: String,
    /// The ref the running action was fetched at, empty outside one.
    pub action_ref: String,

    /// The workflow's `name:`, or its path when it has none.
    pub workflow: String,
    /// The job currently running, as `jobs.<job_id>` spells it.
    pub job: String,
    /// A number identifying this run. Unique per run rather than sequential:
    /// there is no server here handing out run numbers.
    pub run_id: String,
    pub run_number: String,
    pub run_attempt: String,
    /// `branch` or `tag`, derived from the ref.
    pub ref_type: String,
    pub ref_protected: bool,
    /// Set for pull-request events, empty otherwise.
    pub base_ref: String,
    pub head_ref: String,
    pub server_url: String,
    pub api_url: String,
    pub graphql_url: String,
    /// Path to the event payload file, when one was supplied.
    pub event_path: String,
    /// The token from the environment, if there is one. Masked in logs.
    pub token: String,
}

/// Runner context.
#[derive(Debug, Clone, Default)]
pub struct RunnerContext {
    pub os: String,
    pub arch: String,
    pub temp: String,
    pub tool_cache: String,
}

/// The host OS spelled the way GitHub Actions spells `runner.os`
/// (`Linux`, `macOS`, `Windows`).
pub fn runner_os_name() -> String {
    match std::env::consts::OS {
        "macos" => "macOS".to_string(),
        "linux" => "Linux".to_string(),
        "windows" => "Windows".to_string(),
        other => other.to_string(),
    }
}

/// The host architecture spelled the way GitHub Actions spells `runner.arch`
/// (`X86`, `X64`, `ARM`, `ARM64`).
pub fn runner_arch_name() -> String {
    match std::env::consts::ARCH {
        "x86" => "X86".to_string(),
        "x86_64" => "X64".to_string(),
        "arm" => "ARM".to_string(),
        "aarch64" => "ARM64".to_string(),
        other => other.to_string(),
    }
}

/// Runtime value type used in expression evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Map(HashMap<String, Value>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "{}", s),
            Value::Array(a) => write!(
                f,
                "[{}]",
                a.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Map(m) => {
                // Sorted, because the backing map is unordered and this
                // rendering ends up in matrix instance ids — which must be
                // stable from run to run.
                let mut items: Vec<String> =
                    m.iter().map(|(k, v)| format!("{}: {}", k, v)).collect();
                items.sort();
                write!(f, "{{{}}}", items.join(", "))
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

/// The result of running a step or action.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub success: bool,
    pub conclusion: StepConclusion,
    pub outputs: HashMap<String, String>,
    pub artifacts: Vec<Artifact>,
}

/// Conclusion status of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl StepConclusion {
    /// The string form used by `needs.<job_id>.result` and `steps.<id>.outcome`.
    pub fn as_str(&self) -> &'static str {
        match self {
            StepConclusion::Success => "success",
            StepConclusion::Failure => "failure",
            StepConclusion::Cancelled => "cancelled",
            StepConclusion::Skipped => "skipped",
        }
    }
}

/// An artifact produced by a step.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub name: String,
    pub path: std::path::PathBuf,
}

impl Default for StepResult {
    fn default() -> Self {
        Self {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: Vec::new(),
        }
    }
}

/// Errors that can occur during workflow execution.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("Workflow parse error: {0}")]
    ParseError(String),
    #[error("Job '{0}' not found")]
    JobNotFound(String),
    #[error("Step '{0}' failed: {1}")]
    StepFailed(String, String),
    #[error("Expression evaluation error: {0}")]
    ExpressionError(String),
    #[error("Action '{0}' not found")]
    ActionNotFound(String),
    #[error("Circular dependency detected in jobs")]
    CircularDependency,
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
