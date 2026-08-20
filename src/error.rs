//! One error type for the whole binary.
//!
//! A rotator is a single process with one poll loop, so a single enum is the
//! simplest thing that works. Splitting it per module would buy nothing that
//! the variant names do not already say.

use core::fmt;
use std::path::PathBuf;

use shep_client::{ConnectError, RequestError};

use crate::config::ConfigError;

/// Anything that can go wrong in one pass of the rotator.
#[derive(Debug)]
pub enum Error {
    /// The shepherd's socket could not be reached.
    Connect(ConnectError),
    /// A request reached the shepherd and came back an error.
    Request(RequestError),
    /// The shepherd answered with a response this dog cannot use.
    Protocol(String),
    /// The `[dog.log-rotate]` section could not be understood.
    Config(ConfigError),
    /// A filesystem operation failed, naming the path it failed on.
    Io {
        /// The path being read, renamed, compressed or deleted.
        path: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// A generation counter for one log has reached `u32::MAX`, so the next
    /// rotation would need to wrap it rather than advance it.
    ///
    /// Wrapping is worse than refusing: a wrapped dated counter would retest
    /// a same-second slot already known to be occupied, forever, and a
    /// wrapped numeric generation would produce `.0`, a name
    /// `naming::match_generation` never recognises again - orphaned rather
    /// than pruned. See `rotate::rotate_dated` and `rotate::rotate_numeric`.
    Exhausted {
        /// The log (or, mid numeric shift, the specific generation file)
        /// whose next number would have wrapped.
        path: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "cannot reach the shepherd: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused a request: {err}"),
            Self::Protocol(what) => write!(f, "unexpected answer from the shepherd: {what}"),
            Self::Config(err) => write!(f, "bad [dog.log-rotate] section: {err}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Exhausted { path } => {
                write!(
                    f,
                    "{}: no generation numbers left to rotate into",
                    path.display()
                )
            }
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err),
            Self::Request(err) => Some(err),
            Self::Config(err) => Some(err),
            Self::Io { source, .. } => Some(source),
            Self::Protocol(_) | Self::Exhausted { .. } => None,
        }
    }
}

impl From<ConnectError> for Error {
    fn from(err: ConnectError) -> Self {
        Self::Connect(err)
    }
}

impl From<RequestError> for Error {
    fn from(err: RequestError) -> Self {
        Self::Request(err)
    }
}

impl From<ConfigError> for Error {
    fn from(err: ConfigError) -> Self {
        Self::Config(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_io_error_names_the_path_it_failed_on() {
        let err = Error::Io {
            path: PathBuf::from("/var/log/web-0-out.log"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let shown = err.to_string();
        assert!(shown.contains("/var/log/web-0-out.log"), "{shown}");
        assert!(shown.contains("denied"), "{shown}");
    }

    #[test]
    fn every_variant_renders_without_an_em_dash() {
        let err = Error::Protocol("the shepherd answered Pong to a DogConfig".into());
        let shown = err.to_string();
        assert!(!shown.contains('\u{2014}'), "em dash in {shown}");
        assert!(!shown.contains('\u{2013}'), "en dash in {shown}");
    }
}
