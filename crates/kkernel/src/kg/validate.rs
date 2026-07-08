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

/// Taxonomy sets derived from the loaded pack registry.
///
/// `pub(super)`: also consumed by `kg::commit` (`kkernel kg commit`, ADR-102)
/// to reuse the same entity-kind/note-kind vocabulary this module builds for
/// `kg validate`, rather than re-deriving it.
pub(super) struct KgTaxonomy {
    pub(super) entity_kinds: HashSet<String>,
    pub(super) note_kinds: HashSet<String>,
}

/// Build the merged entity-kind and note-kind sets from all registered packs.
///
/// Mirrors the `build_registry()` pattern in `pack_introspect`. No DB is
/// opened — only pack metadata is needed.
///
/// # Strict-actor-mode exemption
///
/// This function does NOT call `enforce_strict_actor_mode`. That enforcement
/// seam protects the **comm dispatch boundary** — it prevents a server from
/// accepting comm operations without a configured actor identity. `build_taxonomy`
/// is metadata/introspection-only: it collects the entity-kind and note-kind
/// sets declared by the loaded packs and never dispatches a verb or reads
/// comm/tenant data. There is no tenant-isolation risk here, so requiring an
/// actor identity would make `kkernel kg validate` fail under
/// `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` without any security benefit — an
/// operator must be able to run taxonomy validation against a strict-mode
/// deployment. See `enforce_strict_actor_mode` in
/// `crates/khive-mcp/src/serve.rs` for the authoritative boundary definition.
pub(super) fn build_taxonomy() -> Result<KgTaxonomy> {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("kkernel-validate")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).context("building taxonomy registry")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build().context("building VerbRegistry")?;

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
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("kkernel-validate")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).context("building edge-rules registry")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build().context("building VerbRegistry")?;
    Ok(registry.all_edge_rules())
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

    let rules_path = args.rules.unwrap_or_else(|| kg_dir.join("rules.toml"));

    let taxonomy = build_taxonomy()?;
    let mut rule_results: Vec<RuleResult> =
        structural_checks(&entities_path, &edges_path, &notes_path, &taxonomy);

    if !args.no_rules && rules_path.exists() {
        let configurable =
            configurable_rule_checks(&entities_path, &edges_path, &notes_path, &rules_path)?;
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

fn structural_checks(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    taxonomy: &KgTaxonomy,
) -> Vec<RuleResult> {
    let mut results = vec![
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
                    let missing: Vec<&str> = ["source_id", "target_id", "relation"]
                        .into_iter()
                        .filter(|f| v.get(f).and_then(|x| x.as_str()).is_none())
                        .collect();
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

/// `pub(super)`: reused by `kg::commit` (ADR-102) to check `create`-op entity
/// kinds against the same pack-declared taxonomy `kg validate` enforces.
pub(super) fn check_valid_entity_kinds(
    entities_path: &Path,
    valid_kinds: &HashSet<String>,
) -> RuleResult {
    let valid_list = {
        let mut v: Vec<&str> = valid_kinds.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.join(" | ")
    };
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(entities_path) {
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
                            rule_id: "valid-entity-kinds".into(),
                            severity: "error",
                            message: format!(
                                "{prefix}unknown entity_kind: {kind_str:?}. \
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
        id: "valid-entity-kinds".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

/// `pub(super)`: reused by `kg::commit` (ADR-102) to check `create`-op note
/// kinds against the same pack-declared taxonomy `kg validate` enforces —
/// meaningful here because `NoteCreateFields::note_kind` is a free-form
/// string, not a closed Rust enum, so it needs a runtime check.
pub(super) fn check_valid_note_kinds(
    notes_path: &Path,
    valid_kinds: &HashSet<String>,
) -> RuleResult {
    let valid_list = {
        let mut v: Vec<&str> = valid_kinds.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.join(" | ")
    };
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(notes_path) {
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
                            rule_id: "valid-note-kinds".into(),
                            severity: "error",
                            message: format!(
                                "{prefix}unknown note_kind: {kind_str:?}. \
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
        id: "valid-note-kinds".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
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
                        let src = v
                            .get("source_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tgt = v
                            .get("target_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
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
                for field in &["source_id", "target_id"] {
                    if let Some(id) = v.get(field).and_then(|i| i.as_str()) {
                        if !known_ids.contains(id) {
                            violations.push(Violation {
                                entity_id: Some(id.to_string()),
                                entity_name: None,
                                entity_kind: None,
                                rule_id: "referential-integrity".into(),
                                severity: "error",
                                message: format!(
                                    "Edge {} references unknown record: {id}",
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

// ── Configurable rule loader ──────────────────────────────────────────────────

/// A single configurable lint rule loaded from `rules.toml`.
///
/// `deny_unknown_fields`: a misspelled key (e.g. `severtiy`) must fail the
/// load loudly, never silently fall back to the field's default (High-2,
/// codex re-review 4e11ee38 — the repo standard here is fail-closed config).
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
/// [`check_edge_direction_conventions`] then skips as a no-op (High-2).
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
/// entries (High-2): each entry's `relation` must name a real [`EdgeRelation`]
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
                results.push(err_result);
            } else {
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
                let src = v.get("source_id").and_then(|s| s.as_str()).unwrap_or("");
                let tgt = v.get("target_id").and_then(|s| s.as_str()).unwrap_or("");
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
            let Some(src_id) = v.get("source_id").and_then(|s| s.as_str()) else {
                continue;
            };
            let Some(tgt_id) = v.get("target_id").and_then(|s| s.as_str()) else {
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
            let Some(src_id) = v.get("source_id").and_then(|s| s.as_str()) else {
                continue;
            };
            let Some(tgt_id) = v.get("target_id").and_then(|s| s.as_str()) else {
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
                for field in &["source_id", "target_id"] {
                    if let Some(id) = v.get(*field).and_then(|i| i.as_str()) {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use khive_types::EndpointKind;
    use tempfile::TempDir;

    use super::*;

    // ── Taxonomy helpers ──────────────────────────────────────────────────────

    /// Build the real pack-registry taxonomy. Tests that need it call this once.
    fn real_taxonomy() -> KgTaxonomy {
        build_taxonomy().expect("build_taxonomy must succeed in test environment")
    }

    /// Minimal entity-kind set covering the 8 base kinds + `resource` (ADR-048).
    fn base_entity_kinds() -> HashSet<String> {
        [
            "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
            "resource",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Minimal note-kind set covering base KG kinds + pack additions.
    fn base_note_kinds() -> HashSet<String> {
        [
            "observation",
            "insight",
            "question",
            "decision",
            "reference",
            "task",
            "memory",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn make_kg_dir(tmp: &TempDir) -> PathBuf {
        let kg_dir = tmp.path().join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        kg_dir
    }

    fn write_entities(kg_dir: &std::path::Path, entities: &[(&str, &str, &str)]) {
        let content: String = entities
            .iter()
            .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
    }

    fn write_edges(kg_dir: &std::path::Path, edges: &[(&str, &str, &str)]) {
        let content: String = edges
            .iter()
            .map(|(src, tgt, rel)| {
                format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
    }

    fn write_notes(kg_dir: &std::path::Path, notes: &[(&str, &str)]) {
        let content: String = notes
            .iter()
            .map(|(id, kind)| format!(r#"{{"id":"{id}","kind":"{kind}"}}"#))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("notes.ndjson"), content + "\n").unwrap();
    }

    // ── Schema-compliance tests (#437) ────────────────────────────────────────

    #[test]
    fn schema_compliance_rejects_malformed_entities_ndjson() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        std::fs::write(kg_dir.join("entities.ndjson"), "not-valid-json\n").unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let taxonomy = KgTaxonomy {
            entity_kinds: base_entity_kinds(),
            note_kinds: base_note_kinds(),
        };
        let results = structural_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &taxonomy,
        );

        let schema_rule = results
            .iter()
            .find(|r| r.id == "schema-compliance")
            .expect("schema-compliance rule must always run");
        assert!(
            !schema_rule.passed,
            "malformed NDJSON must fail schema-compliance"
        );
        assert!(
            schema_rule.violations[0]
                .message
                .contains("entities.ndjson line 1"),
            "violation must point at the malformed line: {}",
            schema_rule.violations[0].message
        );
    }

    #[test]
    fn schema_compliance_passes_well_formed_kg_and_absent_notes() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        write_edges(&kg_dir, &[]);

        let result = check_schema_compliance(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
        );
        assert!(
            result.passed,
            "well-formed KG with absent notes.ndjson must pass: {:?}",
            result.violations
        );
    }

    #[test]
    fn fix_sort_order_refuses_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let path = kg_dir.join("entities.ndjson");
        std::fs::write(&path, "not-valid-json\n").unwrap();

        let err = fix_sort_order(&path, "id").expect_err("fix must refuse malformed JSON");
        assert!(err.to_string().contains("line 1"));
        // The file must be left untouched, not truncated/dropped.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-valid-json\n");
    }

    // ── Entity kind tests ─────────────────────────────────────────────────────

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
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn task_note_depends_on_passes_referential_integrity() {
        // Regression for ADR-017 + GTD pack: `depends_on` between two `task`
        // notes is a valid pack-extended edge. The referential-integrity check
        // must resolve note IDs from notes.ndjson, not only from entities.ndjson.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        // No entity records — the edge endpoints live in notes only.
        std::fs::write(kg_dir.join("entities.ndjson"), "").unwrap();
        write_notes(
            &kg_dir,
            &[
                ("task-0001-0000-0000-0000-000000000001", "task"),
                ("task-0002-0000-0000-0000-000000000002", "task"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "task-0001-0000-0000-0000-000000000001",
                "task-0002-0000-0000-0000-000000000002",
                "depends_on",
            )],
        );
        let result = check_referential_integrity(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(
            result.passed,
            "task note depends_on must pass referential integrity; violations: {:?}",
            result.violations
        );
        assert!(result.violations.is_empty());
    }

    #[test]
    fn note_annotates_edge_passes_referential_integrity() {
        // Regression for ADR-002: `annotates` source is a note, target may be an
        // edge record. The referential-integrity check must include edge IDs
        // (keyed by `edge_id`) in the known-ID set, not only entity/note IDs.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        // Two entity records connected by an `extends` edge that carries an edge_id.
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        // The extends edge with an explicit edge_id.
        let edges = r#"{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
{"source_id":"note-obs-0000-0000-0000-000000000001","target_id":"eeeeeeee-0000-0000-0000-000000000001","relation":"annotates"}
"#;
        std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
        // The observation note that is the annotates source.
        write_notes(
            &kg_dir,
            &[("note-obs-0000-0000-0000-000000000001", "observation")],
        );
        let result = check_referential_integrity(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(
            result.passed,
            "note annotates edge must pass referential integrity; violations: {:?}",
            result.violations
        );
        assert!(result.violations.is_empty());
    }

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
        std::fs::write(&rules_path, "rules = []\n").unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert!(results.is_empty(), "no rules → no results");
    }

    #[test]
    fn configurable_rule_checks_require_field_detects_missing_description() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);

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
            &kg_dir.join("notes.ndjson"),
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
            &kg_dir.join("notes.ndjson"),
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
            &kg_dir.join("notes.ndjson"),
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
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert_eq!(results[0].severity, "error");
    }

    #[test]
    fn configurable_rule_checks_invalid_severity_produces_error_result() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_toml = r#"
[[rules]]
id = "bad-severity"
severity = "erorr"
kind = "entity"
require_field = "description"
message = "bad"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed, "invalid severity must fail");
        assert_eq!(results[0].severity, "error");
        assert!(
            results[0].violations[0]
                .message
                .contains("invalid severity"),
            "error message should mention invalid severity"
        );
    }

    #[test]
    fn sort_order_fix_sorts_entities() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
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

    // ── [High] Entity-kind registry source of truth ───────────────────────────

    #[test]
    fn invalid_entity_kind_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "nonsense", "B"),
            ],
        );
        let kinds = base_entity_kinds();
        let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
        assert!(!result.passed, "invalid entity kind must fail");
        assert_eq!(result.violations.len(), 1);
        assert!(
            result.violations[0].message.contains("nonsense"),
            "violation message should name the bad kind: {}",
            result.violations[0].message
        );
        assert!(
            result.violations[0].message.contains("concept"),
            "violation message should list valid kinds: {}",
            result.violations[0].message
        );
    }

    #[test]
    fn resource_kind_is_accepted_as_pack_registered() {
        // ADR-048: `resource` is registered by the KG pack and must not be rejected.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "resource", "R"),
            ],
        );
        let taxonomy = real_taxonomy();
        assert!(
            taxonomy.entity_kinds.contains("resource"),
            "VerbRegistry must include 'resource' from KG pack (ADR-048)"
        );
        let result =
            check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &taxonomy.entity_kinds);
        assert!(
            result.passed,
            "pack-registered kind 'resource' must pass; violations: {:?}",
            result.violations
        );
        assert!(result.violations.is_empty());
    }

    #[test]
    fn valid_entity_kinds_all_pass() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "document", "B"),
                ("cccccccc-0000-0000-0000-000000000003", "dataset", "C"),
                ("dddddddd-0000-0000-0000-000000000004", "project", "D"),
                ("eeeeeeee-0000-0000-0000-000000000005", "person", "E"),
                ("ffffffff-0000-0000-0000-000000000006", "org", "F"),
                ("11111111-0000-0000-0000-000000000007", "artifact", "G"),
                ("22222222-0000-0000-0000-000000000008", "service", "H"),
            ],
        );
        let kinds = base_entity_kinds();
        let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
        assert!(result.passed, "all 8 canonical kinds must pass");
        assert!(result.violations.is_empty());
    }

    // ── [High] Note-kind validation ───────────────────────────────────────────

    #[test]
    fn invalid_note_kind_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_notes(
            &kg_dir,
            &[("note-0001", "observation"), ("note-0002", "bogus_kind")],
        );
        let kinds = base_note_kinds();
        let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &kinds);
        assert!(!result.passed, "invalid note kind must fail");
        assert_eq!(result.violations.len(), 1);
        assert!(
            result.violations[0].message.contains("bogus_kind"),
            "violation message should name the bad kind: {}",
            result.violations[0].message
        );
        assert!(
            result.violations[0].message.contains("observation"),
            "violation message should list valid kinds: {}",
            result.violations[0].message
        );
    }

    #[test]
    fn valid_note_kinds_all_pass() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_notes(
            &kg_dir,
            &[
                ("note-0001", "observation"),
                ("note-0002", "insight"),
                ("note-0003", "question"),
                ("note-0004", "decision"),
                ("note-0005", "reference"),
                ("note-0006", "task"),
                ("note-0007", "memory"),
            ],
        );
        let kinds = base_note_kinds();
        let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &kinds);
        assert!(result.passed, "all registered note kinds must pass");
        assert!(result.violations.is_empty());
    }

    #[test]
    fn note_kind_task_is_accepted_as_pack_registered() {
        // `task` is registered by the GTD pack — must be accepted by the registry check.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_notes(&kg_dir, &[("note-0001", "task")]);
        let taxonomy = real_taxonomy();
        assert!(
            taxonomy.note_kinds.contains("task"),
            "VerbRegistry must include 'task' from GTD pack"
        );
        let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &taxonomy.note_kinds);
        assert!(
            result.passed,
            "pack-registered note kind 'task' must pass; violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn note_kind_memory_is_accepted_as_pack_registered() {
        // `memory` is registered by the memory pack — must be accepted by the registry check.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_notes(&kg_dir, &[("note-0001", "memory")]);
        let taxonomy = real_taxonomy();
        assert!(
            taxonomy.note_kinds.contains("memory"),
            "VerbRegistry must include 'memory' from memory pack"
        );
        let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &taxonomy.note_kinds);
        assert!(
            result.passed,
            "pack-registered note kind 'memory' must pass; violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn structural_checks_skips_note_check_when_notes_file_absent() {
        // Without notes.ndjson present, structural_checks must not add a note-kind rule.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let taxonomy = real_taxonomy();
        let results = structural_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &taxonomy,
        );
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(
            !ids.contains(&"valid-note-kinds"),
            "valid-note-kinds must not appear when notes.ndjson is absent"
        );
    }

    #[test]
    fn structural_checks_includes_note_check_when_notes_file_present() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        write_notes(&kg_dir, &[("note-0001", "observation")]);
        let taxonomy = real_taxonomy();
        let results = structural_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &taxonomy,
        );
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"valid-note-kinds"),
            "valid-note-kinds must appear when notes.ndjson is present"
        );
    }

    // ── [Medium] Record identifier in rendered output ─────────────────────────

    #[test]
    fn violation_message_includes_entity_id_and_name() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[(
                "bbbbbbbb-0000-0000-0000-000000000002",
                "nonsense",
                "BadEntity",
            )],
        );
        let kinds = base_entity_kinds();
        let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
        assert!(!result.passed);
        let msg = &result.violations[0].message;
        assert!(
            msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
            "violation message must include entity id: {msg}"
        );
        assert!(
            msg.contains("BadEntity"),
            "violation message must include entity name: {msg}"
        );
    }

    #[test]
    fn edge_violation_message_includes_source_target() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "not_a_real_relation",
            )],
        );
        let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
        assert!(!result.passed);
        let msg = &result.violations[0].message;
        assert!(
            msg.contains("aaaaaaaa-0000-0000-0000-000000000001"),
            "edge violation message must include source_id: {msg}"
        );
        assert!(
            msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
            "edge violation message must include target_id: {msg}"
        );
    }

    // ── Edge relation tests (preserved from prior PR) ─────────────────────────

    #[test]
    fn invalid_edge_relation_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[
                (
                    "aaaaaaaa-0000-0000-0000-000000000001",
                    "bbbbbbbb-0000-0000-0000-000000000002",
                    "extends",
                ),
                (
                    "aaaaaaaa-0000-0000-0000-000000000001",
                    "bbbbbbbb-0000-0000-0000-000000000002",
                    "not_a_real_relation",
                ),
            ],
        );
        let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
        assert!(!result.passed, "invalid edge relation must fail");
        assert_eq!(result.violations.len(), 1);
        assert!(
            result.violations[0].message.contains("not_a_real_relation"),
            "violation message should name the bad relation: {}",
            result.violations[0].message
        );
        assert!(
            result.violations[0].message.contains("extends"),
            "violation message should list valid relations: {}",
            result.violations[0].message
        );
    }

    #[test]
    fn valid_edge_relations_all_pass() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[
                (
                    "aaaaaaaa-0000-0000-0000-000000000001",
                    "bbbbbbbb-0000-0000-0000-000000000002",
                    "extends",
                ),
                (
                    "aaaaaaaa-0000-0000-0000-000000000001",
                    "bbbbbbbb-0000-0000-0000-000000000002",
                    "variant_of",
                ),
            ],
        );
        let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
        assert!(result.passed, "valid edge relations must pass");
        assert!(result.violations.is_empty());
    }

    #[test]
    fn structural_checks_include_taxonomy_rules() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "nonsense", "Bad")],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "aaaaaaaa-0000-0000-0000-000000000001",
                "not_valid",
            )],
        );
        let taxonomy = real_taxonomy();
        let results = structural_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &taxonomy,
        );
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"valid-entity-kinds"),
            "structural_checks must include valid-entity-kinds"
        );
        assert!(
            ids.contains(&"valid-edge-relations"),
            "structural_checks must include valid-edge-relations"
        );
        let entity_kind_result = results
            .iter()
            .find(|r| r.id == "valid-entity-kinds")
            .unwrap();
        assert!(!entity_kind_result.passed, "nonsense kind must fail");
        let edge_rel_result = results
            .iter()
            .find(|r| r.id == "valid-edge-relations")
            .unwrap();
        assert!(!edge_rel_result.passed, "invalid relation must fail");
    }

    // ── edge-endpoint-types ────────────────────────────────────────────────────

    #[test]
    fn edge_endpoint_types_passes_base_allowed_pair() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(
            result.passed,
            "concept -[extends]-> concept is base-allowed: {:?}",
            result.violations
        );
    }

    #[test]
    fn edge_endpoint_types_rejects_disallowed_pair() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(
            !result.passed,
            "person -[extends]-> person is not in the base allowlist"
        );
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.severity, "error");
    }

    #[test]
    fn edge_endpoint_types_severity_config_downgrades_to_warning() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "warning".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.severity, "warning");
        assert_eq!(result.violations[0].severity, "warning");
    }

    #[test]
    fn edge_endpoint_types_accepts_pack_extended_note_to_note_pair() {
        // GTD-shaped pack rule: depends_on between two `task` notes. Proves
        // `check_edge_endpoint_types` genuinely consults `pack_rules` (via the
        // reused `endpoint_matches`), not just the base entity-only table.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        std::fs::write(kg_dir.join("entities.ndjson"), "").unwrap();
        write_notes(
            &kg_dir,
            &[
                ("task-0001-0000-0000-0000-000000000001", "task"),
                ("task-0002-0000-0000-0000-000000000002", "task"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "task-0001-0000-0000-0000-000000000001",
                "task-0002-0000-0000-0000-000000000002",
                "depends_on",
            )],
        );
        let pack_rules = vec![EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::NoteOfKind("task"),
            target: EndpointKind::NoteOfKind("task"),
        }];
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &pack_rules,
            &cfg,
        );
        assert!(
            result.passed,
            "pack-extended task->task depends_on must pass: {:?}",
            result.violations
        );
    }

    #[test]
    fn edge_endpoint_types_rejects_entity_annotates_edge_endpoint() {
        // Regression for the edge-substrate endpoint bypass (codex re-review
        // of 4e11ee38, High-1): a `concept -[annotates]-> <edge_id>` edge must
        // fail — `annotates` requires a NOTE source (operations.rs:1226-1236),
        // and an entity source is invalid regardless of what the target is.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
                ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
            ],
        );
        std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                // The referenced edge — gives us a known edge_id to target.
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // concept -[annotates]-> <edge_id>: must be rejected.
                r#"{"source_id":"cccccccc-0000-0000-0000-000000000003","target_id":"edge-0000-0000-0000-000000000099","relation":"annotates"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(
            !result.passed,
            "entity -[annotates]-> edge must fail (annotates source must be a note)"
        );
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn edge_endpoint_types_rejects_edge_as_endpoint_of_extends() {
        // Regression for the edge-substrate endpoint bypass (High-1): a
        // non-`annotates` relation naming a known edge ID as an endpoint must
        // fail — every relation other than `annotates` requires entity
        // endpoints (operations.rs:1355-1402); an edge endpoint is invalid
        // regardless of pack `EDGE_RULES` (`endpoint_matches` never matches
        // substrate `"edge"`).
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
                ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
            ],
        );
        std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // <edge_id> -[extends]-> concept: must be rejected.
                r#"{"source_id":"edge-0000-0000-0000-000000000099","target_id":"cccccccc-0000-0000-0000-000000000003","relation":"extends"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(
            !result.passed,
            "edge -[extends]-> entity must fail (extends requires entity endpoints)"
        );
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn edge_endpoint_types_accepts_note_annotates_edge_endpoint() {
        // Runtime parity (operations.rs:1249-1254): `annotates` target may be
        // ANY substrate, including an edge — only the SOURCE must be a note.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_notes(
            &kg_dir,
            &[("note0001-0000-0000-0000-000000000001", "observation")],
        );
        std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // note -[annotates]-> <edge_id>: must pass.
                r#"{"source_id":"note0001-0000-0000-0000-000000000001","target_id":"edge-0000-0000-0000-000000000099","relation":"annotates"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let cfg = EdgeEndpointTypesConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_edge_endpoint_types(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &[],
            &cfg,
        );
        assert!(
            result.passed,
            "note -[annotates]-> edge must pass: {:?}",
            result.violations
        );
    }

    // ── edge-direction-conventions ─────────────────────────────────────────────

    fn direction_cfg(severity: &str) -> EdgeDirectionConventionsConfig {
        EdgeDirectionConventionsConfig {
            enabled: true,
            severity: severity.to_owned(),
            relations: vec![DirectionRuleConfig {
                relation: "introduced_by".into(),
                forward_source_kinds: vec!["concept".into(), "artifact".into()],
                forward_target_kinds: vec!["document".into(), "person".into()],
            }],
        }
    }

    #[test]
    fn edge_direction_conventions_passes_forward_direction() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "document", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "introduced_by",
            )],
        );
        let cfg = direction_cfg("warning");
        let result = check_edge_direction_conventions(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(
            result.passed,
            "concept -[introduced_by]-> document is the forward direction: {:?}",
            result.violations
        );
    }

    #[test]
    fn edge_direction_conventions_flags_reversed_direction() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "document", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "introduced_by",
            )],
        );
        let cfg = direction_cfg("warning");
        let result = check_edge_direction_conventions(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(
            !result.passed,
            "document -[introduced_by]-> concept matches the reversed pattern"
        );
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.severity, "warning");
    }

    #[test]
    fn edge_direction_conventions_severity_config_escalates_to_error() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "document", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "introduced_by",
            )],
        );
        let cfg = direction_cfg("error");
        let result = check_edge_direction_conventions(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.severity, "error");
        assert_eq!(result.violations[0].severity, "error");
    }

    // ── dangling-refs ──────────────────────────────────────────────────────────

    #[test]
    fn dangling_refs_passes_when_all_endpoints_resolve() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let cfg = DanglingRefsConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_dangling_refs(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(result.passed, "{:?}", result.violations);
    }

    #[test]
    fn dangling_refs_flags_unresolved_target_and_names_it_not_in_dataset() {
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
        let cfg = DanglingRefsConfig {
            enabled: true,
            severity: "error".into(),
        };
        let result = check_dangling_refs(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert!(
            result.violations[0].message.contains("not in dataset"),
            "message must distinguish dataset-scoped resolution: {}",
            result.violations[0].message
        );
    }

    #[test]
    fn dangling_refs_severity_config_downgrades_to_info() {
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
        let cfg = DanglingRefsConfig {
            enabled: true,
            severity: "info".into(),
        };
        let result = check_dangling_refs(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.severity, "info");
    }

    // ── naming-conventions ─────────────────────────────────────────────────────

    fn naming_cfg(severity: &str) -> NamingConventionsConfig {
        NamingConventionsConfig {
            enabled: true,
            severity: severity.to_owned(),
            max_length: 20,
            no_leading_trailing_whitespace: true,
            no_parenthetical_suffix: true,
            kinds: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn naming_conventions_passes_clean_name() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "Clean")],
        );
        let cfg = naming_cfg("warning");
        let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
        assert!(result.passed, "{:?}", result.violations);
    }

    #[test]
    fn naming_conventions_flags_whitespace_and_parenthetical_suffix() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":" Foo (2024 paper) "}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = naming_cfg("warning");
        let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
        assert!(!result.passed);
        // Both the whitespace and parenthetical-suffix predicates fire.
        assert_eq!(result.violations.len(), 2, "{:?}", result.violations);
    }

    #[test]
    fn naming_conventions_flags_empty_name() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities =
            r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":"   "}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = naming_cfg("warning");
        let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].message.contains("empty"));
    }

    #[test]
    fn naming_conventions_max_length_kind_override() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        // 12 chars — passes the global max_length=20 but fails a per-kind
        // override of 5 for "concept".
        write_entities(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "concept",
                "TwelveChars!",
            )],
        );
        let mut cfg = naming_cfg("warning");
        cfg.kinds.insert(
            "concept".to_string(),
            NamingConventionsOverride {
                max_length: Some(5),
                no_leading_trailing_whitespace: None,
                no_parenthetical_suffix: None,
            },
        );
        let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
        assert!(!result.passed, "per-kind max_length override must apply");
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].message.contains("max length 5"));
    }

    #[test]
    fn naming_conventions_severity_config_is_error() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities =
            r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":" Bad "}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = naming_cfg("error");
        let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
        assert!(!result.passed);
        assert_eq!(result.severity, "error");
    }

    // ── citation-date-lint ─────────────────────────────────────────────────────

    fn citation_cfg(severity: &str) -> CitationDateLintConfig {
        CitationDateLintConfig {
            enabled: true,
            severity: severity.to_owned(),
            fields: vec!["year".into(), "date".into()],
        }
    }

    #[test]
    fn citation_date_lint_passes_past_year() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":2020}}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = citation_cfg("warning");
        let result = check_citation_date_lint(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &cfg,
        );
        assert!(result.passed, "{:?}", result.violations);
    }

    #[test]
    fn citation_date_lint_flags_forward_dated_year() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":9999}}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = citation_cfg("warning");
        let result = check_citation_date_lint(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].message.contains("9999"));
    }

    #[test]
    fn citation_date_lint_flags_forward_dated_iso_date_on_a_note() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        std::fs::write(kg_dir.join("entities.ndjson"), "").unwrap();
        let notes = r#"{"id":"note-0001","kind":"observation","properties":{"date":"2999-01-01"}}"#;
        std::fs::write(kg_dir.join("notes.ndjson"), notes.to_owned() + "\n").unwrap();
        let cfg = citation_cfg("warning");
        let result = check_citation_date_lint(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &cfg,
        );
        assert!(!result.passed, "note properties must be checked too");
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn citation_date_lint_severity_config_is_error() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":9999}}"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
        let cfg = citation_cfg("error");
        let result = check_citation_date_lint(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &cfg,
        );
        assert!(!result.passed);
        assert_eq!(result.severity, "error");
    }

    // ── rules.toml wiring through configurable_rule_checks ────────────────────

    #[test]
    fn configurable_rule_checks_wires_up_edge_endpoint_types_from_toml() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "[edge_endpoint_types]\nenabled = true\n").unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "edge-endpoint-types");
        // Default severity for this class is "error" (see default_severity_error).
        assert_eq!(results[0].severity, "error");
        assert!(!results[0].passed);
    }

    #[test]
    fn configurable_rule_checks_section_absent_means_rule_does_not_run() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "rules = []\n").unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert!(
            results.is_empty(),
            "no built-in rule-class sections declared → none run: {results:?}"
        );
    }

    #[test]
    fn configurable_rule_checks_enabled_false_skips_the_rule() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
            ],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(
            &rules_path,
            "[edge_endpoint_types]\nenabled = false\nseverity = \"error\"\n",
        )
        .unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert!(results.is_empty(), "enabled = false must skip evaluation");
    }

    #[test]
    fn configurable_rule_checks_invalid_builtin_severity_produces_error_result() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(
            &rules_path,
            "[naming_conventions]\nseverity = \"catastrophic\"\n",
        )
        .unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert_eq!(results[0].severity, "error");
        assert!(results[0].violations[0]
            .message
            .contains("invalid severity"));
    }

    #[test]
    fn configurable_rule_checks_misspelled_key_fails_the_load() {
        // Regression for High-2 (codex re-review of 4e11ee38): before
        // `#[serde(deny_unknown_fields)]`, a typo like `severtiy` was silently
        // ignored and the field fell back to its default — the class then ran
        // at the DEFAULT severity instead of failing loudly. Now the whole
        // `rules.toml` load must error.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", " Bad ")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "[naming_conventions]\nsevertiy = \"error\"\n").unwrap();

        let err = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .expect_err("a misspelled key must fail the rules.toml load, not silently default");
        assert!(
            format!("{err:#}").contains("severtiy") || format!("{err:#}").contains("unknown field"),
            "error must name the bad key: {err:#}"
        );
    }

    #[test]
    fn configurable_rule_checks_malformed_direction_entry_fails_the_load() {
        // Regression for High-2: `forward_source_kind` (missing the trailing
        // `s`) is not a field `DirectionRuleConfig` recognizes. Before this
        // fix it silently parsed as an unrelated no-op (an entry with an
        // empty `forward_source_kinds` that `check_edge_direction_conventions`
        // skips), producing a green validation for a config that names no
        // real direction rule at all.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(
            &rules_path,
            "[[edge_direction_conventions.relations]]\n\
             relation = \"introduced_by\"\n\
             forward_source_kind = [\"concept\"]\n\
             forward_target_kinds = [\"document\"]\n",
        )
        .unwrap();

        let err = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .expect_err("a misspelled direction-entry field must fail the rules.toml load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("forward_source_kind") || msg.contains("unknown field"),
            "error must name the bad key: {msg}"
        );
    }

    #[test]
    fn configurable_rule_checks_direction_entry_with_empty_kind_list_fails_the_load() {
        // Post-parse validation (High-2): a syntactically valid but
        // semantically empty `forward_source_kinds = []` must also fail the
        // load loudly, not silently no-op the whole entry.
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(
            &rules_path,
            "[[edge_direction_conventions.relations]]\n\
             relation = \"introduced_by\"\n\
             forward_source_kinds = []\n\
             forward_target_kinds = [\"document\"]\n",
        )
        .unwrap();

        let err = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &kg_dir.join("notes.ndjson"),
            &rules_path,
        )
        .expect_err("an empty forward_source_kinds entry must fail the rules.toml load");
        assert!(
            format!("{err:#}").contains("forward_source_kinds"),
            "error must name the empty field: {err:#}"
        );
    }
}
