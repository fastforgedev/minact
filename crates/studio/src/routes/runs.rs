//! `/api/runs` — starting runs, watching them, stopping them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use minact_core::{Engine, LogRecord};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::Stream;

use crate::discovery;
use crate::error::{ApiError, ApiResult};
use crate::runs::{self, RunHandle, RunMeta, RunStatus, RunView, StudioReporter};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StartRun {
    pub workflow_id: String,
    /// The event to simulate. Defaults to the manual trigger, like the CLI.
    #[serde(default = "default_event")]
    pub event: String,
    #[serde(default)]
    pub inputs: HashMap<String, String>,
}

fn default_event() -> String {
    "workflow_dispatch".to_string()
}

#[derive(Debug, Serialize)]
pub struct RunSummary {
    #[serde(flatten)]
    pub meta: RunMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RunDetail {
    #[serde(flatten)]
    pub summary: RunSummary,
    /// Jobs, steps, conclusions and durations folded out of the event stream.
    #[serde(flatten)]
    pub view: RunView,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    /// Only runs of this workflow.
    pub workflow: Option<String>,
    /// Only runs that ended this way — `running`, `success`, `failure`, …
    pub status: Option<RunStatus>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct LogParams {
    /// Limit the log to one job instance.
    pub job: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// Resume from this sequence number. Everything at or after it is replayed
    /// before the live feed begins.
    #[serde(default)]
    pub from: u64,
}

/// Start a run. Returns as soon as it is registered — the run itself happens
/// in the background and is watched over SSE.
pub async fn start_run(
    State(state): State<AppState>,
    Json(body): Json<StartRun>,
) -> ApiResult<(StatusCode, Json<RunSummary>)> {
    let found = discovery::find(&state.workspace, &state.workflow_dirs, &body.workflow_id)
        .ok_or_else(|| ApiError::NotFound(format!("No workflow with id '{}'", body.workflow_id)))?;

    let workflow = found
        .parsed
        .as_ref()
        .map_err(|message| {
            ApiError::BadRequest(format!(
                "{} could not be parsed: {}",
                found.rel_path, message
            ))
        })?
        .clone();

    let workflow_name = if workflow.name.trim().is_empty() {
        found.rel_path.clone()
    } else {
        workflow.name.clone()
    };

    let handle = state
        .runs
        .create(
            found.id.clone(),
            workflow_name,
            found.rel_path.clone(),
            body.event.clone(),
            body.inputs.clone(),
        )
        .await;

    let engine = Engine::with_actions_and_reporter(
        (*state.workspace).clone(),
        (state.actions)(),
        Arc::new(StudioReporter::new(Arc::clone(&handle))),
    );

    let run = Arc::clone(&handle);
    tokio::spawn(async move {
        let outcome = engine
            .run_workflow_cancellable(&workflow, &body.event, body.inputs, run.cancel.clone())
            .await;

        runs::finish(
            &run,
            outcome
                .map(|result| result.success)
                .map_err(|err| err.to_string()),
        )
        .await;
    });

    Ok((StatusCode::ACCEPTED, Json(summary(&handle).await)))
}

pub async fn list_runs(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<Vec<RunSummary>>> {
    let mut summaries = Vec::new();

    for handle in state.runs.list().await {
        let run = summary(&handle).await;

        if params
            .workflow
            .as_ref()
            .is_some_and(|id| &run.meta.workflow_id != id)
        {
            continue;
        }
        if params
            .status
            .is_some_and(|status| run.meta.status != status)
        {
            continue;
        }

        summaries.push(run);
        // Runs come back newest first, so the cap keeps the newest.
        if params.limit.is_some_and(|limit| summaries.len() >= limit) {
            break;
        }
    }

    Ok(Json(summaries))
}

/// The run as plain text — the thing you paste into an issue.
pub async fn get_run_log(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> ApiResult<Response> {
    let handle = find_run(&state, &id).await?;
    let meta = handle.meta.read().await.clone();
    let records = handle.records_or_load().await;

    let text = runs::render_text(&meta, &records, params.job.as_deref());
    let filename = match &params.job {
        Some(job) => format!("minact-run-{}-{}.log", id, slug(job)),
        None => format!("minact-run-{}.log", id),
    };

    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename),
            ),
        ],
        text,
    )
        .into_response())
}

