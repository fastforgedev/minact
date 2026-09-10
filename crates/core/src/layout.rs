//! The runner's private directory tree: `.minact/_work`.
//!
//! GitHub's runner keeps everything it needs at run time under one anchor,
//! `_work`, and derives the rest from it: `_temp` for a job's scratch files,
//! `_tool` for the tool cache, and so on. minact does the same, inside the
//! project:
//!
//! ```text
//! <workspace>/.minact/_work/
//! ├── .gitignore          # `*`, so the tree never needs ignoring by hand
//! ├── _temp/              # scratch space
//! │   └── <job>-xxxxxx/   # one job's $RUNNER_TEMP; removed when it finishes
//! │       └── minact-step-*/  # a step's script and $GITHUB_* files
//! └── _tool/              # $RUNNER_TOOL_CACHE, unless the variable is set
//! ```
//!
//! Keeping the tree under the workspace is what lets a job container reach it
//! without a mount of its own: the workspace is bind-mounted at the same path
//! on both sides, so `$RUNNER_TEMP` means the same thing inside. The
//! underscore prefix marks a directory as the runner's, not the user's — the
//! same convention as `_work` itself — and is what a sync to a remote runner
//! leaves out.
//!
//! A job's temp directory has a random suffix rather than a fixed name because
//! two runs can share a workspace: Studio starts one per request. A fixed
//! `_temp` wiped at every job start, the way GitHub's runner does it, would
//! delete the scripts of a job still running in the other.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::types::WorkflowError;

/// The directory under `.minact` that holds the tree.
pub const WORK_DIR_NAME: &str = "_work";

/// A scratch directory whose contents are older than this at the start of a
/// run are left over from a run that did not get to clean up, and go.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The `.minact/_work` tree for one workspace.
#[derive(Debug, Clone)]
pub struct WorkDir {
    root: PathBuf,
}

impl WorkDir {
    /// The tree at its default place, `<workspace>/.minact/_work`.
    pub fn for_workspace(workspace: &Path) -> Self {
        Self::at(workspace.join(".minact").join(WORK_DIR_NAME))
    }

    /// The tree rooted wherever the caller says — the runner's `--work`.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The anchor everything else hangs off.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where every job's scratch directory is created.
    pub fn temp(&self) -> PathBuf {
        self.root.join("_temp")
    }

    /// The tool cache. `RUNNER_TOOL_CACHE` in the environment overrides it,
    /// the way it does on GitHub's runner, so one cache can serve every
    /// project.
    pub fn tool_cache(&self) -> PathBuf {
        std::env::var_os("RUNNER_TOOL_CACHE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.join("_tool"))
    }

    /// Make the tree ready for a run: the directories exist, the tree
    /// ignores itself, and scratch directories a crashed run left behind
    /// are gone.
    pub fn prepare(&self) -> Result<(), WorkflowError> {
        std::fs::create_dir_all(self.temp())?;
        std::fs::create_dir_all(self.tool_cache())?;

        let ignore = self.root.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "# Created by minact; the runner's own files.\n*\n")?;
        }

