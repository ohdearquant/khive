use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use khive_runtime::EntityCreateSpec;
use khive_storage::EdgeRelation;
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::manifest::{cross_check_llms, Manifest, QuarantinedDeclaration};
use crate::persistence::{WebEdge, WebEntity};
use crate::views::read_view;

pub const WEB_INGEST_NAMESPACE: Uuid = Uuid::from_u128(0x71c1a6f3_8b91_5c7a_a027_9f8f868644a9);

#[derive(Debug, Serialize)]
pub(crate) struct WebIngestReport {
    pub entity_counts: BTreeMap<String, usize>,
    pub relation_counts: BTreeMap<String, usize>,
    pub views_missing: usize,
    pub quarantined: Vec<QuarantinedDeclaration>,
    pub manifest_digest: String,
    pub source: String,
    pub include_views: bool,
    pub ignored_keys: usize,
}

pub(crate) struct Extracted {
    pub entities: Vec<WebEntity>,
    pub edges: Vec<WebEdge>,
    pub report: WebIngestReport,
}

fn identifier(parts: Value) -> Uuid {
    Uuid::new_v5(&WEB_INGEST_NAMESPACE, parts.to_string().as_bytes())
}

fn canonical_path(path: &str) -> Result<String, &'static str> {
    if path.trim().is_empty() || path.contains(['\\', '\0', '?', '#']) || path.contains("://") {
        return Err("url must be a local declared path");
    }
    let path = path.trim_matches('/');
    if path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err("url must not contain dot path segments");
    }
    Ok(format!("/{path}"))
}

impl Extracted {
    fn quarantine(&mut self, field: impl Into<String>, reason: impl Into<String>) {
        self.report.quarantined.push(QuarantinedDeclaration {
            field: field.into(),
            reason: reason.into(),
        });
    }

    fn entity(
        &mut self,
        id: Uuid,
        kind: &str,
        entity_type: &str,
        name: &str,
        declaration: &Value,
        field: &str,
    ) -> bool {
        if self.entities.iter().any(|entity| entity.id == id) {
            self.quarantine(
                field,
                "duplicate declaration has the same canonical identity",
            );
            return false;
        }
        let description = match declaration.get("description") {
            None => None,
            Some(Value::String(description)) => Some(description.clone()),
            Some(_) => {
                self.quarantine(
                    format!("{field}.description"),
                    "description must be a string",
                );
                return false;
            }
        };
        let tags = match declaration.get("tags") {
            None => Vec::new(),
            Some(Value::Array(tags)) if tags.iter().all(Value::is_string) => tags
                .iter()
                .map(|tag| tag.as_str().unwrap().to_string())
                .collect(),
            Some(_) => {
                self.quarantine(format!("{field}.tags"), "tags must be an array of strings");
                return false;
            }
        };
        self.entities.push(WebEntity {
            id,
            spec: EntityCreateSpec {
                kind: kind.to_string(),
                entity_type: Some(entity_type.to_string()),
                name: name.to_string(),
                description,
                properties: Some(declaration.clone()),
                tags,
            },
        });
        *self
            .report
            .entity_counts
            .entry(entity_type.to_string())
            .or_default() += 1;
        true
    }

    fn edge(&mut self, source: Uuid, target: Uuid, relation: EdgeRelation) {
        let id = identifier(json!(["edge", relation.as_str(), source, target]));
        if self.edges.iter().any(|edge| edge.id == id) {
            return;
        }
        self.edges.push(WebEdge {
            id,
            source,
            target,
            relation,
        });
        *self
            .report
            .relation_counts
            .entry(relation.as_str().to_string())
            .or_default() += 1;
    }
}

