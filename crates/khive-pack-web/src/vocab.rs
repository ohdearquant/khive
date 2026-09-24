use std::collections::BTreeMap;

use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, IdResolutionMode, ParamDef,
    VerbCategory, Visibility,
};
use serde_json::Value;
use uuid::Uuid;

trait WebParamType {
    const NAME: &'static str;
}

macro_rules! web_param_types {
    ($($ty:ty => $name:literal),+ $(,)?) => {
        $(impl WebParamType for $ty {
            const NAME: &'static str = $name;
        })+
    };
}

web_param_types! {
    String => "string",
    Uuid => "uuid",
    bool => "boolean",
    u32 => "integer",
    u64 => "integer",
    BTreeMap<String, String> => "object",
    Vec<String> => "array of string",
    // Ingest keeps its existing handler-level source validation and refusals.
    Value => "string | array<string>",
}

impl<T: WebParamType> WebParamType for Option<T> {
    const NAME: &'static str = T::NAME;
}

macro_rules! web_param_required {
    () => {
        true
    };
    (default) => {
        false
    };
}

// Published parameter order and serde's sequence order predate this shared
// definition and differ for fetch/search. Positions preserve both contracts.
const fn ordered_params<const N: usize>(fields: [(usize, ParamDef); N]) -> [ParamDef; N] {
    let mut result = [fields[0].1; N];
    let mut seen = [false; N];
    let mut index = 0;
    while index < N {
        let (position, param) = fields[index];
        assert!(position < N, "web parameter position out of range");
        assert!(!seen[position], "duplicate web parameter position");
        seen[position] = true;
        result[position] = param;
        index += 1;
    }
    result
}

macro_rules! web_verbs {
    ($(
        $params:ident($verb:literal, $description:literal) {
            $(
                $(#[serde($default:ident)])?
                $field:ident: $ty:ty => ($position:literal, $help:literal, $resolution:ident);
            )+
        }
    )+) => {
        $(
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            pub(crate) struct $params {
                $(
                    $(#[serde($default)])?
                    pub(crate) $field: $ty,
                )+
            }

            impl $params {
                const DESCRIPTION: &'static str = $description;
                const PARAMS: &'static [ParamDef] = &ordered_params([$(($position, ParamDef {
                    name: stringify!($field),
                    param_type: <$ty as WebParamType>::NAME,
                    required: web_param_required!($($default)?),
                    description: $help,
                    resolution_mode: IdResolutionMode::$resolution,
                })),+]);
            }
        )+

        #[cfg(test)]
        const WEB_PARAM_DECODERS: [(&str, ParamDecoder); [$(stringify!($params)),+].len()] = [
            $(($verb, decode_params::<$params>)),+
        ];
    };
}

#[cfg(test)]
type ParamDecoder = fn(Value) -> Result<(), serde_json::Error>;

#[cfg(test)]
fn decode_params<T: serde::de::DeserializeOwned>(value: Value) -> Result<(), serde_json::Error> {
    serde_json::from_value::<T>(value).map(|_| ())
}

