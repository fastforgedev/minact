//! Running actions referenced with `uses:`.
//!
//! The tests use local actions (`uses: ./…`) rather than fetching from GitHub:
//! everything past resolution — manifests, inputs, composite steps, `post:`
//! hooks — is the same code either way, and a test suite that needs the
//! network is a test suite that fails on a train.
//!
//! The JavaScript tests need `node` and are skipped without it.

mod common;

use std::path::Path;

use common::{messages_at, run_in, stdout};
use minact_core::{LogEvent, LogLevel, StepConclusion};

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp workspace")
}

/// Write an action into the workspace and return its `uses:` value.
fn write_action(workspace: &Path, name: &str, manifest: &str) -> String {
    let dir = workspace.join(name);
    std::fs::create_dir_all(&dir).expect("action directory");
    std::fs::write(dir.join("action.yml"), manifest).expect("action manifest");
    format!("./{}", name)
}

fn write_file(workspace: &Path, path: &str, contents: &str) {
    let path = workspace.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).expect("parent directory");
    std::fs::write(path, contents).expect("file");
}

async fn docker_available() -> bool {
    let ok = minact_core::executor::docker::is_available("docker").await;
    if !ok {
        eprintln!("skipping: no docker daemon");
    }
    ok
}

fn node_available() -> bool {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: node is not installed");
    }
    ok
}

// ---------------------------------------------------------------------------
// Composite actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_composite_action_runs_its_steps_with_its_own_inputs() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "greet",
        r#"
name: Greet
description: Says hello
inputs:
  who:
    description: Who to greet
    default: World
  excited:
    description: Punctuation
    required: false
runs:
  using: composite
  steps:
    - run: echo "hello ${{ inputs.who }}${{ inputs.excited }}"
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Composite
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: {uses}
        with:
          excited: "!"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    // `who` came from the manifest default, `excited` from `with:`.
    assert!(
        stdout(&events).contains("hello World!"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn a_composite_action_reports_the_outputs_it_declares() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "version",
        r#"
name: Version
description: Computes a version
inputs:
  major:
    default: "1"
outputs:
  full:
    description: The version
    value: ${{ steps.compute.outputs.value }}
runs:
  using: composite
  steps:
    - id: compute
      run: echo "value=${{ inputs.major }}.2.3" >> $GITHUB_OUTPUT
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Outputs
on: workflow_dispatch
jobs:
  build:
    steps:
      - id: v
        uses: {uses}
        with:
          major: "4"
      - run: echo "got ${{{{ steps.v.outputs.full }}}}"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(stdout(&events).contains("got 4.2.3"), "{}", stdout(&events));
}

#[tokio::test]
async fn a_composite_step_cannot_see_the_callers_steps() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "isolated",
        r#"
name: Isolated
description: Looks for something it should not find
runs:
  using: composite
  steps:
    - run: echo "leaked=[${{ steps.outer.outputs.secret }}]"
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Isolation
on: workflow_dispatch
jobs:
  build:
    steps:
      - id: outer
        run: echo "secret=visible-outside" >> $GITHUB_OUTPUT
      - uses: {uses}
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(
        stdout(&events).contains("leaked=[]"),
        "the composite saw the caller's steps: {}",
        stdout(&events)
    );
}

#[tokio::test]
async fn a_composite_exports_env_and_path_to_the_rest_of_the_job() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "setup",
        r#"
name: Setup
description: Exports things
runs:
  using: composite
  steps:
    - run: |
        echo "TOOL_HOME=/opt/tool" >> $GITHUB_ENV
        echo "/opt/tool/bin" >> $GITHUB_PATH
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Exports
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: {uses}
      - run: |
          echo "home=$TOOL_HOME"
          case "$PATH" in /opt/tool/bin:*) echo "path=front" ;; *) echo "path=missing" ;; esac
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    assert!(out.contains("home=/opt/tool"), "{}", out);
    assert!(out.contains("path=front"), "{}", out);
}

#[tokio::test]
async fn a_failing_composite_step_fails_the_step_that_used_it() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "broken",
        r#"
name: Broken
description: Fails halfway
runs:
  using: composite
  steps:
    - run: echo "first"
      shell: bash
    - run: exit 3
      shell: bash
    - run: echo "should not run"
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Failure
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: {uses}
      - run: echo "job continued"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(!result.success);
    let out = stdout(&events);
    assert!(out.contains("first"), "{}", out);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(!out.contains("job continued"), "{}", out);
}

