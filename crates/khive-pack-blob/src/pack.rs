//! Handler table, inventory registration, and runtime dispatch for the blob pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, IdResolutionMode, ParamDef, Visibility};

use crate::{handlers, BlobPack, PACK_NAME};

pub(crate) static BLOB_HANDLERS: [HandlerDef; 7] = [
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "range",
                param_type: "object",
                required: false,
                description: "Optional { offset, length } byte range, applied to the fetched \
                               object (the store has no partial-read capability).",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "blob.begin",
        description: "Begin a sequential upload, or return an existing known content reference without staging.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "size",
                param_type: "integer",
                required: true,
                description: "Declared total bytes, at most 64 MiB.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content_ref",
                param_type: "string",
                required: false,
                description: "Optional BLAKE3 reference; checked at commit, with an existence shortcut at begin.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "blob.put_part",
        description: "Append a base64 part at next_index; an identical tail retry is acknowledged without appending.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "upload_id",
                param_type: "string",
                required: true,
                description: "32-character lowercase hex upload capability returned by blob.begin.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "index",
                param_type: "integer",
                required: true,
                description: "Zero-based next part index, or the last accepted index for an identical retry.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "bytes",
                param_type: "string",
                required: true,
                description: "Base64 part, decoded length no greater than the returned part_limit.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "blob.commit",
        description: "Verify declared size and optional reference, then publish the staged object; consumes the upload id.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "upload_id",
                param_type: "string",
                required: true,
                description: "32-character lowercase hex upload capability returned by blob.begin.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "blob.abort",
        description: "Discard a staged upload and invalidate its process-local capability.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "upload_id",
                param_type: "string",
                required: true,
                description: "32-character lowercase hex upload capability returned by blob.begin.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
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

    fn host_state(&self) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
        if self.runtime().is_read_only() || self.runtime().blob_store().is_none() {
            None
        } else {
            Some(self.uploads.clone())
        }
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
            "blob.begin" => handlers::handle_begin(&self.uploads, token, params).await,
            "blob.put_part" => handlers::handle_put_part(&self.uploads, params).await,
            "blob.commit" => handlers::handle_commit(&self.uploads, params).await,
            "blob.abort" => handlers::handle_abort(&self.uploads, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
