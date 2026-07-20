//! `kkernel` binary — thin shim over the library entrypoint. All CLI parsing
//! and command dispatch live in `kkernel::cli` so downstream crates can
//! compose this binary's command surface from the library target directly.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    kkernel::run().await
}
