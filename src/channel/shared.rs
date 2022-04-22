use std::marker::PhantomData;

use num_enum::{IntoPrimitive, UnsafeFromPrimitive};

use super::error::{SendError, TryRecvError, TrySendError};
use super::{Receiver as _, Sender as _};
use crate::select::{self, Identifier, PermitReader, PermitWriter, Selectable, Selector, TrySelectError};

#[repr(usize)]
#[derive(Copy, Clone, PartialEq, Eq, IntoPrimitive, UnsafeFromPrimitive)]
pub enum Permit {
    Closed = 0,
    Consume = 1,
}

impl From<Permit> for select::Permit {
    fn from(permit: Permit) -> Self {
        Self::with_primitive(permit.into())
    }
}

impl From<select::Permit> for Permit {
    fn from(permit: select::Permit) -> Self {
        unsafe { Permit::from_unchecked(permit.into_primitive()) }
    }
}

pub trait Channel<T>: Clone {
    fn send(&self, trying: bool, value: T) -> Result<(), TrySendError<T>>;

    fn add_sender(&self);
    fn remove_sender(&self);

    fn select_send_permit(&self) -> Option<Permit>;
    fn consume_send_permit(&self, value: T) -> Result<(), SendError<T>>;

    fn watch_send_permit(&self, selector: Selector) -> bool;
    fn unwatch_send_permit(&self, identifier: &Identifier);

    fn recv(&self, trying: bool) -> Result<T, TryRecvError>;

    fn add_receiver(&self);
    fn remove_receiver(&self);

    fn select_recv_permit(&self) -> Option<Permit>;
    fn consume_recv_permit(&self) -> Option<T>;

    fn watch_recv_permit(&self, selector: Selector) -> bool;
    fn unwatch_recv_permit(&self, identifier: &Identifier);

    fn close(&self);
}

pub struct Sender<T, C: Channel<T>> {
    channel: Option<C>,
    marker: PhantomData<T>,
}

impl<T, C: Channel<T>> Sender<T, C> {
    pub fn empty() -> Self {
        Sender { channel: None, marker: PhantomData }
    }

    pub fn new(channel: C) -> Self {
        Sender { channel: Some(channel), marker: PhantomData }
    }
}

impl<T, C: Channel<T>> super::Sender<T> for Sender<T, C> {
    fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        if let Some(channel) = &self.channel {
            return match channel.send(false, value) {
                Ok(_) => Ok(()),
                Err(TrySendError::Full(_)) => unreachable!("got full in blocking send"),
                Err(TrySendError::Closed(value)) => {
                    channel.remove_sender();
                    self.channel = None;
                    Err(SendError::Closed(value))
                },
            };
        }
        Err(SendError::Closed(value))
    }

    fn try_send(&mut self, value: T) -> Result<(), TrySendError<T>> {
        if let Some(channel) = &self.channel {
            return match channel.send(true, value) {
                Ok(_) => Ok(()),
                err @ Err(TrySendError::Full(_)) => err,
                err => {
                    channel.remove_sender();
                    self.channel = None;
                    err
                },
            };
        }
        Err(TrySendError::Closed(value))
    }

    fn close(&mut self) {
        if let Some(channel) = self.channel.take() {
            channel.remove_sender();
        }
    }

    fn is_closed(&self) -> bool {
        self.channel.is_none()
    }
}

impl<T, C: Channel<T>> Clone for Sender<T, C> {
    fn clone(&self) -> Self {
        match &self.channel {
            None => Sender::empty(),
            Some(channel) => {
                channel.add_sender();
                Sender::new(channel.clone())
            },
        }
    }
}

impl<T, C: Channel<T>> Drop for Sender<T, C> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T, C: Channel<T>> Selectable for Sender<T, C> {
    fn parallel(&self) -> bool {
        self.channel.is_some()
    }

