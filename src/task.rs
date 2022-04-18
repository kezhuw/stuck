use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::{mem, ptr};

use ignore_result::Ignore;
use static_assertions::{assert_impl_all, assert_not_impl_any};

use crate::coroutine::stack::StackSize;
use crate::coroutine::{self, Coroutine};
use crate::error::{JoinError, PanicError};
use crate::runtime::Scheduler;

pub mod mpsc;

static TID_COUNTER: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static TASK: Cell<Option<ptr::NonNull<Task>>> = Cell::new(None);
}

pub(crate) fn task() -> Option<ptr::NonNull<Task>> {
    TASK.with(|cell| cell.get())
}

pub(crate) fn current() -> ptr::NonNull<Task> {
    task().expect("no running task")
}

struct Scope {
    task: ptr::NonNull<Task>,
}

impl Scope {
    fn enter(task: &Task) -> Self {
        TASK.with(|cell| {
            assert!(cell.get().is_none());
            cell.set(Some(ptr::NonNull::from(task)));
        });
        Scope { task: ptr::NonNull::from(task) }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        TASK.with(|cell| {
            let task = cell.replace(None).expect("no running task");
            assert!(self.task == task, "running task changed");
        });
    }
}

pub(crate) type FnMain = Box<dyn FnOnce()>;

/// Builder for concurrent task.
#[derive(Default)]
pub struct Builder<'a> {
    stack_size: StackSize,
    scheduler: Option<&'a Scheduler>,
}

// assert_not_impl_any!(Builder<'static>: Send);
assert_impl_all!(Builder<'static>: Send);

impl Builder<'_> {
    /// Constructs a new task builder.
    pub fn new() -> Builder<'static> {
        Builder { stack_size: StackSize::default(), scheduler: None }
    }

    pub(crate) fn with_scheduler(scheduler: &Scheduler) -> Builder<'_> {
        Builder { stack_size: StackSize::default(), scheduler: Some(scheduler) }
    }

    /// Specifies stack size for new task.
    pub fn stack_size(&mut self, stack_size: StackSize) -> &mut Self {
        self.stack_size = stack_size;
        self
    }

    /// Spawns a concurrent task and returns a [JoinHandle] for it.
    ///
    /// See [spawn] for more details
    pub fn spawn<F, T>(&mut self, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        let scheduler = self.scheduler.or_else(Scheduler::try_current).expect("no runtime");
        let (session, waker) = session();
        let handle = JoinHandle::new(session);
        let main: FnMain = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            waker.set_result(result);
        });
        let task = Task::new(main, self.stack_size);
        scheduler.sched(task);
        handle
    }
}

/// JoinHandle provides method to retrieve result of associated concurrent task.
pub struct JoinHandle<T: Send + 'static> {
    session: Session<T>,
}

unsafe impl<T: Send + 'static> Send for JoinHandle<T> {}

assert_impl_all!(JoinHandle<()>: Send);

impl<T: Send + 'static> JoinHandle<T> {
    fn new(session: Session<T>) -> JoinHandle<T> {
        JoinHandle { session }
    }

    /// Waits for associated task to finish and returns its result.
    pub fn join(self) -> Result<T, JoinError> {
        let joint = unsafe { self.session.into_joint() };
        joint.join().map_err(JoinError::new)
    }
}

// Yielding point.
pub(crate) trait Yielding {
    fn interrupt(&self, reason: &'static str);
}

pub(crate) struct Task {
    id: u64,

    // main is special as task will terminate after main terminated
    main: ptr::NonNull<Coroutine>,

    running: Cell<bool>,

    aborting: bool,

    yielding: bool,

    // Newly spawned coroutines.
    spawned_coroutines: Vec<ptr::NonNull<Coroutine>>,

    // We are running
    running_coroutines: VecDeque<ptr::NonNull<Coroutine>>,

    // Yielding cpu in this run.
    yielding_coroutines: Vec<ptr::NonNull<Coroutine>>,

    // Suspending for events inside this task
    suspending_coroutines: HashMap<ptr::NonNull<Coroutine>, &'static dyn Yielding>,

