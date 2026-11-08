//! One-process test gate for actual ANN CPU closures on either execution route.

use std::{
    io,
    os::unix::net::UnixDatagram,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub(crate) mod protocol {
    use std::io;

    pub(crate) const OPEN: &str = "open";
    pub(crate) const HELD: &str = "held";
    pub(crate) const START: &str = "CHILD_START";
    pub(crate) const BACKGROUND: &str = "CONTEXT BACKGROUND";
    pub(crate) const INTERACTIVE: &str = "CONTEXT INTERACTIVE";
    pub(crate) const CAPTURE: &str = "CAPTURE";
    pub(crate) const ENTERED: &str = "ENTERED";
    pub(crate) const COUNTS: &str = "COUNTS";
    pub(crate) const CAPTURE_ONCE: &str = "CAPTURE 1";
    pub(crate) const ENTERED_ONCE: &str = "ENTERED 1";
    pub(crate) const RELEASE: &str = "RELEASE";
    pub(crate) const CLEANUP_RELEASE: &str = "CLEANUP_RELEASE";
    pub(crate) const RELEASED_NORMAL: &str = "RELEASED NORMAL";
    pub(crate) const RELEASED_CLEANUP: &str = "RELEASED CLEANUP";
    pub(crate) const BODY_RETURN: &str = "BODY_SCOPE_EXIT RETURN";
    pub(crate) const BODY_PANIC: &str = "BODY_SCOPE_EXIT PANIC";
    pub(crate) const BODY_CONFIRMED: &str = "BODY_SCOPE_CONFIRMED";
    pub(crate) const EXPIRED: &str = "EXPIRED";
    pub(crate) const IPC_ERROR: &str = "IPC_ERROR";
    pub(crate) const COLD_OK: &str = "COLD_OK";
    pub(crate) const WARM_OK: &str = "WARM_OK";
    pub(crate) const FALLBACK_OK: &str = "FALLBACK_OK";
    pub(crate) const REQUEST_RETURNED: &str = "REQUEST_RETURNED";
    pub(crate) const PRE_DROP: &str = "PRE_DROP";
    pub(crate) const POST_DROP: &str = "POST_DROP";
    pub(crate) const CHILD_OK: &str = "CHILD_OK";
    pub(crate) const REAL_SUBMIT: &str = "REAL_SUBMIT_COMPLETED";
    pub(crate) const REPLACED: &str = "RECEIVED_REPLACED";
    pub(crate) const MAPPING: &str = "MAPPING_RESULT";

    pub(crate) fn validate(event: &str) -> io::Result<()> {
        let fixed = [
            BACKGROUND,
            INTERACTIVE,
            RELEASED_NORMAL,
            RELEASED_CLEANUP,
            BODY_RETURN,
            BODY_PANIC,
            BODY_CONFIRMED,
            EXPIRED,
            IPC_ERROR,
            COLD_OK,
            WARM_OK,
            FALLBACK_OK,
            REQUEST_RETURNED,
            PRE_DROP,
            POST_DROP,
            CHILD_OK,
            REAL_SUBMIT,
        ];
        let fields: Vec<_> = event.split_whitespace().collect();
        let valid = fixed.contains(&event)
            || match fields.as_slice() {
                [START, mode] => {
                    matches!(*mode, OPEN | HELD) || ReceivedCase::from_mode(mode).is_some()
                }
                [CAPTURE | ENTERED, count] => count.parse::<usize>().is_ok(),
                [COUNTS, captures, entries] => {
                    captures.parse::<usize>().is_ok() && entries.parse::<usize>().is_ok()
                }
                [REPLACED, variant] => ReceivedCase::from_variant(variant).is_some(),
                [MAPPING, variant, value, received] => {
                    ReceivedCase::from_variant(variant).is_some()
                        && matches!(*value, "true" | "false")
                        && matches!(*received, "true" | "false")
                }
                _ => false,
            };
        if valid {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown IPC event: {event}"),
            ))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Release {
        Normal,
        Cleanup,
    }

    impl Release {
        pub(crate) fn command(self) -> &'static str {
            match self {
                Self::Normal => RELEASE,
                Self::Cleanup => CLEANUP_RELEASE,
            }
        }

        pub(crate) fn acknowledgment(self) -> &'static str {
            match self {
                Self::Normal => RELEASED_NORMAL,
                Self::Cleanup => RELEASED_CLEANUP,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum ReceivedCase {
        Cancelled,
        Refused,
        FinalizationTimedOut,
    }

    impl ReceivedCase {
        pub(crate) fn mode(self) -> &'static str {
            match self {
                Self::Cancelled => "cancelled",
                Self::Refused => "refused",
                Self::FinalizationTimedOut => "finalization-timed-out",
            }
        }

        pub(crate) fn variant(self) -> &'static str {
            match self {
                Self::Cancelled => "Cancelled",
                Self::Refused => "Refused",
                Self::FinalizationTimedOut => "FinalizationTimedOut",
            }
        }

        pub(crate) fn from_mode(mode: &str) -> Option<Self> {
            [Self::Cancelled, Self::Refused, Self::FinalizationTimedOut]
                .iter()
                .copied()
                .find(|case| case.mode() == mode)
        }

        fn from_variant(variant: &str) -> Option<Self> {
            [Self::Cancelled, Self::Refused, Self::FinalizationTimedOut]
                .iter()
                .copied()
                .find(|case| case.variant() == variant)
        }
    }
}

