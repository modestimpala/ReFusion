use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use unreal_asset::containers::indexed_map::IndexedMap;
use unreal_asset::{
    asset::name_map::NameMap,
    cast,
    containers::shared_resource::SharedResource,
    engine_version::EngineVersion,
    exports::Export,
    properties::{struct_property::StructProperty, Property, PropertyDataTrait},
    types::{
        fname::{FName, FNameContainer, ToSerializedName},
        PackageIndex,
    },
    Asset,
};

const ENGINE: EngineVersion = EngineVersion::VER_UE4_27;

/// A mod whose rows don't match the row struct the installed game version uses.
#[derive(Debug, Clone)]
pub struct IncompatibleMod {
    /// Directory name of the mod pak the rows came from.
    pub mod_name: String,
    /// Table stem, e.g. `list_props`.
    pub table: String,
    /// Human-readable mismatches, capped for display.
    pub problems: Vec<String>,
    /// Merged regardless, because the user allowed this mod through.
    pub merged_anyway: bool,
}

/// Which mismatching mods to merge anyway instead of leaving out.
#[derive(Debug, Clone, Default)]
pub enum OnIncompatible {
    /// Leave every one of them out. Their rows would load with the mismatched
    /// fields blank.
    #[default]
    SkipAll,
    /// Merge all of them, mismatched fields and all.
    MergeAll,
    /// Merge only these mods - the useful case, since a mod that loses one
    /// minor flag is worth keeping while one that loses its meshes is not.
    MergeOnly(std::collections::HashSet<String>),
}

impl OnIncompatible {
    fn allows(&self, mod_name: &str) -> bool {
        match self {
            Self::SkipAll => false,
            Self::MergeAll => true,
            Self::MergeOnly(names) => names.contains(mod_name),
        }
    }
}

/// Table stem (`list_props`) to the mod assets that override it, paired with
/// the name of the mod pak each came from.
type ModFiles = std::collections::HashMap<String, Vec<(String, PathBuf)>>;

/// Merge every mod's rows into the game's tables, in place under `staged_dir`.
///
/// A mod file is `_Foo.uasset` against the game's `Foo.uasset`; the leading
/// underscore is the convention this pipeline inherits from the C# Fusion, and
/// stripping it is how a mod table is matched to a game table.
///
/// Returns every mod whose rows mismatch the game's row structs, whether or not
/// `on_incompatible` kept it. Everything else is merged as usual.
pub fn fuse_datatables(
    staged_dir: &Path,
    extracted_dirs: &[(String, PathBuf)],
    on_incompatible: &OnIncompatible,
) -> Result<Vec<IncompatibleMod>> {
    if !staged_dir.is_dir() {
        warn!("no staged datatable dir at {staged_dir:?}");
        return Ok(Vec::new());
    }

    let templates: Vec<PathBuf> = std::fs::read_dir(staged_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| crate::walk::has_extension(p, "uasset"))
        .collect();
    let mod_files = index_mod_files(extracted_dirs);
    warn_unmatched_tables(&templates, &mod_files);

    let check = shape_check(&templates, &mod_files, on_incompatible);

    for template in &templates {
        let Some(stem) = template.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(mod_paths) = mod_files.get(stem) else {
            debug!("no mod overrides for template {stem}");
            continue;
        };
        let usable: Vec<PathBuf> = mod_paths
            .iter()
            .filter(|(mod_name, _)| !check.rejected.contains(mod_name))
            .map(|(_, path)| path.clone())
            .collect();
        if usable.is_empty() {
            continue;
        }
        if let Err(e) = merge_one(template, &usable) {
            warn!("merge failed for {stem}: {e:#}");
        }
    }

    info!("datatable merge complete");
    Ok(check.incompatible)
}

/// Group every mod's datatable assets by the game table they override.
fn index_mod_files(extracted_dirs: &[(String, PathBuf)]) -> ModFiles {
    let mut out = ModFiles::new();
    for (mod_name, dir) in extracted_dirs {
        for path in find_datatable_uassets(dir) {
            let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let stem = file_name.strip_prefix('_').unwrap_or(file_name).to_string();
            out.entry(stem).or_default().push((mod_name.clone(), path));
        }
    }
    out
}

