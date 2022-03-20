use std::any::Any;
use std::fmt;

use static_assertions::assert_impl_all;

pub(crate) type PanicError = Box<dyn Any + Send + 'static>;

/// Wraps panic as [std::error::Error].
pub struct JoinError {
    panicked: Box<dyn Any + Send + 'static>,
}

assert_impl_all!(JoinError: Send);

impl JoinError {
    fn as_str(&self) -> Option<&str> {
        let panicked = self.panicked.as_ref();
        if let Some(s) = panicked.downcast_ref::<&str>() {
            Some(s)
        } else if let Some(s) = panicked.downcast_ref::<String>() {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub(crate) fn new(err: Box<dyn Any + Send + 'static>) -> Self {
        JoinError { panicked: err }
    }

    /// Converts this error to panicked object.
    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        self.panicked
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            None => write!(f, "JoinError::Panic({:?})", self.panicked.as_ref().type_id()),
            Some(s) => write!(f, "JoinError::Panic({:?})", s),
        }
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "panic({:?})", self.as_str().unwrap_or(".."))
    }
}

impl std::error::Error for JoinError {}
