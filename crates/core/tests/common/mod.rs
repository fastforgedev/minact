//! Shared helpers for the engine integration tests.
//!
//! Each test binary compiles this module separately and uses a different
//! subset of it, so unused helpers are expected rather than dead code.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use minact_core::engine::EngineResult;
use minact_core::{CommandStream, Engine, LogEvent, LogLevel, Reporter, WorkflowParser};

/// A reporter that keeps every event so tests can assert on them.
#[derive(Default)]
pub struct CollectingReporter {
    events: Mutex<Vec<LogEvent>>,
}

#[async_trait]
impl Reporter for CollectingReporter {
    async fn emit(&self, event: LogEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl CollectingReporter {
    pub fn events(&self) -> Vec<LogEvent> {
        self.events.lock().unwrap().clone()
    }
}

/// Run a workflow from YAML in the given workspace.
pub async fn run_in(yaml: &str, workspace: &Path) -> (EngineResult, Vec<LogEvent>) {
    run_with_inputs(yaml, workspace, HashMap::new()).await
}

/// Run a workflow with `workflow_dispatch` inputs.
pub async fn run_with_inputs(
    yaml: &str,
    workspace: &Path,
    inputs: HashMap<String, String>,
) -> (EngineResult, Vec<LogEvent>) {
    let workflow = WorkflowParser::parse_yaml(yaml, None).expect("workflow should parse");
    let reporter = Arc::new(CollectingReporter::default());
    let engine = Engine::with_reporter(workspace.to_path_buf(), reporter.clone());
    let result = engine
        .run_workflow(&workflow, "workflow_dispatch", inputs)
        .await
        .expect("workflow should run");
    (result, reporter.events())
}

/// Every line the steps printed on stdout.
pub fn stdout_lines(events: &[LogEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            LogEvent::CommandOutput {
                stream: CommandStream::Stdout,
                line,
            } => Some(line.clone()),
            _ => None,
        })
        .collect()
}

/// All step output, joined, for substring assertions.
pub fn stdout(events: &[LogEvent]) -> String {
    stdout_lines(events).join("\n")
}

/// Messages emitted at the given level.
pub fn messages_at(events: &[LogEvent], wanted: LogLevel) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            LogEvent::Message { level, message } if *level == wanted => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// The names of the steps that were skipped.
pub fn skipped_steps(events: &[LogEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            LogEvent::StepSkipped { step_name, .. } => Some(step_name.clone()),
            _ => None,
        })
        .collect()
}
