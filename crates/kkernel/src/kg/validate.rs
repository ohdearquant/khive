//! `kkernel kg validate` — structural and configurable rule-pass validation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Datelike;
use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::{base_entity_rule_allows, endpoint_matches, KhiveRuntime, RuntimeConfig};
use khive_storage::EdgeRelation;
use khive_types::EdgeEndpointRule;
use serde::Deserialize;

use super::types::{
    OutputFormat, RuleResult, ValidateArgs, ValidationReport, ValidationSummary, Violation,
};

/// Read an edge record's source endpoint id, accepting either canonical
/// serialization spelling: `source` (khive-vcs sync, kkernel archive, and
/// the runtime portability export all write this) or `source_id` (accepted
/// for forward compatibility with any other producer). See #1225 — every
/// canonical NDJSON writer emits `source`/`target`, not `source_id`/
/// `target_id`, and a validator that only recognized the latter silently
/// skipped the endpoint checks on every record those writers produce.
fn edge_source_id(v: &serde_json::Value) -> Option<&str> {
    v.get("source")
        .or_else(|| v.get("source_id"))
        .and_then(|x| x.as_str())
}

/// Target-endpoint counterpart of [`edge_source_id`].
fn edge_target_id(v: &serde_json::Value) -> Option<&str> {
    v.get("target")
        .or_else(|| v.get("target_id"))
        .and_then(|x| x.as_str())
}

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

pub(super) fn cmd_validate(args: ValidateArgs) -> Result<()> {
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

    let entities = count_ndjson_lines(&entities_path).unwrap_or(0);
    let edges = count_ndjson_lines(&edges_path).unwrap_or(0);
    let notes = count_ndjson_lines(&notes_path).unwrap_or(0);

    let rules_path = args.rules.unwrap_or_else(|| kg_dir.join("rules.toml"));

    let taxonomy = build_taxonomy()?;
    let mut rule_results: Vec<RuleResult> =
        structural_checks(&entities_path, &edges_path, &notes_path, &taxonomy);

    if !args.no_rules && rules_path.exists() {
        let configurable =
            match configurable_rule_checks(&entities_path, &edges_path, &notes_path, &rules_path) {
                Ok(results) => results,
                Err(error) if error.is::<RulesSyntaxOrFormatError>() => {
                    eprintln!("Error: {error:?}");
                    std::process::exit(2);
                }
                Err(error) => return Err(error),
            };
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

    if args.fix
        && report
            .rules
            .iter()
            .any(|rule| !rule.passed && rule.violations.iter().any(|violation| violation.fixable))
    {
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

/// Format a record identifier prefix from the available violation fields.
///
/// Produces `"[id name]"` when both are present, `"[id]"` or `"[name]"` when
/// only one is available, and `""` when neither is set.
fn record_prefix(entity_id: Option<&str>, entity_name: Option<&str>) -> String {
    match (entity_id, entity_name) {
        (Some(id), Some(name)) => format!("[{id} {name:?}] "),
        (Some(id), None) => format!("[{id}] "),
        (None, Some(name)) => format!("[{name:?}] "),
        (None, None) => String::new(),
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
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .ok()
                        .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Collect edge record IDs from edges.ndjson into a set.
///
/// Edge records use `"edge_id"` as the canonical key (ADR-002 / portability
/// layer). Older fixtures may use `"id"` instead; both are collected so the
/// referential-integrity check accepts `annotates` targets that point at edges
/// in either serialization form.
fn collect_edge_ids(path: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    let v: serde_json::Value = serde_json::from_str(l).ok()?;
                    // Prefer the canonical `edge_id` field; fall back to `id`.
                    v.get("edge_id")
                        .or_else(|| v.get("id"))
                        .and_then(|i| i.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
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

/// A single configurable lint rule loaded from `rules.toml`.
///
/// `deny_unknown_fields`: a misspelled key (e.g. `severtiy`) must fail the
/// load loudly, never silently fall back to the field's default (commit
/// 4e11ee38). The repository standard here is fail-closed config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    id: String,
    #[serde(default = "default_severity")]
    severity: String,
    kind: String,
    condition: Option<String>,
    require_field: Option<String>,
    #[serde(default)]
    message: String,
}

fn default_severity() -> String {
    "warning".to_owned()
}

/// Top-level structure of a `rules.toml` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<RuleConfig>,
    /// Rule class 1: edge `(source kind, relation, target kind)` endpoint
    /// contract, checked against the live pack/base allowlist.
    #[serde(default)]
    edge_endpoint_types: Option<EdgeEndpointTypesConfig>,
    /// Rule class 2: likely-inverted directional edges.
    #[serde(default)]
    edge_direction_conventions: Option<EdgeDirectionConventionsConfig>,
    /// Rule class 3: unresolvable edge/annotation endpoint references.
    #[serde(default)]
    dangling_refs: Option<DanglingRefsConfig>,
    /// Rule class 4: entity name hygiene.
    #[serde(default)]
    naming_conventions: Option<NamingConventionsConfig>,
    /// Rule class 5: forward-dated citation/property values.
    #[serde(default)]
    citation_date_lint: Option<CitationDateLintConfig>,
}

fn default_enabled() -> bool {
    true
}

/// Default severity for schema/contract-correctness rule classes
/// (`edge-endpoint-types`, `dangling-refs`) — same default the built-in
/// `error`-severity structural checks use, since both classes flag data that
/// is genuinely wrong per the ADR-002/ADR-017 contract, not merely a style
/// preference.
fn default_severity_error() -> String {
    "error".to_owned()
}

/// Config for the `edge-endpoint-types` rule class (rule 1).
///
/// Checks that every edge's `(source kind, relation, target kind)` triple
/// satisfies the canonical endpoint contract — see [`check_edge_endpoint_types`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeEndpointTypesConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_severity_error")]
    severity: String,
}

/// One directional-convention entry: the "forward" kind patterns for a
/// specific relation. An edge matching the reversed pattern (target's kind in
/// `forward_source_kinds`, source's kind in `forward_target_kinds`) but not
/// the forward pattern is flagged as likely-inverted.
///
/// Post-parse validated by [`validate_direction_rule_config`]: `relation`
/// must name a real [`EdgeRelation`], and both kind lists must be non-empty —
/// a misspelled field name (e.g. `forward_source_kind`) must fail the whole
/// `rules.toml` load, not silently produce an empty-list entry that
/// [`check_edge_direction_conventions`] then skips as a no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectionRuleConfig {
    relation: String,
    #[serde(default)]
    forward_source_kinds: Vec<String>,
    #[serde(default)]
    forward_target_kinds: Vec<String>,
}

/// Config for the `edge-direction-conventions` rule class (rule 2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDirectionConventionsConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(default)]
    relations: Vec<DirectionRuleConfig>,
}

/// Config for the `dangling-refs` rule class (rule 3).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DanglingRefsConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_severity_error")]
    severity: String,
}

/// Per-entity-kind override of the naming-convention defaults.
/// `None` fields fall back to the top-level [`NamingConventionsConfig`] value.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct NamingConventionsOverride {
    max_length: Option<usize>,
    no_leading_trailing_whitespace: Option<bool>,
    no_parenthetical_suffix: Option<bool>,
}

