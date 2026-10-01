//! The first-run record: which surfaces the launch box has already told a
//! user view took over, under which config.
//!
//! Once per surface per config path. A message that repeats every launch is
//! noise a user learns to skip, and the reversal line it carries is the part
//! that must still be read the day they want it back. Keying on the config
//! path as well as the surface means a second config (a bare `--clean`
//! session, a machine-specific file) introduces itself on its own terms.
//!
//! The record is state. Deleting it costs a user one repeated notice, so it
//! lives beside the theme cache, apart from the files a user wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Records every surface in `report` as announced under `config_path`, so
/// the launch box names each of them once.
///
/// A run whose report the record already covers writes nothing.
///
/// `config_path` is `None` for a session running without a config file at
/// all, which is recorded as its own key so it never merges into whichever
/// config ran last.
///
/// A record this build cannot parse is treated as absent and rewritten,
/// re-announcing at most once. A record from a newer schema is left exactly
/// as it is: a downgraded build cannot know what that file already promised
/// the user.
pub fn first_run(
    report: &[Handover],
    config_path: Option<&Path>,
    record: &Path,
) -> Result<(), ToastError> {
    if report.is_empty() {
        return Ok(());
    }

    let mut current = read_record(record)?;
    if current.schema_version > SCHEMA_VERSION {
        return Ok(());
    }
    current.schema_version = SCHEMA_VERSION;

    let key = config_path.map_or_else(String::new, config_key);
    let announced = current.announced.entry(key.clone()).or_default();

    let mut news = false;
    for entry in report {
        let key = entry.record_key();
        if !announced.contains(&key) {
            announced.push(key);
            news = true;
        }
    }
    if !news {
        return Ok(());
    }
    announced.sort();
    write_record(record, &mut current, &key)
}

/// The keys already announced under `config_path`, which a session seeds
/// its once-per-config notices with.
///
/// Read on the same terms [`first_run`] reads: an absent or unreadable
/// record announces nothing, so everything is news once.
pub fn announced_keys(
    config_path: Option<&Path>,
    record: &Path,
) -> Result<Vec<String>, ToastError> {
    let current = read_record(record)?;
    let config = config_path.map_or_else(String::new, config_key);
    Ok(current.announced.get(&config).cloned().unwrap_or_default())
}

