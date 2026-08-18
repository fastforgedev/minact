//! `/api/workflows` — discovery and detail.

use axum::extract::{Path, State};
use axum::Json;

use crate::discovery;
use crate::dto::{WorkflowDetailDto, WorkflowSummaryDto};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub async fn list_workflows(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<WorkflowSummaryDto>>> {
    let summaries = discovery::discover(&state.workspace, &state.workflow_dirs)
        .iter()
        .map(WorkflowSummaryDto::from_discovered)
        .collect();

    Ok(Json(summaries))
}

pub async fn get_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorkflowDetailDto>> {
    let found = discovery::find(&state.workspace, &state.workflow_dirs, &id)
        .ok_or_else(|| ApiError::NotFound(format!("No workflow with id '{}'", id)))?;

    // The editor and the YAML tab want the file verbatim, comments and all —
    // re-serializing the parsed model would throw that away.
    let yaml = std::fs::read_to_string(&found.abs_path)?;

    let workflow = found.parsed.as_ref().map_err(|message| {
        ApiError::BadRequest(format!(
            "{} could not be parsed: {}",
            found.rel_path, message
        ))
    })?;

    Ok(Json(WorkflowDetailDto::build(&found, workflow, yaml)))
}