    // Blocking for events outside this task
    blocking_coroutines: HashMap<ptr::NonNull<Coroutine>, &'static dyn Yielding>,

    // Unblocking by events from outside this task
    unblocking_coroutines: Mutex<Vec<ptr::NonNull<Coroutine>>>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.spawned_coroutines.drain(..).for_each(Self::drop_coroutine);
    }
}

unsafe impl Sync for Task {}
unsafe impl Send for Task {}

pub(crate) enum SchedFlow {
    Yield,
    Block,
    Cease,
}

impl Task {
    fn new(f: Box<dyn FnOnce()>, stack_size: StackSize) -> Arc<Task> {
        let main = Coroutine::new(f, stack_size);
        Arc::new(Self::with_main(main))
    }

    fn with_main(main: Box<Coroutine>) -> Task {
        let co = ptr::NonNull::from(Box::leak(main));
        let id = TID_COUNTER.fetch_add(1, Ordering::Relaxed);
        Task {
            id,
            main: co,
            running: Cell::new(true),
            aborting: false,
            yielding: false,
            spawned_coroutines: vec![co],
            running_coroutines: VecDeque::with_capacity(5),
            yielding_coroutines: Vec::with_capacity(5),
            suspending_coroutines: HashMap::new(),
            blocking_coroutines: HashMap::new(),
            unblocking_coroutines: Mutex::new(Default::default()),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    fn drop_coroutine(co: ptr::NonNull<Coroutine>) {
        drop(unsafe { Box::from_raw(co.as_ptr()) });
    }

    pub fn run_coroutine(&mut self, mut co: ptr::NonNull<Coroutine>) {
        if unsafe { co.as_mut() }.resume() {
            return;
        }
        if co == self.main && !self.aborting {
            self.abort("task main terminated");
        }
        Self::drop_coroutine(co);
    }

    pub fn unblock(&mut self, block: bool) -> bool {
        if self.blocking_coroutines.is_empty() {
            return false;
        }
        let mut unblockings = self.unblocking_coroutines.lock().unwrap();
        let mut unblocked = Vec::new();
        mem::swap(&mut unblocked, &mut unblockings);
        if block && unblocked.is_empty() {
            self.running.set(false);
            return false;
        }
        drop(unblockings);
        for co in unblocked.into_iter() {
            if self.blocking_coroutines.remove(&co).is_some() {
                self.running_coroutines.push_back(co);
            }
        }
        true
    }

    fn interrupt(&mut self, msg: &'static str) {
        for co in self.yielding_coroutines.drain(..) {
            self.running_coroutines.push_back(co);
        }
        for (co, yielding) in self.suspending_coroutines.drain() {
            yielding.interrupt(msg);
            self.running_coroutines.push_back(co);
        }
        for (co, yielding) in self.blocking_coroutines.drain() {
            yielding.interrupt(msg);
            self.running_coroutines.push_back(co);
        }
    }

    pub fn abort(&mut self, msg: &'static str) {
        self.aborting = true;
        loop {
            self.interrupt(msg);
            if self.running_coroutines.is_empty() {
                break;
            }
            while let Some(co) = self.running_coroutines.pop_front() {
                self.run_coroutine(co);
            }
        }
        self.spawned_coroutines.drain(..).for_each(Self::drop_coroutine);
    }

    // Grab this task to runq. Return false if waker win.
    pub fn grab(&self) -> bool {
        let _locker = self.unblocking_coroutines.lock().unwrap();
        !self.running.replace(true)
    }

    pub fn sched(&mut self) -> SchedFlow {
        let _scope = Scope::enter(self);
        self.running_coroutines.extend(self.spawned_coroutines.drain(..));
        self.running_coroutines.extend(self.yielding_coroutines.drain(..));
        self.unblock(false);
        while !(self.yielding || (self.spawned_coroutines.is_empty() && self.running_coroutines.is_empty())) {
            while let Some(co) = self.spawned_coroutines.pop() {
                self.run_coroutine(co);
            }
            while let Some(co) = self.running_coroutines.pop_front() {
                self.run_coroutine(co);
            }
        }
        self.yielding = false;
        if !self.yielding_coroutines.is_empty() {
            SchedFlow::Yield
        } else if self.blocking_coroutines.is_empty() {
            if !self.suspending_coroutines.is_empty() {
                self.abort("deadlock suspending coroutines");
            }
            SchedFlow::Cease
        } else if self.unblock(true) {
            SchedFlow::Yield
        } else {
            SchedFlow::Block
        }
    }

    pub fn resume(&mut self, co: ptr::NonNull<Coroutine>) {
        if self.suspending_coroutines.remove(&co).is_some() {
            self.running_coroutines.push_back(co);
        }
    }

    pub fn suspend(&mut self, mut co: ptr::NonNull<Coroutine>, yielding: &dyn Yielding) {
        assert!(co == coroutine::current(), "suspend: running coroutine changed");
        let yielding = unsafe { std::mem::transmute::<&dyn Yielding, &'_ dyn Yielding>(yielding) };
        self.suspending_coroutines.insert(co, yielding);
        let co = unsafe { co.as_mut() };
        co.suspend();
    }

    fn block(&mut self, mut co: ptr::NonNull<Coroutine>, yielding: &dyn Yielding) {
        assert!(co == coroutine::current(), "Session.block: running coroutine changed");
        let yielding = unsafe { std::mem::transmute::<&dyn Yielding, &'_ dyn Yielding>(yielding) };
        self.blocking_coroutines.insert(co, yielding);
        let co = unsafe { co.as_mut() };
        co.suspend();
    }

    pub fn yield_coroutine(&mut self, mut co: ptr::NonNull<Coroutine>) {
        self.yielding_coroutines.push(co);
        let co = unsafe { co.as_mut() };
        co.suspend();
    }

    fn yield_task(&mut self) {
        self.yielding = true;
        let co = coroutine::current();
        self.yield_coroutine(co);
    }

    fn wake(&self, co: ptr::NonNull<Coroutine>) -> bool {
        let mut unblockings = self.unblocking_coroutines.lock().unwrap();
        let waking = if unblockings.is_empty() && !self.running.get() {
            self.running.set(true);
            true
        } else {
            false
        };
        unblockings.push(co);
        mem::drop(unblockings);
        waking
    }

    pub fn spawn(&mut self, f: FnMain, stack_size: StackSize) -> ptr::NonNull<Coroutine> {
        let co = ptr::NonNull::from(Box::leak(Coroutine::new(f, stack_size)));
        self.spawned_coroutines.push(co);
        co
    }
}

enum SessionState<T: Send + 'static> {
    Empty,
    Value(T),
    Panicked(PanicError),
    TaskJoining { scheduler: ptr::NonNull<Scheduler>, task: Weak<Task>, coroutine: ptr::NonNull<Coroutine> },
    ThreadJoining,
    Joined,
}

struct SessionJoint<T: Send + 'static> {
    // status: AtomicUsize,
    // value: UnsafeCell<SessionValue<T>>,
    state: Mutex<SessionState<T>>,

    // - Use Option to avoid create os resource if not needed.
    // - Use `UnsafeCell` to construct one if necessary.
    // - Store it outside state so we can access it after unlocking.
    condvar: UnsafeCell<Option<Condvar>>,

    wakers: AtomicUsize,
}

// SAFETY: There are two immutable accessors.
unsafe impl<T: Send> Sync for SessionJoint<T> {}

// SAFETY: Normally, two immutable accessors are distributed to different tasks or threads.
unsafe impl<T: Send> Send for SessionJoint<T> {}

impl<T: Send + 'static> Yielding for SessionJoint<T> {
    fn interrupt(&self, reason: &'static str) {
        self.cancel(PanicError::Static(reason));
    }
}

impl<T: Send + 'static> SessionJoint<T> {
    fn new() -> Arc<Self> {
        Arc::new(SessionJoint {
            state: Mutex::new(SessionState::Empty),
            condvar: UnsafeCell::new(None),
            wakers: AtomicUsize::new(1),
        })
    }

