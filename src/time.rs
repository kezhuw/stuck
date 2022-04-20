use std::mem::{forget, replace, MaybeUninit};
use std::ptr;
use std::time::{Duration, Instant};

use crate::channel::parallel::{Receiver, Sender};
use crate::channel::prelude::*;
use crate::coroutine;
use crate::runtime::Scheduler;
use crate::task::{self, SessionWaker};

const TIME_LEAST_SHIFT: usize = 14;
const TIME_LEAST_VALUE: u64 = 1 << TIME_LEAST_SHIFT;
const TIME_LEAST_MASK: u64 = TIME_LEAST_VALUE - 1;

const TIME_LEVEL_COUNT: usize = 5;
const TIME_LEVEL_SHIFT: usize = 10;
const TIME_LEVEL_VALUE: u64 = 1 << TIME_LEVEL_SHIFT;
const TIME_LEVEL_MASK: u64 = TIME_LEVEL_VALUE - 1;

struct Node {
    next: Option<Box<Node>>,
    expire: u64,
    session: MaybeUninit<SessionWaker<()>>,
}

impl Node {
    fn wake(&mut self) {
        let session = unsafe { ptr::read(self.session.as_ptr()) };
        self.expire = 0;
        session.wake(());
    }
}

impl Default for Node {
    fn default() -> Self {
        Node { next: None, expire: 0, session: MaybeUninit::uninit() }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.expire != 0 {
            unsafe { ptr::read(self.session.as_ptr()) };
        }
    }
}

struct List {
    first: Option<Box<Node>>,
    last: std::ptr::NonNull<Option<Box<Node>>>,
}

impl List {
    fn insert(&mut self, node: Box<Node>) {
        let last = ptr::NonNull::from(&node.next);
        *unsafe { self.last.as_mut() } = Some(node);
        self.last = last;
    }

    fn clear(&mut self) -> Option<Box<Node>> {
        self.last = ptr::NonNull::from(&self.first);
        self.first.take()
    }
}

pub(crate) struct Timer {
    time: u64,
    least: [List; TIME_LEAST_VALUE as usize],
    level: [[List; TIME_LEVEL_VALUE as usize]; TIME_LEVEL_COUNT],
    nodes: Option<Box<Node>>,
}

unsafe impl Send for Timer {}

impl Timer {
    // Expose to construct this struct in native thread to avoid stack overflow in coroutine stack.
    pub fn new() -> Box<Timer> {
        let mut timer: Box<Timer> = Box::new(unsafe { std::mem::zeroed() });
        timer.time = 0;
        for list in timer.least.iter_mut() {
            forget(replace(&mut list.first, None));
            list.last = ptr::NonNull::from(&list.first);
        }
        for level in timer.level.iter_mut() {
            for list in level.iter_mut() {
                forget(replace(&mut list.first, None));
                list.last = ptr::NonNull::from(&list.first);
            }
        }
        forget(replace(&mut timer.nodes, None));
        timer
    }

    fn new_node(&mut self) -> Box<Node> {
        if let Some(mut node) = self.nodes.take() {
            self.nodes = node.next.take();
            return node;
        }
        Box::new(Node::default())
    }

    fn free_node(&mut self, mut node: Box<Node>) {
        node.next = self.nodes.take();
        self.nodes = Some(node);
    }

    fn wake(&mut self, mut list: Option<Box<Node>>) {
        while let Some(mut node) = list {
            list = node.next.take();
            node.wake();
            self.free_node(node);
        }
    }

    fn tick(&mut self) {
        self.time += 1;
        if self.time & TIME_LEAST_MASK == 0 {
            let mut time = self.time;
            time >>= TIME_LEAST_SHIFT;
            let mut level = 0;
            loop {
                let value = time & TIME_LEVEL_MASK;
                if value != 0 {
                    let index = (value - 1) as usize;
                    let list = self.level[level][index].clear();
                    self.queue_list(list);
                    break;
                }
                time >>= TIME_LEVEL_SHIFT;
                level += 1;
                assert!(level <= TIME_LEVEL_COUNT);
            }
        }
        let index = (self.time & TIME_LEAST_MASK) as usize;
        let list = self.least[index].clear();
        self.wake(list);
    }

    fn update(&mut self, time: u64) {
        while self.time < time {
            self.tick();
        }
    }

    fn queue_list(&mut self, mut list: Option<Box<Node>>) {
        while let Some(mut node) = list {
            list = node.next.take();
            self.queue_node(node);
        }
    }

    fn queue_node(&mut self, node: Box<Node>) {
        let time = self.time;
        let expire = node.expire;
        if expire - time < TIME_LEAST_VALUE {
            let index = (node.expire & TIME_LEAST_MASK) as usize;
            self.least[index].insert(node);
            return;
        }
        let mut level = 0;
        let mut exp2 = 1 << TIME_LEAST_SHIFT;
        loop {
            exp2 <<= TIME_LEVEL_SHIFT;
            let mask = exp2 - 1;
            if (expire | mask) == (time | mask) {
                let shift = TIME_LEAST_SHIFT + level * TIME_LEVEL_SHIFT;
                let value = (expire >> shift) & TIME_LEVEL_MASK;
                let index = (value - 1) as usize;
                self.level[level][index].insert(node);
                break;
            }
            level += 1;
            assert!(level <= TIME_LEVEL_COUNT);
        }
    }

    fn timeout(&mut self, timeout: u64, session: SessionWaker<()>) {
        let mut node = self.new_node();
        node.expire = self.time + timeout as u64;
        node.session.write(session);
        self.queue_node(node);
    }
}

