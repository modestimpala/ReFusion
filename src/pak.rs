use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use repak::{Compression, PakBuilder, Version};

/// What a pak in the user's pak directory is, as far as fusion cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakKind {
    /// Mod content. Merge sources live at
    /// `Content/Mods/<mod>/_Content/main/datatables/_<table>.uasset`, so a mod
    /// pak never carries a table at the game's own path.
    ModContent,
    /// An already-merged patch pak: tables sitting at the game's template paths.
    /// Both the C# Fusion and this wrapper produce one, and two of them mounted
    /// at once means the engine silently picks a winner.
    MergedOutput,
}

/// Classify `pak_path` from its index alone - no entry data is decompressed, so
/// this is cheap enough to run on every pak at every launch.
///
/// `project_prefix` is the leading path segment of the game's content tree
/// (e.g. `VotV`).
pub fn classify(pak_path: &Path, project_prefix: &str) -> Result<PakKind> {
    let mut file =
        BufReader::new(File::open(pak_path).with_context(|| format!("open {pak_path:?}"))?);
    let pak = PakBuilder::new()
        .reader(&mut file)
        .with_context(|| format!("read index of {pak_path:?}"))?;

    let base = logical_base(pak.mount_point());
    let prefix = format!("{project_prefix}/");
    let mut template_tables = 0usize;
    let mut merged_config = false;
    for entry in pak.files() {
        let full = format!("{base}{}", entry.replace('\\', "/"));
        let rel = full.strip_prefix(&prefix).unwrap_or(&full);
        if rel.starts_with("Content/main/datatables/") && rel.ends_with(".uasset") {
            template_tables += 1;
        } else if rel.starts_with("Config/") && rel.ends_with(".ini") {
            merged_config = true;
        }
    }

    // One or two overridden tables is a legitimate asset-replacement mod; a
    // whole set of them, or tables shipped next to merged `Config/*.ini`, is a
    // fusion output.
    let merged = template_tables >= 5 || (template_tables >= 1 && merged_config);
    Ok(match merged {
        true => PakKind::MergedOutput,
        false => PakKind::ModContent,
    })
}

/// A pak's mount point as a content-tree prefix.
///
/// UnrealPak records where the pak's contents belong, relative to the engine's
/// `Binaries/Win64` (`../../../<Project>/...`). It depends on the folder the
/// author happened to pack, so entry paths alone are not comparable between
/// mods: one pak yields `_Content/main/datatables/_list_props.uasset`, another
/// mounted deeper yields just `_list_props.uasset`. Joining this prefix back on
/// makes every entry a full content path.
fn logical_base(mount_point: &str) -> String {
    let mut base = mount_point;
    while let Some(rest) = base.strip_prefix("../") {
        base = rest;
    }
    let base = base.trim_start_matches('/');
    match base.is_empty() || base.ends_with('/') {
        true => base.to_string(),
        false => format!("{base}/"),
    }
}

/// Extract every entry from `pak_path` for which `keep` returns `Some(rel)`,
/// returning how many files were written.
///
/// `keep` sees the entry's full content path (mount point included, engine-
/// relative `../` stripped), not the raw index entry. The returned `rel` is the
/// path under `out_dir` to write to; returning `None` skips the entry.
pub fn extract_filtered<F>(pak_path: &Path, out_dir: &Path, mut keep: F) -> Result<usize>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut file =
        BufReader::new(File::open(pak_path).with_context(|| format!("open {pak_path:?}"))?);
    let pak = PakBuilder::new()
        .reader(&mut file)
        .with_context(|| format!("read index of {pak_path:?}"))?;

    let base = logical_base(pak.mount_point());
    let mut written = 0usize;
    for entry in pak.files() {
        // Most packers write `/`, but not all; a `\` entry would silently fail
        // every path test `keep` makes and drop the file without a word.
        let full = format!("{base}{}", entry.replace('\\', "/"));
        let Some(rel) = keep(&full) else { continue };
        let bytes = pak
            .get(&entry, &mut file)
            .with_context(|| format!("read entry {entry}"))?;
        let dest = out_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("write {dest:?}"))?;
        written += 1;
    }
    Ok(written)
}

/// Pack `src_dir` into `out_pak`.
///
/// The pak is built beside its destination and renamed into place, because the
/// game mounts whatever sits at that path on the next launch: a run that dies
/// partway through packing - out of disk, permissions, killed process - would
/// otherwise leave a truncated pak that UE either rejects or reads as a table
/// with missing rows. A rename is atomic enough for that; either the previous
/// pak or the complete new one is what the engine finds.
pub fn pack_dir(src_dir: &Path, out_pak: &Path, mount_point: &str) -> Result<()> {
    if let Some(parent) = out_pak.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Per-process name: two game instances starting together would otherwise
    // build into the same file and hand the engine the interleaved result.
    let staging = out_pak.with_extension(format!("pak.{}.partial", std::process::id()));

    let result = build_pak(src_dir, &staging, mount_point);
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
        return result;
    }

    // Windows `rename` refuses an existing destination.
    if out_pak.exists() {
        std::fs::remove_file(out_pak).with_context(|| format!("replace previous {out_pak:?}"))?;
    }
    std::fs::rename(&staging, out_pak).with_context(|| format!("move {staging:?} into place"))?;
    Ok(())
}

fn build_pak(src_dir: &Path, out_pak: &Path, mount_point: &str) -> Result<()> {
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
