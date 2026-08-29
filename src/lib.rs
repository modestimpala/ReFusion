//! ReFusion: an in-process port of Fusion's datatable/input merger, shipped as
//! a shimloader overlay `ue4ss.dll` wrapper.
//!
//! The pipeline (`fuse`) and its building blocks are target-independent so they
//! can be run and diffed natively; `win` holds the `DllMain` half.

use std::path::{Path, PathBuf};

use anyhow::Context;
use log::{debug, error, info, warn};

pub mod datatable;
pub mod input;
pub mod oodle;
pub mod pak;
pub mod text;

mod walk;

#[cfg(windows)]
mod lua_forward;
#[cfg(windows)]
mod win;

const OUTPUT_PAK_NAME: &str = "FusionPatch_P.pak";
const PROJECT_NAME: &str = "VotV";
const MAIN_PAK_NAME: &str = "VotV-WindowsNoEditor.pak";
const PAK_MOUNT: &str = "../../../VotV/";
const CONFIG_NAME: &str = "ReFusion.ini";
/// UE4SS mod folders that drive the C# Fusion, as r2modman installs them.
///
/// Each ships a Lua script that runs before we could react to it, so the overlay
/// masks the folders instead; see [`report_rival_tool`].
const RIVAL_MOD_DIRS: &[(&str, &str)] = &[
    (
        "NynrahGhost-Fusion",
        "launches Fusion.exe, which writes a rival merged pak",
    ),
    (
        "forder-FusionFix",
        "searches for Fusion.exe and launches it, and renames other mods' paks \
         to .fusionfix-disabled when it decides they conflict over a datatable",
    ),
];

/// What a run of the pipeline produced that the caller may want to surface.
#[derive(Debug, Default)]
pub struct FuseOutcome {
    /// Mods whose datatables were built against a different version of the
    /// game's row structs, each flagged with whether it was merged regardless.
    pub incompatible_mods: Vec<datatable::IncompatibleMod>,
    /// Mod paks that could not be read at all, so nothing of theirs was merged.
    /// Usually a damaged download, or a compression method this build can't
    /// decode - most mod paks for this game are Oodle-compressed, and without
    /// the Oodle runtime beside the executable every one of them lands here.
    pub unreadable_paks: Vec<String>,
    /// Whether this run's report differs from the one already shown to the user.
    ///
    /// An unreadable pak suppresses the merge cache so the next launch retries,
    /// which means the same complaint would otherwise interrupt startup on every
    /// single launch until the pak is fixed. Telling them once is enough; the log
    /// keeps the detail.
    pub report_is_new: bool,
}

/// `<cfg_dir>/ReFusion.ini`. Absent means defaults.
#[derive(Debug, Default)]
struct Config {
    /// Which mods to merge even though they don't match the game's row structs.
    ///
    /// Nothing, by default: the safe outcome is a missing mod rather than items
    /// with no name or mesh. `merge_incompatible=all` takes everything;
    /// `merge_incompatible=ModA, ModB` (pak file stems, as they appear in the
    /// log) takes just those, which is the useful form - a mod that only loses
    /// a minor flag is worth keeping, one that loses its meshes is not.
    merge_incompatible: datatable::OnIncompatible,
}

impl Config {
    fn load(path: &Path) -> Self {
        let Ok(text) = crate::text::read(path) else {
            return Self::default();
        };
        let body = text.body;
        let mut config = Self::default();
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !key.trim().eq_ignore_ascii_case("merge_incompatible") {
                continue;
            }
            config.merge_incompatible = parse_merge_incompatible(value.trim());
        }
        config
    }
}

fn parse_merge_incompatible(value: &str) -> datatable::OnIncompatible {
    match value.to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" | "none" => datatable::OnIncompatible::SkipAll,
        "1" | "true" | "yes" | "on" | "all" => datatable::OnIncompatible::MergeAll,
        _ => datatable::OnIncompatible::MergeOnly(
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    }
}

/// How many mods a dialog names before it stops listing them.
const MAX_MODS: usize = 6;

