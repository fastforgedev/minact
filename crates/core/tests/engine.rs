//! End-to-end engine behaviour.

mod common;

use common::{messages_at, run_in, skipped_steps, stdout, stdout_lines};
use minact_core::{LogEvent, LogLevel, StepConclusion};

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp workspace")
}

// ---------------------------------------------------------------------------
// Baseline execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runs_a_basic_workflow() {
    let dir = workspace();
    let yaml = r#"
name: Test
on: workflow_dispatch
jobs:
  greet:
    name: Greet
    steps:
      - name: Say Hello
        run: echo "Hello, World!"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert_eq!(result.workflow_name, "Test");
    assert!(result.success);
    assert_eq!(result.job_results.len(), 1);
    assert!(result.job_results["greet"].success);
    assert!(stdout(&events).contains("Hello, World!"));
}

#[tokio::test]
async fn honours_job_level_conditions() {
    let dir = workspace();
    let yaml = r#"
name: Conditional
on: workflow_dispatch
jobs:
  skip-me:
    name: Skip Me
    if: false
    steps:
      - name: Should Not Run
        run: echo "This should not run"
  run-me:
    name: Run Me
    if: true
    steps:
      - name: Should Run
        run: echo "This should run"
"#;

    let (result, _) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        result.job_results["skip-me"].conclusion,
        StepConclusion::Skipped
    );
    assert_eq!(
        result.job_results["run-me"].conclusion,
        StepConclusion::Success
    );
}

#[tokio::test]
async fn reports_stdout_and_stderr_separately() {
    let dir = workspace();
    let yaml = r#"
name: Logs
on: workflow_dispatch
jobs:
  logs:
    name: Logs
    steps:
      - name: Emit streams
        run: printf 'out\n'; printf 'err\n' >&2
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::CommandStarted { command, shell, .. }
            if command.contains("printf 'out") && shell == "bash"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::CommandOutput { stream: minact_core::CommandStream::Stdout, line } if line == "out"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::CommandOutput { stream: minact_core::CommandStream::Stderr, line } if line == "err"
    )));
    assert!(events
        .iter()
        .any(|event| matches!(event, LogEvent::CommandFinished { success: true, .. })));
}

#[tokio::test]
async fn skipped_step_runs_no_command() {
    let dir = workspace();
    let yaml = r#"
name: Skip
on: workflow_dispatch
jobs:
  skip:
    name: Skip
    steps:
      - name: No command
        if: false
        run: echo "nope"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::StepSkipped { step_name, condition, .. }
            if step_name == "No command" && condition == "false"
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, LogEvent::CommandStarted { .. })));
}

