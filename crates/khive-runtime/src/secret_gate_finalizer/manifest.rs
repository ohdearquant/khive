//! Exact-content SHA-256 manifest mechanism (ADR-115 Amendment 1 §6;
//! executable contract §4) — EMPTY manifest only.
//!
//! Production state is always absent or the exact empty v1 document. No
//! non-empty operator manifest is loaded, embedded, configured, or activated
//! by this module. The fault taxonomy below and the non-deployable
//! `#[cfg(test)]` fixture exist to prove the mechanism fails closed and
//! matches its byte-exact regression vectors — they are not a production
//! content-loading path.
//!
//! Only a runtime-recomputed digest over a runtime-owned [`RuntimeFieldScope`]
//! can ever match; nothing about the caller (path, actor, namespace, verb, or
//! any caller-supplied digest) is a lookup input.
#![allow(dead_code)] // consumed by the finalizer execution lane (later increment)

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Deserialize;
use sha2::{Digest, Sha256};

// ─── Field scope ─────────────────────────────────────────────────────────────

/// Closed v1 set of runtime-owned field scopes eligible for a manifest match.
///
/// Adding or renaming a scope is an ADR-level manifest-contract change —
/// never extend this enum to satisfy an implementation convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RuntimeFieldScope {
    RecordContent,
    NameDescription,
    JsonProperties,
    Tags,
    CodeSource,
}

impl RuntimeFieldScope {
    /// The exact ASCII spelling folded into the digest domain separator.
    const fn ascii_bytes(self) -> &'static [u8] {
        match self {
            RuntimeFieldScope::RecordContent => b"record-content",
            RuntimeFieldScope::NameDescription => b"name-description",
            RuntimeFieldScope::JsonProperties => b"json-properties",
            RuntimeFieldScope::Tags => b"tags",
            RuntimeFieldScope::CodeSource => b"code-source",
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            RuntimeFieldScope::RecordContent => "record-content",
            RuntimeFieldScope::NameDescription => "name-description",
            RuntimeFieldScope::JsonProperties => "json-properties",
            RuntimeFieldScope::Tags => "tags",
            RuntimeFieldScope::CodeSource => "code-source",
        }
    }
}

// ─── Digest ──────────────────────────────────────────────────────────────────

/// A full 32-byte SHA-256 digest. Truncated digests are never valid — see
/// [`ManifestFault::TruncatedDigest`].
pub(crate) type Digest32 = [u8; 32];

/// Domain-separation prefix for every scanned-value digest.
const DIGEST_DOMAIN: &[u8] = b"khive-secret-gate-v1\0";

/// `SHA256(DIGEST_DOMAIN || scope.ascii_bytes() || 0x00 || value.as_bytes())`.
///
/// The input is the exact UTF-8 bytes of `value` as already decoded by the
/// request parser — no Unicode normalization, case-folding, or newline
/// rewrite is applied here or anywhere upstream of this function. Two
/// visually identical strings in different normalization forms, or differing
/// only by a trailing newline, hash to different digests by design.
pub(crate) fn scoped_digest(scope: RuntimeFieldScope, value: &str) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(scope.ascii_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    hasher.finalize().into()
}

/// Lowercase 64-character hex encoding, used only for manifest/audit
/// serialization — the in-memory form is always the full `[u8; 32]`.
pub(crate) fn digest_to_hex(digest: &Digest32) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decode a lowercase or uppercase 64-character hex string into a
/// [`Digest32`]. Any length other than exactly 64 is
/// [`ManifestFault::TruncatedDigest`] — never silently zero-padded or
/// truncated.
fn digest_from_hex(hex: &str) -> Result<Digest32, ManifestFault> {
    if hex.len() != 64 {
        return Err(ManifestFault::TruncatedDigest);
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let byte_str = hex
            .get(i * 2..i * 2 + 2)
            .ok_or(ManifestFault::TruncatedDigest)?;
        *byte = u8::from_str_radix(byte_str, 16).map_err(|_| ManifestFault::TruncatedDigest)?;
    }
    Ok(out)
}

// ─── Fault taxonomy ──────────────────────────────────────────────────────────

