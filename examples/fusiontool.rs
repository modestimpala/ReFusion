//! Offline driver for the fusion pipeline, so it can be exercised and diffed
//! without launching the game.
//!
//! `staged_dir` is a directory of template `.uasset`/`.uexp` pairs as extracted
//! from the game pak; each `extracted_dir` is an unpacked mod pak. Both mirror
//! exactly what the wrapper builds at runtime. `fuse` is the shipping entry
//! point: `<my_dir>` stands in for the spoofed
//! `<install>/<Project>/Binaries/Win64` directory, and
//! `SHIMLOADER_PAK_DIR`/`SHIMLOADER_CFG_DIR` must be set.
//!
//! `skew` and `translate` exist to manufacture conditions that are otherwise
//! hard to obtain: a mod built against an older row struct, and a mod authored
//! in a non-Latin language. Both rewrite a copy you point them at.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use unreal_asset::{
    engine_version::EngineVersion,
    exports::Export,
    properties::{int_property::BytePropertyValue, Property, PropertyDataTrait},
    types::{
        fname::{FName, ToSerializedName},
        PackageIndex,
    },
    Asset,
};

const ENGINE: EngineVersion = EngineVersion::VER_UE4_27;

const USAGE: &str = "\
usage:
  fusiontool dump <uasset>                             text form of a DataTable
  fusiontool merge <staged_dir> <extracted_dir>...     merge in place
  fusiontool fuse <my_dir>                             whole pipeline, SHIMLOADER_* env
  fusiontool roundtrip <uasset>                        parse, write back, compare bytes
  fusiontool skew <uasset> <column-substring> <new>    fake an older row struct
  fusiontool translate <uasset> <text>                 fake non-Latin row content";

/// Same in-memory parsing the pipeline uses, so `dump` sees what it produces.
type DumpAsset = Asset<std::io::Cursor<Vec<u8>>>;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dump") if args.len() == 2 => dump(Path::new(&args[1])),
        Some("merge") if args.len() >= 3 => {
            init_logging();
            let staged = PathBuf::from(&args[1]);
            let extracted: Vec<(String, PathBuf)> = args[2..]
                .iter()
                .map(|d| {
                    let path = PathBuf::from(d);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (name, path)
                })
                .collect();
            let flagged = ue4ss::datatable::fuse_datatables(
                &staged,
                &extracted,
                &ue4ss::datatable::OnIncompatible::SkipAll,
            )?;
            report_flagged(&ue4ss::FuseOutcome {
                incompatible_mods: flagged,
                ..Default::default()
            });
            Ok(())
        }
        Some("fuse") if args.len() == 2 => {
            init_logging();
            let outcome = ue4ss::fuse(Path::new(&args[1]))?;
            report_flagged(&outcome);
            Ok(())
        }
        // Test aid: stands in for a mod built against an older row struct, which
        // is what a changed Blueprint struct member GUID looks like on disk.
        Some("skew") if args.len() == 4 => skew(Path::new(&args[1]), &args[2], &args[3]),
        // Does a parse+serialize cycle reproduce the engine's own bytes? Answers
        // whether `unreal_asset` recomputes anything (name hashes, offsets)
        // differently from how UE wrote it.
        Some("roundtrip") if args.len() == 2 => roundtrip(Path::new(&args[1])),
        // Test aid: stands in for a mod authored in a non-Latin language.
        Some("translate") if args.len() == 3 => translate(Path::new(&args[1]), &args[2]),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn init_logging() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Debug)
        .format_timestamp(None)
        .init();
}

fn report_flagged(outcome: &ue4ss::FuseOutcome) {
    for stem in &outcome.unreadable_paks {
        println!("unreadable pak: {stem}");
    }
    match outcome.incompatible_mods.is_empty() {
        true => println!("shape check: all mods match this game version"),
        false => {
            println!(
                "shape check: {} mismatching mod table(s)",
                outcome.incompatible_mods.len()
            );
            for entry in &outcome.incompatible_mods {
                let verdict = match entry.merged_anyway {
                    true => "merged anyway",
                    false => "skipped",
                };
                println!("  {} ({}) - {verdict}", entry.mod_name, entry.table);
                for problem in &entry.problems {
                    println!("      {problem}");
                }
            }
        }
    }
    // Exactly what the wrapper puts in its message box on Windows.
    let body = ue4ss::left_out_report(outcome);
    if !body.is_empty() {
        let shown = match outcome.report_is_new {
            true => "shown",
            false => "suppressed: same as the last run",
        };
        println!("\n--- message box ({shown}) ---\n{body}\n---");
    }
}

