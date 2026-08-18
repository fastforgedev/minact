//! Runs steps inside a Linux container.
//!
//! This is what makes `runs-on: ubuntu-latest` mean something on a Mac: the
//! step really does execute on Linux, against the image the workflow asks for.
//!
//! # Why the paths match
//!
//! The workspace and the runner's temp directory are bind-mounted at the
//! *same absolute paths* they have on the host. That is deliberate and it is
//! what keeps the rest of the engine unaware of containers: `GITHUB_WORKSPACE`,
//! `working-directory`, the step's script and the four `$GITHUB_*` files are
//! all valid on both sides, so nothing has to be translated. The cost is a
//! host-shaped path (`/Users/...`) inside a Linux container, which is legal
//! and invisible to workflows that use `$GITHUB_WORKSPACE`.
//!
//! # Lifetime
//!
//! One container per job, kept alive between steps, because a step's
//! `$GITHUB_ENV` exports and the files it writes have to survive into the
//! next step.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::local::{env_args, run_tool, supervise};
use super::{Executor, OutputSink, StepOutcome, StepRequest, StepSession};
use crate::logging::LogLevel;
use crate::types::WorkflowError;

/// How to run a job in a container.
#[derive(Debug, Clone)]
pub struct DockerConfig {
    /// Image reference, e.g. `ubuntu:24.04`.
    pub image: String,
    /// Directories to bind-mount at identical paths on both sides.
    pub mounts: Vec<PathBuf>,
    /// Extra arguments passed to `docker run`, e.g. `--platform linux/amd64`.
    pub run_args: Vec<String>,
    /// User to run steps as, e.g. `root` or `1000:1000`.
    pub user: Option<String>,
    /// Pull the image before starting, rather than relying on a local copy.
    pub pull: bool,
    /// The `docker` binary, so a compatible CLI (`podman`) can be swapped in.
    pub binary: String,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: "ubuntu:24.04".to_string(),
            mounts: Vec::new(),
            run_args: Vec::new(),
            user: None,
            pull: false,
            binary: "docker".to_string(),
        }
    }
}

/// Executes steps with `docker exec` in a container that lives for one job.
pub struct DockerExecutor {
    config: DockerConfig,
    state: Mutex<ContainerState>,
}

#[derive(Default)]
struct ContainerState {
    container_id: Option<String>,
    /// Whether the image has bash; `sh` is the fallback for minimal images.
    has_bash: bool,
}

impl DockerExecutor {
    pub fn new(config: DockerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(ContainerState::default()),
        }
    }

    async fn container_id(&self) -> Result<String, WorkflowError> {
        self.state.lock().await.container_id.clone().ok_or_else(|| {
            WorkflowError::Other("docker container was not started for this job".to_string())
        })
    }
}

#[async_trait]
impl Executor for DockerExecutor {
    fn describe(&self) -> String {
        format!("docker ({})", self.config.image)
    }

    async fn prepare(&self, sink: &dyn OutputSink) -> Result<(), WorkflowError> {
        if self.config.pull {
            sink.note(LogLevel::Info, format!("pulling {}", self.config.image))
                .await;
            let (ok, output) = run_tool(
                &self.config.binary,
                &["pull".to_string(), self.config.image.clone()],
            )
            .await?;
            if !ok {
                return Err(WorkflowError::Other(format!(
                    "failed to pull {}: {}",
                    self.config.image, output
                )));
            }
        }

        let mut args = vec!["run".to_string(), "--detach".to_string()];
        for mount in &self.config.mounts {
            let path = mount.to_string_lossy();
            args.push("--volume".to_string());
            args.push(format!("{}:{}", path, path));
        }
        if let Some(user) = &self.config.user {
            args.push("--user".to_string());
            args.push(user.clone());
        }
        args.extend(self.config.run_args.iter().cloned());
        args.push("--entrypoint".to_string());
        args.push("sh".to_string());
        args.push(self.config.image.clone());
        // Keep the container alive for the whole job; steps arrive via exec.
        args.extend(["-c".to_string(), "while :; do sleep 3600; done".to_string()]);

        let (ok, output) = run_tool(&self.config.binary, &args).await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to start a container from {}: {}",
                self.config.image, output
            )));
        }

        let container_id = output.lines().last().unwrap_or_default().trim().to_string();
        if container_id.is_empty() {
            return Err(WorkflowError::Other(
                "docker run returned no container id".to_string(),
            ));
        }

        // Probe once rather than guessing per step: a spawn inside a container
        // fails with an exit code, not an error we can match on.
        let (has_bash, _) = run_tool(
            &self.config.binary,
            &[
                "exec".to_string(),
                container_id.clone(),
                "sh".to_string(),
                "-c".to_string(),
                "command -v bash".to_string(),
            ],
        )
        .await?;

        if !has_bash {
            sink.note(
                LogLevel::Warn,
                format!("{} has no bash; steps will run under sh", self.config.image),
            )
            .await;
        }

        let mut state = self.state.lock().await;
        state.container_id = Some(container_id);
        state.has_bash = has_bash;

        Ok(())
    }

    async fn run_step(
        &self,
        request: StepRequest,
        sink: &dyn OutputSink,
        cancel: &CancellationToken,
    ) -> Result<StepOutcome, WorkflowError> {
        let container_id = self.container_id().await?;
        let has_bash = self.state.lock().await.has_bash;

        // The session lives on the host, inside a mounted directory, so the
        // container writes to the very same files.
        let session = StepSession::create(&request.runner_temp, &request.shell, &request.script)?;

        let mut env = request.env.clone();
        env.extend(session.file_env());

        let script = session.script_path().to_string_lossy().to_string();
        let shell = if request.shell == "bash" && !has_bash {
            "sh"
        } else {
            &request.shell
        };
        let (program, program_args) = request.resolve_command(shell, &script);

        let mut args = vec!["exec".to_string()];
        args.push("--workdir".to_string());
        args.push(request.working_directory.to_string_lossy().to_string());
        args.extend(env_args(&env, "--env"));
        args.push(container_id.clone());
        args.push(program);
        args.extend(program_args);

        let child = Command::new(&self.config.binary)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                WorkflowError::StepFailed(
                    request.step_name.clone(),
                    format!("failed to run `{} exec`: {}", self.config.binary, e),
                )
            })?;

        // Killing the local `docker exec` leaves the process running inside
        // the container, so cancellation has to reach the daemon.
        let binary = self.config.binary.clone();
        let stop_id = container_id.clone();
        let (success, status, cancelled) = supervise(
            child,
            &request.step_name,
            sink,
            cancel,
            move |_pid| async move {
                let _ = run_tool(&binary, &["kill".to_string(), stop_id]).await;
            },
        )
        .await?;

        Ok(StepOutcome {
            success,
            status,
            cancelled,
            files: session.read_back(),
        })
    }

    async fn cleanup(&self, sink: &dyn OutputSink) {
        let container_id = { self.state.lock().await.container_id.take() };
        let Some(container_id) = container_id else {
            return;
        };

        // Best effort: a leaked container is worth a warning, not a failed run.
        match run_tool(
            &self.config.binary,
            &[
                "rm".to_string(),
                "--force".to_string(),
                container_id.clone(),
            ],
        )
        .await
        {
            Ok((true, _)) => {}
            Ok((false, output)) => {
                sink.note(
                    LogLevel::Warn,
                    format!("could not remove container {}: {}", container_id, output),
                )
                .await;
            }
            Err(e) => {
                sink.note(
                    LogLevel::Warn,
                    format!("could not remove container {}: {}", container_id, e),
                )
                .await;
            }
        }
    }
}

