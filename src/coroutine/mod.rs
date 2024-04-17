//! Cooperative coroutines in task.

mod context;
mod page_size;
pub(crate) mod stack;
mod suspension;

use std::cell::{Cell, UnsafeCell};
use std::panic::{self, AssertUnwindSafe};
use std::{mem, ptr};

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
        context.switch(Self::context_mut());
    }

    fn suspend(context: &mut Context) {
        Self::context().switch(context);
    }

    fn restore() {
        Self::context().resume();
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
    context: Box<Context>,
    f: Option<Box<dyn FnOnce()>>,
}

unsafe impl Sync for Coroutine {}

impl Coroutine {
    pub fn new(f: Box<dyn FnOnce()>, stack_size: StackSize) -> Box<Coroutine> {
        #[allow(invalid_value)]
        let mut co = Box::new(Coroutine {
            f: Option::Some(f),
            status: Status::Running,
            context: unsafe { mem::MaybeUninit::zeroed().assume_init() },
        });
        let entry = Entry { f: Self::main, arg: (co.as_mut() as *mut Coroutine) as *mut libc::c_void, stack_size };
        mem::forget(mem::replace(&mut co.context, Context::new(&entry, None)));
        co
    }

    extern "C" fn main(arg: *mut libc::c_void) {
        let co = unsafe { &mut *(arg as *mut Coroutine) };
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
        ThisThread::resume(&self.context);
        self.status
    }

    pub fn suspend(&mut self) {
        ThisThread::suspend(&mut self.context);
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
