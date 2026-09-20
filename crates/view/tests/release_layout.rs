//! The release archive's producer, asserted against the consumer that reads
//! it back.
//!
//! `scripts/package-bundle.sh` plants the layout and
//! [`view_engine::BundledEngine::resolve_from`] resolves it. Nothing else
//! makes the two agree, and a disagreement is silent: the released binary
//! starts, finds no engine beside itself, and falls back to whatever `nvim`
//! the machine happens to carry. So the producer is run for real here and
//! the consumer is asked what it sees.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use view_engine::process::BundledEngine;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root sits two levels above this crate")
}

fn package_bundle(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(view_test_support::bash_program())
        .arg("scripts/package-bundle.sh")
        .args(args)
        .current_dir(root)
        .output()
        .expect("bash runs the packaging script")
}

/// The engine prefix the pinned nvim tarball unpacks to, in miniature: the
/// binary under `bin/`, the runtime under `share/nvim/runtime`, the
/// treesitter parsers under `lib/nvim/parser`.
fn plant_engine_prefix(prefix: &Path) {
    let exe = std::env::consts::EXE_SUFFIX;
    std::fs::create_dir_all(prefix.join("bin")).expect("engine bin dir");
    std::fs::create_dir_all(prefix.join("share").join("nvim").join("runtime"))
        .expect("engine runtime dir");
    std::fs::create_dir_all(prefix.join("lib").join("nvim").join("parser"))
        .expect("engine parser dir");
    std::fs::write(prefix.join("bin").join(format!("nvim{exe}")), b"").expect("engine binary");
    std::fs::write(
        prefix
            .join("share")
            .join("nvim")
            .join("runtime")
            .join("filetype.lua"),
        b"",
    )
    .expect("a runtime file");
    std::fs::write(
        prefix.join("lib").join("nvim").join("parser").join("c.so"),
        b"",
    )
    .expect("a parser");
}

#[test]
fn the_archive_layout_matches_what_the_binary_resolves() {
    let root = repo_root();
    let scratch = view_test_support::ScratchDir::new("release-layout").expect("scratch dir");
    let prefix = scratch.join("engine-prefix");
    plant_engine_prefix(&prefix);
    let editor = scratch.join("view-under-test");
    std::fs::write(&editor, b"").expect("a stand-in for the built editor");

    let bundle = scratch.join("bundle");
    let out = Command::new(view_test_support::bash_program())
        .arg("scripts/package-bundle.sh")
        .args([
            "stage",
            current_target(),
            &view_test_support::bash_arg(&editor),
        ])
        .arg(view_test_support::bash_arg(&bundle))
        .env("VIEW_ENGINE_PREFIX", view_test_support::bash_arg(&prefix))
        .current_dir(&root)
        .output()
        .expect("bash runs the packaging script");
    assert!(
        out.status.success(),
        "the packaging script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let exe = bundle
        .join("bin")
        .join(format!("view{}", std::env::consts::EXE_SUFFIX));
    // Asserted before resolving, because `resolve_from` never stats the
    // executable it is handed: it only walks up from that path. Without
    // this, a packaging that put the editor somewhere other than `bin/`
    // would still resolve against the path this test constructed, and the
    // producer half of the layout would go unchecked.
    assert!(
        exe.is_file(),
        "scripts/package-bundle.sh planted no editor at bin/, so the archive \
         has nothing at the path its engine is resolved relative to; \
         planted: {:?}",
        walk(&bundle)
    );
    let resolved = BundledEngine::resolve_from(&exe);
    assert!(
        resolved.is_some(),
        "the archive scripts/package-bundle.sh plants is not the one the \
         editor resolves beside itself, so a released binary spawns whatever \
         nvim the machine happens to carry; planted: {:?}",
        walk(&bundle)
    );

    // Lifted out of the engine's own prefix, nvim derives no parser
    // directory from its argv[0]; the packaging moves the parsers onto the
    // runtime directory so runtimepath finds them instead. A layout that
    // resolves but highlights nothing passes the check above on its own.
    let engine = resolved.expect("the assertion above holds");
    assert!(
        engine.runtime.join("parser").join("c.so").is_file(),
        "the bundled engine's treesitter parsers are not on the runtime \
         directory, so every highlight silently degrades"
    );
}

/// Every target the release builds for has to have an engine to bundle. The
/// two lists live apart by necessity (a target triple is a Rust concept, an
/// engine asset name is neovim's), so the only thing keeping them in step is
/// asking the packaging for each one.
#[test]
fn every_platform_artifact_carries_a_bundled_engine() {
    let root = repo_root();
    let targets = configured_targets(&root);
    assert!(
        targets.len() >= 3,
        "the release config named {} targets, which is fewer than the three \
         platforms the editor supports",
        targets.len()
    );
    for target in &targets {
        let out = package_bundle(&root, &["asset", target]);
        assert!(
            out.status.success(),
            "no pinned engine asset for release target {target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "the engine asset for release target {target} is empty"
        );
    }
}

/// The release targets, read from the release config rather than repeated:
/// the entries under `defaults.targets`, which is the one list a Rust
/// workspace does not carry on its own.
fn configured_targets(root: &Path) -> Vec<String> {
    let config = std::fs::read_to_string(root.join(".anodizer.yaml"))
        .expect("the release config sits at the workspace root");
    let mut targets = Vec::new();
    let mut inside = false;
    for line in config.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if line.trim_end() == "  targets:" {
            inside = true;
            continue;
        }
        if inside {
            match line.strip_prefix("    - ") {
                Some(target) => targets.push(target.trim().to_string()),
                None => inside = false,
            }
        }
    }
    targets
}

/// The triple this test host would itself be released for, so the staging it
/// drives takes the same branch a real release takes on this platform.
fn current_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => "x86_64-pc-windows-msvc",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", _) => "aarch64-apple-darwin",
        (_, "aarch64") => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

/// The planted tree, for a failure message that says what was there instead
/// of only that nothing resolved.
fn walk(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            if let Ok(rel) = path.strip_prefix(root) {
                found.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}
