//! Every measurement fixture answers for every native feature.
//!
//! The comparison harnesses drive a fixture against view and nvim on the
//! same work, so each fixture ships a `view.toml` that switches the native
//! takeovers off (see the file's own comment for why). A feature added to
//! the registry later is enabled by default and would silently re-open that
//! divergence in whichever fixture forgot to name it, which is what this
//! reads the files to prevent -- the failure it replaces is a compat
//! scenario or a bench arm reporting a plugin fault for a takeover nobody
//! meant to run there.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use view_core::native::registry;
use view_native::config::NativeConfig;

/// The `compat/fixtures` directory, from this crate's manifest.
fn fixtures_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("compat").join("fixtures")
}

#[test]
fn every_fixture_switches_every_native_feature_off() {
    let root = fixtures_root();
    let mut checked = 0;
    for entry in std::fs::read_dir(&root).expect("the fixtures directory must be readable") {
        let dir = entry.expect("a fixture directory entry").path();
        if !dir.is_dir() {
            continue;
        }
        let path = dir.join("view").join("view.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is unreadable: {err}", path.display()));
        let cfg = NativeConfig::from_toml_str(&text)
            .unwrap_or_else(|err| panic!("{} does not parse: {err}", path.display()));
        for feature in registry::features() {
            assert!(
                !cfg.enabled(feature.id),
                "{} leaves native.{} on: a comparison run against this fixture would measure \
                 view superseding a surface nvim still draws",
                path.display(),
                feature.id
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 2,
        "found {checked} fixtures with a view.toml; the minimal and heavy fixtures both \
         drive comparison runs and both must answer"
    );
}