#[tokio::test]
async fn resolves_working_directory_at_every_level() {
    let dir = workspace();
    let root = dir.path();
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(root.join("marker-root.txt"), "root").unwrap();
    std::fs::write(sub.join("marker-sub.txt"), "sub").unwrap();

    // Workflow-level default.
    let (result, _) = run_in(
        r#"
name: WorkflowDefaults
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    steps:
      - name: Check CWD
        run: test -f marker-sub.txt
"#,
        root,
    )
    .await;
    assert!(result.success, "workflow-level defaults should set CWD");

    // Job-level default overrides the workflow default.
    let (result, _) = run_in(
        r#"
name: JobDefaults
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    defaults:
      run:
        working-directory: .
    steps:
      - name: Check CWD
        run: test -f marker-root.txt
"#,
        root,
    )
    .await;
    assert!(result.success, "job defaults should override workflow ones");

    // Step-level override, and inheriting the default in the same job.
    let (result, _) = run_in(
        r#"
name: StepOverride
on: workflow_dispatch
defaults:
  run:
    working-directory: sub
jobs:
  test:
    steps:
      - name: Override to root
        working-directory: .
        run: test -f marker-root.txt
      - name: Inherit from defaults
        run: test -f marker-sub.txt
"#,
        root,
    )
    .await;
    assert!(result.success, "step override and inherit should both work");
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steps_inherit_the_host_path() {
    let dir = workspace();
    let yaml = r#"
name: Path
on: workflow_dispatch
jobs:
  env:
    steps:
      - name: Show PATH
        run: printf 'PATH=%s\n' "$PATH"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let host_path = std::env::var("PATH").unwrap();
    assert!(
        stdout(&events).contains(&host_path),
        "step PATH should include the host PATH, got: {}",
        stdout(&events)
    );
}

#[tokio::test]
async fn a_step_can_run_tools_from_the_host_path() {
    let dir = workspace();
    let yaml = r#"
name: Tools
on: workflow_dispatch
jobs:
  tools:
    steps:
      - name: Resolve git
        run: command -v git > /dev/null && echo "git-found"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(
        result.success,
        "git should resolve through the inherited PATH"
    );
    assert!(stdout(&events).contains("git-found"));
}

#[tokio::test]
async fn sets_the_standard_runner_variables() {
    let dir = workspace();
    let yaml = r#"
name: Vars
on: workflow_dispatch
jobs:
  show:
    steps:
      - name: Print vars
        run: |
          echo "CI=$CI"
          echo "GITHUB_ACTIONS=$GITHUB_ACTIONS"
          echo "WORKSPACE=$GITHUB_WORKSPACE"
          echo "JOB=$GITHUB_JOB"
          echo "EVENT=$GITHUB_EVENT_NAME"
          echo "REF_NAME=$GITHUB_REF_NAME"
          echo "RUNNER_OS=$RUNNER_OS"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("CI=true"), "{}", out);
    assert!(out.contains("GITHUB_ACTIONS=true"), "{}", out);
    assert!(
        out.contains(&format!("WORKSPACE={}", dir.path().display())),
        "{}",
        out
    );
    assert!(out.contains("JOB=show"), "{}", out);
    assert!(out.contains("EVENT=workflow_dispatch"), "{}", out);
    assert!(out.contains("REF_NAME=main"), "{}", out);
    assert!(
        !out.contains("RUNNER_OS=\n"),
        "RUNNER_OS should be set: {}",
        out
    );
}

#[tokio::test]
async fn runner_os_uses_github_spelling() {
    let dir = workspace();
    let expected = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Windows"
    };

    let yaml = r#"
name: RunnerOs
on: workflow_dispatch
jobs:
  show:
    steps:
      - run: echo "os=${{ runner.os }} arch=${{ runner.arch }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout(&events).contains(&format!("os={}", expected)),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn env_is_layered_workflow_then_job_then_step() {
    let dir = workspace();
    let yaml = r#"
name: EnvLayers
on: workflow_dispatch
env:
  LAYER: workflow
  ONLY_WORKFLOW: yes
jobs:
  layered:
    env:
      LAYER: job
    steps:
      - name: Job layer wins over workflow
        run: echo "layer=$LAYER only=$ONLY_WORKFLOW"
      - name: Step layer wins over job
        env:
          LAYER: step
        run: echo "layer=$LAYER"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("layer=job only=yes"), "{}", out);
    assert!(out.contains("layer=step"), "{}", out);
}

#[tokio::test]
async fn job_env_does_not_leak_into_later_jobs() {
    let dir = workspace();
    let yaml = r#"
name: EnvScope
on: workflow_dispatch
jobs:
  first:
    env:
      SCOPED: first-only
    steps:
      - run: echo "first=[$SCOPED]"
  second:
    needs: [first]
    steps:
      - run: echo "second=[$SCOPED]"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("first=[first-only]"), "{}", out);
    assert!(
        out.contains("second=[]"),
        "job env must not leak into the next job: {}",
        out
    );
}

#[tokio::test]
async fn env_values_are_expression_evaluated() {
    let dir = workspace();
    let yaml = r#"
name: EnvExpr
on: workflow_dispatch
env:
  GREETING: hello
jobs:
  expand:
    env:
      JOB_MESSAGE: "${{ env.GREETING }} from job"
    steps:
      - env:
          STEP_MESSAGE: "${{ env.GREETING }} from step"
        run: echo "$JOB_MESSAGE / $STEP_MESSAGE"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout(&events).contains("hello from job / hello from step"),
        "{}",
        stdout(&events)
    );
}

