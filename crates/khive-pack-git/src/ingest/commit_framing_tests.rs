use super::*;

#[test]
fn contributor_control_bytes_do_not_split_or_drop_commits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let tree = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "-w", "-t", "tree", "--stdin"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(tree.status.success());
    let tree = String::from_utf8(tree.stdout).unwrap().trim().to_string();

    let cases = [
        (
            "Author",
            "body separator",
            format!("first\n\npart{RECORD_SEP}tail\n"),
        ),
        (
            "Author",
            "subject separator",
            format!("second{RECORD_SEP}part\n\nbody\n"),
        ),
        (
            "Name\u{1f}Tail",
            "author separator",
            "third\n\nbody\n".to_string(),
        ),
    ];
    let mut parent: Option<String> = None;
    for (author, _, message) in &cases {
        let parent_header = parent
            .as_ref()
            .map(|sha| format!("parent {sha}\n"))
            .unwrap_or_default();
        let raw = format!(
                "tree {tree}\n{parent_header}author {author} <author@example.invalid> 1760000000 +0000\ncommitter Test <test@example.invalid> 1760000000 +0000\n\n{message}"
            );
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "hash-object",
                "--literally",
                "-w",
                "-t",
                "commit",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(raw.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        parent = Some(String::from_utf8(output.stdout).unwrap().trim().to_string());
    }
    let commits = walk_commits(repo, None, parent.as_deref().unwrap()).unwrap();
    assert_eq!(commits.len(), 3);
    assert_eq!(commits[0].subject, "first");
    assert_eq!(commits[0].body, format!("part{RECORD_SEP}tail"));
    assert_eq!(commits[1].subject, format!("second{RECORD_SEP}part"));
    assert_eq!(commits[1].body, "body");
    assert_eq!(commits[2].author, "Name\u{1f}Tail");
    assert_eq!(commits[2].author_email, "author@example.invalid");
}
