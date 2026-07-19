//! The corpus TOML schema and loader: each entry is a durable, reviewable
//! artifact (name, input key-notation, the engine pin it was authored
//! against, an ext-option set name) instead of a hardcoded Rust test, per
//! the design spec's manifest rule (engine pin + ext set travel WITH
//! the entry, not encoded into a path -- a path-encoded scheme cannot carry
//! per-entry quiesce overrides and re-keys every entry on a pin bump,
//! defeating "corpus survives pin bumps").
//!
//! `schema = 1` is the only version this loader accepts; a corpus file
//! stamping any other value is rejected rather than guessed at, since a
//! schema bump implies a shape change this loader has not been taught to
//! read. `#[serde(deny_unknown_fields)]` makes an unrecognized field a hard
//! load error instead of a silently ignored typo (e.g. `qiesce_silence_ms`
//! never actually overriding anything, and nobody finding out).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// The only `schema` value this loader accepts today.
const SUPPORTED_SCHEMA: u32 = 1;

/// The only `ext_set` name recognized today: `wire::UI_EXT_OPTIONS`, the
/// full `ext_*` set both `EngineSession` and `ReferenceSession` always
/// request on attach. A named set (rather than the set itself living in
/// the entry) leaves room for a future reduced-ext-set fixture without
/// changing this field's shape.
const DEFAULT_EXT_SET: &str = "default";

/// Default quiesce silence window, matching the window
/// `view-oracle`'s own end-to-end parity test
/// (`engine_and_reference_sessions_agree_across_the_full_parity_check`)
/// uses: long enough that a real redraw burst never lands inside it by
/// chance, short enough that a healthy run settles in well under a second.
const DEFAULT_QUIESCE_SILENCE_MS: u64 = 200;
/// Default quiesce deadline, matching the same test's bound: generous
/// enough for a slow CI runner to settle, tight enough that a genuinely
/// wedged session fails an entry instead of hanging the whole corpus run.
const DEFAULT_QUIESCE_DEADLINE_MS: u64 = 5_000;

/// The wire shape of a corpus TOML file, deserialized directly by `toml`
/// before any validation. Kept separate from [`CorpusEntry`] (the
/// post-validation, defaults-applied form the runner actually drives) so a
/// missing-field or unknown-field error surfaces from `serde`/`toml`
/// itself, in their own words, rather than being re-derived by hand here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    schema: u32,
    name: String,
    input: String,
    engine_pin: String,
    ext_set: String,
    quiesce_silence_ms: Option<u64>,
    quiesce_deadline_ms: Option<u64>,
}

/// One validated, defaults-applied corpus entry, ready for the runner to
/// drive: a scripted key-notation `input`, the engine pin it was authored
/// against (for the runner's pin-drift warning, never a hardcoded literal
/// here), and the ext-option set name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusEntry {
    pub name: String,
    pub input: String,
    pub engine_pin: String,
    pub ext_set: String,
    pub quiesce_silence_ms: u64,
    pub quiesce_deadline_ms: u64,
}

