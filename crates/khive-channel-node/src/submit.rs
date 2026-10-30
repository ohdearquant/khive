use crate::source::PersistedSubmission;
use serde::ser::{Serialize, SerializeStruct, Serializer};

struct StableSubmission<'a>(&'a PersistedSubmission);
impl Serialize for StableSubmission<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let p = self.0;
        // A.11 specifies this byte order independently of the wire model's declaration order.
        let mut s = serializer.serialize_struct("Submission", 9)?;
        s.serialize_field("ciphertext", &p.ciphertext)?;
        s.serialize_field("contact_generation", &p.contact_generation)?;
        s.serialize_field("enc", &p.enc)?;
        s.serialize_field("logical_message_id", &p.logical_message_id)?;
        s.serialize_field("protocol_version", &p.protocol_version)?;
        s.serialize_field("recipient", &p.recipient)?;
        s.serialize_field("recipient_device_id", &p.recipient_device)?;
        s.serialize_field("recipient_key_epoch", &p.recipient_key_epoch)?;
        s.serialize_field("sender_key_epoch", &p.sender_key_epoch)?;
        s.end()
    }
}

pub fn serialize_submission(
    submission: &PersistedSubmission,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&StableSubmission(submission))
}
