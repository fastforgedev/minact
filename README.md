# minact

> A lightweight GitHub Actions-compatible workflow runner

minact is a lightweight tool that runs GitHub Actions-compatible workflows on your local machine. It parses standard YAML workflow files and executes jobs and steps in dependency order — no Docker daemon, no hosted runners, just your local environment.

## Features

- **Drop-in compatible** — Use your existing `.yml`/`.yaml` workflow files as-is
- **Expression evaluation** — Full support for `${{ github.* }}`, `${{ env.* }}`, `${{ secrets.* }}`, `${{ inputs.* }}`, `${{ needs.* }}`, `${{ steps.* }}`, and functions like `contains()`, `startsWith()`, `success()`, `failure()`
- **Real data flow** — Steps pass values on through `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_PATH` and `$GITHUB_STEP_SUMMARY`
- **GitHub failure semantics** — A failed step skips the rest of its job but still runs `if: always()` cleanup; a failed job skips its dependents while `if: failure()` handlers run
- **Conditional execution** — Skip jobs and steps with `if:` conditions
- **Real actions** — `uses: owner/repo@ref` is fetched from GitHub and run: JavaScript, composite and container actions, with their `pre:`/`post:` hooks
- **Built-in Actions** — Ships with `actions/checkout`, `actions/cache`, `actions/upload-artifact`, `actions/download-artifact`
- **Shell flexibility** — Run steps with `bash`, `sh`, `python`, `node`, `pwsh`, or a custom `{0}` template
- **Cross-platform** — `runs-on:` maps to a container or another machine, so a Linux job really runs on Linux from your Mac
- **Build matrices** — `strategy.matrix` with `include`, `exclude` and `fail-fast`, expanding one job into many
- **DAG scheduler** — Resolves job dependencies (`needs:`) and executes them in topological order
- **Auto-discovery** — Finds workflows in `.minact/workflows/` and `.github/workflows/`
- **Event simulation** — Trigger workflows as `push`, `pull_request`, `workflow_dispatch`, or any event you specify

## Supported Syntax