fn default_max_length() -> usize {
    200
}

/// Config for the `naming-conventions` rule class (rule 4).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamingConventionsConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(default = "default_max_length")]
    max_length: usize,
    #[serde(default = "default_enabled")]
    no_leading_trailing_whitespace: bool,
    #[serde(default = "default_enabled")]
    no_parenthetical_suffix: bool,
    /// Per-entity-kind overrides, keyed by entity kind string (e.g. `"concept"`).
    #[serde(default)]
    kinds: std::collections::BTreeMap<String, NamingConventionsOverride>,
}

fn default_date_lint_fields() -> Vec<String> {
    vec![
        "year".to_owned(),
        "date".to_owned(),
        "published_at".to_owned(),
        "publication_date".to_owned(),
    ]
}

/// Config for the `citation-date-lint` rule class (rule 5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CitationDateLintConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_severity")]
    severity: String,
    /// Property key names checked for forward-dated values.
    #[serde(default = "default_date_lint_fields")]
    fields: Vec<String>,
}

fn severity_static(s: &str) -> &'static str {
    match s {
        "error" => "error",
        "info" => "info",
        _ => "warning",
    }
}

fn validate_severity(rule_id: &str, s: &str) -> Option<RuleResult> {
    match s {
        "error" | "warning" | "info" => None,
        other => Some(RuleResult {
            id: rule_id.to_string(),
            severity: "error",
            passed: false,
            violations: vec![Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: rule_id.to_string(),
                severity: "error",
                message: format!(
                    "Rule {rule_id:?}: invalid severity {other:?}; \
                     must be \"error\", \"warning\", or \"info\""
                ),
                fixable: false,
            }],
        }),
    }
}

