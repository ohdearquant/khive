use std::collections::HashMap;

use chrono::Datelike;
use khive_types::{base_entity_rule_allows, endpoint_matches, EdgeEndpointRule, EdgeRelation};

use crate::config::*;
use crate::{EvaluationContext, EvaluationMode, NdjsonState, RuleResult, Violation};

/// Read an edge record's source endpoint id, accepting either canonical
/// serialization spelling: `source` (khive-vcs sync, kkernel archive, and
/// the runtime portability export all write this) or `source_id` (accepted
/// for forward compatibility with any other producer). See #1225 — every
/// canonical NDJSON writer emits `source`/`target`, not `source_id`/
/// `target_id`, and a validator that only recognized the latter silently
/// skipped the endpoint checks on every record those writers produce.
pub fn edge_source_id(v: &serde_json::Value) -> Option<&str> {
    v.get("source")
        .or_else(|| v.get("source_id"))
        .and_then(|x| x.as_str())
}

/// Target-endpoint counterpart of [`edge_source_id`].
pub fn edge_target_id(v: &serde_json::Value) -> Option<&str> {
    v.get("target")
        .or_else(|| v.get("target_id"))
        .and_then(|x| x.as_str())
}

/// Format a record identifier prefix from the available violation fields.
///
/// Produces `"[id name]"` when both are present, `"[id]"` or `"[name]"` when
/// only one is available, and `""` when neither is set.
pub fn record_prefix(entity_id: Option<&str>, entity_name: Option<&str>) -> String {
    match (entity_id, entity_name) {
        (Some(id), Some(name)) => format!("[{id} {name:?}] "),
        (Some(id), None) => format!("[{id}] "),
        (None, Some(name)) => format!("[{name:?}] "),
        (None, None) => String::new(),
    }
}

/// Collect all IDs from in-memory NDJSON into a set; malformed rows are skipped.
///
/// Reads the `"id"` field — suitable for entities.ndjson and notes.ndjson.
pub fn collect_ids(content: &str) -> std::collections::HashSet<String> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("id")?.as_str().map(str::to_string))
        })
        .collect()
}

/// Collect edge record IDs from edges.ndjson into a set.
///
/// Edge records use `"edge_id"` as the canonical key (ADR-002 / portability
/// layer). Older fixtures may use `"id"` instead; both are collected so the
/// referential-integrity check accepts `annotates` targets that point at edges
/// in either serialization form.
pub fn collect_edge_ids(content: &str) -> std::collections::HashSet<String> {
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

/// Evaluate generic rules followed by the five opt-in rule classes, in their configured order.
pub fn evaluate(
    rules: &Rules,
    state: NdjsonState<'_>,
    context: EvaluationContext<'_>,
) -> Vec<RuleResult> {
    let rules_file = &rules.0;
    let entities_path = state.entities;
    let edges_path = state.edges;
    let notes_path = state.notes;
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
                results.push(check_edge_endpoint_types(
                    entities_path,
                    notes_path,
                    edges_path,
                    context.pack_edge_rules,
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
            } else if context.mode == EvaluationMode::FullDataset {
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
                results.push(check_citation_date_lint(
                    entities_path,
                    notes_path,
                    cfg,
                    context.now,
                ));
            }
        }
    }

    results
}
fn evaluate_rule(rule: &RuleConfig, path: &str) -> Vec<Violation> {
    let content = path;

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
    entities_path: &str,
    notes_path: &str,
    edges_path: &str,
) -> HashMap<String, KindInfo> {
    let mut map = HashMap::new();
    {
        let content = entities_path;
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
    {
        let content = notes_path;
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
/// matcher (`khive-types`) — the actual endpoint-pairing DATA lives in
/// `pack_rules` (fetched live from the pack registry by
/// the host), never re-derived here.
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
/// — uses the shared `khive-types` table and host-supplied pack rules (the sources the `link`/
/// `update` verbs enforce against); only this dispatch shape is restated for
/// the offline path, so the allowlist itself cannot drift out of sync.
///
/// Edges whose endpoints don't resolve within `entities.ndjson`/`notes.ndjson`
/// are skipped here — `dangling-refs` and the always-on `referential-integrity`
/// structural check own that failure mode, so this rule does not double-report it.
pub fn check_edge_endpoint_types(
    entities_path: &str,
    notes_path: &str,
    edges_path: &str,
    pack_rules: &[EdgeEndpointRule],
    cfg: &EdgeEndpointTypesConfig,
) -> RuleResult {
    let kind_map = collect_kind_map(entities_path, notes_path, edges_path);
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    {
        let content = edges_path;
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
pub fn check_edge_direction_conventions(
    entities_path: &str,
    notes_path: &str,
    edges_path: &str,
    cfg: &EdgeDirectionConventionsConfig,
) -> RuleResult {
    let kind_map = collect_kind_map(entities_path, notes_path, edges_path);
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    {
        let content = edges_path;
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
pub fn check_dangling_refs(
    entities_path: &str,
    notes_path: &str,
    edges_path: &str,
    cfg: &DanglingRefsConfig,
) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    let mut known_ids = collect_ids(entities_path);
    known_ids.extend(collect_ids(notes_path));
    known_ids.extend(collect_edge_ids(edges_path));

    {
        let content = edges_path;
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
pub fn check_naming_conventions(entities_path: &str, cfg: &NamingConventionsConfig) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    {
        let content = entities_path;
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
pub fn check_citation_date_lint(
    entities_path: &str,
    notes_path: &str,
    cfg: &CitationDateLintConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> RuleResult {
    let sev = severity_static(&cfg.severity);
    let mut violations = Vec::new();

    for path in [entities_path, notes_path] {
        {
            let content = path;
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
