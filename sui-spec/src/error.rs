//! Errors produced while loading or interpreting sui-spec specs.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum SpecError {
    #[error("spec load error: {0}")]
    Load(String),

    #[error("spec compile error: {0}")]
    Compile(String),

    #[error("interpreter error in {phase}: {message}")]
    Interp { phase: String, message: String },

    #[error("unbound slot {0} — earlier phase never wrote to it")]
    UnboundSlot(String),
}

impl From<tatara_lisp::LispError> for SpecError {
    fn from(err: tatara_lisp::LispError) -> Self {
        SpecError::Compile(err.to_string())
    }
}
