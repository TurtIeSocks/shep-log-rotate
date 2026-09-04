//! One poll of the rotator's loop: read the config, list the flock, rename
//! what has grown, ask the shepherd to reopen, then compress and prune.
//!
//! This is the only module that speaks to the daemon, and it is where the
//! two guards no earlier module could enforce live.
//!
//! The first is a rename guard. A base with no extension and a base whose
//! extension is all digits generate byte-identical names: under `numeric`
//! naming `/var/log/web` rotates into `/var/log/web.1`, which may be the
//! live log of a different sheep whose configured path is literally
//! `/var/log/web.1`. Both readings of that name are correct, so
//! `naming::match_generation` cannot tell them apart and it is not its job
//! to. Only the shepherd knows which paths are being written into, so only
//! this module can refuse, and it refuses by leaving the whole base alone.
//! Renaming a live log out from under an open descriptor is worse than
//! deleting it: shep keeps writing into a path that no longer exists, and
//! the operator's log simply stops appearing.
//!
//! The second is the `protected` [`FileSet`] [`prune::tidy`] takes, built
//! here from every log path the whole flock reported. The collision is
//! between DIFFERENT sheep, so a per-sheep set would miss exactly the case
//! that matters.
//!
//! # This dog rotates its own logs, on purpose
//!
//! A dog is an ordinary supervised entry with a marker, so `ListFlock`
//! reports the metrics dog, the bark dog and this rotator right alongside
//! the flock. Rotating them is the decision rather than an accident that
//! happens to work: dog logs grow like any other, and "your log directory is
//! bounded, except for these four files" is the surprising behaviour. There
//! is no loop, because reopening its own log does not itself cause a
//! rotation.
//!
//! It does mean this dog has to be quiet. A line per file per interval would
//! make its own log the busiest file in the directory it exists to keep
//! small, so [`Report::summary`] returns one line for a tick that rotated
//! something and nothing at all for a tick that did not.
//!
//! [`prune::tidy`]: crate::prune::tidy

use core::fmt;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::SystemTime};

use shep_client::{
    LinkState, ReconnectingClient,
    shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec},
};

use crate::{
    config::{Config, Naming},
    error::Error,
    file_set::{FileSet, ResolvedDir},
    naming::{LogPath, match_generation},
    prune::tidy,
    rotate::rotate,
    stop::Stop,
};

/// The three things one tick asks the shepherd for.
///
/// Narrow on purpose: with the socket behind three methods, the whole of the
/// orchestration below is testable without a daemon. There is one real
/// implementation, [`Live`], and one fake in this module's tests.
///
/// The `async fn` here is used through a generic bound only, never behind
/// `dyn`. That is a decision rather than an accident: an `async fn` in a
/// trait cannot spell an auto-trait bound on the future it returns, so a
/// caller needing `Send` could not ask for one. This binary runs a single
/// current-thread runtime and has no such caller. The `async_fn_in_trait`
/// lint stays quiet here because a binary's trait is not a public surface;
/// lift this into a library and it starts firing, at which point the
/// trade-off wants deciding rather than silencing.
pub trait Daemon {
    /// The dog's own `[dog.<name>]` section, as TOML text.
    ///
    /// Empty when `shep.toml` has no such section, which is the ordinary
    /// case for a dog running on its defaults.
    ///
    /// # Errors
    /// [`Error::Connect`] or [`Error::Request`] if the shepherd cannot be
    /// reached or refuses, and [`Error::Protocol`] if it answers with
    /// something other than a dog section.
    async fn dog_config(&self, name: &str) -> Result<String, Error>;

    /// Every supervised entry, dogs included.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error>;

    /// Ask one sheep, by name, to reopen its log files.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn reopen(&self, name: &str) -> Result<(), Error>;
}

