//! Serializable views over the core types.
//!
//! `minact_core`'s own structs are shaped for execution, not for a UI: jobs
//! live in a `HashMap`, several result types have no `Serialize`, and nothing
//! carries the workspace-relative paths the front-end addresses things by.
//! These DTOs are the API's contract, deliberately decoupled from the engine's
//! internals.

use std::collections::BTreeMap;

use minact_core::workflow::{OnConfig, Step, Workflow};
use minact_core::{runner_arch_name, runner_os_name, JobScheduler};
use serde::Serialize;

use crate::discovery::Discovered;

#[derive(Debug, Serialize)]
pub struct MetaDto {
    pub version: String,
    pub workspace: String,
    pub runner: RunnerDto,
    /// Actions the server can resolve, including any a host app registered.
    pub actions: Vec<String>,
    pub workflow_count: usize,
}

#[derive(Debug, Serialize)]
pub struct RunnerDto {
    pub os: String,
    pub arch: String,
}

impl RunnerDto {
    pub fn current() -> Self {
        Self {
            os: runner_os_name(),
            arch: runner_arch_name(),
        }
    }
}

/// One row in the workflow list.
#[derive(Debug, Serialize)]
pub struct WorkflowSummaryDto {
    pub id: String,
    pub path: String,
    pub source: String,
    /// Falls back to the file name for workflows with no `name:`.
    pub name: String,
    pub triggers: Vec<String>,
    pub job_count: usize,
    pub step_count: usize,
    /// Present when the file could not be parsed; every count is 0 then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowSummaryDto {
    pub fn from_discovered(found: &Discovered) -> Self {
        let base = Self {
            id: found.id.clone(),
            path: found.rel_path.clone(),
            source: found.source.clone(),
            name: file_stem(&found.rel_path),
            triggers: Vec::new(),
            job_count: 0,
            step_count: 0,
            error: None,
        };

        match &found.parsed {
            Err(message) => Self {
                error: Some(message.clone()),
                ..base
            },
            Ok(workflow) => Self {
                name: display_name(workflow, &found.rel_path),
                triggers: triggers_of(&workflow.on),
                job_count: workflow.jobs.len(),
                step_count: workflow.jobs.values().map(|job| job.steps.len()).sum(),
                ..base
            },
        }
    }
}

/// Everything the workflow detail screen needs in one request.
#[derive(Debug, Serialize)]
pub struct WorkflowDetailDto {
    #[serde(flatten)]
    pub summary: WorkflowSummaryDto,
    /// The file as it is on disk — the editor and the YAML tab both read this.
    pub yaml: String,
    pub env: BTreeMap<String, String>,
    pub jobs: Vec<JobDto>,
    pub graph: GraphDto,
    pub dispatch_inputs: Vec<DispatchInputDto>,
}

#[derive(Debug, Serialize)]
pub struct JobDto {
    pub id: String,
    pub name: String,
    pub needs: Vec<String>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<String>,
    pub env: BTreeMap<String, String>,
    pub steps: Vec<StepDto>,
}

#[derive(Debug, Serialize)]
pub struct StepDto {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    pub with: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
}

impl StepDto {
    fn from_step(index: usize, step: &Step) -> Self {
        Self {
            index,
            id: step.id.clone(),
            name: step_label(index, step),
            uses: step.uses.clone(),
            run: step.run.clone(),
            shell: step.shell.clone(),
            working_directory: step.working_directory.clone(),
            if_condition: step.if_condition.clone(),
            continue_on_error: step.continue_on_error,
            with: step.with.clone().into_iter().collect(),
            env: step.env.clone().into_iter().collect(),
        }
    }
}

/// The job DAG, laid out the way the engine will execute it.
#[derive(Debug, Serialize)]
pub struct GraphDto {
    /// Job ids grouped into the layers the scheduler resolved. Empty when the
    /// graph has a cycle — `error` says so.
    pub layers: Vec<Vec<String>>,
    pub edges: Vec<EdgeDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EdgeDto {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct DispatchInputDto {
    pub name: String,
    pub description: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
}

impl WorkflowDetailDto {
    pub fn build(found: &Discovered, workflow: &Workflow, yaml: String) -> Self {
        let mut jobs: Vec<JobDto> = workflow
            .jobs
            .iter()
            .map(|(job_id, job)| JobDto {
                id: job_id.clone(),
                name: if job.name.is_empty() {
                    job_id.clone()
                } else {
                    job.name.clone()
                },
                needs: job.needs.clone().unwrap_or_default(),
                if_condition: job.if_condition.clone(),
                runs_on: job.runs_on.clone(),
                env: job.env.clone().into_iter().collect(),
                steps: job
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| StepDto::from_step(index, step))
                    .collect(),
            })
            .collect();

        let graph = GraphDto::build(workflow);

        // Order jobs the way they will run, so the detail page and the graph
        // agree; anything outside the layers (a cycle) sorts last by id.
        let order: BTreeMap<&str, usize> = graph
            .layers
            .iter()
            .flatten()
            .enumerate()
            .map(|(position, job_id)| (job_id.as_str(), position))
            .collect();
        jobs.sort_by(|a, b| {
            let rank = |id: &str| order.get(id).copied().unwrap_or(usize::MAX);
            rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id))
        });

        Self {
            summary: WorkflowSummaryDto::from_discovered(found),
            yaml,
            env: workflow.env.clone().into_iter().collect(),
            jobs,
            graph,
            dispatch_inputs: dispatch_inputs_of(&workflow.on),
        }
    }
}

