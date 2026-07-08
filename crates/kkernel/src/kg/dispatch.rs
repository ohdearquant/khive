//! `run_kg` — dispatch entry point for `kkernel kg` subcommands.

use anyhow::Result;

use super::archive;
use super::commit;
use super::fetch;
use super::init;
use super::status;
use super::types::KgCommand;
use super::validate;

/// Dispatch `kkernel kg` subcommands to their implementations.
pub async fn run_kg(cmd: KgCommand) -> Result<()> {
    match cmd {
        KgCommand::Validate(args) => validate::cmd_validate(args),
        KgCommand::Init(args) => init::cmd_init(args),
        KgCommand::Fetch(args) => fetch::cmd_fetch(args).await,
        KgCommand::Export(args) => archive::cmd_export(args).await,
        KgCommand::Import(args) => archive::cmd_import(args).await,
        KgCommand::Status(args) => status::cmd_status(args).await,
        KgCommand::Hook(h) => init::cmd_hook(h),
        KgCommand::Commit(args) => commit::cmd_commit(args),
    }
}
