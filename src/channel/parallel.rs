//! Channel utilities for commnication across tasks.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use num_enum::{IntoPrimitive, UnsafeFromPrimitive};

use crate::channel::prelude::*;
use crate::channel::{self, SendError, TryRecvError, TrySendError};
use crate::select::{self, Identifier, PermitReader, PermitWriter, Selectable, Selector};
use crate::task::{self, SessionWaker};

#[repr(usize)]
#[derive(Copy, Clone, PartialEq, Eq, IntoPrimitive, UnsafeFromPrimitive)]
enum Permit {
    Send = 0,
    Recv = 1,
    Closed = 2,
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

enum Waiter<T: Send + 'static> {
    Task { waker: SessionWaker<Result<(), SendError<T>>>, value: T },
    Thread { waker: Arc<ThreadWaker<SendError<T>>>, value: T },
    Selector { selector: Selector },
}

impl<T: Send + 'static> Waiter<T> {
    fn matches(&self, identifier: &Identifier) -> bool {
        if let Waiter::Selector { selector } = self {
            selector.identify(identifier)
        } else {
            false
        }
    }
}

impl<T: Send + 'static> From<Selector> for Waiter<T> {
    fn from(selector: Selector) -> Self {
        Waiter::Selector { selector }
    }
}

enum RecvWaiter {
    Selector { selector: Selector },
    Receiver { session: SessionWaker<Permit> },
}

impl From<Selector> for RecvWaiter {
    fn from(selector: Selector) -> Self {
        RecvWaiter::Selector { selector }
    }
}

impl From<SessionWaker<Permit>> for RecvWaiter {
    fn from(session: SessionWaker<Permit>) -> Self {
        RecvWaiter::Receiver { session }
    }
}

impl RecvWaiter {
    fn matches(&self, identifier: &Identifier) -> bool {
        if let RecvWaiter::Selector { selector } = self {
            selector.identify(identifier)
        } else {
            false
        }
    }
}

struct ThreadWaker<T> {
    condvar: Condvar,
    result: UnsafeCell<Option<Result<(), T>>>,
}

impl<T> ThreadWaker<T> {
    fn new() -> Arc<ThreadWaker<T>> {
        Arc::new(ThreadWaker { condvar: Condvar::new(), result: UnsafeCell::new(None) })
    }

    // This should be called under mutex for result mutation.
    unsafe fn wake(&self, r: Result<(), T>) {
        let result = &mut *self.result.get();
        *result = Some(r);
        self.condvar.notify_one();
    }
}

struct State<T: Send + 'static> {
    closed: bool,

    send_permits: usize,
    recv_permits: usize,

    bound: usize,
    deque: VecDeque<T>,
    senders: VecDeque<Waiter<T>>,
    receivers: VecDeque<RecvWaiter>,
}

