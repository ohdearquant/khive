use super::current::Current;
use super::model::{sql_time, validate_terminal};
use super::mutation::{internal, PlannedChange};
use super::*;
use crate::durable::{EnvelopeVerifier, Principal, RightSpec};
use khive_db::authority::{self, RecordKind};
use khive_storage::{event::Event, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind, Timestamp};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn change_event(
    configuration: &AuthorityConfiguration,
    change: &PlannedChange,
    caller: &AdmittedCaller,
    operation: AdministrationOperation,
    rationale: &str,
    now: Timestamp,
) -> StorageResult<Event> {
    let verb = match operation {
        AdministrationOperation::Grant => "grant",
        AdministrationOperation::Revoke => "revoke",
        AdministrationOperation::RetireAnchor => "retire_anchor",
        AdministrationOperation::History => {
            return Err(internal("history cannot mutate authority"))
        }
    };
    let mut authorization = json!({
        "target": change.target,
        "decision": "allow",
        "implementation": caller.gate_implementation,
        "request_actor": caller.principal,
        "request_verb": operation.canonical_verb(),
        "checked_at": now.as_micros(),
    });
    let payload = if let Some(record) = &change.record {
        let envelope = EnvelopeVerifier::new(&configuration.keys, &configuration.catalog)
            .verify(
                &record.body,
                configuration.epoch.store_id,
                configuration.epoch.epoch_id,
            )
            .map_err(|error| {
                internal(format!(
                    "prepared grant failed immutable verification: {error}"
                ))
            })?;
        let grant = &envelope.body().grant;
        authorization["checked_rights"] = value(&grant.rights)?;
        authorization["administration"] = value(&change.proof)?;
        authorization["delegation_parent"] = match &envelope.body().parent {
            crate::durable::ParentBinding::Grant { grant_id, .. } => json!({"grant_id": grant_id}),
            crate::durable::ParentBinding::BootstrapSlot {
                slot,
                manifest_digest,
                ..
            } => json!({"bootstrap_slot":slot,"manifest_digest":BodyDigest(manifest_digest.0)}),
        };
        json!({"action_id":change.association.action_id,"grant_record_id":record.id,"grantee":grant.grantee,"rights":grant.rights,"rationale":rationale,"authorization":authorization})
    } else {
        let terminal = change
            .transition
            .as_ref()
            .ok_or_else(|| internal("prepared terminal row missing"))?;
        match operation {
            AdministrationOperation::RetireAnchor => {
                authorization["identity_rule"] = json!("structural_equality_with_boot_principal");
                authorization["boot_principal"] = value(&configuration.boot_principal)?;
            }
            AdministrationOperation::Revoke => {
                authorization["administration"] = value(&change.proof)?
            }
            _ => return Err(internal("grant requires complete new record")),
        }
        if operation == AdministrationOperation::RetireAnchor {
            json!({"action_id":change.association.action_id,"root_anchor_id":change.target.id,"root_anchor":change.target,"boot_principal":configuration.boot_principal,"liveness_transition":{"from":"live","to":"retired"},"transition_id":terminal.id,"terminal_state":RecordedState::Retired,"rationale":rationale,"authorization":authorization})
        } else {
            json!({"action_id":change.association.action_id,"grant_record":change.target,"transition_id":terminal.id,"terminal_state":RecordedState::from_database(terminal.state),"rationale":rationale,"authorization":authorization})
        }
    };
    Ok(Event {
        id: change.association.event_id,
        namespace: change.namespace.clone(),
        verb: verb.into(),
        substrate: SubstrateKind::Event,
        // Display only. Authoritative identity remains structured in the payload.
        actor: format!("{}:{}", caller.principal.kind, caller.principal.id),
        kind: EventKind::GrantChange,
        outcome: EventOutcome::Success,
        payload,
        payload_schema_version: 1,
        profile_state_version: None,
        duration_us: 0,
        target_id: Some(change.target.id),
        session_id: None,
        aggregate_kind: None,
        aggregate_id: None,
        created_at: sql_time(now).map_err(|error| internal(error.to_string()))?,
        op_index: None,
        ref_resolution: None,
    })
}
fn value<T: Serialize + ?Sized>(value: &T) -> StorageResult<Value> {
    serde_json::to_value(value)
        .map_err(|error| internal(format!("authority history serialization failed: {error}")))
}

