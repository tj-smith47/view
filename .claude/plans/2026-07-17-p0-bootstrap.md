# view P0 — Repo Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A building, linting, CI-wired Cargo workspace with all eight view crates as empty-but-compiling skeletons, a Taskfile encoding the project workflow, and repo hygiene files.

**Architecture:** Cargo workspace at `/opt/repos/view` with crates under `crates/`. Strict workspace lints (no unwrap/expect/panic in lib code). Taskfile is the single entry point for build/fmt/lint/test; CI mirrors `task ci` exactly.

**Tech Stack:** Rust (stable, edition 2021), cargo workspace lints, Taskfile (go-task), GitHub Actions.

## Global Constraints

- Repo root: `/opt/repos/view`; branch `master`; git identity already configured locally.
- Commits are fine and expected per task. Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- After Task 2 exists, commit via `task commit -- -m "<msg>"`; before that, plain `git commit`.
- Workspace lints (exact values in Task 1) apply to every crate forever: `unsafe_code = "deny"`, clippy `unwrap_used`/`expect_used`/`panic` = deny. Test modules may open with `#![allow(clippy::unwrap_used, clippy::expect_used)]` — test code only, never lib code.
- Inline comments: WHY-only (constraint, invariant, workaround). No session narrative, no "Phase/Task/Step" markers, no "§" chapter refs in code or comments.
- Dev prerequisite: `nvim` ≥ 0.11 on PATH (used from P1 on; CI installs it now so the workflow never changes shape later).
- Rust toolchain: current stable. Install crates with `cargo add` (never hand-pin guessed versions).

---

### Task 1: Cargo workspace + eight crate skeletons

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/view/Cargo.toml`, `crates/view/src/main.rs`
- Create: `crates/view-core/Cargo.toml`, `crates/view-core/src/lib.rs`
- Create: `crates/view-engine/Cargo.toml`, `crates/view-engine/src/lib.rs`
- Create: `crates/view-surface/Cargo.toml`, `crates/view-surface/src/lib.rs`
- Create: `crates/view-native/Cargo.toml`, `crates/view-native/src/lib.rs`
- Create: `crates/view-ai/Cargo.toml`, `crates/view-ai/src/lib.rs`
- Create: `crates/view-tui/Cargo.toml`, `crates/view-tui/src/lib.rs`
- Create: `crates/view-oracle/Cargo.toml`, `crates/view-oracle/src/lib.rs`
- Create: `crates/view-bench/Cargo.toml`, `crates/view-bench/src/lib.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: workspace layout every later task adds code into; the lint wall every later task builds under.

- [ ] **Step 1: Write the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
    "crates/view",
    "crates/view-core",
    "crates/view-engine",
    "crates/view-surface",
    "crates/view-native",
    "crates/view-ai",
    "crates/view-tui",
    "crates/view-oracle",
    "crates/view-bench",
]

[workspace.package]
version = "0.0.1"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/tj-smith47/view"

[workspace.lints.rust]
unsafe_code = "deny"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
unimplemented = "deny"
dbg_macro = "deny"
```

- [ ] **Step 2: Write each crate's `Cargo.toml`**

For every lib crate (`view-core`, `view-engine`, `view-surface`, `view-native`, `view-ai`, `view-tui`, `view-oracle`, `view-bench`), substituting the crate name:

```toml
[package]
name = "view-core"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[lints]
workspace = true
```

For the bin crate `crates/view/Cargo.toml`:

```toml
[package]
name = "view"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "view"
path = "src/main.rs"

[lints]
workspace = true
```

- [ ] **Step 3: Write the source stubs**

Each lib crate gets a `src/lib.rs` containing only a crate doc comment naming its one responsibility (rustdoc renders for users, so a WHAT summary is correct here). Exact content per crate:

```rust
// crates/view-core/src/lib.rs
//! Pure application state: Model, Msg, and update(). No I/O, no rendering.
```

```rust
// crates/view-engine/src/lib.rs
//! Embedded Neovim lifecycle and msgpack-RPC client.
```

```rust
// crates/view-surface/src/lib.rs
//! The render model: what to draw, independent of any frontend.
```

```rust
// crates/view-native/src/lib.rs
//! Native features: picker, file tree, statusline, notifications, palette.
```

```rust
// crates/view-ai/src/lib.rs
//! Agent Client Protocol integration: sessions, panel state, diff review.
```

```rust
// crates/view-tui/src/lib.rs
//! Terminal frontend: paints the surface, reads input, owns the terminal.
```

```rust
// crates/view-oracle/src/lib.rs
//! Differential test harness against a reference Neovim.
```

```rust
// crates/view-bench/src/lib.rs
//! Performance measurement harness: micro-benches and end-to-end latency.
```

And the bin:

```rust
// crates/view/src/main.rs
fn main() {
    println!("view: nothing here yet");
}
```

- [ ] **Step 4: Verify the workspace builds and lints clean**

Run: `cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both succeed with zero warnings.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: workspace skeleton with eight crates and strict lints

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Taskfile — the project workflow entry point

**Files:**
- Create: `Taskfile.yml`