web_verbs! {
    FetchParams("web.fetch", "Fetch one URL over HTTP(S) under egress policy (address-class, \
                      allowlist, credential and header controls). Mints/updates the site \
                      and page/resource entities, stores the body as a blob, and writes an \
                      observation receipt.") {
        url: String => (0, "The URL to fetch. http and https only.", NotApplicable);
        #[serde(default)]
        accept: Option<String> => (1, "Value for the Accept request header.", NotApplicable);
        #[serde(default)]
        method: Option<String> => (5, "Defaults to GET. GET and HEAD are the only permitted methods.", NotApplicable);
        #[serde(default)]
        headers: BTreeMap<String, String> => (6, "Request headers to send, restricted to the allowed request \
                              header set (accept, accept-language, if-none-match, \
                              if-modified-since, user-agent).", NotApplicable);
        #[serde(default)]
        credential: Option<String> => (7, "Named [[web.credentials]] entry to send as an \
                              Authorization: Bearer header. Requires https at every hop, \
                              and only on a host in the credential's own configured set.", NotApplicable);
        #[serde(default)]
        persist: Option<bool> => (2, "Defaults to true. False stores no body or entities, returns the body \
                              as a standard padded base64 string, and writes a receipt with final URL, content \
                              digest, size and fetch time. HEAD returns no body. Transient GET requires effective max_bytes \
                              at most 6288384 before network access; oversized body or header metadata is refused before a receipt.", NotApplicable);
        #[serde(default)]
        max_bytes: Option<u64> => (3, "Caller-supplied byte ceiling; may only lower the operator's \
                              configured maximum, never raise it. With persist=false, GET accepts at most 6288384 raw bytes; \
                              lower this value or use persist=true for larger bodies. HEAD is exempt from this inline limit.", NotApplicable);
        #[serde(default)]
        timeout_s: Option<u64> => (4, "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.", NotApplicable);
        #[serde(default)]
        namespace: Option<String> => (8, "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.", NotApplicable);
    }
    ExtractParams("web.extract", "Parse an already-fetched body into links, sitemap/feed entries, and/or \
                      plain text. Never fetches — refuses `not_fetched` on a document with no \
                      stored body.") {
        #[serde(default)]
        id: Option<Uuid> => (0, "The document entity to extract from. Exactly one of id/url.", UnscopedById);
        #[serde(default)]
        url: Option<String> => (1, "The document's URL, resolved to its entity id. Exactly one of id/url.", NotApplicable);
        #[serde(default)]
        kinds: Option<Vec<String>> => (2, "Subset of [\"text\", \"links\", \"sitemap\", \"feed\"]; default \
                              all applicable to the stored content-type.", NotApplicable);
        #[serde(default)]
        namespace: Option<String> => (3, "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.", NotApplicable);
    }
    IngestParams("web.ingest", "Fetch and extract over a URL, a list of URLs, or (with origin) a served \
                      tree on disk. depth bounds link-following beyond the seed URLs.") {
        source: Value => (0, "A URL, an array of URLs, or (with origin) a directory path.", NotApplicable);
        #[serde(default)]
        origin: Option<String> => (1, "Required when source is a directory path: the URL this tree is \
                              served as, supplying the site identity for every file in it.", NotApplicable);
        #[serde(default)]
        depth: Option<u32> => (2, "How many hops of discovered links to follow beyond the seed \
                              URLs. Defaults to 0 (seeds only).", NotApplicable);
        #[serde(default)]
        limit: Option<u32> => (3, "Maximum number of documents to ingest in one call. For a disk source, this bounds file reads and ingestion, not discovery: the entire tree is inspected and each visited directory is sorted, even at zero. Discovery cost scales with tree size; use a smaller source directory to bound it.", NotApplicable);
        #[serde(default)]
        namespace: Option<String> => (4, "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.", NotApplicable);
    }
    SearchParams("web.search", "Query a configured search provider (a fixture or an HTTP provider) and \
                      write a receipt recording the query, provider, and exact ordered result \
                      set.") {
        query: String => (0, "The search query text.", NotApplicable);
        #[serde(default)]
        limit: Option<u32> => (2, "Caller-supplied result-count ceiling; may only lower the \
                              operator's configured maximum.", NotApplicable);
        #[serde(default)]
        provider: Option<String> => (1, "Named [[web.search_providers]] entry; defaults to the \
                              operator's default provider, or the sole configured one.", NotApplicable);
        #[serde(default)]
        persist: Option<bool> => (3, "Defaults to false. True mints each hit's URL as an unfetched \
                              resource under its site.", NotApplicable);
        #[serde(default)]
        max_bytes: Option<u64> => (4, "Caller-supplied byte ceiling on the provider response; may \
                              only lower the operator's configured maximum, never raise it.", NotApplicable);
        #[serde(default)]
        timeout_s: Option<u64> => (5, "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.", NotApplicable);
        #[serde(default)]
        namespace: Option<String> => (6, "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.", NotApplicable);
    }
    RefreshParams("web.refresh", "Conditionally re-fetch a previously fetched document using its stored \
                      etag/last_modified. An unchanged body writes a receipt only; a changed \
                      body updates the stored blob and properties. Every refresh's receipt \
                      supersedes the previous one for the same document.") {
        id: Uuid => (0, "The document entity to refresh.", UnscopedById);
        #[serde(default)]
        max_bytes: Option<u64> => (1, "Caller-supplied byte ceiling; may only lower the operator's \
                              configured maximum, never raise it.", NotApplicable);
        #[serde(default)]
        timeout_s: Option<u64> => (2, "Caller-supplied time ceiling in seconds; may only lower the \
                              operator's configured maximum, never raise it.", NotApplicable);
        #[serde(default)]
        namespace: Option<String> => (3, "Narrows the write to a namespace; must equal the caller's own \
                              authorized token namespace, never elevates capability.", NotApplicable);
    }
}