/// What one tick did.
///
/// Every field is a count of something an operator might go looking for
/// afterwards, which is why the two "left alone" counts are separate: one
/// says a rename was refused, the other says a compression or a deletion
/// was, and they have different causes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Live log paths renamed. Counts paths, not sheep: two sheep sharing
    /// one path rotate it once between them.
    pub rotated: usize,
    /// Log paths this tick could not consider at all, because they could not
    /// be stat'd or could not be spelled. A sheep registered but never
    /// started has no log file yet, and that is the ordinary case here
    /// rather than a fault. A file that was merely too small to qualify is
    /// not counted: that is not a skip, it is the answer.
    pub skipped: usize,
    /// Generations gzipped.
    pub compressed: usize,
    /// Files deleted for being past `keep`.
    ///
    /// Files rather than generations, because the two can differ. A
    /// generation an earlier crash left half-compressed wears two files, and
    /// they go together, so it contributes two. Assigned straight from
    /// [`Tidied::deleted`](crate::prune::Tidied::deleted), so it carries
    /// that field's unit and says so here rather than leaving the two types
    /// disagreeing across the seam.
    pub deleted: usize,
    /// Logs left unrotated because a name this base rotates into is some
    /// sheep's live log. See the module docs. Anything above zero wants a
    /// human: the two logs need different names before either can rotate.
    pub skipped_collision: usize,
    /// Times `tidy` refused to compress or delete a generation for the same
    /// reason.
    ///
    /// Refusals rather than files: one file can be the reason for two of
    /// them, once as a candidate generation of its own and once as the name
    /// some other generation wanted to compress into. Assigned straight from
    /// [`Tidied::skipped_protected`](crate::prune::Tidied::skipped_protected),
    /// so it carries that field's unit.
    pub skipped_protected: usize,
    /// The sheep whose reopen was refused, and what it said. Set means the
    /// tick stopped early: no further sheep was rotated, and nothing that
    /// sheep or a later one writes to was compressed or pruned. What the
    /// sheep before it had reopened was still tidied.
    pub reopen_failed: Option<String>,
}

impl Report {
    /// The one line worth printing for this tick, or `None` when there is
    /// nothing to say.
    ///
    /// This dog's own log is one of the files it rotates, so silence on a
    /// quiet tick is not tidiness, it is the difference between a log
    /// directory this dog keeps small and one it fills. A tick that only
    /// looked says nothing; a tick that renamed something, refused to, or
    /// was refused a reopen says one line.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.rotated == 0 && self.skipped_collision == 0 && self.reopen_failed.is_none() {
            return None;
        }
        let mut line = format!(
            "rotated {}, compressed {}, deleted {}",
            self.rotated, self.compressed, self.deleted
        );
        if self.skipped_collision > 0 {
            line.push_str(&format!(
                ", left {} alone whose rotated name is a live log",
                self.skipped_collision
            ));
        }
        if self.skipped_protected > 0 {
            line.push_str(&format!(
                ", refused {} times to touch a live log",
                self.skipped_protected
            ));
        }
        if let Some(why) = &self.reopen_failed {
            line.push_str(&format!(", stopped early: reopen refused ({why})"));
        }
        Some(line)
    }
}