#[tokio::test]
async fn a_composite_action_can_use_another_one() {
    let dir = workspace();
    write_action(
        dir.path(),
        "inner",
        r#"
name: Inner
description: The nested one
inputs:
  value:
    default: inner-default
outputs:
  echoed:
    description: What it saw
    value: ${{ steps.e.outputs.v }}
runs:
  using: composite
  steps:
    - id: e
      run: echo "v=${{ inputs.value }}" >> $GITHUB_OUTPUT
      shell: bash
"#,
    );
    let outer = write_action(
        dir.path(),
        "outer",
        r#"
name: Outer
description: Calls the nested one
outputs:
  passed:
    description: What came back
    value: ${{ steps.nested.outputs.echoed }}
runs:
  using: composite
  steps:
    - id: nested
      uses: ./inner
      with:
        value: from-outer
"#,
    );

    let yaml = format!(
        r#"
name: Nested
on: workflow_dispatch
jobs:
  build:
    steps:
      - id: o
        uses: {outer}
      - run: echo "result=${{{{ steps.o.outputs.passed }}}}"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(
        stdout(&events).contains("result=from-outer"),
        "{}",
        stdout(&events)
    );
}

// ---------------------------------------------------------------------------
// JavaScript actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_javascript_action_receives_inputs_and_returns_outputs() {
    if !node_available() {
        return;
    }
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "js",
        r#"
name: JS
description: A JavaScript action
inputs:
  who-to-greet:
    description: Who
    default: World
runs:
  using: node20
  main: index.js
"#,
    );
    write_file(
        dir.path(),
        "js/index.js",
        r#"
const fs = require('fs');
// The toolkit reads inputs from the environment, dashes and all.
const who = process.env['INPUT_WHO-TO-GREET'];
console.log(`greeting ${who}`);
console.log(`action_path=${process.env.GITHUB_ACTION_PATH}`);
fs.appendFileSync(process.env.GITHUB_OUTPUT, `greeted=${who}\n`);
"#,
    );

    let yaml = format!(
        r#"
name: JavaScript
on: workflow_dispatch
jobs:
  build:
    steps:
      - id: greet
        uses: {uses}
        with:
          who-to-greet: minact
      - run: echo "output=${{{{ steps.greet.outputs.greeted }}}}"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    assert!(out.contains("greeting minact"), "{}", out);
    assert!(out.contains("output=minact"), "{}", out);
    // `__dirname` has to be the action's own directory, not a wrapper's.
    assert!(out.contains("action_path="), "{}", out);
    assert!(out.contains("/js"), "{}", out);
}

#[tokio::test]
async fn a_javascript_action_runs_its_post_hook_when_the_job_ends() {
    if !node_available() {
        return;
    }
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "cleanup",
        r#"
name: Cleanup
description: Saves state for its post hook
runs:
  using: node20
  main: main.js
  post: post.js
"#,
    );
    write_file(
        dir.path(),
        "cleanup/main.js",
        r#"
console.log('main ran');
console.log('::save-state name=token::abc123');
"#,
    );
    write_file(
        dir.path(),
        "cleanup/post.js",
        "console.log(`post ran with ${process.env.STATE_token}`);\n",
    );

    let yaml = format!(
        r#"
name: Post
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: {uses}
      - run: echo "later step"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let lines = common::stdout_lines(&events);
    let index = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{:?} has no {}", lines, needle))
    };
    // The hook carries the state the main entry point saved, and runs after
    // every ordinary step rather than straight after its own.
    assert!(lines.iter().any(|l| l.contains("post ran with abc123")));
    assert!(index("later step") < index("post ran"));
}

#[tokio::test]
async fn post_hooks_run_in_reverse_and_after_a_failure() {
    if !node_available() {
        return;
    }
    let dir = workspace();
    for name in ["first", "second"] {
        write_action(
            dir.path(),
            name,
            &format!(
                r#"
name: {name}
description: Registers a post hook
runs:
  using: node20
  main: main.js
  post: post.js
"#
            ),
        );
        write_file(dir.path(), &format!("{name}/main.js"), "\n");
        write_file(
            dir.path(),
            &format!("{name}/post.js"),
            &format!("console.log('post {name}');\n"),
        );
    }

    let yaml = r#"
name: Reverse
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: ./first
      - uses: ./second
      - run: exit 1
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success, "the failing step should fail the job");
    let lines = common::stdout_lines(&events);
    let position = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{:?} has no {}", lines, needle))
    };
    // Cleanup has to survive the failure it is cleaning up, last-registered
    // first.
    assert!(position("post second") < position("post first"));
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_registered_action_wins_over_a_repository_of_the_same_name() {
    let dir = workspace();
    // `actions/checkout` is built in, so nothing is fetched and the step
    // succeeds with no network and no node.
    let yaml = r#"
name: Builtin
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(events.iter().any(
        |event| matches!(event, LogEvent::ActionStarted { uses } if uses == "actions/checkout@v4")
    ));
}

