//! Windows entry point: `DllMain`, real-UE4SS staging, logging.
//!
//! Everything in here needs the Win32 API; the fusion pipeline itself lives in
//! the crate root so it can be exercised natively (see `examples/fusiontool.rs`).

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use chrono::Local;
use log::{error, info, LevelFilter};
use widestring::U16CString;
use windows_sys::Win32::Foundation::{BOOL, HMODULE, MAX_PATH, TRUE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
};

use crate::datatable::IncompatibleMod;

const REAL_UE4SS_NAME: &str = "ue4ss_real.dll";

#[no_mangle]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DllMain(
    dll_module: HMODULE,
    call_reason: u32,
    _reserved: *const c_void,
) -> BOOL {
    if call_reason == DLL_PROCESS_ATTACH {
        run(dll_module);
    }
    TRUE
}

/// Everything happens before the real UE4SS is loaded, and therefore before the
/// engine mounts any pak: the merge has to finish while its output can still
/// change what the game sees. That is why a slow cold merge shows up as a launch
/// delay rather than as a mod that loaded late.
unsafe fn run(my_module: HMODULE) {
    let my_dir = match module_dir(my_module) {
        Some(p) => p,
        None => return,
    };

    init_logger(&my_dir);

    info!("fusion-wrapper start, my_dir={:?}", my_dir);
    log_env();

    match crate::fuse(&my_dir) {
        Ok(outcome) => warn_left_out(&outcome),
        Err(e) => error!("fusion failed: {e:#}"),
    }

    let real = my_dir.join(REAL_UE4SS_NAME);
    if let Err(e) = ensure_real_ue4ss(&real) {
        error!("could not stage real ue4ss: {e:#}");
        return;
    }
    info!("loading real ue4ss from {:?}", real);
    let wide = match U16CString::from_os_str(real.as_os_str()) {
        Ok(w) => w,
        Err(e) => {
            error!("failed to encode real ue4ss path: {e}");
            return;
        }
    };
    if LoadLibraryW(wide.as_ptr()).is_null() {
        error!("LoadLibraryW returned null for real ue4ss");
    }
}

/// Tell the user, on screen, what the merge left out.
///
/// This is the one class of failure a log cannot cover: a mod built against an
/// older row struct produces items with no name, icon or mesh, and a pak that
/// could not be read produces no items at all. Both read as "the mod loader is
/// broken" rather than "that mod needs attention", and the game otherwise starts
/// up looking fine. The merge itself succeeded without them, so this is purely
/// informational: one modal, then startup continues.
///
/// Only shown when the report differs from the last one, because an unreadable
/// pak defeats the merge cache on purpose and would otherwise interrupt every
/// launch until it was fixed.
///
/// Runs inside `DllMain`, so the loader lock is held while the modal pumps
/// messages.
unsafe fn warn_left_out(outcome: &crate::FuseOutcome) {
    let body = crate::left_out_report(outcome);
    if body.is_empty() {
        return;
    }
    error!("mods left out of the merge:\n{body}");
    if !outcome.report_is_new {
        info!("already reported on an earlier launch; not interrupting startup again");
        return;
    }

    let caption = match outcome.unreadable_paks.is_empty() {
        true => "Fusion: incompatible mods skipped",
        false => "Fusion: mod paks could not be read",
    };
    let (Ok(text), Ok(caption)) = (U16CString::from_str(&body), U16CString::from_str(caption))
    else {
        return;
    };
    MessageBoxW(
        std::ptr::null_mut(),
        text.as_ptr(),
        caption.as_ptr(),
        MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
    );
}

/// The wrapper's own directory, from which everything else is derived.
///
/// `GetModuleFileNameW` truncates into a fixed buffer and still reports
/// success, so a fixed `MAX_PATH` array would silently hand back a wrong
/// directory for a deeply nested install; the buffer grows until the path fits.
/// The result is converted with `OsString::from_wide` rather than through
/// `String`, so nothing is lost for paths the Rust string types can't represent.
unsafe fn module_dir(module: HMODULE) -> Option<PathBuf> {
    let mut buf = vec![0u16; MAX_PATH as usize];
    loop {
        let len = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        if len == 0 {
            return None;
        }
        // Truncation is reported by filling the buffer exactly.
        if len == buf.len() {
            if buf.len() >= 32_768 {
                return None;
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]));
        return path.parent().map(Path::to_path_buf);
    }
}

/// Stage `ue4ss_real.dll` next to the wrapper by copying `<profile>/ue4ss.dll`
/// over it on every launch. The wrapper's `GetModuleFileNameW` returns the
/// spoofed `<install>/.../Binaries/Win64` path (shimloader virtualizes file
/// access there), so we can't derive the profile from it. We use
/// `SHIMLOADER_OVERLAY_DIR` instead, which shimloader sets to the real
/// `<profile>/shimloader/overlay` path before our DllMain runs. Re-copying
/// every launch keeps the staged DLL in sync after the user updates UE4SS.
///
/// The destination can stay `my_dir`-relative because shimloader's overlay
/// registry redirects writes under the spoofed Win64 path back to the real
/// overlay dir.
fn ensure_real_ue4ss(dest: &Path) -> anyhow::Result<()> {
    let overlay_dir = std::env::var_os("SHIMLOADER_OVERLAY_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("SHIMLOADER_OVERLAY_DIR not set"))?;
    // SHIMLOADER_OVERLAY_DIR is `<profile>/shimloader/overlay`; profile is two up.
    let profile = overlay_dir.parent().and_then(Path::parent).ok_or_else(|| {
        anyhow::anyhow!(
            "can't derive profile root from SHIMLOADER_OVERLAY_DIR={:?}",
            overlay_dir
        )
    })?;
    let src = profile.join("ue4ss.dll");
    if !src.is_file() {
        return Err(anyhow::anyhow!("real ue4ss source {:?} not found", src));
    }
    info!("staging real ue4ss: {:?} -> {:?}", src, dest);
    std::fs::copy(&src, dest)?;
    Ok(())
}

fn log_env() {
    for var in [
        "SHIMLOADER_MOD_DIR",
        "SHIMLOADER_PAK_DIR",
        "SHIMLOADER_CFG_DIR",
        "SHIMLOADER_OVERLAY_DIR",
    ] {
        match std::env::var_os(var) {
            Some(v) => info!("env {var} = {:?}", v),
            None => info!("env {var} = <unset>"),
        }
    }
}

fn init_logger(my_dir: &Path) {
    let log_path = my_dir.join("fusion-wrapper-log.txt");
    let Ok(file) = File::create(&log_path) else {
        return;
    };
    let _ = env_logger::Builder::new()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .filter(None, LevelFilter::Debug)
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {} {}:{}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.file().unwrap_or("?"),
                record.line().unwrap_or(0),
                record.args()
            )
        })
        .try_init();
}
