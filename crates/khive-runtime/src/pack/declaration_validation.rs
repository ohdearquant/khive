use std::collections::HashSet;

use super::{EndpointKind, PackRuntime, Visibility};
use crate::RuntimeError;

/// Validate exact wire spellings and endpoints against the composed vocabulary.
pub(super) fn validate_declarations(packs: &[Box<dyn PackRuntime>]) -> Result<(), RuntimeError> {
    let mut note_kinds = HashSet::new();
    let mut entity_kinds: HashSet<_> = khive_types::EntityKind::ALL
        .into_iter()
        .map(|kind| kind.name())
        .collect();
    for pack in packs {
        for handler in pack.handlers() {
            if matches!(handler.visibility, Visibility::Verb)
                && !is_public_handler_name(handler.name)
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "pack {:?} declares invalid public handler {:?}; expected one or two ASCII identifier segments",
                    pack.name(), handler.name
                )));
            }
        }
        for (substrate, kinds) in [("note", pack.note_kinds()), ("entity", pack.entity_kinds())] {
            for &kind in kinds {
                if !is_kind_name(kind) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack {:?} declares invalid {substrate} kind {kind:?}; expected [a-z][a-z0-9_]*",
                        pack.name()
                    )));
                }
            }
        }
        note_kinds.extend(pack.note_kinds().iter().copied());
        entity_kinds.extend(pack.entity_kinds().iter().copied());
    }
    for pack in packs {
        for rule in pack.edge_rules() {
            for (side, endpoint) in [("source", rule.source), ("target", rule.target)] {
                let (substrate, kind, known) = match endpoint {
                    EndpointKind::NoteOfKind(kind) => ("note", kind, note_kinds.contains(kind)),
                    EndpointKind::EntityOfKind(kind) | EndpointKind::EntityOfType { kind, .. } => {
                        ("entity", kind, entity_kinds.contains(kind))
                    }
                };
                if !known {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack {:?} edge rule {:?} has unregistered {side} {substrate} kind {kind:?}",
                        pack.name(), rule.relation
                    )));
                }
            }
        }
    }
    Ok(())
}

fn is_kind_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_public_handler_name(name: &str) -> bool {
    // Match the request parser's identifier shape without imposing pack ownership.
    let mut segments = name.split('.');
    is_identifier(segments.next().unwrap_or_default())
        && segments.next().is_none_or(is_identifier)
        && segments.next().is_none()
}

fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
