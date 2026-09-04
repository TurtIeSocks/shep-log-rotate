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
//! Two names, from two places, and conflating them is how this went wrong
//! once already. See [`Identity`].
//!
//! The handshake name is what goes in the `Hello` frame, and it comes from
//! `$SHEP_DOG_NAME` and from nowhere else. shep sets it in the environment
//! it spawns an adopted dog with, and the daemon records a handshake only
//! for a connection that carries one. A dog that connects anonymously
//! serves every request correctly and is still rendered `silent`, restarted
//! once, and then declared stale and left down. There is nothing to guess
//! at here, and guessing would be worse than not knowing: the name is also
//! how the daemon decides which dog to act on when it refuses a handshake,
//! so a borrowed name restarts somebody else's dog. No `$SHEP_DOG_NAME`
//! means no shepherd spawned this process, and it connects without a name
//! at all.
//!
//! The config name is the `[dog.<name>]` key the settings live under.
//! Getting that one wrong is silent in its own way: the daemon answers
//! `DogConfig` for a name nobody adopted with an empty section, which is
//! byte for byte what a dog running on its defaults gets. Adopt this binary
//! as `logrotate` when it asks for `log-rotate` and every setting in the
//! operator's `shep.toml` is discarded without either side saying so. It is
//! the handshake name whenever there is one, and [`DEFAULT_NAME`] when
//! there is not, because somebody running this binary by hand still wants
//! their `shep.toml` read.

#![forbid(unsafe_code)]

mod config;
mod error;
mod file_set;
mod naming;
mod prune;
mod rotate;
#[cfg(test)]
mod test_support;
mod tick;

use core::fmt;
use std::{path::PathBuf, process::ExitCode, time::SystemTime};

use shep_client::{
    ConnectError, LinkState, ReconnectingClient, shep_core::paths::ShepPaths,
    shep_core::values::UpDuration,
};

use crate::{
    config::{Config, PRINT_CONFIG},
    error::Error,
    tick::{Live, tick},
};

