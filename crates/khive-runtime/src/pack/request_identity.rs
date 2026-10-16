use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use khive_gate::GateRef;
use khive_storage::EventStore;
use khive_types::Namespace;
use serde_json::Value;

use crate::error::RuntimeError;
use crate::runtime::NamespaceToken;

use super::{
    DispatchHook, EdgeEndpointRule, EndpointKind, HandlerDef, PackByIdResolver, PackRuntime,
    SPECIAL_RELATIONS,
};

/// Immutable registry that dispatches verb calls to registered packs.
///
/// Clone is cheap (Arc-wrapped). Constructed via `VerbRegistryBuilder`.
#[derive(Clone)]
pub struct VerbRegistry {
    pub(super) packs: std::sync::Arc<Vec<Box<dyn PackRuntime>>>,
    /// Pack ownership and endpoint rules captured together at registry construction.
    pub(super) attributed_edge_rules: Arc<Vec<(String, EdgeEndpointRule)>>,
    pub(super) pack_versions: Arc<HashMap<String, &'static str>>,
    /// Validated property-policy declarations captured once, before activation.
    pub(super) note_property_policies: Arc<Vec<khive_types::NotePropertyPolicySpec>>,
    /// Pack-level by-ID resolvers, in registration order.
    pub(super) resolvers: std::sync::Arc<Vec<(String, Box<dyn PackByIdResolver>)>>,
    /// Read-only KG lookup topology; never used to redirect a pack write.
    pub(super) kg_read_resolver: Option<Arc<crate::kg_read::KgReadResolver>>,
    pub(super) gate: GateRef,
    pub(super) default_namespace: String,
    /// Operator-configured read-visibility set (ADR-007 Rev 4 Rule 3b).
    ///
    /// On the default (no explicit `namespace=` param) dispatch path, reads fan
    /// out over `['local'] ∪ visible_namespaces`. Writes are unaffected — they
    /// still pin to `'local'`. An explicit `namespace=` request param is a
    /// precise single-namespace escape and is not widened by this set.
    pub(super) visible_namespaces: Vec<Namespace>,
    /// Configured actor identity label (ADR-057). When `Some`, dispatch mints
    /// tokens carrying this actor so that `comm.inbox` applies the `to_actor`
    /// filter. When `None`, tokens carry `ActorRef::anonymous()` (party-line).
    pub(super) actor_id: Option<String>,
    /// Audit event sink — `None` means tracing-only (v0.2 default).
    pub(super) event_store: Option<Arc<dyn EventStore>>,
    /// Distinguishes ordinary tracing-only construction from a sink omitted
    /// deliberately because its configured backend is read-only.
    pub(super) audit_store_read_only: bool,
    /// Post-dispatch hook: `None` means no real-time observation.
    pub(super) dispatch_hook: Option<Arc<dyn DispatchHook>>,
    /// Names of all `Visibility::Verb` handlers across all packs, precomputed
    /// once at `build()` time. Used only to render the unknown-verb error
    /// message — the pack set is fixed after construction, so there is no
    /// need to re-scan every pack's handlers on every miss.
    pub(super) available_verbs: Arc<Vec<&'static str>>,
    pub(super) disabled_verbs: Arc<HashSet<&'static str>>,
    /// Static handler metadata indexed once at build time. Duplicate internal
    /// subhandler names retain the first pack's declaration, as before.
    pub(super) handler_by_name: Arc<HashMap<&'static str, &'static HandlerDef>>,
    /// Verbs eligible for admission-pressure audit degradation, precomputed
    /// once at `build()` time from registration-time pack trust plus each
    /// handler's declared category and
    /// [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`]. See
    /// [`VerbRegistry::admission_degrade_safe`].
    pub(super) degrade_safe_verbs: Arc<HashSet<&'static str>>,
    /// Trusted canonical public handlers classified Read by the shared effects table.
    pub(super) read_replay_safe_verbs: Arc<HashSet<&'static str>>,
    /// Recently-referenced ring (unified-verb draft ADR, Slice 1). Daemon-warm,
    /// actor-scoped, never persisted — see `crate::reference_ring`. Shared
    /// across every clone of this registry via the `Arc`, so admissions made
    /// by one dispatch are visible to the next on the same warm daemon.
    pub(super) reference_ring: Arc<crate::reference_ring::ReferenceRing>,
    /// ADR-133 audit-batch seam. `None` exactly when `event_store` is
    /// `None` — no store configured means no seam to construct, and every
    /// audit call site falls back to its pre-ADR-133 tracing-only/no-op
    /// path.
    pub(super) audit_batch: Option<Arc<crate::audit_batch::AuditBatch>>,
}

