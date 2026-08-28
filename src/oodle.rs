//! Places the Oodle runtime where `repak` will find it, without a download.
//!
//! Mod paks for this game are Oodle-compressed (UnrealPak's default), so repak
//! needs `oo2core`. Its `oodle_loader` probes for the library next to
//! `current_exe()` and, when missing, fetches it from GitHub at runtime - which
//! a packaged mod must never do (no network at launch, and the fetch happens
//! inside `DllMain`). The overlay therefore ships the library and we copy it to
//! the probe path before any pak is opened, so the loader always takes its
//! "already there" branch.
//!
//! If the library is absent, repak fails per-pak with an Oodle error and the
//! pipeline skips that mod - the datatables it carries silently go missing, so
//! this logs at error level rather than warning.

use std::path::Path;

use log::{error, info};

/// Exactly the file name `oodle_loader` looks for on this target.
#[cfg(windows)]
const OODLE_LIB: &str = "oo2core_9_win64.dll";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const OODLE_LIB: &str = "liboo2corelinux64.so.9";
#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
const OODLE_LIB: &str = "";

/// Copy `<my_dir>/fusion/<oodle lib>` next to the running executable unless it
/// is already there. Best-effort: every failure is logged and the pipeline
/// continues, because uncompressed and Zlib mod paks still work without it.
pub fn ensure_oodle(my_dir: &Path) {
    if OODLE_LIB.is_empty() {
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            error!("can't locate current exe to place {OODLE_LIB}: {e}");
            return;
        }
    };
    let dest = exe.with_file_name(OODLE_LIB);
    if dest.is_file() {
        info!("oodle runtime already at {dest:?}");
        return;
    }

    let src = my_dir.join("fusion").join(OODLE_LIB);
    if !src.is_file() {
        error!(
            "{src:?} is missing - Oodle-compressed mod paks will be skipped. \
             Ship {OODLE_LIB} in the overlay's fusion/ directory."
        );
        return;
    }

    match std::fs::copy(&src, &dest) {
        Ok(_) => info!("placed oodle runtime {src:?} -> {dest:?}"),
        Err(e) => error!(
            "could not place oodle runtime at {dest:?}: {e} - \
             Oodle-compressed mod paks will be skipped"
        ),
    }
}