| Category | Syntax | Status |
|----------|--------|--------|
| Events | `on: push` / `on: [push, pull_request]` / `on: { push: { branches: [main] } }` | ✅ |
| Environment | `env:` at workflow / job / step level | ✅ |
| Job deps | `jobs.<job_id>.needs` (string or list) | ✅ |
| Conditions | `jobs.<job_id>.if` / `steps[].if` | ✅ |
| Job outputs | `jobs.<job_id>.outputs` | ✅ |
| Step outputs | `$GITHUB_OUTPUT` (incl. heredoc), `::set-output` | ✅ |
| Env files | `$GITHUB_ENV`, `$GITHUB_PATH`, `$GITHUB_STEP_SUMMARY` | ✅ |
| Workflow commands | `::error::`, `::warning::`, `::notice::`, `::debug::`, `::group::`, `::add-mask::`, `::add-path::` | ✅ |
| Steps | `steps[].uses`, `steps[].run`, `steps[].with` | ✅ |
| Remote actions | `uses: owner/repo@ref`, `owner/repo/subdir@ref` | ✅ |
| Local actions | `uses: ./path/to/action` | ✅ |
| Container actions | `uses: docker://image`, `runs.using: docker` | ✅ |
| Action kinds | `runs.using: node16` / `node20` / `node24` / `composite` | ✅ |
| Action lifecycle | `runs.pre` / `runs.post` with `pre-if` / `post-if` | ✅ |
| Action metadata | `inputs` defaults, `required`, `deprecationMessage`, `outputs` | ✅ |
| Step options | `continue-on-error`, `shell`, `working-directory` | ✅ |
| Defaults | `defaults.run.shell`, `defaults.run.working-directory` | ✅ |
| Contexts | `${{ github.* }}`, `${{ env.* }}`, `${{ secrets.* }}` | ✅ |
| More contexts | `${{ runner.* }}`, `${{ inputs.* }}`, `${{ needs.* }}`, `${{ steps.* }}` | ✅ |
| Step status | `steps.<id>.outcome`, `steps.<id>.conclusion`, `needs.<id>.result` | ✅ |
| Matrix strategy | `strategy.matrix` with `include` / `exclude` | ✅ |
| Matrix control | `strategy.fail-fast`, `${{ matrix.* }}`, `${{ strategy.* }}` | ✅ |
| Matrix from a job | `strategy.matrix: ${{ fromJSON(needs.x.outputs.y) }}` | ✅ |
| Functions | `contains()`, `startsWith()`, `endsWith()`, `format()`, `join()` | ✅ |
| JSON functions | `fromJSON()`, `toJSON()`, and property access on the result | ✅ |
| `hashFiles()` | SHA-256 over the files a glob matches | ✅ |
| Status checks | `success()`, `failure()`, `always()`, `cancelled()` | ✅ |
| Timeouts | `jobs.<job_id>.timeout-minutes`, `steps[].timeout-minutes` | ✅ |
| Job options | `jobs.<job_id>.continue-on-error` | ✅ |
| Job container | `jobs.<job_id>.container` (image, `env`, `volumes`, `ports`, `options`) | ✅ |
| Action contexts | `github.action_path`, `action_repository`, `action_ref` | ✅ |
| Run contexts | `github.workflow`, `job`, `run_id`, `run_number`, `run_attempt`, `ref_type` | ✅ |
| URL contexts | `github.server_url`, `api_url`, `graphql_url`, `repositoryUrl` | ✅ |
| Event payload | `github.event.*` from `GITHUB_EVENT_PATH` | ✅ |
| Runner selection | `runs-on` mapped to local / Docker / SSH | ✅ |
| Secrets | `${{ secrets.* }}` resolution | ⚠️ Always empty |
| Event filtering | `on.push.branches`, `on.*.paths` | ⚠️ Parsed, not enforced |
| Parallelism | Jobs in the same layer, matrix instances | ⚠️ Run sequentially |
| `strategy.max-parallel` | Capping concurrency | ⚠️ Parsed, no effect while sequential |
| `jobs.<job_id>.services` | Service containers | ⚠️ Reported, not started |
| Reusable workflows | `on.workflow_call`, `jobs.<job_id>.uses` | ❌ Not supported |
| Async steps | `steps[].background` / `wait` / `parallel` | ❌ Not supported |
| `concurrency`, `permissions`, `environment`, `run-name` | — | ❌ No local meaning |

## Actions

A `uses:` value resolves in this order:

1. **A registered action** — implemented in Rust and held in the engine's
   registry. minact ships `actions/checkout`, `actions/cache`,
   `actions/upload-artifact` and `actions/download-artifact`, and a tool
   embedding the engine adds its own. These win over anything published under
   the same name: they need nothing fetched and nothing installed, and an
   embedding tool's `uses:` names must keep reaching its implementation.
2. **`./path/to/action`** — a directory in the workspace holding an
   `action.yml`. It cannot climb out of the workspace, symlinks included.
3. **`docker://image:tag`** — a container image, run as-is. Its `entrypoint`
   and `args` come from the step's `with:`.
4. **`owner/repo@ref`** — fetched from GitHub. `owner/repo/sub/dir@ref` picks
   an action out of a sub-directory, and `@ref` is a tag, a branch or a full
   commit SHA.

All three kinds GitHub defines then run:

**JavaScript** (`runs.using: node16` / `node20` / `node24`) runs
`node <main>` wherever the job runs, with the action's `with:` values and its
declared defaults arriving as `INPUT_*`. minact uses the `node` on your PATH
rather than shipping its own — set `MINACT_NODE` to point at a different one.

**Composite** (`runs.using: composite`) runs the action's steps in the calling
job, over a context of its own: `${{ inputs.* }}` are the action's inputs, and
its `steps` are invisible to the caller as the caller's are to it. Its declared
`outputs` are evaluated at the end. What it writes to `$GITHUB_ENV` and
`$GITHUB_PATH` does reach the rest of the job, as on GitHub. A composite step
can itself be a `uses:`, nested up to ten deep.

**Container** (`runs.using: docker`) builds the action's `Dockerfile` — cached
by content, so only the first run pays for it — or pulls the image it names,
then runs it with the workspace bind-mounted at the same path it has on the
host. This is the one action kind that gets its own container even when the job
itself is running locally, which is also what GitHub does.

