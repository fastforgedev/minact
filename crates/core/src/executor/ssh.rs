//! Runs steps on another machine over SSH.
//!
//! This is the backend for targets a container cannot provide: Windows, or
//! real macOS hardware for signing and notarisation.
//!
//! # How the workspace gets there
//!
//! Unlike [`docker`](super::docker), the remote filesystem is a different
//! filesystem, so paths cannot simply match. The workspace is pushed with
//! `rsync` before the first step and pulled back after the last, and every
//! host path under the workspace is rewritten to its remote equivalent.
//!
//! # What does not work
//!
//! Built-in actions (`actions/checkout`, `actions/upload-artifact`) run
//! in-process on the *host* and touch the host workspace. On a remote runner
//! they act on the local copy, which is only correct because the workspace is
//! synced back afterwards — a step that reads an artifact mid-job sees the
//! remote copy, not the host one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::local::{run_tool, supervise};
use super::{shell_quote, Executor, OutputSink, StepOutcome, StepRequest};
use crate::logging::LogLevel;
use crate::types::WorkflowError;

/// How to reach a remote runner.
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// Private key to authenticate with; otherwise the agent/default keys.
    pub identity_file: Option<PathBuf>,
    /// Directory on the remote machine that mirrors the local workspace.
    pub remote_workspace: String,
    /// Push the workspace before the job and pull it back afterwards.
    /// Turn off when the remote already has the tree (a shared checkout).
    pub sync: bool,
    /// Extra `ssh` arguments.
    pub ssh_args: Vec<String>,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            user: None,
            port: None,
            identity_file: None,
            remote_workspace: "~/minact-workspace".to_string(),
            sync: true,
            ssh_args: Vec::new(),
        }
    }
}

