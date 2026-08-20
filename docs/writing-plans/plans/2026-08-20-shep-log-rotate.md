# shep-log-rotate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a log-rotation dog for shep as a fully external project, so the dogs API is exercised from outside the monorepo by a real consumer.

**Architecture:** A single binary that connects to the shepherd as an ordinary client over `$SHEP_HOME/run/shep.sock`, polls on an interval, and rotates any log file that has grown past `max_size` or aged past `max_age`. Rotation is `create`-mode throughout: rename the live file, then ask shep to reopen the original path. Compression and pruning happen after the reopen, never before, so a slow gzip cannot widen the window where shep is writing into a renamed file. Pure logic (naming, matching, ordering) is separated from I/O and from the socket, so the bulk of the behaviour is unit-testable without a daemon and the daemon-facing layer is one small trait with a test fake.

**Tech Stack:** Rust 2024, MSRV 1.88. `shep-client` (git dependency, see Deviation 1) which re-exports `shep_core`; `tokio` on the current-thread runtime; `toml` for the dog's config section; `flate2` for gzip; `jiff` for timestamps; `tempfile` for tests.

## Global Constraints

- **Edition 2024, `rust-version = "1.88"`.** Match shep exactly. A dog that needs a newer compiler than the daemon it serves is a bad example.
- **`license = "MIT OR Apache-2.0"`**, with both license files committed at the repo root.
- **`#![forbid(unsafe_code)]`** in `main.rs`. There is no reason for this binary to contain any.
- **One dependency on shep: `shep-client`.** It re-exports `shep_core` (`pub use shep_core;` at `crates/shep-client/src/lib.rs:90`), so reach core types through `shep_client::shep_core::...` and never add a second shep dependency. Verified 2026-08-20 by compiling an out-of-tree crate against the git dependency.
- **`core::error::Error`, never `std::error::Error`.** Every fallible public function carries a `# Errors` doc section. This mirrors shep's own rules (`docs/idiomatic-rust.md`, IR-1..IR-45) because this project is a worked example of shep's ecosystem.
- **No em dashes or en dashes in anything a person reads:** `--print-config` output, error messages, the README. Hyphens only.
- **Never send `Request::Flush`.** Its wire documentation is explicit: it flushes what is pending and then TRUNCATES the recorded paths. Reaching for it before a rename, on the intuition that "flush" settles buffers, deletes the lines being rotated. This project does not send it under any circumstance, and a test asserts the string `Flush` appears nowhere in `src/`.
- **Never touch `shepd.out.log` or `shepd.err.log`.** `Request::Reopen` resolves through the supervisor, which walks sheep only. The daemon's own logs have no reopen path, so renaming them would leave the daemon writing into a file nobody can find. Only paths that `ListFlock` reported are ever rotated.
- **Prune only what this dog created.** Deletion is limited to files whose names match this dog's exact generated pattern for a log path `ListFlock` reported. A file with a date in the name that does not match the exact timestamp shape is not a match. A rotator that deletes something it did not write is a far worse bug than one that leaves files behind.
- **shep's own duration and size spellings, strictness included.** Use `shep_core::values::{MemSize, UpDuration}` and their `FromStr` rather than hand-rolling a parser. A dog that accepts `10MB` while shep refuses it teaches the wrong lesson about the ecosystem it lives in.
- **`deny_unknown_fields` on the config.** A typo in `shep.toml` is reported, not ignored.
- **One cargo shape per task: `cargo test`,** plain, this crate only. There is no workspace. Run gates as their own command and read `$?` directly, never through a pipe: in zsh a pipeline's `$?` is the last command's.

## Deviations from the design, and why

Both were found while verifying the design against the real code on 2026-08-20. Neither changes what the dog does.

### Deviation 1: a git dependency, not crates.io

The design's section 1 calls for depending on the **published** `shep-client` from crates.io, on the grounds that a path dependency would paper over anything missing from the published surface.

**No shep crate is published.** The crates.io sparse index has no key for `shep`, `shep-core`, or `shep-client`, and `cargo package -p shep-client` fails with "no matching package named `shep-core` found; location searched: crates.io index" because a crate cannot be packaged before its dependencies are on the index. This is ordinary publish ordering rather than a defect: the workspace dependency table already carries versions (`shep-core = { path = "crates/shep-core", version = "0.1.0-alpha.1" }`), so publishing is mechanically ready and simply has not happened.

**Use a git dependency on the public repository** for now:

```toml
shep-client = { git = "https://github.com/TurtIeSocks/shep", branch = "main" }
```

This keeps almost all of the value. It resolves from outside the monorepo, against the same source a crates.io release would carry, with no path dependency anywhere. Verified working: a throwaway crate compiled `Client::connect`, `Request::{DogConfig, ListFlock, Reopen}`, `Response::DogSection`, `SelectorSpec::Name`, `ShepPaths::resolve`, and both `MemSize` and `UpDuration` parsers against it on a clean fetch.

What it does not test is **packaging**: `cargo package` excludes files, so a git dependency still sees files a tarball would not. Task 7 records this in the README, and the swap is one line when shep publishes.

Tracking `main` rather than pinning a revision is deliberate. This project exists to surface breakage in the external-dog contract, and a pin would hide exactly the breakage it is here to find.

### Deviation 2: one `Reopen` per affected sheep, not one for the batch

The design's section 3 step 6 says "One `Reopen` for the whole batch, naming the affected sheep." **The protocol cannot express that.** `Request::Reopen` takes a `SelectorSpec`, whose variants are `All`, `Id(u32)`, `Name(String)`, `Regex(String)`, and `Fold(String)`. There is no multi-name selector. `All` would reopen sheep this dog did not rotate, and building a `Regex` by alternation puts name escaping on the critical path of a data-safety operation.

**Send one `Reopen { selector: Name(n) }` per affected sheep,** immediately after renaming that sheep's files. This is better than the batched version it replaces: the rename-to-reopen window is per sheep rather than spanning every sheep in the tick, and a failure leaves fewer sheep in the renamed state. The design's rule that a failed reopen aborts the rest of the tick is unchanged.

## File Structure

```
Cargo.toml
LICENSE-MIT
LICENSE-APACHE
README.md
src/
  main.rs        binary entry, argument handling, the poll loop
  error.rs       one Error enum for the whole binary
  config.rs      the [dog.log-rotate] section: parsing, defaults, --print-config text
  naming.rs      pure: split a log path, generate a rotated name, match one back
  rotate.rs      disk: rename the live file (both schemes)
  prune.rs       disk: compress all but the newest generation, delete past keep
  tick.rs        one pass: the Daemon trait, the orchestration, the report
tests/
  integration.rs feature-gated tier that drives a real shepherd
```

