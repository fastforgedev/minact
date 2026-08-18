//! Fetching remote actions, and keeping them.
//!
//! An action referenced as `owner/repo@ref` has to exist on disk before it can
//! run. The store shallow-clones it with `git` — already a prerequisite for
//! `actions/checkout`, and it brings the user's existing credential setup
//! along for private repositories — into a content-addressed cache under
//! `~/.minact/actions`, so the second run of a workflow fetches nothing.
//!
//! A clone lands in a staging directory and is renamed into place, so an
//! interrupted fetch cannot leave a half-populated cache entry behind.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::logging::LogLevel;
use crate::types::WorkflowError;

/// Where actions are cached when the caller does not say.
pub fn default_cache_dir() -> Result<PathBuf, WorkflowError> {
    let home = dirs::home_dir()
        .ok_or_else(|| WorkflowError::Other("cannot find the home directory".to_string()))?;
    Ok(home.join(".minact").join("actions"))
}

/// The git host actions are fetched from, `https://github.com` unless
/// `GITHUB_SERVER_URL` says otherwise — which is how a GitHub Enterprise
/// install is pointed at.
fn server_url() -> String {
    std::env::var("GITHUB_SERVER_URL")
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| "https://github.com".to_string())
}

/// A local cache of fetched actions.
#[derive(Debug, Clone)]
pub struct ActionStore {
    root: PathBuf,
    /// Re-fetch even when the cache already has the ref.
    refresh: bool,
}

impl ActionStore {
    /// A store rooted at `~/.minact/actions`.
    pub fn new() -> Result<Self, WorkflowError> {
        Ok(Self::with_root(default_cache_dir()?))
    }