/// Post-parse validation for `[[edge_direction_conventions.relations]]`
/// entries: each entry's `relation` must name a real [`EdgeRelation`]
/// and both `forward_source_kinds`/`forward_target_kinds` must be non-empty.
///
/// `#[serde(default)]` on both kind-list fields means TOML deserialization
/// alone cannot distinguish "field present but misspelled" (e.g.
/// `forward_source_kind`, missing the trailing `s`) from "field
/// intentionally omitted" — both parse to an empty `Vec`. Without this check,
/// [`check_edge_direction_conventions`] silently treats a malformed entry as
/// a no-op (it explicitly `continue`s past any entry with an empty kind
/// list), so a typo disables the check instead of failing the load. Erring
/// on the strict side: entries with genuinely empty kind lists are also
/// rejected here rather than allowed as an intentional "always no-op" entry,
/// since a `rules.toml` author has no other way to say "this entry means
/// nothing" than to omit it entirely (`relations` itself defaults to `[]`).
fn validate_direction_rule_entries(relations: &[DirectionRuleConfig]) -> Result<()> {
    for (idx, rule) in relations.iter().enumerate() {
        if rule.relation.parse::<EdgeRelation>().is_err() {
            bail!(
                "edge_direction_conventions.relations[{idx}]: {:?} is not a valid edge relation",
                rule.relation
            );
        }
        if rule.forward_source_kinds.is_empty() {
            bail!(
                "edge_direction_conventions.relations[{idx}] ({:?}): \
                 forward_source_kinds must be non-empty",
                rule.relation
            );
        }
        if rule.forward_target_kinds.is_empty() {
            bail!(
                "edge_direction_conventions.relations[{idx}] ({:?}): \
                 forward_target_kinds must be non-empty",
                rule.relation
            );
        }
    }
    Ok(())
}

/// Load and evaluate configurable rules from a TOML rules file.
pub(super) fn configurable_rule_checks(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    configurable_rule_checks_impl(entities_path, edges_path, notes_path, rules_path, false)
}

/// Same as [`configurable_rule_checks`], but for callers evaluating rules
/// against a *partial* view of the graph (e.g. `kg commit`'s projection of a
/// single staged change-set, not the full dataset). The built-in
/// `dangling-refs` evaluator's *finding* is meaningless over a partial view
/// (every cross-change-set reference looks "dangling"), so its actual check
/// is skipped here. Its configuration is still validated: a malformed
/// `[dangling_refs] severity = "..."` still produces an error-severity
/// `RuleResult` (same as the full-dataset path), and any generic `[[rules]]`
/// entry — even one that happens to share the id `"dangling-refs"` — is
/// still evaluated and returned. Skipping is done by *not invoking* the
/// built-in evaluator, never by filtering results after the fact by id: a
/// post-hoc `id == "dangling-refs"` filter would also swallow the malformed-
/// config error result and any same-id generic rule, silently letting
/// error-severity findings through (see ADR-102 D2; the commit-time rule
/// pass must never suppress a real error to make a partial-view check quiet).
pub(super) fn configurable_rule_checks_partial_view(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    configurable_rule_checks_impl(entities_path, edges_path, notes_path, rules_path, true)
}

