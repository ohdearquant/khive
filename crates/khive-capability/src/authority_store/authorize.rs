use super::current::Current;
use super::model::{AdministrationProof, BodyDigest, CoverageGroup, ProofSet};
use super::{AuthorityError, AuthorityRefusal, AuthorityResult, RecordRef};
use crate::durable::{canonical_principal, AdminTarget, ObservationPermission, RightSpec};

type Selected = (u16, u16, u16);

fn permission_scope(right: &RightSpec) -> AuthorityResult<&[ObservationPermission]> {
    match right {
        RightSpec::RecordObservation(scope) => Ok(&scope.permissions),
        RightSpec::GrantAdmin(admin) => match &admin.target {
            AdminTarget::RecordObservation(scope) => Ok(&scope.permissions),
            _ => Err(crate::durable::EnvelopeError::RightNotSupported(
                crate::durable::UnsupportedRight::OrdinaryNamespace,
            )
            .into()),
        },
        _ => Err(crate::durable::EnvelopeError::RightNotSupported(
            crate::durable::UnsupportedRight::OrdinaryNamespace,
        )
        .into()),
    }
}
fn covers(
    current: &Current<'_, '_>,
    held: &[RightSpec],
    namespace: &str,
    wanted: &ObservationPermission,
) -> Option<(u16, u16)> {
    for (right_index, right) in held.iter().enumerate() {
        if let RightSpec::GrantAdmin(admin) = right {
            if admin.namespace != namespace {
                continue;
            }
            if let AdminTarget::RecordObservation(scope) = &admin.target {
                for (permission_index, permission) in scope.permissions.iter().enumerate() {
                    if current
                        .configuration
                        .catalog
                        .covers_observation_permission(permission, wanted)
                    {
                        return Some((right_index as u16, permission_index as u16));
                    }
                }
            }
        }
    }
    None
}

/// Complete one-proof-per-permission coverage. Candidate discovery never grants
/// authority: selected records must pass the full same-writer effective walk.
pub(super) async fn administration(
    current: &mut Current<'_, '_>,
    namespace: &str,
    rights: &[RightSpec],
) -> AuthorityResult<ProofSet> {
    let permissions = rights
        .iter()
        .map(permission_scope)
        .collect::<AuthorityResult<Vec<_>>>()?;
    let mut selected = permissions
        .iter()
        .map(|scope| vec![None::<Selected>; scope.len()])
        .collect::<Vec<_>>();
    let mut references = Vec::<AdministrationProof>::new();
    let mut complete = false;
    let configuration = current.configuration;
    let mut sources = configuration.bootstrap_sources.iter().collect::<Vec<_>>();
    sources.sort_by(|a, b| a.slot.cmp(&b.slot));
    for source in sources {
        if source.owner != current.caller.principal
            || source.namespace != namespace
            || !source.validity.contains(current.now)
        {
            continue;
        }
        current.live_anchor().await?;
        let reference_index = references.len() as u16;
        let mut used = false;
        for (right, scope) in permissions.iter().enumerate() {
            for (permission, wanted) in scope.iter().enumerate() {
                if selected[right][permission].is_none() {
                    if let Some((admin_right, admin_permission)) =
                        covers(current, &source.rights, namespace, wanted)
                    {
                        selected[right][permission] =
                            Some((reference_index, admin_right, admin_permission));
                        used = true;
                    }
                }
            }
        }
        if used {
            references.push(AdministrationProof::Bootstrap {
                anchor: RecordRef::of(&current.configuration.anchor),
                slot: source.slot.clone(),
                manifest_digest: BodyDigest(source.manifest_digest.0),
            });
        }
        complete = selected.iter().flatten().all(Option::is_some);
        if complete {
            break;
        }
    }
    if !complete {
        let principal = canonical_principal(&current.caller.principal)?;
        let mut after = None;
        loop {
            let page = current
                .tx
                .records_for_principal(current.configuration.epoch.epoch_id, &principal, after, 1)
                .await?;
            for candidate in page.items {
                if !candidate.right_types.iter().any(|tag| tag == "grant_admin") {
                    continue;
                }
                let (record, envelope) = match current.effective(candidate.id).await {
                    Ok(value) => value,
                    Err(AuthorityError::Refused(AuthorityRefusal::NotEffective)) => continue,
                    Err(error) => return Err(error),
                };
                if envelope.body().grant.grantee != current.caller.principal {
                    return Err(AuthorityRefusal::Reject("candidate holder mismatch").into());
                }
                if envelope.body().grant.namespace != namespace {
                    continue;
                }
                let reference_index = u16::try_from(references.len()).map_err(|_| {
                    AuthorityRefusal::Reject("proof count contradicts bounded record")
                })?;
                let mut used = false;
                for (right, scope) in permissions.iter().enumerate() {
                    for (permission, wanted) in scope.iter().enumerate() {
                        if selected[right][permission].is_none() {
                            if let Some((admin_right, admin_permission)) =
                                covers(current, &envelope.body().grant.rights, namespace, wanted)
                            {
                                selected[right][permission] =
                                    Some((reference_index, admin_right, admin_permission));
                                used = true;
                            }
                        }
                    }
                }
                if used {
                    references.push(AdministrationProof::DurableGrant {
                        record: RecordRef::of(&record),
                    });
                }
                complete = selected.iter().flatten().all(Option::is_some);
                if complete {
                    break;
                }
            }
            if complete {
                break;
            }
            match page.next_id {
                Some(id) => after = Some(id),
                None => break,
            }
        }
    }
    if !complete {
        return Err(
            AuthorityRefusal::Denied("complete stored scope requires live administration").into(),
        );
    }
    let coverage = selected
        .into_iter()
        .enumerate()
        .map(|(right, scope)| {
            let role = u16::from(matches!(&rights[right], RightSpec::GrantAdmin(_)));
            let permissions = scope
                .into_iter()
                .enumerate()
                .map(|(permission, selected)| {
                    let (reference, admin_right, admin_permission) =
                        selected.expect("complete selection checked");
                    [
                        permission as u16,
                        reference,
                        admin_right,
                        admin_permission,
                        role,
                    ]
                })
                .collect();
            CoverageGroup {
                right_ordinal: right as u8,
                permissions,
            }
        })
        .collect();
    let references = references
        .chunks(256)
        .map(<[AdministrationProof]>::to_vec)
        .collect();
    Ok(ProofSet {
        references,
        coverage,
    })
}
