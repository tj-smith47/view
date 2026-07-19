//! Shared modules for `view-harness`'s bin targets (`bin/oracle`, and any
//! future `view-bench`-driving bin): the corpus schema and loader live here
//! once, so more than one bin can read the same TOML shape without a
//! second copy of the parsing logic.
//!
//! `view-harness` is deliberately bin-only in the dependency-direction
//! sense enforced by `scripts/audit-deps.sh` -- this lib target exists for
//! the package's own internal module organization and is never a
//! workspace dependency edge itself; nothing else in the workspace may
//! depend on the `view-harness` package.

pub mod corpus;
