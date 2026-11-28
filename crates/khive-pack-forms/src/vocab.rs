//! The forms ontology: ten concept subtypes, the properties each must carry
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
/// Who or what lines of one form revision are about, such as its payer or an
/// account it names. It belongs to that revision and is never shared.
pub(crate) const FORM_SUBJECT: &str = "form_subject";
/// A term of a list the caller loads: a kind of subject, a role, or a
/// relation between two subjects of one form.
pub(crate) const FORM_TERM: &str = "form_term";
/// A kind of document, such as a statement, a certificate or a form: what
/// its revisions are, who issues one, and which side produces it. A form is
/// the kind of document with lines to fill.
pub(crate) const DOCUMENT_TYPE: &str = "document_type";

/// Every type the pack declares.
pub(crate) const ALL_TYPES: [&str; 10] = [
    FORM_ISSUER,
    FORM,
    FORM_SECTION,
    FORM_ITEM,
    FIELD,
    FIELD_VALUE,
    FORM_RULE,
    FORM_SUBJECT,
    FORM_TERM,
    DOCUMENT_TYPE,
];

/// Who produces a document of a type: the side that applies, a third party
/// that issues it, or the work itself.
pub(crate) const ORIGINS: [&str; 3] = ["applicant", "third_party", "work"];

/// What a line of a document does with its field: asks for it to be filled,
/// states it as a value every document of the type carries, or requires a
/// document of another type to be attached.
pub(crate) const LINE_RELATIONS: [&str; 3] = ["asks", "states", "requires"];

/// What an item asks for.
pub(crate) const ITEM_KINDS: [&str; 4] = ["text", "choice", "signature", "attachment"];

/// What kind of answer a field takes.
pub(crate) const VALUE_TYPES: [&str; 5] = ["text", "date", "amount", "identifier", "choice"];

/// The most rows a repeating subject or line has. A check reports each
/// unfilled row of a required line, so the bound keeps one check's findings
/// to a size a reader can act on.
pub(crate) const MAX_ROWS: u64 = 100;

/// How necessary a line is: every row of it, none, any one line of a named
/// group, or at least one row of a repeating line.
pub(crate) const NECESSITIES: [&str; 4] = ["required", "optional", "one_of", "at_least_one_row"];

/// How two answers compare when a constraint holds their lines to match or
/// to differ: as written, with case and spacing folded, by their digits
/// alone, or by a key the caller gives with each answer.
pub(crate) const COMPARISONS: [&str; 4] = ["exact", "folded", "digits", "key"];

/// What a constraint holds two lines to.
pub(crate) const MUSTS: [&str; 2] = ["match", "differ"];

/// The term lists a document loads, by the name the document gives each list
/// and the name a term node carries.
pub(crate) const TERM_LISTS: [(&str, &str); 5] = [
    ("kinds", "kind"),
    ("roles", "role"),
    ("relations", "relation"),
    // Qualifiers that only count (first, second, additional); any other
    // qualifier may change what a subject is.
    ("counting_qualifiers", "counting_qualifier"),
    ("purposes", "purpose"),
];

/// Item kinds that must point at exactly one field.
pub(crate) fn item_needs_field(item_kind: &str) -> bool {
    matches!(item_kind, "text" | "choice" | "signature")
}

/// Properties a node of `entity_type` must carry beside its name.
///
/// The entity name holds the issuer's name, the form's title, the section's and
/// item's label, the field's and value's canonical name, the rule's text, the
/// subject's id and the term itself.
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
        FORM_SUBJECT => &["subject_id", "kind", "role", "form_key"],
        FORM_TERM => &["list"],
        DOCUMENT_TYPE => &["document_type_id", "origin"],
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

/// The ten concept subtypes.
pub(crate) static FORMS_ENTITY_TYPES: [EntityTypeDef; 10] = [
    concept(FORM_ISSUER),
    concept(FORM),
    concept(FORM_SECTION),
    concept(FORM_ITEM),
    concept(FIELD),
    concept(FIELD_VALUE),
    concept(FORM_RULE),
    concept(FORM_SUBJECT),
    concept(FORM_TERM),
    concept(DOCUMENT_TYPE),
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
pub(crate) static FORMS_EDGE_RULES: [EdgeEndpointRule; 23] = [
    rule(EdgeRelation::Contains, DOCUMENT_TYPE, FORM),
    rule(EdgeRelation::Contains, FORM_ISSUER, FORM),
    rule(EdgeRelation::Contains, FORM, FORM_SECTION),
    rule(EdgeRelation::Contains, FORM, FORM_ITEM),
    rule(EdgeRelation::Contains, FORM, FORM_SUBJECT),
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
    rule(EdgeRelation::Enables, FORM_RULE, FORM_SUBJECT),
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
    fn a_document_type_contains_its_form_revisions_by_an_existing_relation() {
        let parents: Vec<(EdgeRelation, &str)> = FORMS_EDGE_RULES
            .iter()
            .filter(|r| subtype(&r.target) == FORM && r.relation == EdgeRelation::Contains)
            .map(|r| (r.relation, subtype(&r.source)))
            .collect();
        assert_eq!(
            parents,
            [
                (EdgeRelation::Contains, DOCUMENT_TYPE),
                (EdgeRelation::Contains, FORM_ISSUER)
            ]
        );
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
