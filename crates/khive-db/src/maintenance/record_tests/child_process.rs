use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

// Polling bounds process lifetime; only observed exit establishes ordering.
struct OwnedChild {
    child: std::process::Child,
    reaped: bool,
}
impl OwnedChild {
    fn until(
        &mut self,
        deadline: std::time::Instant,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(Some(status));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Panic/error cleanup must not introduce another blocking wait.
            let _ = self.child.kill();
            let _ = self.child.try_wait();
        }
    }
}

const CHILD_COMPLETE: &str = "record-test-child.complete";
const CHILD_COMPLETE_BYTES: &[u8] = b"record-test-child-complete\n";

pub(super) fn complete_test_child(root: &Path) {
    let mut marker = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.join(CHILD_COMPLETE))
        .unwrap();
    marker.write_all(CHILD_COMPLETE_BYTES).unwrap();
}

pub(super) fn run_test_child<T>(_test: T, child_env: &str, root: &Path) -> u32 {
    let qualified = std::any::type_name::<T>();
    let (test_module, _) = module_path!().rsplit_once("::").unwrap();
    assert!(
        qualified
            .strip_prefix(test_module)
            .is_some_and(|name| name.starts_with("::")),
        "child selector must name a test function in {test_module}: {qualified}"
    );
    let (crate_name, _) = test_module.split_once("::").unwrap();
    let selector = qualified
        .strip_prefix(crate_name)
        .and_then(|name| name.strip_prefix("::"))
        .unwrap();
    let log_path = root.join("record-test-child.log");
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&log_path)
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--test-threads=1", "--color=never"])
        .env(child_env, root)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut child = OwnedChild {
        child,
        reaped: false,
    };
    let pid = child.child.id();
    let status = match child.until(deadline).expect("poll maintenance test child") {
        Some(status) => status,
        None => {
            let killed = child.child.kill();
            let reaped = child.until(std::time::Instant::now() + std::time::Duration::from_secs(2));
            panic!(
                "maintenance test child {selector} timed out; kill={killed:?}; bounded reap={reaped:?}"
            );
        }
    };
    let output = fs::read_to_string(log_path).unwrap();
    assert!(status.success(), "child {selector} failed: {output}");
    assert!(
        output.lines().any(|line| line == "running 1 test"),
        "child {selector} did not run exactly one test: {output}"
    );
    assert!(
        output
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "child {selector} did not pass exactly one test: {output}"
    );
    assert_eq!(
        fs::read(root.join(CHILD_COMPLETE)).expect("child completion marker"),
        CHILD_COMPLETE_BYTES
    );
    pid
}
