//! A set of files that counts two spellings of one file once.
//!
//! Three guards consult one of these, and all three compare by file name
//! within a directory. `prune::tidy` refuses to compress or delete a member.
//! `tick` refuses to rotate a base into a member's name, and refuses to
//! rename a live log it has already renamed this tick. Path equality is not
//! enough for any of them: it normalises `.` away but not `..`, and it knows
//! nothing about symlinks, so `/var/log/sub/../web.1` and a `/var/log` that
//! is itself a link both slip past a whole-path comparison with nothing said
//! about it. A guard that silently does nothing is worse than no guard at
//! all, because the caller believes the file is safe. Two sheep handed the
//! same log directory under two spellings, one through a link and one not,
//! is the ordinary case rather than a trick: `/var/log` is a link on macOS.
//!
//! So every directory goes through [`ResolvedDir`] before anything is
//! compared, and it goes through once. `canonicalize` is a syscall, and the
//! guards used to resolve every member against every base, a count that
//! grew with the square of the flock. Building the set resolves each
//! distinct directory once and remembers the answer, so a lookup about a
//! directory the set was built from costs nothing.
//!
//! Remembering is also what keeps the two sides in step. The set is built
//! when the tick starts, and the lookups run after the renames and the
//! reopens, and a link can move in between: a deploy flipping `logs` from
//! one release to the next is the ordinary case. A key resolved at build
//! time and a lookup resolved later would miss each other, and the guard
//! would go quiet on a file that is still live. [`FileSet::resolve`] answers
//! for a remembered spelling the way it did when the set was built, so
//! every guard compares against the same picture of the disk.
//!
//! What resolving does not cover: a hard link, a log file that is itself a
//! symlink to another sheep's log, and case on a case-insensitive
//! filesystem, where `/var/log/WEB.1` and `/var/log/web.1` are one file
//! with two spellings. Only the directory is resolved, never the file, so
//! two names in one directory are two members. Every path here comes from
//! one `ListFlock` answer, so each of these needs the shepherd to have
//! reported one file two ways before it can bite.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::naming::LogPath;

/// A directory as `canonicalize` spells it, or as written when it will not
/// resolve.
///
/// The type is the guard. A [`FileSet`] lookup takes one of these rather
/// than a `&Path`, so a caller cannot compare an unresolved spelling against
/// resolved keys and get a silent "not a member" for a file that is one.
///
/// A directory that will not resolve is kept as written, because failing to
/// resolve something is not a reason to stop protecting it: a sheep that is
/// registered but never started has no log directory yet, and its paths
/// still have to be honoured.
///
/// There is deliberately no special case for the empty path. Every
/// directory that reaches this type came through [`LogPath::split`], which
/// already reads a missing directory component as `.`, and a second copy of
/// that rule would be a copy that can go missing from a third place.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedDir(PathBuf);

impl ResolvedDir {
    /// Resolve `dir` as the disk spells it right now. One syscall.
    ///
    /// A guard asking about a directory the flock reported goes through
    /// [`FileSet::resolve`] instead, which remembers this answer from when
    /// the set was built and so agrees with the keys it is looking up.
    pub fn of(dir: &Path) -> Self {
        Self(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()))
    }
}

/// Files, grouped by the directory they live in.
///
/// Membership is by resolved directory and file name, so two spellings of
/// one file are one member. See the module docs for what that does and does
/// not cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSet {
    /// File names per directory.
    by_dir: BTreeMap<ResolvedDir, BTreeSet<String>>,
    /// Every directory spelling this set was built from, and what it
    /// resolved to at the time.
    resolved: BTreeMap<PathBuf, ResolvedDir>,
}

impl FileSet {
    /// Index `paths`, resolving each distinct directory once.
    ///
    /// A path with no file name, or one this dog cannot spell, is dropped
    /// rather than kept: [`LogPath::split`] says what that covers. Every
    /// name this dog builds is UTF-8 and has a file name, so nothing it
    /// could ever look up would match such a path, and keeping it would
    /// protect nothing.
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Self {
        // Grouped by the directory as written first, so that each distinct
        // spelling is resolved once rather than once per path in it.
        let mut by_written_dir: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
        for path in paths {
            let Some(base) = LogPath::split(path) else {
                continue;
            };
            let name = base.live_name();
            by_written_dir.entry(base.dir).or_default().insert(name);
        }
        let mut set = Self::default();
        for (dir, names) in by_written_dir {
            let resolved = ResolvedDir::of(&dir);
            set.by_dir
                .entry(resolved.clone())
                .or_default()
                .extend(names);
            set.resolved.insert(dir, resolved);
        }
        set
    }

    /// Resolve `dir` the way this set did when it was built, or as the disk
    /// spells it now for a spelling the set was not built from.
    ///
    /// Every base a tick acts on came out of the same flock listing the set
    /// was built from, so the first arm is the one that runs, and it costs
    /// a map lookup rather than a syscall.
    pub fn resolve(&self, dir: &Path) -> ResolvedDir {
        self.resolved
            .get(dir)
            .cloned()
            .unwrap_or_else(|| ResolvedDir::of(dir))
    }

    /// Add one file.
    pub fn insert(&mut self, dir: ResolvedDir, name: String) {
        self.by_dir.entry(dir).or_default().insert(name);
    }

