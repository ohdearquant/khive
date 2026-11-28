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
    "One form revision: {form_id, revision, title, source: {url, sha256}, issuer?: {name, jurisdiction}, terms?: {kinds?, roles?, relations?, purposes?: [name | {name, definition?}]}, purposes?: [purpose], subjects?: [{id, kind, role, qualifier?, repeats?, relations?: [{relation, to}]}], fields?: [{name, definition, value_type, aliases?, values?: [{name, definition}], exclusive?, comparison?}], sections?: [{label, order, sections?}], items?: [{label, page, order, item_kind, filled_by, about?, known_to?, state?, repeats?, necessity?, group?, selects?, section?, field?, value?, widget?, links_to?}], rules?: [{text, page, depends_on?: [{field, value?}], enables?: [{section | item | subject | form}]}], relations?: [{relation, form, revision?}], constraints?: [{must, lines: [{subject?, field, state?}, {subject?, field, state?}], text?}]}. item_kind is text, choice, signature or attachment; value_type is text, date, amount, identifier or choice; necessity is required, optional, one_of (with group) or at_least_one_row; relation is competes_with, composed_with, precedes or depends_on, toward a form already loaded; comparison is exact, folded, digits or key; must is match or differ; known_to is a declared subject, or {relation} for the subject the item's own subject names under that relation.",
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
        description: "Load one form revision from one document in one transaction: the form, its issuer, sections, items, the fields and choice values the items mean, printed rules, and relations to forms already loaded. Loading the same form_id and revision again changes nothing. A live field with the same name, definition and value type is reused, and keeps any alias the document gives it that no other live field answers to (counted as aliases_added; the others come back as warnings); a live field of that name with another meaning leaves the items asking for it without a field, with a warning, to be decided with forms.same or forms.split; a printed rule whose premise names such a field is kept, marked as waiting on it. An exclusive choice field holds at most 48 values, and a reused exclusive field's new values exclude its existing ones. A form may declare subjects, the parties and things its lines are about: each has an id unique in the form, a kind and a role from the term lists (loaded by any document under terms and kept), an optional qualifier and row count (repeats), and named relations to other subjects of the form. On such a form every item that asks for a field names the declared subject it is about (about) and the declared subject who writes it (filled_by), and is required unless it states another necessity; an item naming an undeclared subject refuses the whole document. state tells apart two lines on one subject and field (a former and a new value), repeats gives a line its own row count, and selects lists the subjects a line's value may name. A rule may enable a subject, so that its lines are asked only when the rule's premise holds.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[DOCUMENT],
    },
    HandlerDef {
        name: "forms.add_item",
        description: "Add one printed item to a loaded form and say which field it means. With same_as the item means that field and its label becomes an alias of it. With field, a live field already named or aliased by the proposed name comes back as status match, so the caller decides same (forms.same) or different (forms.split); when no field has that name, the up to three nearest fields of the same value type come back as status no_match, sharing a word or not, and the proposed field is created only when no field of that type exists (status new_field). In match and no_match the item is added without a field.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("form", "string", true, "The form: its id, its form_id, or form_id@revision."),
            param("item", "object", true, "The printed item: {label, page, order, item_kind, filled_by, about?, known_to?, state?, repeats?, necessity?, group?, selects?, section?, widget?}, checked against the subjects its form declares as in forms.load."),
            param("field", "object", false, "The proposed field: {name, definition, value_type, aliases?, values?}."),
            param("same_as", "string", false, "The id or name of the field this item means."),
        ],
    },
    HandlerDef {
        name: "forms.same",
        description: "Give an item that has no field its field: an existing one (field) or a new one (new_field). The item's label, and each further wording in aliases, becomes an alias of the field unless another field already answers to it; the reply names the label as alias_added and the further wordings as aliases_added. The decision is logged on the field. A rule of the item's form waiting on the wording the item named takes its premise on that field, and the reply lists, as rules_settled, the rules left with none waiting.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ITEM_ID,
            param("field", "string", false, "The id or name of the field the item means."),
            param("new_field", "object", false, "A field to create for the item: {name, definition, value_type, aliases?, values?}."),
            param("aliases", "array of string", false, "Further wordings of the field, such as a short form or initials, to add as its aliases."),
        ],
    },
    HandlerDef {
        name: "forms.split",
        description: "Record, in one transaction, that an item's wording names a different field than the live field of that name. The live field is renamed existing_name and keeps its items; a new field new_name is created for the item; the shared wording becomes a general field containing both, so resolving it returns both, flagged ambiguous. Item labels never change, and the renamed field keeps its former name. The decision is logged. A rule of the item's form waiting on the wording the item named takes its premise on new_name, as with forms.same.",
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
        description: "Resolve text to fields, choice values and forms through canonical names, aliases and the printed labels of the items that mean them, rung by rung. exact: the wording folded for case, spacing and punctuation, a label also with its leading line marker (\"5\", \"1b\", \"(c)\") dropped. normalised: the same with possessives folded into their word. initials: one word spelling the first letters of a name or alias. words: every meaningful word of the text is a word of a name, alias or label, in any order, with words such as of and the dropped and plain endings stemmed (signed and signature match). semantic, only when min_rung names it: fields by the similarity of their wordings under the configured embedding model, a wording counting only at min_similarity or above; it embeds every wording of every field no lexical rung found on each call, and the reply's semantic part names the floor and how many texts it embedded. Each hit says how it matched (matched_by: name, alias, label, form_id, title, normalised, initials, words or semantic), the wording that matched, and the forms that print it; a value matched through an option's label cites that option. Hits of every type come back in one list ordered by rank, a stronger rung always first, then by coverage or similarity, then name; total counts the hits before limit. A wording that names several fields at its best rung returns each of them, flagged ambiguous; a general field returns its narrower fields. A text nothing matches comes back with near, the nearest fields by shared words, or a note that no field shares a word with it. The same graph always gives the same answer.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("text", "string", true, "The wording to resolve."),
            param("types", "array of string", false, "Only these hit types: field, field_value, form."),
            param("limit", "integer", false, "Most hits returned: 1 to 100, default 10."),
            param("min_rung", "string", false, "The weakest rung tried: exact, normalised, initials, words (the default) or semantic."),
            param("min_similarity", "number", false, "The least similarity a semantic match needs: above 0 and at most 1, default 0.5. What a similarity means depends on the embedding model, so set it for yours."),
        ],
    },
    HandlerDef {
        name: "forms.neighbourhood",
        description: "Show what surrounds a form, a field or any forms node: the typed nodes and edges within depth, each with its source. A form also gets its sections and items in printed order, its rules and its related forms by relation; a field gets every item that asks for it, whom each line on a form with subjects asks it of, its general and narrower fields and its values. A form also lists the subjects it declares; a subject gets the fields its lines ask for; a term gets every subject that names it, with its form and the fields asked of it.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("node", "string", true, "A node id, a form (form_id or form_id@revision), a field name, or a term as kind:<name>, role:<name> or relation:<name>."),
            param("depth", "integer", false, "Edges to follow outward: 1 to 3, default 1."),
        ],
    },
    HandlerDef {
        name: "forms.check",
        description: "Check a set of forms, and optional answers, against the forms' own rules. Findings: two competing forms both present; a form that must be attached is absent; a printed rule whose premise holds and whose section, item or form is not filled; an answer outside its field's values; two exclusive values given together. Each finding cites its form and, for a rule, the page that prints it. An answer given as a list entry names the subject it is for (and the form, when two checked forms declare that subject), the row of a repeating line and the state of the line, and fills only the lines of that subject, row and state; an answer of the object form fills the lines about no subject, and comes back unplaced where the checked forms ask its field only of subjects. A line with a necessity that no answer fills is a missing finding, row by row for a required line, once for a repeating line that needs at least one row, once for a group of which one line is needed; a line a rule enables is not asked, and not missing, while the rule's premise does not hold (counted in lines_not_asked). An answer whose lines are all not asked comes back in answers_not_asked and holds no premise. A required line bound to no field yet is a line_without_field finding. A missing finding names who writes the line (filled_by) and who already holds it (known_to). Two lines a constraint holds to match or to differ, whose answers break it, are a constraint_unmet finding naming both lines; a constraint that cannot be compared, such as a key comparison with an answer that gives no key, comes back in constraints_unchecked. A line whose value selects a subject takes only the subjects it lists. Answers that name no single field are listed apart, answers that fit no subject or no line of it come back in unplaced_answers with the reason, and rules still waiting on a field to be decided in rules_unchecked. consistent is true only when there are no findings and none of those lists has an entry.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("forms", "array of string", true, "The forms in the set: ids, form_ids or form_id@revision."),
            param("answers", "object or array of object", false, "Field name or id to its answer (text, or a list of values), or a list of {subject?, form?, field, row?, state?, value, key?}; key is what a field that compares by key compares the answer by."),
        ],
    },
    HandlerDef {
        name: "forms.lint",
        description: "List what breaks the forms invariants: items with no field, items that mean more than one field or value or a value without being a choice, fields with no definition, fields no item points at directly (marked when their values are printed as options), wordings shared by several fields, values no option points at, rules with no premise or no conclusion, missing required properties, and nodes with no source.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "forms.export",
        description: "Export the namespace's forms graph as one document: nodes by content key, edges between keys, and the log of decisions about what wordings mean. forms.replay of the document into an empty namespace rebuilds the same graph. Refused, naming the nodes, when a node cannot be written under its own key: two nodes sharing a key, or a node lacking the properties its key is made of.",
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
