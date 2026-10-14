use khive_types::EdgeRelation;
use serde::Deserialize;

/// A single configurable lint rule loaded from `rules.toml`.
///
/// `deny_unknown_fields`: a misspelled key (e.g. `severtiy`) must fail the
/// load loudly, never silently fall back to the field's default (commit
/// 4e11ee38). The repository standard here is fail-closed config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub id: String,
    #[serde(default = "default_severity")]
    pub severity: String,
    pub kind: String,
    pub condition: Option<String>,
    pub require_field: Option<String>,
    #[serde(default)]
    pub message: String,
}

fn default_severity() -> String {
    "warning".to_owned()
}

/// Top-level structure of a `rules.toml` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RulesFile {
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    /// Rule class 1: edge `(source kind, relation, target kind)` endpoint
    /// contract, checked against the live pack/base allowlist.
    #[serde(default)]
    pub edge_endpoint_types: Option<EdgeEndpointTypesConfig>,
    /// Rule class 2: likely-inverted directional edges.
    #[serde(default)]
    pub edge_direction_conventions: Option<EdgeDirectionConventionsConfig>,
    /// Rule class 3: unresolvable edge/annotation endpoint references.
    #[serde(default)]
    pub dangling_refs: Option<DanglingRefsConfig>,
    /// Rule class 4: entity name hygiene.
    #[serde(default)]
    pub naming_conventions: Option<NamingConventionsConfig>,
    /// Rule class 5: forward-dated citation/property values.
    #[serde(default)]
    pub citation_date_lint: Option<CitationDateLintConfig>,
}

fn default_enabled() -> bool {
    true
}

/// Default severity for schema/contract-correctness rule classes
/// (`edge-endpoint-types`, `dangling-refs`) — same default the built-in
/// `error`-severity structural checks use, since both classes flag data that
/// is genuinely wrong per the ADR-002/ADR-017 contract, not merely a style
/// preference.
fn default_severity_error() -> String {
    "error".to_owned()
}

/// Config for the `edge-endpoint-types` rule class (rule 1).
///
/// Checks that every edge's `(source kind, relation, target kind)` triple
/// satisfies the canonical endpoint contract — see [`crate::check_edge_endpoint_types`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeEndpointTypesConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_severity_error")]
    pub severity: String,
}

/// One directional-convention entry: the "forward" kind patterns for a
/// specific relation. An edge matching the reversed pattern (target's kind in
/// `forward_source_kinds`, source's kind in `forward_target_kinds`) but not
/// the forward pattern is flagged as likely-inverted.
///
/// Post-parse validated by [`Rules::parse_toml`]: `relation`
/// must name a real [`EdgeRelation`], and both kind lists must be non-empty —
/// a misspelled field name (e.g. `forward_source_kind`) must fail the whole
/// `rules.toml` load, not silently produce an empty-list entry that
/// [`crate::check_edge_direction_conventions`] then skips as a no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectionRuleConfig {
    pub relation: String,
    #[serde(default)]
    pub forward_source_kinds: Vec<String>,
    #[serde(default)]
    pub forward_target_kinds: Vec<String>,
}

/// Config for the `edge-direction-conventions` rule class (rule 2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeDirectionConventionsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default)]
    pub relations: Vec<DirectionRuleConfig>,
}

/// Config for the `dangling-refs` rule class (rule 3).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DanglingRefsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_severity_error")]
    pub severity: String,
}

/// Per-entity-kind override of the naming-convention defaults.
/// `None` fields fall back to the top-level [`NamingConventionsConfig`] value.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NamingConventionsOverride {
    pub max_length: Option<usize>,
    pub no_leading_trailing_whitespace: Option<bool>,
    pub no_parenthetical_suffix: Option<bool>,
}

fn default_max_length() -> usize {
    200
}

/// Config for the `naming-conventions` rule class (rule 4).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamingConventionsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    #[serde(default = "default_enabled")]
    pub no_leading_trailing_whitespace: bool,
    #[serde(default = "default_enabled")]
    pub no_parenthetical_suffix: bool,
    /// Per-entity-kind overrides, keyed by entity kind string (e.g. `"concept"`).
    #[serde(default)]
    pub kinds: std::collections::BTreeMap<String, NamingConventionsOverride>,
}