/// Fail-closed manifest faults, distinguishing every ADR-115 Amendment 1 §5
/// construction plus the base parser faults. A normal manifest MISS (no
/// entry at the scanned scope/digest) is never one of these — it falls
/// through to the unchanged legacy scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManifestFault {
    /// No manifest document is present.
    Absent,
    /// The supplied bytes are not valid UTF-8.
    Unreadable,
    /// The bytes are valid UTF-8 but do not deserialize into
    /// [`ManifestDocumentV1`] (includes any unknown field, since the type is
    /// `deny_unknown_fields`).
    Malformed,
    /// Two entries share `(field_scope, digest)` but disagree on
    /// `overridden_detector`. Byte-identical duplicate entries collapse
    /// silently instead of producing this fault.
    DuplicateConflicting,
    /// `algorithm` is present but is not exactly `"sha256"`.
    UnsupportedAlgorithm,
    /// A `digest_sha256` field is not exactly 64 hex characters.
    TruncatedDigest,
    /// `schema_version` is not exactly `1`.
    UnknownSchemaVersion,
    /// The document has entries but no `expected_corpus_identity` was
    /// supplied by the runtime caller.
    MissingExpectedCorpusIdentity,
    /// The document's `corpus_identity_sha256` does not match the
    /// runtime-supplied expected identity.
    CorpusIdentityMismatch,
    /// A refresh attempt failed; wraps the redacted underlying fault class
    /// name (never the raw source error, which may carry a file path or I/O
    /// detail).
    RefreshFailure { source_class: &'static str },
    /// More than one distinct `(scope, digest)` entry matched across the
    /// scanned candidate. v1 fails closed rather than inventing an array
    /// exemption schema — see [`resolve_match`].
    MultipleMatches,
}

// ─── Wire schema (parse-only; never a production content-loading path) ─────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocumentV1 {
    schema_version: u32,
    manifest_id: String,
    algorithm: String,
    corpus_identity_sha256: Option<String>,
    entries: Vec<ManifestEntryV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntryV1 {
    field_scope: RuntimeFieldScope,
    digest_sha256: String,
    overridden_detector: String,
}

/// Metadata carried by a matched manifest entry. Never contains the
/// scanned content itself — only the detector label the entry overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestEntryMeta {
    pub(crate) overridden_detector: String,
}

// ─── Immutable snapshot ──────────────────────────────────────────────────────

/// Immutable, `Arc`-shared manifest state. Every candidate clones the `Arc`
/// exactly once before its first field scan and uses that one snapshot for
/// every scan and finalization decision in that candidate.
#[derive(Debug)]
pub(crate) struct ManifestSnapshot {
    manifest_id: Arc<str>,
    entries: HashMap<(RuntimeFieldScope, Digest32), ManifestEntryMeta>,
}

/// Canonical empty-manifest id, matching the canonical empty document below.
const EMPTY_MANIFEST_ID: &str = "khive-secret-gate-empty-v1";

impl ManifestSnapshot {
    /// The production default: no entries, so every candidate falls through
    /// to the unchanged legacy scanner.
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self {
            manifest_id: Arc::from(EMPTY_MANIFEST_ID),
            entries: HashMap::new(),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    /// Exact-content lookup: recomputes the digest for `value` under `scope`
    /// and returns the matching entry, or `None` on any miss (wrong scope,
    /// wrong digest, or empty snapshot).
    pub(crate) fn lookup(
        &self,
        scope: RuntimeFieldScope,
        value: &str,
    ) -> Option<&ManifestEntryMeta> {
        let digest = scoped_digest(scope, value);
        self.entries.get(&(scope, digest))
    }
}

/// Resolve every `(scope, value)` candidate scanned for one record against
/// one snapshot. Returns:
/// - `Ok(None)` — no candidate matched (ordinary legacy-scanner path);
/// - `Ok(Some(meta))` — exactly one distinct `(scope, digest)` matched,
///   including when it matched more than one scanned candidate string
///   (e.g. the same exact value appears twice);
/// - `Err(ManifestFault::MultipleMatches)` — two or more DISTINCT
///   `(scope, digest)` pairs matched within the same candidate, which v1
///   fails closed on rather than admitting an ad-hoc multi-match schema.
pub(crate) fn resolve_match<'a>(
    snapshot: &'a ManifestSnapshot,
    scanned: &[(RuntimeFieldScope, &str)],
) -> Result<Option<&'a ManifestEntryMeta>, ManifestFault> {
    let mut found_key: Option<(RuntimeFieldScope, Digest32)> = None;
    for &(scope, value) in scanned {
        let digest = scoped_digest(scope, value);
        if snapshot.entries.contains_key(&(scope, digest)) {
            match found_key {
                None => found_key = Some((scope, digest)),
                Some(prev) if prev == (scope, digest) => {}
                Some(_) => return Err(ManifestFault::MultipleMatches),
            }
        }
    }
    Ok(found_key.map(|key| snapshot.entries.get(&key).expect("key was just found")))
}

