use khive_runtime::RuntimeError;
use khive_types::{Details, KhiveError};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    pub class: &'static str,
    pub reason: &'static str,
    pub fatal: bool,
}

impl Failure {
    pub fn error(reason: &'static str) -> Self {
        Self {
            class: "tool_error",
            reason,
            fatal: false,
        }
    }
    pub fn malformed() -> Self {
        Self {
            class: "tool_malformed",
            reason: "invalid_response",
            fatal: true,
        }
    }
    pub fn timeout() -> Self {
        Self {
            class: "tool_timeout",
            reason: "deadline_exceeded",
            fatal: true,
        }
    }
    pub fn io() -> Self {
        Self {
            class: "tool_error",
            reason: "transport_closed",
            fatal: true,
        }
    }
    pub fn wire(self, mount: &str) -> RuntimeError {
        KhiveError::unavailable(self.class)
            .with_details(Details::new_owned([
                ("class", self.class.to_string()),
                ("reason", self.reason.to_string()),
                ("mount", mount.to_string()),
            ]))
            .into()
    }
}
