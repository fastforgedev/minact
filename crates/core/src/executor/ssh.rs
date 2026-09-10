//! Runs steps on another machine over SSH.
//!
//! This is the backend for targets a container cannot provide: Windows, or
//! real macOS hardware for signing and notarisation.
//!
//! # How commands get there
//!
//! Nothing minact runs is put on the `ssh` command line, because that line is
//! interpreted by whatever login shell the remote hands out — `sh` on a Unix
//! box, `cmd.exe` on a stock Windows OpenSSH server, occasionally PowerShell.
//! Every round trip instead runs one fixed program, a POSIX shell reading its
//! script from stdin (`sh -s`, or Git for Windows' `bash.exe -s`), and the
//! script travels over stdin with it. Workflow data never meets a shell it was
//! not quoted for.
//!
//! # How the workspace gets there
//!
//! Unlike [`docker`](super::docker), the remote filesystem is a different
//! filesystem, so paths cannot simply match. The workspace is pushed before
//! the first step and pulled back after the last, and every host path under
//! the workspace is rewritten to its remote equivalent. `rsync` does the
//! copying when both ends have it; otherwise a `tar` stream goes over the same
//! stdin, and only the files the job touched come back.
//!
//! # Windows
//!
//! A Windows host is recognised from its login shell and driven through Git
//! for Windows' bash, found next to `git.exe`. Steps then behave the way they
//! do on GitHub's Windows runners: `shell: bash` is Git Bash, `shell: pwsh`
//! falls back to Windows PowerShell when PowerShell 7 is not installed, and
//! the `$GITHUB_*` files are read back tolerating the UTF-16 that Windows
//! PowerShell's `>>` writes.
//!
//! # Built-in actions
//!
//! Registered actions (`actions/checkout`, `actions/upload-artifact`) run
//! in-process on the *host* and touch the host workspace. Around each one the
//! workspace is reconciled: what the remote steps produced comes back first,
//! so `upload-artifact` sees the build, and what the action left behind goes
//! over afterwards, so a restored cache is there for the next step. Both
//! directions are incremental.
//!
//! # Node
//!
//! A JavaScript action needs `node`. A remote without one gets the official
//! build of the major the action asked for, verified against its published
//! checksum and kept in the remote's tool cache, the way GitHub's runners
//! carry their own. It is laid out the way `actions/toolkit` lays out a tool
//! cache — `node/<version>/<arch>/` plus a `<arch>.complete` marker — so a
//! later `actions/setup-node` step finds it rather than downloading again.
//! `MINACT_NODE_MIRROR` names a mirror with the same layout as
//! `nodejs.org/dist`.
//!
//! # The remote's own files
//!
//! The remote keeps minact's files where the host does: `.minact/_work`
//! under the workspace, with `_temp` for scripts and step files, `_tool` for
//! the tool cache and `_actions` for actions copied over. Same relative
//! place on both sides, so a host path maps to a remote one by swapping the
//! workspace prefix, and the sync leaves the tree out in both directions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::local::{run_tool, supervise};
use super::{
    shell_quote, Executor, OutputSink, Platform, StepFileContents, StepOutcome, StepRequest,
};
use crate::logging::LogLevel;
use crate::types::WorkflowError;

/// Where Git for Windows puts bash when `where.exe git` cannot say.
const DEFAULT_WINDOWS_BASH: &str = "C:/Program Files/Git/bin/bash.exe";

/// The runner's own tree, relative to the workspace on either side (see
/// [`crate::layout`]). On the host it holds this machine's scratch files and
/// tool cache; on the remote, the remote's. Neither means anything to the
/// other, so neither direction of a sync touches it.
const PRIVATE_DIR: &str = ".minact/_work";

/// The stamp a push leaves so a pull can tell what the job changed, relative
/// to the remote workspace.
const SYNC_STAMP: &str = ".minact/_work/_temp/sync-stamp";

/// The Node installed for JavaScript actions when the remote has none and
/// the action did not say which it wants.
const DEFAULT_NODE_MAJOR: u32 = 20;

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
    /// The program on the remote that runs minact's scripts, fed on stdin.
    /// Detected when unset: `sh`, or Git for Windows' `bash.exe`.
    pub shell: Option<String>,
    /// Patterns left out of the workspace sync, in both directions.
    pub exclude: Vec<String>,
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
            shell: None,
            exclude: Vec::new(),
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

    /// `--exclude` arguments for rsync, in either direction.
    fn rsync_excludes(&self) -> Vec<String> {
        std::iter::once(format!("--exclude=/{}", PRIVATE_DIR))
            .chain(self.user_excludes())
            .collect()
    }

    /// `--exclude` arguments for a local `tar` run from the workspace root.
    fn tar_excludes(&self) -> Vec<String> {
        // tar sees members as `./path`, so the anchor is spelled that way.
        std::iter::once(format!("--exclude=./{}", PRIVATE_DIR))
            .chain(self.user_excludes())
            .collect()
    }

    /// The user's `exclude:` patterns as `--exclude` arguments.
    fn user_excludes(&self) -> Vec<String> {
        self.exclude
            .iter()
            .map(|pattern| format!("--exclude={}", pattern))
            .collect()
    }
}

/// What the remote's login shell is — the shell that interprets the `ssh`
/// command line, and so decides how minact's own shell has to be invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Login {
    /// A Bourne-compatible shell.
    Posix,
    /// `cmd.exe`, the default on Windows OpenSSH.
    Cmd,
    /// PowerShell configured as the OpenSSH default shell.
    PowerShell,
}

/// How the workspace travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Rsync,
    Tar,
}

/// What [`SshExecutor::prepare`] learned about the other end.
#[derive(Debug, Clone)]
struct Remote {
    login: Login,
    /// The program that runs minact's scripts, fed on stdin.
    shell: String,
    /// The workspace with `~` resolved and separators normalised.
    workspace: String,
    /// `runner.os` spelling: `Linux`, `macOS`, `Windows`.
    os: String,
    /// `runner.arch` spelling: `X64`, `ARM64`.
    arch: String,
    /// Whether `node` is on the remote's PATH; without it minact installs one.
    has_node: bool,
    /// Programs the remote lacks, and what it has instead.
    substitutes: HashMap<String, String>,
    transfer: Transfer,
}

impl Remote {
    fn is_windows(&self) -> bool {
        self.os == "Windows"
    }
}

/// Executes steps on a remote host.
pub struct SshExecutor {
    config: SshConfig,
    /// Local workspace root, used to rewrite paths into remote ones.
    workspace: PathBuf,
    /// The job's `$RUNNER_TEMP` on the host. It maps to the remote's own
    /// scratch directory, whatever it was called here.
    host_temp: PathBuf,
    /// Host directories already copied over, and where they landed. A job that
    /// uses the same action in five steps copies it once.
    provisioned: Mutex<HashMap<PathBuf, String>>,
    /// Filled in by `prepare`; the defaults let path mapping work before then.
    remote: RwLock<Remote>,
    /// The `node` minact installed on the remote, once it has.
    node: Mutex<Option<String>>,
}

impl SshExecutor {
    pub fn new(config: SshConfig, workspace: PathBuf, host_temp: PathBuf) -> Self {
        let remote = Remote {
            login: Login::Posix,
            shell: "sh".to_string(),
            workspace: normalise_remote_path(&config.remote_workspace),
            os: String::new(),
            arch: String::new(),
            has_node: true,
            substitutes: HashMap::new(),
            transfer: Transfer::Tar,
        };
        Self {
            config,
            workspace,
            host_temp,
            provisioned: Mutex::new(HashMap::new()),
            remote: RwLock::new(remote),
            node: Mutex::new(None),
        }
    }

