//! Test consumer for the runtime without its own dev-dependency features.
//!
//! Run `cargo test -p khive-runtime-production-tests` separately from workspace
//! tests. The CI artifact check confirms the runtime library was compiled
//! without `cfg(test)`, `test-internals`, or `fault-injection`.
