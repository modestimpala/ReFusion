use std::env;
use std::fs;
use std::path::PathBuf;

const REAL_NAME: &str = "ue4ss_real";

fn main() {
    println!("cargo:rerun-if-env-changed=REAL_UE4SS_PATH");

    let Some(real_path) = env::var_os("REAL_UE4SS_PATH") else {
        println!(
            "cargo:warning=REAL_UE4SS_PATH not set; wrapper will lack export \
             forwarders, so UE4SS C++ mods will fail to load. Set it to a path \
             pointing at the real ue4ss.dll and rebuild."
        );
        return;
    };
    let real_path = PathBuf::from(real_path);
    println!("cargo:rerun-if-changed={}", real_path.display());

    let bytes = fs::read(&real_path)
        .unwrap_or_else(|e| panic!("read REAL_UE4SS_PATH={real_path:?}: {e}"));
    let pe = goblin::pe::PE::parse(&bytes)
        .unwrap_or_else(|e| panic!("parse {real_path:?} as PE: {e}"));

    let mut def = String::from("EXPORTS\n");
    let mut named = 0usize;
    let mut unnamed = 0usize;
    let mut skipped_extern_c: Vec<String> = Vec::new();
    for export in &pe.exports {
        match export.name {
            Some(name) if !name.is_empty() => {
                // MSVC linker treats `.def` forwarders for plain C identifiers
                // as local-symbol aliases (LNK2001) instead of forwarders.
                // Only C++-mangled names (which start with `?`) work
                // statically. Anything else gets a hand-written runtime
                // forwarder in src/lua_forward.rs - if a new extern "C"
                // export shows up here that isn't already covered there,
                // C++ mods importing it will silently fail.
                if !name.starts_with('?') {
                    skipped_extern_c.push(name.to_string());
                    continue;
                }
                def.push_str(&format!("    {name}={REAL_NAME}.{name}\n"));
                named += 1;
            }
            _ => unnamed += 1,
        }
    }

    if named == 0 {
        panic!("no named exports found in {real_path:?}");
    }
    if unnamed > 0 {
        println!(
            "cargo:warning={unnamed} ordinal-only exports in {real_path:?} \
             are NOT forwarded; mods importing by ordinal will fail."
        );
    }
    if !skipped_extern_c.is_empty() {
        println!(
            "cargo:warning=skipped {} extern \"C\" exports (handled by runtime \
             forwarders in src/lua_forward.rs): {}",
            skipped_extern_c.len(),
            skipped_extern_c.join(", "),
        );
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let def_path = out_dir.join("ue4ss.def");
    fs::write(&def_path, &def).unwrap_or_else(|e| panic!("write {def_path:?}: {e}"));

    println!("cargo:warning=ue4ss wrapper: forwarding {named} exports to {REAL_NAME}.dll");
    println!("cargo:rustc-link-arg-cdylib=/DEF:{}", def_path.display());
}
