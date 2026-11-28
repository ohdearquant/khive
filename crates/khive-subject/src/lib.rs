//! Corpus-per-call scanning and pure declaration-to-operation extraction.
//!
//! Scanners preserve source evidence in [`RawDecl`]. Extractors consume those
//! records without opening a source or target store. An [`OperationPlan`] contains
//! intents, not committed records: a caller creates the entities, resolves their
//! returned IDs, then submits the links through the ordinary runtime policy.

#![forbid(unsafe_code)]

mod extract;
mod lean;
mod ontology;

use std::io::BufRead;

use khive_types::{EdgeRelation, EntityKind};

pub use extract::FormalExtractor;
pub use lean::LeanJsonlScanner;
pub use ontology::{DeclaredKind, OntologySpec, RelationMappings, TaxonomyConfig};

/// A source-format-independent declaration, retaining exact source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawDecl {
    pub name: String,
    /// The source kind, before ontology alias resolution.
    pub kind: String,
    pub type_expr: String,
    pub refs: Vec<String>,
    /// Missing/null differs from a present empty string.
    pub doc: Option<String>,
    pub module: String,
}

pub type RawDecls = Vec<RawDecl>;

/// A reusable adapter whose source is supplied separately for every call.
///
/// A failed scan returns no successful prefix. This generic-reader contract does
/// not promise object-safe dispatch or a shared corpus cache.
pub trait Scanner {
    type Error: std::error::Error;

    fn scan<R: BufRead>(&self, source: R) -> Result<RawDecls, Self::Error>;
}

/// A pure extractor with per-call corpus membership and no runtime policy bypass.
pub trait Extractor {
    fn extract(
        &self,
        declarations: &[RawDecl],
        ontology: &OntologySpec,
    ) -> Result<OperationPlan, ExtractError>;
}

/// A typed create intent. Dispatch uses `entity_type` as the granular `kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityIntent {
    pub name: String,
    pub base_kind: EntityKind,
    pub entity_type: String,
    pub type_expr: String,
    pub doc: Option<String>,
    pub module: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DependencyKind {
    Runtime,
}

/// A link between exact declaration names, resolved to stored IDs by the caller.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LinkIntent {
    pub source: String,
    pub target: String,
    pub relation: EdgeRelation,
    pub dependency_kind: Option<DependencyKind>,
}

/// Complete extraction output: input-ordered entities, then first-seen edges.
///
/// Identical edge intents occur once. A forbidden in-corpus endpoint pairing is
/// still an intent; the ordinary runtime remains responsible for its refusal.
/// This value does not promise an atomic transaction across subsequent dispatches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationPlan {
    pub entities: Vec<EntityIntent>,
    pub edges: Vec<LinkIntent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecPart {
    Vocabulary,
    Relations,
    Taxonomy,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("missing ontology part: {part:?}")]
    MissingPart { part: SpecPart },
    #[error("invalid ontology vocabulary: {reason}")]
    InvalidVocabulary { reason: String },
    #[error("invalid ontology relation mapping: {reason}")]
    InvalidRelations { reason: String },
    #[error("unknown declaration kind {kind:?}; available tokens: {available:?}")]
    UnknownKind {
        kind: String,
        available: Vec<&'static str>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot read declaration line {line}: {source}")]
    Read {
        line: usize,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid declaration JSON at line {line}: {source}")]
    Json {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("empty declaration name at record {record}")]
    EmptyName { record: usize },
    #[error("duplicate declaration {name} at records {first_record} and {duplicate_record}")]
    DuplicateName {
        name: String,
        first_record: usize,
        duplicate_record: usize,
    },
    #[error("declaration {name}: {source}")]
    Kind {
        name: String,
        #[source]
        source: SpecError,
    },
}
