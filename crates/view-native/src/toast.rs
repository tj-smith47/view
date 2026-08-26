//! The first-run notice: the one time view tells a user it has taken a
//! surface over, and the exact line that gives it back.
//!
//! Once per surface per config path, not once per session. A message that
//! repeats every launch is noise a user learns to skip, and the reversal
//! line it carries is exactly the part that must still be read the day they
//! want it back. Keying on the config path as well as the surface means a
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

use crate::report::Handover;

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
        /// The directory that could not be created, as it is displayed.
        path: String,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The record exists but could not be read.
    #[error("could not read the first-run record {path}: {source}")]
    Read {
        /// The record path that failed to read, as it is displayed.
        path: String,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The record could not be written back.
    #[error("could not write the first-run record {path}: {source}")]
    Write {
        /// The record path that failed to write, as it is displayed.
        path: String,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The record could not be rendered as TOML.
    #[error("could not serialize the first-run record: {source}")]
    Serialize {
        /// The underlying TOML serialization error.
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
    /// Config path (encoded by [`config_key`]) to the record keys already
    /// announced under it (see
    /// [`Handover::record_key`]). A `BTreeMap` of sorted `Vec`s rather than
    /// hash-ordered containers so the file is stable across writes: a record
    /// that reshuffles itself every launch is unreadable as a diff and
    /// unusable as evidence.
    #[serde(default)]
    announced: BTreeMap<String, Vec<String>>,
}

/// The notices to show for `report` under `config_path`, recording them in
/// `record` so they are shown once and never again.
///
/// Returns one string per surface announcing itself for the first time, in
/// report order, and an empty vec when every surface in the report has
/// already been announced under this config. Writes the record before
/// returning: the alternative -- record after the notice is displayed --
/// needs a second call the display path can forget to make, and forgetting
/// it repeats the notice forever.
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
    report: &[Handover],
    config_path: Option<&Path>,
    record: &Path,
) -> Result<Vec<String>, ToastError> {
    if report.is_empty() {
        return Ok(Vec::new());
    }

    let mut current = read_record(record)?;
    if current.schema_version > SCHEMA_VERSION {
        return Ok(Vec::new());
    }
    current.schema_version = SCHEMA_VERSION;

    let key = config_path.map_or_else(String::new, config_key);
    let announced = current.announced.entry(key).or_default();

    let mut notices = Vec::new();
    for entry in report {
        let key = entry.record_key();
        if announced.contains(&key) {
            continue;
        }
        announced.push(key);
        notices.push(entry.notice());
    }
    if notices.is_empty() {
        return Ok(notices);
    }
    announced.sort();

    write_record(record, &current)?;
    Ok(notices)
}

/// The record key for a config path: its own bytes, with `%` and every byte
/// that is not part of valid UTF-8 written as `%XX`.
///
/// `Path::display` is lossy -- every byte it cannot decode becomes U+FFFD --
/// so two different configs under two different undecodable paths collapse
/// onto one key, and the second one silently inherits the silence the first
/// one earned. A user whose paths are all UTF-8 never meets that, but the
/// map has to be injective for the ones whose paths are not. Escaping rather
/// than hex or base64 over the whole path keeps an ordinary key readable in
/// the file, which is the reason the record is TOML at all.
///
/// `%` is escaped too, and has to be: without it a path spelling the literal
/// text `%C3` and a path holding the undecodable byte `0xC3` produce the
/// same key. The cost is that a config path containing a `%` re-announces
/// once, against records written before this encoding existed.
fn config_key(path: &Path) -> String {
    let mut rest = path.as_os_str().as_encoded_bytes();
    let mut out = String::with_capacity(rest.len());
    while !rest.is_empty() {
        let (decoded, undecodable) = match std::str::from_utf8(rest) {
            Ok(text) => (text, 0),
            Err(e) => (
                // everything below `valid_up_to` is valid UTF-8 by
                // construction, so the fallback arm is unreachable
                std::str::from_utf8(&rest[..e.valid_up_to()]).unwrap_or(""),
                // a `None` error length means the input ran out mid-sequence,
                // so every byte from here on is unrepresentable
                e.error_len().unwrap_or(rest.len() - e.valid_up_to()),
            ),
        };
        for ch in decoded.chars() {
            if ch == '%' {
                out.push_str("%25");
            } else {
                out.push(ch);
            }
        }
        rest = &rest[decoded.len()..];
        for byte in &rest[..undecodable] {
            out.push_str(&format!("%{byte:02X}"));
        }
        rest = &rest[undecodable..];
    }
    out
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
    use crate::report::report;
    use crate::supersede::plan;
    #[cfg(unix)]
    use std::path::PathBuf;
    use view_core::native::mappings::MappingClaim;
    use view_core::native::registry;
    use view_test_support::ScratchDir;

    /// A scratch directory for one test's record file, named for the test
    /// so two of them never share a path.
    fn scratch(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("toast-{name}")).expect("the scratch directory must be creatable")
    }

    fn claim(feature: &str, lhs: &str) -> MappingClaim {
        MappingClaim {
            feature: feature.to_string(),
            lhs: lhs.to_string(),
            had_user_mapping: true,
        }
    }

    /// Both surface kinds in one report, since the toast has to introduce
    /// held options and taken keys through the same pass.
    fn handovers() -> Vec<Handover> {
        report(
            &plan(&NativeConfig::all_enabled(), registry::features()),
            &[claim("picker", "<leader>ff")],
            registry::features(),
        )
    }

    #[test]
    fn the_first_run_announces_every_handed_over_surface_with_its_off_switch() {
        let dir = scratch("first");
        let record = dir.join("native-first-run.toml");
        let report = handovers();

        let notices = first_run(&report, Some(Path::new("/cfg/view.toml")), &record)
            .expect("a writable record must not fail");

        assert_eq!(
            notices.len(),
            report.len(),
            "every handed-over surface introduces itself once, got {notices:?}"
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
        let key = notices
            .iter()
            .find(|n| n.contains("<leader>ff"))
            .expect("a taken key must introduce itself through the same pass");
        assert!(
            key.contains("native.picker = false"),
            "the notice must name the off switch verbatim, got {key:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_key_taken_later_speaks_even_though_its_feature_already_announced() {
        let dir = scratch("later-key");
        let record = dir.join("native-first-run.toml");
        let cfg = Some(Path::new("/cfg/view.toml"));
        let features = registry::features();
        let options = report(&plan(&NativeConfig::all_enabled(), features), &[], features);
        let with_key = report(
            &plan(&NativeConfig::all_enabled(), features),
            &[claim("statusline", "<leader>ss")],
            features,
        );

        let first = first_run(&options, cfg, &record).expect("the options must record");
        assert!(!first.is_empty(), "the first run must announce something");
        let second = first_run(&with_key, cfg, &record).expect("the key must record");

        assert_eq!(
            second.len(),
            1,
            "a key taken from the user is its own news, whatever its feature \
             already said about an option, got {second:?}"
        );
        assert!(second[0].contains("<leader>ss"), "{second:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_run_under_the_same_config_announces_nothing() {
        let dir = scratch("second");
        let record = dir.join("native-first-run.toml");
        let report = handovers();
        let cfg = Some(Path::new("/cfg/view.toml"));

        let first = first_run(&report, cfg, &record).expect("the first run must record");
        assert!(!first.is_empty(), "the first run must announce something");
        let second = first_run(&report, cfg, &record).expect("the second run must read the record");

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
        let report = handovers();

        let first = first_run(&report, Some(Path::new("/cfg/a.toml")), &record)
            .expect("the first config must record");
        let other = first_run(&report, Some(Path::new("/cfg/b.toml")), &record)
            .expect("the second config must record");
        let none = first_run(&report, None, &record).expect("a config-less session must record");

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
        let full = handovers();
        let partial: Vec<Handover> = full.iter().take(1).cloned().collect();

        let first = first_run(&partial, cfg, &record).expect("the partial report must record");
        assert_eq!(first.len(), partial.len());
        let rest = first_run(&full, cfg, &record).expect("the full report must record");

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
            first_run(&handovers(), None, &record).expect("a corrupt record must not fail");

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

        let notices =
            first_run(&handovers(), None, &record).expect("a newer record must not fail the run");

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
            first_run(&handovers(), None, &record).expect("the directory must be created");

        assert!(!notices.is_empty());
        assert!(record.exists(), "the record must exist after a first run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_ordinary_config_path_is_its_own_record_key() {
        assert_eq!(config_key(Path::new("/cfg/view.toml")), "/cfg/view.toml");
        assert_eq!(
            config_key(Path::new("/cfg/ünïcode.toml")),
            "/cfg/ünïcode.toml"
        );
        assert_eq!(
            config_key(Path::new("/cfg/50%/view.toml")),
            "/cfg/50%25/view.toml"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_undecodable_config_paths_do_not_share_one_record_key() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // `Path::display` renders both of these as `/cfg/\u{fffd}/view.toml`,
        // so keying on it would let the second config inherit the silence the
        // first one earned
        let first = PathBuf::from(OsStr::from_bytes(b"/cfg/\xff/view.toml"));
        let second = PathBuf::from(OsStr::from_bytes(b"/cfg/\xfe/view.toml"));
        assert_eq!(
            first.display().to_string(),
            second.display().to_string(),
            "this test is meaningless unless display() really does collide"
        );
        assert_ne!(config_key(&first), config_key(&second));

        let dir = scratch("undecodable");
        let record = dir.join("native-first-run.toml");
        let report = handovers();
        let announced =
            first_run(&report, Some(&first), &record).expect("the first config must record");
        let other =
            first_run(&report, Some(&second), &record).expect("the second config must record");
        assert!(!announced.is_empty());
        assert_eq!(
            announced, other,
            "a second config under a different undecodable path introduces itself on its own terms"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_v1_record_written_by_an_earlier_build_still_silences_its_surfaces() {
        // every byte a literal, including the record keys: derive any half
        // of this blob from the code under test and a spelling change
        // regenerates the "old" file and passes, while the records already
        // on disk -- the only ones this pin exists for -- go stale unseen.
        // `handovers()` is a supersession of `statusline` plus a claimed
        // `<leader>ff`, so these are exactly the two keys a v1 build wrote
        const V1_RECORD: &str = "schema_version = 1\n\n\
             [announced]\n\
             \"/cfg/view.toml\" = [\"picker:key:<leader>ff\", \"statusline\"]\n";

        let dir = scratch("v1-compat");
        let record = dir.join("native-first-run.toml");
        let report = handovers();
        std::fs::write(&record, V1_RECORD).expect("the record must be writable");

        let notices = first_run(&report, Some(Path::new("/cfg/view.toml")), &record)
            .expect("a v1 record must be readable by this build");

        assert!(
            notices.is_empty(),
            "every surface a v1 record already announced must stay quiet, got {notices:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("the record must be readable"),
            V1_RECORD,
            "a run with nothing to announce must not rewrite the record"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_report_writes_nothing_at_all() {
        let dir = scratch("empty");
        let record = dir.join("native-first-run.toml");

        let notices = first_run(&[], None, &record).expect("an empty report must not fail");

        assert!(notices.is_empty());
        assert!(
            !record.exists(),
            "a run with nothing to say must not create a record"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
