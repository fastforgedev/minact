//! Core workflow execution engine.
//!
//! Orchestrates the full lifecycle: load workflow → resolve dependencies →
//! evaluate conditions → execute steps → collect outputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::commands::{self, ParsedCommand};
use crate::config::{Config, RunnerSpec};
use crate::executor::{Executor, OutputSink, StepRequest};
use crate::logging::{
    CommandStream, EventScope, LogEvent, LogLevel, LogRecord, Reporter, TracingReporter,
};
use crate::matrix::{self, MatrixCombination};
use crate::types::{
    runner_arch_name, runner_os_name, Context, GithubContext, RunStatus, RunnerContext,
    StepConclusion, StepResult, StepStatus, StrategyContext, Value, WorkflowError,
};
use crate::workflow::*;

use crate::actions::container::ContainerAction;
use crate::actions::manifest::DockerImageSource;
use crate::actions::{
    self, ActionContext, ActionInputs, ActionRef, ActionRegistry, ActionRuns, ActionStore,
    ResolvedAction,
};
use crate::expr;
use crate::scheduler::JobScheduler;

#[cfg(windows)]
const PATH_SEPARATOR: &str = ";";
#[cfg(not(windows))]
const PATH_SEPARATOR: &str = ":";

/// Per-run plumbing threaded through execution.
///
/// Carries two things that every level of the run needs and that `&self` on
/// `Engine` cannot supply: where events go (stamped with their position in the
/// run and the job/step they belong to), and whether the run has been asked to
/// stop. Narrowing the scope is cheap — [`Run::in_job`] and [`Run::in_step`]
/// clone the shared parts and replace only the scope.
#[derive(Clone)]
struct Run {
    reporter: Arc<dyn Reporter>,
    /// Shared across every clone so sequence numbers are unique run-wide,
    /// which is what lets a reconnecting client resume from one.
    seq: Arc<AtomicU64>,
    scope: EventScope,
    cancel: CancellationToken,
}

impl Run {
    fn new(reporter: Arc<dyn Reporter>, cancel: CancellationToken) -> Self {
        Self {
            reporter,
            seq: Arc::new(AtomicU64::new(0)),
            scope: EventScope::default(),
            cancel,
        }
    }

    /// Narrow to a job instance.
    fn in_job(&self, job_id: &str) -> Self {
        Self {
            scope: EventScope::job(job_id),
            ..self.clone()
        }
    }

    /// Same scope, but stopping on `cancel` as well as on the run's own
    /// token. Used to give a step or a job its own deadline.
    fn with_cancel(&self, cancel: CancellationToken) -> Self {
        Self {
            cancel,
            ..self.clone()
        }
    }

    /// Narrow to a step of the current job.
    fn in_step(&self, step_index: usize) -> Self {
        Self {
            scope: EventScope {
                step_index: Some(step_index),
                ..self.scope.clone()
            },
            ..self.clone()
        }
    }

    async fn emit(&self, event: LogEvent) {
        self.reporter
            .emit_record(LogRecord {
                seq: self.seq.fetch_add(1, Ordering::SeqCst),
                ts: chrono::Utc::now(),
                scope: self.scope.clone(),
                event,
            })
            .await;
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Top-level workflow engine.
pub struct Engine {
    /// Registry of available actions.
    actions: ActionRegistry,
    /// Where actions named `owner/repo@ref` are fetched to.
    store: ActionStore,
    /// The project's configuration, including the `runs-on:` mapping.
    config: Config,
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
            store: default_action_store(),
            config: Config::default(),
            workspace,
            reporter: Arc::new(TracingReporter),
        }
    }

    /// Create a new engine with a custom action registry.
    pub fn with_actions(workspace: PathBuf, actions: ActionRegistry) -> Self {
        Self {
            actions,
            store: default_action_store(),
            config: Config::default(),
            workspace,
            reporter: Arc::new(TracingReporter),
        }
    }

