use std::collections::{HashMap, HashSet};

use khive_types::EdgeRelation;

use crate::{
    DependencyKind, EntityIntent, ExtractError, Extractor, LinkIntent, OntologySpec, OperationPlan,
    RawDecl,
};

/// Builds pure operation intents using only the supplied records and ontology.
#[derive(Clone, Copy, Debug, Default)]
pub struct FormalExtractor;

impl Extractor for FormalExtractor {
    fn extract(
        &self,
        declarations: &[RawDecl],
        ontology: &OntologySpec,
    ) -> Result<OperationPlan, ExtractError> {
        let mut corpus = HashMap::with_capacity(declarations.len());
        for (index, declaration) in declarations.iter().enumerate() {
            if declaration.name.is_empty() {
                return Err(ExtractError::EmptyName { record: index + 1 });
            }
            if let Some(first) = corpus.insert(declaration.name.as_str(), index) {
                return Err(ExtractError::DuplicateName {
                    name: declaration.name.clone(),
                    first_record: first + 1,
                    duplicate_record: index + 1,
                });
            }
        }
        let mut entities = Vec::with_capacity(declarations.len());
        for declaration in declarations {
            let kind =
                ontology
                    .resolve_kind(&declaration.kind)
                    .map_err(|source| ExtractError::Kind {
                        name: declaration.name.clone(),
                        source,
                    })?;
            entities.push(EntityIntent {
                name: declaration.name.clone(),
                base_kind: kind.base_kind,
                entity_type: kind.entity_type,
                type_expr: declaration.type_expr.clone(),
                doc: declaration.doc.clone(),
                module: declaration.module.clone(),
            });
        }
        let mut edges = Vec::new();
        let mut seen = HashSet::new();
        for declaration in declarations {
            for reference in &declaration.refs {
                if corpus.contains_key(reference.as_str()) {
                    push_edge(
                        &mut edges,
                        &mut seen,
                        LinkIntent {
                            source: declaration.name.clone(),
                            target: reference.clone(),
                            relation: ontology.relations().reference,
                            dependency_kind: (ontology.relations().reference
                                == EdgeRelation::DependsOn)
                                .then_some(DependencyKind::Runtime),
                        },
                    );
                }
            }
            if let Some((parent, head)) = parent_projection(declaration, declarations, &corpus) {
                push_edge(
                    &mut edges,
                    &mut seen,
                    LinkIntent {
                        source: parent.into(),
                        target: head.into(),
                        relation: ontology.relations().parent_projection,
                        dependency_kind: (ontology.relations().parent_projection
                            == EdgeRelation::DependsOn)
                            .then_some(DependencyKind::Runtime),
                    },
                );
            }
        }
        Ok(OperationPlan { entities, edges })
    }
}

fn push_edge(edges: &mut Vec<LinkIntent>, seen: &mut HashSet<LinkIntent>, edge: LinkIntent) {
    if seen.insert(edge.clone()) {
        edges.push(edge);
    }
}

fn parent_projection<'a>(
    declaration: &'a RawDecl,
    declarations: &'a [RawDecl],
    corpus: &HashMap<&str, usize>,
) -> Option<(&'a str, &'a str)> {
    let (parent, leaf) = declaration.name.rsplit_once('.')?;
    let suffix = leaf.strip_prefix("to")?;
    if suffix.is_empty() || declaration.kind != "def" {
        return None;
    }
    let parent_record = &declarations[*corpus.get(parent)?];
    if parent_record.kind != "inductive" {
        return None;
    }
    let (head, self_heads) = type_heads(&declaration.type_expr)?;
    let target = resolve_reference(head, &declaration.refs)?;
    let target_record = &declarations[*corpus.get(target)?];
    if target_record.kind != "inductive"
        || target.rsplit('.').next()? != suffix
        || !self_heads
            .iter()
            .any(|head| resolve_reference(head, &declaration.refs) == Some(parent))
    {
        return None;
    }
    Some((parent, target))
}

fn resolve_reference<'a>(head: &str, references: &'a [String]) -> Option<&'a str> {
    let suffix = format!(".{head}");
    let mut resolved = None;
    for reference in references {
        if reference == head || reference.ends_with(&suffix) {
            match resolved {
                Some(previous) if previous != reference.as_str() => return None,
                _ => resolved = Some(reference.as_str()),
            }
        }
    }
    resolved
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Group(char),
    Arrow,
    Colon,
    Comma,
}

#[derive(Clone, Copy)]
struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
}

fn closing(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '⦃' => Some('⦄'),
        _ => None,
    }
}

fn is_closing(character: char) -> bool {
    matches!(character, ')' | ']' | '}' | '⦄')
}

fn group_end(text: &str) -> Option<usize> {
    let mut stack = Vec::new();
    let mut quoted = None;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if let Some(quote) = quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' && quote == '"' {
                escaped = true;
            } else if character == quote {
                quoted = None;
            }
            continue;
        }
        if character == '"' || character == '«' {
            quoted = Some(if character == '«' { '»' } else { '"' });
        } else if let Some(close) = closing(character) {
            stack.push(close);
        } else if is_closing(character) {
            if stack.pop()? != character {
                return None;
            }
            if stack.is_empty() {
                return Some(index + character.len_utf8());
            }
        }
    }
    None
}

