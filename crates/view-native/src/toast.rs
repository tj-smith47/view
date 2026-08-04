//! The first-run notice: the one time view tells a user it has taken a
//! surface over, and the exact line that gives it back.
//!
//! Once per feature per config path, not once per session. A message that
//! repeats every launch is noise a user learns to skip, and the reversal
//! line it carries is exactly the part that must still be read the day they
//! want it back. Keying on the config path as well as the feature means a
//! second config -- a bare `--clean` session, a machine-specific file --
//! introduces itself on its own terms rather than inheriting the silence
//! another config earned.
//!
//! The record is state, not config: deleting it costs a user nothing but a
//! repeated notice, which is why it lives beside the theme cache rather
//! than anywhere a user keeps things they wrote.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::supersede::Supersession;

/// The record format this build writes. Bumped only when an older build
/// would misread a newer file; a newer file is left untouched rather than
/// clobbered, so downgrading a build costs at most a repeated notice.
const SCHEMA_VERSION: u32 = 1;

/// Why a first-run notice could not be recorded.
///
/// Recording failure is reported rather than swallowed, but the caller is
/// free to carry on: an unrecorded notice repeats next launch, which is a
/// far smaller harm than refusing to start an editor over a cache file.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ToastError {
    /// The record's directory could not be created.
    #[error("could not create the state directory {path}: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
    /// The record exists but could not be read.
    #[error("could not read the first-run record {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    /// The record could not be written back.
    #[error("could not write the first-run record {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    /// The record could not be rendered as TOML.
    #[error("could not serialize the first-run record: {source}")]
    Serialize {
        #[from]
        source: toml::ser::Error,
    },
}

/// The on-disk record: which features have already introduced themselves,
/// under which config path.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Record {
    /// Written by every build, read to decide whether this build
    /// understands the file at all.
    schema_version: u32,
    /// Config path to the feature ids already announced under it. A
    /// `BTreeMap` of sorted `Vec`s rather than hash-ordered containers so
    /// the file is stable across writes: a record that reshuffles itself
    /// every launch is unreadable as a diff and unusable as evidence.
    #[serde(default)]
    announced: BTreeMap<String, Vec<String>>,
}

/// The notices to show for `plan` under `config_path`, recording them in
/// `record` so they are shown once and never again.
///
/// Returns one string per feature announcing itself for the first time, in
/// plan order, and an empty vec when every feature in the plan has already
/// been announced under this config. Writes the record before returning:
/// the alternative -- record after the notice is displayed -- needs a
/// second call the display path can forget to make, and forgetting it
/// repeats the notice forever.
///
/// `config_path` is `None` for a session running without a config file at
/// all, which is recorded as its own key rather than merged into whichever
/// config ran last.
///
/// A record this build cannot parse is treated as absent and rewritten,
/// re-announcing at most once; a record from a newer schema is left exactly
/// as it is and nothing is announced, because a downgraded build cannot
/// know what that file already promised the user.
pub fn first_run(
    plan: &[Supersession],
    config_path: Option<&Path>,
    record: &Path,
) -> Result<Vec<String>, ToastError> {
    if plan.is_empty() {
        return Ok(Vec::new());
    }

    let mut current = read_record(record)?;
    if current.schema_version > SCHEMA_VERSION {
        return Ok(Vec::new());
    }
    current.schema_version = SCHEMA_VERSION;

    let key = config_path.map_or_else(String::new, |p| p.display().to_string());
    let announced = current.announced.entry(key).or_default();

    let mut notices = Vec::new();
    for entry in plan {
        if announced.iter().any(|id| id == entry.feature) {
            continue;
        }
        announced.push(entry.feature.to_string());
        notices.push(entry.notice());
    }
    if notices.is_empty() {
        return Ok(notices);
    }
    announced.sort();

    write_record(record, &current)?;
    Ok(notices)
}

