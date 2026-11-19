//! Markdown packing and rendering for knowledge compose briefings.

use super::{Atom, Domain, HashMap, HashSet, KgEntityHit, ScoredTextItem};

const KG_ENTITIES_HEADING: &str = "\n---\n\n## Knowledge graph\n\n";

fn format_kg_entity_line(entity: &KgEntityHit) -> String {
    let mut line = format!("- **{}** ({})", entity.name, entity.kind);
    if !entity.description.is_empty() {
        line.push_str(&format!(" — {}", entity.description));
    }
    line.push('\n');
    line
}

/// The supplementary KG block has its own heading. Price the exact bytes
/// rendered, and skip a too-large hit so smaller lower-ranked hits can fit.
pub(super) fn trim_kg_entities_to_budget(
    hits: Vec<KgEntityHit>,
    remaining_budget: usize,
) -> Vec<KgEntityHit> {
    let mut used = 0usize;
    hits.into_iter()
        .filter(|h| {
            let heading = if used == 0 {
                KG_ENTITIES_HEADING.len()
            } else {
                0
            };
            let cost = heading + format_kg_entity_line(h).len();
            if cost > remaining_budget.saturating_sub(used) {
                false
            } else {
                used += cost;
                true
            }
        })
        .collect()
}

pub(super) fn format_kg_entities_markdown(entities: &[KgEntityHit]) -> String {
    let mut out = String::from(KG_ENTITIES_HEADING);
    for e in entities {
        out.push_str(&format_kg_entity_line(e));
    }
    out
}

fn format_compose_atom_heading(atom: &Atom) -> String {
    format!("\n## {}\n\nSource: {}\n", atom.name, atom.slug)
}

fn format_compose_section(section: &super::compose::ComposeSectionResult, explain: bool) -> String {
    let mut out = if explain {
        format!(
            "\n### {} (score: {:.4})\n\n",
            section.heading, section.score
        )
    } else {
        format!("\n### {}\n\n", section.heading)
    };
    if !section.content.is_empty() {
        out.push_str(&section.content);
        out.push('\n');
    }
    out
}

fn format_compose_whole_atom(atom: &Atom, score: f32, explain: bool) -> String {
    let mut out = format_compose_atom_heading(atom);
    if explain {
        out.push_str(&format!("Score: {score:.4}\n"));
    }
    if !atom.content.is_empty() {
        out.push('\n');
        out.push_str(&atom.content);
        out.push('\n');
    }
    out
}

fn format_compose_domain_footer(domains: &[Domain]) -> String {
    if domains.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = domains.iter().map(|d| d.name.as_str()).collect();
    format!("\n---\n\nDomains: {}\n", names.join(", "))
}

/// A composed body and the exact records that made it into that body.
pub(super) struct PackedCompose<'a> {
    pub(super) markdown: String,
    pub(super) sections: Vec<&'a super::compose::ComposeSectionResult>,
    pub(super) included_atom_ids: HashSet<String>,
}

/// Greedily pack ranked sections, then whole atoms that have no sections.
/// When no section fits, retain the historical whole-atom fallback. Costs use
/// the same fragments as rendering, including shared headings and metadata.
pub(super) fn pack_compose_markdown<'a>(
    query: &str,
    domains: &[Domain],
    atoms: &'a [Atom],
    items: &[ScoredTextItem],
    sections: &'a [super::compose::ComposeSectionResult],
    explain: bool,
    char_budget: usize,
) -> PackedCompose<'a> {
    const PREFIX: &str = "# Knowledge Briefing\n\nQuery: ";
    let mut footer = format_compose_domain_footer(domains);
    if PREFIX.len() + 1 + footer.len() > char_budget {
        // Full domain metadata remains in `data.domains`; omit an oversized
        // display footer rather than letting it consume the entire briefing.
        footer.clear();
    }
    let query_budget = char_budget - PREFIX.len() - 1 - footer.len();
    let mut query_end = query.len().min(query_budget);
    while !query.is_char_boundary(query_end) {
        query_end -= 1;
    }
    let mut markdown = format!("{PREFIX}{}\n", &query[..query_end]);
    let mut body_used = 0usize;
    let body_budget = char_budget - markdown.len() - footer.len();
    let by_id: HashMap<String, &Atom> = atoms.iter().map(|a| (a.id.to_string(), a)).collect();
    let sectioned_atom_ids: HashSet<&str> = sections.iter().map(|s| s.atom_id.as_str()).collect();
    let mut selected_by_atom: HashMap<&str, Vec<&super::compose::ComposeSectionResult>> =
        HashMap::new();
    let mut selected_sections = Vec::new();
    let mut included_atom_ids = HashSet::new();
    for section in sections {
        let Some(atom) = by_id.get(&section.atom_id) else {
            continue;
        };
        let atom_header_cost = if selected_by_atom.contains_key(section.atom_id.as_str()) {
            0
        } else {
            format_compose_atom_heading(atom).len()
        };
        let cost = atom_header_cost + format_compose_section(section, explain).len();
        if cost > body_budget.saturating_sub(body_used) {
            continue;
        }
        body_used += cost;
        selected_by_atom
            .entry(section.atom_id.as_str())
            .or_default()
            .push(section);
        included_atom_ids.insert(section.atom_id.clone());
        selected_sections.push(section);
    }

    let mut whole_atoms = Vec::new();
    for item in items {
        if !selected_sections.is_empty() && sectioned_atom_ids.contains(item.id.as_str()) {
            continue;
        }
        let Some(atom) = by_id.get(&item.id) else {
            continue;
        };
        let fragment = format_compose_whole_atom(atom, item.score, explain);
        if fragment.len() > body_budget.saturating_sub(body_used) {
            continue;
        }
        body_used += fragment.len();
        included_atom_ids.insert(item.id.clone());
        whole_atoms.push(fragment);
    }

    // Sections retain their existing per-atom presentation order. The
    // sectionless whole-atom tail follows the atom rerank order.
    for atom in atoms {
        let atom_id = atom.id.to_string();
        if let Some(secs) = selected_by_atom.get(atom_id.as_str()) {
            markdown.push_str(&format_compose_atom_heading(atom));
            for section in secs {
                markdown.push_str(&format_compose_section(section, explain));
            }
        }
    }
    for fragment in whole_atoms {
        markdown.push_str(&fragment);
    }
    markdown.push_str(&footer);
    debug_assert_eq!(markdown.len(), char_budget - body_budget + body_used);
    PackedCompose {
        markdown,
        sections: selected_sections,
        included_atom_ids,
    }
}
