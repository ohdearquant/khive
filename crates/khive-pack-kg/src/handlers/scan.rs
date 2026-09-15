//! `scan` verb handler: the secret gate's answer for a note body, without a write.

use serde_json::{json, Value};

use khive_runtime::secret_gate::{
    check_at, check_json_at, mask_for_redaction_surface, reject_reserved_secret_gate_property,
    RedactionSurface,
};
use khive_runtime::{NamespaceToken, RuntimeError};

use super::common::deser;
use super::params::ScanParams;
use crate::KgPack;

impl KgPack {
    /// Report whether the secret gate would refuse a note body, which detector
    /// fires, and where. Nothing is stored and no event is written.
    ///
    /// Reserved properties are validated before content, name and properties
    /// reach the same credential detectors used by a note write. Reservation
    /// failures retain the shared validator's error; detector matches become
    /// verdicts. The masked preview uses the shared mask-only surface masker.
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

/// The reservation and credential checks shared with the note-write path:
/// reserved properties, then content, name and properties. Agreement tests
/// cover this sequence; create-only kind and shape validation is outside scan.
fn scan_note_fields(
    content: &str,
    name: Option<&str>,
    properties: Option<&Value>,
) -> Result<(), RuntimeError> {
    reject_reserved_secret_gate_property(properties)?;
    check_at(content, "note", "content")?;
    if let Some(n) = name {
        check_at(n, "note", "name")?;
    }
    if let Some(p) = properties {
        check_json_at(p, "note", "properties")?;
    }
    Ok(())
}
