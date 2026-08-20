//! `shep-log-rotate`: a log-rotation dog for shep.
//!
//! One process, one poll loop. Every interval it asks the shepherd for its
//! own `[dog.<name>]` section and for the flock, renames the log files that
//! have grown or aged past what the section allows, asks the shepherd to
//! reopen them, and then compresses and prunes what it rotated. All of that
//! lives in [`tick`](crate::tick::tick); this file is the process around it.
//!
//! # How it learns its own name
//!
//! The name matters because it is the `[dog.<name>]` key the configuration
//! lives under, and getting it wrong is silent: the daemon answers
//! `DogConfig` for a name nobody adopted with an empty section, which is
//! byte for byte what a dog running on its defaults gets. Adopt this binary
//! as `logrotate` when it asks for `log-rotate` and every setting in the
//! operator's `shep.toml` is discarded without either side saying so.
//!
//! shep exports no name for a dog to read. An adopted dog is spawned with
//! no arguments at all and exactly one environment entry, `SHEP_HOME`, so
//! there is nothing in the process's own environment to read the name out
//! of. What there is instead is the flock listing: it reports a pid per
//! entry and a marker per dog, and this process knows its own pid. See
//! [`adopted_name`].

#![forbid(unsafe_code)]

mod config;
mod error;
mod naming;
mod prune;
mod rotate;
mod tick;

use core::fmt;
use std::{path::PathBuf, process::ExitCode, time::SystemTime};

use shep_client::{Client, shep_core::paths::ShepPaths, shep_core::values::UpDuration};

use crate::{
    config::{Config, PRINT_CONFIG},
    error::Error,
    tick::{Daemon, Live, tick},
};

/// The name to assume when the flock listing cannot say what this dog was
/// adopted as. Documented in the README, so an operator following it lands
/// on the name this constant already expects.
const DEFAULT_NAME: &str = "log-rotate";

/// Everything this binary accepts, printed when it is handed anything else.
///
/// No em dashes and no en dashes: a terminal that cannot render one prints a
/// replacement character in the middle of the one message that exists to be
/// read by somebody who is already confused.
const USAGE: &str = "\
shep-log-rotate: a log-rotation dog for shep.

Usage:
  shep-log-rotate                 Run the poll loop. This is what the
                                  shepherd runs after `shep adopt`.
  shep-log-rotate --print-config  Print a commented [dog.log-rotate] block
                                  naming every option and its default, then
                                  exit.

Settings are read from `shep.toml` over the shepherd's own socket, never
from this process's arguments or environment. The socket comes from
$SHEP_HOME, which the shepherd sets when it spawns this dog.";

/// What this process was asked to do.
///
/// One flag, so no `clap`: a dependency that parses one argument would be
/// larger than the whole of this binary's argument surface, and that surface
/// is deliberately closed. Everything configurable is configured in
/// `shep.toml`, where the shepherd can serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the poll loop until the shepherd stops this process.
    Run,
    /// Print [`PRINT_CONFIG`] and exit.
    PrintConfig,
}

/// An argument this binary does not accept.
///
/// Carries the whole answer, [`USAGE`] included, so a caller prints one
/// thing and is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage(String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n\n{USAGE}", self.0)
    }
}

impl core::error::Error for Usage {}

impl Action {
    /// Read the arguments, which do not include the program name.
    ///
    /// Takes an iterator rather than reading [`std::env::args`] itself, so
    /// the whole argument surface is testable without a process.
    ///
    /// # Errors
    /// [`Usage`] for any argument other than a single `--print-config`.
    /// Unknown flags are refused rather than ignored: a rotator silently
    /// ignoring `--dry-run` would rotate for real.
    pub fn parse<'a, I: IntoIterator<Item = &'a str>>(args: I) -> Result<Self, Usage> {
        let mut action = Self::Run;
        for arg in args {
            match arg {
                "--print-config" if action == Self::Run => action = Self::PrintConfig,
                "--help" | "-h" => {
                    return Err(Usage("shep-log-rotate takes no options.".to_owned()));
                }
                other => {
                    return Err(Usage(format!(
                        "shep-log-rotate does not understand {other}."
                    )));
                }
            }
        }
        Ok(action)
    }
}

/// The name this process was adopted under, or `None` when the flock listing
/// does not name it.
///
/// The pid is the whole of the identification. It is sound because the
/// shepherd spawns an adopted dog directly, so the pid it recorded is this
/// process, and because a pid is unique among running processes. It is also
/// the assumption most likely to be broken later by something entirely
/// reasonable, such as a wrapper script or an interpreter mapping putting a
/// shell between the shepherd and this binary, at which point the recorded
/// pid is the wrapper's and this returns `None`. That is why the caller
/// announces the fallback rather than taking it quietly, and why
/// `tests/integration.rs` drives a real adoption under a name that is not
/// the default: nothing else would ever tell you.
///
/// Errors are folded into `None` on purpose. Every caller's answer to "the
/// shepherd would not say" is the same as its answer to "the shepherd did
/// not know", and the tick that follows reports the connection failure
/// itself.
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

