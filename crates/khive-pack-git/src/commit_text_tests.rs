use super::{decode_commit_message, walk_commits};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn isolated_case(test_name: &str) -> bool {
    const CHILD: &str = "KHIVE_COMMIT_TEXT_ORACLE_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(test_name) {
        return false;
    }
    let home = tempfile::tempdir().unwrap();
    let empty_config = home.path().join("empty.gitconfig");
    std::fs::write(&empty_config, b"").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test_name, "--nocapture", "--test-threads=1"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env(CHILD, test_name)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = command.output().expect("start isolated test executable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("running 1 test"),
        "empty/wrong selection:\n{stdout}\n{stderr}"
    );
    assert!(
        output.status.success(),
        "isolated test failed:\n{stdout}\n{stderr}"
    );
    true
}

fn git(repo: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture requires Git on PATH");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "fixture Git command {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn commit_fixture(encoding: Option<&str>, message: &[u8]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(
        repo,
        &["init", "--quiet", "--object-format=sha1", "--template="],
        b"",
    );
    git(repo, &["config", "i18n.logOutputEncoding", "UTF-8"], b"");
    git(repo, &["config", "log.showSignature", "false"], b"");
    let tree = String::from_utf8(git(
        repo,
        &["hash-object", "-w", "-t", "tree", "--stdin"],
        b"",
    ))
    .unwrap();
    let header = format!(
        "tree {}\nauthor Synthetic <author@example.invalid> 946684800 +0000\ncommitter Synthetic <committer@example.invalid> 946684800 +0000\n",
        tree.trim()
    );
    let mut raw = header.into_bytes();
    if let Some(encoding) = encoding {
        raw.extend_from_slice(format!("encoding {encoding}\n").as_bytes());
    }
    raw.push(b'\n');
    raw.extend_from_slice(message);
    let sha = String::from_utf8(git(
        repo,
        &["hash-object", "-w", "-t", "commit", "--stdin"],
        &raw,
    ))
    .unwrap()
    .trim()
    .to_owned();
    assert_eq!(sha.len(), 40, "fixture is explicitly SHA-1");
    (dir, sha)
}

fn formatted_fields(repo: &Path, sha: &str) -> (String, String) {
    let output = git(
        repo,
        &[
            "log",
            "-1",
            "--encoding=UTF-8",
            "--pretty=format:%s%x00%b",
            sha,
            "--",
        ],
        b"",
    );
    let text = String::from_utf8(output).expect("independent Git UTF-8 oracle");
    let (subject, body) = text.split_once('\0').expect("oracle field boundary");
    (subject.to_owned(), body.trim_end_matches('\n').to_owned())
}

fn require_walk_matches(repo: &Path, sha: &str, subject: &str, body: &str) {
    let oracle = formatted_fields(repo, sha);
    assert_eq!(
        oracle,
        (subject.to_owned(), body.to_owned()),
        "fixture/oracle premise"
    );
    let commits = walk_commits(repo, None, sha).expect("actual ingest walker");
    assert_eq!(commits.len(), 1, "do not silently omit the fixture");
    assert_eq!(commits[0].sha, sha);
    assert_eq!(
        commits[0].subject, subject,
        "walker changed Git's decoded subject"
    );
    assert_eq!(commits[0].body, body, "walker changed Git's decoded body");
}

#[test]
fn latin1_commit_preserves_declared_encoding() {
    if isolated_case("ingest::commit_text_tests::latin1_commit_preserves_declared_encoding") {
        return;
    }
    let (dir, sha) = commit_fixture(Some("ISO-8859-1"), b"caf\xe9\n\nbody ol\xe9\n");
    require_walk_matches(dir.path(), &sha, "café", "body olé");
}

#[test]
fn unsupported_declared_encoding_is_refused() {
    let error = decode_commit_message(b"encoding X-UNSUPPORTED", b"caf\xe9\n")
        .expect_err("an unsupported declared encoding must not be decoded with replacement");
    assert_eq!(
        error.to_string(),
        "git commit declares unsupported message encoding"
    );
}

#[test]
fn utf8_commit_is_a_positive_control() {
    if isolated_case("ingest::commit_text_tests::utf8_commit_is_a_positive_control") {
        return;
    }
    let (dir, sha) = commit_fixture(None, "café\n\nbody olé\n".as_bytes());
    require_walk_matches(dir.path(), &sha, "café", "body olé");
}

#[test]
fn multiline_first_paragraph_retains_git_subject_semantics() {
    if isolated_case(
        "ingest::commit_text_tests::multiline_first_paragraph_retains_git_subject_semantics",
    ) {
        return;
    }
    let (dir, sha) = commit_fixture(None, b"wrapped\nsubject (#123)\n\nbody\n");
    require_walk_matches(dir.path(), &sha, "wrapped subject (#123)", "body");
}
