//! Renames a grown log. Does the rename and nothing else: no reopen, no
//! compression, no pruning. Those belong to the caller, in that order, and
//! keeping them out of here is what makes the rename-to-reopen window - the
//! stretch during which shep is still writing through its old descriptor -
//! as short as it can be.

use std::{fs, io, path::Path, path::PathBuf, time::SystemTime};

use crate::{
    config::Naming,
    error::Error,
    naming::{LogPath, Order, dated_name, match_generation, numeric_name, stamp_utc, with_gz},
};

/// Every generation this dog created for `base`, newest first.
///
/// A missing log directory is not an error: a sheep that is registered but
/// never started has no log file yet, so this returns an empty list rather
/// than failing.
pub fn generations(base: &LogPath, naming: Naming) -> Result<Vec<(PathBuf, Order, bool)>, Error> {
    let entries = match fs::read_dir(&base.dir) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        listing => listing.map_err(Error::io_at(&base.dir))?,
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::io_at(&base.dir))?;
        // Only a regular file can be a generation this dog wrote.
        // match_generation matches by name alone, so anything else that
        // merely collides with a generation's name shape - an operator's own
        // subdirectory, or a symlink, say - would otherwise be swept up and
        // renamed right along with real generations. Filtering by type here,
        // before the name check gets a chance to run, is what keeps that
        // collision from mattering.
        //
        // The symlink case is deliberate, not a side effect of reusing this
        // same check: `DirEntry::file_type` mirrors `symlink_metadata`, so it
        // does not follow a symlink to see what it points at, and a
        // symlink's `is_file()` is `false` regardless of its target. shep
        // never writes a generation as a symlink, so refusing to treat one
        // as a match costs nothing real.
        //
        // An entry whose type cannot be read at all is skipped for the same
        // reason as a rejected name: the module's own rule is that an
        // arguable case is not a match.
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        // A non-UTF-8 file name cannot have come from dated_name/numeric_name
        // (both build their names from base.stem and base.ext, already
        // UTF-8), so match_generation could never match it. Skip rather than
        // fail: an unrelated file with a non-UTF-8 name is not this dog's
        // problem to name in an error.
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((order, compressed)) = match_generation(base, naming, &file_name) else {
            continue;
        };
        found.push((base.dir.join(&file_name), order, compressed));
    }

    found.sort_by(|a, b| Order::newest_first(&a.1, &b.1));
    Ok(found)
}

/// Rename the live file at `base` to its next generation, returning the path
/// it now has.
///
/// Assumes exactly one rotator is acting on `base`'s directory at a time.
/// Finding a free target name and renaming into it are two separate
/// syscalls, not one atomic check-and-rename (dated: `Path::exists` then
/// `fs::rename`; numeric: `generations` then a sequence of `fs::rename`s).
/// A second concurrent `rotate()` on the same `base` can choose the same
/// target in the gap between those calls, and POSIX `fs::rename` silently
/// replaces an existing destination rather than refusing - so the loser's
/// generation is destroyed, not merely delayed.
///
/// This crate does not guard against that, deliberately: the realistic
/// deployment is one rotator per `$SHEP_HOME`, and shep already enforces at
/// most one running instance of an adopted dog by name (`start_dog` is
/// idempotent), so two concurrent rotators would only happen if an operator
/// hand-runs a second `shep-log-rotate` binary alongside the one shep is
/// already supervising. Knowing "is this dog already running" is shep's
/// job, not this crate's - this crate has no visibility into what else is
/// running, so it has nothing to check even if it wanted to.
pub fn rotate(base: &LogPath, naming: Naming, now: SystemTime) -> Result<PathBuf, Error> {
    match naming {
        Naming::Dated => rotate_dated(base, now),
        Naming::Numeric => rotate_numeric(base),
    }
}

/// Dated rotation: find the first free `{stamp}[.{counter}]` slot and rename
/// the live file into it. Existence is checked one candidate at a time
/// because a same-second collision is the rare case, not the common one.
fn rotate_dated(base: &LogPath, now: SystemTime) -> Result<PathBuf, Error> {
    let stamp = stamp_utc(now);
    let mut counter = 0;
    let target = loop {
        let candidate = dated_name(base, &stamp, counter);
        if !candidate.exists() {
            break candidate;
        }
        // Every counter up to and including u32::MAX is already occupied
        // for this second. Wrapping back to 0 would retest a slot already
        // known occupied, forever, so refuse instead.
        counter = counter
            .checked_add(1)
            .ok_or_else(|| Error::Exhausted { path: base.live() })?;
    };
    rename(&base.live(), &target)?;
    Ok(target)
}