#[tokio::test]
async fn a_uses_that_cannot_be_resolved_fails_the_step_and_says_why() {
    let dir = workspace();
    let yaml = r#"
name: Bad
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: ./nowhere
      - run: echo "should not run"
        if: success()
      - run: echo "cleanup ran"
        if: always()
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build"].step_results[0].conclusion,
        StepConclusion::Failure
    );
    let errors = messages_at(&events, LogLevel::Error).join("\n");
    assert!(errors.contains("nowhere"), "{}", errors);
    // A broken `uses:` is a failed step, not an aborted run.
    let out = stdout(&events);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(out.contains("cleanup ran"), "{}", out);
}

#[tokio::test]
async fn a_local_action_cannot_reach_outside_the_workspace() {
    let dir = workspace();
    let yaml = r#"
name: Escape
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: ./../elsewhere
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    let errors = messages_at(&events, LogLevel::Error).join("\n");
    assert!(errors.contains("workspace"), "{}", errors);
}

#[tokio::test]
async fn a_missing_required_input_is_warned_about() {
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "strict",
        r#"
name: Strict
description: Wants an input
inputs:
  token:
    description: A token
    required: true
  legacy:
    description: An old one
    deprecationMessage: use `token`
runs:
  using: composite
  steps:
    - run: echo "ran"
      shell: bash
"#,
    );

    let yaml = format!(
        r#"
name: Warnings
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: {uses}
        with:
          legacy: old-value
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    // Warnings, not failures: GitHub does not enforce `required:` either.
    assert!(result.success, "{:?}", result.job_results);
    let warnings = messages_at(&events, LogLevel::Warn).join("\n");
    assert!(warnings.contains("required input `token`"), "{}", warnings);
    assert!(warnings.contains("deprecated"), "{}", warnings);
}

// ---------------------------------------------------------------------------
// Container actions
//
// These need a Docker daemon and are skipped without one.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_container_action_builds_from_a_dockerfile_and_returns_outputs() {
    if !docker_available().await {
        return;
    }
    let dir = workspace();
    let uses = write_action(
        dir.path(),
        "boxed",
        r#"
name: Boxed
description: Runs in its own container
inputs:
  who:
    description: Who to greet
    default: World
outputs:
  greeted:
    description: Who was greeted
runs:
  using: docker
  image: Dockerfile
  args:
    - ${{ inputs.who }}
  env:
    EXTRA: from-manifest
"#,
    );
    write_file(
        dir.path(),
        "boxed/Dockerfile",
        "FROM alpine:3.20\nCOPY entrypoint.sh /entrypoint.sh\nRUN chmod +x /entrypoint.sh\nENTRYPOINT [\"/entrypoint.sh\"]\n",
    );
    write_file(
        dir.path(),
        "boxed/entrypoint.sh",
        r#"#!/bin/sh -e
echo "arg=$1 input=$INPUT_WHO extra=$EXTRA"
echo "workspace=$GITHUB_WORKSPACE"
echo "greeted=$1" >> "$GITHUB_OUTPUT"
"#,
    );

    let yaml = format!(
        r#"
name: Container
on: workflow_dispatch
jobs:
  build:
    steps:
      - id: boxed
        uses: {uses}
        with:
          who: minact
      - run: echo "output=${{{{ steps.boxed.outputs.greeted }}}}"
"#
    );

    let (result, events) = run_in(&yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    // `args` came through the expression, `with:` through `INPUT_*`, and the
    // manifest's own `env:` alongside them.
    assert!(
        out.contains("arg=minact input=minact extra=from-manifest"),
        "{}",
        out
    );
    // The workspace is mounted at the identical path, so no translation.
    assert!(
        out.contains(&format!("workspace={}", dir.path().display())),
        "{}",
        out
    );
    // And `$GITHUB_OUTPUT` written inside the container reached the job.
    assert!(out.contains("output=minact"), "{}", out);
}

#[tokio::test]
async fn a_bare_image_takes_its_entrypoint_and_args_from_with() {
    if !docker_available().await {
        return;
    }
    let dir = workspace();
    let yaml = r#"
name: Bare
on: workflow_dispatch
jobs:
  build:
    steps:
      - uses: docker://alpine:3.20
        with:
          entrypoint: /bin/sh
          args: -c "echo one two && echo three"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    // The quoted argument survived as one argument rather than three.
    assert!(out.contains("one two"), "{}", out);
    assert!(out.contains("three"), "{}", out);
}
