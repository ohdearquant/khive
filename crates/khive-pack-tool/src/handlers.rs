//! Verb handlers for the tool pack.

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::Direction;
use khive_types::{EdgeRelation, Entity, VerbCategory, Visibility};

use crate::policy::{self, actor_label, now_micros, Decision};
use crate::vocab::{
    CAPABILITY_TAG, DECISIONS, KINDS, REGISTRY_ENTITY_KIND, REGISTRY_TAG, SIDE_EFFECTS,
    TRUST_ORIGINS,
};

// ── parameter helpers ────────────────────────────────────────────────────────

fn opt_str(params: &Value, key: &str) -> Result<Option<String>, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Err(RuntimeError::InvalidInput(
            format!("{key} must be a non-empty string when provided"),
        )),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string; got {other}"
        ))),
    }
}

fn req_str(params: &Value, key: &str) -> Result<String, RuntimeError> {
    opt_str(params, key)?
        .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} is required")))
}

fn opt_u32(params: &Value, key: &str, default: u32, max: u32) -> Result<u32, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| (n as u32).clamp(1, max))
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a non-negative integer"))),
    }
}

fn opt_i64(params: &Value, key: &str) -> Result<Option<i64>, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be an integer"))),
    }
}

fn opt_str_list(params: &Value, key: &str) -> Result<Vec<String>, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "{key} must be an array of non-empty strings"
                        ))
                    })
            })
            .collect(),
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn one_of(value: &str, allowed: &[&str], what: &str) -> Result<(), RuntimeError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{what} must be one of {}; got {value:?}",
            allowed.join(", ")
        )))
    }
}

// ── entity helpers ───────────────────────────────────────────────────────────

fn entity_uuid(e: &Entity) -> Uuid {
    Uuid::from_bytes(*e.header.id.as_bytes())
}

fn props_json(e: &Entity) -> Value {
    serde_json::to_value(&e.properties).unwrap_or(Value::Null)
}

fn prop_str(props: &Value, key: &str) -> Option<String> {
    props.get(key).and_then(Value::as_str).map(str::to_string)
}

fn summary(e: &Entity) -> Value {
    let props = props_json(e);
    let id = entity_uuid(e).to_string();
    json!({
        "id": id[..8].to_string(),
        "full_id": id,
        "name": e.name,
        "kind": e.entity_type,
        "description": e.description,
        "source": prop_str(&props, "source"),
        "side_effect": prop_str(&props, "side_effect"),
        "trust": prop_str(&props, "trust"),
        "tags": e.tags,
    })
}

fn full(e: &Entity) -> Value {
    let mut v = summary(e);
    let props = props_json(e);
    v["schema"] = props.get("schema").cloned().unwrap_or(Value::Null);
    v["properties"] = props;
    v["created_at"] = json!(micros_to_iso(e.header.created_at.as_micros() as i64));
    v["updated_at"] = json!(micros_to_iso(e.header.updated_at.as_micros() as i64));
    v
}

fn side_effect_of(e: &Entity) -> Option<String> {
    prop_str(&props_json(e), "side_effect")
}

async fn registry_entities(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    limit: u32,
    offset: u32,
) -> Result<Vec<Entity>, RuntimeError> {
    rt.list_entities_tagged(token, Some(REGISTRY_ENTITY_KIND), Some(REGISTRY_TAG), limit, offset)
        .await
}

async fn find_by_name(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
) -> Result<Option<Entity>, RuntimeError> {
    let all = registry_entities(rt, token, 5000, 0).await?;
    if let Some(e) = all.iter().find(|e| e.name == name) {
        return Ok(Some(e.clone()));
    }
    Ok(all
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name)))
}

async fn resolve_tool(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    reference: &str,
) -> Result<Entity, RuntimeError> {
    if let Ok(id) = Uuid::parse_str(reference) {
        if let Ok(e) = rt.get_entity(token, id).await {
            return Ok(e);
        }
    }
    if reference.len() >= 8 && reference.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        if let Ok(Some(id)) = rt.resolve_prefix(token, reference).await {
            if let Ok(e) = rt.get_entity(token, id).await {
                return Ok(e);
            }
        }
    }
    find_by_name(rt, token, reference)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("tool {reference:?} is not registered")))
}

async fn ensure_capability(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
) -> Result<Entity, RuntimeError> {
    let existing = rt
        .list_entities_tagged(token, Some("concept"), Some(CAPABILITY_TAG), 5000, 0)
        .await?;
    if let Some(e) = existing
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
    {
        return Ok(e);
    }
    rt.create_entity(
        token,
        "concept",
        Some("capability"),
        name,
        None,
        None,
        vec![CAPABILITY_TAG.to_string()],
    )
    .await
}

