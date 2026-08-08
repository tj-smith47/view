//! The file tree sidebar's off-loop workers: an `ignore`-walked filesystem
//! scan ([`fs::scan`]) and a `git status --porcelain=v2` refresh
//! ([`git::status`]). Both are plain, synchronous, blocking functions with
//! no internal thread-spawning of their own -- the `view` bin crate's
//! `Executor::run` is the one place that spawns the thread each runs on
//! (mirroring `Effect::PickerPreviewFallback`'s worker, not the picker
//! matcher's own persistent worker thread), so a call to either function is
//! trivial to unit test without touching threads at all.

pub mod fs;
pub mod git;
