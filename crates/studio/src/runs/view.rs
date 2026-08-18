//! The run state a UI renders, derived from the event stream.
//!
//! Nothing here is stored: jobs, steps, conclusions and durations are all
//! folded out of the records. That keeps one code path for a run happening now
//! and a run replayed from `events.jsonl`, and it means the events are the only
//! thing that has to be right.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use minact_core::{EventScope, LogEvent, LogRecord, StepConclusion};
use serde::Serialize;

/// A job as the run saw it.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    /// The instance id — `build` normally, `build (os=macos)` under a matrix.
    pub id: String,
    pub name: String,
    /// `null` while the job is still running.
    pub conclusion: Option<StepConclusion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// Why the job was skipped or cancelled, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub steps: Vec<StepView>,
}

/// A step as the run saw it.
#[derive(Debug, Clone, Serialize)]
pub struct StepView {
    pub index: usize,
    pub name: String,
    pub conclusion: Option<StepConclusion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Everything the run screen needs, folded out of the events.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunView {
    /// The execution plan, as the scheduler resolved it for this run.
    pub layers: Vec<Vec<String>>,
    pub jobs: Vec<JobView>,
    /// Highest sequence number folded in. A client subscribes from here.
    pub last_seq: Option<u64>,
}

impl RunView {
    pub fn from_records(records: &[LogRecord]) -> Self {
        let mut view = RunView {
            last_seq: records.last().map(|record| record.seq),
            ..Default::default()
        };

        // Jobs are keyed by instance id and kept in first-seen order, which is
        // the order they ran.
        let mut index: HashMap<String, usize> = HashMap::new();

        for entry in records {
            match &entry.event {
                LogEvent::ExecutionPlan { layers } => view.layers = layers.clone(),

                LogEvent::JobStarted { job_id, job_name } => {
                    let job = view.job_mut(&mut index, job_id, job_name);
                    job.started_at = Some(entry.ts);
                }

                LogEvent::JobSkipped {
                    job_id,
                    job_name,
                    condition,
                } => {
                    let job = view.job_mut(&mut index, job_id, job_name);
                    job.conclusion = Some(StepConclusion::Skipped);
                    job.note = Some(format!("if: {}", condition));
                }

                LogEvent::JobCancelled {
                    job_id,
                    job_name,
                    reason,
                } => {
                    let job = view.job_mut(&mut index, job_id, job_name);
                    job.conclusion = Some(StepConclusion::Cancelled);
                    job.note = Some(reason.clone());
                }

                LogEvent::JobFinished {
                    job_id,
                    job_name,
                    conclusion,
                    ..
                } => {
                    let ts = entry.ts;
                    let job = view.job_mut(&mut index, job_id, job_name);
                    job.conclusion = Some(*conclusion);
                    job.duration_ms = job.started_at.map(|start| (ts - start).num_milliseconds());
                    // A job that ends closes whatever step was still open —
                    // the engine has no per-step "finished" event.
                    close_open_step(job, ts);
                }

                LogEvent::StepStarted {
                    step_index,
                    step_name,
                    ..
                } => {
                    let Some(job_id) = scope_job(&entry.scope) else {
                        continue;
                    };
                    let ts = entry.ts;
                    let job = view.job_mut(&mut index, job_id, job_id);
                    close_open_step(job, ts);

                    let step = step_mut(job, *step_index, step_name);
                    step.started_at = Some(ts);
                }

                LogEvent::StepSkipped {
                    step_index,
                    step_name,
                    condition,
                    ..
                } => {
                    let Some(job_id) = scope_job(&entry.scope) else {
                        continue;
                    };
                    let job = view.job_mut(&mut index, job_id, job_id);
                    let step = step_mut(job, *step_index, step_name);
                    step.conclusion = Some(if condition == "cancelled" {
                        StepConclusion::Cancelled
                    } else {
                        StepConclusion::Skipped
                    });
                    step.duration_ms = Some(0);
                    step.note = Some(condition.clone());
                }

                // Everything else only matters for the log stream.
                _ => {}
            }
        }

        view
    }

    fn job_mut<'a>(
        &'a mut self,
        index: &mut HashMap<String, usize>,
        job_id: &str,
        job_name: &str,
    ) -> &'a mut JobView {
        let position = *index.entry(job_id.to_string()).or_insert_with(|| {
            self.jobs.push(JobView {
                id: job_id.to_string(),
                name: job_name.to_string(),
                conclusion: None,
                started_at: None,
                duration_ms: None,
                note: None,
                steps: Vec::new(),
            });
            self.jobs.len() - 1
        });
        &mut self.jobs[position]
    }
}

fn scope_job(scope: &EventScope) -> Option<&str> {
    scope.job_id.as_deref()
}

fn step_mut<'a>(job: &'a mut JobView, index: usize, name: &str) -> &'a mut StepView {
    if let Some(position) = job.steps.iter().position(|step| step.index == index) {
        return &mut job.steps[position];
    }
    job.steps.push(StepView {
        index,
        name: name.to_string(),
        conclusion: None,
        started_at: None,
        duration_ms: None,
        note: None,
    });
    let last = job.steps.len() - 1;
    &mut job.steps[last]
}

