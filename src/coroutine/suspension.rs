use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::rc::Rc;
use std::{mem, panic, ptr};

use ignore_result::Ignore;
use static_assertions::assert_not_impl_any;

use super::Coroutine;
use crate::error::{JoinError, PanicError};
use crate::select::{Identifier, Permit, PermitReader, Selectable, Selector};
use crate::task::{self, Yielding};

enum SuspensionState<T: 'static> {
    Empty,
    Value(T),
    Panicked(PanicError),
    Joining(ptr::NonNull<Coroutine>),
    Selector(Selector),
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

    fn is_ready(&self) -> bool {
        let state = unsafe { &*self.state.get() };
        matches!(state, SuspensionState::Value(_) | SuspensionState::Panicked(_))
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
        } else if let SuspensionState::Selector(selector) = state {
            selector.apply(Permit::default());
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
        } else if let SuspensionState::Selector(selector) = state {
            selector.apply(Permit::default());
        }
        Ok(())
    }

    fn set_result(&self, result: Result<T, Box<dyn Any + Send + 'static>>) {
        match result {
            Ok(value) => self.wake(value).ignore(),
            Err(err) => self.fault(PanicError::Unwind(err)),
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        let state = unsafe { &mut *self.state.get() };
        match state {
            SuspensionState::Value(_) | SuspensionState::Panicked(_) | SuspensionState::Joined => {
                selector.apply(Permit::default());
                return true;
            },
            SuspensionState::Joining(_) => unreachable!("suspension: joining state"),
            SuspensionState::Selector(_) => unreachable!("suspension: selecting"),
            SuspensionState::Empty => unsafe { ptr::write(state, SuspensionState::Selector(selector)) },
        }
        false
    }

    fn unwatch_permit(&self, identifer: &Identifier) {
        let state = unsafe { &mut *self.state.get() };
        if let SuspensionState::Selector(selector) = state {
            assert!(selector.identify(identifer), "suspension: selecting by other");
            *state = SuspensionState::Empty;
        }
    }

    fn consume_permit(&self) -> Result<T, PanicError> {
        self.take()
    }

    fn take(&self) -> Result<T, PanicError> {
        let state = mem::replace(unsafe { &mut *self.state.get() }, SuspensionState::Joined);
        match state {
            SuspensionState::Value(value) => Ok(value),
            SuspensionState::Panicked(err) => Err(err),
            SuspensionState::Empty => unreachable!("suspension: empty state"),
            SuspensionState::Joining(_) => unreachable!("suspension: joining state"),
            SuspensionState::Joined => unreachable!("suspension: joined state"),
            SuspensionState::Selector(_) => unreachable!("suspension: selecting"),
        }
    }

    fn join(&self) -> Result<T, PanicError> {
        let co = super::current();
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
            SuspensionState::Selector(_) => unreachable!("suspension: selecting"),
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

    /// Checks readiness.
    pub fn is_ready(&self) -> bool {
        self.0.is_ready()
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
    pub fn resume(self, value: T) -> bool {
        let joint = unsafe { self.into_joint() };
        joint.wake(value).is_ok()
    }

    /// Sends and wakes peer if not waked.
    pub fn send(self, value: T) -> Result<(), T> {
        let joint = unsafe { self.into_joint() };
        joint.wake(value)
    }

    pub(super) fn set_result(self, result: Result<T, Box<dyn Any + Send + 'static>>) {
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
    suspension: Option<Suspension<T>>,
}

assert_not_impl_any!(JoinHandle<()>: Send);

impl<T> JoinHandle<T> {
    pub(super) fn new(suspension: Suspension<T>) -> Self {
        JoinHandle { suspension: Some(suspension) }
    }

    /// Waits for associated coroutine to finish and returns its result.
    ///
    /// # Panics
    /// * Panic if already joined by `select!`.
    /// * Panic if main coroutine finished.
    pub fn join(mut self) -> Result<T, JoinError> {
        if let Some(suspension) = self.suspension.take() {
            let joint = unsafe { suspension.into_joint() };
            joint.join().map_err(JoinError::new)
        } else {
            panic!("already joined by select")
        }
    }
}

impl<T: 'static> Selectable for JoinHandle<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Option<Permit> {
        self.suspension.as_ref().filter(|suspension| suspension.is_ready()).map(|_| Permit::default())
    }

    fn watch_permit(&self, selector: Selector) -> Option<bool> {
        self.suspension.as_ref().map(|suspension| suspension.0.watch_permit(selector))
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        if let Some(suspension) = self.suspension.as_ref() {
            suspension.0.unwatch_permit(identifier);
        }
    }
}

impl<T: 'static> PermitReader for JoinHandle<T> {
    type Result = Result<T, JoinError>;

    fn consume_permit(&mut self, _permit: Permit) -> Result<T, JoinError> {
        if let Some(suspension) = self.suspension.take() {
            let joint = unsafe { suspension.into_joint() };
            joint.consume_permit().map_err(JoinError::new)
        } else {
            panic!("JoinHandle: already consumed")
        }
    }
}

#[cfg(test)]
mod tests {
    use ignore_result::Ignore;

    use crate::{coroutine, select};

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
    fn join_handle_join() {
        let join_handle = coroutine::spawn(|| 5);
        assert_eq!(join_handle.join().unwrap(), 5);
    }

    #[crate::test(crate = "crate")]
    fn join_handle_join_panic() {
        const REASON: &'static str = "oooooops";
        let co = coroutine::spawn(|| panic!("{}", REASON));
        let err = co.join().unwrap_err();
        assert!(err.to_string().contains(REASON))
    }

    #[crate::test(crate = "crate")]
    fn join_handle_select() {
        let mut join_handle = coroutine::spawn(|| 5);
        select! {
            r = <-join_handle => assert_eq!(r.unwrap(), 5),
        }
    }

    #[crate::test(crate = "crate")]
    #[should_panic(expected = "already joined by select")]
    fn join_handle_join_consumed() {
        let mut join_handle = coroutine::spawn(|| 5);
        select! {
            r = <-join_handle => assert_eq!(r.unwrap(), 5),
        }
        join_handle.join().ignore();
    }

    #[crate::test(crate = "crate")]
    #[should_panic(expected = "all select cases disabled with no `default`")]
    fn join_handle_select_consumed() {
        let mut join_handle = coroutine::spawn(|| 5);
        select! {
            r = <-join_handle => assert_eq!(r.unwrap(), 5),
        }
        select! {
            r = <-join_handle => assert_eq!(r.unwrap(), 5),
        }
    }
}
