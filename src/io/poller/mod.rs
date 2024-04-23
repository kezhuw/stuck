use std::cell::UnsafeCell;
use std::io::Result;
use std::thread;

use derive_where::derive_where;

use crate::channel::parallel;
use crate::channel::prelude::*;
use crate::task::{self, SessionWaker};

mod operation;
mod pool;

pub use operation::{OpCode, Operation};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod uring;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(target_os = "linux"))]
mod unix;
#[cfg(not(target_os = "linux"))]
pub(crate) use unix::*;

#[derive_where(Debug)]
pub(crate) struct Request {
    #[derive_where(skip)]
    pub result: SessionWaker<Result<i32>>,
    pub operation: Operation,
}

thread_local! {
    static REQUESTER: UnsafeCell<Option<Requester>> = const {  UnsafeCell::new(None) };
}

pub(crate) struct Scope {}

impl Scope {
    pub fn enter(requester: Requester) -> Self {
        REQUESTER.with(move |cell| {
            let value = unsafe { &mut *cell.get() };
            assert!(value.is_none(), "io requester existed");
            *value = Some(requester);
        });
        Scope {}
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        REQUESTER.with(|cell| {
            let value = unsafe { &mut *cell.get() };
            assert!(value.is_some(), "io requester does not exist");
            *value = None;
        });
    }
}

pub(crate) struct Stopper {
    closing: parallel::Sender<()>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Stopper {
    pub fn stop(&mut self) {
        self.closing.close();
        for thread in self.threads.drain(..) {
            thread.join().unwrap();
        }
    }
}

pub(crate) fn requester<'a>() -> &'a mut Requester {
    REQUESTER.with(|cell| {
        let value = unsafe { &mut *cell.get() };
        value.as_mut().expect("no thread local requester")
    })
}

pub(crate) fn request(operation: Operation) -> Result<i32> {
    let requester = requester();
    let opcode = OpCode::from(&operation);
    let (session, waker) = task::session();
    requester.send(Request { result: waker, operation });
    session.wait_uninterruptibly(|user_data| {
        requester.cancel(opcode, user_data);
    })
}
