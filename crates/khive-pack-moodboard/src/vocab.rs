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

/// Agent-visible Moodboard v1 verbs.
pub static MOODBOARD_HANDLERS: [HandlerDef; 3] = [
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
];
