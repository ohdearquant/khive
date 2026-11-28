//! forms.resolve: a wording to the fields, choice values and forms it names,
//! tried rung by rung from the exact wording down to its words.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use khive_runtime::RuntimeError;
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use super::params::{self, Args};
use super::read::cite;
use super::views::{field_reply, usage, value_reply};
use super::Ctx;
use crate::store::{aliases, invalid, is_general, label, prop_text, wordings, Graph};
use crate::text::{fold, initials, label_folds, normalize, stem, stems, without_marker};
use crate::vocab::{FIELD, FIELD_VALUE, FORM, FORM_ITEM};

/// The rungs in the order they are tried. A caller stops at `min_rung`; the
/// semantic rung runs only when it is named.
const RUNGS: [&str; 5] = ["exact", "normalised", "initials", "words", "semantic"];
const EXACT: usize = 0;
const NORMALISED: usize = 1;
const INITIALS: usize = 2;
const WORDS: usize = 3;
const SEMANTIC: usize = 4;

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;
/// How many fields a miss names as near.
const NEAR: usize = 3;
/// The similarity a semantic match needs when the caller names none. What a
/// similarity means depends on the embedding model, so a caller that knows its
/// model passes its own floor.
const DEFAULT_MIN_SIMILARITY: f64 = 0.5;

// Ranks. The exact rung: 0 a field's name, 1 its alias, 2 a value's name or a
// form id, 3 a printed label, 4 a form title. Each weaker rung ranks below.
const RANK_NORMALISED: u8 = 5;
const RANK_NORMALISED_LABEL: u8 = 6;
const RANK_INITIALS: u8 = 7;
const RANK_WORDS: u8 = 8;
const RANK_WORDS_LABEL: u8 = 9;
const RANK_SEMANTIC: u8 = 10;

/// The rung a rank belongs to.
fn rung_of(rank: u8) -> usize {
    match rank {
        0..=4 => EXACT,
        RANK_NORMALISED | RANK_NORMALISED_LABEL => NORMALISED,
        RANK_INITIALS => INITIALS,
        RANK_WORDS | RANK_WORDS_LABEL => WORDS,
        _ => SEMANTIC,
    }
}

/// How a field was found.
#[derive(Clone)]
struct Found {
    rank: u8,
    how: &'static str,
    /// The general field it was reached through.
    via: Option<String>,
    /// The printed wording that matched.
    wording: Option<String>,
    /// Coverage for a words match, similarity for a semantic one.
    score: Option<f64>,
}

impl Found {
    fn new(rank: u8, how: &'static str, wording: Option<String>, score: Option<f64>) -> Self {
        Self {
            rank,
            how,
            via: None,
            wording,
            score,
        }
    }
}

/// Keep the best way a field was found: the lowest rank, then the higher score.
fn offer(found: &mut BTreeMap<Uuid, Found>, id: Uuid, entry: Found) {
    let better = match found.get(&id) {
        None => true,
        Some(known) => {
            entry.rank < known.rank
                || (entry.rank == known.rank
                    && entry.score.unwrap_or(0.0) > known.score.unwrap_or(0.0))
        }
    };
    if better {
        found.insert(id, entry);
    }
}

/// The fields found so far.
struct Matches<'g> {
    graph: &'g Graph,
    found: BTreeMap<Uuid, Found>,
    general: Vec<Value>,
    /// Fields matched by a wording of their own; weaker rungs skip them.
    named: HashSet<Uuid>,
}

impl Matches<'_> {
    /// A field matched by its own wording. A general field answers with the
    /// narrower fields it contains.
    fn field(&mut self, field: &Entity, entry: Found) {
        self.named.insert(field.id);
        if is_general(field) {
            let id = field.id.to_string();
            if !self.general.iter().any(|g| g["id"] == id.as_str()) {
                self.general.push(json!({"id": id, "name": field.name}));
            }
            for child in self.graph.children_of_field(field.id) {
                let entry = Found {
                    via: Some(field.name.clone()),
                    ..entry.clone()
                };
                offer(&mut self.found, child, entry);
            }
        } else {
            offer(&mut self.found, field.id, entry);
        }
    }

    /// The fields an item means, reached through its printed label.
    fn item(&mut self, item: &Entity, entry: Found) {
        for field in self.graph.fields_of_item(item.id) {
            offer(&mut self.found, field, entry.clone());
        }
    }
}

