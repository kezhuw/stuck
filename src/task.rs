//! Lightweight concurrent execution units based on coroutines.

use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::{mem, ptr};

use hashbrown::HashMap;
use ignore_result::Ignore;
use static_assertions::assert_impl_all;

pub use self::session::{session, Session, SessionWaker};
use crate::coroutine::stack::StackSize;
use crate::coroutine::{self, Coroutine};
use crate::error::JoinError;
use crate::runtime::Scheduler;

mod session;

static TID_COUNTER: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static TASK: Cell<Option<ptr::NonNull<Task>>> = const {  Cell::new(None) };
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
            let task = unsafe { current().as_mut() };
            let co = unsafe { coroutine::current().as_mut() };
            task.status = Status::Completing;
            co.suspend();
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
        joint.join(None::<fn()>).map_err(JoinError::new)
    }
}

// Yielding point.
pub(crate) trait Yielding {
    fn interrupt(&self, reason: &'static str) -> bool;
}

static INTERRUPTIBLE: Interruptible = Interruptible {};
static UNINTERRUPTIBLE: Uninterruptible = Uninterruptible {};

struct Interruptible {}
struct Uninterruptible {}

impl Yielding for Interruptible {
    fn interrupt(&self, _reason: &'static str) -> bool {
        true
    }
}

impl Yielding for Uninterruptible {
    fn interrupt(&self, _reason: &'static str) -> bool {
        false
    }
}

pub(crate) enum Interruption<'a, T: FnOnce() = fn()> {
    Cancellation(T),
    Interruptible(&'a dyn Yielding),
    Uninterruptible,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Status {
    Running,
    Completing,
    Completed,
}

pub(crate) struct Task {
    id: u64,

    // main is special as task will terminate after main terminated
    main: ptr::NonNull<Coroutine>,

    status: Status,

    yielding: bool,

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

    // Guarded by above mutex.
    running: Cell<bool>,
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
            status: Status::Running,
            yielding: false,
            running_coroutines: VecDeque::from([co]),
            yielding_coroutines: Vec::with_capacity(5),
            suspending_coroutines: HashMap::new(),
            blocking_coroutines: HashMap::new(),
            unblocking_coroutines: Mutex::new(Default::default()),
            running: Cell::new(true),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    fn drop_coroutine(co: ptr::NonNull<Coroutine>) {
        drop(unsafe { Box::from_raw(co.as_ptr()) });
    }

    fn run_coroutine(&mut self, mut co: ptr::NonNull<Coroutine>) -> coroutine::Status {
        let status = unsafe { co.as_mut().resume() };
        if status != coroutine::Status::Completed {
            return status;
        }
        if co == self.main {
            self.status = Status::Completed;
        }
        Self::drop_coroutine(co);
        status
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
        for (mut co, yielding) in self.suspending_coroutines.drain() {
            yielding.interrupt(msg);
            self.running_coroutines.push_back(co);
            let co = unsafe { co.as_mut() };
            co.status = co.status.into_abort();
        }
        for (mut co, _) in self
            .blocking_coroutines
            .drain_filter(|co, yielding| unsafe { !co.as_ref().is_cancelling() && yielding.interrupt(msg) })
        {
            self.running_coroutines.push_back(co);
            let co = unsafe { co.as_mut() };
            co.status = co.status.into_abort();
        }
        self.unblock(false);
    }

    pub fn abort(&mut self, msg: &'static str) {
        loop {
            self.interrupt(msg);
            if self.running_coroutines.is_empty() {
                if !self.blocking_coroutines.is_empty() {
                    // Someone else win session wakeup. Let's snooze.
                    std::hint::spin_loop();
                    continue;
                }
                break;
            }
            while let Some(co) = self.running_coroutines.pop_front() {
                self.run_coroutine(co);
            }
        }
        let status = self.run_coroutine(self.main);
        assert_eq!(status, coroutine::Status::Completed);
        assert_eq!(self.status, Status::Completed);
    }

    // Grab this task to runq. Return false if waker win.
    pub fn grab(&self) -> bool {
        let _locker = self.unblocking_coroutines.lock().unwrap();
        !self.running.replace(true)
    }

    pub fn sched(&mut self) -> SchedFlow {
        let _scope = Scope::enter(self);
        self.running_coroutines.extend(self.yielding_coroutines.drain(..));
        self.unblock(false);
        while !self.yielding && self.status == Status::Running && !self.running_coroutines.is_empty() {
            let co = unsafe { self.running_coroutines.pop_front().unwrap_unchecked() };
            self.run_coroutine(co);
        }
        if self.status == Status::Completing {
            self.abort("task main terminated");
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

    fn block<F: FnOnce()>(&mut self, mut co: ptr::NonNull<Coroutine>, interruption: Interruption<'_, F>) {
        assert!(co == coroutine::current(), "Session.block: running coroutine changed");
        match interruption {
            Interruption::Interruptible(yielding) => {
                let yielding = unsafe { std::mem::transmute::<&dyn Yielding, &'_ dyn Yielding>(yielding) };
                self.blocking_coroutines.insert(co, yielding);
                let co = unsafe { co.as_mut() };
                co.suspend();
            },
            Interruption::Uninterruptible => {
                self.blocking_coroutines.insert(co, &UNINTERRUPTIBLE);
                let co = unsafe { co.as_mut() };
                co.suspend();
            },
            Interruption::Cancellation(cancellation) => {
                self.blocking_coroutines.insert(co, &INTERRUPTIBLE);
                let co = unsafe { co.as_mut() };
                co.suspend();
                if co.status == coroutine::Status::Aborting {
                    co.status = coroutine::Status::Cancelling;
                    std::panic::catch_unwind(AssertUnwindSafe(cancellation)).ignore();
                    co.status = coroutine::Status::Aborting;
                    // Cancellation completed, wakeup caller to check its completion.
                }
            },
        }
    }

    pub fn yield_coroutine(&mut self, mut co: ptr::NonNull<Coroutine>) {
        if self.status != Status::Running {
            return;
        }
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

    pub fn spawn(&mut self, f: impl FnOnce() + 'static, stack_size: StackSize) {
        if self.status != Status::Running {
            return;
        }
        let f = Box::new(f);
        let co = ptr::NonNull::from(Box::leak(Coroutine::new(f, stack_size)));
        self.running_coroutines.push_back(co);
    }
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
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::time::Duration;

        use scopeguard::defer;

        let read = Arc::new(AtomicI32::new(0));
        let write = read.clone();
        let t = task::spawn(|| {
            let cell = Rc::new(Cell::new(0));
            coroutine::spawn({
                let cell = cell.clone();
                move || {
                    defer! {
                        std::thread::sleep(Duration::from_secs(2));
                        write.store(cell.get(), Ordering::Relaxed);
                    }
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
        assert_eq!(read.load(Ordering::Relaxed), 5);
    }
}