**Interfaces:**
- Consumes: the workspace from Task 1.
- Produces: `task build|fmt|lint|test|ci|commit` — the commands every later task and CI use verbatim.

- [ ] **Step 1: Write `Taskfile.yml`**

```yaml
version: "3"

tasks:
  build:
    desc: Build the whole workspace
    cmd: cargo build --workspace

  fmt:
    desc: Format all Rust code
    cmd: cargo fmt --all

  fmt-check:
    desc: Verify formatting without writing
    cmd: cargo fmt --all --check

  lint:
    desc: Clippy with warnings as errors
    cmd: cargo clippy --workspace --all-targets -- -D warnings

  test:
    desc: Run the full test suite (requires nvim >= 0.11 on PATH)
    cmd: cargo test --workspace

  audit:
    desc: Enforce crate dependency direction from the spec
    cmd: bash scripts/audit-deps.sh

  style:
    desc: Enforce comment and doc style rules
    cmd: bash scripts/check-style.sh

  ci:
    desc: The full local gate; CI mirrors exactly this
    cmds:
      - task: fmt-check
      - task: lint
      - task: audit
      - task: style
      - task: test

  commit:
    desc: Gated commit — runs the ci chain first
    cmds:
      - task: ci
      - git add -A
      - git commit {{.CLI_ARGS}}
```

- [ ] **Step 2: Add the dependency-direction audit**

Create `scripts/audit-deps.sh` enforcing the spec's crate boundaries — core
depends on nothing in-workspace; surface only on core; native/ai never on
engine or tui or each other; only engine may depend on rmpv; only tui/bench
may depend on crossterm/ratatui. Uses `cargo metadata` (not TOML grepping) so
resolved dependency names are reported regardless of aliasing, quoting style,
workspace inheritance, or target gating:

```bash
#!/usr/bin/env bash
set -euo pipefail
for tool in cargo jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "AUDIT FAIL: $tool is required" >&2; exit 1; }
done
meta="$(cargo metadata --format-version 1 --no-deps)"
fail=0
check_absent() { # usage: check_absent <crate> <forbidden-dep>
  if jq -e --arg c "$1" --arg d "$2" \
    '.packages[] | select(.name == $c) | .dependencies[] | select(.name == $d)' \
    <<<"$meta" >/dev/null; then
    echo "AUDIT FAIL: $1 must not depend on $2"; fail=1
  fi
}
for dep in view view-engine view-tui view-surface view-native view-ai view-oracle view-bench rmpv crossterm ratatui tokio async-std smol; do
  check_absent view-core "$dep"
done
for dep in view view-engine view-tui view-native view-ai view-oracle view-bench tokio async-std smol; do
  check_absent view-surface "$dep"
done
for crate in view-native view-ai; do
  for dep in view view-engine view-tui view-oracle view-bench tokio async-std smol; do
    check_absent "$crate" "$dep"
  done
done
check_absent view-native view-ai
check_absent view-ai view-native
for crate in view-core view-engine view-surface view-native view-ai view-oracle view-bench view; do
  for dep in crossterm ratatui; do
    check_absent "$crate" "$dep"
  done
done
for crate in view-core view-surface view-native view-ai view-tui view-oracle view-bench view; do
  check_absent "$crate" rmpv
done
exit $fail
```

The `audit:` task is already wired into `Taskfile.yml` above, positioned
immediately before `ci:` (which it must precede — Taskfile has no forward
reference requirement, but definition-before-use keeps the file readable) and
referenced in `ci`'s cmds list after `lint`.

- [ ] **Step 3: Verify every target runs**

Run: `task fmt && task ci`
Expected: all steps pass (audit exits 0; test step compiles and runs zero tests successfully).

- [ ] **Step 4: Commit using the new gate**

```bash
task commit -- -m "chore: Taskfile workflow and dependency-direction audit

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: CI workflow mirroring `task ci`

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: Taskfile targets from Task 2.
- Produces: the CI gate all later phases extend (bench/oracle jobs attach here in P3).

- [ ] **Step 1: Write `.github/workflows/ci.yml`**

```yaml
name: ci

on:
  push:
    branches: [master]
  pull_request:

permissions:
  contents: read

jobs:
  ci:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - name: Check out repository
        uses: actions/checkout@v4

      - name: Set up Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache Rust build artifacts
        uses: Swatinem/rust-cache@v2

      - name: Set up task runner
        uses: arduino/setup-task@v2
        with:
          version: 3.x
          repo-token: ${{ secrets.GITHUB_TOKEN }}

      - name: Install neovim (linux)
        if: runner.os == 'Linux'
        run: |
          curl -fsSL -o nvim.appimage https://github.com/neovim/neovim/releases/download/stable/nvim-linux-x86_64.appimage
          chmod +x nvim.appimage
          ./nvim.appimage --appimage-extract >/dev/null
          echo "$PWD/squashfs-root/usr/bin" >> "$GITHUB_PATH"

      - name: Install neovim (macos)
        if: runner.os == 'macOS'
        run: brew install neovim

      - name: Install neovim (windows)
        if: runner.os == 'Windows'
        run: choco install neovim -y --no-progress

      - name: Add neovim to PATH (windows)
        if: runner.os == 'Windows'
        shell: bash
        run: echo "/c/tools/neovim/nvim-win64/bin" >> "$GITHUB_PATH"

      - name: Check neovim version
        run: nvim --version

      - name: Run CI suite
        run: task ci