/// A mod table whose name matches nothing the game ships contributes nothing,
/// and from the outside looks exactly like a mod that failed to merge. Usually a
/// typo, or a table renamed by a game update.
fn warn_unmatched_tables(templates: &[PathBuf], mod_files: &ModFiles) {
    let template_stems: std::collections::HashSet<&str> = templates
        .iter()
        .filter_map(|t| t.file_stem().and_then(|s| s.to_str()))
        .collect();
    for (stem, sources) in mod_files {
        if template_stems.contains(stem.as_str()) {
            continue;
        }
        for (mod_name, path) in sources {
            warn!(
                "{mod_name}: no game table named {stem:?} - {:?} will not be merged",
                path.file_name().unwrap_or_default()
            );
        }
    }
}

/// Verdict of the shape check, before anything is merged.
struct ShapeCheck {
    /// Every mismatch found, for the log and the user-facing report.
    incompatible: Vec<IncompatibleMod>,
    /// Mods to leave out of every table.
    rejected: std::collections::HashSet<String>,
}

/// Check every mod table against the installed game's row struct.
///
/// A mod that disagrees in one table is dropped from all of them: its tables
/// reference each other by row name (a store row names a prop row), so a
/// half-merged mod is worse than an absent one.
fn shape_check(
    templates: &[PathBuf],
    mod_files: &ModFiles,
    on_incompatible: &OnIncompatible,
) -> ShapeCheck {
    let started = std::time::Instant::now();
    let mut incompatible: Vec<IncompatibleMod> = Vec::new();
    let mut rejected: std::collections::HashSet<String> = std::collections::HashSet::new();

    for template in templates {
        let Some(stem) = template.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(mod_paths) = mod_files.get(stem) else {
            continue;
        };

        let template_shape = match template_shape(template) {
            Ok(Some(shape)) => shape,
            // An empty table teaches nothing about the struct, so there is
            // nothing to check against.
            Ok(None) => {
                debug!("template {stem} has no rows; skipping shape check");
                continue;
            }
            Err(e) => {
                warn!("cannot read template {stem} for shape check: {e:#}");
                continue;
            }
        };

        for (mod_name, path) in mod_paths {
            let issues = match mod_issues(path, &template_shape) {
                Ok(issues) => issues,
                // A damaged file is not struct drift, so `merge_incompatible`
                // does not apply: there is nothing to merge either way, and
                // merging the mod's other tables would leave exactly the
                // half-merged state this pass exists to prevent.
                Err(e) => {
                    warn!(
                        "mod {mod_name}: {path:?} could not be read; dropping it from the \
                         merge: {e:#}"
                    );
                    rejected.insert(mod_name.clone());
                    incompatible.push(IncompatibleMod {
                        mod_name: mod_name.clone(),
                        table: stem.to_string(),
                        problems: vec![format!("{} could not be read", file_label(path))],
                        merged_anyway: false,
                    });
                    continue;
                }
            };
            if !issues.ignored.is_empty() {
                // The mod predates a field removal. Harmless, but worth a trail
                // when someone is working out why a value has no effect.
                info!(
                    "mod {mod_name} sets fields {stem} no longer has (ignored by the engine): {}",
                    issues.ignored.join("; ")
                );
            }
            if issues.lost.is_empty() {
                continue;
            }
            let merged_anyway = on_incompatible.allows(mod_name);
            match merged_anyway {
                false => {
                    warn!(
                        "mod {mod_name} was built against a different {stem} struct; dropping \
                         it from the merge: {}",
                        issues.lost.join("; ")
                    );
                    rejected.insert(mod_name.clone());
                }
                true => warn!(
                    "mod {mod_name} was built against a different {stem} struct but is allowed \
                     by merge_incompatible; merging it, so these fields will be blank in \
                     game: {}",
                    issues.lost.join("; ")
                ),
            }
            incompatible.push(IncompatibleMod {
                mod_name: mod_name.clone(),
                table: stem.to_string(),
                problems: issues.lost,
                merged_anyway,
            });
        }
    }

    info!("shape check took {}ms", started.elapsed().as_millis());
    ShapeCheck {
        incompatible,
        rejected,
    }
}

/// Just the file name, for messages the user reads.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Flattened `column path -> type signatures` map of a row struct.
///
/// A path maps to a *set* because tagged properties may repeat a name (UE
/// distinguishes them by duplication index); collapsing to one signature would
/// let a mismatched duplicate hide behind a matching one.
type Shape = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