/// A job instance id is not filename-safe — `build (os=macos)` has spaces,
/// parentheses and an `=`.
fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunDetail>> {
    let handle = find_run(&state, &id).await?;
    let records = handle.records_or_load().await;

    Ok(Json(RunDetail {
        summary: summary(&handle).await,
        view: RunView::from_records(&records),
    }))
}

/// Stop a run. Idempotent: cancelling a finished run is a no-op.
pub async fn cancel_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunSummary>> {
    let handle = find_run(&state, &id).await?;
    handle.cancel.cancel();
    Ok(Json(summary(&handle).await))
}

/// The live event feed.
///
/// Replays from `?from=` before switching to the live channel, so a page load
/// or a dropped connection both end up with the complete stream and no gap.
pub async fn stream_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<StreamParams>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let handle = find_run(&state, &id).await?;

    let (mut receiver, snapshot) = handle.subscribe().await;
    let replay: Vec<LogRecord> = if snapshot.is_empty() {
        // Nothing in memory: an older run, read back from disk. It has no live
        // tail, so the stream ends right after the replay.
        handle.records_or_load().await
    } else {
        snapshot
    };
    let replay: Vec<LogRecord> = replay
        .into_iter()
        .filter(|record| record.seq >= params.from)
        .collect();
    let mut highest = replay.last().map(|record| record.seq);

    let stream = async_stream::stream! {
        for record in replay {
            yield Ok(record_event(&record));
        }

        loop {
            tokio::select! {
                received = receiver.recv() => match received {
                    // The replay and the live feed overlap by design; drop
                    // what the client has already been sent.
                    Ok(record) => {
                        if highest.is_none_or(|seq| record.seq > seq) {
                            highest = Some(record.seq);
                            yield Ok(record_event(&record));
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        // The client fell too far behind to be caught up from
                        // the channel. Tell it, so it can reconnect with
                        // `?from=` and pick the gap up from the replay buffer.
                        yield Ok(Event::default()
                            .event("lagged")
                            .data(missed.to_string()));
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = handle.finished() => {
                    // Whatever is still buffered was emitted before the run
                    // ended, so drain it rather than dropping the last lines.
                    while let Ok(record) = receiver.try_recv() {
                        if highest.is_none_or(|seq| record.seq > seq) {
                            highest = Some(record.seq);
                            yield Ok(record_event(&record));
                        }
                    }
                    break;
                }
            }
        }

        let status = handle.meta.read().await.status;
        yield Ok(Event::default()
            .event("end")
            .data(serde_json::to_string(&status).unwrap_or_else(|_| "null".to_string())));
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// One record as an SSE message. The `id:` field is the sequence number, so a
/// browser's automatic reconnect carries it back in `Last-Event-ID`.
fn record_event(record: &LogRecord) -> Event {
    let event = Event::default().id(record.seq.to_string()).event("record");

    match serde_json::to_string(record) {
        Ok(json) => event.data(json),
        // Should not happen; a dropped line beats a dropped stream.
        Err(err) => Event::default()
            .event("error")
            .data(format!("could not encode event {}: {}", record.seq, err)),
    }
}

async fn find_run(state: &AppState, id: &str) -> ApiResult<Arc<RunHandle>> {
    state
        .runs
        .get(id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("No run with id '{}'", id)))
}

async fn summary(handle: &RunHandle) -> RunSummary {
    let meta = handle.meta.read().await.clone();
    RunSummary {
        duration_ms: meta.duration_ms(),
        meta,
    }
}