**`pre:` and `post:`** run around the action. A `post:` hook runs when the job
ends, in reverse registration order, and runs whatever the job did — that is
what makes it cleanup rather than another step. `::save-state::` from the main
entry point arrives as `STATE_*`. `pre-if` and `post-if` default to `always()`.

### The action cache

Fetched actions live in `~/.minact/actions/<owner>/<repo>/<ref>`, so the second
run of a workflow fetches nothing. A clone lands in a staging directory and is
renamed into place, so an interrupted fetch cannot leave a half-populated entry
behind, and two jobs racing for the same action are fine.

Fetching shells out to `git`, which means your existing credential setup
already applies to private actions. `MINACT_ACTIONS_TOKEN` — or `GITHUB_TOKEN`
— is also honoured, passed through a mode-`0600` credential file rather than on
the command line where every process on the machine could read it.
`GITHUB_SERVER_URL` points fetching at a GitHub Enterprise install.

A ref is cached under the name you wrote, so a branch stays pinned to whatever
it pointed at the first time. Delete its directory, or embed the engine with
`Engine::with_action_store(store.refreshing(true))`, to pick up new commits.

### Where an action runs

Actions follow their job. With the **local** runner they run here. With
**docker** they run inside the job's container, which is why the action cache
is bind-mounted into it — so `runs-on: ubuntu-latest` with a JavaScript action
really runs that action on Linux, provided the image has `node`. With **ssh**
the action directory is copied to the remote host with `rsync` before it runs,
once per job rather than once per step.

Two things do not follow the job, and both are deliberate:

* Registered actions run in-process on the host, as they always have.
* A **container action** runs on the host's Docker even when the job is on a
  remote host, with the host's workspace mounted. Over SSH that is the local
  copy, reconciled when the workspace syncs back at the end of the job — the
  same caveat the built-ins carry.

## Execution Model

minact aims to match GitHub Actions semantics. The details worth knowing:

**Environment.** Steps inherit your shell environment (so `git`, `cargo` and
`flutter` are on `PATH`), and minact adds the standard runner variables on top:
`CI`, `GITHUB_ACTIONS`, `GITHUB_WORKSPACE`, `GITHUB_REPOSITORY`, `GITHUB_REF`,
`GITHUB_REF_NAME`, `GITHUB_SHA`, `GITHUB_ACTOR`, `GITHUB_EVENT_NAME`,
`GITHUB_JOB`, `RUNNER_OS`, `RUNNER_ARCH`, `RUNNER_TEMP`, `RUNNER_TOOL_CACHE`.
Workflow `env` is layered under job `env`, which is layered under step `env`.
Job-level `env` does not leak into the next job.

**Shells.** `shell: bash` runs `bash --noprofile --norc -eo pipefail`, matching
GitHub — a failing command aborts the rest of the script, and a failing stage
fails the whole pipe. `shell: sh` runs `sh -e`. If `bash` is not installed,
minact falls back to `sh` and warns. A `shell:` value containing `{0}` is used
as a command template (e.g. `shell: python -u {0}`).

**Failure.** When a step fails, the remaining steps of that job are skipped
unless their `if:` says otherwise, so `if: always()` and `if: failure()` steps
still run. When a job fails or is skipped, jobs that `need` it are skipped
unless they carry their own `if:`. `continue-on-error: true` leaves
`steps.<id>.outcome` as `failure` while reporting `conclusion` as `success`,
and does not fail the job.

**Matrices.** `strategy.matrix` expands a job into one instance per
combination, in axis declaration order. `exclude` runs first, then `include` —
so a workflow can drop a broad set and add one case back. An `include` entry
merges into the combinations it is compatible with, or becomes a combination of
its own when it matches none.

Each instance gets an id of `job-id (values)` and its own `${{ matrix.* }}`,
which is available to the job's `name`, its `if:`, its `env`, and every step.
`${{ strategy.job-index }}` and `${{ strategy.job-total }}` identify the
instance. With the default `fail-fast: true`, the first failure cancels the
instances that have not started; `fail-fast: false` runs them all.

Dependent jobs see one combined result per job id: a failure in any instance
fails the job for everything that `needs` it. Job **outputs** from a matrix job
are last-instance-wins, the same caveat GitHub documents.