Each file has one responsibility, and the two that carry the most subtle behaviour (`naming.rs`, `prune.rs`) are the two with no socket and no daemon in them.

---

### Task 1: Crate skeleton, dependencies, error type

**Files:**
- Create: `Cargo.toml`, `LICENSE-MIT`, `LICENSE-APACHE`, `src/main.rs`, `src/error.rs`, `rust-toolchain.toml`, `.gitignore` (append `target/`)

**Interfaces:**
- Consumes: nothing.
- Produces: `error::Error` with variants `Connect(ConnectError)`, `Request(RequestError)`, `Protocol(String)`, `Config(config::ConfigError)`, `Io { path: PathBuf, source: std::io::Error }`; `impl fmt::Display` and `impl core::error::Error` on it; `From` conversions for the three foreign types.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "shep-log-rotate"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
description = "A log-rotation dog for shep: renames grown logs, asks the shepherd to reopen, then compresses and prunes"
repository = "https://github.com/TurtIeSocks/shep-log-rotate"
license = "MIT OR Apache-2.0"
readme = "README.md"
keywords = ["shep", "process-manager", "logrotate", "logging"]
categories = ["command-line-utilities"]

[features]
# The tier that drives a REAL shepherd. Off by default because it needs a
# `shep` binary, which a fresh clone does not have. See tests/integration.rs.
integration = []

[dependencies]
shep-client = { git = "https://github.com/TurtIeSocks/shep", branch = "main" }
tokio = { version = "1", default-features = false, features = ["rt", "macros", "time", "signal"] }
toml = "0.8"
flate2 = { version = "1", default-features = false, features = ["rust_backend"] }
jiff = { version = "0.2", default-features = false, features = ["std"] }

[dev-dependencies]
tempfile = "3"

[[test]]
name = "integration"
required-features = ["integration"]
```

`flate2`'s `rust_backend` is deliberate: the default `zlib` backend wants a C toolchain, and a dog that will not cross-compile is a poor example. `jiff` with `std` only, no `tzdb` features, because timestamps are UTC (see Task 3).

- [ ] **Step 2: Write `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

Without this the project inherits whatever the machine defaults to, which on this machine has been nightly while CI tested stable.

- [ ] **Step 3: Copy both license files**

```bash
cp /Users/rin/GitHub/pm2-rs/LICENSE-MIT /Users/rin/GitHub/shep-log-rotate/LICENSE-MIT
cp /Users/rin/GitHub/pm2-rs/LICENSE-APACHE /Users/rin/GitHub/shep-log-rotate/LICENSE-APACHE
```

- [ ] **Step 4: Write the failing test for the error type**

In `src/error.rs`:

```rust
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
```