/// Template row struct, indexed both exactly and by member base name.
struct TemplateShape {
    /// `column path -> type signatures`, as serialized.
    exact: Shape,
    /// `base column path -> exact paths`, i.e. member names with the ordinal and
    /// GUID suffix stripped. Lets a rebuilt member be told apart from a member
    /// the game no longer has at all.
    by_base: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Shape of the template's first row, or `None` when the table is empty.
fn template_shape(template: &Path) -> Result<Option<TemplateShape>> {
    let asset = open_asset(template, &template.with_extension("uexp"))?;
    let idx = find_data_table_export(&asset)
        .ok_or_else(|| anyhow::anyhow!("template has no DataTableExport"))?;
    let Some(table) = cast!(Export, DataTableExport, &asset.asset_data.exports[idx]) else {
        return Err(anyhow::anyhow!("template export not a DataTableExport"));
    };
    let Some(first) = table.table.data.first() else {
        return Ok(None);
    };

    let exact = row_shape(first);
    let mut by_base: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for column in exact.keys() {
        by_base
            .entry(base_path(column))
            .or_default()
            .insert(column.clone());
    }
    Ok(Some(TemplateShape { exact, by_base }))
}

/// How a mod's rows disagree with the game's current row struct.
#[derive(Default)]
struct ModIssues {
    /// Values the game will never receive: the member was rebuilt or retyped,
    /// so the mod's tag doesn't match and the field takes its default.
    lost: Vec<String>,
    /// Tags for members this version of the game doesn't have at all. The
    /// engine ignores them and the rest of the row still loads, so these are
    /// reported but not disqualifying.
    ignored: Vec<String>,
}

/// Compare `path`'s rows against the game's row struct.
///
/// Blueprint struct members serialize as `<name>_<ordinal>_<GUID>`, and both the
/// ordinal and the GUID change when the member is re-added in the editor. So
/// there are two very different mismatches, and they need different verdicts:
///
/// - The game still has a member of that name under a different suffix, or the
///   type changed: the mod's value silently goes nowhere and the field defaults.
///   That is what turns a stale mod into nameless, meshless items.
/// - The game has no such member at all: the field was removed, the engine
///   ignores the tag, and everything else in the row still applies.
fn mod_issues(path: &Path, template: &TemplateShape) -> Result<ModIssues> {
    const MAX_REPORTED: usize = 4;

    let asset = open_asset(path, &path.with_extension("uexp"))?;
    let idx = find_data_table_export(&asset)
        .ok_or_else(|| anyhow::anyhow!("mod asset has no DataTableExport"))?;
    let Some(table) = cast!(Export, DataTableExport, &asset.asset_data.exports[idx]) else {
        return Err(anyhow::anyhow!("mod export not a DataTableExport"));
    };

    let mut issues = ModIssues::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &table.table.data {
        for (column, signatures) in row_shape(row) {
            for signature in signatures {
                let (bucket, problem) = match template.exact.get(&column) {
                    Some(expected) if expected.contains(&signature) => continue,
                    Some(expected) => (
                        &mut issues.lost,
                        format!(
                            "`{column}` is {signature} in the mod but {} in the game",
                            expected.iter().cloned().collect::<Vec<_>>().join("/")
                        ),
                    ),
                    None => match template.by_base.get(&base_path(&column)) {
                        Some(current) => (
                            &mut issues.lost,
                            format!(
                                "`{column}` was rebuilt in this version (now `{}`), so the \
                                 mod's value is dropped",
                                current.iter().cloned().collect::<Vec<_>>().join("/")
                            ),
                        ),
                        None => (
                            &mut issues.ignored,
                            format!("`{column}` no longer exists in this version"),
                        ),
                    },
                };
                if seen.insert(problem.clone()) {
                    bucket.push(problem);
                }
            }
        }
    }

    for bucket in [&mut issues.lost, &mut issues.ignored] {
        if bucket.len() > MAX_REPORTED {
            let extra = bucket.len() - MAX_REPORTED;
            bucket.truncate(MAX_REPORTED);
            bucket.push(format!("and {extra} more"));
        }
    }
    Ok(issues)
}

/// Member path with each segment's `_<ordinal>_<GUID>` suffix removed, so
/// `displayName_8_FE83ADBF40AA162942FCE589F5806DD2` becomes `displayName`.
fn base_path(path: &str) -> String {
    let mut base = String::with_capacity(path.len());
    for (i, segment) in path.split('.').enumerate() {
        if i > 0 {
            base.push('.');
        }
        base.push_str(member_base(segment));
    }
    base
}

fn member_base(segment: &str) -> &str {
    let Some((rest, guid)) = segment.rsplit_once('_') else {
        return segment;
    };
    if guid.len() != 32 || !guid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return segment;
    }
    match rest.rsplit_once('_') {
        Some((base, ordinal))
            if !ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => rest,
    }
}

