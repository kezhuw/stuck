use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::{mem, ptr};

use static_assertions::{assert_impl_all, assert_not_impl_any};

use crate::coroutine::stack::StackSize;
use crate::coroutine::{self, Coroutine};
use crate::error::{JoinError, PanicError};
use crate::runtime::Scheduler;

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
        let joint = SessionJoint::new();
        let handle = JoinHandle::new(joint.clone());
        let mut waker = SessionWaker::new(joint);
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
    joint: Arc<SessionJoint<T>>,
}

unsafe impl<T: Send + 'static> Send for JoinHandle<T> {}

assert_impl_all!(JoinHandle<()>: Send);

impl<T: Send + 'static> JoinHandle<T> {
    fn new(joint: Arc<SessionJoint<T>>) -> JoinHandle<T> {
        JoinHandle { joint }
    }

    /// Waits for associated task to finish and returns its result.
    pub fn join(self) -> Result<T, JoinError> {
        self.joint.join().map_err(|err| JoinError::new(err))
    }
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
    suspending_coroutines: HashSet<ptr::NonNull<Coroutine>>,

    // Blocking for events outside this task
    blocking_coroutines: HashSet<ptr::NonNull<Coroutine>>,

    // Unblocking by events from outside this task
    unblocking_coroutines: Mutex<HashMap<ptr::NonNull<Coroutine>, Option<&'static str>>>,
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
            suspending_coroutines: HashSet::new(),
            blocking_coroutines: HashSet::new(),
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

    pub fn interrupt_coroutine(mut co: ptr::NonNull<Coroutine>, msg: &'static str) {
        unsafe { co.as_mut() }.set_panic(msg);
    }

    pub fn unblock(&mut self, block: bool) -> bool {
        if self.blocking_coroutines.is_empty() {
            return false;
        }
        let mut unblockings = self.unblocking_coroutines.lock().unwrap();
        let mut unblocked = HashMap::new();
        mem::swap(&mut unblocked, &mut unblockings);
        if block && unblocked.is_empty() {
            self.running.set(false);
            return false;
        }
        drop(unblockings);
        for (co, panicking) in unblocked.into_iter() {
            if self.blocking_coroutines.remove(&co) {
                if let Some(msg) = panicking {
                    Self::interrupt_coroutine(co, msg);
                }
                self.running_coroutines.push_back(co);
            }
        }
        true
    }

    fn interrupt(&mut self, msg: &'static str) {
        for co in self.yielding_coroutines.drain(..) {
            Self::interrupt_coroutine(co, msg);
            self.running_coroutines.push_back(co);
        }
        for co in self.suspending_coroutines.drain() {
            Self::interrupt_coroutine(co, msg);
            self.running_coroutines.push_back(co);
        }
        for co in self.blocking_coroutines.drain() {
            Self::interrupt_coroutine(co, msg);
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
        if self.suspending_coroutines.remove(&co) {
            self.running_coroutines.push_back(co);
        }
    }

    pub fn suspend(&mut self, mut co: ptr::NonNull<Coroutine>) {
        assert!(co == coroutine::current(), "suspend: running coroutine changed");
        self.suspending_coroutines.insert(co);
        let co = unsafe { co.as_mut() };
        co.suspend();
    }

    fn block(&mut self, mut co: ptr::NonNull<Coroutine>) {
        assert!(co == coroutine::current(), "Session.block: running coroutine changed");
        self.blocking_coroutines.insert(co);
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

    fn wake(&self, co: ptr::NonNull<Coroutine>, panicking: Option<&'static str>) -> bool {
        let mut unblockings = self.unblocking_coroutines.lock().unwrap();
        let waking = if unblockings.is_empty() && !self.running.get() {
            self.running.set(true);
            true
        } else {
            false
        };
        unblockings.insert(co, panicking);
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
    Panicked(Box<dyn Any + Send + 'static>),
    TaskJoining { scheduler: ptr::NonNull<Scheduler>, task: Weak<Task>, coroutine: ptr::NonNull<Coroutine> },
    ThreadJoining,
}

struct SessionJoint<T: Send + 'static> {
    state: Mutex<SessionState<T>>,

    // - Use Option to avoid create os resource if not needed.
    // - Use `UnsafeCell` to construct one if necessary.
    // - Store it outside state so we can access it after unlocking.
    condvar: UnsafeCell<Option<Condvar>>,
}

// SAFETY: There are two immutable accessors.
unsafe impl<T: Send> Sync for SessionJoint<T> {}

// SAFETY: Normally, two immutable accessors are distributed to different tasks or threads.
unsafe impl<T: Send> Send for SessionJoint<T> {}

impl<T: Send + 'static> SessionJoint<T> {
    fn new() -> Arc<Self> {
        Arc::new(SessionJoint { state: Mutex::new(SessionState::Empty), condvar: UnsafeCell::new(None) })
    }

    fn condvar(&self) -> &Condvar {
        unsafe { self.condvar.get().as_ref().unwrap().as_ref().unwrap() }
    }

    fn set_condvar(&self) -> &Condvar {
        let condvar = unsafe { self.condvar.get().as_mut().unwrap() };
        *condvar = Some(Condvar::new());
        condvar.as_ref().unwrap()
    }

    fn unblock(&self, mut state: SessionState<T>) {
        let mut locked = self.state.lock().unwrap();
        state = mem::replace(&mut *locked, state);
        drop(locked);
        match state {
            SessionState::TaskJoining { scheduler, task, coroutine } => {
                if let Some(task) = task.upgrade() {
                    if task.wake(coroutine, None) {
                        // SAFETY: scheduler lives longer than task
                        let scheduler = unsafe { scheduler.as_ref() };
                        scheduler.resume(&task);
                    }
                }
            },
            SessionState::ThreadJoining => {
                let condvar = self.condvar();
                condvar.notify_one();
            },
            SessionState::Empty => {},
            _ => unreachable!("session: dual wakeup"),
        }
    }

    fn abort(&self) {
        self.unblock(SessionState::Panicked(Box::new("session: no wakeup")));
    }

    fn wake(&self, value: T) {
        self.unblock(SessionState::Value(value));
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

    fn wait_on_task(&self, mut task: ptr::NonNull<Task>, locked: MutexGuard<SessionState<T>>) -> Result<T, PanicError> {
        let co = self.set_task_waiter(task, locked);
        let task = unsafe { task.as_mut() };
        task.block(co);
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
        let state = mem::replace(&mut *locked, SessionState::Empty);
        match state {
            SessionState::Empty => match task() {
                None => self.wait_on_thread(locked),
                Some(task) => self.wait_on_task(task, locked),
            },
            SessionState::Value(value) => Ok(value),
            SessionState::Panicked(err) => Err(err),
            _ => unreachable!("already in joining"),
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
    waked: bool,
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

    /// Waits peer to wake it.
    ///
    /// # Panics
    /// Panic if no wakeup from [SessionWaker].
    pub fn wait(self) -> T {
        match self.joint.join() {
            Ok(value) => value,
            Err(err) => panic::resume_unwind(err),
        }
    }
}

impl<T: Send> Drop for SessionWaker<T> {
    fn drop(&mut self) {
        if !self.waked {
            self.joint.abort();
        }
    }
}

impl<T: Send> SessionWaker<T> {
    fn new(joint: Arc<SessionJoint<T>>) -> SessionWaker<T> {
        SessionWaker { joint, waked: false, marker: PhantomData }
    }

    /// Wakes peer.
    pub fn wake(mut self, value: T) {
        self.waked = true;
        self.joint.wake(value)
    }

    fn set_result(&mut self, result: Result<T, PanicError>) {
        self.waked = true;
        let state = match result {
            Ok(value) => SessionState::Value(value),
            Err(err) => SessionState::Panicked(err),
        };
        self.joint.unblock(state);
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

    use crate::runtime::Builder;
    use crate::{coroutine, task};

    #[test]
    fn yield_now() {
        let runtime = Builder::default().parallelism(1).build();
        let five = runtime.spawn(|| {
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
}
