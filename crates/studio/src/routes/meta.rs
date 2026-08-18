//! `/api/meta` — what this Studio is attached to.

use axum::extract::State;
use axum::Json;

use crate::discovery;
use crate::dto::{MetaDto, RunnerDto};
use crate::error::ApiResult;
use crate::state::AppState;

pub async fn get_meta(State(state): State<AppState>) -> ApiResult<Json<MetaDto>> {
    let registry = (state.actions)();
    let mut actions: Vec<String> = registry
        .list_actions()
        .into_iter()
        .map(str::to_string)
        .collect();
    actions.sort();

    Ok(Json(MetaDto {
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: state.workspace.display().to_string(),
        runner: RunnerDto::current(),
        actions,
        workflow_count: discovery::discover(&state.workspace, &state.workflow_dirs).len(),
    }))
}