async fn link_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    tool_id: Uuid,
    capability_id: Uuid,
) -> Result<(), RuntimeError> {
    match rt
        .link(token, tool_id, capability_id, EdgeRelation::Implements, 1.0, None)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string().to_ascii_lowercase();
            if msg.contains("exist") || msg.contains("duplicate") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

async fn capabilities_of(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    tool_id: Uuid,
) -> Result<Vec<Value>, RuntimeError> {
    let hits = rt
        .neighbors(
            token,
            tool_id,
            Direction::Out,
            Some(100),
            Some(vec![EdgeRelation::Implements]),
        )
        .await?;
    Ok(hits
        .into_iter()
        .map(|h| json!({ "id": h.node_id.to_string(), "name": h.name }))
        .collect())
}

// ── register / ingest ────────────────────────────────────────────────────────

struct RegisterSpec {
    name: String,
    kind: String,
    description: Option<String>,
    schema: Option<Value>,
    source: Option<String>,
    side_effect: String,
    trust: String,
    capabilities: Vec<String>,
    tags: Vec<String>,
}

async fn register_one(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    spec: RegisterSpec,
) -> Result<(Entity, bool, Vec<Value>), RuntimeError> {
    one_of(&spec.kind, KINDS, "kind")?;
    one_of(&spec.side_effect, SIDE_EFFECTS, "side_effect")?;
    one_of(&spec.trust, TRUST_ORIGINS, "trust")?;

    let (entity, created) = match find_by_name(rt, token, &spec.name).await? {
        Some(existing) => (existing, false),
        None => {
            let mut props = serde_json::Map::new();
            props.insert("side_effect".into(), json!(spec.side_effect));
            props.insert("trust".into(), json!(spec.trust));
            props.insert("registered_at".into(), json!(micros_to_iso(now_micros())));
            if let Some(source) = &spec.source {
                props.insert("source".into(), json!(source));
            }
            if let Some(schema) = &spec.schema {
                props.insert("schema".into(), schema.clone());
            }
            let mut tags = vec![REGISTRY_TAG.to_string(), spec.kind.clone()];
            for t in &spec.tags {
                if !tags.contains(t) {
                    tags.push(t.clone());
                }
            }
            let e = rt
                .create_entity(
                    token,
                    REGISTRY_ENTITY_KIND,
                    Some(&spec.kind),
                    &spec.name,
                    spec.description.as_deref(),
                    Some(Value::Object(props)),
                    tags,
                )
                .await?;
            (e, true)
        }
    };

    let tool_id = entity_uuid(&entity);
    let mut caps = Vec::new();
    for name in &spec.capabilities {
        let cap = ensure_capability(rt, token, name).await?;
        let cap_id = entity_uuid(&cap);
        link_implements(rt, token, tool_id, cap_id).await?;
        caps.push(json!({ "id": cap_id.to_string(), "name": cap.name }));
    }
    Ok((entity, created, caps))
}

pub(crate) async fn register(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let schema = match params.get("schema") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(_) => {
            return Err(RuntimeError::InvalidInput(
                "schema must be an object".into(),
            ))
        }
    };
    let spec = RegisterSpec {
        name: req_str(&params, "name")?,
        kind: opt_str(&params, "kind")?.unwrap_or_else(|| "tool".into()),
        description: opt_str(&params, "description")?,
        schema,
        source: opt_str(&params, "source")?,
        side_effect: opt_str(&params, "side_effect")?.unwrap_or_else(|| "write".into()),
        trust: opt_str(&params, "trust")?.unwrap_or_else(|| "external".into()),
        capabilities: opt_str_list(&params, "capabilities")?,
        tags: opt_str_list(&params, "tags")?,
    };
    let (entity, created, capabilities) = register_one(rt, token, spec).await?;
    Ok(json!({
        "ok": true,
        "created": created,
        "tool": summary(&entity),
        "capabilities": capabilities,
    }))
}

