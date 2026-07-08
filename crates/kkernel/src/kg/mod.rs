//! `kkernel kg` — KG validation, init, hook management, fetch, export, import, and status.

mod archive;
mod commit;
mod dispatch;
mod fetch;
mod init;
mod status;
pub mod types;
mod validate;

pub use dispatch::run_kg;
pub use types::{
    CommitArgs, CommitReport, ExportArgs, FetchArgs, HookCommand, HookStatus, ImportArgs,
    ImportFormat, InitArgs, KgCommand, KgStatusReport, OutputFormat, RuleResult, StatusArgs,
    ValidateArgs, ValidationReport, ValidationSummary, Violation,
};
