use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use std::{ptr, thread};

use ignore_result::Ignore;

use crate::task::mpsc::{self, Sender};
use crate::task::{self, SchedFlow, Task};
use crate::{net, time};

thread_local! {
    static SCHEDULER: Cell<Option<ptr::NonNull<Scheduler>>> = Cell::new(None);
}

const STOP_MSG: &str = "runtime stopped";

struct TaskPointer(ptr::NonNull<Task>);

unsafe impl Send for TaskPointer {}

impl TaskPointer {
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

/// Builder for [Runtime].
#[derive(Default)]
pub struct Builder {
    parallelism: Option<usize>,
}

impl Builder {
    /// Specifies the number of parallel threads for scheduling.
    pub fn parallelism(&mut self, n: usize) -> &mut Self {
        assert!(n > 0, "parallelism must not be zero");
        self.parallelism = Some(n);
        self
    }

    /// Constructs an [Runtime] to spawn and schedule tasks.
    pub fn build(&mut self) -> Runtime {
        let parallelism = self
            .parallelism
            .unwrap_or_else(|| thread::available_parallelism().unwrap_or(NonZeroUsize::new(4).unwrap()).get());
        let (time_sender, time_receiver) = mpsc::unbounded(512);
        let poller = net::Poller::new().unwrap();
        let scheduler = Scheduler::new(parallelism, time_sender.clone(), poller.registry());
        let stopper = poller.start().unwrap();
        let timer = time::Timer::new();
        let timer = task::Builder::with_scheduler(&scheduler).spawn(move || {
            time::timer(timer, time_receiver);
        });
        let ticker = thread::spawn(move || {
            time::tick(time_sender);
        });
        let scheduling_threads = Scheduler::start(&scheduler);
        Runtime {
            scheduler,
            timer: MaybeUninit::new(timer),
            ticker: MaybeUninit::new(ticker),
            stopper: MaybeUninit::new(stopper),
            scheduling_threads,
        }
    }
}

/// Runtime encapsulates io selecter, timer and task scheduler to serve spawned tasks.
///
/// [Runtime::drop] will stop and join all serving threads.
pub struct Runtime {
    scheduler: Arc<Scheduler>,
    timer: MaybeUninit<task::JoinHandle<()>>,
    ticker: MaybeUninit<thread::JoinHandle<()>>,
    stopper: MaybeUninit<net::Stopper>,
    scheduling_threads: Vec<thread::JoinHandle<()>>,
}

impl Runtime {
    /// Constructs an runtime to serve spawned tasks.
    pub fn new() -> Runtime {
        Builder::default().build()
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
        let timer = unsafe { ptr::read(self.timer.as_ptr()) };
        let ticker = unsafe { ptr::read(self.ticker.as_ptr()) };
        let mut stopper = unsafe { ptr::read(self.stopper.as_ptr()) };
        timer.join().ignore();
        ticker.join().ignore();
        stopper.stop();
        self.scheduler.stop();
        for handle in self.scheduling_threads.drain(..) {
            handle.join().ignore();
        }
    }
}

struct SchedulerState {
    runq: VecDeque<TaskPointer>,
    registry: HashMap<u64, Arc<Task>>,