pub(crate) async fn ingest(
    rt: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let source = req_str(&params, "source")?;
    let trust = opt_str(&params, "trust")?;
    let mut registered = 0usize;
    let mut existing = 0usize;
    let mut names: Vec<String> = Vec::new();

    match source.as_str() {
        "khive" => {
            let handlers: Vec<(String, String, String, VerbCategory)> = registry
                .all_handlers_with_names()
                .into_iter()
                .filter(|(_, def)| matches!(def.visibility, Visibility::Verb))
                .map(|(pack, def)| {
                    (
                        pack.to_string(),
                        def.name.to_string(),
                        def.description.to_string(),
                        def.category,
                    )
                })
                .collect();
            for (pack, name, description, category) in handlers {
                let side_effect = match category {
                    VerbCategory::Assertive => "read",
                    _ => "write",
                };
                let spec = RegisterSpec {
                    name: name.clone(),
                    kind: "verb".into(),
                    description: Some(description),
                    schema: None,
                    source: Some(format!("khive:{pack}")),
                    side_effect: side_effect.into(),
                    trust: trust.clone().unwrap_or_else(|| "first_party".into()),
                    capabilities: vec![pack.clone()],
                    tags: vec![pack],
                };
                let (_, created, _) = register_one(rt, token, spec).await?;
                if created {
                    registered += 1;
                    names.push(name);
                } else {
                    existing += 1;
                }
            }
        }
        "mcp" => {
            let server = req_str(&params, "server")?;
            let tools = params
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("tools must be an array of tool objects".into())
                })?
                .clone();
            for tool in tools {
                let name = req_str(&tool, "name")?;
                let spec = RegisterSpec {
                    name: name.clone(),
                    kind: "tool".into(),
                    description: opt_str(&tool, "description")?,
                    schema: tool
                        .get("inputSchema")
                        .or_else(|| tool.get("schema"))
                        .filter(|v| v.is_object())
                        .cloned(),
                    source: Some(format!("mcp:{server}")),
                    side_effect: opt_str(&tool, "side_effect")?.unwrap_or_else(|| "write".into()),
                    trust: trust.clone().unwrap_or_else(|| "external".into()),
                    capabilities: opt_str_list(&tool, "capabilities")?,
                    tags: vec![format!("mcp:{server}")],
                };
                let (_, created, _) = register_one(rt, token, spec).await?;
                if created {
                    registered += 1;
                    names.push(name);
                } else {
                    existing += 1;
                }
            }
        }
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "source must be khive or mcp; got {other:?}"
            )))
        }
    }

    Ok(json!({
        "ok": true,
        "source": source,
        "registered": registered,
        "existing": existing,
        "names": names,
    }))
}

// ── discovery ────────────────────────────────────────────────────────────────

pub(crate) async fn suggest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let query = req_str(&params, "query")?;
    let limit = opt_u32(&params, "limit", 10, 50)?;
    let kind = opt_str(&params, "kind")?;
    if let Some(k) = &kind {
        one_of(k, KINDS, "kind")?;
    }
    let actor = opt_str(&params, "actor")?.unwrap_or_else(|| actor_label(token));
    let ns = token.namespace().as_str().to_string();

    let query_vector = rt.embed_query(&query).await.ok();
    let registry_tags = vec![REGISTRY_TAG.to_string()];
    let direct = rt
        .hybrid_search(
            token,
            &query,
            query_vector.clone(),
            limit.saturating_mul(2),
            Some(REGISTRY_ENTITY_KIND),
            kind.as_deref(),
            &registry_tags,
            None,
        )
        .await?;

    // (score, capability names the hit was reached through)
    let mut scored: HashMap<Uuid, (f64, Vec<String>)> = HashMap::new();
    for hit in &direct {
        let score = hit.score.to_f64();
        let entry = scored.entry(hit.entity_id).or_insert((0.0, vec![]));
        if score > entry.0 {
            entry.0 = score;
        }
    }

    let capability_tags = vec![CAPABILITY_TAG.to_string()];
    let concepts = rt
        .hybrid_search(
            token,
            &query,
            query_vector,
            5,
            Some("concept"),
            Some("capability"),
            &capability_tags,
            None,
        )
        .await
        .unwrap_or_default();
    let mut via: Vec<Value> = Vec::new();
    for concept in &concepts {
        let cap_name = concept.title.clone().unwrap_or_default();
        let implementers = rt
            .neighbors(
                token,
                concept.entity_id,
                Direction::In,
                Some(50),
                Some(vec![EdgeRelation::Implements]),
            )
            .await
            .unwrap_or_default();
        via.push(json!({
            "id": concept.entity_id.to_string(),
            "name": cap_name,
            "score": concept.score.to_f64(),
            "implementers": implementers.len(),
        }));
        let derived = concept.score.to_f64() * 0.9;
        for hit in implementers {
            let entry = scored.entry(hit.node_id).or_insert((0.0, vec![]));
            if derived > entry.0 {
                entry.0 = derived;
            }
            if !entry.1.contains(&cap_name) {
                entry.1.push(cap_name.clone());
            }
        }
    }

    let mut ranked: Vec<(Uuid, f64, Vec<String>)> = scored
        .into_iter()
        .map(|(id, (score, caps))| (id, score, caps))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut results = Vec::new();
    for (id, score, caps) in ranked {
        if results.len() >= limit as usize {
            break;
        }
        let Ok(entity) = rt.get_entity(token, id).await else {
            continue;
        };
        if !entity.tags.iter().any(|t| t == REGISTRY_TAG) {
            continue;
        }
        if let Some(k) = &kind {
            if entity.entity_type.as_deref() != Some(k.as_str()) {
                continue;
            }
        }
        let decision =
            policy::decide(rt, &ns, &actor, &entity.name, side_effect_of(&entity).as_deref())
                .await?;
        let mut item = summary(&entity);
        item["score"] = json!(score);
        item["via"] = json!(caps);
        item["decision"] = decision.to_json();
        results.push(item);
    }

    Ok(json!({
        "ok": true,
        "query": query,
        "actor": actor,
        "count": results.len(),
        "results": results,
        "capabilities": via,
    }))
}

