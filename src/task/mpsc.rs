//! Multi-producer, single-consumer FIFO queue across tasks and threads.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::task::{self, SessionWaker};

enum Waiter<T: Send + 'static> {
    Task { waker: SessionWaker<Result<(), T>>, value: T },
    Thread { waker: Arc<ThreadWaker<T>>, value: T },
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
    sender_count: usize,
    bound: usize,
    deque: VecDeque<T>,
    senders: VecDeque<Waiter<T>>,
    receiver: Option<SessionWaker<()>>,
}

impl<T: Send + 'static> State<T> {
    fn is_full(&self) -> bool {
        self.deque.len() == self.bound
    }

    fn new(capacity: Capacity) -> Self {
        State {
            closed: false,
            sender_count: 1,
            bound: capacity.max,
            deque: VecDeque::with_capacity(capacity.min),
            senders: VecDeque::with_capacity(5),
            receiver: None,
        }
    }

    fn close(&mut self) {
        self.closed = true;
        for waiter in self.senders.drain(..) {
            match waiter {
                Waiter::Task { waker, value } => waker.wake(Err(value)),
                Waiter::Thread { waker, value } => {
                    unsafe { waker.wake(Err(value)) };
                },
            }
        }
        if let Some(receiver) = self.receiver.take() {
            receiver.wake(());
        }
    }
}

struct Channel<T: Send + 'static> {
    state: Mutex<State<T>>,
}

// SAFETY: There are multiple accessors.
unsafe impl<T: Send> Sync for Channel<T> {}

// SAFETY: Multiple acccessors are distributed across tasks and threads.
unsafe impl<T: Send> Send for Channel<T> {}

impl<T: Send + 'static> Channel<T> {
    fn new(capacity: Capacity) -> Arc<Self> {
        Arc::new(Channel { state: Mutex::new(State::new(capacity)) })
    }

    fn sender(self: &Arc<Channel<T>>) -> Sender<T> {
        let mut state = self.state.lock().unwrap();
        state.sender_count += 1;
        drop(state);
        Sender { channel: self.clone() }
    }

    fn remove_sender(&self) {
        let mut state = self.state.lock().unwrap();
        state.sender_count -= 1;
        if state.sender_count == 0 {
            state.close();
        }
    }

    fn recv(&self) -> Option<T> {
        loop {
            let mut state = self.state.lock().unwrap();
            if let Some(value) = state.deque.pop_front() {
                if let Some(sender) = state.senders.pop_front() {
                    match sender {
                        Waiter::Task { waker, value } => {
                            state.deque.push_back(value);
                            waker.wake(Ok(()));
                        },
                        Waiter::Thread { waker, value } => {
                            state.deque.push_back(value);
                            unsafe { waker.wake(Ok(())) };
                        },
                    }
                }
                return Some(value);
            } else if state.closed {
                return None;
            }
            let (session, waker) = task::session::<()>();
            state.receiver = Some(waker);
            drop(state);
            session.wait();
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.close();
    }

    fn wake_receiver(&self, mut state: MutexGuard<'_, State<T>>) {
        let receiver = state.receiver.take();
        drop(state);
        if let Some(receiver) = receiver {
            receiver.wake(());
        }
    }

    fn send(&self, value: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(value);
        } else if state.deque.is_empty() {
            state.deque.push_back(value);
            self.wake_receiver(state);
            return Ok(());
        } else if !state.is_full() {
            state.deque.push_back(value);
            return Ok(());
        } else if task::task().is_some() {
            let (session, waker) = task::session::<Result<(), T>>();
            let waiter = Waiter::Task { waker, value };
            state.senders.push_back(waiter);
            drop(state);
            return session.wait();
        }
        let waker = ThreadWaker::new();
        state.senders.push_back(Waiter::Thread { waker: waker.clone(), value });
        loop {
            state = waker.condvar.wait(state).unwrap();
            let result = unsafe { &mut *waker.result.get() };
            if let Some(result) = result.take() {
                return result;
            }
        }
    }
}

/// Sending peer of [Receiver]. Additional senders could be constructed by [Sender::clone].
pub struct Sender<T: Send + 'static> {
    channel: Arc<Channel<T>>,
}

impl<T: Send + 'static> Sender<T> {
    /// Sends a value to receiving peer.
    ///
    /// This operation could block if channel is full.
    pub fn send(&mut self, value: T) -> Result<(), T> {
        self.channel.send(value)
    }
}

impl<T: Send + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Channel::sender(&self.channel)
    }
}

impl<T: Send + 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        self.channel.remove_sender()
    }
}

/// Receiving peer of [Sender].
pub struct Receiver<T: Send + 'static> {
    channel: Arc<Channel<T>>,
    closed: bool,
}

impl<T: Send + 'static> Receiver<T> {
    /// Receives a value from [Sender]s.
    ///
    /// Returns [None] if channel is closed and has no buffered values. A channel is considered
    /// as closed if all senders have been dropped or [Receiver::close] has been called.
    pub fn recv(&mut self) -> Option<T> {
        self.channel.recv()
    }

    /// Constructs a sender for this channel.
    pub fn sender(&mut self) -> Sender<T> {
        Channel::sender(&self.channel)
    }

    /// Closes this channel for future sending.
    pub fn close(&mut self) {
        self.closed = true;
        self.channel.close()
    }
}

impl<T: Send + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        if !self.closed {
            self.channel.close()
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
    let sender = Sender { channel: channel.clone() };
    let receiver = Receiver { channel, closed: false };
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runtime::Builder;

    #[test]
    #[should_panic]
    fn bounded_zero() {
        bounded::<()>(0);
    }

    #[test]
    fn bounded_blocking() {
        let runtime = Builder::default().parallelism(1).build();
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
            assert!(now.elapsed() <= Duration::from_secs(5));
            sender.send(6).unwrap();
            assert!(now.elapsed() >= Duration::from_secs(5));
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
        let runtime = Builder::default().parallelism(1).build();
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
            assert!(now.elapsed() <= Duration::from_secs(5));
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
}
