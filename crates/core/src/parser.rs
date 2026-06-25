//! Workflow file parser — loads and validates workflow YAML files.

use std::path::{Path, PathBuf};

use crate::workflow::*;
use crate::types::WorkflowError;

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
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| WorkflowError::IoError(e))?;
        let file_path = Some(path.to_path_buf());
        Self::parse_yaml(&yaml, file_path)
    }

    /// Discover all workflow files in a project directory.
    ///
    /// Search path:
    /// - `.fastforge/workflows/*.yml` or `.fastforge/workflows/*.yaml`
    pub fn discover_workflows(project_dir: &Path) -> Result<Vec<Workflow>, WorkflowError> {
        let mut workflows = Vec::new();

        // Search in .fastforge/workflows/
        let fastforge_workflows = project_dir.join(".fastforge").join("workflows");
        if fastforge_workflows.exists() {
            workflows.extend(Self::find_yaml_files(&fastforge_workflows)?);
        }

        Ok(workflows)
    }

    fn find_yaml_files(dir: &Path) -> Result<Vec<Workflow>, WorkflowError> {
        let mut workflows = Vec::new();

        for ext in &["yml", "yaml"] {
            let pattern = dir.join(format!("*.{}", ext));
            if let Some(pattern_str) = pattern.to_str() {
                if let Ok(entries) = glob::glob(pattern_str) {
                    for entry in entries.flatten() {
                        match Self::parse_file(&entry) {
                            Ok(wf) => workflows.push(wf),
                            Err(e) => {
                                tracing::warn!("Failed to parse {}: {}", entry.display(), e);
                            }
                        }
                    }
                }
            }
        }

        Ok(workflows)
    }

    /// Validate a parsed workflow structure.
    fn validate(workflow: &Workflow) -> Result<(), WorkflowError> {
        if workflow.jobs.is_empty() {
            return Err(WorkflowError::ParseError(
                "Workflow must have at least one job".to_string()
            ));
        }

        for (job_id, job) in &workflow.jobs {
            if job.steps.is_empty() {
                return Err(WorkflowError::ParseError(format!(
                    "Job '{}' must have at least one step", job_id
                )));
            }

            for (step_idx, step) in job.steps.iter().enumerate() {
                // Step must have either uses or run
                if step.uses.is_none() && step.run.is_none() {
                    return Err(WorkflowError::ParseError(format!(
                        "Step {} in job '{}' must have either 'uses' or 'run'",
                        step_idx + 1, job_id
                    )));
                }

                // If it has uses, it shouldn't have run (and vice versa)
                if step.uses.is_some() && step.run.is_some() {
                    return Err(WorkflowError::ParseError(format!(
                        "Step {} in job '{}' cannot have both 'uses' and 'run'",
                        step_idx + 1, job_id
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
        assert_eq!(dispatch.inputs.as_ref().unwrap()["version"].description, "Version to release");
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
}
