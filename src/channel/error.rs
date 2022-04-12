//! Common errors for selectable.

/// Error for blocking send.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SendError<T> {
    Closed(T),
}

/// Error for nonblocking send.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

/// Error for nonblocking receive.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(err: SendError<T>) -> Self {
        let SendError::Closed(value) = err;
        TrySendError::Closed(value)
    }
}

impl<T> From<TrySendError<T>> for SendError<T> {
    fn from(err: TrySendError<T>) -> Self {
        match err {
            TrySendError::Closed(value) => SendError::Closed(value),
            TrySendError::Full(_) => panic!("got full error in blocking send"),
        }
    }
}