impl SshConfig {
    /// The `user@host` form rsync and ssh both take.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{}@{}", user, self.host),
            None => self.host.clone(),
        }
    }

    /// Arguments common to every `ssh` invocation.
    pub fn ssh_base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }
        if let Some(identity) = &self.identity_file {
            args.push("-i".to_string());
            args.push(identity.to_string_lossy().to_string());
        }
        // Never prompt: a runner that blocks on a password looks like a hang.
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());
        args.extend(self.ssh_args.iter().cloned());
        args
    }

    /// `-e ssh ...` so rsync uses the same port and key.
    fn rsync_shell_arg(&self) -> String {
        let mut parts = vec!["ssh".to_string()];
        parts.extend(self.ssh_base_args().iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

/// Executes steps on a remote host.
pub struct SshExecutor {
    config: SshConfig,
    /// Local workspace root, used to rewrite paths into remote ones.
    workspace: PathBuf,
    /// Host directories already copied over, and where they landed. A job that
    /// uses the same action in five steps copies it once.
    provisioned: Mutex<HashMap<PathBuf, String>>,
}

impl SshExecutor {
    pub fn new(config: SshConfig, workspace: PathBuf) -> Self {
        Self {
            config,
            workspace,
            provisioned: Mutex::new(HashMap::new()),
        }
    }

    /// Where a provisioned host directory lands on the remote.
    ///
    /// Named after the host path so that the same action is the same remote
    /// directory across steps, and hashed so that two actions with the same
    /// basename cannot land on top of each other.
    fn remote_support_dir(&self, path: &Path) -> String {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "dir".to_string());
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();

        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(path.to_string_lossy().as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        format!(
            "{}/.minact-support/{}-{}",
            self.config.remote_workspace,
            name,
            &hash[..8]
        )
    }

    /// Rewrite a host path under the workspace to its remote equivalent.
    ///
    /// Paths outside the workspace cannot be mapped and are left alone; the
    /// caller is responsible for not depending on them remotely.
    pub fn remote_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workspace) {
            Ok(relative) if relative.as_os_str().is_empty() => self.config.remote_workspace.clone(),
            Ok(relative) => format!(
                "{}/{}",
                self.config.remote_workspace,
                relative.to_string_lossy().replace('\\', "/")
            ),
            Err(_) => path.to_string_lossy().to_string(),
        }
    }

    /// The remote directory holding this job's scripts and env files.
    fn remote_temp(&self) -> String {
        format!("{}/.minact-temp", self.config.remote_workspace)
    }

    /// Build the remote command for one step.
    ///
    /// The environment is exported inside the script rather than passed on the
    /// command line: `ssh` concatenates its arguments into one string for the
    /// remote shell, so anything unquoted there would be re-interpreted.
    pub fn build_step_script(
        &self,
        request: &StepRequest,
        env: &HashMap<String, String>,
    ) -> String {
        let mut script = String::from("#!/bin/sh\n");
        let mut keys: Vec<&String> = env.keys().collect();
        keys.sort();
        for key in keys {
            script.push_str(&format!("export {}={}\n", key, shell_quote(&env[key])));
        }
        script.push_str(&format!(
            "cd {} || exit 1\n",
            shell_quote(&self.remote_path(&request.working_directory))
        ));
        script
    }

    async fn ssh_exec(&self, command: &str) -> Result<(bool, String), WorkflowError> {
        let mut args = self.config.ssh_base_args();
        args.push(self.config.destination());
        args.push(command.to_string());
        run_tool("ssh", &args).await
    }

    /// Copy a local directory to the remote, mirroring deletions.
    async fn push(&self, from: &Path, to: &str) -> Result<(), WorkflowError> {
        let args = vec![
            "--archive".to_string(),
            "--compress".to_string(),
            "--delete".to_string(),
            "-e".to_string(),
            self.config.rsync_shell_arg(),
            format!("{}/", from.to_string_lossy()),
            format!("{}:{}/", self.config.destination(), to),
        ];
        let (ok, output) = run_tool("rsync", &args).await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to sync the workspace to {}: {}",
                self.config.host, output
            )));
        }
        Ok(())
    }

    /// Copy the remote directory back, without deleting local-only files.
    async fn pull(&self, from: &str, to: &Path) -> Result<(), WorkflowError> {
        let args = vec![
            "--archive".to_string(),
            "--compress".to_string(),
            "-e".to_string(),
            self.config.rsync_shell_arg(),
            format!("{}:{}/", self.config.destination(), from),
            format!("{}/", to.to_string_lossy()),
        ];
        let (ok, output) = run_tool("rsync", &args).await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to sync the workspace back from {}: {}",
                self.config.host, output
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl Executor for SshExecutor {
    fn describe(&self) -> String {
        format!("ssh ({})", self.config.destination())
    }

    async fn prepare(&self, sink: &dyn OutputSink) -> Result<(), WorkflowError> {
        let (reachable, output) = self.ssh_exec("echo minact-ok").await?;
        if !reachable {
            return Err(WorkflowError::Other(format!(
                "cannot reach {}: {}",
                self.config.destination(),
                output
            )));
        }

        self.ssh_exec(&format!(
            "mkdir -p {} {}",
            shell_quote(&self.config.remote_workspace),
            shell_quote(&self.remote_temp())
        ))
        .await?;

        if self.config.sync {
            sink.note(
                LogLevel::Info,
                format!("syncing workspace to {}", self.config.destination()),
            )
            .await;
            self.push(&self.workspace, &self.config.remote_workspace)
                .await?;
        }

        Ok(())
    }

    /// Copy a host directory to the remote and report where it landed.
    ///
    /// Anything already inside the workspace is there by the time a step runs,
    /// so it only needs its path rewritten. Everything else — an action out of
    /// the cache — has to travel.
    async fn provision_dir(
        &self,
        path: &Path,
        sink: &dyn OutputSink,
    ) -> Result<PathBuf, WorkflowError> {
        if path.starts_with(&self.workspace) {
            return Ok(PathBuf::from(self.remote_path(path)));
        }

        if let Some(remote) = self.provisioned.lock().await.get(path) {
            return Ok(PathBuf::from(remote));
        }

        let remote = self.remote_support_dir(path);
        sink.note(
            LogLevel::Info,
            format!(
                "copying {} to {}",
                path.display(),
                self.config.destination()
            ),
        )
        .await;
        self.ssh_exec(&format!("mkdir -p {}", shell_quote(&remote)))
            .await?;
        self.push(path, &remote).await.map_err(|_| {
            WorkflowError::Other(format!(
                "failed to copy {} to {}",
                path.display(),
                self.config.host
            ))
        })?;

        self.provisioned
            .lock()
            .await
            .insert(path.to_path_buf(), remote.clone());
        Ok(PathBuf::from(remote))
    }

    async fn run_step(
        &self,
        request: StepRequest,
        sink: &dyn OutputSink,
        cancel: &CancellationToken,
    ) -> Result<StepOutcome, WorkflowError> {
        let remote_temp = self.remote_temp();
        // A per-step directory so concurrent jobs on one host cannot collide.
        let step_dir = format!("{}/step-{}", remote_temp, uuid::Uuid::new_v4());
        let script_path = format!(
            "{}/script.{}",
            step_dir,
            super::script_extension(&request.shell)
        );
        let files = RemoteStepFiles::new(&step_dir);

        let mut env = request.env.clone();
        env.extend(files.file_env());

        // Everything the step needs is written in one round trip.
        let (program, program_args) = request.resolve_command(&request.shell, &script_path);
        let prelude = self.build_step_script(&request, &env);
        let setup = format!(
            "mkdir -p {dir} && : > {out} && : > {envf} && : > {path} && : > {summary} && cat > {script}",
            dir = shell_quote(&step_dir),
            out = shell_quote(&files.output),
            envf = shell_quote(&files.env),
            path = shell_quote(&files.path),
            summary = shell_quote(&files.summary),
            script = shell_quote(&script_path),
        );

        let (ok, output) = self.ssh_write(&setup, &request.script).await?;
        if !ok {
            return Err(WorkflowError::StepFailed(
                request.step_name.clone(),
                format!("could not stage the step on the remote host: {}", output),
            ));
        }

        // The prelude carries the environment and the working directory; the
        // shell then runs the uploaded script.
        let command = format!(
            "{}{} {}",
            prelude,
            shell_quote(&program),
            program_args
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );

        let mut args = self.config.ssh_base_args();
        args.push(self.config.destination());
        args.push(command);

        let child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                WorkflowError::StepFailed(
                    request.step_name.clone(),
                    format!("failed to run ssh: {}", e),
                )
            })?;

        let (success, status, cancelled) = supervise(
            child,
            &request.step_name,
            sink,
            cancel,
            // Killing the local ssh closes the channel and the remote shell
            // gets a hangup; `supervise` does that kill, so there is nothing
            // extra to do without a control socket and a recorded remote pid.
            |_pid| async move {},
        )
        .await?;

        let contents = self.read_remote_files(&files).await;

        // Best effort cleanup; a leftover temp dir must not fail the step.
        let _ = self
            .ssh_exec(&format!("rm -rf {}", shell_quote(&step_dir)))
            .await;

        Ok(StepOutcome {
            success,
            status,
            cancelled,
            files: contents,
        })
    }

    async fn cleanup(&self, sink: &dyn OutputSink) {
        if !self.config.sync {
            return;
        }
        sink.note(
            LogLevel::Info,
            format!("syncing workspace back from {}", self.config.destination()),
        )
        .await;
        if let Err(e) = self
            .pull(&self.config.remote_workspace, &self.workspace.clone())
            .await
        {
            sink.note(LogLevel::Warn, format!("{}", e)).await;
        }
    }
}

