//! The project's `.minact/config.yml`.
//!
//! Everything minact needs to know about a project that is not in the workflow
//! files themselves. Today that is the runner mapping; the file is structured
//! in sections so more can join it without moving again.
//!
//! GitHub's `runs-on: ubuntu-latest` names a machine in GitHub's fleet, which
//! means nothing locally. Rather than guess, minact takes a mapping from the
//! project:
//!
//! ```yaml
//! # .minact/config.yml
//! runners:
//!   ubuntu-latest:
//!     type: docker
//!     image: ubuntu:24.04
//!   windows-latest:
//!     type: ssh
//!     host: win-builder.local
//!     user: builder
//!     remote-workspace: C:/minact/work
//!   macos-latest:
//!     type: local
//! ```
//!
//! The file is found at `.minact/config.yml` by default; an embedder with its
//! own layout passes candidates to [`Config::discover_in`].
//!
//! A label with no entry falls back to this machine *and says so*. Silently
//! running a `runs-on: windows-latest` job on a Mac and reporting green is the
//! behaviour this module exists to avoid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::executor::docker::{self, DockerConfig, DockerExecutor};
use crate::executor::local::LocalExecutor;
use crate::executor::ssh::{SshConfig, SshExecutor};
use crate::executor::Executor;
use crate::types::WorkflowError;

/// Where the config file is looked for when the caller does not say.
///
/// A tool that keeps its configuration elsewhere passes its own candidates to
/// [`Config::discover_in`] rather than expecting minact to know about its
/// layout.
pub const DEFAULT_CONFIG_FILES: &[&str] = &[".minact/config.yml", ".minact/config.yaml"];

/// One runner definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunnerSpec {
    /// This machine.
    Local,
    /// A container, giving Linux on any host.
    Docker {
        image: String,
        #[serde(default)]
        user: Option<String>,
        /// Pull the image before the job rather than using a local copy.
        #[serde(default)]
        pull: bool,
        /// Extra `docker run` arguments, e.g. `["--platform", "linux/amd64"]`.
        #[serde(default)]
        run_args: Vec<String>,
        /// The CLI to drive; `podman` is compatible.
        #[serde(default = "default_docker_binary")]
        binary: String,
    },
    /// Another machine, for targets a container cannot provide.
    Ssh {
        host: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default, rename = "identity-file")]
        identity_file: Option<PathBuf>,
        #[serde(default = "default_remote_workspace", rename = "remote-workspace")]
        remote_workspace: String,
        /// Push the workspace before the job and pull it back after.
        #[serde(default = "default_true")]
        sync: bool,
        #[serde(default, rename = "ssh-args")]
        ssh_args: Vec<String>,
    },
}

fn default_docker_binary() -> String {
    "docker".to_string()
}

fn default_remote_workspace() -> String {
    "~/minact-workspace".to_string()
}

fn default_true() -> bool {
    true
}

impl RunnerSpec {
    /// A short description for logs.
    pub fn describe(&self) -> String {
        match self {
            RunnerSpec::Local => "local".to_string(),
            RunnerSpec::Docker { image, .. } => format!("docker ({})", image),
            RunnerSpec::Ssh { host, user, .. } => match user {
                Some(user) => format!("ssh ({}@{})", user, host),
                None => format!("ssh ({})", host),
            },
        }
    }

    /// Whether this runner executes on the host machine.
    pub fn is_local(&self) -> bool {
        matches!(self, RunnerSpec::Local)
    }