// ---------------------------------------------------------------------------
// Environment files: $GITHUB_OUTPUT / $GITHUB_ENV / $GITHUB_PATH
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_outputs_flow_to_steps_jobs_and_needs() {
    let dir = workspace();
    let yaml = r#"
name: Outputs
on: workflow_dispatch
jobs:
  produce:
    outputs:
      version: ${{ steps.meta.outputs.version }}
    steps:
      - id: meta
        name: Set outputs
        run: |
          echo "version=1.2.3" >> "$GITHUB_OUTPUT"
          echo "channel=stable" >> "$GITHUB_OUTPUT"
      - name: Read within the job
        run: echo "step=${{ steps.meta.outputs.version }}/${{ steps.meta.outputs.channel }}"
  consume:
    needs: [produce]
    steps:
      - name: Read across jobs
        run: echo "needs=${{ needs.produce.outputs.version }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("step=1.2.3/stable"), "{}", out);
    assert!(out.contains("needs=1.2.3"), "{}", out);
    assert_eq!(
        result.job_results["produce"].outputs.get("version"),
        Some(&"1.2.3".to_string())
    );
}

#[tokio::test]
async fn step_outputs_support_heredoc_values() {
    let dir = workspace();
    let yaml = r#"
name: Heredoc
on: workflow_dispatch
jobs:
  multiline:
    steps:
      - id: notes
        run: |
          {
            echo "body<<EOF"
            echo "line one"
            echo "line two"
            echo "EOF"
          } >> "$GITHUB_OUTPUT"
      - run: echo "notes=${{ steps.notes.outputs.body }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout(&events).contains("notes=line one"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn github_env_is_visible_to_later_steps() {
    let dir = workspace();
    let yaml = r#"
name: EnvFile
on: workflow_dispatch
jobs:
  share:
    steps:
      - name: Export
        run: echo "SHARED=from-earlier-step" >> "$GITHUB_ENV"
      - name: Read in shell
        run: echo "shell=[$SHARED]"
      - name: Read in expression
        run: echo "expr=[${{ env.SHARED }}]"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("shell=[from-earlier-step]"), "{}", out);
    assert!(out.contains("expr=[from-earlier-step]"), "{}", out);
}

#[cfg(unix)]
#[tokio::test]
async fn github_path_is_prepended_for_later_steps() {
    use std::os::unix::fs::PermissionsExt;

    let dir = workspace();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("minact-test-tool");
    std::fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let yaml = r#"
name: PathFile
on: workflow_dispatch
jobs:
  tools:
    steps:
      - name: Register bin
        run: echo "$GITHUB_WORKSPACE/bin" >> "$GITHUB_PATH"
      - name: Use the tool
        run: minact-test-tool
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success, "tool from $GITHUB_PATH should be runnable");
    assert!(stdout(&events).contains("tool-ran"), "{}", stdout(&events));
}

#[tokio::test]
async fn step_summary_is_reported() {
    let dir = workspace();
    let yaml = r#"
name: Summary
on: workflow_dispatch
jobs:
  summarise:
    steps:
      - run: echo '### Build report' >> "$GITHUB_STEP_SUMMARY"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        messages_at(&events, LogLevel::Info)
            .iter()
            .any(|m| m.contains("### Build report")),
        "step summary should be surfaced"
    );
}

#[tokio::test]
async fn malformed_output_file_fails_the_step() {
    let dir = workspace();
    let yaml = r#"
name: BadOutput
on: workflow_dispatch
jobs:
  broken:
    steps:
      - run: echo "this is not a key value line" >> "$GITHUB_OUTPUT"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(
        !result.success,
        "an invalid $GITHUB_OUTPUT should fail the step"
    );
    assert!(
        messages_at(&events, LogLevel::Error)
            .iter()
            .any(|m| m.contains("invalid environment file")),
        "the failure should say what went wrong"
    );
}

