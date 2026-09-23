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
use khive_types::{Details, KhiveError, NoteDraft};

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
        // Shared creation of this kind refuses here, in the pack that owns it, per ADR-021's
        // creation-admission amendment. This is the one place all three admitting paths
        // converge: the shared `create` handler, a `stream.batch` write member, and standalone
        // `stream.append` call this hook before they write a note or a provenance edge. A
        // refusal aborts batch preparation before any sibling commits.
        //
        // Placing it here rather than in the generic handler is the decision, not an
        // implementation preference. The generic pack does not know this kind, and a hook only
        // exists when the pack that owns it is registered — so the refusal can never reach a
        // caller who has no `memory.remember` to dispatch, without asking the registry anything.
        //
        // The refusal is unconditional on the arguments on purpose: a caller supplying the
        // defaults itself still does not get the derivation, the actor routing or the keyed
        // replay contract that `memory.remember` provides, and admitting that request would
        // promise a contract shared creation does not implement.
        //
        // Falsifier: if shared `create` ever grows the full specialized contract — stored type,
        // salience, decay and authenticated actor routing — this refusal is what should be
        // removed, not worked around.
        Err(RuntimeError::InvalidInput(
            "kind=memory is not creatable through shared `create`, `stream.batch`, or standalone \
             `stream.append` — \
             `memory.remember` derives `memory_type`, `salience` and `decay_factor` together and \
             owns episodic actor routing, so a row written here is stored without the fields \
             `memory.recall` supplies at read time and the same record reads differently \
             depending on which path reads it; use `memory.remember` instead"
                .into(),
        ))
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: uuid::Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn normalize_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        normalize_memory_type_update(note, args)
    }

    fn validate_proposal_note(&self, _note: &NoteDraft) -> Result<(), RuntimeError> {
        // ADR-017's 2026-09-22 amendment withdraws ADR-021's prior exception for this one
        // route, so an approved `propose`/`review` changeset no longer admits
        // `AddNote(kind="memory")`. Everything `prepare_create` says above about the
        // shared-create refusal applies here unchanged, and the refusal is unconditional for
        // the same reason: a draft that already carries `memory_type`, `salience`, or
        // `decay_factor` still bypasses the derivation, actor routing, and keyed-replay
        // contract `memory.remember` owns.
        //
        // This runs against the same immutable changeset twice, once when a fresh proposal is
        // created and again when an approved one is applied, including one approved before this
        // hook existed. Neither call site is optional. Dropping the apply-time call would let a
        // pre-upgrade approval slip a memory row past this refusal, and dropping the
        // creation-time call would let a caller wait out a proposal's review cycle only to have
        // it refused for a reason knowable at propose time.
        Err(RuntimeError::Khive(
            KhiveError::invalid_input(
                "kind=memory is not creatable through a proposal AddNote; memory.remember \
                 derives memory_type, salience and decay_factor together and owns episodic \
                 actor routing, so a row admitted here would be stored without the fields \
                 memory.recall supplies at read time; use memory.remember instead",
            )
            .with_details(Details::new([
                ("reason", "kind_admission_refused"),
                ("kind", "memory"),
                ("route", "proposal_add_note"),
            ])),
        ))
    }
}

#[cfg(test)]
mod validate_proposal_note_tests {
    use super::MemoryHook;
    use khive_runtime::{KindHook, RuntimeError};
    use khive_types::NoteDraft;
    use serde_json::json;

    fn refuse(note: NoteDraft) -> RuntimeError {
        MemoryHook
            .validate_proposal_note(&note)
            .expect_err("a memory-kind proposal-note draft must be refused")
    }

    fn assert_refusal_shape(error: &RuntimeError) {
        let RuntimeError::Khive(khive_error) = error else {
            panic!("expected RuntimeError::Khive, got {error:?}");
        };
        let details = khive_error
            .details()
            .expect("refusal must carry structured details");
        assert_eq!(details.get("reason"), Some("kind_admission_refused"));
        assert_eq!(details.get("kind"), Some("memory"));
        assert_eq!(details.get("route"), Some("proposal_add_note"));
        assert!(
            khive_error.to_string().contains("memory.remember"),
            "refusal message must name the writer to use instead: {khive_error}"
        );
    }

    #[test]
    fn bare_draft_is_refused_with_the_canonical_shape() {
        let error = refuse(NoteDraft {
            kind: "memory".to_string(),
            content: "a memory written through a proposal".to_string(),
            name: None,
            properties: None,
        });
        assert_refusal_shape(&error);
    }

    /// A draft carrying values that look like a complete `memory.remember` call is refused
    /// exactly the same way: these fields do not supply the derivation, actor routing, or
    /// keyed-replay contract only `memory.remember` provides.
    #[test]
    fn draft_with_complete_looking_defaults_is_refused_with_the_canonical_shape() {
        let error = refuse(NoteDraft {
            kind: "memory".to_string(),
            content: "a memory written through a proposal with explicit defaults".to_string(),
            name: None,
            properties: Some(json!({
                "memory_type": "episodic",
                "salience": 0.3,
                "decay_factor": 0.02,
            })),
        });
        assert_refusal_shape(&error);
    }
}
