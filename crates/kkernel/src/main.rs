//! `kkernel` binary shell — the CLI itself lives in `kkernel::cli` so
//! downstream distributions can embed it with additional packs linked in.

fn main() -> anyhow::Result<()> {
    kkernel::cli::cli_main()
}
