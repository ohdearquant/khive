//! The forms ontology: seven concept subtypes, the properties each must carry
//! and the typed edges between them.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef};

/// The authority that publishes forms.
pub(crate) const FORM_ISSUER: &str = "form_issuer";
/// One revision of one printed form.
pub(crate) const FORM: &str = "form";
/// A printed section of a form; sections nest.
pub(crate) const FORM_SECTION: &str = "form_section";
/// One printed item: a box, a choice, a signature line or an attachment slot.
pub(crate) const FORM_ITEM: &str = "form_item";
/// The meaning an item asks for, shared by every item that asks for it.
pub(crate) const FIELD: &str = "field";
/// One allowed answer of a choice field.
pub(crate) const FIELD_VALUE: &str = "field_value";
/// A rule printed on a form, such as "if X, complete section 3".
pub(crate) const FORM_RULE: &str = "form_rule";

/// Every type the pack declares.
pub(crate) const ALL_TYPES: [&str; 7] = [
    FORM_ISSUER,
    FORM,
    FORM_SECTION,
    FORM_ITEM,
    FIELD,
    FIELD_VALUE,
    FORM_RULE,
];

/// What an item asks for.
pub(crate) const ITEM_KINDS: [&str; 4] = ["text", "choice", "signature", "attachment"];

/// What kind of answer a field takes.
pub(crate) const VALUE_TYPES: [&str; 5] = ["text", "date", "amount", "identifier", "choice"];

/// Item kinds that must point at exactly one field.
pub(crate) fn item_needs_field(item_kind: &str) -> bool {
    matches!(item_kind, "text" | "choice" | "signature")
}

/// Properties a node of `entity_type` must carry beside its name.
///
/// The entity name holds the issuer's name, the form's title, the section's and
/// item's label, the field's and value's canonical name and the rule's text.
pub(crate) fn required_properties(entity_type: &str) -> &'static [&'static str] {
    match entity_type {
        FORM_ISSUER => &["jurisdiction"],
        FORM => &[
            "form_id",
            "revision",
            "title",
            "source_url",
            "source_sha256",
        ],
        FORM_SECTION => &["label", "order", "form_key"],
        FORM_ITEM => &[
            "label",
            "page",
            "order",
            "item_kind",
            "filled_by",
            "form_key",
        ],
        FIELD => &["definition", "value_type", "aliases"],
        FIELD_VALUE => &["definition"],
        FORM_RULE => &["text", "source_page", "form_key"],
        _ => &[],
    }
}

const fn concept(type_name: &'static str) -> EntityTypeDef {
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name,
        aliases: &[],
    }
}

/// The seven concept subtypes.
pub(crate) static FORMS_ENTITY_TYPES: [EntityTypeDef; 7] = [
    concept(FORM_ISSUER),
    concept(FORM),
    concept(FORM_SECTION),
    concept(FORM_ITEM),
    concept(FIELD),
    concept(FIELD_VALUE),
    concept(FORM_RULE),
];

const fn typed(entity_type: &'static str) -> EndpointKind {
    EndpointKind::EntityOfType {
        kind: "concept",
        entity_type,
    }
}

const fn rule(
    relation: EdgeRelation,
    source: &'static str,
    target: &'static str,
) -> EdgeEndpointRule {
    EdgeEndpointRule {
        relation,
        source: typed(source),
        target: typed(target),
    }
}

/// The typed edges of the forms graph.
///
/// The base allowlist already admits `contains`, `variant_of`,
/// `competes_with`, `composed_with` and `enables` between any two concepts;
/// these rules name the pairs the forms graph uses. `links_to`, `precedes` and
/// `depends_on` between concepts are open only for the pairs below.
pub(crate) static FORMS_EDGE_RULES: [EdgeEndpointRule; 20] = [
    rule(EdgeRelation::Contains, FORM_ISSUER, FORM),
    rule(EdgeRelation::Contains, FORM, FORM_SECTION),
    rule(EdgeRelation::Contains, FORM, FORM_ITEM),
    rule(EdgeRelation::Contains, FORM_SECTION, FORM_SECTION),
    rule(EdgeRelation::Contains, FORM_SECTION, FORM_ITEM),
    rule(EdgeRelation::Contains, FIELD, FIELD),
    rule(EdgeRelation::Contains, FIELD, FIELD_VALUE),
    rule(EdgeRelation::VariantOf, FORM_ITEM, FIELD),
    rule(EdgeRelation::VariantOf, FORM_ITEM, FIELD_VALUE),
    rule(EdgeRelation::LinksTo, FORM_ITEM, FORM_ITEM),
    rule(EdgeRelation::CompetesWith, FORM, FORM),
    rule(EdgeRelation::CompetesWith, FIELD_VALUE, FIELD_VALUE),
    rule(EdgeRelation::ComposedWith, FORM, FORM),
    rule(EdgeRelation::Precedes, FORM, FORM),
    rule(EdgeRelation::DependsOn, FORM, FORM),
    rule(EdgeRelation::DependsOn, FORM_RULE, FIELD),
    rule(EdgeRelation::DependsOn, FORM_RULE, FIELD_VALUE),
    rule(EdgeRelation::Enables, FORM_RULE, FORM_SECTION),
    rule(EdgeRelation::Enables, FORM_RULE, FORM_ITEM),
    rule(EdgeRelation::Enables, FORM_RULE, FORM),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn subtype(endpoint: &EndpointKind) -> &'static str {
        match endpoint {
            EndpointKind::EntityOfType { kind, entity_type } => {
                assert_eq!(*kind, "concept");
                entity_type
            }
            other => panic!("forms rules name typed concept endpoints only, got {other:?}"),
        }
    }

    #[test]
    fn every_rule_joins_two_declared_types() {
        for rule in &FORMS_EDGE_RULES {
            assert!(ALL_TYPES.contains(&subtype(&rule.source)), "{rule:?}");
            assert!(ALL_TYPES.contains(&subtype(&rule.target)), "{rule:?}");
        }
        let declared: Vec<&str> = FORMS_ENTITY_TYPES.iter().map(|t| t.type_name).collect();
        assert_eq!(declared, ALL_TYPES);
    }

    #[test]
    fn depends_on_opens_only_the_three_pairs() {
        let pairs: Vec<(&str, &str)> = FORMS_EDGE_RULES
            .iter()
            .filter(|r| r.relation == EdgeRelation::DependsOn)
            .map(|r| (subtype(&r.source), subtype(&r.target)))
            .collect();
        assert_eq!(
            pairs,
            [(FORM, FORM), (FORM_RULE, FIELD), (FORM_RULE, FIELD_VALUE)]
        );
        assert!(!pairs.contains(&(FORM_ITEM, FORM)));
    }

    #[test]
    fn every_type_names_its_required_properties() {
        for entity_type in ALL_TYPES {
            assert!(
                !required_properties(entity_type).is_empty(),
                "{entity_type}"
            );
        }
        assert!(required_properties("theorem").is_empty());
    }
}