/// User-facing text for everything a run left out, empty when there is nothing
/// to report.
///
/// Lives here rather than in `win` so the wording can be checked without
/// Windows (`fusiontool fuse` prints it); `win::warn_left_out` is what puts it
/// in a message box.
pub fn left_out_report(outcome: &FuseOutcome) -> String {
    // Unreadable paks come first: nothing of those mods reached the merge at
    // all, and when the cause is a missing Oodle runtime it takes out every mod
    // at once, which is a different conversation from one stale mod.
    let mut body = unreadable_section(&outcome.unreadable_paks);
    body.push_str(&incompatible_section(&outcome.incompatible_mods));
    if !body.is_empty() {
        body.push_str("Details are in refusion-log.txt.");
    }
    body
}

fn unreadable_section(stems: &[String]) -> String {
    if stems.is_empty() {
        return String::new();
    }
    let mut body =
        String::from("Some mod paks could not be read, so none of their content was merged:\n\n");
    for stem in stems.iter().take(MAX_MODS) {
        body.push_str(&format!("    {stem}\n"));
    }
    if stems.len() > MAX_MODS {
        body.push_str(&format!("    ...and {} more.\n", stems.len() - MAX_MODS));
    }
    body.push_str(
        "\nIf this is every mod you have, the Oodle runtime is missing from the install \
         - reinstall this mod. Otherwise those paks are damaged; re-download them.\n\n",
    );
    body
}

fn incompatible_section(mods: &[datatable::IncompatibleMod]) -> String {
    const MAX_PROBLEMS: usize = 3;

    if mods.is_empty() {
        return String::new();
    }

    // One entry per mod: the same mod usually mismatches in several tables, and
    // the verdict is per mod, so listing tables separately would just repeat
    // itself. `merged_anyway` is constant across a mod's entries because the
    // config allows or skips whole mods.
    let mut by_mod: std::collections::BTreeMap<&str, (bool, Vec<&str>, Vec<&str>)> =
        std::collections::BTreeMap::new();
    for entry in mods {
        let (merged, tables, problems) = by_mod.entry(entry.mod_name.as_str()).or_default();
        *merged = entry.merged_anyway;
        tables.push(entry.table.as_str());
        let room = MAX_PROBLEMS.saturating_sub(problems.len());
        problems.extend(entry.problems.iter().map(String::as_str).take(room));
    }

    let skipped = by_mod.iter().filter(|(_, (merged, ..))| !merged).count();
    let merged = by_mod.len() - skipped;

    let mut body = String::from("Some mods were built for a different version of the game.\n\n");
    for (mod_name, (was_merged, tables, problems)) in by_mod.iter().take(MAX_MODS) {
        let verdict = match was_merged {
            true => "merged anyway",
            false => "SKIPPED",
        };
        body.push_str(&format!(
            "{mod_name}  ({}) - {verdict}\n",
            tables.join(", ")
        ));
        for problem in problems {
            body.push_str(&format!("    {problem}\n"));
        }
        body.push('\n');
    }
    if by_mod.len() > MAX_MODS {
        body.push_str(&format!("...and {} more.\n\n", by_mod.len() - MAX_MODS));
    }

    if skipped > 0 {
        body.push_str(
            "Skipped mods would have loaded items with no name, icon or mesh, so they were \
             left out; every other mod merged normally. Update or disable them.\n\n\
             To keep one anyway, put its name in ReFusion.ini in the config folder:\n\
             merge_incompatible=ModName\n\n",
        );
    }
    if merged > 0 {
        body.push_str(
            "Mods merged anyway are allowed by merge_incompatible: the fields listed above \
             will be blank in game.\n\n",
        );
    }
    body
}

