//! Private v1 wire format: fixed big-endian integers, explicit byte tags,
//! u32 lengths and exact UTF-8. Schema fields are listed in wire order below.
use super::envelope::*;
use super::keys::KeyRef;
use super::permission::*;
use super::{bound, EnvelopeError, EnvelopeResult, MAX_COLLECTION_ITEMS, MAX_RECORD_BYTES};
use khive_types::Timestamp;
use uuid::Uuid;

pub(super) trait Wire: Sized {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()>;
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self>;
}
pub(super) struct Encoder {
    bytes: Option<Vec<u8>>,
    length: usize,
}
impl Encoder {
    fn put(&mut self, bytes: &[u8]) -> EnvelopeResult<()> {
        self.length = self
            .length
            .checked_add(bytes.len())
            .ok_or(EnvelopeError::OverLimit {
                field: "encoded body",
                measured: usize::MAX,
                bound: MAX_RECORD_BYTES,
            })?;
        if let Some(out) = &mut self.bytes {
            out.extend_from_slice(bytes);
        }
        Ok(())
    }
    fn blob(&mut self, bytes: &[u8]) -> EnvelopeResult<()> {
        bound("byte string", bytes.len(), MAX_RECORD_BYTES)?;
        (bytes.len() as u32).write(self)?;
        self.put(bytes)
    }
}
pub(super) struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> EnvelopeResult<&'a [u8]> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or(EnvelopeError::Reject("wire length overflow"))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(EnvelopeError::Reject("truncated wire value"))?;
        self.cursor = end;
        Ok(value)
    }
    fn blob(&mut self) -> EnvelopeResult<&'a [u8]> {
        let length = u32::read(self)? as usize;
        bound("byte string", length, MAX_RECORD_BYTES)?;
        self.take(length)
    }
}
pub(super) fn encoded_len<T: Wire>(value: &T) -> EnvelopeResult<usize> {
    let mut out = Encoder {
        bytes: None,
        length: 0,
    };
    value.write(&mut out)?;
    Ok(out.length)
}
pub(super) fn encode<T: Wire>(value: &T) -> EnvelopeResult<Vec<u8>> {
    let length = encoded_len(value)?;
    bound("encoded body", length, MAX_RECORD_BYTES)?;
    let mut out = Encoder {
        bytes: Some(Vec::with_capacity(length)),
        length: 0,
    };
    value.write(&mut out)?;
    out.bytes
        .ok_or(EnvelopeError::Reject("missing encoding buffer"))
}
pub(super) fn decode<T: Wire>(bytes: &[u8]) -> EnvelopeResult<T> {
    bound("encoded body", bytes.len(), MAX_RECORD_BYTES)?;
    let mut input = Decoder { bytes, cursor: 0 };
    let value = T::read(&mut input)?;
    if input.cursor != bytes.len() {
        return Err(EnvelopeError::Reject("trailing wire bytes"));
    }
    Ok(value)
}
fn bad_tag() -> EnvelopeError {
    EnvelopeError::Reject("unknown wire tag")
}
macro_rules! integer {
    ($($ty:ty),+ $(,)?) => { $(impl Wire for $ty {
        fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> { out.put(&self.to_be_bytes()) }
        fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
            Ok(Self::from_be_bytes(input.take(std::mem::size_of::<Self>())?.try_into().map_err(|_| EnvelopeError::Reject("integer width"))?))
        }
    })+ };
}
integer!(u8, u16, u32, u64, u128, i64);
impl Wire for bool {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        u8::from(*self).write(out)
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for String {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        out.blob(self.as_bytes())
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        String::from_utf8(input.blob()?.to_vec())
            .map_err(|_| EnvelopeError::Reject("invalid UTF-8"))
    }
}
impl Wire for ExactBytes {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        out.blob(&self.0)
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        Ok(Self(input.blob()?.to_vec()))
    }
}
impl<T: Wire> Wire for Vec<T> {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        bound("collection", self.len(), MAX_COLLECTION_ITEMS)?;
        (self.len() as u32).write(out)?;
        for value in self {
            value.write(out)?;
        }
        Ok(())
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        let length = u32::read(input)? as usize;
        bound("collection", length, MAX_COLLECTION_ITEMS)?;
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(T::read(input)?);
        }
        Ok(values)
    }
}
impl<T: Wire> Wire for Option<T> {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            None => 0_u8.write(out),
            Some(v) => {
                1_u8.write(out)?;
                v.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(None),
            1 => Ok(Some(T::read(input)?)),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for [u8; 32] {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        out.put(self)
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        input
            .take(32)?
            .try_into()
            .map_err(|_| EnvelopeError::Reject("digest width"))
    }
}
impl Wire for Uuid {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        out.put(self.as_bytes())
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        Uuid::from_slice(input.take(16)?).map_err(|_| EnvelopeError::Reject("UUID width"))
    }
}
impl Wire for Timestamp {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        self.as_micros().write(out)
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        Ok(Self::from_micros(u64::read(input)?))
    }
}
impl Wire for ContractDigest {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        1_u8.write(out)?;
        self.0.write(out)
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        if u8::read(input)? != 1 {
            return Err(bad_tag());
        }
        Ok(Self(<[u8; 32]>::read(input)?))
    }
}
macro_rules! structure {
    ($ty:ident { $($field:ident),+ $(,)? }) => { impl Wire for $ty {
        fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> { $(self.$field.write(out)?;)+ Ok(()) }
        fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> { Ok(Self { $($field: Wire::read(input)?,)+ }) }
    } };
}
macro_rules! wrapper {
    ($ty:ident) => {
        impl Wire for $ty {
            fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
                self.0.write(out)
            }
            fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
                Ok(Self(Wire::read(input)?))
            }
        }
    };
}
macro_rules! unit_enum {
    ($ty:ident { $($variant:ident = $tag:literal),+ $(,)? }) => { impl Wire for $ty {
        fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> { match self { $(Self::$variant => ($tag as u8).write(out),)+ } }
        fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> { match u8::read(input)? { $($tag => Ok(Self::$variant),)+ _ => Err(bad_tag()) } }
    } };
}
structure!(Principal { kind, id, leg });
structure!(ContractRef {
    name,
    version,
    digest
});
structure!(RepositoryIdentity {
    forge,
    immutable_id
});
structure!(Validity {
    not_before,
    expires_at
});
wrapper!(RunRef);
structure!(PhaseVisit { phase_id, round });
structure!(ActionOccurrence { visit, action_name });
structure!(ArgumentBinding { name, value });
unit_enum!(ArgumentType { String=0, Integer=1, Duration=2, Bool=3, Word=4 });
structure!(ArgumentSpec { name, kind });
wrapper!(ExactArguments);
unit_enum!(ProducerClass { Actor=0, Instrument=1, Platform=2, System=3 });
unit_enum!(SubmissionChannel { Observe=0, Sample=1, Internal=2 });
structure!(SubjectScope { kind, selector });
structure!(SourceBinding {
    contract,
    identities,
    arguments
});
structure!(ObservationPermission {
    policy_domain,
    entry,
    producer_class,
    channel,
    subject,
    binding
});
structure!(ObservationScope { permissions });
structure!(AdminScope { namespace, target });
structure!(KeyRef { id, generation });
unit_enum!(IssuanceAssurance { ConfiguredLocal=0 });
unit_enum!(CreationOperation { Grant=0, Import=1 });
// Grant UUID starts at body byte 6; the MAC omission control targets this field.
structure!(EnvelopeDraft {
    id,
    store_id,
    epoch_id,
    root_anchor_id,
    issuer,
    grantee,
    namespace,
    rights,
    validity,
    created_at,
    assurance,
    operation,
    action_id,
    configuration_source,
    issuance_id,
    key
});
structure!(IssuanceContext {
    id,
    holder,
    holder_principal,
    target,
    namespace
});
structure!(NativeCapability {
    id,
    holder,
    target,
    rights,
    parent,
    epoch,
    signature
});
structure!(GrantBody {
    schema_version,
    profile_version,
    carrier_version,
    grant,
    parent,
    context,
    native
});

