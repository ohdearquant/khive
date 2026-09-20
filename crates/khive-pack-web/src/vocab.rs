use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, IdResolutionMode, ParamDef,
    VerbCategory, Visibility,
};

pub(crate) static WEB_HANDLERS: [HandlerDef; 5] = [
    HandlerDef {
        name: "web.fetch",
        description: "Fetch one URL over HTTP(S) under egress policy (address-class, \
                      allowlist, credential and header controls). Mints/updates the site \
                      and page/resource entities, stores the body as a blob, and writes an \
                      observation receipt.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "url",
                param_type: "string",
                required: true,
                description: "The URL to fetch. http and https only.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "accept",
                param_type: "string",
                required: false,
                description: "Value for the Accept request header.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "persist",
                param_type: "boolean",
                required: false,
                description: "Defaults to true. False fetches without minting entities or \
                              writing a receipt.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "max_bytes",
                param_type: "integer",
                required: false,
                description: "Caller-supplied byte ceiling; may only lower the operator's \
                              configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "web.extract",
        description: "Parse an already-fetched body into links, sitemap/feed entries, and/or \
                      plain text. Never fetches — refuses `not_fetched` on a document with no \
                      stored body.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: false,
                description: "The document entity to extract from. Exactly one of id/url.",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            ParamDef {
                name: "url",
                param_type: "string",
                required: false,
                description:
                    "The document's URL, resolved to its entity id. Exactly one of id/url.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kinds",
                param_type: "array of string",
                required: false,
                description: "Subset of [\"text\", \"links\", \"sitemap\", \"feed\"]; default \
                              all applicable to the stored content-type.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "web.ingest",
        description: "Fetch and extract over a URL, a list of URLs, or (with origin) a served \
                      tree on disk. depth bounds link-following beyond the seed URLs.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "source",
                param_type: "string | array<string>",
                required: true,
                description: "A URL, an array of URLs, or (with origin) a directory path.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "origin",
                param_type: "string",
                required: false,
                description: "Required when source is a directory path: the URL this tree is \
                              served as, supplying the site identity for every file in it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "depth",
                param_type: "integer",
                required: false,
                description: "How many hops of discovered links to follow beyond the seed \
                              URLs. Defaults to 0 (seeds only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum number of documents to ingest in one call.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "web.search",
        description: "Query a configured search provider (a fixture or an HTTP provider) and \
                      write a receipt recording the query, provider, and exact ordered result \
                      set.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "The search query text.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "provider",
                param_type: "string",
                required: false,
                description: "Named [[web.search_providers]] entry; defaults to the \
                              operator's default provider, or the sole configured one.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Caller-supplied result-count ceiling; may only lower the \
                              operator's configured maximum.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "persist",
                param_type: "boolean",
                required: false,
                description: "Defaults to false. True mints each hit's URL as an unfetched \
                              resource under its site.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "web.refresh",
        description: "Conditionally re-fetch a previously fetched document using its stored \
                      etag/last_modified. An unchanged body writes a receipt only; a changed \
                      body updates the stored blob and properties. Every refresh's receipt \
                      supersedes the previous one for the same document.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "id",
            param_type: "uuid",
            required: true,
            description: "The document entity to refresh.",
            resolution_mode: IdResolutionMode::UnscopedById,
        }],
    },
];

/// Exactly two rows (ADR-191 D2): everything else the pack's operations
/// produce — `page links_to page|resource`, `document derived_from
/// document`, `document supersedes document`, `note annotates *`, `note
/// supersedes note` — is legal under the BASE edge contract already,
/// needing no pack-declared row.
pub(crate) static WEB_EDGE_RULES: [EdgeEndpointRule; 2] = [
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "page",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "resource",
        },
    },
];
