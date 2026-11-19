// ── HOME isolation for local-fallback tests ───────────────────────────────
//
// `build_local_fallback_server` (via `run_exec_inline_with_forward` /
// `run_exec_ops_file`) now loads `KhiveConfig::load_with_home_fallback`
// unconditionally, which falls through to `~/.khive/config.toml` (tier 4)
// when no project-local config is found. Any test that builds a
// `RuntimeConfig` directly (bypassing `resolve_runtime_config`) with
// `db_path: None` would otherwise pick up whatever REAL config a
// developer/CI machine happens to have at `$HOME/.khive/config.toml` —
// including a genuinely multi-backend one — and silently exercise the
// multi-backend arm (or open real backend files) instead of the isolated
// single-backend path the test assumes. Point `HOME` at an empty tempdir
// for the duration of any such test so `khive_cfg` resolves to
// `KhiveConfig::default()` deterministically, regardless of the host.
fn isolate_home_for_test() -> (Option<std::ffi::OsString>, tempfile::TempDir) {
    let prev = std::env::var_os("HOME");
    let dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    std::env::set_var("HOME", dir.path());
    (prev, dir)
}

fn restore_home(prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}

const DAEMON_SPAWN_TEST_ENV_VARS: [&str; 8] = [
    "KHIVE_EMBEDDING_MODEL",
    "KHIVE_ADDITIONAL_EMBEDDING_MODELS",
    "KHIVE_ACTOR",
    "KHIVE_REQUIRE_ATTRIBUTED_ACTOR",
    "KHIVE_DB",
    "KHIVE_PACKS",
    "KHIVE_LOCK",
    "HOME",
];

struct EnvAndCwdGuard {
    original_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    original_cwd: std::path::PathBuf,
}

impl EnvAndCwdGuard {
    fn capture() -> Self {
        Self {
            original_env: DAEMON_SPAWN_TEST_ENV_VARS
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect(),
            original_cwd: std::env::current_dir().expect("read cwd"),
        }
    }
}

impl Drop for EnvAndCwdGuard {
    fn drop(&mut self) {
        for (name, original) in self.original_env.drain(..) {
            match original {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let _ = std::env::set_current_dir(&self.original_cwd);
    }
}

#[test]
#[serial]
fn daemon_spawn_env_guard_restores_every_mutated_variable() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _restore_machine_env = EnvAndCwdGuard::capture();
    for name in DAEMON_SPAWN_TEST_ENV_VARS {
        std::env::set_var(name, format!("sentinel-{name}"));
    }

    {
        let _guard = EnvAndCwdGuard::capture();
        for name in DAEMON_SPAWN_TEST_ENV_VARS {
            std::env::remove_var(name);
        }
    }

    for name in DAEMON_SPAWN_TEST_ENV_VARS {
        assert_eq!(
            std::env::var(name).as_deref(),
            Ok(format!("sentinel-{name}").as_str())
        );
    }
}
