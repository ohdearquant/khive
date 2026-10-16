//! Runtime-owned entity candidate admission. Production snapshots stay empty.

use std::sync::Arc;

use khive_storage::Entity;
use serde_json::Value;

use super::manifest::{digest_to_hex, scoped_digest, ManifestSnapshot, RuntimeFieldScope};
use super::outcome::{FinalizerOutcome, ManifestFault};
use crate::{EventAttribution, NamespaceToken, RuntimeError, RuntimeResult};

#[cfg(test)]
pub(crate) mod fixture;

pub(crate) const EXEMPTION_STAMP: &str = "exempted:content-sha256-manifest-v1";

#[derive(Debug, Clone, Copy)]
pub(crate) enum EntityEntryPoint {
    Create,
    Claim,
    Update,
    Bulk,
    CodeIngest,
    CodeFindingsIngest,
}

impl EntityEntryPoint {
    pub(crate) fn family(self, replacing: bool) -> &'static str {
        match self {
            Self::Update => "entity.update",
            Self::Bulk => "entity.bulk",
            Self::CodeIngest | Self::CodeFindingsIngest if replacing => "entity.update",
            _ => "entity.create",
        }
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::CodeIngest => "code.ingest",
            Self::CodeFindingsIngest => "code.findings_ingest",
            _ => "create",
        }
    }
}

/// Closed direct-ingest origins; callers cannot supply audit verb strings.
#[derive(Debug, Clone, Copy)]
pub enum EntityCandidateOrigin {
    CodeIngest,
    CodeFindingsIngest,
}

/// An immutable admission snapshot captured before direct-ingest preflight.
/// Clones retain that same snapshot through bounded CAS retries.
#[derive(Debug, Clone)]
pub struct EntityCandidateContext {
    namespace: String,
    snapshot: Arc<ManifestSnapshot>,
    entry: EntityEntryPoint,
    record: String,
}

/// A checked direct-ingest candidate, without a caller-visible runtime stamp.
#[derive(Debug)]
pub struct EntityCandidatePrepared {
    context: EntityCandidateContext,
    candidate: Entity,
}

impl EntityCandidatePrepared {
    pub fn entity(&self) -> &Entity {
        &self.candidate
    }

    pub(crate) fn admit(self, token: &NamespaceToken) -> RuntimeResult<EntityAdmission> {
        if token.namespace().as_str() != self.context.namespace {
            return Err(RuntimeError::InvalidInput(
                "direct-ingest candidate namespace differs from its authorization token".into(),
            ));
        }
        admit(self.context, token, self.candidate)
    }
}

impl EntityCandidateContext {
    pub fn new(namespace: &str, origin: EntityCandidateOrigin) -> Self {
        Self::capture(
            namespace,
            match origin {
                EntityCandidateOrigin::CodeIngest => EntityEntryPoint::CodeIngest,
                EntityCandidateOrigin::CodeFindingsIngest => EntityEntryPoint::CodeFindingsIngest,
            },
        )
    }

    fn capture(namespace: &str, entry: EntityEntryPoint) -> Self {
        #[cfg(test)]
        let snapshot = fixture::snapshot(namespace);
        #[cfg(not(test))]
        let snapshot = ManifestSnapshot::empty();
        Self {
            namespace: namespace.into(),
            snapshot,
            entry,
            record: "entity".into(),
        }
    }

    pub fn check_name_description(&self, value: &str) -> RuntimeResult<()> {
        self.check_value(RuntimeFieldScope::NameDescription, value)
    }

    pub fn check_properties(&self, properties: Option<&Value>) -> RuntimeResult<()> {
        crate::secret_gate::reject_reserved_secret_gate_property(properties)?;
        if let Some(value) = properties {
            let mut values = Vec::new();
            property_strings(value, &mut values);
            for value in values {
                self.check_value(RuntimeFieldScope::JsonProperties, value)?;
            }
        }
        Ok(())
    }

    pub fn check_tags(&self, tags: &[String]) -> RuntimeResult<()> {
        for tag in tags {
            self.check_value(RuntimeFieldScope::Tags, tag)?;
        }
        Ok(())
    }

