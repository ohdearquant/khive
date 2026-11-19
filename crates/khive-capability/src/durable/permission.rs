use super::{
    bound, collection, nonempty, sorted_unique, EnvelopeError, EnvelopeResult, UnsupportedRight,
    MAX_RECORD_BYTES,
};
use khive_types::Timestamp;
use lion_core::{Right, Rights};
use std::collections::BTreeMap;
use uuid::Uuid;

#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Principal {
    pub kind: String,
    pub id: String,
    pub leg: Option<String>,
}
impl Principal {
    pub fn validate(&self) -> EnvelopeResult<()> {
        nonempty(&self.kind)?;
        nonempty(&self.id)?;
        if let Some(leg) = &self.leg {
            let bytes = leg.as_bytes();
            if bytes.is_empty()
                || bytes.len() > 64
                || !bytes[0].is_ascii_alphanumeric()
                || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
                || bytes
                    .iter()
                    .any(|b| !b.is_ascii_lowercase() && !b.is_ascii_digit() && *b != b'-')
            {
                return Err(EnvelopeError::InvalidInput("invalid structural leg label"));
            }
        }
        Ok(())
    }
    pub fn parent(&self) -> Self {
        Self {
            kind: self.kind.clone(),
            id: self.id.clone(),
            leg: None,
        }
    }
}