    /// Build an executor for one job.
    ///
    /// `extra_mounts` are host directories steps must be able to reach — the
    /// action cache, in practice. A container fixes its mounts when it starts,
    /// so they have to be known here rather than when a step asks.
    pub fn build(
        &self,
        workspace: &Path,
        runner_temp: &Path,
        extra_mounts: &[PathBuf],
    ) -> Result<Arc<dyn Executor>, WorkflowError> {
        Ok(match self {
            RunnerSpec::Local => Arc::new(LocalExecutor::new()),
            RunnerSpec::Docker {
                image,
                user,
                pull,
                run_args,
                binary,
            } => Arc::new(DockerExecutor::new(DockerConfig {
                image: image.clone(),
                mounts: docker::default_mounts(workspace, runner_temp, extra_mounts),
                run_args: run_args.clone(),
                user: user.clone(),
                pull: *pull,
                binary: binary.clone(),
            })),
            RunnerSpec::Ssh {
                host,
                user,
                port,
                identity_file,
                remote_workspace,
                sync,
                ssh_args,
            } => Arc::new(SshExecutor::new(
                SshConfig {
                    host: host.clone(),
                    user: user.clone(),
                    port: *port,
                    identity_file: identity_file.clone(),
                    remote_workspace: remote_workspace.clone(),
                    sync: *sync,
                    ssh_args: ssh_args.clone(),
                },
                workspace.to_path_buf(),
            )),
        })
    }
}

/// A project's `.minact/config.yml`.
///
/// One object for everything minact needs to know about a project, in named
/// sections. Unknown sections are ignored, so a config written for a newer
/// minact still loads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Maps `runs-on:` labels to the places jobs run.
    #[serde(default)]
    pub runners: HashMap<String, RunnerSpec>,
}

impl Config {
    /// Load a configuration from a file.
    pub fn load(path: &Path) -> Result<Self, WorkflowError> {
        let text = std::fs::read_to_string(path).map_err(WorkflowError::IoError)?;
        serde_yaml::from_str(&text)
            .map_err(|e| WorkflowError::ParseError(format!("{}: {}", path.display(), e)))
    }

    /// Find and load a project's configuration, if it has one.
    pub fn discover(project_dir: &Path) -> Result<Option<(PathBuf, Self)>, WorkflowError> {
        Self::discover_in(project_dir, DEFAULT_CONFIG_FILES)
    }

    /// Find and load a configuration from specific candidate paths, in order.
    pub fn discover_in(
        project_dir: &Path,
        candidates: &[&str],
    ) -> Result<Option<(PathBuf, Self)>, WorkflowError> {
        for candidate in candidates {
            let path = project_dir.join(candidate);
            if path.exists() {
                return Ok(Some((path.clone(), Self::load(&path)?)));
            }
        }
        Ok(None)
    }

