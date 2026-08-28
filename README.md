# fusion-wrapper

In-process port of [NynrahGhost's Fusion](https://github.com/NynrahGhost/Fusion) datatable/input merger, packaged as a
shimloader overlay wrapper. Replaces the standalone two-step C# launcher.

## How it works

- Built as `ue4ss.dll`. Ships in a shimloader overlay package so shimloader's
  `LoadLibraryW("Win64/ue4ss.dll")` resolves to this wrapper instead of the
  real ue4ss.
- Wrapper's `DllMain` reads the `SHIMLOADER_*` env vars to discover where the user's mods, paks, and configs
  live.
- Runs the fusion pipeline: extract the shipped datatable templates and
  `DefaultInput.ini` from the game pak, extract mod paks, merge the mod
  datatables into the templates, merge `DefaultInput.ini` files, write
  `FusionPatch_P.pak` into the user's pak directory.
- Resolves every pak entry against its pak's mount point before filtering.
  UnrealPak records the folder the author packed, so the same file is
  `_Content/main/datatables/_list_props.uasset` in one mod pak and bare
  `_list_props.uasset` in another; matching on entry paths alone silently drops
  the deeply-mounted ones.
- Places the bundled Oodle runtime next to the game executable first, because
  mod paks are Oodle-compressed. See [Bundled Oodle runtime](#bundled-oodle-runtime).
- Copies `<profile>/ue4ss.dll` over `<my_dir>/ue4ss_real.dll` on every
  launch so the staged DLL never goes stale relative to the user's UE4SS
  install. The profile root is derived from `SHIMLOADER_OVERLAY_DIR`
  (whose value is `<profile>/shimloader/overlay`); we can't use the
  wrapper's own module path because shimloader spoofs that to look like a
  Win64 binary path.
- Then `LoadLibraryW("ue4ss_real.dll")` to hand off to the real UE4SS.

## Install layout

The wrapper is shipped as an overlay package. End users never assemble this
by hand - a Thunderstore/r2modman package would lay it out:

```
<profile>/
├── ue4ss.dll                     (the real UE4SS, installed normally)
├── shimloader/cfg/
│   └── FusionWrapper.ini         (optional; see "Mods built for another game version")
└── shimloader/overlay/FusionWrapper/
    ├── ue4ss.dll                 (this crate's build output, renamed)
    ├── Mods/NynrahGhost-Fusion/
    │   └── .shim-removed         (masks the C# Fusion's Lua mod; see below)
    └── fusion/
        └── oo2core_9_win64.dll   (Oodle runtime, see below)
```

`register_overlay_dir` mirrors each overlay package onto
`<install>/<Project>/Binaries/Win64`, registering one mapping **per file** -
directories are only walked, never redirected wholesale. So those two files are
the whole package, and everything the wrapper writes at a `my_dir`-relative path
that the overlay does not claim lands in the real game directory:

```
<install>/<Project>/Binaries/Win64/
├── ue4ss_real.dll                (re-copied each launch from <profile>/ue4ss.dll)
├── oo2core_9_win64.dll           (placed from the overlay copy, where repak looks)
├── fusion-wrapper-log.txt
└── fusion/
    ├── work/                     (extracted paks + merge staging)
    └── last-write.txt            (input mtimes, skips redundant merges)
```

And the launch params gain `--overlay-dir "<profile>/shimloader/overlay"`.

## Coexisting with the C# Fusion

Installing both is the normal case, not user error: item mods list
`NynrahGhost-Fusion` as a Thunderstore dependency, so r2modman installs the C#
tool for anyone who installs one of them. Its `Fusion.exe` writes
`<pak_dir>/NynrahGhost-Fusion/FusionPatch_P.pak`, and shimloader maps the whole
pak directory onto `<game>/Content/Paks/LogicMods`, so both patch paks mount
with the same entry paths. Which one the engine serves is a pak-precedence
detail, and the loser is discarded silently - so a stale C# merge can quietly
replace a correct one (or vice versa) with no visible symptom beyond missing or
outdated rows.

Two layers stop that, upstream and downstream.

**Upstream - keep the C# tool from running at all.** It is a UE4SS Lua mod:
UE4SS loads `Mods/NynrahGhost-Fusion/scripts/main.lua`, which writes a `run.bat`
and `io.popen`s it to start `Fusion.exe`. The overlay package therefore ships an
empty marker file:

```
shimloader/overlay/FusionWrapper/Mods/NynrahGhost-Fusion/.shim-removed
```

`register_overlay_subtree` turns a `.shim-removed` tombstone into a path mask on
the logical `Binaries/Win64/Mods/NynrahGhost-Fusion`, and every file detour
(`CreateFileW`, `NtCreateFile`, `FindFirstFileW`/`FindNextFileW`,
`GetFileAttributes*`) consults `is_masked` before remapping. UE4SS never sees the
folder, so the Lua never loads. Nothing in the user's profile is touched;
removing the overlay package restores the tool. Keep the tombstone one level
*inside* `Mods/` - a tombstone directly in `Mods/` would mask the whole user mod
directory, which shimloader's own tests call out.

Replacing `Fusion.exe` with a do-nothing stub does **not** work, incidentally:
`main.lua` recovers the real profile paths by parsing `shimloader-log.txt` rather
than using the virtual `Mods/` path, and shimloader hooks no process-creation
API, so the child process never touches the overlay.

`report_rival_tool` checks both facts at every launch and logs which case
applies - installed via the real `SHIMLOADER_MOD_DIR` path, visible via the
logical one - because the tombstone is a packaging artifact the DLL cannot
create for itself: shimloader scans the overlay before the wrapper is loaded.

**Downstream - make sure only one merged pak is mounted.** Needed regardless,
since a pak from an earlier launch (or from running the tool outside the game)
is already on disk:

- `pak::classify` tags every pak in the pak directory from its index. Tables at
  the game's own `Content/main/datatables/` paths mean an already-merged output;
  mod content always lives at `Content/Mods/<mod>/_Content/...`. Name-independent,
  so a renamed or older output is still recognised, and content-thresholded (5+
  tables, or tables next to merged `Config/*.ini`) so an ordinary
  asset-replacement mod is not mistaken for one.
- Rival merged paks are renamed to `*.pak.disabled-by-fusion-wrapper`. UE only
  mounts `*.pak`, so the file goes inert while staying visible and reversible -
  drop the suffix to restore it. If the C# tool runs again and rewrites its pak,
  the next launch disables it again.
- Quarantine happens only once the result is known good: after our own pak is
  written, or on a cache hit where it already exists, or when there is nothing
  to merge at all. If any stage fails, rival paks are left mounted - a stale
  merge beats no merge.
- When the last mod is uninstalled, our own output is deleted too, so leftover
  rows can't outlive the mods that supplied them.

To go back to the C# tool, remove the overlay wrapper and drop the
`.disabled-by-fusion-wrapper` suffix.

## Mods built for another game version

Blueprint struct members serialize with a GUID suffix
(`price_27_397ABECF466FF5CA25FEBCBD300108FB`). Re-adding a member in the editor
changes that GUID, so a mod authored against an older row struct emits tags the
engine cannot match: the field falls back to its default and the item shows up
with no name, icon or mesh. Nothing errors - which is why the C# tool merged
such mods silently and the result read as "the loader is broken".

So the merge shape-checks first. `datatable::fuse_datatables` flattens the
template's first row into `column path -> type signatures` (recursing through
nested structs, and describing containers by their first element since length is
data, not shape) and sorts each mod column into one of three buckets:

- **Rebuilt or retyped** - the game still has a member of that base name, under
  a different `_<ordinal>_<GUID>` suffix or with a different type. The mod's
  value goes nowhere and the field defaults. Breaking.
- **Gone** - the game has no member of that name at all. The engine ignores the
  tag and the rest of the row still applies. Logged, not breaking.
- **Missing from the mod** - a field the game added later, filled from the struct
  default. Not reported.

A mod with any breaking column is dropped from **every** table, not just the
failing one: its tables cross-reference each other by row name (a store row
names a prop row), so a half-merged mod is worse than an absent one. Every other
mod merges normally.

That default is deliberately blunt, and real mods land on both sides of it - one
may lose only a spare flag while another loses its meshes and display names. So
`FusionWrapper.ini` in the config folder can let specific mods through:

```ini
# names are pak file stems, exactly as they appear in the log
merge_incompatible=CraftableWhiteMushrooms, SomeOtherMod
# or: merge_incompatible=all
```

Editing it invalidates the merge cache, so the change takes effect on the next
launch. Mods let through are still reported, flagged as merged anyway.

On Windows the report becomes a message box (`win::warn_incompatible_mods`)
naming each mod, its verdict, and the first few mismatching columns, because this
is the one failure a log file cannot surface: the game otherwise starts up
looking fine. The wording is built by `left_out_report` in the portable
half, so `cargo run --example fusiontool -- fuse ...` prints the exact dialog
text. It only appears when the merge actually runs - a cache hit skips
validation - so it shows up on the first launch after the mod set changes.

## Text encodings and non-ASCII paths

None of this pipeline assumes ASCII or UTF-8, which matters because a large part
of this game's audience is Russian-speaking.

**Asset content.** Row keys are FNames and item names/descriptions are FStrings.
`unreal_asset` stores an FString wide (UTF-16, negative length prefix) as soon as
it holds a non-ASCII character and narrow otherwise, and the FName hash it writes
follows the same split - which is what UE itself does. Verified by round-trip:
`fusiontool roundtrip <uasset>` parses an asset and serializes it straight back,
and the game's own `list_props` (642 KB, 2477 rows, 9694 name-map entries)
reproduces **byte-identically**, hashes included. A merge that injects a Cyrillic
row key plus Cyrillic display name, description and string column
(`fusiontool translate`) also round-trips byte-identically. The game's shipped
tables already contain non-ASCII text (typographic quotes and dashes in item
descriptions), so this path is exercised by every merge.

**Paths.** Install directory, mod pak names, mod folders and profile names may all
be non-ASCII; verified end-to-end on Windows with the whole tree under Cyrillic
directories and a pak named `Тест Мод №1.pak`. `module_dir` converts the module
path with `OsString::from_wide` rather than through a lossy `String`, and grows
its buffer instead of truncating at `MAX_PATH` - truncation there would silently
yield a *wrong* directory and misplace every subsequent read and write.

**Config and ini files.** These come from other people's editors, so `text::read`
honours a UTF-8, UTF-16LE or UTF-16BE BOM, detects BOM-less UTF-16 from NUL
placement, and otherwise decodes UTF-8 leniently (a bad byte becomes U+FFFD
rather than failing the file, matching what the C# tool's `File.ReadAllText`
does). This is not hypothetical: PowerShell 5.1's `>` writes UTF-16LE, so
`"merge_incompatible=..." > FusionWrapper.ini` produced a config that
`read_to_string` rejected and which was therefore silently ignored.

The user's `Input.ini` is *appended to*, and the engine rewrites that file as
UTF-16 the moment it holds a non-ANSI character - a Cyrillic action name is
enough. Appended lines are re-encoded to match what was already there, because
appending UTF-8 to a UTF-16 file would leave the user with a config the engine
reads as garbage.

## Failure handling

The guiding rule is that a mod either merges completely or not at all, and that
nothing silently does nothing:

- **Damaged mod file.** A `.uasset`/`.uexp` that won't parse drops that mod from
  every table and is reported in the dialog, for the same cross-reference reason
  as a struct mismatch. `merge_incompatible` does not override this - it is about
  struct drift, not corruption.
- **Interrupted pack.** The output pak is built under a per-process
  `.pak.<pid>.partial` name and renamed into place, so a run that dies partway -
  out of disk, permissions, killed process - can't leave a truncated pak for the
  engine to mount. UE only mounts `*.pak`, so a leftover partial is inert.
- **Nothing to merge.** A mod table whose name matches no game table, and a
  `.uasset` in a `datatables` folder without the `_` merge-source prefix, are both
  logged - each looks identical to a broken merge from the outside.
- **Duplicate row keys.** The engine's row map is keyed by name, so a repeated key
  makes one row unreachable; deliberate when a mod replaces a vanilla item, a
  silent conflict when two mods collide. Logged either way.
- **Unreadable mod pak.** A pak whose index won't parse (damaged download, or a
  missing Oodle runtime, which takes out every mod at once) is named in the log
  and the dialog, and the run is **not cached** so the next launch retries -
  a file locked by an antivirus scan or a still-running mod manager fixes itself.
