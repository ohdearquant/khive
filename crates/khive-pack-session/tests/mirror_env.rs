use std::process::Command;

use khive_pack_session::mirror::MirrorConfig;
use khive_storage::test_support::run_exact_test_in_child;

const ENABLE_FLAGS: [&str; 4] = [
    "KHIVE_MIRROR_ENABLED",
    "KHIVE_MIRROR_CODEX_ENABLED",
    "KHIVE_MIRROR_CHATGPT_ENABLED",
    "KHIVE_MIRROR_CLAUDE_AI_ENABLED",
];
const CHILD_MARKER: &str = "KHIVE_SESSION_MIRROR_ENV_TEST_CHILD";

fn scrub_flags(command: &mut Command) {
    for name in ENABLE_FLAGS {
        command.env_remove(name);
    }
    command.env_remove("KHIVE_MIRROR_BACKFILL");
}

fn enabled_sources(config: &MirrorConfig) -> [bool; 4] {
    [
        config.enabled,
        config.codex_enabled,
        config.chatgpt_enabled,
        config.claude_ai_enabled,
    ]
}

#[test]
fn absent_enable_flags_stay_disabled_and_backfill_stays_enabled() {
    if run_exact_test_in_child(CHILD_MARKER, false, scrub_flags) {
        return;
    }
    let config = MirrorConfig::from_env();
    assert_eq!(enabled_sources(&config), [false; 4]);
    assert!(config.backfill);
}

#[test]
fn mirror_enable_flags_use_shared_boolean_values_independently() {
    if run_exact_test_in_child(CHILD_MARKER, false, scrub_flags) {
        return;
    }

    // Only this synchronous test runs in the child; no sibling can observe these changes.
    for (value, expected) in [
        ("1", true),
        ("true", true),
        ("TrUe", true),
        ("yes", true),
        ("YeS", true),
        ("on", true),
        ("ON", true),
        (" \ttrue\n", true),
        (" 1 ", true),
        (" yes ", true),
        (" on ", true),
        ("0", false),
        ("false", false),
        ("FALSE", false),
        ("no", false),
        ("off", false),
        (" OFF ", false),
        ("", false),
        ("unknown", false),
    ] {
        for (index, name) in ENABLE_FLAGS.into_iter().enumerate() {
            std::env::set_var(name, value);
            let mut expected_sources = [false; 4];
            expected_sources[index] = expected;
            assert_eq!(
                enabled_sources(&MirrorConfig::from_env()),
                expected_sources,
                "{name}={value:?}"
            );
            std::env::remove_var(name);
        }
    }
    for (value, expected) in [("0", false), ("FALSE", false), ("no", false), ("off", true)] {
        std::env::set_var("KHIVE_MIRROR_BACKFILL", value);
        assert_eq!(MirrorConfig::from_env().backfill, expected, "{value:?}");
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_enable_flags_stay_disabled() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    if run_exact_test_in_child(CHILD_MARKER, false, |command| {
        scrub_flags(command);
        for name in ENABLE_FLAGS {
            command.env(name, OsString::from_vec(vec![0xff]));
        }
    }) {
        return;
    }
    let config = MirrorConfig::from_env();
    assert_eq!(enabled_sources(&config), [false; 4]);
    assert!(config.backfill);
}
