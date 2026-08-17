# Embedding-space identity

`EmbeddingSpaceIdentity` is the backend-neutral, immutable fence for one
physical vector space. It is a value contract, not a storage capability and
not a generic canonical-JSON library.

## Construction

```rust
use khive_storage::EmbeddingSpaceIdentity;

let identity = EmbeddingSpaceIdentity::new(
    "moodboard",
    "moodboard.visual-descriptor.v1",
    [0xab; 32],
    "qwen3.5-vlm-pooled-visual",
    1024,
)?;

assert_eq!(
    identity.space_key().as_str(),
    "moodboard_abababababababababababababababababababababababababababababababab_1024"
);
# Ok::<(), khive_storage::EmbeddingSpaceIdentityError>(())
```

The constructor validates:

- a non-empty ASCII-alphanumeric/underscore key prefix;
- a 1–128-byte protocol from `[A-Za-z0-9._-]`;
- a 32-byte owner-supplied fingerprint;
- a non-empty model label of at most 512 bytes with no surrounding whitespace; and
- dimensions in `1..=8192`.

It derives the only physical key as
`{prefix}_{lowercase_hex(fingerprint)}_{dimensions}` and rejects a result over
128 bytes. `EmbeddingSpaceKey` has no public unchecked constructor or
deserializer, so callers cannot supply a table key independently from the
fingerprint and geometry.

## Owner responsibility

The model or protocol owner defines and golden-tests the fingerprint preimage.
Every input that can change emitted vectors belongs in that document, including
the protocol identifier itself. The shared type deliberately does not guess
whether a checkpoint, tokenizer, prompt, transform, pooling rule, provider
revision, adapter, or normalization mode matters, and it does not prepend or
hash the protocol a second time.

The model label is descriptive metadata. It does not select storage. Namespace
is likewise excluded from the identity: namespace remains a row/query scope
inside the same physical space.

## Runtime binding

Pack-owned consumers pass the complete value to
`KhiveRuntime::vectors_for_embedding_space`. The returned `VectorStore` handle
is bound to the derived table and uses the token namespace as its default.
Individual operations remain responsible for carrying only an authorized write
namespace or visible read scope. Runtime verifies the existing table geometry
and stored model metadata before allowing use.

ADR-160 Phase 6 introduces this value and migrates moodboard without changing
its canonical descriptor bytes or table key. Text-provider registration,
registry lineage, vector-row columns, ANN logs, caches, snapshots, rebuild, and
atomic source cutover remain one coordinated Phase 7 change; Phase 6 does not
partially widen those identities.