#[derive(Clone, Debug)]
pub struct HistoricalAdministration {
    pub record: RecordRef,
    pub bootstrap_slot: Option<String>,
    pub rights: Vec<RightSpec>,
}
#[derive(Clone, Debug)]
pub struct HistoricalChange {
    pub event: Event,
    pub target: RecordRef,
    pub former_rights: Vec<RightSpec>,
    pub terminal_state: Option<RecordedState>,
    pub administration: Vec<HistoricalAdministration>,
}

impl AuthorityStore {
    /// Exact trusted-library history read. Public action paging and its scoped
    /// cursors belong to the separate wire/composition adapter.
    pub async fn read_change(
        &self,
        context: Arc<dyn AdmissionContext>,
        action_id: Uuid,
        event_id: Uuid,
    ) -> AuthorityResult<HistoricalChange> {
        let configuration = Arc::clone(&self.configuration);
        let mut canonical = b"KHAH\0\x01".to_vec();
        canonical.extend_from_slice(action_id.as_bytes());
        canonical.extend_from_slice(event_id.as_bytes());
        let outcome = authority::transaction(&*self.sql, move |mut tx| {
            Box::pin(async move {
                let read = async {
                    let mut current = Current::open(
                        &configuration,
                        &mut tx,
                        &*context,
                        AdministrationOperation::History,
                        &canonical,
                    )
                    .await?;
                    hydrate(&mut current, action_id, event_id).await
                }
                .await;
                match read {
                    Ok(value) => Ok(Ok(value)),
                    Err(AuthorityError::Refused(refusal)) => Ok(Err(refusal)),
                    Err(AuthorityError::Storage(error)) => Err(error),
                }
            })
        })
        .await?;
        outcome.map_err(AuthorityError::Refused)
    }
}