impl SshExecutor {
    /// Run a remote command with `stdin` supplied from a string.
    async fn ssh_write(&self, command: &str, stdin: &str) -> Result<(bool, String), WorkflowError> {
        use tokio::io::AsyncWriteExt;

        let mut args = self.config.ssh_base_args();
        args.push(self.config.destination());
        args.push(command.to_string());

        let mut child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| WorkflowError::Other(format!("failed to run ssh: {}", e)))?;

        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(stdin.as_bytes())
                .await
                .map_err(|e| WorkflowError::Other(format!("failed to send the script: {}", e)))?;
            pipe.shutdown()
                .await
                .map_err(|e| WorkflowError::Other(format!("failed to send the script: {}", e)))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;

        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.success(), text.trim().to_string()))
    }

    async fn read_remote_files(&self, files: &RemoteStepFiles) -> super::StepFileContents {
        super::StepFileContents {
            output: self.read_remote(&files.output).await,
            env: self.read_remote(&files.env).await,
            path: self.read_remote(&files.path).await,
            summary: self.read_remote(&files.summary).await,
        }
    }

    async fn read_remote(&self, path: &str) -> String {
        match self
            .ssh_exec(&format!("cat {} 2>/dev/null || true", shell_quote(path)))
            .await
        {
            Ok((_, text)) => text,
            Err(_) => String::new(),
        }
    }
}