**Timeouts.** `timeout-minutes` is enforced on both a step and a whole job.
It cancels the same way stopping a run does, so the running process is actually
killed rather than left behind. A step that runs out of time is a *failure*,
not a cancellation — a cancelled run has to stay distinguishable from a step
that overran on its own. GitHub types the value as a number rather than an
integer, so `timeout-minutes: 0.5` is legal.

**`continue-on-error`.** On a step it leaves `steps.<id>.outcome` as `failure`
while reporting `conclusion` as `success`. On a *job* it does the same thing
one level up: the failure is reported, the workflow still passes, and jobs that
`need` it still run.

**Expressions.** An expression that cannot be evaluated fails the thing it
belonged to and says so. It is not substituted with its own source text —
that used to send a literal `${{ … }}` to the shell, where an unsupported
function surfaced as an unrelated syntax error from bash.

**Secrets.** Anything registered with `::add-mask::` — and `GITHUB_TOKEN`, from
the moment the run starts — is redacted everywhere minact prints, including the
echoed command line and action inputs, not just step output.

**The event payload.** `github.event.*` is empty unless you supply one. Point
`GITHUB_EVENT_PATH` at a JSON file and the payload becomes readable, which is
what makes a `pull_request` workflow testable locally:

```bash
GITHUB_EVENT_PATH=./event.json minact run --event pull_request
```

**Exit code.** `minact run` exits `1` if any job failed, `0` otherwise.

## Where Jobs Run

By default every job runs on your machine. A `runs-on:` label only means
something once you say what it maps to, in `.minact/config.yml`:

```yaml
runners:
  ubuntu-latest:
    type: docker
    image: ubuntu:24.04
  macos-latest:
    type: local
  windows-latest:
    type: ssh
    host: win-builder.local
    user: builder
    remote-workspace: C:/minact/workspace
```

See [examples/config.yml](examples/config.yml) for every option. Pass a
different file with `minact run --config <path>`.

**local** — this machine, the default.

**docker** — a container, so `runs-on: ubuntu-latest` genuinely runs on Linux
whatever your host is. The workspace and the runner temp directory are
bind-mounted at *identical paths* inside the container, so `GITHUB_WORKSPACE`,
`working-directory` and the `$GITHUB_*` files need no translation and files the
container writes appear in your workspace. One container per job, kept alive
across its steps so `$GITHUB_ENV` and `$GITHUB_PATH` carry over, and removed
when the job ends. Set `pull: true` to fetch the image, `run-args` for things
like `--platform linux/amd64`, and `binary: podman` to use a compatible CLI.

**ssh** — another machine, for what a container cannot provide: Windows, or
real macOS hardware for signing. The workspace is pushed with `rsync` before
the job and pulled back afterwards (`sync: false` if the remote manages its own
checkout). Requires key-based login that already works non-interactively.

`runs-on` is evaluated as an expression, so one job definition can land on a
different runner per matrix instance:

```yaml
jobs:
  build:
    runs-on: ${{ matrix.on }}
    strategy:
      matrix:
        on: [ubuntu-latest, macos-latest]
```

**When a label is not mapped**, the job runs here and minact says so. It will
not quietly run a `runs-on: windows-latest` job on a Mac and report green.

### `container:` and `services:`

`jobs.<job_id>.container` says what the steps run *in*, while `runs-on:` only
says which machine picks the job up — so a job with a `container:` runs there
whatever its label maps to, and needs no `config.yml` entry at all:

```yaml
jobs:
  build:
    container:
      image: node:20-alpine
      env:
        CI_FLAVOUR: container
      volumes: [/tmp/cache:/tmp/cache]
      options: --cpus 1
    steps:
      - run: node --version
```

The bare form `container: node:20-alpine` works too. `credentials:` is not
acted on — run `docker login` yourself for a private image.

`services:` cannot work without container networking, so minact **says** it is
not starting them rather than passing and meaning nothing. A workflow that
needs a database will tell you it did not get one.

### Limits worth knowing