    /// Whether `name` in `dir` is a member.
    pub fn contains(&self, dir: &ResolvedDir, name: &str) -> bool {
        self.names_in(dir).contains(name)
    }

    /// Every member living in `dir`.
    pub fn names_in(&self, dir: &ResolvedDir) -> &BTreeSet<String> {
        static NONE: BTreeSet<String> = BTreeSet::new();
        self.by_dir.get(dir).unwrap_or(&NONE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dir(path: &Path) -> ResolvedDir {
        ResolvedDir::of(path)
    }

    #[test]
    fn a_member_is_found_by_its_directory_and_name() {
        let set = FileSet::from_paths([Path::new("/nonexistent/log/web.1")]);
        assert!(set.contains(&dir(Path::new("/nonexistent/log")), "web.1"));
        assert!(!set.contains(&dir(Path::new("/nonexistent/log")), "web.2"));
        assert!(!set.contains(&dir(Path::new("/nonexistent/other")), "web.1"));
    }

    #[test]
    fn a_directory_that_does_not_exist_is_compared_as_written() {
        // A sheep registered but never started has no log directory yet,
        // and failing to resolve its directory is not a reason to stop
        // protecting its paths.
        let never = dir(Path::new("/nonexistent/log"));
        assert_eq!(never, ResolvedDir(PathBuf::from("/nonexistent/log")));
    }

    #[test]
    fn a_directory_spelled_through_a_parent_component_is_the_same_directory() {
        // `PathBuf` equality normalises `.` away but not `..`, so this
        // spelling slips past a whole-path comparison.
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(tmp.path().join("sub")).expect("created");
        let long_way = tmp.path().join("sub").join("..").join("web.1");
        let set = FileSet::from_paths([long_way.as_path()]);
        assert!(set.contains(&dir(tmp.path()), "web.1"));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_reached_through_a_symlink_is_the_same_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        fs::create_dir(&real).expect("created");
        let through = tmp.path().join("through");
        std::os::unix::fs::symlink(&real, &through).expect("linked");
        let set = FileSet::from_paths([through.join("web.1").as_path()]);
        assert!(set.contains(&dir(&real), "web.1"));
        assert!(set.contains(&dir(&through), "web.1"));
        assert_eq!(dir(&real), dir(&through));
    }

    #[test]
    fn a_relative_path_lives_in_the_current_directory() {
        // `Path::new("web.1").parent()` is `Some("")`, which is not a
        // directory anything can resolve. `LogPath::split` reads it as `.`,
        // and this type inherits that rather than repeating it.
        let bare = FileSet::from_paths([Path::new("web.1")]);
        assert!(bare.contains(&dir(Path::new(".")), "web.1"));
        let dotted = FileSet::from_paths([Path::new("./web.1")]);
        assert_eq!(bare, dotted, "one file, two spellings");
    }

    #[test]
    fn a_path_with_no_file_name_is_a_member_of_nothing() {
        let set = FileSet::from_paths([Path::new("/"), Path::new(".."), Path::new("")]);
        assert_eq!(set, FileSet::default());
    }

    #[test]
    fn two_files_in_one_directory_are_listed_together() {
        let set = FileSet::from_paths([
            Path::new("/nonexistent/log/web.1"),
            Path::new("/nonexistent/log/api.log"),
            Path::new("/nonexistent/elsewhere/web.1"),
        ]);
        let names: Vec<&str> = set
            .names_in(&dir(Path::new("/nonexistent/log")))
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(names, ["api.log", "web.1"]);
        assert!(
            set.names_in(&dir(Path::new("/nonexistent/nowhere")))
                .is_empty()
        );
    }

    #[test]
    fn an_inserted_file_is_a_member_and_is_listed() {
        let mut set = FileSet::default();
        let here = dir(Path::new("/nonexistent/log"));
        assert!(!set.contains(&here, "web.log"));
        set.insert(here.clone(), "web.log".to_owned());
        assert!(set.contains(&here, "web.log"));
        assert_eq!(set.names_in(&here).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_the_set_was_built_from_resolves_as_it_did_then() {
        // The lookups run after the renames and the reopens, and a link can
        // move in between: a deploy flipping `logs` from one release to the
        // next is the ordinary case, not a trick. A key resolved when the
        // set was built and a lookup resolved later would miss each other,
        // and the guard would go quiet on a file that is still live.
        let tmp = tempfile::tempdir().expect("tempdir");
        let before = tmp.path().join("release-1");
        let after = tmp.path().join("release-2");
        fs::create_dir(&before).expect("created");
        fs::create_dir(&after).expect("created");
        let link = tmp.path().join("logs");
        std::os::unix::fs::symlink(&before, &link).expect("linked");
        let set = FileSet::from_paths([link.join("web.1").as_path()]);

        fs::remove_file(&link).expect("unlinked");
        std::os::unix::fs::symlink(&after, &link).expect("relinked");

        assert_eq!(
            set.resolve(&link),
            dir(&before),
            "as it was when the set was built"
        );
        assert_ne!(
            ResolvedDir::of(&link),
            dir(&before),
            "which is no longer what the disk says"
        );
        assert!(set.contains(&set.resolve(&link), "web.1"));
    }

    #[test]
    fn a_directory_the_set_was_not_built_from_resolves_now() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let set = FileSet::from_paths([Path::new("/nonexistent/log/web.1")]);
        assert_eq!(set.resolve(tmp.path()), dir(tmp.path()));
    }
}
