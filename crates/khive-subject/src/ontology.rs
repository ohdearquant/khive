use std::collections::BTreeMap;

use khive_types::{
    to_snake_case, EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef,
    EntityTypeRegistry,
};

use crate::{SpecError, SpecPart};

/// Zero-based module segments used by downstream layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaxonomyConfig {
    pub separator: char,
    pub discipline_segment: usize,
    pub subdiscipline_segment: usize,
}

/// Structural roles over closed relations and the downstream pack's additive rules.
#[derive(Clone, Debug)]
pub struct RelationMappings {
    pub reference: EdgeRelation,
    pub parent_projection: EdgeRelation,
    pub edge_rules: Vec<EdgeEndpointRule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredKind {
    pub base_kind: EntityKind,
    pub entity_type: String,
}

/// Validated, immutable schema contract. Construction does not register a pack.
#[derive(Clone, Debug)]
pub struct OntologySpec {
    vocabulary: Vec<EntityTypeDef>,
    relations: RelationMappings,
    taxonomy: TaxonomyConfig,
}

impl OntologySpec {
    pub fn try_new(
        vocabulary: Option<Vec<EntityTypeDef>>,
        relations: Option<RelationMappings>,
        taxonomy: Option<TaxonomyConfig>,
    ) -> Result<Self, SpecError> {
        let vocabulary =
            vocabulary
                .filter(|items| !items.is_empty())
                .ok_or(SpecError::MissingPart {
                    part: SpecPart::Vocabulary,
                })?;
        let relations = relations
            .filter(|mapping| !mapping.edge_rules.is_empty())
            .ok_or(SpecError::MissingPart {
                part: SpecPart::Relations,
            })?;
        let taxonomy = taxonomy.ok_or(SpecError::MissingPart {
            part: SpecPart::Taxonomy,
        })?;

        let mut keys = BTreeMap::new();
        for definition in &vocabulary {
            if definition.type_name.is_empty()
                || to_snake_case(definition.type_name) != definition.type_name
            {
                return Err(SpecError::InvalidVocabulary {
                    reason: format!("noncanonical subtype {:?}", definition.type_name),
                });
            }
            for spelling in
                std::iter::once(definition.type_name).chain(definition.aliases.iter().copied())
            {
                let key = to_snake_case(spelling);
                if key.is_empty() {
                    return Err(SpecError::InvalidVocabulary {
                        reason: format!("empty alias for {:?}", definition.type_name),
                    });
                }
                if let Some(previous) = keys.insert(key.clone(), definition.type_name) {
                    return Err(SpecError::InvalidVocabulary {
                        reason: format!(
                            "ambiguous token {key:?} belongs to {previous:?} and {:?}",
                            definition.type_name
                        ),
                    });
                }
            }
        }

        for rule in &relations.edge_rules {
            for endpoint in [rule.source, rule.target] {
                let valid = match endpoint {
                    EndpointKind::NoteOfKind(name) | EndpointKind::EntityOfKind(name) => {
                        !name.is_empty()
                    }
                    EndpointKind::EntityOfType { kind, entity_type } => {
                        !kind.is_empty() && !entity_type.is_empty()
                    }
                };
                if !valid {
                    return Err(SpecError::InvalidRelations {
                        reason: "empty endpoint spelling".into(),
                    });
                }
            }
        }
        for relation in [relations.reference, relations.parent_projection] {
            if !relations
                .edge_rules
                .iter()
                .any(|rule| rule.relation == relation)
            {
                return Err(SpecError::InvalidRelations {
                    reason: format!("mapped relation {relation:?} has no declared endpoint rule"),
                });
            }
        }

        Ok(Self {
            vocabulary,
            relations,
            taxonomy,
        })
    }