use khive_runtime::{
    resource_admission::{JobOutcome, PriorityError},
    RuntimeError,
};
use protocol::{ReceivedCase, Release};

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
        protocol::validate(event)?;
        let message = format!("{} {event}", self.nonce);
        let sent = self.socket.send(message.as_bytes())?;
        if sent != message.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "child IPC frame"));
        }
        Ok(())
    }

    fn release(&self) -> Release {
        let mut bytes = [0; 1024];
        let length = match self.socket.recv(&mut bytes) {
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                self.send(protocol::EXPIRED);
                panic!("worker gate failsafe expired");
            }
            Err(error) => {
                self.send(protocol::IPC_ERROR);
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
            protocol::RELEASE => Release::Normal,
            protocol::CLEANUP_RELEASE => Release::Cleanup,
            other => panic!("unexpected parent gate message: {other}"),
        }
    }
}

struct State {
    ipc: Ipc,
    captures: AtomicUsize,
    entries: AtomicUsize,
    released: AtomicBool,
    body_scope_exited: AtomicBool,
    received_case: Option<ReceivedCase>,
    replacements: AtomicUsize,
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
        self.0.released.load(Ordering::SeqCst)
    }

    pub(crate) fn wait_body_scope_exit(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while !self.0.body_scope_exited.load(Ordering::SeqCst) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
        }
        true
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
    install_received(ipc, None)
}

pub(crate) fn install_received(ipc: Ipc, received_case: Option<ReceivedCase>) -> Installation {
    let state = Arc::new(State {
        ipc,
        captures: AtomicUsize::new(0),
        entries: AtomicUsize::new(0),
        released: AtomicBool::new(false),
        body_scope_exited: AtomicBool::new(false),
        received_case,
        replacements: AtomicUsize::new(0),
    });
    let mut installed = INSTALLED.lock().expect("gate installation lock");
    assert!(installed.is_none(), "one gate installation per exact child");
    *installed = Some(state.clone());
    Installation(state)
}

pub(crate) struct CapturedGate(Arc<State>);

pub(crate) fn capture() -> Option<CapturedGate> {
    capture_for(protocol::INTERACTIVE)
}

pub(crate) fn capture_background() -> Option<CapturedGate> {
    capture_for(protocol::BACKGROUND)
}

fn capture_for(context: &'static str) -> Option<CapturedGate> {
    let state = INSTALLED
        .lock()
        .expect("gate installation lock")
        .as_ref()
        .cloned()?;
    let count = state.captures.fetch_add(1, Ordering::SeqCst) + 1;
    state.ipc.send(context);
    state.ipc.send(&format!("{} {count}", protocol::CAPTURE));
    assert_eq!(count, 1, "actual_build_gate_capture_count");
    Some(CapturedGate(state))
}

impl CapturedGate {
    pub(crate) fn enter(self) -> BodyScopeExit {
        let count = self.0.entries.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.ipc.send(&format!("{} {count}", protocol::ENTERED));
        assert_eq!(count, 1, "actual_build_gate_entry_count");
        let release = self.0.ipc.release();
        self.0.released.store(true, Ordering::SeqCst);
        self.0.ipc.send(release.acknowledgment());
        BodyScopeExit(self.0)
    }
}

pub(crate) struct BodyScopeExit(Arc<State>);

impl Drop for BodyScopeExit {
    fn drop(&mut self) {
        let event = if std::thread::panicking() {
            protocol::BODY_PANIC
        } else {
            protocol::BODY_RETURN
        };
        if let Err(error) = self.0.ipc.try_send(event) {
            eprintln!("BODY_SCOPE_SEND_FAILED: {error}");
        }
        self.0.body_scope_exited.store(true, Ordering::SeqCst);
    }
}

/// Replace only the received value after real admission and supervisor completion.
/// This synthetic Refused payload is a named test value, not scheduler-refusal evidence.
pub(crate) fn after_real_submit<T>(
    outcome: JobOutcome<Result<T, RuntimeError>>,
) -> JobOutcome<Result<T, RuntimeError>> {
    let state = INSTALLED
        .lock()
        .expect("gate installation lock")
        .as_ref()
        .cloned();
    let Some(state) = state else {
        return outcome;
    };
    let Some(case) = state.received_case else {
        return outcome;
    };
    assert!(
        matches!(&outcome, JobOutcome::Completed(Ok(_))),
        "real_submit_must_succeed"
    );
    let count = state.replacements.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(count, 1, "received_replacement_count");
    state.ipc.send(protocol::REAL_SUBMIT);
    state
        .ipc
        .send(&format!("{} {}", protocol::REPLACED, case.variant()));
    match case {
        ReceivedCase::Cancelled => JobOutcome::Cancelled,
        ReceivedCase::Refused => JobOutcome::Refused(PriorityError::Unsupported { target: "test" }),
        ReceivedCase::FinalizationTimedOut => JobOutcome::FinalizationTimedOut,
    }
}