/// Result of an operation handled outside normal pack dispatch, paired with
/// typed transport metadata that must survive the gate/audit boundary.
///
/// The canonical `result` remains the value used for audit accounting. The
/// metadata is returned to the intercepting transport without being smuggled
/// through a mutex side channel or folded into the verb's public result shape.
#[derive(Debug, Clone, PartialEq)]
pub struct InterceptedDispatchResult<M> {
    /// Canonical verb result used for audit and resource accounting.
    pub result: Value,
    /// Transport-owned metadata that must accompany the canonical result.
    pub metadata: M,
}

impl<M> InterceptedDispatchResult<M> {
    /// Pair a canonical result with its typed transport metadata.
    pub fn new(result: Value, metadata: M) -> Self {
        Self { result, metadata }
    }
}

/// Per-request identity context that overrides a [`VerbRegistry`]'s
/// construction-baked `default_namespace` / `actor_id` / `visible_namespaces`
/// for exactly one [`VerbRegistry::dispatch_with_identity`] call (ADR-096
/// Fork 1 — warm-daemon per-request identity).
///
/// A single warm registry is built once with a baked identity, but must be
/// able to serve requests whose caller resolved a *different* attribution
/// identity (e.g. a different project-local `[actor]`) without a cold
/// fallback and without mis-stamping writes under the registry's own baked
/// actor. Supplying `Some(RequestIdentity { .. })` threads the caller's
/// identity through token minting for that one call; the registry's fields
/// (and every other in-flight call) are untouched. `None` is exactly
/// [`VerbRegistry::dispatch`] — the baked scalars apply, unchanged from
/// before this type existed.
#[derive(Debug, Clone, Default)]
pub struct RequestIdentity {
    /// Storage/gate default namespace for this request (used when the verb's
    /// own params carry no explicit `namespace` field). Overrides
    /// `VerbRegistry::default_namespace`.
    pub namespace: String,
    /// Write-stamp / gate actor label for this request (ADR-057). Overrides
    /// `VerbRegistry::actor_id`. `None` mints `ActorRef::anonymous()`, same
    /// as an unconfigured baked `actor_id`.
    pub actor_id: Option<String>,
    /// Extra read-visibility namespaces for this request (ADR-007 Rev 4 Rule
    /// 3b). Overrides `VerbRegistry::visible_namespaces`. Entries that fail
    /// `Namespace::parse` are skipped with a `tracing::warn!` rather than
    /// failing the whole request — a single malformed visibility entry from a
    /// caller-supplied frame must not block dispatch.
    pub visible_namespaces: Vec<String>,
    /// Opaque process provenance resolved by the originating request process.
    /// `None` means the origin did not set one; a warm daemon must not replace
    /// it with its own process environment. This field is attribution-only and
    /// never participates in the gate or token authority.
    pub process_ref: Option<String>,
    /// Caller-supplied correlation id for this request (khive#948), carried
    /// unchanged from the daemon frame's `request_id` field. Every operation
    /// in one batch or chain receives the same value: it is a request-group
    /// selector, never an operation-unique id. Stamped into the audit event's
    /// `resource.request_id` on every outcome (success, error, and denied) so
    /// a client can join its own pre-send sample to all server-side audit rows
    /// for that request. `None` means the caller
    /// supplied no id (a pre-#948 client, or an internal/non-benchmark
    /// caller) — the audit row then carries no `request_id` key at all.
    pub request_id: Option<u64>,
}

