# fusion-wrapper

In-process port of [NynrahGhost's Fusion](https://github.com/NynrahGhost/Fusion) datatable/input merger, packaged as a
shimloader overlay wrapper. Replaces the standalone two-step C# launcher.

## How it works

- Built as `ue4ss.dll`. Ships in a shimloader overlay package so shimloader's
  `LoadLibraryW("Win64/ue4ss.dll")` resolves to this wrapper instead of the
  real ue4ss.
- Wrapper's `DllMain` reads the `SHIMLOADER_*` env vars to discover where the user's mods, paks, and configs
  live.
- Runs the fusion pipeline: extract mod paks, merge datatables into the
  shipped templates, merge `DefaultInput.ini` files, write
  `FusionPatch_P.pak` into the user's pak directory.
- Copies `<profile>/ue4ss.dll` over `<my_dir>/ue4ss_real.dll` on every
  launch so the staged DLL never goes stale relative to the user's UE4SS
  install. The profile root is derived from `SHIMLOADER_OVERLAY_DIR`
  (whose value is `<profile>/shimloader/overlay`); we can't use the
  wrapper's own module path because shimloader spoofs that to look like a
  Win64 binary path. The destination stays `my_dir`-relative — shimloader's
  overlay registry redirects the write back to the real overlay dir.
- Then `LoadLibraryW("ue4ss_real.dll")` to hand off to the real UE4SS.

## Install layout

The wrapper is shipped as an overlay package. End users never assemble this
by hand - a Thunderstore/r2modman package would lay it out:

```
<profile>/
├── ue4ss.dll                     (the real UE4SS, installed normally)
└── shimloader/overlay/FusionWrapper/
    ├── ue4ss.dll                 (this crate's build output, renamed)
    ├── ue4ss_real.dll            (re-copied each launch from <profile>/ue4ss.dll)
    └── fusion/
        ├── templates/
        │   └── datatables/       (canonical .uasset/.uexp files to merge into)
        └── DefaultInput.ini      (base inputs file)
```

And the launch params gain `--overlay-dir "<profile>/shimloader/overlay"`.

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

It only affects the build — end users never set this, and the wrapper
doesn't read it at runtime.

## Status

`FuseDatatables` and `FuseInputs` only - the dead-code `FuseActors` and
`ReplacementMod` paths from the C# version are intentionally not ported.
