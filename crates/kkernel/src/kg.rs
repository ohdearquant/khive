//! `kkernel kg` — KG validation, init, and hook management (ADR-034, ADR-035).
//!
//! Implements:
//! - `kkernel kg validate` — structural + rule-pass validation
//! - `kkernel kg init`     — initialize `.khive/kg/` directory and `khive.toml`
//! - `kkernel kg hook`     — install / uninstall / status of the pre-commit hook

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

// ── Subcommand tree ────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum KgCommand {
    /// Validate the KG in `.khive/kg/` against structural and rule-pass checks.
    Validate(ValidateArgs),

    /// Initialize `.khive/kg/` and write `.khive/khive.toml` with defaults.
    Init(InitArgs),

    /// Manage the pre-commit hook for KG validation.
    #[command(subcommand)]
    Hook(HookCommand),
}

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

    /// Run ADR-020 built-in structural checks only; skip `rules.yaml`.
    #[arg(long)]
    pub no_rules: bool,
}

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

#[derive(Debug, Serialize)]
pub struct ValidationReport {
    pub rules: Vec<RuleResult>,
    pub summary: ValidationSummary,
}

#[derive(Debug, Serialize)]
pub struct RuleResult {
    pub id: String,
    pub severity: &'static str,
    pub passed: bool,
    pub violations: Vec<Violation>,
}

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

#[derive(Debug, Serialize)]
pub struct ValidationSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub entities: usize,
    pub edges: usize,
    pub passed: bool,
}

// ── Entry points ───────────────────────────────────────────────────────────────

pub fn run_kg(cmd: KgCommand) -> Result<()> {
    match cmd {
        KgCommand::Validate(args) => cmd_validate(args),
        KgCommand::Init(args) => cmd_init(args),
        KgCommand::Hook(h) => cmd_hook(h),
    }
}

// ── validate ──────────────────────────────────────────────────────────────────

fn cmd_validate(args: ValidateArgs) -> Result<()> {
    let kg_dir = args.repo.join(".khive/kg");
    if !kg_dir.exists() {
        bail!(
            "KG directory not found: {}. Run `kkernel kg init` first.",
            kg_dir.display()
        );
    }

    let entities_path = kg_dir.join("entities.ndjson");
    let edges_path = kg_dir.join("edges.ndjson");

    let entities = count_ndjson_lines(&entities_path).unwrap_or(0);
    let edges = count_ndjson_lines(&edges_path).unwrap_or(0);

    let rules_path = args.rules.unwrap_or_else(|| kg_dir.join("rules.toml"));

    // Run structural checks (ADR-020 built-ins).
    let mut rule_results: Vec<RuleResult> = structural_checks(&entities_path, &edges_path);

    // Run configurable rule pass unless --no-rules.
    if !args.no_rules && rules_path.exists() {
        let configurable = configurable_rule_checks(&entities_path, &edges_path, &rules_path)?;
        rule_results.extend(configurable);
    }

    let errors: usize = rule_results
        .iter()
        .filter(|r| r.severity == "error" && !r.passed)
        .count();
    let warnings: usize = rule_results
        .iter()
        .filter(|r| r.severity == "warning" && !r.passed)
        .count();
    let info: usize = rule_results
        .iter()
        .filter(|r| r.severity == "info" && !r.passed)
        .count();

    let passed = if args.strict {
        errors == 0 && warnings == 0
    } else {
        errors == 0
    };

    let summary = ValidationSummary {
        errors,
        warnings,
        info,
        entities,
        edges,
        passed,
    };

    let report = ValidationReport {
        rules: rule_results,
        summary,
    };

    match args.format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&report).expect("serialize ValidationReport");
            println!("{json}");
        }
        OutputFormat::Github => print_github_format(&report),
        OutputFormat::Text => print_text_format(&report, args.verbose, args.quiet),
    }

    if args.fix {
        apply_fixes(&args.repo)?;
    }

    if !report.summary.passed {
        std::process::exit(1);
    }
    Ok(())
}