fn row_shape(row: &StructProperty) -> Shape {
    let mut shape = Shape::new();
    collect_shape(&row.value, "", &mut shape);
    shape
}

fn collect_shape(props: &[Property], prefix: &str, out: &mut Shape) {
    for prop in props {
        let name = prop.get_name().get_content();
        let path = match prefix.is_empty() {
            true => name,
            false => format!("{prefix}.{name}"),
        };
        out.entry(path.clone())
            .or_default()
            .insert(type_signature(prop));
        // Nested structs skew the same way as the row itself. Containers are
        // described by their first element: length is data, not shape.
        match prop {
            Property::StructProperty(p) => collect_shape(&p.value, &path, out),
            Property::ArrayProperty(p) => {
                if let Some(first) = p.value.first() {
                    collect_shape(std::slice::from_ref(first), &path, out);
                }
            }
            Property::SetProperty(p) => {
                if let Some(first) = p.value.value.first() {
                    collect_shape(std::slice::from_ref(first), &path, out);
                }
            }
            _ => {}
        }
    }
}

/// Property type as the engine matches it: kind plus the subtype that has to
/// agree (struct name, enum name, element type).
fn type_signature(prop: &Property) -> String {
    let kind = prop.to_serialized_name();
    let sub = |name: &Option<FName>| {
        name.as_ref()
            .map(FName::get_content)
            .unwrap_or_else(|| "-".to_string())
    };
    match prop {
        Property::StructProperty(p) => format!("{kind}<{}>", sub(&p.struct_type)),
        Property::ArrayProperty(p) => format!("{kind}<{}>", sub(&p.array_type)),
        Property::SetProperty(p) => format!("{kind}<{}>", sub(&p.array_type)),
        Property::ByteProperty(p) => format!("{kind}<{}>", sub(&p.enum_type)),
        Property::EnumProperty(p) => format!("{kind}<{}>", sub(&p.enum_type)),
        Property::MapProperty(p) => format!(
            "{kind}<{},{}>",
            p.key_type.get_content(),
            p.value_type.get_content()
        ),
        _ => kind,
    }
}

/// Merge sources inside an unpacked mod pak: `*.uasset` named `_Foo.uasset` in
/// a `datatables` directory.
fn find_datatable_uassets(dir: &Path) -> Vec<PathBuf> {
    crate::walk::files(dir, &mut |path| {
        if !crate::walk::has_extension(path, "uasset") {
            return false;
        }
        let in_datatables = path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("datatables"));
        if !in_datatables {
            return false;
        }
        // The mod side of a merge is named `_Foo.uasset` against the game's
        // `Foo.uasset`. Without the underscore it is an asset-replacement file,
        // not a merge source, which is worth saying out loud because it sits in
        // exactly the right folder and does nothing.
        let named_as_source = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('_'));
        if !named_as_source {
            warn!("{path:?} is in a datatables folder but not named with a leading underscore, so it is not a merge source");
        }
        named_as_source
    })
    .unwrap_or_default()
}

fn merge_one(template: &Path, mod_uassets: &[PathBuf]) -> Result<()> {
    let template_uexp = template.with_extension("uexp");

    let start = std::time::Instant::now();
    let mut main = open_asset(template, &template_uexp)?;
    let parsed = start.elapsed();

    for mod_uasset in mod_uassets {
        let mod_uexp = mod_uasset.with_extension("uexp");
        let mut modded = open_asset(mod_uasset, &mod_uexp)?;
        merge_into(&mut main, &mut modded).with_context(|| format!("merging {mod_uasset:?}"))?;
    }
    let merged = start.elapsed();

    write_asset(&main, template, &template_uexp)?;
    let written = start.elapsed();
    info!(
        "wrote merged {:?} ({} mod sources) parse={}ms merge={}ms write={}ms",
        template,
        mod_uassets.len(),
        parsed.as_millis(),
        (merged - parsed).as_millis(),
        (written - merged).as_millis()
    );
    Ok(())
}