    fn condvar(&self) -> &Condvar {
        unsafe { self.condvar.get().as_ref().unwrap().as_ref().unwrap() }
    }

    fn set_condvar(&self) -> &Condvar {
        let condvar = unsafe { self.condvar.get().as_mut().unwrap() };
        *condvar = Some(Condvar::new());
        condvar.as_ref().unwrap()
    }

    #[cfg(test)]
    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        matches!(*state, SessionState::Value(_) | SessionState::Panicked(_) | SessionState::Joined)
    }

    fn wake(&self, value: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            SessionState::TaskJoining { scheduler, task, coroutine } => {
                let task = unsafe { ptr::read(task) };
                let scheduler = *scheduler;
                let coroutine = *coroutine;
                unsafe { ptr::write(&mut *state, SessionState::Value(value)) };
                drop(state);
                Self::wake_task(scheduler, task, coroutine);
                Ok(())
            },
            SessionState::ThreadJoining => {
                unsafe { ptr::write(&mut *state, SessionState::Value(value)) };
                drop(state);
                let condvar = self.condvar();
                condvar.notify_one();
                Ok(())
            },
            SessionState::Empty => {
                unsafe { ptr::write(&mut *state, SessionState::Value(value)) };
                drop(state);
                Ok(())
            },
            _ => Err(value),
        }
    }

    fn cancel(&self, err: PanicError) -> Option<(ptr::NonNull<Scheduler>, Weak<Task>, ptr::NonNull<Coroutine>)> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            SessionState::TaskJoining { scheduler, task, coroutine } => {
                let task = unsafe { ptr::read(task) };
                let scheduler = *scheduler;
                let coroutine = *coroutine;
                unsafe { ptr::write(&mut *state, SessionState::Panicked(err)) };
                drop(state);
                Some((scheduler, task, coroutine))
            },
            SessionState::ThreadJoining => {
                unsafe { ptr::write(&mut *state, SessionState::Panicked(err)) };
                drop(state);
                let condvar = self.condvar();
                condvar.notify_one();
                None
            },
            SessionState::Empty => {
                unsafe { ptr::write(&mut *state, SessionState::Panicked(err)) };
                drop(state);
                None
            },
            _ => None,
        }
    }

    fn fault(&self, err: PanicError) {
        if let Some((scheduler, task, coroutine)) = self.cancel(err) {
            Self::wake_task(scheduler, task, coroutine);
        }
    }

    fn add_waker(&self) {
        self.wakers.fetch_add(1, Ordering::Relaxed);
    }

    fn remove_waker(&self) {
        if self.wakers.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.fault(PanicError::Static("session: no wakeup"));
        }
    }

    fn wait_on_thread(&self, mut locked: MutexGuard<SessionState<T>>) -> Result<T, PanicError> {
        *locked = SessionState::ThreadJoining;
        let condvar = self.set_condvar();
        // Loop to avoid spurious wakeup
        loop {
            locked = condvar.wait(locked).unwrap();
            let state = mem::replace(&mut *locked, SessionState::ThreadJoining);
            match state {
                SessionState::Value(value) => return Ok(value),
                SessionState::Panicked(err) => return Err(err),
                SessionState::ThreadJoining => {},
                SessionState::TaskJoining { .. } => {
                    unreachable!("session: state transit from ThreadJoining to TaskJoining")
                },
                SessionState::Empty => unreachable!("session: state transit from ThreadJoining to Empty"),
                SessionState::Joined => unreachable!("session: state transit from ThreadJoining to Joined"),
            }
        }
    }

    fn set_task_waiter(
        &self,
        task: ptr::NonNull<Task>,
        mut locked: MutexGuard<SessionState<T>>,
    ) -> ptr::NonNull<Coroutine> {
        let strong = unsafe { Arc::from_raw(task.as_ptr()) };
        let weak = Arc::downgrade(&strong);
        mem::forget(strong);
        let co = coroutine::current();
        let scheduler = unsafe { ptr::NonNull::from(Scheduler::current()) };
        *locked = SessionState::TaskJoining { scheduler, task: weak, coroutine: co };
        co
    }

    fn wake_task(scheduler: ptr::NonNull<Scheduler>, task: Weak<Task>, coroutine: ptr::NonNull<Coroutine>) {
        if let Some(task) = task.upgrade() {
            if task.wake(coroutine) {
                // SAFETY: scheduler lives longer than task
                let scheduler = unsafe { scheduler.as_ref() };
                scheduler.resume(&task);
            }
        }
    }

    fn wait_on_task(&self, mut task: ptr::NonNull<Task>, locked: MutexGuard<SessionState<T>>) -> Result<T, PanicError> {
        let co = self.set_task_waiter(task, locked);
        let task = unsafe { task.as_mut() };
        task.block(co, self);
        let mut locked = self.state.lock().unwrap();
        let state = mem::replace(&mut *locked, SessionState::Empty);
        match state {
            SessionState::Value(value) => Ok(value),
            SessionState::Panicked(err) => Err(err),
            _ => unreachable!("not in joining"),
        }
    }

    fn join(&self) -> Result<T, PanicError> {
        let mut locked = self.state.lock().unwrap();
        let state = mem::replace(&mut *locked, SessionState::Joined);
        match state {
            SessionState::Empty => match task() {
                None => self.wait_on_thread(locked),
                Some(task) => self.wait_on_task(task, locked),
            },
            SessionState::Value(value) => Ok(value),
            SessionState::Panicked(err) => Err(err),
            SessionState::TaskJoining { .. } | SessionState::ThreadJoining => unreachable!("already joining"),
            SessionState::Joined => unreachable!("already joined"),
        }
    }
}

