//! `kkernel kg validate` — structural and configurable rule-pass validation.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use khive_rule_evaluator::{edge_source_id, edge_target_id, record_prefix};
use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::EdgeRelation;
use khive_types::EdgeEndpointRule;

use super::types::{
    OutputFormat, RuleResult, ValidateArgs, ValidationReport, ValidationSummary, Violation,
};

mod fixes;
mod render;

use fixes::apply_fixes;
use render::render_validation;

#[cfg(test)]
use fixes::fix_sort_order;

/// Taxonomy sets derived from the loaded pack registry; `pub(super)` so
/// `kg::commit` (ADR-102) can reuse them. See
/// `crates/kkernel/docs/kg-rules.md#build_taxonomy--strict-actor-mode-exemption`.
pub(super) struct KgTaxonomy {
    pub(super) entity_kinds: HashSet<String>,
    pub(super) note_kinds: HashSet<String>,
}

/// Build the merged entity-kind and note-kind sets from all registered packs;
/// no DB is opened, only pack metadata. Deliberately does NOT call
/// `enforce_strict_actor_mode` — see
/// `crates/kkernel/docs/kg-rules.md#build_taxonomy--strict-actor-mode-exemption`
/// for why this metadata-only path is exempt from that comm-boundary guard.
pub(super) fn build_taxonomy() -> Result<KgTaxonomy> {
    // Pack-registry metadata has no file-backed writer; ADR-194 allows this explicit opt-out.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("kkernel-validate")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        ..RuntimeConfig::default()
    }
    .for_metadata_registry();
    let runtime = KhiveRuntime::new(config).context("building taxonomy registry")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build_metadata().context("building pack metadata")?;

    let entity_kinds = registry
        .all_entity_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    let note_kinds = registry
        .all_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();

    Ok(KgTaxonomy {
        entity_kinds,
        note_kinds,
    })
}

/// Build the merged pack-declared edge endpoint rule set (ADR-017 `EDGE_RULES`).
///
/// Same no-DB registry construction as [`build_taxonomy`] (kept as a
/// separate function rather than folding into `KgTaxonomy` so existing
/// taxonomy-only callers and their test fixtures are unaffected). The
/// returned rules are `VerbRegistry::all_edge_rules()` — the exact set every
/// pack contributes via `Pack::EDGE_RULES` — so the `edge-endpoint-types`
/// rule class consults the same live data the `link`/`update` verbs enforce,
/// never a hand-copied snapshot.
fn build_pack_edge_rules() -> Result<Vec<EdgeEndpointRule>> {
    // Pack-registry metadata has no file-backed writer; ADR-194 allows this explicit opt-out.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("kkernel-validate")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        ..RuntimeConfig::default()
    }
    .for_metadata_registry();
    let runtime = KhiveRuntime::new(config).context("building edge-rules registry")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build_metadata().context("building pack metadata")?;
    Ok(registry.all_edge_rules())
}

// ADR-034 reserves exit 2 for TOML parse failures and unsupported rules formats.
// Preserve each diagnostic while distinguishing it from other loader failures.
#[derive(Debug)]
struct RulesSyntaxOrFormatError(String);

impl std::fmt::Display for RulesSyntaxOrFormatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug)]
struct SortFix {
    file: &'static str,
    moved: usize,
}

#[derive(Debug, Default)]
struct FixedCounts {
    errors: usize,
    warnings: usize,
    info: usize,
}

#[derive(Debug, Default)]
struct FixSummary {
    files: Vec<SortFix>,
    fixed: FixedCounts,
}

#[derive(Debug)]
struct ValidationRun {
    report: ValidationReport,
    fixes: Option<FixSummary>,
}

pub(super) fn cmd_validate(args: ValidateArgs) -> Result<()> {
    let run = match run_validation(&args) {
        Ok(run) => run,
        Err(error) if error.is::<RulesSyntaxOrFormatError>() => {
            eprintln!("Error: {error:?}");
            std::process::exit(2);
        }
        Err(error) => return Err(error),
    };
    render_validation(
        &run,
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )?;
    if !run.report.summary.passed {
        std::process::exit(1);
    }
    Ok(())
}

