//! Shared test-only helpers for khive-pack-schedule's integration tests.
//!
//! Issue #779: 8 (then 14, then 42, accelerating) year-2099 sentinel fixture
//! rows from schedule pack fixture content ("check the build", "repeat=daily",
//! "window-early", "sort-order-jan", ...) were found parked in a real,
//! on-disk store. Every test in this crate already constructs its own
//! `KhiveRuntime` explicitly rather than resolving one from ambient CLI
//! args/env/config discovery, but that invariant lived only as an unenforced
//! convention duplicated across five separate test files (`agenda.rs`,
//! `cancel.rs`, `create_validation.rs`, `integration.rs`,
//! `pack_registration.rs`) -- nothing stopped a future edit from silently
//! swapping in an ambiently-resolved runtime.
//!
//! `memory_runtime` is the single choke point every one of those files now
//! routes through: it constructs the in-memory runtime AND asserts the
//! resulting backend is not file-backed, so a regression fails the test
//! immediately (in-process, no I/O against any real path) instead of quietly
//! writing fixtures into whatever store the environment happens to resolve.
//! This is a test-only helper -- it is compiled only into the `tests/*`
//! integration test binaries, never into the shipped crate.

use khive_runtime::KhiveRuntime;

/// Construct an in-memory `KhiveRuntime` for schedule pack tests, and refuse
/// to hand back anything file-backed.
///
/// `KhiveRuntime::memory()` always resolves to `db_path: None` (a true
/// SQLite `:memory:` connection, never a file literally named `:memory:`) by
/// construction, so this assertion should never trip today. It exists to
/// catch a future regression -- e.g. a call site switched to
/// `KhiveRuntime::new(RuntimeConfig::default())` or another path that goes
/// through ambient config/env/cwd resolution -- before it can write a single
/// fixture row into a real database.
pub fn memory_runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    assert!(
        !runtime.backend().is_file_backed(),
        "khive-pack-schedule tests must construct an in-memory KhiveRuntime, \
         never a file-backed one (issue #779) -- a file-backed backend here \
         means test fixtures would be written into a real, on-disk store"
    );
    runtime
}