impl Wire for ArgumentValue {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::String(v) => {
                0_u8.write(out)?;
                v.write(out)
            }
            Self::Integer(v) => {
                1_u8.write(out)?;
                v.write(out)
            }
            Self::Duration(v) => {
                2_u8.write(out)?;
                v.write(out)
            }
            Self::Bool(v) => {
                3_u8.write(out)?;
                v.write(out)
            }
            Self::Word(v) => {
                4_u8.write(out)?;
                v.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::String(Wire::read(input)?)),
            1 => Ok(Self::Integer(Wire::read(input)?)),
            2 => Ok(Self::Duration(Wire::read(input)?)),
            3 => Ok(Self::Bool(Wire::read(input)?)),
            4 => Ok(Self::Word(Wire::read(input)?)),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for SubjectSelector {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::Exact(v) => {
                0_u8.write(out)?;
                v.write(out)
            }
            Self::Repositories(v) => {
                1_u8.write(out)?;
                v.write(out)
            }
            Self::AllSubjectsOfKind => 2_u8.write(out),
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::Exact(Wire::read(input)?)),
            1 => Ok(Self::Repositories(Wire::read(input)?)),
            2 => Ok(Self::AllSubjectsOfKind),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for ReportingBinding {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::Role(v) => {
                0_u8.write(out)?;
                v.write(out)
            }
            Self::Initiator(v) => {
                1_u8.write(out)?;
                v.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::Role(Wire::read(input)?)),
            1 => Ok(Self::Initiator(Wire::read(input)?)),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for SubmissionBinding {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::ActorReport {
                run,
                definition,
                role,
            } => {
                0_u8.write(out)?;
                run.write(out)?;
                definition.write(out)?;
                role.write(out)
            }
            Self::ActionResult {
                run,
                definition,
                occurrence,
                reporter,
                arguments,
            } => {
                1_u8.write(out)?;
                run.write(out)?;
                definition.write(out)?;
                occurrence.write(out)?;
                reporter.write(out)?;
                arguments.write(out)
            }
            Self::Instrument(v) => {
                2_u8.write(out)?;
                v.write(out)
            }
            Self::PlatformRelay {
                source,
                response_contract,
            } => {
                3_u8.write(out)?;
                source.write(out)?;
                response_contract.write(out)
            }
            Self::ClaimTimePlatformBundle => 4_u8.write(out),
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::ActorReport {
                run: Wire::read(input)?,
                definition: Wire::read(input)?,
                role: Wire::read(input)?,
            }),
            1 => Ok(Self::ActionResult {
                run: Wire::read(input)?,
                definition: Wire::read(input)?,
                occurrence: Wire::read(input)?,
                reporter: Wire::read(input)?,
                arguments: Wire::read(input)?,
            }),
            2 => Ok(Self::Instrument(Wire::read(input)?)),
            3 => Ok(Self::PlatformRelay {
                source: Wire::read(input)?,
                response_contract: Wire::read(input)?,
            }),
            4 => Ok(Self::ClaimTimePlatformBundle),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for AdminTarget {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::Read => 0_u8.write(out),
            Self::Create => 1_u8.write(out),
            Self::Write => 2_u8.write(out),
            Self::Delete => 3_u8.write(out),
            Self::Send => 4_u8.write(out),
            Self::RecordObservation(v) => {
                5_u8.write(out)?;
                v.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::Read),
            1 => Ok(Self::Create),
            2 => Ok(Self::Write),
            3 => Ok(Self::Delete),
            4 => Ok(Self::Send),
            5 => Ok(Self::RecordObservation(Wire::read(input)?)),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for RightSpec {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::Read => 0_u8.write(out),
            Self::Create => 1_u8.write(out),
            Self::Write => 2_u8.write(out),
            Self::Delete => 3_u8.write(out),
            Self::Send => 4_u8.write(out),
            Self::RecordObservation(v) => {
                5_u8.write(out)?;
                v.write(out)
            }
            Self::GrantAdmin(v) => {
                6_u8.write(out)?;
                v.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::Read),
            1 => Ok(Self::Create),
            2 => Ok(Self::Write),
            3 => Ok(Self::Delete),
            4 => Ok(Self::Send),
            5 => Ok(Self::RecordObservation(Wire::read(input)?)),
            6 => Ok(Self::GrantAdmin(Wire::read(input)?)),
            _ => Err(bad_tag()),
        }
    }
}
impl Wire for ParentBinding {
    fn write(&self, out: &mut Encoder) -> EnvelopeResult<()> {
        match self {
            Self::Grant {
                grant_id,
                issuance_id,
                original_local_id,
                alias_id,
                native_digest,
            } => {
                0_u8.write(out)?;
                grant_id.write(out)?;
                issuance_id.write(out)?;
                original_local_id.write(out)?;
                alias_id.write(out)?;
                native_digest.write(out)
            }
            Self::BootstrapSlot {
                slot,
                manifest_digest,
                anchor_local_id,
            } => {
                1_u8.write(out)?;
                slot.write(out)?;
                manifest_digest.write(out)?;
                anchor_local_id.write(out)
            }
        }
    }
    fn read(input: &mut Decoder<'_>) -> EnvelopeResult<Self> {
        match u8::read(input)? {
            0 => Ok(Self::Grant {
                grant_id: Wire::read(input)?,
                issuance_id: Wire::read(input)?,
                original_local_id: Wire::read(input)?,
                alias_id: Wire::read(input)?,
                native_digest: Wire::read(input)?,
            }),
            1 => Ok(Self::BootstrapSlot {
                slot: Wire::read(input)?,
                manifest_digest: Wire::read(input)?,
                anchor_local_id: Wire::read(input)?,
            }),
            _ => Err(bad_tag()),
        }
    }
}