pub(crate) async fn describe(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let reference = req_str(&params, "tool")?;
    let actor = opt_str(&params, "actor")?.unwrap_or_else(|| actor_label(token));
    let ns = token.namespace().as_str().to_string();
    let entity = resolve_tool(rt, token, &reference).await?;
    let capabilities = capabilities_of(rt, token, entity_uuid(&entity)).await?;
    let decision =
        policy::decide(rt, &ns, &actor, &entity.name, side_effect_of(&entity).as_deref()).await?;
    let mut v = full(&entity);
    v["capabilities"] = json!(capabilities);
    v["decision"] = decision.to_json();
    v["actor"] = json!(actor);
    Ok(json!({ "ok": true, "tool": v }))
}

pub(crate) async fn list(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let kind = opt_str(&params, "kind")?;
    if let Some(k) = &kind {
        one_of(k, KINDS, "kind")?;
    }
    let limit = opt_u32(&params, "limit", 100, 1000)?;
    let offset = match params.get("offset") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_u64().map(|n| n.min(u32::MAX as u64) as u32).ok_or_else(|| {
            RuntimeError::InvalidInput("offset must be a non-negative integer".into())
        })?,
    };
    let entities = registry_entities(rt, token, limit, offset).await?;
    let tools: Vec<Value> = entities
        .iter()
        .filter(|e| match &kind {
            Some(k) => e.entity_type.as_deref() == Some(k.as_str()),
            None => true,
        })
        .map(summary)
        .collect();
    Ok(json!({
        "ok": true,
        "count": tools.len(),
        "limit": limit,
        "offset": offset,
        "tools": tools,
    }))
}

// ── policy and grants ────────────────────────────────────────────────────────

async fn decision_for(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    reference: &str,
    actor: &str,
) -> Result<(String, bool, Decision), RuntimeError> {
    let ns = token.namespace().as_str().to_string();
    match resolve_tool(rt, token, reference).await {
        Ok(entity) => {
            let d = policy::decide(rt, &ns, actor, &entity.name, side_effect_of(&entity).as_deref())
                .await?;
            Ok((entity.name.clone(), true, d))
        }
        Err(RuntimeError::NotFound(_)) => {
            let d = policy::decide(rt, &ns, actor, reference, None).await?;
            Ok((reference.to_string(), false, d))
        }
        Err(e) => Err(e),
    }
}

pub(crate) async fn check(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let reference = req_str(&params, "tool")?;
    let actor = opt_str(&params, "actor")?.unwrap_or_else(|| actor_label(token));
    let (name, registered, decision) = decision_for(rt, token, &reference, &actor).await?;
    let mut v = decision.to_json();
    v["ok"] = json!(true);
    v["tool"] = json!(name);
    v["actor"] = json!(actor);
    v["registered"] = json!(registered);
    Ok(v)
}

