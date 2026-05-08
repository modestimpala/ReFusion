use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use repak::{Compression, PakBuilder, Version};

/// Extract every entry from `pak_path` for which `keep` returns `Some(rel)`.
/// The returned `rel` is the path under `out_dir` to write the entry to.
/// Returning `None` skips the entry.
pub fn extract_filtered<F>(pak_path: &Path, out_dir: &Path, mut keep: F) -> Result<()>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut file = BufReader::new(File::open(pak_path).with_context(|| format!("open {pak_path:?}"))?);
    let pak = PakBuilder::new()
        .reader(&mut file)
        .with_context(|| format!("read index of {pak_path:?}"))?;

    for entry in pak.files() {
        let Some(rel) = keep(&entry) else { continue };
        let bytes = pak
            .get(&entry, &mut file)
            .with_context(|| format!("read entry {entry}"))?;
        let dest = out_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("write {dest:?}"))?;
    }
    Ok(())
}

pub fn pack_dir(src_dir: &Path, out_pak: &Path, mount_point: &str) -> Result<()> {
    if let Some(parent) = out_pak.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let writer = BufWriter::new(File::create(out_pak).context("create output pak")?);
    let mut pak_writer = PakBuilder::new()
        .compression(vec![Compression::Zlib])
        .writer(writer, Version::V11, mount_point.to_string(), None);

    let mut entries = Vec::new();
    walk(src_dir, &mut entries)?;
    entries.sort();

    for path in entries {
        let rel = path
            .strip_prefix(src_dir)
            .context("strip prefix")?
            .to_string_lossy()
            .replace('\\', "/");
        let mut data = Vec::new();
        File::open(&path)?.read_to_end(&mut data)?;
        pak_writer
            .write_file(&rel, true, data)
            .with_context(|| format!("write entry {rel}"))?;
    }

    let mut writer = pak_writer.write_index().context("finalize pak")?;
    writer.flush().context("flush pak")?;
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
