//! Running a container action.
//!
//! `runs.using: docker` is the one action kind that does not go through the
//! job's executor: the action brings its own image, so it gets its own
//! container even when the job itself is running locally. That is also what
//! GitHub does, and it is why a container action works on a Mac without the
//! workflow saying anything about Docker.
//!
//! The workspace and the runner temp directory are bind-mounted at *identical*
//! paths, the same bargain [`crate::executor::docker`] strikes: `$GITHUB_ENV`,
//! `$GITHUB_OUTPUT` and `working-directory` then need no translation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::executor::local::{run_tool, supervise};
use crate::executor::{OutputSink, StepOutcome, StepSession};
use crate::logging::LogLevel;
use crate::types::WorkflowError;

use super::manifest::DockerImageSource;

/// Everything one container action run needs.
pub(crate) struct ContainerAction<'a> {
    /// The CLI to drive — `docker`, or `podman` where the job runner says so.
    pub binary: &'a str,
    pub image: &'a DockerImageSource,
    /// Directory the action was checked out to; the build context, and what
    /// `entrypoint` paths are relative to.
    pub action_dir: &'a Path,
    pub entrypoint: Option<&'a str>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub workspace: &'a Path,
    pub runner_temp: &'a Path,
    pub working_directory: &'a Path,
    pub step_name: &'a str,
}

impl ContainerAction<'_> {
    /// Build or pull the image, then run it to completion.
    pub(crate) async fn run(
        self,
        sink: &dyn OutputSink,
        cancel: &CancellationToken,
    ) -> Result<StepOutcome, WorkflowError> {
        let image = self.resolve_image(sink).await?;

        // The container writes back through the same four files a shell step
        // does, so it needs the same scratch directory.
        let session = StepSession::create(self.runner_temp, "sh", "")?;
        let mut env = self.env.clone();
        env.extend(session.file_env());

        let name = format!("minact-action-{}", uuid::Uuid::new_v4());
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            name.clone(),
            "--workdir".to_string(),
            self.working_directory.to_string_lossy().to_string(),
        ];

        for mount in self.mounts() {
            let path = mount.to_string_lossy();
            args.push("--volume".to_string());
            args.push(format!("{}:{}", path, path));
        }

        // `--env KEY=VALUE` rather than `--env-file`, which cannot carry the
        // multi-line values a `$GITHUB_ENV` heredoc produces.
        let mut names: Vec<&String> = env.keys().collect();
        names.sort();
        for key in names {
            args.push("--env".to_string());
            args.push(format!("{}={}", key, env[key]));
        }

        if let Some(entrypoint) = self.entrypoint {
            args.push("--entrypoint".to_string());
            args.push(entrypoint.to_string());
        }
        args.push(image);
        args.extend(self.args.iter().cloned());

        let child = Command::new(self.binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                WorkflowError::StepFailed(
                    self.step_name.to_string(),
                    format!(
                        "could not run {} ({}) — a container action needs it",
                        self.binary, e
                    ),
                )
            })?;

        let binary = self.binary.to_string();
        let (success, status, cancelled) = supervise(
            child,
            self.step_name,
            sink,
            cancel,
            // Killing the CLI leaves the container running; the daemon owns it.
            |_pid| async move {
                let _ = run_tool(&binary, &["kill".to_string(), name]).await;
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

    /// The image to run, building it first when the action ships a Dockerfile.
    async fn resolve_image(&self, sink: &dyn OutputSink) -> Result<String, WorkflowError> {
        let dockerfile = match self.image {
            DockerImageSource::Registry(image) => return Ok(image.clone()),
            DockerImageSource::Dockerfile(path) => path,
        };

        let context = self.action_dir;
        let file = context.join(dockerfile);
        if !file.is_file() {
            return Err(WorkflowError::Other(format!(
                "the action's `image: {}` is not a file in {}",
                dockerfile,
                context.display()
            )));
        }

        // Tagged after the action's location, so rerunning a workflow reuses
        // the layer cache instead of building the same image again.
        let tag = format!("minact-action:{}", short_hash(&context.to_string_lossy()));
        sink.note(LogLevel::Info, format!("building {}", tag)).await;

        let (ok, output) = run_tool(
            self.binary,
            &[
                "build".to_string(),
                "--tag".to_string(),
                tag.clone(),
                "--file".to_string(),
                file.to_string_lossy().to_string(),
                context.to_string_lossy().to_string(),
            ],
        )
        .await?;

        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to build the action image from {}: {}",
                file.display(),
                output
            )));
        }
        Ok(tag)
    }

    /// Host directories the container has to see, with nothing mounted twice.
    fn mounts(&self) -> Vec<PathBuf> {
        let mut mounts: Vec<PathBuf> = Vec::new();
        for path in [self.workspace, self.runner_temp, self.action_dir] {
            if !mounts.iter().any(|mount| path.starts_with(mount)) {
                mounts.retain(|mount| !mount.starts_with(path));
                mounts.push(path.to_path_buf());
            }
        }
        mounts
    }
}

