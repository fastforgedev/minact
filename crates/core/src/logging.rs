//! Structured workflow execution logging.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::StepConclusion;

/// Stream a command output line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStream {
    Stdout,
    Stderr,
}

/// A structured event emitted while a workflow runs.
///
/// `Deserialize` is part of the contract: a consumer that persists events has
/// to be able to read them back, which is how run history and replay work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogEvent {
    WorkflowStarted {
        workflow_name: String,
        event_name: String,
    },
    ExecutionPlan {
        layers: Vec<Vec<String>>,
    },
    JobStarted {
        job_id: String,
        job_name: String,
    },
    JobSkipped {
        job_id: String,
        job_name: String,
        condition: String,
    },
    /// A job instance that never started, e.g. a matrix instance dropped
    /// because an earlier one failed under `fail-fast`.
    JobCancelled {
        job_id: String,
        job_name: String,
        reason: String,
    },
    JobFinished {
        job_id: String,
        job_name: String,
        success: bool,
        conclusion: StepConclusion,
    },
    StepStarted {
        job_id: String,
        step_index: usize,
        step_name: String,
    },
    StepSkipped {
        job_id: String,
        step_index: usize,
        step_name: String,
        condition: String,
    },
    ActionStarted {
        uses: String,
    },
    ActionInput {
        name: String,
        value: String,
    },
    ActionFinished {
        success: bool,
        conclusion: StepConclusion,
    },
    ActionError {
        message: String,
    },
    CommandStarted {
        command: String,
        shell: String,
        working_dir: String,
    },
    CommandOutput {
        stream: CommandStream,
        line: String,
    },
    CommandFinished {
        success: bool,
        status: String,
    },
    Message {
        level: LogLevel,
        message: String,
    },
}

/// Severity for non-structured messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Which job instance and step an event came from.
///
/// [`LogEvent`] alone cannot say: `CommandOutput` and the `Action*` variants
/// carry no identity, and inferring it from the preceding `StepStarted` only
/// works while one step runs at a time. The scope makes the correlation
/// explicit, which is what a UI needs and what parallel execution will require.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScope {
    /// The job *instance* id — `build` normally, `build (os=macos)` under a
    /// matrix — so sibling instances never share a scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
}

impl EventScope {
    pub fn job(job_id: impl Into<String>) -> Self {
        Self {
            job_id: Some(job_id.into()),
            step_index: None,
        }
    }

    pub fn step(job_id: impl Into<String>, step_index: usize) -> Self {
        Self {
            job_id: Some(job_id.into()),
            step_index: Some(step_index),
        }
    }
}

/// A [`LogEvent`] with everything needed to order it and place it in a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Position in the run, starting at 0. Survives a reconnect: a client that
    /// has seen up to `seq` asks for everything after it.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub scope: EventScope,
    pub event: LogEvent,
}

/// Receives workflow execution log events.
#[async_trait]
pub trait Reporter: Send + Sync {
    async fn emit(&self, event: LogEvent);

    /// The same event, plus its sequence number, timestamp and scope.
    ///
    /// The engine calls this; the default throws the envelope away and falls
    /// back to [`Reporter::emit`], so a console reporter needs no changes.
    /// Override it when you need ordering or job/step correlation.
    async fn emit_record(&self, record: LogRecord) {
        self.emit(record.event).await;
    }
}

/// Reporter that forwards events to tracing.
#[derive(Debug, Default)]
pub struct TracingReporter;

#[async_trait]
impl Reporter for TracingReporter {
    async fn emit(&self, event: LogEvent) {
        match event {
            LogEvent::WorkflowStarted {
                workflow_name,
                event_name,
            } => {
                tracing::info!("Starting workflow: {}", workflow_name);
                tracing::info!("Event: {}", event_name);
            }
            LogEvent::ExecutionPlan { layers } => {
                tracing::info!("Execution plan: {} layers", layers.len());
                for (i, layer) in layers.iter().enumerate() {
                    tracing::info!("  Layer {}: {} jobs", i, layer.len());
                }
            }
            LogEvent::JobStarted { job_id, job_name } => {
                tracing::info!("");
                tracing::info!("═══ Job: {} ({}) ═══", job_name, job_id);
            }
            LogEvent::JobSkipped { condition, .. } => {
                tracing::info!("  → Skipped (condition: {})", condition);
            }
            LogEvent::JobCancelled {
                job_name, reason, ..
            } => {
                tracing::warn!("  ◯ Cancelled '{}' ({})", job_name, reason);
            }
            LogEvent::JobFinished {
                job_name, success, ..
            } => {
                if success {
                    tracing::info!("✓ Job '{}' completed successfully", job_name);
                } else {
                    tracing::error!("✗ Job '{}' failed", job_name);
                }
            }
            LogEvent::StepStarted {
                step_index,
                step_name,
                ..
            } => {
                tracing::info!("");
                tracing::info!("  ── Step {}: {} ──", step_index + 1, step_name);
            }
            LogEvent::StepSkipped { condition, .. } => {
                tracing::info!("    → Skipped (condition: {})", condition);
            }
            LogEvent::ActionStarted { uses } => tracing::info!("    Using: {}", uses),
            LogEvent::ActionInput { name, value } => {
                tracing::info!("    with: {} = {}", name, value)
            }
            LogEvent::ActionFinished { success, .. } => {
                if success {
                    tracing::info!("    ✓ Action completed successfully");
                } else {
                    tracing::error!("    ✗ Action failed");
                }
            }
            LogEvent::ActionError { message } => tracing::error!("    ✗ Action error: {}", message),
            LogEvent::CommandStarted { command, shell, .. } => {
                tracing::info!("    Run: {}", command);
                tracing::info!("    Shell: {}", shell);
            }
            LogEvent::CommandOutput { stream, line } => match stream {
                CommandStream::Stdout => tracing::info!("    │ {}", line),
                CommandStream::Stderr => tracing::warn!("    │ {}", line),
            },
            LogEvent::CommandFinished { success, status } => {
                if success {
                    tracing::info!("    ✓ Command completed with status: {}", status);
                } else {
                    tracing::error!("    ✗ Command failed with status: {}", status);
                }
            }
            LogEvent::Message { level, message } => match level {
                LogLevel::Info => tracing::info!("{}", message),
                LogLevel::Warn => tracing::warn!("{}", message),
                LogLevel::Error => tracing::error!("{}", message),
            },
        }
    }
}

/// Reporter that intentionally discards all events.
#[derive(Debug, Default)]
pub struct NoopReporter;

#[async_trait]
impl Reporter for NoopReporter {
    async fn emit(&self, _event: LogEvent) {}
}
