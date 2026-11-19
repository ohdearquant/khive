//! `kkernel` binary shell — the CLI itself lives in `kkernel::cli` so
//! downstream distributions can embed it with additional packs linked in.

fn main() -> anyhow::Result<()> {
    #[cfg(all(
        target_pointer_width = "64",
        any(target_os = "linux", target_os = "macos")
    ))]
    // SAFETY: this is the first action in the binary, before CLI configuration,
    // Tokio workers, extension registration or any SQLite file I/O can start.
    // Only daemon store claims need the observer, and a claimed pool refuses to
    // open without it, so an unavailable observer must not stop other commands.
    if let Err(error) = unsafe { khive_db::pool::initialize_claimed_file_observer() } {
        eprintln!("kkernel: claimed store verification unavailable: {error}");
    }
    kkernel::cli::cli_main()
}
