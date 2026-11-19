use super::*;

include!("source_tests/source_parse_tests.rs");

include!("source_tests/remote_slug_tests.rs");

fn init_repo_with_origin(dir: &Path, origin: &str) {
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }
}

include!("source_tests/repository_identity_tests.rs");