// ---------------------------------------------------------------------------
// Workflow commands on stdout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_output_command_still_works() {
    let dir = workspace();
    let yaml = r#"
name: SetOutput
on: workflow_dispatch
jobs:
  legacy:
    steps:
      - id: legacy
        run: echo "::set-output name=version::9.9.9"
      - run: echo "version=${{ steps.legacy.outputs.version }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("version=9.9.9"), "{}", out);
    assert!(
        !out.contains("::set-output"),
        "the command line itself should not be echoed: {}",
        out
    );
}

#[tokio::test]
async fn error_and_warning_commands_become_messages() {
    let dir = workspace();
    let yaml = r#"
name: Annotations
on: workflow_dispatch
jobs:
  annotate:
    steps:
      - run: |
          echo "::warning::heads up"
          echo "::error file=src/main.rs,line=10::it broke"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success, "annotations alone do not fail a step");

    assert!(messages_at(&events, LogLevel::Warn)
        .iter()
        .any(|m| m.contains("heads up")));
    assert!(messages_at(&events, LogLevel::Error)
        .iter()
        .any(|m| m.contains("src/main.rs:10") && m.contains("it broke")));
}

#[tokio::test]
async fn add_mask_redacts_later_output() {
    let dir = workspace();
    let yaml = r#"
name: Mask
on: workflow_dispatch
jobs:
  secrets:
    steps:
      - run: |
          echo "::add-mask::sup3rs3cret"
          echo "token is sup3rs3cret"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(!out.contains("sup3rs3cret"), "the secret leaked: {}", out);
    assert!(out.contains("token is ***"), "{}", out);
}

#[tokio::test]
async fn ordinary_output_is_not_mistaken_for_a_command() {
    let dir = workspace();
    let yaml = r#"
name: NotCommands
on: workflow_dispatch
jobs:
  chatty:
    steps:
      - run: |
          echo "::::::::"
          echo "note:: this is prose"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);

    let out = stdout(&events);
    assert!(out.contains("::::::::"), "{}", out);
    assert!(out.contains("note:: this is prose"), "{}", out);
}

// ---------------------------------------------------------------------------
// Failure propagation and status functions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failure_propagates_to_dependent_jobs() {
    let dir = workspace();
    let yaml = r#"
name: Propagate
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Fail
        run: exit 1
  deploy:
    needs: [build]
    steps:
      - run: echo "deploy ran"
  rollback:
    needs: [build]
    if: failure()
    steps:
      - run: echo "rollback ran (result=${{ needs.build.result }})"
  notify:
    needs: [build]
    if: always()
    steps:
      - run: echo "notify ran"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build"].conclusion,
        StepConclusion::Failure
    );
    assert_eq!(
        result.job_results["deploy"].conclusion,
        StepConclusion::Skipped,
        "a job needing a failed job must not run"
    );
    assert_eq!(
        result.job_results["rollback"].conclusion,
        StepConclusion::Success,
        "if: failure() must run when a dependency failed"
    );
    assert_eq!(
        result.job_results["notify"].conclusion,
        StepConclusion::Success
    );

    let out = stdout(&events);
    assert!(!out.contains("deploy ran"), "{}", out);
    assert!(out.contains("rollback ran (result=failure)"), "{}", out);
    assert!(out.contains("notify ran"), "{}", out);
}