        self.sweep_stale_temp();
        Ok(())
    }

    /// A fresh `$RUNNER_TEMP` for a job. Removed when the guard is dropped,
    /// which is when the job is over.
    pub fn job_temp(&self, instance_id: &str) -> Result<JobTemp, WorkflowError> {
        let temp = self.temp();
        std::fs::create_dir_all(&temp)?;
        let dir = tempfile::Builder::new()
            .prefix(&format!("{}-", sanitize(instance_id)))
            .rand_bytes(6)
            .tempdir_in(&temp)?;
        Ok(JobTemp { dir })
    }

    /// Remove scratch directories no run can still be using.
    ///
    /// Only age can tell: a directory's mtime moves whenever a step session
    /// is created or removed inside it, so one that has not changed for a
    /// day belongs to a run that was killed. Best effort throughout — a
    /// directory that cannot be removed is not worth failing the run over.
    fn sweep_stale_temp(&self) {
        let Ok(entries) = std::fs::read_dir(self.temp()) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .map(|age| age > STALE_AFTER)
                .unwrap_or(false);
            if stale && path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// One job's scratch directory, removed on drop.
#[derive(Debug)]
pub struct JobTemp {
    dir: tempfile::TempDir,
}

impl JobTemp {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// A job instance id as a directory name component: `build (ubuntu, 1.75)`
/// becomes `build_ubuntu_1.75`.
fn sanitize(instance_id: &str) -> String {
    let mut out = String::with_capacity(instance_id.len());
    let mut pending_separator = false;
    for ch in instance_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' {
            if pending_separator && !out.is_empty() {
                out.push('_');
            }
            pending_separator = false;
            out.push(ch);
        } else {
            pending_separator = true;
        }
    }
    if out.is_empty() {
        "job".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hangs_everything_off_the_workspace() {
        let work = WorkDir::for_workspace(Path::new("/srv/project"));
        assert_eq!(
            work.root(),
            Path::new("/srv/project/.minact/_work"),
            "the anchor is inside the project's .minact"
        );
        assert_eq!(work.temp(), Path::new("/srv/project/.minact/_work/_temp"));
        assert!(
            work.tool_cache().starts_with(work.root())
                || std::env::var_os("RUNNER_TOOL_CACHE").is_some(),
            "the tool cache is under the anchor unless the environment says otherwise"
        );
    }

    #[test]
    fn prepare_creates_the_tree_and_ignores_it() {
        let workspace = tempfile::tempdir().unwrap();
        let work = WorkDir::for_workspace(workspace.path());
        work.prepare().unwrap();

        assert!(work.temp().is_dir());
        assert!(
            work.root().join("_tool").is_dir() || std::env::var_os("RUNNER_TOOL_CACHE").is_some()
        );
        let ignore = std::fs::read_to_string(work.root().join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|line| line == "*"));

        // Preparing twice is fine, and does not rewrite the ignore file.
        std::fs::write(work.root().join(".gitignore"), "custom\n").unwrap();
        work.prepare().unwrap();
        assert_eq!(
            std::fs::read_to_string(work.root().join(".gitignore")).unwrap(),
            "custom\n"
        );
    }

    #[test]
    fn a_job_gets_its_own_temp_which_goes_with_it() {
        let workspace = tempfile::tempdir().unwrap();
        let work = WorkDir::for_workspace(workspace.path());

        let temp = work.job_temp("build (ubuntu, 1.75)").unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.is_dir());
        assert!(path.starts_with(work.temp()));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("build_ubuntu_1.75-"),
            "the directory is named after the job: {}",
            name
        );

        // Two jobs with the same id do not share a directory.
        let other = work.job_temp("build (ubuntu, 1.75)").unwrap();
        assert_ne!(other.path(), path);

        drop(temp);
        assert!(!path.exists(), "the job's temp is removed with the job");
        assert!(other.path().is_dir(), "...and only the job's own");
    }

    #[test]
    fn stale_scratch_directories_are_swept_and_fresh_ones_kept() {
        let workspace = tempfile::tempdir().unwrap();
        let work = WorkDir::for_workspace(workspace.path());
        std::fs::create_dir_all(work.temp()).unwrap();

        let stale = work.temp().join("old-abc123");
        std::fs::create_dir(&stale).unwrap();
        let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        filetime_set(&stale, two_days_ago);

        let fresh = work.temp().join("new-abc123");
        std::fs::create_dir(&fresh).unwrap();

        work.prepare().unwrap();
        assert!(
            !stale.exists(),
            "a day-old scratch directory is left over from a dead run"
        );
        assert!(
            fresh.exists(),
            "a recent one may belong to a run in progress"
        );
    }

    /// Set a directory's mtime without pulling in a crate for it.
    fn filetime_set(path: &Path, when: SystemTime) {
        let file = std::fs::File::open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn sanitizes_instance_ids() {
        assert_eq!(sanitize("build"), "build");
        assert_eq!(sanitize("build (ubuntu, 1.75)"), "build_ubuntu_1.75");
        assert_eq!(
            sanitize("test (macos-latest, node 20)"),
            "test_macos-latest_node_20"
        );
        assert_eq!(sanitize("(((("), "job");
    }
}