/// Merge every installed mod's datatables and inputs into one pak that the
/// engine mounts over the game's own tables.
///
/// Target-independent: `win::run` calls this from `DllMain`, and
/// `examples/fusiontool.rs` calls it with the same environment offline.
pub fn fuse(my_dir: &Path) -> anyhow::Result<FuseOutcome> {
    let pak_dir = env_path("SHIMLOADER_PAK_DIR")
        .ok_or_else(|| anyhow::anyhow!("SHIMLOADER_PAK_DIR not set; vanilla launch?"))?;
    let cfg_dir = env_path("SHIMLOADER_CFG_DIR")
        .ok_or_else(|| anyhow::anyhow!("SHIMLOADER_CFG_DIR not set"))?;

    report_rival_tool(my_dir);

    let layout = Layout::new(my_dir, &pak_dir)?;
    let scan = scan_paks(&pak_dir, &layout.output_pak)?;
    let config_path = cfg_dir.join(CONFIG_NAME);
    let config = Config::load(&config_path);
    let cfg_files = list_cfgs(&cfg_dir)?;

    // The config decides what gets merged, so editing it has to invalidate the
    // cache exactly like adding a mod pak does.
    let mut cache_inputs = cfg_files.clone();
    if config_path.is_file() {
        cache_inputs.push(config_path.clone());
    }

    info!(
        "{} mod paks, {} cfg files, {} rival merged paks, config={:?}",
        scan.inputs.len(),
        cfg_files.len(),
        scan.rivals.len(),
        config
    );

    // Rival paks are quarantined only once the state without them is known to
    // be right: no mods at all means vanilla is correct, otherwise our own pak
    // has to exist first. On any failure below they stay mounted, because a
    // stale merge beats no merge.
    if scan.inputs.is_empty() && cfg_files.is_empty() {
        info!("nothing to fuse");
        // Our own pak from an earlier launch would keep overriding the game's
        // tables with rows whose mods are gone.
        if layout.output_pak.is_file() {
            match std::fs::remove_file(&layout.output_pak) {
                Ok(()) => info!(
                    "removed stale {:?}; no mods left to merge",
                    layout.output_pak
                ),
                Err(e) => warn!("could not remove stale {:?}: {e}", layout.output_pak),
            }
            let _ = std::fs::remove_file(&layout.last_write);
        }
        quarantine_rivals(&scan.rivals);
        return Ok(FuseOutcome::default());
    }

    if cache_is_valid(&layout.last_write, &scan.inputs, &cache_inputs)
        && layout.output_pak.is_file()
    {
        info!("cache hit, skipping fusion");
        quarantine_rivals(&scan.rivals);
        return Ok(FuseOutcome::default());
    }

    // Mod paks are Oodle-compressed, so repak needs the runtime in place before
    // any of them is opened.
    oodle::ensure_oodle(my_dir);
    layout.reset_work()?;

    let mut stages = Stages::new();
    extract_templates(&layout.game_pak, &layout.staging)?;
    stages.mark("templates");

    let mods = extract_mods(&scan.inputs, &layout.extracted)?;
    stages.mark("mod-paks");

    let mut outcome = FuseOutcome {
        unreadable_paks: mods.unreadable,
        ..Default::default()
    };

    // Both merges are best-effort: an empty staging tree means the game pak
    // isn't what we expect, which is worth a launch with unmerged tables rather
    // than no launch at all.
    let staged_datatables = layout.staged_datatables();
    match staged_datatables.is_dir() {
        true => match datatable::fuse_datatables(
            &staged_datatables,
            &mods.dirs,
            &config.merge_incompatible,
        ) {
            Ok(flagged) => outcome.incompatible_mods = flagged,
            Err(e) => error!("datatable fusion failed: {e:#}"),
        },
        false => warn!("no datatables extracted from game pak; skipping merge"),
    }
    stages.mark("datatables");

    let staged_input = layout.staged_input();
    match staged_input.is_file() {
        true => {
            if let Err(e) = input::fuse_inputs(&staged_input, &cfg_files, &mods.dirs) {
                error!("input fusion failed: {e:#}");
            }
        }
        false => warn!("no DefaultInput.ini extracted from game pak; skipping merge"),
    }
    stages.mark("inputs");

    pak::pack_dir(&layout.staging, &layout.output_pak, PAK_MOUNT)
        .context("failed to pack output")?;
    stages.mark("pack");
    info!("wrote {:?}", layout.output_pak);
    info!("{}", stages.line());

    quarantine_rivals(&scan.rivals);
    update_cache(&layout, &scan, &cache_inputs, &outcome);
    outcome.report_is_new = record_report(&layout.reported, &outcome);

    Ok(outcome)
}

/// Every path the pipeline reads or writes, derived from the two directories
/// shimloader hands us.
///
/// All of our own state sits under `<my_dir>/fusion/`, so deleting that one
/// directory forces a full re-merge and nothing we write can be mistaken for
/// game content.
struct Layout {
    /// The game's own pak: the templates every merge starts from.
    game_pak: PathBuf,
    /// Scratch tree, recreated per merge.
    work: PathBuf,
    /// Templates being merged, and the tree that gets packed.
    staging: PathBuf,
    /// One unpacked directory per mod pak.
    extracted: PathBuf,
    /// Merge cache: what the last complete run merged from.
    last_write: PathBuf,
    /// Last report shown to the user, so an unchanged complaint stays quiet.
    reported: PathBuf,
    /// Our merged pak, in the user's pak directory where the engine mounts it.
    output_pak: PathBuf,
}