impl RequestIdentity {
    /// Reconstruct the effective principal and namespace scope carried by an
    /// already-authorized token for a nested registry dispatch.
    ///
    /// Cross-pack calls must still pass through the registry Gate, but using
    /// the registry's construction-baked identity would silently replace a
    /// warm daemon request's actor and visibility (ADR-096). This projection
    /// preserves the token's exact primary namespace, actor, and read-visible
    /// namespaces, and the origin's process provenance rider. Nested calls
    /// intentionally use `request_id: None` even when the token retains the
    /// ingress id for audit rows within its originating dispatch; `process_ref` IS carried by
    /// the token (ADR-096: an absent value stays absent, a present origin
    /// rider survives nested dispatch without reading the daemon
    /// environment).
    pub fn from_token(token: &NamespaceToken) -> Self {
        Self {
            namespace: token.namespace().as_str().to_string(),
            actor_id: token.actor().binding_id().map(str::to_string),
            visible_namespaces: token
                .visible_namespaces()
                .iter()
                .map(|namespace| namespace.as_str().to_string())
                .collect(),
            process_ref: token.process_ref().map(str::to_owned),
            request_id: None,
        }
    }
}

/// A non-blank, out-of-band authenticated principal for [`VerbRegistry::dispatch_as`].
///
/// Embedding hosts authenticate a principal through their own channel (not the
/// request DSL) and then need that principal to become the effective actor
/// for one dispatch. The constructor rejects an empty or whitespace-only
/// identifier so an authentication-integration failure (an empty subject)
/// fails closed at construction time instead of silently resolving to the
/// anonymous/local actor at dispatch time — see [`crate::actor_identity::resolve_actor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActor(String);

impl VerifiedActor {
    /// Validate and wrap a verified principal identifier.
    ///
    /// Returns `RuntimeError::InvalidInput` when `id` is empty or contains
    /// only whitespace.
    pub fn new(id: impl Into<String>) -> Result<Self, RuntimeError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "VerifiedActor: identifier must not be empty or whitespace-only".to_string(),
            ));
        }
        Ok(Self(id))
    }

    /// Borrow the validated identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_inner(self) -> String {
        self.0
    }
}

/// Error returned by [`VerbRegistry::apply_schema_plans_with_map`] when two
/// packs on the same backend declare the same auxiliary table (ADR-028 §7).
#[derive(Debug)]
pub struct PackSchemaCollisionError {
    /// First pack to declare the table.
    pub pack_a: &'static str,
    /// Second pack that collides with `pack_a`.
    pub pack_b: &'static str,
    /// Table name or DDL error description.
    pub table: String,
}

impl std::fmt::Display for PackSchemaCollisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pack_a == self.pack_b {
            write!(
                f,
                "pack schema boot failure for pack {:?}: {}",
                self.pack_a, self.table
            )
        } else {
            write!(
                f,
                "pack schema collision: packs {:?} and {:?} both declare table {:?} \
                 on the same backend — move one pack to a separate backend or rename the table",
                self.pack_a, self.pack_b, self.table
            )
        }
    }
}

impl std::error::Error for PackSchemaCollisionError {}

/// Extract table names from every statement in a DDL entry.
///
/// Handles SQL trivia, SQLite identifier quoting, optional TEMP/VIRTUAL and a
/// `main.` qualifier. Index and other non-table DDL return no table names.
pub(super) fn extract_table_names(stmt: &str) -> Vec<String> {
    enum SqlToken {
        Bare(String),
        Quoted(String),
        Punctuation(char),
    }

    let mut tokens = Vec::new();
    let mut chars = stmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
            continue;
        }
        if matches!(ch, '"' | '`' | '[' | '\'') {
            let closing = if ch == '[' { ']' } else { ch };
            let mut token = String::new();
            while let Some(next) = chars.next() {
                if next == closing {
                    if chars.peek() == Some(&closing) {
                        chars.next();
                        token.push(closing);
                    } else {
                        break;
                    }
                } else {
                    token.push(next);
                }
            }
            tokens.push(SqlToken::Quoted(token));
            continue;
        }
        if matches!(ch, '.' | '(' | ';') {
            tokens.push(SqlToken::Punctuation(ch));
            continue;
        }
        let mut token = ch.to_string();
        while let Some(next) = chars.peek().copied() {
            let begins_comment = (next == '-' && chars.clone().nth(1) == Some('-'))
                || (next == '/' && chars.clone().nth(1) == Some('*'));
            if next.is_whitespace()
                || matches!(next, '.' | '(' | ';' | '"' | '`' | '[' | '\'')
                || begins_comment
            {
                break;
            }
            token.push(next);
            chars.next();
        }
        tokens.push(SqlToken::Bare(token));
    }

    tokens
        .split(|token| matches!(token, SqlToken::Punctuation(';')))
        .filter_map(|statement| {
            let keyword = |index: usize, word: &str| matches!(statement.get(index), Some(SqlToken::Bare(token)) if token.eq_ignore_ascii_case(word));
            if !keyword(0, "CREATE") {
                return None;
            }
            let mut index = 1;
            if keyword(index, "TEMP") || keyword(index, "TEMPORARY") {
                index += 1;
            }
            if keyword(index, "VIRTUAL") {
                index += 1;
            }
            if !keyword(index, "TABLE") {
                return None;
            }
            index += 1;
            if keyword(index, "IF") && keyword(index + 1, "NOT") && keyword(index + 2, "EXISTS") {
                index += 3;
            }
            let main_qualifier = matches!(
                statement.get(index),
                Some(SqlToken::Bare(name) | SqlToken::Quoted(name)) if name.eq_ignore_ascii_case("main")
            );
            if main_qualifier
                && matches!(statement.get(index + 1), Some(SqlToken::Punctuation('.')))
            {
                index += 2;
            }
            match statement.get(index) {
                Some(SqlToken::Bare(name) | SqlToken::Quoted(name)) if !name.is_empty() => {
                    Some(name.to_ascii_lowercase())
                }
                _ => None,
            }
        })
        .collect()
}

