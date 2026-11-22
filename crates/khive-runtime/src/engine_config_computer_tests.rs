//! ADR-220 sections 2 and 3, and acceptance arm 11: the `[computer]` table's
//! rules, read through the real configuration loader, and the key file's.

use super::*;
use crate::KhiveConfig;

const BASE: [(&str, &str); 5] = [
    ("name", "\"build-1\""),
    ("url", "\"http://127.0.0.1:8700/mcp\""),
    (
        "key_file",
        "\"/home/example/.secrets/computer-build-1.key\"",
    ),
    (
        "packs",
        "[\"computer\", \"exec\", \"git\", \"blob\", \"tool\"]",
    ),
    ("call_timeout_s", "3700"),
];

/// The base `[computer]` table alone, with `key` set to `value`, or left out
/// when `value` is None.
fn computer_table_with(key: &str, value: Option<&str>) -> String {
    let mut lines = vec!["[computer]".to_owned()];
    for (name, default) in BASE {
        match (name == key, value) {
            (false, _) => lines.push(format!("{name} = {default}")),
            (true, Some(value)) => lines.push(format!("{name} = {value}")),
            (true, None) => {}
        }
    }
    lines.join("\n") + "\n"
}

/// A whole file: `[actor].computer` naming the table's computer, then the
/// table from [`computer_table_with`].
fn table_with(key: &str, value: Option<&str>) -> String {
    let name = match (key, value) {
        ("name", Some(value)) => value,
        _ => BASE[0].1,
    };
    format!("[actor]\ncomputer = {name}\n") + &computer_table_with(key, value)
}

fn load(text: &str) -> Result<Option<KhiveConfig>, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    KhiveConfig::load(Some(&path))
}

fn accepted_with(key: &str, value: Option<&str>) -> ComputerSectionConfig {
    let text = table_with(key, value);
    match load(&text) {
        Ok(Some(config)) => config.computer.expect("the [computer] table is read"),
        Ok(None) => panic!("the configuration file was not found"),
        Err(error) => panic!("expected acceptance of:\n{text}\ngot: {error}"),
    }
}

fn refused(text: &str) -> String {
    match load(text) {
        Ok(_) => panic!("expected a refusal of:\n{text}"),
        Err(error) => error.to_string(),
    }
}

fn refused_with(key: &str, value: Option<&str>) -> String {
    refused(&table_with(key, value))
}

fn quoted(text: &str) -> String {
    format!("{text:?}")
}

#[test]
fn a_full_table_loads_and_an_absent_one_is_none() {
    assert_eq!(
        accepted_with("call_timeout_s", Some("3700")),
        ComputerSectionConfig {
            name: "build-1".to_owned(),
            url: "http://127.0.0.1:8700/mcp".to_owned(),
            key_file: PathBuf::from("/home/example/.secrets/computer-build-1.key"),
            packs: ["computer", "exec", "git", "blob", "tool"]
                .map(String::from)
                .to_vec(),
            call_timeout_s: 3700,
        }
    );
    assert_eq!(
        accepted_with("call_timeout_s", None).call_timeout_s,
        DEFAULT_COMPUTER_CALL_TIMEOUT_S
    );
    let without = load("# no computer table\n").expect("an empty file loads");
    assert!(without.and_then(|config| config.computer).is_none());
}

#[test]
fn the_table_is_closed_and_four_keys_are_required() {
    let unknown = refused(&(table_with("call_timeout_s", Some("3700")) + "retries = 2\n"));
    assert!(unknown.contains("retries"), "{unknown}");
    for key in ["name", "url", "key_file", "packs"] {
        let error = refused_with(key, None);
        assert!(error.contains(key), "missing {key}: {error}");
    }
}

#[test]
fn actor_computer_and_the_table_require_each_other_and_name_one_computer() {
    let table = computer_table_with("call_timeout_s", Some("3700"));
    let without_field = refused(&table);
    assert!(
        without_field.contains("[actor].computer") && without_field.contains("required"),
        "{without_field}"
    );
    let other = refused(&("[actor]\ncomputer = \"build-2\"\n".to_owned() + &table));
    assert!(
        other.contains("[actor].computer") && other.contains("build-2"),
        "{other}"
    );
    let without_table = refused("[actor]\ncomputer = \"build-1\"\n");
    assert!(
        without_table.contains("[actor].computer") && without_table.contains("no [computer] table"),
        "{without_table}"
    );
    assert_eq!(
        accepted_with("call_timeout_s", Some("3700")).name,
        "build-1"
    );
}

#[test]
fn name_is_one_to_sixty_four_lowercase_letters_digits_and_dashes() {
    let longest = "a".repeat(64);
    for name in ["a", "0", "build-1", longest.as_str()] {
        assert_eq!(accepted_with("name", Some(&quoted(name))).name, name);
    }
    let too_long = "a".repeat(65);
    for name in [
        "",
        "Build-1",
        "build_1",
        "build 1",
        "build.1",
        "b\u{fc}ild",
        too_long.as_str(),
    ] {
        let error = refused_with("name", Some(&quoted(name)));
        assert!(error.contains("[computer].name"), "{name:?}: {error}");
    }
}

