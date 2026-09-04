//! Every measurement fixture answers for every native feature, and no
//! fixture anywhere in the tree changes the UI protocol its rows are then
//! measured over.
//!
//! The comparison harnesses drive a fixture against view and nvim on the
//! same work, so each fixture ships a `view.toml` that switches the native
//! takeovers off (see the file's own comment for why). A feature added to
//! the registry later is enabled by default and would silently re-open that
//! divergence in whichever fixture forgot to name it, which is what this
//! reads the files to prevent -- the failure it replaces is a compat
//! scenario or a bench arm reporting a plugin fault for a takeover nobody
//! meant to run there.
//!
//! Two of those switches are not renderer switches at all: `palette` and
//! `notifications` decide the `ext_*` set the session attaches with, so
//! naming them costs the view arm nvim's own grid work on top of its own
//! and makes every row describe a protocol no shipped default runs. That is
//! why the first walk below exempts exactly the switches that reach the
//! attach and the second one denies them to every fixture -- the exemption
//! is derived from `ext_surfaces` rather than spelled, so a switch that
//! gains a surface moves between the two walks on its own.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use view_core::native::{ext, registry};
use view_native::config::{ext_surfaces, Overrides, ResolvedConfig, ViewConfig};

/// The fixtures allowed to attach less than every surface, as (the fixture
/// directory's name on disk, the grounds that make rows taken against a
/// non-default attach still say what they claim).
///
/// Empty, and a fixture belongs here only when its own subject *is* the
/// detached protocol -- a scenario state says that with its own `native`
/// override, which no fixture file has to carry for it.
const DETACHED_PROTOCOLS: &[(&str, &str)] = &[];

/// The workspace root, from this crate's manifest.
fn workspace_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root
}

/// The `compat/fixtures` directory: the fixtures both comparison harnesses
/// drive.
fn comparison_fixtures_root() -> PathBuf {
    workspace_root().join("compat").join("fixtures")
}

/// Every fixture directory under `root`, as (directory name, its
/// `view/view.toml`, that file resolved).
///
/// The whole document rather than its `[native]` table alone, and against
/// an empty environment: the attach set answers to `[engine]` too, so a
/// fixture that took the shipped attach mode off would otherwise be
/// invisible to the walks below.
///
/// `view_toml_optional` is for the acceptance roots, where a fixture
/// deliberately ships nvim alone (`habamax`). Under `compat/fixtures/` a
/// missing file is the failure it has always been: a comparison fixture
/// with no `view.toml` runs the shipping defaults it exists to switch off,
/// and skipping it would let that happen quietly.
fn fixture_configs(
    root: &Path,
    view_toml_optional: bool,
) -> Vec<(String, PathBuf, ResolvedConfig)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root)
        .unwrap_or_else(|err| panic!("{} must be readable: {err}", root.display()))
    {
        let dir = entry.expect("a fixture directory entry").path();
        if !dir.is_dir() {
            continue;
        }
        let path = dir.join("view").join("view.toml");
        if view_toml_optional && !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is unreadable: {err}", path.display()));
        let file = ViewConfig::from_toml_str(&text)
            .unwrap_or_else(|err| panic!("{} does not parse: {err}", path.display()));
        let cfg = view_native::config::resolve_with(&file, &Overrides::default(), &|_| None);
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        found.push((name, path, cfg));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Whether moving `feature` off its registry default changes which surfaces
/// a session externalizes, asked of `ext_surfaces` itself so no list here
/// can drift from the one the product filters.
///
/// Off its own default rather than off outright: a feature that ships off
/// still decides the attach, and asking it the question the other way round
/// would read it as a renderer every fixture then owes a line for.
fn decides_the_attach(feature: &registry::FeatureDesc) -> bool {
    let (id, flipped) = (feature.id, !feature.default_on);
    let file = ViewConfig::from_toml_str(&format!("[native]\n{id} = {flipped}\n"))
        .unwrap_or_else(|err| panic!("[native] {id} = {flipped} must parse: {err}"));
    let cfg = view_native::config::resolve_with(&file, &Overrides::default(), &|_| None);
    ext_surfaces(&cfg) != ext::shipped_multigrid()
}

#[test]
fn every_comparison_fixture_switches_off_every_feature_that_only_renders() {
    let fixtures = fixture_configs(&comparison_fixtures_root(), false);
    for (_, path, cfg) in &fixtures {
        for feature in registry::features() {
            if decides_the_attach(feature) {
                continue;
            }
            assert!(
                !cfg.tables.native.enabled(feature.id),
                "{} leaves native.{} on: a comparison run against this fixture would measure \
                 view superseding a surface nvim still draws",
                path.display(),
                feature.id
            );
        }
    }
    assert!(
        fixtures.len() >= 2,
        "found {} fixtures with a view.toml; the minimal and heavy fixtures both drive \
         comparison runs and both must answer",
        fixtures.len()
    );
}

#[test]
fn no_fixture_hands_an_ext_surface_back_to_the_engine() {
    let roots = [
        (comparison_fixtures_root(), false),
        (
            workspace_root()
                .join("scripts")
                .join("acceptance")
                .join("fixtures"),
            true,
        ),
    ];
    let mut checked = 0;
    for (root, view_toml_optional) in &roots {
        for (name, path, cfg) in fixture_configs(root, *view_toml_optional) {
            if let Some((_, grounds)) = DETACHED_PROTOCOLS
                .iter()
                .find(|(fixture, _)| *fixture == name)
            {
                assert!(
                    !grounds.is_empty(),
                    "{} is allowlisted with no grounds",
                    path.display()
                );
                continue;
            }
            let attached = ext_surfaces(&cfg);
            assert_eq!(
                attached,
                ext::shipped_multigrid(),
                "{} attaches {attached:?} rather than the shipped set, so a session running it \
                 hands the rest back to nvim: nvim paints them into the grid, view applies that \
                 damage on top of its own rendering, and every row taken here describes a \
                 protocol no shipped default runs. Take the switch back out of the fixture, or \
                 name the fixture in DETACHED_PROTOCOLS with the grounds that make its rows \
                 readable anyway",
                path.display()
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 3,
        "found {checked} fixtures with a view.toml; both comparison fixtures and the acceptance \
         sweep's own must answer"
    );
}
