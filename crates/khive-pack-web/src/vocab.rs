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
            ParamDef {
                name: "timeout_s",
                param_type: "integer",
                required: false,
                description: "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "method",
                param_type: "string",
                required: false,
                description: "Defaults to GET. GET and HEAD are the only permitted methods.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "headers",
                param_type: "object",
                required: false,
                description: "Request headers to send, restricted to the allowed request \
                              header set (accept, accept-language, if-none-match, \
                              if-modified-since, user-agent).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "credential",
                param_type: "string",
                required: false,
                description: "Named [[web.credentials]] entry to send as an \
                              Authorization: Bearer header. Requires https at every hop, \
                              and only on a host in the credential's own configured set.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.",
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
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.",
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
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.",
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
            ParamDef {
                name: "max_bytes",
                param_type: "integer",
                required: false,
                description: "Caller-supplied byte ceiling on the provider response; may \
                              only lower the operator's configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "timeout_s",
                param_type: "integer",
                required: false,
                description: "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.",
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
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "The document entity to refresh.",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            ParamDef {
                name: "max_bytes",
                param_type: "integer",
                required: false,
                description: "Caller-supplied byte ceiling; may only lower the operator's \
                              configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "timeout_s",
                param_type: "integer",
                required: false,
                description: "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use uuid::Uuid;

    /// A value that deserializes successfully for the given declared
    /// `param_type` — enough to prove the FIELD NAME is accepted by the
    /// verb's params struct, independent of whatever type/semantic
    /// validation the verb's own handler applies afterward.
    fn fixture_for(param_type: &str) -> Value {
        match param_type {
            "uuid" => json!(Uuid::new_v4().to_string()),
            "integer" => json!(1),
            "boolean" => json!(true),
            "object" => json!({}),
            "array of string" => json!(["a"]),
            "string | array<string>" => json!("https://example.test/"),
            _ => json!("x"),
        }
    }

    fn round_trips(handler: &HandlerDef, value: Value) -> Result<(), String> {
        match handler.name {
            "web.fetch" => serde_json::from_value::<crate::fetch::FetchParams>(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            "web.extract" => serde_json::from_value::<crate::extract::ExtractParams>(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            "web.ingest" => serde_json::from_value::<crate::ingest::IngestParams>(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            "web.search" => serde_json::from_value::<crate::search::SearchParams>(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            "web.refresh" => serde_json::from_value::<crate::refresh::RefreshParams>(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            other => panic!("unhandled web verb {other:?} — extend this test alongside it"),
        }
    }

    // The HandlerDef params list and the params struct must
    // be ONE list. Every name declared in a verb's ParamDef set must
    // deserialize through that verb's params struct (the drift `accept`
    // shipped with: documented and declared, refused as "unknown field" by
    // FetchParams); a name never declared must still be refused (the
    // control — every struct is `deny_unknown_fields`, so the reverse drift
    // this test would also catch is a struct field with no ParamDef row).
    #[test]
    fn every_declared_param_name_round_trips_an_undeclared_name_refuses() {
        for handler in WEB_HANDLERS.iter() {
            let mut object = serde_json::Map::new();
            for param in handler.params {
                object.insert(param.name.to_string(), fixture_for(param.param_type));
            }
            let value = Value::Object(object.clone());
            assert!(
                round_trips(handler, value).is_ok(),
                "{}: every declared param name must deserialize through the params struct",
                handler.name
            );

            let mut with_unknown = object;
            with_unknown.insert("definitely_not_a_declared_param".to_string(), json!(true));
            assert!(
                round_trips(handler, Value::Object(with_unknown)).is_err(),
                "{}: an undeclared param name must be refused",
                handler.name
            );
        }
    }
}
