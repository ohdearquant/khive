# ADR-115: Exact-content manifest classification for the write secret gate

**Status**: Accepted
**Date**: 2026-07-16
**Authors**: khive maintainers
**Relates to**: [ADR-014](./ADR-014-curation-operations.md),
[ADR-015](./ADR-015-schema-migrations.md), and
[ADR-018](./ADR-018-authorization-gate.md)

## Context

The write secret gate uses conservative detectors to prevent accidental credential persistence.
Synthetic examples in tests and documentation can share a detector shape with a credential even
after they have been reviewed as non-secret. Weakening the detector for the entire shape would
reduce protection for unrelated writes.

The accepted mechanism must recognize only exact, reviewed fixture values in their declared field
scope. It must not grant a caller a bypass capability, infer trust from a source path, or weaken
scanning of any non-matching value.

## Decision

Adopt a runtime-owned, versioned exact-content manifest for reviewed secret-detector false
positives. A matching value receives a typed internal classification, a reserved record posture,
and an audit event. Every value that does not meet the full contract follows the ordinary secret
scanner unchanged.

The mechanism is intentionally narrow. It classifies one exact detector result; it does not grant
authorization or establish general content safety.

### 1. Eligibility is runtime-owned and exact

The runtime determines eligibility from the canonical value presented to the scanner and the
runtime-assigned field scope. Both inputs participate in a versioned, full-length digest computed
inside the secret gate. A value admitted for one field scope is not admitted for another.

Callers cannot provide a manifest decision, digest assertion, exemption flag, source label, path,
or other metadata that affects eligibility. Runtime execution cannot enroll new values. A manifest
revision is an ordinary reviewed source change and ships with its corresponding public fixtures.

Digest encoding, domain separation, and manifest serialization are versioned implementation
contracts owned by the secret-gate component. Changing any of them requires regenerating the
fixture manifest and passing compatibility tests.

### 2. The public fixture is deterministic

The repository fixture contains only synthetic non-secret values and includes:

- every admitted value and its declared field scope;
- a one-byte mutation and a scope mutation for each admitted value;
- Unicode, newline, and JSON-escape cases;
- credential-shaped controls that must remain blocked; and
- malformed, conflicting, stale-version, and unsupported-format manifests.

The fixture builder invokes the same canonicalization, scope selection, and digest functions as
runtime writes. It accepts an explicit input list and never discovers values by scanning a
directory or observing normal execution.

### 3. Classification is finalized with the write

All property-carrying write paths use one shared finalization boundary. That boundary applies the
reserved posture metadata, appends the audit event, and persists the record in one atomic unit.

Callers cannot create, change, merge, or remove the reserved posture. A byte-identical posture
echo from a read-modify-write client may be ignored before diffing; any different caller-supplied
value is rejected.

An update retains the posture only while the classified field bytes and scope are unchanged. If
either changes, the runtime discards the old classification and evaluates the new value from the
beginning. The new value receives posture metadata only after a fresh exact match.

### 4. Audit is mandatory and content-free

Every admitted classification appends one queryable event recording the mechanism version,
manifest identifier, field scope, runtime-computed digest, canonical operation, request
attribution, persisted record identifier, and outcome. The event contains neither the scanned
value nor a detector excerpt.

The record, posture, and event commit atomically. If the event or posture cannot commit, the
classified write is rejected.

### 5. All failures are fail-closed

A missing, unreadable, malformed, stale, conflicting, unsupported, or unknown-version manifest
cannot classify a value as reviewed. Digest mismatch, scope mismatch, posture failure, and audit
failure also cannot admit the classified write.

For a manifest miss or load failure, the ordinary secret scanner remains authoritative. If the
scanner blocks the value, the write is blocked. No error path converts a detector rejection into
an admission.

### 6. Trust boundary

This feature is enabled only when the runtime, manifest, and database are protected by the same
trusted installation boundary. The manifest is reviewed source input distributed with the code,
not mutable request data. Authorization remains the responsibility of ADR-018, and database file
integrity remains an installation responsibility.

Within that boundary, the decision guarantees exact value-and-scope matching, non-caller-controlled
eligibility, mandatory posture and audit, and unchanged scanning for every miss. An installation
that cannot protect the manifest or database from unauthorized modification MUST disable manifest
classification. The classification must never be described as authentication, attestation, or
proof that the bytes are safe outside the specific detector decision.

### 7. Views

Any projection that exposes record properties preserves the reserved posture. `search`, `context`,
and export do not hide or down-rank classified records by default. A view may explicitly filter or
rank on the posture as a separate policy.

## Implementation fences

### MAY

- Load or explicitly refresh a versioned manifest into an immutable in-memory set.
- Extend the secret-gate result type with an internal reviewed-value classification.
- Add a migration if durable posture or event indexing requires one.
- Add explicit view filters over the reserved posture.

### MAY NOT

- Accept caller-provided eligibility flags, digests, paths, identities, namespaces, sources, or
  operation names.
- Enroll a directory or values observed during normal execution.
- Skip scanning one field because another field matched.
- Add a dedicated alternate write operation.
- Perform manifest file I/O on every write.
- Retain a classification after its field value or scope changes without a new exact match.
- Commit a classified record without its posture and audit event.

## Verification

The public fixture suite must prove:

- every fixture value matches only in its declared scope;
- every one-byte or field-scope mutation misses;
- every credential-shaped control remains blocked;
- builder and runtime digests agree for Unicode, newlines, and JSON escapes;
- caller attempts to mutate the reserved posture fail on every public write path;
- content-changing updates cannot retain stale posture;
- each admitted record has exactly one matching audit event;
- every manifest failure returns control to the unchanged scanner path;
- no hot-path manifest file I/O occurs after load; and
- `search`, `context`, and export preserve the posture.

Tests derive expected counts from the checked-in fixture rather than a machine-specific constant.

## Alternatives considered

| Alternative                                               | Reason rejected                                                     |
| --------------------------------------------------------- | ------------------------------------------------------------------- |
| Weaken the detector                                       | Broadens admission for unrelated values.                            |
| Select by path, identity, namespace, source, or operation | These selectors are coarse and may be caller-influenced.            |
| Put content classification in the authorization Gate      | That boundary does not own scanner canonicalization or field scope. |
| Add an alternate write operation                          | Creates a second write path with different safety semantics.        |
| Discover admissible values at runtime                     | Makes enrollment depend on mutable execution state.                 |

## Consequences

### Positive

- Reviewed synthetic false positives can be stored without weakening a detector category.
- Exact scope, durable posture, and audit make every classification inspectable.
- The public fixture makes canonicalization and failure behavior reproducible.

### Negative

- Every new fixture value requires an intentional manifest revision.
- The shared write-finalization boundary must cover every property-carrying write path.
- A manifest outage may cause reviewed fixtures to be blocked by the ordinary scanner.
- Installations must protect the manifest and database or disable this feature.

## References

- [ADR-014](./ADR-014-curation-operations.md): public curation writes
- [ADR-015](./ADR-015-schema-migrations.md): durable schema evolution
- [ADR-018](./ADR-018-authorization-gate.md): authorization boundary
