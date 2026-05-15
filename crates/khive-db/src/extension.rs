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

    INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

#[cfg(not(feature = "vectors"))]
pub fn ensure_extensions_loaded() {
    // No-op: the `vectors` feature is not enabled.
    // vec0 tables and vec_* functions will not be available on connections
    // opened by this process.
}
