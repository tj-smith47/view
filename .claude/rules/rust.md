---
paths: ["**/*.rs"]
template-source: "rules/rust.md.tmpl"
---
# Rust conventions (edition 2021)

## Errors

- **No `unwrap()` or `expect()` in library code.** Use `?` with proper error types.
- **`thiserror` for module-level error enums** in `errors/`. Public functions return `Result<T, ModuleError>`.
- **`anyhow::Result` only at the CLI/`main.rs` boundary** — never inside library crates.
- **Panics are reserved for invariant violations** (impossible states), never for user-input errors.

## API design

- **`#[must_use]`** on functions whose return values must be checked (especially `Result` returners that build state).
- **`#[non_exhaustive]`** on public enums / structs that may grow.
- **Accept `&str` / `&[T]` / `impl AsRef<...>`**; minimize forced ownership at call sites.
- **Internal mutability via `Cell`/`RefCell`/`Mutex`** only when the API contract demands it; prefer immutable data + builders.

## Lints

- **`clippy::all` is the floor** — fix or silence with rationale comment.
- **`clippy::pedantic`** allowed selectively per crate.
- **`#![deny(unsafe_code)]`** at the crate root unless the crate genuinely needs `unsafe` (FFI, concurrency primitives).

## Tests

- **`#[cfg(test)] mod tests` colocated** with the code under test.
- **Integration tests in `tests/`** for cross-crate behavior.
- **`cargo nextest run`** if available — faster than `cargo test`.

## Tooling floor

- `cargo fmt --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.

## view specifics

- Workspace lints already deny unwrap/expect/panic/todo/unimplemented/dbg. Test modules may open with `#![allow(clippy::unwrap_used, clippy::expect_used)]` — test code only, never lib code.
- Typed errors per crate via `thiserror`; the bin crate `view` renders them for the user.
- Only `view-engine` speaks RPC; only `view-tui` touches the terminal; `view-core` is pure (no I/O, no tokio). `scripts/audit-deps.sh` enforces this — run `task audit`.
- **A `docs/*-wire-capture.md` fence that publishes a chunk verbatim carries the marker `verbatim `NAME_CHUNK`:` on the line above it.** `nvim_api.rs`'s `every_wire_capture_fence_matches_its_chunk_byte_for_byte` walks every capture doc, compares each marked fence to the const it names, and fails on a marker naming a const it does not carry — so a new doc joins the pin by writing the marker, and a fence published without one is a claim nothing checks.
- **A test that asserts a floor -- "nothing arrives for at least N" -- times it from the instant the work was dispatched, never from the start of the wait.** `recv_timeout(floor).is_err()` reads the wrong window: a loaded host can leave the test thread off-CPU for longer than the floor between the dispatch and the call, and the reply sent on time is already queued when the window finally opens, so the assertion reports a punctual reply as an instant one (gh-macos, `the_re_probe_waits_out_the_save_before_looking_again`). The shape that holds: take an `Instant` before dispatching, receive with the full host deadline, then assert `dispatched.elapsed() >= floor` -- a stall moves both ends together. Assert on the received value rather than discarding it, so the panic names which message arrived early; a floor asserted on `is_err()` alone cannot tell a degrade from a premature reply.
- **Every line of a multi-line string literal under `crates/view-engine/` wraps at 80 columns**, along with every `const ..._CHUNK: &str` in `nvim_api.rs`. rustfmt holds the line a literal opens on and nothing else about it, so nothing in the toolchain catches a line past that width -- and no check of what the literal holds: a Lua chunk broken with a trailing `\`, a fixture row and an assert message are one shape, and a rule that tried to tell them apart read two live Lua violations as prose. `scripts/check-style.sh` fails a longer line, and fails when the number of literals and chunks its walks reach stops matching what a plain grep for the same declarations counts -- no number is written down on either side, so a new literal costs nothing and a shape that drifts out of a walk still parts the two counts. Both run in `task ci`.
