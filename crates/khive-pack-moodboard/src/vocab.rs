//! Static vocabulary and verb metadata for the Moodboard pack.

use khive_types::{EntityKind, EntityTypeDef, HandlerDef, ParamDef, VerbCategory, Visibility};

/// The pack adds artifact subtypes, not a new base entity kind.
pub const ENTITY_KINDS: &[&str] = &[];

/// Moodboard v1 contributes no note kind.
pub const NOTE_KINDS: &[&str] = &[];

/// Additive artifact subtypes used by visual assets and later collection/model entities.
pub static MOODBOARD_ENTITY_TYPES: [EntityTypeDef; 3] = [
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "visual_asset",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "moodboard",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "moodboard_model",
        aliases: &[],
    },
];

/// Agent-visible Moodboard visual and preference-learning verbs.
pub static MOODBOARD_HANDLERS: [HandlerDef; 7] = [
    HandlerDef {
        name: "moodboard.model",
        description: "Discover and verify the configured Lattice checkpoint identity without \
                      constructing its weights, then return the immutable experimental \
                      descriptor space used by ingest and search.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "moodboard.ingest",
        description: "Validate one base64 PNG/JPEG/WebP, publish its original bytes to BlobStore, \
                      attach or reuse a visual_asset entity, infer a governed experimental Lattice \
                      descriptor, and insert it into the identity-bound exact visual store.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "image_base64",
                param_type: "string",
                required: true,
                description: "Base64-encoded PNG, JPEG, or WebP bytes (64 MiB decoded maximum).",
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: false,
                description: "Optional non-empty asset display name.",
            },
            ParamDef {
                name: "media_type",
                param_type: "string",
                required: false,
                description: "Optional exact image/png, image/jpeg, or image/webp declaration; \
                              detected bytes must match.",
            },
            ParamDef {
                name: "caption",
                param_type: "string",
                required: false,
                description: "Optional non-empty human caption stored as the entity description; \
                              it does not condition v1 visual pooling.",
            },
        ],
    },
    HandlerDef {
        name: "moodboard.search",
        description:
            "Re-derive one visual_asset descriptor from canonical BlobStore bytes and \
                      return exact cosine nearest neighbors in the same immutable descriptor space.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "asset_id",
                param_type: "string",
                required: true,
                description: "Bare canonical UUID of a live visual_asset entity.",
            },
            ParamDef {
                name: "top_k",
                param_type: "integer",
                required: false,
                description: "Number of non-self hits to return (default 20, maximum 100).",
            },
        ],
    },
    HandlerDef {
        name: "moodboard.serve",
        description: "Validate two scored visual-asset occurrences in one immutable board and \
                      descriptor scope, randomize displayed sides, and persist an explicit \
                      actor-attributed serve event with durable occurrence IDs.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "board_entity_id",
                param_type: "string",
                required: true,
                description: "Canonical UUID of the artifact/moodboard entity.",
            },
            ParamDef {
                name: "board_id",
                param_type: "string",
                required: true,
                description: "Immutable 64-lowercase-hex board fingerprint stored by that entity.",
            },
            ParamDef {
                name: "descriptor",
                param_type: "object",
                required: true,
                description: "Closed {model_key, descriptor_fingerprint} visual descriptor identity.",
            },
            ParamDef {
                name: "feature_schema_id",
                param_type: "string",
                required: false,
                description: "Optional exact installed preference-feature schema fence.",
            },
            ParamDef {
                name: "source_report_sha256",
                param_type: "string",
                required: true,
                description: "SHA-256 of the upstream scored report from which candidates came.",
            },
            ParamDef {
                name: "candidates",
                param_type: "array",
                required: true,
                description: "Exactly two scored asset/content occurrences with the frozen ten features.",
            },
            ParamDef {
                name: "selection",
                param_type: "object",
                required: true,
                description: "Pair-selection policy, optional propensity, and candidate-pool digest.",
            },
            ParamDef {
                name: "exposure",
                param_type: "object",
                required: false,
                description: "Display-exposure provenance; defaults to no ranks or learned probability shown.",
            },
        ],
    },
    HandlerDef {
        name: "moodboard.judge",
        description: "Append one immutable left/right/tie/abstain judgment for an exact serve and \
                      displayed result-occurrence pair; exact retries are idempotent and conflicts fail.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "serve_id",
                param_type: "string",
                required: true,
                description: "Canonical UUID returned by moodboard.serve.",
            },
            ParamDef {
                name: "left_result_occurrence_id",
                param_type: "string",
                required: true,
                description: "Exact displayed-left occurrence UUID.",
            },
            ParamDef {
                name: "right_result_occurrence_id",
                param_type: "string",
                required: true,
                description: "Exact displayed-right occurrence UUID.",
            },
            ParamDef {
                name: "choice",
                param_type: "string",
                required: true,
                description: "One of left, right, tie, or abstain.",
            },
            ParamDef {
                name: "reason_code",
                param_type: "string",
                required: false,
                description: "Closed, choice-compatible reason code; required for abstain.",
            },
            ParamDef {
                name: "response_ms",
                param_type: "integer",
                required: false,
                description: "Optional response latency in milliseconds, bounded to one hour.",
            },
        ],
    },
    HandlerDef {
        name: "moodboard.train_preference",
        description: "Snapshot actor-scoped judgments, group unordered pairs into deterministic \
                      70/15/15 splits, fit deterministic float64 logistic BCE plus L2, calibrate \
                      temperature and a tie band, then persist a real 10->1 linear FANN head.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "board_entity_id",
                param_type: "string",
                required: true,
                description: "Canonical UUID of the training board artifact.",
            },
            ParamDef {
                name: "board_id",
                param_type: "string",
                required: true,
                description: "Immutable board fingerprint.",
            },
            ParamDef {
                name: "descriptor",
                param_type: "object",
                required: true,
                description: "Exact visual descriptor identity for this head.",
            },
            ParamDef {
                name: "feature_schema_id",
                param_type: "string",
                required: false,
                description: "Optional exact installed preference-feature schema fence.",
            },
        ],
    },
    HandlerDef {
        name: "moodboard.preference",
        description: "Load and verify one calibrated identity-bound FANN artifact and return learned \
                      conditional pairwise preference separately from conformal evidence.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "preference_model_id",
                param_type: "string",
                required: true,
                description: "Canonical UUID of a pack-provenanced moodboard_model artifact.",
            },
            ParamDef {
                name: "board_entity_id",
                param_type: "string",
                required: true,
                description: "Canonical UUID of the matching board artifact.",
            },
            ParamDef {
                name: "board_id",
                param_type: "string",
                required: true,
                description: "Immutable matching board fingerprint.",
            },
            ParamDef {
                name: "descriptor",
                param_type: "object",
                required: true,
                description: "Exact matching visual descriptor identity.",
            },
            ParamDef {
                name: "feature_schema_id",
                param_type: "string",
                required: true,
                description: "Exact installed preference-feature schema identity.",
            },
            ParamDef {
                name: "source_report_sha256",
                param_type: "string",
                required: true,
                description: "SHA-256 of the upstream report that supplied both feature vectors.",
            },
            ParamDef {
                name: "left",
                param_type: "object",
                required: true,
                description: "Scored left asset/content identity and ten-feature vector.",
            },
            ParamDef {
                name: "right",
                param_type: "object",
                required: true,
                description: "Scored right asset/content identity and ten-feature vector.",
            },
        ],
    },
];