/// Render an [`EndpointKind`] as the `"<substrate>:<kind>"` label used in
/// `link(help=true)`'s `endpoint_rules` table.
fn endpoint_kind_label(kind: &EndpointKind) -> String {
    match kind {
        EndpointKind::EntityOfKind(k) => format!("entity:{k}"),
        EndpointKind::NoteOfKind(k) => format!("note:{k}"),
        EndpointKind::EntityOfType { kind, entity_type } => {
            format!("entity:{kind}({entity_type})")
        }
    }
}

pub(crate) fn is_special_relation(relation: khive_types::EdgeRelation) -> bool {
    SPECIAL_RELATIONS.contains(&relation)
}

/// Compose the full per-relation endpoint allowlist surfaced by
/// `link(help=true)` (issue #964).
///
/// Combines the base entity-to-entity endpoint contract
/// (`operations::base_entity_endpoint_rules`) with every loaded pack's
/// additive `EDGE_RULES`, the unconditional `note -> note` allowance for the
/// three special relations (`supersedes` / `supports` / `refutes` —
/// `operations.rs`'s dedicated special-relation branch), and the
/// `annotates` note-to-any special case — the exact same sources
/// `valid_relations_for_entity_pair` (`khive-pack-kg`) consults when
/// enriching a rejected `link` call, so a caller reading this table cannot
/// diverge from what the validator itself accepts.
///
/// Pack `EDGE_RULES` for a special relation are deliberately excluded: the
/// validator's special-relation branch returns before `pack_rule_allows` is
/// ever reached (`operations.rs`), so advertising such a rule here would
/// claim enforcement that never actually happens.
pub(super) fn edge_endpoint_table(packs: &[Box<dyn PackRuntime>]) -> Vec<Value> {
    let mut rows: Vec<Value> = crate::operations::base_entity_endpoint_rules()
        .iter()
        .map(|(src, rel, tgt)| {
            serde_json::json!({
                "relation": rel.as_str(),
                "source": format!("entity:{src}"),
                "target": format!("entity:{tgt}"),
            })
        })
        .collect();

    for rel in SPECIAL_RELATIONS {
        rows.push(serde_json::json!({
            "relation": rel.as_str(),
            "source": "note:*",
            "target": "note:*",
        }));
    }

    for pack in packs.iter() {
        for rule in pack.edge_rules().iter() {
            if is_special_relation(rule.relation) {
                continue;
            }
            rows.push(serde_json::json!({
                "relation": rule.relation.as_str(),
                "source": endpoint_kind_label(&rule.source),
                "target": endpoint_kind_label(&rule.target),
            }));
        }
    }

    rows.push(serde_json::json!({
        "relation": "annotates",
        "source": "note:*",
        "target": "any (entity, note, edge, or event)",
    }));

    rows
}