fn configurable_rule_checks_impl(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    rules_path: &Path,
    skip_dangling_refs_partial_view_finding: bool,
) -> Result<Vec<RuleResult>> {
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

    let rules_file: RulesFile = toml::from_str(&content).with_context(|| {
        RulesSyntaxOrFormatError(format!("parse rules TOML {}", rules_path.display()))
    })?;

    if let Some(cfg) = &rules_file.edge_direction_conventions {
        validate_direction_rule_entries(&cfg.relations)
            .with_context(|| format!("validate rules TOML {}", rules_path.display()))?;
    }

    let mut results = Vec::with_capacity(rules_file.rules.len());
    for rule in &rules_file.rules {
        if let Some(err_result) = validate_severity(&rule.id, &rule.severity) {
            results.push(err_result);
            continue;
        }

        let path = match rule.kind.as_str() {
            "entity" => entities_path,
            "edge" => edges_path,
            other => {
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

    // ── Built-in configurable rule classes ─────────────────────────────────
    //
    // Each is opt-in: absent from `rules.toml` means it does not run at all
    // (matching this loader's existing all-or-nothing gate — `rules.toml`
    // itself is entirely optional, see `cmd_validate`). Present-but-disabled
    // (`enabled = false`) also skips evaluation. This keeps every existing
    // `rules.toml` that predates these five sections byte-identical in
    // behavior: no new section, no new checks.
    if let Some(cfg) = &rules_file.edge_endpoint_types {
        if cfg.enabled {
            if let Some(err_result) = validate_severity("edge-endpoint-types", &cfg.severity) {
                results.push(err_result);
            } else {
                let pack_rules = build_pack_edge_rules()
                    .context("building pack edge-endpoint rules for edge-endpoint-types")?;
                results.push(check_edge_endpoint_types(
                    entities_path,
                    notes_path,
                    edges_path,
                    &pack_rules,
                    cfg,
                ));
            }
        }
    }

    if let Some(cfg) = &rules_file.edge_direction_conventions {
        if cfg.enabled {
            if let Some(err_result) = validate_severity("edge-direction-conventions", &cfg.severity)
            {
                results.push(err_result);
            } else {
                results.push(check_edge_direction_conventions(
                    entities_path,
                    notes_path,
                    edges_path,
                    cfg,
                ));
            }
        }
    }

    if let Some(cfg) = &rules_file.dangling_refs {
        if cfg.enabled {
            if let Some(err_result) = validate_severity("dangling-refs", &cfg.severity) {
                // Malformed config is always an error, full-dataset or
                // partial-view alike — never skipped.
                results.push(err_result);
            } else if !skip_dangling_refs_partial_view_finding {
                results.push(check_dangling_refs(
                    entities_path,
                    notes_path,
                    edges_path,
                    cfg,
                ));
            }
        }
    }

    if let Some(cfg) = &rules_file.naming_conventions {
        if cfg.enabled {
            if let Some(err_result) = validate_severity("naming-conventions", &cfg.severity) {
                results.push(err_result);
            } else {
                results.push(check_naming_conventions(entities_path, cfg));
            }
        }
    }

    if let Some(cfg) = &rules_file.citation_date_lint {
        if cfg.enabled {
            if let Some(err_result) = validate_severity("citation-date-lint", &cfg.severity) {
                results.push(err_result);
            } else {
                results.push(check_citation_date_lint(entities_path, notes_path, cfg));
            }
        }
    }

    Ok(results)
}

fn evaluate_rule(rule: &RuleConfig, path: &Path) -> Vec<Violation> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let condition: Option<(&str, &str)> = rule.condition.as_deref().and_then(|c| c.split_once('='));

    let sev = severity_static(&rule.severity);
    let mut violations = Vec::new();

    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(val) => val,
            Err(_) => continue,
        };

        if let Some((field, expected)) = condition {
            if field == "source_id" && expected == "target_id" {
                let src = edge_source_id(&v).unwrap_or("");
                let tgt = edge_target_id(&v).unwrap_or("");
                if src == tgt {
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

            let actual = v.get(field).and_then(|f| f.as_str()).unwrap_or("");
            if actual != expected {
                continue;
            }
        }

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

// ── Built-in configurable rule classes ────────────────────────────────────────

/// A resolved `(substrate, kind, entity_type)` triple for one record ID,
/// gathered by scanning `entities.ndjson` / `notes.ndjson`. The offline
/// equivalent of `KhiveRuntime::resolve_edge_endpoint`, which the DB-backed
/// validator uses and this CLI path cannot (no DB connection).
struct KindInfo {
    substrate: &'static str,
    kind: String,
    entity_type: Option<String>,
}

/// Build the `id -> (substrate, kind, entity_type)` map used by the
/// `edge-endpoint-types` and `edge-direction-conventions` rule classes.
/// Entity records win the `"entity"` substrate, note records the `"note"`
/// substrate; a duplicate UUID across both files (already reported by
/// `no-duplicate-uuids`) resolves to whichever file is scanned last.
///
/// Known edge IDs (from `edges.ndjson`, via [`collect_edge_ids`] — the same
/// set `referential-integrity`/`dangling-refs` already trust) are also
/// entered, as substrate `"edge"`, but only when the ID is not already an
/// entity or note. This closes the edge-substrate endpoint bypass: without
/// it, an edge ID used as an endpoint resolved to "unknown" and was skipped
/// by [`check_edge_endpoint_types`] entirely (deferred to
/// `dangling-refs`/`referential-integrity`, which only check *existence*,
/// not substrate legality) — so `concept -[annotates]-> <edge_id>` or any
/// non-`annotates` relation naming an edge endpoint passed offline even
/// though the live `link`/`update` verbs reject both
/// (`khive-runtime::operations::validate_edge_relation_endpoints`:
/// `annotates` requires a note *source* but accepts any substrate as
/// *target*; every other relation, including `supersedes`/`supports`/
/// `refutes`, rejects an edge endpoint outright). `endpoint_matches` never
/// matches substrate `"edge"` against any `EndpointKind` variant (it only
/// matches `"entity"`/`"note"`), so a resolved edge endpoint still correctly
/// fails every pack/base rule lookup for non-`annotates` relations — the
/// dispatch in [`check_edge_endpoint_types`] only needs one explicit
/// substrate check for the `supersedes`/`supports`/`refutes` family, added
/// alongside this map change.
fn collect_kind_map(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
) -> HashMap<String, KindInfo> {
    let mut map = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(entities_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(id), Some(kind)) = (
                    v.get("id").and_then(|i| i.as_str()),
                    v.get("kind").and_then(|k| k.as_str()),
                ) {
                    map.insert(
                        id.to_string(),
                        KindInfo {
                            substrate: "entity",
                            kind: kind.to_string(),
                            entity_type: v
                                .get("entity_type")
                                .and_then(|t| t.as_str())
                                .map(str::to_string),
                        },
                    );
                }
            }
        }
    }
    if let Ok(content) = std::fs::read_to_string(notes_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(id), Some(kind)) = (
                    v.get("id").and_then(|i| i.as_str()),
                    v.get("kind").and_then(|k| k.as_str()),
                ) {
                    map.insert(
                        id.to_string(),
                        KindInfo {
                            substrate: "note",
                            kind: kind.to_string(),
                            entity_type: None,
                        },
                    );
                }
            }
        }
    }
    for id in collect_edge_ids(edges_path) {
        map.entry(id).or_insert(KindInfo {
            substrate: "edge",
            kind: "edge".to_string(),
            entity_type: None,
        });
    }
    map
}

