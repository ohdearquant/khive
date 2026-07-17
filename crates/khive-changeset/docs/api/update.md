# Update Operations and Field Preimages

An `UpdateOp` pairs a patch with the exact prior values of the fields it touches. The checked constructor and wire deserializer enforce the same substrate, field-set, and numeric invariants.

## `UpdateOp::new`

`new(target_id, patch, preimage)` returns an error unless patch and preimage use the same target variant and have exactly matching populated fields. Every field set or explicitly cleared by the patch must have a captured prior value; no unchanged field may appear in the preimage.

The operation's fields are private so this constructor is the only in-memory construction path. Custom `Deserialize` calls the same constructor, preventing an incongruent wire value from bypassing validation. Accessors return the target ID and borrowed patch/preimage.

## `UpdatePatch`

`UpdatePatch` is tagged by `target` and selects entity, note, or edge fields. An absent field means unchanged. Nullable mutable fields use `Option<Option<T>>`: outer `None` means unchanged, `Some(None)` means explicitly clear to JSON `null`, and `Some(Some(value))` means set.

`EntityPatch` can change name, nullable description, properties, and tags. `NotePatch` can change content, nullable salience/decay factor, properties, and tags. `EdgePatch` can change relation and weight.

An edge patch weight, when present, must be finite and within `[0.0, 1.0]`; custom deserialization enforces the same constraint as link creation and the live edge model.

## `UpdatePreimage`

The preimage is tagged with the same `target` discriminant. `EntityPreimage`, `NotePreimage`, and `EdgePreimage` mirror their patch fields one-for-one: presence means the corresponding field is touched and the value is what existed at staging time.

Captured note salience must be finite and within `[0.0, 1.0]`. Captured note decay factor must be finite and non-negative. Captured edge weight must be finite and within `[0.0, 1.0]`. Values outside those ranges could not represent a valid previously live record and are rejected.

## Congruence failures

Validation reports whether targets differ, a touched field is missing from the preimage, an unchanged field is present, or a captured numeric value violates its live-record range. No partial update object is returned on failure.