/// Rename the first row column matching `needle` in every row of a DataTable,
/// simulating a mod whose struct member GUIDs no longer match the game's.
fn skew(uasset: &Path, needle: &str, new_name: &str) -> Result<()> {
    let uexp = uasset.with_extension("uexp");
    let mut asset = Asset::new(File::open(uasset)?, Some(File::open(&uexp)?), ENGINE)?;

    let renamed = asset.add_fname(new_name);
    let idx = asset
        .asset_data
        .exports
        .iter()
        .position(|e| matches!(e, Export::DataTableExport(_)))
        .context("no DataTableExport")?;
    let Export::DataTableExport(table) = &mut asset.asset_data.exports[idx] else {
        unreachable!()
    };

    let mut hits = 0usize;
    for row in &mut table.table.data {
        for prop in &mut row.value {
            if prop.get_name().get_content().contains(needle) {
                *prop.get_name_mut() = renamed.clone();
                hits += 1;
            }
        }
    }
    anyhow::ensure!(hits > 0, "no column matching {needle:?}");

    let mut main_out = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(uasset)?;
    let mut bulk_out = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&uexp)?;
    asset.write_data(&mut main_out, Some(&mut bulk_out))?;
    println!("renamed {hits} column(s) matching {needle:?} to {new_name:?} in {uasset:?}");
    Ok(())
}

/// Test aid: rewrite the first row's key and every text value in that row to
/// `text`, standing in for a mod authored in a non-Latin language. Row keys are
/// FNames (name-map entries, hashed and stored wide when non-ASCII) while item
/// names and descriptions are FStrings, so one row covers both encodings.
fn translate(uasset: &Path, text: &str) -> Result<()> {
    let uexp = uasset.with_extension("uexp");
    let mut asset: DumpAsset = Asset::new(
        std::io::Cursor::new(std::fs::read(uasset)?),
        Some(std::io::Cursor::new(std::fs::read(&uexp)?)),
        ENGINE,
    )?;

    let renamed = asset.add_fname(text);
    let idx = asset
        .asset_data
        .exports
        .iter()
        .position(|e| matches!(e, Export::DataTableExport(_)))
        .context("no DataTableExport")?;
    let Export::DataTableExport(table) = &mut asset.asset_data.exports[idx] else {
        unreachable!()
    };
    let row = table.table.data.first_mut().context("table has no rows")?;
    row.name = renamed;

    let mut texts = 0usize;
    for prop in &mut row.value {
        match prop {
            Property::StrProperty(p) => {
                p.value = Some(text.to_string());
                texts += 1;
            }
            Property::TextProperty(p) => {
                p.culture_invariant_string = Some(text.to_string());
                texts += 1;
            }
            _ => {}
        }
    }

    let mut main_out = std::io::Cursor::new(Vec::new());
    let mut bulk_out = std::io::Cursor::new(Vec::new());
    asset.write_data(&mut main_out, Some(&mut bulk_out))?;
    std::fs::write(uasset, main_out.into_inner())?;
    std::fs::write(&uexp, bulk_out.into_inner())?;
    println!("row key and {texts} text value(s) set to {text:?} in {uasset:?}");
    Ok(())
}