// ─── Canonical empty document ───────────────────────────────────────────────

/// The exact canonical, no-newline byte sequence of the empty v1 manifest
/// document (127 bytes). This is the only document shape production may ever
/// hold besides absence — see ADR-115 Amendment 1 §1 and executable contract
/// §4.
pub(crate) const CANONICAL_EMPTY_DOCUMENT_BYTES: &[u8] = br#"{"schema_version":1,"manifest_id":"khive-secret-gate-empty-v1","algorithm":"sha256","corpus_identity_sha256":null,"entries":[]}"#;

/// Byte-regression vector for [`CANONICAL_EMPTY_DOCUMENT_BYTES`]. This hash
/// is a provenance/documentation vector only — it is never a corpus identity
/// and never establishes lookup eligibility on its own.
pub(crate) fn canonical_empty_document_sha256_hex() -> String {
    let mut hasher = Sha256::new();
    hasher.update(CANONICAL_EMPTY_DOCUMENT_BYTES);
    digest_to_hex(&hasher.finalize().into())
}

// ─── Parse ───────────────────────────────────────────────────────────────────

/// Parse manifest document bytes into an immutable snapshot.
///
/// `expected_corpus_identity` is supplied by the runtime caller, never by a
/// request — a document with entries but no matching expected identity fails
/// closed. An empty document (`entries` is empty) is valid without one.
///
/// On any error the caller MUST publish [`ManifestSnapshot::empty`] before
/// propagating the fault — this function only classifies the fault, it does
/// not itself hold or swap any shared state (see [`ManifestManager::refresh`]).
pub(crate) fn parse_document(
    bytes: &[u8],
    expected_corpus_identity: Option<Digest32>,
) -> Result<Arc<ManifestSnapshot>, ManifestFault> {
    let text = std::str::from_utf8(bytes).map_err(|_| ManifestFault::Unreadable)?;
    let doc: ManifestDocumentV1 =
        serde_json::from_str(text).map_err(|_| ManifestFault::Malformed)?;

    if doc.schema_version != 1 {
        return Err(ManifestFault::UnknownSchemaVersion);
    }
    if doc.manifest_id.is_empty() {
        return Err(ManifestFault::Malformed);
    }
    if doc.algorithm != "sha256" {
        return Err(ManifestFault::UnsupportedAlgorithm);
    }

    if !doc.entries.is_empty() {
        let expected =
            expected_corpus_identity.ok_or(ManifestFault::MissingExpectedCorpusIdentity)?;
        let declared_hex = doc
            .corpus_identity_sha256
            .as_deref()
            .ok_or(ManifestFault::MissingExpectedCorpusIdentity)?;
        let declared = digest_from_hex(declared_hex)?;
        if declared != expected {
            return Err(ManifestFault::CorpusIdentityMismatch);
        }
    }

    let mut entries: HashMap<(RuntimeFieldScope, Digest32), ManifestEntryMeta> = HashMap::new();
    for entry in &doc.entries {
        let digest = digest_from_hex(&entry.digest_sha256)?;
        let key = (entry.field_scope, digest);
        match entries.get(&key) {
            None => {
                entries.insert(
                    key,
                    ManifestEntryMeta {
                        overridden_detector: entry.overridden_detector.clone(),
                    },
                );
            }
            Some(existing) if existing.overridden_detector == entry.overridden_detector => {
                // Byte-identical duplicate: collapse silently.
            }
            Some(_) => return Err(ManifestFault::DuplicateConflicting),
        }
    }

    Ok(Arc::new(ManifestSnapshot {
        manifest_id: Arc::from(doc.manifest_id.as_str()),
        entries,
    }))
}

// ─── Manager: absent/empty production state + fail-closed refresh ──────────

/// Holds the one live [`ManifestSnapshot`] behind a lock, defaulting to
/// [`ManifestSnapshot::empty`]. `refresh` always publishes an empty snapshot
/// before returning any fault — a failed refresh never leaves the prior,
/// possibly stale, snapshot live.
pub(crate) struct ManifestManager {
    snapshot: RwLock<Arc<ManifestSnapshot>>,
}

