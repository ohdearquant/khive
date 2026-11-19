use super::*;

#[tokio::test]
async fn unmapped_actor_refuses_without_a_resolver() {
    let config = GitWriteSectionConfig {
        credential_resolver: vec!["/does/not/exist".to_string(), "{ref}".to_string()],
        ..Default::default()
    };
    let error = resolve_actor(&config, "unmapped").await.unwrap_err();
    assert_eq!(error.to_string(), "actor_unmapped");
    assert_eq!(format!("{error:?}"), "CredentialError");
}

#[tokio::test]
async fn invalid_programmatic_configuration_refuses_before_spawn() {
    let mut config = GitWriteSectionConfig {
        credential_resolver: vec![],
        ..Default::default()
    };
    config.actors.insert(
        "configured".to_string(),
        GitWriteActorConfig {
            name: "Example".to_string(),
            email: "example@example.invalid".to_string(),
            credential_ref: "example-reference".to_string(),
            platform_identity: "example-login".to_string(),
        },
    );
    assert!(resolve_actor(&config, "configured").await.is_err());
}

#[tokio::test]
async fn resolver_output_preserves_value_and_removes_one_line_ending() {
    for output in [
        b"synthetic-value".as_slice(),
        b"synthetic-value\n",
        b"synthetic-value\r\n",
    ] {
        let mut reader = output;
        let secret = read_secret(&mut reader).await.ok().unwrap();
        assert_eq!(&secret.bytes[..secret.len], b"synthetic-value");
    }
}

#[tokio::test]
async fn malformed_resolver_output_refuses() {
    for output in [b"".as_slice(), b"\n", b"value\nsecond", b"value\0", b"\xff"] {
        let mut reader = output;
        assert!(read_secret(&mut reader).await.is_err());
    }
}

#[tokio::test]
async fn resolver_output_is_bounded() {
    let at_limit = vec![b'x'; MAX_CREDENTIAL_BYTES];
    assert!(read_secret(&mut at_limit.as_slice()).await.is_ok());
    let above_limit = vec![b'x'; MAX_CREDENTIAL_BYTES + 1];
    assert!(read_secret(&mut above_limit.as_slice()).await.is_err());
}

#[cfg(unix)]
#[test]
fn failed_group_signal_requires_a_confirmed_absent_group() {
    let denied = || std::io::Error::from_raw_os_error(libc::EPERM);
    let absent = || std::io::Error::from_raw_os_error(libc::ESRCH);
    assert!(confirmed_group_cleanup(
        Ok(()),
        || panic!("unexpected probe"),
        || panic!("unexpected group census")
    )
    .is_ok());
    assert!(confirmed_group_cleanup(
        Err(absent()),
        || panic!("unexpected probe"),
        || panic!("unexpected group census")
    )
    .is_ok());
    assert!(confirmed_group_cleanup(Err(denied()), || Err(absent()), || false).is_ok());
    assert!(confirmed_group_cleanup(Err(denied()), || Ok(()), || true).is_err());
    assert!(confirmed_group_cleanup(Err(denied()), || Err(denied()), || false).is_err());
    #[cfg(target_os = "macos")]
    {
        assert!(confirmed_group_cleanup(Err(denied()), || Err(denied()), || true).is_ok());
        assert!(confirmed_group_cleanup(
            Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
            || Err(denied()),
            || true
        )
        .is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn resolver_spawn_retries_only_transient_executable_busy() {
    let config = GitWriteSectionConfig {
        credential_resolver: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf synthetic-value".to_owned(),
        ],
        ..Default::default()
    };
    let mut attempts = 0;
    let secret = resolve_reference_with_spawn(&config, "unused", |command| {
        attempts += 1;
        if attempts <= 2 {
            Err(std::io::ErrorKind::ExecutableFileBusy.into())
        } else {
            command.spawn()
        }
    })
    .await
    .expect("transient executable busy must retry");
    assert_eq!(secret.value(), "synthetic-value");
    assert_eq!(attempts, 3, "two busy failures plus one successful spawn");

    let mut exhausted_attempts = 0;
    assert!(resolve_reference_with_spawn(&config, "unused", |_command| {
        exhausted_attempts += 1;
        Err(std::io::ErrorKind::ExecutableFileBusy.into())
    })
    .await
    .is_err());
    assert_eq!(
        exhausted_attempts, 4,
        "the finite budget permits three retries"
    );

    let mut non_busy_attempts = 0;
    assert!(resolve_reference_with_spawn(&config, "unused", |_command| {
        non_busy_attempts += 1;
        Err(std::io::ErrorKind::NotFound.into())
    })
    .await
    .is_err());
    assert_eq!(
        non_busy_attempts, 1,
        "other spawn errors must return immediately"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resolver_without_descendants_preserves_a_mapped_actor() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let program = tmp.path().join("resolver-test");
    let reference = tmp.path().join("reference");
    std::fs::write(&reference, "synthetic-value").unwrap();
    std::fs::write(&program, "#!/bin/sh\nexec /bin/cat \"$1\"\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = GitWriteSectionConfig {
        credential_resolver: vec![program.display().to_string(), "{ref}".to_string()],
        ..Default::default()
    };
    config.actors.insert(
        "mapped".to_string(),
        GitWriteActorConfig {
            name: "Example".to_string(),
            email: "example@example.invalid".to_string(),
            credential_ref: reference.display().to_string(),
            platform_identity: "example-login".to_string(),
        },
    );
    let (_, secret) = resolve_remote(&config, "mapped").await.unwrap();
    assert_eq!(secret.value(), "synthetic-value");
}

#[cfg(unix)]
#[tokio::test]
async fn resolver_reaps_background_descendants_after_normal_exits() {
    use std::os::unix::fs::PermissionsExt;

    for exit_code in [0, 7] {
        let tmp = tempfile::TempDir::new().unwrap();
        let program = tmp.path().join("resolver-test");
        let marker = tmp.path().join("background-ran");
        std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\n(sleep 2; printf ran > \"$1\") </dev/null >/dev/null 2>&1 &\nprintf 'fake-value\\n'\nexit {exit_code}\n"
                ),
            )
            .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = GitWriteSectionConfig {
            credential_resolver: vec![program.display().to_string(), "{ref}".to_string()],
            ..Default::default()
        };
        let result = resolve_reference(&config, marker.to_str().unwrap()).await;
        assert_eq!(result.is_ok(), exit_code == 0);
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert!(
            !marker.exists(),
            "resolver left a running descendant after exit {exit_code}"
        );
    }
}
