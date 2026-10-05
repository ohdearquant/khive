use std::error::Error as _;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const SECRET: &str = "4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b"; // gitleaks:allow
const ENV_VALUE: &str = "KHIVE_S1_CREDENTIAL_VALUE";
const ENV_MISSING: &str = "KHIVE_S1_CREDENTIAL_MISSING";
const ENV_EMPTY: &str = "KHIVE_S1_CREDENTIAL_EMPTY";

fn entry(name: &str, kind: CredentialKind, provider: &str) -> CredentialConfig {
    CredentialConfig {
        name: name.to_owned(),
        kind,
        provider: provider.to_owned(),
        env_var: (provider == "env").then(|| ENV_VALUE.to_owned()),
        header: (kind == CredentialKind::Header).then(|| "X-Api-Key".to_owned()),
    }
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn record_error(error: &dyn std::error::Error) -> String {
    let mut text = format!("{error}\n{error:?}\n");
    let mut source = error.source();
    while let Some(error) = source {
        text.push_str(&format!("{error}\n{error:?}\n"));
        source = error.source();
    }
    text
}

#[test]
fn env_resolution_redacts_diagnostics_and_runtime_refuses_material() {
    const MARKER: &str = "KHIVE_S1_CREDENTIAL_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    let args = ["--exact", name, "--nocapture", "--test-threads=1"];
    if std::env::var(MARKER).ok().as_deref() != Some(name) {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(args)
            .env(MARKER, name)
            .env(ENV_VALUE, SECRET)
            .env(ENV_EMPTY, "")
            .env_remove(ENV_MISSING)
            .env("HOME", home.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.contains(SECRET), "child stdout disclosed material");
        assert!(!stderr.contains(SECRET), "child stderr disclosed material");
        assert!(output.status.success(), "child failed: {stdout}\n{stderr}");
        assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"));
        return;
    }
    assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), args);
    assert!(SECRET.len() >= 32);
    let logs = CapturedLogs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    let present = entry("partner-token", CredentialKind::Header, "env");
    let mut missing = entry("missing-token", CredentialKind::Basic, "env");
    missing.env_var = Some(ENV_MISSING.to_owned());
    let mut empty = entry("empty-token", CredentialKind::Basic, "env");
    empty.env_var = Some(ENV_EMPTY.to_owned());
    let cookie = entry("env-cookie", CredentialKind::CookieJar, "env");
    let registry = CredentialRegistry::new(vec![present, missing, empty, cookie]).unwrap();
    assert_eq!(
        registry.cache_lifetime("partner-token").unwrap(),
        CredentialCacheLifetime::NoCache
    );
    let material = registry.resolve("partner-token").unwrap();
    assert!(material.bytes.as_slice() == SECRET.as_bytes());
    assert_eq!(format!("{material:?}"), "CredentialMaterial([REDACTED])");
    let mut diagnostics = format!("{material:?}\n{material:#?}\n");
    tracing::info!(material = ?material, "credential_redaction_capture");
    for name in ["missing-token", "empty-token"] {
        let error = registry.resolve(name).unwrap_err();
        assert!(error.to_string().contains(name));
        diagnostics.push_str(&record_error(&error));
    }
    for error in [
        registry.resolve("unknown-token").unwrap_err(),
        registry
            .update("partner-token", SECRET.as_bytes().to_vec())
            .unwrap_err(),
        registry
            .update("env-cookie", SECRET.as_bytes().to_vec())
            .unwrap_err(),
    ] {
        diagnostics.push_str(&record_error(&error));
    }

    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    async_runtime.block_on(async {
        let runtime = crate::KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(crate::Namespace::local()).unwrap();
        let before = runtime.count_notes(&token, None).await.unwrap();
        // Deliberately bypass opacity in this child module to test the store backstop.
        let content = format!("api key {}", std::str::from_utf8(&material.bytes).unwrap());
        let result = runtime
            .create_note(&token, "observation", None, &content, None, None, vec![])
            .await;
        assert!(
            matches!(&result, Err(crate::RuntimeError::SecretDetected(_))),
            "runtime accepted credential material"
        );
        let error = result.unwrap_err();
        diagnostics.push_str(&record_error(&error));
        assert_eq!(runtime.count_notes(&token, None).await.unwrap(), before);
    });
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("credential_redaction_capture"),
        "capture must observe tracing"
    );
    assert!(
        !logs.contains(SECRET),
        "full tracing output disclosed material"
    );
    assert!(
        !diagnostics.contains(SECRET),
        "full error/debug output disclosed material"
    );
}