- [ ] **Step 5: Run it to see it fail**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml
```
Expected: FAIL, `Error` not defined.

- [ ] **Step 6: Implement `src/error.rs`**

```rust
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "cannot reach the shepherd: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused a request: {err}"),
            Self::Protocol(what) => write!(f, "unexpected answer from the shepherd: {what}"),
            Self::Config(err) => write!(f, "bad [dog.log-rotate] section: {err}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
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
            Self::Protocol(_) => None,
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
```

Note there is deliberately no `From<std::io::Error>`: the `Io` variant carries a path, and a blanket `From` would let a `?` drop it. Every I/O call site names its own path.

- [ ] **Step 7: Write a minimal `src/main.rs` so the crate builds**

```rust
#![forbid(unsafe_code)]

mod config;
mod error;
mod naming;
mod prune;
mod rotate;
mod tick;

fn main() {
    println!("not yet implemented");
}
```

Create empty `config.rs`, `naming.rs`, `prune.rs`, `rotate.rs`, `tick.rs` carrying only a module doc comment, so the tree compiles from Task 1 onward and each later task fills one in.

- [ ] **Step 8: Run the tests to verify they pass**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml
```
Expected: PASS, 2 tests.

- [ ] **Step 9: Commit**

```bash
git add -A && git commit -m "feat: crate skeleton, dependencies and the error type"
```

---

### Task 2: The `[dog.log-rotate]` section

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs` (`mod tests`)

**Interfaces:**
- Consumes: `error::Error` (Task 1).
- Produces:
  - `pub struct Config { pub max_size: MemSize, pub max_age: Option<UpDuration>, pub keep: usize, pub naming: Naming, pub compress: bool, pub interval: UpDuration }`
  - `pub enum Naming { Dated, Numeric }` (`Copy`, `Eq`)
  - `impl Default for Config`
  - `pub fn Config::from_toml(text: &str) -> Result<Config, ConfigError>`
  - `pub const PRINT_CONFIG: &str`
  - `pub enum ConfigError { Toml(String), Size { field: &'static str, value: String, source: ParseMemSizeError }, Duration { field: &'static str, value: String, source: ParseUpDurationError }, Naming(String) }`

**Context the implementer needs:** the daemon builds this text with `toml::to_string(table)` over the `[dog.<name>]` table (`crates/shep-daemon/src/dogs.rs:218`), so what arrives is a **bare TOML document body with no header line**: `max_size = "10M"\nkeep = 5\n`. A missing file or a missing section arrives as the empty string, which must parse to `Config::default()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(config.max_age.expect("set").as_duration().as_secs(), 7 * 86_400);
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
    fn the_printed_block_parses_back_to_the_defaults() {
        // Every line in PRINT_CONFIG is commented except the header, so what
        // survives uncommenting must be exactly what the defaults already are.
        let uncommented: String = PRINT_CONFIG
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && !line.trim().starts_with('['))
            .collect::<Vec<_>>()
            .join("\n");
        let config = Config::from_toml(&uncommented).expect("the printed block is valid");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn the_printed_block_carries_no_em_dash() {
        assert!(!PRINT_CONFIG.contains('\u{2014}'));
        assert!(!PRINT_CONFIG.contains('\u{2013}'));
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml config
```
Expected: FAIL, `Config` not defined.

- [ ] **Step 3: Implement `src/config.rs`**

Parse into a private `Raw` struct with `#[serde(deny_unknown_fields)]` where every field is `Option<String>` (or `Option<usize>` / `Option<bool>`), then convert. Parsing sizes and durations from `String` through their `FromStr` rather than through serde is what keeps shep's exact spellings and shep's exact refusals.

```rust
//! The `[dog.log-rotate]` section of `shep.toml`.
//!
//! The daemon serves this per request rather than caching it, so this dog
//! re-reads it every tick and never caches it either. Changing `max_size`
//! should not need a `shep disable` and `shep enable`.

use core::fmt;
use core::str::FromStr;

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
            max_size: parse_field(raw.max_size, "max_size", defaults.max_size)?,
            max_age: raw
                .max_age
                .map(|value| parse_one::<UpDuration>(&value, "max_age"))
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
            interval: parse_field(raw.interval, "interval", defaults.interval)?,
        })
    }
}
```

`parse_field` and `parse_one` are small generic helpers over `FromStr` whose `Err` maps into the right `ConfigError` variant; write them with a trait bound rather than duplicating the match arms.

`ConfigError` renders each variant naming the field and the offending value, per the tests above.

- [ ] **Step 4: Write `PRINT_CONFIG`**

```rust
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
# Optionally also rotate on age, whatever the size.
# shep has no day unit: a week is 168h. Unset means size only.
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
```

- [ ] **Step 5: Run the tests**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml config
```
Expected: PASS, 8 tests.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: parse the [dog.log-rotate] section, with a default for every field"
```

---

### Task 3: Naming, both schemes

**Files:**
- Modify: `src/naming.rs`
- Test: `src/naming.rs` (`mod tests`)

**Interfaces:**
- Consumes: `config::Naming` (Task 2).
- Produces:
  - `pub struct LogPath { pub dir: PathBuf, pub stem: String, pub ext: Option<String> }`, `LogPath::split(path: &Path) -> Option<LogPath>`, `LogPath::live(&self) -> PathBuf`
  - `pub enum Order { Dated { stamp: String, counter: u32 }, Numeric { n: u32 } }` (`Ord`, and see the ordering note below)
  - `pub fn stamp_utc(at: SystemTime) -> String`
  - `pub fn dated_name(base: &LogPath, stamp: &str, counter: u32) -> PathBuf`
  - `pub fn numeric_name(base: &LogPath, n: u32) -> PathBuf`
  - `pub fn match_generation(base: &LogPath, naming: Naming, file_name: &str) -> Option<(Order, bool)>` where the `bool` is "already compressed"

**This is the task the safety of pruning rests on.** `match_generation` is the only thing standing between the dog and deleting a file it did not create, so it is strict by construction: a candidate must match the exact generated shape or it is not a match.

**The rules, stated once:**

- A log path splits into `dir`, `stem`, and an optional `ext`, where `ext` is the final extension. `/var/log/web-0-out.log` splits to `("/var/log", "web-0-out", Some("log"))`. `/var/log/web.out` splits to `("/var/log", "web", Some("out"))`. A name with no dot has `ext: None`.
- **Dated:** `{stem}.{stamp}.{ext}`, or with a collision counter `{stem}.{stamp}.{counter}.{ext}`. The extension stays last so the file still matches `*.log`, still opens with log syntax highlighting, and is still found by every glob an operator already has.
- **Numeric:** `{stem}.{ext}.{n}` with `n >= 1`. Newest is `.1`, following logrotate. macOS `newsyslog` disagrees and calls the newest `.0`; shep follows logrotate, and the README says so.
- A `.gz` suffix may follow either shape and is stripped before matching.
- **The stamp is UTC**, formatted `%Y-%m-%dT%H-%M-%S`, exactly 19 characters. Local time would go backwards at the end of daylight saving, which would misorder an hour of generations once a year in a scheme whose pruning depends on that order. UTC has no such hole, and the README says the timestamps are UTC so nobody reads them as local.
- **Ordering is newest-first.** For dated that is `(stamp, counter)` descending, and the counter genuinely matters: a plain lexicographic filename sort puts `...T15-04-05.1.log` *before* `...T15-04-05.log`, because `'1' < 'l'`, which is backwards. For numeric it is `n` ascending.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Naming;
    use std::path::Path;

    fn base() -> LogPath {
        LogPath::split(Path::new("/var/log/web-0-out.log")).expect("splits")
    }

    #[test]
    fn a_log_path_splits_into_directory_stem_and_extension() {
        let split = base();
        assert_eq!(split.dir, Path::new("/var/log"));
        assert_eq!(split.stem, "web-0-out");
        assert_eq!(split.ext.as_deref(), Some("log"));
        assert_eq!(split.live(), Path::new("/var/log/web-0-out.log"));
    }

    #[test]
    fn a_log_path_without_an_extension_still_splits() {
        let split = LogPath::split(Path::new("/var/log/web-out")).expect("splits");
        assert_eq!(split.stem, "web-out");
        assert_eq!(split.ext, None);
        assert_eq!(split.live(), Path::new("/var/log/web-out"));
    }

    #[test]
    fn the_dated_name_keeps_the_extension_last() {
        let made = dated_name(&base(), "2026-08-20T15-04-05", 0);
        assert_eq!(made, Path::new("/var/log/web-0-out.2026-08-20T15-04-05.log"));
    }

    #[test]
    fn a_same_second_collision_appends_a_counter_before_the_extension() {
        let made = dated_name(&base(), "2026-08-20T15-04-05", 1);
        assert_eq!(made, Path::new("/var/log/web-0-out.2026-08-20T15-04-05.1.log"));
    }

    #[test]
    fn the_numeric_name_appends_after_the_extension() {
        assert_eq!(numeric_name(&base(), 1), Path::new("/var/log/web-0-out.log.1"));
    }

    #[test]
    fn a_generated_dated_name_matches_itself_back() {
        for counter in [0, 1, 42] {
            let made = dated_name(&base(), "2026-08-20T15-04-05", counter);
            let name = made.file_name().expect("has a name").to_str().expect("utf8");
            let (order, compressed) =
                match_generation(&base(), Naming::Dated, name).expect("matches");
            assert!(!compressed);
            assert_eq!(
                order,
                Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter }
            );
        }
    }

    #[test]
    fn a_gz_suffix_matches_and_is_reported() {
        let (order, compressed) = match_generation(
            &base(),
            Naming::Dated,
            "web-0-out.2026-08-20T15-04-05.log.gz",
        )
        .expect("matches");
        assert!(compressed);
        assert_eq!(order, Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter: 0 });
    }

    #[test]
    fn near_misses_are_not_matches() {
        // Each of these has a date in the name, or nearly the right shape.
        // None of them was written by this dog, so none may ever be deleted.
        let decoys = [
            "web-0-out.2026-08-20.log",              // no time
            "web-0-out.2026-08-20T15-04.log",        // no seconds
            "web-0-out.2026-8-20T15-04-05.log",      // not zero padded
            "web-0-out.backup-2026-08-20T15-04-05.log", // prefixed
            "web-0-out.2026-08-20T15-04-05.log.bak", // wrong trailing suffix
            "web-0-out.2026-08-20T15-04-05",         // extension dropped
            "web-1-out.2026-08-20T15-04-05.log",     // a different sheep
            "web-0-err.2026-08-20T15-04-05.log",     // the other stream
            "web-0-out.log",                         // the live file itself
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Dated, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn numeric_near_misses_are_not_matches() {
        let decoys = [
            "web-0-out.log.0",   // logrotate starts at 1; .0 is newsyslog's
            "web-0-out.log.x",   // not a number
            "web-0-out.log",     // the live file itself
            "web-0-out.1.log",   // dated-scheme shape
            "web-1-out.log.1",   // a different sheep
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Numeric, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn a_scheme_does_not_match_the_other_schemes_files() {
        assert!(match_generation(&base(), Naming::Numeric,
            "web-0-out.2026-08-20T15-04-05.log").is_none());
        assert!(match_generation(&base(), Naming::Dated,
            "web-0-out.log.1").is_none());
    }

    #[test]
    fn newest_first_ordering_puts_the_counter_after_the_plain_name() {
        // A plain lexicographic sort gets this backwards, because '1' < 'l'.
        let mut orders = vec![
            Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter: 0 },
            Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter: 2 },
            Order::Dated { stamp: "2026-08-20T15-04-06".into(), counter: 0 },
        ];
        orders.sort_by(Order::newest_first);
        assert_eq!(orders[0], Order::Dated { stamp: "2026-08-20T15-04-06".into(), counter: 0 });
        assert_eq!(orders[1], Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter: 2 });
        assert_eq!(orders[2], Order::Dated { stamp: "2026-08-20T15-04-05".into(), counter: 0 });
    }

    #[test]
    fn numeric_newest_first_is_one_before_two() {
        let mut orders = vec![Order::Numeric { n: 3 }, Order::Numeric { n: 1 }, Order::Numeric { n: 2 }];
        orders.sort_by(Order::newest_first);
        assert_eq!(orders, vec![Order::Numeric { n: 1 }, Order::Numeric { n: 2 }, Order::Numeric { n: 3 }]);
    }

    #[test]
    fn the_stamp_is_utc_and_nineteen_characters() {
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);
        let stamp = stamp_utc(at);
        assert_eq!(stamp, "2026-08-21T15-04-05", "the stamp is UTC, not local");
        assert_eq!(stamp.len(), 19);
        assert!(!stamp.contains(':'), "colons are not portable in filenames: {stamp}");
    }
}
```

The expected value is pinned rather than left as a shape assertion, and it is UTC: `date -u -r 1787324645` agrees. If `jiff` produces a local-time value here, the implementation used the wrong clock and the test is doing its job.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml naming
```
Expected: FAIL, nothing defined.

