//! The forms pack: its declaration, its verbs and their dispatch.

use async_trait::async_trait;
use khive_runtime::{KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry};
use khive_types::{
    EdgeEndpointRule, EntityTypeDef, HandlerDef, IdResolutionMode, Pack, ParamDef, VerbCategory,
    Visibility,
};
use serde_json::Value;

use crate::handlers::{self, Ctx};
use crate::vocab::{FORMS_EDGE_RULES, FORMS_ENTITY_TYPES};

const fn param(
    name: &'static str,
    param_type: &'static str,
    required: bool,
    description: &'static str,
) -> ParamDef {
    ParamDef {
        name,
        param_type,
        required,
        description,
        resolution_mode: IdResolutionMode::NotApplicable,
    }
}

const DOCUMENT: ParamDef = param(
    "document",
    "object",
    true,
    "One form revision: {form_id, revision, title, source: {url, sha256}, issuer?: {name, jurisdiction}, fields?: [{name, definition, value_type, aliases?, values?: [{name, definition}], exclusive?}], sections?: [{label, order, sections?}], items?: [{label, page, order, item_kind, filled_by, section?, field?, value?, widget?, links_to?}], rules?: [{text, page, depends_on?: [{field, value?}], enables?: [{section | item | form}]}], relations?: [{relation, form, revision?}]}. item_kind is text, choice, signature or attachment; value_type is text, date, amount, identifier or choice; relation is competes_with, composed_with, precedes or depends_on, toward a form already loaded.",
);
const ITEM_ID: ParamDef = param(
    "item",
    "string",
    true,
    "Id of an item that has no field yet, as forms.add_item returned it.",
);

pub(crate) static FORMS_HANDLERS: [HandlerDef; 10] = [
    HandlerDef {
        name: "forms.load",
        description: "Load one form revision from one document in one transaction: the form, its issuer, sections, items, the fields and choice values the items mean, printed rules, and relations to forms already loaded. Loading the same form_id and revision again changes nothing. A live field with the same name, definition and value type is reused; a live field of that name with another meaning leaves the items asking for it without a field, with a warning, to be decided with forms.same or forms.split.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[DOCUMENT],
    },
    HandlerDef {
        name: "forms.add_item",
        description: "Add one printed item to a loaded form and say which field it means. With same_as the item means that field and its label becomes an alias of it. With field, a live field already named or aliased by the proposed name comes back as status match, so the caller decides same (forms.same) or different (forms.split); when no field has that name, the up to three nearest fields of the same value type come back as status no_match, or the proposed field is created when none is near (status new_field). In match and no_match the item is added without a field.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("form", "string", true, "The form: its id, its form_id, or form_id@revision."),
            param("item", "object", true, "The printed item: {label, page, order, item_kind, filled_by, section?, widget?}."),
            param("field", "object", false, "The proposed field: {name, definition, value_type, aliases?, values?}."),
            param("same_as", "string", false, "The id or name of the field this item means."),
        ],
    },
    HandlerDef {
        name: "forms.same",
        description: "Give an item that has no field its field: an existing one (field) or a new one (new_field). The item's label becomes an alias of an existing field unless another field already answers to it. The decision is logged on the field.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ITEM_ID,
            param("field", "string", false, "The id or name of the field the item means."),
            param("new_field", "object", false, "A field to create for the item: {name, definition, value_type, aliases?, values?}."),
        ],
    },
    HandlerDef {
        name: "forms.split",
        description: "Record, in one transaction, that an item's wording names a different field than the live field of that name. The live field is renamed existing_name and keeps its items; a new field new_name is created for the item; the shared wording becomes a general field containing both, so resolving it returns both, flagged ambiguous. Item labels never change, and the renamed field keeps its former name. The decision is logged.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("field", "string", true, "The live field the item's wording names: its id or name."),
            ITEM_ID,
            param("existing_name", "string", true, "The new canonical name of the live field."),
            param("new_name", "string", true, "The canonical name of the field created for the item."),
            param("new_definition", "string", true, "One-sentence definition of the field created for the item."),
            param("existing_definition", "string", false, "A sharper definition for the live field; its current one is kept when absent."),
            param("general_definition", "string", false, "Definition of the general field. Default: Any of: <existing_name>; <new_name>."),
        ],
    },
    HandlerDef {
        name: "forms.resolve",
        description: "Resolve text to fields, choice values and forms through canonical names, aliases and the printed labels of the items that mean them. Each hit says which wording matched and the forms that print it. A wording that names several fields returns each of them, flagged ambiguous. The same graph always gives the same answer.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("text", "string", true, "The wording to resolve."),
            param("types", "array of string", false, "Only these hit types: field, field_value, form."),
        ],
    },
    HandlerDef {
        name: "forms.neighbourhood",
        description: "Show what surrounds a form, a field or any forms node: the typed nodes and edges within depth, each with its source. A form also gets its sections and items in printed order, its rules and its related forms by relation; a field gets every item that asks for it, its general and narrower fields and its values.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("node", "string", true, "A node id, a form (form_id or form_id@revision) or a field name."),
            param("depth", "integer", false, "Edges to follow outward: 1 to 3, default 1."),
        ],
    },
    HandlerDef {
        name: "forms.check",
        description: "Check a set of forms, and optional answers, against the forms' own rules. Findings: two competing forms both present; a form that must be attached is absent; a printed rule whose premise holds and whose section, item or form is not filled; an answer outside its field's values; two exclusive values given together. Each finding cites its form and, for a rule, the page that prints it. Answers that name no single field are listed apart.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("forms", "array of string", true, "The forms in the set: ids, form_ids or form_id@revision."),
            param("answers", "object", false, "Field name or id to its answer: text, or a list of values."),
        ],
    },
    HandlerDef {
        name: "forms.lint",
        description: "List what breaks the forms invariants: items with no field, fields with no definition or no item, wordings shared by several fields, values no option points at, rules with no premise or no conclusion, missing required properties, and nodes with no source.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "forms.export",
        description: "Export the namespace's forms graph as one document: nodes by content key, edges between keys, and the log of decisions about what wordings mean. forms.replay of the document into an empty namespace rebuilds the same graph.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "forms.replay",
        description: "Rebuild a forms.export document in this namespace in one transaction, its decision log included. Refused, with nothing written, when any node of the document is already present.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[param("document", "object", true, "A document forms.export returned.")],
    },
];

