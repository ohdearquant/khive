//! ADR-139 registered-map read contracts.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

pub(crate) const LIST: HandlerDef = HandlerDef {
    name: "code.list",
    description: "List live code entities in a registered map, ordered by name then id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        ParamDef {
            name: "map",
            param_type: "string",
            required: true,
            description: "Registered map name; configured filesystem paths are never accepted.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kind",
            param_type: "string",
            required: false,
            description: "Filter: project, module, function, datatype, or interface.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "project_id",
            param_type: "string",
            required: false,
            description: "Containing project identifier within the selected map.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "cursor",
            param_type: "string",
            required: false,
            description: "Opaque continuation bound to map registration and all result filters.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Page size: 1..=500, default 100.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
    ],
};

pub(crate) const SEARCH: HandlerDef = HandlerDef {
    name: "code.search",
    description:
        "Search literal entity names in a registered map; exact, prefix, then interior matches.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        ParamDef {
            name: "map",
            param_type: "string",
            required: true,
            description: "Registered map name; configured filesystem paths are never accepted.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "query",
            param_type: "string",
            required: true,
            description:
                "Non-empty literal, case-insensitive entity name match; no operator grammar.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kind",
            param_type: "string",
            required: false,
            description: "Filter: project, module, function, datatype, or interface.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "project_id",
            param_type: "string",
            required: false,
            description: "Containing project identifier within the selected map.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "cursor",
            param_type: "string",
            required: false,
            description: "Opaque continuation bound to map registration and all result filters.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Page size: 1..=200, default 20.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
    ],
};

pub(crate) const DEPS: HandlerDef = HandlerDef {
    name: "code.deps",
    description: "Traverse live map-local depends_on edges breadth-first, ordered by depth then id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        ParamDef { name: "map", param_type: "string", required: true, description: "Registered map name; configured filesystem paths are never accepted.", resolution_mode: IdResolutionMode::NotApplicable },
        ParamDef { name: "id", param_type: "string", required: true, description: "Root entity identifier within the selected map.", resolution_mode: IdResolutionMode::NotApplicable },
        ParamDef { name: "direction", param_type: "string", required: false, description: "outgoing, incoming, or both (default both).", resolution_mode: IdResolutionMode::NotApplicable },
        ParamDef { name: "max_depth", param_type: "integer", required: false, description: "Maximum dependency traversal depth: 1..=10, default 2.", resolution_mode: IdResolutionMode::NotApplicable },
        ParamDef { name: "limit", param_type: "integer", required: false, description: "Total nodes including root: 1..=1000, default 200; truncated reports omitted reachable nodes.", resolution_mode: IdResolutionMode::NotApplicable },
    ],
};