fn count_ndjson_lines(path: &Path) -> Option<usize> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count())
}

fn structural_checks(entities_path: &Path, edges_path: &Path) -> Vec<RuleResult> {
    vec![
        check_no_duplicate_uuids(entities_path),
        check_sort_order(entities_path, edges_path),
        check_referential_integrity(entities_path, edges_path),
    ]
}

fn check_no_duplicate_uuids(entities_path: &Path) -> RuleResult {
    let mut seen = std::collections::HashSet::new();
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(entities_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                    if !seen.insert(id.to_string()) {
                        violations.push(Violation {
                            entity_id: Some(id.to_string()),
                            entity_name: v.get("name").and_then(|n| n.as_str()).map(str::to_string),
                            entity_kind: v.get("kind").and_then(|k| k.as_str()).map(str::to_string),
                            rule_id: "no-duplicate-uuids".into(),
                            severity: "error",
                            message: format!("Duplicate UUID: {id}"),
                            fixable: false,
                        });
                    }
                }
            }
        }
    }

    RuleResult {
        id: "no-duplicate-uuids".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

fn check_sort_order(entities_path: &Path, edges_path: &Path) -> RuleResult {
    let mut violations = Vec::new();

    // Check entities.ndjson sorted by UUID.
    if let Ok(content) = std::fs::read_to_string(entities_path) {
        let ids: Vec<String> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("id")?.as_str().map(str::to_string))
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        if ids != sorted {
            violations.push(Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: "sort-order".into(),
                severity: "warning",
                message: "entities.ndjson is not sorted by UUID; run `kkernel kg validate --fix`"
                    .into(),
                fixable: true,
            });
        }
    }

    // Check edges.ndjson sorted by (source, target, relation).
    if let Ok(content) = std::fs::read_to_string(edges_path) {
        let keys: Vec<(String, String, String)> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                let s = v.get("source_id")?.as_str()?.to_string();
                let t = v.get("target_id")?.as_str()?.to_string();
                let r = v.get("relation")?.as_str()?.to_string();
                Some((s, t, r))
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        if keys != sorted {
            violations.push(Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: "sort-order".into(),
                severity: "warning",
                message:
                    "edges.ndjson is not sorted by (source, target, relation); run `kkernel kg validate --fix`"
                        .into(),
                fixable: true,
            });
        }
    }

    RuleResult {
        id: "sort-order".into(),
        severity: "warning",
        passed: violations.is_empty(),
        violations,
    }
}