    fn remote(&self) -> Remote {
        self.remote
            .read()
            .expect("remote state lock should not be poisoned")
            .clone()
    }

    fn remote_workspace(&self) -> String {
        self.remote().workspace
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

        format!("{}/{}-{}", self.remote_actions(), name, &hash[..8])
    }

    /// Rewrite a host path to its remote equivalent.
    ///
    /// The job's scratch directory becomes the remote's own, and anything
    /// under the workspace keeps its place relative to it. Paths outside
    /// both cannot be mapped and are left alone; the caller is responsible
    /// for not depending on them remotely.
    pub fn remote_path(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.host_temp) {
            return join_remote(&self.remote_temp(), relative);
        }
        match path.strip_prefix(&self.workspace) {
            Ok(relative) => join_remote(&self.remote_workspace(), relative),
            Err(_) => path.to_string_lossy().to_string(),
        }
    }

    /// The runner's own tree on the remote: `<workspace>/.minact/_work`.
    fn remote_work(&self) -> String {
        format!("{}/{}", self.remote_workspace(), PRIVATE_DIR)
    }

    /// The remote directory holding this job's scripts and env files.
    fn remote_temp(&self) -> String {
        format!("{}/_temp", self.remote_work())
    }

    /// The remote's `$RUNNER_TOOL_CACHE`.
    fn remote_tool_cache(&self) -> String {
        format!("{}/_tool", self.remote_work())
    }

    /// Where actions copied from the host land.
    fn remote_actions(&self) -> String {
        format!("{}/_actions", self.remote_work())
    }

    /// The part of a step's script that sets the scene and hands over to the
    /// interpreter: the environment, the working directory, then `exec`.
    ///
    /// The environment travels inside the script rather than on the command
    /// line: `ssh` concatenates its arguments into one string for the remote
    /// login shell, so anything unquoted there would be re-interpreted. Names
    /// a shell cannot `export` — `INPUT_NODE-VERSION`, the way GitHub spells
    /// action inputs — go through `env` instead.
    ///
    /// `tool_paths` are directories minact itself provided on the remote (an
    /// installed `node`); they go on `PATH` behind the step's own additions.
    pub fn build_step_script(
        &self,
        request: &StepRequest,
        env: &HashMap<String, String>,
        program: &str,
        args: &[String],
        tool_paths: &[String],
    ) -> String {
        let remote = self.remote();
        let mut script = String::new();
        let mut awkward: Vec<String> = Vec::new();
        for (key, value) in self.remote_env(env) {
            if is_shell_identifier(&key) {
                script.push_str(&format!("export {}={}\n", key, shell_quote(&value)));
            } else {
                awkward.push(shell_quote(&format!("{}={}", key, value)));
            }
        }

        // `$GITHUB_PATH` additions go in front of the remote's own `PATH`,
        // which is the one thing of the remote's the step must keep. On
        // Windows a `C:/...` entry has to become `/c/...` first, or the colon
        // splits it.
        let mut paths: Vec<String> = request
            .extra_paths
            .iter()
            .map(|path| self.remote_path(Path::new(path)))
            .collect();
        paths.extend(tool_paths.iter().cloned());
        if !paths.is_empty() {
            let entries: Vec<String> = paths
                .iter()
                .map(|path| {
                    if remote.is_windows() {
                        format!("\"$(cygpath -u {})\"", shell_quote(path))
                    } else {
                        shell_quote(path)
                    }
                })
                .collect();
            script.push_str(&format!("export PATH={}:\"$PATH\"\n", entries.join(":")));
        }

        script.push_str(&format!(
            "cd {} || exit 1\n",
            shell_quote(&self.remote_path(&request.working_directory))
        ));

        let mut command = vec![shell_quote(program)];
        command.extend(args.iter().map(|arg| shell_quote(arg)));
        if awkward.is_empty() {
            script.push_str(&format!("exec {}\n", command.join(" ")));
        } else {
            script.push_str(&format!(
                "exec env {} {}\n",
                awkward.join(" "),
                command.join(" ")
            ));
        }
        script
    }

    /// The step's environment as the remote should see it: host paths under
    /// the workspace become remote paths, and the runner's scratch
    /// directories become the remote's own. Sorted, so the script is stable.
    fn remote_env(&self, env: &HashMap<String, String>) -> BTreeMap<String, String> {
        env.iter()
            .map(|(key, value)| {
                let value = match key.as_str() {
                    // The host's may be anywhere — `RUNNER_TOOL_CACHE` in the
                    // environment moves it — but the remote's is its own.
                    "RUNNER_TOOL_CACHE" => self.remote_tool_cache(),
                    // The job's scratch directory and anything under the
                    // workspace map; anything else — a plain value, or a host
                    // path with no remote meaning — comes back unchanged.
                    _ => self.remote_path(Path::new(value)),
                };
                (key.clone(), value)
            })
            .collect()
    }

    /// The `ssh` command line that starts minact's shell reading stdin,
    /// spelled for whichever login shell will parse it.
    fn shell_invocation(remote: &Remote) -> String {
        match remote.login {
            Login::Posix => format!("{} -s", shell_quote(&remote.shell)),
            Login::Cmd => format!("\"{}\" -s", remote.shell.replace('/', "\\")),
            Login::PowerShell => format!("& '{}' -s", remote.shell.replace('/', "\\")),
        }
    }

    /// Full `ssh` arguments for one scripted round trip.
    fn script_args(&self, remote: &Remote) -> Vec<String> {
        let mut args = self.config.ssh_base_args();
        args.push(self.config.destination());
        args.push(Self::shell_invocation(remote));
        args
    }

    fn spawn_ssh(&self, remote: &Remote) -> Result<Child, WorkflowError> {
        Command::new("ssh")
            .args(self.script_args(remote))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| WorkflowError::Other(format!("failed to run ssh: {}", e)))
    }

    /// Run a script on the remote and collect what it printed, raw.
    async fn run_script_raw(&self, script: &str) -> Result<(bool, Vec<u8>, String), WorkflowError> {
        let remote = self.remote();
        let mut child = self.spawn_ssh(&remote)?;
        let feeder = feed_stdin(&mut child, script.as_bytes().to_vec());
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;
        let _ = feeder.await;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Ok((output.status.success(), output.stdout, stderr))
    }

    /// Run a script on the remote; stdout and stderr come back as one text.
    async fn run_script(&self, script: &str) -> Result<(bool, String), WorkflowError> {
        let (ok, stdout, stderr) = self.run_script_raw(script).await?;
        let mut text = String::from_utf8_lossy(&stdout).trim().to_string();
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        Ok((ok, text))
    }

    /// One command on the raw `ssh` command line, for before the remote's
    /// login shell is known. Only ever given text every shell reads the same.
    async fn run_on_login_shell(&self, command: &str) -> Result<(bool, String), WorkflowError> {
        let mut args = self.config.ssh_base_args();
        args.push(self.config.destination());
        args.push(command.to_string());
        run_tool("ssh", &args).await
    }

    /// Find out which login shell answers, which is also the reachability
    /// check.
    async fn login_probe(&self) -> Result<Login, WorkflowError> {
        // `%OS%` expands under cmd.exe and nowhere else; `$(uname -s)`
        // expands under a POSIX shell and nowhere else.
        let (_, output) = self
            .run_on_login_shell("echo minact-ok %OS% $(uname -s)")
            .await?;
        classify_login(&output).ok_or_else(|| {
            WorkflowError::Other(format!(
                "cannot reach {}: {}",
                self.config.destination(),
                output
            ))
        })
    }

    /// Locate Git for Windows' bash, the POSIX shell a Windows host has.
    async fn find_windows_bash(&self) -> Result<String, WorkflowError> {
        // `where.exe` rather than `where`: under PowerShell the bare name is
        // an alias for Where-Object.
        if let Ok((true, output)) = self.run_on_login_shell("where.exe git").await {
            if let Some(bash) = output
                .lines()
                .find_map(|line| git_bash_from_git(line.trim()))
            {
                return Ok(bash);
            }
        }
        Ok(DEFAULT_WINDOWS_BASH.to_string())
    }

    /// Learn everything a job needs to know about the other end.
    async fn discover(&self) -> Result<Remote, WorkflowError> {
        let login = self.login_probe().await?;
        let shell = match &self.config.shell {
            Some(shell) => shell.replace('\\', "/"),
            None => match login {
                Login::Posix => "sh".to_string(),
                Login::Cmd | Login::PowerShell => self.find_windows_bash().await?,
            },
        };

        // Install the shell provisionally so the facts probe can use it.
        {
            let mut remote = self
                .remote
                .write()
                .expect("remote state lock should not be poisoned");
            remote.login = login;
            remote.shell = shell.clone();
        }

        let (ok, output) = self.run_script(FACTS_SCRIPT).await?;
        if !ok || !output.contains("minact-shell-ok") {
            return Err(WorkflowError::Other(format!(
                "{} could not run `{}` (install Git for Windows on a Windows host, \
                 or point the runner's `shell:` at a POSIX shell): {}",
                self.config.destination(),
                shell,
                output
            )));
        }
        let facts: HashMap<&str, &str> = output
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect();
        let has = |key: &str| facts.get(key).is_some_and(|value| !value.is_empty());

        let (os, arch) = platform_names(
            facts.get("os").copied().unwrap_or(""),
            facts.get("arch").copied().unwrap_or(""),
        );
        let workspace = resolve_workspace(
            &self.config.remote_workspace,
            facts.get("home").copied().unwrap_or(""),
        )?;

        // What GitHub's runners have that this machine may not: `pwsh` is
        // PowerShell 7, and Windows ships 5.1 as `powershell`.
        let mut substitutes = HashMap::new();
        for (wanted, fallback) in [("pwsh", "powershell"), ("python3", "python")] {
            if !has(wanted) && has(fallback) {
                substitutes.insert(wanted.to_string(), fallback.to_string());
            }
        }

        // rsync needs to be on both ends, and reachable from the login shell
        // — which rules out a Windows host, where the login shell is cmd.
        let transfer = if login == Login::Posix && has("rsync") && local_has_rsync().await {
            Transfer::Rsync
        } else {
            Transfer::Tar
        };

        Ok(Remote {
            login,
            shell,
            workspace,
            os,
            arch,
            has_node: has("node"),
            substitutes,
            transfer,
        })
    }

    /// A `node` for JavaScript actions on a remote that has none: the
    /// official build of the requested major, checked against its published
    /// checksum, installed once into the remote's tool cache and kept.
    ///
    /// The cache is laid out the way `actions/toolkit` expects —
    /// `node/<version>/<arch>/` with a `<arch>.complete` marker beside it —
    /// so `actions/setup-node` and minact share one copy: whichever installs
    /// first, the other finds it.
    async fn ensure_node(
        &self,
        remote: &Remote,
        major: u32,
        sink: &dyn OutputSink,
    ) -> Result<String, WorkflowError> {
        let mut installed = self.node.lock().await;
        if let Some(node) = installed.as_ref() {
            return Ok(node.clone());
        }

        let (os, ext) = match remote.os.as_str() {
            "Windows" => ("win", "zip"),
            "macOS" => ("darwin", "tar.gz"),
            _ => ("linux", "tar.gz"),
        };
        let arch = match remote.arch.as_str() {
            "ARM64" => "arm64",
            "X86" => "x86",
            _ => "x64",
        };
        // `MINACT_NODE_MIRROR` points at a mirror with the same layout as
        // nodejs.org/dist, for networks where that is the faster way.
        let mirror = std::env::var("MINACT_NODE_MIRROR")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| "https://nodejs.org/dist".to_string());
        let mirror = mirror.trim_end_matches('/').to_string();
        let tools = self.remote_tool_cache();

        // An install is complete only once its `.complete` marker exists, so a
        // download that died half-way is not mistaken for a node. Any
        // finished build of the right major will do, so the first one found
        // is used rather than the highest.
        let script = format!(
            r#"set -e
tools={tools}
arch={arch}
for dir in "$tools"/node/{major}.*/"$arch"; do
  [ -f "$dir.complete" ] || continue
  for candidate in "$dir/bin/node" "$dir/node.exe"; do
    if [ -x "$candidate" ]; then echo "node=$candidate"; exit 0; fi
  done
done
base={mirror}/latest-v{major}.x
tmp="$tools/tmp-node-$$"
mkdir -p "$tmp" && cd "$tmp"
curl -fsSL "$base/SHASUMS256.txt" -o SHASUMS256.txt
file=$(grep -o "node-v{major}\.[0-9]*\.[0-9]*-{os}-{arch}\.{ext_pattern}" SHASUMS256.txt | head -n 1)
if [ -z "$file" ]; then echo "no Node {major} build for {os}-{arch} at $base" >&2; exit 1; fi
curl -fsSL "$base/$file" -o "$file"
expected=$(grep " $file$" SHASUMS256.txt | cut -d' ' -f1)
actual=$( (sha256sum "$file" 2>/dev/null || shasum -a 256 "$file") | cut -d' ' -f1 )
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then echo "checksum mismatch for $file" >&2; exit 1; fi
case "$file" in
  *.zip) unzip -q "$file" ;;
  *) tar -xzf "$file" ;;