/// Records `key` as announced under `config_path`, for a notice a session
/// raised itself.
///
/// A key already there writes nothing, and a record from a newer schema is
/// left as it is, on the terms [`first_run`] gives both.
pub fn record_key(config_path: Option<&Path>, key: &str, record: &Path) -> Result<(), ToastError> {
    let mut current = read_record(record)?;
    if current.schema_version > SCHEMA_VERSION {
        return Ok(());
    }
    current.schema_version = SCHEMA_VERSION;
    let config = config_path.map_or_else(String::new, config_key);
    let announced = current.announced.entry(config.clone()).or_default();
    if announced.iter().any(|known| known == key) {
        return Ok(());
    }
    announced.push(key.to_string());
    announced.sort();
    write_record(record, &mut current, &config)
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

/// The config path `key` was written for, the inverse of [`config_key`].
///
/// `None` for a key this build did not write, and on a platform whose paths
/// are not bytes, for one holding a byte that is not UTF-8.
fn config_path_of(key: &str) -> Option<PathBuf> {
    let mut bytes = Vec::with_capacity(key.len());
    let mut rest = key.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = tail.get(2..)?;
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Whether the record may drop what it holds under `key`: the key names an
/// absolute path the filesystem answers is gone.
///
/// A key that does not decode, a relative one and one whose existence the
/// filesystem cannot answer are all kept, since dropping one re-announces
/// every notice under a config that may still be there.
fn config_is_gone(key: &str) -> bool {
    config_path_of(key)
        .filter(|path| path.is_absolute())
        .is_some_and(|path| matches!(path.try_exists(), Ok(false)))
}

/// Writes `record` to `path`, creating the state directory if this is the
/// first thing view has ever stored there.
///
/// Every config the record names that no longer exists is dropped first,
/// apart from `keep`, the config being written for. A config is written for
/// under a path every launch, and a record nothing prunes keeps one entry
/// for each temporary config a script ever launched with.
///
/// Another view may have written the record since `record` was read, so
/// the file is read again immediately before the rename and its keys merged
/// in. Nothing locks the file between that read and the rename: two views
/// writing inside that window keep only the later one's additions, which
/// costs the other's notice once more on a later launch.
fn write_record(path: &Path, record: &mut Record, keep: &str) -> Result<(), ToastError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ToastError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let on_disk = read_record(path)?;
    if on_disk.schema_version > SCHEMA_VERSION {
        return Ok(());
    }
    for (config, keys) in on_disk.announced {
        let merged = record.announced.entry(config).or_default();
        for key in keys {
            if !merged.contains(&key) {
                merged.push(key);
            }
        }
        merged.sort();
    }
    record
        .announced
        .retain(|config, _| config == keep || !config_is_gone(config));
    let rendered = toml::to_string(record).map_err(|source| ToastError::Serialize { source })?;
    // written beside the record and renamed over it, so a process killed
    // mid-write leaves the previous record whole; the pid keeps two views
    // writing at once off each other's temp file
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let written = std::fs::write(&temp, rendered).and_then(|()| std::fs::rename(&temp, path));
    written.map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        ToastError::Write {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use crate::config::NativeConfig;
    use crate::report::report;
    use crate::supersede::plan;
    use view_core::model::Look;
    use view_core::native::mappings::MappingClaim;
    use view_core::native::registry;
    use view_test_support::ScratchDir;

    /// A scratch directory for one test's record file, named for the test
    /// so two of them never share a path.
    fn scratch(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("toast-{name}")).expect("the scratch directory must be creatable")
    }

    fn claim(feature: &str, lhs: &str) -> MappingClaim {
        MappingClaim::new(feature, lhs, true)
    }

    /// Both surface kinds in one report, since the toast has to introduce
    /// held options and taken keys through the same pass.
    fn handovers() -> Vec<Handover> {
        report(
            &plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            &[claim("picker", "<leader>ff")],
            registry::features(),
        )
    }

    /// Runs [`first_run`] and answers the record keys it added under
    /// `config`, which are the surfaces the launch box names this time.
    fn announce(report: &[Handover], config: Option<&Path>, record: &Path) -> Vec<String> {
        let before = announced_keys(config, record).expect("the record must read");
        first_run(report, config, record).expect("the run must record");
        announced_keys(config, record)
            .expect("the record must read")
            .into_iter()
            .filter(|key| !before.contains(key))
            .collect()
    }

    #[test]
    fn the_first_run_announces_every_handed_over_surface_with_its_off_switch() {
        let dir = scratch("first");
        let record = dir.join("native-first-run.toml");
        let report = handovers();

        let notices = announce(&report, Some(Path::new("/cfg/view.toml")), &record);

        assert_eq!(
            notices.len(),
            report.len(),
            "every handed-over surface introduces itself once, got {notices:?}"
        );
        for key in ["statusline", "picker:key:<leader>ff"] {
            assert!(
                notices.iter().any(|n| n == key),
                "held options and taken keys record through the same pass: {notices:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_key_taken_later_speaks_even_though_its_feature_already_announced() {
        let dir = scratch("later-key");
        let record = dir.join("native-first-run.toml");
        let cfg = Some(Path::new("/cfg/view.toml"));
        let features = registry::features();
        let options = report(
            &plan(&NativeConfig::all_enabled(), features, Look::default()),
            &[],
            features,
        );
        let with_key = report(
            &plan(&NativeConfig::all_enabled(), features, Look::default()),
            &[claim("statusline", "<leader>ss")],
            features,
        );

        let first = announce(&options, cfg, &record);
        assert!(!first.is_empty(), "the first run must announce something");
        let second = announce(&with_key, cfg, &record);

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

        let first = announce(&report, cfg, &record);
        assert!(!first.is_empty(), "the first run must announce something");
        let second = announce(&report, cfg, &record);

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

        let first = announce(&report, Some(Path::new("/cfg/a.toml")), &record);
        let other = announce(&report, Some(Path::new("/cfg/b.toml")), &record);
        let none = announce(&report, None, &record);

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

        let first = announce(&partial, cfg, &record);
        assert_eq!(first.len(), partial.len());
        let rest = announce(&full, cfg, &record);

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

        let notices = announce(&handovers(), None, &record);

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

        let notices = announce(&handovers(), None, &record);

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

        let notices = announce(&handovers(), None, &record);

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
        let announced = announce(&report, Some(&first), &record);
        let other = announce(&report, Some(&second), &record);
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
        // a v1 build superseded `statusline` and claimed `<leader>ff`, so
        // these are exactly the two keys such a file holds
        const V1_RECORD: &str = "schema_version = 1\n\n\
             [announced]\n\
             \"/cfg/view.toml\" = [\"picker:key:<leader>ff\", \"statusline\"]\n";
        const V1_SURFACES: [&str; 2] = ["picker:key:<leader>ff", "statusline"];

        let dir = scratch("v1-compat");
        let record = dir.join("native-first-run.toml");
        // the report is narrowed to what a v1 build could have announced,
        // rather than being today's whole report: this pin is about a v1
        // file still decoding, and a surface added to the plan afterwards is
        // genuinely un-announced -- news the record has no entry to silence
        let report: Vec<Handover> = handovers()
            .into_iter()
            .filter(|h| V1_SURFACES.contains(&h.record_key().as_str()))
            .collect();
        assert_eq!(
            report.len(),
            V1_SURFACES.len(),
            "both surfaces a v1 record names must still exist in this build: {report:?}"
        );
        std::fs::write(&record, V1_RECORD).expect("the record must be writable");

        let notices = announce(&report, Some(Path::new("/cfg/view.toml")), &record);

        assert!(
            notices.is_empty(),
            "every surface a v1 record already announced must stay quiet, got {notices:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("the record must be readable"),
            V1_RECORD,
            "a run with nothing to announce must not rewrite the record"
        );

        // the other half of the same file: a surface this build takes over
        // that no v1 record could name is still news, and announcing it must
        // not re-announce the two the record already covers
        let later = announce(&handovers(), Some(Path::new("/cfg/view.toml")), &record);
        let mut expected: Vec<String> = handovers()
            .iter()
            .filter(|h| !V1_SURFACES.contains(&h.record_key().as_str()))
            .map(Handover::record_key)
            .collect();
        expected.sort();
        assert_eq!(
            later, expected,
            "only the surfaces a v1 record never named may introduce themselves"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A config that has been deleted is dropped from the record at the
    /// next write, while the config being written for, a session with no
    /// config and a config that still exists all stay.
    #[test]
    fn a_record_forgets_configs_that_no_longer_exist() {
        let dir = scratch("prune");
        let record = dir.join("native-first-run.toml");
        let kept = dir.join("kept.toml");
        let gone = dir.join("gone.toml");
        std::fs::write(&kept, "").expect("the kept config must be writable");

        record_key(Some(&kept), "held:statusline", &record).expect("the kept config records");
        record_key(None, "held:statusline", &record).expect("a config-less session records");
        // the config being written for stays even while it does not exist
        record_key(Some(&gone), "held:tabline", &record).expect("a missing config records");
        assert_eq!(
            announced_keys(Some(&gone), &record).expect("the record reads"),
            vec!["held:tabline".to_string()]
        );

        record_key(Some(&kept), "held:vim.notify", &record).expect("the kept config records");

        assert!(
            announced_keys(Some(&gone), &record)
                .expect("the record reads")
                .is_empty(),
            "a deleted config's entry must be dropped: {}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );
        assert_eq!(
            announced_keys(Some(&kept), &record).expect("the record reads"),
            vec!["held:statusline".to_string(), "held:vim.notify".to_string()]
        );
        assert_eq!(
            announced_keys(None, &record).expect("the record reads"),
            vec!["held:statusline".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A key another view wrote after this one read the record survives
    /// this one's write.
    ///
    /// Disconfirm: writing the record as read, with no read before the
    /// rename, drops `held:tabline`.
    #[test]
    fn a_write_keeps_a_key_another_view_wrote_since_the_read() {
        let dir = scratch("merge");
        let record = dir.join("native-first-run.toml");
        let mut read_earlier = read_record(&record).expect("an absent record reads");
        record_key(None, "held:tabline", &record).expect("the other view records");
        read_earlier.schema_version = SCHEMA_VERSION;
        read_earlier
            .announced
            .entry(String::new())
            .or_default()
            .push("held:statusline".to_string());
        write_record(&record, &mut read_earlier, "").expect("the write lands");
        assert_eq!(
            announced_keys(None, &record).expect("the record reads"),
            vec!["held:statusline".to_string(), "held:tabline".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Decoding a key gives back the path it was written for, so the prune
    /// checks the file the user named.
    #[test]
    fn a_record_key_decodes_to_its_own_config_path() {
        for path in ["/cfg/view.toml", "/cfg/50%/view.toml", "/cfg/ünïcode.toml"] {
            assert_eq!(
                config_path_of(&config_key(Path::new(path))),
                Some(PathBuf::from(path))
            );
        }
        assert_eq!(config_path_of("/cfg/%G1"), None);
        assert_eq!(config_path_of("/cfg/%4"), None);
    }

    #[test]
    fn an_empty_report_writes_nothing_at_all() {
        let dir = scratch("empty");
        let record = dir.join("native-first-run.toml");

        first_run(&[], None, &record).expect("an empty report must not fail");

        assert!(
            !record.exists(),
            "a run with nothing to say must not create a record"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A reader that reads the record on every change sees either the
    /// previous record or the next one, whole: a process killed mid-write
    /// leaves a record the next launch can still read.
    #[test]
    fn a_record_is_never_observed_half_written() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = scratch("whole");
        let record = dir.join("native-first-run.toml");
        let done = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = {
            let record = record.clone();
            let done = Arc::clone(&done);
            let reads = Arc::clone(&reads);
            std::thread::spawn(move || {
                let mut torn = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    let Ok(raw) = std::fs::read_to_string(&record) else {
                        continue;
                    };
                    reads.fetch_add(1, Ordering::Relaxed);
                    let parsed = toml::from_str::<Record>(&raw);
                    if parsed.map_or(true, |parsed| parsed.announced.is_empty()) {
                        torn.push(raw.len());
                    }
                }
                torn
            })
        };
        let padding = "k".repeat(200);
        let mut watched = 0usize;
        for n in 0..300 {
            let before = reads.load(Ordering::Relaxed);
            record_key(None, &format!("{padding}:{n:04}"), &record).expect("the write must land");
            // the writer waits for a read to start after its write, so a
            // loaded host that starves the reader thread slows the test
            // down without leaving the writes unwatched
            let deadline = std::time::Instant::now()
                + view_test_support::HostBudget::host_only(std::time::Duration::from_millis(500))
                    .total();
            while reads.load(Ordering::Relaxed) <= before + 1 {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::yield_now();
            }
            if reads.load(Ordering::Relaxed) <= before + 1 {
                break;
            }
            watched += 1;
        }
        done.store(true, Ordering::Relaxed);
        let torn = reader.join().expect("the reader must finish");

        assert_eq!(
            watched, 300,
            "the reader must watch every write: {watched} of 300 were read after they landed"
        );
        assert!(
            torn.is_empty(),
            "a reader saw a half-written record: {torn:?}"
        );
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("the scratch directory must read")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "native-first-run.toml")
            .collect();
        assert!(
            left.is_empty(),
            "a temp file was left beside the record: {left:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
