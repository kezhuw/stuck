//! Channel utilities for commnication across coroutines in one task.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use static_assertions::assert_not_impl_any;

use super::shared::Permit;
use crate::channel::prelude::*;
use crate::channel::{self, SendError, TryRecvError, TrySendError};
use crate::coroutine::{self, Resumption};
use crate::select::{self, Identifier, PermitReader, PermitWriter, Selectable, Selector, TrySelectError};

enum Waker {
    Selector(Selector),
    Resumption(Resumption<()>),
}

impl From<Resumption<()>> for Waker {
    fn from(resumption: Resumption<()>) -> Self {
        Waker::Resumption(resumption)
    }
}

impl From<Selector> for Waker {
    fn from(selector: Selector) -> Self {
        Waker::Selector(selector)
    }
}

impl Waker {
    fn wake(self) -> bool {
        match self {
            Waker::Selector(selector) => selector.apply(Permit::Consume.into()),
            Waker::Resumption(resumption) => {
                resumption.resume(());
                true
            },
        }
    }

    fn matches(&self, identifier: &Identifier) -> bool {
        if let Waker::Selector(selector) = self {
            selector.identify(identifier)
        } else {
            false
        }
    }
}

struct State<T: 'static> {
    closed: bool,

    bound: usize,
    deque: VecDeque<T>,
    senders: VecDeque<Waker>,
    receivers: VecDeque<Waker>,
}

impl<T: 'static> State<T> {
    fn new(cap: usize, bound: usize) -> Self {
        State {
            closed: false,
            bound,
            deque: VecDeque::with_capacity(cap),
            senders: VecDeque::with_capacity(16),
            receivers: VecDeque::with_capacity(5),
        }
    }

    fn is_full(&self) -> bool {
        self.bound == self.deque.len()
    }

    fn is_sendable(&self) -> bool {
        self.closed || !self.is_full()
    }

    fn is_recvable(&self) -> bool {
        !self.deque.is_empty() || self.closed
    }

    fn wake_sender(&mut self) {
        while let Some(waker) = self.senders.pop_front() {
            if waker.wake() {
                break;
            }
        }
    }

    fn wake_receiver(&mut self) {
        while let Some(receiver) = self.receivers.pop_front() {
            if receiver.wake() {
                break;
            }
        }
    }

    fn close(&mut self) {
        self.closed = true;
        while let Some(waker) = self.senders.pop_front() {
            waker.wake();
        }
        while let Some(receiver) = self.receivers.pop_front() {
            receiver.wake();
        }
    }
}

impl<T: 'static> Drop for State<T> {
    fn drop(&mut self) {
        self.close();
    }
}

struct Channel<T: 'static> {
    state: RefCell<State<T>>,
    senders: Cell<usize>,
    receivers: Cell<usize>,
}

