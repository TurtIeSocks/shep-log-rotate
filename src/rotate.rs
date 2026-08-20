//! Renames a grown log. Does the rename and nothing else: no reopen, no
//! compression, no pruning. Those belong to the caller, in that order, and
//! keeping them out of here is what makes the rename-to-reopen window - the
//! stretch during which shep is still writing through its old descriptor -
//! as short as it can be.

use std::{fs, io, path::Path, path::PathBuf, time::SystemTime};

use crate::{
    config::Naming,
    error::Error,
    naming::{LogPath, Order, dated_name, match_generation, numeric_name, stamp_utc},
};

/// Every generation this dog created for `base`, newest first.
///
/// A missing log directory is not an error: a sheep that is registered but
/// never started has no log file yet, so this returns an empty list rather
/// than failing.
pub fn generations(base: &LogPath, naming: Naming) -> Result<Vec<(PathBuf, Order, bool)>, Error> {
    let entries = match fs::read_dir(&base.dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: base.dir.clone(),
                source,
            });
        }
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: base.dir.clone(),
            source,
        })?;
        // Only a regular file can be a generation this dog wrote.
        // match_generation matches by name alone, so a directory (or
        // anything else) that merely collides with a generation's name shape
        // - an operator's own subdirectory, say - would otherwise be swept
        // up and renamed right along with real generations. Filtering by
        // type here, before the name check gets a chance to run, is what
        // keeps that collision from mattering. An entry whose type cannot be
        // read is skipped for the same reason as a rejected name: the
        // module's own rule is that an arguable case is not a match.
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
        counter += 1;
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
        let next = if compressed {
            with_gz(numeric_name(base, n + 1))
        } else {
            numeric_name(base, n + 1)
        };
        rename(&path, &next)?;
    }

    let target = numeric_name(base, 1);
    rename(&base.live(), &target)?;
    Ok(target)
}

/// Append a `.gz` suffix to a path already built by [`numeric_name`].
fn with_gz(path: PathBuf) -> PathBuf {
    let mut name = path.into_os_string();
    name.push(".gz");
    PathBuf::from(name)
}

/// `fs::rename`, mapping its error through `Error::Io` naming the source
/// path - the one a caller is most likely investigating on disk.
fn rename(from: &Path, to: &Path) -> Result<(), Error> {
    fs::rename(from, to).map_err(|source| Error::Io {
        path: from.to_path_buf(),
        source,
    })
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

    /// What a shift failure leaves on disk, and whether shep is still safe
    /// mid-shift.
    ///
    /// A directory-shaped blocker at a generation's target name was the
    /// first thing tried here, and it does not work: `generations` matches
    /// purely by name, so a directory named `web-0-out.log.3` is picked up
    /// as generation 3 itself and gets shifted out of the way like any other
    /// generation, no different from a real one. Forcing a failure needs a
    /// cause external to any single file's identity, so this revokes write
    /// permission on the directory instead - the same permission every
    /// rename in the loop needs, so the very first one attempted (the oldest
    /// generation) is the one that fails. Not root: root ignores the
    /// permission bits this test relies on.
    ///
    /// `fs::rename` is one atomic syscall, so a failed one is a no-op: the
    /// source is exactly where it started. Nothing before this test's single
    /// failure had a chance to commit, but the reasoning generalises from the
    /// source, sequential loop in `rotate_numeric` - `?` returns on the first
    /// `Err`, so every rename queued after the failing one, including the
    /// final live-to-`.1` rename, simply never runs. Whatever renames *did*
    /// commit before the failure stay committed; `rotate` does not roll them
    /// back.
    #[test]
    #[cfg(unix)]
    fn a_shift_failure_leaves_the_live_file_in_place_for_the_next_tick_to_retry() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "web-0-out.log.1", "gen1\n");
        let live = seed(dir.path(), "web-0-out.log", "live\n");
        let base = LogPath::split(&live).expect("splits");

        let original_perms = fs::metadata(dir.path()).expect("stat").permissions();
        fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let outcome = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH);
        fs::set_permissions(dir.path(), original_perms).expect("restore chmod for cleanup");

        let err = outcome.expect_err(
            "a read-only directory must surface as an error, not silently do nothing (skip this test when run as root)",
        );
        assert!(
            matches!(err, Error::Io { .. }),
            "a failed fs call must map through Error::Io: {err:?}"
        );

        // Nothing moved: the source generation and the live file are both
        // exactly where they started, so shep's already-open descriptor is
        // still writing to the path it thinks it is.
        assert_eq!(
            fs::read_to_string(dir.path().join("web-0-out.log.1")).expect("read"),
            "gen1\n"
        );
        assert!(
            live.exists(),
            "the live file must stay put on a failed shift"
        );
        assert_eq!(fs::read_to_string(&live).expect("read"), "live\n");

        // The failure is not self-healing, but it is not damaging either:
        // once whatever blocked the rename clears, the exact same call
        // succeeds, because nothing was left half-done to confuse it.
        let retried = rotate(&base, Naming::Numeric, std::time::SystemTime::UNIX_EPOCH)
            .expect("the next tick recovers once the obstruction is gone");
        assert!(!live.exists());
        assert_eq!(fs::read_to_string(&retried).expect("read"), "live\n");
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
