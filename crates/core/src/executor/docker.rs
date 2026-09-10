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

use super::local::{env_args, run_tool, run_tool_streams, supervise};
use super::{
    container_home, with_host_env, Executor, OutputSink, Platform, StepOutcome, StepRequest,
    StepSession,
};
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
    /// The image's own `PATH`, which is the only one that means anything
    /// inside the container.
    container_path: Option<String>,
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

    fn platform(&self) -> Option<Platform> {
        // A container is Linux whatever the host is; its architecture is the
        // host's unless `run-args` says otherwise, so that is left alone.
        Some(Platform {
            os: "Linux".to_string(),
            arch: None,
        })
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
        // Docker Desktop provides this name already; plain Linux docker does
        // not. It is what a host proxy on loopback gets rewritten to below.
        args.push("--add-host".to_string());
        args.push("host.docker.internal:host-gateway".to_string());
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

        // Two streams, not one: the id is on stdout, and docker puts warnings
        // — a platform mismatch, say — on stderr. Merging them would make the
        // last warning look like the container id.
        let (ok, stdout, stderr) = run_tool_streams(&self.config.binary, &args).await?;
        if !ok {
            let output = if stderr.is_empty() { stdout } else { stderr };
            return Err(WorkflowError::Other(format!(
                "failed to start a container from {}: {}",
                self.config.image, output
            )));
        }

        let container_id = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .next_back()
            .unwrap_or_default()
            .to_string();
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

        // Ask the image what its `PATH` is, once, for the same reason: the
        // host's is about to be handed to every step and it names host
        // directories, not the image's.
        let (ok, container_path) = run_tool(
            &self.config.binary,
            &[
                "exec".to_string(),
                container_id.clone(),
                "sh".to_string(),
                "-c".to_string(),
                "printf %s \"$PATH\"".to_string(),
            ],
        )
        .await?;

        let mut state = self.state.lock().await;
        state.container_id = Some(container_id);
        state.has_bash = has_bash;
        state.container_path =
            (ok && !container_path.trim().is_empty()).then(|| container_path.trim().to_string());

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

        // The host environment goes along, as it always has: the workspace
        // and the action cache are mounted at their host paths, so a good
        // deal of it still means something inside.
        let mut env = with_host_env(&request.env, &request.extra_paths);
        env.extend(session.file_env());

        // ...with two exceptions. The host's home is not in the container, so
        // `$HOME` is a directory of the job's own — unless the workflow set
        // one itself.
        if !request.env.contains_key("HOME") {
            env.insert("HOME".to_string(), container_home(&request.runner_temp)?);
        }

        // And the host's `PATH` names host directories, so
        // inside the container it is at best noise and at worst harmful: it
        // hides the tools the image ships, like the `node` an act-style image
        // keeps under /opt, which is what a JavaScript action runs on. Put the
        // image's own `PATH` back, with `$GITHUB_PATH` additions in front.
        if let Some(container_path) = self.state.lock().await.container_path.clone() {
            let mut parts: Vec<String> = request.extra_paths.to_vec();
            parts.push(container_path);
            env.insert("PATH".to_string(), parts.join(":"));
        }

        // `http_proxy=http://127.0.0.1:7890` is the same shape of problem:
        // loopback is the container, not the host that runs the proxy. Point
        // it back at the host rather than dropping it — the user set it
        // because that is how this machine reaches the network.
        redirect_loopback_proxies(&mut env);

        let script = session.script_path().to_string_lossy().to_string();
        let shell = if super::is_bash(&request.shell) && !has_bash {
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

/// Names of the proxy variables, in both the spellings tools look for.
const PROXY_VARS: [&str; 8] = [
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ftp_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
];

/// The name a container reaches its host by. Docker Desktop defines it, and
/// `--add-host ...:host-gateway` defines it everywhere else.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

/// Repoint proxy variables that name a loopback address at the host.
///
/// A proxy listening on the host's `127.0.0.1` is unreachable from inside a
/// container, where that address is the container itself — every fetch fails
/// with a connection refused that looks nothing like a proxy problem.
fn redirect_loopback_proxies(env: &mut HashMap<String, String>) {
    for name in PROXY_VARS {
        let Some(value) = env.get(name) else { continue };
        if let Some(rewritten) = rewrite_loopback_host(value) {
            env.insert(name.to_string(), rewritten);
        }
    }
}

/// Swap a loopback host in a proxy URL for [`DOCKER_HOST_ALIAS`], or `None`
/// when the URL does not name one.
fn rewrite_loopback_host(url: &str) -> Option<String> {
    let (prefix, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (format!("{}://", scheme), rest),
        None => (String::new(), url),
    };

    // Authority runs to the first `/`, `?` or `#`; credentials stay put.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (credentials, host_port) = match authority.rsplit_once('@') {
        Some((credentials, host_port)) => (format!("{}@", credentials), host_port),
        None => (String::new(), authority),
    };

    let (host, port) = match host_port.rsplit_once(':') {
        // A bare IPv6 literal is full of colons; only a `]:` is a port.
        Some((host, port)) if !port.contains(']') => (host, format!(":{}", port)),
        _ => (host_port, String::new()),
    };

    if !is_loopback(host) {
        return None;
    }

    Some(format!(
        "{}{}{}{}{}",
        prefix, credentials, DOCKER_HOST_ALIAS, port, tail
    ))
}

/// Whether a URL host names the local machine.
fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_proxy_on_loopback_is_repointed_at_the_host() {
        assert_eq!(
            rewrite_loopback_host("http://127.0.0.1:7890").as_deref(),
            Some("http://host.docker.internal:7890")
        );
        assert_eq!(
            rewrite_loopback_host("http://localhost:8080/path").as_deref(),
            Some("http://host.docker.internal:8080/path")
        );
        assert_eq!(
            rewrite_loopback_host("socks5://user:pw@127.0.0.2:1080").as_deref(),
            Some("socks5://user:pw@host.docker.internal:1080")
        );
        assert_eq!(
            rewrite_loopback_host("http://[::1]:7890").as_deref(),
            Some("http://host.docker.internal:7890")
        );
        // No scheme is still a proxy value tools accept.
        assert_eq!(
            rewrite_loopback_host("127.0.0.1:7890").as_deref(),
            Some("host.docker.internal:7890")
        );
    }

    #[test]
    fn a_proxy_that_is_already_reachable_is_left_alone() {
        assert_eq!(rewrite_loopback_host("http://proxy.corp:3128"), None);
        assert_eq!(rewrite_loopback_host("http://10.0.0.5:7890"), None);
        assert_eq!(
            rewrite_loopback_host("http://host.docker.internal:7890"),
            None
        );
    }

    #[test]
    fn only_the_proxy_variables_are_rewritten() {
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert(
            "HTTPS_PROXY".to_string(),
            "http://127.0.0.1:7890".to_string(),
        );
        env.insert(
            "http_proxy".to_string(),
            "http://127.0.0.1:7890".to_string(),
        );
        env.insert("NO_PROXY".to_string(), "localhost,127.0.0.1".to_string());
        env.insert("API_URL".to_string(), "http://127.0.0.1:9000".to_string());

        redirect_loopback_proxies(&mut env);

        assert_eq!(env["HTTPS_PROXY"], "http://host.docker.internal:7890");
        assert_eq!(env["http_proxy"], "http://host.docker.internal:7890");
        // `NO_PROXY` is a host list, not a URL, and the container's own
        // loopback is exactly what it should keep excluding.
        assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1");
        assert_eq!(env["API_URL"], "http://127.0.0.1:9000");
    }

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
