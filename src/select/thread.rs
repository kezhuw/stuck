use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use static_assertions::{assert_impl_all, assert_not_impl_any};

use crate::error::PanicError;

#[derive(Default)]
enum Value<T> {
    #[default]
    Empty,
    Ready(T),
    Panic(PanicError),
    Joined,
    Cancelled,
}

struct State<T> {
    unparkers: usize,
    value: Value<T>,
}

pub struct Parker<T> {
    state: Arc<Mutex<State<T>>>,
    _marker: PhantomData<Rc<()>>,
}

pub struct Unparker<T> {
    state: Arc<Mutex<State<T>>>,
    thread: std::thread::Thread,
}

impl<T> Parker<T> {
    pub fn park(self) -> T {
        let state = unsafe { ptr::read(&self.state) };
        std::mem::forget(self);
        loop {
            let mut state = state.lock().unwrap();
            let value = std::mem::replace(&mut state.value, Value::Joined);
            match value {
                Value::Empty => {
                    state.value = Value::Empty;
                    drop(state);
                    std::thread::park();
                    continue;
                },
                Value::Joined => unreachable!("parker joined"),
                Value::Cancelled => unreachable!("parker cancelled"),
                Value::Ready(value) => return value,
                Value::Panic(panic) => panic.resume(),
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        matches!(state.value, Value::Ready(_) | Value::Panic(_))
    }
}

impl<T> Drop for Parker<T> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if matches!(state.value, Value::Empty) {
            state.value = Value::Cancelled;
        }
    }
}

impl<T> Unparker<T> {
    pub fn unpark(self, value: T) -> bool {
        let state = unsafe { ptr::read(&self.state) };
        let thread = unsafe { ptr::read(&self.thread) };
        std::mem::forget(self);
        let mut locked_state = state.lock().unwrap();
        match &locked_state.value {
            Value::Empty => {
                locked_state.value = Value::Ready(value);
                thread.unpark();
                true
            },
            _ => false,
        }
    }
}

impl<T> Drop for Unparker<T> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.unparkers -= 1;
        if state.unparkers == 0 && matches!(state.value, Value::Empty) {
            state.value = Value::Panic(PanicError::Static("all unparkers dropped"));
            self.thread.unpark();
        }
    }
}

impl<T> Clone for Unparker<T> {
    fn clone(&self) -> Self {
        let state = self.state.clone();
        state.lock().unwrap().unparkers += 1;
        Self { thread: self.thread.clone(), state }
    }
}

assert_not_impl_any!(Parker<()>: Send);
assert_impl_all!(Unparker<()>: Send, Sync, Clone);

pub fn parker<T>() -> (Parker<T>, Unparker<T>) {
    let state = Arc::new(Mutex::new(State { unparkers: 1, value: Value::Empty }));
    let parker = Parker { state: state.clone(), _marker: PhantomData };
    let unparker = Unparker { thread: std::thread::current(), state };
    (parker, unparker)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;

    use super::parker;

    #[test]
    fn parker_cancelled() {
        let (parker, unparker) = parker::<()>();
        drop(parker);
        assert_eq!(unparker.unpark(()), false);
    }

    #[test]
    fn parker_readiness() {
        let (parker, unparker) = parker::<()>();
        assert_eq!(parker.is_ready(), false);
        unparker.unpark(());
        assert_eq!(parker.is_ready(), true);
        parker.park();
    }

    #[test]
    #[should_panic(expected = "all unparkers dropped")]
    fn unparker_dropped() {
        let (parker, unparker) = parker::<()>();
        std::thread::spawn({
            let unparker = unparker.clone();
            move || {
                drop(unparker);
            }
        });
        std::thread::spawn(move || {
            drop(unparker);
        });
        parker.park();
    }

    #[test]
    fn unpark_early() {
        let (parker, unparker) = parker();
        std::thread::spawn(move || {
            unparker.unpark(());
        });
        std::thread::sleep(Duration::from_millis(20));
        parker.park();
    }

    #[test]
    fn unpark_later() {
        let (parker, unparker) = parker();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            unparker.unpark(());
        });
        parker.park();
    }

    #[test]
    fn unpark_concurrent() {
        let (parker, unparker) = parker();
        let thread1 = std::thread::spawn({
            let unparker = unparker.clone();
            move || unparker.unpark(())
        });
        let thread2 = std::thread::spawn({
            let unparker = unparker.clone();
            move || unparker.unpark(())
        });
        parker.park();
        let unparked1 = thread1.join().unwrap();
        let unparked2 = thread2.join().unwrap();
        assert_that!(unparked1, not(eq(unparked2)));
    }

    #[test]
    fn unpark_concurrent_in_task() {
        use crate::runtime::Runtime;

        let mut runtime = Runtime::new();

        let (parker, unparker) = parker();
        let task1 = runtime.spawn({
            let unparker = unparker.clone();
            move || unparker.unpark(())
        });
        let task2 = runtime.spawn({
            let unparker = unparker.clone();
            move || unparker.unpark(())
        });
        parker.park();
        let unparked1 = task1.join().unwrap();
        let unparked2 = task2.join().unwrap();
        assert_that!(unparked1, not(eq(unparked2)));
    }
}