impl Default for ManifestManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ManifestManager {
    pub(crate) fn new() -> Self {
        Self {
            snapshot: RwLock::new(ManifestSnapshot::empty()),
        }
    }

    /// Clone the current live snapshot's `Arc`. Callers take exactly one of
    /// these per candidate before their first field scan.
    pub(crate) fn current(&self) -> Arc<ManifestSnapshot> {
        self.snapshot.read().clone()
    }

    /// Attempt to load and publish new manifest state.
    ///
    /// `bytes = None` means absence (production default). Any parse failure,
    /// including absence, publishes [`ManifestSnapshot::empty`] before
    /// returning its fault — refresh never fails open and never leaves a
    /// stale non-empty snapshot live past a failed attempt.
    pub(crate) fn refresh(
        &self,
        bytes: Option<&[u8]>,
        expected_corpus_identity: Option<Digest32>,
    ) -> Result<(), ManifestFault> {
        let result = match bytes {
            None => Err(ManifestFault::Absent),
            Some(b) => parse_document(b, expected_corpus_identity),
        };
        match result {
            Ok(snapshot) => {
                *self.snapshot.write() = snapshot;
                Ok(())
            }
            Err(fault) => {
                *self.snapshot.write() = ManifestSnapshot::empty();
                Err(fault)
            }
        }
    }
}

// ─── Test-only, non-deployable lookup fixture ───────────────────────────────

#[cfg(any(test, feature = "test-internals"))]
pub(crate) mod fixture {
    use super::*;

    /// Verbatim ADR-115 Amendment 1 disclaimer sentence. The harness asserts
    /// this constant is byte-equal to the amendment text — never paraphrase
    /// or extend it.
    pub(crate) const REQUIRED_DISCLAIMER: &str =
        "This fixture is not evidence of operator adjudication.";

    /// Present only under `#[cfg(test)]`; the production [`ManifestDocumentV1`]
    /// parser has no field named this and would reject it via
    /// `deny_unknown_fields` if it ever appeared in loaded bytes.
    const TEST_FIXTURE_SCHEMA_MARKER: &str = "khive-secret-gate-test-fixture-v1";

    /// This fixture is not evidence of operator adjudication.
    ///
    /// Exists solely to exercise the manifest lookup path in tests. It is
    /// not operator-approved content, not corpus evidence, and not
    /// deployable — no production function accepts this type, and it never
    /// round-trips through [`ManifestDocumentV1`].
    pub(crate) struct TestOnlyManifestFixture {
        test_fixture_schema: &'static str,
        field_scope: RuntimeFieldScope,
        exact_value: String,
        overridden_detector: &'static str,
    }

    impl TestOnlyManifestFixture {
        /// Builds one non-empty exact entry. The credential-shaped value is
        /// assembled at runtime from disjoint fragments — never stored as one
        /// contiguous secret-shaped literal in source.
        pub(crate) fn new() -> Self {
            let prefix = "AKIA";
            let body = "TESTFIXTURE";
            let suffix = "0000000000";
            let assembled = [prefix, body, suffix].concat();
            Self {
                test_fixture_schema: TEST_FIXTURE_SCHEMA_MARKER,
                field_scope: RuntimeFieldScope::RecordContent,
                exact_value: assembled,
                overridden_detector: "aws-access-key-id",
            }
        }

        pub(crate) fn for_exact_value(
            field_scope: RuntimeFieldScope,
            exact_value: impl Into<String>,
        ) -> Self {
            Self {
                test_fixture_schema: TEST_FIXTURE_SCHEMA_MARKER,
                field_scope,
                exact_value: exact_value.into(),
                overridden_detector: "test-only-reviewed-false-positive",
            }
        }

        pub(crate) fn schema_marker(&self) -> &'static str {
            self.test_fixture_schema
        }

        pub(crate) fn field_scope(&self) -> RuntimeFieldScope {
            self.field_scope
        }

        pub(crate) fn exact_value(&self) -> &str {
            &self.exact_value
        }