/// Errors loading or validating a corpus entry.
#[derive(Debug, Error)]
pub enum CorpusError {
    /// The corpus file could not be read from disk.
    #[error("failed to read corpus file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file's content is not valid TOML for the [`RawEntry`] shape --
    /// covers both a malformed document and a `deny_unknown_fields`
    /// rejection of an unrecognized key.
    #[error("failed to parse corpus TOML")]
    Toml(#[from] toml::de::Error),
    /// `schema` named a version this loader has not been taught to read.
    #[error("unsupported corpus schema {0} (only schema = 1 is recognized)")]
    UnsupportedSchema(u32),
    /// `ext_set` named a set this loader does not recognize.
    #[error("unknown ext_set {0:?} (only \"default\" is recognized)")]
    UnknownExtSet(String),
}

/// Parses and validates one corpus entry from its raw TOML text.
///
/// # Errors
///
/// Returns [`CorpusError::Toml`] on malformed TOML or an unrecognized
/// field (missing `ext_set` included: it is a required `RawEntry` field,
/// so `toml`'s own missing-field error covers it without a second check
/// here), [`CorpusError::UnsupportedSchema`] if `schema` is not
/// [`SUPPORTED_SCHEMA`], or [`CorpusError::UnknownExtSet`] if `ext_set`
/// does not name a recognized set.
pub fn parse(raw_toml: &str) -> Result<CorpusEntry, CorpusError> {
    let raw: RawEntry = toml::from_str(raw_toml)?;
    if raw.schema != SUPPORTED_SCHEMA {
        return Err(CorpusError::UnsupportedSchema(raw.schema));
    }
    if raw.ext_set != DEFAULT_EXT_SET {
        return Err(CorpusError::UnknownExtSet(raw.ext_set));
    }
    Ok(CorpusEntry {
        name: raw.name,
        input: raw.input,
        engine_pin: raw.engine_pin,
        ext_set: raw.ext_set,
        quiesce_silence_ms: raw.quiesce_silence_ms.unwrap_or(DEFAULT_QUIESCE_SILENCE_MS),
        quiesce_deadline_ms: raw
            .quiesce_deadline_ms
            .unwrap_or(DEFAULT_QUIESCE_DEADLINE_MS),
    })
}

/// Reads `path` from disk and [`parse`]s it.
///
/// # Errors
///
/// Returns [`CorpusError::Io`] if `path` cannot be read, or any error
/// [`parse`] returns.
pub fn load_file(path: &Path) -> Result<CorpusEntry, CorpusError> {
    let raw = std::fs::read_to_string(path).map_err(|source| CorpusError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const VALID: &str = r#"
schema = 1
name = "insert-basic"
input = "ihello world<Esc>0x"
engine_pin = "v0.12.4"
ext_set = "default"
"#;

    #[test]
    fn valid_entry_parses() {
        let entry = parse(VALID).expect("VALID must parse as a corpus entry");
        assert_eq!(entry.name, "insert-basic");
        assert_eq!(entry.input, "ihello world<Esc>0x");
        assert_eq!(entry.engine_pin, "v0.12.4");
        assert_eq!(entry.ext_set, "default");
        // defaults applied when the TOML omits the quiesce overrides
        assert_eq!(entry.quiesce_silence_ms, DEFAULT_QUIESCE_SILENCE_MS);
        assert_eq!(entry.quiesce_deadline_ms, DEFAULT_QUIESCE_DEADLINE_MS);
    }

    #[test]
    fn quiesce_overrides_are_honored_when_present() {
        let toml = format!("{VALID}\nquiesce_silence_ms = 50\nquiesce_deadline_ms = 9000\n");
        let entry = parse(&toml).expect("VALID plus quiesce overrides must parse");
        assert_eq!(entry.quiesce_silence_ms, 50);
        assert_eq!(entry.quiesce_deadline_ms, 9000);
    }

    #[test]
    fn unknown_field_is_rejected_not_ignored() {
        let toml = format!("{VALID}\nbogus_field = \"nope\"\n");
        let err = parse(&toml).expect_err("an unknown field must be a hard error");
        assert!(
            matches!(err, CorpusError::Toml(_)),
            "expected a Toml error for an unknown field, got {err:?}"
        );
    }

    #[test]
    fn missing_ext_set_is_rejected() {
        let toml = r#"
schema = 1
name = "insert-basic"
input = "ihello world<Esc>0x"
engine_pin = "v0.12.4"
"#;
        let err = parse(toml).expect_err("a missing ext_set must be a hard error");
        assert!(
            matches!(err, CorpusError::Toml(_)),
            "expected a Toml error for a missing required field, got {err:?}"
        );
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let toml = VALID.replace("schema = 1", "schema = 2");
        let err = parse(&toml).expect_err("an unrecognized schema must be a hard error");
        assert!(
            matches!(err, CorpusError::UnsupportedSchema(2)),
            "expected UnsupportedSchema(2), got {err:?}"
        );
    }

    #[test]
    fn unknown_ext_set_is_rejected() {
        let toml = VALID.replace(r#"ext_set = "default""#, r#"ext_set = "minimal""#);
        let err = parse(&toml).expect_err("an unrecognized ext_set must be a hard error");
        assert!(
            matches!(err, CorpusError::UnknownExtSet(ref s) if s == "minimal"),
            "expected UnknownExtSet(\"minimal\"), got {err:?}"
        );
    }
}
