//! Run bookkeeping: starting runs, holding their events, streaming them out.

mod reporter;
mod text;
mod view;

pub use reporter::StudioReporter;
pub use text::render as render_text;
pub use view::{JobView, RunView, StepView};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use minact_core::{CancellationToken, LogRecord};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

/// How many events a live subscriber can fall behind before it is dropped and
/// has to reconnect. A noisy build emits thousands of lines a second, and the
/// client resumes from its last `seq`, so lagging is recoverable.
const BROADCAST_CAPACITY: usize = 4096;

/// Where a run is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Failure,
    Cancelled,
    /// The engine itself errored — a run that never really got going.
    Errored,
}

impl RunStatus {
    pub fn is_finished(self) -> bool {
        !matches!(self, RunStatus::Running)
    }
}

/// The persisted description of a run, independent of its events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub id: String,
    pub workflow_id: String,
    pub workflow_name: String,
    pub workflow_path: String,
    pub event: String,
    pub inputs: HashMap<String, String>,
    pub status: RunStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set when `status` is `Errored`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunMeta {
    pub fn duration_ms(&self) -> Option<i64> {
        let finished = self.finished_at?;
        Some((finished - self.started_at).num_milliseconds())
    }
}

/// One run: its metadata, its events, and the handle to stop it.
pub struct RunHandle {
    pub meta: RwLock<RunMeta>,
    /// Every event so far. Kept in memory so a page load can replay a run from
    /// the beginning without touching disk.
    records: RwLock<Vec<LogRecord>>,
    events: broadcast::Sender<LogRecord>,
    pub cancel: CancellationToken,
    /// Fires once when the run reaches a terminal state. A `CancellationToken`
    /// rather than a `Notify` because it is race-free: a subscriber that
    /// arrives after the run ended still resolves immediately.
    finished: CancellationToken,
    dir: PathBuf,
}

impl RunHandle {
    pub async fn records(&self) -> Vec<LogRecord> {
        self.records.read().await.clone()
    }

    /// Subscribe before snapshotting, never after: a record emitted between a
    /// snapshot and a later subscribe would be in neither, and the client would
    /// have a hole it could not detect.
    pub async fn subscribe(&self) -> (broadcast::Receiver<LogRecord>, Vec<LogRecord>) {
        let receiver = self.events.subscribe();
        let snapshot = self.records.read().await.clone();
        (receiver, snapshot)
    }

    async fn push(&self, record: LogRecord) {
        self.records.write().await.push(record.clone());
        // Errors here only mean nobody is listening.
        let _ = self.events.send(record);
    }

    /// Resolves once the run is over, so a live stream knows to close.
    pub async fn finished(&self) {
        self.finished.cancelled().await;
    }

    async fn finish(&self, status: RunStatus, error: Option<String>) {
        {
            let mut meta = self.meta.write().await;
            meta.status = status;
            meta.finished_at = Some(chrono::Utc::now());
            meta.error = error;
            write_meta(&self.dir, &meta);
        }
        self.finished.cancel();
    }
}

/// Every run this Studio knows about, live or finished.
pub struct RunStore {
    root: PathBuf,
    runs: RwLock<HashMap<String, Arc<RunHandle>>>,
    /// Run ids, newest first.
    order: RwLock<Vec<String>>,
}

impl RunStore {
    /// Open the store for a workspace, adopting any runs already on disk.
    pub fn open(workspace: &Path) -> Self {
        let root = workspace.join(".minact").join("runs");
        let (runs, order) = load_existing(&root);
        Self {
            root,
            runs: RwLock::new(runs),
            order: RwLock::new(order),
        }
    }

    pub async fn get(&self, id: &str) -> Option<Arc<RunHandle>> {
        self.runs.read().await.get(id).cloned()
    }

    /// Handles for every run, newest first.
    pub async fn list(&self) -> Vec<Arc<RunHandle>> {
        let runs = self.runs.read().await;
        self.order
            .read()
            .await
            .iter()
            .filter_map(|id| runs.get(id).cloned())
            .collect()
    }

