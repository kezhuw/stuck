mod context;
mod page_size;
pub(crate) mod stack;

use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::{mem, ptr};

use ignore_result::Ignore;
use static_assertions::assert_not_impl_any;

use self::context::{Context, Entry};
use self::stack::StackSize;
use crate::error::{JoinError, PanicError};
use crate::task::{self, Yielding};

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
    }
}

enum SuspensionState<T: 'static> {
    Empty,
    Value(T),
    Panicked(PanicError),
    Joining(ptr::NonNull<Coroutine>),
    Joined,
}

struct SuspensionJoint<T: 'static> {
    state: UnsafeCell<SuspensionState<T>>,
    wakers: Cell<usize>,
}

impl<T> Yielding for SuspensionJoint<T> {
    fn interrupt(&self, reason: &'static str) -> bool {
        self.cancel(PanicError::Static(reason));
        true
    }
}

impl<T> SuspensionJoint<T> {
    fn new() -> Rc<SuspensionJoint<T>> {
        Rc::new(SuspensionJoint { state: UnsafeCell::new(SuspensionState::Empty), wakers: Cell::new(1) })
    }

    #[cfg(test)]
    fn is_ready(&self) -> bool {
        let state = unsafe { &*self.state.get() };
        matches!(state, SuspensionState::Value(_) | SuspensionState::Panicked(_) | SuspensionState::Joined)
    }

    fn wake_coroutine(co: ptr::NonNull<Coroutine>) {
        let task = unsafe { task::current().as_mut() };
        task.resume(co);
    }

    fn add_waker(&self) {
        let wakers = self.wakers.get() + 1;
        self.wakers.set(wakers);
    }

    fn remove_waker(&self) {
        let wakers = self.wakers.get() - 1;
        self.wakers.set(wakers);
        if wakers == 0 {
            self.fault(PanicError::Static("suspend: no resumption"));
        }
    }

    fn cancel(&self, err: PanicError) -> Option<ptr::NonNull<Coroutine>> {
        let state = unsafe { &mut *self.state.get() };
        if matches!(state, SuspensionState::Value(_) | SuspensionState::Panicked(_) | SuspensionState::Joined) {
            return None;
        }
        let state = unsafe { ptr::replace(state, SuspensionState::Panicked(err)) };
        if let SuspensionState::Joining(co) = state {
            return Some(co);
        }
        None
    }

    fn fault(&self, err: PanicError) {
        if let Some(co) = self.cancel(err) {
            Self::wake_coroutine(co);
        }
    }

    pub fn wake(&self, value: T) -> Result<(), T> {
        let state = unsafe { &mut *self.state.get() };
        if matches!(state, SuspensionState::Value(_) | SuspensionState::Panicked(_) | SuspensionState::Joined) {
            return Err(value);
        }
        let state = unsafe { ptr::replace(state, SuspensionState::Value(value)) };
        if let SuspensionState::Joining(co) = state {
            Self::wake_coroutine(co);
        }
        Ok(())
    }

    fn set_result(&self, result: Result<T, Box<dyn Any + Send + 'static>>) {
        match result {
            Ok(value) => self.wake(value).ignore(),
            Err(err) => self.fault(PanicError::Unwind(err)),
        }
    }

    fn take(&self) -> Result<T, PanicError> {
        let state = mem::replace(unsafe { &mut *self.state.get() }, SuspensionState::Joined);
        match state {
            SuspensionState::Value(value) => Ok(value),
            SuspensionState::Panicked(err) => Err(err),
            SuspensionState::Empty => unreachable!("suspension: empty state"),
            SuspensionState::Joining(_) => unreachable!("suspension: joining state"),
            SuspensionState::Joined => unreachable!("suspension: joined state"),
        }
    }

    fn join(&self) -> Result<T, PanicError> {
        let co = current();
        let state = mem::replace(unsafe { &mut *self.state.get() }, SuspensionState::Joining(co));
        match state {
            SuspensionState::Empty => {
                let task = unsafe { task::current().as_mut() };
                task.suspend(co, self);
                self.take()
            },
            SuspensionState::Value(value) => Ok(value),
            SuspensionState::Panicked(err) => Err(err),
            SuspensionState::Joining(_) => unreachable!("suspension: join joining state"),
            SuspensionState::Joined => unreachable!("suspension: join joined state"),
        }
    }
}