/// The command and its tests share the complete check/fix/recheck pipeline.
fn run_validation(args: &ValidateArgs) -> Result<ValidationRun> {
    let kg_dir = args.repo.join(".khive/kg");
    if !kg_dir.exists() {
        bail!(
            "KG directory not found: {}. Run `kkernel kg init` first.",
            kg_dir.display()
        );
    }
    let entities_path = kg_dir.join("entities.ndjson");
    let edges_path = kg_dir.join("edges.ndjson");
    let notes_path = kg_dir.join("notes.ndjson");
    let rules_path = args
        .rules
        .clone()
        .unwrap_or_else(|| kg_dir.join("rules.toml"));
    let taxonomy = build_taxonomy()?;
    let mut report = collect_validation_report(
        args,
        &entities_path,
        &edges_path,
        &notes_path,
        &rules_path,
        &taxonomy,
    )?;
    let mut fixes = args.fix.then(FixSummary::default);
    if args.fix
        && report.rules.iter().any(|rule| {
            rule.id == "sort-order"
                && !rule.passed
                && rule.violations.iter().any(|violation| violation.fixable)
        })
    {
        let files = apply_fixes(&args.repo)?;
        let final_report = collect_validation_report(
            args,
            &entities_path,
            &edges_path,
            &notes_path,
            &rules_path,
            &taxonomy,
        )?;
        let mut fixed = FixedCounts::default();
        for (before, after) in report.rules.iter().zip(&final_report.rules) {
            if before.id == "sort-order"
                && before.violations.iter().any(|violation| violation.fixable)
                && before.id == after.id
                && before.severity == after.severity
                && !before.passed
                && after.passed
            {
                match before.severity {
                    "error" => fixed.errors += 1,
                    "warning" => fixed.warnings += 1,
                    "info" => fixed.info += 1,
                    _ => {}
                }
            }
        }
        fixes = Some(FixSummary { files, fixed });
        report = final_report;
    }
    Ok(ValidationRun { report, fixes })
}

fn collect_validation_report(
    args: &ValidateArgs,
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    rules_path: &Path,
    taxonomy: &KgTaxonomy,
) -> Result<ValidationReport> {
    let entities = count_ndjson_lines(entities_path).unwrap_or(0);
    let edges = count_ndjson_lines(edges_path).unwrap_or(0);
    let notes = count_ndjson_lines(notes_path).unwrap_or(0);

    let mut rule_results: Vec<RuleResult> =
        structural_checks(entities_path, edges_path, notes_path, taxonomy);

    if !args.no_rules && rules_path.exists() {
        let configurable =
            configurable_rule_checks(entities_path, edges_path, notes_path, rules_path)?;
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
        empty: entities == 0 && edges == 0 && notes == 0,
        passed,
    };

    Ok(ValidationReport {
        rules: rule_results,
        summary,
    })
}

fn count_ndjson_lines(path: &Path) -> Option<usize> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count())
}

fn structural_checks(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    taxonomy: &KgTaxonomy,
) -> Vec<RuleResult> {
    let mut results = vec![
        check_required_input_files(entities_path, edges_path, notes_path),
        check_schema_compliance(entities_path, edges_path, notes_path),
        check_no_duplicate_uuids(entities_path),
        check_sort_order(entities_path, edges_path),
        check_referential_integrity(entities_path, notes_path, edges_path),
        check_valid_entity_kinds(entities_path, &taxonomy.entity_kinds),
        check_valid_edge_relations(edges_path),
    ];
    if notes_path.exists() {
        results.push(check_valid_note_kinds(notes_path, &taxonomy.note_kinds));
    }
    results
}

