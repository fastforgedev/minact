# minact

> A lightweight GitHub Actions-compatible workflow runner

minact is a lightweight tool that runs GitHub Actions-compatible workflows on your local machine. It parses standard YAML workflow files and executes jobs and steps in dependency order — no Docker daemon, no hosted runners, just your local environment.

## Features

- **Drop-in compatible** — Use your existing `.yml`/`.yaml` workflow files as-is
- **Expression evaluation** — Full support for `${{ github.* }}`, `${{ env.* }}`, `${{ secrets.* }}`, `${{ inputs.* }}`, `${{ needs.* }}`, and functions like `contains()`, `startsWith()`, `success()`, `failure()`
- **Conditional execution** — Skip jobs and steps with `if:` conditions
- **Built-in Actions** — Ships with `actions/checkout`, `actions/cache`, `actions/upload-artifact`, `actions/download-artifact`
- **Shell flexibility** — Run steps with `bash`, `sh`, `python`, or `node`
- **DAG scheduler** — Resolves job dependencies (`needs:`) and executes them in topological order
- **Auto-discovery** — Finds workflows in `.minact/workflows/`, `.github/workflows/`, or the project root
- **Event simulation** — Trigger workflows as `push`, `pull_request`, `workflow_dispatch`, or any event you specify

## Supported Syntax

| Category | Syntax | Status |
|----------|--------|--------|
| Events | `on: push` / `on: [push, pull_request]` / `on: { push: { branches: [main] } }` | ✅ |
| Environment | `env:` at workflow / job / step level | ✅ |
| Job deps | `jobs.<job_id>.needs` | ✅ |
| Conditions | `jobs.<job_id>.if` / `steps[].if` | ✅ |
| Outputs | `jobs.<job_id>.outputs` | ✅ |
| Steps | `steps[].uses`, `steps[].run`, `steps[].with` | ✅ |
| Step options | `continue-on-error`, `shell`, `working-directory` | ✅ |
| Contexts | `${{ github.* }}`, `${{ env.* }}`, `${{ secrets.* }}` | ✅ |
| More contexts | `${{ runner.* }}`, `${{ inputs.* }}`, `${{ needs.* }}` | ✅ |
| Functions | `contains()`, `startsWith()`, `endsWith()` | ✅ |
| Status checks | `success()`, `failure()`, `always()` | ✅ |
| Matrix strategy | `strategy.matrix` | 📋 Planned |
| Runner selection | `runs-on` | 📋 Planned |

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+ (for building from source)
- Git (for `actions/checkout`)

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

## Workflow File Discovery

minact searches for workflow files in the following order:

1. `.minact/workflows/*.yml` / `.minact/workflows/*.yaml`
2. `.github/workflows/*.yml` / `.github/workflows/*.yaml` — compatible with GitHub Actions layout
3. `minact.yml` / `minact.yaml` — project root

The first match wins.

## Project Structure

```
minact/
├── Cargo.toml              # Workspace manifest
├── crates/
│   └── core/               # Core engine library
│       └── src/
│           ├── lib.rs
│           ├── types.rs    # Context, Value, StepResult, etc.
│           ├── workflow.rs # Workflow / Job / Step models
│           ├── expr.rs     # Expression parser & evaluator
│           ├── parser.rs   # Workflow YAML parser
│           ├── scheduler.rs# Job DAG scheduler
│           ├── engine.rs   # Execution engine
│           └── actions/    # Built-in GitHub Actions
│               └── mod.rs
├── apps/
│   └── cli/                # CLI binary
│       └── src/
│           └── main.rs
└── examples/
    └── ci.yml              # Example workflow
```

## Contributing

Contributions are welcome! Feel free to open an issue or submit a pull request.

## License

MIT
