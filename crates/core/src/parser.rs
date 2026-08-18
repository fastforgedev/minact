//! Workflow file parser — loads and validates workflow YAML files.

use std::path::{Path, PathBuf};

use crate::types::WorkflowError;
use crate::workflow::*;

/// One location discovery looks in, relative to the project directory.
///
/// Paths are owned so an embedder can build them at runtime — a tool that
/// keeps workflows under its own directory passes that directory in rather
/// than needing minact to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPath {
    /// A directory whose `*.yml` / `*.yaml` files are all workflows.
    Directory(String),
    /// A single workflow file.
    File(String),
}

impl SearchPath {
    /// A directory of workflow files.
    pub fn dir(path: impl Into<String>) -> Self {
        SearchPath::Directory(path.into())
    }

    /// A single workflow file.
    pub fn file(path: impl Into<String>) -> Self {
        SearchPath::File(path.into())
    }

    /// The path as written, without the trailing slash a directory displays.
    pub fn as_str(&self) -> &str {
        match self {
            SearchPath::Directory(path) | SearchPath::File(path) => path,
        }
    }

    /// How this location reads in a "looked in" message.
    pub fn display(&self) -> String {
        match self {
            SearchPath::Directory(dir) => format!("{}/", dir),
            SearchPath::File(file) => file.to_string(),
        }
    }
}

/// Discover and parse workflow files from a project.
pub struct WorkflowParser;

impl WorkflowParser {
    /// Parse a workflow from a YAML string.
    pub fn parse_yaml(yaml: &str, file_path: Option<PathBuf>) -> Result<Workflow, WorkflowError> {
        let mut workflow: Workflow = serde_yaml::from_str(yaml)
            .map_err(|e| WorkflowError::ParseError(format!("YAML parse error: {}", e)))?;
        workflow.file_path = file_path;

        // Validate workflow structure
        Self::validate(&workflow)?;

        Ok(workflow)
    }

    /// Parse a workflow from a YAML file path.
    pub fn parse_file(path: &Path) -> Result<Workflow, WorkflowError> {
        let yaml = std::fs::read_to_string(path).map_err(WorkflowError::IoError)?;
        let file_path = Some(path.to_path_buf());
        Self::parse_yaml(&yaml, file_path)
    }

    /// Discover all workflow files in a project directory, looking in
    /// [`WorkflowParser::default_search_paths`].
    pub fn discover_workflows(project_dir: &Path) -> Result<Vec<Workflow>, WorkflowError> {
        Self::discover_workflows_in(project_dir, &Self::default_search_paths())
    }

    /// The locations minact searches when the caller does not say otherwise:
    /// minact's own directory, and GitHub's for drop-in compatibility.
    ///
    /// Both are directories. A single `minact.yml` at the project root used to
    /// be searched too, but `<tool>.yml` reads as *configuration* everywhere
    /// else, and it collided with `.minact/config.yml` badly enough to be
    /// worth losing.
    ///
    /// A tool that keeps workflows somewhere else passes its own list to
    /// [`WorkflowParser::discover_workflows_in`] rather than expecting minact
    /// to know about it.
    pub fn default_search_paths() -> Vec<SearchPath> {
        vec![
            SearchPath::dir(".minact/workflows"),
            SearchPath::dir(".github/workflows"),
        ]
    }

    /// Discover workflow files in specific locations.
    ///
    /// Embedders that own their own layout — for example a tool that only
    /// recognises `.mytool/workflows/` — can pass their own search paths
    /// instead of inheriting minact's defaults.
    pub fn discover_workflows_in(
        project_dir: &Path,
        search_paths: &[SearchPath],
    ) -> Result<Vec<Workflow>, WorkflowError> {
        let mut workflows = Vec::new();

        for search_path in search_paths {
            match search_path {
                SearchPath::Directory(dir) => {
                    let dir = project_dir.join(dir);
                    if dir.exists() {
                        workflows.extend(Self::find_yaml_files(&dir)?);
                    }
                }
                SearchPath::File(file) => {
                    let path = project_dir.join(file);
                    if path.exists() {
                        workflows.push(Self::parse_file(&path)?);
                    }
                }
            }
        }

        Ok(workflows)
    }

