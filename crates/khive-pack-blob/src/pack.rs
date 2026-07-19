//! Handler table, inventory registration, and runtime dispatch for the blob pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, ParamDef, Visibility};

use crate::{handlers, BlobPack, PACK_NAME};

pub(crate) static BLOB_HANDLERS: [HandlerDef; 3] = [
    HandlerDef {
        name: "blob.put",
        description: "Store bytes (base64) in the content-addressed \
                       blob store; returns the BLAKE3 ContentRef. Idempotent — identical content \
                       returns the same ref without a re-write.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[ParamDef {
            name: "bytes",
            param_type: "string",
            required: true,
            description: "Base64-encoded object content.",
        }],
    },
    HandlerDef {
        name: "blob.get",
        description: "Read an object back by ContentRef, base64-encoded in the response. \
                       Optionally slice a byte range of the object.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "content_ref",
                param_type: "string",
                required: true,
                description: "64-char lowercase-hex BLAKE3 content reference returned by blob.put.",
            },
            ParamDef {
                name: "range",
                param_type: "object",
                required: false,
                description: "Optional { offset, length } byte range, applied to the fetched \
                               object (the store has no partial-read capability).",
            },
        ],
    },
    HandlerDef {
        name: "blob.stat",
        description: "Report whether an object exists and its size, without hydrating its bytes \
                       or implying any lease or reservation. Digest verification happens on the \
                       blob.get read path, where the bytes are already fetched.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[ParamDef {
            name: "content_ref",
            param_type: "string",
            required: true,
            description: "64-char lowercase-hex BLAKE3 content reference returned by blob.put.",
        }],
    },
];

struct BlobPackFactory;

impl khive_runtime::PackFactory for BlobPackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(BlobPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&BlobPackFactory) }

#[async_trait]
impl PackRuntime for BlobPack {
    fn name(&self) -> &str {
        <BlobPack as khive_types::Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <BlobPack as khive_types::Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <BlobPack as khive_types::Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &BLOB_HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        <BlobPack as khive_types::Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "blob.put" => handlers::handle_put(self.runtime(), token, params).await,
            "blob.get" => handlers::handle_get(self.runtime(), token, params).await,
            "blob.stat" => handlers::handle_stat(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