/// Whether a usable container runtime is present.
///
/// Used to give a clear message up front instead of a failure per job.
pub async fn is_available(binary: &str) -> bool {
    matches!(
        run_tool(
            binary,
            &[
                "version".to_string(),
                "--format".to_string(),
                "{{.Server.Version}}".to_string()
            ]
        )
        .await,
        Ok((true, _))
    )
}

/// The mounts a job needs: its workspace, the runner temp directory, and
/// wherever fetched actions live.
///
/// The action cache is mounted whether or not the job uses an action from it.
/// Mounts are fixed when the container starts, and a step that reaches an
/// action later cannot ask for one then.
pub fn default_mounts(
    workspace: &std::path::Path,
    runner_temp: &std::path::Path,
    extra: &[PathBuf],
) -> Vec<PathBuf> {
    let mut mounts = vec![workspace.to_path_buf()];
    // Skip a temp dir nested inside the workspace; one mount already covers it.
    if !runner_temp.starts_with(workspace) {
        mounts.push(runner_temp.to_path_buf());
    }
    for path in extra {
        if !mounts.iter().any(|mount| path.starts_with(mount)) {
            mounts.push(path.clone());
        }
    }
    mounts
}

/// Environment entries as `--env KEY=VALUE`, exposed for testing.
#[allow(dead_code)]
pub(crate) fn docker_env_args(env: &HashMap<String, String>) -> Vec<String> {
    env_args(env, "--env")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn mounts_cover_workspace_and_temp() {
        let mounts = default_mounts(Path::new("/work"), Path::new("/tmp/minact"), &[]);
        assert_eq!(
            mounts,
            vec![PathBuf::from("/work"), PathBuf::from("/tmp/minact")]
        );
    }

    #[test]
    fn a_temp_dir_inside_the_workspace_is_not_mounted_twice() {
        let mounts = default_mounts(Path::new("/work"), Path::new("/work/.tmp"), &[]);
        assert_eq!(mounts, vec![PathBuf::from("/work")]);
    }

    #[test]
    fn the_action_cache_is_mounted_unless_it_is_already_covered() {
        let cache = PathBuf::from("/home/me/.minact/actions");
        let mounts = default_mounts(
            Path::new("/work"),
            Path::new("/tmp/minact"),
            std::slice::from_ref(&cache),
        );
        assert_eq!(
            mounts,
            vec![
                PathBuf::from("/work"),
                PathBuf::from("/tmp/minact"),
                cache.clone()
            ]
        );

        let nested = default_mounts(
            Path::new("/work"),
            Path::new("/tmp/minact"),
            &[PathBuf::from("/work/.cache/actions")],
        );
        assert_eq!(
            nested,
            vec![PathBuf::from("/work"), PathBuf::from("/tmp/minact")]
        );
    }

    #[test]
    fn env_args_are_sorted_and_paired() {
        let env = HashMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        assert_eq!(docker_env_args(&env), vec!["--env", "A=1", "--env", "B=2"]);
    }

    #[test]
    fn describes_the_image() {
        let executor = DockerExecutor::new(DockerConfig {
            image: "alpine:3".to_string(),
            ..Default::default()
        });
        assert_eq!(executor.describe(), "docker (alpine:3)");
    }
}
