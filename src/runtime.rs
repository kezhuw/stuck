use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::{ptr, thread};

use lazy_static::lazy_static;

use crate::task::Task;

lazy_static! {
    static ref REGISTRY: Mutex<HashMap<u64, Arc<Task>>> = Mutex::new(HashMap::new());
    static ref SCHEDULER: Scheduler = Scheduler::new();
}

struct TaskPointer(ptr::NonNull<Task>);

unsafe impl Send for TaskPointer {}

impl From<&Task> for TaskPointer {
    fn from(task: &Task) -> TaskPointer {
        TaskPointer(ptr::NonNull::from(task))
    }
}

struct Scheduler {
    runq: Mutex<VecDeque<TaskPointer>>,
    not_empty: Condvar,
}

impl Scheduler {
    fn new() -> Scheduler {
        Scheduler { runq: Mutex::new(VecDeque::new()), not_empty: Condvar::new() }
    }

    fn serve(&self) {
        loop {
            let mut tasks = self.runq.lock().unwrap();
            if let Some(mut task) = tasks.pop_front() {
                drop(tasks);
                let task = unsafe { task.0.as_mut() };
                Task::run(task);
            }
        }
    }

    fn resume(&self, task: &Task) {
        let mut tasks = self.runq.lock().unwrap();
        tasks.push_back(TaskPointer::from(task));
        drop(tasks);
        self.not_empty.notify_one();
    }
}

fn register(task: Arc<Task>) -> ptr::NonNull<Task> {
    let id = task.id();
    let p = unsafe { ptr::NonNull::new_unchecked(Arc::as_ptr(&task) as *mut Task) };
    let mut map = REGISTRY.lock().unwrap();
    map.insert(id, task);
    p
}

/// Starts runtime and serves spawned tasks.
pub fn serve() -> ! {
    let scheduler = &*SCHEDULER;
    let parallelism = thread::available_parallelism().unwrap_or(NonZeroUsize::new(4).unwrap());
    // We need to collect before blocking Thread::join.
    #[allow(clippy::needless_collect)]
    let threads: Vec<_> = (0..parallelism.get() - 1).map(|_| thread::spawn(|| scheduler.serve())).collect();
    scheduler.serve();
    for td in threads.into_iter() {
        td.join().unwrap();
    }
    unreachable!("runtime::serve stopped")
}

pub(crate) fn sched(t: Arc<Task>) {
    let t = register(t);
    SCHEDULER.resume(unsafe { t.as_ref() });
}

pub(crate) fn resume(t: &Task) {
    SCHEDULER.resume(t);
}

pub(crate) fn retire(t: &Task) {
    let id = t.id();
    let mut registry = REGISTRY.lock().unwrap();
    registry.remove(&id);
}
