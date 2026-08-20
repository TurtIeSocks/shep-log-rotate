//! Naming rotated generations, and recognising them again afterwards.
//!
//! [`match_generation`] is the only thing standing between this dog and
//! deleting a file it did not write, so it is strict by construction: a
//! candidate matches only if it has the exact shape [`dated_name`] or
//! [`numeric_name`] would have produced for this base path. Everything else
//! is somebody else's file. When an edge case is arguable, the answer here is
//! always "not a match" - a missed generation costs disk, a false match costs
//! an operator their data.

use core::cmp::Ordering;
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use jiff::Timestamp;

use crate::config::Naming;

/// The shape of a stamp, one byte per position: `d` is any ASCII digit, and
/// every other byte must appear literally.
///
/// Both the formatter and the matcher are written against this one constant,
/// so the two cannot drift apart.
const STAMP_SHAPE: &[u8] = b"dddd-dd-ddTdd-dd-dd";

/// How a stamp is rendered. Kept next to [`STAMP_SHAPE`], which it must agree
/// with, and deliberately colon-free: a colon is not a portable filename byte.
const STAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

/// A live log file, split into the pieces a generation name is built from.
///
/// `/var/log/web-0-out.log` splits into `/var/log`, `web-0-out` and `log`.
/// The extension is the final one only: `/var/log/web.out` splits into `web`
/// and `out`, not `web` and `out.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPath {
    /// The directory the log and all its generations live in.
    pub dir: PathBuf,
    /// The file name with its final extension removed.
    pub stem: String,
    /// The final extension, without its dot. `None` for a name with no dot.
    pub ext: Option<String>,
}

impl LogPath {
    /// Split a log path into directory, stem and final extension.
    ///
    /// A path with no directory component gets `.`, not the empty path.
    /// `Path::new("app.log").parent()` is `Some("")` rather than `None`, and
    /// the empty path is not a directory anything can read: `fs::read_dir("")`
    /// is `NotFound`. `rotate::generations` reads a `NotFound` as "this sheep
    /// has never started" and returns an empty list, so a base spelled
    /// without a directory component would report no generations however many
    /// were on disk, and the numeric shift would rename the live file over
    /// the top of generation 1. shep does not absolutise a Flockfile's
    /// `out_file`, so a relative one arrives here exactly as it was written.
    ///
    /// The substitution lives here, once, so every consumer inherits it: a
    /// second copy of this rule somewhere downstream is a copy that can be
    /// missing from a third place.
    ///
    /// Returns `None` if `path` has no file name, or if the file name or its
    /// directory is not valid UTF-8. A path this dog cannot spell is a path
    /// it must not go on to rename or delete.
    pub fn split(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        // `file_stem`/`extension` already implement "the final extension,
        // and a leading dot does not start one".
        let stem = Path::new(name).file_stem()?.to_str()?.to_owned();
        let ext = match Path::new(name).extension() {
            Some(ext) => Some(ext.to_str()?.to_owned()),
            None => None,
        };
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Some(Self { dir, stem, ext })
    }

    /// Rebuild the live log path this was split from.
    pub fn live(&self) -> PathBuf {
        self.dir.join(self.file_name(String::new()))
    }

    /// The file name for this base, with `infix` spliced in before the
    /// extension. An empty `infix` rebuilds the live name.
    fn file_name(&self, infix: String) -> String {
        let mut name = String::with_capacity(self.stem.len() + infix.len() + 8);
        name.push_str(&self.stem);
        name.push_str(&infix);
        if let Some(ext) = &self.ext {
            name.push('.');
            name.push_str(ext);
        }
        name
    }
}

/// Where one generation sits in its scheme's ordering.
///
/// The [`Ord`] implementation is **newest first**, not the field order a
/// derive would give: `sort` and [`Order::newest_first`] therefore agree, and
/// a caller on the pruning path cannot get the direction wrong by reaching
/// for the shorter one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    /// A dated generation. `counter` is 0 when the name carried none.
    Dated {
        /// The 19-character UTC stamp, exactly as it appears in the name.
        stamp: String,
        /// The same-second collision counter, or 0 for a name without one.
        counter: u32,
    },
    /// A numeric generation. `n` is 1 for the newest, following logrotate.
    Numeric {
        /// The generation number, always 1 or more.
        n: u32,
    },
}

