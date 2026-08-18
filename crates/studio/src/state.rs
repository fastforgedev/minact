//! Shared server state.

use std::path::PathBuf;
use std::sync::Arc;

use minact_core::ActionRegistry;

use crate::runs::RunStore;

/// Builds the action registry a run executes with.
///
/// A factory rather than a value because `ActionRegistry` holds boxed trait
/// objects and every run needs its own. A host application supplies one that
/// registers its own custom actions.
pub type ActionFactory = Arc<dyn Fn() -> ActionRegistry + Send + Sync>;

/// Handed to every route. Cheap to clone — everything behind it is shared.
#[derive(Clone)]
pub struct AppState {
    pub workspace: Arc<PathBuf>,
    /// Directories to search on top of the ones minact knows about, so Studio
    /// can browse a folder of workflows that is not a project layout.
    pub workflow_dirs: Arc<Vec<PathBuf>>,
    pub actions: ActionFactory,
    pub runs: Arc<RunStore>,
}