pub(crate) enum Message {
    Timeout { timeout: u64, session: SessionWaker<()> },
    UpdateTime { time: u64 },
    Stop,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Timeout { timeout, .. } => write!(f, "Message::Timeout({}ms)", timeout),
            Message::UpdateTime { time } => write!(f, "Message::UpdateTime{{time: {}}}", time),
            Message::Stop => f.write_str("Message::Stop"),
        }
    }
}

static mut RAND: u64 = 0;

/// Random but not well distributed integer.
pub(crate) unsafe fn rand() -> u64 {
    RAND
}

fn init_rand(now: Instant) {
    let zero: Instant = unsafe { std::mem::zeroed() };
    let rand = now.saturating_duration_since(zero).as_millis() as u64;
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    unsafe { RAND = rand.wrapping_add(epoch).wrapping_add(RAND) };
}

pub(crate) fn tickr(mut sender: Sender<Message>) {
    let mut time = 0;
    let start = Instant::now();
    init_rand(start);
    loop {
        std::thread::sleep(Duration::from_millis(1));
        let elapsed = start.elapsed().as_millis() as u64;
        if elapsed > time {
            time = elapsed;
            unsafe { RAND = time.wrapping_add(RAND) };
            if sender.send(Message::UpdateTime { time }).is_err() {
                break;
            }
        }
    }
}

pub(crate) fn timer(mut timer: Box<Timer>, mut receiver: Receiver<Message>) {
    while let Some(message) = receiver.recv() {
        match message {
            // Plus one minimum resolution to avoid earlier wakeup due to partially elapsed tick.
            Message::Timeout { timeout, session } => timer.timeout(timeout + 1, session),
            Message::UpdateTime { time } => timer.update(time),
            Message::Stop => receiver.close(),
        }
    }
}

/// Sleeps for at least given duration.
pub fn sleep(timeout: Duration) {
    let millis = timeout.as_millis() as u64;
    if millis == 0 {
        coroutine::yield_now();
        return;
    }
    let (session, waker) = task::session();
    let mut sender = Scheduler::try_time_sender().expect("no runtime");
    sender.send(Message::Timeout { timeout: millis, session: waker }).expect("runtime stopping");
    session.wait();
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use test_case::test_case;

    use super::*;
    use crate::runtime::Runtime;
    use crate::{task, time};

    #[test_case(0, 1)]
    #[test_case(0, 2)]
    #[test_case(1111, 1)]
    #[test_case(1111, 2)]
    #[test_case(0, 22)]
    #[test_case(22, 222)]
    #[test_case(0, TIME_LEAST_MASK-11)]
    #[test_case(22, TIME_LEAST_MASK-11)]
    #[test_case(0, TIME_LEAST_MASK)]
    #[test_case(0, TIME_LEAST_VALUE)]
    #[test_case(111, TIME_LEAST_MASK)]
    #[test_case(111, TIME_LEAST_VALUE)]
    #[test_case(111, TIME_LEAST_MASK*2 + 333)]
    #[test_case(111, TIME_LEAST_MASK*3 + 333)]
    #[test_case(111, TIME_LEAST_MASK*4 + 333)]
    #[test_case(TIME_LEAST_VALUE*TIME_LEVEL_VALUE, TIME_LEAST_VALUE)]
    fn sleep(time: u64, timeout: u64) {
        let mut timer = Timer::new();
        timer.update(time);
        let (session, waker) = task::session();
        timer.timeout(timeout, waker);
        timer.update(time + timeout);
        session.wait();
    }

    struct SendableSession<T: Send + 'static>(task::Session<T>);
    unsafe impl<T: Send> Send for SendableSession<T> {}
    unsafe impl<T: Send> Sync for SendableSession<T> {}

    #[test_case(0, 1)]
    #[test_case(0, 2)]
    #[test_case(1111, 1)]
    #[test_case(1111, 2)]
    #[test_case(0, 22)]
    #[test_case(22, 222)]
    #[test_case(0, TIME_LEAST_MASK-11)]
    #[test_case(22, TIME_LEAST_MASK-11)]
    #[test_case(0, TIME_LEAST_MASK)]
    #[test_case(0, TIME_LEAST_VALUE)]
    #[test_case(111, TIME_LEAST_MASK)]
    #[test_case(111, TIME_LEAST_VALUE)]
    #[test_case(111, TIME_LEAST_MASK*2 + 333)]
    #[test_case(TIME_LEAST_VALUE*TIME_LEVEL_VALUE, TIME_LEAST_VALUE)]
    fn sleep_blocking(time: u64, timeout: u64) {
        let mut timer = Timer::new();
        timer.update(time);
        let (session, waker) = task::session();
        timer.timeout(timeout, waker);
        timer.update(time + timeout - 1);
        let now = Instant::now();
        let session = Box::new(SendableSession(session));
        let join_handle = thread::spawn(move || {
            session.0.wait();
            now.elapsed()
        });
        thread::sleep(Duration::from_secs(5));
        timer.update(time + timeout + 1);
        assert!(join_handle.join().unwrap() >= Duration::from_secs(5));
    }

    #[test]
    fn runtime_sleep() {
        let mut runtime = Runtime::new();
        let now = Instant::now();
        let sleep = runtime.spawn(|| {
            time::sleep(Duration::from_secs(6));
        });
        sleep.join().unwrap();
        assert!(now.elapsed() >= Duration::from_secs(5));
    }

    #[test]
    fn runtime_sleep_zero() {
        let mut runtime = Runtime::new();
        let sleep = runtime.spawn(|| {
            time::sleep(Duration::ZERO);
        });
        sleep.join().unwrap();
    }
}
