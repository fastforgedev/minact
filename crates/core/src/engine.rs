//! Core workflow execution engine.
//!
//! Orchestrates the full lifecycle: load workflow → resolve dependencies →
//! evaluate conditions → execute steps → collect outputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::logging::{CommandStream, LogEvent, LogLevel, Reporter, TracingReporter};
use crate::types::{
    Context, GithubContext, RunnerContext, StepConclusion, StepResult, WorkflowError,
};
use crate::workflow::*;

use crate::actions::{ActionContext, ActionRegistry};
use crate::expr;
use crate::scheduler::JobScheduler;

/// Top-level workflow engine.
pub struct Engine {
    /// Registry of available actions.
    actions: ActionRegistry,
    /// Current workspace directory.
    workspace: PathBuf,
    /// Receives structured execution log events.
    reporter: Arc<dyn Reporter>,
}

impl Engine {
    /// Create a new engine with the given workspace.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            actions: ActionRegistry::new(),
            workspace,
            reporter: Arc::new(TracingReporter),
        }
    }

    /// Create a new engine with a custom action registry.
    pub fn with_actions(workspace: PathBuf, actions: ActionRegistry) -> Self {
        Self {
            actions,
            workspace,
            reporter: Arc::new(TracingReporter),
        }
    }

    /// Create a new engine with a custom reporter.
    pub fn with_reporter(workspace: PathBuf, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            actions: ActionRegistry::new(),
            workspace,
            reporter,
        }
    }

    /// Create a new engine with a custom action registry and reporter.
    pub fn with_actions_and_reporter(
        workspace: PathBuf,
        actions: ActionRegistry,
        reporter: Arc<dyn Reporter>,
    ) -> Self {
        Self {
            actions,
            workspace,
            reporter,
        }
    }

    /// Register a custom action.
    pub fn register_action(&mut self, action: Box<dyn crate::actions::Action>) {
        self.actions.register(action);
    }

    /// Run a complete workflow.
    pub async fn run_workflow(
        &self,
        workflow: &Workflow,
        event_name: &str,
        inputs: HashMap<String, String>,
    ) -> Result<EngineResult, WorkflowError> {
        self.emit(LogEvent::WorkflowStarted {
            workflow_name: workflow.name.clone(),
            event_name: event_name.to_string(),
        })
        .await;

        // Build the execution context
        let ctx = self.build_context(workflow, event_name, inputs);

        // Resolve job execution order
        let scheduler = JobScheduler::new(workflow);
        let layers = scheduler.resolve_parallel_layers()?;

        self.emit(LogEvent::ExecutionPlan {
            layers: layers.clone(),
        })
        .await;

        let mut all_results: HashMap<String, JobResult> = HashMap::new();
        let mut ctx = ctx;

        // Execute jobs layer by layer
        for layer in &layers {
            // Execute jobs within each layer sequentially
            // (parallel execution across layers is handled by layer ordering)
            for job_id in layer {
                let result = self
                    .execute_job(job_id, workflow, &scheduler, &mut ctx)
                    .await?;
                all_results.insert(job_id.clone(), result);
            }
        }

        let success = all_results.values().all(|r| r.success);

        Ok(EngineResult {
            workflow_name: workflow.name.clone(),
            success,
            job_results: all_results,
        })
    }

    /// Execute a single job.
    async fn execute_job(
        &self,
        job_id: &str,
        workflow: &Workflow,
        _scheduler: &JobScheduler,
        ctx: &mut Context,
    ) -> Result<JobResult, WorkflowError> {
        let job = &workflow.jobs[job_id];
        self.emit(LogEvent::JobStarted {
            job_id: job_id.to_string(),
            job_name: job.name.clone(),
        })
        .await;

        // Check if condition
        if let Some(if_expr) = &job.if_condition {
            let should_run = evaluate_if_condition(if_expr, ctx)?;
            if !should_run {
                self.emit(LogEvent::JobSkipped {
                    job_id: job_id.to_string(),
                    job_name: job.name.clone(),
                    condition: if_expr.clone(),
                })
                .await;
                return Ok(JobResult {
                    job_id: job_id.to_string(),
                    job_name: job.name.clone(),
                    success: true,
                    conclusion: StepConclusion::Skipped,
                    outputs: HashMap::new(),
                    step_results: Vec::new(),
                });
            }
        }

        // Merge job-level env into context
        for (k, v) in &job.env {
            ctx.env.insert(k.clone(), v.clone());
        }

        let mut step_results = Vec::new();
        let mut job_outputs = HashMap::new();
        let mut job_success = true;
        let mut job_conclusion = StepConclusion::Success;

        // Resolve default working directory from job/workflow defaults
let default_wd = job
    .defaults
    .as_ref()
    .and_then(|d| d.run.as_ref())
    .and_then(|r| r.working_directory.clone())
    .or_else(|| {
        workflow
            .defaults
            .as_ref()
            .and_then(|d| d.run.as_ref())
            .and_then(|r| r.working_directory.clone())
    });

// Execute each step
for (step_idx, step) in job.steps.iter().enumerate() {
    let step_result = self
        .execute_step(step, step_idx, job_id, default_wd.as_deref(), ctx)
        .await?;
            let step_success = step_result.success;

            // Record step outputs in context
            if let Some(ref step_id) = step.id {
                ctx.step_outputs
                    .insert(step_id.clone(), step_result.outputs.clone());
            }

            // Handle continue-on-error
            if !step_success {
                if step.continue_on_error.unwrap_or(false) {
                    self.emit(LogEvent::Message {
                        level: LogLevel::Warn,
                        message: format!(
                            "Step '{}' failed but continue-on-error is set",
                            step.name
                        ),
                    })
                    .await;
                } else {
                    job_success = false;
                    job_conclusion = StepConclusion::Failure;
                    step_results.push(step_result);
                    break;
                }
            }

            step_results.push(step_result);
        }

        // Collect job outputs
        if let Some(outputs_config) = &job.outputs {
            for (output_name, expr) in outputs_config {
                let value = expr::evaluate_string(expr, ctx).unwrap_or_else(|_| expr.clone());
                job_outputs.insert(output_name.clone(), value);
            }
        }

        // Store job outputs in context for dependent jobs
        ctx.job_outputs
            .insert(job_id.to_string(), job_outputs.clone());

        self.emit(LogEvent::JobFinished {
            job_id: job_id.to_string(),
            job_name: job.name.clone(),
            success: job_success,
            conclusion: job_conclusion.clone(),
        })
        .await;

        Ok(JobResult {
            job_id: job_id.to_string(),
            job_name: job.name.clone(),
            success: job_success,
            conclusion: job_conclusion,
            outputs: job_outputs,
            step_results,
        })
    }

    /// Execute a single step.
    async fn execute_step(
        &self,
        step: &Step,
        step_idx: usize,
        job_id: &str,
        default_wd: Option<&str>,
        ctx: &Context,
    ) -> Result<StepResult, WorkflowError> {
        let step_name = if step.name.is_empty() {
            format!("step {}", step_idx + 1)
        } else {
            step.name.clone()
        };

        self.emit(LogEvent::StepStarted {
            job_id: job_id.to_string(),
            step_index: step_idx,
            step_name: step_name.clone(),
        })
        .await;

        // Check step-level condition
        if let Some(if_expr) = &step.if_condition {
            let should_run = evaluate_if_condition(if_expr, ctx)?;
            if !should_run {
                self.emit(LogEvent::StepSkipped {
                    job_id: job_id.to_string(),
                    step_index: step_idx,
                    step_name: step_name.clone(),
                    condition: if_expr.clone(),
                })
                .await;
                return Ok(StepResult {
                    success: true,
                    conclusion: StepConclusion::Skipped,
                    outputs: HashMap::new(),
                    artifacts: Vec::new(),
                });
            }
        }

        // Resolve effective working directory: step → job defaults → workflow defaults → workspace
        let effective_wd = step
            .working_directory
            .clone()
            .or_else(|| default_wd.map(String::from));

        if let Some(ref uses) = step.uses {
            self.execute_action_step(step, uses, step_name, effective_wd, ctx)
                .await
        } else if let Some(ref run) = step.run {
            self.execute_shell_step(step, run, step_name, effective_wd, ctx)
                .await
        } else {
            Err(WorkflowError::Other(format!(
                "Step '{}' has no 'uses' or 'run'",
                step_name
            )))
        }
    }

    /// Execute a step that uses an action.
    async fn execute_action_step(
        &self,
        step: &Step,
        uses: &str,
        step_name: String,
        effective_wd: Option<String>,
        ctx: &Context,
    ) -> Result<StepResult, WorkflowError> {
        // Parse action reference: "actions/checkout@v4" → "actions/checkout"
        let action_name = uses.split('@').next().unwrap_or(uses);

        let action = self
            .actions
            .get(action_name)
            .ok_or_else(|| WorkflowError::ActionNotFound(action_name.to_string()))?;

        self.emit(LogEvent::ActionStarted {
            uses: uses.to_string(),
        })
        .await;

        // Resolve with: parameters through expression evaluation
        let mut resolved_inputs = HashMap::new();
        for (k, v) in &step.with {
            let resolved = expr::evaluate_string(v, ctx).unwrap_or_else(|_| v.clone());
            self.emit(LogEvent::ActionInput {
                name: k.clone(),
                value: resolved.clone(),
            })
            .await;
            resolved_inputs.insert(k.clone(), resolved);
        }

        // Merge step env into context
        let mut step_env = step.env.clone();
        for (k, v) in &ctx.env {
            step_env.entry(k.clone()).or_insert_with(|| v.clone());
        }

        let working_dir = effective_wd.as_ref().map(|wd| {
            if Path::new(wd).is_absolute() {
                PathBuf::from(wd)
            } else {
                self.workspace.join(wd)
            }
        });

        let action_ctx = ActionContext {
            inputs: resolved_inputs,
            env: step_env,
            workspace: self.workspace.clone(),
            working_directory: working_dir,
            temp_dir: std::env::temp_dir().join(format!("minact-{}", uuid::Uuid::new_v4())),
            context: ctx.clone(),
        };

        // Validate action inputs
        action
            .validate(&action_ctx)
            .map_err(|e| WorkflowError::StepFailed(step_name.clone(), e.to_string()))?;

        // Run the action
        match action.run(&action_ctx).await {
            Ok(output) => {
                self.emit(LogEvent::ActionFinished {
                    success: output.success,
                    conclusion: output.conclusion.clone(),
                })
                .await;
                Ok(StepResult {
                    success: output.success,
                    conclusion: output.conclusion,
                    outputs: output.outputs,
                    artifacts: output.artifacts,
                })
            }
            Err(e) => {
                self.emit(LogEvent::ActionError {
                    message: e.to_string(),
                })
                .await;
                Ok(StepResult {
                    success: false,
                    conclusion: StepConclusion::Failure,
                    outputs: HashMap::new(),
                    artifacts: Vec::new(),
                })
            }
        }
    }

    /// Execute a shell step.
    async fn execute_shell_step(
        &self,
        step: &Step,
        run: &str,
        step_name: String,
        effective_wd: Option<String>,
        ctx: &Context,
    ) -> Result<StepResult, WorkflowError> {
        // Resolve expressions in the run command
        let resolved_run = expr::evaluate_string(run, ctx).unwrap_or_else(|_| run.to_string());

        let shell = step.shell.as_deref().unwrap_or("bash");

        // Build the command
        let (shell_cmd, shell_arg) = match shell {
            "bash" | "sh" => ("/bin/sh", Some("-c")),
            "python" => ("python3", Some("-c")),
            "powershell" => ("pwsh", Some("-Command")),
            "node" => ("node", Some("-e")),
            other => (other, None),
        };

        let working_dir = effective_wd
            .as_ref()
            .map(|wd| {
                if Path::new(wd).is_absolute() {
                    PathBuf::from(wd)
                } else {
                    self.workspace.join(wd)
                }
            })
            .unwrap_or_else(|| self.workspace.clone());

        self.emit(LogEvent::CommandStarted {
            command: resolved_run.clone(),
            shell: shell.to_string(),
            working_dir: working_dir.display().to_string(),
        })
        .await;

        // Execute the command
        let mut cmd = tokio::process::Command::new(shell_cmd);
        if let Some(arg) = shell_arg {
            cmd.arg(arg);
        }
        cmd.arg(&resolved_run);
        cmd.current_dir(&working_dir);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Set environment variables
        cmd.env_clear();
        cmd.env("HOME", dirs::home_dir().unwrap_or_default());
        cmd.env("PWD", &working_dir);
        for (k, v) in &ctx.env {
            cmd.env(k, v);
        }
        for (k, v) in &step.env {
            cmd.env(k, v);
        }

        // Execute and stream output.
        let mut child = cmd.spawn().map_err(|e| {
            WorkflowError::StepFailed(
                step_name.clone(),
                format!("Failed to execute command: {}", e),
            )
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            WorkflowError::StepFailed(step_name.clone(), "Failed to capture stdout".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            WorkflowError::StepFailed(step_name.clone(), "Failed to capture stderr".to_string())
        })?;

        let stdout_reporter = Arc::clone(&self.reporter);
        let stderr_reporter = Arc::clone(&self.reporter);
        let stdout_task = tokio::spawn(async move {
            emit_command_output(
                BufReader::new(stdout),
                CommandStream::Stdout,
                stdout_reporter,
            )
            .await
        });
        let stderr_task = tokio::spawn(async move {
            emit_command_output(
                BufReader::new(stderr),
                CommandStream::Stderr,
                stderr_reporter,
            )
            .await
        });

        let status = child.wait().await.map_err(|e| {
            WorkflowError::StepFailed(
                step_name.clone(),
                format!("Failed to wait for command: {}", e),
            )
        })?;
        stdout_task.await.map_err(|e| {
            WorkflowError::StepFailed(step_name.clone(), format!("Failed to read stdout: {}", e))
        })??;
        stderr_task.await.map_err(|e| {
            WorkflowError::StepFailed(step_name.clone(), format!("Failed to read stderr: {}", e))
        })??;

        let success = status.success();
        self.emit(LogEvent::CommandFinished {
            success,
            status: status.to_string(),
        })
        .await;

        Ok(StepResult {
            success,
            conclusion: if success {
                StepConclusion::Success
            } else {
                StepConclusion::Failure
            },
            outputs: HashMap::new(),
            artifacts: Vec::new(),
        })
    }

    /// Build the initial execution context.
    fn build_context(
        &self,
        workflow: &Workflow,
        event_name: &str,
        inputs: HashMap<String, String>,
    ) -> Context {
        let temp = std::env::temp_dir().join("minact");
        std::fs::create_dir_all(&temp).ok();
        let tool_cache = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("minact-tools");

        Context {
            github: GithubContext {
                event_name: event_name.to_string(),
                event: HashMap::new(),
                repository: std::env::var("GITHUB_REPOSITORY")
                    .unwrap_or_else(|_| "local/repo".to_string()),
                ref_name: std::env::var("GITHUB_REF")
                    .unwrap_or_else(|_| "refs/heads/main".to_string()),
                sha: std::env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_string()),
                workspace: self.workspace.to_string_lossy().to_string(),
                action: String::new(),
                actor: std::env::var("USER").unwrap_or_else(|_| "local".to_string()),
            },
            env: workflow.env.clone(),
            secrets: HashMap::new(), // Secrets are loaded from env or .env files
            job_outputs: HashMap::new(),
            step_outputs: HashMap::new(),
            inputs,
            runner: RunnerContext {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                temp: temp.to_string_lossy().to_string(),
                tool_cache: tool_cache.to_string_lossy().to_string(),
            },
        }
    }

    async fn emit(&self, event: LogEvent) {
        self.reporter.emit(event).await;
    }
}

