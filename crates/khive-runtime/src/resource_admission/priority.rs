//! Applied once on isolated worker lifetimes, never on a shared async worker.
//! A TLS gate suppresses our controlled wrappers after failure. It is not a
//! claim that Tokio/backend-internal blocking closures use that gate.
use super::PriorityError;
use std::cell::RefCell;

thread_local! {
    static POSTURE: RefCell<Option<Result<(), PriorityError>>> = const { RefCell::new(None) };
}

pub(super) fn lower_current_thread() -> Result<(), PriorityError> {
    let result = platform_lower();
    POSTURE.with(|posture| *posture.borrow_mut() = Some(result.clone()));
    result
}
pub(super) fn check_current_thread() -> Result<(), PriorityError> {
    POSTURE.with(|posture| {
        posture
            .borrow()
            .clone()
            .unwrap_or(Err(PriorityError::Unexpected {
                operation: "background worker entry was not initialized",
                value: 0,
            }))
    })
}
#[cfg(any(target_os = "linux", windows))]
fn os_error(operation: &'static str) -> PriorityError {
    PriorityError::Os {
        operation,
        code: std::io::Error::last_os_error().raw_os_error(),
    }
}

#[cfg(target_os = "linux")]
fn platform_lower() -> Result<(), PriorityError> {
    fn read() -> Result<i32, PriorityError> {
        // SAFETY: errno is this thread's libc TLS slot. PRIO_PROCESS/0 targets
        // this Linux/NPTL thread; the typed libc constant handles GNU/musl ABI.
        let (value, error) = unsafe {
            *libc::__errno_location() = 0;
            let value = libc::getpriority(libc::PRIO_PROCESS, 0);
            (value, *libc::__errno_location())
        };
        if error != 0 {
            return Err(PriorityError::Os {
                operation: "getpriority",
                code: Some(error),
            });
        }
        if !(-20..=19).contains(&value) {
            return Err(PriorityError::Unexpected {
                operation: "getpriority",
                value,
            });
        }
        Ok(value)
    }
    let current = read()?;
    // Linux nice has endpoints -20..19. Do not request a more favorable value
    // when a pool worker inherits an already-lowered coordinator posture.
    let desired = if current == 19 { current } else { current + 1 };
    let target = current.max(desired);
    // SAFETY: scalar arguments select only the current Linux thread.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, target) } != 0 {
        return Err(os_error("setpriority"));
    }
    let observed = read()?;
    if observed < target {
        return Err(PriorityError::Unexpected {
            operation: "getpriority after lowering",
            value: observed,
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_lower() -> Result<(), PriorityError> {
    // ABI grounded in Apple sys/qos.h and pthread/qos.h via the frozen libc
    // source: qos_class_t is repr(u32), Background is 0x09. This tiny declaration
    // avoids assuming a root reexport spelling in an unverified libc version.
    #[repr(u32)]
    enum QosClass {
        Background = 0x09,
    }
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(
            class: QosClass,
            priority: std::ffi::c_int,
        ) -> std::ffi::c_int;
    }
    // SAFETY: valid documented QoS class/relative priority, current thread only.
    let code = unsafe { pthread_set_qos_class_self_np(QosClass::Background, 0) };
    if code != 0 {
        return Err(PriorityError::Os {
            operation: "pthread_set_qos_class_self_np",
            code: Some(code),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn platform_lower() -> Result<(), PriorityError> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn GetThreadPriority(thread: *mut c_void) -> i32;
        fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
    }
    // Only named documented relative priorities are recognized. An unknown
    // value, including the error sentinel, is refused without guessing its
    // numeric declaration or treating arbitrary last-error as authoritative.
    fn recognized(value: i32) -> bool {
        matches!(value, -15 | -2 | -1 | 0 | 1 | 2 | 15)
    }
    // SAFETY: documented current-thread pseudo-handle. It is used only here,
    // on this thread, and must not be closed.
    let thread = unsafe { GetCurrentThread() };
    let current = unsafe { GetThreadPriority(thread) };
    if !recognized(current) {
        let diagnostic_os_error = std::io::Error::last_os_error().raw_os_error();
        tracing::warn!(
            returned = current,
            ?diagnostic_os_error,
            "unrecognized GetThreadPriority result; OS error is diagnostic only"
        );
        return Err(PriorityError::Unexpected {
            operation: "GetThreadPriority",
            value: current,
        });
    }
    let target = current.min(-1);
    // SAFETY: same pseudo-handle and a recognized relative priority.
    if unsafe { SetThreadPriority(thread, target) } == 0 {
        return Err(os_error("SetThreadPriority"));
    }
    let observed = unsafe { GetThreadPriority(thread) };
    if !recognized(observed) || observed > target {
        let diagnostic_os_error = std::io::Error::last_os_error().raw_os_error();
        tracing::warn!(
            returned = observed,
            ?diagnostic_os_error,
            "unexpected GetThreadPriority verification; OS error is diagnostic only"
        );
        return Err(PriorityError::Unexpected {
            operation: "GetThreadPriority after lowering",
            value: observed,
        });
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_lower() -> Result<(), PriorityError> {
    Err(PriorityError::Unsupported {
        target: std::env::consts::OS,
    })
}

#[cfg(test)]
pub(super) fn refuse_current_thread_for_test() {
    POSTURE.with(|posture| {
        *posture.borrow_mut() = Some(Err(PriorityError::Unexpected {
            operation: "test posture refusal",
            value: -999,
        }))
    });
}