    /// Create a new engine with a custom reporter.
    pub fn with_reporter(workspace: PathBuf, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            actions: ActionRegistry::new(),
            store: default_action_store(),
            config: Config::default(),
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
            store: default_action_store(),
            config: Config::default(),
            workspace,
            reporter,
        }
    }

    /// Register a custom action.
    pub fn register_action(&mut self, action: Box<dyn crate::actions::Action>) {
        self.actions.register(action);
    }

    /// Fetch remote actions somewhere other than `~/.minact/actions`.
    ///
    /// Also how a run is made to re-fetch a moving ref: an action pinned to a
    /// branch is otherwise served from the cache forever.
    pub fn with_action_store(mut self, store: ActionStore) -> Self {
        self.store = store;
        self
    }

    /// Apply the project's configuration, which is what gives `runs-on:`
    /// labels somewhere to point.
    ///
    /// Without it every job runs on this machine, and any `runs-on:` naming a
    /// different platform is reported rather than silently ignored.
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Pick the executor for a job from its `runs-on:` label.
    async fn executor_for(
        &self,
        job: &Job,
        ctx: &Context,
        run: &Run,
    ) -> Result<(Arc<dyn Executor>, RunnerSpec), WorkflowError> {
        // `runs-on: ${{ matrix.target }}` is how one job definition lands on a
        // different machine per matrix instance, so it has to be evaluated
        // rather than looked up literally.
        // `container:` says what the steps run *in*; `runs-on:` only says which
        // machine picks the job up. A job with both runs in the container, so
        // it decides before the label is consulted.
        if let Some(container) = &job.container {
            let image = evaluate_at("container.image", &container.image, ctx)?;
            if container.credentials.is_some() {
                run.emit(LogEvent::Message {
                    level: LogLevel::Warn,
                    message: format!(
                        "container.credentials is ignored; run `docker login` yourself if {} is private",
                        image
                    ),
                })
                .await;
            }
            let spec = RunnerSpec::Docker {
                image,
                user: None,
                pull: false,
                run_args: container_run_args(container),
                binary: "docker".to_string(),
            };
            run.emit(LogEvent::Message {
                level: LogLevel::Info,
                message: format!("runner: {}", spec.describe()),
            })
            .await;
            let runner_temp = std::env::temp_dir().join("minact");
            let executor = spec.build(
                &self.workspace,
                &runner_temp,
                &[self.store.root().to_path_buf()],
            )?;
            return Ok((executor, spec));
        }

        let runs_on = match &job.runs_on {
            Some(label) => Some(evaluate_at("runs-on", label, ctx)?),
            None => None,
        };
        let runs_on = runs_on.as_deref();
        let spec = match self.config.resolve(runs_on) {
            Some(spec) => spec.clone(),
            None => {
                // Running a `runs-on: windows-latest` job on a Mac and
                // reporting success would be a lie; say what happened.
                if let Some(label) = runs_on {
                    if !host_matches(label) {
                        run.emit(LogEvent::Message {
                            level: LogLevel::Warn,
                            message: format!(
                                "`runs-on: {}` has no runner configured; running on this machine ({}). \
                                 Map it in .minact/config.yml to run it elsewhere.",
                                label,
                                runner_os_name()
                            ),
                        })
                        .await;
                    }
                }
                RunnerSpec::Local
            }
        };

        if !spec.is_local() {
            run.emit(LogEvent::Message {
                level: LogLevel::Info,
                message: format!("runner: {}", spec.describe()),
            })
            .await;
        }

        let runner_temp = std::env::temp_dir().join("minact");
        // The action cache is mounted into job containers whether or not this
        // job uses it: a container's mounts are fixed before its first step.
        let executor = spec.build(
            &self.workspace,
            &runner_temp,
            &[self.store.root().to_path_buf()],
        )?;
        Ok((executor, spec))
    }

    /// Run a complete workflow.
    pub async fn run_workflow(
        &self,
        workflow: &Workflow,
        event_name: &str,
        inputs: HashMap<String, String>,
    ) -> Result<EngineResult, WorkflowError> {
        self.run_workflow_cancellable(workflow, event_name, inputs, CancellationToken::new())
            .await
    }

    /// Run a workflow that can be stopped part-way.
    ///
    /// Cancellation is checked before each job and each step, and a command
    /// already running is killed rather than waited out. Whatever had not
    /// finished is reported as [`StepConclusion::Cancelled`].
    pub async fn run_workflow_cancellable(
        &self,
        workflow: &Workflow,
        event_name: &str,
        inputs: HashMap<String, String>,
        cancel: CancellationToken,
    ) -> Result<EngineResult, WorkflowError> {
        let run = Run::new(Arc::clone(&self.reporter), cancel);

        run.emit(LogEvent::WorkflowStarted {
            workflow_name: workflow.name.clone(),
            event_name: event_name.to_string(),
        })
        .await;

        // Build the execution context
        let mut ctx = self.build_context(workflow, event_name, inputs);

        // Resolve job execution order
        let scheduler = JobScheduler::new(workflow);
        let layers = scheduler.resolve_parallel_layers()?;

        run.emit(LogEvent::ExecutionPlan {
            layers: layers.clone(),
        })
        .await;

        let mut all_results: HashMap<String, JobResult> = HashMap::new();
        let mut job_order: Vec<String> = Vec::new();

        // Execute jobs layer by layer
        for layer in &layers {
            // Execute jobs within each layer sequentially
            // (parallel execution across layers is handled by layer ordering)
            for job_id in layer {
                let results = self
                    .execute_job_matrix(job_id, workflow, &mut ctx, &run)
                    .await?;

                // Dependent jobs see one conclusion per job id, however many
                // matrix instances that job expanded to.
                ctx.job_results
                    .insert(job_id.clone(), aggregate_conclusion(&results));

                for result in results {
                    job_order.push(result.instance_id.clone());
                    all_results.insert(result.instance_id.clone(), result);
                }
            }
        }

        // A cancelled run is not a successful one, but a skipped job is fine.
        let success = all_results.values().all(|r| {
            matches!(
                r.conclusion,
                StepConclusion::Success | StepConclusion::Skipped
            )
        });

        Ok(EngineResult {
            workflow_name: workflow.name.clone(),
            success,
            job_results: all_results,
            job_order,
        })
    }

    /// Expand a job's `strategy.matrix` and run every instance.
    ///
    /// Jobs without a matrix produce exactly one instance, so this is the
    /// single path all jobs take.
    async fn execute_job_matrix(
        &self,
        job_id: &str,
        workflow: &Workflow,
        ctx: &mut Context,
        run: &Run,
    ) -> Result<Vec<JobResult>, WorkflowError> {
        let job = &workflow.jobs[job_id];
        let strategy = job.strategy.as_ref();

        // A matrix can be computed by an upstream job, so its expressions are
        // resolved here — after `needs` finished — rather than while parsing.
        let resolved_matrix = match strategy.and_then(|s| s.matrix.as_ref()) {
            Some(config) if config.is_dynamic() => match config.resolve(ctx) {
                Ok(resolved) => Some(resolved),
                Err(e) => {
                    run.emit(LogEvent::Message {
                        level: LogLevel::Error,
                        message: format!("job '{}': {}", job_id, e),
                    })
                    .await;
                    return Ok(vec![JobResult {
                        job_id: job_id.to_string(),
                        instance_id: job_id.to_string(),
                        job_name: job_id.to_string(),
                        matrix: HashMap::new(),
                        success: false,
                        conclusion: StepConclusion::Failure,
                        outputs: HashMap::new(),
                        step_results: Vec::new(),
                    }]);
                }
            },
            Some(config) => Some(config.clone()),
            None => None,
        };

        let combinations = match &resolved_matrix {
            Some(config) => matrix::expand(config),
            None => vec![MatrixCombination::new()],
        };

        let fail_fast = strategy.map(|s| s.is_fail_fast()).unwrap_or(true);
        let max_parallel = strategy.and_then(|s| s.max_parallel);
        let total = combinations.len();

        let mut results = Vec::with_capacity(total);
        let mut cancelled = false;

        for (index, combination) in combinations.into_iter().enumerate() {
            // The matrix context goes in first: the job's `name:`, its `if:`
            // and every step are all evaluated against it.
            let saved_matrix = std::mem::replace(&mut ctx.matrix, combination.clone().into());
            let saved_strategy = std::mem::replace(
                &mut ctx.strategy,
                StrategyContext {
                    fail_fast,
                    job_index: index,
                    job_total: total,
                    max_parallel,
                },
            );

            let instance = JobInstance::new(job_id, job, &combination, ctx);
            // Everything this instance emits is tagged with the instance id,
            // never the base job id — sibling matrix instances must not share
            // a scope.
            let job_run = run.in_job(&instance.instance_id);

            let stopped = if run.is_cancelled() {
                Some("cancelled")
            } else if cancelled {
                // fail-fast: once one instance fails, the rest never start.
                Some("fail-fast")
            } else {
                None
            };

            let outcome = match stopped {
                Some(reason) => {
                    job_run
                        .emit(LogEvent::JobCancelled {
                            job_id: instance.instance_id.clone(),
                            job_name: instance.display_name.clone(),
                            reason: reason.to_string(),
                        })
                        .await;
                    Ok(instance.result_with(StepConclusion::Cancelled))
                }
                None => {
                    self.execute_job(&instance, job, workflow, ctx, &job_run)
                        .await
                }
            };

            ctx.matrix = saved_matrix;
            ctx.strategy = saved_strategy;

            let result = outcome?;
            if fail_fast && result.conclusion == StepConclusion::Failure {
                cancelled = true;
            }
            results.push(result);
        }

        Ok(results)
    }

    /// Execute one job instance.
    async fn execute_job(
        &self,
        instance: &JobInstance,
        job: &Job,
        workflow: &Workflow,
        ctx: &mut Context,
        run: &Run,
    ) -> Result<JobResult, WorkflowError> {
        run.emit(LogEvent::JobStarted {
            job_id: instance.instance_id.clone(),
            job_name: instance.display_name.clone(),
        })
        .await;

        let result = self
            .execute_job_inner(instance, job, workflow, ctx, run)
            .await?;

        if result.conclusion != StepConclusion::Skipped {
            run.emit(LogEvent::JobFinished {
                job_id: instance.instance_id.clone(),
                job_name: instance.display_name.clone(),
                success: result.success,
                conclusion: result.conclusion,
            })
            .await;
        }

        Ok(result)
    }

    async fn execute_job_inner(
        &self,
        instance: &JobInstance,
        job: &Job,
        workflow: &Workflow,
        ctx: &mut Context,
        run: &Run,
    ) -> Result<JobResult, WorkflowError> {
        // A job's status is derived from the conclusions of everything it
        // needs, so `if: failure()` on a dependent job means "a dependency
        // failed" rather than a hard-coded constant.
        let needs = job.needs.clone().unwrap_or_default();
        let status =
            RunStatus::from_conclusions(needs.iter().filter_map(|id| ctx.job_results.get(id)));
        ctx.status = status;

        // With no explicit `if:`, the implicit condition is `success()`, which
        // is what skips a job whose dependency failed or was skipped.
        let (should_run, condition_label) = match &job.if_condition {
            Some(if_expr) => (evaluate_if_condition(if_expr, ctx)?, if_expr.clone()),
            None => (ctx.status.success, "success()".to_string()),
        };

        if !should_run {
            run.emit(LogEvent::JobSkipped {
                job_id: instance.instance_id.clone(),
                job_name: instance.display_name.clone(),
                condition: condition_label,
            })
            .await;
            return Ok(instance.result_with(StepConclusion::Skipped));
        }

        // `env` and the `steps` context are job-scoped in GitHub Actions, so
        // snapshot them and restore once the job is done.
        let saved_env = ctx.env.clone();
        let saved_step_outputs = std::mem::take(&mut ctx.step_outputs);
        let saved_step_status = std::mem::take(&mut ctx.step_status);

        // A job-level `timeout-minutes` bounds every step of it at once.
        let deadline = job
            .timeout_minutes
            .map(|minutes| Deadline::arm(&run.cancel, minutes));
        let timed_run = match &deadline {
            Some(deadline) => run.with_cancel(deadline.token()),
            None => run.clone(),
        };

        let mut result = self
            .run_job_steps(instance, job, workflow, ctx, &timed_run)
            .await;

        if let Some(deadline) = &deadline {
            if deadline.expired() {
                run.emit(LogEvent::Message {
                    level: LogLevel::Error,
                    message: format!(
                        "Job '{}' exceeded its timeout-minutes of {}",
                        instance.instance_id,
                        job.timeout_minutes.unwrap_or_default()
                    ),
                })
                .await;
                if let Ok(result) = &mut result {
                    result.success = false;
                    result.conclusion = StepConclusion::Failure;
                }
            }
        }
        drop(deadline);

        ctx.env = saved_env;
        ctx.step_outputs = saved_step_outputs;
        ctx.step_status = saved_step_status;

        result
    }

    /// Run every step of a job, honouring failure and `if:` semantics.
    #[allow(clippy::too_many_arguments)]
    async fn run_job_steps(
        &self,
        instance: &JobInstance,
        job: &Job,
        workflow: &Workflow,
        ctx: &mut Context,
        run: &Run,
    ) -> Result<JobResult, WorkflowError> {
        // `$GITHUB_JOB` and log prefixes disagree on purpose: the env var is
        // the job id as written, the logs identify the matrix instance.
        let job_id = instance.base_id.as_str();

        if !job.services.is_empty() {
            let mut names: Vec<&String> = job.services.keys().collect();
            names.sort();
            run.emit(LogEvent::Message {
                level: LogLevel::Warn,
                message: format!(
                    "`services:` is not started ({}); steps that expect {} \
                     will not find {}",
                    names
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if names.len() == 1 { "it" } else { "them" },
                    if names.len() == 1 { "it" } else { "them" },
                ),
            })
            .await;
        }

        // A job container's `env:` applies to every step of the job, so it
        // goes in underneath the job's own `env:`.
        if let Some(container) = &job.container {
            for (key, value) in &container.env {
                let resolved = evaluate_at(
                    &format!("job '{}' container.env.{}", job_id, key),
                    value,
                    ctx,
                )?;
                ctx.env.insert(key.clone(), resolved);
            }
        }

        // Merge job-level env into the context, evaluating expressions.
        for (key, value) in &job.env {
            match evaluate_at(&format!("job '{}' env.{}", job_id, key), value, ctx) {
                Ok(resolved) => {
                    ctx.env.insert(key.clone(), resolved);
                }
                Err(e) => {
                    run.emit(LogEvent::Message {
                        level: LogLevel::Error,
                        message: format!("{}", e),
                    })
                    .await;
                    return Ok(JobResult {
                        ..instance.result_with(StepConclusion::Failure)
                    });
                }
            }
        }

        let defaults = StepDefaults::resolve(job, workflow);

        // Where this job's steps run. Resolved per job, because `runs-on` is a
        // job-level decision and a container has to live across the steps.
        let (executor, runner_spec) = self.executor_for(job, ctx, run).await?;
        let container_binary = container_binary(&runner_spec);
        if let Err(e) = executor
            .prepare(&StepSink::new(run.clone(), Vec::new()))
            .await
        {
            run.emit(LogEvent::Message {
                level: LogLevel::Error,
                message: format!("{}", e),
            })
            .await;
            return Ok(JobResult {
                ..instance.result_with(StepConclusion::Failure)
            });
        }

        let mut step_results = Vec::new();
        let mut job_failed = false;
        // `$GITHUB_PATH` additions and `::add-mask::` values accumulate across
        // the steps of a job.
        ctx.github.job = job_id.to_string();

        let mut extra_paths: Vec<String> = Vec::new();
        // A token in the environment is a secret wherever it surfaces, so it
        // starts out redacted rather than waiting for an `::add-mask::`.
        let mut masks: Vec<String> = if ctx.github.token.is_empty() {
            Vec::new()
        } else {
            vec![ctx.github.token.clone()]
        };
        // `post:` hooks the steps registered, paired with the step they came
        // from so their output is reported under it.
        let mut pending_post: Vec<(usize, PostHook)> = Vec::new();

        let mut cancelled = false;

        for (step_idx, step) in job.steps.iter().enumerate() {
            let step_name = display_step_name(step, step_idx);
            let step_run = run.in_step(step_idx);

            // Checked between steps rather than mid-step, except for a running
            // command, which is killed — see `execute_shell_step`.
            if run.is_cancelled() {
                cancelled = true;
                step_run
                    .emit(LogEvent::StepSkipped {
                        job_id: job_id.to_string(),
                        step_index: step_idx,
                        step_name: step_name.clone(),
                        condition: "cancelled".to_string(),
                    })
                    .await;
                step_results.push(StepResult {
                    success: false,
                    conclusion: StepConclusion::Cancelled,
                    outputs: HashMap::new(),
                    artifacts: Vec::new(),
                });
                continue;
            }

            // Status as seen by this step's `if:` — did anything fail earlier
            // in this job?
            ctx.status = if job_failed {
                RunStatus::failure()
            } else {
                RunStatus::success()
            };

            let (should_run, condition_label) = match &step.if_condition {
                Some(if_expr) => match evaluate_if_condition(if_expr, ctx) {
                    Ok(should_run) => (should_run, if_expr.clone()),
                    Err(e) => {
                        // A broken condition fails the step rather than
                        // aborting the whole run.
                        step_run
                            .emit(LogEvent::Message {
                                level: LogLevel::Error,
                                message: format!(
                                    "Step '{}' has an invalid `if` condition: {}",
                                    step_name, e
                                ),
                            })
                            .await;
                        job_failed = true;
                        step_results.push(StepResult {
                            success: false,
                            conclusion: StepConclusion::Failure,
                            outputs: HashMap::new(),
                            artifacts: Vec::new(),
                        });
                        continue;
                    }
                },
                None => (!job_failed, "success()".to_string()),
            };

            step_run
                .emit(LogEvent::StepStarted {
                    job_id: job_id.to_string(),
                    step_index: step_idx,
                    step_name: step_name.clone(),
                })
                .await;

            if !should_run {
                step_run
                    .emit(LogEvent::StepSkipped {
                        job_id: job_id.to_string(),
                        step_index: step_idx,
                        step_name: step_name.clone(),
                        condition: condition_label,
                    })
                    .await;
                self.record_step(
                    ctx,
                    step,
                    StepConclusion::Skipped,
                    StepConclusion::Skipped,
                    &HashMap::new(),
                );
                step_results.push(StepResult {
                    success: true,
                    conclusion: StepConclusion::Skipped,
                    outputs: HashMap::new(),
                    artifacts: Vec::new(),
                });
                continue;
            }

            // `timeout-minutes` gets its own token so the deadline can kill the
            // running process, not just stop waiting for it.
            let deadline = step
                .timeout_minutes
                .map(|minutes| Deadline::arm(&step_run.cancel, minutes));
            let timed_run = match &deadline {
                Some(deadline) => step_run.with_cancel(deadline.token()),
                None => step_run.clone(),
            };

            let mut execution = match self
                .execute_step(
                    step,
                    &step_name,
                    job_id,
                    &defaults,
                    &extra_paths,
                    &masks,
                    0,
                    ctx,
                    &timed_run,
                    executor.as_ref(),
                    &container_binary,
                )
                .await
            {
                Ok(execution) => execution,
                Err(e) => {
                    // Turn an engine-level step error (unknown action, spawn
                    // failure, …) into a failed step so the rest of the
                    // workflow still reports meaningfully.
                    step_run
                        .emit(LogEvent::Message {
                            level: LogLevel::Error,
                            message: format!("{}", e),
                        })
                        .await;
                    StepExecution::failed()
                }
            };

            // Running out of time is a failure, not a cancellation: a
            // cancelled *run* has to stay distinguishable from a step that
            // overran on its own.
            if let Some(deadline) = &deadline {
                if deadline.expired() {
                    step_run
                        .emit(LogEvent::Message {
                            level: LogLevel::Error,
                            message: format!(
                                "Step '{}' exceeded its timeout-minutes of {}",
                                step_name,
                                step.timeout_minutes.unwrap_or_default()
                            ),
                        })
                        .await;
                    execution.result.success = false;
                    execution.result.conclusion = StepConclusion::Failure;
                }
            }
            drop(deadline);

            // Apply everything the step handed back to the runner.
            for (key, value) in &execution.env_updates {
                ctx.env.insert(key.clone(), value.clone());
            }
            for path in execution.path_additions.iter().rev() {
                extra_paths.insert(0, path.clone());
            }
            masks.extend(execution.masks.iter().cloned());
            for hook in execution.post {
                pending_post.push((step_idx, hook));
            }

            let outcome = execution.result.conclusion;
            let continue_on_error = step.continue_on_error.unwrap_or(false);

            // `continue-on-error` leaves the outcome as failure but reports the
            // conclusion as success, exactly like GitHub.
            let conclusion = if outcome == StepConclusion::Failure && continue_on_error {
                step_run
                    .emit(LogEvent::Message {
                        level: LogLevel::Warn,
                        message: format!(
                            "Step '{}' failed but continue-on-error is set",
                            step_name
                        ),
                    })
                    .await;
                StepConclusion::Success
            } else {
                outcome
            };

            self.record_step(ctx, step, outcome, conclusion, &execution.result.outputs);

            match conclusion {
                StepConclusion::Failure => job_failed = true,
                StepConclusion::Cancelled => cancelled = true,
                _ => {}
            }

            step_results.push(execution.result);
        }

        // An action's `post:` runs when the job ends, in reverse order, and
        // whatever the job did — that is what makes it cleanup rather than
        // just another step. Its output is reported under the step that
        // registered it rather than inventing step indices the workflow has no
        // entry for.
        ctx.status = if job_failed {
            RunStatus::failure()
        } else if cancelled {
            RunStatus::neutral()
        } else {
            RunStatus::success()
        };
        for (step_idx, hook) in pending_post.into_iter().rev() {
            if run.is_cancelled() {
                break;
            }
            let step_run = run.in_step(step_idx);
            let hook_name = hook.step_name.clone();
            let execution = self
                .run_post_hook(
                    hook,
                    &extra_paths,
                    &masks,
                    job_id,
                    ctx,
                    &step_run,
                    executor.as_ref(),
                )
                .await;

            match execution {
                Ok(execution) => {
                    for (key, value) in &execution.env_updates {
                        ctx.env.insert(key.clone(), value.clone());
                    }
                    masks.extend(execution.masks.iter().cloned());
                    if execution.result.conclusion == StepConclusion::Failure {
                        job_failed = true;
                    }
                }
                Err(e) => {
                    step_run
                        .emit(LogEvent::Message {
                            level: LogLevel::Error,
                            message: format!("post: {} failed: {}", hook_name, e),
                        })
                        .await;
                    job_failed = true;
                }
            }
        }

        // Collect job outputs
        let mut job_outputs = HashMap::new();
        if let Some(outputs_config) = &job.outputs {
            for (output_name, expression) in outputs_config {
                match evaluate_at(
                    &format!("job '{}' outputs.{}", job_id, output_name),
                    expression,
                    ctx,
                ) {
                    Ok(value) => {
                        job_outputs.insert(output_name.clone(), value);
                    }
                    // Handing a dependent job the raw `${{ }}` text would move
                    // the failure somewhere it cannot be explained.
                    Err(e) => {
                        run.emit(LogEvent::Message {
                            level: LogLevel::Error,
                            message: format!("{}", e),
                        })
                        .await;
                        job_failed = true;
                    }
                }
            }
        }

        executor
            .cleanup(&StepSink::new(run.clone(), Vec::new()))
            .await;

        // Store job outputs in context for dependent jobs. Matrix instances
        // all write to the same job id, so the last one to run wins — the same
        // caveat GitHub documents for outputs from a matrix job.
        ctx.job_outputs
            .insert(job_id.to_string(), job_outputs.clone());

        let mut conclusion = if job_failed {
            StepConclusion::Failure
        } else if cancelled {
            StepConclusion::Cancelled
        } else {
            StepConclusion::Success
        };

        // `continue-on-error` on a job is the job-level twin of the step-level
        // one: the failure happened and is reported, but it does not fail the
        // workflow and it does not skip the jobs that need this one.
        if conclusion == StepConclusion::Failure && job.continue_on_error.unwrap_or(false) {
            run.emit(LogEvent::Message {
                level: LogLevel::Warn,
                message: format!(
                    "Job '{}' failed but continue-on-error is set",
                    instance.instance_id
                ),
            })
            .await;
            conclusion = StepConclusion::Success;
        }

        Ok(JobResult {
            outputs: job_outputs,
            step_results,
            ..instance.result_with(conclusion)
        })
    }

    /// Record a finished step's outputs and status in the `steps` context.
    fn record_step(
        &self,
        ctx: &mut Context,
        step: &Step,
        outcome: StepConclusion,
        conclusion: StepConclusion,
        outputs: &HashMap<String, String>,
    ) {
        let Some(step_id) = &step.id else {
            return;
        };
        ctx.step_outputs.insert(step_id.clone(), outputs.clone());
        ctx.step_status.insert(
            step_id.clone(),
            StepStatus {
                outcome,
                conclusion,
            },
        );
    }

    /// Execute a single step.
    #[allow(clippy::too_many_arguments)]
    async fn execute_step(
        &self,
        step: &Step,
        step_name: &str,
        job_id: &str,
        defaults: &StepDefaults,
        extra_paths: &[String],
        masks: &[String],
        depth: usize,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
        container_binary: &str,
    ) -> Result<StepExecution, WorkflowError> {
        // Resolve effective working directory: step → job defaults → workflow defaults → workspace
        let effective_wd = step
            .working_directory
            .clone()
            .or_else(|| defaults.working_directory.clone());

        // Step env is evaluated in the current context and layered on top of
        // the workflow/job env.
        let mut step_env = HashMap::new();
        for (key, value) in &step.env {
            let resolved = evaluate_at(&format!("step '{}' env.{}", step_name, key), value, ctx)?;
            step_env.insert(key.clone(), resolved);
        }

        if let Some(uses) = &step.uses {
            self.execute_action_step(
                step,
                uses,
                step_name,
                effective_wd,
                &step_env,
                extra_paths,
                masks,
                job_id,
                depth,
                ctx,
                run,
                executor,
                container_binary,
            )
            .await
        } else if let Some(script_source) = &step.run {
            let shell = step
                .shell
                .clone()
                .or_else(|| defaults.shell.clone())
                .unwrap_or_else(|| "bash".to_string());
            self.execute_shell_step(
                script_source,
                step_name,
                &shell,
                effective_wd,
                &step_env,
                extra_paths,
                masks,
                job_id,
                ctx,
                run,
                executor,
            )
            .await
        } else {
            Err(WorkflowError::Other(format!(
                "Step '{}' has no 'uses' or 'run'",
                step_name
            )))
        }
    }

    /// Execute a step that uses an action.
    ///
    /// Two kinds of action can answer a `uses:`, and they are tried in this
    /// order. A **registered** action is Rust in this process — the built-ins,
    /// or whatever an embedding tool added — and wins, because it needs
    /// nothing fetched and because a tool's own `uses:` names must keep
    /// resolving to its implementation. Everything else is **external**: a
    /// directory with an `action.yml`, fetched first when it is remote.
    #[allow(clippy::too_many_arguments)]
    async fn execute_action_step(
        &self,
        step: &Step,
        uses: &str,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        depth: usize,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
        container_binary: &str,
    ) -> Result<StepExecution, WorkflowError> {
        if self.actions.has_action(actions::registry_name(uses)) {
            return self
                .execute_registered_action(
                    step,
                    uses,
                    step_name,
                    effective_wd,
                    step_env,
                    masks,
                    job_id,
                    ctx,
                    run,
                )
                .await;
        }

        let reference = ActionRef::parse(uses)?;

        // Fetching happens before the step is announced as started, and says
        // so: a first run of a workflow can spend a while here.
        let mut notes = Vec::new();
        let resolved = actions::resolve_external(
            &reference,
            &self.workspace,
            &self.store,
            &mut |level, message| notes.push((level, message)),
        )
        .await?;
        for (level, message) in notes {
            run.emit(LogEvent::Message { level, message }).await;
        }

        run.emit(LogEvent::ActionStarted {
            uses: uses.to_string(),
        })
        .await;

        // `with:` goes through expression evaluation, then the manifest fills
        // in the defaults the workflow left out.
        let mut with = HashMap::new();
        for (key, value) in &step.with {
            let resolved_value = evaluate_at(&format!("{} with.{}", uses, key), value, ctx)?;
            run.emit(LogEvent::ActionInput {
                name: key.clone(),
                value: commands::apply_masks(&resolved_value, masks),
            })
            .await;
            with.insert(key.clone(), resolved_value);
        }
        let inputs = actions::action_inputs(&resolved.manifest, &with);
        for warning in &inputs.warnings {
            run.emit(LogEvent::Message {
                level: LogLevel::Warn,
                message: format!("{}: {}", uses, warning),
            })
            .await;
        }

        let outcome = match &resolved.manifest.runs {
            ActionRuns::Node {
                main,
                pre,
                pre_if,
                post,
                post_if,
                ..
            } => {
                self.run_node_action(
                    &resolved,
                    main.clone(),
                    pre.clone(),
                    pre_if.clone(),
                    post.clone(),
                    post_if.clone(),
                    &inputs,
                    step_name,
                    effective_wd,
                    step_env,
                    extra_paths,
                    masks,
                    job_id,
                    ctx,
                    run,
                    executor,
                )
                .await
            }

            ActionRuns::Composite { steps } => {
                self.run_composite_action(
                    &resolved,
                    steps,
                    &inputs,
                    step_name,
                    effective_wd,
                    step_env,
                    extra_paths,
                    masks,
                    job_id,
                    depth,
                    ctx,
                    run,
                    executor,
                    container_binary,
                )
                .await
            }

            ActionRuns::Docker {
                image,
                entrypoint,
                args,
                env,
                post_entrypoint,
                post_if,
                ..
            } => {
                self.run_container_action(
                    &resolved,
                    ContainerSpec {
                        image: image.clone(),
                        // A bare `uses: docker://image` has no manifest, so
                        // the step's `with:` is where the two of them live.
                        entrypoint: entrypoint
                            .clone()
                            .or_else(|| with.get("entrypoint").cloned()),
                        args: container_args(args, &with),
                        manifest_env: env.clone().into_iter().collect(),
                        post_entrypoint: post_entrypoint.clone(),
                        post_if: post_if.clone(),
                    },
                    &inputs,
                    &HashMap::new(),
                    step_name,
                    effective_wd,
                    step_env,
                    masks,
                    job_id,
                    ctx,
                    run,
                    container_binary,
                )
                .await
            }
        };

        match outcome {
            Ok(execution) => {
                run.emit(LogEvent::ActionFinished {
                    success: execution.result.success,
                    conclusion: execution.result.conclusion,
                })
                .await;
                Ok(execution)
            }
            Err(e) => {
                run.emit(LogEvent::ActionError {
                    message: e.to_string(),
                })
                .await;
                Ok(StepExecution::failed())
            }
        }
    }

    /// Execute an action implemented in Rust and held in the registry.
    #[allow(clippy::too_many_arguments)]
    async fn execute_registered_action(
        &self,
        step: &Step,
        uses: &str,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
    ) -> Result<StepExecution, WorkflowError> {
        let action_name = actions::registry_name(uses);
        let action = self
            .actions
            .get(action_name)
            .ok_or_else(|| WorkflowError::ActionNotFound(action_name.to_string()))?;

        run.emit(LogEvent::ActionStarted {
            uses: uses.to_string(),
        })
        .await;

        // Resolve with: parameters through expression evaluation
        let mut resolved_inputs = HashMap::new();
        for (k, v) in &step.with {
            let resolved = evaluate_at(&format!("{} with.{}", uses, k), v, ctx)?;
            run.emit(LogEvent::ActionInput {
                name: k.clone(),
                value: commands::apply_masks(&resolved, masks),
            })
            .await;
            resolved_inputs.insert(k.clone(), resolved);
        }

        // Actions see the workflow/job/step env plus the standard runner
        // variables, so a shelling-out action behaves like a `run:` step.
        let mut action_env = self.standard_env(ctx, job_id);
        action_env.extend(ctx.env.clone());
        action_env.extend(step_env.clone());

        let working_dir = effective_wd.as_ref().map(|wd| self.resolve_dir(wd));

        let action_ctx = ActionContext {
            inputs: resolved_inputs,
            env: action_env,
            workspace: self.workspace.clone(),
            working_directory: working_dir,
            temp_dir: PathBuf::from(&ctx.runner.temp)
                .join(format!("minact-{}", uuid::Uuid::new_v4())),
            context: ctx.clone(),
        };

        // Validate action inputs
        action
            .validate(&action_ctx)
            .map_err(|e| WorkflowError::StepFailed(step_name.to_string(), e.to_string()))?;

        // Run the action
        match action.run(&action_ctx).await {
            Ok(output) => {
                run.emit(LogEvent::ActionFinished {
                    success: output.success,
                    conclusion: output.conclusion,
                })
                .await;
                Ok(StepExecution::from_result(StepResult {
                    success: output.success,
                    conclusion: output.conclusion,
                    outputs: output.outputs,
                    artifacts: output.artifacts,
                }))
            }
            Err(e) => {
                run.emit(LogEvent::ActionError {
                    message: e.to_string(),
                })
                .await;
                Ok(StepExecution::failed())
            }
        }
    }

    /// Run a JavaScript action: its `pre:` hook if it has one and the
    /// condition holds, then its `main:`, queueing `post:` for the job's end.
    #[allow(clippy::too_many_arguments)]
    async fn run_node_action(
        &self,
        resolved: &ResolvedAction,
        main: String,
        pre: Option<String>,
        pre_if: Option<String>,
        post: Option<String>,
        post_if: Option<String>,
        inputs: &ActionInputs,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
    ) -> Result<StepExecution, WorkflowError> {
        // Where the action is from the step's point of view: the same place
        // for local and Docker runners, a copy on the far side for SSH.
        let sink = StepSink::new(run.clone(), masks.to_vec());
        let action_dir = executor.provision_dir(&resolved.dir, &sink).await?;

        if let Some(pre) = pre {
            // An absent `pre-if` means `always()`, not `success()`.
            if hook_should_run(pre_if.as_deref(), ctx)? {
                let execution = self
                    .run_node_entry(
                        resolved,
                        &action_dir,
                        &pre,
                        inputs,
                        &HashMap::new(),
                        step_name,
                        effective_wd.clone(),
                        step_env,
                        extra_paths,
                        masks,
                        job_id,
                        ctx,
                        run,
                        executor,
                    )
                    .await?;
                // A failed `pre:` means the action never set itself up; there
                // is nothing for `main:` to do.
                if !execution.result.success {
                    return Ok(execution);
                }
            }
        }

        let mut execution = self
            .run_node_entry(
                resolved,
                &action_dir,
                &main,
                inputs,
                &HashMap::new(),
                step_name,
                effective_wd.clone(),
                step_env,
                extra_paths,
                masks,
                job_id,
                ctx,
                run,
                executor,
            )
            .await?;

        if let Some(post) = post {
            execution.post.push(PostHook {
                step_name: step_name.to_string(),
                action: resolved.clone(),
                action_dir,
                kind: PostKind::Node { entry: post },
                condition: post_if,
                inputs: inputs.clone(),
                // Whatever `main:` saved is exactly what `post:` reads back.
                state: execution.state.iter().cloned().collect(),
                step_env: step_env.clone(),
                working_directory: effective_wd,
            });
        }

        Ok(execution)
    }

    /// Run one JavaScript entry point of an action.
    #[allow(clippy::too_many_arguments)]
    async fn run_node_entry(
        &self,
        resolved: &ResolvedAction,
        action_dir: &Path,
        entry: &str,
        inputs: &ActionInputs,
        state: &HashMap<String, String>,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
    ) -> Result<StepExecution, WorkflowError> {
        let entry_path = action_entry(action_dir, entry)?;
        let working_dir = effective_wd
            .as_ref()
            .map(|wd| self.resolve_dir(wd))
            .unwrap_or_else(|| self.workspace.clone());

        let mut env = self.build_process_env(ctx, job_id, step_env, extra_paths);
        env.extend(inputs.env.clone());
        env.extend(
            state
                .iter()
                .map(|(key, value)| (format!("STATE_{}", key), value.clone())),
        );
        env.extend(self.action_env(resolved, action_dir));

        // Whatever `node` is on the runner. GitHub ships its own; minact uses
        // the one that is there, and `MINACT_NODE` picks a different one.
        let node = std::env::var("MINACT_NODE").unwrap_or_else(|_| "node".to_string());
        let command = vec![node, entry_path.to_string_lossy().to_string()];

        run.emit(LogEvent::CommandStarted {
            command: commands::apply_masks(&command.join(" "), masks),
            shell: "node".to_string(),
            working_dir: working_dir.display().to_string(),
        })
        .await;

        let request = StepRequest {
            step_name: step_name.to_string(),
            script: String::new(),
            shell: "node".to_string(),
            working_directory: working_dir,
            env,
            runner_temp: PathBuf::from(&ctx.runner.temp),
            command: Some(command),
        };

        self.run_through_executor(request, step_name, masks, run, executor)
            .await
    }

    /// Run a composite action: its steps, in the caller's job, over a context
    /// of its own.
    #[allow(clippy::too_many_arguments)]
    async fn run_composite_action(
        &self,
        resolved: &ResolvedAction,
        steps: &[Step],
        inputs: &ActionInputs,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        depth: usize,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
        container_binary: &str,
    ) -> Result<StepExecution, WorkflowError> {
        const MAX_DEPTH: usize = 10;
        if depth >= MAX_DEPTH {
            return Err(WorkflowError::Other(format!(
                "composite actions nest more than {} deep at `{}`; this is usually a cycle",
                MAX_DEPTH, step_name
            )));
        }

        let sink = StepSink::new(run.clone(), masks.to_vec());
        let action_dir = executor.provision_dir(&resolved.dir, &sink).await?;

        // Inside a composite, `inputs` are the action's own and `steps` starts
        // empty: its steps cannot see the caller's, and the caller cannot see
        // theirs.
        let mut inner = ctx.clone();
        inner.inputs = inputs.values.clone();
        inner.step_outputs = HashMap::new();
        inner.step_status = HashMap::new();
        inner.env.extend(step_env.clone());
        inner.status = RunStatus::success();
        for (key, value) in self.action_env(resolved, &action_dir) {
            match key.as_str() {
                "GITHUB_ACTION" => inner.github.action = value,
                "GITHUB_ACTION_PATH" => inner.github.action_path = value,
                "GITHUB_ACTION_REPOSITORY" => inner.github.action_repository = value,
                "GITHUB_ACTION_REF" => inner.github.action_ref = value,
                _ => {}
            }
        }

        let mut execution = StepExecution::from_result(StepResult {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: Vec::new(),
        });
        let mut paths = extra_paths.to_vec();
        let mut inner_masks = masks.to_vec();
        let mut failed = false;
        let mut cancelled = false;

        // A composite has no `defaults:` of its own; its steps inherit the
        // step's working directory and pick their own shell.
        let defaults = StepDefaults {
            working_directory: effective_wd.clone(),
            shell: None,
        };

        for (index, inner_step) in steps.iter().enumerate() {
            if run.is_cancelled() {
                cancelled = true;
                break;
            }

            inner.status = if failed {
                RunStatus::failure()
            } else {
                RunStatus::success()
            };

            let inner_name = display_step_name(inner_step, index);
            let should_run = match &inner_step.if_condition {
                Some(condition) => match evaluate_if_condition(condition, &inner) {
                    Ok(should_run) => should_run,
                    Err(e) => {
                        run.emit(LogEvent::Message {
                            level: LogLevel::Error,
                            message: format!(
                                "{}: step '{}' has an invalid `if` condition: {}",
                                step_name, inner_name, e
                            ),
                        })
                        .await;
                        failed = true;
                        continue;
                    }
                },
                None => !failed,
            };

            if !should_run {
                self.record_step(
                    &mut inner,
                    inner_step,
                    StepConclusion::Skipped,
                    StepConclusion::Skipped,
                    &HashMap::new(),
                );
                continue;
            }

            run.emit(LogEvent::Message {
                level: LogLevel::Info,
                message: format!("{} ▸ {}", step_name, inner_name),
            })
            .await;

            // Boxed because a composite step can be another composite action,
            // which lands back here.
            let inner_execution = Box::pin(self.execute_step(
                inner_step,
                &inner_name,
                job_id,
                &defaults,
                &paths,
                &inner_masks,
                depth + 1,
                &inner,
                run,
                executor,
                container_binary,
            ))
            .await;

            let inner_execution = match inner_execution {
                Ok(execution) => execution,
                Err(e) => {
                    run.emit(LogEvent::Message {
                        level: LogLevel::Error,
                        message: format!("{}: {}", step_name, e),
                    })
                    .await;
                    StepExecution::failed()
                }
            };

            // Everything a nested step exported keeps flowing outwards: a
            // composite writing `$GITHUB_ENV` or `$GITHUB_PATH` changes the
            // job's environment, exactly as on GitHub.
            for (key, value) in &inner_execution.env_updates {
                inner.env.insert(key.clone(), value.clone());
            }
            for path in inner_execution.path_additions.iter().rev() {
                paths.insert(0, path.clone());
            }
            inner_masks.extend(inner_execution.masks.iter().cloned());
            execution
                .env_updates
                .extend(inner_execution.env_updates.iter().cloned());
            execution
                .path_additions
                .extend(inner_execution.path_additions.iter().cloned());
            execution
                .masks
                .extend(inner_execution.masks.iter().cloned());
            // A `post:` registered inside a composite belongs to the job, not
            // to the composite, so it travels up rather than running here.
            execution.post.extend(inner_execution.post);

            let outcome = inner_execution.result.conclusion;
            let conclusion = if outcome == StepConclusion::Failure
                && inner_step.continue_on_error.unwrap_or(false)
            {
                StepConclusion::Success
            } else {
                outcome
            };

            self.record_step(
                &mut inner,
                inner_step,
                outcome,
                conclusion,
                &inner_execution.result.outputs,
            );

            match conclusion {
                StepConclusion::Failure => failed = true,
                StepConclusion::Cancelled => cancelled = true,
                _ => {}
            }
        }

        // A composite's outputs are expressions over its own steps, so they
        // are evaluated here rather than written to `$GITHUB_OUTPUT`.
        let mut outputs = HashMap::new();
        for (name, spec) in &resolved.manifest.outputs {
            if let Some(value) = &spec.value {
                outputs.insert(
                    name.clone(),
                    evaluate_at(&format!("{} outputs.{}", step_name, name), value, &inner)?,
                );
            }
        }

        execution.result = StepResult {
            success: !failed && !cancelled,
            conclusion: match (cancelled, failed) {
                (true, _) => StepConclusion::Cancelled,
                (false, true) => StepConclusion::Failure,
                (false, false) => StepConclusion::Success,
            },
            outputs,
            artifacts: Vec::new(),
        };
        Ok(execution)
    }

    /// Run a container action.
    ///
    /// This is the one action kind that does not go through the job's
    /// executor: the action brings its own image, so it gets its own
    /// container even when the job itself is running locally.
    #[allow(clippy::too_many_arguments)]
    async fn run_container_action(
        &self,
        resolved: &ResolvedAction,
        spec: ContainerSpec,
        inputs: &ActionInputs,
        state: &HashMap<String, String>,
        step_name: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
        container_binary: &str,
    ) -> Result<StepExecution, WorkflowError> {
        let working_dir = effective_wd
            .as_ref()
            .map(|wd| self.resolve_dir(wd))
            .unwrap_or_else(|| self.workspace.clone());

        // `runs.args` and `runs.env` are expressions over the action's own
        // inputs, so they are evaluated against a context that has them.
        let mut inner = ctx.clone();
        inner.inputs = inputs.values.clone();
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|arg| evaluate_at(&format!("{} runs.args", step_name), arg, &inner))
            .collect::<Result<_, _>>()?;

        // A container does not inherit the host environment — its PATH and its
        // tools are its own. Only what the workflow declared goes in.
        let mut env = self.standard_env(ctx, job_id);
        env.extend(ctx.env.clone());
        env.extend(step_env.clone());
        for (key, value) in &spec.manifest_env {
            env.insert(
                key.clone(),
                evaluate_at(&format!("{} runs.env.{}", step_name, key), value, &inner)?,
            );
        }
        env.extend(inputs.env.clone());
        env.extend(
            state
                .iter()
                .map(|(key, value)| (format!("STATE_{}", key), value.clone())),
        );
        env.extend(self.action_env(resolved, &resolved.dir));

        let runner_temp = PathBuf::from(&ctx.runner.temp);
        run.emit(LogEvent::CommandStarted {
            command: commands::apply_masks(
                &match &spec.image {
                    DockerImageSource::Registry(image) => format!("{} {}", image, args.join(" ")),
                    DockerImageSource::Dockerfile(file) => {
                        format!("build {} {}", file, args.join(" "))
                    }
                },
                masks,
            ),
            shell: container_binary.to_string(),
            working_dir: working_dir.display().to_string(),
        })
        .await;

        let sink = StepSink::new(run.clone(), masks.to_vec());
        let outcome = ContainerAction {
            binary: container_binary,
            image: &spec.image,
            action_dir: &resolved.dir,
            entrypoint: spec.entrypoint.as_deref(),
            args,
            env,
            workspace: &self.workspace,
            runner_temp: &runner_temp,
            working_directory: &working_dir,
            step_name,
        }
        .run(&sink, &run.cancel)
        .await?;

        let mut execution = self.finish_execution(outcome, sink, step_name, run).await;

        if let Some(post_entrypoint) = spec.post_entrypoint {
            execution.post.push(PostHook {
                step_name: step_name.to_string(),
                action: resolved.clone(),
                action_dir: resolved.dir.clone(),
                kind: PostKind::Container {
                    image: spec.image,
                    entrypoint: post_entrypoint,
                    manifest_env: spec.manifest_env,
                    binary: container_binary.to_string(),
                },
                condition: spec.post_if,
                inputs: inputs.clone(),
                state: execution.state.iter().cloned().collect(),
                step_env: step_env.clone(),
                working_directory: effective_wd,
            });
        }

        Ok(execution)
    }

    /// Run one queued `post:` hook, when its condition holds.
    #[allow(clippy::too_many_arguments)]
    async fn run_post_hook(
        &self,
        hook: PostHook,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
    ) -> Result<StepExecution, WorkflowError> {
        // A `post:` with no condition runs whatever happened, which is the
        // point of it: cleanup has to survive the failure it is cleaning up.
        if !hook_should_run(hook.condition.as_deref(), ctx)? {
            return Ok(StepExecution::from_result(StepResult {
                success: true,
                conclusion: StepConclusion::Skipped,
                outputs: HashMap::new(),
                artifacts: Vec::new(),
            }));
        }

        run.emit(LogEvent::Message {
            level: LogLevel::Info,
            message: format!("post: {}", hook.step_name),
        })
        .await;

        match hook.kind {
            PostKind::Node { entry } => {
                self.run_node_entry(
                    &hook.action,
                    &hook.action_dir,
                    &entry,
                    &hook.inputs,
                    &hook.state,
                    &hook.step_name,
                    hook.working_directory,
                    &hook.step_env,
                    extra_paths,
                    masks,
                    job_id,
                    ctx,
                    run,
                    executor,
                )
                .await
            }
            PostKind::Container {
                image,
                entrypoint,
                manifest_env,
                binary,
            } => {
                self.run_container_action(
                    &hook.action,
                    ContainerSpec {
                        image,
                        entrypoint: Some(entrypoint),
                        args: Vec::new(),
                        manifest_env,
                        post_entrypoint: None,
                        post_if: None,
                    },
                    &hook.inputs,
                    &hook.state,
                    &hook.step_name,
                    hook.working_directory,
                    &hook.step_env,
                    masks,
                    job_id,
                    ctx,
                    run,
                    &binary,
                )
                .await
            }
        }
    }

    /// The `GITHUB_ACTION*` variables identifying the running action.
    fn action_env(&self, resolved: &ResolvedAction, action_dir: &Path) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert(
            "GITHUB_ACTION".to_string(),
            actions::external::action_slug(&resolved.reference),
        );
        env.insert(
            "GITHUB_ACTION_PATH".to_string(),
            action_dir.to_string_lossy().to_string(),
        );
        env.insert(
            "GITHUB_ACTION_REPOSITORY".to_string(),
            resolved.reference.repository(),
        );
        if let ActionRef::Repository { git_ref, .. } = &resolved.reference {
            env.insert("GITHUB_ACTION_REF".to_string(), git_ref.clone());
        }
        env
    }

    /// Execute a shell step.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn execute_shell_step(
        &self,
        script: &str,
        step_name: &str,
        shell: &str,
        effective_wd: Option<String>,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
        masks: &[String],
        job_id: &str,
        ctx: &Context,
        run: &Run,
        executor: &dyn Executor,
    ) -> Result<StepExecution, WorkflowError> {
        // Resolve expressions in the run command
        let resolved_run = evaluate_at(&format!("step '{}' run", step_name), script, ctx)?;

        let working_dir = effective_wd
            .as_ref()
            .map(|wd| self.resolve_dir(wd))
            .unwrap_or_else(|| self.workspace.clone());

        let env = self.build_process_env(ctx, job_id, step_env, extra_paths);

        // The command is echoed before it runs, so it has to be redacted the
        // same way its output is — a masked secret that leaks in the echo has
        // not been masked.
        run.emit(LogEvent::CommandStarted {
            command: commands::apply_masks(&resolved_run, masks),
            shell: shell.to_string(),
            working_dir: working_dir.display().to_string(),
        })
        .await;

        let request = StepRequest {
            step_name: step_name.to_string(),
            script: resolved_run,
            shell: shell.to_string(),
            working_directory: working_dir,
            env,
            runner_temp: PathBuf::from(&ctx.runner.temp),
            command: None,
        };

        self.run_through_executor(request, step_name, masks, run, executor)
            .await
    }

    /// Run one request through the executor and fold everything it reported
    /// back into a [`StepExecution`].
    ///
    /// Shared by `run:` steps and by JavaScript actions, which differ only in
    /// what they ask the executor to spawn.
    async fn run_through_executor(
        &self,
        request: StepRequest,
        step_name: &str,
        masks: &[String],
        run: &Run,
        executor: &dyn Executor,
    ) -> Result<StepExecution, WorkflowError> {
        // The executor owns the transport; interpreting what the step prints
        // stays here, in the sink.
        let sink = StepSink::new(run.clone(), masks.to_vec());
        let outcome = executor.run_step(request, &sink, &run.cancel).await?;

        run.emit(LogEvent::CommandFinished {
            success: outcome.success,
            status: outcome.status.clone(),
        })
        .await;

        Ok(self.finish_execution(outcome, sink, step_name, run).await)
    }

    /// Turn a finished [`StepOutcome`] into a [`StepExecution`], folding in
    /// what the step printed and what it wrote to its environment files.
    async fn finish_execution(
        &self,
        outcome: crate::executor::StepOutcome,
        sink: StepSink,
        step_name: &str,
        run: &Run,
    ) -> StepExecution {
        let mut success = outcome.success;
        let capture = sink.into_capture().await;

        // Start from what the step printed as `::` commands, then layer the
        // environment files on top — the files are the current mechanism and
        // win on conflict.
        let mut outputs: HashMap<String, String> = capture.outputs.into_iter().collect();
        let mut env_updates = capture.env;
        let mut path_additions = capture.paths;

        match outcome.files.parse() {
            Ok(read) => {
                outputs.extend(read.outputs);
                env_updates.extend(read.env);
                path_additions.extend(read.paths);
                if !read.summary.trim().is_empty() {
                    run.emit(LogEvent::Message {
                        level: LogLevel::Info,
                        message: format!("step summary:\n{}", read.summary.trim_end()),
                    })
                    .await;
                }
            }
            Err(e) => {
                // Malformed output/env files are a step error on GitHub too —
                // silently dropping the values would be worse.
                run.emit(LogEvent::Message {
                    level: LogLevel::Error,
                    message: format!(
                        "Step '{}' wrote an invalid environment file: {}",
                        step_name, e
                    ),
                })
                .await;
                success = false;
            }
        }

        let cancelled = outcome.cancelled;
        StepExecution {
            result: StepResult {
                success: success && !cancelled,
                conclusion: match (cancelled, success) {
                    (true, _) => StepConclusion::Cancelled,
                    (false, true) => StepConclusion::Success,
                    (false, false) => StepConclusion::Failure,
                },
                outputs,
                artifacts: Vec::new(),
            },
            env_updates,
            path_additions,
            masks: capture.masks,
            state: capture.state,
            post: Vec::new(),
        }
    }

    /// Resolve a working-directory string against the workspace.
    fn resolve_dir(&self, dir: &str) -> PathBuf {
        if Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            self.workspace.join(dir)
        }
    }

    /// The `GITHUB_*` / `RUNNER_*` variables every step can rely on.
    fn standard_env(&self, ctx: &Context, job_id: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("CI".to_string(), "true".to_string());
        env.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        env.insert("GITHUB_WORKSPACE".to_string(), ctx.github.workspace.clone());
        env.insert(
            "GITHUB_REPOSITORY".to_string(),
            ctx.github.repository.clone(),
        );
        env.insert(
            "GITHUB_REPOSITORY_OWNER".to_string(),
            ctx.github
                .repository
                .split('/')
                .next()
                .unwrap_or_default()
                .to_string(),
        );
        env.insert("GITHUB_REF".to_string(), ctx.github.ref_name.clone());
        env.insert(
            "GITHUB_REF_NAME".to_string(),
            expr::short_ref_name(&ctx.github.ref_name),
        );
        env.insert("GITHUB_SHA".to_string(), ctx.github.sha.clone());
        env.insert("GITHUB_ACTOR".to_string(), ctx.github.actor.clone());
        env.insert(
            "GITHUB_EVENT_NAME".to_string(),
            ctx.github.event_name.clone(),
        );
        env.insert("GITHUB_JOB".to_string(), job_id.to_string());
        env.insert("GITHUB_WORKFLOW".to_string(), ctx.github.workflow.clone());
        env.insert("GITHUB_RUN_ID".to_string(), ctx.github.run_id.clone());
        env.insert(
            "GITHUB_RUN_NUMBER".to_string(),
            ctx.github.run_number.clone(),
        );
        env.insert(
            "GITHUB_RUN_ATTEMPT".to_string(),
            ctx.github.run_attempt.clone(),
        );
        env.insert("GITHUB_REF_TYPE".to_string(), ctx.github.ref_type.clone());
        env.insert(
            "GITHUB_SERVER_URL".to_string(),
            ctx.github.server_url.clone(),
        );
        env.insert("GITHUB_API_URL".to_string(), ctx.github.api_url.clone());
        env.insert(
            "GITHUB_GRAPHQL_URL".to_string(),
            ctx.github.graphql_url.clone(),
        );
        env.insert(
            "GITHUB_TRIGGERING_ACTOR".to_string(),
            ctx.github.actor.clone(),
        );
        if !ctx.github.base_ref.is_empty() {
            env.insert("GITHUB_BASE_REF".to_string(), ctx.github.base_ref.clone());
        }
        if !ctx.github.head_ref.is_empty() {
            env.insert("GITHUB_HEAD_REF".to_string(), ctx.github.head_ref.clone());
        }
        if !ctx.github.event_path.is_empty() {
            env.insert(
                "GITHUB_EVENT_PATH".to_string(),
                ctx.github.event_path.clone(),
            );
        }
        env.insert("RUNNER_OS".to_string(), ctx.runner.os.clone());
        env.insert("RUNNER_ARCH".to_string(), ctx.runner.arch.clone());
        env.insert("RUNNER_TEMP".to_string(), ctx.runner.temp.clone());
        env.insert(
            "RUNNER_TOOL_CACHE".to_string(),
            ctx.runner.tool_cache.clone(),
        );
        env
    }

    /// Build the full environment for a shell step.
    ///
    /// The host environment is inherited — a local runner without the host
    /// `PATH` cannot run `git`, `cargo` or `flutter` — and is then layered
    /// with the runner variables, the workflow/job env and the step env.
    fn build_process_env(
        &self,
        ctx: &Context,
        job_id: &str,
        step_env: &HashMap<String, String>,
        extra_paths: &[String],
    ) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.extend(self.standard_env(ctx, job_id));
        env.extend(ctx.env.clone());
        env.extend(step_env.clone());

        if !extra_paths.is_empty() {
            let mut parts: Vec<String> = extra_paths.to_vec();
            if let Some(current) = env.get("PATH") {
                if !current.is_empty() {
                    parts.push(current.clone());
                }
            }
            env.insert("PATH".to_string(), parts.join(PATH_SEPARATOR));
        }

        env
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

        // `workflow_dispatch` inputs fall back to their declared defaults.
        let mut resolved_inputs = HashMap::new();
        if let Some(dispatch) = &workflow.on.workflow_dispatch {
            if let Some(declared) = &dispatch.inputs {
                for (name, config) in declared {
                    if let Some(default) = &config.default {
                        resolved_inputs.insert(name.clone(), default.clone());
                    }
                }
            }
        }
        resolved_inputs.extend(inputs);

        let git_ref = std::env::var("GITHUB_REF").unwrap_or_else(|_| "refs/heads/main".to_string());
        let server_url =
            std::env::var("GITHUB_SERVER_URL").unwrap_or_else(|_| "https://github.com".to_string());
        // A real payload makes `github.event.*` mean something locally, which
        // is the difference between testing a `pull_request` workflow and only
        // pretending to.
        let (event_path, event) = load_event_payload();

        Context {
            github: GithubContext {
                event_name: event_name.to_string(),
                event,
                repository: std::env::var("GITHUB_REPOSITORY")
                    .unwrap_or_else(|_| "local/repo".to_string()),
                ref_name: git_ref.clone(),
                sha: std::env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_string()),
                workspace: self.workspace.to_string_lossy().to_string(),
                action: String::new(),
                actor: std::env::var("USER").unwrap_or_else(|_| "local".to_string()),
                // Filled in only while an action is running.
                action_path: String::new(),
                action_repository: String::new(),
                action_ref: String::new(),

                workflow: if workflow.name.is_empty() {
                    workflow
                        .file_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                } else {
                    workflow.name.clone()
                },
                // Set per job, once there is one.
                job: String::new(),
                // No server here hands out run numbers, so the id is unique
                // rather than sequential — enough for naming things after a
                // run, which is what workflows use it for.
                run_id: std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|since| since.as_millis().to_string())
                        .unwrap_or_else(|_| "1".to_string())
                }),
                run_number: std::env::var("GITHUB_RUN_NUMBER").unwrap_or_else(|_| "1".to_string()),
                run_attempt: std::env::var("GITHUB_RUN_ATTEMPT")
                    .unwrap_or_else(|_| "1".to_string()),
                ref_type: ref_type(&git_ref),
                ref_protected: false,
                base_ref: std::env::var("GITHUB_BASE_REF").unwrap_or_default(),
                head_ref: std::env::var("GITHUB_HEAD_REF").unwrap_or_default(),
                server_url,
                api_url: std::env::var("GITHUB_API_URL")
                    .unwrap_or_else(|_| "https://api.github.com".to_string()),
                graphql_url: std::env::var("GITHUB_GRAPHQL_URL")
                    .unwrap_or_else(|_| "https://api.github.com/graphql".to_string()),
                event_path,
                token: std::env::var("GITHUB_TOKEN").unwrap_or_default(),
            },
            env: workflow.env.clone(),
            secrets: HashMap::new(), // Secrets are loaded from env or .env files
            job_outputs: HashMap::new(),
            job_results: HashMap::new(),
            step_outputs: HashMap::new(),
            step_status: HashMap::new(),
            inputs: resolved_inputs,
            matrix: HashMap::new(),
            strategy: StrategyContext::default(),
            runner: RunnerContext {
                os: runner_os_name(),
                arch: runner_arch_name(),
                temp: temp.to_string_lossy().to_string(),
                tool_cache: tool_cache.to_string_lossy().to_string(),
            },
            status: RunStatus::success(),
        }
    }
}

