use std::sync::Arc;
use std::time::Instant;

use khive_gate::{AuditEvent, GateDecision, GateRequest};
use khive_storage::{Event, EventStore, EventView, SubstrateKind};
use khive_types::{EventKind, EventOutcome, Namespace, Visibility};
use serde_json::Value;

use crate::error::{AuditObligationFailure, DispatchError, RuntimeError};
use crate::runtime::NamespaceToken;

use super::request_identity::edge_endpoint_table;
#[cfg(doc)]
use super::IdResolutionMode;
use super::{
    append_audit_event_best_effort, build_audit_storage_event, fold_audit_obligation,
    identifier_resolution_help, link_audit_success_from_result, masked_audit_event,
    persist_git_digest_receipt, resolution_mode_contract, resolve_explicit_namespace,
    GitDigestReceiptOutcome, InterceptedDispatchResult, RequestIdentity, VerbRegistry,
    VerifiedActor,
};

impl VerbRegistry {
    /// Return the help schema envelope for a verb.
    ///
    /// Walks registered packs for the first matching `HandlerDef` and returns a
    /// structured JSON envelope. Subhandlers carry `callable_via_mcp: false`.
    /// Every envelope carries the shared `identifier_resolution` contract.
    /// `link`'s envelope additionally carries `endpoint_rules` — the composed
    /// per-relation source/target allowlist (issue #964) — so batch callers can
    /// defer to the kernel's own table instead of re-implementing it locally.
    /// Every `uuid`/`array of uuid` parameter description has its
    /// declared [`IdResolutionMode`]'s contract text appended — the same
    /// text rendered under `identifier_resolution.resolution_modes` — so the
    /// full-UUID-vs-short-prefix rule is stated once per mode (in
    /// `resolution_mode_contract`) and inherited by every matching param,
    /// instead of restating it per param across every `HandlerDef` in every
    /// pack. Parameters whose mode is [`IdResolutionMode::NotApplicable`]
    /// (every non-identifier parameter) are left unchanged.
    /// Unknown verbs return `RuntimeError::InvalidInput`. Full shape documented
    /// in `docs/protocol.md` §Request Schema.
    pub fn describe_verb(&self, verb: &str) -> Result<Value, RuntimeError> {
        for pack in self.packs.iter() {
            for handler in pack.handlers().iter() {
                if handler.name == verb {
                    let category = format!("{:?}", handler.category);
                    let params_arr: Vec<Value> = handler
                        .params
                        .iter()
                        .map(|p| {
                            let description = match resolution_mode_contract(p.resolution_mode) {
                                Some(contract) => format!("{} {}", p.description, contract),
                                None => p.description.to_string(),
                            };
                            serde_json::json!({
                                "name": p.name,
                                "type": p.param_type,
                                "required": p.required,
                                "description": description,
                            })
                        })
                        .collect();
                    // Subhandlers are not callable via the MCP request surface;
                    // the help payload must match the behaviour the dispatch
                    // path enforces so callers reading `help=true` before
                    // probing see accurate availability.
                    if matches!(handler.visibility, Visibility::Subhandler) {
                        return Ok(serde_json::json!({
                            "verb": verb,
                            "pack": pack.name(),
                            "description": handler.description,
                            "category": category,
                            "params": params_arr,
                            "identifier_resolution": identifier_resolution_help(),
                            "visibility": "internal",
                            "callable_via_mcp": false,
                            "note": "This is an internal subhandler. Calling it via the MCP \
                                     request surface returns permission denied. It can only be \
                                     invoked by internal runtime callers.",
                        }));
                    }
                    let mut envelope = serde_json::json!({
                        "verb": verb,
                        "pack": pack.name(),
                        "description": handler.description,
                        "category": category,
                        "params": params_arr,
                        "identifier_resolution": identifier_resolution_help(),
                    });
                    // A pack that authored its own schema keeps it; every other
                    // verb gets one derived from the declarations the runtime
                    // already holds, so a bridged model has a schema to read
                    // instead of parsing the prose `params[].type`.
                    if let Some(schema) = pack.input_schema(verb) {
                        envelope["input_schema"] = schema;
                    } else {
                        let described: Vec<(String, String)> = params_arr
                            .iter()
                            .map(|p| {
                                (
                                    p["name"].as_str().unwrap_or_default().to_string(),
                                    p["description"].as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect();
                        if let Some(schema) =
                            crate::input_schema::derive_input_schema(handler.params, &described)
                        {
                            envelope["input_schema"] = schema;
                        }
                    }
                    if verb == "link" {
                        envelope["endpoint_rules"] = Value::Array(edge_endpoint_table(&self.packs));
                    }
                    return Ok(envelope);
                }
            }
        }
        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::UnknownVerb(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        )))
    }

    /// Check whether the gate permits writes into `ns`.
    ///
    /// Performs a gate evaluation with verb `"authorize"` before any background
    /// loop is spawned (ADR-056 §6).  Returns `Ok(())` when the gate allows the
    /// namespace, or `Err(RuntimeError::PermissionDenied{..})` when denied.
    /// Gate errors (implementation failures) are surfaced as
    /// `RuntimeError::Internal` carrying the stable classified reason; the
    /// bounded, masked backend detail goes to the server-side log here, since
    /// callers log the returned error.
    pub fn authorize_namespace(&self, ns: Namespace) -> Result<(), RuntimeError> {
        let actor = crate::actor_identity::resolve_actor(self.actor_id.as_deref());
        let req = GateRequest::new(actor, ns, "authorize", serde_json::Value::Null);
        match self.gate.check(&req) {
            Ok(decision) if decision.is_allow() => Ok(()),
            Ok(GateDecision::Deny { reason }) => {
                Err(RuntimeError::permission_denied("authorize", reason))
            }
            Ok(_) => Err(RuntimeError::permission_denied("authorize", "gate denied")),
            Err(e) => {
                tracing::warn!(
                    error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                    "authorize_namespace: gate check failed (fail-closed)"
                );
                Err(RuntimeError::Internal(format!(
                    "gate error: {}",
                    e.wire_reason()
                )))
            }
        }
    }

    /// Gate and execute an operation handled outside normal pack dispatch.
    ///
    /// Multi-backend transports use this to route an operation through a
    /// coordinator while retaining [`Self::dispatch_with_identity`]'s gate and
    /// audit lifecycle. Deny is authoritative, gate errors fail closed, and an
    /// allowed audit is persisted after the intercepted operation resolves so
    /// its outcome and duration reflect the operation result. Successful
    /// `git.digest` interception uses the same strict durable-receipt exception
    /// as normal pack dispatch.
    pub async fn dispatch_intercepted_with_identity<F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<Value, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<Value, RuntimeError>>,
    {
        self.dispatch_intercepted_with_metadata_with_identity(
            verb,
            params,
            identity,
            |namespace| async move {
                dispatch(namespace)
                    .await
                    .map(|result| InterceptedDispatchResult::new(result, ()))
            },
        )
        .await
        .map(|outcome| outcome.result)
    }

    /// Gate and execute an intercepted operation whose transport needs typed
    /// metadata in addition to the canonical verb result.
    ///
    /// Audit accounting always receives `outcome.result`; `outcome.metadata`
    /// crosses the dispatch seam unchanged for the transport to place beside
    /// that result in its own envelope.
    pub async fn dispatch_intercepted_with_metadata_with_identity<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        self.dispatch_intercepted_with_metadata_and_disposition(verb, params, identity, dispatch)
            .await
            .map_err(DispatchError::into_source)
    }