```

- [ ] **Step 2: Verify the workflow is well-formed YAML and the local mirror passes**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))" && task ci`
Expected: no YAML error; `task ci` green. (Real CI proof needs a GHA run on a real runner — **resolved 2026-08-03**, when the workflow first ran on GitHub Actions.)

- [ ] **Step 3: Commit**

```bash
task commit -- -m "ci: fmt/clippy/test matrix on linux, macos, windows with nvim installed

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Repo hygiene — README and licenses

**Files:**
- Create: `README.md`
- Create: `LICENSE-MIT`, `LICENSE-APACHE`

**Interfaces:**
- Consumes: nothing.
- Produces: the public face; later phases extend README's feature list, never its framing.

- [ ] **Step 1: Write `README.md`**

README is user documentation, not a session memo. Exact starting content:

```markdown
# view

A terminal-first modal editor with a modern, coherent UI, powered by an
embedded, pinned Neovim. Your `init.lua`, plugins, LSP servers, and treesitter
setup run unmodified, because a real Neovim runs them.

**Status: pre-alpha. Not yet usable.**

## Why

- **Painless migration**: real Neovim is the engine; compatibility is total
  by construction, not reimplementation.
- **Fast where you feel it**: native Rust rendering, pickers, and UI that
  never jank on plugin Lua; measured against Neovim, budgets enforced in CI.
- **Modern out of the box**: one design system for messages, popups,
  command line, statusline, and notifications. No plugin patchwork required
  (yours still works).

## Requirements

- A terminal. Best experience on kitty, ghostty, or WezTerm; degrades
  gracefully elsewhere.

## License

MIT or Apache-2.0, at your option.
```

- [ ] **Step 2: Write the license files**

`LICENSE-MIT`: the standard MIT license text with the line `Copyright (c) 2026 TJ Smith`.
`LICENSE-APACHE`: the standard Apache-2.0 license text (unmodified from https://www.apache.org/licenses/LICENSE-2.0.txt).

- [ ] **Step 3: Commit**

```bash
task commit -- -m "docs: README and dual MIT/Apache-2.0 licensing

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Project-local .claude scaffolding (gitignored, still required)

**Files:**
- Create: `.claude/CLAUDE.md`
- Create: `.claude/known-bugs.md`
- Create: `.claude/dogfood-journal.md`
- Create: `.claude/settings.json` — hooks wiring: blocks `git push`/plain `git commit`, runs post-edit formatting/style checks on edited Rust files, PreCompact and Stop reminders.
- Create: `.claude/hooks/validate-commands.sh` — PreToolUse gate blocking `git push` and plain `git commit` in favor of `task commit`.
- Create: `.claude/hooks/post-edit-rs.sh` — PostToolUse check on edited `.rs` files for `rustfmt` compliance and session-narrative comment markers.
- Create: `.claude/rules/rust.md` — Rust conventions instantiated from the global template plus view-specific dependency-direction and error-typing notes.
- Create: `scripts/check-style.sh` — comment/doc style gate (session-narrative markers, `§` refs, assistant-citation comments, emdashes in user docs), wired into `task style` and `task ci`.

**Interfaces:**
- Consumes: nothing.
- Produces: session guardrails every future working session loads.

- [ ] **Step 1: Write `.claude/CLAUDE.md`**

```markdown
# view — session context

Spec of record: `.claude/specs/2026-07-17-view-design.md`. Plans:
`.claude/plans/INDEX.md`. On conflict, spec wins.

Hard rules (in addition to global rules):
- nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`.
- The paint loop never awaits RPC. The RPC reader thread never blocks.
- No unwrap/expect/panic in lib crates (workspace lints enforce; do not
  weaken them).
- Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal.
- Performance is a contract: any change touching key dispatch, grid apply,
  or paint states its latency consequence in the PR/commit description.
- Use `task` targets, never raw cargo, for build/fmt/lint/test/commit.
```

- [ ] **Step 2: Write `.claude/known-bugs.md`**

```markdown
# Known bugs / deferred items

Unchecked items must be drained before any "done" declaration or session
sign-off; deferral requires explicit user approval.

- [x] CI workflow proven on a real GHA runner — resolved 2026-08-03.
```

- [ ] **Step 3: Write `.claude/dogfood-journal.md`**

```markdown
# Dogfooding journal

One entry per phase exit: date, what was used for real work, what felt
fast/slow/wrong, unprompted reactions. Feeds spec §3.5 product metrics.
```

- [ ] **Step 4: Verify P0 is complete**

Run: `task ci && git status --short`
Expected: ci green; `git status` shows no unexpected tracked changes (`.claude/` is gitignored, so those files do not appear).

No commit for this task's files (gitignored); P0 ends here.
