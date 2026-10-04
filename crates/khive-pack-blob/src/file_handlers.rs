//! Confined server filesystem transfers. Results carry metadata, never file bytes.

use std::io::Write as _;
use std::path::Path;

use khive_runtime::{file_policy, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::UploadId;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;

use crate::handlers::{parse_content_ref, parse_params, MAX_OBJECT_BYTES};
use crate::uploads::UploadManager;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportParams {
    path: String,
    media_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    content_ref: String,
    path: String,
}

fn file_error(verb: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::InvalidInput(format!("{verb}: {error}"))
}

pub(crate) async fn handle_import(
    runtime: &KhiveRuntime,
    uploads: &UploadManager,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let ImportParams { path, media_type } = parse_params(params, "blob.import")?;
    if runtime.is_read_only() {
        return Err(file_error(
            "blob.import",
            "the blob pack runtime is read-only",
        ));
    }
    let (imports, _) =
        file_policy::confined_file_roots().map_err(|error| file_error("blob.import", error))?;
    let file = file_policy::open_import(&imports, Path::new(&path))
        .map_err(|error| file_error("blob.import", error))?;
    let size = file
        .metadata()
        .map_err(|error| file_error("blob.import", error))?
        .len();
    if size > MAX_OBJECT_BYTES {
        return Err(file_error(
            "blob.import",
            format!("file exceeds the {MAX_OBJECT_BYTES}-byte maximum"),
        ));
    }
    let begun = uploads
        .begin(
            size,
            None,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .await?;
    let id = UploadId::from_hex(begun["upload_id"].as_str().ok_or_else(|| {
        RuntimeError::Internal("blob.import: staging returned no upload id".into())
    })?)
    .map_err(|error| RuntimeError::Internal(format!("blob.import: invalid staging id: {error}")))?;
    let mut file = tokio::fs::File::from_std(file);
    let result = async {
        let mut index = 0;
        loop {
            let mut part = vec![0; 64 * 1024];
            let read = file
                .read(&mut part)
                .await
                .map_err(|error| file_error("blob.import", error))?;
            if read == 0 {
                break;
            }
            part.truncate(read);
            uploads.put_part(&id, index, part).await?;
            index += 1;
        }
        // UploadManager hashes exactly the accepted bytes and checks the declared
        // length before publishing through BlobStore::commit_upload.
        uploads.commit(&id).await
    }
    .await;
    match result {
        Ok(mut metadata) => {
            if let Some(media_type) = media_type {
                metadata["media_type"] = json!(media_type);
            }
            Ok(metadata)
        }
        Err(error) => {
            let _ = uploads.abort(&id).await;
            Err(error)
        }
    }
}

pub(crate) async fn handle_export(
    runtime: &KhiveRuntime,
    params: Value,
) -> Result<Value, RuntimeError> {
    let ExportParams { content_ref, path } = parse_params(params, "blob.export")?;
    let reference = parse_content_ref(&content_ref, "blob.export")?;
    if runtime.is_read_only() {
        return Err(file_error(
            "blob.export",
            "the blob pack runtime is read-only",
        ));
    }
    file_policy::confined_file_roots().map_err(|error| file_error("blob.export", error))?;
    let destination = file_policy::resolve_destination(Path::new(&path), true)
        .map_err(|error| file_error("blob.export", error))?;
    let size = runtime
        .require_blob_store()?
        .size(&reference)
        .await?
        .ok_or_else(|| {
            RuntimeError::NotFound(format!(
                "blob.export: no object stored under content_ref {reference}"
            ))
        })?;
    if size > MAX_OBJECT_BYTES {
        return Err(file_error(
            "blob.export",
            format!("object exceeds the {MAX_OBJECT_BYTES}-byte maximum"),
        ));
    }
    let verified = runtime
        .require_blob_hydrator()?
        .hydrate_verified(&reference, size)
        .await?;
    tokio::task::spawn_blocking(move || {
        let parent = destination
            .parent()
            .ok_or_else(|| file_error("blob.export", "destination has no parent"))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".khive-blob-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|error| file_error("blob.export", error))?;
        temp.write_all(verified.bytes())
            .map_err(|error| file_error("blob.export", error))?;
        temp.flush()
            .map_err(|error| file_error("blob.export", error))?;
        temp.persist(&destination)
            .map_err(|error| file_error("blob.export", error))?;
        Ok(json!({"path": destination.to_string_lossy(), "size": verified.bytes().len()}))
    })
    .await
    .map_err(|error| RuntimeError::Internal(format!("blob.export: file task failed: {error}")))?
}