pub(crate) fn extract(
    source: &Path,
    manifest: Manifest,
    include_views: bool,
) -> Result<Extracted, String> {
    let mut result = Extracted {
        entities: Vec::new(),
        edges: Vec::new(),
        report: WebIngestReport {
            entity_counts: ["site", "page", "machine_view", "agent_tool", "agent_skill"]
                .map(|name| (name.to_string(), 0))
                .into(),
            relation_counts: ["contains", "derived_from", "depends_on", "implements"]
                .map(|name| (name.to_string(), 0))
                .into(),
            views_missing: 0,
            quarantined: cross_check_llms(source, &manifest.raw),
            manifest_digest: manifest.digest,
            source: source.display().to_string(),
            include_views,
            ignored_keys: manifest.ignored_keys,
        },
    };
    let mut site = manifest.raw["site"].clone();
    for field in ["version", "profile", "content_signals"] {
        if let Some(value) = manifest.raw.get(field) {
            site[field] = value.clone();
        }
    }
    let site_id = identifier(json!(["site", manifest.origin]));
    let name = site["name"].as_str().expect("manifest site name validated");
    if !result.entity(site_id, "service", "site", name, &site, "site") {
        return Err("manifest_malformed: unreadable site declaration".to_string());
    }
    for collection in ["content", "tools", "skills"] {
        if manifest
            .raw
            .get(collection)
            .is_some_and(|value| !value.is_array())
        {
            result.quarantine(collection, "declarations must be an array");
        }
    }
    for (index, page) in manifest.raw["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let field = format!("content[{index}]");
        let Some(url) = page.get("url").and_then(Value::as_str) else {
            result.quarantine(format!("{field}.url"), "page url must be a string");
            continue;
        };
        let path = match canonical_path(url) {
            Ok(path) => path,
            Err(reason) => {
                result.quarantine(format!("{field}.url"), reason);
                continue;
            }
        };
        let id = identifier(json!(["page", manifest.origin, path]));
        if !result.entity(id, "document", "page", url, page, &field) {
            continue;
        }
        result.edge(site_id, id, EdgeRelation::Contains);
        if !include_views {
            continue;
        }
        let Some(view) = page.get("markdown_url") else {
            continue;
        };
        let Some(view_url) = view.as_str() else {
            result.quarantine(
                format!("{field}.markdown_url"),
                "markdown_url must be a string",
            );
            continue;
        };
        let view_path = match canonical_path(view_url) {
            Ok(path) => path,
            Err(reason) => {
                result.quarantine(format!("{field}.markdown_url"), reason);
                continue;
            }
        };
        let view_id = identifier(json!(["machine_view", manifest.origin, view_path]));
        if result.entities.iter().any(|entity| entity.id == view_id) {
            // A declared machine view may serve more than one page. Its first
            // accepted frontmatter is the ingest snapshot for that identity.
            result.edge(view_id, id, EdgeRelation::DerivedFrom);
            continue;
        }
        match read_view(source, &view_path) {
            Ok(None) => result.report.views_missing += 1,
            Err(reason) => result.quarantine(format!("{field}.markdown_url"), reason),
            Ok(Some(frontmatter)) => {
                if result.entity(
                    view_id,
                    "document",
                    "machine_view",
                    view_url,
                    &frontmatter,
                    &format!("{field}.markdown_url"),
                ) {
                    result.edge(view_id, id, EdgeRelation::DerivedFrom);
                }
            }
        }
    }
    let mut tools = BTreeMap::new();
    for (index, tool) in manifest.raw["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let field = format!("tools[{index}]");
        let Some(name) = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            result.quarantine(
                format!("{field}.name"),
                "tool name must be a non-empty string",
            );
            continue;
        };
        let id = identifier(json!(["agent_tool", manifest.origin, name]));
        if result.entity(id, "service", "agent_tool", name, tool, &field) {
            result.edge(site_id, id, EdgeRelation::Contains);
            tools.insert(name, id);
        }
    }
    for (index, skill) in manifest.raw["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let field = format!("skills[{index}]");
        let Some(name) = skill
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            result.quarantine(
                format!("{field}.name"),
                "skill name must be a non-empty string",
            );
            continue;
        };
        let id = identifier(json!(["agent_skill", manifest.origin, name]));
        if !result.entity(id, "document", "agent_skill", name, skill, &field) {
            continue;
        }
        result.edge(site_id, id, EdgeRelation::Contains);
        let Some(required) = skill.get("tools_required") else {
            continue;
        };
        let Some(required) = required.as_array() else {
            result.quarantine(
                format!("{field}.tools_required"),
                "tools_required must be an array of names",
            );
            continue;
        };
        let mut seen = HashSet::new();
        for (index, name) in required.iter().enumerate() {
            let reference = format!("{field}.tools_required[{index}]");
            let Some(name) = name.as_str() else {
                result.quarantine(reference, "tool reference must be a string");
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            match tools.get(name) {
                Some(tool) => result.edge(id, *tool, EdgeRelation::DependsOn),
                None => result.quarantine(
                    reference,
                    "tool reference has no readable declaration in this manifest",
                ),
            }
        }
    }
    Ok(result)
}