/// Connect, identify, and hand back both halves of a session.
///
/// The name is looked up per connection rather than once at startup: a
/// shepherd that restarted underneath this dog re-adopted it, and this is
/// the moment to ask again rather than trust an answer from a daemon that is
/// no longer running.
async fn connect(socket: &std::path::Path) -> Result<(Live, String), Error> {
    let live = Live::new(Client::connect(socket).await?);
    let name = match adopted_name(&live).await {
        Some(name) => name,
        None => {
            eprintln!(
                "shep-log-rotate: the shepherd's flock listing does not name this process as a \
                 dog, so it cannot tell what it was adopted as. Assuming {DEFAULT_NAME}, which \
                 means [dog.{DEFAULT_NAME}] in shep.toml. Adopt it under a different name and \
                 that section will be ignored."
            );
            DEFAULT_NAME.to_owned()
        }
    };
    Ok((live, name))
}

/// The poll loop.
///
/// Nothing in here is fatal except a signal. A failed tick is printed and
/// retried on the next interval, because the shepherd restarting underneath
/// a dog is ordinary rather than exceptional, and exiting would ask the
/// supervisor to restart this process for a condition that resolves itself
/// in a few seconds. A connection-shaped failure additionally drops the
/// session, so the next pass reconnects.
///
/// There is no signal handling beyond `ctrl_c`, which is the clean-exit path
/// for an operator running this in a terminal. The shepherd owns this
/// process's signals and its kill ladder. Rotation on `SIGHUP` is a shape
/// people expect from `logrotate`, and adding it here would be arguing with
/// the supervisor about who decides when this process does work.
async fn poll(socket: &std::path::Path) -> ExitCode {
    // The interval to wait when there is no session to read one from. A
    // disconnected dog has no configuration either, so the default is the
    // only honest answer, and it is also the retry delay.
    let default_interval = Config::default().interval;
    let mut session: Option<(Live, String)> = None;

    loop {
        if session.is_none() {
            match connect(socket).await {
                Ok(connected) => session = Some(connected),
                Err(err) => eprintln!("shep-log-rotate: {err}"),
            }
        }

        let mut interval = default_interval;
        if let Some((live, name)) = &session {
            match tick(live, name, SystemTime::now()).await {
                Ok((config, report)) => {
                    interval = config.interval;
                    // Silence on a quiet tick is the point, not tidiness:
                    // this dog rotates its own log along with everybody
                    // else's, so a line per interval would make the file it
                    // exists to keep small the busiest one in the directory.
                    if let Some(line) = report.summary() {
                        println!("{line}");
                    }
                }
                Err(err) => {
                    eprintln!("shep-log-rotate: {err}");
                    if matches!(err, Error::Connect(_) | Error::Request(_)) {
                        session = None;
                    }
                }
            }
        }

        if wait(interval).await == Interrupted::Yes {
            return ExitCode::SUCCESS;
        }
    }
}

/// Whether a wait ended in a signal rather than in the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interrupted {
    /// The interval elapsed.
    No,
    /// `ctrl_c` arrived first.
    Yes,
}