/// The default `run:` settings that apply to a step, resolved from the job's
/// `defaults:` then the workflow's.
#[derive(Debug, Default, Clone)]
struct StepDefaults {
    working_directory: Option<String>,
    shell: Option<String>,
}

impl StepDefaults {
    fn resolve(job: &Job, workflow: &Workflow) -> Self {
        let job_run = job.defaults.as_ref().and_then(|d| d.run.as_ref());
        let workflow_run = workflow.defaults.as_ref().and_then(|d| d.run.as_ref());

        let working_directory = job_run
            .and_then(|r| r.working_directory.clone())
            .or_else(|| workflow_run.and_then(|r| r.working_directory.clone()));

        // `RunDefaults::shell` is a plain String, so empty means "unset".
        let shell = job_run
            .map(|r| r.shell.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                workflow_run
                    .map(|r| r.shell.clone())
                    .filter(|s| !s.is_empty())
            });

        Self {
            working_directory,
            shell,
        }
    }
}

/// What a step handed back to the runner beyond its exit status.
/// What an action's `post:` hook needs to run once the job is over.
///
/// GitHub runs these after the last step, in reverse order — the action that
/// set something up last tears it down first — and runs them whatever the job
/// did, because cleanup has to survive the failure it is cleaning up.
struct PostHook {
    /// The step that registered the hook, for attributing its output.
    step_name: String,
    action: ResolvedAction,
    /// The action directory as steps see it, which is not the host path when
    /// the job runs over SSH.
    action_dir: PathBuf,
    kind: PostKind,
    /// `post-if`, defaulting to `always()`.
    condition: Option<String>,
    inputs: ActionInputs,
    /// What the action saved with `::save-state::`, arriving as `STATE_*`.
    state: HashMap<String, String>,
    step_env: HashMap<String, String>,
    working_directory: Option<String>,
}

