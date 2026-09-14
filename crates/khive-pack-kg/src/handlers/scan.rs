//! `scan` verb handler: the secret gate's answer for a note body, without a write.

use serde_json::{json, Value};

use khive_runtime::secret_gate::{
    check_at, check_json_at, mask_for_redaction_surface, RedactionSurface,
};
use khive_runtime::{NamespaceToken, RuntimeError};

use super::common::deser;
use super::params::ScanParams;
use crate::KgPack;

impl KgPack {
    /// Report whether the secret gate would refuse a note body, which detector
    /// fires, and where. Nothing is stored and no event is written.
    ///
    /// The verdict comes from the same `check_at` / `check_json_at` calls, in
    /// the same field order, that a note write runs before it stores anything,
    /// so the probe cannot disagree with the write path by construction: there
    /// is no second predicate to drift. The masked preview goes through the
    /// same masker every mask-only surface uses.
    pub(crate) async fn handle_scan(
        &self,
        _token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ScanParams = deser(params)?;
        let verdict = scan_note_fields(&p.content, p.name.as_deref(), p.properties.as_ref());
        let masked_preview = json!({
            "content": mask_for_redaction_surface(RedactionSurface::GateProbe, &p.content),
            "name": p
                .name
                .as_deref()
                .map(|n| mask_for_redaction_surface(RedactionSurface::GateProbe, n)),
        });
        Ok(match verdict {
            Ok(()) => json!({
                "would_refuse": false,
                "detector": null,
                "trigger": null,
                "masked": null,
                "location": null,
                "message": null,
                "masked_preview": masked_preview,
            }),
            Err(RuntimeError::SecretDetected(m)) => {
                let message = RuntimeError::SecretDetected(m.clone()).to_string();
                json!({
                    "would_refuse": true,
                    "detector": m.detector,
                    "trigger": m.trigger,
                    "masked": m.masked,
                    "location": m.location,
                    "message": message,
                    "masked_preview": masked_preview,
                })
            }
            Err(other) => return Err(other),
        })
    }
}

/// The note-write gate sequence: content, then name, then properties. Keep in
/// step with the note create path in `khive_runtime`; the agreement test in
/// `handlers/tests.rs` fails if the two diverge on a refused body.
fn scan_note_fields(
    content: &str,
    name: Option<&str>,
    properties: Option<&Value>,
) -> Result<(), RuntimeError> {
    check_at(content, "note", "content")?;
    if let Some(n) = name {
        check_at(n, "note", "name")?;
    }
    if let Some(p) = properties {
        check_json_at(p, "note", "properties")?;
    }
    Ok(())
}