/// The `[dog.<name>]` section to read when `$SHEP_DOG_NAME` is unset, which
/// means nothing adopted this process and somebody is running the binary by
/// hand.
///
/// A config-section default only. It is never announced in the handshake:
/// see [`Identity`] for why the two names part company here rather than
/// sharing one fallback.
///
/// It matches what `shep adopt shep-log-rotate` picks on its own, which the
/// README documents, so an operator following the README lands on the
/// section this constant already expects.
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
from this process's arguments. The environment supplies two things and no
more: $SHEP_HOME names the socket, and $SHEP_DOG_NAME names the dog. The
shepherd sets both when it spawns this dog.";

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
    /// [`Usage`] for any argument other than `--print-config`. Unknown flags
    /// are refused rather than ignored: a rotator silently ignoring
    /// `--dry-run` would rotate for real.
    ///
    /// A repeated `--print-config` is accepted. It names the same action
    /// however many times it is given, and refusing it meant answering
    /// `--print-config --print-config` with "shep-log-rotate does not
    /// understand --print-config", which is a confusing thing to tell
    /// somebody who plainly does.
    pub fn parse<'a, I: IntoIterator<Item = &'a str>>(args: I) -> Result<Self, Usage> {
        let mut action = Self::Run;
        for arg in args {
            match arg {
                "--print-config" => action = Self::PrintConfig,
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

/// The two names this dog needs, and the two different places they come
/// from.
///
/// Separate fields rather than one string, because they answer different
/// questions and only one of them may be guessed at. The module docs have
/// the argument; this type is what stops the code drifting back to one
/// name for both.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    /// What to announce in the `Hello` frame, or `None` for a process no
    /// shepherd spawned.
    ///
    /// Never falls back to [`DEFAULT_NAME`]. A name the daemon did not hand
    /// out is a name it will act on anyway: a refused handshake is recorded
    /// against whatever the frame said, so an invented one asks the daemon
    /// to restart a dog that is running perfectly well.
    handshake: Option<String>,
    /// The `[dog.<name>]` section to read out of `shep.toml`.
    section: String,
}

impl Identity {
    /// Read both names out of the environment.
    ///
    /// Takes a lookup rather than reading [`std::env::var`] itself. The
    /// environment is a process-wide mutable global, and a test that sets
    /// one variable to check the absent case is a test that races every
    /// other test in the binary.
    ///
    /// An empty `$SHEP_DOG_NAME` reads as unset. It cannot be a real dog:
    /// `[dog.]` is not a section anybody can write, and an empty name in a
    /// `Hello` frame is a handshake the daemon cannot attribute either.
    fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let handshake = env("SHEP_DOG_NAME").filter(|name| !name.is_empty());
        let section = handshake.clone().unwrap_or_else(|| DEFAULT_NAME.to_owned());
        Self { handshake, section }
    }
}

/// Connect, announcing the handshake name when there is one.
///
/// The name is settled once, before the loop starts, rather than looked up
/// per connection: it comes out of the environment shep spawned this
/// process with, and a shepherd that restarted underneath the dog did not
/// reach into that environment and rewrite it.
async fn connect(socket: &std::path::Path, identity: &Identity) -> Result<Live, Error> {
    let client = match &identity.handshake {
        Some(name) => ReconnectingClient::connect_as_dog(socket, name).await?,
        None => ReconnectingClient::connect(socket).await?,
    };
    Ok(Live::new(client))
}

/// Say why this dog is stopping, and hand back the code to stop with.
///
/// A refused handshake is protocol-version skew, and it is the one failure
/// in here that waiting cannot fix: the daemon that refused is the only
/// party that can, every later request on that connection fails, and the
/// client's own supervisor has already given up rather than retrying it.
/// Staying up would mean an infinite run of identical failures in the log
/// this dog exists to keep small.
///
/// Exiting is also what makes the refusal actionable. The shepherd restarts
/// a dog from its recorded path, and a skew usually means the binary at
/// that path has already been replaced by the one that matches, so the
/// restart is the fix rather than a retry of the same mistake.
fn refused(daemon_version: Option<&str>, message: &str) -> ExitCode {
    eprintln!(
        "shep-log-rotate: the shepherd refused this dog's handshake, and no amount of \
         reconnecting fixes a protocol-version skew. The shepherd reports {}, and said: \
         {message}. Exiting so it can restart this dog from disk.",
        daemon_version.unwrap_or("no version")
    );
    ExitCode::FAILURE
}

/// The poll loop.
///
/// Nothing in here is fatal except a signal and a refused handshake. A
/// failed tick is printed and retried on the next interval, because the
/// shepherd restarting underneath a dog is ordinary rather than
/// exceptional, and exiting would ask the supervisor to restart this
/// process for a condition that resolves itself in a few seconds.
///
/// A connection-shaped failure no longer drops the session, which is the
/// one thing that changed when [`Live`] started holding a reconnecting
/// client. That client re-establishes its own connection on its own
/// backoff, and requests issued while it is doing so fail rather than
/// queueing, so a tick landing in that gap is precisely the failed tick
/// this loop already retries. Dropping the session would abort a supervisor
/// mid-backoff and replace it with an unsupervised first connection, which
/// is strictly worse. The `Option` survives for the case it was always
/// really about: the first connection, which nothing supervises and which
/// fails whenever this dog is started before the socket exists.
///
/// There is no signal handling beyond `ctrl_c`, which is the clean-exit path
/// for an operator running this in a terminal. The shepherd owns this
/// process's signals and its kill ladder. Rotation on `SIGHUP` is a shape
/// people expect from `logrotate`, and adding it here would be arguing with
/// the supervisor about who decides when this process does work.
async fn poll(socket: &std::path::Path, identity: &Identity) -> ExitCode {
    // The interval to wait when there is no session to read one from. A
    // disconnected dog has no configuration either, so the default is the
    // only honest answer, and it is also the retry delay.
    let default_interval = Config::default().interval;
    let mut session: Option<Live> = None;

    if identity.handshake.is_none() {
        // Once, before the loop, rather than per connection: the answer
        // cannot change while this process runs, and a dog whose socket is
        // not up yet would otherwise print it on every retry.
        //
        // Loudly, because the two things it means are worth telling apart.
        // Run by hand it is expected. Under a shepherd it means something
        // stripped the environment between `shep adopt` and this process,
        // and the daemon is about to call this dog silent and stop
        // restarting it.
        eprintln!(
            "shep-log-rotate: $SHEP_DOG_NAME is not set, so nothing adopted this process. It \
             will connect without naming itself, which the shepherd does not count as a \
             handshake, and read [dog.{DEFAULT_NAME}] in shep.toml."
        );
    }

    loop {
        if session.is_none() {
            match connect(socket, identity).await {
                Ok(live) => session = Some(live),
                Err(Error::Connect(ConnectError::ProtocolMismatch {
                    daemon_version,
                    message,
                    ..
                })) => return refused(daemon_version.as_deref(), &message),
                Err(err) => eprintln!("shep-log-rotate: {err}"),
            }
        }

        let mut interval = default_interval;
        if let Some(live) = &session {
            // Before the tick rather than after a failed one, because a
            // refused link answers every request with the same closed
            // connection and the tick's error would say nothing about why.
            if let LinkState::Refused {
                daemon_version,
                message,
            } = live.link()
            {
                return refused(daemon_version.as_deref(), &message);
            }
            match tick(live, &identity.section, SystemTime::now()).await {
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
                Err(err) => eprintln!("shep-log-rotate: {err}"),
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
    let identity = Identity::from_env(env);
    poll(&paths.socket, &identity).await
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
    use crate::test_support::assert_no_dashes;

    /// An environment holding exactly one variable, which is the only one
    /// [`Identity::from_env`] reads.
    fn only(key: &str, value: &str) -> impl Fn(&str) -> Option<String> {
        let (key, value) = (key.to_owned(), value.to_owned());
        move |asked| (asked == key).then(|| value.clone())
    }

    #[test]
    fn print_config_is_the_only_argument() {
        assert_eq!(Action::parse(["--print-config"]), Ok(Action::PrintConfig));
        assert_eq!(Action::parse([]), Ok(Action::Run));
        assert!(Action::parse(["--rotate-now"]).is_err());
    }

    #[test]
    fn the_same_flag_twice_names_the_same_action() {
        // It used to be refused, with "shep-log-rotate does not understand
        // --print-config" - which is a confusing thing to tell somebody who
        // plainly does understand it.
        assert_eq!(
            Action::parse(["--print-config", "--print-config"]),
            Ok(Action::PrintConfig)
        );
    }

    #[test]
    fn the_usage_text_carries_no_em_dash() {
        let usage = Action::parse(["--nonsense"])
            .expect_err("refused")
            .to_string();
        assert_no_dashes(&usage);
    }

    #[test]
    fn the_refusal_names_the_argument_it_did_not_understand() {
        let usage = Action::parse(["--rotate-now"])
            .expect_err("refused")
            .to_string();
        assert!(usage.contains("--rotate-now"), "{usage}");
        assert!(usage.contains("--print-config"), "{usage}");
    }

    #[test]
    fn the_dog_takes_both_names_from_shep_dog_name() {
        // Not the default name, because the default name is exactly what a
        // broken lookup falls back to.
        let identity = Identity::from_env(only("SHEP_DOG_NAME", "weathervane"));
        assert_eq!(identity.handshake.as_deref(), Some("weathervane"));
        assert_eq!(
            identity.section, "weathervane",
            "the section follows the adopted name, so [dog.weathervane] is what gets read"
        );
    }

    #[test]
    fn without_shep_dog_name_the_dog_names_itself_to_nobody() {
        // The half that matters. A dog nothing adopted must send no name at
        // all: a guessed one is recorded against a real dog, and the daemon
        // restarts that one when this connection is refused.
        let identity = Identity::from_env(|_| None);
        assert_eq!(identity.handshake, None);
        assert_eq!(
            identity.section, DEFAULT_NAME,
            "a hand run still reads a section, and this is the one the README documents"
        );
    }

    #[test]
    fn an_empty_shep_dog_name_is_no_name() {
        let identity = Identity::from_env(only("SHEP_DOG_NAME", ""));
        assert_eq!(identity.handshake, None);
        assert_eq!(identity.section, DEFAULT_NAME);
    }

    #[test]
    fn nothing_else_in_the_environment_names_this_dog() {
        // $SHEP_NAME and $SHEP_INSTANCE are set alongside $SHEP_DOG_NAME and
        // name the shepherd, not the dog. Reading either would put the
        // shepherd's own name in the handshake.
        let identity = Identity::from_env(only("SHEP_NAME", "production"));
        assert_eq!(identity.handshake, None);
        assert_eq!(identity.section, DEFAULT_NAME);
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
