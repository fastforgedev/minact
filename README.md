# minact — Run GitHub Actions workflows locally

minact is a lightweight tool that runs GitHub Actions-compatible workflows on your local machine. It parses standard YAML workflow files and executes jobs and steps in dependency order, right on your local environment.

## Features

- **GitHub Actions compatible** — Directly uses `.yml`/`.yaml` format with `on`/`jobs`/`steps`/`needs`/`if` syntax
- **Expression evaluation** — Supports `${{ github.ref }}`, `${{ env.FOO }}`, `${{ secrets.KEY }}`, and functions like `contains()`, `startsWith()`
- **Conditional execution** — `if: github.event_name == 'push'` to skip jobs/steps
- **Built-in Actions** — `actions/checkout`, `actions/cache`, `actions/upload-artifact`, `actions/download-artifact`
- **Shell steps** — Run shell commands via `run:` with `bash`/`sh`/`python`/`node` support
- **Job dependencies** — DAG resolution via `needs:`, executed in topological order
- **Workflow discovery** — Auto-search in `.minact/workflows/`, `.github/workflows/`, `minact.yml`

## Installation

```bash
# Build from source
cargo build --release
./target/release/minact --help
```

## Quick Start

Create a workflow file `.minact/workflows/ci.yml`:

```yaml
name: CI
on: [push, pull_request]

env:
  APP: my-app

jobs:
  build:
    name: Build
    steps:
      - name: Checkout
        uses: actions/checkout@v4
      - name: Build
        run: echo "Building ${{ env.APP }}..."
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: build-output
          path: ./dist

  test:
    name: Test
    needs: [build]
    steps:
      - name: Run tests
        run: echo "Running tests..."

  deploy:
    name: Deploy
    needs: [test]
    if: github.event_name == 'push'
    steps:
      - name: Deploy
        run: echo "Deploying..."
```

Run:

```bash
# Auto-discover and run
minact run

# Specify a file
minact run --file examples/ci.yml

# Simulate a specific event
minact run --event push

# Pass input parameters
minact run --input version=1.0.0
```

## Commands

| Command | Description |
|---------|-------------|
| `minact run` | Run a workflow |
| `minact list` | List workflows in the project |
| `minact validate <file>` | Validate a workflow file |

### minact run

```
Usage: minact run [OPTIONS]

Options:
  -f, --file <FILE>        Workflow file path (auto-discover if omitted)
  -e, --event <EVENT>      Event type to simulate [default: workflow_dispatch]
  -w, --workspace <DIR>    Working directory [default: current directory]
  -i, --input <KEY=VALUE>  Input parameters (can be specified multiple times)
```

### minact list

```
Usage: minact list [OPTIONS]

Options:
  -d, --dir <DIR>   Project directory [default: current directory]
  -v, --verbose     Show detailed information
```

## Workflow File Discovery

Auto-discovery searches in the following order:

1. `.minact/workflows/*.yml` / `.minact/workflows/*.yaml`
2. `.github/workflows/*.yml` / `.github/workflows/*.yaml` (GitHub Actions compatible)
3. `minact.yml` / `minact.yaml` (project root)

## Project Structure

```
minact/
├── Cargo.toml              # Workspace config
├── crates/
│   └── core/               # Core engine library
│       └── src/
│           ├── lib.rs
│           ├── types.rs    # Context, Value, StepResult, etc.
│           ├── workflow.rs  # Workflow/Job/Step/YAML models
│           ├── expr.rs     # Expression parser + evaluator
│           ├── parser.rs   # Workflow YAML parser
│           ├── scheduler.rs # Job DAG scheduler
│           ├── engine.rs   # Execution engine
│           └── actions/    # Built-in actions
│               └── mod.rs
├── apps/
│   └── cli/                # CLI binary
│       └── src/
│           └── main.rs
└── examples/
    └── ci.yml              # Example workflow
```

## Supported Syntax

| Syntax | Status |
|--------|--------|
| `on: push` / `on: [push, pull_request]` / `on: { push: { branches: [main] } }` | ✅ |
| `env:` | ✅ |
| `jobs.<job_id>.needs` | ✅ |
| `jobs.<job_id>.if` | ✅ |
| `jobs.<job_id>.outputs` | ✅ |
| `steps[].uses` | ✅ |
| `steps[].run` | ✅ |
| `steps[].with` | ✅ |
| `steps[].if` | ✅ |
| `steps[].continue-on-error` | ✅ |
| `steps[].shell` | ✅ |
| `steps[].working-directory` | ✅ |
| `${{ github.* }}` / `${{ env.* }}` / `${{ secrets.* }}` | ✅ |
| `${{ runner.* }}` / `${{ inputs.* }}` / `${{ needs.* }}` | ✅ |
| `${{ contains() }}` / `${{ startsWith() }}` / `${{ endsWith() }}` | ✅ |
| `${{ success() }}` / `${{ failure() }}` / `${{ always() }}` | ✅ |
| Matrix strategy | 📋 Planned |
| `runs-on` selection | 📋 Planned |

## License

MIT