/// How a queued `post:` hook runs.
enum PostKind {
    Node {
        entry: String,
    },
    Container {
        image: DockerImageSource,
        entrypoint: String,
        manifest_env: HashMap<String, String>,
        binary: String,
    },
}

/// The parts of a container action that describe one invocation of it.
struct ContainerSpec {
    image: DockerImageSource,
    entrypoint: Option<String>,
    args: Vec<String>,
    manifest_env: HashMap<String, String>,
    post_entrypoint: Option<String>,
    post_if: Option<String>,
}

struct StepExecution {
    result: StepResult,
    /// `$GITHUB_ENV` / `::set-env::` values, visible to later steps.
    env_updates: Vec<(String, String)>,
    /// `$GITHUB_PATH` / `::add-path::` entries, prepended to `PATH`.
    path_additions: Vec<String>,
    /// `::add-mask::` values, redacted from later log output.
    masks: Vec<String>,
    /// `::save-state::` values, handed to the action's `post:` hook as
    /// `STATE_*` and invisible to everything else.
    state: Vec<(String, String)>,
    /// Hooks this step registered to run when the job ends.
    post: Vec<PostHook>,
}

impl StepExecution {
    fn from_result(result: StepResult) -> Self {
        Self {
            result,
            env_updates: Vec::new(),
            path_additions: Vec::new(),
            masks: Vec::new(),
            state: Vec::new(),
            post: Vec::new(),
        }
    }