/// Session provides method to block current coroutine until waking by [SessionWaker].
pub struct Session<T: Send + 'static> {
    joint: Arc<SessionJoint<T>>,
    marker: PhantomData<NotSendable>,
}

/// SessionWaker provides method to wake associated [Session].
pub struct SessionWaker<T: Send + 'static> {
    joint: Arc<SessionJoint<T>>,
    marker: PhantomData<Sendable>,
}

struct NotSendable(std::rc::Rc<()>);
assert_not_impl_any!(NotSendable: Send, Sync);

struct Sendable(std::rc::Rc<()>);
unsafe impl Send for Sendable {}
assert_impl_all!(Sendable: Send);
assert_not_impl_any!(Sendable: Sync);

// SessionWaker should be able to send across tasks and threads.
assert_impl_all!(SessionWaker<Sendable>: Send);

// SessionWaker should owned by only one task or thread.
assert_not_impl_any!(SessionWaker<Sendable>: Sync);

// Session should be used only be producing task or thread.
assert_not_impl_any!(Session<Sendable>: Send, Sync);

impl<T: Send + 'static> Session<T> {
    fn new(joint: Arc<SessionJoint<T>>) -> Session<T> {
        Session { joint, marker: PhantomData }
    }

    unsafe fn into_joint(self) -> Arc<SessionJoint<T>> {
        let joint = ptr::read(&self.joint);
        mem::forget(self);
        joint
    }

    /// Waits peer to wake it.
    ///
    /// # Panics
    /// Panic if no wakeup from [SessionWaker].
    ///
    /// # Guarantee
    /// Only two situations can happen:
    /// * This method panics and no value sent
    /// * This method returns and only one value sent
    ///
    /// This means that no value linger after panic.
    pub fn wait(self) -> T {
        let joint = unsafe { self.into_joint() };
        match joint.join() {
            Ok(value) => value,
            Err(PanicError::Static(s)) => panic::panic_any(s),
            Err(PanicError::Unwind(err)) => panic::resume_unwind(err),
        }
    }
}