fn check_referential_integrity(entities_path: &Path, edges_path: &Path) -> RuleResult {
    let mut violations = Vec::new();

    let entity_ids: std::collections::HashSet<String> =
        if let Ok(content) = std::fs::read_to_string(entities_path) {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .ok()
                        .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                })
                .collect()
        } else {
            std::collections::HashSet::new()
        };

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                for field in &["source_id", "target_id"] {
                    if let Some(id) = v.get(field).and_then(|i| i.as_str()) {
                        if !entity_ids.contains(id) {
                            violations.push(Violation {
                                entity_id: Some(id.to_string()),
                                entity_name: None,
                                entity_kind: None,
                                rule_id: "referential-integrity".into(),
                                severity: "error",
                                message: format!(
                                    "Edge {} references unknown entity: {id}",
                                    if *field == "source_id" {
                                        "source"
                                    } else {
                                        "target"
                                    }
                                ),
                                fixable: false,
                            });
                        }
                    }
                }
            }
        }
    }

    RuleResult {
        id: "referential-integrity".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

// ── Configurable rule loader (issue #382, ADR-034) ────────────────────────────

/// A single configurable lint rule loaded from `rules.toml`.
///
/// Rules are checked against entities and edges in the KG. Each rule has:
/// - `id`        — unique identifier (used in `RuleResult.id`)
/// - `severity`  — "error" | "warning" | "info"
/// - `kind`      — "entity" | "edge" (what substrate the rule applies to)
/// - `condition` — a `key=value` equality predicate evaluated against each record
///
/// Example `rules.toml`:
/// ```toml
/// [[rules]]
/// id = "concept-must-have-description"
/// severity = "warning"
/// kind = "entity"
/// # field=value predicate: entity.kind must equal "concept"
/// condition = "kind=concept"
/// # require_field: the entity must have a non-empty value for this field
/// require_field = "description"
/// message = "Concept entities must have a description"
///
/// [[rules]]
/// id = "no-self-loops"
/// severity = "error"
/// kind = "edge"
/// condition = "source_id=target_id"
/// message = "Edges must not be self-loops"
/// ```
#[derive(Debug, Deserialize)]
struct RuleConfig {
    /// Unique rule ID.
    id: String,
    /// Severity: "error", "warning", or "info".
    #[serde(default = "default_severity")]
    severity: String,
    /// Substrate: "entity" or "edge".
    kind: String,
    /// `field=value` filter: only records where `record[field] == value` are checked.
    /// Use `source_id=target_id` (literal string) for the special self-loop sentinel.
    condition: Option<String>,
    /// When set, the rule fails for any record (passing the condition filter)
    /// that has an empty or absent value for this field.
    require_field: Option<String>,
    /// Human-readable violation message template. `{id}` is replaced with the
    /// record's `id` field when present.
    #[serde(default)]
    message: String,
}

fn default_severity() -> String {
    "warning".to_owned()
}

/// Top-level structure of a `rules.toml` file.
#[derive(Debug, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

fn severity_static(s: &str) -> &'static str {
    match s {
        "error" => "error",
        "info" => "info",
        _ => "warning",
    }
}

/// Load and evaluate configurable rules from a TOML rules file (issue #382).
///
/// Supports `.toml` files directly. For `.yaml` / `.yml` files the function
/// returns an error directing the user to use TOML format, as `serde_yaml` is
/// not a workspace dependency.
///
/// Rule evaluation:
/// 1. Parse the TOML file into a `RulesFile` containing a `Vec<RuleConfig>`.
/// 2. For each rule, load the appropriate NDJSON file (entities or edges).
/// 3. Apply the `condition` filter (field=value equality or self-loop sentinel).
/// 4. For `require_field` rules, collect violations where the field is absent/empty.
/// 5. Return one `RuleResult` per rule.
fn configurable_rule_checks(
    entities_path: &Path,
    edges_path: &Path,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    // Extension-based format routing.
    let ext = rules_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if matches!(ext, "yaml" | "yml") {
        bail!(
            "rules file {:?} uses YAML format which is not supported in this build. \
             Rename it to {}.toml and use TOML format instead.",
            rules_path,
            rules_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rules")
        );
    }

    let content = std::fs::read_to_string(rules_path)
        .with_context(|| format!("read rules file {}", rules_path.display()))?;

    let rules_file: RulesFile = toml::from_str(&content)
        .with_context(|| format!("parse rules TOML {}", rules_path.display()))?;

    let mut results = Vec::with_capacity(rules_file.rules.len());
    for rule in &rules_file.rules {
        let path = match rule.kind.as_str() {
            "entity" => entities_path,
            "edge" => edges_path,
            other => {
                // Unknown kind — emit an error rule result.
                results.push(RuleResult {
                    id: rule.id.clone(),
                    severity: "error",
                    passed: false,
                    violations: vec![Violation {
                        entity_id: None,
                        entity_name: None,
                        entity_kind: None,
                        rule_id: rule.id.clone(),
                        severity: "error",
                        message: format!(
                            "Rule {:?}: unknown kind {other:?}; must be \"entity\" or \"edge\"",
                            rule.id
                        ),
                        fixable: false,
                    }],
                });
                continue;
            }
        };

        let violations = evaluate_rule(rule, path);
        let sev = severity_static(&rule.severity);
        results.push(RuleResult {
            id: rule.id.clone(),
            severity: sev,
            passed: violations.is_empty(),
            violations,
        });
    }

    Ok(results)
}