fn default_date_lint_fields() -> Vec<String> {
    vec![
        "year".to_owned(),
        "date".to_owned(),
        "published_at".to_owned(),
        "publication_date".to_owned(),
    ]
}

/// Config for the `citation-date-lint` rule class (rule 5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitationDateLintConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_severity")]
    pub severity: String,
    /// Property key names checked for forward-dated values.
    #[serde(default = "default_date_lint_fields")]
    pub fields: Vec<String>,
}

/// Post-parse validation for `[[edge_direction_conventions.relations]]`
/// entries: each entry's `relation` must name a real [`EdgeRelation`]
/// and both `forward_source_kinds`/`forward_target_kinds` must be non-empty.
///
/// `#[serde(default)]` on both kind-list fields means TOML deserialization
/// alone cannot distinguish "field present but misspelled" (e.g.
/// `forward_source_kind`, missing the trailing `s`) from "field
/// intentionally omitted" — both parse to an empty `Vec`. Without this check,
/// [`crate::check_edge_direction_conventions`] silently treats a malformed entry as
/// a no-op (it explicitly `continue`s past any entry with an empty kind
/// list), so a typo disables the check instead of failing the load. Erring
/// on the strict side: entries with genuinely empty kind lists are also
/// rejected here rather than allowed as an intentional "always no-op" entry,
/// since a `rules.toml` author has no other way to say "this entry means
/// nothing" than to omit it entirely (`relations` itself defaults to `[]`).
fn validate_direction_rule_entries(relations: &[DirectionRuleConfig]) -> Result<(), String> {
    for (idx, rule) in relations.iter().enumerate() {
        if rule.relation.parse::<EdgeRelation>().is_err() {
            return Err(format!(
                "edge_direction_conventions.relations[{idx}]: {:?} is not a valid edge relation",
                rule.relation
            ));
        }
        if rule.forward_source_kinds.is_empty() {
            return Err(format!(
                "edge_direction_conventions.relations[{idx}] ({:?}): \
                 forward_source_kinds must be non-empty",
                rule.relation
            ));
        }
        if rule.forward_target_kinds.is_empty() {
            return Err(format!(
                "edge_direction_conventions.relations[{idx}] ({:?}): \
                 forward_target_kinds must be non-empty",
                rule.relation
            ));
        }
    }
    Ok(())
}

/// Parsed and semantically validated rules. The definition cannot be mutated after parsing.
#[derive(Debug)]
pub struct Rules(pub(crate) RulesFile);

/// Rule-file syntax and direction-contract errors, before any graph evaluation.
#[derive(Debug)]
pub enum RulesError {
    Syntax(toml::de::Error),
    Direction(String),
}

impl std::fmt::Display for RulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax(error) => write!(f, "{error}"),
            Self::Direction(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RulesError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Syntax(error) => Some(error),
            Self::Direction(_) => None,
        }
    }
}

impl Rules {
    pub fn parse_toml(content: &str) -> Result<Self, RulesError> {
        let rules: RulesFile = toml::from_str(content).map_err(RulesError::Syntax)?;
        if let Some(config) = &rules.edge_direction_conventions {
            validate_direction_rule_entries(&config.relations).map_err(RulesError::Direction)?;
        }
        Ok(Self(rules))
    }

    /// Whether the host must supply its real pack endpoint metadata.
    pub fn needs_pack_edge_rules(&self) -> bool {
        self.0
            .edge_endpoint_types
            .as_ref()
            .is_some_and(|config| config.enabled && valid_severity(&config.severity))
    }

    /// Whether citation evaluation needs one host-supplied clock reading.
    pub fn needs_current_time(&self) -> bool {
        self.0
            .citation_date_lint
            .as_ref()
            .is_some_and(|config| config.enabled && valid_severity(&config.severity))
    }
}

fn valid_severity(severity: &str) -> bool {
    matches!(severity, "error" | "warning" | "info")
}