/// One full rotation pass.
///
/// Returns the [`Config`] it read as well as the report, because the caller
/// needs `interval` to decide how long to sleep and re-reading it separately
/// would be a second request for a value already in hand.
///
/// The order is load-bearing:
///
/// 1. Read the config. Never cached: the daemon re-reads its side per
///    request, and a dog that cached this would undo that, so changing
///    `max_size` would need a `shep disable` and a `shep enable` rather than
///    taking effect on the next tick.
/// 2. List the flock, and build the protected set from all of it.
/// 3. Stat and qualify every log path, before renaming any of them, so that
///    two sheep sharing one path both see it as it was.
/// 4. Per sheep: rename its qualifying files, then reopen that sheep alone.
/// 5. Only once the reopens are done, compress and prune, and only the logs
///    whose every sheep reopened. Compressing a large log is not quick, and
///    doing it first would widen the stretch during which shep is still
///    writing through its old descriptor. Compressing one whose sheep never
///    reopened would read a file mid-append and then delete what shep is
///    writing to.
///
/// `stop` is consulted from the tidy loop and nowhere else. The renames and
/// the reopens are quick, and a tick interrupted between a rename and its
/// reopen leaves shep writing into a file with the wrong name, which is
/// worse than the wait. The gzip of a large generation is the slow part: it
/// runs on a blocking thread, a stop already requested when its turn comes
/// skips it, and a stop arriving while it runs abandons it. The half-written
/// `.gz` an abandoned gzip leaves is the state the next pass recovers from.
///
/// # Errors
/// - [`Error::Config`] if the `[dog.<name>]` section cannot be understood.
///   Nothing is touched in that case; the tick reads the config first for
///   exactly that reason.
/// - [`Error::Connect`], [`Error::Request`] or [`Error::Protocol`] if the
///   config or the flock listing cannot be had. A refused *reopen* is not an
///   error: it is reported in [`Report::reopen_failed`], because shep is
///   still writing into the renamed file through its existing descriptor, so
///   nothing is lost and the next successful reopen recovers.
/// - [`Error::Io`] or [`Error::Exhausted`] from a rename, a compression or a
///   deletion. A failed rename still reopens the sheep it half rotated, and
///   still tidies what every sheep before it had reopened, before it
///   returns. When both the rename and a tidy fail, the rename is the error
///   reported: it is the first fault, and the tidy recurs next tick.
pub async fn tick<D: Daemon>(
    daemon: &D,
    dog_name: &str,
    now: SystemTime,
    stop: &mut Stop,
) -> Result<(Config, Report), Error> {
    // Named at the point of failure rather than through a `From` impl: the
    // section is `[dog.<name>]` for whatever this dog was adopted as, and an
    // error naming some other block sends the reader to a section they never
    // wrote.
    let config =
        Config::from_toml(&daemon.dog_config(dog_name).await?).map_err(|source| Error::Config {
            section: dog_name.to_owned(),
            source,
        })?;
    let flock = daemon.list_flock().await?;
    // Every live log path the flock reported, in one set, because the
    // collision it exists to catch is between different sheep.
    let protected = Arc::new(FileSet::from_paths(
        flock.iter().flat_map(log_paths).map(Path::new),
    ));
    let mut report = Report::default();

    // Qualify the whole flock before renaming anything. Two sheep can share
    // one log path (`merge_logs`, or an explicit `out_file` on a
    // multi-instance app), and both have to see it as it was.
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<LogPath>> = BTreeMap::new();
    for sheep in &flock {
        for path in log_paths(sheep) {
            // A path this dog cannot spell is a path it must not go on to
            // rename or delete. `LogPath::split` refuses a non-UTF-8 name,
            // and this is where that refusal gets its answer: skip it, count
            // it, and leave it for a human.
            let Some(base) = LogPath::split(Path::new(path)) else {
                report.skipped += 1;
                continue;
            };
            // A sheep registered but never started has no log file yet, and
            // that is normal rather than broken.
            let Ok(metadata) = fs::metadata(base.live()) else {
                report.skipped += 1;
                continue;
            };
            if !qualifies(&metadata, &config, now) {
                continue;
            }
            if !groups.contains_key(&sheep.name) {
                order.push(sheep.name.clone());
            }
            let group = groups.entry(sheep.name.clone()).or_default();
            // `merge_logs` points `out_file` and `err_file` at one path.
            if !group.contains(&base) {
                group.push(base);
            }
        }
    }

    // Rename, then reopen, one sheep at a time rather than one reopen for
    // the batch: `Request::Reopen` takes a single selector and there is no
    // multi-name variant. Per sheep is the better shape anyway, because the
    // rename-to-reopen window is then one sheep wide rather than the whole
    // tick.
    // A `FileSet` rather than a set of paths, for the same reason `protected`
    // is one: two sheep can be handed one file under two spellings, and a
    // second rename of a file that has already moved fails with NotFound and
    // leaves the second sheep never reopened.
    let mut renamed = FileSet::default();
    // Bases whose sheep has reopened, so shep has stopped writing into what
    // was renamed. Only those are safe to compress or delete.
    let mut to_tidy: Vec<LogPath> = Vec::new();
    let mut halt: Option<Error> = None;
    // Where in `order` the loop stopped early, if it did: the sheep at this
    // position and every one after it was not reopened this tick.
    let mut not_reopened_from: Option<usize> = None;

    'flock: for (position, name) in order.iter().enumerate() {
        let mut rotated_any = false;
        let mut rotated_here: Vec<LogPath> = Vec::new();
        for base in &groups[name] {
            // Through `protected` rather than `ResolvedDir::of`, so this
            // base is resolved the way the protected set was, at the moment
            // the set was built. See the `file_set` module docs.
            let dir = protected.resolve(&base.dir);
            let live_name = base.live_name();
            if renamed.contains(&dir, &live_name) {
                // Another sheep shares this path and has already rotated it.
                // This one is still writing through its own descriptor, so
                // it still needs the reopen below.
                rotated_any = true;
                continue;
            }
            if collides_with_a_live_log(base, &dir, config.naming, &protected) {
                report.skipped_collision += 1;
                continue;
            }
            match rotate(base, config.naming, now) {
                Ok(_generation) => {
                    renamed.insert(dir, live_name);
                    rotated_here.push(base.clone());
                    report.rotated += 1;
                    rotated_any = true;
                }
                Err(err) => {
                    // Stop here rather than returning straight out: this
                    // sheep may already have a renamed file behind it, and
                    // the reopen below is what puts its writes back where
                    // the operator looks for them. Nothing later in the tick
                    // runs, but that one reopen does.
                    halt = Some(err);
                    break;
                }
            }
        }

        if rotated_any {
            if let Err(err) = daemon.reopen(name).await {
                // Rotating further sheep while the reopen path is broken
                // turns one recoverable state into several confusing ones.
                report.reopen_failed = Some(format!("{name}: {err}"));
                not_reopened_from = Some(position);
                break 'flock;
            }
            to_tidy.extend(rotated_here);
        }
        if halt.is_some() {
            // This sheep was reopened for whatever it did rotate. The ones
            // after it were never reached.
            not_reopened_from = Some(position + 1);
            break 'flock;
        }
    }

    // A sheep the loop never reached, or whose reopen was refused, is still
    // writing into a renamed file through its old descriptor. Two sheep can
    // share one log path, so a base an earlier sheep reopened may be that
    // same file: compressing it would read it mid-append, and removing the
    // plain copy afterwards would delete what shep is writing to. Anything
    // such a sheep writes to is left for a later tick.
    if let Some(from) = not_reopened_from {
        let mut still_writing = FileSet::default();
        for name in &order[from..] {
            for base in &groups[name] {
                still_writing.insert(protected.resolve(&base.dir), base.live_name());
            }
        }
        to_tidy.retain(|base| {
            !still_writing.contains(&protected.resolve(&base.dir), &base.live_name())
        });
    }

    let tidied = tidy_all(&to_tidy, &config, &protected, &mut report, stop).await;
    if let Some(err) = halt {
        return Err(err);
    }
    tidied?;
    Ok((config, report))
}