/// Assets are parsed from and written to memory, not straight to `File`.
///
/// `unreal_asset` reads and writes in small primitive steps - four bytes per
/// FName index, and so on - so handing it a `File` turns a 2 MB table into
/// hundreds of thousands of syscalls: `list_props` measured 1.2s to parse and
/// 3.6s to write that way, against 6ms for the merge itself. A `Cursor` over a
/// `Vec` costs one read and one write per file instead.
type MemAsset = Asset<std::io::Cursor<Vec<u8>>>;

fn open_asset(uasset: &Path, uexp: &Path) -> Result<MemAsset> {
    let main =
        std::io::Cursor::new(std::fs::read(uasset).with_context(|| format!("read {uasset:?}"))?);
    let bulk = match uexp.is_file() {
        true => Some(std::io::Cursor::new(
            std::fs::read(uexp).with_context(|| format!("read {uexp:?}"))?,
        )),
        false => None,
    };
    Asset::new(main, bulk, ENGINE).with_context(|| format!("parse {uasset:?}"))
}

fn write_asset(asset: &MemAsset, uasset: &Path, uexp: &Path) -> Result<()> {
    let mut main_out = std::io::Cursor::new(Vec::new());
    let mut bulk_out = std::io::Cursor::new(Vec::new());
    asset
        .write_data(&mut main_out, Some(&mut bulk_out))
        .with_context(|| format!("serialize {uasset:?}"))?;
    std::fs::write(uasset, main_out.into_inner()).with_context(|| format!("write {uasset:?}"))?;
    std::fs::write(uexp, bulk_out.into_inner()).with_context(|| format!("write {uexp:?}"))?;
    Ok(())
}

/// Append `modded`'s rows into `main`'s DataTable.
///
/// Two things have to be translated on the way across, and both fail silently
/// if missed:
///
/// - **Names.** Every asset owns a name map, and an `FName` serializes as the
///   raw `(index, number)` pair it was parsed with, so a name carried over
///   unchanged writes out as whatever unrelated string sits at that index in the
///   destination. `rekey_into` re-points every one of them, which has to cover
///   all ~85 property kinds: item icons and meshes are `SoftObjectPathProperty`
///   and display names are `TextProperty`, so a hand-written list of the obvious
///   kinds produces rows with no name, icon or mesh. Instance numbers matter too
///   (a row named `foo_2` is not `foo`), so names are re-keyed rather than
///   re-added.
/// - **Import references.** A `PackageIndex` is not a name, so it is outside
///   that traversal and is shifted here.
///
/// `Asset::rebuild_name_map` does the name half in one call, but it walks the
/// whole destination asset: on `list_props` that is 2477 rows and ~10k names
/// re-examined per merged mod, measured at 6.9s of a 7.0s cold merge. Re-keying
/// only what moved is proportional to the mod instead of to the game's table.
fn merge_into(main: &mut MemAsset, modded: &mut MemAsset) -> Result<()> {
    main.package_guid = modded.package_guid;

    let import_offset = main.imports.len() as i32;
    // Owned handle to main's name map: rows and imports live in other fields, so
    // they can be mutated while this is held.
    let name_map = main.get_name_map();

    // Mod imports land after main's, so their internal references move too.
    let mod_imports = std::mem::take(&mut modded.imports);
    for mut import in mod_imports {
        shift_import_ref(&mut import.outer_index, import_offset);
        rekey_into(&mut import, &name_map);
        main.add_import(import);
    }

    let main_table_idx = find_data_table_export(main)
        .ok_or_else(|| anyhow::anyhow!("template has no DataTableExport"))?;
    let mod_table_idx = find_data_table_export(modded)
        .ok_or_else(|| anyhow::anyhow!("mod has no DataTableExport"))?;

    let mod_rows = {
        let mod_export = cast!(
            Export,
            DataTableExport,
            &mut modded.asset_data.exports[mod_table_idx]
        )
        .ok_or_else(|| anyhow::anyhow!("mod export not a DataTableExport"))?;
        std::mem::take(&mut mod_export.table.data)
    };
    let row_count = mod_rows.len();

    {
        let main_export = cast!(
            Export,
            DataTableExport,
            &mut main.asset_data.exports[main_table_idx]
        )
        .ok_or_else(|| anyhow::anyhow!("template export not a DataTableExport"))?;
        // The engine's row map is keyed by name, so a repeated key means one of
        // the two rows is unreachable. Deliberate when a mod replaces a vanilla
        // item, a silent conflict when two mods pick the same key - either way
        // the user needs it named, since the losing item simply won't exist.
        let existing: std::collections::HashSet<String> = main_export
            .table
            .data
            .iter()
            .map(|row| row.name.get_content())
            .collect();
        for mut row in mod_rows {
            shift_row_refs(&mut row, import_offset);
            rekey_into(&mut row, &name_map);
            let key = row.name.get_content();
            if existing.contains(&key) {
                warn!("row {key:?} already exists in this table; the later definition wins");
            }
            main_export.table.data.push(row);
        }
    }

    debug!("appended {row_count} rows, imports shifted by {import_offset}");
    Ok(())
}