#[tokio::test]
async fn skipped_job_skips_its_dependents() {
    let dir = workspace();
    let yaml = r#"
name: SkipChain
on: workflow_dispatch
jobs:
  optional:
    if: false
    steps:
      - run: echo "optional ran"
  after:
    needs: [optional]
    steps:
      - run: echo "after ran"
  anyway:
    needs: [optional]
    if: always()
    steps:
      - run: echo "anyway ran (result=${{ needs.optional.result }})"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "skips are not failures");
    assert_eq!(
        result.job_results["after"].conclusion,
        StepConclusion::Skipped
    );
    assert_eq!(
        result.job_results["anyway"].conclusion,
        StepConclusion::Success
    );
    assert!(
        stdout(&events).contains("anyway ran (result=skipped)"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn steps_after_a_failure_are_skipped_but_always_still_runs() {
    let dir = workspace();
    let yaml = r#"
name: StepFlow
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Fail here
        run: exit 3
      - name: Never runs
        run: echo "should not run"
      - name: Cleanup
        if: always()
        run: echo "cleanup ran"
      - name: On failure
        if: failure()
        run: echo "failure handler ran"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["build"].conclusion,
        StepConclusion::Failure
    );

    let out = stdout(&events);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(out.contains("cleanup ran"), "{}", out);
    assert!(out.contains("failure handler ran"), "{}", out);
    assert!(skipped_steps(&events).contains(&"Never runs".to_string()));
}

#[tokio::test]
async fn continue_on_error_keeps_the_job_successful() {
    let dir = workspace();
    let yaml = r#"
name: ContinueOnError
on: workflow_dispatch
jobs:
  tolerant:
    steps:
      - id: flaky
        name: Flaky
        continue-on-error: true
        run: exit 1
      - name: Still runs
        run: echo "outcome=${{ steps.flaky.outcome }} conclusion=${{ steps.flaky.conclusion }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    assert_eq!(
        result.job_results["tolerant"].conclusion,
        StepConclusion::Success
    );
    assert!(
        stdout(&events).contains("outcome=failure conclusion=success"),
        "{}",
        stdout(&events)
    );
}

#[tokio::test]
async fn success_is_false_once_a_step_has_failed() {
    let dir = workspace();
    let yaml = r#"
name: SuccessFn
on: workflow_dispatch
jobs:
  build:
    steps:
      - run: exit 1
      - if: success()
        run: echo "success branch"
      - if: always()
        run: echo "always branch"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    let out = stdout(&events);
    assert!(!out.contains("success branch"), "{}", out);
    assert!(out.contains("always branch"), "{}", out);
}

#[tokio::test]
async fn an_unknown_action_fails_only_its_own_job() {
    let dir = workspace();
    let yaml = r#"
name: UnknownAction
on: workflow_dispatch
jobs:
  broken:
    steps:
      - uses: some/never-registered@v1
  healthy:
    steps:
      - run: echo "healthy ran"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["broken"].conclusion,
        StepConclusion::Failure
    );
    assert_eq!(
        result.job_results["healthy"].conclusion,
        StepConclusion::Success,
        "an unknown action must not abort the whole run"
    );
    assert!(stdout(&events).contains("healthy ran"));
}

// ---------------------------------------------------------------------------
// Shell behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_steps_run_in_real_bash() {
    let dir = workspace();
    let yaml = r#"
name: Bash
on: workflow_dispatch
jobs:
  shell:
    steps:
      - name: Bash-only syntax
        shell: bash
        run: |
          [[ -n "$BASH_VERSION" ]] && echo "bash-confirmed"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout(&events).contains("bash-confirmed"),
        "steps declared `shell: bash` must run under bash: {}",
        stdout(&events)
    );
}

#[tokio::test]
async fn a_failing_line_aborts_the_rest_of_the_script() {
    let dir = workspace();
    let yaml = r#"
name: ErrExit
on: workflow_dispatch
jobs:
  strict:
    steps:
      - run: |
          echo "before"
          false
          echo "after"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success, "-e should fail the step");
    let out = stdout(&events);
    assert!(out.contains("before"), "{}", out);
    assert!(
        !out.contains("after"),
        "execution should stop at the failing line: {}",
        out
    );
}

#[tokio::test]
async fn pipefail_is_enabled_for_bash() {
    let dir = workspace();
    let yaml = r#"
name: PipeFail
on: workflow_dispatch
jobs:
  strict:
    steps:
      - run: false | true
"#;

    let (result, _) = run_in(yaml, dir.path()).await;
    assert!(!result.success, "a failing pipe stage should fail the step");
}