async fn hydrate(
    current: &mut Current<'_, '_>,
    action_id: Uuid,
    event_id: Uuid,
) -> AuthorityResult<HistoricalChange> {
    let (association, event) = current
        .tx
        .change_event(current.configuration.epoch.epoch_id, event_id)
        .await?
        .ok_or(AuthorityRefusal::Reject("history event missing"))?;
    if association.action_id != action_id || event.target_id != Some(association.record_id) {
        return Err(AuthorityRefusal::Reject("history action or target identity mismatch").into());
    }
    if !current
        .caller
        .readable_namespaces
        .contains(&event.namespace)
    {
        return Err(
            AuthorityRefusal::Denied("history namespace outside current read scope").into(),
        );
    }
    let (record, rights) = if association.record_id == current.configuration.anchor.id {
        (current.anchor().await?, Vec::new())
    } else {
        let (record, envelope) = current.authentic(association.record_id).await?;
        (record, envelope.body().grant.rights.clone())
    };
    let target = RecordRef::of(&record);
    let grant_operation = matches!(event.verb.as_str(), "grant" | "revoke");
    if (grant_operation
        && (record.kind != RecordKind::Grant || association.kind != authority::ChangeKind::Grant))
        || (event.verb == "retire_anchor"
            && (record.kind != RecordKind::Anchor
                || association.kind != authority::ChangeKind::Anchor))
    {
        return Err(AuthorityRefusal::Reject("history operation and record role mismatch").into());
    }
    if event.namespace != record.namespace || uuid_field(&event.payload, "action_id")? != action_id
    {
        return Err(AuthorityRefusal::Reject("history attribution mismatch").into());
    }
    let authorization = event
        .payload
        .get("authorization")
        .ok_or(AuthorityRefusal::Reject("history authorization missing"))?;
    if reference(
        authorization
            .get("target")
            .ok_or(AuthorityRefusal::Reject("snapshot target missing"))?,
    )? != target
        || authorization.get("decision").and_then(Value::as_str) != Some("allow")
        || authorization.get("checked_at").and_then(Value::as_u64)
            != u64::try_from(event.created_at).ok()
        || authorization
            .get("implementation")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(AuthorityRefusal::Reject("history snapshot binding mismatch").into());
    }
    let actor = principal(
        authorization
            .get("request_actor")
            .ok_or(AuthorityRefusal::Reject("history actor missing"))?,
    )?;
    if actor.leg.is_some() {
        return Err(AuthorityRefusal::Reject("historical administrator is subactor").into());
    }
    let rationale = event
        .payload
        .get("rationale")
        .and_then(Value::as_str)
        .ok_or(AuthorityRefusal::Reject("history rationale missing"))?;
    if rationale.trim().is_empty() || rationale.len() > 4096 {
        return Err(AuthorityRefusal::Reject("invalid history rationale").into());
    }
    let state = match event.verb.as_str() {
        "grant" => {
            if record.kind != RecordKind::Grant
                || uuid_field(&event.payload, "grant_record_id")? != record.id
                || event.payload.get("rights") != Some(&value(&rights)?)
                || authorization.get("checked_rights") != Some(&value(&rights)?)
            {
                return Err(AuthorityRefusal::Reject(
                    "grant history differs from preserved authority",
                )
                .into());
            }
            let (_, envelope) = current.authentic(record.id).await?;
            if event.payload.get("grantee") != Some(&value(&envelope.body().grant.grantee)?) {
                return Err(AuthorityRefusal::Reject("grant history grantee mismatch").into());
            }
            None
        }
        "revoke" | "retire_anchor" => {
            if reference(
                event
                    .payload
                    .get(if event.verb == "retire_anchor" {
                        "root_anchor"
                    } else {
                        "grant_record"
                    })
                    .ok_or(AuthorityRefusal::Reject("terminal target missing"))?,
            )? != target
            {
                return Err(
                    AuthorityRefusal::Reject("terminal event target digest mismatch").into(),
                );
            }
            let terminal = current
                .tx
                .transition(record.epoch_id, record.id)
                .await?
                .ok_or(AuthorityRefusal::Reject("terminal history row missing"))?;
            validate_terminal(&terminal, &record)?;
            let state = RecordedState::from_database(terminal.state);
            if terminal.event_id != event_id
                || terminal.action_id != action_id
                || terminal.id != uuid_field(&event.payload, "transition_id")?
                || event.payload.get("terminal_state") != Some(&value(&state)?)
                || event.payload.get("rights").is_some()
                || authorization.get("checked_rights").is_some()
            {
                return Err(AuthorityRefusal::Reject(
                    "terminal event and preserved transition mismatch",
                )
                .into());
            }
            if event.verb == "retire_anchor"
                && (state != RecordedState::Retired
                    || uuid_field(&event.payload, "root_anchor_id")?
                        != current.configuration.anchor.id
                    || event.payload.get("boot_principal")
                        != Some(&value(&current.configuration.boot_principal)?)
                    || event.payload.get("liveness_transition")
                        != Some(&json!({"from":"live","to":"retired"}))
                    || actor != current.configuration.boot_principal
                    || authorization.get("identity_rule").and_then(Value::as_str)
                        != Some("structural_equality_with_boot_principal")
                    || authorization.get("boot_principal")
                        != Some(&value(&current.configuration.boot_principal)?))
            {
                return Err(
                    AuthorityRefusal::Reject("retirement history identity rule mismatch").into(),
                );
            }
            Some(state)
        }
        _ => {
            return Err(AuthorityRefusal::Reject("unsupported authority history operation").into())
        }
    };
    let expected_verb = match event.verb.as_str() {
        "grant" => "authority.grant",
        "revoke" => "authority.revoke",
        "retire_anchor" => "authority.retire_anchor",
        _ => unreachable!(),
    };
    if authorization.get("request_verb").and_then(Value::as_str) != Some(expected_verb) {
        return Err(AuthorityRefusal::Reject("historical canonical operation mismatch").into());
    }
    let mut administration = Vec::new();
    if event.verb != "retire_anchor" {
        let proof = authorization
            .get("administration")
            .ok_or(AuthorityRefusal::Reject("administration snapshot missing"))?;
        let buckets =
            proof
                .get("references")
                .and_then(Value::as_array)
                .ok_or(AuthorityRefusal::Reject(
                    "administration references missing",
                ))?;
        if buckets.len() > 256 {
            return Err(AuthorityRefusal::Reject("invalid reference bucket count").into());
        }
        for bucket in buckets {
            let refs = bucket
                .as_array()
                .ok_or(AuthorityRefusal::Reject("invalid reference bucket"))?;
            if refs.is_empty() || refs.len() > 256 {
                return Err(AuthorityRefusal::Reject("invalid reference bucket size").into());
            }
            for descriptor in refs {
                match descriptor.get("type").and_then(Value::as_str) {
                    Some("durable_grant") => {
                        let expected = reference(
                            descriptor
                                .get("record")
                                .ok_or(AuthorityRefusal::Reject("proof record missing"))?,
                        )?;
                        let (proof_record, envelope) = current.authentic(expected.id).await?;
                        if RecordRef::of(&proof_record) != expected
                            || envelope.body().grant.grantee != actor
                        {
                            return Err(AuthorityRefusal::Reject(
                                "historical administration digest or holder mismatch",
                            )
                            .into());
                        }
                        administration.push(HistoricalAdministration {
                            record: expected,
                            bootstrap_slot: None,
                            rights: envelope.body().grant.rights.clone(),
                        });
                    }
                    Some("bootstrap") => {
                        let anchor =
                            reference(descriptor.get("anchor").ok_or(
                                AuthorityRefusal::Reject("bootstrap proof anchor missing"),
                            )?)?;
                        let slot = descriptor
                            .get("slot")
                            .and_then(Value::as_str)
                            .ok_or(AuthorityRefusal::Reject("bootstrap proof slot missing"))?;
                        let source = current.source(slot)?;
                        if anchor != RecordRef::of(&current.configuration.anchor)
                            || source.owner != actor
                            || descriptor.get("manifest_digest")
                                != Some(&value(&BodyDigest(source.manifest_digest.0))?)
                        {
                            return Err(AuthorityRefusal::Reject(
                                "historical bootstrap administration mismatch",
                            )
                            .into());
                        }
                        administration.push(HistoricalAdministration {
                            record: anchor,
                            bootstrap_slot: Some(slot.into()),
                            rights: source.rights,
                        });
                    }
                    _ => {
                        return Err(
                            AuthorityRefusal::Reject("unknown administration proof type").into(),
                        )
                    }
                }
            }
        }
        validate_coverage(current, proof, &rights, &administration, &record.namespace)?;
    }
    Ok(HistoricalChange {
        event,
        target,
        former_rights: rights,
        terminal_state: state,
        administration,
    })
}