- [ ] **Step 3: Implement `src/naming.rs`**

`match_generation` works by stripping, never by searching:

1. Strip a trailing `.gz`, recording whether it was there.
2. For **dated**: strip the trailing `.{ext}` if `base.ext` is `Some`, and fail if it is not there. Then require the remainder to begin with `{stem}.`. What is left is either a 19-character stamp, or a stamp then `.` then digits. Validate the stamp position by position: `dddd-dd-ddTdd-dd-dd`. Anything else is not a match.
3. For **numeric**: require the remainder to be exactly `{stem}.{ext}.` followed by digits parsing to `n >= 1` (or `{stem}.` followed by digits when `base.ext` is `None`).

`Order::newest_first(a, b) -> core::cmp::Ordering` is an associated function so it reads as its own name at the call site rather than as a bare `sort_by` closure.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml naming
```
Expected: PASS, 13 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: name and match rotated generations in both schemes"
```

---

### Task 4: Rotating one file on disk

**Files:**
- Modify: `src/rotate.rs`
- Test: `src/rotate.rs` (`mod tests`), using `tempfile::tempdir`

**Interfaces:**
- Consumes: `naming::{LogPath, Order, dated_name, numeric_name, match_generation, stamp_utc}` (Task 3), `config::Naming` (Task 2), `error::Error` (Task 1).
- Produces:
  - `pub fn generations(base: &LogPath, naming: Naming) -> Result<Vec<(PathBuf, Order, bool)>, Error>` - every generation this dog created for `base`, newest first.
  - `pub fn rotate(base: &LogPath, naming: Naming, now: SystemTime) -> Result<PathBuf, Error>` - renames the live file and returns the path it now has.

