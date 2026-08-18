//! Console reporters for workflow runs.
//!
//! These live in the core crate rather than in the CLI so that every embedder
//! renders a run the same way instead of maintaining its own copy:
//!
//! * [`PrettyReporter`] — colourised, indented output for a terminal.
//! * [`PlainReporter`] — fixed-width `job-id  message` prefixes, easy to grep.
//! * [`JsonReporter`] — one JSON object per event, for machine consumption.

use std::io::{IsTerminal, Write};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::engine::{EngineResult, JobResult};
use crate::logging::{CommandStream, LogEvent, LogLevel, Reporter};
use crate::types::StepConclusion;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The single-character marker used for a conclusion in summaries.
pub fn conclusion_symbol(conclusion: &StepConclusion) -> &'static str {
    match conclusion {
        StepConclusion::Success => "✓",
        StepConclusion::Failure => "✗",
        StepConclusion::Cancelled => "◯",
        StepConclusion::Skipped => "−",
    }
}

/// A run's job results in the order they ran.
///
/// Execution order keeps a job's matrix instances together, which sorting by
/// id would not.
pub fn ordered_job_results(result: &EngineResult) -> Vec<&JobResult> {
    result.ordered()
}

/// Print the colourised end-of-run summary that pairs with [`PrettyReporter`].
pub fn print_pretty_summary(result: &EngineResult) {
    println!();
    println!("{}", paint("Summary", Color::Bold));

    let width = summary_width(result);
    for job_result in ordered_job_results(result) {
        let symbol = conclusion_symbol(&job_result.conclusion);
        let status = match job_result.conclusion {
            StepConclusion::Success => paint(symbol, Color::Green),
            StepConclusion::Failure => paint(symbol, Color::Red),
            StepConclusion::Cancelled | StepConclusion::Skipped => paint(symbol, Color::Yellow),
        };
        let line = format!(
            "  {} {:<width$} {}",
            status,
            job_result.instance_id,
            summary_label(job_result),
            width = width
        );
        println!("{}", line.trim_end());
    }
}

/// Print the end-of-run summary that pairs with [`PlainReporter`].
pub fn print_plain_summary(result: &EngineResult) {
    println!();
    println!("summary   {}", result.workflow_name);

    let width = summary_width(result);
    for job_result in ordered_job_results(result) {
        let status = conclusion_symbol(&job_result.conclusion);
        let line = format!(
            "summary   {} {:<width$} {}",
            status,
            job_result.instance_id,
            summary_label(job_result),
            width = width
        );
        println!("{}", line.trim_end());
    }
}

/// The trailing label in a summary line, blank when the job has no name of
/// its own and would just repeat the id.
fn summary_label(job_result: &JobResult) -> &str {
    if job_result.job_name == job_result.instance_id {
        ""
    } else {
        &job_result.job_name
    }
}

/// Column width for the instance id in a summary — matrix instance ids are
/// much longer than plain job ids, so the column adapts.
fn summary_width(result: &EngineResult) -> usize {
    result
        .job_results
        .keys()
        .map(|id| id.chars().count())
        .max()
        .unwrap_or(12)
        .max(12)
}

/// Turn `exit status: 1` into the shorter `exit 1`.
fn short_status(status: &str) -> String {
    status
        .strip_prefix("exit status: ")
        .map(|code| format!("exit {}", code))
        .unwrap_or_else(|| status.to_string())
}

/// The shell prefix shown before a command, empty for the default shell.
fn shell_label(shell: &str) -> String {
    if shell == "bash" {
        String::new()
    } else {
        format!(" [{}]", shell)
    }
}

enum Color {
    Bold,
    Dim,
    Green,
    Yellow,
    Red,
    Blue,
    Cyan,
    Magenta,
}

/// Wrap text in an ANSI colour, unless stdout is not a terminal.
fn paint(text: &str, color: Color) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    let code = match color {
        Color::Bold => "1",
        Color::Dim => "2",
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Red => "31",
        Color::Blue => "34",
        Color::Cyan => "36",
        Color::Magenta => "35",
    };
    format!("\x1b[{}m{}\x1b[0m", code, text)
}

// ---------------------------------------------------------------------------
// Plain
// ---------------------------------------------------------------------------

/// Prefixes every line with the job id, padded to a fixed width.
#[derive(Default)]
pub struct PlainReporter {
    state: Mutex<PlainState>,
}

struct PlainState {
    current_job: String,
    width: usize,
}

impl Default for PlainState {
    fn default() -> Self {
        Self {
            current_job: "workflow".to_string(),
            width: 9,
        }
    }
}

fn print_prefixed(prefix: &str, width: usize, args: std::fmt::Arguments<'_>) {
    println!("{:<width$} {}", prefix, args, width = width);
}

