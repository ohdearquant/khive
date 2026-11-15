# khive-capability

Process-local sealed grants, attenuating delegation and authenticated durable
envelopes over `khive-lion-core`.

This crate is ported from `khive-capability` version 0.6.0 at commit
`341cd4dced8727fa983b20fd3266138d99cf109d`. It now builds on `khive-lion-core`,
the workspace copy of `lion-core` 0.4.0, and is licensed Apache-2.0.

## Library surface

Public re-exports include `khive-lion-core` types, so their API follows the core crate's major version.

- `ActorRegistry` retains session-to-actor bookkeeping.
- `NamespaceMap` retains the deterministic BLAKE3 namespace-to-resource mapping.
- `RequestContext` takes a validated `khive_types::Namespace` and carries session/task scope.
- `CapabilityManager::with_key` takes a caller-supplied random 32-byte process-local key
  and refuses an all-zero key. It does not generate the seal key or fall back to a default key.
- `mint` returns a sealed grant with the requested nonempty rights for a registered actor.
  `bootstrap_root_capability` is a compatibility alias for this sealed mint.
- `compose` checks the held parent's seal and full ancestor liveness, then lets the
  kernel intersect requested rights with the parent's rights. An empty intersection fails.
- `get_cap` returns canonical public grant records, including revoked ones;
  `is_live` checks the stored grant and every ancestor through a known root anchor.
- `validate` combines held canonical identity, seal, complete ancestor liveness,
  namespace and required-right checks. `revoke` invalidates a grant and its descendants.
- `RequestValidator` retains the exhaustive finer operation-to-right classification
  and its separate Read/Write binding projection.
- `CapId`, `Capability`, `PluginId`, `Right`, `Rights` and `SecurityLevel` are re-exported
  from `khive-lion-core`, so callers name the kernel types through this crate without a
  direct `khive-lion-core` dependency.

The manager is a trusted authority-management object. Its mint, compose and revoke
methods do not identify or authorize an external caller. A future caller-facing
adapter must enforce that policy before invoking them. This crate does not install
a runtime `Gate`, create a persistent grant store or mint grants at boot. The
process-local manager does not persist keys or establish cross-process authority. `khive-gate` remains the separate authorization-decision interface;
its coarse `OperationAccess` is not substituted for this crate's finer rights table.

## Seals and roots

The core exposes sealed delegation but no direct parentless-root sealing API. Each
mint therefore creates one private raw anchor with that mint's namespace and rights,
then returns the kernel-sealed child. Anchors are never returned by `get_cap` or
accepted as public grants by validation, composition or revocation. A grant's signed
parent reference may name its anchor; that reference does not expose anchor authority.
Public grant IDs occupy the low half of the ID space. Anchors descend from
`u128::MAX - 1` in the high half; the sentinel `u128::MAX` is excluded and exhaustion
is refused before the ranges can overlap. Independent mints have independent anchors.

`verify_seal` is a pure cryptographic predicate. The core seal authenticates holder,
namespace resource, rights, parent and epoch; it does **not** authenticate the ID or
mutable revocation flag. Successful verification alone does not prove table membership,
holder authority or liveness. Authority checks use the manager's canonical stored
records; caller-supplied records are not inserted through this API.

The ancestor walk is iterative and bounded by the number of stored capabilities,
refusing missing nodes, cycles and unknown roots. It does not assume parent IDs are
smaller than child IDs. A liveness check is a point-in-time observation: a consumer
must keep its own synchronization through a later protected write if revocation can
race that write. No new transaction fence or persistence guarantee is implied.

## Durable envelope v1

The `durable` module owns the closed `RecordObservation` / `GrantAdmin` model,
its provisioned contract catalog, canonical bytes and cryptographic verification.
Ordinary namespace rights, internal producers and claim-time platform bundles
receive named unsupported-profile refusals. The bundle refusal is an M1 limit.
Actor reporting entries bind full contracts; platform relays bind a separate exact
response-envelope contract. No request deserializes a catalog or key registry.

`EnvelopeIssuer` is a trusted composition helper. `issue_from_bootstrap` requires
a finite provisioned source; `delegate` authenticates its parent, checks structural
holder identity and complete typed scope/interval attenuation, then calls the
native delegation API. It does not authorize the caller or inspect a durable
store. The future service must check current administration, terminal state, every
UUID ancestor and the initialized live root in the same writer transaction before
using this helper. `EnvelopeVerifier` and `AuthenticatedEnvelope` attest immutable
bytes only. None is an effective-authority verdict or ActorSignature proof.

Each issuance discards a private keyed scratch kernel. Native outputs occupy
`[0,2^126)`, verified parent aliases `[2^126,2^127)`, and private bootstrap labels
the upper half excluding `u128::MAX`. The alias is selected from the issuance UUID
inside the helper; low-bit collisions are harmless across private contexts. The
full context UUID, original parent UUID/context/native digest and alias mapping
are authenticated. No alias, key bytes or mutable kernel is exposed as authority.

A composition root supplies two distinct nonzero 32-byte keys for each exact
`KeyRef { id, generation }`: native seal and extension HMAC-SHA256 material. The
crate neither generates them nor establishes their entropy. Missing material is
`Unavailable`; wrong ready material is `Reject`. Native epoch is fixed at zero,
separate from deployment key generation. The extension closes the native seal's
intentional omission of its own ID and binds the complete envelope, using the
library's constant-time MAC verification. This cooperative profile does not
detect rollback of the entire store or deletion of terminal history.

The internal wire schema uses explicit tags, big-endian fixed-width numbers,
16-byte UUIDs and checked u32 lengths. Contract digests have tag 1 (BLAKE3-256).
Set collections sort by encoded element bytes and reject duplicates; permission
vectors preserve order. Timestamps retain all u64 microseconds and intervals are
half-open: a grant expires exactly at `expires_at`. Records are at most 2 MiB;
the separate request constant is 1 MiB and each collection holds at most 256
items. Refusals report measured size and bound. Encoding measures before buffer
allocation; decoding checks the outer bound before reading variable-sized data.

The literal codec fixture is independently encoded expected-format data with a
synthetic native signature; it is not a captured old implementation receipt.
Native verification and real three-child-process reconstruction have separate
tests. Running these tests on the host is required before claiming native or
cold-process acceptance.
