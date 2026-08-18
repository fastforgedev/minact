//! minact CLI — run GitHub Actions-compatible workflows locally.
//!
//! Usage:
//!   minact run [--file <path>] [--event <event>] [--workspace <dir>]
//!   minact list [--dir <path>]
//!   minact validate <file>
//!   minact studio [--port <port>] [--workspace <dir>] [--open]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use minact_studio::StudioServer;
use tokio::net::TcpListener;

use minact_core::{
    print_plain_summary, print_pretty_summary, CancellationToken, Config, Engine, JsonReporter,
    PlainReporter, PrettyReporter, Reporter, WorkflowParser,
};

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

        /// Project configuration file
        /// (default: .minact/config.yml, if present)
        #[arg(long)]
        config: Option<PathBuf>,

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

    /// Launch the Studio web UI
    Studio {
        /// Port to listen on (0 picks a free one)
        #[arg(short, long, default_value_t = 4000)]
        port: u16,

        /// Address to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Project directory to serve (default: current directory)
        #[arg(short, long)]
        workspace: Option<PathBuf>,

        /// Extra directory of workflow files to browse, on top of the ones
        /// minact discovers (repeatable)
        #[arg(long = "workflows", value_name = "DIR")]
        workflow_dirs: Vec<PathBuf>,

        /// Open the UI in the default browser
        #[arg(long)]
        open: bool,
    },
}

/// Where `discover_workflows` looks, rendered from the parser's own list so
/// the message can never drift from the behaviour.
fn search_paths_message() -> String {
    format!(
        "Looked in: {}",
        WorkflowParser::search_path_summary(&WorkflowParser::default_search_paths())
    )
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
            config,
            log_format,
        } => cmd_run(file, event, workspace, input, config, log_format).await,
        Commands::List { dir, verbose } => cmd_list(dir, verbose),
        Commands::Validate { file } => cmd_validate(file),
        Commands::Studio {
            port,
            host,
            workspace,
            workflow_dirs,
            open,
        } => cmd_studio(port, host, workspace, workflow_dirs, open).await,
    }
}

async fn cmd_run(
    file: Option<PathBuf>,
    event: String,
    workspace: Option<PathBuf>,
    input: Vec<KeyVal>,
    config: Option<PathBuf>,
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
                    "No workflow files found in {}\n{}",
                    workspace.display(),
                    search_paths_message()
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
    // `runs-on:` only means something once labels are mapped to real places.
    let project_config = match &config {
        Some(path) => Config::load(path)?,
        None => match Config::discover(&workspace)? {
            Some((path, config)) => {
                if matches!(log_format, LogFormat::Plain) {
                    println!("Using config from: {}", path.display());
                }
                config
            }
            None => Config::default(),
        },
    };

    let engine = Engine::with_reporter(workspace, reporter).with_config(project_config);

    // Steps run in their own process group, so Ctrl+C no longer reaches them
    // on its own. Turn it into a cancellation instead, which kills the group
    // and still prints a summary for what did run.
    let cancel = CancellationToken::new();
    let on_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping — cancelling the run…");
            on_signal.cancel();
        }
    });

    let result = engine
        .run_workflow_cancellable(&workflow, &event, inputs, cancel)
        .await?;

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

async fn cmd_studio(
    port: u16,
    host: String,
    workspace: Option<PathBuf>,
    workflow_dirs: Vec<PathBuf>,
    open: bool,
) -> anyhow::Result<()> {
    let workspace = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("Invalid --host address: {}", host))?;

    // Studio runs shell commands on this machine on request, so exposing it
    // beyond loopback hands anyone who can reach the port a shell.
    if !ip.is_loopback() {
        eprintln!(
            "warning: binding to {} exposes workflow execution to your network",
            ip
        );
    }

    // Bind before serving so the printed URL is the port actually in use —
    // `--port 0` has no other way to report it.
    let listener = TcpListener::bind(SocketAddr::new(ip, port))
        .await
        .with_context(|| format!("Could not bind {}:{}", ip, port))?;
    let url = format!("http://{}", listener.local_addr()?);

    println!("minact studio");
    println!("  workspace  {}", workspace.display());
    for dir in &workflow_dirs {
        println!("  workflows  {}", dir.display());
    }
    println!("  listening  {}", url);
    println!();
    println!("Press Ctrl+C to stop.");

    if open {
        open_browser(&url);
    }

    StudioServer::new(workspace)
        .with_workflow_dirs(workflow_dirs)
        .serve_on(listener)
        .await?;

    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = ("xdg-open", vec![url]);

    if let Err(err) = std::process::Command::new(command.0)
        .args(&command.1)
        .spawn()
    {
        eprintln!("warning: could not open a browser ({}). Visit {}", err, url);
    }
}

fn cmd_list(dir: Option<PathBuf>, verbose: bool) -> anyhow::Result<()> {
    let dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let workflows = WorkflowParser::discover_workflows(&dir)?;

    if workflows.is_empty() {
        println!("No workflows found in {}", dir.display());
        println!("{}", search_paths_message());
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
