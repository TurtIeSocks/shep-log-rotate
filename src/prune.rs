//! Compressing rotated generations, and deleting the ones past `keep`.
//!
//! This is the only module in the binary that removes a file. Everything
//! else renames, writes or reads, so every wrong answer made here is
//! somebody's evidence missing from the directory they went to look in after
//! an incident. That asymmetry settles every arguable case on its own: a
//! generation left behind costs disk, and disk is cheap.
//!
//! [`tidy`] runs *after* the reopen, never before. Compressing a large log
//! is not quick, and doing it first would widen the stretch during which
//! shep is still writing through its old descriptor. The caller owns that
//! ordering; this module's half of the bargain is to do nothing slow before
//! it returns.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};

use crate::{config::Config, error::Error, naming::LogPath, rotate::generations};

/// What one pass of [`tidy`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tidied {
    /// Generations gzipped on this pass.
    pub compressed: usize,
    /// Generations deleted on this pass for being past `keep`.
    pub deleted: usize,
    /// Times a generation was left alone because acting on it would have
    /// written over or removed a path the caller named as a live log.
    ///
    /// Anything above zero is a name collision worth reporting rather than
    /// discovering later. A base with no extension generates, byte for byte,
    /// the names a base whose extension is all digits generates: under
    /// `numeric` naming `/var/log/web` produces exactly `/var/log/web.1`,
    /// which may be the live log of a different sheep. Both readings of that
    /// name are correct, so no amount of care in the matcher can separate
    /// them, and the only party that knows which files are being written to
    /// is the shepherd.
    pub skipped_protected: usize,
}

/// Compress and prune the generations of one log.
///
/// The newest generation is left plain in both naming schemes, so the most
/// recent rotation greps without a decompression step first. Everything past
/// `config.keep` is deleted; `keep` counts rotated generations and never the
/// live file, so `keep = 5` leaves five rotated files behind.
///
/// No member of `protected` is ever compressed or deleted, whatever its name
/// says, and none of them counts against `keep` either: a live log that
/// happens to look like a generation is not one of this dog's, so sparing it
/// must not cost a real generation its place. Paths are compared exactly as
/// given, so the caller is the one that has to spell them the way
/// [`std::fs::read_dir`] does.
///
/// # Errors
/// [`Error::Io`], naming the path, if the log directory cannot be listed or
/// if a generation cannot be read, compressed or removed.
pub fn tidy(
    base: &LogPath,
    config: &Config,
    protected: &BTreeSet<PathBuf>,
) -> Result<Tidied, Error> {
    let mut tidied = Tidied::default();

    // Sift the protected paths out before anything counts positions. They
    // are somebody else's live logs that merely read as generations, so they
    // must not take up a slot that `keep` was meant to hold for a real one,
    // and they must not be mistaken for the newest generation and so decide
    // which file stays plain.
    let mut ours: Vec<(PathBuf, bool)> = Vec::new();
    for (path, _order, compressed) in generations(base, config.naming)? {
        if protected.contains(&path) {
            tidied.skipped_protected += 1;
            continue;
        }
        ours.push((path, compressed));
    }

    // Index 0 is the newest and stays plain. Compressing before pruning
    // rather than after is the readable order, not the frugal one: the
    // "newest stays plain" rule sits next to the list it is about.
    if config.compress {
        for (path, compressed) in ours.iter_mut().skip(1) {
            if *compressed {
                continue;
            }
            let target = with_gz(path);
            // The target is a file this pass is about to create or truncate.
            // Truncating a live log is the same harm as deleting one, so it
            // gets the same refusal.
            if protected.contains(&target) {
                tidied.skipped_protected += 1;
                continue;
            }
            compress(path, &target)?;
            *path = target;
            *compressed = true;
            tidied.compressed += 1;
        }
    }

    for (path, _compressed) in ours.iter().skip(config.keep) {
        remove(path)?;
        tidied.deleted += 1;
    }

    Ok(tidied)
}

