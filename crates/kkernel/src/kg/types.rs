//! CLI argument types and output types for `kkernel kg` subcommands.

use std::path::PathBuf;

use clap::Subcommand;
use serde::Serialize;

// ── Subcommand tree ────────────────────────────────────────────────────────────

/// Subcommands for `kkernel kg` — KG validation, sync, import/export, and hook management.
#[derive(Subcommand, Debug)]
pub enum KgCommand {
    /// Validate the KG in `.khive/kg/` against structural and rule-pass checks.
    Validate(ValidateArgs),

    /// Initialize `.khive/kg/` and write `.khive/khive.toml` with defaults.
    Init(InitArgs),

    /// Fetch a remote KG archive into `.khive/kg/remotes/<remote>/`.
    ///
    /// `sync` is a visible alias so `kkernel kg sync --repin <remote>`
    /// reaches the same implementation.
    #[command(visible_alias = "sync")]
    Fetch(FetchArgs),

    /// Export a namespace-scoped KG archive from a SQLite DB.
    Export(ExportArgs),

    /// Import a KG archive or flat adapter records into a SQLite DB.
    Import(ImportArgs),

    /// Compare DB state against `.khive/kg/{entities,edges}.ndjson`.
    Status(StatusArgs),

    /// Manage the pre-commit hook for KG validation.
    #[command(subcommand)]
    Hook(HookCommand),
}

/// CLI arguments for `kkernel kg validate`.
#[derive(clap::Parser, Debug)]
pub struct ValidateArgs {
    /// Repository root containing `.khive/kg/`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Apply fixable rules and report what changed.
    #[arg(long)]
    pub fix: bool,

    /// Treat warnings as errors; exit 1 when warnings > 0.
    #[arg(long)]
    pub strict: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    pub format: OutputFormat,

    /// Show all violations (default: cap at 2 then `+ N more`).
    #[arg(long)]
    pub verbose: bool,

    /// Print summary line only.
    #[arg(long)]
    pub quiet: bool,

    /// Override the default `.khive/kg/rules.toml` path.
    #[arg(long)]
    pub rules: Option<PathBuf>,

    /// Run built-in structural checks only; skip `rules.yaml`.
    #[arg(long)]
    pub no_rules: bool,
}

/// Supported output formats for validation reports.
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
    Github,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Github => write!(f, "github"),
        }
    }
}

/// CLI arguments for `kkernel kg init`.
#[derive(clap::Parser, Debug)]
pub struct InitArgs {
    /// Repository root to initialize.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Also generate `.github/workflows/kg-validate.yml`.
    #[arg(long)]
    pub ci: bool,

    /// Install the pre-commit hook without reinitializing.
    #[arg(long)]
    pub add_hooks: bool,
}

/// CLI arguments for `kkernel kg fetch` (alias: `sync`).
#[derive(clap::Parser, Debug)]
pub struct FetchArgs {
    /// Remote name, used for cache path `.khive/kg/remotes/<remote>`.
    pub remote: String,

    /// Repository root that owns `.khive/`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Git remote URL.
    #[arg(long)]
    pub url: String,

    /// Git ref to fetch.
    #[arg(long = "ref", default_value = "main")]
    pub git_ref: String,

    /// Namespace to assign to fetched records.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Optional content hash pin: sha256:<64 lowercase hex chars>.
    #[arg(long)]
    pub pin: Option<String>,

    /// Accept the fetched content hash and return it for schema.yaml repinning.
    #[arg(long)]
    pub repin: bool,
}

/// CLI arguments for `kkernel kg export`.
#[derive(clap::Parser, Debug)]
pub struct ExportArgs {
    /// Output archive JSON path.
    pub output: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace to export.
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

/// CLI arguments for `kkernel kg import`.
#[derive(clap::Parser, Debug)]
pub struct ImportArgs {
    /// Source archive or adapter input file.
    pub source: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace for imported records.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Import format. Default is the `KgArchive` JSON envelope.
    #[arg(long, value_enum, default_value_t = ImportFormat::Archive)]
    pub format: ImportFormat,

    /// Print adapter warnings to stderr.
    #[arg(long)]
    pub verbose: bool,
}

/// Supported input formats for `kkernel kg import`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Archive,
    Json,
    Ndjson,
}

/// CLI arguments for `kkernel kg status`.
#[derive(clap::Parser, Debug)]
pub struct StatusArgs {
    /// Repository root containing `.khive/kg/{entities,edges}.ndjson`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace to compare.
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

/// Subcommands for `kkernel kg hook` — install, remove, and check the pre-commit hook.
#[derive(Subcommand, Debug)]
pub enum HookCommand {
    /// Create `.git/hooks/pre-commit` symlink pointing to the tracked hook.
    Install {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove the `.git/hooks/pre-commit` symlink.
    Uninstall {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Show whether the hook symlink exists and points to a valid target.
    Status {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

// ── Output types ───────────────────────────────────────────────────────────────

/// Hash-based comparison result between the DB state and on-disk NDJSON files.
#[derive(Debug, Serialize)]
pub struct KgStatusReport {
    pub db_hash: String,
    pub ndjson_hash: String,
    pub clean: bool,
}

/// Aggregate result of a `kkernel kg validate` run.
#[derive(Debug, Serialize)]
pub struct ValidationReport {
    pub rules: Vec<RuleResult>,
    pub summary: ValidationSummary,
}

/// Result for a single validation rule in a `kkernel kg validate` run.
#[derive(Debug, Serialize)]
pub struct RuleResult {
    pub id: String,
    pub severity: &'static str,
    pub passed: bool,
    pub violations: Vec<Violation>,
}

/// A single rule violation with location metadata and a fixability flag.
#[derive(Debug, Serialize)]
pub struct Violation {
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub entity_kind: Option<String>,
    pub rule_id: String,
    pub severity: &'static str,
    pub message: String,
    pub fixable: bool,
}

/// Aggregate counts from a `kkernel kg validate` run.
#[derive(Debug, Serialize)]
pub struct ValidationSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub entities: usize,
    pub edges: usize,
    pub passed: bool,
}

/// Status of the pre-commit hook symlink for KG validation.
#[derive(Debug, Serialize)]
pub struct HookStatus {
    pub symlink_exists: bool,
    pub symlink_target: Option<String>,
    pub target_valid: bool,
}
