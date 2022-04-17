mod context;
mod page_size;
pub(crate) mod stack;

use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::{mem, ptr};

use static_assertions::assert_not_impl_any;

use self::context::{Context, Entry};
use self::stack::StackSize;
use crate::error::{JoinError, PanicError};
use crate::task;

thread_local! {
    static COROUTINE: Cell<Option<ptr::NonNull<Coroutine>>> = Cell::new(None);
    static THREAD_CONTEXT: UnsafeCell<Context> = UnsafeCell::new(Context::empty());
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

pub(crate) struct Coroutine {
    context: Box<Context>,
    completed: bool,
    panicking: Option<&'static str>,
    f: Option<Box<dyn FnOnce()>>,
}

unsafe impl Sync for Coroutine {}

impl Coroutine {
    pub fn new(f: Box<dyn FnOnce()>, stack_size: StackSize) -> Box<Coroutine> {
        #[allow(invalid_value)]
        let mut co = Box::new(Coroutine {
            f: Option::Some(f),
            context: unsafe { mem::MaybeUninit::zeroed().assume_init() },
            completed: false,
            panicking: None,
        });
        let entry = Entry { f: Self::main, arg: (co.as_mut() as *mut Coroutine) as *mut libc::c_void, stack_size };
        mem::forget(mem::replace(&mut co.context, Context::new(&entry, None)));
        co
    }

    extern "C" fn main(arg: *mut libc::c_void) {
        let co = unsafe { &mut *(arg as *mut Coroutine) };
        co.run();
        co.completed = true;
        ThisThread::restore();
    }

    fn run(&mut self) {
        let f = self.f.take().expect("no entry function");
        f();
    }

    pub fn set_panic(&mut self, msg: &'static str) {
        self.panicking = Some(msg);
    }

    /// Resumes coroutine.
    ///
    /// Returns whether this coroutine should be resumed again.
    pub fn resume(&mut self) -> bool {
        let _scope = Scope::enter(self);
        ThisThread::resume(&self.context);
        !self.completed
    }

    pub fn suspend(&mut self) {
        ThisThread::suspend(&mut self.context);
        if let Some(msg) = self.panicking {
            panic::panic_any(msg);
        }
    }
}

enum SuspensionState<T: 'static> {
    Empty,
    Value(T),
    Panicked(Box<dyn Any + Send + 'static>),
    Joining(ptr::NonNull<Coroutine>),
}

struct SuspensionJoint<T: 'static> {
    state: UnsafeCell<SuspensionState<T>>,
}

impl<T> SuspensionJoint<T> {
    fn new() -> Rc<SuspensionJoint<T>> {
        Rc::new(SuspensionJoint { state: UnsafeCell::new(SuspensionState::Empty) })
    }

    fn resume(&self, state: SuspensionState<T>) {
        let state = mem::replace(unsafe { &mut *self.state.get() }, state);
        if let SuspensionState::Joining(co) = state {
            let task = unsafe { task::current().as_mut() };
            task.resume(co);
        }
    }

    fn abort(&self) {
        self.resume(SuspensionState::Panicked(Box::new("suspend: no resumption")))
    }

    fn wake(&self, value: T) {
        self.resume(SuspensionState::Value(value));
    }

    fn set_result(&self, result: Result<T, Box<dyn Any + Send + 'static>>) {
        let state = match result {
            Ok(value) => SuspensionState::Value(value),
            Err(err) => SuspensionState::Panicked(err),
        };
        self.resume(state);
    }

    fn take(&self) -> Result<T, PanicError> {
        let state = mem::replace(unsafe { &mut *self.state.get() }, SuspensionState::Empty);
        match state {
            SuspensionState::Value(value) => Ok(value),
            SuspensionState::Panicked(err) => Err(err),
            _ => unreachable!("suspend: unexpected branch"),
        }
    }

    fn join(&self) -> Result<T, PanicError> {
        let co = current();
        let state = mem::replace(unsafe { &mut *self.state.get() }, SuspensionState::Joining(co));
        match state {
            SuspensionState::Empty => {
                let task = unsafe { task::current().as_mut() };
                task.suspend(co);
                self.take()
            },
            SuspensionState::Value(value) => Ok(value),
            SuspensionState::Panicked(err) => Err(err),
            _ => unreachable!("suspend: unexpected branch"),
        }
    }
}

/// Suspension provides method to suspend calling coroutine.
pub struct Suspension<T: 'static>(Rc<SuspensionJoint<T>>);

/// Resumption provides method to resume suspending coroutine.
pub struct Resumption<T: 'static> {
    joint: Rc<SuspensionJoint<T>>,
    resumed: bool,
}

assert_not_impl_any!(Suspension<()>: Send);
assert_not_impl_any!(Resumption<()>: Send);

impl<T> Suspension<T> {
    /// Suspends calling coroutine until [Resumption::resume].
    ///
    /// # Panics
    /// Panic if no resume from [Resumption].
    pub fn suspend(self) -> T {
        match self.0.join() {
            Ok(value) => value,
            Err(err) => panic::resume_unwind(err),
        }
    }
}

impl<T> Resumption<T> {
    fn new(joint: Rc<SuspensionJoint<T>>) -> Self {
        Resumption { joint, resumed: false }
    }

    /// Resumes suspending coroutine.
    pub fn resume(mut self, value: T) {
        self.resumed = true;
        self.joint.wake(value);
    }

    fn set_result(mut self, result: Result<T, Box<dyn Any + Send + 'static>>) {
        self.resumed = true;
        self.joint.set_result(result);
    }
}

impl<T> Drop for Resumption<T> {
    fn drop(&mut self) {
        if !self.resumed {
            self.joint.abort()
        }
    }
}

/// Constructs cooperative facilities to suspend and resume coroutine in current task.
pub fn suspension<T>() -> (Suspension<T>, Resumption<T>) {
    let joint = SuspensionJoint::new();
    let suspension = Suspension(joint.clone());
    (suspension, Resumption::new(joint))
}

/// JoinHandle provides method to retrieve result of associated cooperative task.
pub struct JoinHandle<T: 'static> {
    joint: Rc<SuspensionJoint<T>>,
}

assert_not_impl_any!(JoinHandle<()>: Send);

impl<T> JoinHandle<T> {
    pub fn join(self) -> Result<T, JoinError> {
        self.joint.join().map_err(|err| JoinError::new(err))
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
    let joint = SuspensionJoint::new();
    let handle = JoinHandle { joint: joint.clone() };
    // Wrap joint to abort JoinHandle::join on Resumption::drop.
    let resumption = Resumption::new(joint);
    let f = Box::new(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(f));
        resumption.set_result(result);
    });
    let task = unsafe { task.as_mut() };
    task.spawn(f, StackSize::default());
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

    #[crate::test(package = "crate")]
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
