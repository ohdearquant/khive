//! Persistence surface for a producer that has already obtained web content.
//!
//! A renderer, browser session, or importer can call this module without
//! calling a web verb or supplying credential material. `store_capture` uses
//! the same settlement path as `web.fetch`: deterministic `site` and
//! `page`/`resource` IDs, `contains` and permanent-redirect edges, a rooted
//! body blob, representation properties, and a `web.receipt` observation.
//! The caller is responsible for acquiring the response and for the egress
//! policy of that acquisition. The supplied body must be the identity-coded
//! bytes that `web.fetch` would store: a GET capture with content and a
//! declared non-identity content coding is refused before any row is written,
//! as are 1xx and 304 statuses. A HEAD or an empty 204 stores no content, so
//! its declared coding is not checked, as in `web.fetch`.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{egress, fetch};

/// Only the two methods whose representations `web.fetch` can settle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMethod {
    Get,
    Head,
}

impl CaptureMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

/// Representation negotiation recorded with a capture for later refresh.
///
/// The fixed `Accept-Encoding: identity` used by `web.fetch` is added by the
/// settlement path. No credential, cookie, authorization, or signing field
/// exists on this type; other request headers are never persisted here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SelectionHeaders {
    pub accept: Vec<String>,
    pub accept_language: Vec<String>,
}

impl SelectionHeaders {
    fn into_pairs(self) -> Result<Vec<(String, String)>, RuntimeError> {
        let mut pairs = Vec::new();
        for (name, values) in [
            ("accept", self.accept),
            ("accept-language", self.accept_language),
        ] {
            for value in values {
                if value.trim().is_empty() || HeaderValue::from_str(&value).is_err() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "invalid {name} selection header"
                    )));
                }
                pairs.push((name.to_string(), value));
            }
        }
        Ok(pairs)
    }
}

/// Response metadata that `web.fetch` can retain in a row or receipt.
///
/// Fields such as `Set-Cookie` and `Authorization` are deliberately absent.
/// A declared content coding is checked but never stored.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureHeaders {
    pub content_type: Option<String>,
    pub content_length: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub vary: Vec<String>,
    pub content_language: Vec<String>,
    pub link: Vec<String>,
    pub content_encoding: Vec<String>,
}

impl CaptureHeaders {
    fn into_header_map(self) -> Result<HeaderMap, RuntimeError> {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", self.content_type),
            ("content-length", self.content_length),
            ("etag", self.etag),
            ("last-modified", self.last_modified),
        ] {
            if let Some(value) = value {
                append_header(&mut headers, name, &value)?;
            }
        }
        for (name, values) in [
            ("vary", self.vary),
            ("content-language", self.content_language),
            ("link", self.link),
            ("content-encoding", self.content_encoding),
        ] {
            for value in values {
                append_header(&mut headers, name, &value)?;
            }
        }
        Ok(headers)
    }
}

fn append_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), RuntimeError> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| RuntimeError::InvalidInput(format!("invalid {name} response header")))?;
    headers.append(name, value);
    Ok(())
}

/// A followed redirect. Permanent redirects mint `new supersedes old`;
/// temporary redirects appear only in the receipt's `redirect_chain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirect {
    pub from: Url,
    pub to: Url,
    pub status: u16,
}

/// An already-obtained HTTP representation to persist as a web capture.
///
/// `GET` requires body bytes, and exactly an empty vector for a 204 response.
/// `HEAD` requires `body = None` and `truncated = false`. An interim 1xx
/// status and a 304 are refused: neither carries a representation to store.
/// `truncated` describes a caller-enforced byte ceiling on the identity body.
#[derive(Clone, Debug)]
pub struct Capture {
    pub method: CaptureMethod,
    pub final_url: Url,
    pub status: u16,
    pub headers: CaptureHeaders,
    pub selection: SelectionHeaders,
    pub body: Option<Vec<u8>>,
    pub truncated: bool,
    pub redirects: Vec<Redirect>,
}

impl Capture {
    pub fn get(final_url: Url, status: u16, body: Vec<u8>) -> Self {
        Self {
            method: CaptureMethod::Get,
            final_url,
            status,
            headers: CaptureHeaders::default(),
            selection: SelectionHeaders::default(),
            body: Some(body),
            truncated: false,
            redirects: Vec::new(),
        }
    }

    pub fn head(final_url: Url, status: u16) -> Self {
        Self {
            method: CaptureMethod::Head,
            final_url,
            status,
            headers: CaptureHeaders::default(),
            selection: SelectionHeaders::default(),
            body: None,
            truncated: false,
            redirects: Vec::new(),
        }
    }
}

/// Mint the address as an unfetched `resource`, returning `(site_id,
/// document_id)`. A later `store_capture` re-types the document in place
/// when its content type identifies a page.
pub async fn mint_resource(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
) -> Result<(Uuid, Uuid), RuntimeError> {
    egress::check_scheme_and_userinfo(url)?;
    fetch::mint_bare(runtime, token, url).await
}