/// Evaluate a single `RuleConfig` against an NDJSON file.
fn evaluate_rule(rule: &RuleConfig, path: &Path) -> Vec<Violation> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    // Parse the condition: "field=value" or the sentinel "source_id=target_id".
    let condition: Option<(&str, &str)> = rule.condition.as_deref().and_then(|c| c.split_once('='));

    let sev = severity_static(&rule.severity);
    let mut violations = Vec::new();

    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(val) => val,
            Err(_) => continue,
        };

        // Apply condition filter.
        if let Some((field, expected)) = condition {
            // Special sentinel: "source_id=target_id" means check for self-loops.
            if field == "source_id" && expected == "target_id" {
                let src = v.get("source_id").and_then(|s| s.as_str()).unwrap_or("");
                let tgt = v.get("target_id").and_then(|s| s.as_str()).unwrap_or("");
                if src == tgt {
                    // Self-loop detected — always a violation for this rule type.
                    violations.push(Violation {
                        entity_id: Some(src.to_owned()),
                        entity_name: None,
                        entity_kind: v
                            .get("relation")
                            .and_then(|r| r.as_str())
                            .map(str::to_owned),
                        rule_id: rule.id.clone(),
                        severity: sev,
                        message: rule.message.replace("{id}", src),
                        fixable: false,
                    });
                }
                continue;
            }

            // Normal field=value filter: only proceed for records where field matches.
            let actual = v.get(field).and_then(|f| f.as_str()).unwrap_or("");
            if actual != expected {
                continue;
            }
        }

        // require_field check: the record must have a non-empty value for this field.
        if let Some(req) = rule.require_field.as_deref() {
            let val = v.get(req).and_then(|f| f.as_str()).unwrap_or("");
            if val.is_empty() {
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                violations.push(Violation {
                    entity_id: if id.is_empty() {
                        None
                    } else {
                        Some(id.to_owned())
                    },
                    entity_name: v.get("name").and_then(|n| n.as_str()).map(str::to_owned),
                    entity_kind: v.get("kind").and_then(|k| k.as_str()).map(str::to_owned),
                    rule_id: rule.id.clone(),
                    severity: sev,
                    message: rule.message.replace("{id}", id),
                    fixable: false,
                });
            }
        }
    }

    violations
}

fn apply_fixes(repo: &Path) -> Result<()> {
    let kg_dir = repo.join(".khive/kg");
    fix_sort_order(&kg_dir.join("entities.ndjson"), "id")?;
    fix_sort_order_edges(&kg_dir.join("edges.ndjson"))?;
    eprintln!("~ sort-order: applied fix to entities.ndjson and edges.ndjson");
    Ok(())
}

fn fix_sort_order(path: &Path, sort_key: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    lines.sort_by(|a, b| {
        let ak = a.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        let bk = b.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        ak.cmp(bk)
    });
    let out: String = lines
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, out + "\n").with_context(|| format!("write {}", path.display()))
}

fn fix_sort_order_edges(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    lines.sort_by(|a, b| {
        let ak = (
            a.get("source_id").and_then(|v| v.as_str()).unwrap_or(""),
            a.get("target_id").and_then(|v| v.as_str()).unwrap_or(""),
            a.get("relation").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let bk = (
            b.get("source_id").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("target_id").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("relation").and_then(|v| v.as_str()).unwrap_or(""),
        );
        ak.cmp(&bk)
    });
    let out: String = lines
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, out + "\n").with_context(|| format!("write {}", path.display()))
}