#[test]
fn plain_http_is_accepted_only_for_a_loopback_host() {
    for (url, host, port, https, loopback) in [
        ("http://127.0.0.1:8700/mcp", "127.0.0.1", 8700, false, true),
        ("http://127.5.6.7:1/mcp", "127.5.6.7", 1, false, true),
        ("http://localhost:8700/mcp", "localhost", 8700, false, true),
        ("HTTP://LOCALHOST/mcp", "LOCALHOST", 80, false, true),
        ("http://[::1]:8700/mcp", "::1", 8700, false, true),
        ("https://127.0.0.1/mcp", "127.0.0.1", 443, true, true),
        (
            "https://computer.example.com/mcp",
            "computer.example.com",
            443,
            true,
            false,
        ),
        (
            "https://10.0.0.5:8443/v1/mcp",
            "10.0.0.5",
            8443,
            true,
            false,
        ),
        (
            "https://[2001:db8::1]:8443/mcp",
            "2001:db8::1",
            8443,
            true,
            false,
        ),
    ] {
        let endpoint = accepted_with("url", Some(&quoted(url)))
            .endpoint()
            .expect("an accepted url parses");
        assert_eq!(
            endpoint,
            ComputerEndpoint {
                https,
                host: host.to_owned(),
                port,
                loopback,
            },
            "{url}"
        );
    }
    for (url, host) in [
        ("http://192.168.1.5:8700/mcp", "192.168.1.5"),
        ("http://computer.example.com/mcp", "computer.example.com"),
        ("http://localhost.example.com/mcp", "localhost.example.com"),
        ("http://127.1/mcp", "127.1"),
        ("http://[::ffff:127.0.0.1]:8700/mcp", "::ffff:127.0.0.1"),
        ("http://[2001:db8::1]/mcp", "2001:db8::1"),
    ] {
        let error = refused_with("url", Some(&quoted(url)));
        assert!(
            error.contains("[computer].url")
                && error.contains("requires https")
                && error.contains(host),
            "{url}: {error}"
        );
    }
}

#[test]
fn a_malformed_url_is_refused() {
    for url in [
        "",
        "127.0.0.1:8700/mcp",
        "ftp://127.0.0.1/mcp",
        "https://computer.example.com",
        "https://computer.example.com?x=1",
        "https://computer.example.com/mcp?x=1",
        "https://computer.example.com/mcp#top",
        "https:///mcp",
        "https://:8443/mcp",
        "http://[::1/mcp",
        "http://[::1]x/mcp",
        "http://[example]/mcp",
        "https://2001:db8::1/mcp",
        "https://computer.example.com:0/mcp",
        "https://computer.example.com:65536/mcp",
        "https://computer.example.com:+443/mcp",
        "https://computer.example.com:/mcp",
        "https://compu ter.example.com/mcp",
        "https://computer_example.com/mcp",
        "https://computer.example.com/m\u{e9}cp",
    ] {
        let error = refused_with("url", Some(&quoted(url)));
        assert!(error.contains("[computer].url"), "{url:?}: {error}");
    }
}

#[test]
fn a_url_with_a_user_name_or_password_is_refused_without_repeating_it() {
    let user = "fixture-user";
    let password = "fixture-pass-7";
    for url in [
        format!("https://{user}:{password}@computer.example.com/mcp"),
        format!("http://{user}@127.0.0.1:8700/mcp"),
    ] {
        let error = refused_with("url", Some(&quoted(&url)));
        assert!(error.contains("user name or password"), "{error}");
        assert!(
            !error.contains(user) && !error.contains(password),
            "the refusal repeats the url: {error}"
        );
    }
}

#[test]
fn key_file_must_be_an_absolute_path() {
    let error = refused_with("key_file", Some("\"secrets/computer.key\""));
    assert!(
        error.contains("[computer].key_file") && error.contains("absolute"),
        "{error}"
    );
}

#[test]
fn packs_are_a_nonempty_set_drawn_from_the_routable_packs() {
    let all = format!("{COMPUTER_ROUTABLE_PACKS:?}");
    assert_eq!(
        accepted_with("packs", Some(&all)).packs,
        COMPUTER_ROUTABLE_PACKS.map(String::from).to_vec()
    );
    for (packs, expected) in [
        ("[]", "at least one"),
        ("[\"exec\", \"git\", \"exec\"]", "listed twice"),
        ("[\"memory\"]", "not a routable pack"),
        ("[\"Exec\"]", "not a routable pack"),
    ] {
        let error = refused_with("packs", Some(packs));
        assert!(
            error.contains("[computer].packs") && error.contains(expected),
            "{packs}: {error}"
        );
    }
    for pack in COMMAND_RUNNING_PACKS {
        assert!(COMPUTER_ROUTABLE_PACKS.contains(&pack), "{pack}");
    }
}