    fn select_permit(&self) -> Result<select::Permit, TrySelectError> {
        if let Some(channel) = &self.channel {
            channel.select_send_permit().map(From::from).map(Result::Ok).unwrap_or(Err(TrySelectError::WouldBlock))
        } else {
            Err(TrySelectError::Completed)
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        self.channel.as_ref().map(|channel| channel.watch_send_permit(selector)).unwrap_or(false)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        if let Some(channel) = &self.channel {
            channel.unwatch_send_permit(identifier)
        }
    }
}

impl<T, C: Channel<T>> PermitWriter for Sender<T, C> {
    type Item = T;
    type Result = Result<(), SendError<T>>;

    fn consume_permit(&mut self, permit: select::Permit, value: Self::Item) -> Self::Result {
        let permit = Permit::from(permit);
        if permit == Permit::Closed {
            self.channel = None;
            Err(SendError::Closed(value))
        } else if let Some(channel) = &self.channel {
            let result = channel.consume_send_permit(value);
            if let Err(SendError::Closed(_)) = &result {
                channel.remove_sender();
                self.channel = None;
            }
            result
        } else {
            Err(SendError::Closed(value))
        }
    }
}

pub struct Receiver<T, C: Channel<T>> {
    channel: Option<C>,
    marker: PhantomData<T>,
}

impl<T, C: Channel<T>> Receiver<T, C> {
    pub fn empty() -> Self {
        Receiver { channel: None, marker: PhantomData }
    }

    pub fn new(channel: C) -> Self {
        Receiver { channel: Some(channel), marker: PhantomData }
    }
}

impl<T, C: Channel<T>> super::Receiver<T> for Receiver<T, C> {
    fn recv(&mut self) -> Option<T> {
        if let Some(channel) = &self.channel {
            return match channel.recv(false) {
                Ok(value) => Some(value),
                Err(TryRecvError::Empty) => unreachable!("got empty in blocking recv"),
                Err(TryRecvError::Closed) => {
                    channel.remove_receiver();
                    self.channel = None;
                    None
                },
            };
        }
        None
    }

    fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(channel) = &self.channel {
            return match channel.recv(true) {
                ok @ Ok(_) => ok,
                err @ Err(TryRecvError::Empty) => err,
                err => {
                    channel.remove_receiver();
                    self.channel = None;
                    err
                },
            };
        }
        Err(TryRecvError::Closed)
    }

    fn close(&mut self) {
        if let Some(channel) = &self.channel {
            channel.close();
        }
    }

    fn terminate(&mut self) {
        if let Some(channel) = self.channel.take() {
            channel.remove_receiver();
        }
    }

    fn is_drained(&self) -> bool {
        self.channel.is_none()
    }
}

impl<T, C: Channel<T>> Clone for Receiver<T, C> {
    fn clone(&self) -> Self {
        match self.channel {
            None => Receiver::empty(),
            Some(ref channel) => {
                channel.add_receiver();
                Receiver::new(channel.clone())
            },
        }
    }
}

impl<T, C: Channel<T>> Drop for Receiver<T, C> {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl<T, C: Channel<T>> Selectable for Receiver<T, C> {
    fn parallel(&self) -> bool {
        self.channel.is_some()
    }

    fn select_permit(&self) -> Result<select::Permit, TrySelectError> {
        if let Some(channel) = &self.channel {
            channel.select_recv_permit().map(From::from).map(Result::Ok).unwrap_or(Err(TrySelectError::WouldBlock))
        } else {
            Err(TrySelectError::Completed)
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        self.channel.as_ref().map(|channel| channel.watch_recv_permit(selector)).unwrap_or(false)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        if let Some(channel) = &self.channel {
            channel.unwatch_recv_permit(identifier)
        }
    }
}

impl<T, C: Channel<T>> PermitReader for Receiver<T, C> {
    type Result = Option<T>;

    fn consume_permit(&mut self, permit: select::Permit) -> Self::Result {
        let permit = Permit::from(permit);
        if permit == Permit::Closed {
            self.channel = None;
            None
        } else if let Some(channel) = &self.channel {
            let result = channel.consume_recv_permit();
            if result.is_none() {
                channel.remove_receiver();
                self.channel = None;
            }
            result
        } else {
            None
        }
    }
}
