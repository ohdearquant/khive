use super::*;
use serde_json::json;

#[cfg(unix)]
#[tokio::test]
async fn graphql_review_read_binds_variables_and_rejects_partial_errors() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let _lock = crate::cache::ENV_MUTEX.lock().await;
    struct RestorePath(Option<std::ffi::OsString>);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            if let Some(path) = &self.0 {
                std::env::set_var("PATH", path);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }
    let _restore = RestorePath(std::env::var_os("PATH"));
    let dir = tempfile::tempdir().unwrap();
    let quoted = |name: &str| {
        format!(
            "'{}'",
            dir.path()
                .join(name)
                .display()
                .to_string()
                .replace('\'', "'\\''")
        )
    };
    std::fs::write(dir.path().join("gh"), format!(
            "#!/bin/sh\n[ \"$GH_TOKEN\" = 'fixture-secret' ] || exit 91\n/bin/cat > {}\nprintf '%s\\n' \"$@\" > {}\n/bin/cat {}\n",
            quoted("request"), quoted("argv"), quoted("response"),
        )).unwrap();
    std::fs::set_permissions(
        dir.path().join("gh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Keep real tools available inside this isolated case while shadowing gh.
    let shimmed = match std::env::var_os("PATH") {
        Some(prior) => {
            let mut paths = vec![dir.path().to_path_buf()];
            paths.extend(std::env::split_paths(&prior));
            std::env::join_paths(paths).unwrap()
        }
        None => dir.path().as_os_str().to_os_string(),
    };
    std::env::set_var("PATH", shimmed);
    assert!(
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success()),
        "this isolated case must leave real git resolvable alongside its gh shim"
    );
    let pr = json!({"headRefOid":"a".repeat(40),"reviewDecision":"APPROVED"});
    let data = json!({"data":{"repository":{"pullRequest":pr}}});
    for (response, expected) in [
        (format!("HTTP/2.0 200 OK\n\n{data}"), Ok(pr.clone())),
        (
            format!(
                "HTTP/2.0 200 OK\n\n{}",
                json!({"data":data["data"],"errors":[{"message":"denied fixture-secret"}]})
            ),
            Err(RemoteError::InvalidResponse),
        ),
        (
            "HTTP/2.0 200 OK\n\n{\"data\":{\"repository\":null}}".into(),
            Err(RemoteError::InvalidResponse),
        ),
        (
            "HTTP/2.0 503 Unavailable\n\n{}".into(),
            Err(RemoteError::Unavailable),
        ),
    ] {
        std::fs::write(dir.path().join("response"), response).unwrap();
        let actual = GhTransport::new("git".into())
            .review_decision("fixture-secret", "owner/project", 42)
            .await;
        assert_eq!(actual, expected);
        assert!(!format!("{actual:?}").contains("fixture-secret"));
        let request: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("request")).unwrap()).unwrap();
        assert_eq!(
            request["variables"],
            json!({"owner":"owner","name":"project","number":42})
        );
        let query = request["query"].as_str().unwrap();
        assert!(query.starts_with("query("));
        assert!(query.contains("headRefOid reviewDecision"));
        assert!(!request.to_string().contains("fixture-secret"));
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("--method\nPOST\n"));
        assert!(argv.ends_with("--input\n-\ngraphql\n"));
        assert!(!argv.contains("fixture-secret"));
    }
}

#[test]
fn api_redacts_json_escaped_credentials_in_keys_and_values() {
    let response = b"HTTP/2.0 200 OK\r\n\r\n{\"s\\u0065cret\": [\"prefix secret suffix\", {\"nested\": \"s\\u0065cret\"}]}";
    let value = parse_api_response(true, response.to_vec(), "secret", true).unwrap();
    assert_eq!(
        value,
        json!({"[redacted]": ["prefix [redacted] suffix", {"nested":"[redacted]"}]})
    );
    assert!(!value.to_string().contains("secret"));
}

#[test]
fn api_ambiguous_effect_replies_are_unknown_and_explicit_refusals_are_not() {
    for response in [
        b"HTTP/2.0 503 Unavailable\n\n{}".as_slice(),
        b"HTTP/2.0 200 OK\n\nnot-json",
        b"no response headers",
    ] {
        assert_eq!(
            parse_api_response(true, response.to_vec(), "secret", true),
            Err(RemoteError::Unknown)
        );
    }
    assert_eq!(
        parse_api_response(
            false,
            b"HTTP/2.0 409 Conflict\n\n{}".to_vec(),
            "secret",
            true
        ),
        Err(RemoteError::Refused)
    );
    assert_eq!(
        parse_api_response(false, b"HTTP/2.0 200 OK\n\n{}".to_vec(), "secret", true),
        Err(RemoteError::Unknown)
    );
    assert_eq!(
        parse_api_response(
            true,
            b"HTTP/2.0 200 OK\n\nnot-json".to_vec(),
            "secret",
            false
        ),
        Err(RemoteError::InvalidResponse)
    );
}

#[test]
fn git_and_gh_commands_keep_their_argv_and_environment() {
    // The environment carries PATH, which other cases rewrite while holding this guard.
    let _guard = crate::cache::ENV_MUTEX.blocking_lock();
    let git = git_command(Path::new("git"), Path::new("/fixture/repo"), None);
    let (args, envs) = git_env::describe(git.as_std());
    assert_eq!(git.as_std().get_program(), "git");
    let settings = [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgsign=false",
        "credential.helper=",
        "core.sshCommand=/usr/bin/false",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "http.sslVerify=true",
        "http.followRedirects=false",
        "http.extraHeader=",
        "push.followTags=false",
        "push.recurseSubmodules=no",
    ];
    let mut expected_args = Vec::new();
    for setting in settings {
        expected_args.extend(["-c", setting]);
    }
    expected_args.extend(["-C", "/fixture/repo"]);
    assert_eq!(args, expected_args);
    let mut expected_envs = std::collections::BTreeMap::new();
    for (key, value) in [
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
    ] {
        expected_envs.insert(key.to_owned(), Some(value.to_owned()));
    }
    if let Some(path) = std::env::var_os("PATH") {
        let path = path.to_string_lossy().into_owned();
        expected_envs.insert("PATH".to_owned(), Some(path));
    }
    assert_eq!(envs, expected_envs);

    let gh = isolated("gh");
    let (gh_args, gh_envs) = git_env::describe(gh.as_std());
    assert_eq!(gh.as_std().get_program(), "gh");
    assert!(gh_args.is_empty());
    assert_eq!(gh_envs, expected_envs);
}