impl GraphDto {
    pub fn build(workflow: &Workflow) -> Self {
        let mut edges: Vec<EdgeDto> = workflow
            .jobs
            .iter()
            .flat_map(|(job_id, job)| {
                job.needs
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |need| EdgeDto {
                        from: need,
                        to: job_id.clone(),
                    })
            })
            .collect();
        edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));

        match JobScheduler::new(workflow).resolve_parallel_layers() {
            // The scheduler returns each layer already sorted, so the plan a
            // workflow shows and the plan a run executed always agree.
            Ok(layers) => Self {
                layers,
                edges,
                error: None,
            },
            Err(err) => Self {
                layers: Vec::new(),
                edges,
                error: Some(err.to_string()),
            },
        }
    }
}

fn display_name(workflow: &Workflow, rel_path: &str) -> String {
    if workflow.name.trim().is_empty() {
        file_stem(rel_path)
    } else {
        workflow.name.clone()
    }
}

fn file_stem(rel_path: &str) -> String {
    rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| rel_path.to_string())
}

fn step_label(index: usize, step: &Step) -> String {
    if !step.name.trim().is_empty() {
        return step.name.clone();
    }
    if let Some(uses) = &step.uses {
        return uses.clone();
    }
    if let Some(run) = &step.run {
        // Same fallback the CLI uses: the first line of the command.
        if let Some(first) = run.lines().find(|line| !line.trim().is_empty()) {
            return first.trim().to_string();
        }
    }
    format!("Step {}", index + 1)
}

fn triggers_of(on: &OnConfig) -> Vec<String> {
    let mut triggers = Vec::new();
    if on.push.is_some() {
        triggers.push("push".to_string());
    }
    if on.pull_request.is_some() {
        triggers.push("pull_request".to_string());
    }
    if on.release.is_some() {
        triggers.push("release".to_string());
    }
    if on.workflow_dispatch.is_some() {
        triggers.push("workflow_dispatch".to_string());
    }
    if on.schedule.is_some() {
        triggers.push("schedule".to_string());
    }
    let mut extra: Vec<String> = on.extra.keys().cloned().collect();
    extra.sort();
    triggers.extend(extra);
    triggers
}

fn dispatch_inputs_of(on: &OnConfig) -> Vec<DispatchInputDto> {
    let Some(dispatch) = &on.workflow_dispatch else {
        return Vec::new();
    };
    let Some(inputs) = &dispatch.inputs else {
        return Vec::new();
    };

    let mut result: Vec<DispatchInputDto> = inputs
        .iter()
        .map(|(name, input)| DispatchInputDto {
            name: name.clone(),
            description: input.description.clone(),
            required: input.required,
            default: input.default.clone(),
            input_type: input.input_type.clone(),
        })
        .collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use minact_core::WorkflowParser;

    const YAML: &str = r#"
name: CI
on:
  push:
    branches: [main]
  workflow_dispatch:
    inputs:
      version:
        description: Version to build
        required: false
        default: "1.0.0"
jobs:
  deploy:
    needs: [build, test]
    steps:
      - run: echo deploy
  build:
    needs: setup
    steps:
      - name: Compile
        run: cargo build
  test:
    needs: setup
    steps:
      - run: |
          cargo test
          echo done
  setup:
    steps:
      - uses: actions/checkout@v4
"#;

    fn workflow() -> Workflow {
        WorkflowParser::parse_yaml(YAML, None).unwrap()
    }

    #[test]
    fn graph_layers_follow_the_scheduler_and_are_stable() {
        let graph = GraphDto::build(&workflow());
        assert_eq!(
            graph.layers,
            vec![
                vec!["setup".to_string()],
                vec!["build".to_string(), "test".to_string()],
                vec!["deploy".to_string()],
            ]
        );
        assert!(graph.error.is_none());
        assert_eq!(graph.edges.len(), 4);
        assert_eq!(graph.edges[0].from, "build");
        assert_eq!(graph.edges[0].to, "deploy");
    }

    #[test]
    fn cycles_report_an_error_instead_of_layers() {
        let cyclic = WorkflowParser::parse_yaml(
            "name: Loop\non: push\njobs:\n  a:\n    needs: [b]\n    steps:\n      - run: echo a\n  b:\n    needs: [a]\n    steps:\n      - run: echo b\n",
            None,
        )
        .unwrap();

        let graph = GraphDto::build(&cyclic);
        assert!(graph.layers.is_empty());
        assert!(graph.error.is_some());
        // Edges still render, so the UI can show what the cycle is.
        assert_eq!(graph.edges.len(), 2);
    }

    #[test]
    fn triggers_and_dispatch_inputs_are_extracted() {
        let workflow = workflow();
        assert_eq!(
            triggers_of(&workflow.on),
            vec!["push".to_string(), "workflow_dispatch".to_string()]
        );

        let inputs = dispatch_inputs_of(&workflow.on);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "version");
        assert_eq!(inputs[0].default.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn unnamed_steps_fall_back_to_uses_then_the_first_run_line() {
        let workflow = workflow();

        let setup = &workflow.jobs["setup"];
        assert_eq!(
            step_label(0, &setup.steps[0]),
            "actions/checkout@v4".to_string()
        );

        let test = &workflow.jobs["test"];
        assert_eq!(step_label(0, &test.steps[0]), "cargo test".to_string());
    }
}