/// Numeric rotation: shift every existing generation up by one, oldest
/// first, then rename the live file into the now-free `.1`.
///
/// `generations` comes back newest first (`Order`'s `Ord` is newest-first for
/// both variants), which is the wrong direction for a shift - renaming `.1`
/// to `.2` before `.2` has moved to `.3` would overwrite `.2`. Reversing
/// gives oldest first, so each rename lands on a slot nothing needs anymore
/// by the time it runs.
fn rotate_numeric(base: &LogPath) -> Result<PathBuf, Error> {
    let mut found = generations(base, Naming::Numeric)?;
    found.reverse();

    for (path, order, compressed) in found {
        let Order::Numeric { n } = order else {
            unreachable!("generations(_, Naming::Numeric) only ever returns Order::Numeric")
        };
        // naming.rs's own numeric_generations_compress_and_stop_at_u32 proves
        // n == u32::MAX is a generation match_generation legitimately
        // recognises, so this is reachable by a planted file, not only in
        // theory. Wrapping would produce `.0` - not a name match_generation
        // ever matches back, so that generation would be orphaned rather
        // than pruned - so refuse instead of wrapping.
        let next_n = n
            .checked_add(1)
            .ok_or_else(|| Error::Exhausted { path: path.clone() })?;
        let next = if compressed {
            with_gz(&numeric_name(base, next_n))
        } else {
            numeric_name(base, next_n)
        };
        rename(&path, &next)?;
    }

    let target = numeric_name(base, 1);
    rename(&base.live(), &target)?;
    Ok(target)
}