impl Order {
    /// Compare two generations newest first, for `sort_by`.
    ///
    /// Named rather than written as a closure so the direction is legible at
    /// the call site, which is a delete path.
    ///
    /// Dated generations order by `(stamp, counter)` descending. That the
    /// counter is part of the key is not a detail: sorting the file names
    /// lexicographically puts `...T15-04-05.1.log` *before*
    /// `...T15-04-05.log`, because `'1' < 'l'`, which is backwards. Numeric
    /// generations order by `n` ascending, because `.1` is the newest.
    pub fn newest_first(a: &Self, b: &Self) -> Ordering {
        a.cmp(b)
    }
}

impl Ord for Order {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                Self::Dated { stamp, counter },
                Self::Dated {
                    stamp: other_stamp,
                    counter: other_counter,
                },
            ) => other_stamp
                .cmp(stamp)
                .then_with(|| other_counter.cmp(counter)),
            (Self::Numeric { n }, Self::Numeric { n: other_n }) => n.cmp(other_n),
            // The two schemes never share a directory's worth of candidates,
            // so this arm exists only to make the ordering total.
            (Self::Dated { .. }, Self::Numeric { .. }) => Ordering::Less,
            (Self::Numeric { .. }, Self::Dated { .. }) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Order {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Render `at` as a UTC stamp, `2026-08-21T15-04-05`, exactly 19 characters.
///
/// UTC rather than local time on purpose. Local time goes backwards for an
/// hour at the end of daylight saving, which would misorder that hour's
/// generations once a year in a scheme whose pruning depends on their order.
/// UTC has no such hole. The README says the stamps are UTC so nobody reads
/// one as local.
///
/// A time outside years 1970 to 9999 is clamped to the nearest end of that
/// range. Only a badly broken clock gets there, and a year outside it renders
/// in more or fewer than four digits, so [`match_generation`] would never
/// recognise the file again and pruning would never reclaim it.
pub fn stamp_utc(at: SystemTime) -> String {
    let timestamp = match Timestamp::try_from(at) {
        Ok(timestamp) => timestamp.max(Timestamp::UNIX_EPOCH),
        Err(_) if at < SystemTime::UNIX_EPOCH => Timestamp::UNIX_EPOCH,
        Err(_) => Timestamp::MAX,
    };
    timestamp.strftime(STAMP_FORMAT).to_string()
}

/// Build the dated name for a generation: `{stem}.{stamp}.{ext}`, or
/// `{stem}.{stamp}.{counter}.{ext}` when `counter` is 1 or more.
///
/// The extension stays last, so the file still matches `*.log`, still opens
/// with log syntax highlighting, and is still found by every glob an operator
/// already has.
///
/// # Panics
/// In debug builds, if `stamp` is not the shape [`stamp_utc`] produces. A
/// name built from a malformed stamp could never be matched back, so it could
/// never be pruned.
#[track_caller]
pub fn dated_name(base: &LogPath, stamp: &str, counter: u32) -> PathBuf {
    debug_assert!(
        is_stamp(stamp.as_bytes()),
        "{stamp} is not the shape stamp_utc produces"
    );
    let infix = if counter == 0 {
        format!(".{stamp}")
    } else {
        format!(".{stamp}.{counter}")
    };
    base.dir.join(base.file_name(infix))
}

/// Build the numeric name for a generation: `{stem}.{ext}.{n}`.
///
/// `.1` is the newest, following logrotate. macOS `newsyslog` disagrees and
/// calls the newest `.0`; shep follows logrotate, and the README says so.
///
/// # Panics
/// In debug builds, if `n` is 0. `.0` is not a name this dog writes, so
/// [`match_generation`] refuses it, so a `.0` written here could never be
/// pruned.
#[track_caller]
pub fn numeric_name(base: &LogPath, n: u32) -> PathBuf {
    debug_assert!(n >= 1, "numeric generations start at 1, not {n}");
    let mut name = base.file_name(String::new());
    name.push('.');
    name.push_str(&n.to_string());
    base.dir.join(name)
}

/// Append a `.gz` suffix to a generation's path.
///
/// Appended to the whole name rather than swapped in as an extension, so
/// `web-0-out.2026-08-20T15-04-05.log` becomes
/// `web-0-out.2026-08-20T15-04-05.log.gz` and every glob an operator already
/// has keeps working.
///
/// It lives next to [`match_generation`] because it is the other half of that
/// function's `.gz` stripping. The suffix one writes is the suffix the other
/// reads back, and two copies of it in two modules is two places for that
/// agreement to drift.
pub fn with_gz(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".gz");
    PathBuf::from(name)
}

