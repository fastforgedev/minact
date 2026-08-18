//! Where a step's commands actually run.
//!
//! The engine decides *what* to run — expressions resolved, environment
//! layered, conditions evaluated. An [`Executor`] decides *where*: this
//! machine, a Linux container, or a remote host. That split is what lets a
//! workflow written for `runs-on: ubuntu-latest` run on a Mac.
//!
//! An executor owns everything that is location-specific:
//!
//! * the temp directory holding the step's script,
//! * the four `$GITHUB_*` files a step writes back through, and where they
//!   are visible from,
//! * spawning, streaming, cancelling and reaping the process.
//!
//! Interpreting the output — `::` workflow commands, secret masking — stays
//! in the engine, which is why the executor only reports lines through an
//! [`OutputSink`].

pub mod docker;
pub mod local;
pub mod ssh;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::commands;
use crate::logging::{CommandStream, LogLevel};
use crate::types::WorkflowError;

/// One step's work, described independently of where it runs.
#[derive(Debug, Clone)]
pub struct StepRequest {
    /// Step name, used only in error messages.
    pub step_name: String,
    /// The script to run, with expressions already resolved.
    pub script: String,
    /// The `shell:` value as written in the workflow.
    pub shell: String,
    /// Absolute working directory, as it exists on the host.
    pub working_directory: PathBuf,
    /// The step's environment, *without* the `GITHUB_OUTPUT` / `GITHUB_ENV` /
    /// `GITHUB_PATH` / `GITHUB_STEP_SUMMARY` variables — only the executor
    /// knows where those files live from the step's point of view.
    pub env: HashMap<String, String>,
    /// Directory the executor may create per-step scratch space in.
    pub runner_temp: PathBuf,
    /// When set, the executor spawns this argv instead of writing `script` to
    /// a file and interpreting it with `shell`.
    ///
    /// Actions need it: a JavaScript action's entry point is a file that
    /// already exists, and running it as `node <path>` rather than through a
    /// generated wrapper is what gives it the `__dirname` and `process.argv`
    /// it would see on a real runner. The step's scratch files are still
    /// created, so `$GITHUB_OUTPUT` works the same way.
    pub command: Option<Vec<String>>,
}

impl StepRequest {
    /// The program and arguments to spawn.
    ///
    /// `shell` is passed separately because a backend may have substituted it
    /// — Docker falls back to `sh` in an image without bash.
    pub(crate) fn resolve_command(&self, shell: &str, script: &str) -> (String, Vec<String>) {
        match self.command.as_deref() {
            Some([program, args @ ..]) => (program.clone(), args.to_vec()),
            _ => resolve_shell(shell, script),
        }
    }
}

/// Raw contents of the four files a step can write back through.
#[derive(Debug, Default, Clone)]
pub struct StepFileContents {
    pub output: String,
    pub env: String,
    pub path: String,
    pub summary: String,
}

impl StepFileContents {
    /// Parse the files into the values the engine applies.
    pub fn parse(&self) -> Result<StepFileValues, String> {
        Ok(StepFileValues {
            outputs: commands::parse_key_value_file(&self.output)
                .map_err(|e| format!("$GITHUB_OUTPUT: {}", e))?,
            env: commands::parse_key_value_file(&self.env)
                .map_err(|e| format!("$GITHUB_ENV: {}", e))?,
            paths: commands::parse_path_file(&self.path),
            summary: self.summary.clone(),
        })
    }
}

/// Values recovered from a step's environment files.
#[derive(Debug, Default, Clone)]
pub struct StepFileValues {
    pub outputs: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
    pub paths: Vec<String>,
    pub summary: String,
}

/// How a step finished.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub success: bool,
    /// Human-readable exit status, e.g. `exit status: 1`.
    pub status: String,
    /// True when the step was stopped rather than finishing on its own.
    pub cancelled: bool,
    pub files: StepFileContents,
}

/// Receives a step's output as it is produced.
#[async_trait]
pub trait OutputSink: Send + Sync {
    /// One line from the step's stdout or stderr.
    async fn line(&self, stream: CommandStream, line: String);

