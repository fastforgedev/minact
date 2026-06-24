//! Action trait and built-in action registry.
//!
//! Actions are reusable units of work that can be referenced via `uses:` in workflow steps.

use std::collections::HashMap;
use std::path::Path;
use async_trait::async_trait;
use crate::types::{Context, StepConclusion, WorkflowError};

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
        tracing::info!("[actions/checkout] Workspace ready at: {}", ctx.workspace.display());

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::from([
                ("repository".to_string(), ctx.context.github.repository.clone()),
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
                "actions/cache requires 'path' input".to_string()
            ));
        }
        if !ctx.inputs.contains_key("key") {
            return Err(WorkflowError::Other(
                "actions/cache requires 'key' input".to_string()
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
            // Restore from cache
            let cache_path = Path::new(path);
            if cache_path.exists() {
                std::fs::remove_dir_all(cache_path).ok();
            }
            copy_recursive(&cache_entry, cache_path)?;
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
            outputs: HashMap::from([
                ("cache-hit".to_string(), cache_hit.to_string()),
            ]),
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

fn copy_recursive(src: &Path, dst: &Path) -> Result<(), WorkflowError> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    } else if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if file_type.is_dir() {
                copy_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
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
        if !ctx.inputs.contains_key("name") {
            return Err(WorkflowError::Other(
                "actions/upload-artifact requires 'name' input".to_string()
            ));
        }
        if !ctx.inputs.contains_key("path") {
            return Err(WorkflowError::Other(
                "actions/upload-artifact requires 'path' input".to_string()
            ));
        }
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        let name = &ctx.inputs["name"];
        let path = &ctx.inputs["path"];
        let src_path = if Path::new(path).is_absolute() {
            Path::new(path).to_path_buf()
        } else {
            ctx.workspace.join(path)
        };

        // Store artifacts in the workspace's artifact directory
        let artifact_dir = ctx.workspace.join(".minact-artifacts").join(name);
        std::fs::create_dir_all(&artifact_dir)?;

        if src_path.exists() {
            copy_recursive(&src_path, &artifact_dir)?;
            tracing::info!("[actions/upload-artifact] Uploaded '{}' from {}", name, path);
        } else {
            tracing::warn!("[actions/upload-artifact] Path '{}' does not exist", path);
        }

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: vec![crate::types::Artifact {
                name: name.clone(),
                path: artifact_dir,
            }],
        })
    }
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

    fn validate(&self, ctx: &ActionContext) -> Result<(), WorkflowError> {
        if !ctx.inputs.contains_key("name") {
            return Err(WorkflowError::Other(
                "actions/download-artifact requires 'name' input".to_string()
            ));
        }
        Ok(())
    }

    async fn run(&self, ctx: &ActionContext) -> Result<ActionOutput, WorkflowError> {
        let name = &ctx.inputs["name"];
        let dest = ctx.inputs.get("path").cloned().unwrap_or_else(|| ".".to_string());

        let artifact_dir = ctx.workspace.join(".minact-artifacts").join(name);
        let dest_path = if Path::new(&dest).is_absolute() {
            Path::new(&dest).to_path_buf()
        } else {
            ctx.workspace.join(&dest)
        };

        if artifact_dir.exists() {
            std::fs::create_dir_all(&dest_path)?;
            copy_recursive(&artifact_dir, &dest_path)?;
            tracing::info!("[actions/download-artifact] Downloaded '{}' to {}", name, dest);
        } else {
            tracing::warn!("[actions/download-artifact] Artifact '{}' not found", name);
        }

        Ok(ActionOutput {
            success: true,
            conclusion: StepConclusion::Success,
            outputs: HashMap::new(),
            artifacts: vec![],
        })
    }
}