#[async_trait]
impl Reporter for PlainReporter {
    async fn emit(&self, event: LogEvent) {
        let mut state = self.state.lock().await;
        match event {
            LogEvent::WorkflowStarted {
                workflow_name,
                event_name,
            } => {
                print_prefixed(
                    "workflow",
                    state.width,
                    format_args!("{} · {}", workflow_name, event_name),
                );
            }
            LogEvent::ExecutionPlan { layers } => {
                for layer in &layers {
                    for job_id in layer {
                        state.width = state.width.max(job_id.len());
                    }
                }
                let plan = layers
                    .iter()
                    .map(|layer| layer.join(", "))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                print_prefixed("plan", state.width, format_args!("{}", plan));
            }
            LogEvent::JobStarted { job_id, job_name } => {
                state.current_job = job_id.clone();
                println!();
                print_prefixed(&job_id, state.width, format_args!("{}", job_name));
            }
            LogEvent::JobSkipped {
                job_id, condition, ..
            } => {
                print_prefixed(&job_id, state.width, format_args!("skipped {}", condition));
            }
            LogEvent::JobCancelled { job_id, reason, .. } => {
                print_prefixed(&job_id, state.width, format_args!("cancelled {}", reason));
            }
            LogEvent::JobFinished {
                job_id, success, ..
            } => {
                let status = if success {
                    "✓ job done"
                } else {
                    "✗ job failed"
                };
                print_prefixed(&job_id, state.width, format_args!("{}", status));
            }
            LogEvent::StepStarted { step_name, .. } => {
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("{}", step_name),
                );
            }
            LogEvent::StepSkipped { condition, .. } => {
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("skipped {}", condition),
                );
            }
            LogEvent::ActionStarted { uses } => {
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("uses {}", uses),
                );
            }
            LogEvent::ActionInput { name, value } => {
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("with {}={}", name, value),
                );
            }
            LogEvent::ActionFinished {
                success,
                conclusion,
            } => {
                let status = if success { "✓ done" } else { "✗ failed" };
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("{} {}", status, conclusion_symbol(&conclusion)),
                );
            }
            LogEvent::ActionError { message } => {
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("action error: {}", message),
                );
            }
            LogEvent::CommandStarted { command, shell, .. } => {
                let mut lines = command.lines();
                if let Some(first_line) = lines.next() {
                    print_prefixed(
                        &state.current_job,
                        state.width,
                        format_args!("${} {}", shell_label(&shell), first_line),
                    );
                }
                for line in lines {
                    print_prefixed(&state.current_job, state.width, format_args!("  {}", line));
                }
            }
            LogEvent::CommandOutput { stream, line } => {
                let marker = match stream {
                    CommandStream::Stdout | CommandStream::Stderr => "│",
                };
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("{} {}", marker, line),
                );
            }
            LogEvent::CommandFinished { success, status } => {
                let marker = if success { "✓ done" } else { "✗ failed" };
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("{} {}", marker, short_status(&status)),
                );
            }
            LogEvent::Message { level, message } => {
                let level = match level {
                    LogLevel::Info => "info",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                print_prefixed(
                    &state.current_job,
                    state.width,
                    format_args!("{}: {}", level, message),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pretty
// ---------------------------------------------------------------------------

/// Colourised, indented output intended for a terminal.
#[derive(Default)]
pub struct PrettyReporter {
    state: Mutex<PrettyState>,
}

#[derive(Default)]
struct PrettyState {
    current_job: String,
}

#[async_trait]
impl Reporter for PrettyReporter {
    async fn emit(&self, event: LogEvent) {
        let mut state = self.state.lock().await;
        match event {
            LogEvent::WorkflowStarted {
                workflow_name,
                event_name,
            } => {
                println!(
                    "{} {} {}",
                    paint("●", Color::Cyan),
                    paint(&workflow_name, Color::Bold),
                    paint(&format!("· {}", event_name), Color::Dim)
                );
            }
            LogEvent::ExecutionPlan { layers } => {
                let plan = layers
                    .iter()
                    .map(|layer| layer.join(", "))
                    .collect::<Vec<_>>()
                    .join(" → ");
                println!("  {} {}", paint("plan", Color::Dim), plan);
            }
            LogEvent::JobStarted { job_id, job_name } => {
                state.current_job = job_id;
                println!();
                println!(
                    "{} {} {}",
                    paint("◆", Color::Blue),
                    paint(&state.current_job, Color::Bold),
                    paint(&job_name, Color::Dim)
                );
            }
            LogEvent::JobSkipped {
                job_id, condition, ..
            } => {
                println!(
                    "  {} {} {}",
                    paint("−", Color::Yellow),
                    paint(&job_id, Color::Bold),
                    paint(&format!("skipped {}", condition), Color::Dim)
                );
            }
            LogEvent::JobCancelled { job_id, reason, .. } => {
                println!(
                    "  {} {} {}",
                    paint("◯", Color::Yellow),
                    paint(&job_id, Color::Bold),
                    paint(&format!("cancelled ({})", reason), Color::Dim)
                );
            }
            LogEvent::JobFinished { success, .. } => {
                if success {
                    println!(
                        "  {} {}",
                        paint("✓", Color::Green),
                        paint("job done", Color::Dim)
                    );
                } else {
                    println!(
                        "  {} {}",
                        paint("✗", Color::Red),
                        paint("job failed", Color::Red)
                    );
                }
            }
            LogEvent::StepStarted { step_name, .. } => {
                println!(
                    "  {} {}",
                    paint("›", Color::Magenta),
                    paint(&step_name, Color::Bold)
                );
            }
            LogEvent::StepSkipped { condition, .. } => {
                println!("    {} skipped {}", paint("−", Color::Yellow), condition);
            }
            LogEvent::ActionStarted { uses } => {
                println!("    {} {}", paint("uses", Color::Dim), uses);
            }
            LogEvent::ActionInput { name, value } => {
                println!("    {} {}={}", paint("with", Color::Dim), name, value);
            }
            LogEvent::ActionFinished { success, .. } => {
                if success {
                    println!(
                        "    {} {}",
                        paint("✓", Color::Green),
                        paint("done", Color::Dim)
                    );
                } else {
                    println!(
                        "    {} {}",
                        paint("✗", Color::Red),
                        paint("failed", Color::Red)
                    );
                }
            }
            LogEvent::ActionError { message } => {
                println!("    {} {}", paint("error", Color::Red), message);
            }
            LogEvent::CommandStarted { command, shell, .. } => {
                let mut lines = command.lines();
                if let Some(first_line) = lines.next() {
                    println!(
                        "    {}{} {}",
                        paint("$", Color::Green),
                        shell_label(&shell),
                        first_line
                    );
                }
                for line in lines {
                    println!("      {}", line);
                }
            }
            LogEvent::CommandOutput { stream, line } => {
                let pipe = match stream {
                    CommandStream::Stdout => paint("│", Color::Dim),
                    CommandStream::Stderr => paint("│", Color::Yellow),
                };
                println!("    {} {}", pipe, line);
            }
            LogEvent::CommandFinished { success, status } => {
                if success {
                    println!(
                        "    {} {}",
                        paint("✓", Color::Green),
                        paint(&short_status(&status), Color::Dim)
                    );
                } else {
                    println!(
                        "    {} {}",
                        paint("✗", Color::Red),
                        paint(&short_status(&status), Color::Red)
                    );
                }
            }
            LogEvent::Message { level, message } => {
                let label = match level {
                    LogLevel::Info => paint("info", Color::Dim),
                    LogLevel::Warn => paint("warn", Color::Yellow),
                    LogLevel::Error => paint("error", Color::Red),
                };
                println!("    {} {}", label, message);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Emits one JSON object per event on stdout, flushed as it goes.
#[derive(Default)]
pub struct JsonReporter {
    output: Mutex<()>,
}

#[async_trait]
impl Reporter for JsonReporter {
    async fn emit(&self, event: LogEvent) {
        let _guard = self.output.lock().await;
        match serde_json::to_string(&event) {
            Ok(line) => println!("{}", line),
            Err(e) => eprintln!("failed to serialize log event: {}", e),
        }
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_exit_status() {
        assert_eq!(short_status("exit status: 1"), "exit 1");
        assert_eq!(short_status("signal: 9"), "signal: 9");
    }

    #[test]
    fn hides_the_label_for_the_default_shell() {
        assert_eq!(shell_label("bash"), "");
        assert_eq!(shell_label("python"), " [python]");
    }

    #[tokio::test]
    async fn reporters_accept_every_event() {
        // A smoke test that the match arms stay exhaustive and nothing panics.
        let events = vec![
            LogEvent::WorkflowStarted {
                workflow_name: "wf".into(),
                event_name: "push".into(),
            },
            LogEvent::ExecutionPlan {
                layers: vec![vec!["a".into()]],
            },
            LogEvent::JobStarted {
                job_id: "a".into(),
                job_name: "A".into(),
            },
            LogEvent::StepStarted {
                job_id: "a".into(),
                step_index: 0,
                step_name: "s".into(),
            },
            LogEvent::CommandStarted {
                command: "echo hi\necho there".into(),
                shell: "bash".into(),
                working_dir: "/tmp".into(),
            },
            LogEvent::CommandOutput {
                stream: CommandStream::Stdout,
                line: "hi".into(),
            },
            LogEvent::CommandFinished {
                success: true,
                status: "exit status: 0".into(),
            },
            LogEvent::Message {
                level: LogLevel::Warn,
                message: "careful".into(),
            },
            LogEvent::JobFinished {
                job_id: "a".into(),
                job_name: "A".into(),
                success: true,
                conclusion: StepConclusion::Success,
            },
        ];

        let plain = PlainReporter::default();
        let pretty = PrettyReporter::default();
        let json = JsonReporter::default();
        for event in events {
            plain.emit(event.clone()).await;
            pretty.emit(event.clone()).await;
            json.emit(event).await;
        }
    }
}