/// Compress and prune each of `bases`, adding what happened to `report`.
///
/// Each base is tidied on a blocking thread rather than on the runtime's
/// own. This binary runs one current-thread runtime, and the client's
/// supervisor lives on it: a gzip run in place would hold that thread for
/// as long as the gzip takes, so a shepherd restarting underneath a large
/// compression would not be reconnected to until it finished, and the next
/// tick would fail for it. Off the thread, the supervisor reconnects while
/// the gzip runs.
///
/// A stop already requested when a base's turn comes skips it and every
/// base after it. A stop arriving while a gzip runs abandons that gzip: the
/// thread finishes or is cut off when the process exits, and either way
/// the plain file is still there, since it is removed only after the `.gz`
/// is complete and synced. Whatever was already counted stays counted; it
/// describes work that was done.
async fn tidy_all(
    bases: &[LogPath],
    config: &Config,
    protected: &Arc<FileSet>,
    report: &mut Report,
    stop: &mut Stop,
) -> Result<(), Error> {
    for base in bases {
        if stop.requested() {
            return Ok(());
        }
        let gzip = tokio::task::spawn_blocking({
            let (base, config, protected) = (base.clone(), config.clone(), Arc::clone(protected));
            move || tidy(&base, &config, &protected)
        });
        let joined = tokio::select! {
            joined = gzip => joined,
            () = stop.wait() => return Ok(()),
        };
        let tidied = match joined {
            Ok(tidied) => tidied?,
            Err(join) => match join.try_into_panic() {
                // A panic in tidy is a bug, and it is re-raised here so the
                // process fails the way it would have with tidy in place.
                Ok(payload) => std::panic::resume_unwind(payload),
                // A blocking task is cancelled only by a runtime shutting
                // down, and this future has been dropped by then.
                Err(join) => {
                    unreachable!("a blocking task was cancelled under a live runtime: {join}")
                }
            },
        };
        report.compressed += tidied.compressed;
        report.deleted += tidied.deleted;
        report.skipped_protected += tidied.skipped_protected;
    }
    Ok(())
}

/// Whether a log has grown or aged past what `config` allows.
///
/// An empty file never qualifies, whatever its age. Rotating one frees no
/// disk and produces an empty generation, and that generation then pushes a
/// real one past `keep` and has it deleted: a rotator destroying the history
/// it exists to keep. A sheep that logs nothing for a week is the ordinary
/// way to reach this, not a contrived one.
fn qualifies(metadata: &fs::Metadata, config: &Config, now: SystemTime) -> bool {
    if metadata.len() == 0 {
        return false;
    }
    if metadata.len() >= config.max_size.bytes() {
        return true;
    }
    let Some(max_age) = config.max_age else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    now.duration_since(modified)
        .is_ok_and(|age| age >= max_age.as_duration())
}

/// The log paths one sheep writes to: `out_file` and `err_file`, whichever
/// of them are set.
fn log_paths(sheep: &ProcessInfo) -> impl Iterator<Item = &str> {
    [sheep.out_file.as_deref(), sheep.err_file.as_deref()]
        .into_iter()
        .flatten()
}

/// Whether rotating `base` could disturb a path some sheep is using as a
/// live log. `dir` is `base.dir`, resolved through `protected`.
///
/// A `true` here skips the base whole: not rotated, and so not tidied
/// either.
///
/// Asked by name against the flock's own list rather than by listing the
/// directory and intersecting. Every path `generations` could return and
/// find in `protected` is a name this matches, and asking by name picks up
/// two cases a listing cannot see:
///
/// - A sheep that is registered but stopped has no log file, so `generations`
///   cannot see its name. Rotating into it would hand that sheep a file full
///   of somebody else's log the moment it starts.
/// - `generations` deliberately skips symlinks and anything that is not a
///   regular file, so a live log that is a symlink is invisible to it.
///
/// It also costs one lookup instead of a `read_dir` per base.
///
/// Getting this wrong is worse than `tidy` getting its own guard wrong,
/// which is why both consult the same [`FileSet`] rather than each resolving
/// directories its own way. `tidy` declining to protect a file it should
/// have costs a deletion, which is visible. This guard declining costs a
/// rename of a file some other sheep has open, and shep goes on writing
/// into an inode with no name: that sheep's log simply stops appearing,
/// with nothing logged anywhere to say why. Measured against a real
/// shepherd before the two agreed on how a directory is spelled: two sheep
/// sharing one directory under two spellings, and one of them lost its live
/// log to the other's rotation on the first tick.
fn collides_with_a_live_log(
    base: &LogPath,
    dir: &ResolvedDir,
    naming: Naming,
    protected: &FileSet,
) -> bool {
    protected
        .names_in(dir)
        .iter()
        .any(|name| match_generation(base, naming, name).is_some())
}