#[tokio::test]
async fn default_shell_can_be_set_by_defaults() {
    let dir = workspace();
    let yaml = r#"
name: DefaultShell
on: workflow_dispatch
defaults:
  run:
    shell: sh
jobs:
  shell:
    steps:
      - run: echo "ran under sh"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(events.iter().any(|event| matches!(
        event,
        LogEvent::CommandStarted { shell, .. } if shell == "sh"
    )));
}

#[tokio::test]
async fn python_steps_run_under_python() {
    let dir = workspace();
    let yaml = r#"
name: Python
on: workflow_dispatch
jobs:
  py:
    steps:
      - shell: python
        run: |
          import sys
          print("python", sys.version_info[0])
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout_lines(&events)
            .iter()
            .any(|l| l.starts_with("python 3")),
        "{}",
        stdout(&events)
    );
}

// ---------------------------------------------------------------------------
// Expressions and parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_context_properties_render_empty() {
    let dir = workspace();
    let yaml = r#"
name: UnknownProps
on: workflow_dispatch
jobs:
  probe:
    steps:
      - run: |
          echo "unset=[${{ github.retention_days }}] misc=[${{ runner.not_a_thing }}]"
          echo "populated=[${{ github.run_number }}]"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(
        result.success,
        "an unset context property must not abort the run"
    );
    let out = stdout(&events);
    // A property minact has no local answer for reads as empty, the way
    // GitHub renders one it did not populate.
    assert!(out.contains("unset=[] misc=[]"), "{}", out);
    // But one it *can* answer for is answered, rather than being empty too.
    assert!(out.contains("populated=[1]"), "{}", out);
}

#[tokio::test]
async fn workflow_dispatch_inputs_use_declared_defaults() {
    let dir = workspace();
    let yaml = r#"
name: Inputs
on:
  workflow_dispatch:
    inputs:
      build_mode:
        description: Build mode
        required: true
        default: release
jobs:
  show:
    steps:
      - run: echo "mode=${{ inputs.build_mode }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;
    assert!(result.success);
    assert!(
        stdout(&events).contains("mode=release"),
        "{}",
        stdout(&events)
    );
}

// ---------------------------------------------------------------------------
// timeout-minutes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_step_that_overruns_its_timeout_fails() {
    let dir = workspace();
    // `timeout-minutes` is a number on GitHub, not an integer, so a fraction
    // is legal — and it is what makes this testable in under a second.
    let yaml = r#"
name: Step Timeout
on: workflow_dispatch
jobs:
  slow:
    steps:
      - name: overruns
        timeout-minutes: 0.02
        run: sleep 30
      - name: cleanup
        if: always()
        run: echo "cleanup ran"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    let steps = &result.job_results["slow"].step_results;
    // Running out of time is a failure, not a cancellation: a cancelled run
    // has to stay distinguishable from a step that overran on its own.
    assert_eq!(steps[0].conclusion, StepConclusion::Failure);
    assert!(messages_at(&events, LogLevel::Error)
        .iter()
        .any(|m| m.contains("timeout-minutes")));
    // And the job carries on the way it does after any other failed step.
    assert!(stdout(&events).contains("cleanup ran"));
}

#[tokio::test]
async fn a_job_that_overruns_its_timeout_fails() {
    let dir = workspace();
    let yaml = r#"
name: Job Timeout
on: workflow_dispatch
jobs:
  slow:
    timeout-minutes: 0.02
    steps:
      - run: sleep 30
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(!result.success);
    assert_eq!(
        result.job_results["slow"].conclusion,
        StepConclusion::Failure
    );
    assert!(messages_at(&events, LogLevel::Error)
        .iter()
        .any(|m| m.contains("timeout-minutes")));
}

#[tokio::test]
async fn a_step_within_its_timeout_is_untouched() {
    let dir = workspace();
    let yaml = r#"
name: Fast Enough
on: workflow_dispatch
jobs:
  quick:
    timeout-minutes: 5
    steps:
      - timeout-minutes: 5
        run: echo "done in time"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    assert!(stdout(&events).contains("done in time"));
}