/// Forms read into a typed graph: issuers, forms, sections, items, the fields
/// and values items mean, and printed rules.
pub struct FormsPack {
    runtime: KhiveRuntime,
}

impl Pack for FormsPack {
    const NAME: &'static str = "forms";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &FORMS_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &FORMS_EDGE_RULES;
    const ENTITY_TYPES: &'static [EntityTypeDef] = &FORMS_ENTITY_TYPES;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl FormsPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct FormsPackFactory;

impl khive_runtime::PackFactory for FormsPackFactory {
    khive_runtime::pack_factory_metadata!(FormsPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(FormsPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&FormsPackFactory) }

#[async_trait]
impl PackRuntime for FormsPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let handler = FORMS_HANDLERS
            .iter()
            .find(|handler| handler.name == verb)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!("forms pack does not handle verb {verb:?}"))
            })?;
        if let Value::Object(map) = &params {
            if let Some(name) = map
                .keys()
                .find(|name| !handler.params.iter().any(|p| p.name == name.as_str()))
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "{verb} takes no parameter {name:?}; it takes {}",
                    handler
                        .params
                        .iter()
                        .map(|p| p.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        let ctx = Ctx {
            runtime: &self.runtime,
            registry,
            token,
        };
        match verb {
            "forms.load" => handlers::load(&ctx, params).await,
            "forms.add_item" => handlers::add_item(&ctx, params).await,
            "forms.same" => handlers::same(&ctx, params).await,
            "forms.split" => handlers::split(&ctx, params).await,
            "forms.resolve" => handlers::resolve(&ctx, params).await,
            "forms.neighbourhood" => handlers::neighbourhood(&ctx, params).await,
            "forms.check" => handlers::check(&ctx, params).await,
            "forms.lint" => handlers::lint(&ctx, params).await,
            "forms.export" => handlers::export(&ctx, params).await,
            "forms.replay" => handlers::replay(&ctx, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "forms pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
