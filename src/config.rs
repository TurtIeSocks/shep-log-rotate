//! The `[dog.log-rotate]` section of `shep.toml`.
//!
//! The daemon serves this per request rather than caching it, so this dog
//! re-reads it every tick and never caches it either. Changing `max_size`
//! should not need a `shep disable` and `shep enable`.

use core::fmt;

use serde::Deserialize;
use shep_client::shep_core::values::{
    MemSize, ParseMemSizeError, ParseUpDurationError, UpDuration,
};

/// How rotated generations are named. See the README for the trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    /// `web-0-out.2026-08-20T15-04-05.log`. The default.
    Dated,
    /// `web-0-out.log.1`, shifting on every rotation. Newest is `.1`.
    Numeric,
}

/// The dog's settings, with a default for every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Rotate a log once it reaches this size.
    pub max_size: MemSize,
    /// Optionally also rotate on age, whatever the size.
    pub max_age: Option<UpDuration>,
    /// Generations to keep. Older ones are deleted.
    pub keep: usize,
    /// How rotated generations are named.
    pub naming: Naming,
    /// gzip rotated generations, newest one left plain so it stays greppable.
    pub compress: bool,
    /// How often to look.
    pub interval: UpDuration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_size: MemSize::from_bytes(10 * 1024 * 1024),
            max_age: None,
            keep: 5,
            naming: Naming::Dated,
            compress: true,
            interval: UpDuration::from_millis(60_000),
        }
    }
}

/// What could not be understood in a `[dog.log-rotate]` section.
///
/// Every variant names the offending field, and where possible the value
/// that was rejected, so an operator can find the typo without reading this
/// dog's source.
#[derive(Debug)]
pub enum ConfigError {
    /// The text was not valid TOML, or carried a key this dog does not know.
    ///
    /// Names no section. [`Error::Config`](crate::error::Error::Config) wraps
    /// this and supplies the `[dog.<name>]` the text came from, so spelling one
    /// here would print two sections for one fault and get one of them wrong
    /// for any dog not adopted under the default name.
    Toml(String),
    /// A `max_size` or similar was not spelled the way shep spells it.
    Size {
        /// The field the offending value was read from.
        field: &'static str,
        /// The value as written in `shep.toml`.
        value: String,
        /// The underlying parse failure.
        source: ParseMemSizeError,
    },
    /// A `max_age`/`interval` or similar was not spelled the way shep spells
    /// it.
    Duration {
        /// The field the offending value was read from.
        field: &'static str,
        /// The value as written in `shep.toml`.
        value: String,
        /// The underlying parse failure.
        source: ParseUpDurationError,
    },
    /// `naming` was neither `dated` nor `numeric`.
    Naming(String),
    /// `keep = 0`, which would delete every rotation the moment it was made.
    Keep,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(message) => write!(f, "invalid TOML: {message}"),
            Self::Size {
                field,
                value,
                source,
            } => write!(
                f,
                "{field} = \"{value}\" is not a size shep accepts: {source}"
            ),
            Self::Duration {
                field,
                value,
                source,
            } => write!(
                f,
                "{field} = \"{value}\" is not a duration shep accepts: {source}"
            ),
            Self::Naming(value) => write!(
                f,
                "naming = \"{value}\" is not a naming scheme; use \"dated\" or \"numeric\""
            ),
            Self::Keep => write!(
                f,
                "keep = 0 would delete every rotation the moment it was made; keep must be at least 1"
            ),
        }
    }
}

impl core::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Size { source, .. } => Some(source),
            Self::Duration { source, .. } => Some(source),
            Self::Toml(_) | Self::Naming(_) | Self::Keep => None,
        }
    }
}

/// The section's fields, read as strings so `max_size` and `max_age` go
/// through shep's own `FromStr` rather than serde's numeric deserializers.
/// Deserializing a bare number would silently accept spellings shep itself
/// refuses.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    max_size: Option<String>,
    max_age: Option<String>,
    keep: Option<usize>,
    naming: Option<String>,
    compress: Option<bool>,
    interval: Option<String>,
}

