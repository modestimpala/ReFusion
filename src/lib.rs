#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Local;
use log::{error, info, warn, LevelFilter};
use widestring::U16CString;
use windows_sys::Win32::Foundation::{BOOL, HMODULE, MAX_PATH, TRUE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

mod datatable;
mod input;
mod lua_forward;
mod pak;

const REAL_UE4SS_NAME: &str = "ue4ss_real.dll";
const OUTPUT_PAK_NAME: &str = "FusionPatch_P.pak";
const PROJECT_NAME: &str = "VotV";
const MAIN_PAK_NAME: &str = "VotV-WindowsNoEditor.pak";
const PAK_MOUNT: &str = "../../../VotV/";

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

unsafe fn run(my_module: HMODULE) {
    let my_dir = match module_dir(my_module) {
        Some(p) => p,
        None => return,
    };

    init_logger(&my_dir);

    info!("fusion-wrapper start, my_dir={:?}", my_dir);
    log_env();

    if let Err(e) = fuse(&my_dir) {
        error!("fusion failed: {e:#}");
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

fn fuse(my_dir: &Path) -> anyhow::Result<()> {
    let pak_dir = env_path("SHIMLOADER_PAK_DIR")
        .ok_or_else(|| anyhow::anyhow!("SHIMLOADER_PAK_DIR not set; vanilla launch?"))?;
    let cfg_dir = env_path("SHIMLOADER_CFG_DIR")
        .ok_or_else(|| anyhow::anyhow!("SHIMLOADER_CFG_DIR not set"))?;

    // my_dir is the spoofed Win64 path: <install>/<Project>/Binaries/Win64.
    // Project root = my_dir/../.. (e.g. <install>/VotV/)
    let project_root = my_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("can't derive project root from {my_dir:?}"))?;
    let game_pak = project_root.join("Content").join("Paks").join(MAIN_PAK_NAME);
    info!("project_root={:?} game_pak={:?}", project_root, game_pak);

    if !game_pak.is_file() {
        return Err(anyhow::anyhow!("game pak not found at {game_pak:?}"));
    }

    let mod_paks = list_mod_paks(&pak_dir)?;
    let cfg_files = list_cfgs(&cfg_dir)?;
    info!("{} mod paks, {} cfg files", mod_paks.len(), cfg_files.len());

    if mod_paks.is_empty() && cfg_files.is_empty() {
        info!("nothing to fuse");
        return Ok(());
    }

    let work_dir = my_dir.join("fusion").join("work");
    let last_write_path = my_dir.join("fusion").join("last-write.txt");
    let output_pak = pak_dir.join(OUTPUT_PAK_NAME);

    if cache_is_valid(&last_write_path, &mod_paks, &cfg_files) && output_pak.is_file() {
        info!("cache hit, skipping fusion");
        return Ok(());
    }

    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir)?;
    let staging = work_dir.join("staging");
    let extracted_root = work_dir.join("extracted");
    std::fs::create_dir_all(&staging)?;
    std::fs::create_dir_all(&extracted_root)?;

    info!("extracting templates from {:?}", game_pak);
    pak::extract_filtered(&game_pak, &staging, |entry| {
        // Main pak entries are stored with project prefix, e.g.
        // "VotV/Content/main/datatables/Foo.uasset". Strip that to land them
        // at staging/Content/... so the output pak's mount lines up.
        let rel = entry.strip_prefix(&format!("{PROJECT_NAME}/")).unwrap_or(entry);
        let take = rel.starts_with("Content/main/datatables/")
            || rel.starts_with("Config/")
                && rel
                    .rsplit('/')
                    .next()
                    .is_some_and(|n| n.eq_ignore_ascii_case("DefaultInput.ini"));
        if take {
            Some(rel.to_string())
        } else {
            None
        }
    })?;

    let mut extracted_dirs: Vec<(String, PathBuf)> = Vec::new();
    for pak in &mod_paks {
        let stem = pak
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let out = extracted_root.join(&stem);
        std::fs::create_dir_all(&out)?;
        match pak::extract_filtered(pak, &out, |entry| {
            let rel = entry
                .strip_prefix(&format!("{PROJECT_NAME}/"))
                .unwrap_or(entry);
            let lower = rel.to_ascii_lowercase();
            if lower.contains("/datatables/")
                || lower.ends_with("defaultinput.ini")
            {
                Some(rel.to_string())
            } else {
                None
            }
        }) {
            Ok(()) => extracted_dirs.push((stem, out)),
            Err(e) => warn!("skip {pak:?}: {e:#}"),
        }
    }

    let staged_datatables = staging.join("Content").join("main").join("datatables");
    if staged_datatables.is_dir() {
        if let Err(e) = datatable::fuse_datatables(&staged_datatables, &extracted_dirs) {
            error!("datatable fusion failed: {e:#}");
        }
    } else {
        warn!("no datatables extracted from game pak; skipping merge");
    }

    let staged_input = staging.join("Config").join("DefaultInput.ini");
    if staged_input.is_file() {
        if let Err(e) = input::fuse_inputs(&staged_input, &cfg_files, &extracted_dirs) {
            error!("input fusion failed: {e:#}");
        }
    } else {
        warn!("no DefaultInput.ini extracted from game pak; skipping merge");
    }

    if let Err(e) = pak::pack_dir(&staging, &output_pak, PAK_MOUNT) {
        return Err(anyhow::anyhow!("failed to pack output: {e:#}"));
    }
    info!("wrote {:?}", output_pak);

    if let Err(e) = write_cache(&last_write_path, &mod_paks, &cfg_files) {
        warn!("failed to write cache: {e:#}");
    }

    Ok(())
}

unsafe fn module_dir(module: HMODULE) -> Option<PathBuf> {
    let mut buf = [0u16; MAX_PATH as usize];
    let len = GetModuleFileNameW(module, buf.as_mut_ptr(), MAX_PATH);
    if len == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    PathBuf::from(s).parent().map(Path::to_path_buf)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
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
    let profile = overlay_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "can't derive profile root from SHIMLOADER_OVERLAY_DIR={:?}",
                overlay_dir
            )
        })?;
    let src = profile.join("ue4ss.dll");
    if !src.is_file() {
        return Err(anyhow::anyhow!(
            "real ue4ss source {:?} not found",
            src
        ));
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
    let Ok(file) = File::create(&log_path) else { return };
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

/// All `*.pak` files under `dir`, sorted, EXCLUDING our own output pak.
fn list_mod_paks(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_pak(dir, &mut out)?;
    out.retain(|p| {
        p.file_name()
            .map(|n| !n.eq_ignore_ascii_case(OUTPUT_PAK_NAME))
            .unwrap_or(true)
    });
    out.sort();
    Ok(out)
}

fn walk_pak(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_pak(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pak"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn list_cfgs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_cfg(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_cfg(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_cfg(&path, out)?;
        } else if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().ends_with("defaultinput.ini"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn cache_is_valid(cache_path: &Path, paks: &[PathBuf], cfgs: &[PathBuf]) -> bool {
    let Ok(body) = std::fs::read_to_string(cache_path) else { return false };
    let lines: Vec<&str> = body.lines().collect();
    let inputs: Vec<&PathBuf> = paks.iter().chain(cfgs.iter()).collect();
    if lines.len() != inputs.len() {
        return false;
    }
    for (line, path) in lines.iter().zip(inputs.iter()) {
        let Some((cached_path, cached_mtime)) = line.split_once('\t') else { return false };
        if cached_path != path.to_string_lossy() {
            return false;
        }
        let Ok(meta) = std::fs::metadata(path) else { return false };
        let Ok(now) = meta.modified() else { return false };
        let Ok(now_secs) = now.duration_since(std::time::UNIX_EPOCH) else { return false };
        if cached_mtime.parse::<u64>().ok() != Some(now_secs.as_secs()) {
            return false;
        }
    }
    true
}

fn write_cache(cache_path: &Path, paks: &[PathBuf], cfgs: &[PathBuf]) -> anyhow::Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for path in paks.iter().chain(cfgs.iter()) {
        let meta = std::fs::metadata(path)?;
        let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs();
        out.push_str(&path.to_string_lossy());
        out.push('\t');
        out.push_str(&mtime.to_string());
        out.push('\n');
    }
    std::fs::write(cache_path, out)?;
    Ok(())
}
