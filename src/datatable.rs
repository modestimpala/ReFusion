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
    properties::{
        array_property::ArrayProperty, struct_property::StructProperty, Property, PropertyDataTrait,
    },
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

/// Mod-side datatable assets, split by whether they can merge at all.
struct ModTables {
    /// Assets named `_Foo.uasset`, keyed by the `Foo` they merge into.
    sources: ModFiles,
    /// Assets in a datatables folder without the leading underscore, keyed by
    /// their stem. Most are a mod's own private tables; one named after a game
    /// table replaces that asset wholesale instead of merging into it.
    plain: ModFiles,
}

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
    let mod_tables = index_mod_files(extracted_dirs);
    warn_unusable_tables(&templates, &mod_tables);

    let shapes = GameShapes::learn(&templates);
    let check = shape_check(&mod_tables.sources, &shapes, on_incompatible);

    for template in &templates {
        let Some(stem) = template.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(mod_paths) = mod_tables.sources.get(stem) else {
            debug!("no mod overrides for template {stem}");
            continue;
        };
        let usable: Vec<(String, PathBuf)> = mod_paths
            .iter()
            .filter(|(mod_name, _)| !check.rejected.contains(mod_name))
            .cloned()
            .collect();
        if usable.is_empty() {
            continue;
        }
        if let Err(e) = merge_one(template, &usable, &shapes) {
            warn!("merge failed for {stem}: {e:#}");
        }
    }

    info!("datatable merge complete");
    Ok(check.incompatible)
}

/// Group every mod's datatable assets by the game table they name.
fn index_mod_files(extracted_dirs: &[(String, PathBuf)]) -> ModTables {
    let mut out = ModTables {
        sources: ModFiles::new(),
        plain: ModFiles::new(),
    };
    for (mod_name, dir) in extracted_dirs {
        for path in find_datatable_uassets(dir) {
            let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let (bucket, stem) = match file_name.strip_prefix('_') {
                Some(stem) => (&mut out.sources, stem),
                None => (&mut out.plain, file_name),
            };
            bucket
                .entry(stem.to_string())
                .or_default()
                .push((mod_name.clone(), path));
        }
    }
    out
}