    /// A message from the executor itself, not from the step.
    async fn note(&self, level: LogLevel, message: String);
}

/// A place where steps run.
///
/// One executor serves one job: [`prepare`](Executor::prepare) runs before the
/// first step and [`cleanup`](Executor::cleanup) after the last, so a backend
/// can hold something expensive — a container, an SSH connection — across the
/// job's steps. State that a step leaves behind, such as `$GITHUB_ENV` exports
/// or files it wrote, has to survive to the next step of the same job.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Short description for logs, e.g. `local` or `docker (ubuntu:24.04)`.
    fn describe(&self) -> String;

    /// Called once before a job's first step.
    async fn prepare(&self, _sink: &dyn OutputSink) -> Result<(), WorkflowError> {
        Ok(())
    }

    /// Make a directory on the host reachable from steps, returning the path
    /// they should use to reach it.
    ///
    /// External actions live outside the workspace — a fetched one sits in the
    /// action cache — so running one means answering "where is this directory
    /// from the step's point of view?". Local and Docker answer "the same
    /// place": Docker bind-mounts at identical paths. SSH has to copy it over
    /// and hand back a remote path.
    async fn provision_dir(
        &self,
        path: &Path,
        _sink: &dyn OutputSink,
    ) -> Result<PathBuf, WorkflowError> {
        Ok(path.to_path_buf())
    }

    /// Run one step to completion.
    async fn run_step(
        &self,
        request: StepRequest,
        sink: &dyn OutputSink,
        cancel: &CancellationToken,
    ) -> Result<StepOutcome, WorkflowError>;

    /// Called once after a job's last step, including when a step failed.
    async fn cleanup(&self, _sink: &dyn OutputSink) {}
}

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

/// The per-step scratch directory: the script, plus the four files the step
/// writes back through.
///
/// Local and Docker both use this directly — the container mounts the same
/// paths, so a file written inside it is the same file on the host.
pub(crate) struct StepSession {
    /// Removed when the session is dropped.
    _dir: tempfile::TempDir,
    script: PathBuf,
    output: PathBuf,
    env: PathBuf,
    path: PathBuf,
    summary: PathBuf,
}

impl StepSession {
    pub(crate) fn create(
        runner_temp: &Path,
        shell: &str,
        script: &str,
    ) -> Result<Self, WorkflowError> {
        std::fs::create_dir_all(runner_temp)?;
        let dir = tempfile::Builder::new()
            .prefix("minact-step-")
            .tempdir_in(runner_temp)?;

        let script_path = dir
            .path()
            .join(format!("script.{}", script_extension(shell)));
        std::fs::write(&script_path, script)?;

        let session = Self {
            script: script_path,
            output: dir.path().join("github_output"),
            env: dir.path().join("github_env"),
            path: dir.path().join("github_path"),
            summary: dir.path().join("github_step_summary"),
            _dir: dir,
        };

        // The files must exist up front: steps append to them with `>>`.
        for path in [
            &session.output,
            &session.env,
            &session.path,
            &session.summary,
        ] {
            std::fs::write(path, "")?;
        }

        Ok(session)
    }

    pub(crate) fn script_path(&self) -> &Path {
        &self.script
    }

    /// The `GITHUB_*` variables pointing at this session's files.
    pub(crate) fn file_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "GITHUB_OUTPUT".to_string(),
                self.output.to_string_lossy().to_string(),
            ),
            (
                "GITHUB_ENV".to_string(),
                self.env.to_string_lossy().to_string(),
            ),
            (
                "GITHUB_PATH".to_string(),
                self.path.to_string_lossy().to_string(),
            ),
            (
                "GITHUB_STEP_SUMMARY".to_string(),
                self.summary.to_string_lossy().to_string(),
            ),
        ]
    }

    pub(crate) fn read_back(&self) -> StepFileContents {
        StepFileContents {
            output: read_or_empty(&self.output),
            env: read_or_empty(&self.env),
            path: read_or_empty(&self.path),
            summary: read_or_empty(&self.summary),
        }
    }
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The file extension to give a step's script, so interpreters that care
/// (PowerShell) get the right one.
pub(crate) fn script_extension(shell: &str) -> &'static str {
    match shell {
        "python" | "python3" => "py",
        "node" => "js",
        "pwsh" | "powershell" => "ps1",
        _ => "sh",
    }
}

