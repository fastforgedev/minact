//! Action trait and built-in action registry.
//!
//! Actions are reusable units of work that can be referenced via `uses:` in
//! workflow steps. There are two kinds and they are looked up in this order:
//!
//! * **Registered** — implemented in Rust and held in an [`ActionRegistry`].
//!   minact ships four, and an embedding tool adds its own. They need nothing
//!   fetched and nothing installed, which is why they win over a same-named
//!   action published on GitHub.
//! * **External** — the ones written in the workflow as `owner/repo@ref`,
//!   `./local-action` or `docker://image`. They carry an `action.yml` saying
//!   how to run them, and [`store`] fetches them when they are remote.

pub(crate) mod container;
pub mod external;
pub mod manifest;
pub mod reference;
pub mod store;

pub use external::{action_inputs, resolve as resolve_external, ActionInputs, ResolvedAction};
pub use manifest::{ActionManifest, ActionRuns, DockerImageSource};
pub use reference::{registry_name, ActionRef};
pub use store::ActionStore;

use crate::types::{Context, StepConclusion, WorkflowError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where `upload-artifact` puts its files and `download-artifact` looks for
/// them, relative to the workspace. Inside `.minact` with everything else of
/// minact's, but not under `_work`: artifacts are the user's output, and the
/// underscore marks what is the runner's own.
pub const ARTIFACTS_DIR: &str = ".minact/artifacts";

/// The output from a single action execution.
#[derive(Debug, Clone)]
pub struct ActionOutput {
    pub success: bool,
    pub conclusion: StepConclusion,
    pub outputs: HashMap<String, String>,
    pub artifacts: Vec<crate::types::Artifact>,
}

/// The context passed to an action for execution.
#[derive(Debug, Clone)]
pub struct ActionContext {
    /// Input parameters from the `with:` section.
    pub inputs: HashMap<String, String>,

    /// Environment variables.
    pub env: HashMap<String, String>,

    /// The workspace directory.
    pub workspace: std::path::PathBuf,

    /// The step's working directory (if specified).
    pub working_directory: Option<std::path::PathBuf>,

    /// Temporary directory for this action.
    pub temp_dir: std::path::PathBuf,

    /// Full workflow context for expression evaluation.
    pub context: Context,
}

/// Trait that all actions must implement.
#[async_trait]
pub trait Action: Send + Sync {
    /// Unique identifier for the action (e.g., "actions/checkout").
    fn id(&self) -> &'static str;

    /// Validate the action's inputs before execution.
    fn validate(&self, ctx: &ActionContext) -> Result<(), WorkflowError>;

    /// Execute the action with the given context.
    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError>;
}

/// Registry of available actions mapped by their fully-qualified name.
pub struct ActionRegistry {
    actions: HashMap<String, Box<dyn Action>>,
}

impl ActionRegistry {
    /// Create a new action registry with the default built-in actions.
    pub fn new() -> Self {
        let mut registry = Self {
            actions: HashMap::new(),
        };
        registry.register_builtins();
        registry
    }

    /// Register a custom action.
    pub fn register(&mut self, action: Box<dyn Action>) {
        self.actions.insert(action.id().to_string(), action);
    }

    /// Find an action by its fully-qualified name.
    pub fn get(&self, name: &str) -> Option<&dyn Action> {
        self.actions.get(name).map(|b| b.as_ref())
    }

    /// Check if an action exists.
    pub fn has_action(&self, name: &str) -> bool {
        self.actions.contains_key(name)
    }

    /// List all registered action names.
    pub fn list_actions(&self) -> Vec<&str> {
        self.actions.keys().map(|s| s.as_str()).collect()
    }

    fn register_builtins(&mut self) {
        self.register(Box::new(CheckoutAction));
        self.register(Box::new(CacheAction));
        self.register(Box::new(UploadArtifactAction));
        self.register(Box::new(DownloadArtifactAction));
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in: actions/checkout
// ---------------------------------------------------------------------------

struct CheckoutAction;

#[async_trait]
impl Action for CheckoutAction {
    fn id(&self) -> &'static str {
        "actions/checkout"
    }

    fn validate(&self, _ctx: &ActionContext) -> Result<(), WorkflowError> {
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        tracing::info!("[actions/checkout] Checking out repository...");
        // In local mode, the workspace already has the content, so this is a no-op.
        // We just ensure the workspace directory exists.
        if !ctx.workspace.exists() {
            std::fs::create_dir_all(&ctx.workspace)?;
        }
        tracing::info!(
            "[actions/checkout] Workspace ready at: {}",
            ctx.workspace.display()
        );

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::from([
                (
                    "repository".to_string(),
                    ctx.context.github.repository.clone(),
                ),
                ("ref".to_string(), ctx.context.github.ref_name.clone()),
                ("sha".to_string(), ctx.context.github.sha.clone()),
            ]),
            artifacts: vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// Built-in: actions/cache
// ---------------------------------------------------------------------------

struct CacheAction;

#[async_trait]
impl Action for CacheAction {
    fn id(&self) -> &'static str {
        "actions/cache"
    }

    fn validate(&self, ctx: &ActionContext) -> Result<(), WorkflowError> {
        if !ctx.inputs.contains_key("path") {
            return Err(WorkflowError::Other(
                "actions/cache requires 'path' input".to_string(),
            ));
        }
        if !ctx.inputs.contains_key("key") {
            return Err(WorkflowError::Other(
                "actions/cache requires 'key' input".to_string(),
            ));
        }
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        let path = &ctx.inputs["path"];
        let key = &ctx.inputs["key"];
        // In local execution, cache is stored in ~/.minact/cache/
        let cache_dir = dirs::home_dir()
            .ok_or_else(|| WorkflowError::Other("Cannot find home directory".to_string()))?
            .join(".minact")
            .join("cache");

        let cache_key_hash = sha2_hex(key);
        let cache_entry = cache_dir.join(&cache_key_hash);

        let cache_hit = if cache_entry.exists() {
            tracing::info!("[actions/cache] Cache hit for key: {}", key);
            // Restore from cache. What is there now is set aside rather than
            // deleted, so a copy that fails halfway leaves it as it was — a
            // half-restored Flutter SDK is worse than an un-restored one.
            let cache_path = Path::new(path);
            let previous = cache_path.with_extension("minact-previous");
            let had_previous = cache_path.exists();
            if had_previous {
                if previous.exists() {
                    std::fs::remove_dir_all(&previous).ok();
                }
                std::fs::rename(cache_path, &previous)?;
            }
            match copy_recursive(&cache_entry, cache_path) {
                Ok(()) => {
                    if had_previous {
                        std::fs::remove_dir_all(&previous).ok();
                    }
                }
                Err(e) => {
                    std::fs::remove_dir_all(cache_path).ok();
                    if had_previous {
                        std::fs::rename(&previous, cache_path).ok();
                    }
                    return Err(e);
                }
            }
            true
        } else {
            tracing::info!("[actions/cache] Cache miss for key: {}", key);
            false
        };

        // If there's a post-job step, we'd save the cache here.
        // For simplicity, we save immediately.
        if !cache_hit {
            let src = Path::new(path);
            if src.exists() {
                std::fs::create_dir_all(&cache_dir)?;
                copy_recursive(src, &cache_entry)?;
            }
        }

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::from([("cache-hit".to_string(), cache_hit.to_string())]),
            artifacts: vec![],
        })
    }
}

fn sha2_hex(input: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Copy a file or a tree. A symlink is recreated as a symlink — a macOS
/// framework is mostly `Resources -> Versions/Current/Resources` — and
/// anything that is neither file, directory nor link (a socket, a fifo) is
/// left out rather than failing the copy.
fn copy_recursive(src: &Path, dst: &Path) -> Result<(), WorkflowError> {
    let kind = std::fs::symlink_metadata(src)?.file_type();
    if kind.is_symlink() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let target = std::fs::read_link(src)?;
        if dst.exists() || std::fs::symlink_metadata(dst).is_ok() {
            std::fs::remove_file(dst)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dst)?;
        #[cfg(windows)]
        {
            if src.is_dir() {
                std::os::windows::fs::symlink_dir(&target, dst)?;
            } else {
                std::os::windows::fs::symlink_file(&target, dst)?;
            }
        }
    } else if kind.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    } else if kind.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Built-in: actions/upload-artifact
// ---------------------------------------------------------------------------

struct UploadArtifactAction;

#[async_trait]
impl Action for UploadArtifactAction {
    fn id(&self) -> &'static str {
        "actions/upload-artifact"
    }

    fn validate(&self, ctx: &ActionContext) -> Result<(), WorkflowError> {
        if !ctx.inputs.contains_key("path") {
            return Err(WorkflowError::Other(
                "actions/upload-artifact requires 'path' input".to_string(),
            ));
        }
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        // GitHub's default when `name:` is left out.
        let name = ctx
            .inputs
            .get("name")
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or("artifact")
            .to_string();
        let if_no_files = ctx
            .inputs
            .get("if-no-files-found")
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "warn".to_string());

        // `path:` is one entry per line, each a path or a glob, the way the
        // real action takes it.
        let mut matched: Vec<PathBuf> = Vec::new();
        for line in ctx.inputs["path"].lines() {
            let pattern = line.trim();
            if pattern.is_empty() {
                continue;
            }
            if pattern.starts_with('!') {
                tracing::warn!(
                    "[actions/upload-artifact] exclusion patterns are not supported, ignoring {}",
                    pattern
                );
                continue;
            }
            matched.extend(expand_path(&ctx.workspace, pattern));
        }
        matched.sort();
        matched.dedup();

        if matched.is_empty() {
            let message = format!(
                "No files were found with the provided path: {}",
                ctx.inputs["path"].trim()
            );
            match if_no_files.as_str() {
                "error" => return Err(WorkflowError::Other(message)),
                "ignore" => {}
                _ => tracing::warn!("[actions/upload-artifact] {}", message),
            }
        }

        // A fresh upload replaces an earlier one of the same name.
        let artifact_dir = ctx.workspace.join(ARTIFACTS_DIR).join(&name);
        if artifact_dir.exists() {
            std::fs::remove_dir_all(&artifact_dir)?;
        }
        std::fs::create_dir_all(&artifact_dir)?;

        // A single directory uploads its contents; anything else keeps the
        // hierarchy below the paths' common ancestor, as on GitHub.
        let root = artifact_root(&matched);
        for entry in &matched {
            let relative = entry.strip_prefix(&root).unwrap_or(entry);
            let target = if relative.as_os_str().is_empty() {
                artifact_dir.clone()
            } else {
                artifact_dir.join(relative)
            };
            copy_recursive(entry, &target)?;
        }
        tracing::info!(
            "[actions/upload-artifact] Uploaded '{}' ({} entries)",
            name,
            matched.len()
        );

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: vec![crate::types::Artifact {
                name,
                path: artifact_dir,
            }],
        })
    }
}