fn print_text_format(report: &ValidationReport, verbose: bool, quiet: bool) {
    if !quiet {
        for r in &report.rules {
            let symbol = if r.passed {
                "\u{2713}"
            } else if r.severity == "error" {
                "\u{2717}"
            } else {
                "\u{26a0}"
            };
            if r.violations.is_empty() {
                println!("  {symbol} {}", r.id);
            } else {
                println!("  {symbol} {}: {} violation(s)", r.id, r.violations.len());
                let shown = if verbose {
                    r.violations.len()
                } else {
                    2.min(r.violations.len())
                };
                for v in &r.violations[..shown] {
                    println!("    - {}", v.message);
                }
                if !verbose && r.violations.len() > 2 {
                    println!("    + {} more (run with --verbose)", r.violations.len() - 2);
                }
            }
        }
    }
    let s = &report.summary;
    println!(
        "\nSummary: {} error(s), {} warning(s), {} entities, {} edges",
        s.errors, s.warnings, s.entities, s.edges
    );
}

fn print_github_format(report: &ValidationReport) {
    for r in &report.rules {
        for v in &r.violations {
            let level = if r.severity == "error" {
                "error"
            } else {
                "warning"
            };
            println!("::{level} ::{}", v.message);
        }
    }
}

// ── init ──────────────────────────────────────────────────────────────────────

const DEFAULT_KHIVE_TOML: &str = r#"# .khive/khive.toml — project KG configuration (ADR-035)
# Committed to git. All collaborators use these settings.

[[backends]]
name = "main"
path = "~/.khive/khive.db"
cache_mb = 256
journal_mode = "wal"

[[engines]]
name = "mE5-small"
dim = 384
weight = 1.0

[packs.kg]
backend = "main"
engines = ["mE5-small"]

[packs.memory]
backend = "main"
engines = ["mE5-small"]

[packs.gtd]
backend = "main"
engines = []

[embed]
model = "mE5-small"
dimensions = 384
auto_embed = true
batch_size = 64

[embed.fields]
include = ["name", "description"]

[schema]
strict = true
"#;

const GITIGNORE_CONTENT: &str = "*\n!.gitignore\n!kg/\n!kg/**\n!khive.toml\n";

const PRE_COMMIT_HOOK: &str = r#"#!/usr/bin/env bash
# .khive/kg/hooks/pre-commit
# Generated by kkernel kg init.
# Runs KG validation on staged NDJSON files.
# Bypass with: git commit --no-verify

set -euo pipefail

staged=$(git diff --cached --name-only \
  | grep -E '^\.khive/kg/(entities|edges)\.ndjson$' || true)
if [ -z "$staged" ]; then
  exit 0
fi

kkernel kg validate
"#;

const CI_WORKFLOW: &str = r#"name: KG Validate
on:
  push:
    paths: [".khive/kg/**"]
  pull_request:
    paths: [".khive/kg/**"]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Validate KG
        run: kkernel kg validate --format github
"#;

fn cmd_init(args: InitArgs) -> Result<()> {
    if args.add_hooks {
        return hook_install(&args.repo);
    }

    let khive_dir = args.repo.join(".khive");
    let kg_dir = khive_dir.join("kg");
    let hooks_dir = kg_dir.join("hooks");

    std::fs::create_dir_all(&kg_dir).with_context(|| format!("create {}", kg_dir.display()))?;
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("create {}", hooks_dir.display()))?;

    // Write entities.ndjson and edges.ndjson if absent.
    for name in &["entities.ndjson", "edges.ndjson"] {
        let path = kg_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, "").with_context(|| format!("create {}", path.display()))?;
        }
    }

    // Write .khive/.gitignore.
    let gitignore = khive_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE_CONTENT)
            .with_context(|| format!("write {}", gitignore.display()))?;
    }

    // Write .khive/khive.toml (do not overwrite).
    let toml_path = khive_dir.join("khive.toml");
    if !toml_path.exists() {
        std::fs::write(&toml_path, DEFAULT_KHIVE_TOML)
            .with_context(|| format!("write {}", toml_path.display()))?;
        println!("  Initialized {}", toml_path.display());
    } else {
        println!("  Skipped {} (already exists)", toml_path.display());
    }

    // Write pre-commit hook script.
    let hook_script = hooks_dir.join("pre-commit");
    if !hook_script.exists() {
        std::fs::write(&hook_script, PRE_COMMIT_HOOK)
            .with_context(|| format!("write {}", hook_script.display()))?;
        // Make hook script executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_script, perms)?;
        }
    }

    println!("  Initialized .khive/kg/ (entities.ndjson, edges.ndjson, hooks/pre-commit)");

    if args.ci {
        let workflow_dir = args.repo.join(".github/workflows");
        std::fs::create_dir_all(&workflow_dir)
            .with_context(|| format!("create {}", workflow_dir.display()))?;
        let workflow_path = workflow_dir.join("kg-validate.yml");
        if !workflow_path.exists() {
            std::fs::write(&workflow_path, CI_WORKFLOW)
                .with_context(|| format!("write {}", workflow_path.display()))?;
            println!("  Generated {}", workflow_path.display());
        }
    }

    Ok(())
}