impl<T: 'static> Channel<T> {
    fn new(cap: usize, bound: usize) -> Rc<Channel<T>> {
        let state = State::new(cap, bound);
        Rc::new(Channel { state: RefCell::new(state), senders: Cell::new(1), receivers: Cell::new(1) })
    }

    fn add_sender(&self) {
        let senders = self.senders.get() + 1;
        self.senders.set(senders);
    }

    fn remove_sender(&self) {
        let senders = self.senders.get() - 1;
        self.senders.set(senders);
        if senders == 0 {
            let mut state = self.state.borrow_mut();
            state.close();
        }
    }

    fn add_receiver(&self) {
        let receivers = self.receivers.get() + 1;
        self.receivers.set(receivers);
    }

    fn remove_receiver(&self) {
        let receivers = self.receivers.get() - 1;
        self.receivers.set(receivers);
        if receivers == 0 {
            let mut state = self.state.borrow_mut();
            state.close();
        }
    }

    fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.close();
    }

    fn send(&self, trying: bool, value: T) -> Result<(), TrySendError<T>> {
        loop {
            let mut state = self.state.borrow_mut();
            if state.closed {
                return Err(TrySendError::Closed(value));
            } else if !state.is_full() {
                state.deque.push_back(value);
                state.wake_receiver();
                return Ok(());
            } else if trying {
                return Err(TrySendError::Full(value));
            } else {
                let (suspension, resumption) = coroutine::suspension();
                state.senders.push_back(Waker::from(resumption));
                drop(state);
                suspension.suspend();
            }
        }
    }

    fn is_sendable(&self) -> bool {
        let state = self.state.borrow();
        state.is_sendable()
    }

    fn is_recvable(&self) -> bool {
        let state = self.state.borrow();
        state.is_recvable()
    }

    fn watch_send_permit(&self, watcher: Selector) -> bool {
        assert!(!self.is_sendable(), "wait on sendable channel");
        let mut state = self.state.borrow_mut();
        state.senders.push_back(Waker::from(watcher));
        true
    }

    fn watch_recv_permit(&self, selector: Selector) -> bool {
        assert!(!self.is_recvable(), "wait on recvable channel");
        let mut state = self.state.borrow_mut();
        state.receivers.push_back(Waker::from(selector));
        true
    }

    fn unwatch_send_permit(&self, identifier: &Identifier) {
        let mut state = self.state.borrow_mut();
        if let Some(position) = state.senders.iter().position(|w| w.matches(identifier)) {
            state.senders.remove(position);
        }
    }

    fn unwatch_recv_permit(&self, identifier: &Identifier) {
        let mut state = self.state.borrow_mut();
        if let Some(position) = state.receivers.iter().position(|w| w.matches(identifier)) {
            state.receivers.remove(position);
        }
    }

    fn recv(&self, trying: bool) -> Result<T, TryRecvError> {
        loop {
            let mut state = self.state.borrow_mut();
            if let Some(value) = state.deque.pop_front() {
                state.wake_sender();
                return Ok(value);
            } else if state.closed {
                return Err(TryRecvError::Closed);
            } else if trying {
                return Err(TryRecvError::Empty);
            }
            let (suspension, resumption) = coroutine::suspension();
            state.receivers.push_back(Waker::from(resumption));
            drop(state);
            suspension.suspend();
        }
    }
}

impl<T> super::shared::Channel<T> for Rc<Channel<T>> {
    fn send(&self, trying: bool, value: T) -> Result<(), TrySendError<T>> {
        Channel::send(self, trying, value)
    }

    fn add_sender(&self) {
        Channel::add_sender(self)
    }

    fn remove_sender(&self) {
        Channel::remove_sender(self)
    }

    fn select_send_permit(&self) -> Option<Permit> {
        if self.is_sendable() {
            Some(Permit::Consume)
        } else {
            None
        }
    }

    fn consume_send_permit(&self, value: T) -> Result<(), SendError<T>> {
        match self.send(true, value) {
            Ok(()) => Ok(()),
            Err(TrySendError::Closed(value)) => Err(SendError::Closed(value)),
            Err(TrySendError::Full(_)) => panic!("not ready to send"),
        }
    }

    fn watch_send_permit(&self, selector: Selector) -> bool {
        Channel::watch_send_permit(self, selector)
    }

    fn unwatch_send_permit(&self, identifier: &Identifier) {
        Channel::unwatch_send_permit(self, identifier)
    }

    fn recv(&self, trying: bool) -> Result<T, TryRecvError> {
        Channel::recv(self, trying)
    }

    fn add_receiver(&self) {
        Channel::add_receiver(self)
    }

    fn remove_receiver(&self) {
        Channel::remove_receiver(self)
    }

    fn select_recv_permit(&self) -> Option<Permit> {
        if self.is_recvable() {
            Some(Permit::Consume)
        } else {
            None
        }
    }

    fn consume_recv_permit(&self) -> Option<T> {
        match self.recv(true) {
            Ok(value) => Some(value),
            Err(TryRecvError::Empty) => panic!("not ready to recv"),
            Err(TryRecvError::Closed) => None,
        }
    }

    fn watch_recv_permit(&self, selector: Selector) -> bool {
        Channel::watch_recv_permit(self, selector)
    }

    fn unwatch_recv_permit(&self, identifier: &Identifier) {
        Channel::unwatch_recv_permit(self, identifier)
    }

    fn close(&self) {
        Channel::close(self)
    }
}

/// Sending peer of [Receiver]. Additional senders could be constructed by [Sender::clone].
pub struct Sender<T: 'static>(super::shared::Sender<T, Rc<Channel<T>>>);

impl<T: 'static> channel::Sender<T> for Sender<T> {
    fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        self.0.send(value)
    }

    fn try_send(&mut self, value: T) -> Result<(), TrySendError<T>> {
        self.0.try_send(value)
    }

    fn close(&mut self) {
        self.0.close()
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

impl<T: 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender(self.0.clone())
    }
}

