//! Cooperative coroutines in task.

mod context;
mod page_size;
pub(crate) mod stack;
mod suspension;

use std::cell::{Cell, UnsafeCell};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;

use self::context::{Context, Entry};
use self::stack::StackSize;
pub use self::suspension::{suspension, JoinHandle, Resumption, Suspension};
use crate::task;

thread_local! {
    static COROUTINE: Cell<Option<ptr::NonNull<Coroutine>>> = const {  Cell::new(None) };
    static THREAD_CONTEXT: UnsafeCell<Context> = UnsafeCell::new(Context::empty());
}

pub(crate) fn try_current() -> Option<ptr::NonNull<Coroutine>> {
    COROUTINE.with(|p| p.get())
}

pub(crate) fn current() -> ptr::NonNull<Coroutine> {
    COROUTINE.with(|p| p.get()).expect("no running coroutine")
}

struct Scope {
    co: ptr::NonNull<Coroutine>,
}

impl Scope {
    fn enter(co: &Coroutine) -> Scope {
        COROUTINE.with(|cell| {
            assert!(cell.get().is_none(), "running coroutine not exited");
            cell.set(Some(ptr::NonNull::from(co)));
        });
        Scope { co: ptr::NonNull::from(co) }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        COROUTINE.with(|cell| {
            let co = cell.replace(None).expect("no running coroutine");
            assert!(co == self.co, "running coroutine changed");
        })
    }
}

struct ThisThread;

impl ThisThread {
    fn context<'a>() -> &'a Context {
        THREAD_CONTEXT.with(|c| unsafe { &*c.get() })
    }

    fn context_mut<'a>() -> &'a mut Context {
        THREAD_CONTEXT.with(|c| unsafe { &mut *c.get() })
    }

    fn resume(context: &Context) {
        context.switch(Self::context_mut()).unwrap();
    }

    fn suspend(context: &mut Context) {
        Self::context().switch(context).unwrap();
    }

    fn restore() {
        Self::context().resume().unwrap();
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub(crate) enum Status {
    Running,
    Aborting,
    Cancelling,
    Completed,
}

impl Status {
    pub fn into_abort(self) -> Self {
        match self {
            Self::Running | Self::Aborting => Self::Aborting,
            _ => self,
        }
    }
}

pub(crate) struct Coroutine {
    pub status: Status,
    context: Option<Box<Context>>,
    f: Option<Box<dyn FnOnce()>>,
}

unsafe impl Sync for Coroutine {}

impl Coroutine {
    pub fn new(f: Box<dyn FnOnce()>, stack_size: StackSize) -> Box<Coroutine> {
        let mut co = Box::new(Coroutine { f: Option::Some(f), status: Status::Running, context: None });
        let ptr = co.as_mut() as *mut Coroutine as usize;
        #[cfg(target_pointer_width = "64")]
        let (low, high) = (ptr as u32, (ptr >> 32) as u32);
        #[cfg(target_pointer_width = "32")]
        let (low, high) = (ptr as u32, 0);
        let entry = Entry { f: Self::main, arg1: low, arg2: high, stack_size };
        co.context = Some(Context::new(&entry, None));
        co
    }

    extern "C" fn main(low: u32, _high: u32) {
        #[cfg(target_pointer_width = "64")]
        let ptr = ((_high as usize) << 32) | low as usize;
        #[cfg(target_pointer_width = "32")]
        let ptr = low as usize;
        let co = unsafe { &mut *(ptr as *const Coroutine as *mut Coroutine) };
        co.run();
        co.status = Status::Completed;
        ThisThread::restore();
    }

    fn run(&mut self) {
        let f = self.f.take().expect("no entry function");
        f();
    }

    /// Resumes coroutine.
    ///
    /// Returns whether this coroutine should be resumed again.
    pub fn resume(&mut self) -> Status {
        let _scope = Scope::enter(self);
        ThisThread::resume(self.context.as_ref().unwrap());
        self.status
    }

    pub fn suspend(&mut self) {
        ThisThread::suspend(self.context.as_mut().unwrap());
    }

    pub fn is_cancelling(&self) -> bool {
        self.status == Status::Cancelling
    }
}

/// Spawns a cooperative task and returns a [JoinHandle] for it.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: 'static,
    T: 'static,
{
    let mut task = task::current();
    let (suspension, resumption) = suspension();
    let handle = JoinHandle::new(suspension);
    let task = unsafe { task.as_mut() };
    task.spawn(
        move || {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            resumption.set_result(result);
        },
        StackSize::default(),
    );
    handle
}

/// Yields coroutine for next scheduling cycle.
pub fn yield_now() {
    let t = unsafe { task::current().as_mut() };
    let co = current();
    t.yield_coroutine(co);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use pretty_assertions::assert_eq;

    use crate::{coroutine, task};

    #[crate::test(crate = "crate")]
    fn yield_now() {
        let five = task::spawn(|| {
            let value = Rc::new(Cell::new(0));
            let shared_value = value.clone();
            coroutine::spawn(move || {
                shared_value.as_ref().set(5);
            });
            coroutine::yield_now();
            value.as_ref().get()
        });
        assert_eq!(5, five.join().unwrap());
    }
}