fn check_required_input_files(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
) -> RuleResult {
    let mut violations = Vec::new();
    for path in [entities_path, edges_path] {
        if let Err(error) = std::fs::read_to_string(path) {
            violations.push(Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: "required-input-files".into(),
                severity: "error",
                message: format!("cannot read mandatory input {}: {error}", path.display()),
                fixable: false,
            });
        }
    }

    // notes.ndjson is optional only when absent. Once a path is present it is
    // part of the validation input and must be readable UTF-8 just like the
    // mandatory files. Use symlink_metadata so a dangling symlink is treated
    // as a present-but-unreadable input rather than as an absent optional file.
    match std::fs::symlink_metadata(notes_path) {
        Ok(_) => {
            if let Err(error) = std::fs::read_to_string(notes_path) {
                violations.push(Violation {
                    entity_id: None,
                    entity_name: None,
                    entity_kind: None,
                    rule_id: "required-input-files".into(),
                    severity: "error",
                    message: format!(
                        "cannot read optional input {} when present: {error}",
                        notes_path.display()
                    ),
                    fixable: false,
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => violations.push(Violation {
            entity_id: None,
            entity_name: None,
            entity_kind: None,
            rule_id: "required-input-files".into(),
            severity: "error",
            message: format!(
                "cannot inspect optional input {}: {error}",
                notes_path.display()
            ),
            fixable: false,
        }),
    }

    RuleResult {
        id: "required-input-files".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

fn schema_violation(file: &str, line_no: usize, message: impl std::fmt::Display) -> Violation {
    Violation {
        entity_id: None,
        entity_name: None,
        entity_kind: None,
        rule_id: "schema-compliance".into(),
        severity: "error",
        message: format!("{file} line {line_no}: {message}"),
        fixable: false,
    }
}

/// Fail-closed schema-compliance check: every non-empty NDJSON line in
/// entities.ndjson, edges.ndjson, and (if present) notes.ndjson must parse as
/// JSON and carry the minimal required fields for its record type. Unlike the
/// other structural checks, malformed lines here are reported as violations
/// instead of being silently skipped, so corrupt NDJSON cannot pass `kg
/// validate` only to fail later in `kkernel sync` / `kg import`.
fn check_schema_compliance(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
) -> RuleResult {
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(entities_path) {
        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let line_no = idx + 1;
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    let missing: Vec<&str> = ["id", "kind", "name"]
                        .into_iter()
                        .filter(|f| v.get(f).and_then(|x| x.as_str()).is_none())
                        .collect();
                    if !missing.is_empty() {
                        violations.push(schema_violation(
                            "entities.ndjson",
                            line_no,
                            format!("missing required field(s): {}", missing.join(", ")),
                        ));
                    }
                }
                Err(e) => {
                    violations.push(schema_violation(
                        "entities.ndjson",
                        line_no,
                        format!("invalid JSON: {e}"),
                    ));
                }
            }
        }
    }

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let line_no = idx + 1;
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    let mut missing: Vec<&str> = Vec::new();
                    if edge_source_id(&v).is_none() {
                        missing.push("source (or source_id)");
                    }
                    if edge_target_id(&v).is_none() {
                        missing.push("target (or target_id)");
                    }
                    if v.get("relation").and_then(|x| x.as_str()).is_none() {
                        missing.push("relation");
                    }
                    if !missing.is_empty() {
                        violations.push(schema_violation(
                            "edges.ndjson",
                            line_no,
                            format!("missing required field(s): {}", missing.join(", ")),
                        ));
                    }
                }
                Err(e) => {
                    violations.push(schema_violation(
                        "edges.ndjson",
                        line_no,
                        format!("invalid JSON: {e}"),
                    ));
                }
            }
        }
    }

    // notes.ndjson is optional: an absent file is fine, a present-but-malformed
    // file is not.
    if notes_path.exists() {
        if let Ok(content) = std::fs::read_to_string(notes_path) {
            for (idx, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let line_no = idx + 1;
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => {
                        let missing: Vec<&str> = ["id", "kind"]
                            .into_iter()
                            .filter(|f| v.get(f).and_then(|x| x.as_str()).is_none())
                            .collect();
                        if !missing.is_empty() {
                            violations.push(schema_violation(
                                "notes.ndjson",
                                line_no,
                                format!("missing required field(s): {}", missing.join(", ")),
                            ));
                        }
                    }
                    Err(e) => {
                        violations.push(schema_violation(
                            "notes.ndjson",
                            line_no,
                            format!("invalid JSON: {e}"),
                        ));
                    }
                }
            }
        }
    }

    RuleResult {
        id: "schema-compliance".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

fn check_valid_kinds(
    records_path: &Path,
    valid_kinds: &HashSet<String>,
    rule_id: &str,
    kind_label: &str,
) -> RuleResult {
    let valid_list = {
        let mut v: Vec<&str> = valid_kinds.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.join(" | ")
    };
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(records_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(kind_str) = v.get("kind").and_then(|k| k.as_str()) {
                    if !valid_kinds.contains(kind_str) {
                        let id = v
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = v.get("name").and_then(|n| n.as_str()).map(str::to_string);
                        let prefix = record_prefix(
                            if id.is_empty() { None } else { Some(&id) },
                            name.as_deref(),
                        );
                        violations.push(Violation {
                            entity_id: if id.is_empty() { None } else { Some(id) },
                            entity_name: name,
                            entity_kind: Some(kind_str.to_string()),
                            rule_id: rule_id.into(),
                            severity: "error",
                            message: format!(
                                "{prefix}unknown {kind_label}: {kind_str:?}. \
                                 Valid: {valid_list}"
                            ),
                            fixable: false,
                        });
                    }
                }
            }
        }
    }

    RuleResult {
        id: rule_id.into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

/// `pub(super)`: reused by `kg::commit` (ADR-102) to check `create`-op entity
/// kinds against the same pack-declared taxonomy `kg validate` enforces.
pub(super) fn check_valid_entity_kinds(
    entities_path: &Path,
    valid_kinds: &HashSet<String>,
) -> RuleResult {
    check_valid_kinds(
        entities_path,
        valid_kinds,
        "valid-entity-kinds",
        "entity_kind",
    )
}

/// `pub(super)`: reused by `kg::commit` (ADR-102) to check `create`-op note
/// kinds against the same pack-declared taxonomy `kg validate` enforces —
/// meaningful here because `NoteCreateFields::note_kind` is a free-form
/// string, not a closed Rust enum, so it needs a runtime check.
pub(super) fn check_valid_note_kinds(
    notes_path: &Path,
    valid_kinds: &HashSet<String>,
) -> RuleResult {
    check_valid_kinds(notes_path, valid_kinds, "valid-note-kinds", "note_kind")
}

fn check_valid_edge_relations(edges_path: &Path) -> RuleResult {
    let valid_list = {
        let mut names = EdgeRelation::VALID_NAMES.to_vec();
        names.sort_unstable();
        names.join(" | ")
    };
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(rel_str) = v.get("relation").and_then(|r| r.as_str()) {
                    if !EdgeRelation::VALID_NAMES.contains(&rel_str) {
                        let edge_id = v
                            .get("edge_id")
                            .and_then(|i| i.as_str())
                            .or_else(|| v.get("id").and_then(|i| i.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let src = edge_source_id(&v).unwrap_or("").to_string();
                        let tgt = edge_target_id(&v).unwrap_or("").to_string();
                        let id_display = if !edge_id.is_empty() {
                            edge_id.clone()
                        } else if !src.is_empty() && !tgt.is_empty() {
                            format!("{src}→{tgt}")
                        } else {
                            String::new()
                        };
                        let prefix = if id_display.is_empty() {
                            String::new()
                        } else {
                            format!("[{id_display}] ")
                        };
                        violations.push(Violation {
                            entity_id: if edge_id.is_empty() {
                                None
                            } else {
                                Some(edge_id)
                            },
                            entity_name: None,
                            entity_kind: None,
                            rule_id: "valid-edge-relations".into(),
                            severity: "error",
                            message: format!(
                                "{prefix}unknown edge relation: {rel_str:?}. \
                                 Valid: {valid_list}"
                            ),
                            fixable: false,
                        });
                    }
                }
            }
        }
    }

    RuleResult {
        id: "valid-edge-relations".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
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

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        let keys: Vec<(String, String, String)> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                let s = edge_source_id(&v)?.to_string();
                let t = edge_target_id(&v)?.to_string();
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

/// Collect all IDs from an NDJSON file into a set. Returns an empty set when
/// the file is absent or unreadable.
///
/// Reads the `"id"` field — suitable for entities.ndjson and notes.ndjson.
fn collect_ids(path: &Path) -> std::collections::HashSet<String> {
    khive_rule_evaluator::collect_ids(&std::fs::read_to_string(path).unwrap_or_default())
}

/// Collect edge record IDs from edges.ndjson into a set.
///
/// Edge records use `"edge_id"` as the canonical key (ADR-002 / portability
/// layer). Older fixtures may use `"id"` instead; both are collected so the
/// referential-integrity check accepts `annotates` targets that point at edges
/// in either serialization form.
fn collect_edge_ids(path: &Path) -> std::collections::HashSet<String> {
    khive_rule_evaluator::collect_edge_ids(&std::fs::read_to_string(path).unwrap_or_default())
}

/// Check that every edge endpoint resolves to a known record.
///
/// The known-ID set is the union of:
/// - entities.ndjson (entity records)
/// - notes.ndjson (note records — pack-extended endpoints, e.g. GTD task→task)
/// - edges.ndjson edge IDs (ADR-002: `annotates` target may be an edge record)
///
/// Events are not materialized in the git-native KG format and are therefore
/// not included in the known-ID set.
fn check_referential_integrity(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
) -> RuleResult {
    let mut violations = Vec::new();

    let mut known_ids = collect_ids(entities_path);
    known_ids.extend(collect_ids(notes_path));
    known_ids.extend(collect_edge_ids(edges_path));

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                for (label, id) in [
                    ("source", edge_source_id(&v)),
                    ("target", edge_target_id(&v)),
                ] {
                    if let Some(id) = id {
                        if !known_ids.contains(id) {
                            violations.push(Violation {
                                entity_id: Some(id.to_string()),
                                entity_name: None,
                                entity_kind: None,
                                rule_id: "referential-integrity".into(),
                                severity: "error",
                                message: format!("Edge {label} references unknown record: {id}"),
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

// ── Configurable rule loader ──────────────────────────────────────────────────

pub(super) fn load_rules(rules_path: &Path) -> Result<khive_rule_evaluator::Rules> {
    let ext = rules_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if matches!(ext, "yaml" | "yml") {
        return Err(anyhow::Error::msg(RulesSyntaxOrFormatError(format!(
            "rules file {:?} uses YAML format which is not supported in this build. \
             Rename it to {}.toml and use TOML format instead.",
            rules_path,
            rules_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rules")
        ))));
    }

    let content = std::fs::read_to_string(rules_path)
        .with_context(|| format!("read rules file {}", rules_path.display()))?;

    khive_rule_evaluator::Rules::parse_toml(&content).map_err(|error| match error {
        khive_rule_evaluator::RulesError::Syntax(source) => anyhow::Error::new(source).context(
            RulesSyntaxOrFormatError(format!("parse rules TOML {}", rules_path.display())),
        ),
        khive_rule_evaluator::RulesError::Direction(message)
        | khive_rule_evaluator::RulesError::Tier(message) => anyhow::Error::msg(message)
            .context(format!("validate rules TOML {}", rules_path.display())),
    })
}

/// Capture one configurable-pass view; structural checks retain their own file diagnostics.
pub(super) fn configurable_rule_checks(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    let rules = load_rules(rules_path)?;
    let entities = std::fs::read_to_string(entities_path).unwrap_or_default();
    let edges = std::fs::read_to_string(edges_path).unwrap_or_default();
    let notes = std::fs::read_to_string(notes_path).unwrap_or_default();
    evaluate_captured_rules(
        &rules,
        khive_rule_evaluator::NdjsonState {
            entities: &entities,
            edges: &edges,
            notes: &notes,
        },
        khive_rule_evaluator::EvaluationMode::FullDataset,
    )
}

/// Evaluate projected change-set strings without round-tripping through temporary files.
pub(super) fn configurable_rule_checks_partial_view(
    state: khive_rule_evaluator::NdjsonState<'_>,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    let rules = load_rules(rules_path)?;
    evaluate_captured_rules(
        &rules,
        state,
        khive_rule_evaluator::EvaluationMode::PartialView,
    )
}

pub(super) fn evaluate_captured_rules(
    rules: &khive_rule_evaluator::Rules,
    state: khive_rule_evaluator::NdjsonState<'_>,
    mode: khive_rule_evaluator::EvaluationMode,
) -> Result<Vec<RuleResult>> {
    let pack_rules = if rules.needs_pack_edge_rules() {
        build_pack_edge_rules()
            .context("building pack edge-endpoint rules for edge-endpoint-types")?
    } else {
        Vec::new()
    };
    // Capture once, only when citation lint will run. The pure pass shares this
    // instant; it no longer reads the clock midway through its fifth rule class.
    let now = if rules.needs_current_time() {
        chrono::Utc::now()
    } else {
        chrono::DateTime::UNIX_EPOCH
    };
    Ok(khive_rule_evaluator::evaluate(
        rules,
        state,
        khive_rule_evaluator::EvaluationContext {
            mode,
            pack_edge_rules: &pack_rules,
            now,
        },
    ))
}

// Path-shaped adapters retain the existing CLI fixture suite unchanged.
#[cfg(test)]
use khive_rule_evaluator::{
    CitationDateLintConfig, DanglingRefsConfig, DirectionRuleConfig,
    EdgeDirectionConventionsConfig, EdgeEndpointTypesConfig, NamingConventionsConfig,
    NamingConventionsOverride,
};

#[cfg(test)]
fn check_edge_endpoint_types(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    pack_rules: &[EdgeEndpointRule],
    cfg: &EdgeEndpointTypesConfig,
) -> RuleResult {
    khive_rule_evaluator::check_edge_endpoint_types(
        &std::fs::read_to_string(entities_path).unwrap_or_default(),
        &std::fs::read_to_string(notes_path).unwrap_or_default(),
        &std::fs::read_to_string(edges_path).unwrap_or_default(),
        pack_rules,
        cfg,
    )
}

#[cfg(test)]
fn check_edge_direction_conventions(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    cfg: &EdgeDirectionConventionsConfig,
) -> RuleResult {
    khive_rule_evaluator::check_edge_direction_conventions(
        &std::fs::read_to_string(entities_path).unwrap_or_default(),
        &std::fs::read_to_string(notes_path).unwrap_or_default(),
        &std::fs::read_to_string(edges_path).unwrap_or_default(),
        cfg,
    )
}

#[cfg(test)]
fn check_dangling_refs(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    cfg: &DanglingRefsConfig,
) -> RuleResult {
    khive_rule_evaluator::check_dangling_refs(
        &std::fs::read_to_string(entities_path).unwrap_or_default(),
        &std::fs::read_to_string(notes_path).unwrap_or_default(),
        &std::fs::read_to_string(edges_path).unwrap_or_default(),
        cfg,
    )
}

#[cfg(test)]
fn check_naming_conventions(entities_path: &Path, cfg: &NamingConventionsConfig) -> RuleResult {
    khive_rule_evaluator::check_naming_conventions(
        &std::fs::read_to_string(entities_path).unwrap_or_default(),
        cfg,
    )
}

#[cfg(test)]
fn check_citation_date_lint(
    entities_path: &Path,
    notes_path: &Path,
    cfg: &CitationDateLintConfig,
) -> RuleResult {
    khive_rule_evaluator::check_citation_date_lint(
        &std::fs::read_to_string(entities_path).unwrap_or_default(),
        &std::fs::read_to_string(notes_path).unwrap_or_default(),
        cfg,
        chrono::Utc::now(),
    )
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
