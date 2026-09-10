# Working in this repository

Rules for anyone — human or agent — making changes here. `CLAUDE.md` is a
symlink to this file.

## Commits

- Commit messages carry **no trailers**: no `Co-Authored-By`, no
  `Generated with`, no tool attribution of any kind. The author is the
  person whose git identity made the commit.
- Subject line in the style of the history: `feat(scope): …`, `fix(scope): …`,
  `docs: …`, `test(core): …`, `chore: …`. Scope is the crate or area
  (`core`, `studio`, `cli`) when it helps; leave it off for changes that span
  the tree.
- Body explains why, in prose, wrapped at 72 columns. Bullet lists for
  changes that have several independent parts.
- Commit directly on `main`; this project does not use feature branches or
  pull requests for its own work. Do not push unless asked.
- Never commit `.minact/` at the repository root (local runner config with
  machine addresses), nor run output such as `.minact/artifacts/` or
  `.minact/_work/`.

## Before committing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three must be clean. `rustfmt` on `crates/core/src/executor/mod.rs`
formats its child modules too, so expect `docker.rs`, `local.rs` and
`ssh.rs` to move when it runs.

The Studio front-end lives in `crates/studio/web` and is embedded into the
binary from `web/dist`, which is not tracked. After touching anything under
`web/src`, run `npm run build` there so the embedded page matches the code.

## Runtime directory layout

Everything minact needs at run time hangs off one anchor,
`<workspace>/.minact/_work`, resolved in `crates/core/src/layout.rs`. The
design follows GitHub's own runner and these rules keep it coherent:

- **One anchor.** New runtime paths derive from `WorkDir`; do not compute a
  path from `std::env::temp_dir()`, `dirs::*` or `$HOME` in the engine or an
  executor.
- **Underscore means the runner's.** `_temp`, `_tool`, `_actions` are
  minact's own and may be deleted at any time. Anything a user will look at —
  `.minact/artifacts`, `.minact/runs`, `.minact/config.yml`,
  `.minact/workflows` — has no underscore and does not live under `_work`.
- **Same relative place on every side.** A remote runner keeps
  `.minact/_work` under its workspace with the same sub-directories, so a
  host path maps to a remote one by swapping the workspace prefix. A sync
  never carries `_work` in either direction.
- **Per-job scratch is a directory of the job's own** under `_temp`, with a
  random suffix, removed when the job ends. Two runs can share a workspace
  (Studio starts one per request), so never wipe `_temp` wholesale.
- **Containers get a job-scoped `$HOME`** at `_temp/<job>/_github_home`;
  the host's home does not exist inside them.
- **Tool caches use the `actions/toolkit` layout**:
  `<tool>/<version>/<arch>/` with a `<arch>.complete` marker, so
  `setup-*` actions and minact find each other's installs.

## Engine conventions

- Docker and container actions bind-mount host directories at the **same
  absolute paths** inside the container. Nothing translates paths; keep it
  that way rather than adding a mapping layer.
- `${{ }}` expressions are evaluated strictly. An expression that cannot be
  evaluated is an error; never fall back to passing the raw text through.
- `owner/repo@ref` actions resolve registry-first: a registered
  (in-process) action of that name wins over fetching the repository.
- Cross-platform paths are verified against real targets, not simulated:
  Docker for Linux, the SSH runner for Windows
  (`MINACT_SSH_HOST=… cargo test -p minact-core --test cross_platform -- --ignored`).
