//! `kkernel` binary shell — the CLI itself lives in `kkernel::cli` so
//! downstream distributions can embed it with additional packs linked in.

fn main() -> anyhow::Result<()> {
    #[cfg(all(
        target_pointer_width = "64",
        any(target_os = "linux", target_os = "macos")
    ))]
    // SAFETY: this is the first action in the binary, before CLI configuration,
    // Tokio workers, extension registration or any SQLite file I/O can start.
    unsafe {
        khive_db::pool::initialize_claimed_file_observer()?;
    }
    kkernel::cli::cli_main()
}