#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractDigest(pub [u8; 32]);
impl ContractDigest {
    pub fn blake3(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractRef {
    pub name: String,
    pub version: u32,
    pub digest: ContractDigest,
}
impl ContractRef {
    fn validate(&self) -> EnvelopeResult<()> {
        nonempty(&self.name)
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExactBytes(pub Vec<u8>);
impl ExactBytes {
    fn validate(&self) -> EnvelopeResult<()> {
        bound("reference bytes", self.0.len(), MAX_RECORD_BYTES)?;
        if self.0.is_empty() {
            return Err(EnvelopeError::InvalidInput("empty exact reference"));
        }
        Ok(())
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepositoryIdentity {
    pub forge: String,
    pub immutable_id: ExactBytes,
}
impl RepositoryIdentity {
    fn validate(&self) -> EnvelopeResult<()> {
        nonempty(&self.forge)?;
        self.immutable_id.validate()
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Validity {
    pub not_before: Timestamp,
    pub expires_at: Option<Timestamp>,
}
impl Validity {
    pub fn validate(&self) -> EnvelopeResult<()> {
        if self.expires_at.is_some_and(|end| end <= self.not_before) {
            return Err(EnvelopeError::InvalidInput("expiry must follow not_before"));
        }
        Ok(())
    }
    pub fn contains(&self, now: Timestamp) -> bool {
        now >= self.not_before && self.expires_at.is_none_or(|end| now < end)
    }
    pub fn covers(&self, child: &Self) -> bool {
        self.not_before <= child.not_before
            && match (self.expires_at, child.expires_at) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(a), Some(b)) => b <= a,
            }
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunRef(pub Uuid);
impl RunRef {
    pub fn parse(value: &str) -> EnvelopeResult<Self> {
        let id =
            Uuid::parse_str(value).map_err(|_| EnvelopeError::InvalidInput("invalid run UUID"))?;
        if id.to_string() != value {
            return Err(EnvelopeError::InvalidInput("noncanonical run UUID"));
        }
        Ok(Self(id))
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhaseVisit {
    pub phase_id: String,
    pub round: i64,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActionOccurrence {
    pub visit: PhaseVisit,
    pub action_name: String,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArgumentValue {
    String(String),
    Integer(i64),
    Duration(i64),
    Bool(bool),
    Word(String),
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArgumentType {
    String,
    Integer,
    Duration,
    Bool,
    Word,
}
impl ArgumentValue {
    fn kind(&self) -> ArgumentType {
        match self {
            Self::String(_) => ArgumentType::String,
            Self::Integer(_) => ArgumentType::Integer,
            Self::Duration(_) => ArgumentType::Duration,
            Self::Bool(_) => ArgumentType::Bool,
            Self::Word(_) => ArgumentType::Word,
        }
    }
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArgumentBinding {
    pub name: String,
    pub value: ArgumentValue,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExactArguments(pub Vec<ArgumentBinding>);
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArgumentSpec {
    pub name: String,
    pub kind: ArgumentType,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProducerClass {
    Actor,
    Instrument,
    Platform,
    System,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubmissionChannel {
    Observe,
    Sample,
    Internal,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubjectSelector {
    Exact(Vec<ExactBytes>),
    Repositories(Vec<RepositoryIdentity>),
    AllSubjectsOfKind,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubjectScope {
    pub kind: ContractRef,
    pub selector: SubjectSelector,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceBinding {
    pub contract: ContractRef,
    pub identities: Vec<ExactBytes>,
    pub arguments: Vec<ExactArguments>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportingBinding {
    Role(String),
    Initiator(Principal),
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubmissionBinding {
    ActorReport {
        run: RunRef,
        definition: ContractDigest,
        role: String,
    },
    ActionResult {
        run: RunRef,
        definition: ContractDigest,
        occurrence: ActionOccurrence,
        reporter: ReportingBinding,
        arguments: ExactArguments,
    },
    Instrument(SourceBinding),
    PlatformRelay {
        source: SourceBinding,
        response_contract: ContractRef,
    },
    ClaimTimePlatformBundle,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationPermission {
    pub policy_domain: String,
    pub entry: ContractRef,
    pub producer_class: ProducerClass,
    pub channel: SubmissionChannel,
    pub subject: SubjectScope,
    pub binding: SubmissionBinding,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationScope {
    pub permissions: Vec<ObservationPermission>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdminTarget {
    Read,
    Create,
    Write,
    Delete,
    Send,
    RecordObservation(ObservationScope),
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdminScope {
    pub namespace: String,
    pub target: AdminTarget,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RightSpec {
    Read,
    Create,
    Write,
    Delete,
    Send,
    RecordObservation(ObservationScope),
    GrantAdmin(AdminScope),
}

#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceFormat {
    Opaque,
    CanonicalUuid,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct SubjectContract {
    pub identity: ContractRef,
    pub reference_format: ReferenceFormat,
    pub repository_independent: bool,
    pub repositories: Vec<RepositoryIdentity>,
    pub membership: Vec<(ExactBytes, RepositoryIdentity)>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct SourceContract {
    pub identity: ContractRef,
    pub reference_format: ReferenceFormat,
    pub identities: Vec<ExactBytes>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct EntryContract {
    pub identity: ContractRef,
    pub class: ProducerClass,
    pub subject_kind: ContractRef,
    pub source_contract: Option<ContractRef>,
    pub response_contract: Option<ContractRef>,
    pub authority_arguments: Vec<ArgumentSpec>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct DeclaredAction {
    pub entry: ContractRef,
    pub phase_id: String,
    pub action_name: String,
    pub by: Option<String>,
    pub phase_owner: Option<String>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct DefinitionContract {
    pub digest: ContractDigest,
    pub reporting_roles: Vec<(ContractRef, String)>,
    pub actions: Vec<DeclaredAction>,
}
#[cfg_attr(feature = "authority-store", derive(serde::Serialize))]
#[derive(Clone, Debug)]
pub struct RunContract {
    pub policy_domain: String,
    pub id: RunRef,
    pub definition: ContractDigest,
    pub bindings: Vec<(String, Principal)>,
    pub initiating_principal: Principal,
    pub subject_kind: ContractRef,
    pub subject: ExactBytes,
}

/// Provisioned contract snapshots, never parsed from a grant request.
/// Run facts must come from trusted immutable run bindings; a later authority
/// service still discharges current run/subject obligations in its transaction.
#[derive(Clone, Debug)]
pub struct ClosedCatalog {
    subjects: BTreeMap<ContractRef, SubjectContract>,
    sources: BTreeMap<ContractRef, SourceContract>,
    entries: BTreeMap<ContractRef, EntryContract>,
    definitions: BTreeMap<ContractDigest, DefinitionContract>,
    runs: BTreeMap<RunRef, RunContract>,
}
impl ClosedCatalog {
    pub fn provision(
        subjects: Vec<SubjectContract>,
        sources: Vec<SourceContract>,
        entries: Vec<EntryContract>,
        definitions: Vec<DefinitionContract>,
        runs: Vec<RunContract>,
    ) -> EnvelopeResult<Self> {
        collection(&subjects, false)?;
        collection(&sources, false)?;
        collection(&entries, true)?;
        collection(&definitions, false)?;
        collection(&runs, false)?;
        let mut catalog = Self {
            subjects: BTreeMap::new(),
            sources: BTreeMap::new(),
            entries: BTreeMap::new(),
            definitions: BTreeMap::new(),
            runs: BTreeMap::new(),
        };
        for subject in subjects {
            subject.identity.validate()?;
            sorted_unique(&subject.repositories, false)?;
            collection(&subject.membership, false)?;
            if subject.repository_independent
                && (!subject.repositories.is_empty() || !subject.membership.is_empty())
            {
                return Err(EnvelopeError::InvalidInput(
                    "repository-independent contract has repository bindings",
                ));
            }
            for repository in &subject.repositories {
                repository.validate()?;
            }
            let mut members = BTreeMap::new();
            for (reference, repository) in &subject.membership {
                validate_reference(reference, subject.reference_format)?;
                if !subject.repositories.contains(repository)
                    || members.insert(reference, repository).is_some()
                {
                    return Err(EnvelopeError::InvalidInput(
                        "ambiguous or unknown repository membership",
                    ));
                }
            }
            if catalog
                .subjects
                .insert(subject.identity.clone(), subject)
                .is_some()
            {
                return Err(EnvelopeError::InvalidInput("duplicate subject contract"));
            }
        }
        for source in sources {
            source.identity.validate()?;
            sorted_unique(&source.identities, true)?;
            for reference in &source.identities {
                validate_reference(reference, source.reference_format)?;
            }
            if catalog
                .sources
                .insert(source.identity.clone(), source)
                .is_some()
            {
                return Err(EnvelopeError::InvalidInput("duplicate source contract"));
            }
        }
        for entry in entries {
            entry.identity.validate()?;
            sorted_unique(&entry.authority_arguments, false)?;
            if entry
                .authority_arguments
                .windows(2)
                .any(|p| p[0].name == p[1].name)
            {
                return Err(EnvelopeError::InvalidInput("duplicate argument name"));
            }
            for arg in &entry.authority_arguments {
                nonempty(&arg.name)?;
            }
            if !catalog.subjects.contains_key(&entry.subject_kind) {
                return Err(EnvelopeError::InvalidInput(
                    "unknown entry subject contract",
                ));
            }
            if let Some(source) = &entry.source_contract {
                if !catalog.sources.contains_key(source) {
                    return Err(EnvelopeError::InvalidInput("unknown entry source contract"));
                }
            }
            if entry.class == ProducerClass::Platform {
                entry
                    .response_contract
                    .as_ref()
                    .ok_or(EnvelopeError::InvalidInput(
                        "platform entry lacks response contract",
                    ))?
                    .validate()?;
            } else if entry.response_contract.is_some() {
                return Err(EnvelopeError::InvalidInput(
                    "non-platform response contract",
                ));
            }
            if catalog
                .entries
                .insert(entry.identity.clone(), entry)
                .is_some()
            {
                return Err(EnvelopeError::InvalidInput("duplicate entry contract"));
            }
        }
        for definition in definitions {
            collection(&definition.reporting_roles, false)?;
            collection(&definition.actions, false)?;
            for (entry, role) in &definition.reporting_roles {
                if !catalog.entries.contains_key(entry) {
                    return Err(EnvelopeError::InvalidInput(
                        "unknown reporting entry contract",
                    ));
                }
                nonempty(role)?;
            }
            for action in &definition.actions {
                if !catalog.entries.contains_key(&action.entry) {
                    return Err(EnvelopeError::InvalidInput("unknown action entry"));
                }
                nonempty(&action.phase_id)?;
                nonempty(&action.action_name)?;
                if let Some(role) = &action.by {
                    nonempty(role)?;
                }
                if let Some(role) = &action.phase_owner {
                    nonempty(role)?;
                }
            }
            if catalog
                .definitions
                .insert(definition.digest, definition)
                .is_some()
            {
                return Err(EnvelopeError::InvalidInput("duplicate definition contract"));
            }
        }
        for run in runs {
            nonempty(&run.policy_domain)?;
            collection(&run.bindings, false)?;
            run.initiating_principal.validate()?;
            if !catalog.definitions.contains_key(&run.definition) {
                return Err(EnvelopeError::InvalidInput("unknown run definition"));
            }
            let subject = catalog
                .subjects
                .get(&run.subject_kind)
                .ok_or(EnvelopeError::InvalidInput("unknown run subject kind"))?;
            validate_reference(&run.subject, subject.reference_format)?;
            let mut roles = BTreeMap::new();
            for (role, principal) in &run.bindings {
                nonempty(role)?;
                principal.validate()?;
                if roles.insert(role, principal).is_some() {
                    return Err(EnvelopeError::InvalidInput("duplicate run role"));
                }
            }
            if catalog.runs.insert(run.id, run).is_some() {
                return Err(EnvelopeError::InvalidInput("duplicate run contract"));
            }
        }
        Ok(catalog)
    }

    pub fn validate_rights(
        &self,
        rights: &[RightSpec],
        namespace: &str,
        grantee: &Principal,
    ) -> EnvelopeResult<Rights> {
        self.validate_rights_inner(rights, namespace, grantee, true)
    }
    // A finite bootstrap source is a bound on future grants, not registration of
    // its owner as the producer. Emitted grants still use validate_rights above.
    pub(crate) fn validate_source_rights(
        &self,
        rights: &[RightSpec],
        namespace: &str,
        owner: &Principal,
    ) -> EnvelopeResult<Rights> {
        self.validate_rights_inner(rights, namespace, owner, false)
    }
    fn validate_rights_inner(
        &self,
        rights: &[RightSpec],
        namespace: &str,
        grantee: &Principal,
        bind_producer: bool,
    ) -> EnvelopeResult<Rights> {
        nonempty(namespace)?;
        grantee.validate()?;
        sorted_unique(rights, true)?;
        let mut carriers = Rights::empty();
        for right in rights {
            let scope = match right {
                RightSpec::RecordObservation(scope) => {
                    carriers = carriers.with(Right::Execute);
                    scope
                }
                RightSpec::GrantAdmin(admin) => {
                    if grantee.leg.is_some() {
                        return Err(EnvelopeError::InvalidInput(
                            "subactor cannot hold administration",
                        ));
                    }
                    if admin.namespace != namespace {
                        return Err(EnvelopeError::InvalidInput(
                            "admin namespace differs from grant",
                        ));
                    }
                    let AdminTarget::RecordObservation(scope) = &admin.target else {
                        return Err(EnvelopeError::RightNotSupported(
                            UnsupportedRight::OrdinaryNamespace,
                        ));
                    };
                    carriers = carriers.with(Right::Delegate);
                    scope
                }
                _ => {
                    return Err(EnvelopeError::RightNotSupported(
                        UnsupportedRight::OrdinaryNamespace,
                    ))
                }
            };
            collection(&scope.permissions, true)?;
            for (index, permission) in scope.permissions.iter().enumerate() {
                if scope.permissions[..index].contains(permission) {
                    return Err(EnvelopeError::InvalidInput(
                        "duplicate observation permission",
                    ));
                }
                self.validate_permission(
                    permission,
                    if bind_producer && matches!(right, RightSpec::RecordObservation(_)) {
                        Some(grantee)
                    } else {
                        None
                    },
                )?;
            }
        }
        Ok(carriers)
    }

    #[cfg(feature = "authority-store")]
    pub(crate) fn covers_observation_permission(
        &self,
        held: &ObservationPermission,
        wanted: &ObservationPermission,
    ) -> bool {
        held.policy_domain == wanted.policy_domain
            && held.entry == wanted.entry
            && held.producer_class == wanted.producer_class
            && held.channel == wanted.channel
            && self.covers_subject(&held.subject, &wanted.subject)
            && binding_covers(&held.binding, &wanted.binding)
    }

    pub fn covers_rights(&self, parent: &[RightSpec], requested: &[RightSpec]) -> bool {
        requested.iter().all(|child| {
            parent.iter().any(|held| match (held, child) {
                (RightSpec::RecordObservation(a), RightSpec::RecordObservation(b)) => {
                    self.covers_scope(a, b)
                }
                (RightSpec::GrantAdmin(a), RightSpec::GrantAdmin(b))
                    if a.namespace == b.namespace =>
                {
                    match (&a.target, &b.target) {
                        (AdminTarget::RecordObservation(a), AdminTarget::RecordObservation(b)) => {
                            self.covers_scope(a, b)
                        }
                        _ => false,
                    }
                }
                _ => false,
            })
        })
    }
    fn covers_scope(&self, parent: &ObservationScope, child: &ObservationScope) -> bool {
        child.permissions.iter().all(|wanted| {
            parent.permissions.iter().any(|held| {
                held.policy_domain == wanted.policy_domain
                    && held.entry == wanted.entry
                    && held.producer_class == wanted.producer_class
                    && held.channel == wanted.channel
                    && self.covers_subject(&held.subject, &wanted.subject)
                    && binding_covers(&held.binding, &wanted.binding)
            })
        })
    }
    fn validate_permission(
        &self,
        p: &ObservationPermission,
        grantee: Option<&Principal>,
    ) -> EnvelopeResult<()> {
        nonempty(&p.policy_domain)?;
        if matches!(p.binding, SubmissionBinding::ClaimTimePlatformBundle) {
            return Err(EnvelopeError::RightNotSupported(
                UnsupportedRight::ClaimTimePlatformBundleInM1,
            ));
        }
        if p.channel != SubmissionChannel::Observe || p.producer_class == ProducerClass::System {
            return Err(EnvelopeError::RightNotSupported(
                UnsupportedRight::InternalProducer,
            ));
        }
        let entry = self
            .entries
            .get(&p.entry)
            .ok_or(EnvelopeError::InvalidInput("unknown entry contract"))?;
        if p.producer_class != entry.class || p.subject.kind != entry.subject_kind {
            return Err(EnvelopeError::InvalidInput(
                "entry class or subject contract mismatch",
            ));
        }
        self.validate_subject(&p.subject)?;
        match &p.binding {
            SubmissionBinding::Instrument(source)
            | SubmissionBinding::PlatformRelay { source, .. } => {
                if let SubmissionBinding::PlatformRelay {
                    response_contract, ..
                } = &p.binding
                {
                    if entry.response_contract.as_ref() != Some(response_contract) {
                        return Err(EnvelopeError::InvalidInput(
                            "platform response contract mismatch",
                        ));
                    }
                }
                let class = if matches!(p.binding, SubmissionBinding::Instrument(_)) {
                    ProducerClass::Instrument
                } else {
                    ProducerClass::Platform
                };
                if p.producer_class != class
                    || entry.source_contract.as_ref() != Some(&source.contract)
                {
                    return Err(EnvelopeError::InvalidInput(
                        "source binding contract mismatch",
                    ));
                }
                let known = self
                    .sources
                    .get(&source.contract)
                    .ok_or(EnvelopeError::InvalidInput("unknown source contract"))?;
                sorted_unique(&source.identities, true)?;
                sorted_unique(&source.arguments, true)?;
                for identity in &source.identities {
                    validate_reference(identity, known.reference_format)?;
                    if !known.identities.contains(identity) {
                        return Err(EnvelopeError::InvalidInput("unknown source identity"));
                    }
                }
                for args in &source.arguments {
                    validate_arguments(args, &entry.authority_arguments)?;
                }
            }
            SubmissionBinding::ActorReport {
                run,
                definition,
                role,
            } => {
                let (run, def) = self.actor_context(p, *run, *definition)?;
                let roles: Vec<_> = def
                    .reporting_roles
                    .iter()
                    .filter(|(identity, _)| identity == &p.entry)
                    .collect();
                if roles.len() != 1
                    || roles[0].1 != *role
                    || grantee.is_some_and(|p| resolve_role(run, role).ok() != Some(p))
                {
                    return Err(EnvelopeError::InvalidInput(
                        "reporting role does not resolve to grantee",
                    ));
                }
                resolve_role(run, role)?;
                if !entry.authority_arguments.is_empty() {
                    return Err(EnvelopeError::InvalidInput(
                        "actor report lacks required argument binding",
                    ));
                }
            }
            SubmissionBinding::ActionResult {
                run,
                definition,
                occurrence,
                reporter,
                arguments,
            } => {
                let (run, def) = self.actor_context(p, *run, *definition)?;
                nonempty(&occurrence.visit.phase_id)?;
                nonempty(&occurrence.action_name)?;
                if occurrence.visit.round < 1 {
                    return Err(EnvelopeError::InvalidInput(
                        "round zero is not a phase visit",
                    ));
                }
                let actions: Vec<_> = def
                    .actions
                    .iter()
                    .filter(|a| {
                        a.phase_id == occurrence.visit.phase_id
                            && a.action_name == occurrence.action_name
                    })
                    .collect();
                if actions.len() != 1 {
                    return Err(EnvelopeError::InvalidInput(
                        "ambiguous or unknown action occurrence",
                    ));
                }
                let action = actions[0];
                if action.entry != p.entry {
                    return Err(EnvelopeError::InvalidInput(
                        "action entry differs from permission",
                    ));
                }
                let expected = match action.by.as_ref().or(action.phase_owner.as_ref()) {
                    Some(role) => {
                        if grantee.is_some_and(|p| resolve_role(run, role).ok() != Some(p)) {
                            return Err(EnvelopeError::InvalidInput(
                                "action reporter does not resolve to grantee",
                            ));
                        }
                        {
                            resolve_role(run, role)?;
                            ReportingBinding::Role(role.clone())
                        }
                    }
                    None => ReportingBinding::Initiator(run.initiating_principal.clone()),
                };
                if &expected != reporter
                    || (matches!(expected, ReportingBinding::Initiator(_))
                        && grantee.is_some_and(|p| &run.initiating_principal != p))
                {
                    return Err(EnvelopeError::InvalidInput(
                        "action reporting binding mismatch",
                    ));
                }
                validate_arguments(arguments, &entry.authority_arguments)?;
            }
            SubmissionBinding::ClaimTimePlatformBundle => unreachable!(),
        }
        Ok(())
    }
    fn actor_context(
        &self,
        p: &ObservationPermission,
        id: RunRef,
        digest: ContractDigest,
    ) -> EnvelopeResult<(&RunContract, &DefinitionContract)> {
        if p.producer_class != ProducerClass::Actor {
            return Err(EnvelopeError::InvalidInput(
                "actor binding has non-actor class",
            ));
        }
        let run = self
            .runs
            .get(&id)
            .ok_or(EnvelopeError::InvalidInput("unknown named run"))?;
        let definition = self
            .definitions
            .get(&digest)
            .ok_or(EnvelopeError::InvalidInput("unknown pinned definition"))?;
        if run.policy_domain != p.policy_domain
            || run.definition != digest
            || run.subject_kind != p.subject.kind
            || !self.subject_contains(&p.subject, &run.subject)
        {
            return Err(EnvelopeError::InvalidInput(
                "run subject or definition mismatch",
            ));
        }
        Ok((run, definition))
    }
    fn validate_subject(&self, scope: &SubjectScope) -> EnvelopeResult<()> {
        let contract = self
            .subjects
            .get(&scope.kind)
            .ok_or(EnvelopeError::InvalidInput("unknown subject kind"))?;
        match &scope.selector {
            SubjectSelector::Exact(refs) => {
                sorted_unique(refs, true)?;
                for reference in refs {
                    validate_reference(reference, contract.reference_format)?;
                }
            }
            SubjectSelector::Repositories(repositories) => {
                sorted_unique(repositories, true)?;
                if contract.repository_independent
                    || repositories
                        .iter()
                        .any(|r| !contract.repositories.contains(r))
                {
                    return Err(EnvelopeError::InvalidInput(
                        "unknown or unsupported repository selector",
                    ));
                }
            }
            SubjectSelector::AllSubjectsOfKind if !contract.repository_independent => {
                return Err(EnvelopeError::InvalidInput(
                    "repository-bearing subject cannot use all-subjects",
                ))
            }
            SubjectSelector::AllSubjectsOfKind => {}
        }
        Ok(())
    }
    fn subject_contains(&self, scope: &SubjectScope, reference: &ExactBytes) -> bool {
        let Some(contract) = self.subjects.get(&scope.kind) else {
            return false;
        };
        match &scope.selector {
            SubjectSelector::Exact(refs) => refs.contains(reference),
            SubjectSelector::AllSubjectsOfKind => contract.repository_independent,
            SubjectSelector::Repositories(repos) => contract
                .membership
                .iter()
                .any(|(r, repo)| r == reference && repos.contains(repo)),
        }
    }
    fn covers_subject(&self, parent: &SubjectScope, child: &SubjectScope) -> bool {
        if parent.kind != child.kind {
            return false;
        }
        match (&parent.selector, &child.selector) {
            (_, SubjectSelector::Exact(refs)) => {
                refs.iter().all(|r| self.subject_contains(parent, r))
            }
            (SubjectSelector::Repositories(a), SubjectSelector::Repositories(b)) => {
                b.iter().all(|r| a.contains(r))
            }
            (SubjectSelector::AllSubjectsOfKind, SubjectSelector::AllSubjectsOfKind) => self
                .subjects
                .get(&parent.kind)
                .is_some_and(|s| s.repository_independent),
            _ => false,
        }
    }
}
fn resolve_role<'a>(run: &'a RunContract, role: &str) -> EnvelopeResult<&'a Principal> {
    run.bindings
        .iter()
        .find(|(name, _)| name == role)
        .map(|(_, p)| p)
        .ok_or(EnvelopeError::InvalidInput(
            "explicit reporting role is unbound",
        ))
}
fn validate_reference(reference: &ExactBytes, format: ReferenceFormat) -> EnvelopeResult<()> {
    reference.validate()?;
    if format == ReferenceFormat::CanonicalUuid {
        RunRef::parse(
            std::str::from_utf8(&reference.0)
                .map_err(|_| EnvelopeError::InvalidInput("non-UTF8 UUID reference"))?,
        )?;
    }
    Ok(())
}
fn validate_arguments(arguments: &ExactArguments, schema: &[ArgumentSpec]) -> EnvelopeResult<()> {
    collection(&arguments.0, false)?;
    if arguments.0.len() != schema.len() {
        return Err(EnvelopeError::InvalidInput("incomplete argument tuple"));
    }
    for (arg, expected) in arguments.0.iter().zip(schema) {
        if arg.name != expected.name || arg.value.kind() != expected.kind {
            return Err(EnvelopeError::InvalidInput(
                "argument name or type mismatch",
            ));
        }
        if let ArgumentValue::String(text) | ArgumentValue::Word(text) = &arg.value {
            bound("argument text", text.len(), MAX_RECORD_BYTES)?;
        }
    }
    Ok(())
}
fn binding_covers(parent: &SubmissionBinding, child: &SubmissionBinding) -> bool {
    match (parent, child) {
        (SubmissionBinding::Instrument(a), SubmissionBinding::Instrument(b)) => source_covers(a, b),
        (
            SubmissionBinding::PlatformRelay {
                source: a,
                response_contract: ar,
            },
            SubmissionBinding::PlatformRelay {
                source: b,
                response_contract: br,
            },
        ) => ar == br && source_covers(a, b),
        _ => parent == child,
    }
}

fn source_covers(a: &SourceBinding, b: &SourceBinding) -> bool {
    a.contract == b.contract
        && b.identities.iter().all(|i| a.identities.contains(i))
        && b.arguments.iter().all(|args| a.arguments.contains(args))
}