impl Layout {
    fn new(my_dir: &Path, pak_dir: &Path) -> anyhow::Result<Self> {
        // my_dir is the spoofed <install>/<Project>/Binaries/Win64, so the
        // project root is two levels up.
        let project_root = my_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("can't derive project root from {my_dir:?}"))?;
        let game_pak = project_root
            .join("Content")
            .join("Paks")
            .join(MAIN_PAK_NAME);
        if !game_pak.is_file() {
            return Err(anyhow::anyhow!("game pak not found at {game_pak:?}"));
        }
        info!("project_root={project_root:?} game_pak={game_pak:?}");

        let state = my_dir.join("fusion");
        let work = state.join("work");
        Ok(Self {
            game_pak,
            staging: work.join("staging"),
            extracted: work.join("extracted"),
            work,
            last_write: state.join("last-write.txt"),
            reported: state.join("reported.txt"),
            output_pak: pak_dir.join(OUTPUT_PAK_NAME),
        })
    }

    /// Start the scratch tree empty: leftovers from an interrupted run would
    /// otherwise be packed into the output.
    fn reset_work(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_dir_all(&self.work);
        std::fs::create_dir_all(&self.staging)?;
        std::fs::create_dir_all(&self.extracted)
    }

    /// Staged paths mirror the game's content tree, because that is what the
    /// output pak's mount point resolves against.
    fn staged_datatables(&self) -> PathBuf {
        self.staging.join("Content").join("main").join("datatables")
    }

    fn staged_input(&self) -> PathBuf {
        self.staging.join("Config").join("DefaultInput.ini")
    }
}

/// Wall-clock split per stage, for the one log line that answers "why was this
/// launch slow". Only the template stage touches the multi-GB game pak, so the
/// split matters more than the total.
struct Stages {
    start: std::time::Instant,
    last: std::time::Instant,
    laps: Vec<(&'static str, u128)>,
}

impl Stages {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            start: now,
            last: now,
            laps: Vec::new(),
        }
    }

    fn mark(&mut self, name: &'static str) {
        let now = std::time::Instant::now();
        self.laps.push((name, (now - self.last).as_millis()));
        self.last = now;
    }

    fn line(&self) -> String {
        let mut out = String::from("timings ms:");
        for (name, ms) in &self.laps {
            out.push_str(&format!(" {name}={ms}"));
        }
        out.push_str(&format!(" total={}", self.start.elapsed().as_millis()));
        out
    }
}

/// Extract the game's own datatables and `DefaultInput.ini`: the templates
/// every merge starts from, and the tree that gets packed back up.
fn extract_templates(game_pak: &Path, staging: &Path) -> anyhow::Result<()> {
    info!("extracting templates from {game_pak:?}");
    pak::extract_filtered(game_pak, staging, |entry| {
        // Game pak entries carry the project prefix ("VotV/Content/..."), which
        // is stripped so the staged tree lines up with the output pak's mount.
        let rel = entry
            .strip_prefix(&format!("{PROJECT_NAME}/"))
            .unwrap_or(entry);
        let wanted = rel.starts_with("Content/main/datatables/")
            || (rel.starts_with("Config/")
                && rel
                    .rsplit('/')
                    .next()
                    .is_some_and(|n| n.eq_ignore_ascii_case("DefaultInput.ini")));
        wanted.then(|| rel.to_string())
    })?;
    Ok(())
}

/// What the mod paks yielded.
struct ModContent {
    /// One unpacked directory per mod pak that had something to merge, paired
    /// with the pak's file stem: the name the user sees in the log and writes in
    /// `ReFusion.ini`.
    dirs: Vec<(String, PathBuf)>,
    /// Stems of paks that could not be read at all. A different failure from a
    /// mod that merges badly: none of its rows ever reached the merge, so the
    /// run must not be remembered as complete.
    unreadable: Vec<String>,
}

