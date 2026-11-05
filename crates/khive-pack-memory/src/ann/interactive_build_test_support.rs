//! One-process test gate for the actual interactive blocking-build closure.

use std::{
    io,
    os::unix::net::UnixDatagram,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

static INSTALLED: Mutex<Option<Arc<State>>> = Mutex::new(None);

#[derive(Clone)]
pub(crate) struct Ipc {
    socket: Arc<UnixDatagram>,
    nonce: Arc<str>,
}

impl Ipc {
    pub(crate) fn connect(
        local: &Path,
        parent: &Path,
        nonce: String,
        failsafe: Duration,
    ) -> io::Result<Self> {
        let socket = UnixDatagram::bind(local)?;
        socket.connect(parent)?;
        socket.set_read_timeout(Some(failsafe))?;
        socket.set_write_timeout(Some(failsafe))?;
        Ok(Self {
            socket: Arc::new(socket),
            nonce: nonce.into(),
        })
    }

    pub(crate) fn send(&self, event: &str) {
        self.try_send(event).expect("child IPC send");
    }

    fn try_send(&self, event: &str) -> io::Result<()> {
        let message = format!("{} {event}", self.nonce);
        let sent = self.socket.send(message.as_bytes())?;
        if sent != message.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "child IPC frame"));
        }
        Ok(())
    }

    fn release(&self) -> usize {
        let mut bytes = [0; 1024];
        let length = match self.socket.recv(&mut bytes) {
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                self.send("EXPIRED");
                panic!("worker gate failsafe expired");
            }
            Err(error) => {
                self.send("IPC_ERROR");
                panic!("worker gate IPC failed: {error}");
            }
        };
        assert!(length < bytes.len(), "parent IPC frame truncated");
        let message = std::str::from_utf8(&bytes[..length]).expect("parent IPC UTF-8");
        let event = message
            .strip_prefix(self.nonce.as_ref())
            .and_then(|message| message.strip_prefix(' '))
            .expect("parent IPC nonce");
        match event {
            "RELEASE" => 1,
            "CLEANUP_RELEASE" => 2,
            other => panic!("unexpected parent gate message: {other}"),
        }
    }
}

struct State {
    ipc: Ipc,
    captures: AtomicUsize,
    entries: AtomicUsize,
    released: AtomicUsize,
}

pub(crate) struct Installation(Arc<State>);

impl Installation {
    pub(crate) fn counts(&self) -> (usize, usize) {
        (
            self.0.captures.load(Ordering::SeqCst),
            self.0.entries.load(Ordering::SeqCst),
        )
    }

    pub(crate) fn released(&self) -> bool {
        self.0.released.load(Ordering::SeqCst) != 0
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let mut installed = INSTALLED.lock().expect("gate installation lock");
        if installed
            .as_ref()
            .is_some_and(|state| Arc::ptr_eq(state, &self.0))
        {
            *installed = None;
        }
    }
}

pub(crate) fn install(ipc: Ipc) -> Installation {
    let state = Arc::new(State {
        ipc,
        captures: AtomicUsize::new(0),
        entries: AtomicUsize::new(0),
        released: AtomicUsize::new(0),
    });
    let mut installed = INSTALLED.lock().expect("gate installation lock");
    assert!(installed.is_none(), "one gate installation per exact child");
    *installed = Some(state.clone());
    Installation(state)
}

pub(crate) struct CapturedGate(Arc<State>);

pub(crate) fn capture() -> Option<CapturedGate> {
    let state = INSTALLED
        .lock()
        .expect("gate installation lock")
        .as_ref()
        .cloned()?;
    let count = state.captures.fetch_add(1, Ordering::SeqCst) + 1;
    state.ipc.send(&format!("CAPTURE {count}"));
    assert_eq!(count, 1, "actual_build_gate_capture_count");
    Some(CapturedGate(state))
}

impl CapturedGate {
    pub(crate) fn enter(self) -> WorkerExit {
        let count = self.0.entries.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.ipc.send(&format!("ENTERED {count}"));
        assert_eq!(count, 1, "actual_build_gate_entry_count");
        let release = self.0.ipc.release();
        self.0.released.store(release, Ordering::SeqCst);
        self.0.ipc.send(if release == 1 {
            "RELEASED NORMAL"
        } else {
            "RELEASED CLEANUP"
        });
        WorkerExit(self.0)
    }
}

pub(crate) struct WorkerExit(Arc<State>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        let event = if std::thread::panicking() {
            "WORKER_EXIT PANIC"
        } else {
            "WORKER_EXIT RETURN"
        };
        if let Err(error) = self.0.ipc.try_send(event) {
            eprintln!("WORKER_EXIT_SEND_FAILED: {error}");
        }
    }
}
