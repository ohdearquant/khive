use super::*;
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};

fn parse_handler_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|error| {
        mask_for_redaction_surface(RedactionSurface::GateProbe, &error.to_string()).into_owned()
    })
}

// ── BrainPack handler impl ────────────────────────────────────────────────────

impl BrainPack {
    // ── brain.state ───────────────────────────────────────────────────────

    pub(crate) async fn handle_state(&self, _params: Value) -> Result<Value, RuntimeError> {
        let state = self.state.lock().unwrap();
        let snapshot = state.to_snapshot();
        let signals_applied = state
            .signals_applied
            .load(std::sync::atomic::Ordering::Relaxed);
        let snapshot_serializations = state
            .snapshot_serializations
            .load(std::sync::atomic::Ordering::Relaxed);
        drop(state);
        let cold_hook_signals_dropped = self
            .persistence
            .lock()
            .unwrap()
            .pending_hook_signals_dropped();
        let contended_hook_signals_dropped = self.hook_queue.dropped();
        let mut value = serde_json::to_value(&snapshot)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        // Beside the state rather than inside it: the snapshot is the persisted
        // shape, and these counters are process-local diagnostics that restart
        // at zero on every load.
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "dispatch_counters".to_string(),
                serde_json::json!({
                    "signals_applied": signals_applied,
                    "snapshot_serializations": snapshot_serializations,
                    "cold_hook_signals_dropped": cold_hook_signals_dropped,
                    "contended_hook_signals_dropped": contended_hook_signals_dropped,
                }),
            );
        }
        Ok(value)
    }

    // ── brain.config ──────────────────────────────────────────────────────

    pub(crate) async fn handle_config(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ConfigParams {
            parameter: Option<String>,
        }
        let p: ConfigParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let state = self.state.lock().unwrap();
        let br = &state.balanced_recall;

        let param_map = [
            ("recall::relevance_weight", &br.relevance),
            ("recall::salience_weight", &br.salience),
            ("recall::temporal_weight", &br.temporal),
        ];

        match p.parameter {
            Some(key) => {
                let posterior = param_map
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, p)| *p)
                    .ok_or_else(|| {
                        RuntimeError::NotFound(format!(
                            "parameter {key:?}; valid: {}",
                            param_map
                                .iter()
                                .map(|(k, _)| *k)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                Ok(json!({
                    "parameter": key,
                    "mean": posterior.mean(),
                    "variance": posterior.variance(),
                    "ess": posterior.effective_sample_size(),
                    "alpha": posterior.alpha(),
                    "beta": posterior.beta(),
                }))
            }
            None => {
                let configs: serde_json::Map<String, Value> = param_map
                    .iter()
                    .map(|(k, p)| {
                        (
                            (*k).to_owned(),
                            json!({
                                "mean": p.mean(),
                                "variance": p.variance(),
                                "ess": p.effective_sample_size(),
                            }),
                        )
                    })
                    .collect();
                Ok(Value::Object(configs))
            }
        }
    }

    // ── brain.events ──────────────────────────────────────────────────────

    pub(crate) async fn handle_events(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventsParams {
            limit: Option<u32>,
        }
        let p: EventsParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let limit = p.limit.unwrap_or(20).min(100);
        let ns = token.namespace().as_str().to_string();

        let store = self.runtime.events(token)?;
        let filter = EventFilter {
            verbs: vec![
                "recall".into(),
                "search".into(),
                "brain.feedback".into(),
                "brain.emit".into(), // retained for backward-compat queries
                "get".into(),
                "remember".into(),
            ],
            ..EventFilter::default()
        };
        let _ = ns;
        let page = store
            .query_events(filter, PageRequest { offset: 0, limit })
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let events: Vec<Value> = page
            .items
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "verb": e.verb,
                    "outcome": e.outcome,
                    "target_id": e.target_id.map(|t| t.to_string()),
                    "duration_us": e.duration_us,
                    "created_at": micros_to_iso(e.created_at),
                    "payload": e.payload,
                })
            })
            .collect();

        Ok(json!({
            "count": events.len(),
            "events": events,
        }))
    }

    // ── brain.event_counts ──────────────────────────────────────────────────
    //
    // ADR-103 Stage 1 (#724 Ask A): a windowed, per-actor, per-kind, per-verb
    // read verb over the event plane, additionally surfacing `work_class`
    // (ADR-103-resource-attribution-model.md:303) via `counts_by_work_class`.
    // `work_class` is read from `payload.work_class` first — the shape the
    // three Phase* events (`PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`,
    // `khive_storage::telemetry`) use today — falling back to
    // `payload.resource.work_class` for the not-yet-emitted future shape
    // where a generic audit row carries a `resource` sub-object (ADR-103
    // Stage 1's "resource payload enrichment", still open). Events with
    // neither path present are simply not counted in the split.
    // `cost_unit` (ADR-103 Amendment 1, `payload.resource.cost_unit`,
    // stamped on every successful verb dispatch since PR #927 via
    // `khive_runtime::cost_unit::resource_payload`) sums into
    // `total_cost_unit` and `cost_unit_by_verb`, both omitted rather than
    // zero-filled when no event in the window carries it. Amendment 1's
    // "absence has exactly two meanings" rule means a missing
    // `resource.cost_unit` is either a pre-Amendment-1 event or an
    // errored/denied dispatch — this verb does not distinguish the two, it
    // simply excludes the event from the sum, matching the `work_class`
    // split's existing exclusion convention.
    //
    // Named `brain.event_counts` rather than the literal
    // `brain.events(...)` shape sketched on the issue — that name is already
    // taken by the `Visibility::Subhandler` debug-listing verb above, which an
    // MCP-boundary test (`subhandler_verbs_are_blocked_at_mcp_boundary`)
    // requires to stay internal-only, and which several existing tests depend
    // on for its `{count, events}` shape. Reusing the name would either break
    // that invariant or overload one verb with two incompatible response
    // shapes; a second, public verb name keeps both surfaces intact.

    /// Cap on events aggregated by a single `brain.event_counts` window query.
    /// Initial window totals use storage-side Kind grouping. Payload-derived
    /// profile, cross and cost aggregates still fold the admitted rows. A window
    /// wider than this cap sets `truncated: true` and keeps `window_event_total`
    /// separate from the page-scoped aggregates.
    pub(super) const MAX_WINDOW_EVENTS: u32 = 50_000;
    pub(super) const EXHAUSTIVE_PAGE_SIZE: u32 = 10_000;
    pub(super) const MAX_EXHAUSTIVE_EVENTS: u64 = 40 * Self::MAX_WINDOW_EVENTS as u64;

    /// Issue #14: when the window's aggregation ran over a bounded page
    /// rather than the full window (`truncated`), a bare scalar total next
    /// to complete-looking `counts_by_*` maps reads as authoritative even
    /// though it is only a page-scoped estimate. Storage-side admission totals
    /// do not make the admitted-row aggregates complete: when truncated, the misleading key is
    /// never emitted; the same page-scoped value is returned under an
    /// explicitly incomplete name instead. A missing `total` gets noticed;
    /// a `total` that is wrong by an order of magnitude does not.
    /// `window_event_total` (the true count) and `truncated` (the marker)
    /// are unaffected -- this only renames the page-scoped scalar's own key.
    pub(crate) fn truncatable_total_key(base_key: &str, truncated: bool) -> String {
        if truncated {
            format!("{base_key}_page_scoped")
        } else {
            base_key.to_string()
        }
    }

    pub(super) fn check_read_actor(
        token: &NamespaceToken,
        actor: &str,
    ) -> Result<(), RuntimeError> {
        if actor != token.actor().label() && !token.visible_namespace_strs().contains(&actor) {
            return Err(RuntimeError::InvalidInput(format!(
                "actor {actor:?} is not visible to this caller"
            )));
        }
        Ok(())
    }

    pub(crate) async fn handle_event_counts(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let allowance = khive_storage::request_read_timeout_from_env();
        let deadline = khive_storage::effective_request_read_deadline(
            khive_storage::RequestReadDeadline::after(allowance),
        );
        khive_storage::scope_request_read_deadline_at(deadline, async {
            let operation = format!(
                "brain.event_counts (operator read budget {}s; earlier caller deadline retained)",
                allowance.as_secs()
            );
            if khive_storage::request_read_is_cancelled() {
                return Err(RuntimeError::Storage(
                    khive_storage::StorageError::Timeout {
                        operation: operation.into(),
                    },
                ));
            }
            let result = self.handle_event_counts_in_read_scope(token, params).await;
            if result.is_ok() && khive_storage::request_read_is_cancelled() {
                return Err(RuntimeError::Storage(
                    khive_storage::StorageError::Timeout {
                        operation: operation.into(),
                    },
                ));
            }
            result
        })
        .await
    }

    pub(super) async fn handle_event_counts_in_read_scope(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventCountsParams {
            actor: Option<String>,
            all_actors: Option<bool>,
            kind: Option<String>,
            group_by: Option<crate::event_counts_grouping::EventCountGroupBy>,
            // `Option`, not a required `String`: a bare-missing `since` must go through
            // the same named-field-plus-example-format error as a malformed one, not
            // serde's generic "missing field `since`" message.
            since: Option<String>,
            until: Option<String>,
            // #21: opt into full-window aggregation (paginates through every
            // matching event instead of a single bounded page) so a coverage
            // panel gets non-sampled per-verb/kind/actor counts in one call,
            // without stitching sampled windows client-side. The window is a
            // best-effort live view, not a snapshot (see the walk's docs). Default false — the
            // bounded page is cheaper and sufficient for most callers.
            exhaustive: Option<bool>,
        }
        let p: EventCountsParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let actor_scope = event_actor_read_scope(
            &self.runtime,
            token,
            p.actor.as_deref(),
            p.all_actors.unwrap_or(false),
        )?;

        let since_raw = p.since.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "missing `since`: required ISO-8601/RFC-3339 datetime; expected e.g. \
                 \"2026-07-01T00:00:00Z\""
                    .to_string(),
            )
        })?;
        let display_tz = self.runtime.config().display_timezone;
        let since_us = parse_rfc3339_micros("since", since_raw, false, display_tz)?;
        let until_us = match p.until.as_deref() {
            Some(u) => parse_rfc3339_micros("until", u, true, display_tz)?,
            None => Utc::now().timestamp_micros(),
        };

        let kind_filter = match p.kind.as_deref() {
            Some(k) => Some(
                k.parse::<khive_types::EventKind>()
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid `kind`: {e}")))?,
            ),
            None => None,
        };

        // The event plane is read through a store scoped to exactly one
        // namespace (`Runtime::events` opens `token.namespace()` and the SQL
        // pins `namespace = ?`), so a visibility set does not widen this
        // window the way it widens a note read. Name the namespace the answer
        // covers, and name the others this caller may ask for by passing
        // `namespace=`, so an undercount is legible from the result itself.
        let scope_namespace = token.namespace().as_str().to_string();
        let mut scope_other: Vec<String> = token
            .visible_namespace_strs()
            .iter()
            .map(|n| n.to_string())
            .filter(|n| n != &scope_namespace)
            .collect();
        scope_other.sort();
        scope_other.dedup();

        let store = self.runtime.events(token)?;
        let base_filter = EventFilter {
            actors: actor_scope.actors,
            kinds: kind_filter.into_iter().collect(),
            // Half-open window [since, until): `EventFilter.after` is a strict
            // `created_at > after`, so subtracting one microsecond from `since`
            // makes the boundary itself inclusive; `before` is already a strict
            // `created_at < before`, which is exactly the exclusive `until` we want.
            after: Some(since_us - 1),
            before: Some(until_us),
            ..EventFilter::default()
        };

        let exhaustive = p.exhaustive.unwrap_or(false);
        let mut counts =
            EventCountsAccumulator::new(actor_scope.default_caller.as_deref(), p.group_by);
        let (total, window_event_total, truncated) = if exhaustive {
            fold_event_counts_window_exhaustive(
                store.as_ref(),
                &base_filter,
                Self::EXHAUSTIVE_PAGE_SIZE,
                Self::MAX_EXHAUSTIVE_EVENTS,
                &mut counts,
            )
            .await?
        } else {
            let (items, window_event_total, truncated) = fetch_event_counts_window(
                store.as_ref(),
                &base_filter,
                p.kind.is_none(),
                Self::MAX_WINDOW_EVENTS,
            )
            .await?;
            for event in &items {
                counts.observe(event);
            }
            (items.len() as u64, window_event_total, truncated)
        };

        let mut result = json!({
            "since": micros_to_iso(since_us),
            "until": micros_to_iso(until_us),
            "actor": p.actor,
            "kind": p.kind,
            "truncated": truncated,
            "window_event_total": window_event_total,
            "exhaustive": exhaustive,
            // The scope the answer was computed under, stated rather than
            // implied. `namespace` is the single namespace this window covers;
            // `other_namespaces` are the ones this caller may read by asking
            // for them explicitly, and whose rows are absent here. A caller can
            // therefore tell a real zero from an out-of-scope population, and
            // knows what to pass to reach the rest.
            "scope": {
                "namespace": scope_namespace,
                "other_namespaces": scope_other,
            },
        });
        result[Self::truncatable_total_key("total", truncated)] = json!(total);
        counts.add_to_result(&mut result, truncated);
        Ok(result)
    }

    // ── brain.profiles ────────────────────────────────────────────────────

    pub(crate) async fn handle_profiles(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProfilesParams {
            lifecycle: Option<String>,
        }
        let p: ProfilesParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let state = self.state.lock().unwrap();
        // Only expose the three public-facing lifecycle states.
        // `defined` and `registered` are internal implementation states that
        // callers cannot filter on; listing them in the error would confuse users.
        let filter_lc: Option<ProfileLifecycle> = p
            .lifecycle
            .as_deref()
            .map(|s| match s {
                "active" => Ok(ProfileLifecycle::Active),
                "inactive" => Ok(ProfileLifecycle::Inactive),
                "archived" => Ok(ProfileLifecycle::Archived),
                other => Err(RuntimeError::InvalidInput(format!(
                    "invalid lifecycle {:?}; expected one of 'active', 'inactive', 'archived'",
                    mask_for_redaction_surface(RedactionSurface::GateProbe, other)
                ))),
            })
            .transpose()?;

        // Sort by id for deterministic output regardless of HashMap iteration order.
        let mut profiles: Vec<&ProfileRecord> = state
            .profiles
            .values()
            .filter(|r| filter_lc.as_ref().is_none_or(|lc| &r.lifecycle == lc))
            .collect();
        profiles.sort_by(|a, b| a.id.cmp(&b.id));

        let items: Vec<Value> = profiles
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "description": r.description,
                    "consumer_kind": r.consumer_kind,
                    "state_class": r.state_class,
                    "lifecycle": r.lifecycle,
                    "total_events": r.total_events,
                    "exploration_epoch": r.exploration_epoch,
                    "created_at": r.created_at,
                })
            })
            .collect();

        Ok(json!({ "count": items.len(), "profiles": items }))
    }

    // ── brain.profile ─────────────────────────────────────────────────────

    pub(crate) async fn handle_profile(&self, params: Value) -> Result<Value, RuntimeError> {
        // Accept both `profile_id` (canonical) and `id` (legacy alias).
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProfileParams {
            profile_id: Option<String>,
            id: Option<String>,
        }
        let p: ProfileParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;
        let profile_id = p
            .profile_id
            .or(p.id)
            .ok_or_else(|| RuntimeError::InvalidInput("missing field `profile_id`".into()))?;

        let state = self.state.lock().unwrap();
        // The snapshot in the response is serialized here, on the read, rather
        // than on every signal that preceded it.
        let record = state
            .materialized_profile(&profile_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?;
        let record = &record;

        // Build per-section posterior summary for the response.
        let section_summary = if let Some(ss) = state.section_states.get(&profile_id) {
            let weights = derive_deterministic_weights(ss);
            let mut sections_json: serde_json::Map<String, Value> =
                serde_json::Map::with_capacity(ss.posteriors.len());
            for (section, posterior) in &ss.posteriors {
                let w = weights.get(section).copied().unwrap_or(0.0);
                sections_json.insert(
                    section.as_str().to_owned(),
                    json!({
                        "alpha": posterior.alpha(),
                        "beta": posterior.beta(),
                        "mean": posterior.mean(),
                        "variance": posterior.variance(),
                        "ess": posterior.effective_sample_size(),
                        "weight": w,
                    }),
                );
            }
            Value::Object(sections_json)
        } else {
            Value::Null
        };

        Ok(json!({
            "id": record.id,
            "description": record.description,
            "consumer_kind": record.consumer_kind,
            "state_class": record.state_class,
            "lifecycle": record.lifecycle,
            "total_events": record.total_events,
            "exploration_epoch": record.exploration_epoch,
            "created_at": record.created_at,
            "state_snapshot": record.state_snapshot,
            "section_posteriors": section_summary,
        }))
    }

    // ── brain.resolve ─────────────────────────────────────────────────────

    pub(crate) async fn handle_resolve(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResolveParams {
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: String,
        }
        let p: ResolveParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let caller = token.actor().label();
        // Anonymous omission must retain the serve path's wildcard-only resolution.
        let actor = match p.actor.as_deref() {
            Some(a) => {
                Self::check_read_actor(token, a)?;
                Some(a)
            }
            None if token.actor().is_anonymous() => None,
            None => Some(caller.as_str()),
        };

        let state = self.state.lock().unwrap();
        // Return requested_consumer_kind + matched_consumer_kind + matched_binding so
        // callers can distinguish a real binding match from a system-default fallback.
        // ADR-035 tier-2 requires matched_binding=true; a false value means tier-3 fires.
        match state.resolve_with_match(actor, p.namespace.as_deref(), &p.consumer_kind) {
            Some((record, matched_kind, matched_binding)) => Ok(json!({
                "resolved_profile_id": record.id,
                "lifecycle": record.lifecycle,
                "requested_consumer_kind": p.consumer_kind,
                "matched_consumer_kind": matched_kind,
                "matched_binding": matched_binding,
            })),
            None if registry
                .all_brain_consumer_kinds()
                .contains(&p.consumer_kind.as_str()) =>
            {
                Ok(json!({
                    "resolved_profile_id": null,
                    "lifecycle": null,
                    "requested_consumer_kind": p.consumer_kind,
                    "matched_consumer_kind": null,
                    "matched_binding": false,
                }))
            }
            None => Err(RuntimeError::NotFound(format!(
                "no profile resolved for consumer_kind={:?}",
                p.consumer_kind
            ))),
        }
    }

    // ── brain.activate / deactivate / archive ─────────────────────────────

    pub(crate) async fn handle_activate(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Active)
            .await
    }

    pub(crate) async fn handle_deactivate(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Inactive)
            .await
    }

    pub(crate) async fn handle_archive(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.set_lifecycle(token, params, ProfileLifecycle::Archived)
            .await
    }

    /// Route a profile lifecycle transition through the durable-write helper
    /// (issue #457): the transition is validated and applied against a
    /// proposed state copy, and only takes effect in `self.state` after the
    /// brain event-log append + snapshot upsert commit in one transaction.
    pub(crate) async fn set_lifecycle(
        &self,
        token: &NamespaceToken,
        params: Value,
        lifecycle: ProfileLifecycle,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LifecycleParams {
            profile_id: String,
        }
        let p: LifecycleParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let event_kind = match lifecycle {
            ProfileLifecycle::Active => "brain.activate",
            ProfileLifecycle::Inactive => "brain.deactivate",
            ProfileLifecycle::Archived => "brain.archive",
            // set_lifecycle is only ever invoked (via handle_activate /
            // handle_deactivate / handle_archive) with one of the three
            // targets above; other lifecycle values are registry-only states
            // that no handler transitions to here.
            ProfileLifecycle::Defined | ProfileLifecycle::Registered => "brain.set_lifecycle",
        };
        let profile_id = p.profile_id;
        let event_payload = json!({ "profile_id": profile_id, "lifecycle": lifecycle.clone() });

        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: event_kind.to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                if profile_id == "balanced-recall-v1"
                    && matches!(
                        &lifecycle,
                        ProfileLifecycle::Inactive | ProfileLifecycle::Archived
                    )
                {
                    return Err(RuntimeError::InvalidInput(
                        "balanced-recall-v1 is the required default profile and cannot be deactivated or archived".into(),
                    ));
                }
                let record = state
                    .profiles
                    .get_mut(&profile_id)
                    .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?;

                // Lifecycle DAG: defined → registered → active ⟷ inactive → archived
                // Terminal state: archived is read-only/audit-retained.
                // No transition OUT of archived is legal.
                // Active → archived is also illegal; must deactivate first.
                match (&record.lifecycle, &lifecycle) {
                    // archived is terminal — nothing leaves it
                    (ProfileLifecycle::Archived, _) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is archived; archive is terminal and no transition is permitted",
                            profile_id
                        )));
                    }
                    // active → archived is illegal (must go through inactive first)
                    (ProfileLifecycle::Active, ProfileLifecycle::Archived) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is active; deactivate it before archiving",
                            profile_id
                        )));
                    }
                    // all other transitions are permitted
                    _ => {}
                }

                record.lifecycle = lifecycle.clone();
                Ok(json!({
                    "profile_id": profile_id,
                    "lifecycle": lifecycle,
                }))
            },
        )
        .await
    }

    // ── brain.reset ───────────────────────────────────────────────────────

    pub(crate) async fn handle_reset(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        // profile_id is optional; defaults to "balanced-recall-v1".
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResetParams {
            profile_id: Option<String>,
        }
        let p: ResetParams = parse_handler_params(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("brain.reset: {e}")))?;
        let profile_id = p
            .profile_id
            .unwrap_or_else(|| "balanced-recall-v1".to_string());
        let event_payload = json!({ "profile_id": profile_id });

        // Route through the durable-write helper (issue #457): reset is
        // applied to a proposed state copy and only takes effect after the
        // brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: "brain.reset".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                // Validate profile exists.
                let lifecycle = state
                    .profiles
                    .get(&profile_id)
                    .ok_or_else(|| RuntimeError::NotFound(format!("profile {:?}", profile_id)))?
                    .lifecycle
                    .clone();

                // Reject archived profiles — archive is terminal and read-only.
                if lifecycle == ProfileLifecycle::Archived {
                    return Err(RuntimeError::InvalidInput(format!(
                        "profile {:?} is archived; archive is terminal and reset is not permitted",
                        profile_id
                    )));
                }

                if profile_id == "balanced-recall-v1" {
                    state.reset_posteriors();
                    // Sync profile record after reset so brain.profile reflects the restored
                    // domain-informed priors, not stale pre-reset values.
                    sync_balanced_recall_record(state);
                } else if state.profile_states.contains_key(&profile_id) {
                    // User-created Bayesian profile — reset its own posteriors.
                    state.reset_profile_posteriors(&profile_id);
                } else {
                    // Profile exists in registry but has no live state (e.g. non-Bayesian).
                    // Increment exploration_epoch on the record to mark the reset event.
                    if let Some(record) = state.profiles.get_mut(&profile_id) {
                        record.exploration_epoch += 1;
                    }
                }

                let epoch = if profile_id == "balanced-recall-v1" {
                    state.balanced_recall.exploration_epoch
                } else {
                    state.profiles[&profile_id].exploration_epoch
                };

                Ok(json!({
                    "reset": true,
                    "profile_id": profile_id,
                    "exploration_epoch": epoch,
                }))
            },
        )
        .await
    }

    /// Resolve the effective serving profile for feedback attribution
    /// (ADR-035 tiers 1-2, #697, #1851): a known explicit profile wins;
    /// otherwise resolve an actor+namespace-scoped binding via the same
    /// `resolve_with_match` table `brain.resolve` uses, before falling back to
    /// the system default only when no explicit ID was supplied. An unknown
    /// explicit ID without a matching binding is refused, never credited to
    /// the default. `brain.auto_feedback` forwards into `handle_feedback`
    /// unresolved so it inherits this without duplicating the logic.
    ///
    /// Consumer kind is fixed to `recall`: feedback routed directly through
    /// `brain.feedback`/`brain.auto_feedback` (as opposed to `memory.feedback`,
    /// which resolves its own tier and passes an explicit profile) always
    /// originates from the recall serve loop, so it must resolve against the
    /// same binding bucket the serve path used.
    /// Returns the profile id plus a resolution marker (`explicit` |
    /// `binding` | `default`) so event payloads can record call-site
    /// discipline separately from attribution (#1016).
    pub(super) fn resolve_effective_feedback_profile(
        &self,
        token: &NamespaceToken,
        explicit: Option<&str>,
    ) -> Result<(String, &'static str), RuntimeError> {
        let state = self.state.lock().unwrap();
        if let Some(profile_id) = explicit {
            if state.profiles.contains_key(profile_id) {
                return Ok((profile_id.to_string(), "explicit"));
            }
        }
        let actor = token.actor().binding_id();
        let namespace = token.namespace().as_str();
        match state.resolve_with_match(actor, Some(namespace), ConsumerKind::Recall.as_str()) {
            Some((record, _matched_kind, true)) => Ok((record.id.clone(), "binding")),
            _ => match explicit {
                Some(profile_id) => Err(RuntimeError::NotFound(format!(
                    "serving profile {:?} not found in profile registry",
                    profile_id
                ))),
                None => Ok(("balanced-recall-v1".to_string(), "default")),
            },
        }
    }

    pub(super) fn validate_feedback_serving_profile(
        &self,
        profile_id: &str,
    ) -> Result<(), RuntimeError> {
        let state = self.state.lock().unwrap();
        match state.profiles.get(profile_id) {
            None => Err(RuntimeError::NotFound(format!(
                "serving profile {:?} not found in profile registry",
                profile_id
            ))),
            Some(rec) if rec.lifecycle == khive_brain_core::ProfileLifecycle::Archived => {
                Err(RuntimeError::InvalidInput(format!(
                    "serving profile {:?} is archived; feedback cannot credit archived profiles",
                    profile_id
                )))
            }
            Some(_) => Ok(()),
        }
    }

    // ── brain.feedback ────────────────────────────────────────────────────

    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        self.handle_feedback_from(token, params, "brain.feedback", registry)
            .await
    }

    pub(super) async fn handle_feedback_from(
        &self,
        token: &NamespaceToken,
        params: Value,
        originating_verb: &'static str,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let feedback_start = Instant::now();

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FeedbackParams {
            target_id: String,
            signal: String,
            served_by_profile_id: Option<String>,
            serve_attribution: Option<ServeAttribution>,
            section_signals: Option<serde_json::Value>,
            // ADR-081 §6: scorer provenance, additive and optional. Must be
            // supplied together (validated just below) — one without the other
            // is rejected rather than silently coerced.
            scorer_run_id: Option<String>,
            serve_ledger_id: Option<String>,
            // #1509 cheap-half: `brain.auto_feedback` receives the serving `query`
            // and the full `results` list. Persisting what
            // already arrives (emission-side only, no new join) unblocks
            // "what else was on the candidate list this feedback was chosen
            // from" without requiring the full RecallExecuted-linkage gap
            // (#807's harder half) to close first. Both optional so a direct
            // `brain.feedback` caller (no serving context) is unaffected.
            query: Option<String>,
            candidate_ids: Option<Vec<String>>,
        }
        let p: FeedbackParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        // ADR-081 §6: together-or-rejected. Absent-both is the unchanged,
        // ordinary feedback path; present-both enables dedup + ledger backfill
        // below. Exactly one present is invalid input.
        if p.scorer_run_id.is_some() != p.serve_ledger_id.is_some() {
            return Err(RuntimeError::InvalidInput(
                "scorer_run_id and serve_ledger_id must be supplied together".to_string(),
            ));
        }

        let target: uuid::Uuid =
            resolve_auto_feedback_target(&self.runtime, token, registry, &p.target_id).await?;

        let signal = match p.signal.as_str() {
            "useful" => "useful",
            "not_useful" => "not_useful",
            "wrong" => "wrong",
            // Semantic event taxonomy names are also valid.
            "explicit_positive" => "explicit_positive",
            "explicit_negative" => "explicit_negative",
            "implicit_positive" => "implicit_positive",
            "implicit_negative" => "implicit_negative",
            "correction" => "correction",
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown signal {:?}; valid: useful | not_useful | wrong | \
                     explicit_positive | explicit_negative | implicit_positive | \
                     implicit_negative | correction",
                    mask_for_redaction_surface(RedactionSurface::GateProbe, other)
                )))
            }
        };
        let is_gated_implicit = matches!(
            FeedbackEventKind::from_signal_str(signal),
            Some(FeedbackEventKind::ImplicitPositive) | Some(FeedbackEventKind::ImplicitNegative)
        );
        // A3 preserves the grade slot after unjudged telemetry. Caller-judged
        // auto-feedback can claim that slot in the same atomic transaction as
        // its explicit fold. Manual feedback keeps ADR-081's existing policy.
        let is_graded_auto = originating_verb == "brain.auto_feedback"
            && matches!(signal, "useful" | "not_useful" | "wrong");
        if p.scorer_run_id.is_some() && !is_gated_implicit && !is_graded_auto {
            return Err(RuntimeError::InvalidInput(
                "scorer_run_id and serve_ledger_id require implicit_positive or implicit_negative"
                    .to_string(),
            ));
        }

        // Caller identity and serve attribution are separate: omitting the
        // profile may resolve a shared default, but cannot authorize an
        // anonymous caller to train it with an explicit judgment (#2282).
        // Keep implicit anonymous controls on their existing admission path.
        // A configured actor id of "local" is the unattributed pool, not a
        // caller, so it is refused by the same rule as the anonymous actor.
        let unattributed =
            token.actor().is_anonymous() || khive_runtime::actor_is_unattributed(token.actor());
        if unattributed && !is_gated_implicit {
            return Err(khive_types::KhiveError::invalid_input(
                "explicit or correction feedback requires an attributed caller; configure actor.id",
            )
            .into());
        }

        // Resolve the target by UUID with no namespace filter (ADR-007 Rule 2 /
        // PR-A1: by-ID ops are namespace-agnostic; authorization is the Gate's,
        // not a post-fetch namespace check). Rule 3b recall fans out actor-stamped
        // memories from other namespaces by design, so a primary-only check would
        // reject the feedback loop's own recalled targets. NotFound only for a
        // genuinely absent UUID.
        use khive_runtime::Resolved;
        // ADR-041 permits both entity and note signal targets; the resolved
        // substrate is threaded onto the emitted event below (#831) so the decoder can tell entity and
        // note signal observations apart instead of hard-coding entity.
        let target_substrate = match registry
            .resolve_kg_read_by_id(&self.runtime, token, target, false)
            .await?
        {
            Some(Resolved::Entity(_)) => khive_types::SubstrateKind::Entity,
            Some(Resolved::Note(_)) => khive_types::SubstrateKind::Note,
            _ => {
                return Err(RuntimeError::NotFound(format!(
                    "target_id {target:?} not found"
                )));
            }
        };

        // Preserve the serve-time distinction between omitted attribution and
        // explicit negative knowledge. Only the former may use the historical
        // binding/default resolver. Legacy callers that send only a profile id
        // remain attributed without needing the new marker.
        let mut serve_attribution = p.serve_attribution.unwrap_or_else(|| {
            if p.served_by_profile_id.is_some() {
                ServeAttribution::Profile
            } else {
                ServeAttribution::Unspecified
            }
        });
        match (serve_attribution, p.served_by_profile_id.as_deref()) {
            (ServeAttribution::Profile, None) => {
                return Err(RuntimeError::InvalidInput(
                    "serve_attribution=\"profile\" requires served_by_profile_id".to_string(),
                ));
            }
            (ServeAttribution::Unattributed | ServeAttribution::Unspecified, Some(_)) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "serve_attribution=\"{}\" cannot include served_by_profile_id",
                    serve_attribution.as_str()
                )));
            }
            _ => {}
        }

        // Compute the effective serving profile (known explicit, else a matching
        // actor+namespace binding; only an omitted ID may use the system default
        // — #697, #1851), unless the
        // serve was explicitly unattributed, in which case there is no profile
        // to resolve at all. Then perform a warm-state fast-path check that it
        // exists and is not Archived; the persistence transaction repeats this
        // against the authoritative rebased snapshot before writing any side
        // effect on every path where a profile was actually resolved.
        let (mut effective_profile, mut profile_resolution) =
            if serve_attribution == ServeAttribution::Unattributed {
                (None, "serve_unattributed")
            } else {
                let (profile_id, resolution) = self
                    .resolve_effective_feedback_profile(token, p.served_by_profile_id.as_deref())?;
                (Some(profile_id), resolution)
            };

        // Validate section_signals up front using the shared validator.
        // Malformed input is rejected here rather than silently dropped during replay.
        if let Some(ref ss) = p.section_signals {
            crate::validate_section_signals(ss)?;
        }

        // A null ledger accounting key may suppress a fold, but it cannot
        // make an invalid caller-resolved profile a valid request.
        if let Some(profile_id) = effective_profile.as_deref() {
            self.validate_feedback_serving_profile(profile_id)?;
        }

        let sql = self.runtime.sql();
        let now_us = Utc::now().timestamp_micros();

        // ADR-081 §6: when both scorer_run_id and serve_ledger_id are supplied,
        // resolve dedup + the accounting_profile_id fail-safe against the serve
        // ledger before the fold gate runs.
        let mut forced_zero_weight = effective_profile.is_none();
        if let (Some(scorer_run_id), Some(serve_ledger_id)) =
            (p.scorer_run_id.as_deref(), p.serve_ledger_id.as_deref())
        {
            let (already_graded, accounting_profile_id, ledger_attribution) =
                match crate::serve_ledger::resolve(
                    sql.as_ref(),
                    serve_ledger_id,
                    scorer_run_id,
                    token.namespace().as_str(),
                    &target.to_string(),
                    p.served_by_profile_id.as_deref(),
                )
                .await?
                {
                    crate::serve_ledger::ServeLedgerResolution::NotFound => {
                        return Err(RuntimeError::NotFound(format!(
                            "serve_ledger_id {:?} not found",
                            serve_ledger_id
                        )));
                    }
                    crate::serve_ledger::ServeLedgerResolution::Found {
                        already_graded,
                        accounting_profile_id,
                        serve_attribution: ledger_attribution,
                    } => (already_graded, accounting_profile_id, ledger_attribution),
                };

            if let Some(accounting_profile_id) = accounting_profile_id {
                if serve_attribution == ServeAttribution::Unattributed {
                    return Err(RuntimeError::InvalidInput(
                        "serve_attribution=\"unattributed\" conflicts with the serve ledger accounting_profile_id"
                            .to_string(),
                    ));
                }
                // The serve row owns the accounting key. A caller's
                // binding/default may differ from the profile that
                // actually served this target.
                effective_profile = Some(accounting_profile_id);
                profile_resolution = "serve_ledger";
                serve_attribution = ServeAttribution::Profile;
                forced_zero_weight = false;
            } else if ledger_attribution != Some(ServeAttribution::Unspecified) {
                // ADR-081 §4 fail-safe: only a row stamped `unspecified`
                // permits legacy binding/default fallback. A null accounting
                // profile otherwise records the implicit event at zero weight.
                forced_zero_weight = true;
                effective_profile = None;
                profile_resolution = "serve_ledger_unattributed";
                serve_attribution = ServeAttribution::Unattributed;
            }

            if already_graded {
                return Ok(json!({
                    "emitted": false,
                    "deduped": true,
                    "verb": "brain.feedback",
                    "signal": signal,
                    "target_id": target.to_string(),
                    "serve_ledger_id": serve_ledger_id,
                    "scorer_run_id": scorer_run_id,
                    "served_by_profile_id": effective_profile.as_deref(),
                    "serve_attribution": serve_attribution,
                }));
            }
        }

        // Validate the profile that will actually receive the fold, after the
        // ledger has supplied its authoritative accounting_profile_id.
        if let Some(profile_id) = effective_profile.as_deref() {
            self.validate_feedback_serving_profile(profile_id)?;
        }

        if effective_profile.is_none() && !is_gated_implicit {
            return Err(RuntimeError::InvalidInput(
                "unattributed serve feedback must use implicit_positive or implicit_negative; no profile can receive explicit credit"
                    .to_string(),
            ));
        }

        // ADR-081 §2/§6: when both scorer fields are present, the dedup claim
        // is made atomically inside the SAME `BEGIN IMMEDIATE` transaction as
        // the fold gate's mass check-and-write, the public event, the serve
        // grade, the private brain event, and the posterior snapshot — the `resolve`
        // check above is a non-atomic fast path only (it still handles the
        // common sequential case and the NotFound / forced-zero-weight
        // determination); this is the authoritative correctness mechanism
        // under concurrent duplicate submissions.
        let dedup_key: Option<(String, String)> =
            match (p.scorer_run_id.as_deref(), p.serve_ledger_id.as_deref()) {
                (Some(r), Some(l)) => Some((r.to_string(), l.to_string())),
                _ => None,
            };

        // Base feedback payload, shared by every signal kind. The gated
        // (implicit) path below adds a "gate" key inside the atomic unit;
        // the ungated path (explicit/correction) adds nothing further.
        // #1016: always stamp the resolved profile (not just the
        // caller-explicit value) so event_counts attribution and ADR-032
        // replay match what the posterior fold actually credited; the
        // marker records how it was resolved.
        let mut base_data = json!({
            "signal": signal,
            "originating_verb": originating_verb,
            "served_by_profile_id": effective_profile.as_deref(),
            "serve_attribution": serve_attribution,
            "profile_resolution": profile_resolution,
        });
        if let Some(ref ss) = p.section_signals {
            base_data["section_signals"] = ss.clone();
        }
        if let (Some(ref scorer_run_id), Some(ref serve_ledger_id)) =
            (p.scorer_run_id.as_ref(), p.serve_ledger_id.as_ref())
        {
            base_data["scorer_run_id"] = json!(scorer_run_id);
            base_data["serve_ledger_id"] = json!(serve_ledger_id);
        }
        // #1509 cheap-half: persist what `brain.auto_feedback` already receives
        // instead of dropping it. `candidate_ids` are the raw (pre-resolution)
        // ids `auto_feedback` was handed — not necessarily equal to `target_id`
        // (which is caller-selected and resolved to a full UUID) — so a reader
        // can see the full candidate set the choice was made from.
        if let Some(ref query) = p.query {
            base_data["query"] = json!(query);
        }
        if let Some(ref candidate_ids) = p.candidate_ids {
            base_data["candidate_ids"] = json!(candidate_ids);
        }

        // ADR-081 §2: the bounded-mass fold gate applies only to implicit
        // signals. Explicit/correction signals are never gated (they are the
        // clamp's own comparator, ADR-081 §1).
        let duration_us = feedback_start.elapsed().as_micros().max(1) as i64;
        let event = Event::new(
            token.namespace().as_str().to_string(),
            "brain.feedback",
            khive_types::EventKind::FeedbackExplicit,
            target_substrate,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_target(target)
        .with_payload(base_data)
        .with_duration_us(duration_us);

        let event_write = if is_gated_implicit {
            let nominal_weight = FeedbackEventKind::from_signal_str(signal)
                .expect("is_gated_implicit implies from_signal_str is Some")
                .update_weight();
            let gate_mode = if forced_zero_weight {
                crate::fold_gate::FeedbackGateMode::ForcedZero
            } else {
                crate::fold_gate::FeedbackGateMode::Nominal(nominal_weight)
            };
            crate::persist::FeedbackEventWrite::Gated {
                event,
                target_id: target.to_string(),
                gate_mode,
                gate_now_us: now_us,
                grade: dedup_key.as_ref().map(|_| signal.to_owned()),
                dedup_key,
            }
        } else if let Some((scorer_run_id, serve_ledger_id)) = dedup_key {
            crate::persist::FeedbackEventWrite::Graded {
                event,
                scorer_run_id,
                serve_ledger_id,
                grade: signal.to_owned(),
            }
        } else {
            crate::persist::FeedbackEventWrite::Direct(event)
        };

        let event: Event = if let Some(serving_profile_owned) = effective_profile.clone() {
            // #1592: profile existence/lifecycle was already fast-path-checked
            // above under the warm-state lock; `persist_feedback_state_mutation`
            // repeats that check against the authoritative rebased snapshot
            // inside the same atomic unit that appends the event and mutates
            // the posterior, so a profile archived/deleted between the two
            // checks is still caught.
            #[cfg(test)]
            crate::pack::run_feedback_precommit_hook(serving_profile_owned.as_str()).await;
            let event = crate::persist::persist_feedback_state_mutation(
                self.runtime.sql().as_ref(),
                token,
                &self.persistence,
                &self.state,
                serving_profile_owned.clone(),
                event_write,
                ENTITY_CACHE_CAPACITY,
                {
                    let serving_profile_owned = serving_profile_owned.clone();
                    move |state: &mut khive_brain_core::BrainState, brain_signal| {
                        let serving_profile = serving_profile_owned.as_str();

                        if serving_profile == "balanced-recall-v1" {
                            state.balanced_recall.apply_signal(brain_signal);
                            sync_balanced_recall_record(state);
                        } else if state.profile_states.contains_key(serving_profile) {
                            let ps = state
                                .profile_states
                                .get_mut(serving_profile)
                                .expect("key checked above");
                            ps.apply_signal(brain_signal);
                            let snap = serde_json::to_value(ps.to_snapshot()).ok();
                            let total = ps.total_events;
                            if let Some(record) = state.profiles.get_mut(serving_profile) {
                                record.total_events = total;
                                record.state_snapshot = snap;
                            }
                        } else {
                            state.balanced_recall.apply_signal(brain_signal);
                            sync_balanced_recall_record(state);
                        }

                        let section_state = crate::ensure_section_state_seeded(
                            &mut state.section_states,
                            &serving_profile_owned,
                        );
                        section_state.apply_signal(brain_signal);
                    }
                },
            )
            .await?;
            let Some(event) = event else {
                return Ok(json!({
                    "emitted": false,
                    "deduped": true,
                    "verb": "brain.feedback",
                    "signal": signal,
                    "target_id": target.to_string(),
                    "serve_ledger_id": p.serve_ledger_id,
                    "scorer_run_id": p.scorer_run_id,
                    "served_by_profile_id": effective_profile.as_deref(),
                    "serve_attribution": serve_attribution,
                }));
            };

            // lattice-router: build the context vector from the now-published live
            // state and forward through the fann network. This is a best-effort
            // routing computation independent of durability, so it reads from
            // `self.state` only after the mutation above has durably committed.
            // Consumption of routed weights lands with the engine route() seam (#343) and the
            // compose value-gate (#346); no per-event stdout/stderr spam.
            #[cfg(feature = "lattice-router")]
            {
                let state = self.state.lock().unwrap();
                let serving_profile = serving_profile_owned.as_str();
                let (rel, sal, temp) = if serving_profile == "balanced-recall-v1" {
                    (
                        state.balanced_recall.relevance.clone(),
                        state.balanced_recall.salience.clone(),
                        state.balanced_recall.temporal.clone(),
                    )
                } else if let Some(ps) = state.profile_states.get(serving_profile) {
                    (
                        ps.relevance.clone(),
                        ps.salience.clone(),
                        ps.temporal.clone(),
                    )
                } else {
                    (
                        state.balanced_recall.relevance.clone(),
                        state.balanced_recall.salience.clone(),
                        state.balanced_recall.temporal.clone(),
                    )
                };
                let sec = state
                    .section_states
                    .get(serving_profile)
                    .map(|ss| ss.posteriors.clone());
                drop(state);
                let ctx = build_context_vector(&rel, &sal, &temp, sec.as_ref());
                let _routed = route_via_fann(&ctx);
            }

            event
        } else {
            // ADR-081 §4 fail-safe: no profile was resolved at all (an explicit
            // serve-time `unattributed` marker, or a ledger row whose accounting
            // profile could not be resolved and was not `unspecified`). The
            // guard above already rejected this case unless `is_gated_implicit`,
            // so `event_write` is always `Gated` here. There is no posterior to
            // mutate and no profile to validate against the registry, so this
            // bypasses `persist_feedback_state_mutation` (and its #1592
            // authoritative re-check, which only applies where a profile was
            // actually resolved) and appends the audit event directly through
            // the fold gate's own atomic unit instead.
            let crate::persist::FeedbackEventWrite::Gated {
                event,
                target_id,
                gate_mode,
                gate_now_us,
                dedup_key,
                grade,
            } = event_write
            else {
                unreachable!(
                    "effective_profile=None implies is_gated_implicit, so event_write is always Gated"
                );
            };
            let dedup_ref = dedup_key
                .as_ref()
                .map(|(scorer, ledger)| (scorer.as_str(), ledger.as_str()));
            let namespace = token.namespace().as_str().to_string();
            let attribution = EventAttribution::from_token(token);
            // Forced-zero unattributed events do not touch the mass table, so
            // this sentinel is an atomic-unit routing key only and can never be
            // mistaken for a registered serving profile.
            let outcome = crate::fold_gate::apply_fold_gate_and_append_event(
                sql.as_ref(),
                &namespace,
                attribution,
                "__unattributed__",
                &target_id,
                gate_mode,
                gate_now_us,
                dedup_ref,
                grade.as_deref(),
                move |fold_outcome, forced_zero| {
                    let mut event = event;
                    let (effective_weight, mass_before, mass_after) = match fold_outcome {
                        Some(o) => (o.effective_weight, o.mass_before, o.mass_after),
                        None => (0.0, 0.0, 0.0),
                    };
                    event.payload["gate"] = json!({
                        "effective_weight": effective_weight,
                        "mass_before": mass_before,
                        "mass_after": mass_after,
                        "forced_zero_weight": forced_zero,
                    });
                    event
                },
            )
            .await?;

            match outcome {
                // This is now the ONLY place a scorer-tagged
                // implicit call returns `deduped` — reached only when the
                // atomic unit's claim conflicted, meaning either a prior call
                // already committed claim+fold+event together, or (never)
                // a partially-failed prior attempt, since a failed attempt
                // rolls back its claim too.
                crate::fold_gate::GateAndAppendOutcome::Deduped => {
                    return Ok(json!({
                        "emitted": false,
                        "deduped": true,
                        "verb": "brain.feedback",
                        "signal": signal,
                        "target_id": target.to_string(),
                        "serve_ledger_id": p.serve_ledger_id,
                        "scorer_run_id": p.scorer_run_id,
                        "served_by_profile_id": effective_profile.as_deref(),
                        "serve_attribution": serve_attribution,
                    }));
                }
                crate::fold_gate::GateAndAppendOutcome::Applied(result) => result.event,
            }
        };

        Ok(json!({
            "emitted": true,
            "event_id": event.id.to_string(),
            "verb": "brain.feedback",
            "signal": signal,
            "target_id": target.to_string(),
            "served_by_profile_id": effective_profile,
            "serve_attribution": serve_attribution,
        }))
    }

    // ── brain.record_serve ────────────────────────────────────────────────

    /// ADR-081 §4/§5 (#394): append one serve-ledger row per target id. The
    /// append remains best-effort accounting, but all rows from one recall
    /// commit atomically so a failed target cannot leave a partial ledger.
    pub(crate) async fn handle_record_serve(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RecordServeParams {
            consumer_kind: String,
            served_by_profile_id: Option<String>,
            #[serde(default)]
            serve_attribution: Option<ServeAttribution>,
            target_ids: Vec<String>,
            query_raw: String,
            served_at: Option<i64>,
        }
        let p: RecordServeParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        if p.target_ids.is_empty() {
            return Ok(json!({
                "ok": true,
                "written": 0,
                "skipped": 0,
                "verb": "brain.record_serve",
            }));
        }

        let namespace = token.namespace().as_str().to_string();
        let query_class = crate::serve_ledger::compute_query_class(&p.query_raw);
        let served_at = p.served_at.unwrap_or_else(|| Utc::now().timestamp_micros());
        let sql = self.runtime.sql();

        let rows = p
            .target_ids
            .iter()
            .map(|target_id| crate::serve_ledger::ServeLedgerInsert {
                id: uuid::Uuid::new_v4().to_string(),
                target_id: target_id.clone(),
            })
            .collect::<Vec<_>>();
        let summary = match crate::serve_ledger::record_serves(
            sql.as_ref(),
            &rows,
            &namespace,
            &p.consumer_kind,
            p.served_by_profile_id.as_deref(),
            None,
            None,
            &query_class,
            &p.query_raw,
            served_at,
            p.serve_attribution.map(ServeAttribution::as_str),
        )
        .await
        {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!("[brain] serve ledger batch write failed (non-fatal): {e}");
                crate::serve_ledger::ServeLedgerBatchSummary::default()
            }
        };

        Ok(json!({
            "ok": true,
            "written": summary.written,
            "skipped": summary.skipped,
            "query_class": query_class,
            "verb": "brain.record_serve",
        }))
    }

    // ── brain.mark_turn ───────────────────────────────────────────────────

    /// Best-effort per-actor work-unit marker. Reuses the
    /// ADR-103 Stage 1 `PhaseStarted`/`work_class` vocabulary (fixed
    /// `work_class = "actor_turn"`) instead of minting a new `EventKind`
    /// variant, since `EventKind` is a closed enum owned by the OSS
    /// `khive-types` crate.
    pub(crate) async fn handle_mark_turn(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MarkTurnParams {
            label: Option<String>,
        }
        let p: MarkTurnParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;
        if let Some(label) = p.label.as_deref() {
            khive_runtime::secret_gate::check_at(label, "turn", "label")?;
        }
        let phase = p.label.unwrap_or_else(|| "actor_turn".to_string());
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        khive_runtime::emit_phase_event(
            &self.runtime,
            token,
            "brain.mark_turn",
            khive_types::EventKind::PhaseStarted,
            khive_storage::PhaseStartedPayload {
                work_class: "actor_turn".to_string(),
                phase,
                corpus_size: None,
            },
        )
        .await;

        Ok(json!({
            "ok": true,
            "work_class": "actor_turn",
            "actor": actor,
        }))
    }

    // ── brain.emit (deprecated) ───────────────────────────────────────────

    /// Deprecated alias for `brain.feedback`; routes to `handle_feedback`.
    pub(crate) async fn handle_emit(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        self.handle_feedback_from(token, params, "brain.emit", registry)
            .await
    }

    // ── brain.bind ────────────────────────────────────────────────────────

    pub(crate) async fn handle_bind(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BindParams {
            profile_id: String,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
            priority: Option<i32>,
        }
        let p: BindParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let actor = p.actor.unwrap_or_else(|| "*".into());
        let namespace = p.namespace.unwrap_or_else(|| "*".into());
        let consumer_kind = p.consumer_kind.unwrap_or_else(|| "*".into());

        // Validate that '*' is not used as a real value ('*' is reserved as the wildcard sentinel)
        for (field, val) in [
            ("actor", &actor),
            ("namespace", &namespace),
            ("consumer_kind", &consumer_kind),
        ] {
            if val.as_str() != "*" && val.contains('*') {
                return Err(RuntimeError::InvalidInput(format!(
                    "{field}: '*' is reserved as the wildcard sentinel and cannot appear inside a real value"
                )));
            }
        }

        // Secret gate: scan arbitrary text fields before writing.
        // Wildcard sentinel `*` is safe; real values are scanned.
        if actor != "*" {
            khive_runtime::secret_gate::check_at(&actor, "binding", "actor")?;
        }
        if namespace != "*" {
            khive_runtime::secret_gate::check_at(&namespace, "binding", "namespace")?;
        }
        if consumer_kind != "*" {
            khive_runtime::secret_gate::check_at(&consumer_kind, "binding", "consumer_kind")?;
        }

        if consumer_kind != "*" {
            let mut valid = registry.all_brain_consumer_kinds();
            if !valid.contains(&consumer_kind.as_str()) {
                valid.sort_unstable();
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown consumer_kind {consumer_kind:?}; valid: * | {}",
                    valid.join(" | ")
                )));
            }
        }

        let profile_id = p.profile_id;
        let priority = p.priority.unwrap_or(0);
        let event_payload = json!({
            "profile_id": profile_id,
            "actor": actor,
            "namespace": namespace,
            "consumer_kind": consumer_kind,
            "priority": priority,
        });

        // Route through the durable-write helper (issue #457): the binding
        // change is applied to a proposed state copy and only takes effect
        // after the brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: profile_id.clone(),
                event_kind: "brain.bind".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                // Verify the profile exists and is not archived (archived = terminal, no new bindings).
                match state.profiles.get(&profile_id) {
                    None => {
                        return Err(RuntimeError::NotFound(format!("profile {:?}", profile_id)));
                    }
                    Some(record) if record.lifecycle == ProfileLifecycle::Archived => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "profile {:?} is archived; bindings to archived profiles are not permitted",
                            profile_id
                        )));
                    }
                    Some(_) => {}
                }

                // Remove any existing binding for the same (actor, namespace, consumer_kind)
                state.bindings.retain(|b| {
                    !(b.actor == actor
                        && b.namespace == namespace
                        && b.consumer_kind == consumer_kind)
                });

                state.bindings.push(ProfileBinding {
                    actor: actor.clone(),
                    namespace: namespace.clone(),
                    consumer_kind: consumer_kind.clone(),
                    profile_id: profile_id.clone(),
                    priority,
                    created_at: Utc::now(),
                });

                Ok(json!({
                    "bound": true,
                    "profile_id": profile_id,
                    "actor": actor,
                    "namespace": namespace,
                    "consumer_kind": consumer_kind,
                }))
            },
        )
        .await
    }

    // ── brain.unbind ──────────────────────────────────────────────────────

    pub(crate) async fn handle_unbind(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UnbindParams {
            profile_id: Option<String>,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
        }
        let p: UnbindParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        // Require at least one non-null filter to prevent accidental bulk-delete.
        if p.profile_id.is_none()
            && p.actor.is_none()
            && p.namespace.is_none()
            && p.consumer_kind.is_none()
        {
            return Err(RuntimeError::InvalidInput(
                "unbind requires at least one filter; pass profile_id, actor, namespace, or consumer_kind".into(),
            ));
        }

        let event_payload = json!({
            "profile_id": p.profile_id,
            "actor": p.actor,
            "namespace": p.namespace,
            "consumer_kind": p.consumer_kind,
        });
        let event_profile_id = p.profile_id.clone().unwrap_or_else(|| "*".to_string());

        // Route through the durable-write helper (issue #457): the binding
        // removal is applied to a proposed state copy and only takes effect
        // after the brain event-log append + snapshot upsert commit.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: event_profile_id,
                event_kind: "brain.unbind".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                let before = state.bindings.len();

                state.bindings.retain(|b| {
                    let pid_match = p.profile_id.as_ref().is_none_or(|id| &b.profile_id == id);
                    let actor_match = p.actor.as_ref().is_none_or(|a| &b.actor == a);
                    let ns_match = p.namespace.as_ref().is_none_or(|n| &b.namespace == n);
                    let kind_match = p
                        .consumer_kind
                        .as_ref()
                        .is_none_or(|k| &b.consumer_kind == k);
                    // Retain if this binding does NOT match ALL of the provided filters.
                    // A filter that is absent (None) matches everything — only bindings
                    // satisfying every supplied criterion are removed.
                    !(pid_match && actor_match && ns_match && kind_match)
                });

                let removed = before - state.bindings.len();
                // `removed` is the canonical mutation count. Keep `unbound`
                // as a compatibility alias for existing callers and smoke
                // tests. In particular, both fields remain present at zero so
                // a successful no-op cannot be mistaken for a confirmed
                // removal.
                Ok(json!({ "removed": removed, "unbound": removed }))
            },
        )
        .await
    }

    // ── brain.bindings ────────────────────────────────────────────────────

    pub(crate) async fn handle_bindings(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        // Inspection verb — list binding rows, optionally filtered.
        #[derive(Deserialize)]
        struct BindingsParams {
            profile_id: Option<String>,
            actor: Option<String>,
            namespace: Option<String>,
            consumer_kind: Option<String>,
        }
        let p: BindingsParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let caller = token.actor().label();
        let actor = p.actor.as_deref().unwrap_or(&caller);
        Self::check_read_actor(token, actor)?;

        let state = self.state.lock().unwrap();
        let rows: Vec<Value> = state
            .bindings
            .iter()
            .filter(|b| {
                p.profile_id.as_ref().is_none_or(|id| &b.profile_id == id)
                    && b.actor == actor
                    && p.namespace.as_ref().is_none_or(|n| &b.namespace == n)
                    && p.consumer_kind
                        .as_ref()
                        .is_none_or(|k| &b.consumer_kind == k)
            })
            .map(|b| {
                json!({
                    "profile_id": b.profile_id,
                    "actor": b.actor,
                    "namespace": b.namespace,
                    "consumer_kind": b.consumer_kind,
                    "priority": b.priority,
                    "created_at": b.created_at,
                })
            })
            .collect();

        Ok(json!({ "count": rows.len(), "bindings": rows }))
    }

    // ── brain.create_profile ──────────────────────────────────────────────

    pub(crate) async fn handle_create_profile(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        // Allow external agents to create new profiles via MCP.
        // seed_priors: optional object for seeding section priors, or null for defaults.
        #[derive(Deserialize)]
        struct CreateProfileParams {
            name: String,
            description: Option<String>,
            consumer_kind: Option<String>,
            seed_priors: Option<serde_json::Value>,
        }
        let p: CreateProfileParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        // Validate profile-id grammar: trim, reject empty, enforce ASCII alphanumeric + hyphen.
        let name = p.name.trim().to_string();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "name must not be empty or whitespace-only".into(),
            ));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(RuntimeError::InvalidInput(format!(
                "name {:?} is invalid; profile IDs must match [a-zA-Z0-9-]+ (alphanumeric and hyphens only)",
                name
            )));
        }
        let p_name = name;

        // Validate consumer_kind — reject empty/whitespace and wildcard sentinel.
        let consumer_kind = p.consumer_kind.unwrap_or_else(|| "recall".into());
        let ck_trimmed = consumer_kind.trim();
        if ck_trimmed.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "consumer_kind must not be empty or whitespace".into(),
            ));
        }
        if ck_trimmed == "*" {
            return Err(RuntimeError::InvalidInput(
                "consumer_kind '*' is the wildcard sentinel and is not permitted for profile creation; provide a specific operation kind (e.g. \"recall\", \"search\")"
                    .into(),
            ));
        }

        let description = p
            .description
            .unwrap_or_else(|| format!("User-created profile: {}", p_name));

        // Secret gate: scan caller-supplied text before any write.
        // `p_name` is already constrained to [a-zA-Z0-9-]+ and cannot carry a secret.
        khive_runtime::secret_gate::check_at(&description, "profile", "description")?;
        khive_runtime::secret_gate::check_at(&consumer_kind, "profile", "consumer_kind")?;
        if let Some(ref seed) = p.seed_priors {
            khive_runtime::secret_gate::check_json_at(seed, "profile", "seed_priors")?;
            if let Some(unsupported) = seed.as_object().and_then(|fields| {
                fields
                    .keys()
                    .find(|key| key.as_str() != "section_posteriors")
            }) {
                return Err(RuntimeError::InvalidInput(format!(
                    "seed_priors has unsupported field {unsupported:?}; only 'section_posteriors' is accepted"
                )));
            }
        }
        let seed_priors = p.seed_priors;

        let event_payload = json!({
            "profile_id": p_name,
            "consumer_kind": consumer_kind,
            "description": description,
        });

        // Route through the durable-write helper (issue #457): the new
        // profile/state/section rows are applied to a proposed state copy
        // and only take effect after the brain event-log append + snapshot
        // upsert commit — so a persistence failure never leaves a phantom
        // profile that vanishes on restart.
        crate::persist::persist_brain_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            crate::persist::BrainMutationEvent {
                profile_id: p_name.clone(),
                event_kind: "brain.create_profile".to_string(),
                payload: event_payload,
            },
            ENTITY_CACHE_CAPACITY,
            move |state: &mut khive_brain_core::BrainState| -> Result<Value, RuntimeError> {
                if state.profiles.contains_key(&p_name) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "profile {:?} already exists",
                        p_name
                    )));
                }

                // Initialize live BalancedRecallState for this profile so that reset and
                // feedback can route to its actual posteriors rather than a metadata-only record.
                let ps = khive_brain_core::BalancedRecallState::new(ENTITY_CACHE_CAPACITY);
                let snap = serde_json::to_value(ps.to_snapshot()).ok();

                let record = ProfileRecord {
                    id: p_name.clone(),
                    description: description.clone(),
                    consumer_kind: consumer_kind.clone(),
                    state_class: "Bayesian".into(),
                    lifecycle: ProfileLifecycle::Inactive,
                    created_at: Utc::now(),
                    state_snapshot: snap,
                    total_events: 0,
                    exploration_epoch: 0,
                };

                // Seed section posteriors: parse explicit section_posteriors object if provided,
                // else use default informative priors.
                let section_state = if let Some(ref seed) = seed_priors {
                    if let Some(sp_obj) = seed.get("section_posteriors").and_then(|v| v.as_object())
                    {
                        let mut priors = std::collections::HashMap::new();
                        for (key, val) in sp_obj {
                            let st: SectionType = key.parse().map_err(|_| {
                                RuntimeError::InvalidInput(format!("unknown section type: {key:?}"))
                            })?;
                            let alpha =
                                val.get("alpha").and_then(|v| v.as_f64()).ok_or_else(|| {
                                    RuntimeError::InvalidInput(format!(
                                        "missing or invalid alpha for section {key:?}"
                                    ))
                                })?;
                            let beta =
                                val.get("beta").and_then(|v| v.as_f64()).ok_or_else(|| {
                                    RuntimeError::InvalidInput(format!(
                                        "missing or invalid beta for section {key:?}"
                                    ))
                                })?;
                            let posterior = khive_brain_core::BetaPosterior::try_new(alpha, beta)
                                .map_err(|e| {
                                RuntimeError::InvalidInput(format!(
                                    "invalid seed_priors for section {key:?}: {e}"
                                ))
                            })?;
                            let ess = posterior.effective_sample_size();
                            if ess > DEFAULT_ESS_CAP {
                                return Err(RuntimeError::InvalidInput(format!(
                                    "seed_priors for section {key:?}: alpha+beta ({ess}) exceeds \
                                     maximum allowed ESS ({DEFAULT_ESS_CAP}); use values where \
                                     alpha + beta <= {DEFAULT_ESS_CAP}"
                                )));
                            }
                            priors.insert(st, posterior);
                        }
                        SectionPosteriorState::from_priors(priors)
                    } else {
                        return Err(RuntimeError::InvalidInput(
                            "seed_priors must contain a 'section_posteriors' object".into(),
                        ));
                    }
                } else {
                    SectionPosteriorState::new()
                };

                state.profiles.insert(p_name.clone(), record);
                state.profile_states.insert(p_name.clone(), ps);
                state.section_states.insert(p_name.clone(), section_state);

                Ok(json!({
                    "created": true,
                    "profile_id": p_name,
                    "consumer_kind": consumer_kind,
                    "lifecycle": "inactive",
                    "description": description,
                }))
            },
        )
        .await
    }

    // ── brain.register_adapter ────────────────────────────────────────────

    pub(crate) async fn handle_register_adapter(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RegisterAdapterParams {
            adapter_id: String,
            content_hash: String,
            base_model_revision: String,
            metadata: Option<serde_json::Map<String, serde_json::Value>>,
        }
        let p: RegisterAdapterParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let active_revision = std::env::var("KHIVE_BRAIN_BASE_MODEL_REVISION")
            .unwrap_or_else(|_| DEFAULT_BASE_MODEL_REVISION.to_string());

        if p.base_model_revision != active_revision {
            return Err(RuntimeError::InvalidInput(format!(
                "base_model_revision mismatch: expected {:?}, got {:?}",
                active_revision, p.base_model_revision
            )));
        }

        let mut props = serde_json::json!({
            "content_hash": p.content_hash,
            "base_model_revision": p.base_model_revision,
        });
        if let Some(meta) = p.metadata {
            let props_obj = props.as_object_mut().unwrap();
            for (k, v) in meta {
                if k != "content_hash" && k != "base_model_revision" {
                    props_obj.insert(k, v);
                }
            }
        }

        let (_, embedding_report) = self
            .runtime
            .create_entity_with_embedding_report(
                token,
                "artifact",
                Some("adapter"),
                &p.adapter_id,
                None,
                Some(props),
                vec![],
            )
            .await?;

        let mut response = json!({
            "registered": true,
            "adapter_id": p.adapter_id,
            "content_hash": p.content_hash,
            "base_model_revision": p.base_model_revision,
        });
        if embedding_report.any_truncated() {
            response["warnings"] =
                json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
        }
        Ok(response)
    }
}
