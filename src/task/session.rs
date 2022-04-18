use std::any::Any;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{mem, panic, ptr, thread};

use ignore_result::Ignore;
use num_enum::{IntoPrimitive, UnsafeFromPrimitive};
use static_assertions::{assert_impl_all, assert_not_impl_any};

use crate::coroutine::{self, Coroutine};
use crate::error::PanicError;
use crate::runtime::Scheduler;
use crate::task::{self, Task, Yielding};

#[derive(Copy, Clone)]
struct SessionTask {
    scheduler: ptr::NonNull<Scheduler>,
    task: ptr::NonNull<Task>,
    coroutine: ptr::NonNull<Coroutine>,
}

// Least bit used as release flag.
#[repr(usize)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoPrimitive, UnsafeFromPrimitive)]
enum SessionStatus {
    Empty = 0b0000,
    Value = 0b0010,
    Joining = 0b0100,
    Joined = 0b0110,
}

impl SessionStatus {
    fn into_release(self) -> usize {
        let bits: usize = self.into();
        bits | 0x01
    }

    fn from_bits(bits: usize) -> SessionStatus {
        unsafe { SessionStatus::from_unchecked(bits & !0x01) }
    }
}

enum SessionValue<T> {
    Value(T),
    Panic(PanicError),
}

impl<T> From<SessionValue<T>> for Result<T, PanicError> {
    fn from(value: SessionValue<T>) -> Result<T, PanicError> {
        match value {
            SessionValue::Value(value) => Ok(value),
            SessionValue::Panic(err) => Err(err),
        }
    }
}

impl<T> SessionValue<T> {
    unsafe fn into_value(self) -> T {
        if let SessionValue::Value(value) = self {
            return value;
        }
        std::hint::unreachable_unchecked()
    }

    unsafe fn into_panic(self) -> PanicError {
        if let SessionValue::Panic(err) = self {
            return err;
        }
        std::hint::unreachable_unchecked()
    }
}

#[derive(Copy, Clone)]
enum SessionJoiner {
    Task { task: SessionTask },
    Thread { thread: &'static thread::Thread },
}

union SessionState<T> {
    value: ManuallyDrop<SessionValue<T>>,
    joiner: ManuallyDrop<SessionJoiner>,
}

pub(super) struct SessionJoint<T: Send + 'static> {
    status: AtomicUsize,
    state: UnsafeCell<SessionState<T>>,
    wakers: AtomicUsize,
}

// SAFETY: There are multiple immutable accessors.
unsafe impl<T: Send> Sync for SessionJoint<T> {}

// SAFETY: Normally, multiple immutable accessors are distributed to different tasks or threads.
unsafe impl<T: Send> Send for SessionJoint<T> {}

impl<T: Send + 'static> Yielding for SessionJoint<T> {
    fn interrupt(&self, reason: &'static str) -> bool {
        self.cancel(PanicError::Static(reason)).is_ok()
    }
}

// Safety guard in case session is forgot.
impl<T: Send + 'static> Drop for SessionJoint<T> {
    fn drop(&mut self) {
        self.drop_value();
    }
}

