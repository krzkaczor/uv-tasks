# uv-tasks

A task runner for [uv workspaces](https://docs.astral.sh/uv/concepts/projects/workspaces/), written in Rust. Think npm scripts plus `pnpm -r`, but native to `pyproject.toml`.

uv has no task runner ([astral-sh/uv#5903](https://github.com/astral-sh/uv/issues/5903)) and no way to run a command in every workspace member. uv-tasks fills that gap: each package declares tasks in its own `pyproject.toml`, and it runs them — in one package, or across the whole workspace in parallel while respecting the dependency graph.

## Install

Add it as a dev dependency in the workspace root:

```sh
uv add --dev uv-tasks
```

The PyPI distribution is named `uv-tasks`; the installed command is `ut`, invoked as `uv run ut ...`. The wheels ship a prebuilt binary for macOS and Linux — no Rust toolchain needed. Everyone on the team gets the pinned version automatically on `uv sync`.

## Define tasks

Add a `[tool.ut.tasks]` table to a workspace member's `pyproject.toml`:

```toml
[tool.ut.tasks]
test = "pytest -s"
typecheck = "ty check ."
check = ["ruff check .", "ty check ."]   # a sequence: stops at the first failure
```

The workspace root can define tasks too, typically to fan out across the workspace:

```toml
# root pyproject.toml
[tool.ut.tasks]
test = "ut run -w test"                  # run every member's "test"
check = ["ut run -w lint", "ut run -w test"]
```

Before anything runs, `ut` brings the workspace environment up to date with a single `uv sync --all-packages`. Commands then run through `sh -c` in the package's directory with the workspace venv activated (`<venv>/bin` on `PATH`, `VIRTUAL_ENV` set) — no per-command `uv run`, so parallel tasks never contend on uv's sync lock. The directory containing `ut` itself is prepended to `PATH`, so task definitions call bare `ut` (as above) even though it only lives in the project venv; nested `ut` invocations spawned this way skip the redundant sync.

## Run tasks

```sh
uv run ut test                    # run "test" in the package containing the current directory
                                  # (or the root's "test" when run from the workspace root)
uv run ut test -- -k "scope"      # extra args are appended to the command
uv run ut run -w test             # run "test" in every member that defines it
uv run ut run -w --filter pkg test  # restrict to the named package(s)
uv run ut run -w -s test          # run sequentially instead of in parallel
uv run ut list                    # members in dependency order, with their tasks
```

`ut <task>` is shorthand for `ut run <task>`, so `uv run ut test` and `uv run ut run test` are equivalent. If a task name collides with a built-in subcommand (`run`, `list`, `completion`, `help`), use the explicit `ut run <task>` form.

### Workspace runs

`ut run -w <task>`:

- Discovers members from `[tool.uv.workspace]` in the root `pyproject.toml` (glob `members`, `exclude`). The root itself is never a `-w` target, like `pnpm -r`: root tasks run with `ut <task>` from the root and can fan out with `ut run -w <task>` without recursing into themselves.
- Builds the dependency graph from `[tool.uv.sources] <name> = { workspace = true }` entries, with requirement-name matching as a fallback.
- Runs in parallel by default, up to the number of logical CPUs; pass `-s`/`--sequential` to run one member at a time. A member's task starts only after the tasks of all its workspace dependencies succeed — including through members that don't define the task.
- Skips members that don't define the task, like `pnpm -r`.
- Streams output line by line, prefixed with the member name.
- Fails fast: after the first failure no new tasks start, in-flight tasks finish, and `ut` exits with code 1, listing failed and skipped members.

## Design notes

uv-tasks parses `[tool.uv.workspace]` itself and shells out to `uv` instead of linking uv's Rust crates. The `pyproject.toml` format and the uv CLI are uv's stable surfaces; the crates are explicitly unstable (versioned `0.0.x`, patch-bumped on every uv release). uv-tasks only needs workspace metadata — members, names, dependency edges — which is a small amount of TOML and glob parsing.

Known limitations:

- Tasks run through `sh -c`, so Windows isn't supported.
- Passthrough args work only for string-form tasks, not sequences.
- The venv is synced with `--all-packages`, so every member's dependencies are installed together and a member can import a package it doesn't declare. Per-member dependency isolation (what `uv run --package` gives you) is traded for speed — dependency *versions* were never isolated anyway, since a uv workspace resolves to a single lockfile.
- The venv is located the way uv locates it: `UV_PROJECT_ENVIRONMENT` if set (relative values resolve against the workspace root), else `<root>/.venv`; an already-activated `VIRTUAL_ENV` is ignored, as uv does by default.

Not yet implemented: task-level `depends`, pre/post hooks, continue-on-error (`--no-bail`), buffered per-package output.

## Develop

```sh
cargo test      # unit + integration tests; tests/cli.rs stubs uv, tests/e2e.rs runs the real uv against tests/fixtures/acme
cargo clippy --all-targets
cargo fmt
```

## Release

Pushing a git tag builds macOS (arm64, x86_64) and manylinux (x86_64, aarch64) wheels plus an sdist, and publishes them to PyPI through trusted publishing (`.github/workflows/release.yml`). The version comes from `Cargo.toml`. To release: bump the version, commit, tag, push the tag.