/// Parse `value` into a [`MemSize`], naming `field` in the error.
fn parse_size(value: String, field: &'static str) -> Result<MemSize, ConfigError> {
    value
        .parse::<MemSize>()
        .map_err(|source| ConfigError::Size {
            field,
            value,
            source,
        })
}

/// Parse `value` into an [`UpDuration`], naming `field` in the error.
fn parse_duration(value: String, field: &'static str) -> Result<UpDuration, ConfigError> {
    value
        .parse::<UpDuration>()
        .map_err(|source| ConfigError::Duration {
            field,
            value,
            source,
        })
}

impl Config {
    /// Parse the `[dog.log-rotate]` table's body.
    ///
    /// The empty string is the ordinary case: a dog with no section in
    /// `shep.toml` gets every default.
    ///
    /// # Errors
    /// - [`ConfigError::Toml`] - the text is not valid TOML, or carries a key
    ///   this dog does not know.
    /// - [`ConfigError::Size`] / [`ConfigError::Duration`] - a value is not
    ///   spelled the way shep spells it.
    /// - [`ConfigError::Naming`] - `naming` is neither `dated` nor `numeric`.
    /// - [`ConfigError::Keep`] - `keep = 0`, which would delete every
    ///   rotation the moment it was made.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let raw: Raw = toml::from_str(text).map_err(|err| ConfigError::Toml(err.to_string()))?;
        let defaults = Self::default();
        Ok(Self {
            max_size: raw
                .max_size
                .map(|value| parse_size(value, "max_size"))
                .transpose()?
                .unwrap_or(defaults.max_size),
            max_age: raw
                .max_age
                .map(|value| parse_duration(value, "max_age"))
                .transpose()?,
            keep: match raw.keep {
                Some(0) => return Err(ConfigError::Keep),
                Some(keep) => keep,
                None => defaults.keep,
            },
            naming: match raw.naming.as_deref() {
                None => defaults.naming,
                Some("dated") => Naming::Dated,
                Some("numeric") => Naming::Numeric,
                Some(other) => return Err(ConfigError::Naming(other.to_owned())),
            },
            compress: raw.compress.unwrap_or(defaults.compress),
            interval: raw
                .interval
                .map(|value| parse_duration(value, "interval"))
                .transpose()?
                .unwrap_or(defaults.interval),
        })
    }
}

