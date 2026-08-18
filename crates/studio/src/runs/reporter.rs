//! The `Reporter` that feeds Studio.

use std::sync::Arc;

use async_trait::async_trait;
use minact_core::{LogEvent, LogRecord, Reporter};

use super::{record, RunHandle};

/// Forwards every event to a run: memory, disk, and any live subscriber.
///
/// This implements [`Reporter::emit_record`] rather than
/// [`Reporter::emit`] — the envelope's sequence number is what a reconnecting
/// browser resumes from, and its scope is what places a log line under the
/// right job and step.
pub struct StudioReporter {
    run: Arc<RunHandle>,
}

impl StudioReporter {
    pub fn new(run: Arc<RunHandle>) -> Self {
        Self { run }
    }
}

#[async_trait]
impl Reporter for StudioReporter {
    async fn emit(&self, _event: LogEvent) {
        // The engine always calls `emit_record`. Reaching here would mean an
        // event arrived with no sequence number, which nothing downstream can
        // order or resume from — dropping it beats inventing one.
        tracing::warn!("studio reporter received an event with no envelope");
    }

    async fn emit_record(&self, entry: LogRecord) {
        record(&self.run, entry).await;
    }
}
