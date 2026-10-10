use super::*;

struct UnjudgedServe<'a> {
    profile_id: Option<&'a str>,
    attribution: Option<ServeAttribution>,
    scorer_pair: Option<(&'a str, &'a str)>,
}

impl BrainPack {
    // ── brain.auto_feedback ───────────────────────────────────────────────

    /// Emit caller-attributed feedback for one selected `memory.recall` result.
    pub(crate) async fn handle_auto_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AutoFeedbackParams {
            query: String,
            results: Vec<AutoFeedbackResult>,
            target_id: Option<String>,
            signal: Option<String>,
            served_by_profile_id: Option<String>,
            serve_attribution: Option<ServeAttribution>,
            // ADR-081 §6: together-or-rejected provenance. Unjudged retains
            // it without claiming a grade; judgments own the atomic claim.
            scorer_run_id: Option<String>,
            serve_ledger_id: Option<String>,
            /// Exact namespace for the emitted event and posterior fold. The
            /// registry normally pre-applies this to `token`; retaining the
            /// field here gives direct `PackRuntime` callers the same contract.
            namespace: Option<String>,
        }

        #[derive(Deserialize)]
        struct AutoFeedbackResult {
            id: String,
            full_id: Option<String>,
            served_by_profile_id: Option<String>,
            serve_attribution: Option<ServeAttribution>,
        }

        // `results` is the shape callers get wrong, because `results[].id` is
        // exactly what they just read out of a recall response and an array of
        // those ids is one character away from the accepted form. The
        // deserializer's own refusal names an internal type and neither the
        // parameter nor the shape, so a caller who cannot read this file has
        // nowhere to go. This check only improves the message for inputs serde
        // rejects anyway; it adds no rule, and anything it admits still has to
        // pass the deserializer below.
        fn malformed_results(params: &Value) -> Option<String> {
            const WANTED: &str =
                "`results` must be an array of result objects, each with a string \
                                  `id`; pass the whole result set, for example \
                                  results=[{\"id\": \"1e8807ef\"}, {\"id\": \"c3f21b90\"}]";
            let results = params.get("results")?;
            let Some(items) = results.as_array() else {
                return Some(format!(
                    "auto_feedback: {WANTED}. Received {} instead of an array.",
                    json_shape(results)
                ));
            };
            let (index, bad) = items
                .iter()
                .enumerate()
                .find(|(_, item)| !item.get("id").is_some_and(Value::is_string))?;
            Some(format!(
                "auto_feedback: {WANTED}. Element {index} is {}.",
                json_shape(bad)
            ))
        }

        fn json_shape(value: &Value) -> String {
            match value {
                Value::Null => "null".to_string(),
                Value::Bool(_) => "a boolean".to_string(),
                Value::Number(_) => "a number".to_string(),
                Value::String(_) => "a string".to_string(),
                Value::Array(_) => "an array".to_string(),
                Value::Object(_) => "an object with no string `id`".to_string(),
            }
        }

        if let Some(message) = malformed_results(&params) {
            return Err(RuntimeError::InvalidInput(message));
        }

        let p: AutoFeedbackParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // Registry dispatch pre-mints an exact token for an explicit
        // namespace. Direct PackRuntime callers must supply that same token;
        // a business parameter is never authority to mint a capability.
        if let Some(ns_str) = p.namespace.as_deref() {
            let requested = Namespace::parse(ns_str).map_err(|e| {
                RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
            })?;
            if &requested != token.namespace() {
                return Err(RuntimeError::InvalidInput(format!(
                    "auto_feedback: namespace {ns_str:?} does not match authorized token namespace {:?}",
                    token.namespace().as_str()
                )));
            }
        }