/// `true` if any pack-declared edge endpoint rule admits `(src, relation, tgt)`.
///
/// Thin `.any()` wrapper over the reused, canonical [`endpoint_matches`]
/// matcher (`khive-runtime`) — the actual endpoint-pairing DATA lives in
/// `pack_rules` (fetched live from the pack registry by
/// [`build_pack_edge_rules`]), never re-derived here.
fn pack_rule_allows_kinds(
    rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src: &KindInfo,
    tgt: &KindInfo,
) -> bool {
    rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(
                &r.source,
                src.substrate,
                &src.kind,
                src.entity_type.as_deref(),
            )
            && endpoint_matches(
                &r.target,
                tgt.substrate,
                &tgt.kind,
                tgt.entity_type.as_deref(),
            )
    })
}

/// Rule class 1: edge `(source kind, relation, target
/// kind)` endpoint contract.
///
/// Mirrors `KhiveRuntime::validate_edge_relation_endpoints`'s per-relation
/// dispatch (`annotates` crosses substrates; `supersedes`/`supports`/
/// `refutes` require same-substrate endpoints; every other relation consults
/// the pack/base allowlist) but works from plain `(substrate, kind,
/// entity_type)` triples parsed out of NDJSON, since `kg validate` never
/// opens a DB connection to resolve live records. The rule DATA —
/// `base_entity_rule_allows`'s base table and `pack_rules`' `EdgeEndpointRule`s
/// — is always read live from `khive-runtime` (the same source the `link`/
/// `update` verbs enforce against); only this dispatch shape is restated for
/// the offline path, so the allowlist itself cannot drift out of sync.
///
/// Edges whose endpoints don't resolve within `entities.ndjson`/`notes.ndjson`
/// are skipped here — `dangling-refs` and the always-on `referential-integrity`
/// structural check own that failure mode, so this rule does not double-report it.
fn check_edge_endpoint_types(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    pack_rules: &[EdgeEndpointRule],
    cfg: &EdgeEndpointTypesConfig,
) -> RuleResult {
    let kind_map = collect_kind_map(entities_path, notes_path, edges_path);
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(src_id) = edge_source_id(&v) else {
                continue;
            };
            let Some(tgt_id) = edge_target_id(&v) else {
                continue;
            };
            let Some(rel_str) = v.get("relation").and_then(|r| r.as_str()) else {
                continue;
            };
            let Ok(relation) = rel_str.parse::<EdgeRelation>() else {
                // Unknown relation: `valid-edge-relations` already reports it.
                continue;
            };
            let (Some(src), Some(tgt)) = (kind_map.get(src_id), kind_map.get(tgt_id)) else {
                // Unresolved endpoint: `referential-integrity`/`dangling-refs` own this.
                continue;
            };

            let allowed = if relation == EdgeRelation::Annotates {
                // Runtime parity (operations.rs:1226): source must be a note;
                // target may be ANY substrate, including an edge.
                src.substrate == "note"
            } else if matches!(
                relation,
                EdgeRelation::Supersedes | EdgeRelation::Supports | EdgeRelation::Refutes
            ) {
                // Runtime parity (operations.rs:1289-1333): an edge endpoint on
                // either side is rejected outright for this relation family
                // (folded into the substrate-mismatch branch below, since
                // `"edge" != "edge"` is false but `"edge"` must still never
                // reach the entity/note arms), regardless of the other
                // endpoint's substrate.
                if src.substrate != tgt.substrate || src.substrate == "edge" {
                    false
                } else if src.substrate == "entity" {
                    base_entity_rule_allows(&src.kind, relation, &tgt.kind)
                } else {
                    // Runtime parity: same-substrate note<->note is unrestricted
                    // for supersedes/supports/refutes (operations.rs).
                    true
                }
            } else {
                let base_ok = src.substrate == "entity"
                    && tgt.substrate == "entity"
                    && base_entity_rule_allows(&src.kind, relation, &tgt.kind);
                base_ok || pack_rule_allows_kinds(pack_rules, relation, src, tgt)
            };

            if !allowed {
                violations.push(Violation {
                    entity_id: Some(src_id.to_string()),
                    entity_name: None,
                    entity_kind: Some(src.kind.clone()),
                    rule_id: "edge-endpoint-types".into(),
                    severity: sev,
                    message: format!(
                        "[{src_id}\u{2192}{tgt_id}] ({} {}) -[{}]-> ({} {}) is not a permitted \
                         endpoint pairing for this relation",
                        src.substrate,
                        src.kind,
                        relation.as_str(),
                        tgt.substrate,
                        tgt.kind
                    ),
                    fixable: false,
                });
            }
        }
    }

    RuleResult {
        id: "edge-endpoint-types".into(),
        severity: sev,
        passed: violations.is_empty(),
        violations,
    }
}

