use std::borrow::Cow;
use std::ops::Deref;
use std::path::PathBuf;

use chrono::DateTime;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

pub const SCHEMA_VERSION: &str = "khive.repo.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaVersion {
    #[serde(rename = "khive.repo.v1")]
    KhiveRepoV1,
}

impl JsonSchema for SchemaVersion {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SchemaVersion")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "const": SCHEMA_VERSION
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Timestamp(String);

impl Timestamp {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        DateTime::parse_from_rfc3339(&value)
            .map_err(|error| format!("invalid RFC3339 timestamp: {error}"))?;
        Ok(Self(value))
    }
}

impl Deref for Timestamp {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for Timestamp {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Timestamp")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time"
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Ratio(f64);

impl Ratio {
    pub fn new(value: f64) -> Result<Self, String> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!(
                "ratio must be a finite value in [0,1], got {value}"
            ))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Ratio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Ratio {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Ratio")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "number",
            "minimum": 0.0,
            "maximum": 1.0
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoBundle {
    pub schema_version: SchemaVersion,
    pub meta: BundleMeta,
    pub graph: RepoGraph,
    pub aggregates: RepoAggregates,
    pub capability: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Availability<T> {
    Available { value: T },
    Unavailable { reason: String },
}

impl<T> Availability<T> {
    pub fn available(value: T) -> Self {
        Self::Available { value }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BoundKind {
    All,
    TopN,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageBound {
    pub kind: BoundKind,
    #[schemars(range(max = 50_000))]
    pub max_items: u32,
    pub order: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureStatus {
    Complete,
    Truncated,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Disclosure {
    pub status: DisclosureStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Page<T> {
    #[schemars(length(max = 50_000))]
    pub items: Vec<T>,
    pub total_count: Availability<u64>,
    pub bound: PageBound,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub disclosure: Disclosure,
}

impl<T> Page<T> {
    pub fn complete(items: Vec<T>, max_items: u32, order: impl Into<String>) -> Self {
        let total = items.len() as u64;
        Self {
            items,
            total_count: Availability::available(total),
            bound: PageBound {
                kind: BoundKind::All,
                max_items,
                order: order.into(),
            },
            next_cursor: None,
            truncated: false,
            disclosure: Disclosure {
                status: DisclosureStatus::Complete,
                reason: None,
            },
        }
    }

    pub fn unavailable(
        max_items: u32,
        order: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            items: Vec::new(),
            total_count: Availability::unavailable(reason.clone()),
            bound: PageBound {
                kind: BoundKind::All,
                max_items,
                order: order.into(),
            },
            next_cursor: None,
            truncated: false,
            disclosure: Disclosure {
                status: DisclosureStatus::Unavailable,
                reason: Some(reason),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BundleMeta {
    pub repository: RepositoryIdentity,
    pub snapshot: SnapshotIdentity,
    pub producer: ProducerIdentity,
    pub ingest: PipelineProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub host: String,
    pub owner: String,
    pub name: String,
    #[schemars(url)]
    pub canonical_url: String,
    pub default_branch: Availability<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotIdentity {
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub head_sha: String,
    pub ingested_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProducerIdentity {
    pub exporter: String,
    pub kkernel_version: String,
    pub khive_pack_git_version: String,
    pub khive_pack_code_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineProvenance {
    pub git_digest: Availability<GitDigestProvenance>,
    pub code_ingest: Availability<CodeIngestProvenance>,
    pub clone_tags: SourceCoverage,
}

impl PipelineProvenance {
    pub fn unknown(source_revision: impl Into<String>) -> Self {
        Self {
            git_digest: Availability::unavailable("git.digest report was not supplied"),
            code_ingest: Availability::unavailable(format!(
                "code.ingest report was not supplied for revision {}",
                source_revision.into()
            )),
            clone_tags: SourceCoverage::Unknown {
                reason: "clone tag coverage was not supplied".into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDigestProvenance {
    pub calls: u32,
    pub history_exhausted: bool,
    pub cursor_stalled: bool,
    pub writes_refused: u64,
    pub changed_paths_filtered_noncanonical: u64,
    pub sources: HistorySourceCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistorySourceCoverage {
    pub commits: SourceCoverage,
    pub issues: SourceCoverage,
    pub pull_requests: SourceCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceCoverage {
    Completed,
    StoppedEarly { reason: String },
    Skipped { reason: String },
    Unrequested,
    Unknown { reason: String },
}

impl SourceCoverage {
    pub fn completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    pub fn unavailable_reason(&self, label: &str) -> Option<String> {
        match self {
            Self::Completed => None,
            Self::StoppedEarly { reason } => Some(format!("{label} stopped early: {reason}")),
            Self::Skipped { reason } => Some(format!("{label} skipped: {reason}")),
            Self::Unrequested => Some(format!("{label} was not requested")),
            Self::Unknown { reason } => Some(format!("{label} coverage unknown: {reason}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeIngestProvenance {
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub source_revision: String,
    pub languages: Vec<String>,
    pub blocked_count: u64,
    pub files_dropped_without_source_path: u64,
    pub files_skipped_without_module_path: u64,
    pub coverage_stamps_missed: u64,
    pub warnings_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoGraph {
    pub repository: RepositoryNode,
    pub packages: Page<PackageNode>,
    pub modules: Page<ModuleNode>,
    pub functions: SymbolPage,
    pub datatypes: SymbolPage,
    pub interfaces: SymbolPage,
    pub commits: Page<CommitNode>,
    pub issues: Page<IssueNode>,
    pub pull_requests: Page<PullRequestNode>,
    pub structure_edges: Page<GraphEdge>,
    pub history_edges: Page<GraphEdge>,
    pub commit_module_edges: Page<GraphEdge>,
    pub history_navigation: HistoryNavigation,
    pub join_resolution: JoinResolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryNode {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PackageNode {
    pub id: String,
    pub name: String,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleNode {
    pub id: String,
    pub package_id: String,
    pub name: String,
    pub language: String,
    pub module_path: String,
    pub source_path: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub source_revision: String,
    pub content_hash: String,
    pub import_scan_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolNode {
    pub id: String,
    pub module_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolPage {
    #[schemars(length(max = 0))]
    pub items: Vec<SymbolNode>,
    pub total_count: Availability<u64>,
    pub bound: PageBound,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub disclosure: Disclosure,
}

impl SymbolPage {
    pub fn empty() -> Self {
        let reason = "symbol-tier ingest is deferred in khive.repo.v1".to_string();
        Self {
            items: Vec::new(),
            total_count: Availability::unavailable(reason.clone()),
            bound: PageBound {
                kind: BoundKind::All,
                max_items: 0,
                order: "symbol_id".into(),
            },
            next_cursor: None,
            truncated: false,
            disclosure: Disclosure {
                status: DisclosureStatus::Unavailable,
                reason: Some(reason),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitNode {
    pub id: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub sha: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{7,40}$"))]
    pub short_sha: String,
    pub author: String,
    pub committed_at: Timestamp,
    #[schemars(inner(regex(pattern = r"^[0-9a-f]{40}$")))]
    pub parents: Vec<String>,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueNode {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub author: Availability<String>,
    pub created_at: Availability<Timestamp>,
    pub closed_at: Availability<Timestamp>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestNode {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub author: Availability<String>,
    pub created_at: Availability<Timestamp>,
    pub merged_at: Availability<Timestamp>,
    pub closed_at: Availability<Timestamp>,
    pub base_ref: Availability<String>,
    pub head_ref: Availability<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryNavigation {
    pub by_module: Page<ModuleHistoryNavigation>,
    pub by_commit: Page<CommitHistoryNavigation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleHistoryNavigation {
    pub module_id: String,
    pub commits: Page<String>,
    pub pull_requests: Availability<Page<String>>,
    pub issues: Availability<Page<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitHistoryNavigation {
    pub commit_id: String,
    pub modules: Page<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOrigin {
    Ingested,
    Derived,
}

impl EdgeOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingested => "ingested",
            Self::Derived => "derived",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = enforce_graph_edge_provenance)]
pub struct GraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub relation: String,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub weight: f64,
    pub origin: EdgeOrigin,
    pub derivation: Option<EdgeDerivation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphEdgeWire {
    id: String,
    source: String,
    target: String,
    relation: String,
    weight: f64,
    origin: EdgeOrigin,
    derivation: serde_json::Value,
}

impl<'de> Deserialize<'de> for GraphEdge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GraphEdgeWire::deserialize(deserializer)?;
        if !wire.weight.is_finite() || !(0.0..=1.0).contains(&wire.weight) {
            return Err(serde::de::Error::custom(
                "graph edge weight must be a finite value in [0,1]",
            ));
        }
        let derivation = if wire.derivation.is_null() {
            None
        } else {
            Some(serde_json::from_value(wire.derivation).map_err(serde::de::Error::custom)?)
        };
        match (wire.origin, derivation.is_some()) {
            (EdgeOrigin::Derived, true) | (EdgeOrigin::Ingested, false) => Ok(Self {
                id: wire.id,
                source: wire.source,
                target: wire.target,
                relation: wire.relation,
                weight: wire.weight,
                origin: wire.origin,
                derivation,
            }),
            (EdgeOrigin::Derived, false) => Err(serde::de::Error::custom(
                "derived graph edge requires derivation provenance",
            )),
            (EdgeOrigin::Ingested, true) => Err(serde::de::Error::custom(
                "ingested graph edge must not carry derivation provenance",
            )),
        }
    }
}

fn enforce_graph_edge_provenance(schema: &mut Schema) {
    let required = schema
        .ensure_object()
        .entry("required")
        .or_insert_with(|| serde_json::json!([]));
    let required = required
        .as_array_mut()
        .expect("object schema required keyword is an array");
    if !required.iter().any(|field| field == "derivation") {
        required.push(serde_json::json!("derivation"));
    }
    schema.insert(
        "allOf".into(),
        serde_json::json!([
            {
                "if": {
                    "properties": {"origin": {"const": "derived"}},
                    "required": ["origin"]
                },
                "then": {
                    "properties": {"derivation": {"not": {"type": "null"}}},
                    "required": ["derivation"]
                }
            },
            {
                "if": {
                    "properties": {"origin": {"const": "ingested"}},
                    "required": ["origin"]
                },
                "then": {
                    "properties": {"derivation": {"type": "null"}},
                    "required": ["derivation"]
                }
            }
        ]),
    );
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum EdgeDerivation {
    ChangedPathSourcePathExact {
        #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
        source_revision: String,
        source_path: String,
    },
    ClonePathFallback {
        #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
        source_revision: String,
        source_path: String,
    },
    RepositoryPackageNormalization {
        source_project: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinResolution {
    pub scope: JoinScope,
    pub repositories: Availability<Vec<RepositoryResolution>>,
    pub historical: Availability<Vec<HistoricalJoinCoverage>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinScope {
    pub languages: Vec<String>,
    pub python: Availability<bool>,
    pub typescript: Availability<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryResolution {
    #[schemars(url)]
    pub repository: String,
    pub language: String,
    pub files: u64,
    pub derived_keys: u64,
    pub entity_keys: u64,
    pub matched: u64,
    pub resolution_rate: Availability<Ratio>,
    pub residuals: Page<JoinResidual>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResidualSide {
    Path,
    Entity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinResidual {
    pub side: ResidualSide,
    pub source_project: String,
    pub module_path: String,
    pub source_path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoricalJoinCoverage {
    #[schemars(url)]
    pub repository: String,
    pub language: String,
    pub total_changed_paths: u64,
    pub rust_in_scope_paths: u64,
    pub matched_rust_paths: u64,
    pub out_of_scope_paths: u64,
    pub unresolved_rust_paths: Page<HistoricalPathResidual>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoricalPathResidual {
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub commit_sha: String,
    pub source_path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Repository,
    Module,
    ModuleSymbolDeferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinTag {
    HistoryOnly,
    StructureOnly,
    Join,
    FieldTagged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewStatus {
    Available,
    Unavailable,
}

impl ViewStatus {
    pub fn is_available(self) -> bool {
        self == Self::Available
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewCapability {
    pub label: String,
    pub granularity: Granularity,
    pub join: JoinTag,
    pub status: ViewStatus,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryStructureViewCapability {
    pub label: String,
    pub granularity: Granularity,
    pub join: JoinTag,
    pub status: ViewStatus,
    pub unavailable_reason: Option<String>,
    pub commit_module_facet: Availability<bool>,
    pub pull_request_module_facet: Availability<bool>,
    pub issue_module_facet: Availability<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewCatalog {
    pub structure_graph: ViewCapability,
    pub history_structure_navigation: HistoryStructureViewCapability,
    pub dependency_topology: ViewCapability,
    pub hotspot_quadrant: ViewCapability,
    pub hidden_coupling: ViewCapability,
    pub structure_treemap: ViewCapability,
    pub cadence_timeline: ViewCapability,
    pub ownership: ViewCapability,
    pub api_surface: ViewCapability,
    pub scorecard: ViewCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub mode: CapabilityMode,
    pub read_only: bool,
    pub writes: bool,
    pub live_queries: bool,
    pub on_demand_ingest: bool,
    pub languages: LanguageCapabilities,
    pub labels: CapabilityLabels,
    pub views: ViewCatalog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    StaticShowcase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanguageCapabilities {
    pub rust: LanguageCapability,
    pub python: LanguageCapability,
    pub typescript: LanguageCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanguageCapability {
    pub label: String,
    pub module_join: bool,
    pub measured: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityLabels {
    pub product: String,
    pub input_placeholder: String,
    pub lookup_action: String,
    pub miss_title: String,
    pub miss_body: String,
    pub unavailable: String,
    pub truncated: String,
    pub derived: String,
    pub ingested: String,
    pub node_types: NodeTypeLabels,
    pub metrics: MetricLabels,
    pub hotspot_quadrants: HotspotQuadrantLabels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricLabels {
    pub change_frequency: String,
    pub fan_in: String,
    pub fan_out: String,
    pub cochange_count: String,
    pub support: String,
    pub source_files: String,
    pub recent_activity: String,
    pub week: String,
    pub commits: String,
    pub issues_opened: String,
    pub issues_closed: String,
    pub pull_requests_opened: String,
    pub pull_requests_merged: String,
    pub lead_time: String,
    pub p50: String,
    pub p90: String,
    pub p95: String,
    pub author_concentration: String,
    pub bus_factor: String,
    pub dependent_count: String,
    pub cycle_count: String,
    pub resolution: String,
    pub repository_age: String,
    pub package_count: String,
    pub module_count: String,
    pub symbol_count: String,
    pub activity_trend: String,
    pub top_hotspots: String,
    pub ownership_warnings: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HotspotQuadrantLabels {
    pub high_churn_high_fan_in: String,
    pub high_churn_low_fan_in: String,
    pub low_churn_high_fan_in: String,
    pub low_churn_low_fan_in: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeTypeLabels {
    pub repository: String,
    pub package: String,
    pub module: String,
    pub function: String,
    pub datatype: String,
    pub interface: String,
    pub commit: String,
    pub issue: String,
    pub pull_request: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    AllHistory,
    RollingDays,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalysisWindow {
    pub kind: WindowKind,
    pub start: Option<Timestamp>,
    pub end: Option<Timestamp>,
    pub days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalysisMeta {
    pub label_key: String,
    pub granularity: Granularity,
    pub join: JoinTag,
    pub status: ViewStatus,
    pub unavailable_reason: Option<String>,
    pub inputs: Vec<String>,
    pub window: AnalysisWindow,
    pub bound: PageBound,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Analysis<T> {
    pub meta: AnalysisMeta,
    pub data: Page<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoAggregates {
    pub dependency_topology: DependencyTopologyAnalysis,
    pub hotspot_quadrant: Analysis<HotspotRow>,
    pub hidden_coupling: Analysis<HiddenCouplingRow>,
    pub structure_treemap: Analysis<TreemapRow>,
    pub cadence_timeline: CadenceAnalysis,
    pub ownership: OwnershipAnalysis,
    pub api_surface: Analysis<ApiSurfaceRow>,
    pub scorecard: ScorecardAnalysis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyTopologyAnalysis {
    pub meta: AnalysisMeta,
    pub modules: Page<DependencyModuleRow>,
    pub cycles: Page<DependencyCycle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyModuleRow {
    pub module_id: String,
    pub fan_in: u64,
    pub fan_out: u64,
    pub cycle_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyCycle {
    pub id: String,
    pub module_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HotspotQuadrant {
    HighChurnHighFanIn,
    HighChurnLowFanIn,
    LowChurnHighFanIn,
    LowChurnLowFanIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HotspotRow {
    pub module_id: String,
    pub commit_count: u64,
    pub fan_in: u64,
    pub quadrant: HotspotQuadrant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HiddenCouplingRow {
    pub left_module_id: String,
    pub right_module_id: String,
    pub cochange_count: u64,
    pub support: Ratio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TreemapRow {
    pub package_id: String,
    pub module_id: String,
    pub source_file_count: u64,
    pub recent_commit_count: Availability<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CadenceAnalysis {
    pub meta: AnalysisMeta,
    pub commits: Page<CadencePoint>,
    pub issues_opened: Page<CadencePoint>,
    pub issues_closed: Page<CadencePoint>,
    pub pull_requests_opened: Page<CadencePoint>,
    pub pull_requests_merged: Page<CadencePoint>,
    pub release_tags: Page<ReleaseTag>,
    pub pull_request_lead_time_hours: Availability<Percentiles>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CadencePoint {
    #[schemars(regex(pattern = r"^\d{4}-\d{2}-\d{2}$"))]
    pub week_start: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseTag {
    pub name: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub target_sha: String,
    pub committed_at: Availability<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Percentiles {
    #[schemars(range(min = 0.0))]
    pub p50: f64,
    #[schemars(range(min = 0.0))]
    pub p90: f64,
    #[schemars(range(min = 0.0))]
    pub p95: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnershipRow {
    pub module_id: String,
    pub commit_count: u64,
    pub author_concentration: Availability<Ratio>,
    pub bus_factor: Availability<u64>,
    pub authors: Page<AuthorShare>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnershipAnalysis {
    pub meta: AnalysisMeta,
    pub modules: Page<OwnershipRow>,
    pub repository_author_concentration: Availability<Ratio>,
    pub repository_bus_factor: Availability<u64>,
    pub repository_authors: Page<AuthorShare>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthorShare {
    pub author: String,
    pub commits: u64,
    pub share: Ratio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApiSurfaceRow {
    pub module_id: String,
    pub dependent_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScorecardAnalysis {
    pub meta: AnalysisMeta,
    pub fields: Vec<ScorecardField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScorecardKey {
    RepositoryAgeDays,
    PackageCount,
    ModuleCount,
    SymbolCount,
    ActivityTrend,
    TopHotspots,
    DependencyCycleCount,
    OwnershipWarnings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScorecardField {
    pub key: ScorecardKey,
    pub label_key: String,
    pub granularity: Granularity,
    pub join: JoinTag,
    pub value: Availability<ScorecardValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "value_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScorecardValue {
    Count { value: u64 },
    Ratio { value: f64 },
    Text { value: String },
    ModuleIds { value: Page<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportBounds {
    /// Section-specific limits are enforced by the exporter. The generic Page schema carries a
    /// 50,000-item safety ceiling because a single generic definition cannot express each field's
    /// tighter producer bound without duplicating the closed wire model.
    #[schemars(range(max = 2_048))]
    pub packages: u32,
    #[schemars(range(max = 10_000))]
    pub modules: u32,
    #[schemars(range(max = 5_000))]
    pub commits: u32,
    #[schemars(range(max = 2_000))]
    pub issues: u32,
    #[schemars(range(max = 2_000))]
    pub pull_requests: u32,
    #[schemars(range(max = 50_000))]
    pub structure_edges: u32,
    #[schemars(range(max = 50_000))]
    pub history_edges: u32,
    #[schemars(range(max = 50_000))]
    pub commit_module_edges: u32,
    #[schemars(range(max = 5_000))]
    pub residuals: u32,
    #[schemars(range(max = 5_000))]
    pub aggregate_rows: u32,
    #[schemars(range(max = 10_000))]
    pub navigation_entities: u32,
    #[schemars(range(max = 50))]
    pub navigation_per_entity: u32,
    #[schemars(range(max = 100))]
    pub authors_per_scope: u32,
}

impl Default for ExportBounds {
    fn default() -> Self {
        Self {
            packages: 2_048,
            modules: 10_000,
            commits: 5_000,
            issues: 2_000,
            pull_requests: 2_000,
            structure_edges: 50_000,
            history_edges: 50_000,
            commit_module_edges: 50_000,
            residuals: 5_000,
            aggregate_rows: 1_000,
            navigation_entities: 10_000,
            navigation_per_entity: 50,
            authors_per_scope: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    pub repo_path: PathBuf,
    pub history_db: PathBuf,
    pub map_db: PathBuf,
    pub generated_at: String,
    pub repository_url: String,
    pub bounds: ExportBounds,
    pub provenance: PipelineProvenance,
    /// Explicit input metadata. The exporter never reads mutable branch refs from the clone.
    pub default_branch: Availability<String>,
}
