use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use unreal_asset::{
    cast,
    engine_version::EngineVersion,
    exports::Export,
    properties::Property,
    types::{fname::FName, PackageIndex},
    Asset,
};

const ENGINE: EngineVersion = EngineVersion::VER_UE4_27;

/// Port of C# `FuseDatatables`.
///
/// For each template `.uasset` in `staged_dir`, look for a matching
/// `.uasset` inside any of the extracted mod directories under
/// `<extract>/VotV/Content/Mods/<mod>/_Content/main/datatables/`. Mod files
/// are named `_Foo.uasset` while the template is `Foo.uasset` - the leading
/// underscore is the convention the C# loader strips when matching.
pub fn fuse_datatables(
    staged_dir: &Path,
    extracted_dirs: &[(String, PathBuf)],
) -> Result<()> {
    if !staged_dir.is_dir() {
        warn!("no staged datatable dir at {:?}", staged_dir);
        return Ok(());
    }

    let templates: Vec<PathBuf> = std::fs::read_dir(staged_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("uasset")))
        .collect();

    let mut mod_files_by_stem: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();

    for (_mod_name, dir) in extracted_dirs {
        for path in find_datatable_uassets(dir) {
            let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let stem = file_name.strip_prefix('_').unwrap_or(file_name).to_string();
            mod_files_by_stem.entry(stem).or_default().push(path);
        }
    }

    for template in templates {
        let Some(stem) = template.file_stem().and_then(|s| s.to_str()) else { continue };
        let Some(mod_paths) = mod_files_by_stem.get(stem) else {
            debug!("no mod overrides for template {stem}");
            continue;
        };

        if let Err(e) = merge_one(&template, mod_paths) {
            warn!("merge failed for {stem}: {e:#}");
        }
    }

    info!("datatable merge complete");
    Ok(())
}

/// Walk `dir` recursively, collecting any `*.uasset` whose file name starts
/// with `_` (the mod-side datatable convention) and whose containing
/// directory ends in `datatables` (case-insensitive).
fn find_datatable_uassets(dir: &Path) -> Vec<PathBuf> {
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
            continue;
        }
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("uasset")) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with('_') {
            continue;
        }
        let parent_is_datatables = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("datatables"))
            .unwrap_or(false);
        if parent_is_datatables {
            out.push(path);
        }
    }
}

fn merge_one(template: &Path, mod_uassets: &[PathBuf]) -> Result<()> {
    let template_uexp = template.with_extension("uexp");

    let mut main = open_asset(template, &template_uexp)?;

    for mod_uasset in mod_uassets {
        let mod_uexp = mod_uasset.with_extension("uexp");
        let mut modded = open_asset(mod_uasset, &mod_uexp)?;
        merge_into(&mut main, &mut modded)
            .with_context(|| format!("merging {mod_uasset:?}"))?;
    }

    write_asset(&main, template, &template_uexp)?;
    info!("wrote merged {:?} ({} mod sources)", template, mod_uassets.len());
    Ok(())
}

fn open_asset(uasset: &Path, uexp: &Path) -> Result<Asset<File>> {
    let main_file = File::open(uasset).with_context(|| format!("open {uasset:?}"))?;
    let bulk = if uexp.is_file() {
        Some(File::open(uexp).with_context(|| format!("open {uexp:?}"))?)
    } else {
        None
    };
    Asset::new(main_file, bulk, ENGINE).with_context(|| format!("parse {uasset:?}"))
}

fn write_asset(asset: &Asset<File>, uasset: &Path, uexp: &Path) -> Result<()> {
    // write_data needs Read+Seek+Write, so the output file must be opened r/w.
    let mut main_out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(uasset)
        .with_context(|| format!("create {uasset:?}"))?;
    let mut bulk_out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(uexp)
        .with_context(|| format!("create {uexp:?}"))?;
    asset
        .write_data(&mut main_out, Some(&mut bulk_out))
        .with_context(|| format!("write {uasset:?}"))?;
    Ok(())
}