- **Cache.** Keyed on path, mtime *and size* of every input, because swapping a
  mod for another build of itself can reproduce the mtime exactly (r2modman and
  backup restores both preserve timestamps).
- **Repeat reports.** The dialog only interrupts startup when its text differs
  from the last one shown (`fusion/reported.txt`). Without that, an unreadable
  pak - which deliberately defeats the cache - would pop a modal on every single
  launch until it was fixed. The log still records it every time.

## Bundled Oodle runtime

Mod paks for this game can be Oodle-compressed, so `repak` needs `oo2core`.
`repak`'s `oodle_loader` looks for the library next to the running executable
and, when it is missing, downloads it from GitHub - inside `DllMain`, at game
launch. Thunderstore doesn't allow runtime download and executing of binaries, 
so the overlay ships the library and `src/oodle.rs` copies it to the path the 
loader probes before any pak is opened.

Fetch it once when building the package:

```
curl -Lo oo2core_9_win64.dll https://github.com/WorkingRobot/OodleUE/raw/refs/heads/main/Engine/Source/Programs/Shared/EpicGames.Oodle/Sdk/2.9.10/win/redist/oo2core_9_win64.dll
sha256sum oo2core_9_win64.dll   # 6f5d41a7892ea6b2db420f2458dad2f84a63901c9a93ce9497337b16c195f457
```
That is the same file, version and hash `oodle_loader` would have downloaded.
Without it the wrapper still runs, logs an error, and skips every
Oodle-compressed mod pak - the datatables in those mods silently go missing.