esac
version=${{file#node-v}}
version=${{version%%-*}}
dir="$tools/node/$version/$arch"
mkdir -p "$tools/node/$version"
rm -rf "$dir" "$dir.complete"
mv "${{file%.{ext}}}" "$dir"
: > "$dir.complete"
cd / && rm -rf "$tmp"
echo installed=1
for candidate in "$dir/bin/node" "$dir/node.exe"; do
  if [ -x "$candidate" ]; then echo "node=$candidate"; exit 0; fi
done
echo "Node {major} unpacked but no node binary found in $dir" >&2
exit 1
"#,
            tools = shell_quote(&tools),
            mirror = shell_quote(&mirror),
            major = major,
            os = os,
            arch = arch,
            ext = ext,
            ext_pattern = ext.replace('.', "\\."),
        );

        sink.note(
            LogLevel::Info,
            format!(
                "{} has no `node`; using Node {} from the tool cache at {}",
                self.config.destination(),
                major,
                tools
            ),
        )
        .await;
        let (ok, output) = self.run_script(&script).await?;
        let node = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("node="))
            .map(|path| path.to_string());
        let node = match (ok, node) {
            (true, Some(node)) => node,
            _ => {
                return Err(WorkflowError::Other(format!(
                    "could not install Node {} on {}: {}",
                    major,
                    self.config.destination(),
                    output
                )))
            }
        };
        if output.contains("installed=1") {
            sink.note(
                LogLevel::Info,
                format!("installed Node {} on {}", major, self.config.destination()),
            )
            .await;
        }
        *installed = Some(node.clone());
        Ok(node)
    }

    /// Copy a local directory to the remote, mirroring deletions.
    ///
    /// `stamp` is touched once the copy is complete, so that a later pull can
    /// tell what the job changed from what arrived with it.
    async fn push(&self, from: &Path, to: &str, stamp: Option<&str>) -> Result<(), WorkflowError> {
        let remote = self.remote();
        match remote.transfer {
            Transfer::Rsync => self.push_rsync(from, to).await,
            Transfer::Tar => self.push_tar(&remote, from, to, stamp).await,
        }
    }

    async fn push_rsync(&self, from: &Path, to: &str) -> Result<(), WorkflowError> {
        let mut args = vec![
            "--archive".to_string(),
            "--compress".to_string(),
            "--delete".to_string(),
        ];
        args.extend(self.config.rsync_excludes());
        args.extend([
            "-e".to_string(),
            self.config.rsync_shell_arg(),
            format!("{}/", from.to_string_lossy()),
            format!("{}:{}/", self.config.destination(), to),
        ]);
        let (ok, output) = run_tool("rsync", &args).await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to sync the workspace to {}: {}",
                self.config.host, output
            )));
        }
        Ok(())
    }

    /// `tar` on this side, piped into `tar` on the other, behind a script
    /// that clears the destination first so the result mirrors the source.
    async fn push_tar(
        &self,
        remote: &Remote,
        from: &Path,
        to: &str,
        stamp: Option<&str>,
    ) -> Result<(), WorkflowError> {
        let extract = match stamp {
            Some(stamp) => format!(
                "tar -xf - -C {} && touch {}",
                shell_quote(to),
                shell_quote(stamp)
            ),
            None => format!("exec tar -xf - -C {}", shell_quote(to)),
        };
        // Everything goes except the remote's own `.minact/_work`: clear the
        // top level around `.minact`, then `.minact` around `_work`.
        let script = format!(
            "set -e\n\
             mkdir -p {to}\n\
             find {to} -mindepth 1 -maxdepth 1 ! -name .minact -exec rm -rf {{}} +\n\
             [ -d {to}/.minact ] && find {to}/.minact -mindepth 1 -maxdepth 1 ! -name _work -exec rm -rf {{}} +\n\
             {extract}\n",
            to = shell_quote(to),
            extract = extract,
        );

        let mut args = local_tar_create_args(remote).await;
        args.extend(self.config.tar_excludes());
        args.extend([
            "-C".to_string(),
            from.to_string_lossy().to_string(),
            ".".to_string(),
        ]);
        let mut tar = Command::new("tar");
        tar.args(&args).stdin(Stdio::null());
        self.send_archive(remote, tar, None, script, &from.display().to_string())
            .await
    }

    /// Stream a local `tar` into a remote script that ends by extracting it.
    ///
    /// `list` is fed to tar's stdin, for a `-T -` invocation that archives
    /// named files rather than a directory.
    async fn send_archive(
        &self,
        remote: &Remote,
        mut tar: Command,
        list: Option<Vec<u8>>,
        script: String,
        what: &str,
    ) -> Result<(), WorkflowError> {
        let mut tar = tar
            // No AppleDouble `._*` companions from macOS.
            .env("COPYFILE_DISABLE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| WorkflowError::Other(format!("failed to run tar: {}", e)))?;
        let mut ssh = self.spawn_ssh(remote)?;

        let list_feeder = match (list, tar.stdin.take()) {
            (Some(list), Some(mut stdin)) => Some(tokio::spawn(async move {
                let _ = stdin.write_all(&list).await;
                let _ = stdin.shutdown().await;
            })),
            _ => None,
        };
        let mut archive = tar.stdout.take().expect("tar stdout is piped");
        let mut pipe = ssh.stdin.take().expect("ssh stdin is piped");
        let pump = tokio::spawn(async move {
            // The script first, then the archive it ends by extracting.
            if pipe.write_all(script.as_bytes()).await.is_ok() {
                let _ = tokio::io::copy(&mut archive, &mut pipe).await;
            }
            let _ = pipe.shutdown().await;
        });

        let ssh_output = ssh
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;
        let tar_output = tar
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("tar failed: {}", e)))?;
        let _ = pump.await;
        if let Some(feeder) = list_feeder {
            let _ = feeder.await;
        }

        if !tar_output.status.success() {
            return Err(WorkflowError::Other(format!(
                "failed to archive {}: {}",
                what,
                String::from_utf8_lossy(&tar_output.stderr).trim()
            )));
        }
        if !ssh_output.status.success() {
            return Err(WorkflowError::Other(format!(
                "failed to sync {} to {} ({}): {}",
                what,
                self.config.host,
                ssh_output.status,
                combined_output(&ssh_output)
            )));
        }
        Ok(())
    }

    /// Send the files the host changed since `since`, on top of what the
    /// remote already has. Nothing is deleted: this runs mid-job, and the
    /// remote's own state — a `node_modules` the sync excludes, say — has to
    /// survive it.
    async fn push_changes_tar(
        &self,
        remote: &Remote,
        since: SystemTime,
    ) -> Result<(), WorkflowError> {
        // `find -newer` wants a file to compare against.
        let stamp = tempfile::NamedTempFile::new()?;
        stamp.as_file().set_modified(since)?;

        let listing = Command::new("find")
            .args([".", "-type", "f", "-newer"])
            .arg(stamp.path())
            .arg("-print0")
            .current_dir(&self.workspace)
            .output()
            .await
            .map_err(|e| WorkflowError::Other(format!("failed to run find: {}", e)))?;
        if !listing.status.success() {
            return Err(WorkflowError::Other(format!(
                "could not list changed files in {}: {}",
                self.workspace.display(),
                String::from_utf8_lossy(&listing.stderr).trim()
            )));
        }
        let changed: Vec<&[u8]> = listing
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .filter(|entry| !self.is_excluded(&String::from_utf8_lossy(entry)))
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        let mut list = Vec::new();
        for entry in changed {
            list.extend_from_slice(entry);
            list.push(0);
        }

        let mut args = local_tar_create_args(remote).await;
        args.extend(self.config.tar_excludes());
        args.extend(["--null".to_string(), "-T".to_string(), "-".to_string()]);
        let mut tar = Command::new("tar");
        tar.args(&args)
            .current_dir(&self.workspace)
            .stdin(Stdio::piped());

        // The stamp moves too: what just arrived is not something the job
        // changed, so the next pull must not bring it straight back.
        let script = format!(
            "cd {ws} || exit 1\ntar -xf - && touch {stamp}\n",
            ws = shell_quote(&remote.workspace),
            stamp = SYNC_STAMP,
        );
        self.send_archive(remote, tar, Some(list), script, "changed files")
            .await
    }

    /// Whether a workspace-relative path falls under an `exclude:` pattern,
    /// taking a pattern as a path component the way rsync and tar do.
    fn is_excluded(&self, relative: &str) -> bool {
        let unprefixed = relative.strip_prefix("./").unwrap_or(relative);
        if unprefixed == PRIVATE_DIR || unprefixed.starts_with(&format!("{}/", PRIVATE_DIR)) {
            return true;
        }
        relative.split('/').any(|component| {
            self.config
                .exclude
                .iter()
                .any(|pattern| pattern == component)
        })
    }

    /// Copy the remote workspace back, without deleting local-only files.
    async fn pull(&self, to: &Path) -> Result<(), WorkflowError> {
        let remote = self.remote();
        match remote.transfer {
            Transfer::Rsync => self.pull_rsync(&remote.workspace, to).await,
            Transfer::Tar => self.pull_tar(&remote, to).await,
        }
    }

    async fn pull_rsync(&self, from: &str, to: &Path) -> Result<(), WorkflowError> {
        let mut args = vec!["--archive".to_string(), "--compress".to_string()];
        args.extend(self.config.rsync_excludes());
        args.extend([
            "-e".to_string(),
            self.config.rsync_shell_arg(),
            format!("{}:{}/", self.config.destination(), from),
            format!("{}/", to.to_string_lossy()),
        ]);
        let (ok, output) = run_tool("rsync", &args).await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "failed to sync the workspace back from {}: {}",
                self.config.host, output
            )));
        }
        Ok(())
    }

    /// Only what changed since the push comes back: the files newer than the
    /// stamp the push left behind.
    async fn pull_tar(&self, remote: &Remote, to: &Path) -> Result<(), WorkflowError> {
        let skip = format!(" -not -path './{}/*'", PRIVATE_DIR);
        let excludes: String = self
            .config
            .exclude
            .iter()
            .map(|pattern| format!(" --exclude={}", shell_quote(pattern)))
            .collect();
        let script = format!(
            "cd {ws} || exit 1\n\
             stamp={stamp}\n\
             list={stamp}-list\n\
             next={stamp}.next\n\
             [ -e \"$stamp\" ] || exit 0\n\
             : > \"$next\"\n\
             find . -type f \\( -newer \"$stamp\" -o -cnewer \"$stamp\" \\){skip} -print0 > \"$list\"\n\
             mv -f \"$next\" \"$stamp\"\n\
             [ -s \"$list\" ] || exit 0\n\
             exec tar -cf - --no-xattrs{excludes} --null -T \"$list\"\n",
            ws = shell_quote(&remote.workspace),
            stamp = SYNC_STAMP,
            skip = skip,
            excludes = excludes,
        );

        let mut ssh = self.spawn_ssh(remote)?;
        let feeder = feed_stdin(&mut ssh, script.into_bytes());
        let mut archive = ssh.stdout.take().expect("ssh stdout is piped");

        // An empty stream means nothing changed; `tar` would call it a
        // corrupt archive.
        let mut head = vec![0u8; 64 * 1024];
        let first = archive
            .read(&mut head)
            .await
            .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;
        if first == 0 {
            let output = ssh
                .wait_with_output()
                .await
                .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;
            let _ = feeder.await;
            if !output.status.success() {
                return Err(WorkflowError::Other(format!(
                    "failed to sync the workspace back from {}: {}",
                    self.config.host,
                    combined_output(&output)
                )));
            }
            return Ok(());
        }

        let mut tar = Command::new("tar")
            .args(["-xf", "-", "-C", &to.to_string_lossy()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| WorkflowError::Other(format!("failed to run tar: {}", e)))?;
        let mut pipe = tar.stdin.take().expect("tar stdin is piped");
        let pump = tokio::spawn(async move {
            if pipe.write_all(&head[..first]).await.is_ok() {
                let _ = tokio::io::copy(&mut archive, &mut pipe).await;
            }
            let _ = pipe.shutdown().await;
        });

        let tar_output = tar
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("tar failed: {}", e)))?;
        let ssh_output = ssh
            .wait_with_output()
            .await
            .map_err(|e| WorkflowError::Other(format!("ssh failed: {}", e)))?;
        let _ = pump.await;
        let _ = feeder.await;

        if !ssh_output.status.success() {
            return Err(WorkflowError::Other(format!(
                "failed to sync the workspace back from {}: {}",
                self.config.host,
                combined_output(&ssh_output)
            )));
        }
        if !tar_output.status.success() {
            return Err(WorkflowError::Other(format!(
                "failed to unpack the workspace from {}: {}",
                self.config.host,
                String::from_utf8_lossy(&tar_output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Fetch the four files a step wrote through, and remove its directory,
    /// in one round trip.
    async fn collect_files(&self, step_dir: &str) -> StepFileContents {
        let script = format!(
            "for f in {names}; do printf '===minact:%s===\\n' \"$f\"; cat {dir}/\"$f\" 2>/dev/null; printf '\\n'; done\n\
             rm -rf {dir}\n",
            names = STEP_FILES.join(" "),
            dir = shell_quote(step_dir),
        );
        match self.run_script_raw(&script).await {
            Ok((_, stdout, _)) => parse_collected(&stdout),
            Err(_) => StepFileContents::default(),
        }
    }
}

/// Reports what the remote is, in `key=value` lines the executor reads back.
const FACTS_SCRIPT: &str = r#"echo minact-shell-ok
echo "os=$(uname -s 2>/dev/null)"
echo "arch=$(uname -m 2>/dev/null)"
if command -v cygpath >/dev/null 2>&1; then echo "home=$(cygpath -m "$HOME")"; else echo "home=$HOME"; fi
for p in pwsh powershell python3 python rsync node; do echo "$p=$(command -v "$p" 2>/dev/null)"; done
"#;

/// The files a step can write back through, in the order they are collected.
const STEP_FILES: [&str; 4] = [
    "github_output",
    "github_env",
    "github_path",
    "github_step_summary",
];

#[async_trait]
impl Executor for SshExecutor {
    fn describe(&self) -> String {
        format!("ssh ({})", self.config.destination())
    }

    fn platform(&self) -> Option<Platform> {
        let remote = self.remote();
        if remote.os.is_empty() {
            return None;
        }
        Some(Platform {
            os: remote.os,
            arch: if remote.arch.is_empty() {
                None
            } else {
                Some(remote.arch)
            },
        })
    }

    async fn prepare(&self, sink: &dyn OutputSink) -> Result<(), WorkflowError> {
        let remote = self.discover().await?;
        *self
            .remote
            .write()
            .expect("remote state lock should not be poisoned") = remote.clone();

        sink.note(
            LogLevel::Info,
            format!(
                "{} is {} {}; steps run through {}",
                self.config.destination(),
                remote.os,
                remote.arch,
                remote.shell
            ),
        )
        .await;

        let (ok, output) = self
            .run_script(&format!(
                "mkdir -p {} {} {}\n",
                shell_quote(&remote.workspace),
                shell_quote(&self.remote_temp()),
                shell_quote(&self.remote_tool_cache())
            ))
            .await?;
        if !ok {
            return Err(WorkflowError::Other(format!(
                "cannot create {} on {}: {}",
                remote.workspace,
                self.config.destination(),
                output
            )));
        }

        if self.config.sync {
            sink.note(
                LogLevel::Info,
                format!(
                    "syncing workspace to {} with {}",
                    self.config.destination(),
                    match remote.transfer {
                        Transfer::Rsync => "rsync",
                        Transfer::Tar => "tar",
                    }
                ),
            )
            .await;
            let stamp = format!("{}/{}", remote.workspace, SYNC_STAMP);
            self.push(&self.workspace, &remote.workspace, Some(&stamp))
                .await?;
        }

        Ok(())
    }

    /// Copy a host directory to the remote and report where it landed.
    ///
    /// Anything already inside the workspace is there by the time a step runs,
    /// so it only needs its path rewritten — except under the runner's own
    /// tree, which the sync leaves out. Something the engine put in the job's
    /// scratch space (the event payload) goes to the same place in the
    /// remote's; everything else — an action out of the cache — lands under
    /// `_actions`.
    async fn provision_dir(
        &self,
        path: &Path,
        sink: &dyn OutputSink,
    ) -> Result<PathBuf, WorkflowError> {
        let private = self.workspace.join(PRIVATE_DIR);
        if path.starts_with(&self.workspace)
            && !path.starts_with(&private)
            && !path.starts_with(&self.host_temp)
        {
            return Ok(PathBuf::from(self.remote_path(path)));
        }

        if let Some(remote) = self.provisioned.lock().await.get(path) {
            return Ok(PathBuf::from(remote));
        }

        let remote = if path.starts_with(&self.host_temp) {
            self.remote_path(path)
        } else {
            self.remote_support_dir(path)
        };
        sink.note(
            LogLevel::Info,
            format!(
                "copying {} to {}",
                path.display(),
                self.config.destination()
            ),
        )
        .await;
        self.push(path, &remote, None).await.map_err(|e| {
            WorkflowError::Other(format!(
                "failed to copy {} to {}: {}",
                path.display(),
                self.config.host,
                e
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
        let remote = self.remote();
        // A per-step directory so concurrent jobs on one host cannot collide.
        let step_dir = format!("{}/step-{}", self.remote_temp(), uuid::Uuid::new_v4());
        let script_path = format!(
            "{}/script.{}",
            step_dir,
            super::script_extension(&request.shell)
        );
        let files = RemoteStepFiles::new(&step_dir);

        let mut env = request.env.clone();
        env.extend(files.file_env());

        let (program, program_args) = request.resolve_command(&request.shell, &script_path);
        let mut program = remote.substitutes.get(&program).cloned().unwrap_or(program);

        // A JavaScript action needs `node`. A remote without one gets a
        // private copy, installed once and kept — the same way GitHub's
        // runners carry their own rather than expecting one on the machine.
        let mut tool_paths: Vec<String> = Vec::new();
        if request.command.is_some() && program == "node" && !remote.has_node {
            let node = self
                .ensure_node(&remote, request.node.unwrap_or(DEFAULT_NODE_MAJOR), sink)
                .await
                .map_err(|e| WorkflowError::StepFailed(request.step_name.clone(), e.to_string()))?;
            if let Some(dir) = Path::new(&node).parent() {
                tool_paths.push(dir.to_string_lossy().replace('\\', "/"));
            }
            program = node;
        }

        // One round trip stages the script, sets the scene and runs it. The
        // script body arrives as a quoted heredoc, so nothing in it is
        // interpreted before its own interpreter sees it.
        let delimiter = format!("MINACT_EOF_{}", uuid::Uuid::new_v4().simple());
        let mut body = request.script.clone();
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        let session = format!(
            "mkdir -p {dir} && : > {out} && : > {envf} && : > {path} && : > {summary} || exit 1\n\
             cat > {script} <<'{delim}'\n{body}{delim}\n\
             {prelude}",
            dir = shell_quote(&step_dir),
            out = shell_quote(&files.output),
            envf = shell_quote(&files.env),
            path = shell_quote(&files.path),
            summary = shell_quote(&files.summary),
            script = shell_quote(&script_path),
            delim = delimiter,
            body = body,
            prelude = self.build_step_script(&request, &env, &program, &program_args, &tool_paths),
        );

        let mut child = self
            .spawn_ssh(&remote)
            .map_err(|e| WorkflowError::StepFailed(request.step_name.clone(), format!("{}", e)))?;
        let feeder = feed_stdin(&mut child, session.into_bytes());

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
        let _ = feeder.await;

        let contents = self.collect_files(&step_dir).await;

        Ok(StepOutcome {
            success,
            status,
            cancelled,
            files: contents,
        })
    }

    async fn sync_back(&self, sink: &dyn OutputSink) -> Result<(), WorkflowError> {
        if !self.config.sync {
            return Ok(());
        }
        sink.note(
            LogLevel::Info,
            format!("syncing changes back from {}", self.config.destination()),
        )
        .await;
        self.pull(&self.workspace.clone()).await
    }

    async fn sync_forward(
        &self,
        since: SystemTime,
        _sink: &dyn OutputSink,
    ) -> Result<(), WorkflowError> {
        if !self.config.sync {
            return Ok(());
        }
        let remote = self.remote();
        match remote.transfer {
            // rsync works out the difference itself; without `--delete` this
            // is additive, which is what a mid-job push has to be.
            Transfer::Rsync => {
                let mut args = vec!["--archive".to_string(), "--compress".to_string()];
                args.extend(self.config.rsync_excludes());
                args.extend([
                    "-e".to_string(),
                    self.config.rsync_shell_arg(),
                    format!("{}/", self.workspace.to_string_lossy()),
                    format!("{}:{}/", self.config.destination(), remote.workspace),
                ]);
                let (ok, output) = run_tool("rsync", &args).await?;
                if !ok {
                    return Err(WorkflowError::Other(format!(
                        "failed to sync changes to {}: {}",
                        self.config.host, output
                    )));
                }
                Ok(())
            }
            Transfer::Tar => self.push_changes_tar(&remote, since).await,
        }
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
        if let Err(e) = self.pull(&self.workspace.clone()).await {
            sink.note(LogLevel::Warn, format!("{}", e)).await;
        }
    }
}

/// Write `input` to the child's stdin on its own task, then close it.
///
/// A remote that exits early closes the pipe; that surfaces through the exit
/// status, not as a write error worth reporting.
fn feed_stdin(child: &mut Child, input: Vec<u8>) -> tokio::task::JoinHandle<()> {
    let stdin = child.stdin.take();
    tokio::spawn(async move {
        if let Some(mut pipe) = stdin {
            let _ = pipe.write_all(&input).await;
            let _ = pipe.shutdown().await;
        }
    })
}

/// Whether a name is one a POSIX shell can `export`.
fn is_shell_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn combined_output(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim());
    }
    text
}

/// How to start a `tar -c` on this machine for a remote to unpack: no
/// extended attributes, no BSD file flags, nothing the other end's tar might
/// not know. bsdtar (macOS) writes `SCHILY.fflags` headers for files with
/// flags like `hidden`, and GNU tar on the remote stops on them.
async fn local_tar_create_args(remote: &Remote) -> Vec<String> {
    let mut args = vec![
        "-cf".to_string(),
        "-".to_string(),
        "--no-xattrs".to_string(),
    ];
    if local_tar_is_bsd().await {
        args.push("--no-fflags".to_string());
        args.push("--no-acls".to_string());
    }
    // Windows cannot generally create symlinks, so send what they point at.
    if remote.is_windows() {
        args.push("-h".to_string());
    }
    args
}

async fn local_tar_is_bsd() -> bool {
    static BSD: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *BSD.get_or_init(|| async {
        matches!(
            run_tool("tar", &["--version".to_string()]).await,
            Ok((true, output)) if output.contains("bsdtar")
        )
    })
    .await
}

async fn local_has_rsync() -> bool {
    matches!(
        run_tool("rsync", &["--version".to_string()]).await,
        Ok((true, _))
    )
}

/// Read the login-probe output: which shell expanded what.
fn classify_login(output: &str) -> Option<Login> {
    let line = output.lines().find(|line| line.contains("minact-ok"))?;
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let start = tokens.iter().position(|token| *token == "minact-ok")?;
    Some(match &tokens[start + 1..] {
        [os, ..] if *os == "Windows_NT" => Login::Cmd,
        ["%OS%", kernel, ..] if !kernel.is_empty() => Login::Posix,
        // Neither expansion happened: PowerShell echoed the literals and
        // complained about `uname` on stderr.
        _ => Login::PowerShell,
    })
}

/// `C:\Program Files\Git\cmd\git.exe` → `C:/Program Files/Git/bin/bash.exe`.
fn git_bash_from_git(git: &str) -> Option<String> {
    let git = git.replace('\\', "/");
    let lower = git.to_ascii_lowercase();
    // Longest suffixes first: `/bin/git.exe` is also the tail of the mingw ones.
    let root_len = [
        "/mingw64/bin/git.exe",
        "/mingw32/bin/git.exe",
        "/cmd/git.exe",
        "/bin/git.exe",
    ]
    .iter()
    .find_map(|suffix| lower.strip_suffix(suffix).map(|root| root.len()))?;
    Some(format!("{}/bin/bash.exe", &git[..root_len]))
}

/// Forward slashes, no trailing one.
/// `base/relative` spelled for the remote: `/`-separated, and just `base`
/// when there is nothing to add.
fn join_remote(base: &str, relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        return base.to_string();
    }
    format!("{}/{}", base, relative.to_string_lossy().replace('\\', "/"))
}

fn normalise_remote_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// Expand a leading `~` against the remote's home directory.
///
/// Quoting keeps a `~` from expanding on the remote, and quoting is not
/// optional, so it is expanded here instead.
fn resolve_workspace(configured: &str, home: &str) -> Result<String, WorkflowError> {
    let path = normalise_remote_path(configured);
    let expanded = if path == "~" || path.starts_with("~/") {
        if home.is_empty() {
            return Err(WorkflowError::Other(format!(
                "cannot resolve `{}`: the remote did not report a home directory",
                configured
            )));
        }
        format!("{}{}", normalise_remote_path(home), &path[1..])
    } else {
        path
    };
    Ok(normalise_remote_path(&expanded))
}

/// `uname` output spelled the way `runner.os` and `runner.arch` are.
fn platform_names(kernel: &str, machine: &str) -> (String, String) {
    let os = if kernel.starts_with("Darwin") {
        "macOS"
    } else if kernel == "Linux" {
        "Linux"
    } else if ["MINGW", "MSYS", "CYGWIN"]
        .iter()
        .any(|prefix| kernel.starts_with(prefix))
    {
        "Windows"
    } else {
        kernel
    };
    let arch = match machine {
        "x86_64" | "amd64" => "X64",
        "aarch64" | "arm64" => "ARM64",
        "i686" | "i386" | "x86" => "X86",
        other => other,
    };
    (os.to_string(), arch.to_string())
}

/// Split the output of the collection script back into the four files.
fn parse_collected(bytes: &[u8]) -> StepFileContents {
    let marker = |name: &str| format!("===minact:{}===\n", name).into_bytes();
    let mut sections: Vec<String> = Vec::with_capacity(STEP_FILES.len());
    let mut cursor = 0;
    for (index, name) in STEP_FILES.iter().enumerate() {
        let Some(start) = find_bytes(bytes, &marker(name), cursor) else {
            sections.push(String::new());
            continue;
        };
        let start = start + marker(name).len();
        let end = STEP_FILES
            .get(index + 1)
            .and_then(|next| find_bytes(bytes, &marker(next), start))
            .unwrap_or(bytes.len());
        sections.push(decode_text(&bytes[start..end]));
        cursor = end;
    }
    StepFileContents {
        output: sections[0].clone(),
        env: sections[1].clone(),
        path: sections[2].clone(),
        summary: sections[3].clone(),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

/// A file's bytes as text, whatever Windows PowerShell did to them.
///
/// `>>` in Windows PowerShell writes UTF-16LE with a byte-order mark and CRLF
/// line endings, which is also what a workflow written for GitHub's Windows
/// runners produces there.
fn decode_text(bytes: &[u8]) -> String {
    // The newline the collection script printed after the file.
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text = if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        String::from_utf16_lossy(&utf16_units(body, true))
    } else if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        String::from_utf16_lossy(&utf16_units(body, false))
    } else {
        let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        String::from_utf8_lossy(body).to_string()
    };
    text.replace("\r\n", "\n").trim().to_string()
}

fn utf16_units(body: &[u8], little_endian: bool) -> Vec<u16> {
    (0..body.len() / 2)
        .map(|i| {
            let pair = [body[2 * i], body[2 * i + 1]];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect()
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
            PathBuf::from("/home/me/project/.minact/_work/_temp/build-a1b2c3"),
        )
    }

    #[test]
    fn the_job_temp_maps_to_the_remotes_own() {
        let executor = executor();
        assert_eq!(
            executor.remote_path(Path::new(
                "/home/me/project/.minact/_work/_temp/build-a1b2c3"
            )),
            "/srv/work/.minact/_work/_temp"
        );
        assert_eq!(
            executor.remote_path(Path::new(
                "/home/me/project/.minact/_work/_temp/build-a1b2c3/_github_workflow/event.json"
            )),
            "/srv/work/.minact/_work/_temp/_github_workflow/event.json"
        );
        // The rest of the workspace keeps its relative place.
        assert_eq!(
            executor.remote_path(Path::new("/home/me/project/.minact/config.yml")),
            "/srv/work/.minact/config.yml"
        );
    }

    #[test]
    fn the_private_tree_is_never_synced() {
        let config = executor().config;
        assert!(config
            .rsync_excludes()
            .contains(&"--exclude=/.minact/_work".to_string()));
        assert!(config
            .tar_excludes()
            .contains(&"--exclude=./.minact/_work".to_string()));
        let executor = executor();
        assert!(executor.is_excluded("./.minact/_work/_temp/x/script.sh"));
        assert!(executor.is_excluded(".minact/_work"));
        assert!(!executor.is_excluded("./.minact/config.yml"));
        assert!(!executor.is_excluded("./src/_work/file.rs"));
    }

    fn windows_remote(login: Login) -> Remote {
        Remote {
            login,
            shell: "C:/Program Files/Git/bin/bash.exe".to_string(),
            workspace: "C:/work".to_string(),
            os: "Windows".to_string(),
            arch: "X64".to_string(),
            has_node: false,
            substitutes: HashMap::new(),
            transfer: Transfer::Tar,
        }
    }

    fn request(working_directory: &str) -> StepRequest {
        StepRequest {
            step_name: "test".to_string(),
            script: "echo hi".to_string(),
            shell: "bash".to_string(),
            working_directory: PathBuf::from(working_directory),
            env: HashMap::new(),
            extra_paths: Vec::new(),
            node: None,
            runner_temp: PathBuf::from("/tmp"),
            command: None,
        }
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
    fn a_windows_workspace_maps_with_forward_slashes() {
        let executor = SshExecutor::new(
            SshConfig {
                host: "win".to_string(),
                remote_workspace: "C:\\minact\\work\\".to_string(),
                ..Default::default()
            },
            PathBuf::from("/home/me/project"),
            PathBuf::from("/home/me/project/.minact/_work/_temp/build-a1b2c3"),
        );
        assert_eq!(
            executor.remote_path(Path::new("/home/me/project/src")),
            "C:/minact/work/src"
        );
    }

    #[test]
    fn exports_the_environment_safely() {
        let request = request("/home/me/project/sub");
        let env = HashMap::from([
            ("SAFE".to_string(), "value".to_string()),
            ("HOSTILE".to_string(), "'; rm -rf /; echo '".to_string()),
        ]);

        let script = executor().build_step_script(
            &request,
            &env,
            "bash",
            &["-e".to_string(), "/srv/work/.minact-temp/s.sh".to_string()],
            &[],
        );

        assert!(script.contains("export SAFE=value"));
        assert!(script.contains("cd /srv/work/sub"));
        assert!(script.ends_with("exec bash -e /srv/work/.minact-temp/s.sh\n"));
        // The injected command must be inside quotes, not executable.
        assert!(!script.contains("export HOSTILE='; rm -rf /; echo '\n"));
        assert!(script.contains(r"'\''"));
    }

    #[test]
    fn names_a_shell_cannot_export_go_through_env() {
        // GitHub spells action inputs `INPUT_NODE-VERSION`: a fine
        // environment variable, not a shell identifier.
        let env = HashMap::from([
            ("INPUT_NODE-VERSION".to_string(), "20".to_string()),
            ("PLAIN".to_string(), "x".to_string()),
        ]);
        let script = executor().build_step_script(
            &request("/home/me/project"),
            &env,
            "node",
            &["/srv/work/a/index.js".to_string()],
            &[],
        );
        assert!(script.contains("export PLAIN=x\n"));
        assert!(!script.contains("export INPUT_NODE-VERSION"));
        assert!(
            script.ends_with("exec env INPUT_NODE-VERSION=20 node /srv/work/a/index.js\n"),
            "{}",
            script
        );
        // A value a shell would otherwise interpret is quoted on the way.
        let env = HashMap::from([("INPUT_A-B".to_string(), "x y $HOME".to_string())]);
        let script =
            executor().build_step_script(&request("/home/me/project"), &env, "node", &[], &[]);
        assert!(
            script.ends_with("exec env 'INPUT_A-B=x y $HOME' node\n"),
            "{}",
            script
        );
        assert!(is_shell_identifier("_A1"));
        assert!(!is_shell_identifier("1A"));
        assert!(!is_shell_identifier(""));
    }

    #[test]
    fn windows_path_entries_are_converted_before_joining() {
        let executor = executor();
        *executor.remote.write().unwrap() = windows_remote(Login::Cmd);
        let mut request = request("/home/me/project");
        request.extra_paths = vec!["/home/me/project/bin".to_string()];
        let script = executor.build_step_script(
            &request,
            &HashMap::new(),
            "bash",
            &[],
            &["C:/Users/me/.minact/tools/node-v20".to_string()],
        );
        // A `C:/...` entry would be split at its colon; cygpath makes it `/c/...`.
        assert!(
            script.contains(
                "export PATH=\"$(cygpath -u C:/work/bin)\":\"$(cygpath -u C:/Users/me/.minact/tools/node-v20)\":\"$PATH\"\n"
            ),
            "{}",
            script
        );
    }

    #[test]
    fn the_remote_sees_its_own_paths() {
        let mut request = request("/home/me/project");
        request.extra_paths = vec!["/home/me/project/bin".to_string()];
        let env = HashMap::from([
            (
                "GITHUB_WORKSPACE".to_string(),
                "/home/me/project".to_string(),
            ),
            (
                "RUNNER_TEMP".to_string(),
                "/home/me/project/.minact/_work/_temp/build-a1b2c3".to_string(),
            ),
            (
                "GITHUB_EVENT_PATH".to_string(),
                "/home/me/project/.minact/_work/_temp/build-a1b2c3/_github_workflow/event.json"
                    .to_string(),
            ),
            // Moved by the environment on the host; the remote's is its own.
            (
                "RUNNER_TOOL_CACHE".to_string(),
                "/home/me/.cache/minact-tools".to_string(),
            ),
            ("PLAIN".to_string(), "true".to_string()),
            // A different directory that merely shares a prefix.
            ("OTHER".to_string(), "/home/me/project-2/x".to_string()),
        ]);

        let script = executor().build_step_script(
            &request,
            &env,
            "bash",
            &[],
            &["/srv/tools/node/bin".to_string()],
        );

        assert!(
            script.contains("export GITHUB_WORKSPACE=/srv/work\n"),
            "{}",
            script
        );
        assert!(script.contains("export RUNNER_TEMP=/srv/work/.minact/_work/_temp\n"));
        assert!(script.contains(
            "export GITHUB_EVENT_PATH=/srv/work/.minact/_work/_temp/_github_workflow/event.json\n"
        ));
        assert!(script.contains("export RUNNER_TOOL_CACHE=/srv/work/.minact/_work/_tool\n"));
        assert!(script.contains("export PLAIN=true\n"));
        assert!(script.contains("export OTHER=/home/me/project-2/x\n"));
        // The remote keeps its own PATH; the step's addition goes in front,
        // then what minact provided.
        assert!(
            script.contains("export PATH=/srv/work/bin:/srv/tools/node/bin:\"$PATH\"\n"),
            "{}",
            script
        );
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

    #[test]
    fn recognises_the_login_shell_from_what_it_expanded() {
        // cmd.exe expands %OS% and leaves $(...) alone.
        assert_eq!(
            classify_login("minact-ok Windows_NT $(uname -s)"),
            Some(Login::Cmd)
        );
        // A POSIX shell does the opposite.
        assert_eq!(classify_login("minact-ok %OS% Darwin"), Some(Login::Posix));
        assert_eq!(classify_login("minact-ok %OS% Linux\n"), Some(Login::Posix));
        // PowerShell expands neither and complains about uname.
        assert_eq!(
            classify_login("minact-ok %OS%\nuname : The term 'uname' is not recognized"),
            Some(Login::PowerShell)
        );
        // No echo at all means nothing answered.
        assert_eq!(classify_login("Permission denied (publickey)."), None);
    }

    #[test]
    fn invokes_the_shell_the_way_the_login_shell_expects() {
        let posix = Remote {
            shell: "sh".to_string(),
            ..windows_remote(Login::Posix)
        };
        assert_eq!(SshExecutor::shell_invocation(&posix), "sh -s");
        assert_eq!(
            SshExecutor::shell_invocation(&windows_remote(Login::Cmd)),
            "\"C:\\Program Files\\Git\\bin\\bash.exe\" -s"
        );
        assert_eq!(
            SshExecutor::shell_invocation(&windows_remote(Login::PowerShell)),
            "& 'C:\\Program Files\\Git\\bin\\bash.exe' -s"
        );
    }

    #[test]
    fn finds_bash_next_to_git() {
        assert_eq!(
            git_bash_from_git("C:\\Program Files\\Git\\cmd\\git.exe").as_deref(),
            Some("C:/Program Files/Git/bin/bash.exe")
        );
        assert_eq!(
            git_bash_from_git("D:/tools/Git/mingw64/bin/git.exe").as_deref(),
            Some("D:/tools/Git/bin/bash.exe")
        );
        assert_eq!(git_bash_from_git("INFO: Could not find files"), None);
    }

    #[test]
    fn resolves_a_tilde_against_the_remote_home() {
        assert_eq!(
            resolve_workspace("~/minact-workspace", "C:/Users/me").unwrap(),
            "C:/Users/me/minact-workspace"
        );
        assert_eq!(resolve_workspace("~", "/home/me/").unwrap(), "/home/me");
        assert_eq!(
            resolve_workspace("/srv/work/", "/home/me").unwrap(),
            "/srv/work"
        );
        assert_eq!(
            resolve_workspace("C:\\minact\\work", "").unwrap(),
            "C:/minact/work"
        );
        assert!(resolve_workspace("~/x", "").is_err());
    }

    #[test]
    fn spells_the_platform_the_way_github_does() {
        assert_eq!(
            platform_names("MINGW64_NT-10.0-26200", "x86_64"),
            ("Windows".to_string(), "X64".to_string())
        );
        assert_eq!(
            platform_names("Darwin", "arm64"),
            ("macOS".to_string(), "ARM64".to_string())
        );
        assert_eq!(
            platform_names("Linux", "aarch64"),
            ("Linux".to_string(), "ARM64".to_string())
        );
    }

    #[test]
    fn splits_collected_files_and_decodes_what_powershell_wrote() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"===minact:github_output===\n");
        // UTF-16LE with a BOM and CRLF, as Windows PowerShell's `>>` writes.
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        for unit in "ver=1.2\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(b"===minact:github_env===\n");
        bytes.extend_from_slice(b"\xEF\xBB\xBFKEY=value\r\n\n");
        bytes.extend_from_slice(b"===minact:github_path===\n\n");
        bytes.extend_from_slice(b"===minact:github_step_summary===\n# done\n\n");

        let files = parse_collected(&bytes);
        assert_eq!(files.output, "ver=1.2");
        assert_eq!(files.env, "KEY=value");
        assert_eq!(files.path, "");
        assert_eq!(files.summary, "# done");
    }

    #[test]
    fn a_missing_marker_leaves_that_file_empty() {
        let files = parse_collected(b"garbage");
        assert_eq!(files.output, "");
        assert_eq!(files.summary, "");
    }
}