`rotate` does the rename and nothing else. It does not reopen, it does not compress, and it does not prune: those belong to the caller, in that order, and keeping them out of here is what makes the rename-to-reopen window as short as it can be.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Naming;
    use std::fs;

    fn seed(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("seeded");
        path
    }

    #[test]
    fn dated_rotation_renames_the_live_file_and_leaves_the_path_free() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = seed(dir.path(), "web-0-out.log", "one\ntwo\n");
        let base = LogPath::split(&live).expect("splits");
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);

        let rotated = rotate(&base, Naming::Dated, at).expect("rotates");

        assert!(!live.exists(), "the live path must be free for shep to reopen");
        assert_eq!(fs::read_to_string(&rotated).expect("read"), "one\ntwo\n");
        assert!(
            rotated.file_name().expect("name").to_str().expect("utf8").ends_with(".log"),
            "the extension stays last: {}",
            rotated.display()
        );
    }

    #[test]
    fn a_second_rotation_in_the_same_second_gets_a_counter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = seed(dir.path(), "web-0-out.log", "first\n");
        let base = LogPath::split(&live).expect("splits");
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);

        let first = rotate(&base, Naming::Dated, at).expect("rotates");
        seed(dir.path(), "web-0-out.log", "second\n");
        let second = rotate(&base, Naming::Dated, at).expect("rotates again");

        assert_ne!(first, second, "the first generation must not be overwritten");
        assert_eq!(fs::read_to_string(&first).expect("read"), "first\n");
        assert_eq!(fs::read_to_string(&second).expect("read"), "second\n");
    }

    #[test]
    fn numeric_rotation_shifts_oldest_first_so_nothing_is_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.2", "gen2\n");
        seed(dir.path(), "web-0-out.log.1", "gen1\n");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect("rotates");

        assert!(!live.exists());
        assert_eq!(fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"), "live\n");
        assert_eq!(fs::read_to_string(dir.path().join("web-0-out.log.2")).expect("read"), "gen1\n");
        assert_eq!(fs::read_to_string(dir.path().join("web-0-out.log.3")).expect("read"), "gen2\n");
    }

    #[test]
    fn the_numeric_shift_carries_the_gz_suffix_along() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.1.gz", "compressed\n");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect("rotates");

        assert!(dir.path().join("web-0-out.log.2.gz").exists(), "a compressed generation keeps its suffix");
        assert!(!dir.path().join("web-0-out.log.1.gz").exists());
        assert_eq!(fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"), "live\n");
    }

    #[test]
    fn the_shift_never_touches_a_file_this_dog_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.0", "newsyslog wrote this\n");
        seed(dir.path(), "notes-about-web-0-out.log.1", "an operator wrote this\n");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect("rotates");

        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.0")).expect("read"),
            "newsyslog wrote this\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("notes-about-web-0-out.log.1")).expect("read"),
            "an operator wrote this\n"
        );
    }

    #[test]
    fn generations_come_back_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.2026-08-20T15-04-05.log", "older\n");
        seed(dir.path(), "web-0-out.2026-08-20T15-04-06.log", "newer\n");
        seed(dir.path(), "web-0-out.log", "live\n");
        seed(dir.path(), "unrelated.txt", "not ours\n");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let found = generations(&base, Naming::Dated).expect("listed");

        assert_eq!(found.len(), 2, "the live file and the decoy are not generations");
        assert!(found[0].0.to_str().expect("utf8").contains("15-04-06"));
        assert!(found[1].0.to_str().expect("utf8").contains("15-04-05"));
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_failing() {
        let base = LogPath::split(std::path::Path::new("/nonexistent/web-0-out.log")).expect("splits");
        assert!(generations(&base, Naming::Dated).expect("no error").is_empty());
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml rotate
```
Expected: FAIL.

- [ ] **Step 3: Implement `src/rotate.rs`**

`rotate` for dated: compute `stamp_utc(now)`, then find the first free counter starting at 0 by testing existence, then `fs::rename`.

`rotate` for numeric: list generations, sort by `n` **descending** so the shift starts at the oldest and nothing is overwritten, rename each `n` to `n + 1` preserving the `.gz` suffix, then rename the live file to `.1`.

Every `fs` call maps its error through `Error::Io { path, source }` naming the path it was working on. A `read_dir` on a directory that does not exist returns an empty list rather than an error: a sheep registered but never started has no log directory yet, and that is normal rather than broken.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml rotate
```
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: rotate one log file, in both naming schemes"
```

---

### Task 5: Compress and prune

**Files:**
- Modify: `src/prune.rs`
- Test: `src/prune.rs` (`mod tests`)

**Interfaces:**
- Consumes: `rotate::generations` (Task 4), `naming::{LogPath, Order}` (Task 3), `config::{Config, Naming}` (Task 2).
- Produces: `pub fn tidy(base: &LogPath, config: &Config) -> Result<Tidied, Error>`, `pub struct Tidied { pub compressed: usize, pub deleted: usize }`

`tidy` runs after the reopen, never before, so a slow gzip cannot widen the rename-to-reopen window.

Order of operations inside `tidy`: list generations newest-first, compress every generation except index 0 when `config.compress` is on, then delete everything past `config.keep`. Compressing before pruning is deliberate for numeric: pruning first would leave the compressor working on a shorter list, which is the same answer with more steps. Both orders are correct, and this one keeps the "newest stays plain" rule readable.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Naming};
    use std::fs;

    fn config(naming: Naming, keep: usize, compress: bool) -> Config {
        Config { naming, keep, compress, ..Config::default() }
    }

    #[test]
    fn the_newest_generation_stays_plain_so_it_is_greppable() {
        let dir = tempfile::tempdir().expect("tempdir");
        for stamp in ["15-04-05", "15-04-06", "15-04-07"] {
            fs::write(dir.path().join(format!("web-0-out.2026-08-20T{stamp}.log")), "body\n")
                .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied = tidy(&base, &config(Naming::Dated, 5, true)).expect("tidied");

        assert_eq!(tidied.compressed, 2);
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-07.log").exists(), "newest stays plain");
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-06.log.gz").exists());
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz").exists());
        assert!(!dir.path().join("web-0-out.2026-08-20T15-04-05.log").exists(), "the plain copy goes");
    }

    #[test]
    fn compression_round_trips_the_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "line one\nline two\nline three\n";
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-05.log"), body).expect("seeded");
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-06.log"), "newest\n").expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 5, true)).expect("tidied");

        let gz = fs::File::open(dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz")).expect("open");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(gz), &mut out).expect("decode");
        assert_eq!(out, body, "a rotator that mangles a log is worse than no rotator");
    }

    #[test]
    fn keep_bounds_the_generations_and_deletes_the_oldest() {
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=6 {
            fs::write(
                dir.path().join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                format!("gen{second}\n"),
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied = tidy(&base, &config(Naming::Dated, 2, false)).expect("tidied");

        assert_eq!(tidied.deleted, 4);
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-06.log").exists());
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-05.log").exists());
        assert!(!dir.path().join("web-0-out.2026-08-20T15-04-04.log").exists());
        assert!(dir.path().join("web-0-out.log").exists(), "the live file is never a generation");
    }

    #[test]
    fn pruning_never_deletes_a_file_this_dog_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=6 {
            fs::write(dir.path().join(format!("web-0-out.2026-08-20T15-04-0{second}.log")), "ours\n")
                .expect("seeded");
        }
        // Every one of these is a plausible near miss. None was written here.
        let decoys = [
            "web-0-out.2026-08-20.log",
            "web-0-out.2026-08-20T15-04-05.log.bak",
            "web-0-out-2026-08-20T15-04-05.log",
            "web-1-out.2026-08-20T15-04-05.log",
            "web-0-out.log.1",
            "important.log",
        ];
        for decoy in decoys {
            fs::write(dir.path().join(decoy), "not ours\n").expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 1, true)).expect("tidied");

        for decoy in decoys {
            assert!(dir.path().join(decoy).exists(), "{decoy} was deleted and must not have been");
        }
    }

    #[test]
    fn numeric_pruning_deletes_above_keep() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=6 {
            fs::write(dir.path().join(format!("web-0-out.log.{n}")), format!("gen{n}\n"))
                .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Numeric, 3, false)).expect("tidied");

        assert!(dir.path().join("web-0-out.log.3").exists());
        assert!(!dir.path().join("web-0-out.log.4").exists());
        assert!(!dir.path().join("web-0-out.log.6").exists());
    }

    #[test]
    fn switching_scheme_leaves_the_other_schemes_files_alone() {
        // Documented behaviour, not an accident: they stop being pruned and
        // are left for the operator. Deleting files a previous configuration
        // created is not this dog's call to make.
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=9 {
            fs::write(dir.path().join(format!("web-0-out.log.{n}")), "old scheme\n").expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 1, true)).expect("tidied");

        for n in 1..=9 {
            assert!(dir.path().join(format!("web-0-out.log.{n}")).exists(), "left for the operator");
        }
    }

    #[test]
    fn an_already_compressed_generation_is_not_compressed_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz"), "already\n").expect("seeded");
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-06.log"), "newest\n").expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied = tidy(&base, &config(Naming::Dated, 5, true)).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(!dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz.gz").exists());
    }

    #[test]
    fn compression_off_leaves_everything_plain() {
        let dir = tempfile::tempdir().expect("tempdir");
        for stamp in ["15-04-05", "15-04-06"] {
            fs::write(dir.path().join(format!("web-0-out.2026-08-20T{stamp}.log")), "body\n")
                .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied = tidy(&base, &config(Naming::Dated, 5, false)).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(dir.path().join("web-0-out.2026-08-20T15-04-05.log").exists());
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml prune
```
Expected: FAIL.

- [ ] **Step 3: Implement `src/prune.rs`**

Compression writes `{path}.gz` with `flate2::write::GzEncoder`, then removes the plain file, and only in that order: a crash between the two leaves both, which is recoverable, while removing first would lose the log.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml prune
```
Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: compress all but the newest generation, and prune past keep"
```

---

### Task 6: One tick

**Files:**
- Modify: `src/tick.rs`
- Test: `src/tick.rs` (`mod tests`), with a hand-rolled fake `Daemon`

**Interfaces:**
- Consumes: everything from Tasks 2 to 5.
- Produces:
  - `pub trait Daemon { async fn dog_config(&self, name: &str) -> Result<String, Error>; async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error>; async fn reopen(&self, name: &str) -> Result<(), Error>; }`
  - `pub struct Live(shep_client::Client)` implementing it
  - `pub async fn tick<D: Daemon>(daemon: &D, dog_name: &str, now: SystemTime) -> Result<(Config, Report), Error>`
  - `pub struct Report { pub rotated: usize, pub skipped: usize, pub compressed: usize, pub deleted: usize, pub reopen_failed: Option<String> }`

`async fn` in a trait is stable and used with generics only, never behind `dyn`. There is one real implementation and one test fake, both statically dispatched.

`tick` returns the `Config` it read as well as the report, because the caller needs `interval` to decide how long to sleep and re-reading it separately would be a second request for a value already in hand.

**The order inside one tick, and why:**

1. `dog_config(dog_name)`, parsed into a `Config`. Never cached.
2. `list_flock()`.
3. For each sheep, for each of `out_file` and `err_file` that is `Some`: `metadata()` the path. A path that cannot be stat'd is counted as skipped, not an error. A sheep registered but never started has no log file yet, and that is normal rather than broken.
4. Qualify: `len() >= max_size`, or `max_age` is set and the file's `modified()` is older than it.
5. **Group the qualifying files by sheep name.** For each sheep, rename all of its qualifying files, then send `reopen(name)` for that sheep alone (Deviation 2).
6. **If a reopen fails, stop rotating for this tick.** shep is still writing into the renamed file through its existing handle, so nothing is lost and the situation self-corrects on the next successful reopen. Record it in `reopen_failed` and return; do not rotate the remaining sheep.
7. After all reopens, `tidy` every base that was rotated.

**Two sheep can share one log path** (`merge_logs`, or an explicit `out_file` on a multi-instance app). Deduplicate paths before rotating, or the second sheep's rename will fail on a file the first already moved. Reopen every sheep that named a rotated path, not just the first.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use shep_client::shep_core::protocol::ProcessInfo;
    use shep_client::shep_core::status::ProcStatus;
    use std::cell::RefCell;
    use std::fs;

    /// A daemon that answers from a script and records what it was asked.
    struct Fake {
        config: String,
        flock: Vec<ProcessInfo>,
        reopen_fails: Option<String>,
        reopened: RefCell<Vec<String>>,
    }

    impl Daemon for Fake {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            Ok(self.config.clone())
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Ok(self.flock.clone())
        }
        async fn reopen(&self, name: &str) -> Result<(), Error> {
            self.reopened.borrow_mut().push(name.to_owned());
            match &self.reopen_fails {
                Some(why) if why == name => Err(Error::Protocol(format!("refused for {name}"))),
                _ => Ok(()),
            }
        }
    }

    // `ProcessInfo` is `#[non_exhaustive]`, so an outside crate CANNOT write
    // it as a struct literal. The builder is the only way in, and finding
    // that out is exactly the kind of thing this project exists to find out.
    fn sheep(
        name: &str,
        out: Option<&std::path::Path>,
        err: Option<&std::path::Path>,
    ) -> ProcessInfo {
        ProcessInfo::builder(0, name, ProcStatus::Online)
            .out_file(out.map(|path| path.display().to_string()))
            .err_file(err.map(|path| path.display().to_string()))
            .build()
    }

    #[tokio::test]
    async fn a_file_over_max_size_is_rotated_and_its_sheep_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        let fake = Fake {
            config: "max_size = \"1K\"\n".into(),
            flock: vec![sheep("web", Some(&out), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };

        let (_config, report) = tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect("ticked");

        assert_eq!(report.rotated, 1);
        assert!(!out.exists(), "the live path is free for shep to reopen");
        assert_eq!(*fake.reopened.borrow(), vec!["web".to_owned()]);
    }

    #[tokio::test]
    async fn a_file_under_max_size_is_left_alone_and_nothing_is_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "small\n").expect("seeded");
        let fake = Fake {
            config: "max_size = \"1M\"\n".into(),
            flock: vec![sheep("web", Some(&out), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };

        let (_config, report) = tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect("ticked");

        assert_eq!(report.rotated, 0);
        assert!(out.exists());
        assert!(fake.reopened.borrow().is_empty(), "no rotation means no reopen");
    }

    #[tokio::test]
    async fn a_sheep_with_no_log_file_yet_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake {
            config: String::new(),
            flock: vec![sheep("web", Some(&dir.path().join("never-started.log")), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };

        let (_config, report) = tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect("a missing log file is ordinary");

        assert_eq!(report.skipped, 1);
        assert_eq!(report.rotated, 0);
    }

    #[tokio::test]
    async fn a_refused_reopen_stops_the_tick_rather_than_rotating_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("api-0-out.log");
        let second = dir.path().join("web-0-out.log");
        fs::write(&first, "x".repeat(2048)).expect("seeded");
        fs::write(&second, "x".repeat(2048)).expect("seeded");
        let fake = Fake {
            config: "max_size = \"1K\"\n".into(),
            flock: vec![sheep("api", Some(&first), None), sheep("web", Some(&second), None)],
            reopen_fails: Some("api".into()),
            reopened: RefCell::new(Vec::new()),
        };

        let (_config, report) = tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect("a refused reopen is reported, not returned as an error");

        assert!(report.reopen_failed.is_some(), "the failure is reported");
        assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()], "web was never reached");
        assert!(second.exists(), "rotating on through a broken reopen multiplies a recoverable state");
    }

    #[tokio::test]
    async fn two_sheep_sharing_one_log_path_rotate_it_once_and_both_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let fake = Fake {
            config: "max_size = \"1K\"\n".into(),
            flock: vec![sheep("web-0", Some(&shared), None), sheep("web-1", Some(&shared), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };

        let (_config, report) = tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect("ticked");

        assert_eq!(report.rotated, 1, "one path, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(reopened.contains(&"web-1".to_owned()));
    }

    #[tokio::test]
    async fn max_age_rotates_a_small_but_old_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "tiny\n").expect("seeded");
        let fake = Fake {
            config: "max_size = \"1G\"\nmax_age = \"1s\"\n".into(),
            flock: vec![sheep("web", Some(&out), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };
        // `now` a year on, so the file's real mtime is unambiguously older.
        let far_future = std::time::SystemTime::now() + core::time::Duration::from_secs(31_536_000);

        let (_config, report) = tick(&fake, "log-rotate", far_future).await.expect("ticked");

        assert_eq!(report.rotated, 1);
    }

    #[tokio::test]
    async fn a_bad_config_section_fails_the_tick_without_touching_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(4096)).expect("seeded");
        let fake = Fake {
            config: "max_size = \"10MB\"\n".into(),
            flock: vec![sheep("web", Some(&out), None)],
            reopen_fails: None,
            reopened: RefCell::new(Vec::new()),
        };

        tick(&fake, "log-rotate", std::time::SystemTime::now())
            .await
            .expect_err("10MB is not a spelling shep accepts");

        assert!(out.exists(), "a config this dog cannot read means it touches nothing");
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml tick
```
Expected: FAIL.

- [ ] **Step 3: Implement `src/tick.rs`**

`Live`'s three methods each send one request and match one response, turning any other response into `Error::Protocol` naming what came back. `Request::DogConfig { name }` answers `Response::DogSection { toml }`, and `toml.as_str()` is the text. `Request::ListFlock` answers `Response::Flock(Vec<ProcessInfo>)`. `Request::Reopen { selector: SelectorSpec::Name(name) }` answers `Response::Reopened(_)`.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml tick
```
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: one rotation pass, with per-sheep reopen and abort on refusal"
```

---

### Task 7: The binary, the README, and the real-shepherd tier

**Files:**
- Modify: `src/main.rs`, `README.md`
- Create: `tests/integration.rs`

**Interfaces:**
- Consumes: everything.
- Produces: the binary.

- [ ] **Step 1: Write the failing tests for the argument surface**

In `src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_config_is_the_only_argument() {
        assert_eq!(Action::parse(["--print-config"]), Ok(Action::PrintConfig));
        assert_eq!(Action::parse([]), Ok(Action::Run));
        assert!(Action::parse(["--rotate-now"]).is_err());
    }

    #[test]
    fn the_usage_text_carries_no_em_dash() {
        let usage = Action::parse(["--nonsense"]).expect_err("refused").to_string();
        assert!(!usage.contains('\u{2014}'));
        assert!(!usage.contains('\u{2013}'));
    }

    #[test]
    fn this_dog_never_sends_flush() {
        // Flush truncates the recorded paths. "Flush before rotating" is the
        // natural instinct and it deletes the lines being rotated. Nothing in
        // this crate may reach for it, and this test is the tripwire.
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read");
            for (number, line) in source.lines().enumerate() {
                assert!(
                    !line.contains("Request::Flush"),
                    "{}:{}: Flush truncates the log it is asked to settle",
                    path.display(),
                    number + 1
                );
            }
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml
```
Expected: FAIL, `Action` not defined.

- [ ] **Step 3: Implement `main`**

No `clap`: one optional flag does not earn a dependency, and this binary's argument surface is deliberately closed. `Action::parse` takes an iterator of arguments so it is testable without a process.

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode { /* ... */ }
```

The loop:

1. Resolve `ShepPaths::resolve(&|key| std::env::var(key).ok(), &home_dir)` to find the socket. `$SHEP_HOME` is the only thing a dog inherits, and `resolve` is what reads it.
2. `Client::connect(&paths.socket)`.
3. Loop: `tick`, log the report, then `tokio::select!` on `tokio::time::sleep(config.interval.as_duration())` and `tokio::signal::ctrl_c()`.
4. **A failed tick is logged and retried on the next interval, not fatal.** The shepherd restarting underneath a dog is ordinary; exiting would make the supervisor restart this process for a condition that resolves itself. Reconnect if the error is `Connect` or `Request`.
5. **No signal handler beyond ctrl_c for a clean exit.** The shepherd owns this process's signals and its kill ladder. `SIGHUP`-triggered rotation is a shape people expect, and adding it here would be arguing with the supervisor.

**Discovering this dog's own name, which is harder than it looks and matters more than it looks.**

Checked so you do not have to: **shep exports no such variable.** An adopted dog is spawned with `DogSource::Adopted { path } => (path.clone(), Vec::new())` and exactly one environment entry, `config.env.insert("SHEP_HOME", ...)` (`crates/shep-daemon/src/dogs.rs`). No argv, one variable. The comment there explains why, and the reasoning is sound: "an argv shep invented for it is one more thing it has to agree with before it can start."

But the name is the one thing this dog needs, because it is the `[dog.<name>]` key its config lives under. **The failure is silent.** `Request::DogConfig { name }` for a name nobody adopted returns the **empty string**, which is exactly what a real dog with no config section returns. Adopt this binary as `logrotate` instead of `log-rotate` and every setting in the operator's `shep.toml` is ignored, with no error printed anywhere, by either side.

So find the name rather than assuming it. This process knows its own pid, and `ListFlock` reports a pid per entry:

```rust
/// The name this dog was adopted under, found by looking for ourselves in
/// the flock.
///
/// A dog is given no argv and one environment variable, so it cannot be
/// TOLD its own name. It can still work it out: this process knows its pid,
/// and the flock entry that is a dog and carries that pid is this process.
///
/// `None` means we could not identify ourselves, which the caller reports
/// out loud before falling back. It must never fail silently: a wrong name
/// means the operator's whole configuration section is ignored and every
/// default is used instead, which looks exactly like working.
async fn adopted_name<D: Daemon>(daemon: &D) -> Option<String> {
    let me = std::process::id();
    daemon
        .list_flock()
        .await
        .ok()?
        .into_iter()
        .find(|info| info.dog.is_some() && info.pid == Some(me))
        .map(|info| info.name)
}
```

Fall back to `log-rotate` when it returns `None`, and print a line to stderr saying the dog could not identify itself in the flock and is assuming that name. A dog that cannot find itself still works; a dog that silently reads the wrong config section does not.

**This adds a `Daemon` method requirement:** `adopted_name` uses `list_flock`, which the trait already has. No new method.

**Prove it in the integration tier, because the pid assumption is exactly the kind of thing that is true until it is not** (an interpreter mapping, a wrapper, or a future exec would all break it). Adopt the dog under a NON-default name, give it a `[dog.<that name>]` section with a distinctive `max_size`, and assert the dog rotates at that size rather than at the 10M default. That test fails loudly if pid discovery ever stops working, which is the only way anyone would find out.

- [ ] **Step 4: Write `tests/integration.rs`**

Feature-gated behind `integration`, needing `$SHEP_BIN` to point at a built `shep`. The tests:

- **No log line is lost across a rotation.** Boot a shepherd in a temporary `$SHEP_HOME`, start a sheep that prints a monotonically increasing counter as fast as it can, set `max_size` very low to force several rotations, stop everything, concatenate every generation in order, and assert the counter has no gaps. **Run it for both schemes**: the rename counts differ, so the rename-to-reopen window differs, and the scheme with more renames is the one more likely to expose a race.
- **The adoption path end to end**: `shep adopt`, confirm it appears in `shep dogs`, `shep rehome`, confirm it is gone. That is the external-dog contract itself under test, which is the reason this project exists.

Add the silent-skip tripwire, borrowed from shep-client's own hard-won lesson (`crates/shep-client/Cargo.toml`): cargo skips a target whose required features are off in **silence**, so a bare `cargo test` reports a fraction of this crate's cases and reads as the whole. In `src/main.rs`:

```rust
/// Compiles only when the `integration` feature is OFF, so a plain
/// `cargo test` says out loud that the real-shepherd tier did not run.
/// Cargo skips a target whose required features are off in silence, and
/// nobody measuring coverage opens Cargo.toml first.
#[cfg(all(test, not(feature = "integration")))]
#[test]
fn heads_up_the_real_shepherd_tier_is_not_running() {
    println!(
        "tests/integration.rs did NOT run: it needs --features integration and $SHEP_BIN \
         pointing at a built shep binary."
    );
}
```

- [ ] **Step 5: Run both tiers**

```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml
```
Expected: PASS, and the heads-up test's output visible with `-- --nocapture`.

```bash
SHEP_BIN=/Users/rin/GitHub/pm2-rs/target/release/shep cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml --features integration
```
Expected: PASS. Build the binary first with `cargo build --release --manifest-path /Users/rin/GitHub/pm2-rs/Cargo.toml -p shep`.

**Never point `$SHEP_HOME` at `~/.shep`.** A live shepherd runs there supervising one of Rin's real services. Every integration test uses a `tempfile::tempdir` as its `$SHEP_HOME`, and a test that reads the real one is a defect.

- [ ] **Step 6: Write the README**

It is public-facing prose, so run the `humanizer` skill over it before committing. It must cover: what the dog does; `cargo install --git` and `shep adopt log-rotate <path>`; `--print-config`; every option and its default; **the two naming schemes and the trade-off between them**, including that timestamps are UTC and that `.1` is the newest in the numeric scheme while macOS `newsyslog` calls it `.0`; that switching `naming` leaves the old scheme's files unpruned for the operator; that the daemon's own logs are deliberately not rotated and why; that the window between rename and reopen is real, that lines written in it land in the previous generation rather than vanishing, and that this is the honest outcome; and Deviation 1, that the shep dependency is a git dependency until shep publishes.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: the binary, the real-shepherd test tier, and the README"
```

---

## Final verification

```bash
cargo fmt --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml --check
```
```bash
cargo clippy --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml --all-targets -- -D warnings
```
```bash
cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml
```
```bash
SHEP_BIN=/Users/rin/GitHub/pm2-rs/target/release/shep cargo test --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml --features integration
```
```bash
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path /Users/rin/GitHub/shep-log-rotate/Cargo.toml --no-deps
```

Each as its own command with `$?` read directly, never through a pipe.

**The one gate that matters** is the integration tier. Every unit test in this crate can pass while the dog loses log lines under load, because only a real shepherd writing into a real descriptor can prove it does not.

## Carried back to shep

Three findings from this exercise, already recorded in `docs/specs/deferred.md` or worth adding:

1. **`Flush`'s name invites a destructive mistake.** "Flush the logs before rotating" is the natural instinct and it truncates them. The wire documentation is accurate; the name is the trap. Worth a sentence in `docs/dogs.md` warning a dog author off it, since a rotator is the most likely thing to reach for it.
2. **The daemon's own logs cannot be rotated by anything.** No reopen path exists for `shepd.out.log` or `shepd.err.log`. On a long-running shepherd they grow without bound.
3. **`docs/dogs.md` documents the wire but not a worked external dog.** This project can become the example it points at.
4. **A dog cannot learn the name it was adopted under, and getting it wrong fails silently.** An adopted dog receives no argv and one environment variable, `SHEP_HOME`. Its name is the `[dog.<name>]` key its own configuration lives under, and `DogConfig` for a name nobody adopted returns the empty string, which is indistinguishable from a real dog with no section. So adopting a dog under a name its author did not expect silently discards the operator's entire configuration for it. This dog works around it by finding its own pid in `ListFlock`, which works but is a workaround every dog author would have to reinvent. Worth a sentence in `docs/dogs.md` at minimum, and worth considering whether `dog_app` should pass the name after all, or whether `DogConfig` should distinguish "no such dog" from "no section".
