//! The `[dvr]` table: whether this session is recorded for rewind, and how
//! much memory the recording may hold.

use serde::{Deserialize, Serialize};
use view_core::config::{discarded_file, Source, BOOL_EXPECTED};

use super::resolve::{env_read, layer, parse_bool};

/// What `[dvr] max_mb` answers when nothing names it.
const MAX_MB_DEFAULT: u32 = 64;

/// The memory bounds `[dvr] max_mb` accepts, in MiB.
const MAX_MB_RANGE: std::ops::RangeInclusive<u32> = 1..=4096;

/// What a `[dvr] max_mb` value has to be.
const MAX_MB_EXPECTED: &str = "a whole number of MiB from 1 to 4096";

/// The `[dvr]` table's wire shape. Unknown keys are refused, for the
/// reason `[supervision]`'s own check states, and each key is optional so
/// a document that spelled the default stays distinguishable from one
/// that left the key out.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DvrTable {
    /// Whether the session is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The memory the recording may hold, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mb: Option<u32>,
}

/// The session recording's resolved settings.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DvrConfig {
    /// Whether the session's painted frames and input are recorded.
    pub enabled: bool,
    /// The memory the recording may hold, in MiB. The oldest whole frames
    /// are dropped past it.
    pub max_mb: u32,
}

impl Default for DvrConfig {
    /// Off, with a 64 MiB bound for when it is turned on.
    fn default() -> Self {
        Self {
            enabled: false,
            max_mb: MAX_MB_DEFAULT,
        }
    }
}

impl DvrConfig {
    /// [`Self::max_mb`] in bytes.
    #[must_use]
    pub fn max_bytes(self) -> usize {
        usize::try_from(self.max_mb)
            .unwrap_or(usize::MAX)
            .saturating_mul(1 << 20)
    }
}

/// A `max_mb` value, or `None` for text that is no whole number in range.
fn parse_max_mb(value: &str) -> Option<u32> {
    value.parse().ok().filter(|mb| MAX_MB_RANGE.contains(mb))
}

/// The `[dvr]` table with the environment layered over `file`, and where
/// `enabled` and `max_mb` came from. A `max_mb` out of range in either
/// layer is set aside with a notice and the layer below answers.
pub(super) fn resolve(
    file: &DvrTable,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> (DvrConfig, Source, Source) {
    let enabled = layer(
        None,
        env_read(env, "dvr", "enabled", BOOL_EXPECTED, parse_bool, notices),
        file.enabled,
        DvrConfig::default().enabled,
    );
    let file_mb = file.max_mb.filter(|mb| {
        let fits = MAX_MB_RANGE.contains(mb);
        if !fits {
            notices.push(discarded_file(
                &mb.to_string(),
                MAX_MB_EXPECTED,
                "dvr",
                "max_mb",
            ));
        }
        fits
    });
    let max_mb = layer(
        None,
        env_read(env, "dvr", "max_mb", MAX_MB_EXPECTED, parse_max_mb, notices),
        file_mb,
        MAX_MB_DEFAULT,
    );
    let config = DvrConfig {
        enabled: enabled.value,
        max_mb: max_mb.value,
    };
    (config, enabled.source, max_mb.source)
}

/// One `[dvr]` key's rendered value and the layer it came from, with
/// `sources` in `enabled`, `max_mb` order.
pub(super) fn report(key: &str, cfg: &DvrConfig, sources: [Source; 2]) -> Option<(String, Source)> {
    let [enabled, max_mb] = sources;
    match key {
        "enabled" => Some((cfg.enabled.to_string(), enabled)),
        "max_mb" => Some((cfg.max_mb.to_string(), max_mb)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{resolve_with, Overrides, ViewConfig};

    fn rows(toml: &str, env: &[(&str, &str)]) -> (Vec<(String, String, Source)>, Vec<String>) {
        let file = ViewConfig::from_toml_str(toml).unwrap();
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        };
        let resolved = resolve_with(&file, &Overrides::default(), &lookup);
        let rows = resolved
            .rows()
            .into_iter()
            .filter(|(key, _, _)| key.table == "dvr")
            .map(|(key, value, source)| (key.key.to_string(), value, source))
            .collect();
        (rows, resolved.notices().to_vec())
    }

    fn row(key: &str, value: &str, source: Source) -> (String, String, Source) {
        (key.to_string(), value.to_string(), source)
    }

    #[test]
    fn dvr_is_off_unless_enabled() {
        let (absent, _) = rows("", &[]);
        assert_eq!(
            absent,
            [
                row("enabled", "false", Source::Derived),
                row("max_mb", "64", Source::Derived),
            ]
        );
        assert!(!ViewConfig::defaults().dvr.enabled);
        let on = ViewConfig::from_toml_str("[dvr]\nenabled = true\n").unwrap();
        assert!(on.dvr.enabled);
        let (spelled, _) = rows("[dvr]\nenabled = true\n", &[]);
        assert_eq!(spelled[0], row("enabled", "true", Source::File));
    }

    #[test]
    fn max_mb_rejects_zero_with_a_notice_and_keeps_the_default() {
        let (file, notices) = rows("[dvr]\nmax_mb = 0\n", &[]);
        assert_eq!(file[1], row("max_mb", "64", Source::Derived));
        assert!(
            notices.iter().any(|n| n.contains("[dvr] max_mb = 0")),
            "{notices:?}"
        );
        let (env, notices) = rows("", &[("VIEW_DVR_MAX_MB", "5000")]);
        assert_eq!(env[1], row("max_mb", "64", Source::Derived));
        assert!(
            notices.iter().any(|n| n.contains("VIEW_DVR_MAX_MB=5000")),
            "{notices:?}"
        );
        let (edges, notices) = rows("[dvr]\nmax_mb = 4096\n", &[("VIEW_DVR_MAX_MB", "1")]);
        assert_eq!(edges[1], row("max_mb", "1", Source::Env));
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn env_overrides_the_file_for_dvr() {
        let (resolved, _) = rows(
            "[dvr]\nenabled = true\nmax_mb = 32\n",
            &[("VIEW_DVR_ENABLED", "false"), ("VIEW_DVR_MAX_MB", "128")],
        );
        assert_eq!(
            resolved,
            [
                row("enabled", "false", Source::Env),
                row("max_mb", "128", Source::Env),
            ]
        );
        let file = ViewConfig::from_toml_str("[dvr]\nmax_mb = 32\n").unwrap();
        let env = |name: &str| (name == "VIEW_DVR_ENABLED").then(|| "true".to_string());
        let tables = resolve_with(&file, &Overrides::default(), &env).tables;
        assert_eq!(
            tables.dvr,
            DvrConfig {
                enabled: true,
                max_mb: 32
            }
        );
        assert_eq!(tables.dvr.max_bytes(), 32 << 20);
    }
}