    /// Register a new run and give back its handle.
    pub async fn create(
        &self,
        workflow_id: String,
        workflow_name: String,
        workflow_path: String,
        event: String,
        inputs: HashMap<String, String>,
    ) -> Arc<RunHandle> {
        let mut order = self.order.write().await;
        let mut runs = self.runs.write().await;

        // Runs are numbered like CI builds. The next number has to clear
        // everything on disk, not just what is loaded, or a run started by
        // another Studio would be overwritten.
        let id = self.next_id(&runs);
        let dir = self.root.join(&id);
        let _ = std::fs::create_dir_all(&dir);

        let meta = RunMeta {
            id: id.clone(),
            workflow_id,
            workflow_name,
            workflow_path,
            event,
            inputs,
            status: RunStatus::Running,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
        };
        write_meta(&dir, &meta);

        let handle = Arc::new(RunHandle {
            meta: RwLock::new(meta),
            records: RwLock::new(Vec::new()),
            events: broadcast::channel(BROADCAST_CAPACITY).0,
            cancel: CancellationToken::new(),
            finished: CancellationToken::new(),
            dir,
        });

        order.insert(0, id.clone());
        runs.insert(id, Arc::clone(&handle));

        handle
    }

    fn next_id(&self, runs: &HashMap<String, Arc<RunHandle>>) -> String {
        let highest_known = runs.keys().filter_map(|id| id.parse::<u64>().ok()).max();

        let highest_on_disk = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
            .max();

        let next = highest_known.max(highest_on_disk).unwrap_or(0) + 1;
        next.to_string()
    }
}

/// A signal that has already fired.
fn finished_token() -> CancellationToken {
    let token = CancellationToken::new();
    token.cancel();
    token
}

/// Read the metadata of previously recorded runs. Their events stay on disk
/// until something asks for them.
fn load_existing(root: &Path) -> (HashMap<String, Arc<RunHandle>>, Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (HashMap::new(), Vec::new());
    };

    let mut found: Vec<RunMeta> = entries
        .flatten()
        .filter_map(|entry| {
            let raw = std::fs::read_to_string(entry.path().join("meta.json")).ok()?;
            serde_json::from_str::<RunMeta>(&raw).ok()
        })
        .map(|mut meta| {
            // A run recorded as running cannot still be running: the process
            // that owned it is gone.
            if meta.status == RunStatus::Running {
                meta.status = RunStatus::Cancelled;
                meta.finished_at.get_or_insert_with(chrono::Utc::now);
            }
            meta
        })
        .collect();

    found.sort_by_key(|meta| std::cmp::Reverse(meta.started_at));

    let mut runs = HashMap::new();
    let mut order = Vec::new();
    for meta in found {
        let dir = root.join(&meta.id);
        order.push(meta.id.clone());
        runs.insert(
            meta.id.clone(),
            Arc::new(RunHandle {
                meta: RwLock::new(meta),
                records: RwLock::new(Vec::new()),
                events: broadcast::channel(BROADCAST_CAPACITY).0,
                cancel: CancellationToken::new(),
                // Loaded runs are already over.
                finished: finished_token(),
                dir,
            }),
        );
    }

    (runs, order)
}

/// Append a record to a run: memory, subscribers, and the log on disk.
pub async fn record(handle: &RunHandle, entry: LogRecord) {
    append_event(&handle.dir, &entry);
    handle.push(entry).await;
}

/// Mark a run finished. `Ok(success)` came from the engine; `Err` means the
/// engine could not complete the run at all.
pub async fn finish(handle: &RunHandle, outcome: Result<bool, String>) {
    match outcome {
        Ok(true) => handle.finish(RunStatus::Success, None).await,
        Ok(false) if handle.cancel.is_cancelled() => {
            handle.finish(RunStatus::Cancelled, None).await
        }
        Ok(false) => handle.finish(RunStatus::Failure, None).await,
        Err(message) => handle.finish(RunStatus::Errored, Some(message)).await,
    }
}

fn write_meta(dir: &Path, meta: &RunMeta) {
    if let Ok(json) = serde_json::to_string_pretty(meta) {
        // Losing the metadata file must not take the run down with it; the run
        // is still fully usable from memory for this process's lifetime.
        if let Err(err) = std::fs::write(dir.join("meta.json"), json) {
            tracing::warn!("could not write run metadata: {}", err);
        }
    }
}

fn append_event(dir: &Path, entry: &LogRecord) {
    use std::io::Write;

    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"));

    if let Ok(mut file) = file {
        let _ = writeln!(file, "{}", line);
    }
}

/// Read a finished run's events back from disk.
pub fn read_events(dir: &Path) -> Vec<LogRecord> {
    let Ok(raw) = std::fs::read_to_string(dir.join("events.jsonl")) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

impl RunHandle {
    /// Events for this run, reading them back from disk if this process never
    /// held them (a run recorded before Studio restarted).
    pub async fn records_or_load(&self) -> Vec<LogRecord> {
        let in_memory = self.records().await;
        if !in_memory.is_empty() {
            return in_memory;
        }
        read_events(&self.dir)
    }
}
