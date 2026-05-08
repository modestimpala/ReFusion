use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};

const APPDATA_INPUT_REL: &str = r"VotV\Saved\Config\WindowsNoEditor\Input.ini";

/// Port of C# `FuseInputs`. Two outputs:
/// 1. Append every mod's `DefaultInput.ini` (and any `--cfg-dir` ones) to the
///    staged `DefaultInput.ini` that gets packed into FusionPatch_P.
/// 2. Append unique key bindings to the user's `Input.ini` in LOCALAPPDATA.
pub fn fuse_inputs(
    staged_default_input: &Path,
    cfg_files: &[PathBuf],
    extracted_dirs: &[(String, PathBuf)],
) -> Result<()> {
    let mut all_lines: Vec<String> = Vec::new();

    let mut default_input_writer = OpenOptions::new()
        .append(true)
        .open(staged_default_input)
        .context("open staged DefaultInput.ini")?;

    for (name, dir) in extracted_dirs {
        for path in find_default_inputs(dir) {
            let body = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!("skip unreadable input {:?}: {e}", path);
                    continue;
                }
            };
            writeln!(default_input_writer, "\n#{name}\\{}\n{body}", path.file_name().unwrap_or_default().to_string_lossy())?;
            all_lines.extend(body.lines().map(str::to_owned));
        }
    }

    for cfg in cfg_files {
        let body = std::fs::read_to_string(cfg).with_context(|| format!("read {cfg:?}"))?;
        let parent = cfg
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = cfg
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        writeln!(default_input_writer, "\n#{parent}\\{name}\n{body}")?;
        all_lines.extend(body.lines().map(str::to_owned));
    }

    drop(default_input_writer);

    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let user_input = PathBuf::from(local_appdata).join(APPDATA_INPUT_REL);
        if let Err(e) = append_unique_bindings(&user_input, &all_lines) {
            warn!("could not update user Input.ini at {:?}: {e:#}", user_input);
        }
    }

    info!("fused inputs from {} mods + {} cfg files", extracted_dirs.len(), cfg_files.len());
    Ok(())
}

fn find_default_inputs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("DefaultInput.ini"))
        {
            out.push(path);
        }
    }
}

/// Mirrors the C# loop: for each line containing a quoted token, append it to
/// the user's Input.ini iff the quoted token isn't already present anywhere
/// in the existing file. Lines starting with `+`/`-` get the leading char
/// stripped (these are r2modman-style additive markers in the source files
/// that should land as plain bindings).
fn append_unique_bindings(user_input: &Path, lines: &[String]) -> Result<()> {
    if !user_input.is_file() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(user_input).context("read user Input.ini")?;
    let mut writer = OpenOptions::new()
        .append(true)
        .open(user_input)
        .context("open user Input.ini for append")?;

    for line in lines {
        let Some(start) = line.find('"') else { continue };
        let Some(end_rel) = line[start + 1..].find('"') else { continue };
        let token = &line[start..=start + 1 + end_rel];
        if existing.contains(token) {
            continue;
        }
        let to_write: &str = if line.starts_with('+') || line.starts_with('-') {
            &line[1..]
        } else {
            line
        };
        writeln!(writer, "{to_write}")?;
    }
    Ok(())
}