struct SpyProvider {
    resolved: AtomicUsize,
    updated: AtomicUsize,
}

impl CredentialProvider for SpyProvider {
    fn resolve(&self, _name: &str) -> Result<CredentialMaterial, CredentialError> {
        self.resolved.fetch_add(1, Ordering::SeqCst);
        Ok(CredentialMaterial::new(SECRET.as_bytes().to_vec()))
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::Process
    }

    fn update(&self, _name: &str, material: Zeroizing<Vec<u8>>) -> Result<(), CredentialError> {
        assert!(material.as_slice() == SECRET.as_bytes());
        self.updated.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn registered_provider_receives_only_cookie_updates() {
    let entries = vec![
        entry("header", CredentialKind::Header, "host"),
        entry("basic", CredentialKind::Basic, "host"),
        entry("signer", CredentialKind::SigningKey, "host"),
        entry("cookie", CredentialKind::CookieJar, "host"),
    ];
    let mut registry = CredentialRegistry::new(entries).unwrap();
    assert!(matches!(
        registry.resolve("cookie"),
        Err(CredentialError::UnknownProvider { .. })
    ));
    let provider = Arc::new(SpyProvider {
        resolved: AtomicUsize::new(0),
        updated: AtomicUsize::new(0),
    });
    registry
        .register_provider("host".to_owned(), provider.clone())
        .unwrap();
    assert_eq!(
        registry.cache_lifetime("cookie").unwrap(),
        CredentialCacheLifetime::Process
    );
    assert!(registry.resolve("cookie").is_ok());
    assert_eq!(provider.resolved.load(Ordering::SeqCst), 1);
    for name in ["header", "basic", "signer"] {
        assert!(matches!(
            registry.update(name, SECRET.as_bytes().to_vec()),
            Err(CredentialError::UpdateNotAllowed { .. })
        ));
    }
    assert_eq!(provider.updated.load(Ordering::SeqCst), 0);
    registry
        .update("cookie", SECRET.as_bytes().to_vec())
        .unwrap();
    assert_eq!(provider.updated.load(Ordering::SeqCst), 1);
    assert!(registry
        .register_provider("host".to_owned(), provider.clone())
        .is_err());
    assert!(registry
        .register_provider("env".to_owned(), provider)
        .is_err());
}

struct FailingProvider;

impl CredentialProvider for FailingProvider {
    fn resolve(&self, _name: &str) -> Result<CredentialMaterial, CredentialError> {
        Err(CredentialError::Unavailable {
            name: SECRET.to_owned(),
        })
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }

    fn update(&self, _name: &str, _material: Zeroizing<Vec<u8>>) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable {
            name: SECRET.to_owned(),
        })
    }
}

#[test]
fn registry_discards_provider_error_payloads() {
    let mut registry =
        CredentialRegistry::new(vec![entry("cookie", CredentialKind::CookieJar, "host")]).unwrap();
    registry
        .register_provider("host".to_owned(), Arc::new(FailingProvider))
        .unwrap();
    for error in [
        registry.resolve("cookie").unwrap_err(),
        registry
            .update("cookie", SECRET.as_bytes().to_vec())
            .unwrap_err(),
    ] {
        let text = record_error(&error);
        assert!(text.contains("cookie"));
        assert!(!text.contains(SECRET));
        assert!(error.source().is_none());
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_env_material_is_unavailable_without_disclosure() {
    use std::os::unix::ffi::OsStringExt;
    const MARKER: &str = "KHIVE_S1_NON_UNICODE_CHILD";
    if khive_storage::test_support::run_exact_test_in_child(MARKER, false, |command| {
        let mut bytes = SECRET.as_bytes().to_vec();
        bytes.push(0xff);
        command.env(ENV_VALUE, std::ffi::OsString::from_vec(bytes));
    }) {
        return;
    }
    let registry =
        CredentialRegistry::new(vec![entry("invalid", CredentialKind::Basic, "env")]).unwrap();
    let error = registry.resolve("invalid").unwrap_err();
    assert!(matches!(error, CredentialError::Unavailable { .. }));
    assert!(!record_error(&error).contains(SECRET));
}