* Built-in actions (`actions/checkout`, `actions/upload-artifact`) run
  in-process on the host. With Docker that is fine — the workspace is the same
  filesystem. Over SSH they act on the *local* copy, which is only reconciled
  when the workspace syncs back at the end of the job. Container actions carry
  the same caveat; see [Actions](#where-an-action-runs).
* A JavaScript action needs `node` wherever it runs. A container image without
  one — `ubuntu:24.04`, say — will fail the step rather than the run.
* `docker` needs a running daemon; `ssh` needs `ssh` and `rsync` on your PATH.
* Cancelling a container step kills the container. Cancelling an SSH step
  closes the connection, which hangs up the remote shell but cannot guarantee
  its grandchildren die.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+ (for building from source)
- Git — for `actions/checkout`, and for fetching `uses: owner/repo@ref`
- Node — only to run JavaScript actions, which is most published ones
- Docker — only to run container actions

## Installation

```bash
cargo build --release
./target/release/minact --help
```

## Quick Start

Create a workflow file at `.minact/workflows/ci.yml`:

```yaml
name: CI Pipeline
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_dispatch:
    inputs:
      version:
        description: "Version to build"
        required: false

env:
  APP_NAME: my-app
  NODE_ENV: production

jobs:
  setup:
    name: Setup
    steps:
      - name: Checkout code
        uses: actions/checkout@v4
      - name: Print info
        run: |
          echo "Running on ${{ runner.os }}"
          echo "Workspace: ${{ github.workspace }}"
          echo "App: ${{ env.APP_NAME }}"

  build:
    name: Build
    needs: [setup]
    steps:
      - name: Install dependencies
        run: echo "Installing dependencies..."
      - name: Build project
        run: echo "Building ${{ env.APP_NAME }}..."
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: build-output
          path: ./dist

  test:
    name: Test
    needs: [setup]
    steps:
      - name: Run tests
        run: echo "Running tests..."
      - name: Upload results
        uses: actions/upload-artifact@v4
        with:
          name: test-results
          path: ./test-results

  deploy:
    name: Deploy
    needs: [build, test]
    if: github.event_name == 'push' || github.event_name == 'workflow_dispatch'
    steps:
      - name: Deploy application
        run: echo "Deploying ${{ env.APP_NAME }}..."
      - name: Verify deployment
        run: echo "Verifying deployment..."
```

Run it:

**Using the compiled binary:**

```bash
# Auto-discover and run
minact run

# Specify a file
minact run --file examples/ci.yml

# Simulate a specific event
minact run --event push

# Pass input parameters
minact run --input version=1.0.0

# Emit structured JSON log events
minact run --log-format json

# Use compact fixed-prefix logs
minact run --log-format plain
```

**Using cargo:**

```bash
cargo run -- run
cargo run -- run --file examples/ci.yml
cargo run -- run --event push
cargo run -- run --input version=1.0.0
cargo run -- run --log-format json
cargo run -- run --log-format plain
```

## Commands

| Command | Description |
|---------|-------------|
| `minact run` | Run a workflow |
| `minact list` | List workflows in the project |
| `minact validate <file>` | Validate a workflow file |
| `minact studio` | Open the Studio web UI |

### `minact run`

```
Usage: minact run [OPTIONS]

Options:
  -f, --file <FILE>        Workflow file path (auto-discover if omitted)
  -e, --event <EVENT>      Event type to simulate [default: workflow_dispatch]
  -w, --workspace <DIR>    Working directory [default: current directory]
  -i, --input <KEY=VALUE>  Input parameters (can be specified multiple times)
      --log-format <FMT>   Log output format: pretty, plain, or json [default: pretty]
```

### `minact list`

```
Usage: minact list [OPTIONS]

Options:
  -d, --dir <DIR>   Project directory [default: current directory]
  -v, --verbose     Show detailed information
```

### `minact validate`

```
Usage: minact validate <FILE>

Arguments:
  <FILE>  Path to the workflow file to validate
```

### `minact studio`

```
Usage: minact studio [OPTIONS]

Options:
  -p, --port <PORT>      Port to listen on, 0 picks a free one [default: 4000]
      --host <HOST>      Address to bind [default: 127.0.0.1]
  -w, --workspace <DIR>  Project directory to serve [default: current directory]
      --workflows <DIR>  Extra directory of workflow files (repeatable)
      --open             Open the UI in the default browser
```

`--workflows` mounts a directory that is not a project layout, so a folder of
loose workflow files can be browsed and run like any other:

```bash
minact studio --workflows examples --open
```

Studio serves a visual view of the workspace: every discovered workflow, its
job DAG laid out the way the scheduler will execute it, and the steps, YAML and
environment behind each one. From there you can run a workflow and watch it —
the DAG colours in as jobs finish, steps report their durations, and the log
streams live. A run can be cancelled from the UI, which kills the running
command rather than waiting it out.

Runs are recorded under `.minact/runs/<n>/` as `meta.json` plus an
`events.jsonl` of the engine's event stream, so history survives restarting
Studio and any run can be replayed. The run list filters by workflow and
status, a run downloads as a plain-text log, and the Artifacts screen browses,
previews and downloads whatever `actions/upload-artifact` left in
`.minact-artifacts/`.

It binds to loopback by default. Studio can run workflows, which means running
shell commands on this machine, so binding it to a reachable address hands a
shell to anyone who can reach the port — the command warns when you do.

## Workflow File Discovery

minact collects workflow files from all of these locations:

1. `.minact/workflows/*.yml` / `.minact/workflows/*.yaml`
2. `.github/workflows/*.yml` / `.github/workflows/*.yaml` — compatible with GitHub Actions layout

`minact list` shows everything it found. `minact run` without `--file` requires
exactly one match; if there are several, pass `--file` to pick one.

These are only the defaults. A tool embedding the engine passes its own
locations instead of expecting minact to know its layout:

```rust
use minact_core::{SearchPath, WorkflowParser};

let workflows = WorkflowParser::discover_workflows_in(
    project_dir,
    &[SearchPath::dir(".mytool/workflows")],
)?;
```

The same applies to the config file: `.minact/config.yml` is the default, and
`Config::discover_in(project_dir, &[".mytool/config.yml"])` looks wherever the
caller says.

## Project Structure

```
minact/
├── Cargo.toml               # Workspace manifest
├── crates/
│   ├── core/                # Core engine library
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── types.rs     # Context, Value, StepResult, RunStatus, etc.
│   │   │   ├── workflow.rs  # Workflow / Job / Step models
│   │   │   ├── expr.rs      # Expression parser & evaluator
│   │   │   ├── parser.rs    # Workflow YAML parsing & discovery
│   │   │   ├── scheduler.rs # Job DAG scheduler
│   │   │   ├── matrix.rs    # strategy.matrix expansion
│   │   │   ├── config.rs    # .minact/config.yml, runs-on -> runner mapping
│   │   │   ├── executor/    # where steps run: local, docker, ssh
│   │   │   ├── commands.rs  # $GITHUB_OUTPUT and `::` workflow commands
│   │   │   ├── logging.rs   # Structured log events & the Reporter trait
│   │   │   ├── reporters.rs # Pretty / plain / JSON console reporters
│   │   │   ├── engine.rs    # Execution engine
│   │   │   └── actions/     # Actions, built-in and external
│   │   │       ├── mod.rs        # Action trait & registry
│   │   │       ├── reference.rs  # Parsing `uses:` values
│   │   │       ├── store.rs      # Fetching and caching remote actions
│   │   │       ├── manifest.rs   # action.yml
│   │   │       ├── external.rs   # Resolution and input mapping
│   │   │       └── container.rs  # Running container actions
│   │   └── tests/
│   │       ├── engine.rs    # End-to-end engine behaviour
│   │       ├── matrix.rs    # End-to-end matrix behaviour
│   │       ├── actions.rs   # End-to-end `uses:` behaviour
│   │       └── cross_platform.rs # Docker/SSH runner behaviour
│   └── studio/              # Web UI for the engine
│       ├── src/             # axum router, DTOs, embedded assets
│       └── web/             # TanStack Start + Tailwind front-end
├── apps/
│   └── cli/                 # CLI binary
│       └── src/
│           └── main.rs
└── examples/
    ├── ci.yml               # Example workflow
    ├── outputs.yml          # Step/job outputs and failure control flow
    ├── matrix.yml           # Build matrices, include/exclude, fail-fast
    ├── actions.yml          # Built-in, remote, local and container actions
    ├── actions/             # A local composite action actions.yml uses
    └── config.yml           # Project config: runs-on -> runner mapping
```

## Embedding the Engine

`minact-core` is usable as a library. Register your own actions, reuse the
built-in reporters, and point discovery at your own layout:

```rust
use std::sync::Arc;
use minact_core::{Engine, PrettyReporter, SearchPath, WorkflowParser};

let workflows = WorkflowParser::discover_workflows_in(
    project_dir,
    &[SearchPath::Directory(".mytool/workflows")],
)?;

let engine = Engine::with_actions_and_reporter(
    workspace,
    registry,                          // your ActionRegistry
    Arc::new(PrettyReporter::default()),
);
let result = engine.run_workflow(&workflows[0], "push", inputs).await?;
minact_core::print_pretty_summary(&result);
```

### Studio HTTP API

Studio's UI is a client of this API; so can anything else be.

| Endpoint | Purpose |
|----------|---------|
| `GET /api/meta` | Workspace path, runner, version, registered actions |
| `GET /api/workflows` | Discovered workflows, including ones that fail to parse |
| `GET /api/workflows/{id}` | Parsed workflow, raw YAML, and the layered graph |
| `POST /api/runs` | `{ workflow_id, event, inputs }` → `202` with the new run |
| `GET /api/runs` | Run history; `?workflow=`, `?status=`, `?limit=` |
| `GET /api/runs/{id}` | Run metadata plus jobs, steps, conclusions and durations |
| `GET /api/runs/{id}/events` | SSE. `?from=<seq>` replays, then follows live |
| `POST /api/runs/{id}/cancel` | Stop a run |
| `GET /api/runs/{id}/logs` | Plain text; `?job=` for one job instance |
| `GET /api/artifacts` | Artifacts with their files and sizes |
| `GET /api/artifacts/{name}/{path}` | One file out of an artifact |

Workflow ids are the workspace-relative path, base64url-encoded — opaque, but
derived from the file rather than from a database.

### Mounting extra workflow directories

`minact-studio` takes the same option as a library, for an application that
keeps its workflows somewhere minact does not search:

```rust
StudioServer::new(workspace)
    .with_workflow_dirs(["examples", "/srv/shared-workflows"])
    .serve(addr)
    .await?;
```

Relative paths resolve against the workspace. A directory that the default
search already covers is not listed twice.

## Watching a Run From Code

The engine reports through the `Reporter` trait. `emit` receives the bare
event; `emit_record` receives it wrapped in a `LogRecord` carrying a sequence
number, a timestamp and the job instance and step it belongs to. The engine
calls `emit_record`, and the default implementation forwards to `emit`, so a
console reporter needs nothing extra — implement `emit_record` when you need to
order events or attribute them:

```rust
use minact_core::{LogRecord, Reporter};

#[async_trait::async_trait]
impl Reporter for MyReporter {
    async fn emit(&self, _event: LogEvent) {}

    async fn emit_record(&self, record: LogRecord) {
        // record.seq, record.ts, record.scope.job_id, record.scope.step_index
        self.sink.send(record).await;
    }
}
```

A run can be stopped by passing a token:

```rust
use minact_core::CancellationToken;

let cancel = CancellationToken::new();
let result = engine
    .run_workflow_cancellable(&workflow, "push", inputs, cancel.clone())
    .await?;
```

Cancelling kills the running step's process group, so a build the step started
stops too. `run_workflow` is the same call with a token nobody cancels.

## Building Studio

Studio's front-end is a TanStack Start app in SPA mode. `cargo build` embeds
whatever is in `crates/studio/web/dist/client`, so build the front-end first:

```bash
cd crates/studio/web
npm install
npm run build
```

Then build the binary as usual. Without that step the binary still compiles and
runs — it just serves a placeholder page saying the front-end is missing.

For front-end work, run the Rust server and Vite side by side:

```bash
minact studio --workspace /path/to/project   # API on :4000
cd crates/studio/web && npm run dev          # UI on :3000, proxies /api
```

`minact-studio` is a library as well as a CLI subcommand. An application that
embeds the engine can mount the same router with its own actions:

```rust
use minact_studio::StudioServer;

StudioServer::new(workspace)
    .with_actions(registry)     // your ActionRegistry
    .serve("127.0.0.1:4000".parse()?)
    .await?;
```

## Contributing

Contributions are welcome! Feel free to open an issue or submit a pull request.

`cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test --workspace` all run in CI on Linux and macOS, so run them before
opening a pull request.

## License

MIT