/// Resolve a `shell:` value to the program and arguments that run a script.
///
/// A shell containing `{0}` is treated as a command template, the same as
/// GitHub's `shell: python -u {0}` form.
pub(crate) fn resolve_shell(shell: &str, script: &str) -> (String, Vec<String>) {
    if shell.contains("{0}") {
        let mut parts = shell
            .split_whitespace()
            .map(|part| part.replace("{0}", script))
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return ("sh".to_string(), vec![script.to_string()]);
        }
        let program = parts.remove(0);
        return (program, parts);
    }

    let args = |args: &[&str]| {
        let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        all.push(script.to_string());
        all
    };

    match shell {
        // GitHub's default: fail on the first error and on a failing pipe stage.
        "bash" => (
            "bash".to_string(),
            args(&["--noprofile", "--norc", "-eo", "pipefail"]),
        ),
        "sh" => ("sh".to_string(), args(&["-e"])),
        "python" | "python3" => ("python3".to_string(), args(&[])),
        "node" => ("node".to_string(), args(&[])),
        "pwsh" | "powershell" => (
            "pwsh".to_string(),
            args(&["-NoLogo", "-NoProfile", "-File"]),
        ),
        other => (other.to_string(), args(&[])),
    }
}

/// Quote a string for safe inclusion in a POSIX shell command line.
///
/// Used by backends that can only reach the remote side through a shell, where
/// an unquoted value would let workflow data change the command.
pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+".contains(c))
    {
        return value.to_string();
    }
    // Single quotes protect everything except a single quote itself, which has
    // to be closed, escaped and reopened.
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_default_shells() {
        let (program, args) = resolve_shell("bash", "/tmp/s.sh");
        assert_eq!(program, "bash");
        assert_eq!(
            args,
            ["--noprofile", "--norc", "-eo", "pipefail", "/tmp/s.sh"]
        );

        let (program, args) = resolve_shell("sh", "/tmp/s.sh");
        assert_eq!(program, "sh");
        assert_eq!(args, ["-e", "/tmp/s.sh"]);
    }

    #[test]
    fn resolves_a_custom_template() {
        let (program, args) = resolve_shell("python -u {0}", "/tmp/s.py");
        assert_eq!(program, "python");
        assert_eq!(args, ["-u", "/tmp/s.py"]);
    }

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(shell_quote("simple-value_1.2/x"), "simple-value_1.2/x");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("rm -rf /; echo"), "'rm -rf /; echo'");
    }

    #[test]
    fn quoting_survives_embedded_single_quotes() {
        // The classic injection shape: a value that tries to close the quote
        // and append its own command.
        for hostile in [
            "'; rm -rf /tmp/x; echo '",
            "$(touch /tmp/pwned)",
            "`id`",
            "a\nb",
            "x\"y",
            "$HOME",
        ] {
            let quoted = shell_quote(hostile);

            // The only assertion that matters: a real shell must hand the
            // value back unchanged, as exactly one argument.
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", quoted))
                .output()
                .expect("sh should run");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                hostile,
                "{:?} did not survive quoting as {}",
                hostile,
                quoted
            );
        }

        // And nothing was executed along the way.
        assert!(!std::path::Path::new("/tmp/pwned").exists());
    }

    #[test]
    fn session_exposes_and_reads_back_its_files() {
        let temp = tempfile::tempdir().unwrap();
        let session = StepSession::create(temp.path(), "bash", "echo hi").unwrap();

        let env = session.file_env();
        assert_eq!(env.len(), 4);
        let output_path = env
            .iter()
            .find(|(k, _)| k == "GITHUB_OUTPUT")
            .map(|(_, v)| v.clone())
            .unwrap();

        std::fs::write(&output_path, "ver=1\n").unwrap();
        let files = session.read_back();
        assert_eq!(files.output, "ver=1\n");

        let values = files.parse().unwrap();
        assert_eq!(values.outputs, vec![("ver".to_string(), "1".to_string())]);
    }
}
