//! SQLite extension registration for sqlite-vec.
//!
//! Provides `ensure_extensions_loaded` which registers `sqlite-vec` as an
//! auto-extension so every new connection has `vec0` and `vec_*` available.

/// Register sqlite-vec as a SQLite auto-extension so that every new connection
/// automatically has the `vec0` virtual table and `vec_*` scalar functions
/// available.
///
/// Must be called once before any connection that creates or queries `vec0`
/// tables. It is safe to call multiple times — subsequent calls are no-ops.
///
/// Requires the `vectors` Cargo feature. When the feature is absent this
/// function is a no-op and any attempt to use `vec0` tables will fail with a
/// SQLite "no such module" error.
#[cfg(feature = "vectors")]
pub fn ensure_extensions_loaded() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    // SAFETY: `sqlite3_vec_init` is a static function that satisfies the
    // 3-arg xEntryPoint ABI after transmute (the standard SQLite auto-extension
    // pattern). `INIT.call_once` guarantees a single registration, preventing
    // duplicate auto-extension entries. The function pointer is `'static`.
    INIT.call_once(|| unsafe {
        // sqlite-vec exports sqlite3_vec_init as `extern "C" fn()` for
        // auto_extension registration. sqlite3_auto_extension expects
        // the 3-arg xEntryPoint signature, so we transmute the function
        // pointer — this is the standard pattern for static extensions.
        type AutoExtFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;

        let f: AutoExtFn =
            std::mem::transmute::<unsafe extern "C" fn(), AutoExtFn>(sqlite_vec::sqlite3_vec_init);
        rusqlite::ffi::sqlite3_auto_extension(Some(f));
    });
}

/// No-op stub when the `vectors` feature is disabled.
#[cfg(not(feature = "vectors"))]
pub fn ensure_extensions_loaded() {
    // No-op: the `vectors` feature is not enabled.
    // vec0 tables and vec_* functions will not be available on connections
    // opened by this process.
}
