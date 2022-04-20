use std::any::{Any, TypeId};
use std::fmt;

use static_assertions::assert_impl_all;

pub(crate) enum PanicError {
    Static(&'static str),
    Unwind(Box<dyn Any + Send + 'static>),
}

impl PanicError {
    fn into_boxed(self) -> Box<dyn Any + Send + 'static> {
        match self {
            PanicError::Unwind(boxed) => boxed,
            PanicError::Static(str) => Box::new(str),
        }
    }

    fn as_str(&self) -> Result<&str, TypeId> {
        match self {
            PanicError::Static(s) => Ok(s),
            PanicError::Unwind(boxed) => {
                let panicked = boxed.as_ref();
                if let Some(s) = panicked.downcast_ref::<&str>() {
                    Ok(s)
                } else if let Some(s) = panicked.downcast_ref::<String>() {
                    Ok(s.as_str())
                } else {
                    Err(panicked.type_id())
                }
            },
        }
    }
}

/// Wraps panic as [std::error::Error].
pub struct JoinError {
    err: PanicError,
}

assert_impl_all!(JoinError: Send);

impl JoinError {
    pub(crate) fn new(err: PanicError) -> Self {
        JoinError { err }
    }

    /// Converts this error to panicked object.
    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        self.err.into_boxed()
    }
}

impl From<PanicError> for JoinError {
    fn from(err: PanicError) -> Self {
        JoinError { err }
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.err.as_str() {
            Ok(s) => write!(f, "JoinError::Panic({})", s),
            Err(type_id) => write!(f, "JoinError::Panic({:?})", type_id),
        }
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "panic({})", self.err.as_str().unwrap_or(".."))
    }
}

impl std::error::Error for JoinError {}