fn validate_coverage(
    current: &Current<'_, '_>,
    proof: &Value,
    rights: &[RightSpec],
    administration: &[HistoricalAdministration],
    namespace: &str,
) -> AuthorityResult<()> {
    use crate::durable::AdminTarget;
    let groups = proof
        .get("coverage")
        .and_then(Value::as_array)
        .ok_or(AuthorityRefusal::Reject("coverage missing"))?;
    if groups.len() != rights.len() {
        return Err(AuthorityRefusal::Reject("coverage right count mismatch").into());
    }
    for (right_index, (group, right)) in groups.iter().zip(rights).enumerate() {
        let scope = match right {
            RightSpec::RecordObservation(scope) => scope,
            RightSpec::GrantAdmin(admin) => match &admin.target {
                AdminTarget::RecordObservation(scope) => scope,
                _ => {
                    return Err(AuthorityRefusal::Reject(
                        "unsupported historical administrative target",
                    )
                    .into())
                }
            },
            _ => return Err(AuthorityRefusal::Reject("unsupported historical right").into()),
        };
        let witnesses = group
            .get("permissions")
            .and_then(Value::as_array)
            .ok_or(AuthorityRefusal::Reject("permission coverage missing"))?;
        if group.get("right_ordinal").and_then(Value::as_u64) != Some(right_index as u64)
            || witnesses.len() != scope.permissions.len()
        {
            return Err(AuthorityRefusal::Reject("coverage ordinals mismatch").into());
        }
        for (permission_index, (witness, wanted)) in
            witnesses.iter().zip(&scope.permissions).enumerate()
        {
            let values = witness
                .as_array()
                .filter(|values| values.len() == 5)
                .ok_or(AuthorityRefusal::Reject("invalid permission witness"))?;
            let indices = values
                .iter()
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or(AuthorityRefusal::Reject("invalid witness index"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if indices[0] != permission_index
                || indices[4] != usize::from(matches!(right, RightSpec::GrantAdmin(_)))
            {
                return Err(AuthorityRefusal::Reject("permission witness role mismatch").into());
            }
            let source = administration
                .get(indices[1])
                .ok_or(AuthorityRefusal::Reject("proof reference index missing"))?;
            let admin = match source.rights.get(indices[2]) {
                Some(RightSpec::GrantAdmin(admin)) if admin.namespace == namespace => admin,
                _ => {
                    return Err(
                        AuthorityRefusal::Reject("selected proof is not administration").into(),
                    )
                }
            };
            let held = match &admin.target {
                AdminTarget::RecordObservation(scope) => scope.permissions.get(indices[3]),
                _ => None,
            }
            .ok_or(AuthorityRefusal::Reject(
                "selected administrative permission missing",
            ))?;
            if !current
                .configuration
                .catalog
                .covers_observation_permission(held, wanted)
            {
                return Err(AuthorityRefusal::Reject(
                    "historical proof does not cover complete permission",
                )
                .into());
            }
        }
    }
    Ok(())
}
fn uuid_field(value: &Value, name: &str) -> AuthorityResult<Uuid> {
    let text = value
        .get(name)
        .and_then(Value::as_str)
        .ok_or(AuthorityRefusal::Reject("history UUID field missing"))?;
    let id = Uuid::parse_str(text).map_err(|_| AuthorityRefusal::Reject("invalid history UUID"))?;
    if id.to_string() != text {
        return Err(AuthorityRefusal::Reject("noncanonical history UUID").into());
    }
    Ok(id)
}
fn reference(value: &Value) -> AuthorityResult<RecordRef> {
    let id = uuid_field(value, "id")?;
    let digest = value
        .get("body_digest")
        .ok_or(AuthorityRefusal::Reject("history body digest missing"))?;
    if digest.get("algorithm").and_then(Value::as_str) != Some("blake3-256") {
        return Err(AuthorityRefusal::Reject("unknown history digest algorithm").into());
    }
    let hex = digest
        .get("hex")
        .and_then(Value::as_str)
        .filter(|hex| hex.len() == 64)
        .ok_or(AuthorityRefusal::Reject("invalid history digest length"))?;
    let mut bytes = [0; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        fn nibble(value: u8) -> AuthorityResult<u8> {
            match value {
                b'0'..=b'9' => Ok(value - b'0'),
                b'a'..=b'f' => Ok(value - b'a' + 10),
                _ => Err(AuthorityRefusal::Reject("noncanonical history digest").into()),
            }
        }
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(RecordRef {
        id,
        body_digest: BodyDigest(bytes),
    })
}
fn principal(value: &Value) -> AuthorityResult<Principal> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(AuthorityRefusal::Reject("historical actor kind missing"))?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or(AuthorityRefusal::Reject("historical actor id missing"))?;
    let leg = match value.get("leg") {
        None | Some(Value::Null) => None,
        Some(Value::String(leg)) => Some(leg.clone()),
        _ => return Err(AuthorityRefusal::Reject("invalid historical leg").into()),
    };
    let principal = Principal {
        kind: kind.into(),
        id: id.into(),
        leg,
    };
    principal.validate()?;
    Ok(principal)
}
