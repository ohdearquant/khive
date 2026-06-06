use thiserror::Error;

// ---------- Error ----------

/// Errors returned by [`Gate::check`].
#[derive(Error, Debug)]
pub enum GateError {
    #[error("policy error: {0}")]
    Policy(String),
    #[error("evaluation error: {0}")]
    Evaluation(String),
    #[error("internal gate error: {0}")]
    Internal(String),
}
