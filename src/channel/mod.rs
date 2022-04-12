//! Channel utilities for commnication across coroutines and tasks.

mod error;
pub mod parallel;
pub mod prelude;
pub mod select;
mod select_macro;
pub mod serial;

pub use self::error::{SendError, TryRecvError, TrySendError};

/// Sending peer of [Receiver].
pub trait Sender<T> {
    /// Sends a value to receiving peer.
    ///
    /// This operation could block if channel is full.
    fn send(&mut self, value: T) -> Result<(), SendError<T>>;

    /// Attempts to send a value to receiver peer without blocking current execution.
    fn try_send(&mut self, value: T) -> Result<(), TrySendError<T>>;

    /// Closes this sender. If this is the last sender, the underlying channel will also be closed.
    fn close(&mut self);

    /// Returns true if sender or channel closed.
    ///
    /// It may not reflect newest channel status, but it must reflect last send or close. That is
    /// if caller observed [SendError::Closed] or [TrySendError::Closed] from last [Sender::send]
    /// or [Sender::try_send], it must return `true`.
    fn is_closed(&self) -> bool;
}

/// Receiving peer of [Sender].
pub trait Receiver<T> {
    /// Receives a value from [Sender]s.
    ///
    /// Returns [None] if channel is closed and has no buffered values. A channel is considered
    /// as closed if all senders have been dropped or [Receiver::close] has been called.
    fn recv(&mut self) -> Option<T>;

    /// Attempts to receive a value from channel without blocking current execution.
    fn try_recv(&mut self) -> Result<T, TryRecvError>;

    /// Closes this channel for future sending.
    fn close(&mut self);

    /// Returns true if channel is closed and has no buffered values.
    ///
    /// It may not reflect newest channel status, but it must reflect last recv. That is if caller
    /// observed `None` from [Receiver::recv] or [TryRecvError::Closed] from [Receiver::try_recv],
    /// it must return `true`.
    fn is_drained(&self) -> bool;
}