/// Recognise `file_name` as a generation of `base` under `naming`, reporting
/// where it sits in the ordering and whether it is already compressed.
///
/// `None` means "this dog did not write that file", and the pruning path must
/// treat it as untouchable. Matching works by stripping known pieces off the
/// ends, never by searching for a date somewhere in the middle, so a name
/// that merely contains a timestamp is not a match.
pub fn match_generation(base: &LogPath, naming: Naming, file_name: &str) -> Option<(Order, bool)> {
    let attempt = |rest: &str| match naming {
        Naming::Dated => match_dated(base, rest),
        Naming::Numeric => match_numeric(base, rest),
    };
    // A trailing `.gz` is normally the marker this dog adds when it
    // compresses. It is ambiguous only for a log whose own extension is
    // `gz`, where `web-0-out.gz` rotates to `web-0-out.<stamp>.gz` with
    // nothing compressed at all. So read the `.gz` as the marker first, and
    // fall back to reading it as the extension. Both readings accept only an
    // exactly-generated name, so the fallback cannot widen what matches.
    if let Some(rest) = file_name.strip_suffix(".gz")
        && let Some(order) = attempt(rest)
    {
        return Some((order, true));
    }
    Some((attempt(file_name)?, false))
}

/// Match `rest` (already `.gz`-stripped) as `{stem}.{stamp}[.{counter}][.{ext}]`.
fn match_dated(base: &LogPath, rest: &str) -> Option<Order> {
    let rest = strip_ext(base, rest)?;
    let middle = rest.strip_prefix(&base.stem)?.strip_prefix('.')?;

    let bytes = middle.as_bytes();
    let (stamp, tail) = bytes.split_at_checked(STAMP_SHAPE.len())?;
    if !is_stamp(stamp) {
        return None;
    }
    let counter = match tail {
        [] => 0,
        [b'.', digits @ ..] => parse_index(digits)?,
        _ => return None,
    };
    Some(Order::Dated {
        // `is_stamp` accepted it, so the first 19 bytes are ASCII and this
        // slice is on a character boundary.
        stamp: middle[..STAMP_SHAPE.len()].to_owned(),
        counter,
    })
}

/// Match `rest` (already `.gz`-stripped) as `{stem}.{ext}.{n}`.
fn match_numeric(base: &LogPath, rest: &str) -> Option<Order> {
    let live = base.file_name(String::new());
    let digits = rest.strip_prefix(&live)?.strip_prefix('.')?;
    Some(Order::Numeric {
        n: parse_index(digits.as_bytes())?,
    })
}

/// Strip the base's final extension off `rest`, or `None` if it is not there.
///
/// A base with no extension has nothing to strip and always succeeds.
fn strip_ext<'a>(base: &LogPath, rest: &'a str) -> Option<&'a str> {
    match &base.ext {
        Some(ext) => rest.strip_suffix(ext)?.strip_suffix('.'),
        None => Some(rest),
    }
}

/// Whether `candidate` is exactly the 19 bytes of [`STAMP_SHAPE`], and every
/// field is in range for a real UTC civil datetime.
///
/// The range check is stricter than the shape alone needs. It costs nothing
/// and it means an operator's own `app.9999-99-99T99-99-99.log` is never
/// mistaken for something this dog wrote, while every stamp [`stamp_utc`]
/// produces still matches.
fn is_stamp(candidate: &[u8]) -> bool {
    if candidate.len() != STAMP_SHAPE.len() {
        return false;
    }
    let shaped = candidate
        .iter()
        .zip(STAMP_SHAPE)
        .all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            literal => byte == literal,
        });
    if !shaped {
        return false;
    }
    // Positions are fixed by the shape above, so every slice below is digits.
    let field = |from: usize, to: usize| -> u32 {
        candidate[from..to]
            .iter()
            .fold(0, |acc, byte| acc * 10 + u32::from(byte - b'0'))
    };
    // The date is handed to jiff rather than range-checked field by field.
    // Independent ranges accept dates that never happened -- 2026-02-30,
    // 2026-02-29 in a year that is not a leap year, April 31st -- and every
    // one of those is a name `stamp_utc` cannot write, so accepting it hands
    // the pruner a file this dog did not create.
    //
    // The year is bounded below at the epoch for the same reason: `stamp_utc`
    // renders a real instant, so a generation dated 1969 is somebody else's
    // file whatever else it looks like.
    let (year, month, day) = (field(0, 4), field(5, 7), field(8, 10));
    if !(1970..=9999).contains(&year) {
        return false;
    }
    let Ok(month) = i8::try_from(month) else {
        return false;
    };
    let Ok(day) = i8::try_from(day) else {
        return false;
    };
    let Ok(year) = i16::try_from(year) else {
        return false;
    };
    if jiff::civil::Date::new(year, month, day).is_err() {
        return false;
    }
    // The time stays hand-checked. Parsing it with jiff would accept second
    // 60 as a leap second, which `stamp_utc` never writes and which the
    // nonsense-digits test already pins as a refusal.
    field(11, 13) <= 23        // hour
        && field(14, 16) <= 59 // minute
        && field(17, 19) <= 59 // second
}

