//! Rendering a recorded run as plain text.
//!
//! This is the format you paste into an issue: a header saying what ran, then
//! every event with a timestamp relative to the start of the run, indented by
//! job and step. It is derived from the same records the UI renders, so it can
//! never disagree with what was on screen.

use std::fmt::Write as _;

use minact_core::{CommandStream, LogEvent, LogRecord};

use super::RunMeta;

/// Render a run's log. `job` limits the output to one job instance.
pub fn render(meta: &RunMeta, records: &[LogRecord], job: Option<&str>) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# minact run {}", meta.id);
    let _ = writeln!(
        out,
        "workflow: {} ({})",
        meta.workflow_name, meta.workflow_path
    );
    let _ = writeln!(out, "event:    {}", meta.event);
    let _ = writeln!(
        out,
        "started:  {}",
        meta.started_at.format("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    let _ = writeln!(
        out,
        "status:   {}{}",
        serde_json::to_string(&meta.status)
            .unwrap_or_default()
            .trim_matches('"'),
        match meta.duration_ms() {
            Some(ms) => format!(" ({})", human_duration(ms)),
            None => String::new(),
        }
    );

    if let Some(job) = job {
        let _ = writeln!(out, "job:      {}", job);
    }
    if let Some(error) = &meta.error {
        let _ = writeln!(out, "error:    {}", error);
    }
    let _ = writeln!(out);

    // Relative to the first event rather than to `started_at`: the run's clock
    // is what the timings in the UI are measured against.
    let origin = records.first().map(|record| record.ts);

    for record in records {
        if let Some(job) = job {
            // Events with no job (the plan, the workflow header) belong to
            // every view of the run.
            if record.scope.job_id.as_deref().is_some_and(|id| id != job) {
                continue;
            }
        }

        let Some(line) = render_event(&record.event) else {
            continue;
        };

        // Jobs get a blank line above them. Putting it in the rendered text
        // would stamp a timestamp on an empty line.
        if matches!(record.event, LogEvent::JobStarted { .. }) {
            let _ = writeln!(out);
        }

        let offset = origin
            .map(|start| (record.ts - start).num_milliseconds())
            .unwrap_or(0);

        let _ = writeln!(out, "[{}] {}", clock(offset), line);
    }

    out
}

fn render_event(event: &LogEvent) -> Option<String> {
    Some(match event {
        LogEvent::WorkflowStarted {
            workflow_name,
            event_name,
        } => format!("workflow {} ({})", workflow_name, event_name),
        LogEvent::ExecutionPlan { layers } => format!(
            "plan: {}",
            layers
                .iter()
                .map(|layer| layer.join(", "))
                .collect::<Vec<_>>()
                .join(" → ")
        ),
        LogEvent::JobStarted { job_name, .. } => format!("job {}", job_name),
        LogEvent::JobSkipped {
            job_name,
            condition,
            ..
        } => format!("job {} skipped ({})", job_name, condition),
        LogEvent::JobCancelled {
            job_name, reason, ..
        } => format!("job {} cancelled ({})", job_name, reason),
        LogEvent::JobFinished {
            job_name,
            conclusion,
            ..
        } => format!("job {} → {}", job_name, conclusion.as_str()),
        LogEvent::StepStarted {
            step_index,
            step_name,
            ..
        } => format!("  step {}: {}", step_index + 1, step_name),
        LogEvent::StepSkipped {
            step_index,
            step_name,
            condition,
            ..
        } => format!(
            "  step {}: {} skipped ({})",
            step_index + 1,
            step_name,
            condition
        ),
        LogEvent::ActionStarted { uses } => format!("    uses: {}", uses),
        LogEvent::ActionInput { name, value } => format!("    with {} = {}", name, value),
        LogEvent::ActionFinished { conclusion, .. } => {
            format!("    action → {}", conclusion.as_str())
        }
        LogEvent::ActionError { message } => format!("    action error: {}", message),
        LogEvent::CommandStarted { command, shell, .. } => {
            let command = command.trim_end();
            match command.split_once('\n') {
                Some((first, rest)) => {
                    format!("    $ {} ({})\n{}", first, shell, indent_continuation(rest))
                }
                None => format!("    $ {} ({})", command, shell),
            }
        }
        LogEvent::CommandOutput { stream, line } => match stream {
            CommandStream::Stdout => format!("    {}", line),
            CommandStream::Stderr => format!("    {} [stderr]", line),
        },
        LogEvent::CommandFinished { success, status } => {
            if *success {
                return None;
            }
            format!("    command failed: {}", status)
        }
        LogEvent::Message { level, message } => format!(
            "  {}: {}",
            match level {
                minact_core::LogLevel::Info => "info",
                minact_core::LogLevel::Warn => "warn",
                minact_core::LogLevel::Error => "error",
            },
            message.trim_end().replace('\n', "\n                ")
        ),
    })
}