## Performance

Measured on the same inputs (VotV a09n, SmartTV + SlotMachine, 34 templates out
of an 8 GB game pak), release build, median of three runs. "Cold" is a full
merge; "warm" is an unchanged mod set:

| | C# Fusion | this wrapper |
|---|---|---|
| cold merge | 1425 ms | 321 ms |
| warm launch | 213 ms | 19 ms (~4 ms in-process) |
| output pak | 4,475,500 B | 801,122 B |
| processes spawned | `repak.exe` per pak + pack, then **relaunches the game** | none |

The C# tool's cost is structural rather than sloppy: it shells out to
`repak.exe` for every extract and for the pack (`Utils.cs`), packs without
compression, and `Utils.LaunchGame()` writes a `pass` marker and starts a second
game process, so any change to the mod set costs a full second game startup on
top of the merge. Running in-process removes that entirely.

Two things dominated our own cold time before they were fixed, both worth
knowing if this code is extended:

- **Unbuffered asset I/O.** `unreal_asset` reads and writes in primitive-sized
  steps, so passing it a `File` made `list_props` (2477 rows, 642 KB + 1.4 MB)
  take 1229 ms to parse and 3552 ms to write. Parsing from and serializing into
  a `Cursor<Vec<u8>>` took those to 36 ms and 9 ms.