/// Rule class 2: likely-inverted directional edges.
///
/// For each `[[edge_direction_conventions.relations]]` entry, an edge whose
/// relation matches but whose `(source kind, target kind)` matches the
/// REVERSED pattern (target's kind is one of the configured
/// `forward_source_kinds`, source's kind is one of the configured
/// `forward_target_kinds`) while NOT matching the forward pattern is flagged
/// as likely-inverted. A relation with no configured entry is not checked —
/// this rule class does not guess which relations are directional. `warn` by
/// default, since this is a heuristic, not a hard contract violation like
/// `edge-endpoint-types`.
fn check_edge_direction_conventions(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    cfg: &EdgeDirectionConventionsConfig,
) -> RuleResult {
    let kind_map = collect_kind_map(entities_path, notes_path, edges_path);
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(src_id) = edge_source_id(&v) else {
                continue;
            };
            let Some(tgt_id) = edge_target_id(&v) else {
                continue;
            };
            let Some(rel_str) = v.get("relation").and_then(|r| r.as_str()) else {
                continue;
            };
            let (Some(src), Some(tgt)) = (kind_map.get(src_id), kind_map.get(tgt_id)) else {
                continue;
            };

            for rule in &cfg.relations {
                if rule.relation != rel_str
                    || rule.forward_source_kinds.is_empty()
                    || rule.forward_target_kinds.is_empty()
                {
                    continue;
                }
                let forward = rule.forward_source_kinds.iter().any(|k| k == &src.kind)
                    && rule.forward_target_kinds.iter().any(|k| k == &tgt.kind);
                if forward {
                    continue;
                }
                let reversed = rule.forward_source_kinds.iter().any(|k| k == &tgt.kind)
                    && rule.forward_target_kinds.iter().any(|k| k == &src.kind);
                if reversed {
                    violations.push(Violation {
                        entity_id: Some(src_id.to_string()),
                        entity_name: None,
                        entity_kind: Some(src.kind.clone()),
                        rule_id: "edge-direction-conventions".into(),
                        severity: sev,
                        message: format!(
                            "[{src_id}\u{2192}{tgt_id}] {rel_str} from {} to {} matches the \
                             reversed direction convention configured for this relation; \
                             likely inverted",
                            src.kind, tgt.kind
                        ),
                        fixable: false,
                    });
                }
            }
        }
    }

    RuleResult {
        id: "edge-direction-conventions".into(),
        severity: sev,
        passed: violations.is_empty(),
        violations,
    }
}

