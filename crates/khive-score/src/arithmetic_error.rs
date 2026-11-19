//! Validation failures for checked floating-point score arithmetic.

use std::fmt;

/// An invalid input or unrepresentable intermediate in checked score arithmetic.
///
/// These errors describe arithmetic domains. Callers retain their own public
/// diagnostics and configuration policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticError {
    /// An input is NaN or infinite.
    NonFinite { parameter: &'static str },
    /// A finite input lies outside the operation's domain.
    OutOfRange { parameter: &'static str },
    /// An ordered arithmetic intermediate is not finite.
    Overflow { operation: &'static str },
    /// A finite lower bound exceeds its upper bound.
    InvalidBounds,
    /// An ordered weight sequence is empty.
    EmptyWeights,
    /// An ordered weight sequence has no strictly positive weight.
    NoPositiveWeight,
    /// The declared feature sequence names a key more than once.
    DuplicateKey,
}

impl fmt::Display for ArithmeticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { parameter } => write!(f, "{parameter} must be finite"),
            Self::OutOfRange { parameter } => write!(f, "{parameter} is outside its valid range"),
            Self::Overflow { operation } => {
                write!(f, "{operation} produced a nonfinite intermediate")
            }
            Self::InvalidBounds => f.write_str("lower bound exceeds upper bound"),
            Self::EmptyWeights => f.write_str("weight sequence must not be empty"),
            Self::NoPositiveWeight => f.write_str("weight sequence needs a positive weight"),
            Self::DuplicateKey => f.write_str("declared feature keys must be unique"),
        }
    }
}

impl std::error::Error for ArithmeticError {}
