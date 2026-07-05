//! Workflow model types representing the YAML workflow schema.
//!
//! Mirrors the GitHub Actions workflow YAML structure:
//! - `name`, `on` (triggers), `env`, `jobs`, `defaults`

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Top-level workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// Human-readable workflow name.
    #[serde(default)]
    pub name: String,

    /// Event triggers that activate this workflow.
    #[serde(default)]
    pub on: OnConfig,

    /// Global environment variables available to all jobs.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Default settings applied to all jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Defaults>,

    /// The set of jobs in this workflow.
    pub jobs: HashMap<String, Job>,

    /// The file path this workflow was loaded from (not serialized).
    #[serde(skip)]
    pub file_path: Option<std::path::PathBuf>,
}

/// Event trigger configuration.
///
/// Supports three YAML forms:
/// - String: `on: push`
/// - Sequence: `on: [push, pull_request]`
/// - Map: `on: { push: { branches: [main] } }`
#[derive(Debug, Clone, Default, Serialize)]
pub struct OnConfig {
    /// Push event trigger.
    pub push: Option<PushConfig>,

    /// Pull request event trigger.
    pub pull_request: Option<BranchFilter>,

    /// Release event trigger.
    pub release: Option<ReleaseConfig>,

    /// Manual workflow dispatch trigger.
    pub workflow_dispatch: Option<WorkflowDispatchConfig>,

    /// Scheduled trigger via cron.
    pub schedule: Option<Vec<ScheduleConfig>>,

    /// Additional event triggers stored as raw map for extensibility.
    pub extra: HashMap<String, serde_yaml::Value>,
}

impl<'de> Deserialize<'de> for OnConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        // First deserialize as a generic YAML value
        let value = serde_yaml::Value::deserialize(deserializer)?;

        match value {
            // Map form: `on: { push: { branches: [main] }, workflow_dispatch: ~ }`
            serde_yaml::Value::Mapping(map) => {
                let mut config = OnConfig::default();

                for (key, val) in map {
                    let key_str = key.as_str().unwrap_or("");
                    match key_str {
                        "push" => {
                            if val.is_mapping() || val.is_null() {
                                config.push = serde_yaml::from_value(val).ok();
                            }
                        }
                        "pull_request" => {
                            if val.is_mapping() || val.is_null() {
                                config.pull_request = serde_yaml::from_value(val).ok();
                            }
                        }
                        "release" => {
                            config.release = serde_yaml::from_value(val).ok();
                        }
                        "workflow_dispatch" => {
                            if val.is_mapping() || val.is_null() {
                                config.workflow_dispatch = serde_yaml::from_value(val).ok();
                            }
                        }
                        "schedule" => {
                            config.schedule = serde_yaml::from_value(val).ok();
                        }
                        _ => {
                            config.extra.insert(key_str.to_string(), val);
                        }
                    }
                }

                Ok(config)
            }
            // String form: `on: push`
            serde_yaml::Value::String(event) => {
                let mut config = OnConfig::default();
                match event.as_str() {
                    "push" => config.push = Some(PushConfig::default()),
                    "pull_request" => config.pull_request = Some(BranchFilter::default()),
                    "workflow_dispatch" => config.workflow_dispatch = Some(WorkflowDispatchConfig::default()),
                    other => {
                        config.extra.insert(other.to_string(), serde_yaml::Value::Null);
                    }
                }
                Ok(config)
            }
            // Sequence form: `on: [push, pull_request]`
            serde_yaml::Value::Sequence(seq) => {
                let mut config = OnConfig::default();
                for item in seq {
                    if let Some(event) = item.as_str() {
                        match event {
                            "push" => config.push = Some(PushConfig::default()),
                            "pull_request" => config.pull_request = Some(BranchFilter::default()),
                            "workflow_dispatch" => {
                                config.workflow_dispatch = Some(WorkflowDispatchConfig::default());
                            }
                            other => {
                                config.extra.insert(other.to_string(), serde_yaml::Value::Null);
                            }
                        }
                    }
                }
                Ok(config)
            }
            _ => Err(Error::custom(format!(
                "expected a string, sequence, or map for `on`, got {:?}",
                value
            ))),
        }
    }
}