/// Rule class 3: unresolvable edge endpoint references —
/// the user-configurable counterpart to the always-on `referential-integrity`
/// structural check (fixed at `error` severity, cannot be disabled or
/// downgraded).
///
/// **Scope note**: `kg validate` has no `--db` flag and never opens a live
/// graph connection — every reference is resolved only within the validated
/// NDJSON dataset itself (`entities.ndjson` + `notes.ndjson` + edge IDs in
/// `edges.ndjson`, reusing the same [`collect_ids`]/[`collect_edge_ids`]
/// helpers `referential-integrity` uses, rather than re-deriving the known-ID
/// set). An unresolvable reference is therefore always reported as "not in
/// dataset" — there is no live-graph mode in this build to distinguish from
/// "checked nowhere". That limitation is stated explicitly in every
/// violation message rather than silently passing.
fn check_dangling_refs(
    entities_path: &Path,
    notes_path: &Path,
    edges_path: &Path,
    cfg: &DanglingRefsConfig,
) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    let mut known_ids = collect_ids(entities_path);
    known_ids.extend(collect_ids(notes_path));
    known_ids.extend(collect_edge_ids(edges_path));

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                for (field, id) in [
                    ("source", edge_source_id(&v)),
                    ("target", edge_target_id(&v)),
                ] {
                    if let Some(id) = id {
                        if !known_ids.contains(id) {
                            violations.push(Violation {
                                entity_id: Some(id.to_string()),
                                entity_name: None,
                                entity_kind: None,
                                rule_id: "dangling-refs".into(),
                                severity: sev,
                                message: format!(
                                    "edge {field} {id} not in dataset (validated offline \
                                     within the NDJSON dataset only; no live-graph check \
                                     available in this build)"
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
        id: "dangling-refs".into(),
        severity: sev,
        passed: violations.is_empty(),
        violations,
    }
}

/// `true` if `name`'s trimmed form ends with `)` and has a matching `(`
/// preceded by other content — the shape of a parenthetical suffix like
/// `"Foo (2024 paper)"`. Deliberately simple (no regex dependency): this is
/// a heuristic lint, not a parser.
fn has_parenthetical_suffix(name: &str) -> bool {
    let trimmed = name.trim();
    trimmed.ends_with(')') && trimmed.rfind('(').is_some_and(|i| i > 0)
}

/// Rule class 4: entity name hygiene.
///
/// Checks `name` against: non-empty, no leading/trailing whitespace, no
/// parenthetical suffix (e.g. `"Foo (2024 paper)"` — qualifiers belong in
/// `properties`, not `name`), and a configurable max length.
/// `[naming_conventions.kinds.<entity_kind>]` overrides any of the three
/// predicate toggles or `max_length` for that kind only. `warn` by default.
fn check_naming_conventions(entities_path: &Path, cfg: &NamingConventionsConfig) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(entities_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let id = v.get("id").and_then(|i| i.as_str()).map(str::to_string);
            let kind = v.get("kind").and_then(|k| k.as_str()).map(str::to_string);
            let overrides = kind.as_deref().and_then(|k| cfg.kinds.get(k));
            let max_length = overrides
                .and_then(|o| o.max_length)
                .unwrap_or(cfg.max_length);
            let no_ws = overrides
                .and_then(|o| o.no_leading_trailing_whitespace)
                .unwrap_or(cfg.no_leading_trailing_whitespace);
            let no_paren = overrides
                .and_then(|o| o.no_parenthetical_suffix)
                .unwrap_or(cfg.no_parenthetical_suffix);
            let prefix = record_prefix(id.as_deref(), Some(name));

            if name.trim().is_empty() {
                violations.push(Violation {
                    entity_id: id.clone(),
                    entity_name: Some(name.to_string()),
                    entity_kind: kind.clone(),
                    rule_id: "naming-conventions".into(),
                    severity: sev,
                    message: format!("{prefix}name is empty or whitespace-only"),
                    fixable: false,
                });
                continue;
            }
            if no_ws && name != name.trim() {
                violations.push(Violation {
                    entity_id: id.clone(),
                    entity_name: Some(name.to_string()),
                    entity_kind: kind.clone(),
                    rule_id: "naming-conventions".into(),
                    severity: sev,
                    message: format!("{prefix}name has leading/trailing whitespace"),
                    fixable: false,
                });
            }
            if no_paren && has_parenthetical_suffix(name) {
                violations.push(Violation {
                    entity_id: id.clone(),
                    entity_name: Some(name.to_string()),
                    entity_kind: kind.clone(),
                    rule_id: "naming-conventions".into(),
                    severity: sev,
                    message: format!(
                        "{prefix}name carries a parenthetical suffix; use `properties` for \
                         qualifiers instead of embedding them in `name`"
                    ),
                    fixable: false,
                });
            }
            if name.chars().count() > max_length {
                violations.push(Violation {
                    entity_id: id.clone(),
                    entity_name: Some(name.to_string()),
                    entity_kind: kind.clone(),
                    rule_id: "naming-conventions".into(),
                    severity: sev,
                    message: format!(
                        "{prefix}name exceeds max length {max_length} ({} chars)",
                        name.chars().count()
                    ),
                    fixable: false,
                });
            }
        }
    }

    RuleResult {
        id: "naming-conventions".into(),
        severity: sev,
        passed: violations.is_empty(),
        violations,
    }
}

/// `Some(description)` if `value` encodes a date/year strictly after `now`.
/// Recognises a bare 4-digit year (JSON number or string) and RFC-3339 /
/// `YYYY-MM-DD` date strings; any other shape is left unchecked (returns
/// `None`, not a violation) rather than guessed at.
fn future_date_description(
    value: &serde_json::Value,
    now: &chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let current_year = now.year();
    match value {
        serde_json::Value::Number(n) => {
            let y = n.as_i64()?;
            if (1000..=9999).contains(&y) && y > i64::from(current_year) {
                Some(format!("year {y} is after the current year {current_year}"))
            } else {
                None
            }
        }
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
                let y: i64 = s.parse().ok()?;
                return if y > i64::from(current_year) {
                    Some(format!("year {y} is after the current year {current_year}"))
                } else {
                    None
                };
            }
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                let dt_utc = dt.with_timezone(&chrono::Utc);
                return if dt_utc > *now {
                    Some(format!("date {s} is in the future (validated at {now})"))
                } else {
                    None
                };
            }
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                if d > now.date_naive() {
                    return Some(format!("date {s} is in the future (validated at {now})"));
                }
            }
            None
        }
        _ => None,
    }
}

