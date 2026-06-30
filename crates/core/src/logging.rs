//! Structured workflow execution logging.

use async_trait::async_trait;
use serde::Serialize;

use crate::types::StepConclusion;

/// Stream a command output line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStream {
    Stdout,
    Stderr,
}

/// A structured event emitted while a workflow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Receives workflow execution log events.
#[async_trait]
pub trait Reporter: Send + Sync {
    async fn emit(&self, event: LogEvent);
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