// ── hook ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HookStatus {
    pub symlink_exists: bool,
    pub symlink_target: Option<String>,
    pub target_valid: bool,
}

fn cmd_hook(cmd: HookCommand) -> Result<()> {
    match cmd {
        HookCommand::Install { repo } => hook_install(&repo),
        HookCommand::Uninstall { repo } => hook_uninstall(&repo),
        HookCommand::Status { repo } => hook_status(&repo),
    }
}

fn hook_install(repo: &Path) -> Result<()> {
    let hook_script = repo.join(".khive/kg/hooks/pre-commit");
    let git_hook = repo.join(".git/hooks/pre-commit");

    if !hook_script.exists() {
        bail!(
            "Hook script not found: {}. Run `kkernel kg init` first.",
            hook_script.display()
        );
    }

    if let Some(parent) = git_hook.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    if git_hook.exists() || git_hook.is_symlink() {
        std::fs::remove_file(&git_hook)
            .with_context(|| format!("remove existing {}", git_hook.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Use the absolute path for the symlink target.
        let absolute_script = hook_script
            .canonicalize()
            .unwrap_or_else(|_| hook_script.clone());
        symlink(&absolute_script, &git_hook)
            .with_context(|| format!("create symlink {}", git_hook.display()))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::copy(&hook_script, &git_hook)
            .with_context(|| format!("copy hook to {}", git_hook.display()))?;
    }

    println!(
        "  Installed: {} -> {}",
        git_hook.display(),
        hook_script.display()
    );
    Ok(())
}

fn hook_uninstall(repo: &Path) -> Result<()> {
    let git_hook = repo.join(".git/hooks/pre-commit");
    if git_hook.exists() || git_hook.is_symlink() {
        std::fs::remove_file(&git_hook)
            .with_context(|| format!("remove {}", git_hook.display()))?;
        println!("  Uninstalled: {}", git_hook.display());
    } else {
        println!("  No hook installed at {}", git_hook.display());
    }
    Ok(())
}

fn hook_status(repo: &Path) -> Result<()> {
    let git_hook = repo.join(".git/hooks/pre-commit");
    let symlink_exists = git_hook.exists() || git_hook.is_symlink();
    let symlink_target = if symlink_exists {
        std::fs::read_link(&git_hook)
            .ok()
            .map(|p| p.display().to_string())
    } else {
        None
    };
    let target_valid = symlink_target
        .as_deref()
        .map(|t| Path::new(t).exists())
        .unwrap_or(false);

    let status = HookStatus {
        symlink_exists,
        symlink_target,
        target_valid,
    };
    let json = serde_json::to_string(&status).expect("serialize HookStatus");
    println!("{json}");
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_kg_dir(tmp: &TempDir) -> PathBuf {
        let kg_dir = tmp.path().join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        kg_dir
    }

    fn write_entities(kg_dir: &Path, entities: &[(&str, &str, &str)]) {
        let content: String = entities
            .iter()
            .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
    }

    fn write_edges(kg_dir: &Path, edges: &[(&str, &str, &str)]) {
        let content: String = edges
            .iter()
            .map(|(src, tgt, rel)| {
                format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
    }

    #[test]
    fn duplicate_uuid_detected() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A-dup"),
            ],
        );
        let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
        assert!(!result.passed, "duplicate UUID should fail");
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn no_duplicates_passes() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
        assert!(result.passed);
    }

    #[test]
    fn referential_integrity_catches_missing_target() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let result = check_referential_integrity(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn init_creates_expected_files() {
        let tmp = TempDir::new().unwrap();
        let args = InitArgs {
            repo: tmp.path().to_path_buf(),
            ci: false,
            add_hooks: false,
        };
        cmd_init(args).unwrap();

        assert!(tmp.path().join(".khive/kg/entities.ndjson").exists());
        assert!(tmp.path().join(".khive/kg/edges.ndjson").exists());
        assert!(tmp.path().join(".khive/khive.toml").exists());
        assert!(tmp.path().join(".khive/kg/hooks/pre-commit").exists());
    }

    #[test]
    fn init_does_not_overwrite_existing_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".khive")).unwrap();
        let toml_path = tmp.path().join(".khive/khive.toml");
        std::fs::write(&toml_path, "# custom\n").unwrap();

        let args = InitArgs {
            repo: tmp.path().to_path_buf(),
            ci: false,
            add_hooks: false,
        };
        cmd_init(args).unwrap();

        let content = std::fs::read_to_string(&toml_path).unwrap();
        assert_eq!(content, "# custom\n", "should not overwrite existing toml");
    }

    // ── configurable_rule_checks (issue #382) ─────────────────────────────────

    #[test]
    fn configurable_rule_checks_empty_rules_file_returns_no_results() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_path = tmp.path().join("rules.toml");
        // Valid TOML with empty rules array.
        std::fs::write(&rules_path, "rules = []\n").unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert!(results.is_empty(), "no rules → no results");
    }

    #[test]
    fn configurable_rule_checks_require_field_detects_missing_description() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);

        // One entity with description, one without.
        let entities = r#"{"id":"aaa1","kind":"concept","name":"A","description":"has one"}
{"id":"aaa2","kind":"concept","name":"B"}
"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_toml = r#"
[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.id, "concept-must-have-description");
        assert!(
            !r.passed,
            "rule should fail when a concept lacks description"
        );
        assert_eq!(r.violations.len(), 1);
        assert_eq!(r.violations[0].entity_id.as_deref(), Some("aaa2"));
    }

    #[test]
    fn configurable_rule_checks_self_loop_sentinel_detects_loop() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);

        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        // One self-loop edge, one valid edge.
        let edges = r#"{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"aaaaaaaa-0000-0000-0000-000000000001","relation":"extends"}
{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
"#;
        std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();

        let rules_toml = r#"
[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
condition = "source_id=target_id"
message = "Self-loop detected on {id}"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(!r.passed);
        assert_eq!(r.violations.len(), 1, "exactly one self-loop");
    }

    #[test]
    fn configurable_rule_checks_yaml_extension_returns_error() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_path = tmp.path().join("rules.yaml");
        std::fs::write(&rules_path, "rules: []\n").unwrap();

        let result = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        );
        assert!(result.is_err(), "YAML extension must return an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("YAML") || msg.contains("toml"),
            "error message should mention TOML: {msg}"
        );
    }

    #[test]
    fn configurable_rule_checks_unknown_kind_produces_error_result() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_toml = r#"
[[rules]]
id = "bad-kind"
severity = "error"
kind = "note"
condition = "kind=concept"
require_field = "description"
message = "bad"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert_eq!(results[0].severity, "error");
    }

    #[test]
    fn sort_order_fix_sorts_entities() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        // Write out-of-order entities.
        write_entities(
            &kg_dir,
            &[
                ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        fix_sort_order(&kg_dir.join("entities.ndjson"), "id").unwrap();
        let result = check_sort_order(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(result.passed, "sort-order should pass after fix");
    }
}