async fn emit_command_output<R>(
    reader: R,
    stream: CommandStream,
    reporter: Arc<dyn Reporter>,
) -> Result<(), WorkflowError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        reporter
            .emit(LogEvent::CommandOutput { stream, line })
            .await;
    }
    Ok(())
}

/// Evaluate a job/step `if` condition.
fn evaluate_if_condition(expr_str: &str, ctx: &Context) -> Result<bool, WorkflowError> {
    // Handle simple expressions
    let expr_str = expr_str.trim();

    // If the expression is wrapped in ${{ }}, parse it
    if expr_str.starts_with("${{") && expr_str.ends_with("}}") {
        let inner = &expr_str[3..expr_str.len() - 2].trim();
        let parsed = crate::expr::parse_expression(inner)
            .map_err(|e| WorkflowError::ExpressionError(format!("{}", e)))?;
        return expr::evaluate_bool(&parsed, ctx);
    }

    // Otherwise, treat as a raw expression string
    let parsed = crate::expr::parse_expression(expr_str)
        .map_err(|e| WorkflowError::ExpressionError(format!("{}", e)))?;
    expr::evaluate_bool(&parsed, ctx)
}

/// Result of a complete workflow run.
#[derive(Debug, Clone)]
pub struct EngineResult {
    pub workflow_name: String,
    pub success: bool,
    pub job_results: HashMap<String, JobResult>,
}

