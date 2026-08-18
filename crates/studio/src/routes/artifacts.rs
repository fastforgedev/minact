//! `/api/artifacts` — what the runs produced.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::artifacts::{self, ArtifactDto};
use crate::error::ApiResult;
use crate::state::AppState;

pub async fn list_artifacts(State(state): State<AppState>) -> ApiResult<Json<Vec<ArtifactDto>>> {
    Ok(Json(artifacts::list(&state.workspace)))
}

/// Serve one file out of an artifact.
pub async fn get_artifact_file(
    State(state): State<AppState>,
    Path((name, file_path)): Path<(String, String)>,
) -> ApiResult<Response> {
    let path = artifacts::resolve(&state.workspace, &name, &file_path)?;
    let bytes = std::fs::read(&path)?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();

    // `inline` so a text file or an image opens in the browser; the UI adds
    // `download` on the link when the reader wants the file itself.
    let disposition = format!(
        "inline; filename=\"{}\"",
        path.file_name()
            .map(|name| name.to_string_lossy().replace('"', ""))
            .unwrap_or_else(|| "artifact".into())
    );

    Ok((
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}