impl<T: Send + 'static> State<T> {
    fn is_full(&self) -> bool {
        self.deque.len() + self.send_permits == self.bound
    }

    fn is_empty(&self) -> bool {
        // `>` could happen when:
        // * channeld closed due to all senders dropped.
        // * pending recv permits
        // * new recv permit reservation
        //
        // This allows permit reservation non blocking.
        self.recv_permits >= self.deque.len()
    }

    fn reserve_send_permit(&mut self) -> Option<Permit> {
        if self.closed {
            Some(Permit::Closed)
        } else if !self.is_full() {
            self.send_permits += 1;
            Some(Permit::Send)
        } else {
            None
        }
    }

    fn reserve_recv_permit(&mut self) -> Option<Permit> {
        if !self.is_empty() {
            self.recv_permits += 1;
            Some(Permit::Recv)
        } else if self.closed {
            if self.send_permits != 0 {
                None
            } else if self.deque.is_empty() {
                Some(Permit::Closed)
            } else {
                // Let permittees contend for remaining values.
                self.recv_permits += 1;
                Some(Permit::Recv)
            }
        } else {
            None
        }
    }

    fn is_recvable(&self) -> bool {
        self.send_permits != 0 || !self.deque.is_empty()
    }

    fn wake_sender(&mut self) {
        while let Some(sender) = self.senders.pop_front() {
            match sender {
                Waiter::Task { waker, value } => {
                    self.deque.push_back(value);
                    waker.wake(Ok(()));
                    return;
                },
                Waiter::Thread { waker, value } => {
                    self.deque.push_back(value);
                    unsafe { waker.wake(Ok(())) };
                    return;
                },
                Waiter::Selector { selector } => {
                    if selector.apply(Permit::Send.into()) {
                        self.send_permits += 1;
                        return;
                    }
                },
            }
        }
    }

    fn wake_receiver(&mut self) {
        while let Some(waker) = self.receivers.pop_front() {
            let waked = match waker {
                RecvWaiter::Selector { selector } => selector.apply(Permit::Recv.into()),
                RecvWaiter::Receiver { session } => session.wake(Permit::Recv),
            };
            if waked {
                self.recv_permits += 1;
                break;
            }
        }
    }

    fn close_senders(&mut self) {
        for waiter in self.senders.drain(..) {
            match waiter {
                Waiter::Task { waker, value } => {
                    waker.wake(Err(SendError::Closed(value)));
                },
                Waiter::Thread { waker, value } => {
                    unsafe { waker.wake(Err(SendError::Closed(value))) };
                },
                Waiter::Selector { selector } => {
                    selector.apply(Permit::Closed.into());
                },
            }
        }
    }

    fn close_receivers(&mut self) {
        while let Some(waker) = self.receivers.pop_front() {
            match waker {
                RecvWaiter::Selector { selector } => selector.apply(Permit::Closed.into()),
                RecvWaiter::Receiver { session } => session.wake(Permit::Closed),
            };
        }
    }

    fn new(capacity: Capacity) -> Self {
        State {
            send_permits: 0,
            recv_permits: 0,
            closed: false,
            bound: capacity.max,
            deque: VecDeque::with_capacity(capacity.min),
            senders: VecDeque::with_capacity(5),
            receivers: VecDeque::with_capacity(5),
        }
    }

    fn close(&mut self) {
        self.closed = true;
        self.close_senders();
        if !self.is_recvable() {
            self.close_receivers();
        }
    }
}

struct Channel<T: Send + 'static> {
    state: Mutex<State<T>>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
}

// SAFETY: There are multiple accessors.
unsafe impl<T: Send> Sync for Channel<T> {}

// SAFETY: Multiple acccessors are distributed across tasks and threads.
unsafe impl<T: Send> Send for Channel<T> {}