/// The printed wordings of a field: its name, then each alias.
fn names_of(field: &Entity) -> Vec<String> {
    let mut out = vec![field.name.clone()];
    for alias in aliases(field) {
        if !out.contains(&alias) {
            out.push(alias);
        }
    }
    out
}

/// The printed wording of `field` that folds to `folded`.
fn wording_of(field: &Entity, folded: &str) -> Option<String> {
    aliases(field)
        .into_iter()
        .chain(std::iter::once(field.name.clone()))
        .find(|w| normalize(w) == folded)
}

/// The share of `candidate`'s words the wanted words cover, when every wanted
/// word is one of them.
fn coverage(wanted: &BTreeSet<String>, candidate: &str) -> Option<f64> {
    let have = stems(candidate);
    (!wanted.is_empty() && wanted.is_subset(&have)).then(|| wanted.len() as f64 / have.len() as f64)
}

/// `coverage` of a printed label, with and without its leading line marker.
fn label_coverage(wanted: &BTreeSet<String>, label: &str) -> Option<f64> {
    let whole = coverage(wanted, label);
    let bare = without_marker(label).and_then(|rest| coverage(wanted, rest));
    match (whole, bare) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The candidate with the highest score; the first one on a tie.
fn best(candidates: impl Iterator<Item = (String, f64)>) -> Option<(String, f64)> {
    candidates.fold(None, |best, (wording, score)| match best {
        Some((_, known)) if known >= score => best,
        _ => Some((wording, score)),
    })
}

/// Whether an item prints the text, exactly or normalised, with or without
/// its line marker.
fn prints(item: &Entity, folded: &str, joined: &str) -> bool {
    let printed = label(item);
    label_folds(&printed, normalize).iter().any(|f| f == folded)
        || label_folds(&printed, fold).iter().any(|f| f == joined)
}

/// Order key for a score: higher first.
fn order_score(score: Option<f64>) -> i64 {
    -(score.unwrap_or(0.0) * 1e6).round() as i64
}

fn limit(args: &Args) -> Result<usize, RuntimeError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| {
                invalid(format!(
                    "limit must be a whole number from 1 to {MAX_LIMIT}"
                ))
            }),
    }
}