fn short_hash(value: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action<'a>(
        workspace: &'a Path,
        runner_temp: &'a Path,
        action_dir: &'a Path,
        image: &'a DockerImageSource,
    ) -> ContainerAction<'a> {
        ContainerAction {
            binary: "docker",
            image,
            action_dir,
            entrypoint: None,
            args: Vec::new(),
            env: HashMap::new(),
            workspace,
            runner_temp,
            working_directory: workspace,
            step_name: "test",
        }
    }

    #[test]
    fn mounts_every_directory_the_action_needs() {
        let image = DockerImageSource::Registry("alpine".into());
        let mounts = action(
            Path::new("/work"),
            Path::new("/tmp/minact"),
            Path::new("/home/me/.minact/actions/o/r/v1"),
            &image,
        )
        .mounts();
        assert_eq!(
            mounts,
            vec![
                PathBuf::from("/work"),
                PathBuf::from("/tmp/minact"),
                PathBuf::from("/home/me/.minact/actions/o/r/v1"),
            ]
        );
    }

    #[test]
    fn a_nested_directory_is_not_mounted_twice() {
        let image = DockerImageSource::Registry("alpine".into());
        // A local action and a temp dir both inside the workspace are already
        // covered by the workspace mount.
        let mounts = action(
            Path::new("/work"),
            Path::new("/work/.tmp"),
            Path::new("/work/.github/actions/build"),
            &image,
        )
        .mounts();
        assert_eq!(mounts, vec![PathBuf::from("/work")]);
    }

    #[test]
    fn a_parent_replaces_the_children_it_covers() {
        let image = DockerImageSource::Registry("alpine".into());
        let mounts = action(
            Path::new("/work/inner"),
            Path::new("/work"),
            Path::new("/work/actions/a"),
            &image,
        )
        .mounts();
        assert_eq!(mounts, vec![PathBuf::from("/work")]);
    }

    #[tokio::test]
    async fn a_registry_image_is_used_as_written() {
        let temp = tempfile::tempdir().unwrap();
        let image = DockerImageSource::Registry("alpine:3.18".into());
        let resolved = action(temp.path(), temp.path(), temp.path(), &image)
            .resolve_image(&NullSink)
            .await
            .unwrap();
        assert_eq!(resolved, "alpine:3.18");
    }

    #[tokio::test]
    async fn a_missing_dockerfile_is_reported_before_anything_runs() {
        let temp = tempfile::tempdir().unwrap();
        let image = DockerImageSource::Dockerfile("Dockerfile".into());
        let error = action(temp.path(), temp.path(), temp.path(), &image)
            .resolve_image(&NullSink)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Dockerfile"));
    }

    struct NullSink;

    #[async_trait::async_trait]
    impl OutputSink for NullSink {
        async fn line(&self, _stream: crate::logging::CommandStream, _line: String) {}
        async fn note(&self, _level: LogLevel, _message: String) {}
    }
}