        /// A one-entry snapshot built with the same [`scoped_digest`]
        /// production uses, for lookup-path tests only.
        pub(crate) fn snapshot(&self) -> Arc<ManifestSnapshot> {
            let digest = scoped_digest(self.field_scope, &self.exact_value);
            let mut entries = HashMap::new();
            entries.insert(
                (self.field_scope, digest),
                ManifestEntryMeta {
                    overridden_detector: self.overridden_detector.to_string(),
                },
            );
            Arc::new(ManifestSnapshot {
                manifest_id: Arc::from(TEST_FIXTURE_SCHEMA_MARKER),
                entries,
            })
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::fixture::{TestOnlyManifestFixture, REQUIRED_DISCLAIMER};
    use super::*;

    // --- Digest regression vectors (exact empty-value bytes per scope) ---

    #[test]
    fn digest_vectors_match_regression_exactly() {
        let cases: &[(RuntimeFieldScope, &str)] = &[
            (
                RuntimeFieldScope::RecordContent,
                "8babc16495ddbc04d2fd382d3b423d452bedb7567e64cfbe5bb85a5bcb4ff04a",
            ),
            (
                RuntimeFieldScope::NameDescription,
                "db6c4a6305fabfdc246bfc3f37cba55907ee65e5035827f06598a37b478e97d1",
            ),
            (
                RuntimeFieldScope::JsonProperties,
                "2d1a970af8016a1721b1457c7675b08355b6da0b3c859934da1fabf7e660ee28",
            ),
            (
                RuntimeFieldScope::Tags,
                "a1037f3591a751f9d34748fd090a4551d87cbdbb819e7b612e470b6a3a58f833",
            ),
            (
                RuntimeFieldScope::CodeSource,
                "1a53a3a30c62d6c805aabd11ace2124699821b5f046df4c08cf96ac9fa18e892",
            ),
        ];
        for (scope, expected_hex) in cases {
            let digest = scoped_digest(*scope, "");
            assert_eq!(
                &digest_to_hex(&digest),
                expected_hex,
                "empty-value digest mismatch for {scope:?}"
            );
        }
    }

    #[test]
    fn canonical_empty_document_bytes_and_hash_match_regression() {
        assert_eq!(CANONICAL_EMPTY_DOCUMENT_BYTES.len(), 127);
        assert_eq!(
            canonical_empty_document_sha256_hex(),
            "ee4e2ab801099252459bcf930583bed9e8107aad2cc7af2db361f85ee65a31b9"
        );
    }

    // --- Newline / encoding sensitivity (exact-content, no normalization) ---

    #[test]
    fn scoped_digest_is_sensitive_to_embedded_newline() {
        let with_lf = scoped_digest(RuntimeFieldScope::RecordContent, "line1\nline2");
        let with_crlf = scoped_digest(RuntimeFieldScope::RecordContent, "line1\r\nline2");
        let without_newline = scoped_digest(RuntimeFieldScope::RecordContent, "line1line2");
        assert_ne!(with_lf, with_crlf, "LF and CRLF must hash differently");
        assert_ne!(
            with_lf, without_newline,
            "newline must be part of exact bytes"
        );
    }

    #[test]
    fn scoped_digest_is_sensitive_to_unicode_normalization_form() {
        // "café" as NFC (single U+00E9) vs NFD (e + U+0301 combining acute).
        let nfc = "caf\u{00e9}";
        let nfd = "cafe\u{0301}";
        assert_ne!(nfc, nfd, "test strings must actually differ byte-for-byte");
        let digest_nfc = scoped_digest(RuntimeFieldScope::NameDescription, nfc);
        let digest_nfd = scoped_digest(RuntimeFieldScope::NameDescription, nfd);
        assert_ne!(
            digest_nfc, digest_nfd,
            "no Unicode normalization may be applied before hashing"
        );
    }

    #[test]
    fn document_hash_changes_with_a_single_trailing_byte() {
        let mut tampered = CANONICAL_EMPTY_DOCUMENT_BYTES.to_vec();
        tampered.push(b'\n');
        let mut hasher = Sha256::new();
        hasher.update(&tampered);
        let tampered_hex = digest_to_hex(&hasher.finalize().into());
        assert_ne!(
            tampered_hex,
            canonical_empty_document_sha256_hex(),
            "appending one byte must change the exact-content hash"
        );
    }

    // --- Parse: absence, malformed, tamper cases ---

    #[test]
    fn parse_empty_document_succeeds_and_is_empty() {
        let snapshot =
            parse_document(CANONICAL_EMPTY_DOCUMENT_BYTES, None).expect("valid empty doc");
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.manifest_id(), EMPTY_MANIFEST_ID);
    }