fn min_similarity(args: &Args) -> Result<f64, RuntimeError> {
    match args.get("min_similarity") {
        None | Some(Value::Null) => Ok(DEFAULT_MIN_SIMILARITY),
        Some(value) => value
            .as_f64()
            .filter(|n| *n > 0.0 && *n <= 1.0)
            .ok_or_else(|| invalid("min_similarity must be a number above 0 and at most 1")),
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut aa, mut bb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa <= 0.0 || bb <= 0.0 {
        0.0
    } else {
        dot / (aa.sqrt() * bb.sqrt())
    }
}

/// The semantic rung: each field no lexical rung found, by the similarity of
/// its nearest wording to the text under the configured embedding model, when
/// that similarity reaches `floor`. Every wording of every such field is
/// embedded on each call. Returns what the reply says about the rung.
async fn semantic(
    ctx: &Ctx<'_>,
    text: &str,
    fields: &[&Entity],
    floor: f64,
    matches: &mut Matches<'_>,
) -> Result<Value, RuntimeError> {
    let model = ctx.runtime.default_embedder_name().to_owned();
    if model.is_empty() {
        return Ok(json!({"ran": false, "reason": "no embedding model is configured"}));
    }
    let mut owners: Vec<(&Entity, String)> = Vec::new();
    for field in fields {
        if matches.named.contains(&field.id) || matches.found.contains_key(&field.id) {
            continue;
        }
        for wording in names_of(field) {
            owners.push((*field, wording));
        }
    }
    if owners.is_empty() {
        return Ok(json!({"ran": true, "model": model, "min_similarity": floor, "embedded": 0}));
    }
    let mut texts = vec![text.to_owned()];
    texts.extend(owners.iter().map(|(_, wording)| wording.clone()));
    let vectors = ctx.runtime.embed_batch(&texts).await?;
    if vectors.len() != texts.len() {
        return Err(RuntimeError::Internal(format!(
            "the embedding model returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        )));
    }
    let mut nearest: BTreeMap<Uuid, (&Entity, String, f64)> = BTreeMap::new();
    for ((field, wording), vector) in owners.into_iter().zip(&vectors[1..]) {
        let similarity = cosine(&vectors[0], vector);
        if similarity < floor {
            continue;
        }
        let closer = nearest
            .get(&field.id)
            .is_none_or(|(_, _, known)| similarity > *known);
        if closer {
            nearest.insert(field.id, (field, wording, similarity));
        }
    }
    for (field, wording, similarity) in nearest.into_values() {
        matches.field(
            field,
            Found::new(RANK_SEMANTIC, "semantic", Some(wording), Some(similarity)),
        );
    }
    Ok(json!({
        "ran": true,
        "model": model,
        "min_similarity": floor,
        "embedded": texts.len(),
    }))
}

/// The fields nearest a wording that matched nothing: most shared words
/// first, at most `NEAR` of them.
fn near(graph: &Graph, fields: &[&Entity], text: &str) -> Vec<Value> {
    let wanted = stems(text);
    let mut scored: Vec<(f64, &Entity, String, BTreeSet<String>)> = Vec::new();
    for field in fields {
        let mut candidates = names_of(field);
        candidates.extend(
            graph
                .items_of_field(field.id)
                .into_iter()
                .filter_map(|item| graph.get(item))
                .map(label),
        );
        let mut closest: Option<(f64, String, BTreeSet<String>)> = None;
        for wording in candidates {
            let have = stems(&wording);
            let shared: BTreeSet<String> = wanted.intersection(&have).cloned().collect();
            if shared.is_empty() {
                continue;
            }
            let score = shared.len() as f64 / wanted.union(&have).count() as f64;
            if closest.as_ref().is_none_or(|(known, ..)| score > *known) {
                closest = Some((score, wording, shared));
            }
        }
        if let Some((score, wording, shared)) = closest {
            scored.push((score, *field, wording, shared));
        }
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(a.1.name.cmp(&b.1.name))
            .then(a.1.id.cmp(&b.1.id))
    });
    let folded = fold(text);
    scored
        .into_iter()
        .take(NEAR)
        .map(|(score, field, wording, shared)| {
            let mut words: Vec<&str> = Vec::new();
            for word in folded.split(' ') {
                if shared.contains(&stem(word)) && !words.contains(&word) {
                    words.push(word);
                }
            }
            json!({
                "id": field.id.to_string(),
                "name": field.name,
                "wording": wording,
                "shared": words,
                "overlap": score,
            })
        })
        .collect()
}

/// forms.resolve: text to fields, values and forms.
pub(crate) async fn resolve(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let text = params::text(&args, "text")?;
    let types = params::string_list(&args, "types")?;
    for t in &types {
        params::one_of(t.clone(), "types", &[FIELD, FIELD_VALUE, FORM])?;
    }
    let wants = |t: &str| types.is_empty() || types.iter().any(|x| x == t);
    let last = match params::opt_text(&args, "min_rung")? {
        Some(rung) => {
            let rung = params::one_of(rung, "min_rung", &RUNGS)?;
            RUNGS.iter().position(|r| *r == rung).unwrap_or(WORDS)
        }
        None => WORDS,
    };
    let limit = limit(&args)?;
    let floor = min_similarity(&args)?;
    let folded = normalize(&text);
    if folded.is_empty() {
        return Err(invalid("text must contain a letter or a digit"));
    }
    let joined = fold(&text);
    let wanted = stems(&text);
    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let fields = graph.of_type(FIELD);
    let items = graph.of_type(FORM_ITEM);
    let mut matches = Matches {
        graph: &graph,
        found: BTreeMap::new(),
        general: Vec::new(),
        named: HashSet::new(),
    };

    // Exact: a field's name or alias, or a printed label.
    for field in &fields {
        let how = if normalize(&field.name) == folded {
            Some((0, "name"))
        } else if wordings(field).contains(&folded) {
            Some((1, "alias"))
        } else {
            None
        };
        if let Some((rank, how)) = how {
            matches.field(
                field,
                Found::new(rank, how, wording_of(field, &folded), None),
            );
        }
    }
    let labelled: Vec<&Entity> = items
        .iter()
        .copied()
        .filter(|item| label_folds(&label(item), normalize).contains(&folded))
        .collect();
    for item in &labelled {
        matches.item(item, Found::new(3, "label", Some(label(item)), None));
    }

    // Normalised: the same words once possessives fold into their word.
    let mut labelled_joined: Vec<&Entity> = Vec::new();
    if last >= NORMALISED {
        for field in &fields {
            if matches.named.contains(&field.id) {
                continue;
            }
            if let Some(wording) = names_of(field).into_iter().find(|w| fold(w) == joined) {
                matches.field(
                    field,
                    Found::new(RANK_NORMALISED, "normalised", Some(wording), None),
                );
            }
        }
        labelled_joined = items
            .iter()
            .copied()
            .filter(|item| {
                !labelled.iter().any(|l| l.id == item.id)
                    && label_folds(&label(item), fold).contains(&joined)
            })
            .collect();
        for item in &labelled_joined {
            matches.item(
                item,
                Found::new(RANK_NORMALISED_LABEL, "normalised", Some(label(item)), None),
            );
        }
    }

    // Initials: one word that spells the first letters of a name or alias.
    if last >= INITIALS && !joined.contains(' ') && joined.chars().count() >= 2 {
        for field in &fields {
            if matches.named.contains(&field.id) {
                continue;
            }
            if let Some(wording) = names_of(field)
                .into_iter()
                .find(|w| initials(w).contains(&joined))
            {
                matches.field(
                    field,
                    Found::new(RANK_INITIALS, "initials", Some(wording), None),
                );
            }
        }
    }

    // Words: every meaningful word of the text is a word of a name, an alias
    // or a label, in any order; the better covered first.
    if last >= WORDS && !wanted.is_empty() {
        for field in &fields {
            if matches.named.contains(&field.id) {
                continue;
            }
            let closest = best(
                names_of(field)
                    .into_iter()
                    .filter_map(|w| coverage(&wanted, &w).map(|c| (w, c))),
            );
            if let Some((wording, share)) = closest {
                matches.field(
                    field,
                    Found::new(RANK_WORDS, "words", Some(wording), Some(share)),
                );
            }
        }
        for item in &items {
            let printed = label(item);
            if let Some(share) = label_coverage(&wanted, &printed) {
                matches.item(
                    item,
                    Found::new(RANK_WORDS_LABEL, "words", Some(printed), Some(share)),
                );
            }
        }
    }

    let semantic_rung = if last >= SEMANTIC && wants(FIELD) {
        Some(semantic(ctx, &text, &fields, floor, &mut matches).await?)
    } else {
        None
    };

    let Matches { found, general, .. } = matches;
    let in_rung = |rung: usize| found.values().filter(|f| rung_of(f.rank) == rung).count();
    let ambiguous = found
        .values()
        .map(|f| rung_of(f.rank))
        .min()
        .is_some_and(|rung| in_rung(rung) > 1);

    // Every hit with its rank, its order score and the name and id it is
    // ordered by, so fields, values and forms come back as one list.
    let mut hits: Vec<(u8, i64, String, Uuid, Value)> = Vec::new();
    if wants(FIELD) {
        for (id, entry) in &found {
            let Some(field) = graph.get(*id) else {
                continue;
            };
            let items: Vec<&Entity> = graph
                .items_of_field(field.id)
                .into_iter()
                .filter_map(|i| graph.get(i))
                .collect();
            let printing: Vec<Value> = items
                .iter()
                .filter(|i| prints(i, &folded, &joined))
                .map(|i| usage(i))
                .collect();
            let mut hit = field_reply(&graph, field);
            hit["type"] = json!(FIELD);
            hit["rank"] = json!(entry.rank);
            hit["matched_by"] = json!(entry.how);
            hit["matched_wording"] = json!(entry.wording);
            hit["ambiguous"] = json!(in_rung(rung_of(entry.rank)) > 1);
            if let Some(via) = &entry.via {
                hit["via"] = json!(via);
            }
            if let Some(score) = entry.score {
                let key = if entry.rank == RANK_SEMANTIC {
                    "similarity"
                } else {
                    "coverage"
                };
                hit[key] = json!(score);
            }
            hit["forms"] = Value::Array(if printing.is_empty() {
                items.iter().map(|i| usage(i)).collect()
            } else {
                printing
            });
            hits.push((
                entry.rank,
                order_score(entry.score),
                field.name.clone(),
                field.id,
                hit,
            ));
        }
    }
    if wants(FIELD_VALUE) {
        for value in graph.of_type(FIELD_VALUE) {
            let options = |list: &[&Entity]| -> Vec<Value> {
                list.iter()
                    .filter(|i| graph.out(i.id, EdgeRelation::VariantOf).contains(&value.id))
                    .map(|i| usage(i))
                    .collect()
            };
            let exact = options(labelled.as_slice());
            let (rank, how, wording, forms) = if normalize(&value.name) == folded {
                (2, "name", value.name.clone(), exact)
            } else if !exact.is_empty() {
                (3, "label", text.clone(), exact)
            } else if last >= NORMALISED && fold(&value.name) == joined {
                (
                    RANK_NORMALISED,
                    "normalised",
                    value.name.clone(),
                    options(labelled_joined.as_slice()),
                )
            } else {
                let joined_options = options(labelled_joined.as_slice());
                if joined_options.is_empty() {
                    continue;
                }
                (
                    RANK_NORMALISED_LABEL,
                    "normalised",
                    text.clone(),
                    joined_options,
                )
            };
            let mut hit = value_reply(value);
            hit["type"] = json!(FIELD_VALUE);
            hit["rank"] = json!(rank);
            hit["matched_by"] = json!(how);
            hit["matched_wording"] = json!(wording);
            hit["field"] = json!(graph
                .parent_field(value.id)
                .and_then(|f| graph.get(f))
                .map(|f| f.name.clone()));
            hit["forms"] = Value::Array(forms);
            hits.push((rank, 0, value.name.clone(), value.id, hit));
        }
    }
    if wants(FORM) {
        for form in graph.of_type(FORM) {
            let by_title = normalize(&form.name) == folded;
            let by_id = prop_text(form, "form_id").is_some_and(|id| normalize(&id) == folded)
                || graph.key(form.id).as_deref() == Some(text.as_str());
            let by_joined_title = last >= NORMALISED && fold(&form.name) == joined;
            let (rank, how) = if by_id {
                (2, "form_id")
            } else if by_title {
                (4, "title")
            } else if by_joined_title {
                (RANK_NORMALISED_LABEL, "normalised")
            } else {
                continue;
            };
            let mut hit = cite(form);
            hit["type"] = json!(FORM);
            hit["rank"] = json!(rank);
            hit["matched_by"] = json!(how);
            hits.push((rank, 0, form.name.clone(), form.id, hit));
        }
    }
    hits.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    let total = hits.len();
    hits.truncate(limit);
    let mut reply = json!({
        "text": text,
        "ambiguous": ambiguous,
        "general": general,
        "hits": hits.into_iter().map(|(.., hit)| hit).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "min_rung": RUNGS[last],
    });
    if total == 0 && wants(FIELD) {
        let near = near(&graph, &fields, &text);
        if near.is_empty() {
            reply["note"] = json!("no field shares a word with the text");
        }
        reply["near"] = json!(near);
    }
    if let Some(semantic) = semantic_rung {
        reply["semantic"] = semantic;
    }
    Ok(reply)
}