/// Unpack the datatables and inputs of every mod pak, one directory per pak.
fn extract_mods(paks: &[PathBuf], into: &Path) -> anyhow::Result<ModContent> {
    let mut content = ModContent {
        dirs: Vec::new(),
        unreadable: Vec::new(),
    };
    for pak in paks {
        let stem = pak
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let out = into.join(&stem);
        std::fs::create_dir_all(&out)?;
        match pak::extract_filtered(pak, &out, |entry| {
            let rel = entry
                .strip_prefix(&format!("{PROJECT_NAME}/"))
                .unwrap_or(entry);
            let lower = rel.to_ascii_lowercase();
            let wanted = lower.contains("/datatables/") || lower.ends_with("defaultinput.ini");
            wanted.then(|| rel.to_string())
        }) {
            // A pak with nothing for us is the normal case (most mods ship only
            // assets), but it is logged because from the outside it looks
            // exactly like a mod whose merge failed.
            Ok(0) => info!("{stem}: no datatables or inputs to merge"),
            Ok(n) => {
                info!("{stem}: extracted {n} file(s) to merge");
                content.dirs.push((stem, out));
            }
            Err(e) => {
                error!("could not read {pak:?}: {e:#}");
                content.unreadable.push(stem);
            }
        }
    }
    Ok(content)
}

/// Record what this run merged from, unless it could not read all of it.
///
/// Caching an incomplete run would make the next launch a no-op and leave those
/// mods missing until the mod set happened to change. Retrying costs one merge.
fn update_cache(layout: &Layout, scan: &PakScan, cache_inputs: &[PathBuf], outcome: &FuseOutcome) {
    if !outcome.unreadable_paks.is_empty() {
        let _ = std::fs::remove_file(&layout.last_write);
        warn!(
            "not caching this run: {} mod pak(s) could not be read, so the next launch retries",
            outcome.unreadable_paks.len()
        );
        return;
    }
    if let Err(e) = write_cache(&layout.last_write, &scan.inputs, cache_inputs) {
        warn!("failed to write cache: {e:#}");
    }
}

/// Is this run's complaint different from the one the user already saw?
///
/// Keeps the last report on disk beside the merge cache. Only reached on a real
/// merge - a cache hit re-validates nothing, so it leaves the record alone.
fn record_report(path: &Path, outcome: &FuseOutcome) -> bool {
    let body = left_out_report(outcome);
    let previous = std::fs::read_to_string(path).unwrap_or_default();
    if body == previous {
        return false;
    }
    match body.is_empty() {
        true => {
            let _ = std::fs::remove_file(path);
        }
        false => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(path, &body) {
                warn!("could not record the report at {path:?}: {e}");
            }
        }
    }
    !body.is_empty()
}