// ---------------------------------------------------------------------------
// Job-level continue-on-error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continue_on_error_on_a_job_keeps_its_dependents_running() {
    let dir = workspace();
    let yaml = r#"
name: Soft Failure
on: workflow_dispatch
jobs:
  soft:
    continue-on-error: true
    steps:
      - run: exit 1
  after:
    needs: [soft]
    steps:
      - run: echo "dependent ran"
  strict:
    steps:
      - run: exit 1
  after-strict:
    needs: [strict]
    steps:
      - run: echo "should not run"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    // The soft job's failure neither fails the workflow nor skips dependents,
    // while an ordinary failure still does both.
    assert!(!result.success, "the strict job should still fail the run");
    assert_eq!(
        result.job_results["soft"].conclusion,
        StepConclusion::Success
    );
    let out = stdout(&events);
    assert!(out.contains("dependent ran"), "{}", out);
    assert!(!out.contains("should not run"), "{}", out);
    assert!(messages_at(&events, LogLevel::Warn)
        .iter()
        .any(|m| m.contains("continue-on-error")));
}

// ---------------------------------------------------------------------------
// The github context
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_github_context_answers_what_it_can_derive_locally() {
    let dir = workspace();
    let yaml = r#"
name: My Workflow
on: workflow_dispatch
jobs:
  probe:
    steps:
      - run: |
          echo "workflow=[${{ github.workflow }}] job=[${{ github.job }}]"
          echo "ref_type=[${{ github.ref_type }}] attempt=[${{ github.run_attempt }}]"
          echo "server=[${{ github.server_url }}] api=[${{ github.api_url }}]"
          echo "env_workflow=[$GITHUB_WORKFLOW] env_run=[$GITHUB_RUN_NUMBER]"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success, "{:?}", result.job_results);
    let out = stdout(&events);
    assert!(
        out.contains("workflow=[My Workflow] job=[probe]"),
        "{}",
        out
    );
    assert!(out.contains("ref_type=[branch] attempt=[1]"), "{}", out);
    assert!(
        out.contains("server=[https://github.com] api=[https://api.github.com]"),
        "{}",
        out
    );
    // Actions read the environment, not expressions, so both have to agree.
    assert!(
        out.contains("env_workflow=[My Workflow] env_run=[1]"),
        "{}",
        out
    );
}

#[tokio::test]
async fn a_step_sees_a_stable_run_id() {
    let dir = workspace();
    let yaml = r#"
name: Run Id
on: workflow_dispatch
jobs:
  a:
    steps:
      - run: echo "one=${{ github.run_id }}"
  b:
    steps:
      - run: echo "two=${{ github.run_id }}"
"#;

    let (result, events) = run_in(yaml, dir.path()).await;

    assert!(result.success);
    let out = stdout(&events);
    let value = |prefix: &str| {
        out.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("{:?} has no {}", out, prefix))
            .to_string()
    };
    // Every job of one run shares it, and it is not empty.
    let one = value("one=");
    assert!(!one.is_empty());
    assert_eq!(one, value("two="));
}

// ---------------------------------------------------------------------------
// Secret redaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_masked_value_is_redacted_in_the_echoed_command_too() {
    let dir = workspace();
    let yaml = r#"
name: Masking
on: workflow_dispatch
env:
  PASSWORD: hunter2
jobs:
  j:
    steps:
      # The value reaches the command through the environment, so the step
      # that registers the mask does not spell it out either.
      - run: echo "::add-mask::$PASSWORD"
      - run: echo "the password is ${{ env.PASSWORD }}"
"#;

    let (_, events) = run_in(yaml, dir.path()).await;

    // The command is echoed before it runs; a secret that leaks there has not
    // been masked, however clean the output looks.
    let commands: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            LogEvent::CommandStarted { command, .. } => Some(command.clone()),
            _ => None,
        })
        .collect();
    assert!(
        commands.iter().all(|command| !command.contains("hunter2")),
        "{:?}",
        commands
    );
    assert!(!stdout(&events).contains("hunter2"), "{}", stdout(&events));
}