impl<T: Send + 'static> SessionJoint<T> {
    fn new() -> Arc<Self> {
        Arc::new(SessionJoint {
            status: AtomicUsize::new(0),
            state: unsafe { mem::zeroed() },
            wakers: AtomicUsize::new(1),
        })
    }

    #[cfg(test)]
    fn is_ready(&self) -> bool {
        let status = self.status();
        matches!(status, SessionStatus::Value | SessionStatus::Joined)
    }

    fn status(&self) -> SessionStatus {
        let bits = self.status.load(Ordering::Relaxed);
        SessionStatus::from_bits(bits)
    }

    fn drop_value(&self) {
        let mut status = self.status();
        if status == SessionStatus::Empty {
            if let Err(bits) = self.status.compare_exchange(
                0,
                SessionStatus::Joined.into_release(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                status = SessionStatus::from_bits(bits);
            } else {
                return;
            }
        }
        if status == SessionStatus::Joined {
            return;
        }
        while status == SessionStatus::Value {
            let result = self.status.compare_exchange_weak(
                status.into_release(),
                // We owns state after claiming joined.
                SessionStatus::Joined.into_release(),
                // * Acquire load to see released session value
                // * Relaxed store to contend session status but not release session value
                Ordering::Acquire,
                // Same as status load
                // * Loop on Task/Thread
                // * Fail on other status
                Ordering::Relaxed,
            );
            if let Err(bits) = result {
                status = SessionStatus::from_bits(bits);
                // Peer has not release session value
                std::hint::spin_loop();
                continue;
            }
            let cell = unsafe { &mut *self.state.get() };
            unsafe { ManuallyDrop::take(&mut cell.value) };
        }
        // We could be in joining due to task abortion. Nothing to reclaim for joining state.
    }

    fn join_value(&self, joiner: SessionJoiner) -> SessionValue<T> {
        let mut status = self.status();
        if status == SessionStatus::Empty {
            if let Err(bits) =
                self.status.compare_exchange(0, SessionStatus::Joining.into(), Ordering::Relaxed, Ordering::Relaxed)
            {
                status = SessionStatus::from_bits(bits);
            } else {
                let cell = unsafe { &mut *self.state.get() };
                unsafe { ptr::write(&mut cell.joiner, ManuallyDrop::new(joiner)) };
                self.status.store(SessionStatus::Joining.into_release(), Ordering::Release);
                status = self.wait_value(joiner);
            }
        }
        if status == SessionStatus::Value {
            loop {
                let result = self.status.compare_exchange_weak(
                    status.into_release(),
                    // We owns state after claiming joined.
                    SessionStatus::Joined.into_release(),
                    // * Acquire load to see released session value
                    // * Relaxed store to contend session status but not release session value
                    Ordering::Acquire,
                    // Same as status load
                    // * Loop on Task/Thread
                    // * Fail on other status
                    Ordering::Relaxed,
                );
                if let Err(bits) = result {
                    let new_status = SessionStatus::from_bits(bits);
                    if new_status != status {
                        status = new_status;
                        break;
                    }
                    // Peer has not release session value
                    std::hint::spin_loop();
                    continue;
                }
                let cell = unsafe { &mut *self.state.get() };
                let value = unsafe { ManuallyDrop::take(&mut cell.value) };
                return value;
            }
        }
        unreachable!("unexpected session status during joining: {:?}", status)
    }

    fn set_value(&self, value: SessionValue<T>) -> Result<Option<SessionTask>, SessionValue<T>> {
        let mut status = self.status();
        if status == SessionStatus::Empty {
            // success: Ordering::Relaxed this is not the release path
            // failure: same as status load
            match self.status.compare_exchange(0, SessionStatus::Value.into(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    let cell = unsafe { &mut *self.state.get() };
                    unsafe { ptr::write(&mut cell.value, ManuallyDrop::new(value)) };
                    self.status.store(SessionStatus::Value.into_release(), Ordering::Release);
                    return Ok(None);
                },
                Err(bits) => status = SessionStatus::from_bits(bits),
            }
        }
        if status == SessionStatus::Joining {
            loop {
                let r = self.status.compare_exchange_weak(
                    status.into_release(),
                    SessionStatus::Value.into(),
                    // * Acquire load to see released session value
                    // * Relaxed store to contend session status but not release session value
                    Ordering::Acquire,
                    // Same as status load
                    // * Loop on Task/Thread
                    // * Fail on other status
                    Ordering::Relaxed,
                );
                match r {
                    // We win wakeup
                    Ok(_) => break,
                    // Status changed, someone else win wakeup
                    Err(bits) if SessionStatus::from_bits(bits) != status => return Err(value),
                    // Peer has not release session value
                    _ => continue,
                }
            }
            let cell = unsafe { &mut *self.state.get() };
            let joiner = unsafe { ManuallyDrop::take(&mut cell.joiner) };
            unsafe { ptr::write(&mut cell.value, ManuallyDrop::new(value)) };
            match joiner {
                SessionJoiner::Task { task } => {
                    self.status.store(SessionStatus::Value.into_release(), Ordering::Release);
                    Ok(Some(task))
                },
                SessionJoiner::Thread { thread } => {
                    // Unpark before release as park could wake spuriously, detect release flag and
                    // run out execution which will make thread dangling.
                    thread.unpark();
                    self.status.store(SessionStatus::Value.into_release(), Ordering::Release);
                    Ok(None)
                },
            }
        } else {
            Err(value)
        }
    }

    fn wake(&self, value: T) -> Result<(), T> {
        match self.set_value(SessionValue::Value(value)) {
            Err(value) => Err(unsafe { value.into_value() }),
            Ok(task) => {
                Self::wake_task(task);
                Ok(())
            },
        }
    }

    fn cancel(&self, err: PanicError) -> Result<Option<SessionTask>, PanicError> {
        match self.set_value(SessionValue::Panic(err)) {
            Err(value) => Err(unsafe { value.into_panic() }),
            Ok(task) => Ok(task),
        }
    }

    fn fault(&self, err: PanicError) {
        if let Ok(task) = self.cancel(err) {
            Self::wake_task(task);
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

    fn wake_task(task: Option<SessionTask>) {
        if let Some(SessionTask { mut task, scheduler, coroutine }) = task {
            // SAFETY: We have win wakeup contention, task will have to wait us to wake it.
            let task = unsafe { task.as_mut() };
            if task.wake(coroutine) {
                // SAFETY: scheduler lives longer than task
                let scheduler = unsafe { scheduler.as_ref() };
                scheduler.resume(task);
            }
        }
    }

    fn wait_value(&self, joiner: SessionJoiner) -> SessionStatus {
        match joiner {
            SessionJoiner::Task { task } => self.wait_on_task(task.task, task.coroutine),
            SessionJoiner::Thread { .. } => self.wait_on_thread(),
        }
    }

    fn wait_on_task(&self, mut task: ptr::NonNull<Task>, co: ptr::NonNull<Coroutine>) -> SessionStatus {
        let task = unsafe { task.as_mut() };
        task.block(co, self);
        self.status()
    }

    fn wait_on_thread(&self) -> SessionStatus {
        loop {
            thread::park();
            let status = self.status();
            if status == SessionStatus::Joining {
                continue;
            }
            return status;
        }
    }

    fn join_on_task(&self, task: ptr::NonNull<Task>) -> Result<T, PanicError> {
        let scheduler = unsafe { ptr::NonNull::from(Scheduler::current()) };
        let coroutine = coroutine::current();
        let joiner = SessionJoiner::Task { task: SessionTask { scheduler, task, coroutine } };
        let value = self.join_value(joiner);
        value.into()
    }

    fn join_on_thread(&self) -> Result<T, PanicError> {
        let thread = thread::current();
        let joiner =
            SessionJoiner::Thread { thread: unsafe { mem::transmute::<&_, &'static thread::Thread>(&thread) } };
        let value = self.join_value(joiner);
        value.into()
    }

    pub(super) fn join(&self) -> Result<T, PanicError> {
        if let Some(task) = task::task() {
            self.join_on_task(task)
        } else {
            self.join_on_thread()
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

    pub(super) unsafe fn into_joint(self) -> Arc<SessionJoint<T>> {
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
        self.joint.drop_value();
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
    pub(super) fn new(joint: Arc<SessionJoint<T>>) -> SessionWaker<T> {
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

    pub(super) fn set_result(self, result: Result<T, Box<dyn Any + Send + 'static>>) {
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

#[cfg(test)]
mod tests {
    use crate::task;

    #[crate::test(crate = "crate")]
    fn session_waker() {
        let (session, waker) = task::session();
        drop(waker.clone());
        assert_eq!(session.joint.is_ready(), false);
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
}
