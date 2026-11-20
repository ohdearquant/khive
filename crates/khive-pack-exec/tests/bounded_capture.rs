#![cfg(target_os = "macos")]

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::BlobStore;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    registry: VerbRegistry,
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("exec-root");
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("khive.db")),
            exec: ExecSectionConfig {
                root: Some(root.to_string_lossy().into_owned()),
                read_roots: vec!["/bin".into(), "/usr/bin".into()],
                timeout_default_s: Some(5.0),
                timeout_max_s: Some(10.0),
                keep: true,
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let blob_store =
            khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        runtime
            .install_blob_store(Arc::new(blob_store) as Arc<dyn BlobStore>)
            .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(BlobPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        builder.register(ExecPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            registry,
            _dir: dir,
            root,
        }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn ready_tree(&self) -> String {
        self.call(
            "tool.register",
            json!({
                "name": "sh",
                "kind": "tool",
                "description": "shell",
                "source": "exec:/bin/sh",
                "side_effect": "write",
                "trust": "first_party"
            }),
        )
        .await;
        self.call(
            "tool.policy",
            json!({"actor": "*", "tool": "sh", "decision": "allow"}),
        )
        .await;
        self.call("exec.tree", json!({"entries": []})).await["tree"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[tokio::test]
async fn run_deadline_closes_a_descendant_held_pipe() {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;
    let script = r#"/usr/bin/perl -MPOSIX -e '
        pipe(my $signal_read, my $signal_write) or die "pipe: $!";
        my $child = fork(); die "fork: $!" unless defined $child;
        if ($child) {
            close($signal_write);
            my $signal = <$signal_read>;
            die "no readiness signal" unless defined $signal;
            close($signal_read);
            exit 0;
        }
        close($signal_read);
        POSIX::setsid();
        $SIG{PIPE} = "IGNORE";
        open(my $ready, ">", "$ENV{HOME}/pipe-ready") or die "ready: $!";
        print $ready "ready";
        close($ready);
        print $signal_write "ready\n";
        close($signal_write);
        select(undef, undef, undef, 2.5);
        my $written = syswrite(STDOUT, "late");
        open(my $state, ">", "$ENV{HOME}/pipe-state") or die "state: $!";
        print $state defined($written) ? "open" : "closed";
        close($state);
        sleep 6;
    ' & wait"#;
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": ["-c", script],
                "actor": "local",
                "timeout_s": 2.0
            }),
        ),
    )
    .await
    .expect("run exceeded its outer deadline while a descendant held the pipe");
    assert!(started.elapsed() < Duration::from_secs(3));
    let receipt = &result["receipt"];
    assert!(
        receipt["duration_ms"].as_i64().unwrap() <= 2400,
        "stream collection exceeded the run deadline plus closure grace: {receipt}"
    );
    assert_eq!(receipt["timed_out"], true, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert_eq!(receipt["stdout_capture"], "incomplete", "{receipt}");
    assert!(receipt["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("output collection reached the run deadline"));

    let state_path = fixture
        .root
        .join(receipt["id"].as_str().unwrap())
        .join("pipe-state");
    assert_eq!(
        std::fs::read(state_path.with_file_name("pipe-ready")).unwrap(),
        b"ready",
        "descendant was not ready before the run deadline"
    );
    let state = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(bytes) = std::fs::read(&state_path) {
                if !bytes.is_empty() {
                    break bytes;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("descendant did not report pipe state");
    assert_eq!(state, b"closed", "host read end stayed open");
}

#[tokio::test]
async fn detached_timeout_survivor_retains_file_and_network_denials() {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;
    let outside = fixture._dir.path().join("outside-guard");
    std::fs::write(&outside, b"guard").expect("host can write outside the run root");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let baseline = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .expect("host can connect to the loopback listener");
    drop(baseline);
    let bind_control = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("unsandboxed host can bind and listen on loopback");
    drop(bind_control);

    let script = r#"/usr/bin/perl -MPOSIX -MIO::Socket::INET -e '
        pipe(my $signal_read, my $signal_write) or die "pipe: $!";
        my $child = fork(); die "fork: $!" unless defined $child;
        if ($child) {
            close($signal_write);
            my $signal = <$signal_read>;
            die "no readiness signal" unless defined $signal;
            close($signal_read);
            sleep 20;
            exit 0;
        }
        close($signal_read);
        POSIX::setsid() >= 0 or die "setsid: $!";
        close(STDOUT);
        close(STDERR);
        open(my $ready, ">", "$ENV{HOME}/survivor-ready") or die "ready: $!";
        print $ready "ready";
        close($ready);
        print $signal_write "ready\n";
        close($signal_write);

        my $release = "$ENV{HOME}/survivor-release";
        for (1..750) {
            last if -e $release;
            select(undef, undef, undef, 0.02);
        }
        exit 2 unless -e $release;

        my $write_allowed = open(my $outside_file, ">", $ARGV[0]);
        if ($write_allowed) {
            print $outside_file "escaped";
            close($outside_file);
        }
        my $socket = IO::Socket::INET->new(
            PeerAddr => "127.0.0.1", PeerPort => $ARGV[1],
            Proto => "tcp", Timeout => 1
        );
        my $connect_allowed = defined $socket;
        close($socket) if $connect_allowed;
        my $listener = IO::Socket::INET->new(
            LocalAddr => "127.0.0.1", LocalPort => 0,
            Proto => "tcp", Listen => 1
        );
        my $bind_allowed = defined $listener;
        close($listener) if $bind_allowed;
        open(my $state, ">", "$ENV{HOME}/survivor-state.tmp") or die "state: $!";
        print $state "write=", ($write_allowed ? "allowed" : "denied"), "\n";
        print $state "connect=", ($connect_allowed ? "allowed" : "denied"), "\n";
        print $state "bind=", ($bind_allowed ? "allowed" : "denied"), "\n";
        close($state);
        rename("$ENV{HOME}/survivor-state.tmp", "$ENV{HOME}/survivor-state")
            or die "publish state: $!";
    ' "$1" "$2" & wait"#;
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        fixture.call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": [
                    "-c",
                    script,
                    "sh",
                    outside.to_string_lossy().into_owned(),
                    address.port().to_string()
                ],
                "actor": "local",
                "timeout_s": 1.0
            }),
        ),
    )
    .await
    .expect("run did not return after killing its initial process group");
    let receipt = &result["receipt"];
    assert_eq!(receipt["timed_out"], true, "{receipt}");
    assert!(
        receipt["sandbox"]["profile_digest"].is_string(),
        "{receipt}"
    );

    let run_dir = fixture.root.join(receipt["id"].as_str().unwrap());
    assert_eq!(
        std::fs::read(run_dir.join("survivor-ready")).expect("detached descendant readiness"),
        b"ready"
    );
    std::fs::write(run_dir.join("survivor-release"), b"go")
        .expect("release survivor after the timeout receipt");
    let state = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(bytes) = std::fs::read(run_dir.join("survivor-state")) {
                if !bytes.is_empty() {
                    break bytes;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("detached descendant did not report post-timeout probes");
    assert_eq!(state, b"write=denied\nconnect=denied\nbind=denied\n");
    assert_eq!(std::fs::read(outside).unwrap(), b"guard");
}

async fn original_group_liveness_reader(root: &std::path::Path) -> std::fs::File {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let ready_path = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path().join("original-group-ready");
                    if path.is_file() {
                        return path;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("original-group fixture did not publish readiness");
    let readiness: Value = serde_json::from_slice(&std::fs::read(&ready_path).unwrap()).unwrap();
    let leader = readiness["leader"].as_i64().unwrap();
    let grandchild = readiness["grandchild"].as_i64().unwrap();
    let pgid = readiness["pgid"].as_i64().unwrap();
    assert!(leader > 1 && grandchild > 1);
    assert_ne!(leader, grandchild);
    assert_eq!(
        pgid, leader,
        "fixture grandchild escaped the original group"
    );
    assert_eq!(
        unsafe { libc::getpgid(grandchild as libc::pid_t) },
        leader as libc::pid_t,
        "host must confirm group membership while leader is still held"
    );
    let mut liveness = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(ready_path.with_file_name("original-group-live"))
        .expect("open original-group liveness FIFO");
    let mut byte = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match liveness.read(&mut byte) {
                Ok(1) => break,
                Ok(0) => {}
                Ok(_) => unreachable!("one-byte liveness read"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read initial liveness byte: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("grandchild did not establish a live descriptor");
    assert_eq!(byte, [b'R']);
    liveness
}

async fn assert_original_group_fifo_closed(mut liveness: std::fs::File) {
    use std::io::Read;

    let mut byte = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match liveness.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => panic!("unexpected post-readiness liveness bytes"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read liveness EOF: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("original-group grandchild remains live after exec receipt");
}

async fn original_group_run_receipt(normal_exit: bool) -> Value {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;
    // The FIFO proves liveness without using an orphan zombie's numeric PID as
    // a death oracle. Every helper self-expires if a cleanup mutant omits kill.
    let script = r#"exec /usr/bin/perl -MPOSIX -e '
        my $leader = $$;
        pipe(my $signal_read, my $signal_write) or die "pipe: $!";
        my $child = fork(); die "fork: $!" unless defined $child;
        if ($child) {
            close($signal_write);
            my $signal = <$signal_read>;
            die "no live grandchild" unless defined($signal) && $signal eq "ready\n";
            close($signal_read);
            if ($ARGV[0] eq "normal") { exit 23; }
            sleep 10;
            exit 24;
        }
        close($signal_read);
        my $grandchild = fork(); die "grandchild fork: $!" unless defined $grandchild;
        if ($grandchild) {
            close($signal_write);
            exit 0;
        }
        close(STDOUT);
        close(STDERR);
        $SIG{ALRM} = sub { POSIX::_exit(25); };
        alarm 10;
        my $fifo = "$ENV{HOME}/original-group-live";
        POSIX::mkfifo($fifo, 0600) == 0 or die "mkfifo: $!";
        open(my $ready, ">", "$ENV{HOME}/original-group-ready.tmp") or die "ready: $!";
        print $ready "{\"leader\":$leader,\"grandchild\":$$,\"pgid\":", getpgrp(0), "}";
        close($ready);
        rename("$ENV{HOME}/original-group-ready.tmp", "$ENV{HOME}/original-group-ready")
            or die "publish ready: $!";
        open(my $live, ">", $fifo) or die "fifo writer: $!";
        syswrite($live, "R") == 1 or die "live byte: $!";
        print $signal_write "ready\n";
        close($signal_write);
        sleep 10;
    ' "$1""#;
    let params = json!({
        "tree": tree,
        "tool": "sh",
        "args": ["-c", script, "sh", if normal_exit { "normal" } else { "timeout" }],
        "actor": "local",
        "timeout_s": 5.0
    });
    let (result, liveness) = tokio::time::timeout(Duration::from_secs(9), async {
        tokio::join!(
            fixture.call("exec.run", params),
            original_group_liveness_reader(&fixture.root)
        )
    })
    .await
    .expect("original-group exec fixture exceeded its outer setup/run bound");
    let receipt = &result["receipt"];
    assert_eq!(receipt["timed_out"], !normal_exit, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    if normal_exit {
        assert_eq!(
            receipt["exit_code"], 23,
            "normal leader status was changed: {receipt}"
        );
    } else {
        assert!(
            receipt["exit_code"].is_null(),
            "timeout exposed a terminal status: {receipt}"
        );
    }
    assert_original_group_fifo_closed(liveness).await;
    result
}

#[tokio::test]
async fn normal_exit_receipt_ends_original_group_grandchild_liveness() {
    original_group_run_receipt(true).await;
}

#[tokio::test]
async fn timeout_receipt_ends_original_group_grandchild_liveness() {
    original_group_run_receipt(false).await;
}

#[tokio::test]
async fn run_without_descendants_preserves_nonzero_exit_status() {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": ["-c", "exit 23"],
                "actor": "local",
                "timeout_s": 3.0
            }),
        ),
    )
    .await
    .expect("no-descendant exec run did not finish within its outer bound");
    let receipt = &result["receipt"];
    assert_eq!(
        receipt["exit_code"], 23,
        "normal leader status was lost: {receipt}"
    );
    assert!(receipt["exit_signal"].is_null(), "{receipt}");
    assert_eq!(receipt["timed_out"], false, "{receipt}");
    assert_eq!(
        receipt["success"], false,
        "exit 23 is unsuccessful: {receipt}"
    );
    assert!(
        !receipt["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("process group cleanup failed"),
        "no-descendant normal exit must not invent cleanup failure: {receipt}"
    );
}
