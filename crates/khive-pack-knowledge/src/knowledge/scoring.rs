//! TF-IDF scoring, candidate tokenization, term expansion, and top-k selection.
//!
//! Pure computation extracted from the handler module to stay within the
//! file-size gate (<700 LOC per file).

use std::collections::{HashMap, HashSet};

use super::matching;
use super::schema::{Atom, SearchParams};
use super::util::{
    D_COVERAGE_ALPHA, D_EXPAND_DISCOUNT, D_W_BIGRAM, D_W_CONTENT, D_W_DESCRIPTION,
    D_W_EXACT_NAME, D_W_NAME, D_W_TAGS, STOP_WORDS,
};

fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

pub(super) struct Weights {
    pub w_exact_name: f32,
    pub w_name: f32,
    pub w_description: f32,
    pub w_tags: f32,
    pub w_content: f32,
    pub expand_discount: f32,
    pub coverage_alpha: f32,
    pub w_bigram: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            w_exact_name: D_W_EXACT_NAME,
            w_name: D_W_NAME,
            w_description: D_W_DESCRIPTION,
            w_tags: D_W_TAGS,
            w_content: D_W_CONTENT,
            expand_discount: D_EXPAND_DISCOUNT,
            coverage_alpha: D_COVERAGE_ALPHA,
            w_bigram: D_W_BIGRAM,
        }
    }
}

impl Weights {
    pub fn from_opts(opts: &SearchParams) -> Self {
        let w = opts.weights.as_ref();
        Self {
            w_exact_name: w
                .and_then(|w| w.w_exact_name)
                .map_or(D_W_EXACT_NAME, |v| v as f32),
            w_name: w.and_then(|w| w.w_name).map_or(D_W_NAME, |v| v as f32),
            w_description: w
                .and_then(|w| w.w_description)
                .map_or(D_W_DESCRIPTION, |v| v as f32),
            w_tags: w.and_then(|w| w.w_tags).map_or(D_W_TAGS, |v| v as f32),
            w_content: w
                .and_then(|w| w.w_content)
                .map_or(D_W_CONTENT, |v| v as f32),
            expand_discount: w
                .and_then(|w| w.expand_discount)
                .map_or(D_EXPAND_DISCOUNT, |v| v as f32),
            coverage_alpha: w
                .and_then(|w| w.coverage_alpha)
                .map_or(D_COVERAGE_ALPHA, |v| v as f32),
            w_bigram: w.and_then(|w| w.w_bigram).map_or(D_W_BIGRAM, |v| v as f32),
        }
    }
}

pub(super) struct Candidate {
    pub id: String,
    pub slug: String,
    pub name_raw: String,
    pub description_raw: Option<String>,
    pub tags_raw: Option<String>,
    pub status_raw: Option<String>,
    pub finalized: bool,
    pub is_domain: bool,
    pub name: Vec<String>,
    pub description: Vec<String>,
    pub tags: Vec<String>,
    pub content: Vec<String>,
}

pub(super) fn load_candidates_from_atoms(
    atoms: &[Atom],
    type_filter: Option<&str>,
) -> Vec<Candidate> {
    let want_domain = type_filter == Some("domain");
    let want_atom = type_filter == Some("atom");

    atoms
        .iter()
        .filter_map(|atom| {
            let tags_str = atom.tags_display();
            let is_domain = {
                let tags_arr: Vec<String> = serde_json::from_str(&atom.tags).unwrap_or_default();
                tags_arr.iter().any(|t| t == "type:domain")
            };
            if (want_domain && !is_domain) || (want_atom && is_domain) {
                return None;
            }
            Some(Candidate {
                id: atom.id.to_string(),
                slug: atom.slug.clone(),
                name_raw: atom.name.clone(),
                description_raw: Some(atom.content.clone()).filter(|s| !s.is_empty()),
                tags_raw: Some(tags_str.clone()),
                status_raw: atom.status.clone(),
                finalized: atom.finalized,
                is_domain,
                name: matching::tokenize_field(&atom.name),
                description: matching::tokenize_field(&atom.content),
                tags: matching::tokenize_field(&tags_str),
                content: matching::tokenize_field(&atom.content),
            })
        })
        .collect()
}

pub(super) fn compute_idf(
    candidates: &[Candidate],
    terms: &[String],
    expanded: &HashSet<String>,
    discount: f32,
) -> HashMap<String, f32> {
    let n = candidates.len() as f32;
    let mut df: HashMap<String, usize> = terms.iter().map(|t| (t.clone(), 0)).collect();
    for cand in candidates {
        for term in terms {
            if matching::has_in_tokens(&cand.content, term)
                || matching::has_in_tokens(&cand.name, term)
                || matching::has_in_tokens(&cand.description, term)
                || matching::has_in_tokens(&cand.tags, term)
            {
                if let Some(d) = df.get_mut(term) {
                    *d += 1;
                }
            }
        }
    }
    df.into_iter()
        .map(|(term, d)| {
            let raw = (n / (d as f32 + 1.0)).ln().max(0.1);
            let idf = if expanded.contains(&term) {
                raw * discount
            } else {
                raw
            };
            (term, idf)
        })
        .collect()
}

