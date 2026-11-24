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
fn api_spent_allowance_is_its_own_refusal_and_carries_the_reset() {
    let spent = RemoteError::RateLimited {
        reset: Some(1_795_346_246),
    };
    for status in ["403 Forbidden", "429 Too Many Requests"] {
        for effect in [true, false] {
            let response = format!(
                "HTTP/2.0 {status}\r\nContent-Type: application/json\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1795346246\r\n\r\n{{}}"
            );
            assert_eq!(
                parse_api_response(false, response.into_bytes(), "secret", effect),
                Err(spent),
                "{status} effect={effect}"
            );
        }
    }
    // Header names are matched without regard to case, and a missing or unreadable reset
    // still names the cause.
    for (headers, reset) in [
        ("x-ratelimit-remaining: 0\nx-ratelimit-reset: 12", Some(12)),
        ("X-RateLimit-Remaining: 0", None),
        ("X-RateLimit-Remaining: 0\nX-RateLimit-Reset: soon", None),
    ] {
        let response = format!("HTTP/2.0 403 Forbidden\n{headers}\n\n{{}}");
        assert_eq!(
            parse_api_response(false, response.into_bytes(), "secret", false),
            Err(RemoteError::RateLimited { reset }),
            "{headers}"
        );
    }
    // Controls: allowance left, no allowance header, and another refusal status with a zero
    // count are the plain refusal; a zero count on a success is a success.
    for response in [
        "HTTP/2.0 403 Forbidden\nX-Ratelimit-Remaining: 12\n\n{}",
        "HTTP/2.0 403 Forbidden\n\n{}",
        "HTTP/2.0 404 Not Found\nX-Ratelimit-Remaining: 0\n\n{}",
    ] {
        assert_eq!(
            parse_api_response(false, response.as_bytes().to_vec(), "secret", false),
            Err(RemoteError::Refused),
            "{response}"
        );
    }
    assert_eq!(
        parse_api_response(
            true,
            b"HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 0\n\n{}".to_vec(),
            "secret",
            false
        ),
        Ok(json!({}))
    );
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

#[test]
fn pack_files_move_with_each_index_last_and_no_keep_file() {
    let names = [
        "pack-b.idx",
        "pack-a.pack",
        "tmp_pack_x",
        "pack-a.idx",
        "pack-b.keep",
        "pack-a.rev",
        "pack-b.pack",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(
        pack_files_in_order(names),
        [
            "pack-a.pack",
            "pack-a.rev",
            "pack-b.pack",
            "pack-a.idx",
            "pack-b.idx"
        ]
    );
}

#[test]
fn received_objects_are_linked_into_the_store_and_nothing_else_is() {
    let root = tempfile::tempdir().unwrap();
    let objects = root.path().join("objects");
    let incoming = objects.join("khive-incoming-test");
    let loose = "0".repeat(38);
    for dir in ["ab", "cd", "info", "pack", "not-hex"] {
        std::fs::create_dir_all(incoming.join(dir)).unwrap();
    }
    std::fs::create_dir_all(objects.join("cd")).unwrap();
    std::fs::write(incoming.join("ab").join(&loose), "received").unwrap();
    std::fs::write(incoming.join("ab").join("tmp_obj_x"), "partial").unwrap();
    // The store already holds this name: its copy stays.
    std::fs::write(incoming.join("cd").join(&loose), "received").unwrap();
    std::fs::write(objects.join("cd").join(&loose), "held").unwrap();
    std::fs::write(incoming.join("info").join("alternates"), "/elsewhere").unwrap();
    std::fs::write(incoming.join("not-hex").join(&loose), "stray").unwrap();
    for name in ["pack-a.pack", "pack-a.idx", "pack-a.keep", "tmp_pack_x"] {
        std::fs::write(incoming.join("pack").join(name), name).unwrap();
    }

    migrate_incoming(&incoming, &objects).unwrap();

    let read = |path: &[&str]| {
        let mut full = objects.clone();
        full.extend(path);
        std::fs::read_to_string(full).ok()
    };
    assert_eq!(read(&["ab", &loose]).as_deref(), Some("received"));
    assert_eq!(read(&["cd", &loose]).as_deref(), Some("held"));
    assert_eq!(
        read(&["pack", "pack-a.pack"]).as_deref(),
        Some("pack-a.pack")
    );
    assert_eq!(read(&["pack", "pack-a.idx"]).as_deref(), Some("pack-a.idx"));
    for left in [
        &["ab", "tmp_obj_x"][..],
        &["info", "alternates"],
        &["not-hex", &loose],
        &["pack", "pack-a.keep"],
        &["pack", "tmp_pack_x"],
    ] {
        assert_eq!(read(left), None, "{left:?}");
    }
}

/// A reader that hands out at most `step` bytes per read, so a header block and a body arrive
/// split across reads.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl tokio::io::AsyncRead for Trickle<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let n = self.step.min(self.data.len()).min(buf.remaining());
        let (now, rest) = self.data.split_at(n);
        buf.put_slice(now);
        self.data = rest;
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn read_by_end_keeps_the_header_whole_and_the_body_by_its_end() {
    let body: Vec<u8> = (0..150_000_u32).map(|i| (i % 251) as u8).collect();
    for header in [
        &b"HTTP/2.0 200 OK\r\nContent-Type: text/plain\r\n\r\n"[..],
        b"HTTP/2.0 200 OK\nContent-Type: text/plain\n\n",
    ] {
        let mut response = header.to_vec();
        response.extend_from_slice(&body);
        let expected_head = &header[..header.len()
            - if header[header.len() - 2] == b'\r' {
                4
            } else {
                2
            }];
        for step in [3, 4096, 64 * 1024] {
            for limit in [0_u64, 1, 10, 70_000, 149_999, 150_000, 500_000] {
                let (head, kept, total) = read_by_end(
                    Trickle {
                        data: &response,
                        step,
                    },
                    limit,
                )
                .await
                .unwrap();
                assert_eq!(head, expected_head, "step {step} limit {limit}");
                assert_eq!(total, body.len() as u64, "step {step} limit {limit}");
                let keep = (limit as usize).min(body.len());
                assert_eq!(
                    kept,
                    &body[body.len() - keep..],
                    "step {step} limit {limit}"
                );
            }
        }
    }
    // A header block alone is an empty body; a header with no end past the bound is refused.
    let (head, kept, total) = read_by_end(&b"HTTP/2.0 204 No Content\n\n"[..], 10)
        .await
        .unwrap();
    assert_eq!(
        (head.as_slice(), kept.len(), total),
        (&b"HTTP/2.0 204 No Content"[..], 0, 0)
    );
    let endless = vec![b'h'; HEADER_LIMIT + 2];
    assert!(read_by_end(&endless[..], 10).await.is_err());
}

#[test]
fn redact_bytes_replaces_the_credential_and_a_cut_ending() {
    let token = "tok-0123456789";
    assert_eq!(
        redact_bytes(b"a tok-0123456789 b tok-0123456789".to_vec(), token, false),
        b"a [redacted] b [redacted]"
    );
    // The window starts inside an occurrence: its last four or more bytes are replaced.
    assert_eq!(
        redact_bytes(b"6789 rest".to_vec(), token, true),
        b"[redacted] rest"
    );
    assert_eq!(
        redact_bytes(b"0123456789 rest tok-0123456789".to_vec(), token, true),
        b"[redacted] rest [redacted]"
    );
    // Controls: three bytes are too few to be taken for an ending, a whole log is never read
    // as cut, and an empty credential changes nothing.
    assert_eq!(redact_bytes(b"789 rest".to_vec(), token, true), b"789 rest");
    assert_eq!(
        redact_bytes(b"6789 rest".to_vec(), token, false),
        b"6789 rest"
    );
    assert_eq!(redact_bytes(b"any".to_vec(), "", true), b"any");
}

#[test]
fn parse_download_reads_the_status() {
    let ok = b"HTTP/2.0 200 OK\r\nContent-Type: text/plain";
    assert_eq!(
        parse_download(true, ok, b"x y".to_vec(), 100),
        Ok(Download {
            kept: b"x y".to_vec(),
            total: 100
        })
    );
    for (status, expected) in [
        ("400 Bad Request", RemoteError::Refused),
        ("401 Unauthorized", RemoteError::Refused),
        ("403 Forbidden", RemoteError::Refused),
        ("404 Not Found", RemoteError::Refused),
        ("410 Gone", RemoteError::Refused),
        ("422 Unprocessable Entity", RemoteError::Refused),
        ("500 Internal Server Error", RemoteError::Unavailable),
        ("302 Found", RemoteError::Unavailable),
    ] {
        let headers = format!("HTTP/2.0 {status}\nContent-Type: application/json");
        assert_eq!(
            parse_download(false, headers.as_bytes(), b"{}".to_vec(), 2),
            Err(expected),
            "{status}"
        );
    }
    for status in ["403 Forbidden", "429 Too Many Requests"] {
        let headers = format!("HTTP/2.0 {status}\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 12");
        assert_eq!(
            parse_download(false, headers.as_bytes(), Vec::new(), 0),
            Err(RemoteError::RateLimited { reset: Some(12) }),
            "{status}"
        );
    }
    // A success status from a process that failed, no status line, and headers that are not
    // text are never read as a log.
    assert_eq!(
        parse_download(false, ok, b"x".to_vec(), 1),
        Err(RemoteError::Unavailable)
    );
    assert_eq!(
        parse_download(true, b"", b"x".to_vec(), 1),
        Err(RemoteError::Unavailable)
    );
    assert_eq!(
        parse_download(true, b"HTTP/2.0 200 \xFF", b"x".to_vec(), 1),
        Err(RemoteError::InvalidResponse)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn download_sends_the_credential_to_the_api_host_only_and_reads_the_log_by_its_end() {
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
    // Each invocation records its arguments and the configuration it read, and answers with the
    // response numbered for it.
    std::fs::write(
        dir.path().join("curl"),
        format!(
            "#!/bin/sh\nn=$(/bin/cat {count} 2>/dev/null || echo 0)\nn=$((n + 1))\necho \"$n\" > {count}\nprintf '%s\\n' \"$@\" > {argv}.$n\n/bin/cat > {config}.$n\n/bin/cat {response}.$n\n",
            count = quoted("count"),
            argv = quoted("argv"),
            config = quoted("config"),
            response = quoted("response"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        dir.path().join("curl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let shimmed = match std::env::var_os("PATH") {
        Some(prior) => {
            let mut paths = vec![dir.path().to_path_buf()];
            paths.extend(std::env::split_paths(&prior));
            std::env::join_paths(paths).unwrap()
        }
        None => dir.path().as_os_str().to_os_string(),
    };
    std::env::set_var("PATH", shimmed);

    let body = "line fixture-secret\nlast\n";
    let path = "repos/owner/project/actions/jobs/7/logs";
    let location = "https://logs.example.invalid/job/7?sig=abc";
    let redirect = format!("HTTP/2 302 \r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n");
    let log = format!("HTTP/2 200 \r\ncontent-type: text/plain\r\n\r\n{body}");
    let whole = Download {
        kept: body.as_bytes().to_vec(),
        total: body.len() as u64,
    };
    let cut = Download {
        kept: b"cret\nlast\n".to_vec(),
        total: body.len() as u64,
    };
    // An arm: its name, the response to each invocation in turn, the bound, the result.
    type Arm = (
        &'static str,
        Vec<String>,
        u64,
        Result<Download, RemoteError>,
    );
    let arms: Vec<Arm> = vec![
        (
            "whole",
            vec![redirect.clone(), log.clone()],
            1024,
            Ok(whole),
        ),
        (
            "by its end",
            vec![redirect.clone(), log.clone()],
            10,
            Ok(cut),
        ),
        (
            "log gone",
            vec![redirect.clone(), "HTTP/2 410 \r\n\r\n".into()],
            1024,
            Err(RemoteError::Refused),
        ),
        (
            "job unknown",
            vec!["HTTP/2 404 \r\n\r\n{}".into()],
            1024,
            Err(RemoteError::Refused),
        ),
        (
            "plain-text location",
            vec!["HTTP/2 302 \r\nlocation: http://logs.example.invalid/x\r\n\r\n".into()],
            1024,
            Err(RemoteError::InvalidResponse),
        ),
        (
            "no redirect",
            vec![log.clone()],
            1024,
            Err(RemoteError::InvalidResponse),
        ),
        (
            "allowance spent",
            vec![
                "HTTP/2 403 \r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 12\r\n\r\n{}".into(),
            ],
            1024,
            Err(RemoteError::RateLimited { reset: Some(12) }),
        ),
    ];
    let argv =
        "-q\n--silent\n--show-error\n--include\n--proto\n=https\n--max-redirs\n0\n--config\n-\n";
    for (arm, responses, limit, expected) in arms {
        for name in ["count", "argv.1", "argv.2", "config.1", "config.2"] {
            let _ = std::fs::remove_file(dir.path().join(name));
        }
        for (n, response) in responses.iter().enumerate() {
            std::fs::write(dir.path().join(format!("response.{}", n + 1)), response).unwrap();
        }
        let actual = GhTransport::new("git".into())
            .download("fixture-secret", path.into(), limit)
            .await;
        assert_eq!(actual, expected, "{arm}");
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("count").trim(), responses.len().to_string(), "{arm}");
        // The first process is sent to the API host with the credential, which never appears in
        // its arguments.
        assert_eq!(read("argv.1"), argv, "{arm}");
        assert_eq!(
            read("config.1"),
            format!(
                "url = \"https://api.github.com/{path}\"\nheader = \"User-Agent: khive\"\nheader = \"X-GitHub-Api-Version: 2022-11-28\"\nheader = \"Authorization: Bearer fixture-secret\"\n"
            ),
            "{arm}"
        );
        // The second is sent to the location alone and is never given the credential.
        if responses.len() == 2 {
            assert_eq!(read("argv.2"), argv, "{arm}");
            let config = read("config.2");
            assert_eq!(
                config,
                format!("url = \"{location}\"\nheader = \"User-Agent: khive\"\n"),
                "{arm}"
            );
            assert!(!config.contains("fixture-secret"), "{arm}");
            assert!(!config.contains("Authorization"), "{arm}");
        }
    }
}

#[test]
fn parse_redirect_takes_only_an_https_location_from_a_redirect() {
    let to = "https://logs.example.invalid/a?sig=b";
    for status in ["301", "302", "303", "307", "308"] {
        let reply = format!("HTTP/2 {status} \r\nLocation: {to}\r\n\r\n");
        assert_eq!(
            parse_redirect(true, reply.as_bytes()),
            Ok(to.to_string()),
            "{status}"
        );
    }
    // Header names are matched without regard to case, and bare line feeds are read too.
    assert_eq!(
        parse_redirect(
            true,
            format!("HTTP/1.1 302 Found\nlocation:  {to} \n\n").as_bytes()
        ),
        Ok(to.to_string())
    );
    for (reply, expected) in [
        ("HTTP/2 302 \r\n\r\n", RemoteError::InvalidResponse),
        (
            "HTTP/2 302 \r\nLocation: http://logs.example.invalid/a\r\n\r\n",
            RemoteError::InvalidResponse,
        ),
        (
            "HTTP/2 302 \r\nLocation: /relative\r\n\r\n",
            RemoteError::InvalidResponse,
        ),
        (
            "HTTP/2 302 \r\nLocation: https://logs.example.invalid/a b\r\n\r\n",
            RemoteError::InvalidResponse,
        ),
        (
            "HTTP/2 302 \r\nLocation: https://logs.example.invalid/a\"b\r\n\r\n",
            RemoteError::InvalidResponse,
        ),
        (
            "HTTP/2 302 \r\nLocation: https://logs.example.invalid/a\\b\r\n\r\n",
            RemoteError::InvalidResponse,
        ),
        ("HTTP/2 200 \r\n\r\nlog", RemoteError::InvalidResponse),
        ("HTTP/2 404 \r\n\r\n{}", RemoteError::Refused),
        ("HTTP/2 410 \r\n\r\n", RemoteError::Refused),
        (
            "HTTP/2 403 \r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 12\r\n\r\n",
            RemoteError::RateLimited { reset: Some(12) },
        ),
        ("HTTP/2 500 \r\n\r\n", RemoteError::Unavailable),
        ("", RemoteError::Unavailable),
    ] {
        assert_eq!(
            parse_redirect(true, reply.as_bytes()),
            Err(expected),
            "{reply:?}"
        );
    }
    // A process that failed is unavailable whatever it printed.
    let reply = format!("HTTP/2 302 \r\nLocation: {to}\r\n\r\n");
    assert_eq!(
        parse_redirect(false, reply.as_bytes()),
        Err(RemoteError::Unavailable)
    );
}

#[test]
fn curl_config_carries_the_credential_only_when_given_and_refuses_unquotable_text() {
    assert_eq!(
        curl_config("https://api.github.com/x", Some("tok_1"))
            .unwrap()
            .as_str(),
        "url = \"https://api.github.com/x\"\nheader = \"User-Agent: khive\"\nheader = \"X-GitHub-Api-Version: 2022-11-28\"\nheader = \"Authorization: Bearer tok_1\"\n"
    );
    assert_eq!(
        curl_config("https://logs.example.invalid/y?sig=z", None)
            .unwrap()
            .as_str(),
        "url = \"https://logs.example.invalid/y?sig=z\"\nheader = \"User-Agent: khive\"\n"
    );
    for (url, token) in [
        ("https://x.invalid/\"", None),
        ("https://x.invalid/\\", None),
        ("https://x.invalid/ y", None),
        ("https://x.invalid/\ny", None),
        ("https://x.invalid/\u{e9}", None),
        ("", None),
        ("https://x.invalid/", Some("a\"b")),
        ("https://x.invalid/", Some("a\nb")),
        ("https://x.invalid/", Some("")),
    ] {
        assert_eq!(
            curl_config(url, token).map(|config| config.to_string()),
            Err(RemoteError::Unavailable),
            "{url:?} {token:?}"
        );
    }
}