    /// Consume one clone for the exact final candidate; no database is opened.
    pub fn prepare(self, candidate: Entity) -> RuntimeResult<EntityCandidatePrepared> {
        if candidate.namespace != self.namespace {
            return Err(RuntimeError::InvalidInput(
                "direct-ingest candidate namespace changed after preflight".into(),
            ));
        }
        let matched = scan_candidate(&self, &candidate)?;
        // This direct lane writes structural rows, not attachment state. A
        // committed receipt must not echo an unverified attachment projection.
        if matched.is_some() && candidate.content_ref.is_some() {
            return Err(RuntimeError::InvalidInput(
                "manifest direct-ingest candidates must not carry content_ref; use the attachment-aware entity constructor".into(),
            ));
        }
        Ok(EntityCandidatePrepared {
            context: self,
            candidate,
        })
    }

    fn check_value(&self, scope: RuntimeFieldScope, value: &str) -> RuntimeResult<()> {
        if self.snapshot.lookup(scope, value).is_some() {
            Ok(())
        } else {
            crate::secret_gate::check(value)
        }
    }
}

pub(crate) enum EntityAdmission {
    Legacy(Entity),
    Exempt(PreparedEntityExemption),
}

/// The ordinary patch route retains its historical patch-only scanner when
/// no manifest is active, while a matched candidate uses final-object checks.
pub(crate) struct OrdinaryEntityUpdateContext(EntityCandidateContext);

impl OrdinaryEntityUpdateContext {
    pub(crate) fn capture(token: &NamespaceToken) -> Self {
        Self(EntityCandidateContext::capture(
            token.namespace().as_str(),
            EntityEntryPoint::Update,
        ))
    }

    pub(crate) fn check_patch(
        &self,
        name: Option<&str>,
        description: Option<&str>,
        properties: Option<&Value>,
        tags: Option<&[String]>,
    ) -> RuntimeResult<()> {
        crate::secret_gate::reject_reserved_secret_gate_property(properties)?;
        if let Some(name) = name {
            crate::secret_gate::locate(self.0.check_name_description(name), "entity", "name")?;
        }
        if let Some(description) = description {
            crate::secret_gate::locate(
                self.0.check_name_description(description),
                "entity",
                "description",
            )?;
        }
        if let Some(properties) = properties {
            crate::secret_gate::locate(
                self.0.check_properties(Some(properties)),
                "entity",
                "properties",
            )?;
        }
        if let Some(tags) = tags {
            crate::secret_gate::locate(self.0.check_tags(tags), "entity", "tags")?;
        }
        Ok(())
    }