/// Log whether each rival tool's UE4SS mod folder can still reach the game.
///
/// The overlay package ships `Mods/<name>/.shim-removed` for every entry in
/// [`RIVAL_MOD_DIRS`]; shimloader turns each tombstone into a path mask on the
/// logical `Binaries/Win64/Mods/<name>`, which its file detours check before any
/// remap. UE4SS then never sees the folder, so the mod's `main.lua` never loads.
///
/// Masking the folder is the only point of control. Shimming `Fusion.exe` would
/// not work: the scripts recover the *real* profile paths by parsing
/// `shimloader-log.txt` and start the process through `io.popen` or `cmd.exe`,
/// and shimloader hooks no process-creation API, so the child sees the physical
/// file system. FusionFix goes further and searches for `Fusion.exe` by name
/// rather than trusting a fixed folder, so hiding one package would not stop it.
///
/// This can only be checked, not fixed, at runtime: shimloader scans the overlay
/// before this DLL is loaded. Quarantining the merged pak afterwards stays the
/// backstop for a tool that ran out-of-process anyway, but it cannot undo a mod
/// that FusionFix renamed out of the way.
fn report_rival_tool(my_dir: &Path) {
    let Some(mod_dir) = env_path("SHIMLOADER_MOD_DIR") else {
        return;
    };

    for (name, effect) in RIVAL_MOD_DIRS {
        // Real profile path: masks are keyed on the logical path, so this
        // answers "is it installed" regardless of masking.
        if !mod_dir.join(name).is_dir() {
            debug!("{name} is not installed");
            continue;
        }

        // Logical path: goes through the detours, so this answers "can the game
        // still see it".
        match my_dir.join("Mods").join(name).exists() {
            false => info!("{name} is installed but hidden from the game's Mods view"),
            true => warn!(
                "{name} is installed and still visible to the game, where it {effect}. The \
                 overlay package should ship Mods/{name}/.shim-removed to stop that"
            ),
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Suffix appended to a rival merged pak. UE only mounts `*.pak`, so the
/// renamed file is inert, stays visible, and is trivially reversible.
const QUARANTINE_SUFFIX: &str = ".disabled-by-refusion";

/// The paks under the user's pak directory, split by what they are.
struct PakScan {
    /// Mod paks to merge from, sorted.
    inputs: Vec<PathBuf>,
    /// Other already-merged patch paks - in practice the C# Fusion's output,
    /// which r2modman installs alongside any mod that depends on it. Left
    /// mounted it would shadow our merge (or be shadowed by it) depending on
    /// pak precedence, with no way for the user to tell which won.
    rivals: Vec<PathBuf>,
}

/// Classify every `*.pak` under `dir`. Our own `output_pak` is neither an input
/// nor a rival.
///
/// Classification is by content, not file name: a renamed or older fusion
/// output still gets recognised, and a mod that merely ships `_`-prefixed
/// tables under `Content/Mods/...` still counts as an input.
fn scan_paks(dir: &Path, output_pak: &Path) -> anyhow::Result<PakScan> {
    let paks = walk::files(dir, &mut |p| walk::has_extension(p, "pak"))?;

    let mut scan = PakScan {
        inputs: Vec::new(),
        rivals: Vec::new(),
    };
    for pak in paks {
        if pak == output_pak {
            continue;
        }
        let named_like_output = pak
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(OUTPUT_PAK_NAME));
        let kind = match pak::classify(&pak, PROJECT_NAME) {
            Ok(kind) => kind,
            Err(e) => {
                // Unreadable index: extraction will fail and warn too. Treat it
                // as mod content so we never quarantine a pak we can't judge.
                warn!("cannot classify {pak:?}: {e:#}");
                pak::PakKind::ModContent
            }
        };
        match kind == pak::PakKind::MergedOutput || named_like_output {
            true => scan.rivals.push(pak),
            false => scan.inputs.push(pak),
        }
    }
    Ok(scan)
}

/// Rename rival merged paks out of the way so exactly one merged pak is mounted:
/// ours. Best-effort and idempotent - if the C# Fusion runs again and rewrites
/// its pak, the next launch quarantines it again.
fn quarantine_rivals(rivals: &[PathBuf]) {
    for pak in rivals {
        let Some(name) = pak.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let disabled = pak.with_file_name(format!("{name}{QUARANTINE_SUFFIX}"));
        // Windows `rename` refuses an existing destination.
        if disabled.exists() {
            if let Err(e) = std::fs::remove_file(&disabled) {
                warn!("could not replace {disabled:?}: {e}");
                continue;
            }
        }
        match std::fs::rename(pak, &disabled) {
            Ok(()) => warn!(
                "disabled rival merged pak {pak:?} (renamed to {:?}) - two merged paks \
                 leave the engine to pick a winner. Expected when the C# Fusion is \
                 installed alongside this wrapper; re-enable it by removing the suffix.",
                disabled.file_name().unwrap_or_default()
            ),
            Err(e) => error!(
                "could not disable rival merged pak {pak:?}: {e} - the game may load \
                 its tables instead of ours"
            ),
        }
    }
}

/// Only `*DefaultInput.ini`, so our own `ReFusion.ini` in the same
/// directory is never swept into the input merge.
fn list_cfgs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    Ok(walk::files(dir, &mut |p| {
        walk::name_ends_with(p, "DefaultInput.ini")
    })?)
}

/// Has anything we merged from changed since the cached run?
///
/// Keyed on path, mtime and size. Size matters because swapping a mod for
/// another build of itself can reproduce the mtime exactly - r2modman copies
/// files with their timestamps, and restoring from a backup does the same -
/// which would otherwise leave the previous merge in place and make the new mod
/// version look like it did nothing.
fn cache_is_valid(cache_path: &Path, paks: &[PathBuf], cfgs: &[PathBuf]) -> bool {
    let Ok(body) = std::fs::read_to_string(cache_path) else {
        return false;
    };
    let lines: Vec<&str> = body.lines().collect();
    let inputs: Vec<&PathBuf> = paks.iter().chain(cfgs.iter()).collect();
    if lines.len() != inputs.len() {
        return false;
    }
    for (line, path) in lines.iter().zip(inputs.iter()) {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if *line != cache_line(path, &meta) {
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
        out.push_str(&cache_line(path, &meta));
        out.push('\n');
    }
    std::fs::write(cache_path, out)?;
    Ok(())
}

fn cache_line(path: &Path, meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}\t{mtime}\t{}", path.to_string_lossy(), meta.len())
}