/// A commented block naming every option and its default, for
/// `shep-log-rotate --print-config`.
///
/// Every line is commented, so appending it to `shep.toml` changes nothing
/// until the operator uncomments a line. A test asserts that what survives
/// uncommenting parses back to [`Config::default()`], so this text cannot
/// drift away from the code it documents.
pub const PRINT_CONFIG: &str = r#"[dog.log-rotate]
# Rotate a log once it reaches this size. shep's spelling: 10M, not 10MB.
#max_size = "10M"
# Optionally also rotate on age, whatever the size. Unset means size only.
# shep's UpDuration has no day unit: spell a week as hours, not "7d".
#max_age = "168h"
# Generations to keep. Older ones are deleted. Must be at least 1.
#keep = 5
# "dated" writes web-0-out.2026-08-20T15-04-05.log, in UTC, and still
# matches *.log. "numeric" writes web-0-out.log.1 and shifts on every
# rotation, the logrotate convention. Switching does not migrate existing
# files: they stop being pruned and are left for you.
#naming = "dated"
# gzip rotated generations. The newest is left plain so it stays greppable.
#compress = true
# How often to look.
#interval = "60s"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_no_dashes;

    #[test]
    fn an_absent_section_is_a_working_configuration() {
        let config = Config::from_toml("").expect("empty is valid");
        assert_eq!(config, Config::default());
        assert_eq!(config.max_size.bytes(), 10 * 1024 * 1024);
        assert_eq!(config.keep, 5);
        assert_eq!(config.naming, Naming::Dated);
        assert!(config.compress);
        assert_eq!(config.interval.as_duration().as_secs(), 60);
        assert_eq!(config.max_age, None);
    }

    #[test]
    fn every_field_is_read() {
        let config = Config::from_toml(
            r#"
max_size = "1M"
max_age = "168h"
keep = 3
naming = "numeric"
compress = false
interval = "5s"
"#,
        )
        .expect("valid");
        assert_eq!(config.max_size.bytes(), 1024 * 1024);
        // 168h, not "7d": UpDuration's grammar is `^\d+(h|m|s)?$`, with no
        // day unit, so "7d" is a spelling shep itself refuses. 168h is the
        // same span written the way shep actually accepts it.
        assert_eq!(
            config.max_age.expect("set").as_duration().as_secs(),
            7 * 86_400
        );
        assert_eq!(config.keep, 3);
        assert_eq!(config.naming, Naming::Numeric);
        assert!(!config.compress);
        assert_eq!(config.interval.as_duration().as_secs(), 5);
    }

    #[test]
    fn a_size_shep_refuses_is_refused_here_too() {
        // shep spells it 10M. A dog that also took 10MB would teach the wrong
        // thing about the ecosystem it lives in.
        let err = Config::from_toml(r#"max_size = "10MB""#).expect_err("refused");
        let shown = err.to_string();
        assert!(shown.contains("max_size"), "{shown}");
        assert!(shown.contains("10MB"), "{shown}");
    }

    #[test]
    fn an_unknown_key_is_reported_not_ignored() {
        let err = Config::from_toml(r#"max_sixe = "10M""#).expect_err("refused");
        assert!(err.to_string().contains("max_sixe"), "{err}");
    }

    #[test]
    fn an_unknown_naming_scheme_names_the_two_that_exist() {
        let err = Config::from_toml(r#"naming = "rolling""#).expect_err("refused");
        let shown = err.to_string();
        assert!(shown.contains("rolling"), "{shown}");
        assert!(shown.contains("dated"), "{shown}");
        assert!(shown.contains("numeric"), "{shown}");
    }

    #[test]
    fn keep_zero_is_refused_because_it_would_delete_every_rotation() {
        let err = Config::from_toml("keep = 0").expect_err("refused");
        assert!(err.to_string().contains("keep"), "{err}");
    }

    #[test]
    fn every_value_the_printed_block_documents_is_the_value_the_code_uses() {
        // PRINT_CONFIG has three kinds of line: the `[dog.log-rotate]` header,
        // prose comments (`# ` with a space), and commented settings
        // (`#key = value`, no space). Uncomment only the settings.
        let uncommented: Vec<&str> = PRINT_CONFIG
            .lines()
            .filter_map(|line| line.strip_prefix('#'))
            .filter(|rest| !rest.starts_with(' '))
            .collect();

        // The guard needs its own guard. The first version of this test used a
        // filter that matched NOTHING, so it asserted `from_toml("")` equals
        // the defaults -- true, vacuous, and a duplicate of
        // `an_absent_section_is_a_working_configuration`. It would have passed
        // with a `7d` in the block, the one spelling `UpDuration` refuses,
        // which is precisely the drift this test exists to catch.
        assert_eq!(
            uncommented.len(),
            6,
            "expected one line per setting, got {uncommented:?}"
        );

        let config =
            Config::from_toml(&uncommented.join("\n")).expect("the printed block is valid");

        // `max_age` is the one field whose default is "unset", so the block
        // documents a sample rather than a default. Everything else must be
        // exactly what the code already does.
        assert_eq!(
            config,
            Config {
                max_age: Some("168h".parse().expect("a spelling shep accepts")),
                ..Config::default()
            }
        );
    }

    #[test]
    fn the_printed_block_carries_no_em_dash() {
        assert_no_dashes(PRINT_CONFIG);
    }
}