    pub(crate) fn admit(
        self,
        token: &NamespaceToken,
        candidate: Entity,
    ) -> RuntimeResult<EntityAdmission> {
        if self.0.snapshot.is_empty() {
            crate::secret_gate::reject_reserved_secret_gate_property(
                candidate.properties.as_ref(),
            )?;
            reject_injected_manifest_fault(&candidate.namespace)?;
            Ok(EntityAdmission::Legacy(candidate))
        } else {
            admit(self.0, token, candidate)
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedEntityExemption {
    pub(super) entity: Entity,
    pub(super) snapshot: Arc<ManifestSnapshot>,
    pub(super) entry: EntityEntryPoint,
    pub(super) attribution: EventAttribution,
    pub(super) digest_sha256: String,
    pub(super) field_scope: &'static str,
    pub(super) overridden_detector: String,
}

impl PreparedEntityExemption {
    /// The finalized, runtime-stamped candidate. Do not pass it back through
    /// caller-input reservation checks; the opaque plan owns its write binding.
    pub(crate) fn entity(&self) -> &Entity {
        &self.entity
    }
}

pub(crate) fn prepare_entity_admission(
    token: &NamespaceToken,
    entry: EntityEntryPoint,
    candidate: Entity,
) -> RuntimeResult<EntityAdmission> {
    prepare_entity_admission_at(token, entry, candidate, "entity")
}

pub(crate) fn prepare_entity_admission_at(
    token: &NamespaceToken,
    entry: EntityEntryPoint,
    candidate: Entity,
    record: &str,
) -> RuntimeResult<EntityAdmission> {
    let mut context = EntityCandidateContext::capture(&candidate.namespace, entry);
    context.record = record.into();
    admit(context, token, candidate)
}

fn admit(
    context: EntityCandidateContext,
    token: &NamespaceToken,
    mut candidate: Entity,
) -> RuntimeResult<EntityAdmission> {
    reject_injected_manifest_fault(&candidate.namespace)?;
    let matched = scan_candidate(&context, &candidate)?;
    let Some((scope, digest_sha256, overridden_detector)) = matched else {
        return Ok(EntityAdmission::Legacy(candidate));
    };
    let properties = candidate
        .properties
        .get_or_insert_with(|| serde_json::json!({}));
    let Some(properties) = properties.as_object_mut() else {
        return Err(RuntimeError::InvalidInput(
            "manifest-exempt entity properties must be an object".into(),
        ));
    };
    properties.insert(
        crate::secret_gate::RESERVED_SECRET_GATE_KEY.into(),
        EXEMPTION_STAMP.into(),
    );
    let record_token = token.with_namespace(
        crate::Namespace::parse(&candidate.namespace)
            .map_err(|_| RuntimeError::InvalidInput("invalid entity namespace".into()))?,
    );
    Ok(EntityAdmission::Exempt(PreparedEntityExemption {
        entity: candidate,
        snapshot: context.snapshot,
        entry: context.entry,
        attribution: EventAttribution::from_token(&record_token),
        digest_sha256,
        field_scope: match scope {
            RuntimeFieldScope::NameDescription => "name-description",
            RuntimeFieldScope::JsonProperties => "json-properties",
            RuntimeFieldScope::Tags => "tags",
            RuntimeFieldScope::RecordContent => "record-content",
            RuntimeFieldScope::CodeSource => "code-source",
        },
        overridden_detector,
    }))
}

fn reject_injected_manifest_fault(namespace: &str) -> RuntimeResult<()> {
    if let Some(fault) = super::faults::consume_manifest_invalid(namespace) {
        observe(namespace, FinalizerOutcome::ManifestInvalid(fault));
        return Err(RuntimeError::InvalidInput(
            "secret-gate manifest invalid".into(),
        ));
    }
    Ok(())
}

type CandidateMatch = (RuntimeFieldScope, String, String);

fn scan_candidate(
    context: &EntityCandidateContext,
    entity: &Entity,
) -> RuntimeResult<Option<CandidateMatch>> {
    crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
    if context.snapshot.is_empty() {
        crate::secret_gate::check_at(&entity.name, &context.record, "name")?;
        if let Some(description) = &entity.description {
            crate::secret_gate::check_at(description, &context.record, "description")?;
        }
        if let Some(properties) = &entity.properties {
            crate::secret_gate::check_json_at(properties, &context.record, "properties")?;
        }
        crate::secret_gate::check_tags_at(&entity.tags, &context.record, "tags")?;
        return Ok(None);
    }
    let mut values = vec![(
        RuntimeFieldScope::NameDescription,
        entity.name.as_str(),
        "name",
    )];
    if let Some(description) = &entity.description {
        values.push((
            RuntimeFieldScope::NameDescription,
            description,
            "description",
        ));
    }
    if let Some(properties) = &entity.properties {
        let mut leaves = Vec::new();
        property_strings(properties, &mut leaves);
        values.extend(
            leaves
                .into_iter()
                .map(|value| (RuntimeFieldScope::JsonProperties, value, "properties")),
        );
    }
    values.extend(
        entity
            .tags
            .iter()
            .map(|value| (RuntimeFieldScope::Tags, value.as_str(), "tags")),
    );
    let mut found: Option<CandidateMatch> = None;
    for (scope, value, field) in values {
        if let Some(meta) = context.snapshot.lookup(scope, value) {
            let digest = digest_to_hex(&scoped_digest(scope, value));
            if found.as_ref().is_some_and(|(old_scope, old_digest, _)| {
                *old_scope != scope || *old_digest != digest
            }) {
                observe(
                    &entity.namespace,
                    FinalizerOutcome::ManifestInvalid(ManifestFault::MultipleMatches),
                );
                return Err(RuntimeError::InvalidInput(
                    "secret-gate manifest has multiple candidate matches".into(),
                ));
            }
            found = Some((scope, digest, meta.overridden_detector.clone()));
        } else {
            crate::secret_gate::check_at(value, &context.record, field)?;
        }
    }
    Ok(found)
}

fn property_strings<'a>(value: &'a Value, values: &mut Vec<&'a str>) {
    match value {
        Value::String(value) => values.push(value),
        Value::Array(array) => array
            .iter()
            .for_each(|value| property_strings(value, values)),
        Value::Object(object) => {
            for (key, value) in object {
                values.push(key);
                property_strings(value, values);
            }
        }
        _ => {}
    }
}

pub(super) fn observe(namespace: &str, outcome: FinalizerOutcome) {
    #[cfg(test)]
    fixture::record_outcome(namespace, outcome);
    #[cfg(not(test))]
    let _ = (namespace, outcome);
}