    /// A store rooted wherever the caller says.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            refresh: false,
        }
    }

    /// Discard a cached copy before using it, so a moving ref such as a branch
    /// picks up new commits.
    pub fn refreshing(mut self, refresh: bool) -> Self {
        self.refresh = refresh;
        self
    }

    /// The cache root, which is the directory job containers need mounted.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Make `owner/repo@git_ref` available locally and return the directory
    /// holding its `action.yml`, descending into `path` when the reference
    /// named a sub-directory.
    pub async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        path: Option<&str>,
        git_ref: &str,
        report: &mut (dyn FnMut(LogLevel, String) + Send),
    ) -> Result<PathBuf, WorkflowError> {
        let checkout = self.checkout_dir(owner, repo, git_ref);

        if checkout.is_dir() && self.refresh {
            std::fs::remove_dir_all(&checkout)?;
        }

        if !checkout.is_dir() {
            report(
                LogLevel::Info,
                format!("fetching {}/{}@{}", owner, repo, git_ref),
            );
            self.clone_into(owner, repo, git_ref, &checkout).await?;
        }

        let action_dir = match path {
            Some(path) => checkout.join(path),
            None => checkout.clone(),
        };

        if !action_dir.is_dir() {
            return Err(WorkflowError::Other(format!(
                "{}/{}@{} has no directory `{}`",
                owner,
                repo,
                git_ref,
                path.unwrap_or(".")
            )));
        }

        // `..` is rejected when the reference is parsed, but a symlink in the
        // fetched repository could still point outside it.
        let resolved = action_dir.canonicalize()?;
        let root = checkout.canonicalize()?;
        if !resolved.starts_with(&root) {
            return Err(WorkflowError::Other(format!(
                "`{}` in {}/{}@{} resolves outside the action",
                path.unwrap_or("."),
                owner,
                repo,
                git_ref
            )));
        }

        Ok(resolved)
    }

    /// Where a given reference is cached.
    ///
    /// The ref is both slugged, so the path stays readable, and hashed, so two
    /// refs that slug the same (`v1.0` and `v1/0`) cannot collide.
    fn checkout_dir(&self, owner: &str, repo: &str, git_ref: &str) -> PathBuf {
        self.root
            .join(sanitize(owner))
            .join(sanitize(repo))
            .join(format!("{}-{}", sanitize(git_ref), short_hash(git_ref)))
    }

    async fn clone_into(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        destination: &Path,
    ) -> Result<(), WorkflowError> {
        let parent = destination
            .parent()
            .ok_or_else(|| WorkflowError::Other("invalid action cache path".to_string()))?;
        std::fs::create_dir_all(parent)?;

        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(parent)?;
        let work = staging.path().join("action");
        let url = format!("{}/{}/{}", server_url(), owner, repo);

        let credentials = Credentials::create(staging.path(), &url, token_from_env().as_deref())?;
        let errors = self
            .try_strategies(&url, git_ref, &work, credentials.as_ref())
            .await?;

        if let Some(errors) = errors {
            return Err(WorkflowError::Other(format!(
                "could not fetch {}/{}@{}:\n{}",
                owner,
                repo,
                git_ref,
                errors.trim_end()
            )));
        }

        // The cache mirrors what GitHub hands a runner: the tree at that ref,
        // without the repository's history.
        std::fs::remove_dir_all(work.join(".git")).ok();

        match std::fs::rename(&work, destination) {
            Ok(()) => Ok(()),
            // Another job fetched the same action while this one was cloning.
            // Its copy is the same tree, so keep it and drop ours.
            Err(_) if destination.is_dir() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Try each way of getting a ref, returning the collected failures when
    /// none of them worked.
    async fn try_strategies(
        &self,
        url: &str,
        git_ref: &str,
        work: &Path,
        credentials: Option<&Credentials>,
    ) -> Result<Option<String>, WorkflowError> {
        let work_str = work.to_string_lossy().to_string();
        let mut errors = String::new();

        // A tag or branch: one shallow clone, and by far the common case.
        let shallow = vec![
            "clone".to_string(),
            "--depth".to_string(),
            "1".to_string(),
            "--branch".to_string(),
            git_ref.to_string(),
            url.to_string(),
            work_str.clone(),
        ];
        match run_git(None, &shallow, credentials).await? {
            Ok(()) => return Ok(None),
            Err(e) => errors.push_str(&e),
        }
        std::fs::remove_dir_all(work).ok();

        // A commit SHA. `--branch` cannot name one, but most hosts allow
        // fetching it directly, which still avoids downloading the history.
        std::fs::create_dir_all(work)?;
        let by_sha = async {
            for step in [
                vec!["init".to_string(), "--quiet".to_string()],
                vec![
                    "remote".to_string(),
                    "add".to_string(),
                    "origin".to_string(),
                    url.to_string(),
                ],
                vec![
                    "fetch".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    "origin".to_string(),
                    git_ref.to_string(),
                ],
                vec![
                    "checkout".to_string(),
                    "--quiet".to_string(),
                    "FETCH_HEAD".to_string(),
                ],
            ] {
                if let Err(e) = run_git(Some(work), &step, credentials).await? {
                    return Ok(Err(e));
                }
            }
            Ok::<Result<(), String>, WorkflowError>(Ok(()))
        }
        .await?;
        match by_sha {
            Ok(()) => return Ok(None),
            Err(e) => errors.push_str(&e),
        }
        std::fs::remove_dir_all(work).ok();

        // Whatever is left — an old host, or a SHA that is not a fetch tip.
        // A full clone always works, so it is worth the download.
        let full = vec!["clone".to_string(), url.to_string(), work_str];
        match run_git(None, &full, credentials).await? {
            Ok(()) => {}
            Err(e) => {
                errors.push_str(&e);
                return Ok(Some(errors));
            }
        }
        match run_git(
            Some(work),
            &[
                "checkout".to_string(),
                "--quiet".to_string(),
                git_ref.to_string(),
            ],
            credentials,
        )
        .await?
        {
            Ok(()) => Ok(None),
            Err(e) => {
                errors.push_str(&e);
                Ok(Some(errors))
            }
        }
    }
}

/// A token for a private action, kept in a file rather than on the command
/// line: arguments are visible to every process on the machine.
struct Credentials {
    file: PathBuf,
}

/// The token to authenticate a fetch with, if the environment offers one.
fn token_from_env() -> Option<String> {
    std::env::var("MINACT_ACTIONS_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|token| !token.trim().is_empty())
}

impl Credentials {
    /// Write a credential file for `token`, if there is one.
    fn create(dir: &Path, url: &str, token: Option<&str>) -> Result<Option<Self>, WorkflowError> {
        let Some(token) = token else {
            return Ok(None);
        };

        // git matches credentials by scheme and host, so only the origin of
        // the URL being cloned belongs in the file.
        let origin = match url.split_once("://") {
            Some((scheme, rest)) => {
                let host = rest.split('/').next().unwrap_or_default();
                format!("{}://x-access-token:{}@{}", scheme, token.trim(), host)
            }
            None => return Ok(None),
        };

        let file = dir.join("git-credentials");
        std::fs::write(&file, format!("{}\n", origin))?;
        restrict_to_owner(&file)?;
        Ok(Some(Self { file }))
    }

    fn helper_arg(&self) -> String {
        format!("credential.helper=store --file={}", self.file.display())
    }
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), WorkflowError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<(), WorkflowError> {
    Ok(())
}

/// Run one git command.
///
/// The outer result is whether git could be run at all; the inner one is
/// whether it succeeded, because a failure is often just "this ref is not a
/// branch" and the caller wants to try the next strategy.
async fn run_git(
    cwd: Option<&Path>,
    args: &[String],
    credentials: Option<&Credentials>,
) -> Result<Result<(), String>, WorkflowError> {
    let mut command = Command::new("git");
    // Nothing here can answer a prompt, and a terminal prompt would hang the
    // run rather than fail it.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_ADVICE", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(credentials) = credentials {
        command.arg("-c").arg(credentials.helper_arg());
    }
    command.args(args);

    let output = command.output().await.map_err(|e| {
        WorkflowError::Other(format!(
            "could not run git ({}) — it is required to fetch remote actions",
            e
        ))
    })?;

    if output.status.success() {
        return Ok(Ok(()));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(Err(format!(
        "  git {}: {}\n",
        args.first().map(String::as_str).unwrap_or_default(),
        stderr.trim()
    )))
}

/// Turn one path segment of a reference into something safe to put in a path.
fn sanitize(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A segment of dots would be a path traversal once joined.
    if cleaned.chars().all(|c| c == '.') {
        return format!("-{}", cleaned);
    }
    cleaned
}

fn short_hash(value: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_a_reference_under_a_readable_path() {
        let store = ActionStore::with_root(PathBuf::from("/cache"));
        let dir = store.checkout_dir("actions", "checkout", "v4");
        assert!(dir.starts_with("/cache/actions/checkout"));
        assert!(dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("v4-"));
    }

    #[test]
    fn refs_that_slug_alike_do_not_collide() {
        let store = ActionStore::with_root(PathBuf::from("/cache"));
        assert_ne!(
            store.checkout_dir("o", "r", "v1.0"),
            store.checkout_dir("o", "r", "v1/0")
        );
        // And the same ref is always the same directory, so the cache hits.
        assert_eq!(
            store.checkout_dir("o", "r", "main"),
            store.checkout_dir("o", "r", "main")
        );
    }

    #[test]
    fn sanitizing_cannot_produce_a_traversal() {
        assert_eq!(sanitize("v4"), "v4");
        assert_eq!(sanitize("refs/heads/main"), "refs-heads-main");

        // Separators become ordinary characters, so what went in as several
        // segments comes out as one that cannot climb anywhere.
        for hostile in ["..", ".", "../../etc", "..\\..\\etc", "/", "a/../b"] {
            let cleaned = sanitize(hostile);
            assert_ne!(cleaned, "..", "{} sanitized to a parent segment", hostile);
            assert!(
                !cleaned.contains('/') && !cleaned.contains('\\'),
                "{} sanitized to {}, which is still several segments",
                hostile,
                cleaned
            );
        }

        let store = ActionStore::with_root(PathBuf::from("/cache"));
        let dir = store.checkout_dir("..", "..", "..");
        assert!(dir.starts_with("/cache"));
        assert!(!dir.components().any(|c| c.as_os_str() == ".."));
    }

    #[test]
    fn a_token_lands_in_a_file_and_not_in_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let credentials =
            Credentials::create(dir.path(), "https://github.com/o/r", Some("secret-token"))
                .unwrap()
                .expect("a token was supplied");

        let stored = std::fs::read_to_string(&credentials.file).unwrap();
        assert_eq!(
            stored.trim(),
            "https://x-access-token:secret-token@github.com"
        );
        assert!(!credentials.helper_arg().contains("secret-token"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&credentials.file)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn no_token_means_no_credential_helper() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Credentials::create(dir.path(), "https://github.com/o/r", None)
                .unwrap()
                .is_none()
        );
        // A host that is not a URL has nowhere to scope the credential to.
        assert!(Credentials::create(dir.path(), "github.com/o/r", Some("t"))
            .unwrap()
            .is_none());
    }
}