    #[test]
    fn refresh_with_no_bytes_publishes_empty_snapshot_and_returns_absent() {
        let manager = ManifestManager::new();
        let err = manager.refresh(None, None).unwrap_err();
        assert_eq!(err, ManifestFault::Absent);
        assert!(manager.current().is_empty());
    }

    #[test]
    fn parse_rejects_non_utf8_bytes_as_unreadable() {
        let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
        assert_eq!(
            parse_document(bytes, None).unwrap_err(),
            ManifestFault::Unreadable
        );
    }

    #[test]
    fn parse_rejects_garbage_json_as_malformed() {
        let bytes = b"not json at all {{{";
        assert_eq!(
            parse_document(bytes, None).unwrap_err(),
            ManifestFault::Malformed
        );
    }

    #[test]
    fn parse_rejects_unknown_field_as_malformed() {
        let bytes = br#"{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":null,"entries":[],"test_fixture_schema":"khive-secret-gate-test-fixture-v1"}"#;
        assert_eq!(
            parse_document(bytes, None).unwrap_err(),
            ManifestFault::Malformed,
            "the test-fixture marker field must never be accepted by the production parser"
        );
    }

    #[test]
    fn parse_rejects_wrong_schema_version() {
        let bytes = br#"{"schema_version":2,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":null,"entries":[]}"#;
        assert_eq!(
            parse_document(bytes, None).unwrap_err(),
            ManifestFault::UnknownSchemaVersion
        );
    }

    #[test]
    fn parse_rejects_unsupported_algorithm() {
        let bytes = br#"{"schema_version":1,"manifest_id":"x","algorithm":"md5","corpus_identity_sha256":null,"entries":[]}"#;
        assert_eq!(
            parse_document(bytes, None).unwrap_err(),
            ManifestFault::UnsupportedAlgorithm
        );
    }

    #[test]
    fn parse_rejects_truncated_digest() {
        let bytes = br#"{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":"aa","entries":[{"field_scope":"record-content","digest_sha256":"deadbeef","overridden_detector":"x"}]}"#;
        let expected = {
            let mut h = Sha256::new();
            h.update(b"aa");
            digest_to_hex(&h.finalize().into())
        };
        let bytes = String::from_utf8(bytes.to_vec())
            .unwrap()
            .replace("\"aa\"", &format!("\"{expected}\""));
        assert_eq!(
            parse_document(bytes.as_bytes(), Some(digest_from_hex(&expected).unwrap()))
                .unwrap_err(),
            ManifestFault::TruncatedDigest,
            "digest_sha256 shorter than 64 hex chars must fail closed"
        );
    }

