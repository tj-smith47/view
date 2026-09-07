//! Every production site that names a geometry to the engine, pinned per
//! file with the ground the pair it spends came from.
//!
//! An attach is refused outright below `view_core::model::ENGINE_MIN_SIZE`,
//! and a spawn seeded at a size the attach does not repeat relayouts every
//! window on screen, so a `(width, height)` that reaches either call has to
//! have come from `view_core::model::grid_target_for` -- and the terminal's
//! own reading, which is what a caller has in hand, is exactly the pair
//! that must not. Nothing at a call site says which one it holds:
//! `startup.rs` legitimately spends a `width, height` bound off a channel,
//! so an identifier walk either accepts the raw reading everywhere or
//! rejects the one correct site.
//!
//! What holds instead is the shape `check_tied_spawns` uses for spawns in
//! `scripts/check-style.sh`: the whole population pinned per file with a
//! grounds row each, so a new site fails this test by name until whoever
//! adds it writes down where its geometry came from. A row that says the
//! call carries no geometry at all is a legitimate row -- `release(`
//! reaches a lock and a scan gate as well as the attach guard.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// The call spellings that carry a `(width, height)` to the engine.
///
/// Spelled as substrings rather than as parsed calls: an attach is reached
/// through a handle, a trait object, a `Box`, an enum variant and the wire
/// method name itself, and every one of those is a site somebody could give
/// its own size to -- a `handle.request("nvim_ui_attach", ...)` spelled by
/// hand most of all, which is why the literal counts here too.
const GEOMETRY_CALLS: &[&str] = &["ui_attach", "UiAttach", "try_resize("];

/// Whether `line` calls or declares a bare `release`, the attach guard's
/// own hand-off of the geometry its spawn was seeded with.
///
/// Bounded on the left, unlike [`GEOMETRY_CALLS`]: `release` is an ordinary
/// English word that ends other names in this tree
/// (`note_hidden_release`, `steps_after_release`), and those carry no
/// geometry and never will.
fn names_a_release(line: &str) -> bool {
    line.match_indices("release(").any(|(at, _)| {
        at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_'
    })
}

/// `(path under crates/, production lines matching, where the geometry came
/// from)`.
const GEOMETRY_SITES: &[(&str, usize, &str)] = &[
    (
        "view-core/src/model.rs",
        1,
        "the one RpcCall::UiAttach production builds, from Model::grid_target -- grid_target_for over the model's own terminal size",
    ),
    (
        "view-core/src/msg.rs",
        1,
        "the UiAttach variant's declaration; it carries the pair its builder put in it",
    ),
    (
        "view-core/src/update/ai_fs.rs",
        4,
        "an AI filesystem lock release and its own helper, no geometry anywhere",
    ),
    (
        "view-engine/src/nvim_api.rs",
        6,
        "the handle's own attach and resize entry points plus the nvim_ui_attach method name they send; each spends what its caller hands it",
    ),
    (
        "view-oracle/src/hang.rs",
        3,
        "the adversarial harness attaching and resizing its own engine at the fixture size it opened the session with",
    ),
    (
        "view-oracle/src/lib.rs",
        2,
        "the oracle driver attaching at the size its caller opened the session with, and the TryResize effect it forwards",
    ),
    (
        "view-oracle/src/reference.rs",
        2,
        "the second applier attaching and resizing at the size the session under comparison is held at",
    ),
    (
        "view-oracle/src/speculate.rs",
        2,
        "the speculative-echo battery attaching and resizing at its own fixture geometry",
    ),
    (
        "view-test-support/src/lib.rs",
        1,
        "a scan gate's release, no geometry",
    ),
    (
        "view/src/engine_ops.rs",
        14,
        "the EngineOps attach and resize surface: one declaration and the forwarding impls behind it, each spending the pair it was handed",
    ),
    (
        "view/src/main.rs",
        1,
        "the attach guard's release, spending spawn_size -- what grid_target_for answered the terminal reading with, and what the spawn's own geometry --cmd already told the child",
    ),
    (
        "view/src/runtime/executor.rs",
        3,
        "the executor spending the pair the UiAttach and TryResize effects carry, which update() built from the model",
    ),
    (
        "view/src/startup.rs",
        5,
        "the attach guard's release and the attaches it feeds, all spending the pair main released rather than a reading of their own",
    ),
];