impl<T: Send + 'static> Channel<T> {
    fn new(capacity: Capacity) -> Arc<Self> {
        Arc::new(Channel {
            state: Mutex::new(State::new(capacity)),
            senders: AtomicUsize::new(1),
            receivers: AtomicUsize::new(1),
        })
    }

    fn add_sender(&self) {
        self.senders.fetch_add(1, Ordering::Relaxed);
    }

    fn remove_sender(&self) {
        if self.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            let mut state = self.state.lock().unwrap();
            state.close();
        }
    }

    fn add_receiver(&self) {
        self.receivers.fetch_add(1, Ordering::Relaxed);
    }

    fn remove_receiver(&self) {
        if self.receivers.fetch_sub(1, Ordering::Relaxed) == 1 {
            let mut state = self.state.lock().unwrap();
            state.close();
        }
    }

    fn recv(&self, trying: bool) -> Result<T, TryRecvError> {
        let mut state = self.state.lock().unwrap();
        let value = if state.is_empty() {
            if state.closed && !state.is_recvable() {
                return Err(TryRecvError::Closed);
            } else if trying {
                return Err(TryRecvError::Empty);
            }
            let (session, waker) = task::session();
            state.receivers.push_back(RecvWaiter::from(waker));
            drop(state);
            let permit = session.wait();
            assert!(permit != Permit::Send);
            if permit == Permit::Closed {
                return Err(TryRecvError::Closed);
            }
            self.consume_recv_permit()
        } else {
            state.wake_sender();
            state.deque.pop_front()
        };
        match value {
            None => Err(TryRecvError::Closed),
            Some(value) => Ok(value),
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.close();
    }

    fn watch_send_permit(&self, selector: Selector) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(permit) = state.reserve_send_permit() {
            if !selector.apply(permit.into()) && permit == Permit::Send {
                state.send_permits -= 1;
            }
            return true;
        }
        state.senders.push_back(Waiter::from(selector));
        false
    }

    fn unwatch_send_permit(&self, identifier: &Identifier) {
        let mut state = self.state.lock().unwrap();
        if let Some(position) = state.senders.iter().position(|waker| waker.matches(identifier)) {
            state.receivers.remove(position);
        }
    }

    fn reserve_send_permit(&self) -> Option<Permit> {
        let mut state = self.state.lock().unwrap();
        state.reserve_send_permit()
    }

    fn consume_send_permit(&self, value: T) -> Result<(), SendError<T>> {
        let mut state = self.state.lock().unwrap();
        assert!(state.send_permits > 0);
        state.send_permits -= 1;
        assert!(!state.is_full());
        state.deque.push_back(value);
        if !state.closed {
            state.wake_receiver();
        } else if !state.is_recvable() {
            state.close_receivers();
        }
        Ok(())
    }

    fn reserve_recv_permit(&self) -> Option<Permit> {
        let mut state = self.state.lock().unwrap();
        state.reserve_recv_permit()
    }

    fn consume_recv_permit(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        let full = state.is_full();
        state.recv_permits -= 1;
        let value = state.deque.pop_front();
        if value.is_some() {
            if state.closed {
                if !state.is_recvable() {
                    state.close_receivers();
                }
            } else if full {
                state.wake_sender();
            }
        }
        value
    }

    fn watch_recv_permit(&self, selector: Selector) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(permit) = state.reserve_recv_permit() {
            if !selector.apply(permit.into()) && permit == Permit::Recv {
                state.recv_permits -= 1;
            }
            return true;
        }
        state.receivers.push_back(RecvWaiter::from(selector));
        false
    }

    fn unwatch_recv_permit(&self, identifier: &Identifier) {
        let mut state = self.state.lock().unwrap();
        if let Some(position) = state.receivers.iter().position(|waker| waker.matches(identifier)) {
            state.receivers.remove(position);
        }
    }

    fn send(&self, trying: bool, value: T) -> Result<(), TrySendError<T>> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(TrySendError::Closed(value));
        } else if !state.is_full() {
            state.deque.push_back(value);
            state.wake_receiver();
            return Ok(());
        } else if trying {
            return Err(TrySendError::Full(value));
        } else if task::task().is_some() {
            let (session, waker) = task::session::<Result<(), SendError<T>>>();
            let waiter = Waiter::Task { waker, value };
            state.senders.push_back(waiter);
            drop(state);
            return Ok(session.wait()?);
        }
        let waker = ThreadWaker::new();
        state.senders.push_back(Waiter::Thread { waker: waker.clone(), value });
        loop {
            state = waker.condvar.wait(state).unwrap();
            let result = unsafe { &mut *waker.result.get() };
            if let Some(result) = result.take() {
                result?;
                return Ok(());
            }
        }
    }
}

/// Sending peer of [Receiver]. Additional senders could be constructed by [Sender::clone].
pub struct Sender<T: Send + 'static> {
    channel: Option<Arc<Channel<T>>>,
}

impl<T: Send + 'static> channel::Sender<T> for Sender<T> {
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

impl<T: Send + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match &self.channel {
            None => Sender { channel: None },
            Some(channel) => {
                channel.add_sender();
                Sender { channel: Some(channel.clone()) }
            },
        }
    }
}

impl<T: Send + 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T: Send + 'static> Selectable for Sender<T> {
    fn parallel(&self) -> bool {
        self.channel.is_some()
    }

    fn select_permit(&self) -> Option<select::Permit> {
        if let Some(channel) = &self.channel {
            channel.reserve_send_permit().map(From::from)
        } else {
            Some(Permit::Closed.into())
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        if let Some(channel) = &self.channel {
            channel.watch_send_permit(selector)
        } else {
            selector.apply(Permit::Closed.into());
            true
        }
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        if let Some(channel) = &self.channel {
            channel.unwatch_send_permit(identifier)
        }
    }
}