/// Rule class 5: forward-dated citation/property values.
///
/// Checks the configured `properties` field names (default: `year`, `date`,
/// `published_at`, `publication_date`) on both entities and notes for values
/// that encode a date after the validation-time `now`, catching forward-dated
/// citation typos (e.g. `year = 2124`). `warn` by default.
fn check_citation_date_lint(
    entities_path: &Path,
    notes_path: &Path,
    cfg: &CitationDateLintConfig,
) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let now = chrono::Utc::now();
    let mut violations = Vec::new();

    for path in [entities_path, notes_path] {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines().filter(|l| !l.trim().is_empty()) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(props) = v.get("properties").and_then(|p| p.as_object()) else {
                    continue;
                };
                let id = v.get("id").and_then(|i| i.as_str()).map(str::to_string);
                let name = v.get("name").and_then(|n| n.as_str()).map(str::to_string);
                let kind = v.get("kind").and_then(|k| k.as_str()).map(str::to_string);
                let prefix = record_prefix(id.as_deref(), name.as_deref());

                for field in &cfg.fields {
                    let Some(value) = props.get(field) else {
                        continue;
                    };
                    if let Some(desc) = future_date_description(value, &now) {
                        violations.push(Violation {
                            entity_id: id.clone(),
                            entity_name: name.clone(),
                            entity_kind: kind.clone(),
                            rule_id: "citation-date-lint".into(),
                            severity: sev,
                            message: format!("{prefix}property {field:?}: {desc}"),
                            fixable: false,
                        });
                    }
                }
            }
        }
    }

    RuleResult {
        id: "citation-date-lint".into(),
        severity: sev,
        passed: violations.is_empty(),
        violations,
    }
}

fn apply_fixes(repo: &std::path::Path) -> Result<()> {
    let kg_dir = repo.join(".khive/kg");
    fix_sort_order(&kg_dir.join("entities.ndjson"), "id")?;
    fix_sort_order_edges(&kg_dir.join("edges.ndjson"))?;
    eprintln!("~ sort-order: applied fix to entities.ndjson and edges.ndjson");
    Ok(())
}

pub(super) fn fix_sort_order(path: &Path, sort_key: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<serde_json::Value> = Vec::new();
    for (idx, l) in content
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let v = serde_json::from_str(l).with_context(|| {
            format!(
                "{} line {}: refusing to apply --fix over malformed JSON; run `kg validate` for details",
                path.display(),
                idx + 1
            )
        })?;
        lines.push(v);
    }
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
    let mut lines: Vec<serde_json::Value> = Vec::new();
    for (idx, l) in content
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let v = serde_json::from_str(l).with_context(|| {
            format!(
                "{} line {}: refusing to apply --fix over malformed JSON; run `kg validate` for details",
                path.display(),
                idx + 1
            )
        })?;
        lines.push(v);
    }
    lines.sort_by(|a, b| {
        let ak = (
            edge_source_id(a).unwrap_or(""),
            edge_target_id(a).unwrap_or(""),
            a.get("relation").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let bk = (
            edge_source_id(b).unwrap_or(""),
            edge_target_id(b).unwrap_or(""),
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
                    let prefix = record_prefix(v.entity_id.as_deref(), v.entity_name.as_deref());
                    // Include record identifier when not already in the message.
                    if prefix.is_empty() || v.message.starts_with(prefix.trim()) {
                        println!("    - {}", v.message);
                    } else {
                        println!("    - {}{}", prefix, v.message);
                    }
                }
                if !verbose && r.violations.len() > 2 {
                    println!("    + {} more (run with --verbose)", r.violations.len() - 2);
                }
            }
        }
    }
    let s = &report.summary;
    let empty = if s.empty { "; empty graph" } else { "" };
    println!(
        "\nSummary: {} error(s), {} warning(s), {} entities, {} edges{empty}",
        s.errors, s.warnings, s.entities, s.edges
    );
}

fn print_github_format(report: &ValidationReport) {
    if report.summary.empty {
        println!("::notice ::Empty graph: no entity, edge, or note records read");
    }
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

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