/// [`Daemon`] over a real connection to the shepherd.
///
/// A [`ReconnectingClient`] rather than a plain `Client`, because a dog
/// outlives the daemon it connected to: shep hands over to a successor and
/// the dog is still running with a dead socket in its hand. The reconnect
/// is the client's own business, so nothing above this type reconnects.
///
/// `Debug` is written rather than derived, and it prints nothing about the
/// connection. [`ReconnectingClient`]'s own `Debug` carries its socket path,
/// which is `$SHEP_HOME/run/shep.sock` and so usually sits under somebody's
/// home directory. This is a `pub` type, so a consumer that logged one, or a
/// `#[derive(Debug)]` on a struct that held one, would put that path
/// somewhere an operator later pastes into a bug report. None of the four
/// fields helps: the socket comes from the environment, the announced name is
/// already in every message this dog writes, and the link state and handshake
/// ack are both readable from the client itself when they are wanted.
///
/// Pinned by `debug_says_nothing_about_the_connection`, because a later
/// `#[derive(Debug)]` here would be a silent regression.
pub struct Live(ReconnectingClient);

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Live(<shepherd session>)")
    }
}

impl Live {
    /// Wrap a connected client.
    #[must_use]
    pub fn new(client: ReconnectingClient) -> Self {
        Self(client)
    }

    /// What the client's supervisor is doing right now.
    ///
    /// On [`Daemon`] deliberately not: a fake has no connection to report
    /// on, and the one caller that asks is the poll loop, which only ever
    /// asks a real one. See `main::poll` for what it does with the answer.
    #[must_use]
    pub fn link(&self) -> LinkState {
        self.0.link()
    }
}

impl Daemon for Live {
    async fn dog_config(&self, name: &str) -> Result<String, Error> {
        let asked = Request::DogConfig {
            name: name.to_owned(),
        };
        match self.0.request(asked).await? {
            Response::DogSection { toml } => Ok(toml.as_str().to_owned()),
            other => Err(unexpected("DogConfig", &other)),
        }
    }

    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
        match self.0.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            other => Err(unexpected("ListFlock", &other)),
        }
    }

    async fn reopen(&self, name: &str) -> Result<(), Error> {
        let asked = Request::Reopen {
            selector: SelectorSpec::Name(name.to_owned()),
        };
        match self.0.request(asked).await? {
            Response::Reopened(_) => Ok(()),
            other => Err(unexpected("Reopen", &other)),
        }
    }
}

/// The shepherd answered something this dog cannot use.
fn unexpected(asked: &str, got: &Response) -> Error {
    Error::Protocol(format!("{} in answer to {asked}", named(got)))
}