/// Append `modded`'s rows into `main`'s DataTable. Mirrors the C# logic:
/// re-key all of mod's name and import references through main's tables, then
/// extend main's row vector with the mod's rows.
fn merge_into(main: &mut Asset<File>, modded: &mut Asset<File>) -> Result<()> {
    main.package_guid = modded.package_guid;

    // Pull every name in mod's name map into main's. Snapshot first so we
    // can drop the borrow before mutating main.
    let mod_names: Vec<String> = modded
        .get_name_map()
        .get_ref()
        .get_name_map_index_list()
        .to_vec();
    for name in mod_names {
        main.add_name_reference(name, false);
    }

    let import_offset = main.imports.len() as i32;

    // Re-key mod's imports through main's name table, then append. Internal
    // references (`outer_index` pointing back into mod's import space, raw
    // negative) get shifted by `-import_offset` so they land on the new
    // positions in main's now-extended import list.
    let mod_imports = std::mem::take(&mut modded.imports);
    for mut import in mod_imports {
        let class_package = import.class_package.get_content();
        let class_name = import.class_name.get_content();
        let object_name = import.object_name.get_content();
        import.class_package = main.add_fname(&class_package);
        import.class_name = main.add_fname(&class_name);
        import.object_name = main.add_fname(&object_name);
        if import.outer_index.index != 0 {
            import.outer_index.index -= import_offset;
        }
        main.add_import(import);
    }

    let main_table_idx = find_data_table_export(main)
        .ok_or_else(|| anyhow::anyhow!("template has no DataTableExport"))?;
    let mod_table_idx = find_data_table_export(modded)
        .ok_or_else(|| anyhow::anyhow!("mod has no DataTableExport"))?;

    // Pull the mod's rows out for re-keying.
    let mod_rows = {
        let mod_export = cast!(Export, DataTableExport, &mut modded.asset_data.exports[mod_table_idx])
            .ok_or_else(|| anyhow::anyhow!("mod export not a DataTableExport"))?;
        std::mem::take(&mut mod_export.table.data)
    };

    // Snapshot the template's first-row column shape and struct type, so
    // short mod rows can be padded and so we can release the immutable
    // borrow before mutating `main` again.
    let (template_row_shape, template_struct_type): (Vec<Property>, Option<String>) = {
        let main_export = cast!(Export, DataTableExport, &main.asset_data.exports[main_table_idx])
            .ok_or_else(|| anyhow::anyhow!("template export not a DataTableExport"))?;
        let first = main_export.table.data.first();
        let shape = first.map(|row| row.value.clone()).unwrap_or_default();
        let st = first.and_then(|row| row.struct_type.as_ref().map(FName::get_content));
        (shape, st)
    };

    for mut row in mod_rows {
        let row_name_str = row.name.get_content();
        row.name = main.add_fname(&row_name_str);

        let n = std::cmp::min(template_row_shape.len(), row.value.len());
        for k in 0..n {
            re_key_property(&mut row.value[k], &template_row_shape[k], main, import_offset);
        }
        for tail in template_row_shape.iter().skip(n) {
            row.value.push(tail.clone());
        }

        if let Some(ref s) = template_struct_type {
            row.struct_type = Some(main.add_fname(s));
        }

        if let Some(main_export) = cast!(Export, DataTableExport, &mut main.asset_data.exports[main_table_idx]) {
            main_export.table.data.push(row);
        }
    }

    Ok(())
}

fn find_data_table_export(asset: &Asset<File>) -> Option<usize> {
    asset
        .asset_data
        .exports
        .iter()
        .position(|e| matches!(e, Export::DataTableExport(_)))
}

/// Re-key a single property's index references through `main`'s tables.
/// Mirrors C# `propertyDeviser`: walks Object/Name/Byte/Struct/Array property
/// kinds and adjusts their FName indices and (for ObjectProperty)
/// PackageIndex references. The C# version subtracts `import_offset`
/// unconditionally from `ObjectProperty.value.index` regardless of sign; the
/// same is preserved here.
fn re_key_property(
    prop: &mut Property,
    main_template: &Property,
    main: &mut Asset<File>,
    import_offset: i32,
) {
    let canonical_name = property_name(main_template);
    if let Some(name) = canonical_name {
        let new_fname = main.add_fname(&name);
        set_property_name(prop, new_fname);
    }

    if let Some(p) = cast!(Property, ObjectProperty, prop) {
        p.value = PackageIndex::new(p.value.index - import_offset);
        return;
    }
    if let Some(p) = cast!(Property, NameProperty, prop) {
        let v = p.value.get_content();
        p.value = main.add_fname(&v);
        return;
    }
    if let Some(p) = cast!(Property, ByteProperty, prop) {
        if let Some(et) = &p.enum_type {
            let s = et.get_content();
            p.enum_type = Some(main.add_fname(&s));
        }
        if let unreal_asset::properties::int_property::BytePropertyValue::FName(ref mut fname) = p.value {
            let s = fname.get_content();
            *fname = main.add_fname(&s);
        }
        return;
    }
    if let Some(p) = cast!(Property, StructProperty, prop) {
        if let Some(st) = &p.struct_type {
            let s = st.get_content();
            p.struct_type = Some(main.add_fname(&s));
        }
        let template_inner = match cast!(Property, StructProperty, main_template) {
            Some(t) => t.value.clone(),
            None => return,
        };
        let n = std::cmp::min(p.value.len(), template_inner.len());
        for i in 0..n {
            re_key_property(&mut p.value[i], &template_inner[i], main, import_offset);
        }
        return;
    }
    if let Some(p) = cast!(Property, ArrayProperty, prop) {
        if let Some(at) = &p.array_type {
            let s = at.get_content();
            p.array_type = Some(main.add_fname(&s));
        }
        let template_first = cast!(Property, ArrayProperty, main_template)
            .and_then(|t| t.value.first().cloned());
        if let Some(template_first) = template_first {
            for v in &mut p.value {
                re_key_property(v, &template_first, main, import_offset);
            }
        }
    }
}

fn property_name(prop: &Property) -> Option<String> {
    use unreal_asset::properties::PropertyDataTrait;
    Some(prop.get_name().get_content())
}

fn set_property_name(prop: &mut Property, new_name: FName) {
    if let Some(p) = cast!(Property, ObjectProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, NameProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, ByteProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, StructProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, ArrayProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, IntProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, FloatProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, BoolProperty, prop) { p.name = new_name; return; }
    if let Some(p) = cast!(Property, StrProperty, prop) { p.name = new_name; }
}