/// Parse a generation index: ASCII digits, no sign, no leading zero, 1 or
/// more, and within `u32`.
///
/// Every rejection here is a shape this dog never writes. `.0` is
/// `newsyslog`'s newest and not ours, and a counter of 0 is written as no
/// counter at all; `.01` is nobody's.
fn parse_index(digits: &[u8]) -> Option<u32> {
    if digits.first() == Some(&b'0') || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // `from_utf8` cannot fail: every byte was just checked to be a digit.
    core::str::from_utf8(digits).ok()?.parse::<u32>().ok()
}

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
    fn every_name_with_gz_writes_is_matched_back_as_compressed() {
        // The pairing that puts `with_gz` in this module: the suffix it
        // writes is the suffix `match_generation` strips. A generation
        // whose compressed name did not match back would be a file this
        // dog created and could never prune.
        let base = base();
        let dated = dated_name(&base, "2026-08-20T15-04-05", 0);
        let numeric = numeric_name(&base, 3);
        for (path, naming) in [(dated, Naming::Dated), (numeric, Naming::Numeric)] {
            let gz = with_gz(&path);
            let name = gz.file_name().expect("name").to_str().expect("utf8");
            let (order, compressed) =
                match_generation(&base, naming, name).expect("its own .gz name matches back");
            assert!(compressed, "{name} is compressed");
            let plain = path.file_name().expect("name").to_str().expect("utf8");
            let (plain_order, _) =
                match_generation(&base, naming, plain).expect("the plain name matches back");
            assert_eq!(
                order, plain_order,
                "{name} is the same generation as {plain}, wearing a second file"
            );
        }
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
        assert_eq!(
            made,
            Path::new("/var/log/web-0-out.2026-08-20T15-04-05.log")
        );
    }

    #[test]
    fn a_same_second_collision_appends_a_counter_before_the_extension() {
        let made = dated_name(&base(), "2026-08-20T15-04-05", 1);
        assert_eq!(
            made,
            Path::new("/var/log/web-0-out.2026-08-20T15-04-05.1.log")
        );
    }

    #[test]
    fn the_numeric_name_appends_after_the_extension() {
        assert_eq!(
            numeric_name(&base(), 1),
            Path::new("/var/log/web-0-out.log.1")
        );
    }

    #[test]
    fn a_generated_dated_name_matches_itself_back() {
        for counter in [0, 1, 42] {
            let made = dated_name(&base(), "2026-08-20T15-04-05", counter);
            let name = made
                .file_name()
                .expect("has a name")
                .to_str()
                .expect("utf8");
            let (order, compressed) =
                match_generation(&base(), Naming::Dated, name).expect("matches");
            assert!(!compressed);
            assert_eq!(
                order,
                Order::Dated {
                    stamp: "2026-08-20T15-04-05".into(),
                    counter
                }
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
        assert_eq!(
            order,
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 0
            }
        );
    }

    #[test]
    fn near_misses_are_not_matches() {
        // Each of these has a date in the name, or nearly the right shape.
        // None of them was written by this dog, so none may ever be deleted.
        let decoys = [
            "web-0-out.2026-08-20.log",                 // no time
            "web-0-out.2026-08-20T15-04.log",           // no seconds
            "web-0-out.2026-8-20T15-04-05.log",         // not zero padded
            "web-0-out.backup-2026-08-20T15-04-05.log", // prefixed
            "web-0-out.2026-08-20T15-04-05.log.bak",    // wrong trailing suffix
            "web-0-out.2026-08-20T15-04-05",            // extension dropped
            "web-1-out.2026-08-20T15-04-05.log",        // a different sheep
            "web-0-err.2026-08-20T15-04-05.log",        // the other stream
            "web-0-out.log",                            // the live file itself
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
            "web-0-out.log.0", // logrotate starts at 1; .0 is newsyslog's
            "web-0-out.log.x", // not a number
            "web-0-out.log",   // the live file itself
            "web-0-out.1.log", // dated-scheme shape
            "web-1-out.log.1", // a different sheep
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
        assert!(
            match_generation(
                &base(),
                Naming::Numeric,
                "web-0-out.2026-08-20T15-04-05.log"
            )
            .is_none()
        );
        assert!(match_generation(&base(), Naming::Dated, "web-0-out.log.1").is_none());
    }

    #[test]
    fn newest_first_ordering_puts_the_counter_after_the_plain_name() {
        // A plain lexicographic sort gets this backwards, because '1' < 'l'.
        let mut orders = [
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 0,
            },
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 2,
            },
            Order::Dated {
                stamp: "2026-08-20T15-04-06".into(),
                counter: 0,
            },
        ];
        orders.sort_by(Order::newest_first);
        assert_eq!(
            orders[0],
            Order::Dated {
                stamp: "2026-08-20T15-04-06".into(),
                counter: 0
            }
        );
        assert_eq!(
            orders[1],
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 2
            }
        );
        assert_eq!(
            orders[2],
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 0
            }
        );
    }

    #[test]
    fn a_date_that_never_happened_is_not_a_match() {
        // `stamp_utc` renders a real instant, so it can never write any of
        // these. A matcher that accepts them hands Task 5 a file to delete
        // that this dog did not create.
        let base = LogPath::split(Path::new("/var/log/web-0-out.log")).expect("splits");
        let impossible = [
            "web-0-out.2026-02-30T00-00-00.log", // February has no 30th
            "web-0-out.2026-02-29T00-00-00.log", // 2026 is not a leap year
            "web-0-out.2100-02-29T00-00-00.log", // a century that is not a leap year
            "web-0-out.2026-04-31T00-00-00.log", // April has 30 days
            "web-0-out.2026-06-31T00-00-00.log",
            "web-0-out.2026-09-31T00-00-00.log",
            "web-0-out.2026-11-31T00-00-00.log",
            "web-0-out.1969-07-20T20-17-40.log", // before the epoch
            "web-0-out.0000-01-01T00-00-00.log",
        ];
        for candidate in impossible {
            assert!(
                match_generation(&base, Naming::Dated, candidate).is_none(),
                "{candidate} is not a date this dog can have written"
            );
        }
    }

    #[test]
    fn numeric_newest_first_is_one_before_two() {
        let mut orders = vec![
            Order::Numeric { n: 3 },
            Order::Numeric { n: 1 },
            Order::Numeric { n: 2 },
        ];
        orders.sort_by(Order::newest_first);
        assert_eq!(
            orders,
            vec![
                Order::Numeric { n: 1 },
                Order::Numeric { n: 2 },
                Order::Numeric { n: 3 }
            ]
        );
    }

    #[test]
    fn the_stamp_is_utc_and_nineteen_characters() {
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);
        let stamp = stamp_utc(at);
        assert_eq!(stamp, "2026-08-21T15-04-05");
        assert_eq!(stamp.len(), 19);
        assert_eq!(stamp.as_bytes()[10], b'T');
        assert!(
            !stamp.contains(':'),
            "colons are not portable in filenames: {stamp}"
        );
    }

    // ---------------------------------------------------------------------
    // Beyond the brief's decoys. Every case below is a file an operator might
    // really have in a log directory, and every one of them is a file this
    // dog must never delete.
    // ---------------------------------------------------------------------

    #[test]
    fn a_name_that_is_only_separators_is_never_a_generation() {
        let junk = ["", ".", "..", "...", ".log", ".gz", ".log.gz", "gz"];
        for naming in [Naming::Dated, Naming::Numeric] {
            for name in junk {
                assert!(
                    match_generation(&base(), naming, name).is_none(),
                    "{name:?} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn a_stem_that_merely_starts_with_ours_is_a_different_sheep() {
        // "web-0-out" is a prefix of "web-0-outer". Requiring the separator
        // after the stem is what keeps the longer name out.
        let decoys = [
            ("web-0-outer.2026-08-20T15-04-05.log", Naming::Dated),
            ("web-0-out2.2026-08-20T15-04-05.log", Naming::Dated),
            ("web-0-outer.log.1", Naming::Numeric),
            ("web-0-out2.log.1", Naming::Numeric),
        ];
        for (decoy, naming) in decoys {
            assert!(
                match_generation(&base(), naming, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn a_counter_this_dog_would_never_write_is_not_a_match() {
        // Counter 0 is written as no counter at all, and counters carry no
        // leading zero, no sign and no padding. Anything else is somebody
        // else's convention.
        let decoys = [
            "web-0-out.2026-08-20T15-04-05.0.log", // 0 is written as absent
            "web-0-out.2026-08-20T15-04-05.00.log", // and padded 0 doubly so
            "web-0-out.2026-08-20T15-04-05.01.log", // leading zero
            "web-0-out.2026-08-20T15-04-05.+1.log", // signed
            "web-0-out.2026-08-20T15-04-05.-1.log", // signed
            "web-0-out.2026-08-20T15-04-05.1x.log", // trailing junk
            "web-0-out.2026-08-20T15-04-05..log",  // empty counter
            "web-0-out.2026-08-20T15-04-05.1.2.log", // two counters
            "web-0-out.2026-08-20T15-04-05.4294967296.log", // one past u32
            "web-0-out.2026-08-20T15-04-05 .log",  // trailing space
            "web-0-out.2026-08-20T15-04-050.log",  // stamp run on
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Dated, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn a_right_length_stamp_of_nonsense_digits_is_not_a_match() {
        // The shape is right, so a matcher that only counted digits would
        // delete these. No real clock produced any of them.
        let decoys = [
            "web-0-out.9999-99-99T99-99-99.log",
            "web-0-out.2026-00-20T15-04-05.log", // month 0
            "web-0-out.2026-13-20T15-04-05.log", // month 13
            "web-0-out.2026-08-00T15-04-05.log", // day 0
            "web-0-out.2026-08-32T15-04-05.log", // day 32
            "web-0-out.2026-08-20T24-04-05.log", // hour 24
            "web-0-out.2026-08-20T15-60-05.log", // minute 60
            "web-0-out.2026-08-20T15-04-60.log", // second 60
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Dated, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn case_and_non_ascii_lookalikes_are_not_matches() {
        let decoys = [
            "WEB-0-OUT.2026-08-20T15-04-05.log",     // shouted stem
            "web-0-out.2026-08-20T15-04-05.LOG",     // shouted extension
            "web-0-out.2026-08-20t15-04-05.log",     // lowercase separator
            "web-0-out.2026-08-20T15-04-05.log.GZ",  // shouted gz
            "web-0-out.２０２６-08-20T15-04-05.log", // full-width digits
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Dated, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn a_gz_is_stripped_exactly_once_and_only_at_the_end() {
        // One .gz is ours. A second, or a .gz with anything after it, is an
        // archive somebody else made.
        let decoys = [
            "web-0-out.2026-08-20T15-04-05.log.gz.gz",
            "web-0-out.2026-08-20T15-04-05.gz.log",
            "web-0-out.2026-08-20T15-04-05.log.tar.gz",
            "web-0-out.log.gz", // the live file, compressed by hand
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Dated, decoy).is_none(),
                "{decoy} must not match"
            );
        }
        // A counter and a .gz together is a shape this dog does write.
        let (order, compressed) = match_generation(
            &base(),
            Naming::Dated,
            "web-0-out.2026-08-20T15-04-05.7.log.gz",
        )
        .expect("matches");
        assert!(compressed);
        assert_eq!(
            order,
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 7
            }
        );
    }

    #[test]
    fn a_log_whose_own_extension_is_gz_reads_the_suffix_both_ways() {
        // The one place the grammar is genuinely ambiguous: a log called
        // web-0-out.gz rotates to web-0-out.<stamp>.gz with nothing
        // compressed, and only a second .gz means compression.
        let gz = LogPath::split(Path::new("/var/log/web-0-out.gz")).expect("splits");
        assert_eq!(
            match_generation(&gz, Naming::Dated, "web-0-out.2026-08-20T15-04-05.gz"),
            Some((
                Order::Dated {
                    stamp: "2026-08-20T15-04-05".into(),
                    counter: 0
                },
                false
            ))
        );
        assert_eq!(
            match_generation(&gz, Naming::Dated, "web-0-out.2026-08-20T15-04-05.gz.gz"),
            Some((
                Order::Dated {
                    stamp: "2026-08-20T15-04-05".into(),
                    counter: 0
                },
                true
            ))
        );
        assert_eq!(
            match_generation(&gz, Naming::Numeric, "web-0-out.gz.1.gz"),
            Some((Order::Numeric { n: 1 }, true))
        );
        // The live file, and the live file compressed by hand, are still not
        // generations of themselves.
        for decoy in ["web-0-out.gz", "web-0-out.gz.gz"] {
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&gz, naming, decoy).is_none(),
                    "{decoy} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn numeric_generations_compress_and_stop_at_u32() {
        let (order, compressed) =
            match_generation(&base(), Naming::Numeric, "web-0-out.log.2.gz").expect("matches");
        assert!(compressed);
        assert_eq!(order, Order::Numeric { n: 2 });

        let (order, _) = match_generation(&base(), Naming::Numeric, "web-0-out.log.4294967295")
            .expect("u32::MAX still matches");
        assert_eq!(order, Order::Numeric { n: u32::MAX });

        let decoys = [
            "web-0-out.log.4294967296", // one past u32
            "web-0-out.log.01",         // leading zero
            "web-0-out.log.+1",         // signed
            "web-0-out.log.-1",         // signed
            "web-0-out.log.1x",         // trailing junk
            "web-0-out.log.",           // empty
            "web-0-out.log.1.log",      // dressed up as a log again
            "web-0-out.log.log.1",      // doubled extension
        ];
        for decoy in decoys {
            assert!(
                match_generation(&base(), Naming::Numeric, decoy).is_none(),
                "{decoy} must not match"
            );
        }
    }

    #[test]
    fn a_base_without_an_extension_round_trips_and_still_refuses_decoys() {
        let bare = LogPath::split(Path::new("/var/log/web-out")).expect("splits");
        let made = dated_name(&bare, "2026-08-20T15-04-05", 3);
        assert_eq!(made, Path::new("/var/log/web-out.2026-08-20T15-04-05.3"));
        assert_eq!(
            match_generation(&bare, Naming::Dated, "web-out.2026-08-20T15-04-05.3"),
            Some((
                Order::Dated {
                    stamp: "2026-08-20T15-04-05".into(),
                    counter: 3
                },
                false
            ))
        );

        assert_eq!(numeric_name(&bare, 1), Path::new("/var/log/web-out.1"));
        assert_eq!(
            match_generation(&bare, Naming::Numeric, "web-out.1"),
            Some((Order::Numeric { n: 1 }, false))
        );

        // An extension where the base has none is a different file entirely.
        for decoy in ["web-out.2026-08-20T15-04-05.log", "web-out", "web-outer.1"] {
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&bare, naming, decoy).is_none(),
                    "{decoy} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn a_stem_with_its_own_dots_keeps_only_the_final_extension() {
        let dotted = LogPath::split(Path::new("/var/log/web.0.out.log")).expect("splits");
        assert_eq!(dotted.stem, "web.0.out");
        assert_eq!(dotted.ext.as_deref(), Some("log"));
        assert_eq!(dotted.live(), Path::new("/var/log/web.0.out.log"));
        assert_eq!(
            match_generation(&dotted, Naming::Dated, "web.0.out.2026-08-20T15-04-05.log"),
            Some((
                Order::Dated {
                    stamp: "2026-08-20T15-04-05".into(),
                    counter: 0
                },
                false
            ))
        );
        // Losing one of the stem's own dots makes it a different file.
        assert!(
            match_generation(&dotted, Naming::Dated, "web.0out.2026-08-20T15-04-05.log").is_none()
        );
    }

    #[test]
    fn a_relative_path_splits_to_the_current_directory() {
        // `Path::new("web-0-out.log").parent()` is `Some("")`, not `None`,
        // and an empty directory is not a directory anything can read:
        // `fs::read_dir("")` is `NotFound`. `rotate::generations` reads a
        // `NotFound` as "this sheep has never started", which for a base
        // with no directory component would mean every generation on disk
        // is invisible and the numeric shift renames over the top of one.
        // The empty parent is turned into `.` here, once, so no consumer
        // has to know about it.
        let here = LogPath::split(Path::new("web-0-out.log")).expect("splits");
        assert_eq!(here.dir, Path::new("."));
        assert_eq!(here.live(), Path::new("./web-0-out.log"));
        assert_eq!(
            dated_name(&here, "2026-08-20T15-04-05", 0),
            Path::new("./web-0-out.2026-08-20T15-04-05.log")
        );
        assert_eq!(numeric_name(&here, 1), Path::new("./web-0-out.log.1"));
    }

    #[test]
    fn a_path_with_no_file_name_does_not_split() {
        for path in ["/", "", ".", "..", "/..", "../.."] {
            assert!(
                LogPath::split(Path::new(path)).is_none(),
                "{path:?} has no file name"
            );
        }
        // A trailing slash is not a signal std keeps: "/var/log/" and
        // "/var/log" are the same Path, so this splits as a file called
        // "log" in "/var". Callers pass the path of a log file, not of a
        // directory, so nothing downstream depends on telling them apart.
        let slashed = LogPath::split(Path::new("/var/log/")).expect("splits");
        assert_eq!(slashed.stem, "log");
        assert_eq!(slashed.dir, Path::new("/var"));
    }

    #[test]
    fn every_stamp_this_dog_writes_is_matched_back() {
        // The one property the safety of pruning rests on: whatever
        // stamp_utc produces, match_generation recognises. A stamp it could
        // not recognise would be a file it could never prune.
        let seconds = [
            0,             // the epoch itself
            1,             //
            951_782_400,   // 2000-02-29, a leap day
            1_787_324_645, // the pinned example
            4_102_444_800, // 2100-01-01, past the 32-bit rollover
        ];
        let times = seconds
            .into_iter()
            .map(|secs| std::time::UNIX_EPOCH + core::time::Duration::from_secs(secs))
            .chain([SystemTime::now()]);
        for at in times {
            let stamp = stamp_utc(at);
            assert!(is_stamp(stamp.as_bytes()), "{stamp} is not a valid stamp");
            for counter in [0, 1, 9, 4_294_967_295] {
                let made = dated_name(&base(), &stamp, counter);
                let name = made.file_name().expect("named").to_str().expect("utf8");
                assert_eq!(
                    match_generation(&base(), Naming::Dated, name),
                    Some((
                        Order::Dated {
                            stamp: stamp.clone(),
                            counter
                        },
                        false
                    )),
                    "{name} did not match itself back"
                );
            }
        }
    }

    #[test]
    fn odd_base_paths_still_round_trip_through_both_schemes() {
        // Whatever a log file is called, a name this dog builds from it must
        // be a name this dog recognises. Anything it builds and cannot
        // recognise is a file it can never prune.
        let odd = [
            "/var/log/web-0-out.log",
            "/var/log/web-out",       // no extension
            "/var/log/web.0.out.log", // dots inside the stem
            "/var/log/.hidden.log",   // leading dot
            "/var/log/.hidden",       // leading dot, no extension
            "/var/log/web-0-out.",    // trailing dot
            "/var/log/a",             // one character
            "/var/log/log.log",       // stem equal to the extension
            "/var/log/web-0-out.gz",  // an extension we also use as a suffix
            "/var/log/w b.l g",       // spaces
            "/var/log/日誌.log",      // not ASCII
            "web-0-out.log",          // relative
        ];
        for path in odd {
            let base = LogPath::split(Path::new(path)).expect("splits");
            let dated = dated_name(&base, "2026-08-20T15-04-05", 4);
            let name = dated.file_name().expect("named").to_str().expect("utf8");
            assert_eq!(
                match_generation(&base, Naming::Dated, name),
                Some((
                    Order::Dated {
                        stamp: "2026-08-20T15-04-05".into(),
                        counter: 4
                    },
                    false
                )),
                "{path}: {name} did not match itself back"
            );

            let numeric = numeric_name(&base, 2);
            let name = numeric.file_name().expect("named").to_str().expect("utf8");
            assert_eq!(
                match_generation(&base, Naming::Numeric, name),
                Some((Order::Numeric { n: 2 }, false)),
                "{path}: {name} did not match itself back"
            );

            // The live file is never one of its own generations.
            let live = base.live();
            let name = live.file_name().expect("named").to_str().expect("utf8");
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&base, naming, name).is_none(),
                    "{path}: the live file matched under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn every_numeric_name_this_dog_writes_is_matched_back() {
        for n in [1, 2, 9, 10, 99, 4_294_967_295] {
            let made = numeric_name(&base(), n);
            let name = made.file_name().expect("named").to_str().expect("utf8");
            assert_eq!(
                match_generation(&base(), Naming::Numeric, name),
                Some((Order::Numeric { n }, false)),
                "{name} did not match itself back"
            );
        }
    }

    #[test]
    fn a_broken_clock_still_produces_a_matchable_stamp() {
        // Before the epoch and past year 9999 both clamp, because a year that
        // is not four digits would make a name this dog could never prune.
        let before = std::time::UNIX_EPOCH - core::time::Duration::from_secs(86_400);
        assert_eq!(stamp_utc(before), "1970-01-01T00-00-00");

        let after = std::time::UNIX_EPOCH + core::time::Duration::from_secs(300_000_000_000);
        let stamp = stamp_utc(after);
        assert!(stamp.starts_with("9999-"), "{stamp}");
        assert!(is_stamp(stamp.as_bytes()), "{stamp}");
    }

    #[test]
    fn the_natural_order_is_the_same_newest_first_order() {
        // sort() and sort_by(newest_first) must not disagree: the shorter one
        // is on a delete path, and a derived ordering would run it backwards.
        let mut natural = vec![
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 0,
            },
            Order::Dated {
                stamp: "2026-08-20T15-04-06".into(),
                counter: 0,
            },
            Order::Dated {
                stamp: "2026-08-20T15-04-05".into(),
                counter: 2,
            },
        ];
        let mut explicit = natural.clone();
        natural.sort();
        explicit.sort_by(Order::newest_first);
        assert_eq!(natural, explicit);
        assert_eq!(
            natural[0],
            Order::Dated {
                stamp: "2026-08-20T15-04-06".into(),
                counter: 0
            }
        );
    }
}
