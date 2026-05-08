//! Runtime export forwarders for UE4SS's extern "C" Lua API symbols.
//!
//! MSVC's `.def`-file forwarder syntax (`name=otherdll.name`) doesn't work
//! for plain C identifiers - the linker treats them as local-symbol aliases
//! and emits LNK2001 because no local definition exists. C++-mangled names
//! (which all start with `?`) sidestep this, so build.rs handles those
//! statically and the small set of extern "C" exports is forwarded here at
//! runtime via `GetProcAddress` against the already-loaded `ue4ss_real.dll`.
//!
//! If UE4SS adds more extern "C" exports, build.rs prints a `cargo:warning`
//! listing the names that were skipped - add a matching forwarder here.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::OnceLock;

use widestring::U16CString;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

type Opaque = *mut c_void;

unsafe fn resolve<F: Copy>(slot: &OnceLock<Option<F>>, name: &[u8]) -> Option<F> {
    *slot.get_or_init(|| {
        let dll = U16CString::from_str("ue4ss_real.dll").ok()?;
        let h: HMODULE = GetModuleHandleW(dll.as_ptr());
        if h.is_null() {
            return None;
        }
        let f = GetProcAddress(h, name.as_ptr())?;
        Some(std::mem::transmute_copy(&f))
    })
}

#[no_mangle]
pub unsafe extern "C" fn get_lua_state_by_mod_name(mod_name: *const c_char) -> Opaque {
    static F: OnceLock<Option<unsafe extern "C" fn(*const c_char) -> Opaque>> = OnceLock::new();
    match resolve(&F, b"get_lua_state_by_mod_name\0") {
        Some(f) => f(mod_name),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn execute_lua_in_mod(
    mod_name: *const c_char,
    script: *const c_char,
    output_buffer: *mut c_char,
) -> *const c_char {
    static F: OnceLock<
        Option<unsafe extern "C" fn(*const c_char, *const c_char, *mut c_char) -> *const c_char>,
    > = OnceLock::new();
    match resolve(&F, b"execute_lua_in_mod\0") {
        Some(f) => f(mod_name, script, output_buffer),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn set_script_variable_int32(
    mod_name: *const c_char,
    variable_name: *const c_char,
    new_value: i32,
    return_value: Opaque,
) {
    static F: OnceLock<Option<unsafe extern "C" fn(*const c_char, *const c_char, i32, Opaque)>> =
        OnceLock::new();
    if let Some(f) = resolve(&F, b"set_script_variable_int32\0") {
        f(mod_name, variable_name, new_value, return_value);
    }
}

#[no_mangle]
pub unsafe extern "C" fn set_script_variable_default_data(
    mod_name: *const c_char,
    variable_name: *const c_char,
    external_data: Opaque,
    return_value: Opaque,
) {
    static F: OnceLock<
        Option<unsafe extern "C" fn(*const c_char, *const c_char, Opaque, Opaque)>,
    > = OnceLock::new();
    if let Some(f) = resolve(&F, b"set_script_variable_default_data\0") {
        f(mod_name, variable_name, external_data, return_value);
    }
}

#[no_mangle]
pub unsafe extern "C" fn call_script_function(
    mod_name: *const c_char,
    function_name: *const c_char,
    return_value: Opaque,
    script_func_return_value: Opaque,
) {
    static F: OnceLock<
        Option<unsafe extern "C" fn(*const c_char, *const c_char, Opaque, Opaque)>,
    > = OnceLock::new();
    if let Some(f) = resolve(&F, b"call_script_function\0") {
        f(mod_name, function_name, return_value, script_func_return_value);
    }
}

#[no_mangle]
pub unsafe extern "C" fn is_ue4ss_initialized() -> bool {
    static F: OnceLock<Option<unsafe extern "C" fn() -> bool>> = OnceLock::new();
    match resolve(&F, b"is_ue4ss_initialized\0") {
        Some(f) => f(),
        None => false,
    }
}
