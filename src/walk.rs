//! Recursive file collection.
//!
//! Every directory this wrapper scans belongs to someone else (the user's pak
//! and config directories) or is a mod pak we just unpacked, so all of them are
//! arbitrarily nested and none of them can be assumed to exist.

use std::path::{Path, PathBuf};

/// Every file under `dir` that `keep` accepts, sorted.
///
/// A missing `dir` yields nothing rather than an error: an empty pak or config
/// directory is a normal install, not a failure. Errors from an existing
/// directory are propagated, because scanning fewer inputs than the user has
/// installed would silently merge the wrong set.
pub fn files(dir: &Path, keep: &mut impl FnMut(&Path) -> bool) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(dir, keep, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(
    dir: &Path,
    keep: &mut impl FnMut(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        match entry.file_type()?.is_dir() {
            true => collect(&path, keep, out)?,
            false => {
                if keep(&path) {
                    out.push(path);
                }
            }
        }
    }
    Ok(())
}

/// Does `path` have this extension, ignoring case?
pub fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Does `path`'s file name end with `suffix`, ignoring case?
///
/// Matches on the name only, so a directory named after the file cannot match.
pub fn name_ends_with(path: &Path, suffix: &str) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase())
    })
}
