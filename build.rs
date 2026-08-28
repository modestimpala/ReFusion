use std::env;
use std::fs;
use std::path::PathBuf;

const REAL_NAME: &str = "ue4ss_real";

/// `extern "C"` exports implemented locally in `src/lua_forward.rs`.
///
/// A `.def` file replaces the entire export table, so `#[no_mangle]` functions
/// are dropped unless they are listed in it as well. Keep this in sync with
/// `lua_forward.rs`; a name here that has no local definition is an LNK2001.
const LOCAL_C_EXPORTS: &[&str] = &[
    "call_script_function",
    "execute_lua_in_mod",
    "get_lua_state_by_mod_name",
    "is_ue4ss_initialized",
    "set_script_variable_default_data",
    "set_script_variable_int32",
];

fn main() {
    println!("cargo:rerun-if-env-changed=REAL_UE4SS_PATH");

    // The `.def` forwarder trick is MSVC-only; native (Linux) builds of the
    // library exist purely to run the pipeline offline, so skip it there.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

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

    let bytes =
        fs::read(&real_path).unwrap_or_else(|e| panic!("read REAL_UE4SS_PATH={real_path:?}: {e}"));
    let pe =
        goblin::pe::PE::parse(&bytes).unwrap_or_else(|e| panic!("parse {real_path:?} as PE: {e}"));

    let mut def = String::from("EXPORTS\n");
    let mut named = 0usize;
    let mut unnamed = 0usize;
    let mut skipped_extern_c: Vec<String> = Vec::new();
    for export in &pe.exports {
        match export.name {
            Some(name) if !name.is_empty() => {
                // MSVC treats a `.def` forwarder for a plain C identifier as a
                // local-symbol alias (LNK2001) rather than a forwarder, so
                // those are re-exported from our own runtime forwarders in
                // src/lua_forward.rs and listed without a target below. An
                // extern "C" export with no local implementation can't be
                // covered at all - C++ mods importing it would silently fail -
                // so it gets a build warning.
                if !name.starts_with('?') {
                    if !LOCAL_C_EXPORTS.contains(&name) {
                        skipped_extern_c.push(name.to_string());
                    }
                    continue;
                }
                def.push_str(&format!("    {name}={REAL_NAME}.{name}\n"));
                named += 1;
            }
            _ => unnamed += 1,
        }
    }

    // Local implementations: listed with no target, so the linker exports our
    // own symbol instead of forwarding.
    for name in LOCAL_C_EXPORTS {
        def.push_str(&format!("    {name}\n"));
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
            "cargo:warning={} extern \"C\" exports in {real_path:?} have no \
             local forwarder in src/lua_forward.rs and are NOT exported; C++ \
             mods importing them will fail: {}",
            skipped_extern_c.len(),
            skipped_extern_c.join(", "),
        );
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let def_path = out_dir.join("ue4ss.def");
    fs::write(&def_path, &def).unwrap_or_else(|e| panic!("write {def_path:?}: {e}"));

    println!(
        "cargo:warning=ue4ss wrapper: forwarding {named} exports to \
         {REAL_NAME}.dll, plus {} local extern \"C\" exports",
        LOCAL_C_EXPORTS.len()
    );
    println!("cargo:rustc-link-arg-cdylib=/DEF:{}", def_path.display());
}