impl<T: 'static> Selectable for Sender<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<select::Permit, TrySelectError> {
        self.0.select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        self.0.watch_permit(selector)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        self.0.unwatch_permit(identifier)
    }
}

impl<T: 'static> PermitWriter for Sender<T> {
    type Item = T;
    type Result = Result<(), SendError<T>>;

    fn consume_permit(&mut self, permit: select::Permit, value: Self::Item) -> Self::Result {
        self.0.consume_permit(permit, value)
    }
}

/// Receiving peer of [Sender].
pub struct Receiver<T: 'static>(super::shared::Receiver<T, Rc<Channel<T>>>);

impl<T: 'static> channel::Receiver<T> for Receiver<T> {
    fn recv(&mut self) -> Option<T> {
        self.0.recv()
    }

    fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }

    fn close(&mut self) {
        self.0.close()
    }

    fn terminate(&mut self) {
        self.0.terminate()
    }

    fn is_drained(&self) -> bool {
        self.0.is_drained()
    }
}

impl<T: 'static> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Receiver(self.0.clone())
    }
}

impl<T: 'static> Selectable for Receiver<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<select::Permit, TrySelectError> {
        self.0.select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        self.0.watch_permit(selector)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        self.0.unwatch_permit(identifier)
    }
}

impl<T: 'static> PermitReader for Receiver<T> {
    type Result = Option<T>;

    fn consume_permit(&mut self, permit: select::Permit) -> Self::Result {
        self.0.consume_permit(permit)
    }
}

assert_not_impl_any!(Sender<()>: Send, Sync);
assert_not_impl_any!(Receiver<()>: Send, Sync);

impl<T: 'static> IntoIterator for Receiver<T> {
    type IntoIter = IntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

/// An iterator that owns its source receiver.
pub struct IntoIter<T: 'static> {
    receiver: Receiver<T>,
}

impl<T: 'static> std::iter::Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv()
    }
}

impl<T: 'static> std::iter::FusedIterator for IntoIter<T> {}

fn channel<T: 'static>(capacity: usize, bound: usize) -> (Sender<T>, Receiver<T>) {
    let channel = Channel::new(capacity, bound);
    let sender = Sender(super::shared::Sender::new(channel.clone()));
    let receiver = Receiver(super::shared::Receiver::new(channel));
    (sender, receiver)
}

/// Constructs a pair of completed sender and receiver.
pub fn completed<T: 'static>() -> (Sender<T>, Receiver<T>) {
    let sender = Sender(super::shared::Sender::empty());
    let receiver = Receiver(super::shared::Receiver::empty());
    (sender, receiver)
}

/// Constructs a bounded channel.
pub fn bounded<T: 'static>(bound: usize) -> (Sender<T>, Receiver<T>) {
    assert!(bound > 0, "bound must be greater than 0");
    channel(bound, bound)
}

