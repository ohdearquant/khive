//! `kkernel kg` — KG validation, review, init, hooks, fetch, import/export, status, and diff.

mod archive;
mod commit;
mod diff;
mod dispatch;
mod fetch;
mod init;
mod review;
mod status;
pub mod types;
mod update;
mod validate;

pub use dispatch::run_kg;
pub use types::{
    CommitArgs, CommitReport, DiffArgs, ExportArgs, FetchArgs, HookCommand, HookStatus, ImportArgs,
    ImportFormat, InitArgs, KgCommand, KgStatusReport, OutputFormat, ReviewArgs, ReviewCapability,
    ReviewChangeSet, ReviewFinding, ReviewGate, ReviewOperation, ReviewReport, ReviewTierSummary,
    ReviewValidationSummary, RuleResult, StatusArgs, UpdateArgs, ValidateArgs, ValidationReport,
    ValidationSummary, Violation,
};