pub(crate) async fn request(
    rt: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let reference = req_str(&params, "tool")?;
    let actor = opt_str(&params, "actor")?.unwrap_or_else(|| actor_label(token));
    let scope = opt_str(&params, "scope")?;
    let reason = opt_str(&params, "reason")?;
    let notify = opt_str(&params, "notify")?;
    let ns = token.namespace().as_str().to_string();

    let (name, registered, decision) = decision_for(rt, token, &reference, &actor).await?;
    if decision.decision == "allow" {
        let mut v = decision.to_json();
        v["ok"] = json!(true);
        v["tool"] = json!(name);
        v["actor"] = json!(actor);
        v["registered"] = json!(registered);
        v["request_id"] = Value::Null;
        return Ok(v);
    }

    let row = policy::insert_grant_request(
        rt,
        &ns,
        &actor,
        &name,
        scope.as_deref(),
        reason.as_deref(),
    )
    .await?;

    let mut notified = false;
    if let Some(to) = notify {
        if registry.has_verb("comm.send") {
            let content = format!(
                "Tool-use approval requested: actor {actor} asks for {name} (request {}). Reason: {}. Scope: {}. Decide with tool.grant(id=\"{}\") or tool.deny(id=\"{}\").",
                &row.id[..8],
                reason.as_deref().unwrap_or("none given"),
                scope.as_deref().unwrap_or("unspecified"),
                &row.id[..8],
                &row.id[..8],
            );
            notified = registry
                .dispatch(
                    "comm.send",
                    json!({
                        "to": to,
                        "subject": format!("tool.request: {actor} asks for {name}"),
                        "content": content,
                        "tags": ["tool-request", format!("tool:{name}")],
                    }),
                )
                .await
                .is_ok();
        }
    }

    let mut v = decision.to_json();
    v["ok"] = json!(true);
    v["tool"] = json!(name);
    v["actor"] = json!(actor);
    v["registered"] = json!(registered);
    v["request_id"] = json!(row.id);
    v["status"] = json!(row.status);
    v["notified"] = json!(notified);
    Ok(v)
}

pub(crate) async fn decide_request(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
    status: &str,
) -> Result<Value, RuntimeError> {
    let id = req_str(&params, "id")?;
    let note = opt_str(&params, "note")?;
    let ns = token.namespace().as_str().to_string();
    let current = policy::get_grant(rt, &ns, &id).await?;
    let allowed = match status {
        "granted" => matches!(current.status.as_str(), "requested" | "denied"),
        "denied" => matches!(current.status.as_str(), "requested" | "granted"),
        "revoked" => current.status == "granted",
        _ => false,
    };
    if !allowed {
        return Err(RuntimeError::InvalidInput(format!(
            "grant {} is {}; cannot move it to {status}",
            &current.id[..8],
            current.status
        )));
    }
    let decider = actor_label(token);
    if status == "granted" && current.actor == decider {
        return Err(RuntimeError::InvalidInput(format!(
            "grant {} is {} and was requested by {}; a requester cannot grant its own request",
            &current.id[..8],
            current.status,
            current.actor
        )));
    }
    let expires_at = if status == "granted" {
        opt_i64(&params, "expires_in_s")?.map(|s| now_micros() + s.max(0) * 1_000_000)
    } else {
        None
    };
    let row = policy::set_grant_status(
        rt,
        &ns,
        &current.id,
        status,
        &decider,
        expires_at,
        note.as_deref(),
    )
    .await?;
    Ok(json!({ "ok": true, "grant": row.to_json() }))
}

pub(crate) async fn requests(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let status = opt_str(&params, "status")?;
    if let Some(s) = &status {
        one_of(s, crate::vocab::GRANT_STATUSES, "status")?;
    }
    let actor = opt_str(&params, "actor")?;
    let tool = opt_str(&params, "tool")?;
    let limit = opt_u32(&params, "limit", 50, 500)?;
    let ns = token.namespace().as_str().to_string();
    let rows = policy::list_grants(
        rt,
        &ns,
        status.as_deref(),
        actor.as_deref(),
        tool.as_deref(),
        limit,
    )
    .await?;
    Ok(json!({
        "ok": true,
        "count": rows.len(),
        "requests": rows.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
    }))
}

pub(crate) async fn set_policy(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let actor = req_str(&params, "actor")?;
    let tool = req_str(&params, "tool")?;
    let decision = req_str(&params, "decision")?;
    one_of(&decision, DECISIONS, "decision")?;
    let note = opt_str(&params, "note")?;
    let ns = token.namespace().as_str().to_string();
    let row = policy::insert_policy(
        rt,
        &ns,
        &actor,
        &tool,
        &decision,
        note.as_deref(),
        &actor_label(token),
    )
    .await?;
    Ok(json!({ "ok": true, "policy": row.to_json() }))
}

pub(crate) async fn policies(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let actor = opt_str(&params, "actor")?;
    let limit = opt_u32(&params, "limit", 100, 1000)?;
    let ns = token.namespace().as_str().to_string();
    let rows = policy::list_policies(rt, &ns, actor.as_deref(), limit).await?;
    Ok(json!({
        "ok": true,
        "count": rows.len(),
        "policies": rows.iter().map(|p| p.to_json()).collect::<Vec<_>>(),
    }))
}