impl<T: Send + 'static> Drop for Session<T> {
    fn drop(&mut self) {
        self.joint.cancel(PanicError::Static("session dropped"));
    }
}

impl<T: Send> Clone for SessionWaker<T> {
    fn clone(&self) -> Self {
        self.joint.add_waker();
        Self { joint: self.joint.clone(), marker: PhantomData }
    }
}

impl<T: Send> Drop for SessionWaker<T> {
    fn drop(&mut self) {
        self.joint.remove_waker();
    }
}

impl<T: Send> SessionWaker<T> {
    fn new(joint: Arc<SessionJoint<T>>) -> SessionWaker<T> {
        SessionWaker { joint, marker: PhantomData }
    }

    // SAFETY: Forget self and cancel drop to wake peer with no suspension interleave.
    unsafe fn into_joint(self) -> Arc<SessionJoint<T>> {
        let joint = ptr::read(&self.joint);
        mem::forget(self);
        joint
    }

    /// Wakes peer.
    pub fn wake(self, value: T) {
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
        match result {
            Ok(value) => joint.wake(value).ignore(),
            Err(err) => joint.fault(PanicError::Unwind(err)),
        };
    }
}

/// Constructs cooperative facilities to wait and wake coroutine across task boundary.
pub fn session<T>() -> (Session<T>, SessionWaker<T>)
where
    T: Send,
{
    let joint = SessionJoint::new();
    let session = Session::new(joint.clone());
    let session_waker = SessionWaker::new(joint);
    (session, session_waker)
}