/// Sleep for `interval`, or until `ctrl_c`.
async fn wait(interval: UpDuration) -> Interrupted {
    tokio::select! {
        () = tokio::time::sleep(interval.as_duration()) => Interrupted::No,
        _ = tokio::signal::ctrl_c() => Interrupted::Yes,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let action = match Action::parse(args.iter().map(String::as_str)) {
        Ok(action) => action,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    if action == Action::PrintConfig {
        print!("{PRINT_CONFIG}");
        return ExitCode::SUCCESS;
    }

    let env = |key: &str| std::env::var(key).ok();
    // The same reading shep's own CLI takes: `$SHEP_HOME` decides on its
    // own when it is set, and `$HOME` is only needed for the default it
    // replaces. An adopted dog always has `$SHEP_HOME`, so the third arm is
    // for somebody running this binary by hand in a stripped environment.
    let home_dir = match (std::env::var_os("HOME"), env("SHEP_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(_)) => PathBuf::new(),
        (None, None) => {
            eprintln!(
                "shep-log-rotate: neither $HOME nor $SHEP_HOME is set, so there is no shep home \
                 to find a socket in."
            );
            return ExitCode::FAILURE;
        }
    };
    let paths = ShepPaths::resolve(&env, &home_dir);
    poll(&paths.socket).await
}

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

#[cfg(test)]
mod tests {
    use super::*;

    use shep_client::shep_core::{
        protocol::{DogSource, ProcessInfo},
        status::ProcStatus,
    };

    /// A [`Daemon`] that answers `list_flock` from a fixed listing and
    /// nothing else. Every `adopted_name` question is a question about that
    /// listing.
    struct FlockFake(Vec<ProcessInfo>);

    impl Daemon for FlockFake {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Ok(self.0.clone())
        }
        async fn reopen(&self, _name: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    /// A [`Daemon`] that cannot be reached.
    struct Unreachable;

    impl Daemon for Unreachable {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn reopen(&self, _name: &str) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
    }

    fn entry(id: u32, name: &str, pid: Option<u32>, dog: bool) -> ProcessInfo {
        ProcessInfo::builder(id, name, ProcStatus::Online)
            .pid(pid)
            .dog(dog.then(|| DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_owned(),
            }))
            .build()
    }

    #[test]
    fn print_config_is_the_only_argument() {
        assert_eq!(Action::parse(["--print-config"]), Ok(Action::PrintConfig));
        assert_eq!(Action::parse([]), Ok(Action::Run));
        assert!(Action::parse(["--rotate-now"]).is_err());
    }

    #[test]
    fn the_usage_text_carries_no_em_dash() {
        let usage = Action::parse(["--nonsense"])
            .expect_err("refused")
            .to_string();
        assert!(!usage.contains('\u{2014}'));
        assert!(!usage.contains('\u{2013}'));
    }

    #[test]
    fn the_refusal_names_the_argument_it_did_not_understand() {
        let usage = Action::parse(["--rotate-now"])
            .expect_err("refused")
            .to_string();
        assert!(usage.contains("--rotate-now"), "{usage}");
        assert!(usage.contains("--print-config"), "{usage}");
    }

    #[tokio::test]
    async fn the_dog_finds_the_name_it_was_adopted_under() {
        // Not the default name, because the default name is exactly what a
        // broken lookup falls back to.
        let flock = FlockFake(vec![
            entry(0, "web", Some(std::process::id() + 1), false),
            entry(1, "weathervane", Some(std::process::id()), true),
        ]);
        assert_eq!(
            adopted_name(&flock).await.as_deref(),
            Some("weathervane"),
            "the entry with this process's pid is this process"
        );
    }

    #[tokio::test]
    async fn a_sheep_sharing_this_pid_is_not_this_dog() {
        // A pid match alone is not identification. The listing reports one
        // pid per entry, and only a dog entry can be this process; a sheep
        // wearing the same number would mean the shepherd handed out a pid
        // it does not own, and adopting its name would read somebody else's
        // config section.
        let flock = FlockFake(vec![entry(0, "web", Some(std::process::id()), false)]);
        assert_eq!(adopted_name(&flock).await, None);
    }

    #[tokio::test]
    async fn a_dog_at_another_pid_is_not_this_dog() {
        let flock = FlockFake(vec![entry(
            0,
            "metrics",
            Some(std::process::id() + 1),
            true,
        )]);
        assert_eq!(adopted_name(&flock).await, None);
    }

    #[tokio::test]
    async fn a_shepherd_that_will_not_answer_yields_no_name() {
        assert_eq!(adopted_name(&Unreachable).await, None);
    }

    #[test]
    fn this_dog_never_sends_flush() {
        // Flush truncates the recorded paths. "Flush before rotating" is the
        // natural instinct and it deletes the lines being rotated. Nothing in
        // this crate may reach for it, and this test is the tripwire.
        //
        // The count at the bottom is the tripwire's own tripwire. A walk that
        // finds no files runs its loop body zero times and passes having
        // checked nothing, which is the exact shape of a guard this project
        // already shipped once and had to fix.
        //
        // Spelled in two pieces because the walk includes this file, and a
        // tripwire that fires on its own source can only ever be deleted.
        let forbidden = concat!("Request", "::Flush");
        let mut scanned: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read");
            for (number, line) in source.lines().enumerate() {
                assert!(
                    !line.contains(forbidden),
                    "{}:{}: Flush truncates the log it is asked to settle",
                    path.display(),
                    number + 1
                );
            }
            scanned.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .expect("a source file name")
                    .to_owned(),
            );
        }
        assert!(
            scanned.len() >= 7,
            "the walk found {} source files, and this crate has 7 or more; \
             a tripwire that scans nothing passes for the wrong reason: {scanned:?}",
            scanned.len()
        );
        assert!(
            scanned.iter().any(|name| name == "tick.rs"),
            "tick.rs is the only module that builds a Request at all, so a walk \
             that missed it is not watching the file that matters: {scanned:?}"
        );
    }
}
