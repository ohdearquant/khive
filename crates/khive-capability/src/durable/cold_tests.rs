use super::fixtures::*;
use super::*;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cold_issuers_with_distinct_aliases_verify_in_third_process() {
    const ROLE: &str = "KHIVE_ENVELOPE_COLD_ROLE";
    const DIR: &str = "KHIVE_ENVELOPE_COLD_DIR";
    const TEST: &str =
        "durable::cold_tests::cold_issuers_with_distinct_aliases_verify_in_third_process";
    let keys = keys();
    let catalog = catalog();
    let issuer = EnvelopeIssuer::new(&keys, &catalog);
    let verifier = EnvelopeVerifier::new(&keys, &catalog);
    if let Ok(role) = std::env::var(ROLE) {
        let path = PathBuf::from(std::env::var_os(DIR).expect("private child directory"));
        // Every child independently freezes the format fixture as well.
        assert_eq!(
            super::codec::encode(&fixed_body()).unwrap(),
            canonical_fixture_bytes()
        );
        match role.as_str() {
            "a" => {
                let parent = parent(&keys, &catalog);
                std::fs::write(path.join("parent"), parent.as_bytes()).unwrap();
                let mut grant = draft(2, "alice", "bob");
                grant.issuance_id = Uuid::from_u128(0);
                let child = issuer.delegate(&parent, grant).unwrap();
                std::fs::write(path.join("a"), child.as_bytes()).unwrap();
            }
            "b" => {
                let bytes = std::fs::read(path.join("parent")).unwrap();
                let parent = verifier
                    .verify(&bytes, Uuid::from_u128(101), Uuid::from_u128(102))
                    .unwrap();
                let mut grant = draft(3, "alice", "bob");
                grant.issuance_id = Uuid::from_u128(1);
                let child = issuer.delegate(&parent, grant).unwrap();
                std::fs::write(path.join("b"), child.as_bytes()).unwrap();
            }
            "c" => {
                let read = |name: &str| {
                    let bytes = std::fs::read(path.join(name)).unwrap();
                    verifier
                        .verify(&bytes, Uuid::from_u128(101), Uuid::from_u128(102))
                        .unwrap()
                };
                let parent = read("parent");
                let a = read("a");
                let b = read("b");
                verifier.verify_delegation_link(&a, &parent).unwrap();
                verifier.verify_delegation_link(&b, &parent).unwrap();
                assert_ne!(a.body().context.id, b.body().context.id);
                assert_ne!(a.body().native.parent, b.body().native.parent);
                assert_eq!(a.body().native.id, b.body().native.id);
                std::fs::write(
                    path.join("verified"),
                    b"two distinct contexts and aliases verified",
                )
                .unwrap();
            }
            _ => panic!("unknown child role"),
        }
        return;
    }
    // This harness launches three actual fresh test processes. C never issues.
    let directory = Directory(std::env::temp_dir().join(format!(
        "khive-envelope-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    for role in ["a", "b", "c"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, role)
            .env(DIR, &directory.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child {role}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        std::fs::read(directory.0.join("verified")).unwrap(),
        b"two distinct contexts and aliases verified"
    );
}
