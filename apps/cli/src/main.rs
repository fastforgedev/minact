//! minact CLI — run GitHub Actions-compatible workflows locally.
//!
//! Usage:
//!   minact run [--file <path>] [--event <event>] [--workspace <dir>]
//!   minact list [--dir <path>]
//!   minact validate <file>

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use minact_core::{
    CommandStream, Engine, LogEvent, LogLevel, Reporter, StepConclusion, WorkflowParser,
};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(
    name = "minact",
    version,
    about = "Run GitHub Actions workflows locally"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a workflow
    Run {
        /// Path to a specific workflow file
        #[arg(short, long)]
        file: Option<PathBuf>,

        /// Event name to simulate (default: workflow_dispatch)
        #[arg(short, long, default_value = "workflow_dispatch")]
        event: String,

        /// Working directory (default: current directory)
        #[arg(short, long)]
        workspace: Option<PathBuf>,

        /// Input parameters (key=value)
        #[arg(short, long, value_parser = parse_key_val)]
        input: Vec<KeyVal>,

        /// Log output format
        #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
        log_format: LogFormat,
    },

    /// List available workflows
    List {
        /// Project directory to search
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Show detailed info
        #[arg(short, long)]
        verbose: bool,
    },

    /// Validate a workflow file
    Validate {
        /// Path to workflow file
        file: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct KeyVal {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum LogFormat {
    Pretty,
    Plain,
    Json,
}

fn parse_key_val(s: &str) -> Result<KeyVal, String> {
    let pos = s
        .find('=')
        .ok_or_else(|| format!("Invalid KEY=value format: {}", s))?;
    Ok(KeyVal {
        key: s[..pos].to_string(),
        value: s[pos + 1..].to_string(),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            file,
            event,
            workspace,
            input,
            log_format,
        } => cmd_run(file, event, workspace, input, log_format).await,
        Commands::List { dir, verbose } => cmd_list(dir, verbose),
        Commands::Validate { file } => cmd_validate(file),
    }
}

async fn cmd_run(
    file: Option<PathBuf>,
    event: String,
    workspace: Option<PathBuf>,
    input: Vec<KeyVal>,
    log_format: LogFormat,
) -> anyhow::Result<()> {
    let workspace = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let inputs: HashMap<String, String> = input.into_iter().map(|kv| (kv.key, kv.value)).collect();

    // Load workflow
    let workflow = if let Some(file_path) = file {
        if matches!(log_format, LogFormat::Plain) {
            println!("Loading workflow from: {}", file_path.display());
        }
        WorkflowParser::parse_file(&file_path)?
    } else {
        // Discover workflows in workspace
        let workflows = WorkflowParser::discover_workflows(&workspace)?;
        match workflows.len() {
            0 => {
                anyhow::bail!(
                    "No workflow files found in {}\n\
                     Looked in: .minact/workflows/, .github/workflows/, minact.yml/yaml",
                    workspace.display()
                );
            }
            1 => workflows.into_iter().next().unwrap(),
            n => {
                // Multiple workflows found; show them and ask user to specify
                eprintln!("Found {} workflows. Use --file to specify one.\n", n);
                for (i, wf) in workflows.iter().enumerate() {
                    let source = wf
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    eprintln!("  {}. {} ({})", i + 1, wf.name, source);
                }
                anyhow::bail!("Please specify a workflow file with --file");
            }
        }
    };

    // Run the workflow
    let reporter: Arc<dyn Reporter> = match log_format {
        LogFormat::Pretty => Arc::new(PrettyReporter::default()),
        LogFormat::Plain => Arc::new(PlainReporter::default()),
        LogFormat::Json => Arc::new(JsonReporter::default()),
    };
    let engine = Engine::with_reporter(workspace, reporter);

    let result = engine.run_workflow(&workflow, &event, inputs).await?;

    // Print summary
    match log_format {
        LogFormat::Pretty => print_pretty_summary(&result),
        LogFormat::Plain => print_plain_summary(&result),
        LogFormat::Json => {}
    }

    if !result.success {
        std::process::exit(1);
    }

    Ok(())
}

fn sorted_job_results(
    result: &minact_core::EngineResult,
) -> Vec<(&String, &minact_core::engine::JobResult)> {
    let mut jobs: Vec<_> = result.job_results.iter().collect();
    jobs.sort_by(|(left_id, _), (right_id, _)| left_id.cmp(right_id));
    jobs
}

fn print_pretty_summary(result: &minact_core::EngineResult) {
    println!();
    println!("{}", paint("Summary", Color::Bold));

    for (job_id, job_result) in sorted_job_results(result) {
        let status = match job_result.conclusion {
            StepConclusion::Success => paint("✓", Color::Green),
            StepConclusion::Failure => paint("✗", Color::Red),
            StepConclusion::Cancelled => paint("◯", Color::Yellow),
            StepConclusion::Skipped => paint("−", Color::Yellow),
        };
        println!("  {} {:<12} {}", status, job_id, job_result.job_name);
    }
}

fn print_plain_summary(result: &minact_core::EngineResult) {
    println!();
    println!("summary   {}", result.workflow_name);

    for (job_id, job_result) in sorted_job_results(result) {
        let status = conclusion_symbol(&job_result.conclusion);
        println!(
            "summary   {} {:<12} {}",
            status, job_id, job_result.job_name
        );
    }
}

fn conclusion_symbol(conclusion: &StepConclusion) -> &'static str {
    match conclusion {
        StepConclusion::Success => "✓",
        StepConclusion::Failure => "✗",
        StepConclusion::Cancelled => "◯",
        StepConclusion::Skipped => "–",
    }
}

#[derive(Default)]
struct PlainReporter {
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
            LogEvent::CommandStarted {
                command,
                shell,
                working_dir: _,
            } => {
                let mut lines = command.lines();
                if let Some(first_line) = lines.next() {
                    let shell = if shell == "bash" {
                        String::new()
                    } else {
                        format!(" [{}]", shell)
                    };
                    print_prefixed(
                        &state.current_job,
                        state.width,
                        format_args!("${} {}", shell, first_line),
                    );
                }
                for line in lines {
                    print_prefixed(&state.current_job, state.width, format_args!("  {}", line));
                }
            }
            LogEvent::CommandOutput { stream, line } => {
                let marker = match stream {
                    CommandStream::Stdout => "│",
                    CommandStream::Stderr => "│",
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

fn print_prefixed(prefix: &str, width: usize, args: std::fmt::Arguments<'_>) {
    println!("{:<width$} {}", prefix, args, width = width);
}

fn short_status(status: &str) -> String {
    status
        .strip_prefix("exit status: ")
        .map(|code| format!("exit {}", code))
        .unwrap_or_else(|| status.to_string())
}

#[derive(Default)]
struct PrettyReporter {
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
                    let shell = if shell == "bash" {
                        String::new()
                    } else {
                        format!(" [{}]", shell)
                    };
                    println!("    {}{} {}", paint("$", Color::Green), shell, first_line);
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

#[derive(Default)]
struct JsonReporter {
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

fn cmd_list(dir: Option<PathBuf>, verbose: bool) -> anyhow::Result<()> {
    let dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let workflows = WorkflowParser::discover_workflows(&dir)?;

    if workflows.is_empty() {
        println!("No workflows found in {}", dir.display());
        println!("Looked in: .minact/workflows/, .github/workflows/, minact.yml/yaml");
        return Ok(());
    }

    println!("Workflows in {}:", dir.display());
    println!();

    for workflow in &workflows {
        let source = workflow
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        if verbose {
            println!("  ┌─ {}", workflow.name);
            println!("  ├─ File: {}", source);
            println!("  ├─ Jobs: {}", workflow.jobs.len());
            println!("  ├─ Events:");
            if workflow.on.push.is_some() {
                println!("  │    • push");
            }
            if workflow.on.pull_request.is_some() {
                println!("  │    • pull_request");
            }
            if workflow.on.release.is_some() {
                println!("  │    • release");
            }
            if workflow.on.workflow_dispatch.is_some() {
                println!("  │    • workflow_dispatch");
            }
            if workflow.on.schedule.is_some() {
                println!("  │    • schedule");
            }
            println!("  └─ Jobs:");
            for (job_id, job) in &workflow.jobs {
                println!("       • {} ({})", job.name, job_id);
            }
            println!();
        } else {
            println!("  • {} ({})", workflow.name, source);
        }
    }

    Ok(())
}

fn cmd_validate(file: PathBuf) -> anyhow::Result<()> {
    if !file.exists() {
        anyhow::bail!("File not found: {}", file.display());
    }

    match WorkflowParser::parse_file(&file) {
        Ok(workflow) => {
            println!("✓ Valid workflow: {}", workflow.name);
            println!("  Jobs: {}", workflow.jobs.len());
            for (job_id, job) in &workflow.jobs {
                println!(
                    "    • {} ({}) — {} step(s)",
                    job.name,
                    job_id,
                    job.steps.len()
                );
            }
            Ok(())
        }
        Err(e) => {
            println!("✗ Invalid workflow: {}", e);
            std::process::exit(1);
        }
    }
}