/// Resolve one `path:` line against the workspace: a plain path if it
/// exists, or everything a glob matches.
fn expand_path(workspace: &Path, pattern: &str) -> Vec<PathBuf> {
    let absolute = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        workspace.join(pattern)
    };
    if !pattern.contains(['*', '?', '[']) {
        return if absolute.exists() {
            vec![absolute]
        } else {
            Vec::new()
        };
    }
    crate::expr::glob_files(workspace, pattern)
}

/// The directory an artifact's paths are stored relative to: the directory
/// itself when there is exactly one, otherwise the deepest directory holding
/// every entry.
fn artifact_root(entries: &[PathBuf]) -> PathBuf {
    if let [only] = entries {
        if only.is_dir() {
            return only.clone();
        }
    }
    let mut root: Option<PathBuf> = None;
    for entry in entries {
        let dir = entry.parent().map(Path::to_path_buf).unwrap_or_default();
        root = Some(match root {
            None => dir,
            Some(current) => common_ancestor(&current, &dir),
        });
    }
    root.unwrap_or_default()
}

fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

// ---------------------------------------------------------------------------
// Built-in: actions/download-artifact
// ---------------------------------------------------------------------------

struct DownloadArtifactAction;

#[async_trait]
impl Action for DownloadArtifactAction {
    fn id(&self) -> &'static str {
        "actions/download-artifact"
    }

    fn validate(&self, _ctx: &ActionContext) -> Result<(), WorkflowError> {
        // `name` is optional: without it every artifact is downloaded.
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        let store = ctx.workspace.join(ARTIFACTS_DIR);
        let dest = ctx
            .inputs
            .get("path")
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let dest_path = if Path::new(dest).is_absolute() {
            PathBuf::from(dest)
        } else {
            ctx.workspace.join(dest)
        };
        let name = ctx
            .inputs
            .get("name")
            .map(|name| name.trim())
            .filter(|name| !name.is_empty());

        match name {
            // One artifact: its contents land in `path`.
            Some(name) => {
                let artifact_dir = store.join(name);
                if artifact_dir.exists() {
                    std::fs::create_dir_all(&dest_path)?;
                    copy_recursive(&artifact_dir, &dest_path)?;
                    tracing::info!(
                        "[actions/download-artifact] Downloaded '{}' to {}",
                        name,
                        dest
                    );
                } else {
                    tracing::warn!("[actions/download-artifact] Artifact '{}' not found", name);
                }
            }
            // No name: every artifact, each in its own directory under
            // `path` — or all in `path` with `merge-multiple: true`.
            None => {
                let merge = ctx
                    .inputs
                    .get("merge-multiple")
                    .map(|value| value.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let patterns: Vec<glob::Pattern> = ctx
                    .inputs
                    .get("pattern")
                    .map(|value| {
                        value
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(|line| {
                                glob::Pattern::new(line).map_err(|e| {
                                    WorkflowError::Other(format!(
                                        "actions/download-artifact: bad pattern {}: {}",
                                        line, e
                                    ))
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default();

                let mut names: Vec<String> = match std::fs::read_dir(&store) {
                    Ok(entries) => entries
                        .filter_map(Result::ok)
                        .filter(|entry| entry.path().is_dir())
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .filter(|name| {
                            patterns.is_empty() || patterns.iter().any(|p| p.matches(name))
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                };
                names.sort();
                if names.is_empty() {
                    tracing::warn!("[actions/download-artifact] No artifacts to download");
                }
                for name in &names {
                    let target = if merge {
                        dest_path.clone()
                    } else {
                        dest_path.join(name)
                    };
                    std::fs::create_dir_all(&target)?;
                    copy_recursive(&store.join(name), &target)?;
                }
                tracing::info!(
                    "[actions/download-artifact] Downloaded {} artifact(s) to {}",
                    names.len(),
                    dest
                );
            }
        }

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: vec![],
        })
    }
}
