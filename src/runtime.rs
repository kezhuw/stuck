use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::{ptr, thread};

use ignore_result::Ignore;

use crate::task::{self, Task};

thread_local! {
    static SCHEDULER: Cell<Option<ptr::NonNull<Scheduler>>> = Cell::new(None);
}

struct TaskPointer(ptr::NonNull<Task>);

unsafe impl Send for TaskPointer {}

impl From<&Task> for TaskPointer {
    fn from(task: &Task) -> TaskPointer {
        TaskPointer(ptr::NonNull::from(task))
    }
}

struct Scope {}

impl Scope {
    fn enter(scheduler: &Scheduler) {
        SCHEDULER.with(|cell| {
            assert!(cell.get().is_none(), "runtime scheduler existed");
            cell.set(Some(ptr::NonNull::from(scheduler)));
        });
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        SCHEDULER.with(|cell| {
            assert!(cell.get().is_some(), "runtime scheduler does not exist");
            cell.set(None);
        });
    }
}

/// Runtime encapsulates io selecter, timer and task scheduler to serve spawned tasks.
///
/// [Runtime::drop] will stop and join all serving threads.
pub struct Runtime {
    scheduler: Arc<Scheduler>,
    scheduling_threads: Vec<thread::JoinHandle<()>>,
}

impl Runtime {
    /// Constructs an runtime to serve spawned tasks.
    pub fn new() -> Runtime {
        let scheduler = Scheduler::new();
        let scheduling_threads = Scheduler::start(&scheduler);
        Runtime { scheduler, scheduling_threads }
    }

    /// Constructs a task builder to spawn task.
    pub fn builder(&self) -> task::Builder<'_> {
        task::Builder::with_scheduler(&self.scheduler)
    }

    /// Spawns a concurrent task and returns a [task::JoinHandle] for it.
    ///
    /// See [task::spawn] for more details
    pub fn spawn<F, T>(&self, f: F) -> task::JoinHandle<T>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        task::Builder::with_scheduler(&self.scheduler).spawn(f)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.scheduler.stop();
        for handle in self.scheduling_threads.drain(..) {
            handle.join().ignore();
        }
    }
}

pub(crate) struct Scheduler {
    parallelism: usize,
    tasks: Mutex<HashMap<u64, Arc<Task>>>,
    runq: Mutex<VecDeque<TaskPointer>>,
    stopped: Cell<bool>,
    waker: Condvar,
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

impl Scheduler {
    fn new() -> Arc<Scheduler> {
        let parallelism = thread::available_parallelism().unwrap_or(NonZeroUsize::new(4).unwrap()).get();
        Arc::new(Scheduler {
            parallelism,
            tasks: Mutex::new(HashMap::new()),
            runq: Mutex::new(VecDeque::new()),
            waker: Condvar::new(),
            stopped: Cell::new(false),
        })
    }

    /// Starts threads to serve spawned tasks.
    fn start(self: &Arc<Scheduler>) -> Vec<thread::JoinHandle<()>> {
        let parallelism = self.parallelism;
        (0..parallelism)
            .map(move |_| {
                let scheduler = self.clone();
                thread::spawn(move || scheduler.serve())
            })
            .collect()
    }

    fn stop(&self) {
        let locker = self.runq.lock().unwrap();
        self.stopped.set(true);
        self.waker.notify_all();
        drop(locker);
    }

    pub(crate) unsafe fn current<'a>() -> &'a Scheduler {
        SCHEDULER.with(|s| s.get().unwrap_unchecked().as_ref())
    }

    pub(crate) fn try_current<'a>() -> Option<&'a Scheduler> {
        SCHEDULER.with(|s| s.get().map(|s| unsafe { s.as_ref() }))
    }

    pub fn sched(&self, t: Arc<Task>) {
        let t = self.register(t);
        self.resume(unsafe { t.as_ref() });
    }

    pub(crate) fn resume(&self, t: &Task) {
        let mut tasks = self.runq.lock().unwrap();
        tasks.push_back(TaskPointer::from(t));
        drop(tasks);
        self.waker.notify_one();
    }

    pub(crate) fn retire(&self, t: &Task) {
        let id = t.id();
        let mut tasks = self.tasks.lock().unwrap();
        tasks.remove(&id);
    }

    fn register(&self, task: Arc<Task>) -> ptr::NonNull<Task> {
        let id = task.id();
        let p = unsafe { ptr::NonNull::new_unchecked(Arc::as_ptr(&task) as *mut Task) };
        let mut map = self.tasks.lock().unwrap();
        map.insert(id, task);
        p
    }

    fn serve(&self) {
        let _scope = Scope::enter(self);
        let mut tasks = self.runq.lock().unwrap();
        while !self.stopped.get() {
            if let Some(mut task) = tasks.pop_front() {
                drop(tasks);
                let task = unsafe { task.0.as_mut() };
                Task::run(task);
                tasks = self.runq.lock().unwrap();
            } else {
                tasks = self.waker.wait(tasks).unwrap();
            }
        }
    }
}

pub(crate) fn resume(t: &Task) {
    let scheduler = unsafe { Scheduler::current() };
    scheduler.resume(t);
}

pub(crate) fn retire(t: &Task) {
    let scheduler = unsafe { Scheduler::current() };
    scheduler.retire(t);
}