/// Re-point every `FName` in `container` that still belongs to a foreign name
/// map at `name_map`, adding the string there if needed.
///
/// Same logic as `Asset::rebuild_name_map`'s closure, applied to one subtree.
/// The instance number is left alone - it is not part of the map.
fn rekey_into(container: &mut impl FNameContainer, name_map: &SharedResource<NameMap>) {
    // `get_mut` needs a mutable handle; cloning one shares the same cell.
    let mut map = name_map.clone();
    container.traverse_fnames(&mut |name: &mut FName| {
        let content = name.get_content();
        let FName::Backed {
            index,
            number: _,
            ty: _,
            name_map: owner,
        } = name
        else {
            return;
        };
        if owner != name_map {
            *index = map.get_mut().add_name_reference(content, false);
            *owner = name_map.clone();
        }
    });
}

fn find_data_table_export(asset: &MemAsset) -> Option<usize> {
    asset
        .asset_data
        .exports
        .iter()
        .position(|e| matches!(e, Export::DataTableExport(_)))
}

/// Imports are addressed by negative `PackageIndex` (`-(import_index + 1)`), so
/// appending the mod's import list at `offset` moves each of its references down
/// by `offset`. Positive indices address the mod's own export table and 0 means
/// "none", so shifting unconditionally would corrupt every row that references
/// an export.
fn shift_import_ref(index: &mut PackageIndex, offset: i32) {
    if index.is_import() {
        index.index -= offset;
    }
}

fn shift_row_refs(row: &mut StructProperty, offset: i32) {
    for prop in &mut row.value {
        shift_property_refs(prop, offset);
    }
}

/// Shift every `PackageIndex` reachable from `prop`. Only three property kinds
/// hold one (object refs and the two delegate shapes); the rest of the arms are
/// containers that can nest them.
fn shift_property_refs(prop: &mut Property, offset: i32) {
    match prop {
        Property::ObjectProperty(p) => shift_import_ref(&mut p.value, offset),
        Property::DelegateProperty(p) => shift_import_ref(&mut p.value.object, offset),
        Property::MulticastDelegateProperty(p) => {
            for delegate in &mut p.value {
                shift_import_ref(&mut delegate.object, offset);
            }
        }
        Property::MulticastSparseDelegateProperty(p) => {
            for delegate in &mut p.value {
                shift_import_ref(&mut delegate.object, offset);
            }
        }
        Property::MulticastInlineDelegateProperty(p) => {
            for delegate in &mut p.value {
                shift_import_ref(&mut delegate.object, offset);
            }
        }
        Property::StructProperty(p) => {
            for inner in &mut p.value {
                shift_property_refs(inner, offset);
            }
        }
        Property::ArrayProperty(p) => {
            for inner in &mut p.value {
                shift_property_refs(inner, offset);
            }
        }
        Property::SetProperty(p) => {
            for inner in &mut p.value.value {
                shift_property_refs(inner, offset);
            }
            for inner in &mut p.removed_items.value {
                shift_property_refs(inner, offset);
            }
        }
        Property::MapProperty(p) => {
            // Keys are hashed, so the map has to be rebuilt rather than mutated.
            p.value = std::mem::replace(&mut p.value, IndexedMap::new())
                .into_iter()
                .map(|(_, mut key, mut value)| {
                    shift_property_refs(&mut key, offset);
                    shift_property_refs(&mut value, offset);
                    (key, value)
                })
                .collect();
            if let Some(keys) = p.keys_to_remove.as_mut() {
                for key in keys {
                    shift_property_refs(key, offset);
                }
            }
        }
        _ => {}
    }
}
