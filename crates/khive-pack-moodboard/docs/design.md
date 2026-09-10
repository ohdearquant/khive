# khive-pack-moodboard Design

## Purpose

`khive-pack-moodboard` is an opt-in experimental pack for raster ingest, exact visual retrieval,
and actor-scoped pairwise preference learning over Khive substrates. It combines BlobStore content,
graph entities, identity-bound vector tables, a governed Lattice vision checkpoint, and durable
preference provenance. It does not register a text embedding provider.

## Storage and identity model

- Original image bytes live in the configured `BlobStore` and are identified by `ContentRef`.
- Graph identity and metadata live in artifact entities. The pack registers `visual_asset`,
  `moodboard`, and `moodboard_model` as artifact subtypes; it adds no base entity or note kind.
- Visual descriptors live in named vector tables whose identity includes the checkpoint,
  preprocessing, prompt, and descriptor fingerprints. In multi-backend setups, graph entities use
  the shared core store while vectors stay on the pack-selected runtime.
- Immutable serve, judgment, and model publication events retain actor, board, descriptor, feature,
  source-report, pair-selection, and model-artifact provenance.

## Key types and modules

- `MoodboardPack` binds one `KhiveRuntime` and one lazily initialized `VisionModelState`; the pack
  requires the `kg` pack.
- `model.rs` verifies checkpoint identity, controls model loading/inference concurrency, and defines
  immutable descriptor identities.
- `preprocess.rs` validates and normalizes PNG, JPEG, and WebP rasters into the governed inference
  rendition.
- `handlers.rs` implements `moodboard.model`, `moodboard.ingest`, and `moodboard.search`.
- `preference.rs` defines the fixed ten-feature schema, deterministic pairwise fitting,
  calibration, split policy, and FANN model bundle.
- `preference_handlers.rs` implements `moodboard.serve`, `moodboard.judge`,
  `moodboard.train_preference`, and `moodboard.preference`.
- `preference_artifact.rs` authenticates persisted model bundles, FANN network attachments, and
  immutable publication evidence, including the legacy attachment cutover checks.

## Invariants

- Raster admission fails closed before expensive allocation. Decoded input is limited to supported
  formats, non-zero dimensions, an 8192-pixel source side, a 256 MiB post-decode working budget,
  and a 64 MiB encoded object ceiling at the verb boundary.
- Preprocessing is deterministic: alpha is composited onto the fixed RGB `[128,128,128]` matte,
  images are downscaled without upscaling to a maximum side of 448, dimensions are padded to a
  multiple of 32, and the inference rendition is encoded as PNG.
- Ingest stores original bytes, reuses content identity safely under striped locks, and writes the
  descriptor only into its exact immutable vector space. Search re-derives the query descriptor
  from canonical BlobStore bytes and compares it only within that space.
- Descriptor embeddings must have the configured dimension, contain only finite values, and have a
  valid norm. Checkpoint files and identities are verified before weights are served.
- Preference features are a closed, ordered ten-value schema in `[0,1]`; its canonical bytes and
  hash are identity-bearing. A preference model is fenced to one namespace, actor, board,
  descriptor space, and feature schema.
- A serve records the exact two result occurrences and deterministic side-randomization provenance.
  A judgment names those displayed occurrences; exact retries are idempotent, while conflicting
  retries fail.
- Training snapshots actor-scoped evidence, keeps unordered pairs in deterministic 70/15/15
  groups, fits a deterministic zero-intercept logistic objective, calibrates temperature and a tie
  band, and persists a hashed FANN network plus authenticated bundle. Serving re-verifies both
  artifacts before inference.