impl<T: Send + 'static> PermitWriter for Sender<T> {
    type Item = T;
    type Result = Result<(), SendError<T>>;

    fn consume_permit(&mut self, permit: select::Permit, value: Self::Item) -> Self::Result {
        let permit = Permit::from(permit);
        assert!(permit != Permit::Recv);
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

/// Receiving peer of [Sender].
pub struct Receiver<T: Send + 'static> {
    channel: Option<Arc<Channel<T>>>,
}

impl<T: Send + 'static> channel::Receiver<T> for Receiver<T> {
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

    fn is_drained(&self) -> bool {
        self.channel.is_none()
    }
}

impl<T: Send + 'static> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        match self.channel {
            None => Receiver { channel: None },
            Some(ref channel) => {
                channel.add_receiver();
                Receiver { channel: Some(channel.clone()) }
            },
        }
    }
}

impl<T: Send + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.take() {
            channel.remove_receiver();
        }
    }
}

impl<T: Send + 'static> Selectable for Receiver<T> {
    fn parallel(&self) -> bool {
        self.channel.is_some()
    }

    fn select_permit(&self) -> Option<select::Permit> {
        if let Some(channel) = &self.channel {
            channel.reserve_recv_permit().map(From::from)
        } else {
            Some(Permit::Closed.into())
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        if let Some(channel) = &self.channel {
            channel.watch_recv_permit(selector)
        } else {
            selector.apply(Permit::Closed.into());
            true
        }
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        if let Some(channel) = &self.channel {
            channel.unwatch_recv_permit(identifier)
        }
    }
}

impl<T: Send + 'static> PermitReader for Receiver<T> {
    type Result = Option<T>;

    fn consume_permit(&mut self, permit: select::Permit) -> Self::Result {
        let permit = Permit::from(permit);
        assert!(permit != Permit::Send);
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

#[derive(Debug, Clone, Copy)]
struct Capacity {
    min: usize,
    max: usize,
}

impl Capacity {
    pub fn bounded(capacity: usize) -> Capacity {
        assert!(capacity > 0);
        Capacity { min: capacity, max: capacity }
    }

    pub fn unbounded(initial_capacity: usize) -> Capacity {
        Capacity { min: initial_capacity, max: usize::MAX }
    }
}

fn channel<T: Send + 'static>(capacity: Capacity) -> (Sender<T>, Receiver<T>) {
    let channel = Channel::new(capacity);
    let sender = Sender { channel: Some(channel.clone()) };
    let receiver = Receiver { channel: Some(channel) };
    (sender, receiver)
}

/// Constructs a bounded FIFO channel.
pub fn bounded<T: Send + 'static>(bound: usize) -> (Sender<T>, Receiver<T>) {
    channel(Capacity::bounded(bound))
}

/// Constructs an unbounded FIFO channel.
pub fn unbounded<T: Send + 'static>(initial_capacity: usize) -> (Sender<T>, Receiver<T>) {
    channel(Capacity::unbounded(initial_capacity))
}

impl<T: Send + 'static> IntoIterator for Receiver<T> {
    type IntoIter = IntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

/// An iterator that owns its source receiver.
pub struct IntoIter<T: Send + 'static> {
    receiver: Receiver<T>,
}

impl<T: Send + 'static> std::iter::Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv()
    }
}