    #[test]
    fn parse_rejects_duplicate_conflicting_entries() {
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "same-value",
        ));
        let corpus = digest_to_hex(&scoped_digest(RuntimeFieldScope::RecordContent, "corpus"));
        let bytes = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":"{corpus}","entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}},
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"b"}}
            ]}}"#
        );
        let expected = digest_from_hex(&corpus).unwrap();
        assert_eq!(
            parse_document(bytes.as_bytes(), Some(expected)).unwrap_err(),
            ManifestFault::DuplicateConflicting
        );
    }

    #[test]
    fn parse_collapses_byte_identical_duplicate_entries() {
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "same-value",
        ));
        let corpus = digest_to_hex(&scoped_digest(RuntimeFieldScope::RecordContent, "corpus"));
        let bytes = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":"{corpus}","entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}},
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}}
            ]}}"#
        );
        let expected = digest_from_hex(&corpus).unwrap();
        let snapshot =
            parse_document(bytes.as_bytes(), Some(expected)).expect("identical dup collapses");
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.entries.len(), 1);
    }

    #[test]
    fn parse_rejects_missing_expected_corpus_identity() {
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "same-value",
        ));
        let bytes = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":null,"entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}}
            ]}}"#
        );
        assert_eq!(
            parse_document(bytes.as_bytes(), None).unwrap_err(),
            ManifestFault::MissingExpectedCorpusIdentity
        );
    }

    #[test]
    fn parse_rejects_corpus_identity_mismatch() {
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "same-value",
        ));
        let corpus = digest_to_hex(&scoped_digest(RuntimeFieldScope::RecordContent, "corpus"));
        let bytes = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":"{corpus}","entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}}
            ]}}"#
        );
        let wrong_expected = scoped_digest(RuntimeFieldScope::RecordContent, "not-the-corpus");
        assert_eq!(
            parse_document(bytes.as_bytes(), Some(wrong_expected)).unwrap_err(),
            ManifestFault::CorpusIdentityMismatch
        );
    }

    #[test]
    fn refresh_failure_publishes_empty_snapshot_not_stale_state() {
        let manager = ManifestManager::new();
        // First, a successful non-empty load.
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "same-value",
        ));
        let corpus = digest_to_hex(&scoped_digest(RuntimeFieldScope::RecordContent, "corpus"));
        let good = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":"{corpus}","entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}}
            ]}}"#
        );
        let expected = digest_from_hex(&corpus).unwrap();
        manager
            .refresh(Some(good.as_bytes()), Some(expected))
            .expect("first load succeeds");
        assert!(!manager.current().is_empty());

        // Then a malformed refresh must evict the previously valid snapshot.
        let bad = b"not json";
        let err = manager.refresh(Some(bad), None).unwrap_err();
        assert_eq!(err, ManifestFault::Malformed);
        assert!(
            manager.current().is_empty(),
            "a failed refresh must never leave stale non-empty state live"
        );
    }

    /// ADR-115 Amendment 1's one-snapshot invariant: a candidate clones the
    /// manager's live snapshot `Arc` exactly once before its first field
    /// scan and uses that one clone for every scan and finalization
    /// decision. A `refresh` that swaps the manager's live snapshot after
    /// that clone was taken must never mutate the already-cloned `Arc` — the
    /// candidate's in-flight decision stays pinned to the snapshot it
    /// started with, so a concurrent manifest refresh can never straddle one
    /// candidate's decision (MatrixCaseKind::OneSnapshotRefreshRace).
    #[test]
    fn snapshot_taken_before_refresh_is_unaffected_by_a_later_refresh() {
        let manager = ManifestManager::new();
        let digest = digest_to_hex(&scoped_digest(
            RuntimeFieldScope::RecordContent,
            "race-fixture-value",
        ));
        let corpus = digest_to_hex(&scoped_digest(RuntimeFieldScope::RecordContent, "corpus"));
        let first = format!(
            r#"{{"schema_version":1,"manifest_id":"race-v1","algorithm":"sha256","corpus_identity_sha256":"{corpus}","entries":[
                {{"field_scope":"record-content","digest_sha256":"{digest}","overridden_detector":"a"}}
            ]}}"#
        );
        let expected = digest_from_hex(&corpus).unwrap();
        manager
            .refresh(Some(first.as_bytes()), Some(expected))
            .expect("first load succeeds");

        // The candidate's one clone, taken before the field scan begins.
        let candidate_snapshot = manager.current();
        assert_eq!(candidate_snapshot.manifest_id(), "race-v1");
        assert!(candidate_snapshot
            .lookup(RuntimeFieldScope::RecordContent, "race-fixture-value")
            .is_some());

        // A refresh lands concurrently, publishing a completely different
        // (empty) manifest as the manager's new live snapshot.
        manager
            .refresh(None, None)
            .expect_err("absent bytes is a refresh fault by design");
        assert!(
            manager.current().is_empty(),
            "manager's live state advanced"
        );

        // The candidate's already-cloned Arc must still resolve exactly as
        // it did at clone time — untouched by the manager's later refresh.
        assert_eq!(candidate_snapshot.manifest_id(), "race-v1");
        assert!(
            candidate_snapshot
                .lookup(RuntimeFieldScope::RecordContent, "race-fixture-value")
                .is_some(),
            "a snapshot cloned before refresh must not observe a later refresh"
        );
    }

    // --- Lookup: exact match, one-byte miss, wrong-scope miss ---

    #[test]
    fn lookup_exact_match_hits_on_empty_snapshot_miss_otherwise() {
        let empty = ManifestSnapshot::empty();
        assert!(empty
            .lookup(RuntimeFieldScope::RecordContent, "anything")
            .is_none());
    }

    #[test]
    fn fixture_lookup_exact_match_hits() {
        let fixture = TestOnlyManifestFixture::new();
        let snapshot = fixture.snapshot();
        let hit = snapshot.lookup(fixture.field_scope(), fixture.exact_value());
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().overridden_detector, "aws-access-key-id");
    }

    #[test]
    fn fixture_lookup_misses_on_one_byte_mutation() {
        let fixture = TestOnlyManifestFixture::new();
        let snapshot = fixture.snapshot();
        let mut mutated = fixture.exact_value().to_string();
        mutated.push('X');
        assert!(snapshot.lookup(fixture.field_scope(), &mutated).is_none());
    }

    #[test]
    fn fixture_lookup_misses_on_wrong_scope() {
        let fixture = TestOnlyManifestFixture::new();
        let snapshot = fixture.snapshot();
        assert!(snapshot
            .lookup(RuntimeFieldScope::JsonProperties, fixture.exact_value())
            .is_none());
    }

    // --- resolve_match: none / single / multiple distinct ---

    #[test]
    fn resolve_match_returns_none_on_no_candidate_hit() {
        let snapshot = ManifestSnapshot::empty();
        let scanned = [(RuntimeFieldScope::RecordContent, "anything")];
        assert_eq!(resolve_match(&snapshot, &scanned).unwrap(), None);
    }

    #[test]
    fn resolve_match_returns_single_match() {
        let fixture = TestOnlyManifestFixture::new();
        let snapshot = fixture.snapshot();
        let scanned = [(fixture.field_scope(), fixture.exact_value())];
        let result = resolve_match(&snapshot, &scanned).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn resolve_match_fails_closed_on_two_distinct_matches() {
        // Build a snapshot with two distinct entries, then scan candidates
        // that hit both — this must fail closed, never pick one.
        let value_a = "candidate-value-a";
        let value_b = "candidate-value-b";
        let digest_a = scoped_digest(RuntimeFieldScope::RecordContent, value_a);
        let digest_b = scoped_digest(RuntimeFieldScope::NameDescription, value_b);
        let mut entries = HashMap::new();
        entries.insert(
            (RuntimeFieldScope::RecordContent, digest_a),
            ManifestEntryMeta {
                overridden_detector: "a".to_string(),
            },
        );
        entries.insert(
            (RuntimeFieldScope::NameDescription, digest_b),
            ManifestEntryMeta {
                overridden_detector: "b".to_string(),
            },
        );
        let snapshot = ManifestSnapshot {
            manifest_id: Arc::from("test-multi"),
            entries,
        };
        let scanned = [
            (RuntimeFieldScope::RecordContent, value_a),
            (RuntimeFieldScope::NameDescription, value_b),
        ];
        assert_eq!(
            resolve_match(&snapshot, &scanned).unwrap_err(),
            ManifestFault::MultipleMatches
        );
    }

    #[test]
    fn resolve_match_same_distinct_match_twice_is_not_multiple() {
        let fixture = TestOnlyManifestFixture::new();
        let snapshot = fixture.snapshot();
        let scanned = [
            (fixture.field_scope(), fixture.exact_value()),
            (fixture.field_scope(), fixture.exact_value()),
        ];
        let result = resolve_match(&snapshot, &scanned).unwrap();
        assert!(result.is_some());
    }

    // --- Fixture non-deployability and disclaimer ---

    #[test]
    fn fixture_disclaimer_matches_amendment_sentence_exactly() {
        assert_eq!(
            REQUIRED_DISCLAIMER,
            "This fixture is not evidence of operator adjudication."
        );
    }

    #[test]
    fn fixture_schema_marker_is_never_a_valid_production_field() {
        let fixture = TestOnlyManifestFixture::new();
        assert_eq!(fixture.schema_marker(), "khive-secret-gate-test-fixture-v1");
        // Same assertion as parse_rejects_unknown_field_as_malformed, phrased
        // from the fixture's own marker constant to keep both in lockstep.
        let bytes = format!(
            r#"{{"schema_version":1,"manifest_id":"x","algorithm":"sha256","corpus_identity_sha256":null,"entries":[],"test_fixture_schema":"{}"}}"#,
            fixture.schema_marker()
        );
        assert_eq!(
            parse_document(bytes.as_bytes(), None).unwrap_err(),
            ManifestFault::Malformed
        );
    }

    #[test]
    fn fixture_value_is_not_a_contiguous_literal_in_source() {
        // Structural guard: the assembled value must actually be non-empty and
        // shaped like a credential, proving `new()` assembles rather than
        // returning a placeholder — the runtime-assembly requirement itself
        // is enforced by source review (no contiguous literal appears above),
        // this just pins the assembled shape.
        let fixture = TestOnlyManifestFixture::new();
        assert!(fixture.exact_value().starts_with("AKIA"));
        assert_eq!(fixture.exact_value().len(), 25);
    }
}