/// The record at `path`, or a fresh one when it is absent or unreadable as
/// this build's format.
///
/// A malformed record answers as empty rather than as an error: it is a
/// cache of what a user has already been told, and the worst a rebuild
/// costs is one repeated notice, whereas failing here would let a truncated
/// file (a machine that lost power mid-write) block a feature's only
/// explanation of itself forever.
fn read_record(path: &Path) -> Result<Record, ToastError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Record::default()),
        Err(source) => {
            return Err(ToastError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    Ok(toml::from_str(&raw).unwrap_or_default())
}

/// Writes `record` to `path`, creating the state directory if this is the
/// first thing view has ever stored there.
fn write_record(path: &Path, record: &Record) -> Result<(), ToastError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ToastError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let rendered = toml::to_string(record)?;
    std::fs::write(path, rendered).map_err(|source| ToastError::Write {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use crate::config::NativeConfig;
    use crate::supersede::plan;
    use std::path::PathBuf;
    use view_core::native::registry;

    /// A scratch directory for one test's record file, named for the test
    /// so two of them never share a path.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("view-toast-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory must be creatable");
        dir
    }

    fn statusline_plan() -> Vec<Supersession> {
        plan(&NativeConfig::all_enabled(), registry::features())
    }

    #[test]
    fn the_first_run_announces_every_superseded_feature_with_its_off_switch() {
        let dir = scratch("first");
        let record = dir.join("native-first-run.toml");
        let plan = statusline_plan();

        let notices = first_run(&plan, Some(Path::new("/cfg/view.toml")), &record)
            .expect("a writable record must not fail");

        assert_eq!(
            notices.len(),
            plan.len(),
            "every superseded feature introduces itself once, got {notices:?}"
        );
        let statusline = notices
            .iter()
            .find(|n| n.contains("statusline"))
            .expect("the statusline must introduce itself");
        assert!(
            statusline.contains("native.statusline = false"),
            "the notice must name the off switch verbatim, got {statusline:?}"
        );
        assert!(
            statusline.contains("lualine"),
            "the notice must name what it superseded, got {statusline:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_run_under_the_same_config_announces_nothing() {
        let dir = scratch("second");
        let record = dir.join("native-first-run.toml");
        let plan = statusline_plan();
        let cfg = Some(Path::new("/cfg/view.toml"));

        let first = first_run(&plan, cfg, &record).expect("the first run must record");
        assert!(!first.is_empty(), "the first run must announce something");
        let second = first_run(&plan, cfg, &record).expect("the second run must read the record");

        assert!(
            second.is_empty(),
            "an announced feature must stay quiet, got {second:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_different_config_path_introduces_itself_on_its_own_terms() {
        let dir = scratch("per-config");
        let record = dir.join("native-first-run.toml");
        let plan = statusline_plan();

        let first = first_run(&plan, Some(Path::new("/cfg/a.toml")), &record)
            .expect("the first config must record");
        let other = first_run(&plan, Some(Path::new("/cfg/b.toml")), &record)
            .expect("the second config must record");
        let none = first_run(&plan, None, &record).expect("a config-less session must record");

        assert!(
            !first.is_empty(),
            "the first config must announce something"
        );
        assert_eq!(first, other, "each config path announces the same features");
        assert_eq!(first, none, "a config-less session announces them too");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newly_enabled_feature_announces_itself_beside_already_announced_ones() {
        let dir = scratch("incremental");
        let record = dir.join("native-first-run.toml");
        let cfg = Some(Path::new("/cfg/view.toml"));
        let full = statusline_plan();
        let partial: Vec<Supersession> = full.iter().take(1).cloned().collect();

        let first = first_run(&partial, cfg, &record).expect("the partial plan must record");
        assert_eq!(first.len(), partial.len());
        let rest = first_run(&full, cfg, &record).expect("the full plan must record");

        assert_eq!(
            rest.len(),
            full.len() - partial.len(),
            "only the features not yet announced may speak, got {rest:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unparseable_record_is_rebuilt_rather_than_left_broken() {
        let dir = scratch("corrupt");
        let record = dir.join("native-first-run.toml");
        std::fs::write(&record, "this is not toml {{{").expect("the record must be writable");

        let notices =
            first_run(&statusline_plan(), None, &record).expect("a corrupt record must not fail");

        assert!(
            !notices.is_empty(),
            "a corrupt record cannot prove anything was announced"
        );
        let rebuilt = std::fs::read_to_string(&record).expect("the record must be readable");
        assert!(
            rebuilt.contains("schema_version"),
            "the record must be rebuilt in this build's format, got {rebuilt:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_record_from_a_newer_build_is_left_exactly_as_it_is() {
        let dir = scratch("newer");
        let record = dir.join("native-first-run.toml");
        let newer = format!("schema_version = {}\n", SCHEMA_VERSION + 1);
        std::fs::write(&record, &newer).expect("the record must be writable");

        let notices = first_run(&statusline_plan(), None, &record)
            .expect("a newer record must not fail the run");

        assert!(
            notices.is_empty(),
            "a newer record's promises are unknown, so nothing may be announced, got {notices:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("the record must be readable"),
            newer,
            "a newer build's record must not be clobbered"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_is_created_rather_than_failing_the_run() {
        let dir = scratch("nested");
        let record = dir.join("deeper").join("native-first-run.toml");

        let notices =
            first_run(&statusline_plan(), None, &record).expect("the directory must be created");

        assert!(!notices.is_empty());
        assert!(record.exists(), "the record must exist after a first run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_plan_writes_nothing_at_all() {
        let dir = scratch("empty");
        let record = dir.join("native-first-run.toml");

        let notices = first_run(&[], None, &record).expect("an empty plan must not fail");

        assert!(notices.is_empty());
        assert!(
            !record.exists(),
            "a run with nothing to say must not create a record"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