    fn failed() -> Self {
        Self::from_result(StepResult {
            success: false,
            conclusion: StepConclusion::Failure,
            outputs: HashMap::new(),
            artifacts: Vec::new(),
        })
    }
}

/// Interprets a step's output as it arrives: workflow commands are consumed,
/// everything else is masked and reported.
///
/// This is the half of step output that stays with the engine no matter where
/// the step ran, which is why executors only hand back lines.
struct StepSink {
    run: Run,
    capture: Mutex<CommandCapture>,
}

impl StepSink {
    fn new(run: Run, masks: Vec<String>) -> Self {
        Self {
            run,
            capture: Mutex::new(CommandCapture::new(masks)),
        }
    }

    async fn into_capture(self) -> CommandCapture {
        self.capture.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl OutputSink for StepSink {
    async fn line(&self, stream: CommandStream, line: String) {
        let mut capture = self.capture.lock().await;

        if let Some(command) = commands::parse_workflow_command(&line) {
            if let Some(event) = handle_workflow_command(&command, &mut capture) {
                self.run.emit(event).await;
            }
            return;
        }

        let line = commands::apply_masks(&line, &capture.masks);
        drop(capture);
        self.run
            .emit(LogEvent::CommandOutput { stream, line })
            .await;
    }

    async fn note(&self, level: LogLevel, message: String) {
        self.run.emit(LogEvent::Message { level, message }).await;
    }
}

/// Everything picked up from a step's stdout/stderr while it runs.
#[derive(Default, Clone)]
struct CommandCapture {
    outputs: Vec<(String, String)>,
    env: Vec<(String, String)>,
    paths: Vec<String>,
    masks: Vec<String>,
    state: Vec<(String, String)>,
}

impl CommandCapture {
    fn new(masks: Vec<String>) -> Self {
        Self {
            masks,
            ..Default::default()
        }
    }
}

/// Apply a workflow command, returning a log event when it should surface.
///
/// Returning `None` means the command was consumed silently — either it is
/// runner bookkeeping, or it is a command whose only effect is on `capture`.
fn handle_workflow_command(
    command: &ParsedCommand,
    capture: &mut CommandCapture,
) -> Option<LogEvent> {
    match command.name.as_str() {
        "set-output" => {
            let name = command.property("name")?.to_string();
            capture.outputs.push((name, command.message.clone()));
            None
        }
        "set-env" => {
            let name = command.property("name")?.to_string();
            capture.env.push((name, command.message.clone()));
            None
        }
        "add-path" => {
            capture.paths.push(command.message.clone());
            None
        }
        // Only the action's own `post:` hook ever reads this back, so it is
        // captured rather than merged into the environment.
        "save-state" => {
            let name = command.property("name")?.to_string();
            capture.state.push((name, command.message.clone()));
            None
        }
        "add-mask" => {
            if !command.message.is_empty() {
                capture.masks.push(command.message.clone());
            }
            None
        }
        "error" | "warning" | "notice" => {
            let level = match command.name.as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warn,
                _ => LogLevel::Info,
            };
            Some(LogEvent::Message {
                level,
                message: format_annotation(command),
            })
        }
        "debug" => Some(LogEvent::Message {
            level: LogLevel::Info,
            message: format!("debug: {}", command.message),
        }),
        "group" => Some(LogEvent::Message {
            level: LogLevel::Info,
            message: format!("▾ {}", command.message),
        }),
        // Runner bookkeeping with no local meaning.
        "endgroup" | "echo" | "stop-commands" | "add-matcher" | "remove-matcher" => None,
        _ => None,
    }
}

/// Render an `::error file=…,line=…::message` annotation as one line.
fn format_annotation(command: &ParsedCommand) -> String {
    let mut location = String::new();
    if let Some(file) = command.property("file") {
        location.push_str(file);
        if let Some(line) = command.property("line") {
            location.push(':');
            location.push_str(line);
            if let Some(col) = command
                .property("col")
                .or_else(|| command.property("column"))
            {
                location.push(':');
                location.push_str(col);
            }
        }
    }

    let title = command.property("title").unwrap_or_default();
    match (location.is_empty(), title.is_empty()) {
        (true, true) => command.message.clone(),
        (true, false) => format!("{}: {}", title, command.message),
        (false, true) => format!("{}: {}", location, command.message),
        (false, false) => format!("{} [{}]: {}", location, title, command.message),
    }
}

/// The action cache to use when the caller does not supply one.
///
/// A cache that cannot be created must not stop a run that uses no remote
/// actions, so this falls back to a temp directory rather than failing.
fn default_action_store() -> ActionStore {
    crate::actions::store::default_cache_dir()
        .map(ActionStore::with_root)
        .unwrap_or_else(|_| ActionStore::with_root(std::env::temp_dir().join("minact-actions")))
}

/// `branch` or `tag`, as `github.ref_type` spells it.
fn ref_type(git_ref: &str) -> String {
    if git_ref.starts_with("refs/tags/") {
        "tag".to_string()
    } else {
        "branch".to_string()
    }
}

/// Read the event payload named by `GITHUB_EVENT_PATH`, if there is one.
///
/// Returns the path alongside the payload so `github.event_path` can point at
/// the same file the payload came from.
fn load_event_payload() -> (String, HashMap<String, serde_json::Value>) {
    let Ok(path) = std::env::var("GITHUB_EVENT_PATH") else {
        return (String::new(), HashMap::new());
    };
    if path.is_empty() {
        return (String::new(), HashMap::new());
    }
    match std::fs::read_to_string(&path)
        .ok()
        .and_then(|source| serde_json::from_str::<serde_json::Value>(&source).ok())
    {
        Some(serde_json::Value::Object(fields)) => (path, fields.into_iter().collect()),
        // A payload that is missing or not an object leaves `github.event`
        // empty rather than failing the run before anything has been tried.
        _ => (path, HashMap::new()),
    }
}

/// The extra `docker run` arguments a job-level `container:` asks for.
///
/// `volumes` and `ports` map straight onto their docker flags; `options` is a
/// command line the workflow wrote, split the way a shell would split it.
fn container_run_args(container: &crate::workflow::JobContainer) -> Vec<String> {
    let mut args = Vec::new();
    for volume in &container.volumes {
        args.push("--volume".to_string());
        args.push(volume.clone());
    }
    for port in &container.ports {
        args.push("--publish".to_string());
        args.push(port.clone());
    }
    if let Some(options) = &container.options {
        args.extend(actions::external::split_arguments(options));
    }
    args
}

/// The container CLI a container action should use.
///
/// A job that runs on `podman` should not have its actions reach for `docker`,
/// so the job's own runner decides.
fn container_binary(spec: &RunnerSpec) -> String {
    match spec {
        RunnerSpec::Docker { binary, .. } => binary.clone(),
        _ => "docker".to_string(),
    }
}

/// Resolve an entry point declared in a manifest against the action directory.
///
/// The path comes out of a file the workflow author may not have written, so a
/// `main:` of `../../../etc/passwd` is refused rather than joined.
fn action_entry(dir: &Path, entry: &str) -> Result<PathBuf, WorkflowError> {
    let entry = entry.trim().replace('\\', "/");
    if entry.is_empty()
        || Path::new(&entry).is_absolute()
        || entry.split('/').any(|segment| segment == "..")
    {
        return Err(WorkflowError::Other(format!(
            "action entry point `{}` must be a relative path inside the action",
            entry
        )));
    }
    Ok(dir.join(entry))
}

/// Whether a `pre-if` / `post-if` condition holds.
///
/// These default to `always()`, not to `success()` — an action's cleanup is
/// expected to run after the failure it cleans up.
fn hook_should_run(condition: Option<&str>, ctx: &Context) -> Result<bool, WorkflowError> {
    match condition {
        Some(condition) => evaluate_if_condition(condition, ctx),
        None => Ok(true),
    }
}

/// The arguments a container action is invoked with.
///
/// A manifest declares them as a list. A bare `uses: docker://image` has no
/// manifest, and the step's `with.args` stands in — one string, split the way
/// a shell would split it.
fn container_args(declared: &[String], with: &HashMap<String, String>) -> Vec<String> {
    if !declared.is_empty() {
        return declared.to_vec();
    }
    with.get("args")
        .map(|args| actions::external::split_arguments(args))
        .unwrap_or_default()
}

/// A `timeout-minutes` deadline, armed for as long as it is held.
///
/// It cancels through the same token cancellation uses, so the running process
/// is actually killed rather than the future being abandoned. Dropping it
/// disarms the timer, so a step that finishes in time leaves nothing behind.
struct Deadline {
    token: CancellationToken,
    timer: tokio::task::JoinHandle<()>,
    /// The run's own token, to tell "ran out of time" from "the user stopped
    /// the run" — they reach the step the same way but mean different things.
    parent: CancellationToken,
}

impl Deadline {
    /// Arm a deadline `minutes` from now, as a child of `parent` so cancelling
    /// the run still stops the work.
    ///
    /// GitHub types `timeout-minutes` as a number rather than an integer, so a
    /// fractional minute is legal and worth honouring.
    fn arm(parent: &CancellationToken, minutes: f64) -> Self {
        let token = parent.child_token();
        let fire = token.clone();
        let seconds = (minutes * 60.0).max(0.0);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
            fire.cancel();
        });
        Self {
            token,
            timer,
            parent: parent.clone(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Whether the deadline is what stopped the work, rather than the run
    /// being cancelled from outside.
    fn expired(&self) -> bool {
        self.token.is_cancelled() && !self.parent.is_cancelled()
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// Evaluate an expression, saying what it belonged to when it fails.
///
/// A failure used to fall back to the raw text, which meant `${{ }}` reached
/// the shell verbatim and failed there with something unrelated — an
/// unsupported function showed up as a syntax error from bash. Naming the
/// source makes the actual cause the thing the user is told about.
fn evaluate_at(what: &str, source: &str, ctx: &Context) -> Result<String, WorkflowError> {
    expr::evaluate_string(source, ctx).map_err(|e| {
        // Unwrap the inner error's own prefix; one "expression error" per
        // message is enough.
        let detail = match &e {
            WorkflowError::ExpressionError(message) => message.clone(),
            other => other.to_string(),
        };
        WorkflowError::ExpressionError(format!("{}: {}", what, detail))
    })
}

/// Whether a `runs-on:` label plausibly names the machine we are on.
///
/// Only used to decide whether the fallback is worth warning about — running
/// a `macos-latest` job on a Mac needs no explanation.
fn host_matches(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    if label.contains("self-hosted") {
        return true;
    }
    let host = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "ubuntu",
        "windows" => "windows",
        other => other,
    };
    label.contains(host) || (std::env::consts::OS == "linux" && label.contains("linux"))
}

/// The name to show for a step, falling back to its position.
fn display_step_name(step: &Step, step_idx: usize) -> String {
    if !step.name.is_empty() {
        return step.name.clone();
    }
    if let Some(uses) = &step.uses {
        return uses.clone();
    }
    format!("step {}", step_idx + 1)
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
    /// Every job instance that ran, keyed by [`JobResult::instance_id`].
    ///
    /// A job without a matrix is keyed by its plain job id, so
    /// `job_results["build"]` works as expected; a matrix job contributes one
    /// entry per combination, keyed `build (ubuntu-latest, 20)`.
    pub job_results: HashMap<String, JobResult>,
    /// Instance ids in the order they ran.
    pub job_order: Vec<String>,
}

impl EngineResult {
    /// Job results in execution order.
    pub fn ordered(&self) -> Vec<&JobResult> {
        self.job_order
            .iter()
            .filter_map(|id| self.job_results.get(id))
            .collect()
    }

    /// Every instance of one job id, in execution order.
    pub fn instances_of<'a>(&'a self, job_id: &str) -> Vec<&'a JobResult> {
        self.ordered()
            .into_iter()
            .filter(|result| result.job_id == job_id)
            .collect()
    }
}

/// Result of a single job instance.
#[derive(Debug, Clone)]
pub struct JobResult {
    /// The job id as written in the workflow, shared by all matrix instances.
    pub job_id: String,
    /// Unique id for this instance: the job id, plus the matrix values when
    /// the job has a `strategy.matrix`.
    pub instance_id: String,
    /// Human-readable name shown in logs.
    pub job_name: String,
    /// The matrix combination this instance ran with; empty when there is none.
    pub matrix: HashMap<String, Value>,
    pub success: bool,
    pub conclusion: StepConclusion,
    pub outputs: HashMap<String, String>,
    pub step_results: Vec<StepResult>,
}

/// One job instance to run: a job id plus the matrix combination it runs with.
struct JobInstance {
    base_id: String,
    instance_id: String,
    display_name: String,
    matrix: HashMap<String, Value>,
}

impl JobInstance {
    /// Build the instance. `ctx` must already carry this combination's
    /// `matrix` and `strategy`, because the job's `name:` can interpolate them.
    fn new(job_id: &str, job: &Job, combination: &MatrixCombination, ctx: &Context) -> Self {
        let suffix = combination.display_suffix();
        let instance_id = format!("{}{}", job_id, suffix);

        let base_name = if job.name.is_empty() {
            job_id.to_string()
        } else {
            expr::evaluate_string(&job.name, ctx).unwrap_or_else(|_| job.name.clone())
        };

        // A `name:` that interpolates the matrix already tells its instances
        // apart, so only append the values when it does not.
        let display_name = if job.name.contains("matrix.") {
            base_name
        } else {
            format!("{}{}", base_name, suffix)
        };

        Self {
            base_id: job_id.to_string(),
            instance_id,
            display_name,
            matrix: combination.clone().into(),
        }
    }

    /// A result for this instance that ran no steps.
    fn result_with(&self, conclusion: StepConclusion) -> JobResult {
        JobResult {
            job_id: self.base_id.clone(),
            instance_id: self.instance_id.clone(),
            job_name: self.display_name.clone(),
            matrix: self.matrix.clone(),
            success: conclusion != StepConclusion::Failure,
            conclusion,
            outputs: HashMap::new(),
            step_results: Vec::new(),
        }
    }
}

/// Collapse the results of a job's matrix instances into the one conclusion
/// that dependent jobs see through `needs`.
fn aggregate_conclusion(results: &[JobResult]) -> StepConclusion {
    if results
        .iter()
        .any(|r| r.conclusion == StepConclusion::Failure)
    {
        StepConclusion::Failure
    } else if results
        .iter()
        .any(|r| r.conclusion == StepConclusion::Cancelled)
    {
        StepConclusion::Cancelled
    } else if !results.is_empty()
        && results
            .iter()
            .all(|r| r.conclusion == StepConclusion::Skipped)
    {
        StepConclusion::Skipped
    } else {
        StepConclusion::Success
    }
}
