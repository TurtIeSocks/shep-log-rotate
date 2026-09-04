//! Helpers shared by this crate's unit tests. Compiled under `cfg(test)`
//! only, so nothing here reaches the shipped binary.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Write `body` to `name` inside `dir`, and hand back the path.
pub fn seed(dir: &Path, name: impl AsRef<Path>, body: impl AsRef<[u8]>) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("seeded");
    path
}

/// Assert `text` carries neither an em dash nor an en dash.
///
/// Every string this dog prints for a person is checked with this. A
/// terminal that cannot render either prints a replacement character in
/// the middle of the one message that exists to be read by somebody who is
/// already confused.
#[track_caller]
pub fn assert_no_dashes(text: &str) {
    assert!(!text.contains('\u{2014}'), "em dash in {text:?}");
    assert!(!text.contains('\u{2013}'), "en dash in {text:?}");
}