- **Whole-asset name-map rebuilds.** `Asset::rebuild_name_map` re-examines every
  FName in the destination table once per merged mod. `rekey_into` re-keys only
  the rows and imports that actually moved, which is proportional to the mod
  rather than to the game's table.

`fuse` logs a stage breakdown (`timings ms: templates=… datatables=… pack=…`)
and `merge_one` logs per-table `parse/merge/write`, so a regression here shows up
in the log without re-instrumenting.

## Prerequisites

- Rust stable with the `x86_64-pc-windows-msvc` target. `rust-toolchain.toml`
  and `.cargo/config.toml` already pin both, so `rustup` will fetch what's
  needed on first build.
- A working MSVC toolchain (Visual Studio Build Tools with the C++ workload,
  or equivalent) so the `windows-msvc` target can link.
- A sibling checkout of [`repak`](https://github.com/trumank/repak) at
  `../repak` relative to this crate - `Cargo.toml` resolves `repak` via a
  path dependency. Adjust the path there if your layout differs.

## Build

```
cargo build --release
```

Produces `target/x86_64-pc-windows-msvc/release/ue4ss.dll`.

### `REAL_UE4SS_PATH` (build-time)

`build.rs` reads the `REAL_UE4SS_PATH` env var at compile time, parses the
PE at that path, and emits a `.def` file that statically forwards every
C++-mangled export to `ue4ss_real.dll`. Without it the wrapper still
builds, but UE4SS C++ mods that link against UE4SS exports will fail to
load. Set it to the path of a real `ue4ss.dll` whose ABI matches the one
end users will run, e.g.:

```
REAL_UE4SS_PATH=/path/to/ue4ss.dll cargo build --release
```

It only affects the build: end users never set this, and the wrapper
doesn't read it at runtime.

A `.def` file replaces the whole export table, so UE4SS's plain `extern "C"`
Lua API symbols would otherwise disappear from the wrapper. `build.rs` lists
them in the `.def` without a forwarding target, which exports the runtime
forwarders in `src/lua_forward.rs` instead; the built DLL should have 6 plain
exports alongside the mangled forwarders. Any `extern "C"` export in the real
DLL that has no forwarder there produces a build warning.

## Offline verification

The pipeline is target-independent (only `src/win.rs` and `src/lua_forward.rs`
need Windows), so it can be run and diffed without launching the game:

```
# whole pipeline, exactly as DllMain runs it
SHIMLOADER_PAK_DIR=<profile>/shimloader/pak SHIMLOADER_CFG_DIR=<profile>/shimloader/cfg \
  cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- \
  fuse <fake-install>/<Project>/Binaries/Win64

# merge in place into a directory of templates, then read a table back
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- merge <staged_dir> <extracted_mod_dir>...
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- dump <uasset>

# serialization fidelity: parse an asset, write it back, compare bytes
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- roundtrip <uasset>
```

`dump` prints every row with resolved FName contents, instance numbers, and
import targets, which is what makes cross-checking possible: dump the merged
table and the same table out of the C# Fusion's `FusionPatch_P.pak`, sort, and
diff. Row order depends on mod processing order, so compare sorted.

Two commands exist to manufacture conditions that are otherwise hard to obtain:
`skew <uasset> <column-substring> <new-name>` rewrites a column name in a real
asset, reproducing the GUID drift of a mod built against an older game version,
and `translate <uasset> <text>` rewrites a row key and its text values, standing
in for a mod authored in a non-Latin language. Both operate on a copy you point
them at.

## Status

`FuseDatatables` and `FuseInputs` only - the dead-code `FuseActors` and
`ReplacementMod` paths from the C# version are intentionally not ported.