/// Parse an asset and serialize it straight back, then compare bytes with the
/// original. Any difference is something `unreal_asset` recomputes rather than
/// preserves - most importantly the per-name FName hashes, which it always
/// regenerates. Reports the first differing offset and which region it lands in.
fn roundtrip(uasset: &Path) -> Result<()> {
    let uexp = uasset.with_extension("uexp");
    let original = std::fs::read(uasset).with_context(|| format!("read {uasset:?}"))?;
    let original_bulk = std::fs::read(&uexp).with_context(|| format!("read {uexp:?}"))?;

    let asset: DumpAsset = Asset::new(
        std::io::Cursor::new(original.clone()),
        Some(std::io::Cursor::new(original_bulk.clone())),
        ENGINE,
    )
    .with_context(|| format!("parse {uasset:?}"))?;

    let name_offset = asset.asset_data.exports.len();
    let names = asset
        .get_name_map()
        .get_ref()
        .get_name_map_index_list()
        .len();

    let mut main_out = std::io::Cursor::new(Vec::new());
    let mut bulk_out = std::io::Cursor::new(Vec::new());
    asset.write_data(&mut main_out, Some(&mut bulk_out))?;
    let (rewritten, rewritten_bulk) = (main_out.into_inner(), bulk_out.into_inner());

    println!("{} exports, {names} names", name_offset);
    for (label, a, b) in [
        ("uasset", &original, &rewritten),
        ("uexp", &original_bulk, &rewritten_bulk),
    ] {
        match a.iter().zip(b.iter()).position(|(x, y)| x != y) {
            None if a.len() == b.len() => println!("{label}: IDENTICAL ({} bytes)", a.len()),
            None => println!(
                "{label}: common prefix identical, lengths differ ({} vs {})",
                a.len(),
                b.len()
            ),
            Some(at) => println!(
                "{label}: DIFFERS at 0x{at:x} ({} vs {} bytes): {:02x?} vs {:02x?}",
                a.len(),
                b.len(),
                &a[at..(at + 8).min(a.len())],
                &b[at..(at + 8).min(b.len())]
            ),
        }
    }
    Ok(())
}

fn dump(uasset: &Path) -> Result<()> {
    let uexp = uasset.with_extension("uexp");
    let asset: DumpAsset = Asset::new(
        std::io::Cursor::new(std::fs::read(uasset).with_context(|| format!("read {uasset:?}"))?),
        Some(std::io::Cursor::new(
            std::fs::read(&uexp).with_context(|| format!("read {uexp:?}"))?,
        )),
        ENGINE,
    )
    .with_context(|| format!("parse {uasset:?}"))?;

    let table = asset
        .asset_data
        .exports
        .iter()
        .find_map(|e| match e {
            Export::DataTableExport(dt) => Some(dt),
            _ => None,
        })
        .context("asset has no DataTableExport")?;

    println!(
        "# rows={} imports={} names={}",
        table.table.data.len(),
        asset.imports.len(),
        asset
            .get_name_map()
            .get_ref()
            .get_name_map_index_list()
            .len()
    );
    for row in &table.table.data {
        println!(
            "ROW {} struct={}",
            name(&row.name),
            row.struct_type.as_ref().map(name).unwrap_or_default()
        );
        for prop in &row.value {
            print_prop(&asset, prop, 1);
        }
    }
    Ok(())
}

/// `content` plus the FName instance number when it carries one - a dropped
/// number silently renames rows (`foo_2` -> `foo`), so it has to be visible.
fn name(fname: &FName) -> String {
    match fname.get_number() {
        0 => fname.get_content(),
        n => format!("{}#{}", fname.get_content(), n),
    }
}

fn opt_name(fname: &Option<FName>) -> String {
    fname.as_ref().map(name).unwrap_or_else(|| "-".to_string())
}

fn describe_ref(asset: &DumpAsset, index: PackageIndex) -> String {
    if index.index == 0 {
        return "None".to_string();
    }
    if index.is_import() {
        let slot = (-index.index - 1) as usize;
        return match asset.imports.get(slot) {
            Some(import) => format!(
                "import {}:{}",
                name(&import.class_name),
                name(&import.object_name)
            ),
            None => format!("import OUT-OF-RANGE({})", index.index),
        };
    }
    format!("export {}", index.index)
}