// Keep verb identities and admission categories explicit for the source census;
// each parameter list is still generated from its typed field definition above.
pub(crate) static WEB_HANDLERS: [HandlerDef; 5] = [
    HandlerDef {
        name: "web.fetch",
        description: FetchParams::DESCRIPTION,
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: FetchParams::PARAMS,
    },
    HandlerDef {
        name: "web.extract",
        description: ExtractParams::DESCRIPTION,
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: ExtractParams::PARAMS,
    },
    HandlerDef {
        name: "web.ingest",
        description: IngestParams::DESCRIPTION,
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: IngestParams::PARAMS,
    },
    HandlerDef {
        name: "web.search",
        description: SearchParams::DESCRIPTION,
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: SearchParams::PARAMS,
    },
    HandlerDef {
        name: "web.refresh",
        description: RefreshParams::DESCRIPTION,
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: RefreshParams::PARAMS,
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

    // Explicitly guard the manual handler order against the generated decoders
    // before pairing them, so missing or reordered rows cannot escape coverage.
    #[test]
    fn every_declared_param_name_round_trips_an_undeclared_name_refuses() {
        assert_eq!(WEB_HANDLERS.len(), WEB_PARAM_DECODERS.len());
        for (handler, (verb, decode)) in WEB_HANDLERS.iter().zip(WEB_PARAM_DECODERS) {
            assert_eq!(handler.name, verb, "handler and decoder order must match");
            let mut object = serde_json::Map::new();
            for param in handler.params {
                object.insert(param.name.to_string(), fixture_for(param.param_type));
            }
            let value = Value::Object(object.clone());
            assert!(
                decode(value).is_ok(),
                "{}: every declared param name must deserialize through the params struct",
                handler.name
            );

            let mut with_unknown = object;
            with_unknown.insert("definitely_not_a_declared_param".to_string(), json!(true));
            assert!(
                decode(Value::Object(with_unknown)).is_err(),
                "{}: an undeclared param name must be refused",
                handler.name
            );
        }
    }

    #[test]
    fn params_preserve_omitted_field_defaults() {
        let fetch: FetchParams = serde_json::from_value(json!({"url": "https://example.test/"}))
            .expect("fetch optional fields may be omitted");
        assert_eq!(fetch.accept, None);
        assert_eq!(fetch.method, None);
        assert!(fetch.headers.is_empty());
        assert_eq!(fetch.credential, None);
        assert_eq!(fetch.persist, None);
        assert_eq!(fetch.max_bytes, None);
        assert_eq!(fetch.timeout_s, None);
        assert_eq!(fetch.namespace, None);

        let extract: ExtractParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(extract.id, None);
        assert_eq!(extract.url, None);
        assert_eq!(extract.kinds, None);
        assert_eq!(extract.namespace, None);

        let ingest: IngestParams =
            serde_json::from_value(json!({"source": ["https://example.test/"]})).unwrap();
        assert_eq!(ingest.origin, None);
        assert_eq!(ingest.depth, None);
        assert_eq!(ingest.limit, None);
        assert_eq!(ingest.namespace, None);

        let search: SearchParams = serde_json::from_value(json!({"query": "test"})).unwrap();
        assert_eq!(search.provider, None);
        assert_eq!(search.limit, None);
        assert_eq!(search.persist, None);
        assert_eq!(search.max_bytes, None);
        assert_eq!(search.timeout_s, None);
        assert_eq!(search.namespace, None);

        let refresh: RefreshParams = serde_json::from_value(json!({"id": Uuid::nil()})).unwrap();
        assert_eq!(refresh.max_bytes, None);
        assert_eq!(refresh.timeout_s, None);
        assert_eq!(refresh.namespace, None);
    }

    #[test]
    fn params_preserve_typed_refusals() {
        for value in [
            json!({}),
            json!({"url": 1}),
            json!({"url": "https://example.test/", "headers": null}),
            json!({"url": "https://example.test/", "headers": {"accept": true}}),
            json!({"url": "https://example.test/", "persist": "true"}),
            json!({"url": "https://example.test/", "max_bytes": -1}),
        ] {
            assert!(serde_json::from_value::<FetchParams>(value).is_err());
        }
        assert!(serde_json::from_str::<FetchParams>(r#"{"url":"a","url":"b"}"#).is_err());
        assert!(serde_json::from_value::<ExtractParams>(json!({"id": "bad-id"})).is_err());
        assert!(serde_json::from_value::<ExtractParams>(json!({"kinds": [1]})).is_err());
        assert!(serde_json::from_value::<IngestParams>(json!({
            "source": "https://example.test/", "depth": u64::from(u32::MAX) + 1
        }))
        .is_err());
        assert!(
            serde_json::from_value::<SearchParams>(json!({"query": "q", "limit": -1})).is_err()
        );
        assert!(serde_json::from_value::<RefreshParams>(json!({"id": "bad-id"})).is_err());
    }

    #[test]
    fn params_preserve_sequence_and_published_order() {
        let fetch: FetchParams = serde_json::from_value(json!([
            "https://example.test/", "text/plain", "HEAD", {"accept-language": "en"},
            "credential", false, 123, 4, "local"
        ]))
        .unwrap();
        assert_eq!(fetch.method.as_deref(), Some("HEAD"));
        assert_eq!(
            fetch.headers.get("accept-language").map(String::as_str),
            Some("en")
        );
        assert_eq!(fetch.credential.as_deref(), Some("credential"));
        assert_eq!(fetch.persist, Some(false));
        assert_eq!(fetch.max_bytes, Some(123));

        let search: SearchParams =
            serde_json::from_value(json!(["query", 3, "provider", true, 321, 5, "local"])).unwrap();
        assert_eq!(search.limit, Some(3));
        assert_eq!(search.provider.as_deref(), Some("provider"));
        assert_eq!(search.persist, Some(true));
        assert_eq!(search.max_bytes, Some(321));

        assert_eq!(WEB_HANDLERS[0].params[2].name, "persist");
        assert_eq!(WEB_HANDLERS[3].params[1].name, "provider");
    }
}
