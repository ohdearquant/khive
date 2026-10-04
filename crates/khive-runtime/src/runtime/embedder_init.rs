use super::*;

impl KhiveRuntime {
    pub(super) async fn embedder_inner(
        &self,
        name: &str,
        token: Option<&NamespaceToken>,
    ) -> RuntimeResult<(Arc<dyn EmbeddingService>, bool)> {
        // Fall back to the literal name (not the alias table) so custom
        // providers registered with non-lattice names stay reachable.
        let canonical_key = match parse_embedding_model_alias(name) {
            Some(model) => model.to_string(),
            None => name.to_owned(),
        };
        if request_excludes_embedder(&canonical_key) {
            return Err(crate::RuntimeError::UnknownModel(name.to_string()));
        }
        // Clone the entry so we don't hold the RwLockGuard across the
        // async OnceCell initialisation (Send bound).
        let entry = {
            let registry = self.embedder_registry.read().map_err(|_| {
                crate::RuntimeError::Internal("embedder registry lock poisoned".into())
            })?;
            registry
                .get_entry(&canonical_key)
                .ok_or_else(|| crate::RuntimeError::UnknownModel(name.to_string()))?
        };
        let audited_document_preparation = entry.has_audited_document_preparation();
        if let Some(service) = entry.cached_service() {
            return Ok((service, audited_document_preparation));
        }
        let runtime = self.clone();
        let token = token.cloned();
        let operation = khive_storage::operation_context::current_operation_attribution();
        let usage = crate::usage::current();
        // Cold construction and its one-shot event belong to the shared entry,
        // rather than the lifetime of the request that first encounters it.
        // Preserve model exclusions, event provenance and usage accounting, but
        // do not inherit the request's read deadline/cancellation. Query
        // embedding stays inline.
        let initialization = inherit_request_embedder_scope(async move {
            let initialize = async move {
                let (service, init_duration_us) = entry.resolve().await?;
                if let Some(duration_us) = init_duration_us {
                    if let Some(token) = token {
                        runtime
                            .emit_embedder_initialized(&token, &canonical_key, duration_us)
                            .await;
                    } else if let Ok(token) =
                        runtime.authorize(runtime.config.default_namespace.clone())
                    {
                        runtime
                            .emit_embedder_initialized(&token, &canonical_key, duration_us)
                            .await;
                    }
                }
                Ok::<_, RuntimeError>((service, audited_document_preparation))
            };
            match operation {
                Some(operation) => {
                    khive_storage::operation_context::scope_operation_attribution(
                        operation, initialize,
                    )
                    .await
                }
                None => initialize.await,
            }
        });
        tokio::spawn(async move {
            match usage {
                Some(usage) => crate::usage::scope(usage, initialization).await,
                None => initialization.await,
            }
        })
        .await
        .map_err(|error| {
            RuntimeError::Internal(format!(
                "embedder '{name}' initialization task failed: {error}"
            ))
        })?
    }

    async fn emit_embedder_initialized(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        duration_us: i64,
    ) {
        // Lazy embedder construction can happen during daemon warm or an
        // assertive request. A snapshot has no durable audit sink, so do not
        // resolve an EventStore merely to attempt a known-rejected append.
        if self.is_read_only() {
            return;
        }
        let Ok(store) = self.events(token) else {
            return;
        };
        let event = Event::new(
            token.namespace().as_str(),
            "embedder.init",
            EventKind::EmbedderInitialized,
            SubstrateKind::Event,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_payload(serde_json::json!({
            "model_name": model_name,
            "duration_us": duration_us,
        }))
        .with_duration_us(duration_us);
        if let Err(err) = store.append_event(event).await {
            tracing::warn!(error = %err, model_name, "embedder initialization event append failed");
        }
    }
}