/// Constructs a unbounded channel.
pub fn unbounded<T: 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    channel(capacity, usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ignore_result::Ignore;
    use more_asserts::{assert_ge, assert_le};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::channel::serial;
    use crate::{select, time};

    #[crate::test(crate = "crate")]
    fn completed() {
        let (mut sender, mut receiver) = serial::completed();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
        assert_eq!(receiver.recv(), None);

        assert_eq!(sender.send(()), Err(SendError::Closed(())));
        assert_eq!(sender.try_send(()), Err(TrySendError::Closed(())));

        assert!(sender.is_closed());
        assert!(receiver.is_drained());
        select! {
            _ = <-receiver => unreachable!("completed"),
            complete => {},
        }
        select! {
            _ = sender<-() => unreachable!("completed"),
            complete => {},
        }
    }

    #[crate::test(crate = "crate")]
    fn receiver_into_iter() {
        let (mut sender, receiver) = serial::bounded(3);
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

    #[test]
    #[should_panic]
    fn bounded_zero() {
        bounded::<()>(0);
    }

    fn series_send(mut sender: Sender<i32>, mut receiver: Receiver<i32>) {
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
        series_send(sender, receiver);
    }

    #[test]
    fn unbounded_send() {
        let (sender, receiver) = unbounded::<i32>(2);
        series_send(sender, receiver);
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

    #[crate::test(crate = "crate")]
    fn bounded_blocking() {
        let (mut ready_sender, mut ready_receiver) = bounded::<()>(1);
        let (mut sender, mut receiver) = bounded::<i32>(5);
        let sending = coroutine::spawn(move || {
            let now = Instant::now();
            sender.send(1).unwrap();
            sender.send(2).unwrap();
            sender.send(3).unwrap();
            sender.send(4).unwrap();
            sender.send(5).unwrap();
            assert_le!(now.elapsed(), Duration::from_secs(5));
            let now = Instant::now();
            ready_sender.send(()).unwrap();
            sender.send(6).unwrap();
            assert_ge!(now.elapsed(), Duration::from_secs(5));
        });
        ready_receiver.recv().unwrap();
        time::sleep(Duration::from_secs(6));
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
    fn unbounded_nonblocking() {
        let (mut ready_sender, mut ready_receiver) = bounded::<()>(1);
        let (mut sender, mut receiver) = unbounded::<i32>(0);
        let sending = coroutine::spawn(move || {
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
        time::sleep(Duration::from_secs(6));
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
    fn send_close() {
        let (mut sender, _receiver) = bounded(3);
        sender.send(1).unwrap();

        sender.close();

        select! {
            _ = sender<-2 => panic!("completed"),
            complete => {},
        }

        assert_eq!(sender.send(3), Err(SendError::Closed(3)));
    }

    #[crate::test(crate = "crate")]
    fn receiver_close() {
        let (mut sender, mut receiver) = bounded(3);
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();

        receiver.close();
        assert_eq!(receiver.recv(), Some(1));
        select! {
            r = <-receiver => assert_eq!(r, Some(2)),
            complete => panic!("not completed"),
        }
        assert_eq!(receiver.recv(), Some(3));
        select! {
            r = <-receiver => assert_eq!(r, None),
            complete => panic!("not completed"),
        }
        select! {
            _ = <-receiver => panic!("completed"),
            complete => {},
        }
        assert_eq!(receiver.recv(), None);
    }

    #[crate::test(crate = "crate")]
    fn receiver_terminate() {
        let (mut sender, mut receiver) = bounded(3);
        sender.send(1).unwrap();
        sender.send(2).unwrap();

        receiver.terminate();
        select! {
            _ = <-receiver => panic!("terminated"),
            complete => {},
        }
        assert_eq!(receiver.recv(), None);
    }

    #[crate::test(crate = "crate")]
    fn sender_select() {
        let (mut sender1, receiver1) = bounded(1);
        let (mut sender2, receiver2) = unbounded(1);

        let task1 = coroutine::spawn(move || receiver1.into_iter().collect::<Vec<_>>());

        let task2 = coroutine::spawn(move || receiver2.into_iter().collect::<Vec<_>>());

        let mut values1 = VecDeque::from(vec![1, 3, 5]);
        let mut values2 = VecDeque::from(vec![2, 4, 6]);

        loop {
            select! {
                _ = sender1<-values1.pop_front().unwrap() => if values1.is_empty() {
                    sender1.close();
                },
                _ = sender2<-values2.pop_front().unwrap() => if values2.is_empty() {
                    sender2.close();
                },
                complete => break,
            }
        }

        assert_eq!(task1.join().unwrap(), vec![1, 3, 5]);
        assert_eq!(task2.join().unwrap(), vec![2, 4, 6]);
    }

    #[crate::test(crate = "crate")]
    fn receiver_select() {
        let (mut sender1, mut receiver1) = bounded(10);
        let (mut sender2, mut receiver2) = unbounded(10);

        coroutine::spawn(move || {
            for v in vec![1, 3, 5] {
                sender1.send(v).ignore();
            }
        });

        coroutine::spawn(move || {
            for v in vec![2, 4, 6] {
                sender2.send(v).ignore();
            }
        });

        let mut values1 = Vec::new();
        let mut values2 = Vec::new();

        loop {
            select! {
                r = <-receiver1 => if let Some(v) = r {
                    values1.push(v);
                },
                r = <-receiver2 => if let Some(v) = r {
                    values2.push(v);
                },
                complete => break,
            }
        }

        assert_eq!(values1, vec![1, 3, 5]);
        assert_eq!(values2, vec![2, 4, 6]);
    }
}