    // -1: running
    //  0: start stopping
    // +n: n stopped threads
    stopped: isize,
}

impl SchedulerState {
    fn new() -> Self {
        SchedulerState { runq: VecDeque::with_capacity(256), registry: HashMap::with_capacity(256), stopped: -1 }
    }
}

pub(crate) struct Scheduler {
    parallelism: usize,
    timer: Sender<time::Message>,
    state: Mutex<SchedulerState>,
    waker: Condvar,
    registry: Arc<net::Registry>,
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

impl Scheduler {
    fn new(parallelism: usize, timer: Sender<time::Message>, registry: Arc<net::Registry>) -> Arc<Scheduler> {
        Arc::new(Scheduler {
            parallelism,
            timer,
            state: Mutex::new(SchedulerState::new()),
            waker: Condvar::new(),
            registry,
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

    /// This method is designed to be called twice. One for stop signal and one after all attendant
    /// threads stopped.
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped += 1;
        self.waker.notify_all();
    }

    pub unsafe fn registry<'a>() -> &'a net::Registry {
        &Self::current().registry
    }

    pub(crate) unsafe fn current<'a>() -> &'a Scheduler {
        SCHEDULER.with(|s| s.get().unwrap_unchecked().as_ref())
    }

    pub(crate) fn try_current<'a>() -> Option<&'a Scheduler> {
        SCHEDULER.with(|s| s.get().map(|s| unsafe { s.as_ref() }))
    }

    pub(crate) fn try_time_sender() -> Option<Sender<time::Message>> {
        Self::try_current().map(|s| s.timer.clone())
    }

    pub fn sched(&self, t: Arc<Task>) {
        let mut state = self.state.lock().unwrap();
        let id = t.id();
        let pointer = TaskPointer::from(&t);
        state.registry.insert(id, t);
        state.runq.push_back(pointer);
        self.waker.notify_one();
    }

    pub(crate) fn resume(&self, t: &Task) {
        let mut state = self.state.lock().unwrap();
        state.runq.push_back(TaskPointer::from(t));
        self.waker.notify_one();
    }

    fn run<'a>(&'a self, mut state: MutexGuard<'a, SchedulerState>) -> MutexGuard<'a, SchedulerState> {
        if let Some(mut task) = state.runq.pop_front() {
            drop(state);
            let task = unsafe { task.0.as_mut() };
            let flow = task.sched();
            let id = task.id();
            state = self.state.lock().unwrap();
            match flow {
                SchedFlow::Yield => state.runq.push_back(TaskPointer::from(task)),
                SchedFlow::Block => {},
                SchedFlow::Cease => {
                    state.registry.remove(&id);
                },
            }
            state
        } else {
            self.waker.wait(state).unwrap()
        }
    }

    fn serve(&self) {
        let _scope = Scope::enter(self);
        let mut state = self.state.lock().unwrap();
        while state.stopped < 0 {
            state = self.run(state)
        }
        let stopped = state.stopped + 1;
        state.stopped = stopped;
        if stopped as usize != self.parallelism {
            return;
        }
        // This is the last scheduling thread.
        drop(state);
        self.timer.clone().send(time::Message::Stop).ignore();
        state = self.state.lock().unwrap();
        while state.stopped == self.parallelism as isize {
            state = self.run(state)
        }
        while !state.registry.is_empty() {
            // SAFETY: Avoid compilation warning in read to `registry` and write to `runq`.
            let registry: &HashMap<u64, Arc<Task>> = unsafe { std::mem::transmute::<_, _>(&state.registry) };
            registry.values().filter(|t| t.grab()).map(|t| TaskPointer::from(t)).for_each(|t| state.runq.push_back(t));
            while let Some(mut task) = state.runq.pop_front() {
                drop(state);
                let task = unsafe { task.0.as_mut() };
                let id = task.id();
                task.abort(STOP_MSG);
                state = self.state.lock().unwrap();
                state.registry.remove(&id);
            }
            drop(state);
            // Sleep to let waker resume task after winning Task::grab(eg. `running` state).
            std::thread::sleep(Duration::from_millis(500));
            state = self.state.lock().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    thread_local! {
        static LOCAL_SECRET: Cell<usize> = Cell::new(0);
    }

    #[test]
    #[should_panic]
    fn runtime_builder_parallelism_zero() {
        Builder::default().parallelism(0).build();
    }

    #[test]
    fn runtime_builder_parallelism_one() {
        let runtime = Builder::default().parallelism(1).build();
        let secret = 333;
        let set_secret = runtime.spawn(move || {
            thread::sleep(Duration::from_secs(10));
            LOCAL_SECRET.with(|cell| cell.set(secret));
        });
        let get_secret = runtime.spawn(move || LOCAL_SECRET.with(|cell| cell.get()));
        set_secret.join().unwrap();
        assert_eq!(secret, get_secret.join().unwrap());
    }

    #[test]
    fn runtime_builder_parallelism_multiple() {
        let runtime = Builder::default().parallelism(2).build();
        let secret = 111;
        let set_secret = runtime.spawn(move || {
            thread::sleep(Duration::from_secs(10));
            LOCAL_SECRET.with(|cell| cell.set(secret));
        });
        let get_secret = runtime.spawn(move || LOCAL_SECRET.with(|cell| cell.get()));
        set_secret.join().unwrap();
        assert_ne!(secret, get_secret.join().unwrap());
    }
}
