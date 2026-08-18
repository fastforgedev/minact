//! End-to-end behaviour of `strategy.matrix`.

mod common;

use common::{messages_at, run_in, stdout};
use minact_core::{LogEvent, StepConclusion};

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp workspace")
}

/// The instance ids that ran, in order.
fn instance_ids(result: &minact_core::EngineResult) -> Vec<String> {
    result
        .ordered()
        .into_iter()
        .map(|job| job.instance_id.clone())
        .collect()
}

#[tokio::test]
async fn runs_one_instance_per_combination() {
    let dir = workspace();
    let yaml = r#"
name: Matrix
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        os: [linux, macos]
        arch: [x64, arm64]
    steps:
      - run: echo "building ${{ matrix.os }}/${{ matrix.arch }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        instance_ids(&result),
        vec![
            "build (linux, x64)",
            "build (linux, arm64)",
            "build (macos, x64)",
            "build (macos, arm64)",
        ]
    );

    let out = stdout(&events);
    for expected in [
        "building linux/x64",
        "building linux/arm64",
        "building macos/x64",
        "building macos/arm64",
    ] {
        assert!(out.contains(expected), "missing {}: {}", expected, out);
    }
}

#[tokio::test]
async fn a_job_without_a_matrix_keeps_its_plain_id() {
    let dir = workspace();
    let yaml = r#"
name: NoMatrix
on: workflow_dispatch
jobs:
  build:
    steps:
      - run: echo hi
"#;

    let (result, _) = run_in(yaml, dir.path()).await;

    assert_eq!(instance_ids(&result), vec!["build"]);
    assert_eq!(
        result.job_results["build"].conclusion,
        StepConclusion::Success
    );
    assert!(result.job_results["build"].matrix.is_empty());
}

#[tokio::test]
async fn matrix_values_are_available_everywhere_in_the_job() {
    let dir = workspace();
    let yaml = r#"
name: MatrixContext
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        target: [debug, release]
    env:
      PROFILE: ${{ matrix.target }}
    if: matrix.target != 'nope'
    steps:
      - if: matrix.target == 'release'
        run: echo "release-only step"
      - env:
          STEP_TARGET: ${{ matrix.target }}
        run: echo "profile=$PROFILE step=$STEP_TARGET index=${{ strategy.job-index }}/${{ strategy.job-total }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    let out = stdout(&events);
    assert!(
        out.contains("profile=debug step=debug index=0/2"),
        "{}",
        out
    );
    assert!(
        out.contains("profile=release step=release index=1/2"),
        "{}",
        out
    );
    // The `if:` on the first step only matched one of the two instances.
    assert_eq!(out.matches("release-only step").count(), 1, "{}", out);
}

#[tokio::test]
async fn structured_matrix_values_can_be_indexed() {
    let dir = workspace();
    let yaml = r#"
name: StructuredMatrix
on: workflow_dispatch
jobs:
  package:
    strategy:
      matrix:
        target:
          - platform: android
            format: apk
          - platform: macos
            format: dmg
    steps:
      - run: echo "${{ matrix.target.platform }} -> ${{ matrix.target.format }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    let out = stdout(&events);
    assert!(out.contains("android -> apk"), "{}", out);
    assert!(out.contains("macos -> dmg"), "{}", out);
}

#[tokio::test]
async fn exclude_and_include_shape_the_run() {
    let dir = workspace();
    let yaml = r#"
name: ExcludeInclude
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        os: [linux, macos]
        node: [18, 20]
        exclude:
          - os: macos
            node: 18
        include:
          - os: linux
            node: 20
            experimental: true
          - os: windows
            node: 20
    steps:
      - run: echo "${{ matrix.os }}-${{ matrix.node }} experimental=${{ matrix.experimental }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        instance_ids(&result),
        vec![
            "build (linux, 18)",
            "build (linux, 20, true)",
            "build (macos, 20)",
            "build (windows, 20)",
        ]
    );

    let out = stdout(&events);
    assert!(out.contains("linux-20 experimental=true"), "{}", out);
    assert!(out.contains("linux-18 experimental="), "{}", out);
    assert!(
        !out.contains("macos-18"),
        "excluded combination ran: {}",
        out
    );
}

#[tokio::test]
async fn fail_fast_cancels_the_remaining_instances() {
    let dir = workspace();
    let yaml = r#"
name: FailFast
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        os: [linux, macos, windows]
    steps:
      - run: |
          echo "running ${{ matrix.os }}"
          [ "${{ matrix.os }}" != "macos" ]
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build (linux)"].conclusion,
        StepConclusion::Success
    );
    assert_eq!(
        result.job_results["build (macos)"].conclusion,
        StepConclusion::Failure
    );
    assert_eq!(
        result.job_results["build (windows)"].conclusion,
        StepConclusion::Cancelled,
        "fail-fast should cancel instances after the failure"
    );

    let out = stdout(&events);
    assert!(!out.contains("running windows"), "{}", out);
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::JobCancelled { job_id, reason, .. }
            if job_id == "build (windows)" && reason == "fail-fast"
    )));
}

#[tokio::test]
async fn fail_fast_false_runs_every_instance() {
    let dir = workspace();
    let yaml = r#"
name: NoFailFast
on: workflow_dispatch
jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        os: [linux, macos, windows]
    steps:
      - run: |
          echo "running ${{ matrix.os }}"
          [ "${{ matrix.os }}" != "macos" ]
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build (windows)"].conclusion,
        StepConclusion::Success,
        "fail-fast: false must let later instances run"
    );
    assert!(stdout(&events).contains("running windows"));
}