    /// Resolve a `runs-on:` label.
    ///
    /// Returns `None` when the label has no mapping, which the caller reports
    /// before falling back to the local machine.
    pub fn resolve(&self, runs_on: Option<&str>) -> Option<&RunnerSpec> {
        let label = runs_on?;
        self.runners.get(label).or_else(|| {
            // `runs-on: [self-hosted, linux]` is a list in GitHub; the parser
            // keeps it as written, so match on the first label too.
            let first = label
                .trim_start_matches('[')
                .split(',')
                .next()?
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == ']');
            self.runners.get(first)
        })
    }

    /// Whether any runner is mapped.
    pub fn has_runners(&self) -> bool {
        !self.runners.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("runner config should parse")
    }

    #[test]
    fn parses_every_runner_type() {
        let config = config(
            r#"
runners:
  ubuntu-latest:
    type: docker
    image: ubuntu:24.04
  windows-latest:
    type: ssh
    host: win-box
    user: builder
    remote-workspace: C:/work
  macos-latest:
    type: local
"#,
        );

        assert_eq!(config.runners.len(), 3);
        assert!(matches!(
            config.runners["ubuntu-latest"],
            RunnerSpec::Docker { .. }
        ));
        assert!(matches!(
            config.runners["windows-latest"],
            RunnerSpec::Ssh { .. }
        ));
        assert!(config.runners["macos-latest"].is_local());
    }

    #[test]
    fn docker_defaults_are_sensible() {
        let config = config("runners:\n  x:\n    type: docker\n    image: alpine:3\n");
        match &config.runners["x"] {
            RunnerSpec::Docker {
                binary, pull, user, ..
            } => {
                assert_eq!(binary, "docker");
                assert!(!pull);
                assert!(user.is_none());
            }
            other => panic!("expected docker, got {:?}", other),
        }
    }

    #[test]
    fn ssh_defaults_sync_on() {
        let config = config("runners:\n  x:\n    type: ssh\n    host: box\n");
        match &config.runners["x"] {
            RunnerSpec::Ssh {
                sync,
                remote_workspace,
                ..
            } => {
                assert!(sync);
                assert_eq!(remote_workspace, "~/minact-workspace");
            }
            other => panic!("expected ssh, got {:?}", other),
        }
    }

    #[test]
    fn resolves_exact_labels() {
        let config = config("runners:\n  ubuntu-latest:\n    type: local\n");
        assert!(config.resolve(Some("ubuntu-latest")).is_some());
        assert!(config.resolve(Some("windows-latest")).is_none());
        assert!(config.resolve(None).is_none());
    }

    #[test]
    fn resolves_the_first_label_of_a_list() {
        let config = config("runners:\n  self-hosted:\n    type: local\n");
        // `runs-on: [self-hosted, linux]` reaches us as written.
        assert!(config.resolve(Some("[self-hosted, linux]")).is_some());
    }

    #[test]
    fn describes_each_runner() {
        let config = config(
            r#"
runners:
  a:
    type: local
  b:
    type: docker
    image: alpine:3
  c:
    type: ssh
    host: box
    user: me
"#,
        );
        assert_eq!(config.runners["a"].describe(), "local");
        assert_eq!(config.runners["b"].describe(), "docker (alpine:3)");
        assert_eq!(config.runners["c"].describe(), "ssh (me@box)");
    }

    /// `runners` is one section of the project config, not the whole file, so
    /// a config carrying sections this build does not know about still loads.
    #[test]
    fn unknown_sections_are_ignored() {
        let config = config(
            r#"
runners:
  ubuntu-latest:
    type: local
cache:
  enabled: true
some-future-section:
  whatever: 1
"#,
        );
        assert!(config.has_runners());
        assert!(config.resolve(Some("ubuntu-latest")).is_some());
    }

    #[test]
    fn an_empty_config_is_valid() {
        let config = config("{}");
        assert!(!config.has_runners());
        assert!(config.resolve(Some("anything")).is_none());
    }

    #[test]
    fn defaults_belong_to_minact_only() {
        // A downstream tool's location is that tool's to pass in.
        assert_eq!(
            DEFAULT_CONFIG_FILES,
            &[".minact/config.yml", ".minact/config.yaml"]
        );
    }

    #[test]
    fn discovery_can_look_where_the_caller_says() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".mytool")).unwrap();
        std::fs::write(
            dir.path().join(".mytool/config.yml"),
            "runners:\n  x:\n    type: local\n",
        )
        .unwrap();

        // Not in the default locations, so the default search finds nothing...
        assert!(Config::discover(dir.path()).unwrap().is_none());

        // ...but the caller can say where to look.
        let (path, config) = Config::discover_in(dir.path(), &[".mytool/config.yml"])
            .unwrap()
            .unwrap();
        assert!(path.ends_with(".mytool/config.yml"));
        assert!(config.resolve(Some("x")).is_some());
    }

    #[test]
    fn discovers_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".minact")).unwrap();
        std::fs::write(
            dir.path().join(".minact/config.yml"),
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: ubuntu:24.04\n",
        )
        .unwrap();

        let (path, config) = Config::discover(dir.path()).unwrap().unwrap();
        assert!(path.ends_with("config.yml"));
        assert!(config.resolve(Some("ubuntu-latest")).is_some());

        // A project without one is not an error.
        let empty = tempfile::tempdir().unwrap();
        assert!(Config::discover(empty.path()).unwrap().is_none());
    }
}