/// `fs::rename`, mapping its error through `Error::Io` naming the source
/// path - the one a caller is most likely investigating on disk.
fn rename(from: &Path, to: &Path) -> Result<(), Error> {
    fs::rename(from, to).map_err(Error::io_at(from))
}

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

    /// Serialises the two tests that have to run from inside a temporary
    /// directory. The working directory belongs to the process, not to a
    /// thread, and the default test harness runs these in parallel.
    ///
    /// Poisoning is recovered from rather than propagated: a panic in one
    /// of these tests must not turn the other into a second failure
    /// reporting the first one's crime.
    static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `body` with the process working directory set to `dir`, putting
    /// the old one back before returning whatever `body` produced.
    ///
    /// `body` does the file system work and nothing else; the assertions
    /// belong to the caller, after the working directory is back. A panic
    /// inside here would leave every later test looking at the wrong
    /// directory.
    fn within<T>(dir: &std::path::Path, body: impl FnOnce() -> T) -> T {
        let _guard = CWD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::current_dir().expect("a working directory");
        std::env::set_current_dir(dir).expect("moved into the temporary directory");
        let outcome = body();
        std::env::set_current_dir(previous).expect("moved back");
        outcome
    }

    #[test]
    fn a_base_with_no_directory_component_still_sees_its_generations() {
        // `Path::new("web-0-out.log").parent()` is `Some("")`, and
        // `fs::read_dir("")` is `NotFound`, which this module swallows as
        // "this sheep has never started". A base spelled without a
        // directory component would therefore report no generations
        // however many are on disk. `LogPath::split` reads that empty
        // parent as `.` so this cannot happen.
        let dir = tempfile::tempdir().expect("tempdir");
        let found = within(dir.path(), || {
            seed(std::path::Path::new("."), "web-0-out.log.1", "older\n");
            seed(std::path::Path::new("."), "web-0-out.log.2", "oldest\n");
            seed(std::path::Path::new("."), "web-0-out.log", "live\n");
            let base = LogPath::split(std::path::Path::new("web-0-out.log")).expect("splits");
            generations(&base, Naming::Numeric).expect("listed")
        });

        assert_eq!(
            found.len(),
            2,
            "both generations are on disk and both are ours: {found:?}"
        );
    }

    #[test]
    fn a_bare_named_base_does_not_overwrite_its_own_first_generation() {
        // The consequence of the above, measured rather than argued. With
        // an unreadable directory the numeric shift believes there is
        // nothing to move, so the second rotation renames the live file
        // straight over generation 1 and that log is gone.
        let dir = tempfile::tempdir().expect("tempdir");
        let (first, second) = within(dir.path(), || {
            seed(std::path::Path::new("."), "web-0-out.log", "first\n");
            let base = LogPath::split(std::path::Path::new("web-0-out.log")).expect("splits");
            rotate(&base, Naming::Numeric, SystemTime::UNIX_EPOCH).expect("rotates");
            seed(std::path::Path::new("."), "web-0-out.log", "second\n");
            rotate(&base, Naming::Numeric, SystemTime::UNIX_EPOCH).expect("rotates again");
            (
                fs::read_to_string(dir.path().join("web-0-out.log.2")).ok(),
                fs::read_to_string(dir.path().join("web-0-out.log.1")).ok(),
            )
        });

        assert_eq!(
            first.as_deref(),
            Some("first\n"),
            "generation 1 was shifted to .2, not overwritten"
        );
        assert_eq!(second.as_deref(), Some("second\n"));
    }

    #[test]
    fn dated_rotation_renames_the_live_file_and_leaves_the_path_free() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = seed(dir.path(), "web-0-out.log", "one\ntwo\n");
        let base = LogPath::split(&live).expect("splits");
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);

        let rotated = rotate(&base, Naming::Dated, at).expect("rotates");

        assert!(
            !live.exists(),
            "the live path must be free for shep to reopen"
        );
        assert_eq!(fs::read_to_string(&rotated).expect("read"), "one\ntwo\n");
        assert!(
            rotated
                .file_name()
                .expect("name")
                .to_str()
                .expect("utf8")
                .ends_with(".log"),
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

        assert_ne!(
            first, second,
            "the first generation must not be overwritten"
        );
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
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"),
            "live\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.2")).expect("read"),
            "gen1\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.3")).expect("read"),
            "gen2\n"
        );
    }

    #[test]
    fn the_numeric_shift_carries_the_gz_suffix_along() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.1.gz", "compressed\n");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect("rotates");

        assert!(
            dir.path().join("web-0-out.log.2.gz").exists(),
            "a compressed generation keeps its suffix"
        );
        assert!(!dir.path().join("web-0-out.log.1.gz").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"),
            "live\n"
        );
    }

    #[test]
    fn the_shift_never_touches_a_file_this_dog_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.0", "newsyslog wrote this\n");
        seed(
            dir.path(),
            "notes-about-web-0-out.log.1",
            "an operator wrote this\n",
        );
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

        assert_eq!(
            found.len(),
            2,
            "the live file and the decoy are not generations"
        );
        assert!(found[0].0.to_str().expect("utf8").contains("15-04-06"));
        assert!(found[1].0.to_str().expect("utf8").contains("15-04-05"));
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_failing() {
        let base =
            LogPath::split(std::path::Path::new("/nonexistent/web-0-out.log")).expect("splits");
        assert!(
            generations(&base, Naming::Dated)
                .expect("no error")
                .is_empty()
        );
    }

    /// What a shift failure leaves on disk when it lands on the *second*
    /// rename attempted, not the first - proving an earlier commit survives
    /// a later failure in the same shift, not just that a single-rename
    /// shift leaves the live file alone.
    ///
    /// Real generations at `.2` and `.4` (a deliberate gap at `.1` and
    /// `.3`), plus a plain directory planted at `.3`. Oldest first, the
    /// shift attempts `.4` -> `.5` before `.2` -> `.3`: `.5` is free, so the
    /// first rename commits; `.3` is the planted directory, so the second
    /// lands on it and fails (`fs::rename` refuses to replace a directory
    /// with a file). The gap at `.1`/`.3` is what makes this possible at
    /// all - a blocker can only sit on a name no *real* generation currently
    /// occupies, since the two can't coexist at one path. See
    /// `a_directory_that_matches_the_name_shape_is_not_a_generation` for the
    /// simpler one-rename version of that same constraint.
    #[test]
    fn a_later_shift_failure_leaves_the_earlier_commit_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.2", "gen2\n");
        seed(dir.path(), "web-0-out.log.4", "gen4\n");
        fs::create_dir(dir.path().join("web-0-out.log.3")).expect("planted blocker");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        let err = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH)
            .expect_err("the blocked rename must surface, not get swallowed");
        assert!(
            matches!(err, Error::Io { .. }),
            "a failed fs call must map through Error::Io: {err:?}"
        );

        // The earlier step (oldest generation, .4 -> .5) already committed
        // before the failure and stays committed - rotate does not roll it
        // back just because a later step in the same shift failed.
        assert!(!dir.path().join("web-0-out.log.4").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.5")).expect("read"),
            "gen4\n"
        );

        // The failing step itself (.2 -> .3) left its source untouched -
        // fs::rename is a single atomic syscall, so a failed one is a no-op.
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.2")).expect("read"),
            "gen2\n"
        );
        assert!(
            dir.path().join("web-0-out.log.3").is_dir(),
            "the blocker is unaffected"
        );

        // The live file's own rename is queued after every generation shift,
        // so it never even ran: shep's already-open descriptor is still
        // writing to the path it thinks it is.
        assert!(
            live.exists(),
            "the live file must stay put on a failed shift"
        );
        assert_eq!(fs::read_to_string(&live).expect("read"), "live\n");

        // Not self-healing, but not damaging either: once the obstruction is
        // gone, the exact same call succeeds, because nothing was left
        // half-done to confuse it.
        fs::remove_dir(dir.path().join("web-0-out.log.3")).expect("clear the blocker");
        let retried = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH)
            .expect("the next tick recovers once the obstruction is gone");
        assert!(!live.exists());
        assert_eq!(fs::read_to_string(&retried).expect("read"), "live\n");
    }

    /// The `checked_add` guard in `rotate_numeric`, exercised for real:
    /// naming.rs's own `numeric_generations_compress_and_stop_at_u32` proves
    /// `.{u32::MAX}` is a name `match_generation` legitimately recognises,
    /// so a single planted file reaches the overflow path directly - no
    /// need to seed the 2^32 generations that would be needed to reach it
    /// by counting up from `.1`. (The dated side's `counter` guard takes
    /// the identical `checked_add`/`ok_or_else` shape but has no equivalent
    /// shortcut - reaching it needs every counter from 0 to `u32::MAX` to
    /// already exist - so it is exercised only by inspection, not a test.)
    #[test]
    fn a_numeric_generation_at_u32_max_refuses_rather_than_wraps() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(
            dir.path(),
            &format!("web-0-out.log.{}", u32::MAX),
            "oldest\n",
        );
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        let err = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH)
            .expect_err("incrementing u32::MAX must refuse, not wrap around to .0");
        assert!(
            matches!(err, Error::Exhausted { .. }),
            "must be Error::Exhausted, not swallowed or misreported: {err:?}"
        );

        // Refusing leaves everything exactly where it was: no half-applied
        // wrap, and critically no orphaned `.0` that match_generation could
        // never find again.
        assert_eq!(
            fs::read_to_string(dir.path().join(format!("web-0-out.log.{}", u32::MAX)))
                .expect("read"),
            "oldest\n"
        );
        assert!(
            !dir.path().join("web-0-out.log.0").exists(),
            "must not have wrapped to .0"
        );
        assert!(live.exists(), "the live file must stay put on refusal");
    }

    /// Same decision as
    /// `a_directory_that_matches_the_name_shape_is_not_a_generation`, made
    /// explicit for the other entry type `file_type()` distinguishes.
    /// `DirEntry::file_type` mirrors `symlink_metadata`, so it does not
    /// follow a symlink to see what it points at - a symlink's `is_file()`
    /// is `false` regardless of its target, so it is excluded by the same
    /// check, deliberately: shep never writes a generation as a symlink.
    ///
    /// Unlike a directory, a symlink does not *block* a rename that lands on
    /// it - `fs::rename` treats it like any other non-directory destination
    /// and replaces the directory entry outright. The symlink disappears;
    /// whatever it pointed at is untouched, since rename only ever touches
    /// the directory entry, never the target.
    #[test]
    #[cfg(unix)]
    fn a_symlink_that_matches_the_name_shape_is_not_a_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = seed(dir.path(), "elsewhere.txt", "not a generation\n");
        std::os::unix::fs::symlink(&target, dir.path().join("web-0-out.log.1")).expect("symlink");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        let found = generations(&base, Naming::Numeric).expect("listed");
        assert!(
            found.is_empty(),
            "a symlink is not a generation even when its name matches: {found:?}"
        );

        rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect("rotates");
        assert!(!dir.path().join("web-0-out.log.1").is_symlink());
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"),
            "live\n"
        );
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "not a generation\n"
        );
    }

    /// A directory can collide with a generation's name shape (an
    /// operator's own subdirectory, say). `match_generation` in naming.rs
    /// matches by name alone, so without a type check here it would be swept
    /// into the shift and renamed away like real generation data - exactly
    /// the mistake naming.rs's own docs warn about ("a false match costs an
    /// operator their data"), just made one layer up from where those docs
    /// live.
    #[test]
    fn a_directory_that_matches_the_name_shape_is_not_a_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("web-0-out.log.1")).expect("planted directory");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        let found = generations(&base, Naming::Numeric).expect("listed");
        assert!(
            found.is_empty(),
            "a directory is not a generation even when its name matches: {found:?}"
        );

        // With the directory excluded from the shift, it still legitimately
        // occupies the live file's own target name (`.1`), and fs::rename
        // refuses to replace a directory with a file. Surfacing that as an
        // error and leaving both exactly where they were is the safe
        // outcome - the alternative would be silently relocating an
        // operator's directory to make room.
        let err = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH).expect_err(
            "a directory occupying the target name must block the rename, not be displaced",
        );
        assert!(matches!(err, Error::Io { .. }));
        assert!(
            dir.path().join("web-0-out.log.1").is_dir(),
            "the planted directory must be untouched"
        );
        assert!(
            live.exists(),
            "the live file must stay put rather than vanish into a failed rename"
        );
    }
}