#[test]
fn every_production_geometry_site_is_pinned_with_the_ground_its_pair_came_from() {
    let found = production_geometry_sites();
    let mut pinned: Vec<(String, usize)> = GEOMETRY_SITES
        .iter()
        .map(|(path, sites, _)| ((*path).to_string(), *sites))
        .collect();
    pinned.sort();
    assert_eq!(
        found, pinned,
        "a production site that names a geometry to the engine sits outside \
         the pinned set.\nAn attach is refused below \
         view_core::model::ENGINE_MIN_SIZE and a spawn seeded past its \
         attach relayouts every window, so the pair a new site spends comes \
         from view_core::model::grid_target_for -- never from the \
         terminal's own reading. Add a row to GEOMETRY_SITES saying where \
         this one's geometry came from, or say there that it carries none."
    );
}

/// Every production file under `crates/*/src` that names one of
/// [`GEOMETRY_CALLS`], with how many of its production lines do.
fn production_geometry_sites() -> Vec<(String, usize)> {
    let crates = crates_root();
    let mut members: Vec<PathBuf> = std::fs::read_dir(&crates)
        .expect("the workspace's crates directory must be readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    members.sort();
    let mut walked = 0_usize;
    let mut found = Vec::new();
    for member in members {
        for source in rust_sources(&member.join("src")) {
            walked += 1;
            // `src/thing/tests.rs` is the other half of a `#[cfg(test)] mod
            // tests;` and carries no attribute of its own, so nothing in
            // the file itself says it is test code
            if is_test_module_file(&source) {
                continue;
            }
            let text = std::fs::read_to_string(&source).expect("a source file must be readable");
            let sites = production_code(&text)
                .lines()
                .filter(|line| {
                    GEOMETRY_CALLS.iter().any(|call| line.contains(call)) || names_a_release(line)
                })
                .count();
            if sites > 0 {
                let name = source
                    .strip_prefix(&crates)
                    .unwrap_or(&source)
                    .to_string_lossy()
                    .replace('\\', "/");
                found.push((name, sites));
            }
        }
    }
    // fail closed: a walk that looked in the wrong place finds nothing and
    // would otherwise pass by finding nothing
    assert!(
        walked > 100,
        "the walk read only {walked} production sources, so it is not \
         looking where the workspace keeps them"
    );
    found.sort();
    found
}

/// Whether a `src` file is a test module outright rather than a source file
/// with one at the bottom.
fn is_test_module_file(source: &Path) -> bool {
    source.file_stem().is_some_and(|stem| stem == "tests")
        || source
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == "tests")
}

/// `text` with its comments gone and its `#[cfg(test)]` items with them, so
/// a doc comment naming an attach and a test double implementing one are
/// invisible here.
///
/// A gated item is skipped by its own indentation rather than by cutting
/// the file at its first `#[cfg(test)]`: `view-oracle/src/lib.rs` gates a
/// `mod` declaration two thirds of the way up its production code, and a
/// walk that stopped there would see none of the attaches below it.
fn production_code(text: &str) -> String {
    enum State {
        Code,
        /// A gated attribute has been read and its item has not started.
        Gated(String),
        /// Inside a gated item, which ends at this exact line.
        Skipping(String),
    }

    let mut state = State::Code;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        let trimmed = code.trim();
        match &state {
            State::Skipping(close) => {
                if code == close {
                    state = State::Code;
                }
            }
            State::Gated(indent) => {
                if trimmed.is_empty() {
                } else if code.contains('{') {
                    state = State::Skipping(format!("{indent}}}"));
                } else if trimmed.ends_with(';') {
                    state = State::Code;
                }
            }
            State::Code => {
                if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(all(test") {
                    let indent = code[..code.len() - code.trim_start().len()].to_string();
                    state = State::Gated(indent);
                } else {
                    out.push_str(code);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Every `.rs` file at or under `dir`, sorted, or nothing where `dir` does
/// not exist.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut nested: Vec<PathBuf> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            nested.push(path);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            paths.push(path);
        }
    }
    nested.sort();
    for dir in nested {
        paths.extend(rust_sources(&dir));
    }
    paths.sort();
    paths
}

/// The workspace's `crates` directory.
fn crates_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate sits inside the workspace's crates directory")
        .to_owned()
}