pub(super) fn score_field(
    tokens: &[String],
    terms: &[String],
    idf: &HashMap<String, f32>,
) -> f32 {
    let mut score = 0.0;
    for term in terms {
        let count = matching::count_in_tokens(tokens, term);
        if count > 0 {
            let tf = 1.0 + (count as f32).ln();
            score += tf * idf.get(term).copied().unwrap_or(1.0);
        }
    }
    score
}

pub(super) fn bigram_bonus_field(tokens: &[String], query_order: &[String]) -> f32 {
    if query_order.len() < 2 {
        return 0.0;
    }
    let filtered: Vec<&str> = tokens
        .iter()
        .filter(|t| !is_stop(t))
        .map(|t| t.as_str())
        .collect();
    let mut bonus = 0.0f32;
    for window in query_order.windows(2) {
        let (a, b) = (window[0].as_str(), window[1].as_str());
        for w in filtered.windows(2) {
            if w[0] == a && w[1] == b {
                bonus += 1.0;
                break;
            }
        }
    }
    bonus
}

pub(super) fn exact_name_bonus(name: &str, raw_query: &str, bonus: f32) -> f32 {
    let q = raw_query.trim().to_lowercase();
    if !q.is_empty() && name.to_lowercase().contains(&q) {
        bonus
    } else {
        0.0
    }
}

pub(super) fn score_candidate(
    cand: &Candidate,
    terms: &[String],
    original_terms: &[String],
    query_order: &[String],
    idf: &HashMap<String, f32>,
    raw_query: &str,
    w: &Weights,
) -> f32 {
    let bigrams = bigram_bonus_field(&cand.name, query_order)
        + bigram_bonus_field(&cand.description, query_order)
        + bigram_bonus_field(&cand.tags, query_order)
        + bigram_bonus_field(&cand.content, query_order);

    let base = exact_name_bonus(&cand.name_raw, raw_query, w.w_exact_name)
        + w.w_name * score_field(&cand.name, terms, idf)
        + w.w_description * score_field(&cand.description, terms, idf)
        + w.w_tags * score_field(&cand.tags, terms, idf)
        + w.w_content * score_field(&cand.content, terms, idf)
        + w.w_bigram * bigrams;

    if w.coverage_alpha > 0.0 && !original_terms.is_empty() {
        let matched = original_terms
            .iter()
            .filter(|orig| {
                let has_exact = matching::has_in_tokens(&cand.name, orig)
                    || matching::has_in_tokens(&cand.description, orig)
                    || matching::has_in_tokens(&cand.tags, orig)
                    || matching::has_in_tokens(&cand.content, orig);
                if has_exact {
                    return true;
                }
                terms.iter().filter(|t| *t != *orig).any(|exp| {
                    matching::has_in_tokens(&cand.name, exp)
                        || matching::has_in_tokens(&cand.description, exp)
                        || matching::has_in_tokens(&cand.tags, exp)
                        || matching::has_in_tokens(&cand.content, exp)
                })
            })
            .count();
        let coverage = matched as f32 / original_terms.len() as f32;
        base * coverage.powf(w.coverage_alpha)
    } else {
        base
    }
}

pub(super) fn expand_terms(terms: &mut Vec<String>) -> HashSet<String> {
    let originals: HashSet<String> = terms.iter().cloned().collect();
    let snapshot: Vec<String> = terms.clone();
    for t in &snapshot {
        if !t.ends_with('s') && t.len() >= 3 {
            terms.push(format!("{t}s"));
        }
        if t.ends_with("ies") && t.len() > 4 {
            let s = format!("{}y", &t[..t.len() - 3]);
            if s.len() >= 3 {
                terms.push(s);
            }
        } else if t.ends_with('s') && !t.ends_with("ss") && t.len() > 3 {
            let s = t[..t.len() - 1].to_string();
            if s.len() >= 3 {
                terms.push(s);
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
        .iter()
        .filter(|t| !originals.contains(*t))
        .cloned()
        .collect()
}

pub(super) fn top_k_sorted<T>(
    items: &mut Vec<T>,
    k: usize,
    cmp: impl Fn(&T, &T) -> std::cmp::Ordering,
) {
    if items.len() <= k {
        items.sort_by(&cmp);
        return;
    }
    items.select_nth_unstable_by(k, &cmp);
    items.truncate(k);
    items.sort_by(&cmp);
}
