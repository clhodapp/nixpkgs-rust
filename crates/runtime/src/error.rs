//! Evaluation errors.

use std::fmt;

/// What kind of failure an [`Error`] is. `builtins.tryEval` catches
/// [`Kind::Throw`] and [`Kind::Assert`] and nothing else, as Nix does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `throw`, and the errors Nix reports as thrown (a missing attribute is not one).
    Throw,
    /// A failed `assert`.
    Assert,
    /// `abort`.
    Abort,
    /// Every other evaluation error: type errors, missing attributes, and so on.
    Eval,
    /// A thunk was forced while it was already being forced.
    InfiniteRecursion,
}

/// An evaluation error with the context lines collected while it propagated.
#[derive(Debug, Clone)]
pub struct Error {
    pub kind: Kind,
    pub msg: String,
    /// Context added by `builtins.addErrorContext` and by the runtime, innermost first.
    pub trace: Vec<String>,
}

/// The result type of everything that evaluates. The error is boxed so the
/// result of a successful evaluation stays the size of a value.
pub type R<T> = Result<T, Box<Error>>;

impl Error {
    pub fn new(kind: Kind, msg: impl Into<String>) -> Box<Error> {
        Box::new(Error {
            kind,
            msg: msg.into(),
            trace: Vec::new(),
        })
    }

    pub fn eval(msg: impl Into<String>) -> Box<Error> {
        Error::new(Kind::Eval, msg)
    }

    /// Whether `builtins.tryEval` catches this error.
    pub fn catchable(&self) -> bool {
        matches!(self.kind, Kind::Throw | Kind::Assert)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for t in self.trace.iter().rev() {
            writeln!(f, "       … {t}")?;
        }
        match self.kind {
            Kind::Abort => write!(f, "error: evaluation aborted with the following error message: '{}'", self.msg),
            Kind::Assert => write!(f, "error: {}", self.msg),
            _ => write!(f, "error: {}", self.msg),
        }
    }
}

/// Returns an [`Kind::Eval`] error built from a format string.
#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => {
        return Err($crate::error::Error::eval(format!($($t)*)))
    };
}

/// Adds a context line to an error as it propagates.
pub trait Context<T> {
    fn context(self, f: impl FnOnce() -> String) -> R<T>;
}

impl<T> Context<T> for R<T> {
    fn context(self, f: impl FnOnce() -> String) -> R<T> {
        self.map_err(|mut e| {
            if e.trace.len() < 64 {
                e.trace.push(f());
            }
            e
        })
    }
}
