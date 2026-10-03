use super::{Args, Command, EventsDaemonArgs};
use clap::Parser;
use khive_db::{WalCeilingPolicy, WalCeilingSource};

fn events_args(arguments: &[&str]) -> EventsDaemonArgs {
    let args = Args::try_parse_from(arguments).expect("parse events daemon arguments");
    match args.command.expect("events daemon command") {
        Command::EventsDaemon(args) => args,
        other => panic!("expected events daemon, got {other:?}"),
    }
}

#[test]
fn events_daemon_accepts_explicit_zero_backend_field_policy() {
    let args = events_args(&[
        "kkernel",
        "events-daemon",
        "--wal-ceiling-bytes",
        "0",
        "--wal-ceiling-source",
        "backend_field",
    ]);
    assert_eq!(
        args.wal_ceiling_policy().unwrap(),
        Some(WalCeilingPolicy {
            bytes: 0,
            source: WalCeilingSource::BackendField,
        })
    );
}

#[test]
fn events_daemon_preserves_each_closed_policy_source() {
    for (argument, expected) in [
        ("backend_field", WalCeilingSource::BackendField),
        ("environment", WalCeilingSource::Environment),
        ("default", WalCeilingSource::Default),
    ] {
        let args = events_args(&[
            "kkernel",
            "events-daemon",
            "--wal-ceiling-bytes",
            "65536",
            "--wal-ceiling-source",
            argument,
        ]);
        assert_eq!(
            args.wal_ceiling_policy().unwrap(),
            Some(WalCeilingPolicy {
                bytes: 65536,
                source: expected,
            }),
            "{argument}"
        );
    }
}

#[test]
fn events_daemon_requires_source_when_bytes_are_present() {
    let error = Args::try_parse_from(["kkernel", "events-daemon", "--wal-ceiling-bytes", "0"])
        .expect_err("bytes without their source must refuse");
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    assert!(error.to_string().contains("--wal-ceiling-source"));
}

#[test]
fn events_daemon_requires_bytes_when_source_is_present() {
    let error = Args::try_parse_from([
        "kkernel",
        "events-daemon",
        "--wal-ceiling-source",
        "backend_field",
    ])
    .expect_err("source without bytes must refuse");
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    assert!(error.to_string().contains("--wal-ceiling-bytes"));
}

#[test]
fn events_daemon_refuses_unknown_policy_sources() {
    for source in ["unknown", "backend-field", "BACKEND_FIELD"] {
        let error = Args::try_parse_from([
            "kkernel",
            "events-daemon",
            "--wal-ceiling-bytes",
            "0",
            "--wal-ceiling-source",
            source,
        ])
        .expect_err("source vocabulary must remain closed");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::InvalidValue,
            "{source}"
        );
        assert!(error.to_string().contains("--wal-ceiling-source"));
    }
}

#[test]
fn events_daemon_refuses_byte_parse_overflow() {
    let error = Args::try_parse_from([
        "kkernel",
        "events-daemon",
        "--wal-ceiling-bytes",
        "18446744073709551616",
        "--wal-ceiling-source",
        "backend_field",
    ])
    .expect_err("out-of-range u64 bytes must refuse during parsing");
    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert!(error.to_string().contains("--wal-ceiling-bytes"));
}

#[test]
fn events_daemon_refuses_offset_overflow_before_path_resolution() {
    let args = events_args(&[
        "kkernel",
        "events-daemon",
        "--db",
        "not-opened/events.db",
        "--socket",
        "not-opened/events.sock",
        "--wal-ceiling-bytes",
        "9223372036854775808",
        "--wal-ceiling-source",
        "backend_field",
    ]);
    let error = args
        .wal_ceiling_policy()
        .expect_err("resolved offsets must fit before resolving or opening paths");
    let cause = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<khive_db::SqliteError>());
    assert!(matches!(
        cause,
        Some(khive_db::SqliteError::WalCeilingOffsetOverflow { bytes })
            if *bytes == 9_223_372_036_854_775_808
    ));
}

#[test]
fn events_daemon_retains_legacy_arguments_without_policy_flags() {
    let args = events_args(&[
        "kkernel",
        "events-daemon",
        "--db",
        "events.db",
        "--socket",
        "events.sock",
    ]);
    assert_eq!(args.db.as_deref(), Some(std::path::Path::new("events.db")));
    assert_eq!(
        args.socket.as_deref(),
        Some(std::path::Path::new("events.sock"))
    );
    assert_eq!(args.wal_ceiling_policy().unwrap(), None);
    let defaults = events_args(&["kkernel", "events-daemon"]);
    assert_eq!(defaults.wal_ceiling_policy().unwrap(), None);
}