/// Suspension provides method to suspend calling coroutine.
pub struct Suspension<T: 'static>(Rc<SuspensionJoint<T>>);

/// Resumption provides method to resume suspending coroutine.
pub struct Resumption<T: 'static> {
    joint: Rc<SuspensionJoint<T>>,
}

assert_not_impl_any!(Suspension<()>: Send);
assert_not_impl_any!(Resumption<()>: Send);

impl<T> Suspension<T> {
    unsafe fn into_joint(self) -> Rc<SuspensionJoint<T>> {
        let joint = ptr::read(&self.0);
        mem::forget(self);
        joint
    }

    /// Suspends calling coroutine until [Resumption::resume].
    ///
    /// # Panics
    /// Panic if no resume from [Resumption].
    ///
    /// # Guarantee
    /// Only two situations can happen:
    /// * This method panics and no value sent
    /// * This method returns and only one value sent
    ///
    /// This means that no value linger after panic.
    pub fn suspend(self) -> T {
        let joint = unsafe { self.into_joint() };
        match joint.join() {
            Ok(value) => value,
            Err(PanicError::Unwind(err)) => panic::resume_unwind(err),
            Err(PanicError::Static(s)) => panic::panic_any(s),
        }
    }
}

impl<T> Drop for Suspension<T> {
    fn drop(&mut self) {
        self.0.cancel(PanicError::Static("suspension dropped"));
    }
}

impl<T> Resumption<T> {
    fn new(joint: Rc<SuspensionJoint<T>>) -> Self {
        Resumption { joint }
    }

    // SAFETY: Forget self and cancel drop to wake peer with no suspension interleave.
    unsafe fn into_joint(self) -> Rc<SuspensionJoint<T>> {
        let joint = ptr::read(&self.joint);
        mem::forget(self);
        joint
    }

    /// Resumes suspending coroutine.
    pub fn resume(self, value: T) {
        let joint = unsafe { self.into_joint() };
        joint.wake(value).ignore();
    }

    /// Sends and wakes peer if not waked.
    pub fn send(self, value: T) -> Result<(), T> {
        let joint = unsafe { self.into_joint() };
        joint.wake(value)
    }

    fn set_result(self, result: Result<T, Box<dyn Any + Send + 'static>>) {
        let joint = unsafe { self.into_joint() };
        joint.set_result(result);
    }
}

impl<T> Clone for Resumption<T> {
    fn clone(&self) -> Self {
        self.joint.add_waker();
        Resumption { joint: self.joint.clone() }
    }
}

impl<T> Drop for Resumption<T> {
    fn drop(&mut self) {
        self.joint.remove_waker();
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
    suspension: Suspension<T>,
}

assert_not_impl_any!(JoinHandle<()>: Send);

impl<T> JoinHandle<T> {
    pub fn join(self) -> Result<T, JoinError> {
        let joint = unsafe { self.suspension.into_joint() };
        joint.join().map_err(JoinError::new)
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
    let handle = JoinHandle { suspension };
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

    #[crate::test(crate = "crate")]
    fn resumption() {
        let (suspension, resumption) = coroutine::suspension();
        drop(resumption.clone());
        assert_eq!(suspension.0.is_ready(), false);
        let co1 = coroutine::spawn({
            let resumption = resumption.clone();
            move || resumption.send(5)
        });
        let co2 = coroutine::spawn(move || resumption.send(6));
        let value = suspension.suspend();
        let mut result1 = co1.join().unwrap();
        let mut result2 = co2.join().unwrap();
        if result1.is_err() {
            std::mem::swap(&mut result1, &mut result2);
        }
        assert_eq!(result1, Ok(()));
        assert_eq!(result2.is_err(), true);
        assert_eq!(value, 11 - result2.unwrap_err());
    }

    #[crate::test(crate = "crate")]
    fn suspension_dropped() {
        let (suspension, resumption) = coroutine::suspension::<()>();
        drop(suspension);
        assert_eq!(resumption.joint.is_ready(), true);
    }

    #[crate::test(crate = "crate")]
    fn panic() {
        const REASON: &'static str = "oooooops";
        let co = coroutine::spawn(|| panic!("{}", REASON));
        let err = co.join().unwrap_err();
        assert!(err.to_string().contains(REASON))
    }
}