/// Result of a single job execution.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub job_id: String,
    pub job_name: String,
    pub success: bool,
    pub conclusion: StepConclusion,
    pub outputs: HashMap<String, String>,
    pub step_results: Vec<StepResult>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{CommandStream, LogEvent, Reporter};
    use crate::parser::WorkflowParser;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CollectingReporter {
        events: Mutex<Vec<LogEvent>>,
    }

    #[async_trait]
    impl Reporter for CollectingReporter {
        async fn emit(&self, event: LogEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl CollectingReporter {
        fn events(&self) -> Vec<LogEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn test_basic_workflow_execution() {
        let yaml = r#"
name: Test
on: workflow_dispatch
jobs:
  greet:
    name: Greet
    steps:
      - name: Say Hello
        run: echo "Hello, World!"
"#;

        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let engine = Engine::new(PathBuf::from("/tmp"));
        let result = engine
            .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();

        assert_eq!(result.workflow_name, "Test");
        assert!(result.success);
        assert_eq!(result.job_results.len(), 1);
        assert!(result.job_results["greet"].success);
    }

    #[tokio::test]
    async fn test_workflow_with_condition() {
        let yaml = r#"
name: Conditional
on: workflow_dispatch
jobs:
  skip-me:
    name: Skip Me
    if: false
    steps:
      - name: Should Not Run
        run: echo "This should not run"
  run-me:
    name: Run Me
    if: true
    steps:
      - name: Should Run
        run: echo "This should run"
"#;

        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let engine = Engine::new(PathBuf::from("/tmp"));
        let result = engine
            .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(
            result.job_results["skip-me"].conclusion,
            StepConclusion::Skipped
        );
        assert_eq!(
            result.job_results["run-me"].conclusion,
            StepConclusion::Success
        );
    }

    #[tokio::test]
    async fn test_reporter_emits_command_stdout_and_stderr() {
        let yaml = r#"
name: Logs
on: workflow_dispatch
jobs:
  logs:
    name: Logs
    steps:
      - name: Emit streams
        run: printf 'out\n'; printf 'err\n' >&2
"#;

        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let reporter = Arc::new(CollectingReporter::default());
        let engine = Engine::with_reporter(PathBuf::from("/tmp"), reporter.clone());
        let result = engine
            .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();

        assert!(result.success);
        let events = reporter.events();
        assert!(events.iter().any(|event| matches!(
            event,
            LogEvent::CommandStarted { command, shell, .. }
                if command.contains("printf 'out") && shell == "bash"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            LogEvent::CommandOutput { stream: CommandStream::Stdout, line } if line == "out"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            LogEvent::CommandOutput { stream: CommandStream::Stderr, line } if line == "err"
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, LogEvent::CommandFinished { success: true, .. })));
    }

    #[tokio::test]
    async fn test_reporter_emits_skipped_step_without_command() {
        let yaml = r#"
name: Skip
on: workflow_dispatch
jobs:
  skip:
    name: Skip
    steps:
      - name: No command
        if: false
        run: echo "nope"
"#;

        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let reporter = Arc::new(CollectingReporter::default());
        let engine = Engine::with_reporter(PathBuf::from("/tmp"), reporter.clone());
        let result = engine
            .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();

        assert!(result.success);
        let events = reporter.events();
        assert!(events.iter().any(|event| matches!(
            event,
            LogEvent::StepSkipped { step_name, condition, .. }
                if step_name == "No command" && condition == "false"
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, LogEvent::CommandStarted { .. })));
    }

    #[tokio::test]
    async fn test_working_directory_resolution() {
        // Create a temp workspace with known subdirectories
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let sub_dir = workspace.join("sub");
        std::fs::create_dir(&sub_dir).unwrap();

        // Write a marker file in each directory so we can verify CWD
        std::fs::write(workspace.join("marker-root.txt"), "root").unwrap();
        std::fs::write(sub_dir.join("marker-sub.txt"), "sub").unwrap();

        let reporter = Arc::new(CollectingReporter::default());

        // Test 1: workflow-level defaults.run.working-directory
        let yaml = r#"
name: WorkflowDefaults
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Check CWD
        run: test -f marker-sub.txt
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let engine = Engine::with_reporter(workspace.clone(), reporter.clone());
        let result = engine
            .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();
        assert!(
            result.success,
            "workflow-level defaults should set CWD to sub/"
        );

        // Test 2: job-level defaults overrides workflow-level
        let yaml2 = r#"
name: JobDefaults
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: .
    steps:
      - name: Check CWD
        run: test -f marker-root.txt
"#;
        let workflow2 = WorkflowParser::parse_yaml(yaml2, None).unwrap();
        let result2 = engine
            .run_workflow(&workflow2, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();
        assert!(
            result2.success,
            "job-level defaults should override workflow-level"
        );

        // Test 3: step-level working-directory overrides job defaults
        let yaml3 = r#"
name: StepOverride
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Override to root
        working-directory: .
        run: test -f marker-root.txt
      - name: Inherit from defaults
        run: test -f marker-sub.txt
"#;
        let workflow3 = WorkflowParser::parse_yaml(yaml3, None).unwrap();
        let result3 = engine
            .run_workflow(&workflow3, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();
        assert!(
            result3.success,
            "both step override and default inherit should work"
        );

        // Test 4: all levels working together
        let yaml4 = r#"
name: AllLevels
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: .
    steps:
      - name: Step override back to sub
        working-directory: sub
        run: test -f marker-sub.txt
"#;
        let workflow4 = WorkflowParser::parse_yaml(yaml4, None).unwrap();
        let result4 = engine
            .run_workflow(&workflow4, "workflow_dispatch", HashMap::new())
            .await
            .unwrap();
        assert!(
            result4.success,
            "all three levels should compose correctly"
        );
    }
}