    /// The five declaration kinds constrain the existing composed formal schema.
    ///
    /// Goal remains part of the runtime's compatibility rules, not a sixth input
    /// declaration kind. No builtin subtype is registered again by this library.
    pub fn formal_math() -> Result<Self, SpecError> {
        let builtin = EntityTypeRegistry::builtin();
        let mut vocabulary = Vec::new();
        for token in ["theorem", "definition", "structure", "instance", "axiom"] {
            let definition = builtin
                .definitions()
                .iter()
                .find(|definition| {
                    definition.kind == EntityKind::Concept && definition.type_name == token
                })
                .ok_or_else(|| SpecError::InvalidVocabulary {
                    reason: format!("missing builtin concept subtype {token:?}"),
                })?;
            vocabulary.push(definition.clone());
        }
        Self::try_new(
            Some(vocabulary),
            Some(RelationMappings {
                reference: EdgeRelation::DependsOn,
                parent_projection: EdgeRelation::Extends,
                edge_rules: vec![
                    formal_rule(EdgeRelation::DependsOn, "theorem", "theorem"),
                    formal_rule(EdgeRelation::DependsOn, "theorem", "definition"),
                    formal_rule(EdgeRelation::DependsOn, "theorem", "structure"),
                    formal_rule(EdgeRelation::DependsOn, "theorem", "axiom"),
                    formal_rule(EdgeRelation::DependsOn, "definition", "definition"),
                    formal_rule(EdgeRelation::DependsOn, "definition", "structure"),
                    formal_rule(EdgeRelation::DependsOn, "definition", "theorem"),
                    formal_rule(EdgeRelation::DependsOn, "definition", "axiom"),
                    formal_rule(EdgeRelation::DependsOn, "instance", "structure"),
                    formal_rule(EdgeRelation::DependsOn, "instance", "definition"),
                    formal_rule(EdgeRelation::DependsOn, "goal", "theorem"),
                    formal_rule(EdgeRelation::DependsOn, "goal", "definition"),
                    formal_rule(EdgeRelation::DependsOn, "goal", "structure"),
                    formal_rule(EdgeRelation::DependsOn, "goal", "axiom"),
                    formal_rule(EdgeRelation::InstanceOf, "instance", "structure"),
                    formal_rule(EdgeRelation::Extends, "structure", "structure"),
                    formal_rule(EdgeRelation::Extends, "definition", "definition"),
                    formal_rule(EdgeRelation::VariantOf, "theorem", "theorem"),
                    formal_rule(EdgeRelation::VariantOf, "definition", "definition"),
                    formal_rule(EdgeRelation::VariantOf, "goal", "theorem"),
                    formal_rule(EdgeRelation::VariantOf, "goal", "definition"),
                ],
            }),
            Some(TaxonomyConfig {
                separator: '.',
                discipline_segment: 1,
                subdiscipline_segment: 2,
            }),
        )
    }

    pub fn vocabulary(&self) -> &[EntityTypeDef] {
        &self.vocabulary
    }

    pub fn relations(&self) -> &RelationMappings {
        &self.relations
    }

    pub fn taxonomy(&self) -> &TaxonomyConfig {
        &self.taxonomy
    }

    pub fn resolve_kind(&self, raw: &str) -> Result<DeclaredKind, SpecError> {
        let normalized = to_snake_case(raw.trim());
        let definition = self.vocabulary.iter().find(|definition| {
            definition.type_name == normalized
                || definition
                    .aliases
                    .iter()
                    .any(|alias| to_snake_case(alias) == normalized)
        });
        let unknown = || SpecError::UnknownKind {
            kind: raw.into(),
            available: self.vocabulary.iter().map(|d| d.type_name).collect(),
        };
        let definition = definition.ok_or_else(unknown)?;
        let resolved = EntityTypeRegistry::new(self.vocabulary.iter().cloned())
            .resolve(definition.kind, Some(raw))
            .map_err(|_| unknown())?;
        Ok(DeclaredKind {
            base_kind: resolved.kind,
            entity_type: resolved.entity_type.ok_or_else(unknown)?,
        })
    }
}

const fn formal_rule(
    relation: EdgeRelation,
    source: &'static str,
    target: &'static str,
) -> EdgeEndpointRule {
    EdgeEndpointRule {
        relation,
        source: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: source,
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: target,
        },
    }
}