fn print_prop(asset: &DumpAsset, prop: &Property, depth: usize) {
    let pad = "  ".repeat(depth);
    let label = name(&prop.get_name());
    let ty = prop.to_serialized_name();

    match prop {
        Property::StructProperty(p) => {
            println!("{pad}{label} Struct<{}>", opt_name(&p.struct_type));
            for inner in &p.value {
                print_prop(asset, inner, depth + 1);
            }
        }
        Property::ArrayProperty(p) => {
            println!(
                "{pad}{label} Array<{}> len={}",
                opt_name(&p.array_type),
                p.value.len()
            );
            for inner in &p.value {
                print_prop(asset, inner, depth + 1);
            }
        }
        Property::SetProperty(p) => {
            println!(
                "{pad}{label} Set<{}> len={}",
                opt_name(&p.array_type),
                p.value.value.len()
            );
            for inner in &p.value.value {
                print_prop(asset, inner, depth + 1);
            }
        }
        Property::MapProperty(p) => {
            println!(
                "{pad}{label} Map<{},{}> len={}",
                name(&p.key_type),
                name(&p.value_type),
                p.value.len()
            );
            for (_, key, value) in p.value.iter() {
                print_prop(asset, key, depth + 1);
                print_prop(asset, value, depth + 2);
            }
        }
        Property::NameProperty(p) => println!("{pad}{label} Name = {}", name(&p.value)),
        Property::StrProperty(p) => {
            println!("{pad}{label} Str = {:?}", p.value)
        }
        Property::TextProperty(p) => println!(
            "{pad}{label} Text = value={:?} invariant={:?} ns={:?} table={} flags={} history={:?}",
            p.value,
            p.culture_invariant_string,
            p.namespace,
            opt_name(&p.table_id),
            p.flags,
            p.history_type
        ),
        Property::ObjectProperty(p) => {
            println!("{pad}{label} Object = {}", describe_ref(asset, p.value))
        }
        Property::SoftObjectProperty(p) => println!(
            "{pad}{label} SoftObject = pkg={} asset={} sub={:?}",
            opt_name(&p.value.asset_path.package_name),
            name(&p.value.asset_path.asset_name),
            p.value.sub_path_string
        ),
        Property::AssetObjectProperty(p) => {
            println!("{pad}{label} AssetObject = {:?}", p.value)
        }
        Property::SoftObjectPathProperty(p) => println!(
            "{pad}{label} SoftObjectPath = asset={} sub={:?} path={:?}",
            opt_name(&p.asset_path_name),
            p.sub_path,
            p.path
        ),
        Property::SoftAssetPathProperty(p) => println!(
            "{pad}{label} SoftAssetPath = asset={} sub={:?} path={:?}",
            opt_name(&p.asset_path_name),
            p.sub_path,
            p.path
        ),
        Property::SoftClassPathProperty(p) => println!(
            "{pad}{label} SoftClassPath = asset={} sub={:?} path={:?}",
            opt_name(&p.asset_path_name),
            p.sub_path,
            p.path
        ),
        Property::ByteProperty(p) => {
            let value = match &p.value {
                BytePropertyValue::Byte(b) => b.to_string(),
                BytePropertyValue::FName(f) => name(f),
            };
            println!("{pad}{label} Byte<{}> = {value}", opt_name(&p.enum_type));
        }
        Property::EnumProperty(p) => println!(
            "{pad}{label} Enum<{}> = {}",
            opt_name(&p.enum_type),
            opt_name(&p.value)
        ),
        Property::BoolProperty(p) => println!("{pad}{label} Bool = {}", p.value),
        Property::IntProperty(p) => println!("{pad}{label} Int = {}", p.value),
        Property::Int8Property(p) => println!("{pad}{label} Int8 = {}", p.value),
        Property::Int16Property(p) => println!("{pad}{label} Int16 = {}", p.value),
        Property::Int64Property(p) => println!("{pad}{label} Int64 = {}", p.value),
        Property::UInt16Property(p) => println!("{pad}{label} UInt16 = {}", p.value),
        Property::UInt32Property(p) => println!("{pad}{label} UInt32 = {}", p.value),
        Property::UInt64Property(p) => println!("{pad}{label} UInt64 = {}", p.value),
        Property::FloatProperty(p) => println!("{pad}{label} Float = {:?}", p.value),
        Property::DoubleProperty(p) => println!("{pad}{label} Double = {:?}", p.value),
        Property::GuidProperty(p) => println!("{pad}{label} Guid = {:?}", p.value),
        Property::LinearColorProperty(p) => println!("{pad}{label} LinearColor = {:?}", p.color),
        Property::ColorProperty(p) => println!("{pad}{label} Color = {:?}", p.color),
        Property::VectorProperty(p) => println!("{pad}{label} Vector = {:?}", p.value),
        Property::Vector2DProperty(p) => println!("{pad}{label} Vector2D = {:?}", p.value),
        Property::RotatorProperty(p) => println!("{pad}{label} Rotator = {:?}", p.value),
        Property::IntPointProperty(p) => println!("{pad}{label} IntPoint = {:?}", p.value),
        // Anything else: type only. If a diff ever lands on one of these lines,
        // add an arm rather than guessing.
        _ => println!("{pad}{label} {ty} = <value not printed>"),
    }
}
