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
- Then `LoadLibraryW("ue4ss_real.dll")` to hand off to the real UE4SS.

## Install layout

The wrapper is shipped as an overlay package. End users never assemble this
by hand - a Thunderstore/r2modman package would lay it out:

```
<profile>/shimloader/overlay/FusionWrapper/
├── ue4ss.dll              (this crate's build output, renamed)
├── ue4ss_real.dll         (the real UE4SS, renamed)
└── fusion/
    ├── templates/
    │   └── datatables/    (canonical .uasset/.uexp files to merge into)
    └── DefaultInput.ini   (base inputs file)
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

## Status

`FuseDatatables` and `FuseInputs` only - the dead-code `FuseActors` and
`ReplacementMod` paths from the C# version are intentionally not ported.
