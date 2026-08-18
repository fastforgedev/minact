//! Running jobs somewhere other than this machine.
//!
//! The Docker tests need a working container runtime and are skipped when
//! there is none, so the suite still passes on a machine without Docker.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use common::{messages_at, stdout, CollectingReporter};
use minact_core::executor::docker;
use minact_core::{
    Config, Engine, EngineResult, LogEvent, LogLevel, StepConclusion, WorkflowParser,
};

const IMAGE: &str = "ubuntu:24.04";

async fn docker_ready() -> bool {
    if !docker::is_available("docker").await {
        eprintln!("skipping: no docker daemon");
        return false;
    }
    // A missing image would otherwise look like a bug in the executor.
    match tokio::process::Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .output()
        .await
    {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!("skipping: {} is not pulled locally", IMAGE);
            false
        }
    }
}

/// Run a workflow with a runner mapping.
async fn run_with_runners(
    yaml: &str,
    runners_yaml: &str,
    workspace: &Path,
) -> (EngineResult, Vec<LogEvent>) {
    let workflow = WorkflowParser::parse_yaml(yaml, None).expect("workflow should parse");
    let runners: Config = serde_yaml::from_str(runners_yaml).expect("runner config should parse");

    let reporter = Arc::new(CollectingReporter::default());
    let engine =
        Engine::with_reporter(workspace.to_path_buf(), reporter.clone()).with_config(runners);
    let result = engine
        .run_workflow(&workflow, "workflow_dispatch", HashMap::new())
        .await
        .expect("workflow should run");
    (result, reporter.events())
}

// ---------------------------------------------------------------------------
// Falling back to this machine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unmapped_foreign_label_warns_instead_of_pretending() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Unmapped
on: workflow_dispatch
jobs:
  build:
    runs-on: windows-latest
    steps:
      - run: echo ran
"#;

    let (result, events) = run_with_runners(yaml, "runners: {}", dir.path()).await;

    assert!(result.success, "the job still runs, locally");
    let warnings = messages_at(&events, LogLevel::Warn).join("\n");
    assert!(
        warnings.contains("windows-latest") && warnings.contains("no runner configured"),
        "an unmapped foreign runner must be reported, got: {}",
        warnings
    );
}

#[tokio::test]
async fn a_label_matching_this_host_is_not_worth_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let host_label = if cfg!(target_os = "macos") {
        "macos-latest"
    } else if cfg!(target_os = "linux") {
        "ubuntu-latest"
    } else {
        "windows-latest"
    };
    let yaml = format!(
        r#"
name: Host
on: workflow_dispatch
jobs:
  build:
    runs-on: {}
    steps:
      - run: echo ran
"#,
        host_label
    );

    let (result, events) = run_with_runners(&yaml, "runners: {}", dir.path()).await;

    assert!(result.success);
    let warnings = messages_at(&events, LogLevel::Warn).join("\n");
    assert!(
        !warnings.contains("no runner configured"),
        "running a {} job here needs no explanation, got: {}",
        host_label,
        warnings
    );
}

#[tokio::test]
async fn an_explicit_local_runner_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Mapped local
on: workflow_dispatch
jobs:
  build:
    runs-on: windows-latest
    steps:
      - run: echo ran
"#;

    let (result, events) = run_with_runners(
        yaml,
        "runners:\n  windows-latest:\n    type: local\n",
        dir.path(),
    )
    .await;

    assert!(result.success);
    let warnings = messages_at(&events, LogLevel::Warn).join("\n");
    assert!(!warnings.contains("no runner configured"), "{}", warnings);
}

