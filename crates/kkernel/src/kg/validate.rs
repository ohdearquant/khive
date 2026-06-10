//! `kkernel kg validate` — structural and configurable rule-pass validation.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde::Deserialize;

use super::types::{
    OutputFormat, RuleResult, ValidateArgs, ValidationReport, ValidationSummary, Violation,
};

/// Taxonomy sets derived from the loaded pack registry.
struct KgTaxonomy {
    entity_kinds: HashSet<String>,
    note_kinds: HashSet<String>,
}

/// Build the merged entity-kind and note-kind sets from all registered packs.
///
/// Mirrors the `build_registry()` pattern in `pack_introspect`. No DB is
/// opened — only pack metadata is needed.
fn build_taxonomy() -> Result<KgTaxonomy> {
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

fn structural_checks(
    entities_path: &Path,
    edges_path: &Path,
    notes_path: &Path,
    taxonomy: &KgTaxonomy,
) -> Vec<RuleResult> {
    let mut results = vec![
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

fn check_valid_entity_kinds(entities_path: &Path, valid_kinds: &HashSet<String>) -> RuleResult {
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

fn check_valid_note_kinds(notes_path: &Path, valid_kinds: &HashSet<String>) -> RuleResult {
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
    use khive_storage::EdgeRelation;

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
#[derive(Debug, Deserialize)]
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

/// Load and evaluate configurable rules from a TOML rules file.
pub(super) fn configurable_rule_checks(
    entities_path: &Path,
    edges_path: &Path,
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
}