fn tokens(mut text: &str) -> Option<Vec<Token<'_>>> {
    let mut output = Vec::new();
    while !text.is_empty() {
        text = text.trim_start();
        let Some(character) = text.chars().next() else {
            break;
        };
        let (kind, length) = if closing(character).is_some() {
            (TokenKind::Group(character), group_end(text)?)
        } else if is_closing(character) {
            return None;
        } else if text.starts_with("->") {
            (TokenKind::Arrow, 2)
        } else if character == '→' {
            (TokenKind::Arrow, character.len_utf8())
        } else if character == ':' {
            (TokenKind::Colon, 1)
        } else if character == ',' {
            (TokenKind::Comma, 1)
        } else {
            let end = text
                .char_indices()
                .skip(1)
                .find(|(index, ch)| {
                    ch.is_whitespace()
                        || closing(*ch).is_some()
                        || is_closing(*ch)
                        || matches!(*ch, '→' | ':' | ',')
                        || text[*index..].starts_with("->")
                })
                .map_or(text.len(), |(index, _)| index);
            (TokenKind::Word, end)
        };
        output.push(Token {
            kind,
            text: &text[..length],
        });
        text = &text[length..];
    }
    Some(output)
}

fn inside(group: &str) -> &str {
    let first = group.chars().next().expect("group token").len_utf8();
    let last = group.chars().next_back().expect("group token").len_utf8();
    &group[first..group.len() - last]
}

fn unparenthesized(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim();
        let parts = tokens(text)?;
        if let [Token {
            kind: TokenKind::Group('('),
            text: group,
        }] = parts.as_slice()
        {
            text = inside(group);
        } else {
            return Some(text);
        }
    }
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            let mut chars = part.chars();
            matches!(chars.next(), Some(first) if first == '_' || first.is_alphabetic())
                && chars.all(|ch| ch == '_' || ch == '\'' || ch.is_alphanumeric())
        })
}

// Only an entire identifier-headed application qualifies; an infix expression's
// first identifier must not become a parent or codomain by accident.
fn application_head(text: &str) -> Option<&str> {
    let text = unparenthesized(text)?;
    let parts = tokens(text)?;
    let first = parts.first()?;
    if first.kind != TokenKind::Word {
        return None;
    }
    let mut head = first.text.strip_prefix('@').unwrap_or(first.text);
    let mut argument_start = 1;
    if let Some(without_dot) = head.strip_suffix('.') {
        let universe = parts.get(1)?;
        if universe.kind != TokenKind::Group('{') || inside(universe.text).trim().is_empty() {
            return None;
        }
        head = without_dot;
        argument_start = 2;
    }
    if !identifier(head) || matches!(head, "fun" | "let" | "by" | "_") {
        return None;
    }
    for argument in &parts[argument_start..] {
        match argument.kind {
            TokenKind::Group(_) => {}
            TokenKind::Word
                if identifier(argument.text)
                    || argument.text.chars().all(|ch| ch.is_ascii_digit()) => {}
            _ => return None,
        }
    }
    Some(head)
}

fn self_binder(text: &str) -> Option<&str> {
    let parts = tokens(text.trim())?;
    let body = match parts.as_slice() {
        [Token {
            kind: TokenKind::Group(_),
            text,
        }] => inside(text),
        _ => text,
    };
    let parts = tokens(body)?;
    let colon = parts
        .iter()
        .position(|token| token.kind == TokenKind::Colon)?;
    if colon != 1 || parts[0].kind != TokenKind::Word || parts[0].text != "self" {
        return None;
    }
    let after_colon = body.find(':')? + 1;
    application_head(&body[after_colon..])
}

fn type_heads(mut text: &str) -> Option<(&str, Vec<&str>)> {
    let mut self_heads = Vec::new();
    loop {
        text = unparenthesized(text)?;
        let parts = tokens(text)?;
        let first = parts.first()?;
        if first.kind == TokenKind::Word && matches!(first.text, "∀" | "forall") {
            let comma = parts
                .iter()
                .position(|token| token.kind == TokenKind::Comma)?;
            let binders = &parts[1..comma];
            if binders.is_empty() {
                return None;
            }
            if binders
                .iter()
                .all(|token| matches!(token.kind, TokenKind::Group(_)))
            {
                for binder in binders {
                    if let Some(head) = self_binder(binder.text) {
                        self_heads.push(head);
                    }
                }
            } else {
                let start = first.text.len();
                let end = parts[comma].text.as_ptr() as usize - text.as_ptr() as usize;
                if let Some(head) = self_binder(&text[start..end]) {
                    self_heads.push(head);
                }
            }
            let end = parts[comma].text.as_ptr() as usize - text.as_ptr() as usize + 1;
            text = &text[end..];
        } else if let Some(arrow) = parts.iter().find(|token| token.kind == TokenKind::Arrow) {
            let start = arrow.text.as_ptr() as usize - text.as_ptr() as usize;
            if text[..start].trim().is_empty() {
                return None;
            }
            if let Some(head) = self_binder(&text[..start]) {
                self_heads.push(head);
            }
            text = &text[start + arrow.text.len()..];
        } else {
            return Some((application_head(text)?, self_heads));
        }
    }
}