    /// Append the `GateDenied` row of a refused dispatch and report what the
    /// caller may cite: the row's id when it committed, otherwise why not.
    pub(super) async fn append_gate_denied_row(
        &self,
        store: &Arc<dyn EventStore>,
        event: Event,
        verb: &str,
    ) -> crate::error::DenialReceipt {
        let audit_event_id = event.id;
        match append_audit_event_best_effort(
            self.audit_batch.as_ref(),
            store,
            event,
            verb,
            crate::audit_batch::AuditProducer::GateDenied,
            false,
        )
        .await
        {
            Ok(()) => crate::error::DenialReceipt {
                audit_event_id: Some(audit_event_id),
                audit_outcome: crate::error::DenialAuditOutcome::Committed,
            },
            Err(failure) => crate::error::DenialReceipt {
                audit_event_id: None,
                audit_outcome: crate::error::DenialAuditOutcome::NotCommitted(failure.wire_code()),
            },
        }
    }

    /// Execute an intercepted operation while retaining this boundary's failure provenance.
    /// Successful canonical results and typed metadata are returned unchanged.
    pub async fn dispatch_intercepted_with_metadata_and_disposition<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, DispatchError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        self.dispatch_intercepted_with_token_and_disposition(verb, params, identity, |token| {
            dispatch(token.gate_namespace().clone())
        })
        .await
    }

    /// Intercept an operation with the sealed caller token minted after the
    /// gate decision. Coordinated reads retain the resolved actor and their
    /// existing namespace selection without reconstructing identity from args.
    pub async fn dispatch_intercepted_with_token_and_disposition<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, DispatchError>
    where
        F: FnOnce(NamespaceToken) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        let request_id = identity.and_then(|id| id.request_id);
        let gate_req = self
            .gate_request_with_identity(verb, params, identity)
            .map_err(DispatchError::before_dispatch)?;
        let gate_decision = khive_gate::check_with_mailbox_policy(self.gate.as_ref(), &gate_req);
        let mut deferred_audit = match gate_decision {
            Ok(decision) => {
                let audit = masked_audit_event(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );
                if let GateDecision::Deny { reason } = decision {
                    let receipt = match &self.event_store {
                        Some(store) => {
                            let event = build_audit_storage_event(
                                &gate_req,
                                &audit,
                                EventOutcome::Denied,
                                Some(crate::cost_unit::base_resource_payload(request_id)),
                            );
                            // The dispatch returns `PermissionDenied` below
                            // whether or not this row commits — a deny never
                            // reports success — so a commit failure has no
                            // caller-visible outcome to fold into; the receipt
                            // on the refusal says whether the row the caller
                            // could cite exists.
                            self.append_gate_denied_row(store, event, verb).await
                        }
                        None => crate::error::DenialReceipt::no_store(),
                    };
                    return Err(DispatchError::before_dispatch(
                        RuntimeError::PermissionDenied {
                            verb: verb.to_string(),
                            reason,
                            receipt: Box::new(receipt),
                        },
                    ));
                }
                Some(audit)
            }
            Err(err) => {
                return Err(DispatchError::before_dispatch(
                    self.gate_unavailable_error(&gate_req, &err, request_id, None)
                        .await,
                ));
            }
        };

        let started = Instant::now();
        let token = self.mint_intercepted_read_token(&gate_req, params, identity);
        let mut result = dispatch(token).await;
        let domain_succeeded = result.is_ok();
        let duration_us = started.elapsed().as_micros() as i64;
        let receipt_outcome = if verb == "git.digest" && result.is_ok() {
            let resource = result.as_ref().ok().map(|outcome| {
                crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    &outcome.result,
                    || 0,
                    request_id,
                )
            });
            // The receipt helper operates on the canonical verb result. Move
            // that value out temporarily so it can turn receipt failures into
            // the outer dispatch error without discarding successful typed
            // transport metadata.
            let mut receipt_result: Result<Value, RuntimeError> = match result.as_mut() {
                Ok(outcome) => Ok(std::mem::take(&mut outcome.result)),
                Err(_) => unreachable!("git.digest receipt path is guarded by result.is_ok()"),
            };
            let outcome = persist_git_digest_receipt(
                self.event_store.as_ref(),
                self.audit_batch.as_ref(),
                &gate_req,
                deferred_audit.as_ref(),
                &mut receipt_result,
                duration_us,
                resource,
            )
            .await;
            match receipt_result {
                Ok(receipted_result) => {
                    if let Ok(intercepted) = &mut result {
                        intercepted.result = receipted_result;
                    }
                }
                Err(error) => result = Err(error),
            }
            Some(outcome)
        } else {
            None
        };
        if receipt_outcome.is_none()
            || receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected)
        {
            if let Some(audit) = deferred_audit.take() {
                let audit_outcome = self
                    .persist_intercepted_audit(
                        verb,
                        &gate_req,
                        audit,
                        result.as_ref().map(|outcome| &outcome.result),
                        duration_us,
                        request_id,
                    )
                    .await;
                result = fold_audit_obligation(result, audit_outcome, |outcome| outcome.result);
            }
        }
        result.map_err(|error| DispatchError::after_handler(error, domain_succeeded))
    }

    async fn persist_intercepted_audit(
        &self,
        verb: &str,
        gate_req: &GateRequest,
        audit: AuditEvent,
        result: Result<&Value, &RuntimeError>,
        duration_us: i64,
        request_id: Option<u64>,
    ) -> Result<(), AuditObligationFailure> {
        let Some(store) = &self.event_store else {
            return Ok(());
        };
        let event = match result {
            Ok(value) if verb == "link" && gate_req.args.get("links").is_none() => {
                let resource = crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                );
                match link_audit_success_from_result(audit.clone(), value) {
                    Some((edge_id, mut payload)) => {
                        if let Value::Object(ref mut map) = payload {
                            map.insert("resource".to_string(), resource);
                        }
                        Event::new(
                            gate_req.namespace.as_str(),
                            gate_req.verb.as_str(),
                            EventKind::Audit,
                            SubstrateKind::Event,
                            format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                        )
                        .with_outcome(EventOutcome::Success)
                        .with_target(edge_id)
                        .with_payload(payload)
                        .with_payload_schema_version(2)
                        .with_duration_us(duration_us)
                    }
                    None => build_audit_storage_event(
                        gate_req,
                        &audit,
                        EventOutcome::Success,
                        Some(resource),
                    )
                    .with_duration_us(duration_us),
                }
            }
            Ok(value) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Success,
                Some(crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                )),
            )
            .with_duration_us(duration_us),
            Err(_) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            )
            .with_duration_us(duration_us),
        };
        let producer = if result.is_ok() {
            crate::audit_batch::AuditProducer::DispatchSucceeded
        } else {
            crate::audit_batch::AuditProducer::DispatchFailed
        };
        append_audit_event_best_effort(
            self.audit_batch.as_ref(),
            store,
            event,
            verb,
            producer,
            self.admission_degrade_safe(verb),
        )
        .await
    }

    /// A create refusal may reveal its key holder only when the same caller can list it.
    pub fn allows_note_key_disclosure(
        &self,
        token: &NamespaceToken,
        kind: &str,
        key: &str,
    ) -> bool {
        let request = GateRequest::new(
            token.actor().clone(),
            token.namespace().clone(),
            "list",
            serde_json::json!({"kind":"note", "note_kind":kind, "key_prefix":key}),
        );
        self.gate
            .check(&request)
            .is_ok_and(|decision| decision.is_allow())
    }

    fn mint_intercepted_read_token(
        &self,
        gate_req: &GateRequest,
        params: &Value,
        identity: Option<&RequestIdentity>,
    ) -> NamespaceToken {
        // Preserve the coordinator's existing namespace selection: the gate
        // namespace is primary, and explicit namespace input stays narrow.
        let visible = if params.get("namespace").is_some() {
            Vec::new()
        } else {
            let mut visible = match identity {
                Some(identity) => identity
                    .visible_namespaces
                    .iter()
                    .filter_map(|namespace| Namespace::parse(namespace).ok())
                    .collect(),
                None => self.visible_namespaces.clone(),
            };
            visible.push(Namespace::local());
            visible
        };
        NamespaceToken::mint_with_visibility(
            gate_req.namespace.clone(),
            visible,
            gate_req.actor.clone(),
        )
        .with_gate_namespace(gate_req.namespace.clone())
        .with_gate_explicit_namespace(
            params
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .with_request_id(identity.and_then(|identity| identity.request_id))
        .with_process_ref(match identity {
            Some(identity) => identity.process_ref.clone(),
            None => crate::config::process_ref_from_env(),
        })
    }

    fn gate_request_with_identity(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
    ) -> Result<GateRequest, RuntimeError> {
        let default_namespace = identity
            .map(|id| id.namespace.as_str())
            .unwrap_or(self.default_namespace.as_str());
        let namespace = resolve_explicit_namespace(params, default_namespace)?;
        let actor_id = identity
            .map(|id| id.actor_id.as_deref())
            .unwrap_or(self.actor_id.as_deref());
        let actor = crate::actor_identity::resolve_actor(actor_id);
        // GateRequest.args deliberately captures submitted dispatch arguments.
        // The handler's canonicalization and kind hooks have not run; a policy
        // requiring their effective values belongs after that handler work.
        let req = GateRequest::new(actor, namespace, verb, params.clone());
        crate::mailbox_view::validate_mailbox_request(&req)?;
        Ok(req)
    }

    pub(super) async fn gate_unavailable_error(
        &self,
        gate_req: &GateRequest,
        error: &khive_gate::GateError,
        request_id: Option<u64>,
        effective_target: Option<uuid::Uuid>,
    ) -> RuntimeError {
        let audit = AuditEvent::gate_unavailable(gate_req, self.gate.impl_name())
            .with_operation_attribution(
                khive_storage::operation_context::current_operation_attribution(),
            );
        tracing::info!(
            audit_event = %serde_json::to_string(&audit)
                .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
            "gate.check"
        );
        tracing::warn!(
            verb = %gate_req.verb,
            error = %crate::secret_gate::bounded_masked_log_text(&error.to_string()),
            "gate check failed (fail-closed)"
        );
        if let Some(store) = &self.event_store {
            let mut event = build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            );
            if let Some(target) = effective_target {
                event = event.with_target(target);
            }
            let _ = append_audit_event_best_effort(
                self.audit_batch.as_ref(),
                store,
                event,
                gate_req.verb.as_str(),
                crate::audit_batch::AuditProducer::GateUnavailable,
                false,
            )
            .await;
        }
        RuntimeError::GateUnavailable {
            verb: gate_req.verb.clone(),
            // Caller-visible: a stable, classified reason derived from the
            // `GateError` variant only. `error`'s `Display` text is logged
            // above (server-side, via `tracing::warn!`) and must never be
            // interpolated here — a gate backend's error message can embed
            // connection details, addresses, or credentials.
            reason: error.wire_reason().to_string(),
        }
    }

    /// Dispatch a verb to the first pack that handles it.
    ///
    /// Routes through the gate, then invokes the matching pack handler. When
    /// `params["help"] == true`, short-circuits to `describe_verb` with no side effects.
    /// Gate errors fail closed. Full dispatch flow documented in `docs/protocol.md`.
    ///
    /// Equivalent to `self.dispatch_with_identity(verb, params, None)` — uses
    /// this registry's construction-baked `default_namespace` / `actor_id` /
    /// `visible_namespaces`.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.dispatch_with_identity(verb, params, None).await
    }

    /// Dispatch a verb, optionally overriding this registry's baked identity
    /// scalars for exactly this call (ADR-096 Fork 1).
    ///
    /// `identity = None` behaves exactly like [`Self::dispatch`]. `identity =
    /// Some(id)` uses `id.namespace` / `id.actor_id` / `id.visible_namespaces`
    /// in place of `self.default_namespace` / `self.actor_id` /
    /// `self.visible_namespaces` for this call's namespace resolution, gate
    /// request, and token minting. The registry's own fields are never mutated,
    /// so concurrent calls with different (or no) identity are independent.
    /// See `docs/api/pack.md#dispatch_with_identity` for why this enables one warm
    /// registry to serve many attribution identities over a shared backend.
    pub async fn dispatch_with_identity(
        &self,
        verb: &str,
        params: Value,
        identity: Option<RequestIdentity>,
    ) -> Result<Value, RuntimeError> {
        self.dispatch_with_disposition(verb, params, identity)
            .await
            .map_err(DispatchError::into_source)
    }

    /// Dispatch with provenance for this operation's own domain result.
    /// Errors returned by a nested dispatch remain handler errors at this boundary.
    pub async fn dispatch_with_disposition(
        &self,
        verb: &str,
        params: Value,
        identity: Option<RequestIdentity>,
    ) -> Result<Value, DispatchError> {
        // help=true interception: short-circuit before gate/pack.
        if params.get("help").and_then(Value::as_bool) == Some(true) {
            let result = match self.describe_verb(verb) {
                Ok(value) => Ok(value),
                Err(error) => match self.mounted_verb_catalog().await {
                    Ok(catalog) => catalog
                        .into_iter()
                        .find(|entry| entry["verb"] == verb)
                        .ok_or(error),
                    Err(error) => Err(error),
                },
            };
            return result.map_err(DispatchError::before_dispatch);
        }
        // Resolve namespace before `params` is moved into pack.dispatch, so the
        // post-dispatch hook can reference it.
        //
        // Absent `namespace` and a present-but-malformed `namespace` are
        // different cases. A present non-string value (null, number, bool,
        // array, object) is explicit caller input that failed to parse and
        // must fail closed, not silently coerce to the default namespace.
        // Only a genuinely absent key defaults. Shared with the multi-backend
        // coordinator intercept via `resolve_explicit_namespace` so every MCP
        // ingress path applies the same fail-closed rule.
        let explicit_namespace = params.get("namespace").is_some_and(Value::is_string);
        // The caller-supplied correlation id (khive#948), if any. Read once
        // here so it is in scope for every audit-append site below,
        // including the ones that run before pack dispatch is attempted.
        let request_id: Option<u64> = identity.as_ref().and_then(|id| id.request_id);
        // Thread the configured actor identity into the gate request so the
        // gate can distinguish human vs agent callers at the dispatch seam.
        // Resolved once via the shared actor-identity policy and reused for
        // token minting below, so the gate's notion of "who is the caller"
        // and the storage token's notion can never drift apart.
        let gate_req = self
            .gate_request_with_identity(verb, &params, identity.as_ref())
            .map_err(DispatchError::before_dispatch)?;
        let ns = gate_req.namespace.clone();
        let resolved_actor = gate_req.actor.clone();

        // Consult the gate.
        //
        // - Ok(Allow) → proceed to pack dispatch (tracing + optional EventStore).
        // - Ok(Deny) → emit audit, persist if store configured, return PermissionDenied.
        // - Err(_) → emit an outage audit and return GateUnavailable.
        let gate_decision = khive_gate::check_with_mailbox_policy(self.gate.as_ref(), &gate_req);
        let (gate_blocked, mut deferred_audit) = match gate_decision {
            Ok(decision) => {
                let is_deny = matches!(decision, GateDecision::Deny { .. });

                // Emit audit event via tracing.
                let audit = masked_audit_event(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );

                // Drain any process-lifetime `OnceLock` config locks queued
                // since the last dispatch and persist them as `ConfigLocked`
                // events, riding this same audit-persistence gate. The
                // namespace/actor stamped on these rows are whichever
                // dispatch happens to observe the queue non-empty first:
                // an accepted provenance quirk, preferred over threading an
                // `EventStore` handle into every synchronous
                // `OnceLock::get_or_init` call site. The verb column is NOT
                // inherited from that bystander dispatch: a config-lock row
                // wearing an operation verb pollutes verb-filtered queries
                // (e.g. per-verb receipt counts), so these rows carry their
                // own `config.lock` pseudo-verb and remain discoverable by
                // `EventKind::ConfigLocked`.
                if let Some(store) = &self.event_store {
                    if crate::config_ledger::PENDING
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        for (key, value) in crate::config_ledger::drain_config_locked() {
                            let payload = serde_json::json!({ "key": key, "value": value });
                            let storage_event = Event::new(
                                gate_req.namespace.as_str(),
                                "config.lock",
                                EventKind::ConfigLocked,
                                SubstrateKind::Event,
                                format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                            )
                            .with_payload(payload);
                            // ConfigLocked is pure observability: the helper
                            // never returns `Err` for it, so there is
                            // nothing to fold.
                            let _ = append_audit_event_best_effort(
                                self.audit_batch.as_ref(),
                                store,
                                storage_event,
                                "config.lock",
                                crate::audit_batch::AuditProducer::ConfigLocked,
                                false,
                            )
                            .await;
                        }
                    }
                }

                // Every Allow-outcome audit row defers its append until pack
                // dispatch returns, so the row can carry the measured
                // dispatch time in `duration_us` (persisting before dispatch
                // ran always recorded the `Event::new` default of 0). A
                // singleton `link` call (no `links` bulk array) additionally
                // enriches the deferred row with the created/resolved edge
                // fields (schema v2) once dispatch resolves. Denied calls
                // have no dispatch to wait for and keep the immediate v1
                // append below.
                //
                // Accepted trade-off for ordinary verbs: a crash between this
                // Allow decision and the deferred append loses the audit row.
                // `git.digest` narrows the caller-visible contract below: it
                // never returns success until the deferred receipt append is
                // confirmed, though a process crash can still leave committed
                // ingest writes with no response and no completed receipt.
                let defer_audit = !is_deny;

                // Persist to EventStore immediately only for denied calls;
                // the receipt rides on the refusal so the caller can cite
                // the row.
                let reason = if is_deny {
                    let reason = match decision {
                        GateDecision::Deny { reason } => reason,
                        _ => String::new(),
                    };
                    let receipt = match &self.event_store {
                        Some(store) => {
                            // ADR-103 Decision (a): the closed `work_class` enum
                            // is stamped on every event, denial included -- only
                            // `resource.cost_unit` is scoped to a successful
                            // dispatch by Amendment 1. `base_resource_payload()`
                            // carries `work_class` alone, no `cost_unit` key.
                            let storage_event = build_audit_storage_event(
                                &gate_req,
                                &audit,
                                EventOutcome::Denied,
                                Some(crate::cost_unit::base_resource_payload(request_id)),
                            );
                            // This path always returns `PermissionDenied`
                            // below, so there is no success outcome to fold a
                            // commit failure into; the receipt says whether
                            // the row exists.
                            self.append_gate_denied_row(store, storage_event, verb)
                                .await
                        }
                        None => crate::error::DenialReceipt::no_store(),
                    };
                    Some((reason, receipt))
                } else {
                    None
                };
                let deferred = if defer_audit { Some(audit) } else { None };
                (reason, deferred)
            }
            Err(err) => {
                return Err(DispatchError::before_dispatch(
                    self.gate_unavailable_error(&gate_req, &err, request_id, None)
                        .await,
                ));
            }
        };

        // Hard enforcement: Deny is authoritative.
        if let Some((reason, receipt)) = gate_blocked {
            return Err(DispatchError::before_dispatch(
                RuntimeError::PermissionDenied {
                    verb: verb.to_string(),
                    reason,
                    receipt: Box::new(receipt),
                },
            ));
        }

        // Mint the authorized storage token at the dispatch boundary.
        //
        // Writes pin to `local` by default. Actor identity and config
        // `[actor] id` are attribution and gate-context inputs only: they
        // never route storage. The explicit `namespace=` request param is a
        // precise single-namespace escape: the caller deliberately
        // reads/writes exactly that one set; it is NOT widened by `visible_namespaces`.
        //
        // When actor_id is configured, mint a token carrying that actor
        // label so that comm.inbox applies the to_actor filter for directed delivery.
        // Otherwise, use ActorRef::anonymous() and inbox falls back to party-line.
        // `actor_id_str` already reflects the per-request identity override
        // when supplied (resolved above into `resolved_actor`, mirrored into
        // the gate request). Reusing the same value here guarantees the
        // gate's actor and the storage token's actor can never diverge.
        //
        // On the default (no explicit `namespace=`) path, the read scope
        // widens to `['local'] ∪ visible_namespaces` (baked, or the
        // per-request override). `'local'` is always included
        // (mint_with_visibility deduplicates). Writes remain pinned to
        // `'local'`. Per-actor distinctions use view-layer tag filters
        // (assignee, actor_id, from/to), not namespace partitions. `ns`/
        // `explicit_namespace` were already validated above: reuse them
        // instead of re-reading `params["namespace"]` with `as_str()`, which
        // would silently drop malformed non-string values again.
        let token = if explicit_namespace {
            // Explicit escape: precise single-namespace scope, read+write. NOT widened.
            NamespaceToken::mint_with_visibility(ns.clone(), vec![], resolved_actor)
        } else {
            // Default path: write namespace = local; read scope = ['local'] ∪ visible_namespaces.
            let primary = Namespace::local();
            let mut extra_visible: Vec<Namespace> = match identity.as_ref() {
                Some(id) => id
                    .visible_namespaces
                    .iter()
                    .filter_map(|s| match Namespace::parse(s) {
                        Ok(parsed) => Some(parsed),
                        Err(e) => {
                            tracing::warn!(
                                namespace = %s,
                                error = %e,
                                "dispatch_with_identity: skipping invalid visible_namespace \
                                 entry from per-request identity"
                            );
                            None
                        }
                    })
                    .collect(),
                None => self.visible_namespaces.clone(),
            };
            // ADR-007 Rev 4 Rule 3b, applied once at the seam every identity
            // path shares: a non-`local` actor reads its own namespace by
            // default (its episodic memories land there), whether the identity
            // came from the config loader, a daemon frame, a scheduled replay
            // or an embedding host. Writes stay pinned to `local` (Rule 0).
            if let Some(actor_namespace) = resolved_actor
                .binding_id()
                .filter(|id| *id != Namespace::LOCAL)
                .and_then(|id| Namespace::parse(id).ok())
            {
                extra_visible.push(actor_namespace);
            }
            extra_visible.push(Namespace::local()); // 'local' always readable; mint dedups
            NamespaceToken::mint_with_visibility(primary, extra_visible, resolved_actor)
        }
        .with_gate_namespace(ns.clone())
        .with_gate_explicit_namespace(
            params
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .with_request_id(request_id)
        .with_process_ref(match identity.as_ref() {
            Some(id) => id.process_ref.clone(),
            None => crate::config::process_ref_from_env(),
        });

        for pack in self.packs.iter() {
            let handler_def = pack.handlers().iter().find(|v| v.name == verb);
            let mounted_name = pack.mounted_namespace().and_then(|prefix| {
                verb.strip_prefix(prefix)
                    .and_then(|suffix| suffix.strip_prefix('.'))
            });
            if handler_def.is_some() || mounted_name.is_some() {
                let definition = if let Some(name) = mounted_name {
                    pack.mounted_catalog().await.and_then(|catalog| {
                        catalog
                            .into_iter()
                            .find(|definition| definition.name == name)
                            .map(Some)
                            .ok_or_else(|| RuntimeError::UnknownVerb(verb.to_owned()))
                    })
                } else {
                    Ok(None)
                };
                // Strip `namespace` from params before forwarding to packs.
                // The registry has already consumed it to mint the NamespaceToken.
                //
                // Exception: if the handler's own `params` schema declares
                // `"namespace"` as a valid field (e.g. brain.bind, brain.unbind,
                // brain.bindings, brain.resolve), the field is a *business* argument
                // — not a transport routing key — and must be passed through
                // unchanged. Stripping it would silently default the binding to the
                // "*" wildcard, broadening profile scope across namespaces.
                let handler_accepts_namespace = handler_def
                    .is_some_and(|h| h.params.iter().any(|p| p.name == "namespace"))
                    || definition
                        .as_ref()
                        .ok()
                        .and_then(|value| value.as_ref())
                        .is_some_and(|definition| {
                            definition
                                .input_schema
                                .get("properties")
                                .is_some_and(|properties| properties.get("namespace").is_some())
                        });
                let params = if !handler_accepts_namespace {
                    if let Value::Object(mut map) = params {
                        map.remove("namespace");
                        Value::Object(map)
                    } else {
                        params
                    }
                } else {
                    params
                };
                let dispatch_start = Instant::now();
                let mounted_audit = definition.as_ref().ok().and_then(|v| v.as_ref()).map(|v| {
                    serde_json::json!({"mount": pack.name(), "effect": v.effect, "generation": v.generation})
                });
                let mut result = match definition {
                    Ok(Some(definition)) => {
                        pack.dispatch_mounted(&definition, verb, params, self, &token)
                            .await
                    }
                    Ok(None) => pack.dispatch(verb, params, self, &token).await,
                    Err(error) => Err(error),
                };
                let domain_succeeded = result.is_ok();
                let dispatch_us = dispatch_start.elapsed().as_micros() as i64;

                // Unlike ordinary audit rows, a successful `git.digest`
                // response is returned only after its complete report has
                // been durably persisted as a schema-v2 audit receipt. The
                // receipt helper borrows the deferred audit row so malformed
                // handler output can still fall back to one generic Error
                // audit. Handler errors use that same ordinary path below.
                let git_digest_receipt_outcome = if verb == "git.digest" && result.is_ok() {
                    let resource = result.as_ref().ok().map(|value| {
                        crate::cost_unit::resource_payload(
                            verb,
                            &gate_req.args,
                            value,
                            || pack.registered_embedding_model_names().len() as i64,
                            request_id,
                        )
                    });
                    Some(
                        persist_git_digest_receipt(
                            self.event_store.as_ref(),
                            self.audit_batch.as_ref(),
                            &gate_req,
                            deferred_audit.as_ref(),
                            &mut result,
                            dispatch_us,
                            resource,
                        )
                        .await,
                    )
                } else {
                    None
                };

                // Append the deferred Allow-outcome audit row now that
                // dispatch has resolved, so `duration_us` carries the
                // measured `dispatch_us` instead of the `Event::new` default
                // of 0. A successful singleton `link` call enriches the row
                // with the created/resolved edge (schema v2); anything that
                // cannot be enriched, or is not a singleton `link` call,
                // falls back to the generic v1 audit shape so no audit row
                // is ever dropped for the deferred path.
                let needs_generic_audit = git_digest_receipt_outcome.is_none()
                    || git_digest_receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected);
                if let (true, Some(audit)) = (needs_generic_audit, deferred_audit.take()) {
                    if let Some(store) = &self.event_store {
                        let is_link_singleton =
                            verb == "link" && gate_req.args.get("links").is_none();
                        // Read-only pass over `result` first: every arm below
                        // only needs `audit_outcome` afterward, and folding a
                        // failure into `result` requires a mutable borrow
                        // that cannot coexist with the `&result` match below.
                        let audit_outcome: Result<(), AuditObligationFailure> = match &result {
                            Ok(ok_val) if is_link_singleton => {
                                // ADR-103 Amendment 1: `link` (singleton or
                                // bulk) has no embedding-bearing path — edges
                                // carry no embedded body — so cost_unit is
                                // always base_weight("link") alone. The
                                // registered-model closure is never invoked
                                // (per_item_weight("link", ..) short-circuits
                                // to 0 before `model_count` reads it).
                                let resource = crate::cost_unit::resource_payload(
                                    verb,
                                    &gate_req.args,
                                    ok_val,
                                    || pack.registered_embedding_model_names().len() as i64,
                                    request_id,
                                );
                                match link_audit_success_from_result(audit.clone(), ok_val) {
                                    Some((edge_id, mut payload)) => {
                                        if let Value::Object(ref mut map) = payload {
                                            map.insert("resource".to_string(), resource);
                                        }
                                        let storage_event = Event::new(
                                            gate_req.namespace.as_str(),
                                            gate_req.verb.as_str(),
                                            EventKind::Audit,
                                            SubstrateKind::Event,
                                            format!(
                                                "{}:{}",
                                                gate_req.actor.kind, gate_req.actor.id
                                            ),
                                        )
                                        .with_outcome(EventOutcome::Success)
                                        .with_target(edge_id)
                                        .with_payload(payload)
                                        .with_payload_schema_version(2)
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                    None => {
                                        tracing::warn!(
                                            verb,
                                            "link audit v2 enrichment parse failed; \
                                             falling back to v1 audit shape"
                                        );
                                        let storage_event = build_audit_storage_event(
                                            &gate_req,
                                            &audit,
                                            EventOutcome::Success,
                                            Some(resource),
                                        )
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                }
                            }
                            _ => {
                                // The persisted audit outcome must reflect
                                // the dispatch result, not be hardcoded to
                                // Success — otherwise a failed dispatch is
                                // recorded as successful work and disappears
                                // from `outcome=error` queries.
                                //
                                // ADR-103 Amendment 1: `resource.cost_unit` is
                                // computed ONLY on a successful dispatch —
                                // there is no handler `Value` to read
                                // `item_count` from on an error, and the
                                // amendment's "absence has exactly two
                                // meanings" rule requires the field be
                                // omitted, never defaulted to 0, on an
                                // errored dispatch. `work_class` itself is
                                // NOT one of those two omission cases
                                // (ADR-103 Decision (a) stamps it on every
                                // event), so an errored dispatch still gets
                                // `resource: {"work_class": "interactive"}`,
                                // just with no `cost_unit` key.
                                let (outcome, resource) = match &result {
                                    Ok(ok_val) => (
                                        EventOutcome::Success,
                                        Some(crate::cost_unit::resource_payload(
                                            verb,
                                            &gate_req.args,
                                            ok_val,
                                            || pack.registered_embedding_model_names().len() as i64,
                                            request_id,
                                        )),
                                    ),
                                    Err(_) => (
                                        EventOutcome::Error,
                                        Some(crate::cost_unit::base_resource_payload(request_id)),
                                    ),
                                };
                                let producer = if result.is_ok() {
                                    crate::audit_batch::AuditProducer::DispatchSucceeded
                                } else {
                                    crate::audit_batch::AuditProducer::DispatchFailed
                                };
                                let mut storage_event =
                                    build_audit_storage_event(&gate_req, &audit, outcome, resource)
                                        .with_duration_us(dispatch_us);
                                if let Some(metadata) = &mounted_audit {
                                    storage_event.payload["mounted_tool"] = metadata.clone();
                                }
                                append_audit_event_best_effort(
                                    self.audit_batch.as_ref(),
                                    store,
                                    storage_event,
                                    verb,
                                    producer,
                                    self.admission_degrade_safe(verb),
                                )
                                .await
                            }
                        };
                        // Only a would-be-success dispatch can be flipped by
                        // an obligation failure (ADR-133 D2/D3/D4): an
                        // already-erroring dispatch (DispatchFailed producer)
                        // keeps its original error, matching
                        // `fold_audit_obligation`'s contract.
                        result =
                            fold_audit_obligation(result, audit_outcome, std::convert::identity);
                    }
                }

                // Post-dispatch hook: fires on success, opt-in.
                if let (Ok(ref ok_val), Some(hook)) = (&result, &self.dispatch_hook) {
                    let mut dispatch_event = Event::new(
                        ns.as_str(),
                        verb,
                        EventKind::Audit,
                        SubstrateKind::Event,
                        pack.name(),
                    )
                    .with_outcome(EventOutcome::Success)
                    .with_duration_us(dispatch_us);

                    // For recall verbs: extract the first result's id as
                    // target_id so the brain temporal posterior can observe
                    // real hit/miss and latency. Copy the serve-attribution
                    // fields from that same hit so the hook credits the profile
                    // that actually served instead of always crediting default.
                    if verb == "memory.recall" {
                        let first_result =
                            ok_val.as_array().and_then(|arr| arr.first()).or_else(|| {
                                ok_val
                                    .get("results")
                                    .and_then(Value::as_array)
                                    .and_then(|arr| arr.first())
                            });
                        let first_note_id = first_result
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<uuid::Uuid>().ok());
                        if let Some(note_id) = first_note_id {
                            dispatch_event = dispatch_event.with_target(note_id);
                        }
                        let mut payload = serde_json::Map::new();
                        if let Some(profile_id) = first_result
                            .and_then(|v| v.get("served_by_profile_id"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "served_by_profile_id".to_string(),
                                Value::String(profile_id.to_string()),
                            );
                        }
                        if let Some(attribution) = first_result
                            .and_then(|v| v.get("serve_attribution"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "serve_attribution".to_string(),
                                Value::String(attribution.to_string()),
                            );
                        }
                        dispatch_event = dispatch_event.with_payload(Value::Object(payload));
                        // No first result → target_id stays None (RecallMiss
                        // in brain's event interpreter).
                    }

                    let dispatch_view = EventView {
                        event: dispatch_event,
                        observations: Vec::new(),
                    };
                    let hook = Arc::clone(hook);
                    hook.on_dispatch(&dispatch_view).await;
                }

                // Recently-referenced ring admission: only by-id touches admit
                // an id. Runs unconditionally (not gated on `dispatch_hook`,
                // which is opt-in) because the ring is a core
                // dispatch-boundary capability, not an observer.
                //
                // Keyed on `token.namespace()`, NOT `ns`: `ns` is the
                // gate-resolved namespace, which on the default
                // (non-explicit) dispatch path can be a non-local
                // `default_namespace` (e.g. "foreign") while the storage
                // token that actually created/touched the record is pinned
                // to `local`. The ring must be keyed on the namespace the
                // record actually lives in: the same namespace
                // `resolve_reference`'s ring lookup uses: or admission and
                // lookup silently diverge on any non-local `default_namespace`
                // config.
                if let Ok(ref ok_val) = result {
                    let admissions = crate::reference_ring::ring_admissions_for(verb, ok_val);
                    if !admissions.is_empty() {
                        let actor_key = format!("{}:{}", gate_req.actor.kind, gate_req.actor.id);
                        for (id, name) in admissions {
                            self.reference_ring.admit(
                                token.namespace().as_str(),
                                &actor_key,
                                id,
                                name,
                            );
                        }
                    }
                }

                return result
                    .map_err(|error| DispatchError::after_handler(error, domain_succeeded));
            }
        }

        // No pack owns this verb: the gate allowed it, but no dispatch runs.
        // Persist the deferred audit row now (duration stays at the
        // `Event::new` default of 0 — no dispatch occurred to measure) so an
        // allowed-but-unknown verb is never silently dropped from the audit
        // trail (matches the "no audit row is ever dropped" contract above).
        if let Some(audit) = deferred_audit.take() {
            if let Some(store) = &self.event_store {
                // Dispatch is about to return `UnknownVerb` below (no pack
                // owns this verb), so the persisted outcome must be `Error`,
                // not `Success`. `work_class` is still stamped (ADR-103
                // Decision (a)); `resource.cost_unit` is omitted, matching
                // every other errored-dispatch row.
                let storage_event = build_audit_storage_event(
                    &gate_req,
                    &audit,
                    EventOutcome::Error,
                    Some(crate::cost_unit::base_resource_payload(request_id)),
                );
                // Dispatch already returns `UnknownVerb` below regardless, so
                // — as with the deny paths above — there is no success
                // outcome to fold a commit failure into.
                let _ = append_audit_event_best_effort(
                    self.audit_batch.as_ref(),
                    store,
                    storage_event,
                    verb,
                    crate::audit_batch::AuditProducer::UnknownVerb,
                    false,
                )
                .await;
            }
        }

        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(DispatchError::before_dispatch(RuntimeError::UnknownVerb(
            format!(
                "unknown verb {verb:?}; available: {}",
                self.available_verbs.join(", ")
            ),
        )))
    }

    /// Dispatch a verb under an out-of-band verified actor identity.
    ///
    /// `verified_actor` is a typed [`VerifiedActor`] (constructor rejects blank
    /// identifiers) — only code holding a `VerbRegistry` handle can supply it.
    /// `dispatch_as` never reads `params["actor"]` to derive the effective actor;
    /// individual verbs may still accept an `actor` field for their own documented
    /// business semantics, unrelated to the acting principal. Every pack handler
    /// that reads "who is calling" resolves it from the `NamespaceToken` the
    /// dispatch boundary mints, so `verified_actor` becomes exactly the principal
    /// those handlers observe.
    ///
    /// Equivalent to `dispatch_with_identity(verb, params, Some(identity))` with
    /// `identity.actor_id = Some(verified_actor)` and every other identity scalar
    /// (namespace, visible namespaces) left at this registry's construction-baked
    /// value. [`Self::dispatch`] and [`Self::dispatch_with_identity`] are unaffected.
    /// See `docs/api/pack.md#dispatch_as` for the embedding-host use case and the
    /// blank-identifier safety rationale.
    pub async fn dispatch_as(
        &self,
        verb: &str,
        params: Value,
        verified_actor: VerifiedActor,
    ) -> Result<Value, RuntimeError> {
        let identity = RequestIdentity {
            namespace: self.default_namespace.clone(),
            actor_id: Some(verified_actor.into_inner()),
            visible_namespaces: self
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            process_ref: crate::config::process_ref_from_env(),
            request_id: None,
        };
        self.dispatch_with_identity(verb, params, Some(identity))
            .await
    }
}