/// Yields task for next scheduling cycle.
pub fn yield_now() {
    let t = unsafe { current().as_mut() };
    t.yield_task();
}

/// Spawns a concurrent task and returns a [JoinHandle] for it.
///
/// The given fn serves as `main` in spawned task. All other coroutines in that task will be
/// aborted through [panic!] when given fn completed.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: Send + 'static,
    T: Send + 'static,
{
    Builder::new().spawn(f)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use pretty_assertions::assert_eq;

    use crate::{coroutine, task, time};

    #[crate::test(crate = "crate", parallelism = 1)]
    fn yield_now() {
        let five = task::spawn(|| {
            let shared_value = Arc::new(Mutex::new(0));
            let shared_task_value = shared_value.clone();
            let shared_coroutine_value = shared_value.clone();
            coroutine::spawn(move || {
                let mut value = shared_coroutine_value.lock().unwrap();
                if *value == 0 {
                    *value = 6;
                }
            });
            task::spawn(move || {
                let mut value = shared_task_value.lock().unwrap();
                if *value == 0 {
                    *value = 5;
                }
            });
            task::yield_now();
            let value = shared_value.lock().unwrap();
            *value
        });
        assert_eq!(5, five.join().unwrap());
    }

    #[crate::test(crate = "crate")]
    fn session_waker() {
        let (session, waker) = task::session();
        drop(waker.clone());
        let task1 = task::spawn({
            let waker = waker.clone();
            move || waker.send(5)
        });
        let task2 = task::spawn(move || waker.send(6));
        let value = session.wait();
        let mut result1 = task1.join().unwrap();
        let mut result2 = task2.join().unwrap();
        if result1.is_err() {
            std::mem::swap(&mut result1, &mut result2);
        }
        assert_eq!(result1, Ok(()));
        assert_eq!(result2.is_err(), true);
        assert_eq!(value, 11 - result2.unwrap_err());
    }

    #[crate::test(crate = "crate")]
    fn session_dropped() {
        let (session, waker) = task::session::<()>();
        drop(session);
        assert_eq!(waker.joint.is_ready(), true);
    }

    #[crate::test(crate = "crate")]
    fn panic() {
        const REASON: &'static str = "oooooops";
        let t = task::spawn(|| panic!("{}", REASON));
        let err = t.join().unwrap_err();
        assert!(err.to_string().contains(REASON))
    }

    #[crate::test(crate = "crate")]
    fn main_coroutine() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::Duration;

        let t = task::spawn(|| {
            let cell = Rc::new(Cell::new(0));
            coroutine::spawn({
                let cell = cell.clone();
                move || {
                    time::sleep(Duration::from_secs(20));
                    cell.set(10);
                }
            });
            coroutine::spawn({
                let cell = cell.clone();
                move || {
                    cell.set(5);
                }
            })
            .join()
            .unwrap();
            cell.get()
        });
        assert_eq!(t.join().unwrap(), 5);
    }
}