/// Persist a capture through the exact row, blob, and receipt path used by
/// `web.fetch` with `persist = true`.
///
/// The result is the same JSON reply shape as `web.fetch`, including `id`,
/// `content_ref`, and `receipt_id`. The receipt uses `web.fetch`'s fields and
/// `web.receipt` tag, so existing extraction and refresh consumers can read
/// captures from this producer without a separate row convention.
pub async fn store_capture(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    capture: Capture,
) -> Result<Value, RuntimeError> {
    if !(100..=599).contains(&capture.status) {
        return Err(RuntimeError::InvalidInput(
            "invalid HTTP status".to_string(),
        ));
    }
    if capture.status < 200 {
        return Err(RuntimeError::InvalidInput(
            "capture status must be a final response, not an interim 1xx".to_string(),
        ));
    }
    // A 304 validates a representation the caller already holds instead of
    // carrying one. Settling it as a capture would replace the stored body
    // with an empty one, so revalidation stays with `web.refresh`.
    if capture.status == 304 {
        return Err(RuntimeError::InvalidInput(
            "a 304 capture carries no representation; revalidate with web.refresh".to_string(),
        ));
    }
    match (capture.method, capture.body.as_ref(), capture.truncated) {
        (CaptureMethod::Get, None, _) => {
            return Err(RuntimeError::InvalidInput(
                "GET capture requires body bytes".to_string(),
            ));
        }
        (CaptureMethod::Get, Some(body), truncated)
            if capture.status == 204 && (!body.is_empty() || truncated) =>
        {
            return Err(RuntimeError::InvalidInput(
                "a 204 capture cannot carry body bytes or truncation".to_string(),
            ));
        }
        (CaptureMethod::Head, Some(_), _) | (CaptureMethod::Head, None, true) => {
            return Err(RuntimeError::InvalidInput(
                "HEAD capture cannot carry body bytes or truncation".to_string(),
            ));
        }
        _ => {}
    }
    egress::check_scheme_and_userinfo(&capture.final_url)?;
    if capture.redirects.len() > fetch::MAX_REDIRECTS as usize {
        return Err(RuntimeError::InvalidInput(
            "capture exceeds the web redirect limit".to_string(),
        ));
    }
    for (index, hop) in capture.redirects.iter().enumerate() {
        egress::check_scheme_and_userinfo(&hop.from)?;
        egress::check_scheme_and_userinfo(&hop.to)?;
        if !(300..=399).contains(&hop.status)
            || (index > 0 && capture.redirects[index - 1].to != hop.from)
        {
            return Err(RuntimeError::InvalidInput(
                "capture has an invalid redirect chain".to_string(),
            ));
        }
    }
    if capture
        .redirects
        .last()
        .is_some_and(|hop| hop.to != capture.final_url)
    {
        return Err(RuntimeError::InvalidInput(
            "capture redirect chain does not end at final_url".to_string(),
        ));
    }
    let headers = capture.headers.into_header_map()?;
    // Only an empty 204 skips the coding check, as in `web.fetch`: it has no
    // content to mislabel.
    if capture.method == CaptureMethod::Get && capture.status != 204 {
        fetch::refuse_content_encoding(&headers)?;
    }
    let selection = capture.selection.into_pairs()?;
    let redirects: Vec<_> = capture
        .redirects
        .into_iter()
        .map(|hop| fetch::RedirectHop {
            from: hop.from,
            to: hop.to,
            status: hop.status,
        })
        .collect();
    fetch::settle_with_request_headers(
        runtime,
        token,
        capture.method.as_str(),
        &capture.final_url,
        capture.status,
        &headers,
        capture.body.map(|body| (body, capture.truncated)),
        &redirects,
        true,
        &selection,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use khive_runtime::{Namespace, VerbRegistryBuilder};
    use khive_storage::{Direction, EdgeRelation};
    use serde_json::json;

    use super::*;
    use crate::WebPack;

    fn fixture() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = KhiveRuntime::memory().unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(WebPack::new(runtime.clone()));
        runtime.install_edge_rules(builder.build().unwrap().all_edge_rules());
        let store = khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        runtime.install_blob_store(Arc::new(store)).unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        (runtime, token, dir)
    }

    async fn row_and_receipt(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        reply: &Value,
    ) -> (Value, Value) {
        let id = Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let mut row = serde_json::to_value(
            runtime
                .entities(token)
                .unwrap()
                .get_entity(id)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        for field in ["created_at", "updated_at", "version"] {
            row.as_object_mut().unwrap().remove(field);
        }
        let properties = row["properties"].as_object_mut().unwrap();
        properties.remove("fetched_at");
        properties.remove("capture_receipt_id");
        let mut receipt = serde_json::to_value(
            runtime
                .notes(token)
                .unwrap()
                .get_note(receipt_id)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        for field in ["id", "created_at", "updated_at", "version"] {
            receipt.as_object_mut().unwrap().remove(field);
        }
        receipt["properties"]["request"]
            .as_object_mut()
            .unwrap()
            .remove("fetched_at");
        (row, receipt)
    }

    #[tokio::test]
    async fn producer_and_fetch_settlement_have_identical_persisted_shape() {
        let (fetched, fetched_token, _fetched_dir) = fixture();
        let (produced, produced_token, _produced_dir) = fixture();
        let url = Url::parse("https://fixture.example.test/a?z=2&a=1").unwrap();
        let old_url = Url::parse("https://fixture.example.test/old").unwrap();
        let bytes = b"<html><body>same fixture</body></html>".to_vec();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
        headers.insert("etag", "\"v1\"".parse().unwrap());
        headers.insert("vary", "Accept-Language".parse().unwrap());
        headers.insert("content-language", "en".parse().unwrap());
        let selection = vec![
            ("accept".to_string(), "text/html".to_string()),
            ("accept-language".to_string(), "en".to_string()),
        ];
        let fetched_reply = fetch::settle_with_request_headers(
            &fetched,
            &fetched_token,
            "GET",
            &url,
            200,
            &headers,
            Some((bytes.clone(), false)),
            &[fetch::RedirectHop {
                from: old_url.clone(),
                to: url.clone(),
                status: 301,
            }],
            true,
            &selection,
        )
        .await
        .unwrap();
        let mut capture = Capture::get(url.clone(), 200, bytes);
        capture.headers = CaptureHeaders {
            content_type: Some("text/html; charset=utf-8".to_string()),
            etag: Some("\"v1\"".to_string()),
            vary: vec!["Accept-Language".to_string()],
            content_language: vec!["en".to_string()],
            ..Default::default()
        };
        capture.selection = SelectionHeaders {
            accept: vec!["text/html".to_string()],
            accept_language: vec!["en".to_string()],
        };
        capture.redirects = vec![Redirect {
            from: old_url.clone(),
            to: url.clone(),
            status: 301,
        }];
        let produced_reply = store_capture(&produced, &produced_token, capture)
            .await
            .unwrap();

        let mut fetched_fields = fetched_reply.clone();
        fetched_fields.as_object_mut().unwrap().remove("receipt_id");
        let mut produced_fields = produced_reply.clone();
        produced_fields
            .as_object_mut()
            .unwrap()
            .remove("receipt_id");
        assert_eq!(produced_fields, fetched_fields);
        assert_eq!(
            row_and_receipt(&produced, &produced_token, &produced_reply).await,
            row_and_receipt(&fetched, &fetched_token, &fetched_reply).await,
        );

        let canonical = crate::identity::canonicalize(url);
        let site = crate::identity::site_id(fetched_token.namespace(), &canonical);
        let id = crate::identity::document_id(site, &crate::identity::path_and_query(&canonical));
        let old_canonical = crate::identity::canonicalize(old_url);
        let old_id =
            crate::identity::document_id(site, &crate::identity::path_and_query(&old_canonical));
        let mut fetched_old = serde_json::to_value(
            fetched
                .entities(&fetched_token)
                .unwrap()
                .get_entity(old_id)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let mut produced_old = serde_json::to_value(
            produced
                .entities(&produced_token)
                .unwrap()
                .get_entity(old_id)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        for row in [&mut fetched_old, &mut produced_old] {
            for field in ["created_at", "updated_at"] {
                row.as_object_mut().unwrap().remove(field);
            }
        }
        assert_eq!(produced_old, fetched_old);
        for (runtime, token, reply) in [
            (&fetched, &fetched_token, &fetched_reply),
            (&produced, &produced_token, &produced_reply),
        ] {
            assert_eq!(reply["id"], json!(id.to_string()));
            let contains = runtime
                .neighbors(
                    token,
                    site,
                    Direction::Out,
                    None,
                    Some(vec![EdgeRelation::Contains]),
                )
                .await
                .unwrap();
            let mut contained_ids: Vec<_> = contains.iter().map(|edge| edge.node_id).collect();
            contained_ids.sort();
            let mut expected_ids = vec![id, old_id];
            expected_ids.sort();
            assert_eq!(contained_ids, expected_ids);
            let supersedes = runtime
                .neighbors(
                    token,
                    id,
                    Direction::Out,
                    None,
                    Some(vec![EdgeRelation::Supersedes]),
                )
                .await
                .unwrap();
            assert_eq!(supersedes.len(), 1);
            assert_eq!(supersedes[0].node_id, old_id);
            let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
            let annotates = runtime
                .neighbors(
                    token,
                    receipt_id,
                    Direction::Out,
                    None,
                    Some(vec![EdgeRelation::Annotates]),
                )
                .await
                .unwrap();
            let mut annotated_ids: Vec<_> = annotates.iter().map(|edge| edge.node_id).collect();
            annotated_ids.sort();
            assert_eq!(annotated_ids, expected_ids);
        }
    }
}
