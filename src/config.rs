//! The `[dog.log-rotate]` section of `shep.toml`. Filled in by a later task.

use core::fmt;

/// Placeholder so `error::Error::Config` has a concrete type to wrap and the
/// crate compiles from this task onward. A later task replaces this with the
/// real variants (`Toml`, `Size`, `Duration`, `Naming`).
#[derive(Debug)]
pub struct ConfigError;

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config error")
    }
}

impl core::error::Error for ConfigError {}
