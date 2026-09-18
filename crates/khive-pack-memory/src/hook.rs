//! Validation and derived-field maintenance for the `memory` note kind.
//!
//! `memory.remember` is the pack's only writer of `memory_type`, and it does two things with the
//! value: it checks it against a closed set, and it picks the row's stored `salience` and
//! `decay_factor` from it. The generic property-update path reached the same field through
//! `properties`, a free-form map, and the pack registered no kind hook at all, so an update could
//! store a third value and could move the label without moving the two numbers derived from it.

use async_trait::async_trait;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError};
use khive_storage::Note;

use crate::handlers::{
    validate_memory_type, DEFAULT_DECAY_EPISODIC, DEFAULT_DECAY_SEMANTIC,
    DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC,
};

#[derive(Debug, Default)]
/// KindHook implementation for the `memory` note kind.
pub(crate) struct MemoryHook;

/// The two numbers `memory.remember` derives from a memory type, in the order it derives them.
fn derived_defaults(memory_type: &str) -> (f64, f64) {
    match memory_type {
        "semantic" => (DEFAULT_SALIENCE_SEMANTIC, DEFAULT_DECAY_SEMANTIC),
        _ => (DEFAULT_SALIENCE_EPISODIC, DEFAULT_DECAY_EPISODIC),
    }
}

/// True when a stored value is still the one the create path would have derived for this type.
///
/// This is what separates a value the caller chose from a value that was chosen for them. It is not
/// perfect: a caller who explicitly asked for exactly the default is indistinguishable from one who
/// said nothing. The residue is bounded and its direction is benign, and the alternative — treating
/// every stored number as derived — discards deliberate values, which for this field is the worse
/// error, because `salience` is a first-class parameter of `remember` that callers set on purpose.
fn is_still_derived(stored: Option<f64>, derived: f64) -> bool {
    match stored {
        None => true,
        Some(value) => (value - derived).abs() < f64::EPSILON,
    }
}

/// Validate a `memory_type` written through the generic property path, and move the two numbers
/// derived from it when it actually changes.
///
/// A caller who names `salience` or `decay_factor` in the same update keeps their value, and a
/// value that was already caller-chosen is left alone: what is derived for a caller is replaced,
/// what a caller states is not.
fn normalize_memory_type_update(note: &Note, args: &mut Value) -> Result<(), RuntimeError> {
    let Some(root) = args.as_object_mut() else {
        return Ok(());
    };
    // Presence, not truthiness: the shared patch contract reads an absent key as "leave it alone"
    // and an explicit null as "clear it", so reading null as "the caller said nothing" would
    // overwrite a clear with a derived value and silently ignore the instruction.
    let caller_set_salience = root.contains_key("salience");
    let caller_set_decay = root.contains_key("decay_factor");

    let Some(named) = root
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("memory_type"))
    else {
        return Ok(());
    };

    // Every memory row carries a type: `remember` defaults it to episodic and derives two stored
    // numbers from it, so clearing it would leave both numbers describing a label that is gone —
    // a row no create path can write.
    if named.is_null() {
        return Err(RuntimeError::InvalidInput(
            "memory_type cannot be cleared; a memory always carries one of: episodic | semantic"
                .into(),
        ));
    }
    let named = named
        .as_str()
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "memory_type must be a string naming episodic or semantic; got {named}"
            ))
        })?
        .to_string();
    validate_memory_type(&named)?;

    let current = note
        .properties
        .as_ref()
        .and_then(|properties| properties.get("memory_type"))
        .and_then(Value::as_str);
    if current == Some(named.as_str()) {
        // Naming the type it already has is not a change, so nothing is re-derived. This is not
        // only an optimization: a row written through the generic create path carries no salience
        // and no decay, and without this return the re-derivation below would read "unset" as
        // "still derived" and populate both. An unset field and a field holding the default are
        // different states, and only a writer that meant to set one may move between them.
        return Ok(());
    }

    let (old_salience, old_decay) = derived_defaults(current.unwrap_or("episodic"));
    let (new_salience, new_decay) = derived_defaults(&named);

    if !caller_set_salience && is_still_derived(note.salience, old_salience) {
        root.insert("salience".into(), json!(new_salience));
    }
    if !caller_set_decay && is_still_derived(note.decay_factor, old_decay) {
        root.insert("decay_factor".into(), json!(new_decay));
    }
    Ok(())
}

#[async_trait]
impl KindHook for MemoryHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        // `memory.remember` owns the create path and validates there. This hook exists for the
        // update path; adding create-side checks here would duplicate that verb's contract.
        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: uuid::Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn prepare_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        normalize_memory_type_update(note, args)
    }
}