/// Paths of the four environment files on the remote side.
struct RemoteStepFiles {
    output: String,
    env: String,
    path: String,
    summary: String,
}

impl RemoteStepFiles {
    fn new(step_dir: &str) -> Self {
        Self {
            output: format!("{}/github_output", step_dir),
            env: format!("{}/github_env", step_dir),
            path: format!("{}/github_path", step_dir),
            summary: format!("{}/github_step_summary", step_dir),
        }
    }

    fn file_env(&self) -> Vec<(String, String)> {
        vec![
            ("GITHUB_OUTPUT".to_string(), self.output.clone()),
            ("GITHUB_ENV".to_string(), self.env.clone()),
            ("GITHUB_PATH".to_string(), self.path.clone()),
            ("GITHUB_STEP_SUMMARY".to_string(), self.summary.clone()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executor() -> SshExecutor {
        SshExecutor::new(
            SshConfig {
                host: "build-box".to_string(),
                user: Some("builder".to_string()),
                port: Some(2222),
                remote_workspace: "/srv/work".to_string(),
                ..Default::default()
            },
            PathBuf::from("/home/me/project"),
        )
    }

    #[test]
    fn builds_the_destination() {
        assert_eq!(executor().config.destination(), "builder@build-box");

        let no_user = SshConfig {
            host: "box".to_string(),
            ..Default::default()
        };
        assert_eq!(no_user.destination(), "box");
    }

    #[test]
    fn never_prompts_for_a_password() {
        let args = executor().config.ssh_base_args();
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]));
    }

    #[test]
    fn maps_workspace_paths_to_the_remote() {
        let executor = executor();
        assert_eq!(
            executor.remote_path(Path::new("/home/me/project")),
            "/srv/work"
        );
        assert_eq!(
            executor.remote_path(Path::new("/home/me/project/src/app")),
            "/srv/work/src/app"
        );
        // Outside the workspace there is nothing sensible to map to.
        assert_eq!(executor.remote_path(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn exports_the_environment_safely() {
        let request = StepRequest {
            step_name: "test".to_string(),
            script: "echo hi".to_string(),
            shell: "bash".to_string(),
            working_directory: PathBuf::from("/home/me/project/sub"),
            env: HashMap::new(),
            runner_temp: PathBuf::from("/tmp"),
            command: None,
        };
        let env = HashMap::from([
            ("SAFE".to_string(), "value".to_string()),
            ("HOSTILE".to_string(), "'; rm -rf /; echo '".to_string()),
        ]);

        let script = executor().build_step_script(&request, &env);

        assert!(script.contains("export SAFE=value"));
        assert!(script.contains("cd /srv/work/sub"));
        // The injected command must be inside quotes, not executable.
        assert!(!script.contains("export HOSTILE='; rm -rf /; echo '\n"));
        assert!(script.contains(r"'\''"));
    }

    #[test]
    fn a_provisioned_directory_lands_somewhere_stable_and_unique() {
        let executor = executor();
        let one = executor.remote_support_dir(Path::new("/home/me/.minact/actions/o/r/v1"));
        let two = executor.remote_support_dir(Path::new("/home/me/.minact/actions/o/r/v2"));

        // Same input, same place: a job using an action five times copies once.
        assert_eq!(
            one,
            executor.remote_support_dir(Path::new("/home/me/.minact/actions/o/r/v1"))
        );
        // Two actions with the same basename cannot land on top of each other.
        assert_ne!(one, two);
        assert!(one.starts_with(&executor.config.remote_workspace));
        assert!(!one.contains(".."));
    }

    #[test]
    fn rsync_reuses_the_ssh_options() {
        let shell = executor().config.rsync_shell_arg();
        assert!(shell.starts_with("ssh "));
        assert!(shell.contains("-p 2222"));
        assert!(shell.contains("BatchMode=yes"));
    }

    #[test]
    fn remote_files_live_under_the_step_directory() {
        let files = RemoteStepFiles::new("/srv/work/.minact-temp/step-1");
        assert_eq!(files.output, "/srv/work/.minact-temp/step-1/github_output");
        assert_eq!(files.file_env().len(), 4);
    }
}