/// gzip `path` into `target`, then remove `path`.
///
/// The order is the whole point. A crash after the write and before the
/// remove leaves both files, and the pass that follows finds both, skips the
/// `.gz` as already compressed and finishes the job. Removing first would
/// lose the log outright, so the plain file is not touched until the
/// compressed one is on the disk and durable.
///
/// The permissions come across with the bytes. A log written 0600 because it
/// carries something private stays 0600 once compressed; letting it default
/// to whatever the umask allows would quietly widen it.
fn compress(path: &Path, target: &Path) -> Result<(), Error> {
    let mut source = fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let permissions = source
        .metadata()
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();

    let sink = fs::File::create(target).map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;
    let mut encoder = GzEncoder::new(sink, Compression::default());
    io::copy(&mut source, &mut encoder).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sink = encoder.finish().map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;
    // Durable before the plain copy goes, not merely written. Without this
    // the remove can reach the disk first, and a power cut in between takes
    // the log with it.
    sink.sync_all().map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;
    fs::set_permissions(target, permissions).map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;

    remove(path)
}

/// `fs::remove_file`, mapping its error through [`Error::Io`].
///
/// The one line in this binary that destroys data.
fn remove(path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Append a `.gz` suffix to a generation's path.
///
/// Appended to the whole name rather than swapped in as an extension, so
/// `web-0-out.2026-08-20T15-04-05.log` becomes
/// `web-0-out.2026-08-20T15-04-05.log.gz` and every glob an operator already
/// has keeps working.
fn with_gz(path: &Path) -> PathBuf {
    let mut name = path.to_path_buf().into_os_string();
    name.push(".gz");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Naming};
    use std::fs;

    fn config(naming: Naming, keep: usize, compress: bool) -> Config {
        Config {
            naming,
            keep,
            compress,
            ..Config::default()
        }
    }

    #[test]
    fn the_newest_generation_stays_plain_so_it_is_greppable() {
        let dir = tempfile::tempdir().expect("tempdir");
        for stamp in ["15-04-05", "15-04-06", "15-04-07"] {
            fs::write(
                dir.path().join(format!("web-0-out.2026-08-20T{stamp}.log")),
                "body\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.compressed, 2);
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-07.log")
                .exists(),
            "newest stays plain"
        );
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-06.log.gz")
                .exists()
        );
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log.gz")
                .exists()
        );
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log")
                .exists(),
            "the plain copy goes"
        );
    }

    #[test]
    fn compression_round_trips_the_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "line one\nline two\nline three\n";
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-05.log"), body).expect("seeded");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-06.log"),
            "newest\n",
        )
        .expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        let gz =
            fs::File::open(dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz")).expect("open");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(gz), &mut out)
            .expect("decode");
        assert_eq!(
            out, body,
            "a rotator that mangles a log is worse than no rotator"
        );
    }

    #[test]
    fn keep_bounds_the_generations_and_deletes_the_oldest() {
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=6 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                format!("gen{second}\n"),
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 2, false), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.deleted, 4);
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-06.log")
                .exists()
        );
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log")
                .exists()
        );
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-04.log")
                .exists()
        );
        assert!(
            dir.path().join("web-0-out.log").exists(),
            "the live file is never a generation"
        );
    }

    #[test]
    fn pruning_never_deletes_a_file_this_dog_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=6 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                "ours\n",
            )
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

        tidy(&base, &config(Naming::Dated, 1, true), &BTreeSet::new()).expect("tidied");

        for decoy in decoys {
            assert!(
                dir.path().join(decoy).exists(),
                "{decoy} was deleted and must not have been"
            );
        }
    }

    #[test]
    fn numeric_pruning_deletes_above_keep() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=6 {
            fs::write(
                dir.path().join(format!("web-0-out.log.{n}")),
                format!("gen{n}\n"),
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Numeric, 3, false), &BTreeSet::new()).expect("tidied");

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
            fs::write(
                dir.path().join(format!("web-0-out.log.{n}")),
                "old scheme\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 1, true), &BTreeSet::new()).expect("tidied");

        for n in 1..=9 {
            assert!(
                dir.path().join(format!("web-0-out.log.{n}")).exists(),
                "left for the operator"
            );
        }
    }

    #[test]
    fn an_already_compressed_generation_is_not_compressed_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz"),
            "already\n",
        )
        .expect("seeded");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-06.log"),
            "newest\n",
        )
        .expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log.gz.gz")
                .exists()
        );
    }

    #[test]
    fn compression_off_leaves_everything_plain() {
        let dir = tempfile::tempdir().expect("tempdir");
        for stamp in ["15-04-05", "15-04-06"] {
            fs::write(
                dir.path().join(format!("web-0-out.2026-08-20T{stamp}.log")),
                "body\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, false), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log")
                .exists()
        );
    }

    #[test]
    fn a_protected_live_log_is_never_compressed_or_deleted() {
        // `/var/log/web` has no extension, so under numeric naming it
        // generates the exact names `web.1`, `web.2` and so on. Every one of
        // those may be the live log of a different sheep, whose configured
        // path is literally `/var/log/web.2`. Both readings of the name are
        // correct and the matcher cannot choose between them, so the caller
        // hands over the paths the shepherd says it is writing to.
        //
        // `web.2` sits at index 1 here, which is where the test bites: with
        // `keep = 1` and compression on, index 1 would otherwise be gzipped
        // and then deleted on this same pass.
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=5 {
            fs::write(dir.path().join(format!("web.{n}")), format!("gen{n}\n")).expect("seeded");
        }
        fs::write(dir.path().join("web"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web")).expect("splits");
        let live_elsewhere = dir.path().join("web.2");
        let protected = BTreeSet::from([live_elsewhere.clone()]);

        let tidied = tidy(&base, &config(Naming::Numeric, 1, true), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(
            fs::read_to_string(&live_elsewhere).expect("a live log must survive"),
            "gen2\n",
            "contents intact"
        );
        assert!(
            !dir.path().join("web.2.gz").exists(),
            "a live log must not be compressed either"
        );
        // Sparing it must not cost a real generation its place: `keep = 1`
        // still means one of this dog's own generations, and `web.1` is it.
        assert!(dir.path().join("web.1").exists(), "the newest stays plain");
        for n in 3..=5 {
            assert!(!dir.path().join(format!("web.{n}")).exists());
            assert!(!dir.path().join(format!("web.{n}.gz")).exists());
        }
    }

    #[test]
    fn a_protected_generation_past_keep_survives_the_prune() {
        // The same guard with compression off, so nothing but the delete
        // path can be what spares the file.
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=4 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                format!("gen{second}\n"),
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");
        let oldest = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        let protected = BTreeSet::from([oldest.clone()]);

        let tidied = tidy(&base, &config(Naming::Dated, 1, false), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(tidied.deleted, 2, "the two genuine generations past keep");
        assert_eq!(fs::read_to_string(&oldest).expect("survives"), "gen1\n");
    }

    #[test]
    fn a_live_log_is_not_truncated_by_being_some_generations_gz_target() {
        // Compression creates its target, which truncates whatever is
        // already there. If that target is a live log, truncating it is the
        // same harm as deleting it, so it gets the same refusal and the
        // plain generation is simply left uncompressed.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-01.log"),
            "ours\n",
        )
        .expect("seeded");
        let live_elsewhere = dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz");
        fs::write(&live_elsewhere, "somebody is writing here\n").expect("seeded");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-02.log"),
            "newest\n",
        )
        .expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");
        let protected = BTreeSet::from([live_elsewhere.clone()]);

        let tidied = tidy(&base, &config(Naming::Dated, 5, true), &protected).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert_eq!(
            fs::read_to_string(&live_elsewhere).expect("untouched"),
            "somebody is writing here\n"
        );
        assert!(
            dir.path()
                .join("web-0-out.2026-08-20T15-04-01.log")
                .exists(),
            "and the generation it could not compress stays plain"
        );
        // Two refusals over one file, because that file reaches this module
        // twice: once as a candidate generation of its own, and once as the
        // name a different generation wanted to write to. The count is of
        // refusals rather than of files, and either one alone is worth the
        // caller's attention.
        assert_eq!(tidied.skipped_protected, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_shaped_like_a_generation_is_never_followed() {
        // Following it would compress and then delete a file in a directory
        // this dog was never pointed at.
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = dir.path().join("elsewhere.log");
        fs::write(&elsewhere, "somebody else's data\n").expect("seeded");
        let link = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("linked");
        for second in 2..=4 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                "ours\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 1, true), &BTreeSet::new()).expect("tidied");

        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "the link itself must still be there"
        );
        assert_eq!(
            fs::read_to_string(&elsewhere).expect("the target survives"),
            "somebody else's data\n"
        );
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-01.log.gz")
                .exists(),
            "and it was not compressed through the link"
        );
    }

    #[test]
    fn a_directory_shaped_like_a_generation_is_never_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let collision = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::create_dir(&collision).expect("created");
        fs::write(collision.join("notes.txt"), "an operator's own\n").expect("seeded");
        for second in 2..=4 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                "ours\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 1, true), &BTreeSet::new()).expect("tidied");

        assert!(collision.is_dir(), "still a directory");
        assert_eq!(
            fs::read_to_string(collision.join("notes.txt")).expect("still there"),
            "an operator's own\n"
        );
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-01.log.gz")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn compression_keeps_the_source_files_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let private = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&private, "a token, probably\n").expect("seeded");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("chmod");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-02.log"),
            "newest\n",
        )
        .expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        let mode = fs::metadata(dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz"))
            .expect("the compressed copy")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a log kept private must not widen when it is compressed"
        );
    }

    #[test]
    fn keep_larger_than_the_generation_count_deletes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        for second in 1..=3 {
            fs::write(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log")),
                "ours\n",
            )
            .expect("seeded");
        }
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 10, false), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.deleted, 0);
        for second in 1..=3 {
            assert!(
                dir.path()
                    .join(format!("web-0-out.2026-08-20T15-04-0{second}.log"))
                    .exists()
            );
        }
    }

    #[test]
    fn a_base_with_nothing_to_tidy_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied, Tidied::default());
        assert!(
            dir.path().join("web-0-out.log").exists(),
            "and the live file is still there"
        );

        // A sheep that is registered but never started has no log directory
        // at all yet. That is not a failure either.
        let never_started = dir.path().join("not-yet").join("web-0-out.log");
        let base = LogPath::split(&never_started).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied, Tidied::default());
    }

    #[test]
    fn a_crash_between_the_gz_and_the_remove_is_finished_on_the_next_pass() {
        // Compression writes `{path}.gz` and only then removes the plain
        // file, so a crash in between leaves both. Both read as the same
        // generation, so both come back from the listing, and this pass
        // finishes what the last one started.
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "the log that nearly got away\n";
        fs::write(dir.path().join("web-0-out.2026-08-20T15-04-01.log"), body).expect("seeded");
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz"),
            "half a gzip stream",
        )
        .expect("seeded");
        // A newer generation, so the interrupted pair cannot land on index 0
        // and be spared as "the newest stays plain" whichever order the
        // directory happens to list them in.
        fs::write(
            dir.path().join("web-0-out.2026-08-20T15-04-02.log"),
            "newest\n",
        )
        .expect("seeded");
        fs::write(dir.path().join("web-0-out.log"), "live\n").expect("seeded");
        let base = LogPath::split(&dir.path().join("web-0-out.log")).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &BTreeSet::new()).expect("tidied");

        assert_eq!(tidied.compressed, 1, "the plain half, and only it");
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-01.log")
                .exists()
        );
        let gz =
            fs::File::open(dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz")).expect("open");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(gz), &mut out)
            .expect("decode");
        assert_eq!(out, body, "the partial stream was replaced, not appended");
    }
}
