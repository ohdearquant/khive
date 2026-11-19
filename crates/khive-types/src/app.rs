//! App manifest: the declared public identity of a khive app (ADR-203 D1, D2).
//!
//! Nothing about an app is learned by importing its crate. The manifest is
//! plain data a host reads before any app code runs: the verb inventory with
//! an explicit rights class per handler, the ontology slice the app
//! contributes, a lifecycle class for every row type it writes, the
//! capability classes it requests and a residency request. Two manifests
//! that do not conflict compose without knowing of each other; a conflict on
//! a verb name or a kind claim refuses the later one naming both owners.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Rights class of one handler.
///
/// Declared in the manifest and never derived from the handler's speech-act
/// category: `Commissive` covers create-shaped and send-shaped verbs alike,
/// so the category cannot stand in for the rights a handler needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightsClass {
    Read,
    Create,
    Write,
    Delete,
    Send,
}

impl RightsClass {
    /// Every rights class, in declaration order.
    pub const ALL: [RightsClass; 5] = [
        RightsClass::Read,
        RightsClass::Create,
        RightsClass::Write,
        RightsClass::Delete,
        RightsClass::Send,
    ];

    /// The wire token for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            RightsClass::Read => "read",
            RightsClass::Create => "create",
            RightsClass::Write => "write",
            RightsClass::Delete => "delete",
            RightsClass::Send => "send",
        }
    }

    /// Parse an exact wire token; anything else is `None`.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == token)
    }
}

impl fmt::Display for RightsClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle class of one row type the app writes (ADR-203 D6).
///
/// `Primary` rows live in the shared substrate and outlive stop, uninstall
/// and purge. `Corpus` rows live in the store the manifest names and are
/// dropped as a unit by purge; a representation (an index, an embedding, a
/// derived view) is a corpus row whose store is the index it lives in, so
/// the `representation` token resolves to `Corpus`. `Blob` rows are content
/// addressed and referenced from primary rows; they name no store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleClass {
    Primary,
    Corpus { store: String },
    Blob,
}

impl LifecycleClass {
    /// Resolve a wire token and the store it names, if any. `representation`
    /// is an alias of `corpus`; a corpus token with no store resolves to an
    /// empty store name, which [`AppManifest::validate`] refuses. Anything
    /// else is `None`.
    pub fn from_token(token: &str, store: Option<&str>) -> Option<Self> {
        match token {
            "primary" => Some(LifecycleClass::Primary),
            "corpus" | "representation" => Some(LifecycleClass::Corpus {
                store: store.unwrap_or_default().into(),
            }),
            "blob" => Some(LifecycleClass::Blob),
            _ => None,
        }
    }

    /// The canonical wire token for this class.
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleClass::Primary => "primary",
            LifecycleClass::Corpus { .. } => "corpus",
            LifecycleClass::Blob => "blob",
        }
    }
}

/// The residency the app asks for; the host validates it against the
/// payload (ADR-203 D3) and the request never decides by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Residency {
    None,
    Linked,
    Process,
}

impl Residency {
    /// The wire token for this residency.
    pub fn as_str(self) -> &'static str {
        match self {
            Residency::None => "none",
            Residency::Linked => "linked",
            Residency::Process => "process",
        }
    }
}

/// Which substrate a declared row type lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowSubstrate {
    Note,
    Entity,
}

impl RowSubstrate {
    /// The substrate name as the kind registry spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            RowSubstrate::Note => "note",
            RowSubstrate::Entity => "entity",
        }
    }
}

/// One verb the app registers, with the rights class its handler needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppVerb {
    pub name: String,
    pub rights: RightsClass,
}

/// The lifecycle declaration for one row type the app writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowTypeDecl {
    pub substrate: RowSubstrate,
    pub kind: String,
    pub lifecycle: LifecycleClass,
}

/// The manifest of one khive app.
///
/// `entity_kinds` and `note_kinds` are the ontology slice: every kind the
/// app writes. Each of them needs a row in `row_types`, and every row there
/// names a kind of the slice, so the class of each row type is declared and
/// never defaulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppManifest {
    pub name: String,
    pub version: String,
    pub signer: String,
    pub verbs: Vec<AppVerb>,
    pub entity_kinds: Vec<String>,
    pub note_kinds: Vec<String>,
    pub row_types: Vec<RowTypeDecl>,
    pub capabilities: Vec<String>,
    pub residency: Residency,
}

/// The names one registered owner already holds: a pack linked into the
/// binary or an installed app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedNames<'a> {
    pub owner: &'a str,
    pub verbs: Vec<&'a str>,
    pub entity_kinds: Vec<&'a str>,
    pub note_kinds: Vec<&'a str>,
}

/// Why a manifest is refused at installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallRefusal {
    /// `name`, `version` or `signer` is empty.
    EmptyIdentity { field: &'static str },
    /// The manifest lists the same verb name twice.
    DuplicateVerb { verb: String },
    /// A kind in the ontology slice has no lifecycle declaration.
    UndeclaredRowType {
        substrate: RowSubstrate,
        kind: String,
    },
    /// A lifecycle declaration names a kind the ontology slice does not contribute.
    RowTypeOutsideSlice {
        substrate: RowSubstrate,
        kind: String,
    },
    /// A corpus row type names no store.
    CorpusWithoutStore {
        substrate: RowSubstrate,
        kind: String,
    },
    /// A verb name is already registered by another owner.
    VerbOwned {
        verb: String,
        owner: String,
        candidate: String,
    },
    /// A kind is already contributed by another owner.
    KindOwned {
        substrate: RowSubstrate,
        kind: String,
        owner: String,
        candidate: String,
    },
}