/// Warn about mod tables that cannot do what their name suggests.
///
/// Both cases look from the outside exactly like a merge that failed, and
/// neither is visible in the game:
///
/// - `_Foo.uasset` where the game ships no `Foo`: a typo, or a table renamed by
///   a game update.
/// - `Foo.uasset` where the game *does* ship `Foo`: without the underscore it
///   replaces the whole table rather than merging into it, and our merged pak
///   will shadow it. A mod's own private tables land here too, which is why only
///   names the game also uses are worth mentioning.
fn warn_unusable_tables(templates: &[PathBuf], mod_tables: &ModTables) {
    let template_stems: std::collections::HashSet<&str> = templates
        .iter()
        .filter_map(|t| t.file_stem().and_then(|s| s.to_str()))
        .collect();
    for (stem, sources) in &mod_tables.sources {
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
    for (stem, sources) in &mod_tables.plain {
        if !template_stems.contains(stem.as_str()) {
            continue;
        }
        for (mod_name, path) in sources {
            warn!(
                "{mod_name}: {:?} replaces the whole {stem} table instead of merging into it \
                 (a merge source is named _{stem}.uasset); our merged table will shadow it",
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
    mod_files: &ModFiles,
    shapes: &GameShapes,
    on_incompatible: &OnIncompatible,
) -> ShapeCheck {
    let started = std::time::Instant::now();
    let mut incompatible: Vec<IncompatibleMod> = Vec::new();
    let mut rejected: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (stem, mod_paths) in mod_files {
        let Some(template_shape) = shapes.tables.get(stem) else {
            continue;
        };

        for (mod_name, path) in mod_paths {
            let issues = match mod_issues(path, template_shape, shapes) {
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
            if !issues.drifted.is_empty() {
                // The merge remaps these, so the mod's values arrive; the count
                // it reports is the authority on what was actually rewritten,
                // since an empty struct array is left as authored.
                info!(
                    "mod {mod_name}: {stem} member tag(s) named differently in this version: {}",
                    issues.drifted.join("; ")
                );
            }
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

/// One table's row struct, indexed both exactly and by member base name.
struct TemplateShape {
    /// `column path -> type signatures`, as serialized.
    exact: Shape,
    /// `base column path -> exact paths`, i.e. member names with the ordinal and
    /// GUID suffix stripped. Lets a member whose suffix merely drifted be told
    /// apart from one the game no longer has at all.
    by_base: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Every row struct the game ships, plus how it spells each member.
pub struct GameShapes {
    /// Table stem -> that table's shape.
    tables: std::collections::HashMap<String, TemplateShape>,
    /// `(base column path, type signature) -> the game's spelling`, pooled
    /// across every table, or `None` where tables disagree.
    ///
    /// Needed because these structs are shared and only partly populated: no
    /// `list_equipment` row fills in the nested save-data struct that other
    /// tables do, so a mod's tags for it cannot be checked against
    /// `list_equipment` alone. Pooling is safe only where every table that has
    /// the member spells it the same way, hence the `None`.
    pooled: std::collections::HashMap<(String, String), Option<String>>,
}

impl GameShapes {
    /// Learn every template's shape once, up front.
    ///
    /// From *all* rows, not just the first: optional containers are empty in
    /// most rows, and an empty container serializes no tags for its contents.
    pub fn learn(templates: &[PathBuf]) -> Self {
        let mut shapes = GameShapes {
            tables: std::collections::HashMap::new(),
            pooled: std::collections::HashMap::new(),
        };
        for template in templates {
            let Some(stem) = template.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match table_shape(template) {
                Ok(Some(shape)) => {
                    shapes.pool(&shape);
                    shapes.tables.insert(stem.to_string(), shape);
                }
                Ok(None) => debug!("template {stem} has no rows; nothing to learn from it"),
                Err(e) => warn!("cannot read template {stem}: {e:#}"),
            }
        }
        shapes
    }

    fn pool(&mut self, shape: &TemplateShape) {
        for (column, signatures) in &shape.exact {
            let base = base_path(column);
            let name = last_segment(column).to_string();
            for signature in signatures {
                match self.pooled.entry((base.clone(), signature.clone())) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(Some(name.clone()));
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        if slot.get().as_deref() != Some(name.as_str()) {
                            slot.insert(None);
                        }
                    }
                }
            }
        }
    }

    /// The game's own spelling of a member the mod spells differently, when that
    /// is unambiguous.
    ///
    /// The member's own table decides whenever it knows the path at all; the
    /// pooled index only answers for paths that table never populates. Both
    /// require the base name and the serialized type to agree and exactly one
    /// candidate to survive: with two same-named members of the same type there
    /// is no way to tell which one the mod meant, and a changed type means the
    /// value could not be delivered anyway.
    fn drifted_name<'a>(
        &'a self,
        table: &'a TemplateShape,
        base: &str,
        signature: &str,
    ) -> Option<&'a str> {
        match table.by_base.get(base) {
            Some(candidates) => {
                let mut matching = candidates.iter().filter(|path| {
                    table
                        .exact
                        .get(*path)
                        .is_some_and(|sigs| sigs.contains(signature))
                });
                let only = matching.next()?;
                match matching.next() {
                    None => Some(last_segment(only)),
                    Some(_) => None,
                }
            }
            None => self
                .pooled
                .get(&(base.to_string(), signature.to_string()))?
                .as_deref(),
        }
    }
}

/// Shape of a template on disk, or `None` when the table has no rows: there is
/// no struct to learn, so nothing can be checked or remapped against it.
fn table_shape(template: &Path) -> Result<Option<TemplateShape>> {
    let asset = open_asset(template, &template.with_extension("uexp"))?;
    let idx = find_data_table_export(&asset)
        .ok_or_else(|| anyhow::anyhow!("template has no DataTableExport"))?;
    let Some(table) = cast!(Export, DataTableExport, &asset.asset_data.exports[idx]) else {
        return Err(anyhow::anyhow!("template export not a DataTableExport"));
    };
    if table.table.data.is_empty() {
        return Ok(None);
    }

    let mut exact = Shape::new();
    for row in &table.table.data {
        collect_shape(&row.value, "", &mut exact);
    }
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
    /// Values the game will never receive: the type changed, or the name is
    /// ambiguous, so the mod's tag cannot be matched to a member and the field
    /// takes its default. This is what turns a stale mod into nameless,
    /// meshless items.
    lost: Vec<String>,
    /// Members whose suffix drifted but whose name and type still identify one
    /// member of the game's struct. The merge renames these, so the value
    /// arrives; they are logged only to explain why a mod needed rewriting.
    drifted: Vec<String>,
    /// Tags for members this version of the game doesn't have at all. The
    /// engine ignores them and the rest of the row still loads, so these are
    /// reported but not disqualifying.
    ignored: Vec<String>,
}

/// Compare `path`'s rows against the game's row struct.
///
/// Blueprint struct members serialize as `<name>_<ordinal>_<GUID>`, and the
/// engine matches a row's tags against that whole mangled name. Both the ordinal
/// and the GUID are assigned by the editor, so they differ between the game's
/// struct and any struct an author rebuilt or hand-wrote, even when the member
/// is meant to be the same one. Three outcomes follow, and they need different
/// verdicts:
///
/// - Name and type still identify exactly one of the game's members: the tag is
///   stale but the intent is unambiguous, so the merge rewrites it.
/// - The type changed, or the name matches several members: the value cannot be
///   delivered and the field silently defaults.
/// - The game has no such member at all: the field was removed, the engine
///   ignores the tag, and everything else in the row still applies.
fn mod_issues(path: &Path, table_shape: &TemplateShape, shapes: &GameShapes) -> Result<ModIssues> {
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
                let base = base_path(&column);
                let (bucket, problem) = match table_shape.exact.get(&column) {
                    Some(expected) if expected.contains(&signature) => continue,
                    Some(expected) => (
                        &mut issues.lost,
                        format!(
                            "`{column}` is {signature} in the mod but {} in the game",
                            expected.iter().cloned().collect::<Vec<_>>().join("/")
                        ),
                    ),
                    None => match shapes.drifted_name(table_shape, &base, &signature) {
                        Some(current) => (
                            &mut issues.drifted,
                            format!("`{column}` is `{current}` in this version"),
                        ),
                        None => match table_shape.by_base.get(&base) {
                            Some(current) => (
                                &mut issues.lost,
                                format!(
                                    "`{column}` cannot be matched to `{}`, so the mod's value \
                                     is dropped",
                                    current.iter().cloned().collect::<Vec<_>>().join("/")
                                ),
                            ),
                            None => (
                                &mut issues.ignored,
                                format!("`{column}` no longer exists in this version"),
                            ),
                        },
                    },
                };
                if seen.insert(problem.clone()) {
                    bucket.push(problem);
                }
            }
        }
    }

    for bucket in [&mut issues.lost, &mut issues.drifted, &mut issues.ignored] {
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

/// Final segment of a dotted column path: the member's own serialized name.
fn last_segment(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
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

/// Every `*.uasset` in a `datatables` directory of an unpacked mod pak, merge
/// source or not. `index_mod_files` sorts out which is which by name.
fn find_datatable_uassets(dir: &Path) -> Vec<PathBuf> {
    crate::walk::files(dir, &mut |path| {
        crate::walk::has_extension(path, "uasset")
            && path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("datatables"))
    })
    .unwrap_or_default()
}

/// Merge every source for one table into the staged template, in place.
fn merge_one(template: &Path, sources: &[(String, PathBuf)], shapes: &GameShapes) -> Result<()> {
    let template_uexp = template.with_extension("uexp");
    let stem = template
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_label(template));

    let start = std::time::Instant::now();
    let mut main = open_asset(template, &template_uexp)?;
    let parsed = start.elapsed();

    let mut renamed = 0usize;
    // Row key -> the mod that last defined it, so a key arriving twice can be
    // told apart: replacing one of the game's rows is what a translation or a
    // rebalance mod is for, while two mods claiming one key means one of them
    // silently loses its item.
    let mut author: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut replaced: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut collisions: Vec<String> = Vec::new();

    for (mod_name, mod_uasset) in sources {
        let mod_uexp = mod_uasset.with_extension("uexp");
        let mut modded = open_asset(mod_uasset, &mod_uexp)?;
        let appended = merge_into(&mut main, &mut modded, shapes, shapes.tables.get(&stem))
            .with_context(|| format!("merging {mod_uasset:?}"))?;
        renamed += appended.renamed;
        for key in appended.overridden {
            match author.get(&key) {
                Some(previous) => collisions.push(format!("{key:?} ({previous} then {mod_name})")),
                None => *replaced.entry(mod_name).or_default() += 1,
            }
        }
        for key in appended.keys {
            author.insert(key, mod_name.clone());
        }
    }
    let merged = start.elapsed();

    write_asset(&main, template, &template_uexp)?;
    let written = start.elapsed();

    for (mod_name, count) in &replaced {
        info!("{mod_name} redefines {count} of the game's own {stem} row(s)");
    }
    if !collisions.is_empty() {
        const MAX_SHOWN: usize = 4;
        warn!(
            "{} {stem} row(s) claimed by more than one mod; the later definition wins: {}{}",
            collisions.len(),
            collisions
                .iter()
                .take(MAX_SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            match collisions.len() > MAX_SHOWN {
                true => format!(" and {} more", collisions.len() - MAX_SHOWN),
                false => String::new(),
            }
        );
    }
    info!(
        "wrote merged {:?} ({} mod sources{}) parse={}ms merge={}ms write={}ms",
        template,
        sources.len(),
        match renamed {
            0 => String::new(),
            n => format!(", {n} drifted tag(s) rewritten"),
        },
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

/// Append `modded`'s rows into `main`'s DataTable, returning how many drifted
/// member tags had to be rewritten.
///
/// Three things have to be translated on the way across, and all three fail
/// silently if missed:
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
/// - **Member tags.** A mod built against a rebuilt or hand-written struct
///   carries a different `_<ordinal>_<GUID>` suffix for the same member, and the
///   engine matches tags by that whole name. `remap_drift` rewrites those to the
///   game's spelling; see [`GameShapes::drifted_name`] for when that is provable.
///
/// `Asset::rebuild_name_map` does the name half in one call, but it walks the
/// whole destination asset: on `list_props` that is 2477 rows and ~10k names
/// re-examined per merged mod, measured at 6.9s of a 7.0s cold merge. Re-keying
/// only what moved is proportional to the mod instead of to the game's table.
fn merge_into(
    main: &mut MemAsset,
    modded: &mut MemAsset,
    shapes: &GameShapes,
    table_shape: Option<&TemplateShape>,
) -> Result<Appended> {
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
        // the two rows is unreachable. Which one is left for the caller to
        // judge: it knows whether the row already there came from the game or
        // from another mod.
        let existing: std::collections::HashSet<String> = main_export
            .table
            .data
            .iter()
            .map(|row| row.name.get_content())
            .collect();
        let mut appended = Appended {
            keys: Vec::with_capacity(row_count),
            overridden: Vec::new(),
            renamed: 0,
        };
        for mut row in mod_rows {
            shift_row_refs(&mut row, import_offset);
            if let Some(table_shape) = table_shape {
                appended.renamed += remap_drift(&mut row.value, "", shapes, table_shape, &name_map);
            }
            rekey_into(&mut row, &name_map);
            let key = row.name.get_content();
            if existing.contains(&key) {
                appended.overridden.push(key.clone());
            }
            appended.keys.push(key);
            main_export.table.data.push(row);
        }

        debug!("appended {row_count} rows, imports shifted by {import_offset}");
        Ok(appended)
    }
}

/// What one mod source contributed to a table.
struct Appended {
    /// Row keys the source defined, in order.
    keys: Vec<String>,
    /// Of those, the keys that were already in the table.
    overridden: Vec<String>,
    /// Member tags rewritten to the game's spelling.
    renamed: usize,
}

/// Rewrite every member tag in `props` whose suffix drifted to the name the
/// game's struct uses, returning how many were rewritten.
///
/// Mirrors `collect_shape`'s walk so the paths line up with the shape that was
/// measured, and keys the lookup on *base* paths, which are suffix-free and so
/// unaffected by drift in an enclosing struct's own name.
///
/// Container elements share their container's tag, so they are renamed with it:
/// leaving them behind would write a tag the engine cannot match either.
fn remap_drift(
    props: &mut [Property],
    base_prefix: &str,
    shapes: &GameShapes,
    table_shape: &TemplateShape,
    name_map: &SharedResource<NameMap>,
) -> usize {
    let mut renamed = 0;
    for prop in props.iter_mut() {
        let name = prop.get_name().get_content();
        let base = match base_prefix.is_empty() {
            true => member_base(&name).to_string(),
            false => format!("{base_prefix}.{}", member_base(&name)),
        };

        // `drifted_name` returning the mod's own spelling means the tag already
        // matches, so the comparison is the whole test: no separate exact-path
        // lookup is needed, and base paths stay valid however the enclosing
        // struct's own suffix drifted.
        // An empty struct array is left as authored: see `tag_is_rewritable`.
        if tag_is_rewritable(prop) {
            if let Some(current) = shapes.drifted_name(table_shape, &base, &type_signature(prop)) {
                if current != name {
                    let fixed = name_map.clone().get_mut().add_fname(current);
                    rename_with_elements(prop, &name, &fixed);
                    renamed += 1;
                }
            }
        }

        renamed += match prop {
            Property::StructProperty(p) => {
                remap_drift(&mut p.value, &base, shapes, table_shape, name_map)
            }
            Property::ArrayProperty(p) => {
                remap_drift(&mut p.value, &base, shapes, table_shape, name_map)
            }
            Property::SetProperty(p) => {
                remap_drift(&mut p.value.value, &base, shapes, table_shape, name_map)
            }
            _ => 0,
        };
    }
    renamed
}

/// Set `prop`'s name everywhere the serializer will look for it.
///
/// A container's elements carry the container's tag, and `unreal_asset` writes a
/// struct array's inner tag from its first element, then reads that tag back
/// *over* the property's name. Renaming the elements keeps the two consistent.
fn rename_with_elements(prop: &mut Property, old: &str, fixed: &FName) {
    *prop.get_name_mut() = fixed.clone();
    match prop {
        Property::ArrayProperty(p) => rename_array(p, old, fixed),
        Property::SetProperty(p) => {
            rename_array(&mut p.value, old, fixed);
            rename_array(&mut p.removed_items, old, fixed);
        }
        _ => {}
    }
}

fn rename_array(array: &mut ArrayProperty, old: &str, fixed: &FName) {
    if array.name.get_content() == old {
        array.name = fixed.clone();
    }
    for element in &mut array.value {
        if element.get_name().get_content() == old {
            *element.get_name_mut() = fixed.clone();
        }
    }
}

/// Can this property's tag be rewritten consistently?
///
/// An empty struct array is the one case where it cannot: `unreal_asset` writes
/// its inner tag from a private `dummy_property` there is no way to reach, and
/// reads that tag back over the property's name, so a rename would produce a
/// file that contradicts itself on the next read. Nothing is lost by leaving it:
/// the array carries no elements, and a tag the engine cannot match leaves the
/// member at its default, which for an array is empty either way.
fn tag_is_rewritable(prop: &Property) -> bool {
    match prop {
        Property::ArrayProperty(p) => {
            !p.value.is_empty()
                || p.array_type
                    .as_ref()
                    .is_none_or(|t| t.get_content() != "StructProperty")
        }
        _ => true,
    }
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
