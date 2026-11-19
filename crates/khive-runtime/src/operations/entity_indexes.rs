//! Required synchronous index effects for admitted entity candidates.

use super::*;

impl KhiveRuntime {
    /// Prepare required index effects before an admitted entity takes its writer.
    /// Bulk and direct structural ingest deliberately defer dense vectors.
    pub(crate) async fn prepare_admitted_entity_indexes(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        attachments: &[Attachment],
        embed: bool,
    ) -> RuntimeResult<(
        Vec<PlanStatement>,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let _ = self.entities(token)?;
        let _ = self.text(token)?;
        let document = entity_fts_document(entity);
        let mut statements = vec![PlanStatement {
            statement: khive_db::stores::text::delete_document_statement(
                "fts_entities",
                &entity.namespace,
                entity.id,
            ),
            guard: None,
        }];
        statements.extend(
            insert_document_statements("fts_entities", &document)
                .into_iter()
                .map(|statement| PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }),
        );
        for attachment in attachments {
            statements.push(PlanStatement {
                statement: khive_db::stores::attachment::attachment_upsert_statement(attachment)?,
                guard: Some(AffectedRowGuard::exactly(1)),
            });
        }
        let mut report = crate::retrieval::EmbeddingTruncationReport::default();
        if !embed {
            return Ok((statements, report));
        }

        let models = self.registered_embedding_model_names();
        let model_metadata = models
            .iter()
            .map(|name| {
                self.vector_model_metadata(name)
                    .map(|(name, dimensions)| (crate::config::sanitize_key(&name), dimensions))
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        let model_specs: Vec<_> = model_metadata
            .iter()
            .map(|(key, dimensions)| (key.as_str(), *dimensions))
            .collect();
        if !model_specs.is_empty() {
            self.backend().ensure_vector_tables(&model_specs)?;
        }
        for (model, (table_key, dimensions)) in models.iter().zip(model_metadata.iter()) {
            let outcome = self
                .embed_document_with_model_outcome_for_token(token, model, &document.body)
                .await?;
            if outcome.vector.len() != *dimensions {
                return Err(RuntimeError::InvalidInput(format!(
                    "entity embedding dimension mismatch for model {model}"
                )));
            }
            if outcome.vector.iter().any(|value| !value.is_finite()) {
                return Err(RuntimeError::InvalidInput(
                    "entity embedding contains a non-finite value".into(),
                ));
            }
            report.observe(&outcome);
            statements.extend(
                crate::atomic_message::vector_insert_statements_for_substrate(
                    &format!("vec_{table_key}"),
                    &entity.namespace,
                    entity.id,
                    SubstrateKind::Entity,
                    "entity.body",
                    model,
                    &outcome.vector,
                    "entity-finalizer-vector",
                    Some(AffectedRowGuard::exactly(1)),
                ),
            );
        }
        Ok((statements, report))
    }
}