/// Name a response without printing its body.
///
/// `Debug` alone would do for the last arm only: a listing variant carries
/// its whole listing, and a `DogSection` carries a section that routinely
/// holds webhook credentials. Naming the variants this dog asks for by hand
/// keeps both out of an error message.
fn named(response: &Response) -> String {
    match response {
        Response::Pong => "Pong".to_owned(),
        Response::Flock(flock) => format!("a Flock of {}", flock.len()),
        Response::DogSection { .. } => "a DogSection".to_owned(),
        Response::Reopened(sheep) => format!("a Reopened of {}", sheep.len()),
        // `Response` is `#[non_exhaustive]`, so this arm is not optional. A
        // variant added to the protocol after this dog was written is
        // exactly the one worth naming, and only `Debug` can name it.
        // Truncated, because some of them are listings.
        other => format!("{other:?}").chars().take(60).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_no_dashes;
    use shep_client::shep_core::status::ProcStatus;
    use std::cell::RefCell;

    /// `Live`'s `Debug` is written rather than derived so a socket path under
    /// somebody's home directory cannot reach a log or a bug report. Proven
    /// against a real `ReconnectingClient` over a real socket, because the
    /// thing being guarded against is the DERIVED impl, and a fake that did
    /// not hold a client could not tell the two apart.
    #[tokio::test]
    async fn debug_says_nothing_about_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let (_fake, _served) = shep_client::testing::fake_daemon_accepting_repeatedly(
            &socket,
            Response::Flock(Vec::new()),
        );
        let client = ReconnectingClient::connect_as_dog(&socket, "weathervane")
            .await
            .unwrap();
        let shown = format!("{:?}", Live::new(client));

        assert_eq!(shown, "Live(<shepherd session>)");
        assert!(
            !shown.contains("weathervane"),
            "the announced name leaked: {shown}"
        );
        assert!(
            !shown.contains(&socket.display().to_string()),
            "the socket path leaked: {shown}"
        );
    }
    /// A daemon that answers from a script and records what it was asked.
    struct Fake {
        config: String,
        flock: Vec<ProcessInfo>,
        reopen_fails: Option<String>,
        reopened: RefCell<Vec<String>>,
    }

    impl Fake {
        /// A daemon serving `config` and `flock` that grants every reopen.
        fn new(config: &str, flock: Vec<ProcessInfo>) -> Self {
            Self {
                config: config.to_owned(),
                flock,
                reopen_fails: None,
                reopened: RefCell::new(Vec::new()),
            }
        }
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

    /// One tick against `fake`, with a stop nothing will ever request.
    async fn run(fake: &Fake, now: SystemTime) -> Result<(Config, Report), Error> {
        tick(fake, "log-rotate", now, &mut Stop::never()).await
    }

    /// A `now` a year ahead of the clock, so a file written moments ago is
    /// unambiguously older than any `max_age` a test sets.
    fn a_year_on() -> SystemTime {
        SystemTime::now() + core::time::Duration::from_secs(31_536_000)
    }

    #[tokio::test]
    async fn a_file_over_max_size_is_rotated_and_its_sheep_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1);
        assert!(!out.exists(), "the live path is free for shep to reopen");
        assert_eq!(*fake.reopened.borrow(), vec!["web".to_owned()]);
    }

    #[tokio::test]
    async fn a_file_under_max_size_is_left_alone_and_nothing_is_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "small\n").expect("seeded");
        let fake = Fake::new("max_size = \"1M\"\n", vec![sheep("web", Some(&out), None)]);

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert!(out.exists());
        assert!(
            fake.reopened.borrow().is_empty(),
            "no rotation means no reopen"
        );
    }

    #[tokio::test]
    async fn a_sheep_with_no_log_file_yet_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::new(
            "",
            vec![sheep(
                "web",
                Some(&dir.path().join("never-started.log")),
                None,
            )],
        );

        let (_config, report) = run(&fake, SystemTime::now())
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
            reopen_fails: Some("api".into()),
            ..Fake::new(
                "max_size = \"1K\"\n",
                vec![
                    sheep("api", Some(&first), None),
                    sheep("web", Some(&second), None),
                ],
            )
        };

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a refused reopen is reported, not returned as an error");

        assert!(report.reopen_failed.is_some(), "the failure is reported");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["api".to_owned()],
            "web was never reached"
        );
        assert!(
            second.exists(),
            "rotating on through a broken reopen multiplies a recoverable state"
        );
    }

    #[tokio::test]
    async fn two_sheep_sharing_one_log_path_rotate_it_once_and_both_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&shared), None),
                sheep("web-1", Some(&shared), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

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
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"1s\"\n",
            vec![sheep("web", Some(&out), None)],
        );
        let (_config, report) = run(&fake, a_year_on()).await.expect("ticked");

        assert_eq!(report.rotated, 1);
    }

    #[tokio::test]
    async fn a_bad_config_section_fails_the_tick_without_touching_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(4096)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"10MB\"\n",
            vec![sheep("web", Some(&out), None)],
        );

        run(&fake, SystemTime::now())
            .await
            .expect_err("10MB is not a spelling shep accepts");

        assert!(
            out.exists(),
            "a config this dog cannot read means it touches nothing"
        );
    }

    // The two guards below are the reason this module builds a protected set
    // at all. Both were mutation-checked: inverting the collision test in
    // `collides_with_a_live_log` makes each of them fail, on the file it
    // says shep is still writing into.

    #[tokio::test]
    async fn a_generation_slot_that_is_another_sheeps_live_log_is_left_alone() {
        // A base with no extension and a base whose extension is all digits
        // generate byte-identical names: `/dir/web` rotates into
        // `/dir/web.1`, which here is the live log of a different sheep.
        let dir = tempfile::tempdir().expect("tempdir");
        let grown = dir.path().join("web");
        let live = dir.path().join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        fs::write(&live, "one's live log\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&live), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0, "neither log may be renamed");
        assert_eq!(report.skipped_collision, 1, "the collision is reported");
        assert_eq!(
            fs::read_to_string(&live).expect("still there"),
            "one's live log\n",
            "renaming a live log out from under an open descriptor is worse than deleting it"
        );
        assert!(grown.exists(), "the grown log is left for a human to sort");
        assert!(fake.reopened.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_generation_slot_reached_through_a_symlinked_directory_is_still_left_alone() {
        // The same collision, except the two sheep were handed the same
        // directory under two spellings: one through a symlink and one not.
        // Path equality reads those as different directories, so a guard
        // comparing `candidate.parent()` to `base.dir` as written sees no
        // collision at all and renames a file another sheep has open.
        //
        // Measured against a real shepherd before this resolved the
        // directory: the rename went through on the first tick, and the
        // sheep holding the open descriptor went on writing into an inode
        // that no longer had a name.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let grown = link.join("web");
        let live = real.join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        fs::write(&live, "one's live log\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&live), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0, "neither log may be renamed");
        assert_eq!(
            report.skipped_collision, 1,
            "a symlinked directory is the same directory"
        );
        assert_eq!(
            fs::read_to_string(&live).expect("still there"),
            "one's live log\n"
        );
        assert!(fake.reopened.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_generation_slot_claimed_by_a_sheep_that_never_started_is_left_alone() {
        // Same collision, except the other sheep is registered and stopped,
        // so its log file does not exist yet. Rotating into its name would
        // hand it a file full of somebody else's log the moment it starts.
        let dir = tempfile::tempdir().expect("tempdir");
        let grown = dir.path().join("web");
        let claimed = dir.path().join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&claimed), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert_eq!(report.skipped_collision, 1);
        assert!(!claimed.exists(), "the stopped sheep's name is still free");
        assert_eq!(report.skipped, 1, "the stopped sheep's own log is skipped");
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_reopens_the_sheep_it_half_rotated() {
        // Propagating the failure straight out would leave the file that DID
        // rotate with no reopen behind it, and shep writing into a renamed
        // file that no later tick ever revisits.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        // The last generation number there is, so the shift has nowhere to
        // go and `rotate` refuses rather than wrapping.
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let failure = run(&fake, SystemTime::now())
            .await
            .expect_err("no generation numbers left");

        assert!(matches!(failure, Error::Exhausted { .. }), "{failure:?}");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["api".to_owned()],
            "the half that rotated still needs its reopen"
        );
        assert!(dir.path().join("api-0-out.log.1").exists());
    }

    #[tokio::test]
    async fn an_empty_log_is_never_rotated_however_old_it_is() {
        // Rotating an empty file frees nothing and produces an empty
        // generation, which then pushes a real one past `keep`.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"1s\"\n",
            vec![sheep("web", Some(&out), None)],
        );
        let (_config, report) = run(&fake, a_year_on()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert!(out.exists());
        assert!(fake.reopened.borrow().is_empty());
    }

    #[test]
    fn a_quiet_tick_says_nothing_and_a_busy_one_says_one_line() {
        // This dog's own logs are in the flock it rotates, so a line per
        // file per interval would make its log the busiest file in the
        // directory it exists to keep small.
        let quiet = Report {
            skipped: 3,
            ..Report::default()
        };
        assert_eq!(quiet.summary(), None);

        let busy = Report {
            rotated: 2,
            compressed: 1,
            ..Report::default()
        };
        let line = busy.summary().expect("a tick that rotated says so");
        assert_eq!(line.lines().count(), 1, "{line}");
        assert_no_dashes(&line);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_sheep_sharing_one_log_under_two_spellings_rotate_it_once_and_both_reopen() {
        // The same file, reached through a symlinked directory by one sheep
        // and directly by the other. Path equality reads those as two files,
        // so the second rename ran against a path the first had already
        // moved, failed NotFound, and the second sheep was never reopened:
        // it went on writing into the renamed generation for good.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let through_link = link.join("web-out.log");
        let direct = real.join("web-out.log");
        fs::write(&direct, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&through_link), None),
                sheep("web-1", Some(&direct), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("one file under two spellings is one rotation, not an error");

        assert_eq!(report.rotated, 1, "one file, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(
            reopened.contains(&"web-1".to_owned()),
            "the second sheep still holds the old descriptor and needs its reopen"
        );
    }

    #[tokio::test]
    async fn a_refused_reopen_still_tidies_the_sheep_that_did_reopen() {
        // api rotates and reopens, then web's reopen is refused. api's
        // generations are safe to tidy: shep has stopped writing into the
        // one that was renamed. Leaving them for api's next rotation is
        // what used to happen, and a sheep that grows slowly waited weeks.
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api-0-out.log");
        let web = dir.path().join("web-0-out.log");
        fs::write(&api, "x".repeat(2048)).expect("seeded");
        fs::write(&web, "x".repeat(2048)).expect("seeded");
        for second in [1, 2] {
            fs::write(
                dir.path()
                    .join(format!("api-0-out.2026-08-20T15-04-0{second}.log")),
                "old\n",
            )
            .expect("seeded");
        }
        let webs_old = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&webs_old, "old\n").expect("seeded");
        let fake = Fake {
            reopen_fails: Some("web".into()),
            ..Fake::new(
                "max_size = \"1K\"\nkeep = 1\ncompress = false\n",
                vec![
                    sheep("api", Some(&api), None),
                    sheep("web", Some(&web), None),
                ],
            )
        };

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a refused reopen is reported, not returned as an error");

        assert!(report.reopen_failed.is_some());
        assert_eq!(report.rotated, 2, "both rotated before the refusal");
        assert_eq!(
            report.deleted, 2,
            "api's two old generations went, and only those"
        );
        assert!(
            webs_old.exists(),
            "web never reopened, so nothing of web's may be touched"
        );
    }

    #[tokio::test]
    async fn a_refused_reopen_leaves_a_shared_path_alone() {
        // web-0 and web-1 write to one file. web-0 reopens, web-1 is
        // refused. web-1 is still writing into the renamed generation
        // through its old descriptor, so that file is not safe to compress
        // or delete however many sheep did reopen it.
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let old = dir.path().join("web-out.2026-08-20T15-04-01.log");
        fs::write(&old, "old\n").expect("seeded");
        let fake = Fake {
            reopen_fails: Some("web-1".into()),
            ..Fake::new(
                "max_size = \"1K\"\nkeep = 1\ncompress = false\n",
                vec![
                    sheep("web-0", Some(&shared), None),
                    sheep("web-1", Some(&shared), None),
                ],
            )
        };

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1);
        assert!(report.reopen_failed.is_some());
        assert_eq!(
            report.deleted, 0,
            "the shared path was left for a later tick"
        );
        assert!(old.exists());
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_tidies_what_did_reopen() {
        // api's out log rotates, its err log cannot (no generation numbers
        // left), api is reopened for the half that moved. The out log's
        // generations are then safe to tidy, and the rename fault is still
        // the error the tick returns.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        fs::write(dir.path().join("api-0-out.log.1"), "old\n").expect("seeded");
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let failure = run(&fake, SystemTime::now())
            .await
            .expect_err("no generation numbers left");

        assert!(matches!(failure, Error::Exhausted { .. }), "{failure:?}");
        assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()]);
        assert!(
            dir.path().join("api-0-out.log.1").exists(),
            "the live file moved to .1"
        );
        assert!(
            !dir.path().join("api-0-out.log.2").exists(),
            "the old generation, shifted to .2, was past keep = 1 and went"
        );
    }

    #[tokio::test]
    async fn a_stop_requested_before_the_tidy_skips_it() {
        // The renames and the reopen are quick and must not be interrupted:
        // a sheep left between the two writes into a file with the wrong
        // name. The gzip afterwards is the slow part, and a stop already
        // requested when the tidy loop starts means none of it runs.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        let older = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&older, "would be gzipped\n").expect("seeded");
        let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);
        let (mut stop, request) = Stop::new();
        request.request();

        let (_config, report) = tick(&fake, "log-rotate", SystemTime::now(), &mut stop)
            .await
            .expect("ticked");

        assert_eq!(report.rotated, 1, "the rename still happened");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["web".to_owned()],
            "and the reopen"
        );
        assert_eq!(report.compressed, 0, "but nothing was compressed");
        assert!(older.exists(), "the older generation is still plain");
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-01.log.gz")
                .exists()
        );
    }

    #[tokio::test]
    async fn a_stop_requested_during_a_gzip_abandons_it() {
        // 32 MiB of noise takes well over half a second to deflate. The stop
        // arrives after a fifth of one, and the tick must come back on the
        // stop rather than on the gzip. Whatever the abandoned thread writes
        // afterwards is the half-written .gz the next pass already
        // overwrites, and the test runtime waits for it on its way out.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        let mut noise = vec![0u8; 32 << 20];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for byte in &mut noise {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-01.log"), &noise).expect("seeded");
        drop(noise);
        let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);
        let (mut stop, request) = Stop::new();
        tokio::spawn(async move {
            tokio::time::sleep(core::time::Duration::from_millis(200)).await;
            request.request();
        });

        let started = std::time::Instant::now();
        let (_config, report) = tick(&fake, "log-rotate", SystemTime::now(), &mut stop)
            .await
            .expect("ticked");
        let took = started.elapsed();

        assert_eq!(report.rotated, 1);
        assert_eq!(report.compressed, 0, "the gzip was abandoned, not counted");
        assert!(
            took < core::time::Duration::from_millis(1500),
            "the tick waited for the gzip rather than the stop: {took:?}"
        );
    }
}
