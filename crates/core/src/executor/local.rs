//! Runs steps directly on this machine.
//!
//! The default, and the only backend that needs nothing installed.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::{resolve_shell, Executor, OutputSink, StepOutcome, StepRequest, StepSession};
use crate::logging::{CommandStream, LogLevel};
use crate::types::WorkflowError;

/// Executes steps as child processes of the runner.
#[derive(Debug, Default)]
pub struct LocalExecutor;

impl LocalExecutor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    fn describe(&self) -> String {
        "local".to_string()
    }

    async fn run_step(
        &self,
        request: StepRequest,
        sink: &dyn OutputSink,
        cancel: &CancellationToken,
    ) -> Result<StepOutcome, WorkflowError> {
        let session = StepSession::create(&request.runner_temp, &request.shell, &request.script)?;

        let mut env = request.env.clone();
        env.extend(session.file_env());

        let script = session.script_path().to_string_lossy().to_string();
        let (program, args) = request.resolve_command(&request.shell, &script);

        let spawn = |program: &str, args: &[String]| {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.current_dir(&request.working_directory);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            cmd.env_clear();
            cmd.envs(&env);
            // Its own process group, so cancelling can signal everything the
            // script spawned rather than just the shell. The trade is that the
            // terminal's Ctrl+C no longer reaches the child — the CLI makes up
            // for that by turning Ctrl+C into a cancellation.
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.spawn()
        };

        let child = match spawn(&program, &args) {
            Ok(child) => child,
            // Not every machine has bash (minimal containers, some BSDs).
            // Fall back to sh rather than failing the step outright.
            Err(e)
                if e.kind() == ErrorKind::NotFound
                    && request.shell == "bash"
                    && request.command.is_none() =>
            {
                sink.note(
                    LogLevel::Warn,
                    "bash not found, falling back to sh".to_string(),
                )
                .await;
                let (fallback_program, fallback_args) = resolve_shell("sh", &script);
                spawn(&fallback_program, &fallback_args).map_err(|e| {
                    WorkflowError::StepFailed(
                        request.step_name.clone(),
                        format!("Failed to execute command: {}", e),
                    )
                })?
            }
            Err(e) => {
                return Err(WorkflowError::StepFailed(
                    request.step_name.clone(),
                    format!("Failed to execute command: {}", e),
                ))
            }
        };

        let (success, status, cancelled) =
            supervise(child, &request.step_name, sink, cancel, kill_process_group).await?;

        Ok(StepOutcome {
            success,
            status,
            cancelled,
            files: session.read_back(),
        })
    }
}

/// Stream a child's output, waiting for it to finish or for cancellation.
///
/// `stop` is the backend-specific half of cancelling, given the child's pid:
/// local signals the process group, Docker asks the daemon to kill the
/// container. `supervise` always kills the child itself afterwards, so a
/// backend with nothing extra to do can pass a no-op.
pub(crate) async fn supervise<F, Fut>(
    mut child: Child,
    step_name: &str,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
    stop: F,
) -> Result<(bool, String, bool), WorkflowError>
where
    F: FnOnce(Option<u32>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let stdout = child.stdout.take().ok_or_else(|| {
        WorkflowError::StepFailed(
            step_name.to_string(),
            "Failed to capture stdout".to_string(),
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        WorkflowError::StepFailed(
            step_name.to_string(),
            "Failed to capture stderr".to_string(),
        )
    })?;

    // The sink is borrowed, so the readers run on this task instead of being
    // spawned; joining them keeps both streams flowing as output arrives.
    let mut cancelled = false;
    let status = tokio::select! {
        // Cancelling has to reach the child; waiting it out would make "stop"
        // mean "stop after this build finishes". The readers are simply
        // dropped — a grandchild can hold the pipes open after the shell is
        // gone, and the tail of a cancelled step is not worth blocking on.
        _ = cancel.cancelled() => {
            sink.note(LogLevel::Warn, format!("Cancelling step '{}'", step_name)).await;
            cancelled = true;
            stop(child.id()).await;
            let _ = child.kill().await;
            child.wait().await
        }
        result = async {
            // Draining the pipes before reaping avoids deadlocking on a child
            // that fills them, and gives us the complete output either way.
            tokio::join!(
                pump(BufReader::new(stdout), CommandStream::Stdout, sink),
                pump(BufReader::new(stderr), CommandStream::Stderr, sink),
            );
            child.wait().await
        } => result,
    }
    .map_err(|e| {
        WorkflowError::StepFailed(
            step_name.to_string(),
            format!("Failed to wait for command: {}", e),
        )
    })?;

    Ok((status.success(), status.to_string(), cancelled))
}

async fn pump<R>(reader: R, stream: CommandStream, sink: &dyn OutputSink)
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        sink.line(stream, line).await;
    }
}

/// Stop everything a locally-spawned step started.
#[cfg(unix)]
pub(crate) async fn kill_process_group(pid: Option<u32>) {
    // The shell is a process-group leader (see the spawn), so signalling the
    // negated pid reaches the build, test runner or server it started. Killing
    // only the shell would leave those running after the run reports stopped.
    if let Some(pid) = pid {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub(crate) async fn kill_process_group(_pid: Option<u32>) {
    // No process groups here; grandchildren of a cancelled step survive.
}

/// A sink that discards everything, for callers that only want the exit status.
#[derive(Debug, Default)]
pub struct NullSink;

#[async_trait]
impl OutputSink for NullSink {
    async fn line(&self, _stream: CommandStream, _line: String) {}
    async fn note(&self, _level: LogLevel, _message: String) {}
}

/// Run a command to completion, returning its exit status and output.
///
/// Used by backends to talk to their tooling (`docker`, `ssh`) rather than to
/// run workflow steps.
pub(crate) async fn run_tool(
    program: &str,
    args: &[String],
) -> Result<(bool, String), WorkflowError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| WorkflowError::Other(format!("failed to run `{}`: {}", program, e)))?;

    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    Ok((output.status.success(), text.trim().to_string()))
}

/// Environment as `-e KEY=VALUE` style arguments.
pub(crate) fn env_args(env: &HashMap<String, String>, flag: &str) -> Vec<String> {
    // Sorted so a command line is reproducible and testable.
    let mut keys: Vec<&String> = env.keys().collect();
    keys.sort();
    let mut args = Vec::with_capacity(keys.len() * 2);
    for key in keys {
        args.push(flag.to_string());
        args.push(format!("{}={}", key, env[key]));
    }
    args
}