#[tokio::test]
async fn dependents_wait_for_the_whole_matrix() {
    let dir = workspace();
    let yaml = r#"
name: MatrixNeeds
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        os: [linux, macos]
    steps:
      - run: echo "built ${{ matrix.os }}"
  publish:
    needs: [build]
    steps:
      - run: echo "publishing after ${{ needs.build.result }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        instance_ids(&result),
        vec!["build (linux)", "build (macos)", "publish"]
    );
    assert!(
        stdout(&events).contains("publishing after success"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn one_failing_instance_fails_the_whole_job_for_dependents() {
    let dir = workspace();
    let yaml = r#"
name: MatrixNeedsFailure
on: workflow_dispatch
jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        os: [linux, macos]
    steps:
      - run: '[ "${{ matrix.os }}" != "macos" ]'
  publish:
    needs: [build]
    steps:
      - run: echo "publishing"
  report:
    needs: [build]
    if: always()
    steps:
      - run: echo "build was ${{ needs.build.result }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["publish"].conclusion,
        StepConclusion::Skipped,
        "a partially failed matrix must not let dependents run"
    );
    assert!(
        stdout(&events).contains("build was failure"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn an_explicit_name_keeps_its_matrix_suffix() {
    let dir = workspace();
    let yaml = r#"
name: MatrixNaming
on: workflow_dispatch
jobs:
  build:
    name: Build
    strategy:
      matrix:
        os: [linux]
    steps:
      - run: echo hi
  package:
    name: Package ${{ matrix.os }}
    strategy:
      matrix:
        os: [macos]
    steps:
      - run: echo hi
"#;

    let (result, _) = run_in(yaml, dir.path()).await;

    // A static name gets the values appended so instances stay distinguishable.
    assert_eq!(
        result.job_results["build (linux)"].job_name,
        "Build (linux)"
    );
    // A name that already interpolates the matrix is left alone.
    assert_eq!(
        result.job_results["package (macos)"].job_name,
        "Package macos"
    );
}

#[tokio::test]
async fn a_skipped_matrix_job_skips_every_instance() {
    let dir = workspace();
    let yaml = r#"
name: SkippedMatrix
on: workflow_dispatch
jobs:
  build:
    if: false
    strategy:
      matrix:
        os: [linux, macos]
    steps:
      - run: echo "should not run"
  after:
    needs: [build]
    if: always()
    steps:
      - run: echo "build was ${{ needs.build.result }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        result.job_results["build (linux)"].conclusion,
        StepConclusion::Skipped
    );
    assert_eq!(
        result.job_results["build (macos)"].conclusion,
        StepConclusion::Skipped
    );
    assert!(
        stdout(&events).contains("build was skipped"),
        "an all-skipped matrix aggregates to skipped: {}",
        stdout(&events)
    );
}

#[tokio::test]
async fn instances_of_collects_one_job_id() {
    let dir = workspace();
    let yaml = r#"
name: Instances
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        os: [linux, macos, windows]
    steps:
      - run: echo hi
"#;

    let (result, _) = run_in(yaml, dir.path()).await;

    let instances = result.instances_of("build");
    assert_eq!(instances.len(), 3);
    assert_eq!(
        instances[0].matrix.get("os").map(|v| v.to_string()),
        Some("linux".to_string())
    );
    assert!(result.instances_of("nope").is_empty());
}

// ---------------------------------------------------------------------------
// Matrices computed by an upstream job
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_axis_can_come_from_an_upstream_job() {
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = r#"
name: Dynamic Axis
on: workflow_dispatch
jobs:
  plan:
    outputs:
      targets: ${{ steps.p.outputs.targets }}
    steps:
      - id: p
        run: echo 'targets=["linux","macos"]' >> $GITHUB_OUTPUT
  build:
    needs: [plan]
    strategy:
      matrix:
        target: ${{ fromJSON(needs.plan.outputs.targets) }}
    steps:
      - run: echo "building ${{ matrix.target }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    assert!(out.contains("building linux"), "{}", out);
    assert!(out.contains("building macos"), "{}", out);
    // One instance per value the upstream job produced.
    assert_eq!(
        result
            .job_results
            .keys()
            .filter(|id| id.starts_with("build"))
            .count(),
        2
    );
}

#[tokio::test]
async fn the_whole_matrix_can_come_from_an_upstream_job() {
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = r#"
name: Dynamic Matrix
on: workflow_dispatch
jobs:
  plan:
    outputs:
      spec: ${{ steps.p.outputs.spec }}
    steps:
      - id: p
        run: echo 'spec={"os":["a","b"],"include":[{"os":"a","extra":"yes"}]}' >> $GITHUB_OUTPUT
  build:
    needs: [plan]
    strategy:
      matrix: ${{ fromJSON(needs.plan.outputs.spec) }}
    steps:
      - run: echo "os=${{ matrix.os }} extra=[${{ matrix.extra }}]"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    // `include` merges into the combination it matches, and only that one.
    assert!(out.contains("os=a extra=[yes]"), "{}", out);
    assert!(out.contains("os=b extra=[]"), "{}", out);
}

#[tokio::test]
async fn a_matrix_expression_that_does_not_resolve_fails_its_job() {
    let dir = tempfile::tempdir().expect("temp workspace");
    let yaml = r#"
name: Bad Matrix
on: workflow_dispatch
jobs:
  build:
    strategy:
      matrix:
        target: ${{ fromJSON('not json') }}
    steps:
      - run: echo "should not run"
  after:
    steps:
      - run: echo "other jobs still run"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    // The failure is the matrix, reported as such, and it does not abort the
    // rest of the run.
    let errors = messages_at(&events, minact_core::LogLevel::Error).join("\n");
    assert!(errors.contains("matrix"), "{}", errors);
    let out = stdout(&events);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(out.contains("other jobs still run"), "{}", out);
}