/// Keep multi-line values readable under the timestamp column.
fn indent_continuation(text: &str) -> String {
    format!(
        "                {}",
        text.trim_end().replace('\n', "\n                ")
    )
}

/// `mm:ss.mmm`, counting from the first event.
fn clock(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}.{:03}",
        ms / 60_000,
        (ms / 1000) % 60,
        ms % 1000
    )
}

fn human_duration(ms: i64) -> String {
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{:.1}s", seconds);
    }
    format!("{}m {:02}s", (ms / 60_000), (ms / 1000) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use chrono::DateTime;
    use minact_core::{EventScope, StepConclusion};
    use std::collections::HashMap;

    fn meta() -> RunMeta {
        RunMeta {
            id: "7".into(),
            workflow_id: "abc".into(),
            workflow_name: "CI".into(),
            workflow_path: ".minact/workflows/ci.yml".into(),
            event: "push".into(),
            inputs: HashMap::new(),
            status: RunStatus::Success,
            started_at: DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            finished_at: DateTime::from_timestamp(1_800_000_002, 0),
            error: None,
        }
    }

    fn at(seq: u64, ms: i64, scope: EventScope, event: LogEvent) -> LogRecord {
        LogRecord {
            seq,
            ts: DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap(),
            scope,
            event,
        }
    }

    fn records() -> Vec<LogRecord> {
        vec![
            at(
                0,
                0,
                EventScope::default(),
                LogEvent::WorkflowStarted {
                    workflow_name: "CI".into(),
                    event_name: "push".into(),
                },
            ),
            at(
                1,
                10,
                EventScope::job("build"),
                LogEvent::JobStarted {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                },
            ),
            at(
                2,
                20,
                EventScope::step("build", 0),
                LogEvent::StepStarted {
                    job_id: "build".into(),
                    step_index: 0,
                    step_name: "Compile".into(),
                },
            ),
            at(
                3,
                1_250,
                EventScope::step("build", 0),
                LogEvent::CommandOutput {
                    stream: CommandStream::Stdout,
                    line: "compiled".into(),
                },
            ),
            at(
                4,
                1_260,
                EventScope::job("test"),
                LogEvent::JobStarted {
                    job_id: "test".into(),
                    job_name: "Test".into(),
                },
            ),
            at(
                5,
                1_300,
                EventScope::step("test", 0),
                LogEvent::CommandOutput {
                    stream: CommandStream::Stderr,
                    line: "a warning".into(),
                },
            ),
            at(
                6,
                2_000,
                EventScope::job("build"),
                LogEvent::JobFinished {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                    success: true,
                    conclusion: StepConclusion::Success,
                },
            ),
        ]
    }

    #[test]
    fn renders_a_header_and_relative_timestamps() {
        let text = render(&meta(), &records(), None);

        assert!(text.starts_with("# minact run 7\n"));
        assert!(text.contains("workflow: CI (.minact/workflows/ci.yml)"));
        assert!(text.contains("status:   success (2.0s)"));

        assert!(text.contains("[00:00.000] workflow CI (push)"));
        assert!(
            text.contains("\n\n[00:00.010] job Build"),
            "jobs get a blank line above"
        );
        assert!(text.contains("[00:00.020]   step 1: Compile"));
        assert!(text.contains("[00:01.250]     compiled"));
        assert!(text.contains("[00:01.300]     a warning [stderr]"));
        assert!(text.contains("[00:02.000] job Build → success"));
    }

    #[test]
    fn filtering_by_job_keeps_the_run_wide_events() {
        let text = render(&meta(), &records(), Some("build"));

        assert!(text.contains("job:      build"));
        // Belongs to the run, not to a job.
        assert!(text.contains("workflow CI (push)"));
        assert!(text.contains("compiled"));
        assert!(!text.contains("a warning"), "{}", text);
        assert!(!text.contains("job Test"), "{}", text);
    }

    #[test]
    fn multi_line_commands_stay_under_the_timestamp_column() {
        let text = render(
            &meta(),
            &[at(
                0,
                0,
                EventScope::step("build", 0),
                LogEvent::CommandStarted {
                    command: "echo one\necho two\n".into(),
                    shell: "bash".into(),
                    working_dir: "/tmp".into(),
                },
            )],
            None,
        );

        assert!(
            text.contains("[00:00.000]     $ echo one (bash)\n                echo two"),
            "{}",
            text,
        );
    }
}