impl<T: Send + 'static> std::iter::FusedIterator for IntoIter<T> {}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use more_asserts::{assert_ge, assert_le};

    use super::*;
    use crate::runtime::Builder;

    #[test]
    #[should_panic]
    fn bounded_zero() {
        bounded::<()>(0);
    }

    fn channel_send(mut sender: Sender<i32>, mut receiver: Receiver<i32>) {
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        assert_eq!(1, receiver.recv().unwrap());
        assert_eq!(2, receiver.recv().unwrap());
        drop(receiver);

        assert_eq!(sender.send(3).unwrap_err(), SendError::Closed(3));
        assert_eq!(sender.try_send(6).unwrap_err(), TrySendError::Closed(6));
    }

    #[test]
    fn bounded_send() {
        let (sender, receiver) = bounded::<i32>(2);
        channel_send(sender, receiver);
    }

    #[test]
    fn unbounded_send() {
        let (sender, receiver) = unbounded::<i32>(2);
        channel_send(sender, receiver);
    }

    #[test]
    fn bounded_try_send_full() {
        let (mut sender, mut receiver) = bounded::<i32>(2);
        sender.try_send(1).unwrap();
        sender.try_send(2).unwrap();
        assert_eq!(sender.try_send(3).unwrap_err(), TrySendError::Full(3));
        drop(sender);
        assert_eq!(1, receiver.recv().unwrap());
        assert_eq!(2, receiver.recv().unwrap());
        assert_eq!(None, receiver.recv());
    }

    #[test]
    fn unbounded_try_send() {
        let (mut sender, mut receiver) = unbounded::<i32>(1);
        sender.try_send(1).unwrap();
        sender.try_send(2).unwrap();
        sender.try_send(3).unwrap();
        drop(sender);
        assert_eq!(1, receiver.recv().unwrap());
        assert_eq!(2, receiver.recv().unwrap());
        assert_eq!(3, receiver.recv().unwrap());
        assert_eq!(None, receiver.recv());
    }

    #[test]
    fn bounded_blocking() {
        let mut runtime = Builder::default().parallelism(1).build();
        let (mut ready_sender, mut ready_receiver) = bounded::<()>(1);
        let (mut sender, mut receiver) = bounded::<i32>(5);
        let sending = runtime.spawn(move || {
            ready_sender.send(()).unwrap();
            let now = Instant::now();
            sender.send(1).unwrap();
            sender.send(2).unwrap();
            sender.send(3).unwrap();
            sender.send(4).unwrap();
            sender.send(5).unwrap();
            assert_le!(now.elapsed(), Duration::from_secs(5));
            sender.send(6).unwrap();
            assert_ge!(now.elapsed(), Duration::from_secs(5));
        });
        ready_receiver.recv().unwrap();
        std::thread::sleep(Duration::from_secs(10));
        assert_eq!(1, receiver.recv().unwrap());
        assert_eq!(2, receiver.recv().unwrap());
        assert_eq!(3, receiver.recv().unwrap());
        assert_eq!(4, receiver.recv().unwrap());
        assert_eq!(5, receiver.recv().unwrap());
        assert_eq!(6, receiver.recv().unwrap());
        assert_eq!(None, receiver.recv());
        sending.join().unwrap();
    }

    #[test]
    fn unbounded_nonblocking() {
        let mut runtime = Builder::default().parallelism(1).build();
        let (mut ready_sender, mut ready_receiver) = bounded::<()>(1);
        let (mut sender, mut receiver) = unbounded::<i32>(0);
        let sending = runtime.spawn(move || {
            ready_sender.send(()).unwrap();
            let now = Instant::now();
            sender.send(1).unwrap();
            sender.send(2).unwrap();
            sender.send(3).unwrap();
            sender.send(4).unwrap();
            sender.send(5).unwrap();
            sender.send(6).unwrap();
            assert_le!(now.elapsed(), Duration::from_secs(5));
        });
        ready_receiver.recv().unwrap();
        std::thread::sleep(Duration::from_secs(10));
        assert_eq!(1, receiver.recv().unwrap());
        assert_eq!(2, receiver.recv().unwrap());
        assert_eq!(3, receiver.recv().unwrap());
        assert_eq!(4, receiver.recv().unwrap());
        assert_eq!(5, receiver.recv().unwrap());
        assert_eq!(6, receiver.recv().unwrap());
        assert_eq!(None, receiver.recv());
        sending.join().unwrap();
    }

    #[crate::test(crate = "crate")]
    fn receiver_into_iter() {
        let (mut sender, receiver) = bounded(3);
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();
        drop(sender);

        let mut iter = receiver.into_iter();
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), Some(3));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None);
    }
}