    /// Render the search locations for a "no workflows found" message.
    pub fn search_path_summary(search_paths: &[SearchPath]) -> String {
        search_paths
            .iter()
            .map(SearchPath::display)
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn find_yaml_files(dir: &Path) -> Result<Vec<Workflow>, WorkflowError> {
        let mut paths = Vec::new();

        for ext in &["yml", "yaml"] {
            let pattern = dir.join(format!("*.{}", ext));
            if let Some(pattern_str) = pattern.to_str() {
                if let Ok(entries) = glob::glob(pattern_str) {
                    paths.extend(entries.flatten());
                }
            }
        }

        // Glob order is filesystem order; sort so a run is reproducible and
        // `minact list` is stable.
        paths.sort();

        let mut workflows = Vec::new();
        for path in paths {
            match Self::parse_file(&path) {
                Ok(wf) => workflows.push(wf),
                Err(e) => {
                    tracing::warn!("Failed to parse {}: {}", path.display(), e);
                }
            }
        }

        Ok(workflows)
    }

    /// Validate a parsed workflow structure.
    fn validate(workflow: &Workflow) -> Result<(), WorkflowError> {
        if workflow.jobs.is_empty() {
            return Err(WorkflowError::ParseError(
                "Workflow must have at least one job".to_string(),
            ));
        }

        for (job_id, job) in &workflow.jobs {
            if job.steps.is_empty() {
                return Err(WorkflowError::ParseError(format!(
                    "Job '{}' must have at least one step",
                    job_id
                )));
            }

            for (step_idx, step) in job.steps.iter().enumerate() {
                // Step must have either uses or run
                if step.uses.is_none() && step.run.is_none() {
                    return Err(WorkflowError::ParseError(format!(
                        "Step {} in job '{}' must have either 'uses' or 'run'",
                        step_idx + 1,
                        job_id
                    )));
                }

                // If it has uses, it shouldn't have run (and vice versa)
                if step.uses.is_some() && step.run.is_some() {
                    return Err(WorkflowError::ParseError(format!(
                        "Step {} in job '{}' cannot have both 'uses' and 'run'",
                        step_idx + 1,
                        job_id
                    )));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_workflow() {
        let yaml = r#"
name: Test Workflow
on:
  push:
    branches: [main]
  workflow_dispatch:

env:
  FOO: bar

jobs:
  build:
    name: Build
    runs-on: ubuntu-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4
      - name: Run a script
        run: echo "Hello, world!"
        shell: bash
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: my-artifact
          path: ./dist
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        assert_eq!(workflow.name, "Test Workflow");
        assert!(workflow.on.push.is_some());
        assert!(workflow.on.workflow_dispatch.is_some());
        assert_eq!(workflow.env.get("FOO").unwrap(), "bar");
        assert_eq!(workflow.jobs.len(), 1);
        assert_eq!(workflow.jobs["build"].steps.len(), 3);
    }

    #[test]
    fn test_parse_release_workflow() {
        let yaml = r#"
name: Release Pipeline
on:
  release:
    types: [published]
  workflow_dispatch:
    inputs:
      version:
        description: "Version to release"
        required: true

env:
  CARGO_TERM_COLOR: always

jobs:
  package:
    name: Package
    steps:
      - name: Checkout
        uses: actions/checkout@v4
      - name: Build
        run: cargo build --release
  publish:
    name: Publish
    needs: [package]
    if: github.event_name == 'release'
    steps:
      - name: Publish
        run: cargo publish
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        assert_eq!(workflow.name, "Release Pipeline");
        assert!(workflow.on.release.is_some());
        assert!(workflow.on.workflow_dispatch.is_some());
        let dispatch = workflow.on.workflow_dispatch.as_ref().unwrap();
        assert!(dispatch.inputs.is_some());
        assert_eq!(
            dispatch.inputs.as_ref().unwrap()["version"].description,
            "Version to release"
        );
        assert!(dispatch.inputs.as_ref().unwrap()["version"].required);
        assert_eq!(workflow.jobs.len(), 2);
        assert!(workflow.jobs["publish"].if_condition.is_some());
    }

    #[test]
    fn test_parse_error_empty_jobs() {
        let yaml = r#"
name: Empty Workflow
on: workflow_dispatch
jobs: {}
"#;
        let result = WorkflowParser::parse_yaml(yaml, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one job"));
    }

    #[test]
    fn test_discover_no_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = WorkflowParser::discover_workflows(dir.path()).unwrap();
        assert!(workflows.is_empty());
    }

    #[test]
    fn test_discover_minact_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join(".minact").join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();

        let yaml = r#"
name: CI
on: push
jobs:
  test:
    name: Test
    steps:
      - run: echo test
"#;
        std::fs::write(workflows_dir.join("ci.yml"), yaml).unwrap();

        let workflows = WorkflowParser::discover_workflows(dir.path()).unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].name, "CI");
    }

    /// Every advertised search path must actually be searched. This is what
    /// keeps the default list — and therefore the CLI's "Looked in"
    /// message — honest.
    #[test]
    fn test_every_default_search_path_is_discovered() {
        for search_path in &WorkflowParser::default_search_paths() {
            let dir = tempfile::tempdir().unwrap();
            let name = format!("Workflow at {}", search_path.display());
            let yaml = format!(
                "name: {}\non: push\njobs:\n  test:\n    steps:\n      - run: echo test\n",
                name
            );

            let target = match search_path {
                SearchPath::Directory(rel) => {
                    let dir = dir.path().join(rel);
                    std::fs::create_dir_all(&dir).unwrap();
                    dir.join("workflow.yml")
                }
                SearchPath::File(rel) => dir.path().join(rel),
            };
            std::fs::write(&target, yaml).unwrap();

            let workflows = WorkflowParser::discover_workflows(dir.path()).unwrap();
            assert_eq!(
                workflows.len(),
                1,
                "nothing discovered in {}",
                search_path.display()
            );
            assert_eq!(workflows[0].name, name);
        }
    }

    /// The defaults describe minact's own layout and GitHub's, and nothing
    /// else. Anything belonging to a particular downstream tool is that tool's
    /// to pass in, so a new entry here should be a deliberate decision.
    #[test]
    fn test_defaults_are_only_minact_and_github() {
        let defaults: Vec<String> = WorkflowParser::default_search_paths()
            .iter()
            .map(|path| path.as_str().to_string())
            .collect();

        assert_eq!(defaults, vec![".minact/workflows", ".github/workflows"]);
    }

    #[test]
    fn test_search_path_summary_covers_every_default() {
        let defaults = WorkflowParser::default_search_paths();
        let summary = WorkflowParser::search_path_summary(&defaults);
        for search_path in &defaults {
            assert!(
                summary.contains(&search_path.display()),
                "{} missing from: {}",
                search_path.display(),
                summary
            );
        }
    }

    #[test]
    fn test_discovery_is_ordered_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join(".minact").join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();

        for name in ["zebra", "alpha", "middle"] {
            let yaml = format!(
                "name: {}\non: push\njobs:\n  test:\n    steps:\n      - run: echo test\n",
                name
            );
            std::fs::write(workflows_dir.join(format!("{}.yml", name)), yaml).unwrap();
        }

        let names: Vec<String> = WorkflowParser::discover_workflows(dir.path())
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    /// No default is a single file any more, so this is the only thing keeping
    /// `SearchPath::File` working for callers that do want one.
    #[test]
    fn test_a_caller_can_still_name_a_single_workflow_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pipeline.yml"),
            "name: Root\non: push\njobs:\n  test:\n    steps:\n      - run: echo test\n",
        )
        .unwrap();

        // Not a default location, so nothing is found by default...
        assert!(WorkflowParser::discover_workflows(dir.path())
            .unwrap()
            .is_empty());

        // ...but a caller can point at the file itself.
        let workflows =
            WorkflowParser::discover_workflows_in(dir.path(), &[SearchPath::file("pipeline.yml")])
                .unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].name, "Root");
    }

    #[test]
    fn test_discovery_can_be_restricted_to_custom_paths() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "name: Custom\non: push\njobs:\n  test:\n    steps:\n      - run: echo test\n";

        // A workflow in a default location...
        let default_dir = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&default_dir).unwrap();
        std::fs::write(default_dir.join("ci.yml"), yaml).unwrap();

        // ...is invisible when the caller supplies its own search paths.
        let workflows = WorkflowParser::discover_workflows_in(
            dir.path(),
            &[SearchPath::dir(".mytool/workflows")],
        )
        .unwrap();
        assert!(workflows.is_empty());
    }

    /// `runs-on` is hyphenated in YAML but `runs_on` in Rust; without the
    /// rename it silently parsed as `None`, so every job looked unplaced and
    /// the runner mapping could never fire.
    #[test]
    fn test_runs_on_is_parsed() {
        let yaml = r#"
name: RunsOn
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        assert_eq!(
            workflow.jobs["build"].runs_on.as_deref(),
            Some("ubuntu-latest")
        );
    }

    #[test]
    fn test_runs_on_accepts_every_github_shape() {
        let yaml = r#"
name: RunsOn
on: workflow_dispatch
jobs:
  plain:
    runs-on: macos-latest
    steps:
      - run: echo x
  list:
    runs-on: [self-hosted, linux, x64]
    steps:
      - run: echo x
  group:
    runs-on:
      group: my-group
      labels: [gpu, large]
    steps:
      - run: echo x
  absent:
    steps:
      - run: echo x
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let jobs = &workflow.jobs;

        assert_eq!(jobs["plain"].runs_on.as_deref(), Some("macos-latest"));
        // A list collapses to the first label, which is what a mapping keys on.
        assert_eq!(jobs["list"].runs_on.as_deref(), Some("self-hosted"));
        assert_eq!(jobs["group"].runs_on.as_deref(), Some("gpu"));
        assert_eq!(jobs["absent"].runs_on, None);
    }

    #[test]
    fn test_needs_accepts_a_bare_string() {
        let yaml = r#"
name: Needs
on: workflow_dispatch
jobs:
  build:
    steps:
      - run: echo build
  test:
    needs: build
    steps:
      - run: echo test
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        assert_eq!(
            workflow.jobs["test"].needs.as_deref(),
            Some(["build".to_string()].as_slice())
        );
    }

    #[test]
    fn test_needs_accepts_a_sequence() {
        let yaml = r#"
name: Needs
on: workflow_dispatch
jobs:
  build:
    steps:
      - run: echo build
  lint:
    steps:
      - run: echo lint
  test:
    needs: [build, lint]
    steps:
      - run: echo test
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        let needs = workflow.jobs["test"].needs.clone().unwrap();
        assert_eq!(needs, vec!["build".to_string(), "lint".to_string()]);
    }

    #[test]
    fn test_env_and_with_accept_non_string_scalars() {
        let yaml = r#"
name: Scalars
on: workflow_dispatch
env:
  RETRIES: 3
  VERBOSE: true
  EMPTY:
jobs:
  build:
    env:
      TIMEOUT: 12.5
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          lfs: false
"#;
        let workflow = WorkflowParser::parse_yaml(yaml, None).unwrap();
        assert_eq!(workflow.env["RETRIES"], "3");
        assert_eq!(workflow.env["VERBOSE"], "true");
        assert_eq!(workflow.env["EMPTY"], "");
        assert_eq!(workflow.jobs["build"].env["TIMEOUT"], "12.5");

        let with = &workflow.jobs["build"].steps[0].with;
        assert_eq!(with["fetch-depth"], "0");
        assert_eq!(with["lfs"], "false");
    }
}
