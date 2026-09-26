use khive_runtime::error::RuntimeError;
use khive_runtime::secret_gate::{check, check_json};

const CODE: &str = "`CryptoX509::verifyKeyUsage<V2>()`";
const ENV: &str = "AZURE_GCP_X509_JWT_OIDC_PUBKEY_SHA256_V2_FILE_PATH";
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const LATEX: &str = "\\mathsf{KeyValueProjection}_{Q^2}(x)";
const RESOURCE: &str = "arn:aws:iam::123456789012:role/DevOps/KeyRotation/PolicyReadOnly";
fn opaque() -> String {
    joined(&[
        "ABCDEF", "GHIJKL", "MNOPQR", "STUVWX", "YZabcd", "efghij", "klmn",
    ])
}

fn joined(parts: &[&str]) -> String {
    parts.concat()
}

fn accepted(text: &str) {
    assert!(check(text).is_ok(), "unexpected refusal: {:?}", check(text));
}

fn refused_by(text: &str, detector: &str) {
    match check(text) {
        Err(RuntimeError::SecretDetected(found)) => assert_eq!(found.detector, detector, "{text}"),
        other => panic!("expected {detector} refusal for {text}: {other:?}"),
    }
}

#[test]
fn code_identifier_beside_trigger_is_accepted() {
    accepted(&format!("the key parser cites {CODE}"));
}

#[test]
fn code_identifier_slot_with_credential_is_refused() {
    refused_by(
        &format!("the key parser cites `sk_live_{}`", "A".repeat(30)),
        "stripe-secret-key",
    );
}

#[test]
fn environment_name_beside_trigger_is_accepted() {
    accepted(&format!("the key is {ENV}"));
}

#[test]
fn environment_name_slot_with_credential_is_refused() {
    refused_by(
        &joined(&["the ke", "y is A", "KIAABC", "DEFGHI", "JKLMNO", "P"]),
        "aws-access-key-id",
    );
}

#[test]
fn sha256_digest_beside_trigger_is_accepted() {
    let prose = format!("sha256 key digest {DIGEST}");
    accepted(&prose);
    assert!(check_json(&serde_json::json!({"body": prose})).is_ok());
}

#[test]
fn sha256_digest_slot_with_credential_is_refused() {
    refused_by(
        &format!("sha256 key digest {}", opaque()),
        "high-entropy-token",
    );
    refused_by(
        &format!("api_key: sha256 digest {DIGEST}"),
        "hex-credential-token",
    );
}

#[test]
fn latex_macro_beside_trigger_is_accepted() {
    accepted(&format!("api key: {LATEX}"));
}

#[test]
fn latex_macro_slot_with_credential_is_refused() {
    refused_by(&format!("api key: ghp_{}", "A".repeat(36)), "github-token");
}

#[test]
fn cloud_resource_name_beside_trigger_is_accepted() {
    accepted(&format!("api key reference: {RESOURCE}"));
}

#[test]
fn cloud_resource_name_slot_with_credential_is_refused() {
    refused_by(
        &format!("api key reference: {}", opaque()),
        "high-entropy-token",
    );
}

#[test]
fn credential_runs_inside_reference_carriers_still_refuse() {
    let provider = format!("ghp_{}", "A".repeat(36));
    refused_by(
        &format!("api key reference: arn:aws:iam::123456789012:role/{provider}"),
        "github-token",
    );
    refused_by(
        &format!("api key: \\mathsf{{{provider}}}_{{Q^2}}"),
        "github-token",
    );
    refused_by(
        &format!("the key parser cites `CryptoX509::{provider}()`"),
        "github-token",
    );
    refused_by(
        &joined(&[
            "the ke", "y is A", "ZURE_G", "CP_AKI", "AABCDE", "FGHIJK", "LMNOP_", "FILE_P", "ATH",
        ]),
        "aws-access-key-id",
    );
}

#[test]
fn credential_detector_verdicts_unchanged() {
    let cases = [
        (
            joined(&["AKIAAB", "CDEFGH", "IJKLMN", "OP"]),
            "aws-access-key-id",
        ),
        (format!("ghp_{}", "A".repeat(36)), "github-token"),
        (format!("sk-proj-{}", "A".repeat(80)), "openai-api-key"),
        (format!("sk-ant-{}", "A".repeat(108)), "anthropic-api-key"),
        (format!("sk_live_{}", "A".repeat(30)), "stripe-secret-key"),
        (
            format!("rk_live_{}", "A".repeat(30)),
            "stripe-restricted-key",
        ),
        (format!("fm2_{}", "A".repeat(20)), "fly-token"),
        ("FlyV1 AAAA".to_owned(), "fly-token"),
        (format!("vercel_{}", "A".repeat(20)), "vercel-token"),
        (format!("xoxb-{}", "A".repeat(40)), "slack-token"),
        (
            format!("AGE-SECRET-KEY-{}", "A".repeat(60)),
            "age-secret-key",
        ),
        (format!("sk-{}", "A".repeat(30)), "openai-api-key"),
        (
            format!(
                "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
                "A".repeat(64)
            ),
            "pem-private-key",
        ),
        (
            joined(&[
                "eyJhbG", "ciOiJu", "b25lIn", "0.eyJz", "dWIiOi", "JkZW1v", "In0.fa", "ke_sig",
                "nature",
            ]),
            "jwt",
        ),
        (
            joined(&[
                "postgr", "es://r", "eader:", "plaint", "est@lo", "calhos", "t/db",
            ]),
            "url-userinfo",
        ),
        (
            joined(&[
                "api_ke", "y 550e", "8400-e", "29b-41", "d4-a71", "6-4466", "554400", "00",
            ]),
            "uuid-near-trigger",
        ),
        (
            joined(&[
                "secret", " 01234", "56789a", "bcdef0", "123456", "789abc", "def",
            ]),
            "hex-credential-token",
        ),
        (format!("secret {}", opaque()), "high-entropy-token"),
        (
            format!("secret sha256-{}", "A".repeat(43)),
            "content-hash-near-trigger",
        ),
    ];
    for (text, detector) in cases {
        refused_by(&text, detector);
    }
}
