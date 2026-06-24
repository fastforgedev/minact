//! minact CLI — run GitHub Actions-compatible workflows locally.
//!
//! Usage:
//!   minact run [--file <path>] [--event <event>] [--workspace <dir>]
//!   minact list [--dir <path>]
//!   minact validate <file>

use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use minact_core::{Engine, WorkflowParser};

#[derive(Parser)]
#[command(name = "minact", version, about = "Run GitHub Actions workflows locally")]
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

fn parse_key_val(s: &str) -> Result<KeyVal, String> {
    let pos = s.find('=').ok_or_else(|| format!("Invalid KEY=value format: {}", s))?;
    Ok(KeyVal {
        key: s[..pos].to_string(),
        value: s[pos + 1..].to_string(),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run { file, event, workspace, input } => {
            cmd_run(file, event, workspace, input).await
        }
        Commands::List { dir, verbose } => {
            cmd_list(dir, verbose)
        }
        Commands::Validate { file } => {
            cmd_validate(file)
        }
    }
}

async fn cmd_run(
    file: Option<PathBuf>,
    event: String,
    workspace: Option<PathBuf>,
    input: Vec<KeyVal>,
) -> anyhow::Result<()> {
    let workspace = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let inputs: HashMap<String, String> = input.into_iter().map(|kv| (kv.key, kv.value)).collect();

    // Load workflow
    let workflow = if let Some(file_path) = file {
        tracing::info!("Loading workflow from: {}", file_path.display());
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
                tracing::warn!("Found {} workflows. Use --file to specify one.\n", n);
                for (i, wf) in workflows.iter().enumerate() {
                    let source = wf.file_path.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    tracing::warn!("  {}. {} ({})", i + 1, wf.name, source);
                }
                anyhow::bail!("Please specify a workflow file with --file");
            }
        }
    };

    tracing::info!("Workflow: {}", workflow.name);
    tracing::info!("Workspace: {}", workspace.display());
    tracing::info!("Event: {}", event);
    if !inputs.is_empty() {
        tracing::info!("Inputs: {:?}", inputs);
    }

    // Run the workflow
    let engine = Engine::new(workspace);

    let result = engine.run_workflow(&workflow, &event, inputs).await?;

    // Print summary
    println!();
    println!("═══════════════════════════════════════");
    println!("  Workflow: {}", result.workflow_name);
    println!("  Result: {}", if result.success { "✓ SUCCESS" } else { "✗ FAILED" });
    println!("═══════════════════════════════════════");

    for (job_id, job_result) in &result.job_results {
        let status = match job_result.conclusion {
            minact_core::StepConclusion::Success => "✓",
            minact_core::StepConclusion::Failure => "✗",
            minact_core::StepConclusion::Cancelled => "◯",
            minact_core::StepConclusion::Skipped => "–",
        };
        println!("  {} {} ({})", status, job_result.job_name, job_id);
    }

    if !result.success {
        std::process::exit(1);
    }

    Ok(())
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
        let source = workflow.file_path.as_ref()
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
                println!("    • {} ({}) — {} step(s)", job.name, job_id, job.steps.len());
            }
            Ok(())
        }
        Err(e) => {
            println!("✗ Invalid workflow: {}", e);
            std::process::exit(1);
        }
    }
}
