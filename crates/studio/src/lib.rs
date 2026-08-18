//! minact-studio: a web UI for the minact workflow engine.
//!
//! The crate is a library first and a binary second. `minact studio` is a thin
//! wrapper around [`StudioServer`], and a host application that embeds the
//! engine — registering its own actions — mounts the same router with its own
//! [`ActionRegistry`] rather than reimplementing the UI:
//!
//! ```no_run
//! # use minact_studio::StudioServer;
//! # use minact_core::ActionRegistry;
//! # fn my_actions() -> ActionRegistry { ActionRegistry::new() }
//! # async fn example() -> std::io::Result<()> {
//! StudioServer::new(std::env::current_dir()?)
//!     .with_actions(my_actions)
//!     .serve("127.0.0.1:4000".parse().unwrap())
//!     .await
//! # }
//! ```

mod artifacts;
mod assets;
mod discovery;
mod dto;
mod error;
mod routes;
pub mod runs;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use minact_core::ActionRegistry;
use tokio::net::TcpListener;

use crate::runs::RunStore;
use crate::state::{ActionFactory, AppState};

/// The Studio HTTP server.
pub struct StudioServer {
    workspace: PathBuf,
    workflow_dirs: Vec<PathBuf>,
    actions: ActionFactory,
}

impl StudioServer {
    /// Serve the workflows found under `workspace`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            workflow_dirs: Vec::new(),
            actions: Arc::new(ActionRegistry::new),
        }
    }

    /// Search these directories as well as the ones minact discovers.
    ///
    /// Relative paths resolve against the workspace. Use this to browse a
    /// folder that is not a project layout — `examples/`, a scratch directory
    /// — which minact would otherwise never look in.
    pub fn with_workflow_dirs<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.workflow_dirs.extend(dirs.into_iter().map(Into::into));
        self
    }

    /// Supply the action registry runs execute with.
    ///
    /// The factory is called once per run, because a registry owns its actions
    /// and cannot be shared. Pass one that registers your custom actions and
    /// the UI will list — and the runs will resolve — exactly that set.
    pub fn with_actions<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> ActionRegistry + Send + Sync + 'static,
    {
        self.actions = Arc::new(factory);
        self
    }

    /// The full router: `/api/*` plus the embedded front-end on every other
    /// path. Mount it as-is, or nest it inside a larger application.
    pub fn router(self) -> Router {
        let state = AppState {
            runs: Arc::new(RunStore::open(&self.workspace)),
            workspace: Arc::new(self.workspace),
            workflow_dirs: Arc::new(self.workflow_dirs),
            actions: self.actions,
        };

        Router::new()
            .nest("/api", routes::router())
            .fallback(assets::handler)
            .with_state(state)
    }

    /// Bind `addr` and serve until the process ends.
    pub async fn serve(self, addr: SocketAddr) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.serve_on(listener).await
    }

    /// Serve on an already-bound listener.
    ///
    /// Bind separately when the caller needs the resolved address before
    /// serving — with `--port 0` that is the only way to learn which port the
    /// OS handed out, and the CLI prints a URL before it starts.
    pub async fn serve_on(self, listener: TcpListener) -> std::io::Result<()> {
        axum::serve(listener, self.router()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn workspace_with_workflows() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".minact/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ci.yml"),
            "name: CI\non: push\njobs:\n  build:\n    needs: [setup]\n    steps:\n      - run: echo build\n  setup:\n    steps:\n      - run: echo setup\n",
        )
        .unwrap();
        tmp
    }

    async fn get(router: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn meta_reports_the_workspace_and_builtin_actions() {
        let tmp = workspace_with_workflows();
        let (status, body) = get(StudioServer::new(tmp.path()).router(), "/api/meta").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["workflow_count"], 1);
        let actions = body["actions"].as_array().unwrap();
        assert!(actions.iter().any(|a| a == "actions/checkout"));
    }

    #[tokio::test]
    async fn workflow_list_then_detail_round_trips_the_id() {
        let tmp = workspace_with_workflows();
        let router = StudioServer::new(tmp.path()).router();

        let (status, list) = get(router.clone(), "/api/workflows").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "CI");
        assert_eq!(list[0]["job_count"], 2);
        assert_eq!(list[0]["triggers"][0], "push");

        let id = list[0]["id"].as_str().unwrap();
        let (status, detail) = get(router, &format!("/api/workflows/{}", id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["graph"]["layers"][0][0], "setup");
        assert_eq!(detail["graph"]["layers"][1][0], "build");
        assert!(detail["yaml"].as_str().unwrap().contains("echo build"));
        // Jobs come back in execution order, not HashMap order.
        assert_eq!(detail["jobs"][0]["id"], "setup");
    }

    #[tokio::test]
    async fn unknown_workflow_id_is_a_404() {
        let tmp = workspace_with_workflows();
        let (status, body) = get(
            StudioServer::new(tmp.path()).router(),
            "/api/workflows/bm8K",
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
    }

    #[tokio::test]
    async fn unknown_paths_fall_back_to_the_spa_shell() {
        let tmp = workspace_with_workflows();
        let response = StudioServer::new(tmp.path())
            .router()
            .oneshot(
                Request::builder()
                    .uri("/runs/42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/html",
            "the client router must receive the shell, not a 404"
        );
    }
}