impl fmt::Display for InstallRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallRefusal::EmptyIdentity { field } => {
                write!(f, "manifest field {field:?} is empty")
            }
            InstallRefusal::DuplicateVerb { verb } => {
                write!(f, "manifest declares verb {verb:?} twice")
            }
            InstallRefusal::UndeclaredRowType { substrate, kind } => write!(
                f,
                "{} kind {kind:?} is in the ontology slice but has no lifecycle class; every row type the app writes declares primary or corpus",
                substrate.as_str()
            ),
            InstallRefusal::RowTypeOutsideSlice { substrate, kind } => write!(
                f,
                "{} kind {kind:?} has a lifecycle declaration but is not in the ontology slice",
                substrate.as_str()
            ),
            InstallRefusal::CorpusWithoutStore { substrate, kind } => write!(
                f,
                "{} kind {kind:?} is a corpus row type and names no store",
                substrate.as_str()
            ),
            InstallRefusal::VerbOwned {
                verb,
                owner,
                candidate,
            } => write!(
                f,
                "verb {verb:?} is already registered by {owner:?}; app {candidate:?} cannot register it"
            ),
            InstallRefusal::KindOwned {
                substrate,
                kind,
                owner,
                candidate,
            } => write!(
                f,
                "{} kind {kind:?} is already contributed by {owner:?}; app {candidate:?} cannot contribute it",
                substrate.as_str()
            ),
        }
    }
}

impl AppManifest {
    /// Check the manifest against itself: identity present, verb names
    /// unique, every kind in the slice declared exactly once with a class,
    /// every corpus row type naming its store.
    pub fn validate(&self) -> Result<(), InstallRefusal> {
        for (field, value) in [
            ("name", &self.name),
            ("version", &self.version),
            ("signer", &self.signer),
        ] {
            if value.is_empty() {
                return Err(InstallRefusal::EmptyIdentity { field });
            }
        }
        for (index, verb) in self.verbs.iter().enumerate() {
            if self.verbs[..index]
                .iter()
                .any(|seen| seen.name == verb.name)
            {
                return Err(InstallRefusal::DuplicateVerb {
                    verb: verb.name.clone(),
                });
            }
        }
        for (substrate, kinds) in [
            (RowSubstrate::Entity, &self.entity_kinds),
            (RowSubstrate::Note, &self.note_kinds),
        ] {
            for kind in kinds {
                if !self
                    .row_types
                    .iter()
                    .any(|row| row.substrate == substrate && row.kind == *kind)
                {
                    return Err(InstallRefusal::UndeclaredRowType {
                        substrate,
                        kind: kind.clone(),
                    });
                }
            }
        }
        for row in &self.row_types {
            let slice = match row.substrate {
                RowSubstrate::Entity => &self.entity_kinds,
                RowSubstrate::Note => &self.note_kinds,
            };
            if !slice.contains(&row.kind) {
                return Err(InstallRefusal::RowTypeOutsideSlice {
                    substrate: row.substrate,
                    kind: row.kind.clone(),
                });
            }
            if matches!(&row.lifecycle, LifecycleClass::Corpus { store } if store.is_empty()) {
                return Err(InstallRefusal::CorpusWithoutStore {
                    substrate: row.substrate,
                    kind: row.kind.clone(),
                });
            }
        }
        Ok(())
    }

    /// The names this manifest would hold once installed.
    pub fn owned_names(&self) -> OwnedNames<'_> {
        OwnedNames {
            owner: &self.name,
            verbs: self.verbs.iter().map(|verb| verb.name.as_str()).collect(),
            entity_kinds: self.entity_kinds.iter().map(String::as_str).collect(),
            note_kinds: self.note_kinds.iter().map(String::as_str).collect(),
        }
    }

    /// Admit the manifest beside the owners already registered: it must
    /// validate, and no verb name or kind it brings may be held by any of
    /// them. The first conflict, in owner order, is the refusal; it names
    /// both owners.
    pub fn admit(&self, owners: &[OwnedNames<'_>]) -> Result<(), InstallRefusal> {
        self.validate()?;
        for owner in owners {
            if let Some(verb) = self
                .verbs
                .iter()
                .find(|verb| owner.verbs.iter().any(|held| *held == verb.name))
            {
                return Err(InstallRefusal::VerbOwned {
                    verb: verb.name.clone(),
                    owner: owner.owner.into(),
                    candidate: self.name.clone(),
                });
            }
            for (substrate, kinds, held) in [
                (
                    RowSubstrate::Entity,
                    &self.entity_kinds,
                    &owner.entity_kinds,
                ),
                (RowSubstrate::Note, &self.note_kinds, &owner.note_kinds),
            ] {
                if let Some(kind) = kinds.iter().find(|kind| held.contains(&kind.as_str())) {
                    return Err(InstallRefusal::KindOwned {
                        substrate,
                        kind: kind.clone(),
                        owner: owner.owner.into(),
                        candidate: self.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
