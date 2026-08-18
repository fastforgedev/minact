//! The event envelope and cancellation.
//!
//! These are the two things a live UI needs from the engine: enough identity
//! on each event to place it in the run, and a way to stop.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use minact_core::{
    CancellationToken, CommandStream, Engine, EventScope, LogEvent, LogRecord, Reporter,
    StepConclusion, WorkflowParser,
};

/// Collects the full envelope, unlike `common::CollectingReporter` which only
/// implements `emit` — that one doubles as the proof the default forwarding
/// still works for reporters that never opted in.
#[derive(Default)]
struct RecordingReporter {
    records: Mutex<Vec<LogRecord>>,
}

#[async_trait]
impl Reporter for RecordingReporter {
    async fn emit(&self, _event: LogEvent) {
        unreachable!("the engine emits records; `emit` is only the fallback")
    }

    async fn emit_record(&self, record: LogRecord) {
        self.records.lock().unwrap().push(record);
    }
}

impl RecordingReporter {
    fn records(&self) -> Vec<LogRecord> {
        self.records.lock().unwrap().clone()
    }
}

async fn record(yaml: &str) -> (bool, Vec<LogRecord>) {
    let dir = tempfile::tempdir().expect("temp workspace");
    let workflow = WorkflowParser::parse_yaml(yaml, None).expect("workflow should parse");
    let reporter = Arc::new(RecordingReporter::default());
    let engine = Engine::with_reporter(dir.path().to_path_buf(), reporter.clone());

    let result = engine
        .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
        .await
        .expect("workflow should run");

    (result.success, reporter.records())
}

#[tokio::test]
async fn records_are_sequential_and_timestamped() {
    let (_, records) = record(
        r#"
name: Envelope
on: workflow_dispatch
jobs:
  one:
    steps:
      - run: echo first
      - run: echo second
"#,
    )
    .await;

    assert!(records.len() > 4);

    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq must be dense and in order");
    }

    let timestamps: Vec<_> = records.iter().map(|r| r.ts).collect();
    assert!(
        timestamps.windows(2).all(|pair| pair[0] <= pair[1]),
        "timestamps must not go backwards",
    );
}

#[tokio::test]
async fn command_output_carries_the_job_and_step_it_came_from() {
    let (_, records) = record(
        r#"
name: Scoped
on: workflow_dispatch
jobs:
  build:
    steps:
      - run: echo from-step-one
      - run: echo from-step-two
"#,
    )
    .await;

    let output_scopes: Vec<(&EventScope, &str)> = records
        .iter()
        .filter_map(|record| match &record.event {
            LogEvent::CommandOutput {
                stream: CommandStream::Stdout,
                line,
            } => Some((&record.scope, line.as_str())),
            _ => None,
        })
        .collect();

    assert_eq!(output_scopes.len(), 2);

    // Without this the only way to attribute a line is "whatever step started
    // most recently", which stops being true the moment jobs run in parallel.
    assert_eq!(output_scopes[0].0.job_id.as_deref(), Some("build"));
    assert_eq!(output_scopes[0].0.step_index, Some(0));
    assert_eq!(output_scopes[0].1, "from-step-one");

    assert_eq!(output_scopes[1].0.step_index, Some(1));
    assert_eq!(output_scopes[1].1, "from-step-two");
}

#[tokio::test]
async fn matrix_instances_get_their_own_scope() {
    let (_, records) = record(
        r#"
name: Matrix
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        target: [alpha, beta]
    steps:
      - run: echo ${{ matrix.target }}
"#,
    )
    .await;

    let scopes: Vec<String> = records
        .iter()
        .filter(|record| matches!(record.event, LogEvent::CommandOutput { .. }))
        .filter_map(|record| record.scope.job_id.clone())
        .collect();

    assert_eq!(scopes.len(), 2);
    assert_ne!(
        scopes[0], scopes[1],
        "sibling matrix instances must not share a scope",
    );
    assert!(scopes.iter().all(|scope| scope.starts_with("build")));
}

#[tokio::test]
async fn workflow_scoped_events_have_no_job() {
    let (_, records) = record(
        r#"
name: Plan
on: workflow_dispatch
jobs:
  one:
    steps:
      - run: echo hi
"#,
    )
    .await;

    let started = records
        .iter()
        .find(|record| matches!(record.event, LogEvent::WorkflowStarted { .. }))
        .expect("workflow_started");

    assert_eq!(started.scope, EventScope::default());
}

#[tokio::test]
async fn records_round_trip_through_json() {
    let (_, records) = record(
        r#"
name: Persisted
on: workflow_dispatch
jobs:
  one:
    steps:
      - run: echo persist-me
"#,
    )
    .await;

    // This is what run history rests on: events written to disk have to come
    // back as the same events.
    let lines: Vec<String> = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize"))
        .collect();

    let parsed: Vec<LogRecord> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("deserialize"))
        .collect();

    assert_eq!(parsed, records);
}

#[tokio::test]
async fn cancelling_stops_a_running_command_and_the_steps_after_it() {
    let dir = tempfile::tempdir().expect("temp workspace");
    let workflow = WorkflowParser::parse_yaml(
        r#"
name: Long
on: workflow_dispatch
jobs:
  slow:
    steps:
      - run: sleep 30
      - run: echo never-runs
"#,
        None,
    )
    .expect("workflow should parse");

    let reporter = Arc::new(RecordingReporter::default());
    let engine = Engine::with_reporter(dir.path().to_path_buf(), reporter.clone());
    let cancel = CancellationToken::new();

    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });

    let started = Instant::now();
    let result = engine
        .run_workflow_cancellable(&workflow, "workflow_dispatch", HashMap::new(), cancel)
        .await
        .expect("cancelled run still returns a result");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "cancelling must kill `sleep 30`, not wait it out (took {:?})",
        elapsed,
    );
    assert!(!result.success, "a cancelled run is not a successful one");
    assert_eq!(
        result.job_results["slow"].conclusion,
        StepConclusion::Cancelled,
    );

    let records = reporter.records();
    assert!(
        !records.iter().any(
            |record| matches!(&record.event, LogEvent::CommandOutput { line, .. }
                if line == "never-runs")
        ),
        "steps after the cancellation point must not run",
    );
}

#[tokio::test]
async fn cancelling_before_the_first_job_cancels_every_job() {
    let dir = tempfile::tempdir().expect("temp workspace");
    let workflow = WorkflowParser::parse_yaml(
        r#"
name: Never
on: workflow_dispatch
jobs:
  first:
    steps:
      - run: echo one
  second:
    needs: [first]
    steps:
      - run: echo two
"#,
        None,
    )
    .expect("workflow should parse");

    let reporter = Arc::new(RecordingReporter::default());
    let engine = Engine::with_reporter(dir.path().to_path_buf(), reporter.clone());

    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = engine
        .run_workflow_cancellable(&workflow, "workflow_dispatch", HashMap::new(), cancel)
        .await
        .expect("cancelled run still returns a result");

    assert!(!result.success);
    for job in result.job_results.values() {
        assert_eq!(job.conclusion, StepConclusion::Cancelled);
    }

    let reasons: Vec<String> = reporter
        .records()
        .into_iter()
        .filter_map(|record| match record.event {
            LogEvent::JobCancelled { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();

    // Reported as cancelled, not silently skipped — the distinction is what
    // tells a reader whether the run stopped or the condition said no.
    assert_eq!(reasons, vec!["cancelled", "cancelled"]);
}
