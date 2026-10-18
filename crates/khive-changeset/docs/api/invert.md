# Stage an inverse change-set

`ChangeSet::invert` builds a reverse-ordered op-list against current records supplied
by the caller. It performs no graph reads, clock reads, writes, or other I/O.

```rust,ignore
pub fn invert(
    &self,
    envelope: Envelope,
    current: &CurrentState,
) -> Result<ChangeSet, InvertError>
```

The caller reads the affected records when staging the revert and supplies the
revert's own provenance. An envelope equal to the forward envelope is refused,
including for an empty op-list. A revert is independently reviewed as a new
change-set under [ADR-102 D4](../../../../docs/adr/ADR-102-tiered-validate-and-merge.md).

`CurrentState` contains three `BTreeMap`s: entities and notes keyed by their header
IDs, and edges keyed by their edge IDs. Supply each record required by a create,
link, update, soft delete, or restore in its corresponding map. A map key that disagrees with the
embedded record ID is refused. Extra records do not affect the inverse order.

The caller must supply records in the staged fields' representation. Inversion
does not normalize namespaces, tag order, properties, or optional values. For
example, a runtime-added owner property or numeric default differs from an
omitted staged value and causes a create or link to be refused. Resolving that
representation when reading through the daemon is part of the revert caller
(#4886).

## Operation rules

The function walks the forward op-list backwards using a private clone of the
current maps. The forward change-set and supplied maps remain unchanged on both
success and error. Only a complete inverse is returned.

| Forward operation    | Inverse and current-record check                                                                                                                                                                          |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Entity create        | Soft delete of the same ID. The live entity must match the staged namespace, kind, type, name, description, properties, and tags.                                                                         |
| Note create          | Soft delete of the same ID. The live note must match the staged namespace, kind, content, properties, tags, salience, and decay factor; its status must remain the default and its expiry must be absent. |
| Link                 | Soft delete of the same edge ID. The live edge must match the staged namespace, endpoints, relation, weight, and properties.                                                                              |
| Update               | Swap the patch and preimage through `UpdateOp::new`. Every touched working field must equal the forward patch value. Then rewind those working fields to their captured preimage values.                  |
| Soft delete          | Restore the captured full live record; current tombstoned content must match under the [lifecycle checks](restore.md).                                                                                    |
| Restore              | Soft delete carrying the actual current live record; content must match the captured record.                                                                                                              |
| Hard delete or merge | Refused with `NotInvertible`; use reviewed compensation or the git backstop.                                                                                                                              |

Create and link also require `deleted_at` to be absent. Their inverse always has
`hard: false`; its boxed `DeletePreimage` is the checked working record, including
its actual timestamps. No timestamp or field is synthesized from the envelope
or a record constructor. Reversing an update before an earlier create therefore
captures the rewound fields with the supplied record's timestamps.

An update checks only fields present in its patch. Unrelated fields, timestamps,
and record metadata are not compared or rewritten. Nullable fields retain three
distinct states: outer `None` means untouched, `Some(None)` clears the value, and
`Some(Some(value))` sets it. A properties field replaces the whole map; tags retain
their order. Rewinding the working record allows several updates to the same ID
to check the correct intermediate values.

## Refusals

`ReusedEnvelope` is checked before any operation. The other errors name the
zero-based index in the original forward list, even though traversal is reversed.

| Error                              | Meaning                                                                                          |
| ---------------------------------- | ------------------------------------------------------------------------------------------------ |
| `NotInvertible { index, op }`      | A forward hard `delete` or `merge` cannot be represented exactly.                                |
| `MissingCurrent { index, id }`     | The required record is absent from its substrate map.                                            |
| `Invalidated { index, id, field }` | A required current field differs; `field: "id"` identifies an incoherent map key.                |
| `InverseRestore { index, reason }` | A delete preimage cannot be restored, including an archived note or an already-deleted preimage. |
| `InverseUpdate { index, reason }`  | `UpdateOp::new` rejects the swapped pair, preserving its diagnostic.                             |

A forward note patch can contain an out-of-range new salience or decay factor
even though its captured values were validated. Swapping makes that value a
preimage, so the checked constructor refuses it with `InverseUpdate`. There is no
missing-preimage error: the existing update constructor and decoder already
require a congruent preimage.

These are staging checks. Another writer can change a record afterwards; atomic
comparison with captured preimages when landing is tracked separately in #5156.
This API does not perform that landing check.

For update-only lists, reinverting against the restored field values with the
original envelope reproduces the original NDJSON. Soft-delete/restore double inversion preserves record content; lifecycle
timestamps reflect the actual supplied records, as described in [restore](restore.md). The implementation uses in-memory ordered maps on native and
wasm32 targets, consistent with [ADR-101 D5](../../../../docs/adr/ADR-101-kg-changeset-model.md).
