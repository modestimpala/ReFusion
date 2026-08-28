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
    // Where each body came from, for the comment header written above it. Mod
    // files and `--cfg-dir` files are merged identically; only the label
    // differs, and it exists so a stray binding can be traced to its source.
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    for (name, dir) in extracted_dirs {
        for path in find_default_inputs(dir) {
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            sources.push((format!("{name}\\{file}"), path.clone()));
        }
    }
    for cfg in cfg_files {
        let parent = cfg.parent().and_then(|p| p.file_name()).unwrap_or_default();
        let file = cfg.file_name().unwrap_or_default();
        sources.push((
            format!("{}\\{}", parent.to_string_lossy(), file.to_string_lossy()),
            cfg.clone(),
        ));
    }

    let mut default_input_writer = OpenOptions::new()
        .append(true)
        .open(staged_default_input)
        .context("open staged DefaultInput.ini")?;

    let mut all_lines: Vec<String> = Vec::new();
    for (label, path) in &sources {
        // One unreadable file must not cost every other mod its keybinds.
        let body = match crate::text::read(path) {
            Ok(text) => text.body,
            Err(e) => {
                warn!("skip unreadable input {path:?}: {e}");
                continue;
            }
        };
        writeln!(default_input_writer, "\n#{label}\n{body}")?;
        all_lines.extend(body.lines().map(str::to_owned));
    }
    drop(default_input_writer);

    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let user_input = PathBuf::from(local_appdata).join(APPDATA_INPUT_REL);
        if let Err(e) = append_unique_bindings(&user_input, &all_lines) {
            warn!("could not update user Input.ini at {user_input:?}: {e:#}");
        }
    }

    info!(
        "fused inputs from {} mods + {} cfg files",
        extracted_dirs.len(),
        cfg_files.len()
    );
    Ok(())
}

fn find_default_inputs(dir: &Path) -> Vec<PathBuf> {
    crate::walk::files(dir, &mut |p| {
        p.file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("DefaultInput.ini"))
    })
    .unwrap_or_default()
}

/// Append every binding the user's `Input.ini` does not already have.
///
/// A line qualifies when its first quoted token appears nowhere in the existing
/// file, which is how the C# tool decides too: keybind lines are the only ones
/// carrying a quoted action name, and matching on the whole line would re-add a
/// binding the user has since rebound. Leading `+`/`-` markers are r2modman's
/// additive syntax in the source files and have to land as plain bindings.
///
/// The engine rewrites this file as UTF-16 as soon as it holds a non-ANSI
/// character, so appended lines are encoded to match what is already there.
/// Appending UTF-8 to a UTF-16 file leaves the user with a config the engine
/// reads as garbage.
fn append_unique_bindings(user_input: &Path, lines: &[String]) -> Result<()> {
    if !user_input.is_file() {
        return Ok(());
    }
    let existing = crate::text::read(user_input).context("read user Input.ini")?;

    let mut pending = String::new();
    for line in lines {
        let Some(start) = line.find('"') else {
            continue;
        };
        let Some(end_rel) = line[start + 1..].find('"') else {
            continue;
        };
        let token = &line[start..=start + 1 + end_rel];
        if existing.body.contains(token) || pending.contains(token) {
            continue;
        }
        let to_write: &str = if line.starts_with('+') || line.starts_with('-') {
            &line[1..]
        } else {
            line
        };
        pending.push_str(to_write);
        pending.push('\n');
    }
    if pending.is_empty() {
        return Ok(());
    }

    let mut writer = OpenOptions::new()
        .append(true)
        .open(user_input)
        .context("open user Input.ini for append")?;
    writer
        .write_all(&existing.encode(&pending))
        .context("append to user Input.ini")?;
    Ok(())
}