#[test]
fn call_timeout_is_one_to_seventy_two_hundred_seconds() {
    for value in [1, MAX_COMPUTER_CALL_TIMEOUT_S] {
        assert_eq!(
            accepted_with("call_timeout_s", Some(&value.to_string())).call_timeout_s,
            value
        );
    }
    for value in ["0", "7201"] {
        let error = refused_with("call_timeout_s", Some(value));
        assert!(
            error.contains("[computer].call_timeout_s"),
            "{value}: {error}"
        );
    }
    refused_with("call_timeout_s", Some("-1"));
}

#[test]
fn debug_prints_key_file_as_a_path() {
    let computer = accepted_with("call_timeout_s", None);
    assert!(format!("{computer:?}").contains("/home/example/.secrets/computer-build-1.key"));
}

#[cfg(unix)]
mod key_file {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::super::*;

    const TOKEN: &str = "fixture-token-0123456789";

    fn key_file(dir: &tempfile::TempDir, bytes: &[u8], mode: u32) -> PathBuf {
        let path = dir.path().join("computer.key");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// The refusal for `path`, after checking its message names the table key
    /// and repeats neither the path nor the token.
    fn refusal(path: &Path) -> ComputerKeyRefusal {
        let error = read_computer_key(path).expect_err("the key must be refused");
        let message = error.to_string();
        assert!(
            message.starts_with("computer_key_unavailable: [computer].key_file "),
            "{message}"
        );
        assert!(!message.contains(&*path.to_string_lossy()), "{message}");
        assert!(!message.contains(TOKEN), "{message}");
        assert_eq!(ComputerKeyError::KIND, "computer_key_unavailable");
        error.refusal
    }

    #[test]
    fn an_owner_only_file_yields_its_token() {
        let dir = tempfile::tempdir().unwrap();
        for content in [
            TOKEN.to_owned(),
            format!("{TOKEN}\n"),
            format!("{TOKEN}\r\n"),
        ] {
            let key = read_computer_key(&key_file(&dir, content.as_bytes(), 0o600)).unwrap();
            assert_eq!(key.token(), TOKEN);
            assert_eq!(format!("{key:?}"), "ComputerKey(<redacted>)");
        }
        let read_only = read_computer_key(&key_file(&dir, TOKEN.as_bytes(), 0o400)).unwrap();
        assert_eq!(read_only.token(), TOKEN);
    }

    #[test]
    fn group_or_other_permission_bits_refuse_the_key() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o640, 0o604, 0o660, 0o644, 0o610, 0o601, 0o704, 0o620] {
            let path = key_file(&dir, TOKEN.as_bytes(), mode);
            assert_eq!(
                refusal(&path),
                ComputerKeyRefusal::GroupOrOtherPermissionBits { mode },
                "{mode:o}"
            );
        }
    }

    #[test]
    fn anything_but_a_regular_file_is_refused() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let target = key_file(&dir, TOKEN.as_bytes(), 0o600);
        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(refusal(&link), ComputerKeyRefusal::NotRegularFile);

        let directory = dir.path().join("keys");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(refusal(&directory), ComputerKeyRefusal::NotRegularFile);

        // A FIFO with no writer would block a plain open; the key reader must
        // refuse it at once.
        let fifo = dir.path().join("fifo.key");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_path is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert_eq!(refusal(&fifo), ComputerKeyRefusal::NotRegularFile);

        assert_eq!(
            refusal(&dir.path().join("missing.key")),
            ComputerKeyRefusal::Unreadable(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn the_file_holds_one_token_of_at_most_4096_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let largest = "k".repeat(MAX_COMPUTER_KEY_BYTES);
        for content in [
            largest.clone(),
            format!("{largest}\n"),
            format!("{largest}\r\n"),
        ] {
            let key = read_computer_key(&key_file(&dir, content.as_bytes(), 0o600)).unwrap();
            assert_eq!(key.token(), largest);
        }
        let over = "k".repeat(MAX_COMPUTER_KEY_BYTES + 1);
        for (content, expected) in [
            (String::new(), ComputerKeyRefusal::Empty),
            ("\n".to_owned(), ComputerKeyRefusal::Empty),
            ("\r\n".to_owned(), ComputerKeyRefusal::Empty),
            (over.clone(), ComputerKeyRefusal::TooLarge),
            (format!("{over}\n"), ComputerKeyRefusal::TooLarge),
            (format!("{largest}\n\n"), ComputerKeyRefusal::TooLarge),
            ("k".repeat(10_000), ComputerKeyRefusal::TooLarge),
            ("fixture token".to_owned(), ComputerKeyRefusal::NotOneToken),
            ("fixture\ttoken".to_owned(), ComputerKeyRefusal::NotOneToken),
            (" fixture".to_owned(), ComputerKeyRefusal::NotOneToken),
            ("fixture\n\n".to_owned(), ComputerKeyRefusal::NotOneToken),
            ("fixture\r".to_owned(), ComputerKeyRefusal::NotOneToken),
            ("fixture\0".to_owned(), ComputerKeyRefusal::NotOneToken),
            ("fixtur\u{e9}".to_owned(), ComputerKeyRefusal::NotOneToken),
        ] {
            assert_eq!(
                refusal(&key_file(&dir, content.as_bytes(), 0o600)),
                expected,
                "{} bytes: {:?}",
                content.len(),
                content.get(..20)
            );
        }
    }
}