/// Close the step that is still open, if any.
///
/// The engine reports when a step starts but not when it ends, so a step's
/// duration is the gap until the next step starts or the job finishes. That is
/// exact for sequential steps, which is how steps always run.
fn close_open_step(job: &mut JobView, ts: DateTime<Utc>) {
    let Some(open) = job
        .steps
        .iter_mut()
        .rev()
        .find(|step| step.conclusion.is_none() && step.started_at.is_some())
    else {
        return;
    };

    let start = open.started_at.expect("filtered on started_at");
    open.duration_ms = Some((ts - start).num_milliseconds());
    // The conclusion comes from the job: a step that ran to the next step
    // succeeded, and a job that failed failed on its last open step.
    open.conclusion = Some(match job.conclusion {
        Some(StepConclusion::Failure) => StepConclusion::Failure,
        Some(StepConclusion::Cancelled) => StepConclusion::Cancelled,
        _ => StepConclusion::Success,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use minact_core::{EventScope, LogEvent, LogRecord};

    fn at(seq: u64, secs: i64, scope: EventScope, event: LogEvent) -> LogRecord {
        LogRecord {
            seq,
            ts: DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap(),
            scope,
            event,
        }
    }

    #[test]
    fn folds_jobs_steps_and_durations_out_of_the_stream() {
        let records = vec![
            at(
                0,
                0,
                EventScope::default(),
                LogEvent::ExecutionPlan {
                    layers: vec![vec!["build".into()]],
                },
            ),
            at(
                1,
                0,
                EventScope::job("build"),
                LogEvent::JobStarted {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                },
            ),
            at(
                2,
                1,
                EventScope::step("build", 0),
                LogEvent::StepStarted {
                    job_id: "build".into(),
                    step_index: 0,
                    step_name: "Compile".into(),
                },
            ),
            at(
                3,
                4,
                EventScope::step("build", 1),
                LogEvent::StepStarted {
                    job_id: "build".into(),
                    step_index: 1,
                    step_name: "Test".into(),
                },
            ),
            at(
                4,
                9,
                EventScope::job("build"),
                LogEvent::JobFinished {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                    success: true,
                    conclusion: StepConclusion::Success,
                },
            ),
        ];

        let view = RunView::from_records(&records);

        assert_eq!(view.layers, vec![vec!["build".to_string()]]);
        assert_eq!(view.last_seq, Some(4));
        assert_eq!(view.jobs.len(), 1);

        let job = &view.jobs[0];
        assert_eq!(job.name, "Build");
        assert_eq!(job.conclusion, Some(StepConclusion::Success));
        assert_eq!(job.duration_ms, Some(9_000));

        // Step 1 runs until step 2 starts; step 2 until the job ends.
        assert_eq!(job.steps[0].duration_ms, Some(3_000));
        assert_eq!(job.steps[1].duration_ms, Some(5_000));
        assert!(job
            .steps
            .iter()
            .all(|step| step.conclusion == Some(StepConclusion::Success)));
    }

    #[test]
    fn a_running_job_has_no_conclusion_yet() {
        let records = vec![
            at(
                0,
                0,
                EventScope::job("build"),
                LogEvent::JobStarted {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                },
            ),
            at(
                1,
                1,
                EventScope::step("build", 0),
                LogEvent::StepStarted {
                    job_id: "build".into(),
                    step_index: 0,
                    step_name: "Compile".into(),
                },
            ),
        ];

        let view = RunView::from_records(&records);
        assert_eq!(view.jobs[0].conclusion, None);
        assert_eq!(view.jobs[0].steps[0].conclusion, None);
        assert_eq!(view.jobs[0].steps[0].duration_ms, None);
    }

    #[test]
    fn a_failed_job_marks_the_step_it_died_on() {
        let records = vec![
            at(
                0,
                0,
                EventScope::job("build"),
                LogEvent::JobStarted {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                },
            ),
            at(
                1,
                0,
                EventScope::step("build", 0),
                LogEvent::StepStarted {
                    job_id: "build".into(),
                    step_index: 0,
                    step_name: "Compile".into(),
                },
            ),
            at(
                2,
                2,
                EventScope::job("build"),
                LogEvent::JobFinished {
                    job_id: "build".into(),
                    job_name: "Build".into(),
                    success: false,
                    conclusion: StepConclusion::Failure,
                },
            ),
        ];

        let view = RunView::from_records(&records);
        assert_eq!(
            view.jobs[0].steps[0].conclusion,
            Some(StepConclusion::Failure),
        );
    }

    #[test]
    fn matrix_instances_become_separate_jobs() {
        let mut records = Vec::new();
        for (seq, instance) in ["build (os=linux)", "build (os=macos)"].iter().enumerate() {
            records.push(at(
                seq as u64,
                seq as i64,
                EventScope::job(*instance),
                LogEvent::JobStarted {
                    job_id: (*instance).to_string(),
                    job_name: (*instance).to_string(),
                },
            ));
        }

        let view = RunView::from_records(&records);
        assert_eq!(view.jobs.len(), 2);
        assert_eq!(view.jobs[0].id, "build (os=linux)");
        assert_eq!(view.jobs[1].id, "build (os=macos)");
    }

    #[test]
    fn a_skipped_job_records_why() {
        let records = vec![at(
            0,
            0,
            EventScope::job("deploy"),
            LogEvent::JobSkipped {
                job_id: "deploy".into(),
                job_name: "Deploy".into(),
                condition: "success()".into(),
            },
        )];

        let view = RunView::from_records(&records);
        assert_eq!(view.jobs[0].conclusion, Some(StepConclusion::Skipped));
        assert_eq!(view.jobs[0].note.as_deref(), Some("if: success()"));
    }
}
