//! HTTP API. Every route is mounted under `/api`.

mod artifacts;
mod meta;
mod runs;
mod workflows;

use axum::routing::{get, post};
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/meta", get(meta::get_meta))
        .route("/workflows", get(workflows::list_workflows))
        .route("/workflows/{id}", get(workflows::get_workflow))
        .route("/runs", get(runs::list_runs).post(runs::start_run))
        .route("/runs/{id}", get(runs::get_run))
        .route("/runs/{id}/events", get(runs::stream_events))
        .route("/runs/{id}/cancel", post(runs::cancel_run))
        .route("/runs/{id}/logs", get(runs::get_run_log))
        .route("/artifacts", get(artifacts::list_artifacts))
        .route(
            "/artifacts/{name}/{*path}",
            get(artifacts::get_artifact_file),
        )
}