/// Configuration for push events.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PushConfig {
    /// Branch patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,

    /// Tag patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Simple branch filter (for pull_request).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchFilter {
    /// Branch patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,
}

/// Configuration for release events.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReleaseConfig {
    /// Release event types (published, created, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
}

/// Configuration for workflow_dispatch (manual triggers).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowDispatchConfig {
    /// Input parameters for manual dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<HashMap<String, DispatchInput>>,
}

/// An input parameter for workflow_dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchInput {
    /// Human-readable description.
    #[serde(default)]
    pub description: String,

    /// Whether the input is required.
    #[serde(default)]
    pub required: bool,

    /// Default value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Input type (string, boolean, choice, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub input_type: Option<String>,
}

/// Configuration for scheduled triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Cron expression.
    pub cron: String,
}

/// Default settings applied to jobs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Defaults {
    /// Default run configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunDefaults>,
}

/// Default settings for shell steps.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunDefaults {
    /// Default shell to use.
    #[serde(default)]
    pub shell: String,

    /// Default working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,
}

/// A single job definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Human-readable job name.
    #[serde(default)]
    pub name: String,

    /// List of job IDs that must complete before this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<Vec<String>>,

    /// Condition expression to determine if this job should run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// Job-level environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Job-level default settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Defaults>,

    /// The steps in this job.
    #[serde(default)]
    pub steps: Vec<Step>,

    /// Outputs from this job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<HashMap<String, String>>,

    /// The runner to use (e.g., "ubuntu-latest" — for local execution we may ignore or map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<String>,

    /// Timeout in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u64>,

    /// Strategy (matrix, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<Strategy>,
}

/// Strategy configuration (matrix, fail-fast, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    /// Matrix configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<MatrixConfig>,

    /// Whether to fail fast.
    #[serde(default = "default_true")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,

    /// Maximum parallelism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "max-parallel")]
    pub max_parallel: Option<u64>,
}

fn default_true() -> Option<bool> {
    Some(true)
}

/// Matrix configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Matrix axis values.
    #[serde(flatten)]
    pub axes: HashMap<String, Vec<serde_yaml::Value>>,

    /// Excluded combinations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<HashMap<String, serde_yaml::Value>>>,

    /// Included additional combinations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<HashMap<String, serde_yaml::Value>>>,
}

/// A single step within a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Optional step identifier for referencing outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Human-readable step name.
    #[serde(default)]
    pub name: String,

    /// An action to use (e.g., "actions/checkout@v4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,

    /// A shell command to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// The shell to use for run commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Working directory for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,

    /// Input parameters for the action.
    #[serde(default)]
    pub with: HashMap<String, String>,

    /// Environment variables for this step only.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Condition expression (`if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// Continue on error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "continue-on-error")]
    pub continue_on_error: Option<bool>,

    /// Timeout in minutes for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "timeout-minutes")]
    pub timeout_minutes: Option<u64>,
}

/// A parsed workflow ready for execution, with all expressions resolved.
#[derive(Debug, Clone)]
pub struct ResolvedWorkflow {
    pub name: String,
    pub env: HashMap<String, String>,
    pub jobs: Vec<ResolvedJob>,
}

/// A resolved job with unmet dependencies tracked.
#[derive(Debug, Clone)]
pub struct ResolvedJob {
    pub id: String,
    pub name: String,
    pub env: HashMap<String, String>,
    pub steps: Vec<Step>,
    pub needs: Vec<String>,
    pub if_condition: Option<String>,
    pub outputs: Option<HashMap<String, String>>,
    pub timeout_minutes: Option<u64>,
    pub matrix: Option<MatrixConfig>,
}

/// Supported event types for workflow dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum EventType {
    Push,
    PullRequest,
    Release,
    WorkflowDispatch,
    Schedule,
}