        if p.query.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "auto_feedback: `query` must not be empty".into(),
            ));
        }
        // Preserve ADR-081's malformed-pair rejection even when abstention
        // returns before the delegated feedback handler runs.
        if p.scorer_run_id.is_some() != p.serve_ledger_id.is_some() {
            return Err(RuntimeError::InvalidInput(
                "scorer_run_id and serve_ledger_id must be supplied together".to_string(),
            ));
        }

        let signal = match p.signal.as_deref() {
            Some(signal) => signal,
            None if p.results.is_empty() => {
                return Ok(json!({
                    "emitted": false,
                    "verb": "brain.auto_feedback",
                    "reason": "no_results",
                }));
            }
            None => {
                return Ok(json!({
                    "emitted": false,
                    "verb": "brain.auto_feedback",
                    "reason": "no_signal",
                    "result_count": p.results.len(),
                }));
            }
        };

        let selected_id = p.target_id.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "auto_feedback: `target_id` is required when `signal` is supplied; it must exactly match one result object's id or full_id"
                    .to_string(),
            )
        })?;
        let mut matching_results = p.results.iter().filter(|result| {
            result.id == selected_id || result.full_id.as_deref() == Some(selected_id)
        });
        let selected = matching_results.next().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "auto_feedback: target_id {selected_id:?} does not match any results[].id or results[].full_id"
            ))
        })?;
        if matching_results.next().is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "auto_feedback: target_id {selected_id:?} matches more than one result; the judged result must be unique"
            )));
        }

        let target = match selected.full_id.as_deref() {
            Some(full_id) => full_id.parse::<uuid::Uuid>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "auto_feedback: invalid full_id {full_id:?}; expected full UUID"
                ))
            })?,
            None => {
                resolve_auto_feedback_target(&self.runtime, token, registry, &selected.id).await?
            }
        };

        let mut feedback_params = json!({
            "target_id": target.to_string(),
            "signal": signal,
        });
        // Prefer explicit top-level attribution, otherwise carry it directly
        // from the selected recall result. Rank position never selects either
        // the target or its serving metadata.
        let (served_by_profile_id, serve_attribution) =
            if p.served_by_profile_id.is_some() || p.serve_attribution.is_some() {
                (p.served_by_profile_id.as_ref(), p.serve_attribution)
            } else {
                (
                    selected.served_by_profile_id.as_ref(),
                    selected.serve_attribution,
                )
            };
        if let Some(profile_id) = served_by_profile_id {
            feedback_params["served_by_profile_id"] = json!(profile_id);
        }
        if let Some(attribution) = serve_attribution {
            feedback_params["serve_attribution"] = json!(attribution);
        }
        if let Some(ref scorer_run_id) = p.scorer_run_id {
            feedback_params["scorer_run_id"] = json!(scorer_run_id);
        }
        if let Some(ref serve_ledger_id) = p.serve_ledger_id {
            feedback_params["serve_ledger_id"] = json!(serve_ledger_id);
        }
        // #1509 cheap-half: forward what already arrived instead of dropping it —
        // the serving `query` and the raw (pre-resolution) candidate ids for
        // every result, not just the explicitly selected one.
        feedback_params["query"] = json!(p.query);
        feedback_params["candidate_ids"] =
            json!(p.results.iter().map(|r| r.id.clone()).collect::<Vec<_>>());

        let mut out = if signal == "unjudged" {
            self.handle_unjudged_feedback(
                token,
                target,
                UnjudgedServe {
                    profile_id: served_by_profile_id.map(String::as_str),
                    attribution: serve_attribution,
                    scorer_pair: p.scorer_run_id.as_deref().zip(p.serve_ledger_id.as_deref()),
                },
                feedback_params,
                registry,
            )
            .await?
        } else {
            self.handle_feedback_from(token, feedback_params, "brain.auto_feedback", registry)
                .await?
        };
        out["verb"] = json!("brain.auto_feedback");
        out["feedback_verb"] = json!("brain.feedback");
        out["result_count"] = json!(p.results.len());
        Ok(out)
    }

    /// A3 telemetry has provenance but no training destination or grade claim.
    async fn handle_unjudged_feedback(
        &self,
        token: &NamespaceToken,
        target: uuid::Uuid,
        provenance: UnjudgedServe<'_>,
        mut payload: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let started = Instant::now();
        let target_substrate = match registry
            .resolve_kg_read_by_id(&self.runtime, token, target, false)
            .await?
        {
            Some(khive_runtime::Resolved::Entity(_)) => khive_types::SubstrateKind::Entity,
            Some(khive_runtime::Resolved::Note(_)) => khive_types::SubstrateKind::Note,
            _ => {
                return Err(RuntimeError::NotFound(format!(
                    "target_id {target:?} not found"
                )));
            }
        };
        let mut profile_id = provenance.profile_id.map(str::to_owned);
        let mut attribution = provenance.attribution.unwrap_or_else(|| {
            if profile_id.is_some() {
                ServeAttribution::Profile
            } else {
                ServeAttribution::Unspecified
            }
        });
        match (attribution, profile_id.as_deref()) {
            (ServeAttribution::Profile, None) => {
                return Err(RuntimeError::InvalidInput(
                    "serve_attribution=\"profile\" requires served_by_profile_id".to_string(),
                ));
            }
            (ServeAttribution::Unattributed | ServeAttribution::Unspecified, Some(_)) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "serve_attribution=\"{}\" cannot include served_by_profile_id",
                    attribution.as_str()
                )));
            }
            _ => {}
        }

        if let Some((scorer_run_id, serve_ledger_id)) = provenance.scorer_pair {
            match crate::serve_ledger::resolve(
                self.runtime.sql().as_ref(),
                serve_ledger_id,
                scorer_run_id,
                token.namespace().as_str(),
                &target.to_string(),
                provenance.profile_id,
            )
            .await?
            {
                crate::serve_ledger::ServeLedgerResolution::NotFound => {
                    return Err(RuntimeError::NotFound(format!(
                        "serve_ledger_id {serve_ledger_id:?} not found"
                    )));
                }
                crate::serve_ledger::ServeLedgerResolution::Found {
                    accounting_profile_id,
                    serve_attribution: ledger_attribution,
                    // Telemetry neither consumes nor requires an unused slot.
                    already_graded: _,
                } => {
                    if let Some(accounting_profile_id) = accounting_profile_id {
                        if attribution == ServeAttribution::Unattributed {
                            return Err(RuntimeError::InvalidInput(
                                "serve_attribution=\"unattributed\" conflicts with the serve ledger accounting_profile_id"
                                    .to_string(),
                            ));
                        }
                        profile_id = Some(accounting_profile_id);
                        attribution = ServeAttribution::Profile;
                    } else if ledger_attribution != Some(ServeAttribution::Unspecified) {
                        profile_id = None;
                        attribution = ServeAttribution::Unattributed;
                    }
                }
            }
        }

        // Preserve serve-time provenance, including historical profiles. No
        // binding/default resolver or profile lifecycle credit check applies:
        // this event never trains a profile and does not enter its private log.
        payload["originating_verb"] = json!("brain.auto_feedback");
        payload["served_by_profile_id"] = json!(profile_id);
        payload["serve_attribution"] = json!(attribution);
        let event = Event::new(
            token.namespace().as_str().to_string(),
            "brain.feedback",
            khive_types::EventKind::FeedbackUnjudged,
            target_substrate,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_target(target)
        .with_payload(payload)
        .with_duration_us(started.elapsed().as_micros().max(1) as i64);
        let event_id = event.id;
        self.runtime.events(token)?.append_event(event).await?;
        Ok(json!({
            "emitted": true,
            "event_id": event_id.to_string(),
            "verb": "brain.feedback",
            "signal": "unjudged",
            "target_id": target.to_string(),
            "served_by_profile_id": profile_id,
            "serve_attribution": attribution,
        }))
    }
}
