# ReFusion

<div align="center">
    <img src="./thunderstore/icon_full.png" alt="Project Screenshot" width="200">
</div>

In-process port of [NynrahGhost's Fusion](https://github.com/NynrahGhost/Fusion) datatable/input merger, packaged as a
shimloader overlay wrapper. Replaces the standalone two-step C# launcher.

## How it works

- Ships as `ue4ss.dll` in a shimloader overlay, so
  `LoadLibraryW("Win64/ue4ss.dll")` loads this wrapper first.
- `DllMain` reads the `SHIMLOADER_*` environment variables, places the bundled
  Oodle runtime next to the game executable, and runs the merge before the game
  initializes. See [Bundled Oodle runtime](#bundled-oodle-runtime).
- The merge extracts the game's datatable templates and `DefaultInput.ini`,
  merges in mod content, and writes `FusionPatch_P.pak` to the user's pak
  directory.
- Pak entries are resolved against their mount points before filtering, since
  the same asset may be stored under a full `_Content/...` path or as a bare
  filename depending on how the mod was packed.
- On every launch, the wrapper derives `<profile>` from
  `SHIMLOADER_OVERLAY_DIR`, copies `<profile>/ue4ss.dll` to
  `<my_dir>/ue4ss_real.dll`, then loads it to hand off to the real UE4SS. This
  keeps the staged DLL current and avoids shimloader's spoofed module path.

## Why this exists

This exists to address problems that I think are either unsolvable from the
classic Lua/C++ mod approach or inherent to Fusion itself.

### No extra game launch

Pak access depends on game filesystem locks, so any Fusion datatable merge has
to run while the game is closed and finish before it launches.

The original C# Fusion mod relied on a Lua script to launch an executable. That
executable killed the game process to release the file lock, ran the datatable
and asset merges, created the pak, and relaunched the game. That means an
*extra* game process launch every time the datatables have to be re-fused.

This only happens when paks are changed or added, but it is still unnecessary
overhead. Fusion also cannot close the game until its Lua mod initializes,
which can mean several seconds of waiting depending on the environment.

This mod bypasses that entirely by pretending to *be* UE4SS at first. The
normal path is Game Executable Launch -> Shimloader -> UE4SS -> Actual UE Game
Initialization. We're inserted after shimloader and before UE4SS, so all pak
operations finish before the game has any idea those paks even exist.

### Visible compatibility failures

The original Fusion mod would silently fail on non-English paths and mods that
no longer line up with the game's current structs. This version supports a wide
variety of text encodings and visibly warns players about incompatible mods
while continuing to merge the ones that do work.

### Less overhead and smaller paks

The entire pipeline stays in Rust from extraction through packing. It never has
to cross into Lua or game code, so it can use the Rust-native `unreal_asset` and
`repak` crates directly and keep all the work in-process.

The original mod runs several `repak` processes per pak, and the result is often
several MB for just a few small datatables. This version includes Oodle support
and built-in optimizations, making a cold merge over 4x faster and producing an
output pak about 82% smaller in the measurements below.


## Install layout

The wrapper is shipped as an overlay package. 

```
<profile>/
├── ue4ss.dll                     (the real UE4SS, installed normally)
├── shimloader/cfg/
│   └── ReFusion.ini         (optional; see "Mods built for another game version")
└── shimloader/overlay/ReFusion/
    ├── ue4ss.dll                 (this repo's build output, renamed)
    ├── Mods/NynrahGhost-Fusion/
    │   └── .shim-removed         (masks the C# Fusion's Lua mod; see below)
    └── fusion/
        └── oo2core_9_win64.dll   (Oodle runtime, see below)
```

Shimloader mirrors the overlay package onto
`<install>/<Project>/Binaries/Win64`, mapping each file individually rather
than redirecting whole directories. Anything the wrapper writes to a
`my_dir`-relative path not claimed by the overlay therefore lands in the real
game directory:

```
<install>/<Project>/Binaries/Win64/
├── ue4ss_real.dll                (re-copied each launch from <profile>/ue4ss.dll)
├── oo2core_9_win64.dll           (placed from the overlay copy, where repak looks)
├── refusion-log.txt
└── fusion/
    ├── work/                     (extracted paks + merge staging)
    └── last-write.txt            (input mtimes, skips redundant merges)
```

And the launch params gain `--overlay-dir "<profile>/shimloader/overlay"` (set by r2modman, etc).

## Coexisting with the C# Fusion

This is both a drop-in replacement for the C# Fusion and a standalone one. New
mods can depend on this package alone, but many existing mods still depend on
`NynrahGhost-Fusion`, so mod managers will often install both side by side.
That is expected, not user error.

Both tools write a `FusionPatch_P.pak` containing the same entry paths. If both
outputs mount, pak precedence decides which one wins, and a stale merge can
silently replace the correct one. The wrapper prevents that at two points.

**Upstream - stop the C# tool from running.** The overlay ships this empty
marker:

```
shimloader/overlay/ReFusion/Mods/NynrahGhost-Fusion/.shim-removed
```

Shimloader treats `.shim-removed` as a tombstone, hiding the C# Fusion mod from
UE4SS so its Lua launcher never runs. The real installation is untouched;
removing this overlay makes it visible again. The marker must stay inside
`Mods/NynrahGhost-Fusion` - placing it directly inside `Mods` would hide every
user mod. `report_rival_tool` logs whether the C# tool is installed and whether
the mask is working.

**Downstream - disable stale merged paks.** `pak::classify` identifies merged
outputs by their contents rather than their filenames, then renames rivals to
`*.pak.disabled-by-refusion`. The files remain visible and reversible but
no longer mount.

This only happens after our result is known good: after a successful write, on
a valid cache hit, or when there is nothing to merge. If the merge fails, rival
paks remain mounted because a stale merge is better than no merge. When the last
mod is removed, our own output is deleted as well.

To go back to the C# tool, remove the overlay wrapper and drop the
`.disabled-by-refusion` suffix.

## Mods built against a different struct

Blueprint struct members serialize with an ordinal and GUID suffix
(`price_27_397ABECF466FF5CA25FEBCBD300108FB`). Both are minted once, when the
member is created in the editor, so a mod built against the game's own struct
carries the game's spelling, while a mod whose author rebuilt or hand-wrote the
struct carries its own. The engine matches tagged properties by that whole
mangled name (`UStruct::SerializeTaggedProperties`), so a member whose suffix
differs is not matched at all: names, icons, and meshes silently fall back to
defaults with no error. The engine's own GUID-based redirect cannot help, because
it keys on the member GUID, which is exactly what differs.

ReFusion compares each mod table against the game's current struct and sorts
every column into one of three outcomes:

- **Drifted.** Same base name, same serialized type, one unambiguous match in the
  game's struct. The merge rewrites the tag to the game's spelling, so the mod's
  value arrives. This is what makes a hand-written struct work, and it is
  logged per table with a count of what was rewritten.
- **Breaking.** The type changed, or the name matches several members and no
  rename can be proven. The value cannot be delivered.
- **Removed.** The game has no such member. The engine ignores the tag and the
  rest of the row still loads, so this is logged but not breaking.

A mod with any breaking column is dropped from **every** table, not just the
failing one. Its tables may cross-reference each other, so a partial merge is
worse than leaving the mod out. Compatible mods still merge normally.

Drift is matched per column path, pooled across every table, because these
structs are shared and only partly populated: no `list_equipment` row fills in
the nested save-data struct that other tables do, so its members can only be
learned elsewhere.

One case is deliberately left alone. An empty `Array<StructProperty>` cannot be
renamed consistently: `unreal_asset` writes its inner tag from a private field
and reads that tag back over the property's name, so a rename would produce a
file that contradicts itself. Nothing is lost, because an unmatched array member
defaults to empty, which is what it already was.

That breaking default is deliberately blunt: a mismatch may be an unused flag or
something essential. Specific mods can be forced through with `ReFusion.ini`:

```ini
# names are pak file stems, exactly as they appear in the log
merge_incompatible=CraftableWhiteMushrooms, SomeOtherMod
# or: merge_incompatible=all
```

Editing it invalidates the merge cache, so the change takes effect on the next
launch. Forced mods are still reported as incompatible but marked as merged.

On Windows, a message box names each incompatible mod, whether it was skipped or
forced, and its first few mismatched columns. It only appears when the report
differs from the last one shown.

## Text encodings and non-ASCII paths

The pipeline does not assume ASCII or UTF-8. Non-Latin paths, filenames, and mod
content are supported throughout.

**Asset content.** Row keys are FNames and item names/descriptions are FStrings.
`unreal_asset` writes non-ASCII strings as UTF-16 and produces the same FName
hashes as UE. This was verified by round-tripping the game's 642 KB `list_props`
table **byte-identically**, then repeating the test with Cyrillic row keys and
text injected by `fusiontool translate`.

**Paths.** Install directory, mod pak names, mod folders and profile names may all
be non-ASCII. This was verified end to end on Windows with the entire tree under
Cyrillic directories and a pak named `Тест Мод №1.pak`. Module paths stay as
native wide strings, and the path buffer grows beyond `MAX_PATH` instead of
silently truncating.

**Config and ini files.** `text::read` handles UTF-8, UTF-16LE, and UTF-16BE,
with or without a BOM, and replaces invalid UTF-8 bytes rather than rejecting
the file. This also supports UTF-16 configs produced by Windows PowerShell 5.1.
When appending to the user's `Input.ini`, new lines are encoded to match the
existing file so UTF-8 is never mixed into a UTF-16 config.

## Failure handling

Mods either merge completely or are visibly left out:

- **Damaged mod file.** A `.uasset`/`.uexp` that won't parse drops that mod from
  every table and reports it. `merge_incompatible` cannot override corruption.
- **Interrupted pack.** Output is written as `.pak.<pid>.partial` and renamed
  only when complete. Leftover partials do not mount.
- **Nothing to merge.** Unmatched tables and datatable assets without the `_`
  merge-source prefix are logged rather than silently ignored.
- **Duplicate row keys.** Vanilla replacements and conflicts between mods are
  both logged.
- **Unreadable mod pak.** Damaged, locked, or Oodle-compressed paks that cannot
  be read are reported. The run is not cached, so the next launch retries.
- **Cache.** Inputs are keyed by path, modification time, and size, covering mod
  updates that preserve timestamps.
- **Repeat reports.** A dialog appears only when its text changes, avoiding the
  same modal on every retry. The log still records every failure.

## Bundled Oodle runtime

Oodle-compressed mod paks require `oo2core`. Normally, `repak` downloads the
runtime when it is missing, but doing that inside `DllMain` at game launch is
not suitable for Thunderstore. The overlay bundles it instead, and the wrapper
places it next to the game executable before opening any paks.

Fetch it once when building the package:

```
curl -Lo oo2core_9_win64.dll https://github.com/WorkingRobot/OodleUE/raw/refs/heads/main/Engine/Source/Programs/Shared/EpicGames.Oodle/Sdk/2.9.10/win/redist/oo2core_9_win64.dll
sha256sum oo2core_9_win64.dll   # 6f5d41a7892ea6b2db420f2458dad2f84a63901c9a93ce9497337b16c195f457
```
That is the same file, version and hash `oodle_loader` would have downloaded at the time of this commit. 
Without it the wrapper still runs, logs an error, and skips every
Oodle-compressed mod pak. 

## Performance

Release build, median of three runs using VotV a09n, SmartTV + SlotMachine, and
34 templates from an 8 GB game pak:

| | C# Fusion | this wrapper |
|---|---|---|
| cold merge | 1425 ms | 321 ms (77% faster) |
| warm launch | 213 ms | 19 ms (91% faster; ~4 ms in-process) |
| output pak | 4,475,500 B | 801,122 B (82% smaller) |
| processes spawned | `repak.exe` per pak + pack, then **relaunches the game** | none |

The C# tool starts `repak.exe` for every extraction and pack, writes an
uncompressed output, then relaunches the game. Running the pipeline in-process
removes that overhead.

Cold performance also depends on buffered asset I/O and incremental name-map
updates. Reading and writing through `Cursor<Vec<u8>>` reduced the largest
table's parse/write time from 1229/3552 ms to 36/9 ms, while `rekey_into` avoids
rebuilding the entire destination name map for every mod.

The log includes stage and per-table timings so regressions are visible without
additional instrumentation.

## Prerequisites

- Rust stable and `x86_64-pc-windows-msvc`, both pinned by the repo.
- An MSVC C++ toolchain for linking.
- A [`repak`](https://github.com/trumank/repak) checkout at `../repak`, or an
  adjusted path in `Cargo.toml`.

## Build

```
cargo build --release
```

Produces `target/x86_64-pc-windows-msvc/release/ue4ss.dll`.

### `REAL_UE4SS_PATH` (build-time)

Set `REAL_UE4SS_PATH` to an ABI-compatible real `ue4ss.dll` when building:

```
REAL_UE4SS_PATH=/path/to/ue4ss.dll cargo build --release
```

`build.rs` uses it to generate a `.def` that forwards the real DLL's C++ exports
to `ue4ss_real.dll`. The wrapper builds without it, **but UE4SS C++ mods cannot
link through the wrapper**. End users do not set this variable.

The six plain C Lua API exports are implemented by `src/lua_forward.rs` rather
than forwarded statically. Any unsupported plain export produces a build
warning.

## Offline verification

The merge pipeline is cross-platform, so it can be tested without launching the
game:

```
# whole pipeline, exactly as DllMain runs it
SHIMLOADER_PAK_DIR=<profile>/shimloader/pak SHIMLOADER_CFG_DIR=<profile>/shimloader/cfg \
  cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- \
  fuse <fake-install>/<Project>/Binaries/Win64

# merge extracted assets, inspect a table, or verify a byte-identical round trip
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- merge <staged_dir> <extracted_mod_dir>...
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- dump <uasset>
cargo run --target x86_64-unknown-linux-gnu --example fusiontool -- roundtrip <uasset>
```

`dump` resolves FNames and imports for comparison with the C# Fusion output.
Sort before diffing because row order depends on mod processing order.

`skew <uasset> <column-substring> <new-name>` simulates struct GUID drift, while
`translate <uasset> <text>` replaces a row key and its text values for non-Latin
content testing. Both modify only the file provided.

## Status

Feature parity with the C# Fusion for datatable and input merging. The unused
`FuseActors` and `ReplacementMod` paths were intentionally not ported.