// ---------------------------------------------------------------------------
// Actually running elsewhere
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_linux_job_really_runs_on_linux() {
    if !docker_ready().await {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Linux
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: |
          echo "kernel=$(uname -s)"
          echo "workspace=$GITHUB_WORKSPACE"
"#;

    let (result, events) = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    assert!(result.success, "{}", stdout(&events));
    let out = stdout(&events);
    assert!(
        out.contains("kernel=Linux"),
        "the step should run on Linux, got: {}",
        out
    );
    // The workspace keeps its host path, which is what lets the rest of the
    // engine stay unaware of the container.
    assert!(
        out.contains(&format!("workspace={}", dir.path().display())),
        "{}",
        out
    );
}

#[tokio::test]
async fn the_workspace_is_shared_with_the_container() {
    if !docker_ready().await {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("from-host.txt"), "host wrote this").unwrap();

    let yaml = r#"
name: Shared workspace
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: |
          cat from-host.txt
          echo "container wrote this" > from-container.txt
"#;

    let (result, events) = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    assert!(result.success, "{}", stdout(&events));
    assert!(stdout(&events).contains("host wrote this"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("from-container.txt"))
            .unwrap()
            .trim(),
        "container wrote this",
        "what the container wrote should be on the host"
    );
}

#[tokio::test]
async fn environment_files_work_across_steps_in_a_container() {
    if !docker_ready().await {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Container data flow
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      ver: ${{ steps.meta.outputs.ver }}
    steps:
      - id: meta
        run: |
          echo "ver=9.9.9" >> "$GITHUB_OUTPUT"
          echo "SHARED=from-step-one" >> "$GITHUB_ENV"
      - run: |
          echo "output=${{ steps.meta.outputs.ver }}"
          echo "env=$SHARED"
  after:
    needs: [build]
    steps:
      - run: |
          echo "needs=${{ needs.build.outputs.ver }}"
"#;

    let (result, events) = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    assert!(result.success, "{}", stdout(&events));
    let out = stdout(&events);
    assert!(out.contains("output=9.9.9"), "{}", out);
    assert!(out.contains("env=from-step-one"), "{}", out);
    // The dependent job runs locally, but still reads the container's outputs.
    assert!(out.contains("needs=9.9.9"), "{}", out);
}

#[tokio::test]
async fn a_failing_container_step_fails_the_job() {
    if !docker_ready().await {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Container failure
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: exit 3
      - run: echo "should not run"
      - if: always()
        run: echo "cleanup ran"
"#;

    let (result, events) = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build"].conclusion,
        StepConclusion::Failure
    );
    let out = stdout(&events);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(out.contains("cleanup ran"), "{}", out);
}

#[tokio::test]
async fn containers_do_not_outlive_the_job() {
    if !docker_ready().await {
        return;
    }

    let before = running_containers().await;

    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: Cleanup
on: workflow_dispatch
jobs:
  ok:
    runs-on: ubuntu-latest
    steps:
      - run: echo fine
  broken:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
"#;

    let _ = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    // Even the failing job must take its container with it.
    assert_eq!(
        running_containers().await,
        before,
        "a container outlived its job"
    );
}

async fn running_containers() -> usize {
    let output = tokio::process::Command::new("docker")
        .args(["ps", "--quiet", "--filter", &format!("ancestor={}", IMAGE)])
        .output()
        .await
        .expect("docker ps should run");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[tokio::test]
async fn a_matrix_can_span_runners() {
    if !docker_ready().await {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    // The shape a cross-platform build actually takes: one job, one instance
    // per target, each landing on a different runner.
    let yaml = r#"
name: Matrix across runners
on: workflow_dispatch
jobs:
  build:
    runs-on: ${{ matrix.on }}
    strategy:
      fail-fast: false
      matrix:
        on: [ubuntu-latest, host-machine]
    steps:
      - run: |
          echo "${{ matrix.on }} -> $(uname -s)"
"#;

    let (result, events) = run_with_runners(
        yaml,
        &format!(
            "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n  host-machine:\n    type: local\n",
            IMAGE
        ),
        dir.path(),
    )
    .await;

    assert!(result.success, "{}", stdout(&events));
    let out = stdout(&events);
    assert!(out.contains("ubuntu-latest -> Linux"), "{}", out);

    let host_kernel = if cfg!(target_os = "macos") {
        "Darwin"
    } else {
        "Linux"
    };
    assert!(
        out.contains(&format!("host-machine -> {}", host_kernel)),
        "{}",
        out
    );
}

// ---------------------------------------------------------------------------
// SSH
// ---------------------------------------------------------------------------

/// End-to-end proof that the SSH backend works, against a real host.
///
/// Ignored by default because it needs infrastructure this repository cannot
/// provide: a reachable machine with key-based login already trusted. To run
/// it against your own box:
///
/// ```text
/// MINACT_SSH_HOST=user@build-box cargo test --test cross_platform -- --ignored
/// ```
///
/// Everything about the backend that can be checked without a remote host —
/// argument construction, path mapping and shell quoting — is covered by the
/// unit tests in `executor::ssh`.
#[tokio::test]
#[ignore = "needs a reachable SSH host; set MINACT_SSH_HOST"]
async fn a_job_runs_on_a_remote_host() {
    let Ok(destination) = std::env::var("MINACT_SSH_HOST") else {
        panic!("set MINACT_SSH_HOST=user@host to run this test");
    };
    let (user, host) = match destination.split_once('@') {
        Some((user, host)) => (Some(user.to_string()), host.to_string()),
        None => (None, destination.clone()),
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("from-host.txt"), "host wrote this").unwrap();

    let yaml = r#"
name: Remote
on: workflow_dispatch
jobs:
  build:
    runs-on: remote
    steps:
      - id: meta
        run: |
          cat from-host.txt
          echo "kernel=$(uname -s)"
          echo "ver=1.2.3" >> "$GITHUB_OUTPUT"
          echo "built-remotely" > from-remote.txt
      - run: |
          echo "output=${{ steps.meta.outputs.ver }}"
"#;

    let runners = format!(
        "runners:\n  remote:\n    type: ssh\n    host: {}\n{}    remote-workspace: /tmp/minact-test-workspace\n",
        host,
        user.map(|u| format!("    user: {}\n", u)).unwrap_or_default(),
    );

    let (result, events) = run_with_runners(yaml, &runners, dir.path()).await;

    assert!(result.success, "{}", stdout(&events));
    let out = stdout(&events);
    assert!(
        out.contains("host wrote this"),
        "workspace was not pushed: {}",
        out
    );
    assert!(out.contains("kernel="), "{}", out);
    assert!(
        out.contains("output=1.2.3"),
        "$GITHUB_OUTPUT did not come back: {}",
        out
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("from-remote.txt"))
            .unwrap()
            .trim(),
        "built-remotely",
        "the workspace was not synced back"
    );
}

#[tokio::test]
async fn an_action_runs_inside_the_job_container() {
    if !docker_ready().await {
        return;
    }
    let dir = tempfile::tempdir().expect("temp workspace");
    let action = dir.path().join("probe");
    std::fs::create_dir_all(&action).expect("action directory");
    std::fs::write(
        action.join("action.yml"),
        r#"
name: Probe
description: Reports where it ran
inputs:
  label:
    default: unset
runs:
  using: composite
  steps:
    - run: echo "action ran on $(uname -s) with ${{ inputs.label }}"
      shell: sh
"#,
    )
    .expect("action manifest");

    let yaml = r#"
name: Action In Container
on: workflow_dispatch
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./probe
        with:
          label: from-workflow
"#;
    let runners = format!(
        "runners:\n  ubuntu-latest:\n    type: docker\n    image: {}\n",
        IMAGE
    );

    let (result, events) = run_with_runners(yaml, &runners, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    // The action's steps went through the job's executor, so they ran on Linux
    // even when the suite is running on a Mac.
    assert!(
        stdout(&events).contains("action ran on Linux with from-workflow"),
        "{}",
        stdout(&events)
    );
}

// ---------------------------------------------------------------------------
// jobs.<id>.container
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_job_container_runs_every_step_inside_it() {
    if !docker_ready().await {
        return;
    }
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = format!(
        r#"
name: Job Container
on: workflow_dispatch
jobs:
  boxed:
    container:
      image: {}
      env:
        FROM_CONTAINER: yes
    steps:
      - run: echo "kernel=$(uname -s) env=$FROM_CONTAINER"
"#,
        IMAGE
    );

    let (result, events) = run_with_runners(&yaml, "runners: {}", dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    // `container:` decides where steps run even with no `runs-on:` mapping at
    // all, and its `env:` reaches every step.
    assert!(out.contains("kernel=Linux env=yes"), "{}", out);
}

#[tokio::test]
async fn a_container_shorthand_is_just_an_image() {
    if !docker_ready().await {
        return;
    }
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = format!(
        r#"
name: Shorthand
on: workflow_dispatch
jobs:
  boxed:
    container: {}
    steps:
      - run: echo "kernel=$(uname -s)"
"#,
        IMAGE
    );

    let (result, events) = run_with_runners(&yaml, "runners: {}", dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(
        stdout(&events).contains("kernel=Linux"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn services_are_reported_rather_than_ignored() {
    // No Docker needed: the point is that the job says it is not starting
    // them, instead of passing and meaning nothing.
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = r#"
name: Services
on: workflow_dispatch
jobs:
  needs-db:
    services:
      db:
        image: postgres:16
    steps:
      - run: echo "ran"
"#;

    let (result, events) = run_with_runners(yaml, "runners: {}", dir.path()).await;

    assert!(result.success);
    let warnings = messages_at(&events, LogLevel::Warn).join("\n");
    assert!(warnings.contains("services"), "{}", warnings);
    assert!(warnings.contains("db"), "{}", warnings);
}
