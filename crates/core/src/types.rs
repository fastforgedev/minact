//! Core type definitions for the minact workflow system.

use std::collections::HashMap;

use serde::Serialize;

/// Represents the execution context available during workflow runs.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub github: GithubContext,
    pub env: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
    pub job_outputs: HashMap<String, HashMap<String, String>>,
    pub step_outputs: HashMap<String, HashMap<String, String>>,
    pub inputs: HashMap<String, String>,
    pub runner: RunnerContext,
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
}

/// Runner context.
#[derive(Debug, Clone, Default)]
pub struct RunnerContext {
    pub os: String,
    pub arch: String,
    pub temp: String,
    pub tool_cache: String,
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

impl Value {
    pub fn to_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::String(s) => s.clone(),
            Value::Array(a) => format!(
                "[{}]",
                a.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Map(m) => {
                let items: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.to_string()))
                    .collect();
                format!("{{{}}}", items.join(", "))
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
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
