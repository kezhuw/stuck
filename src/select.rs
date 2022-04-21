//! Selectively read and write values to/from multiple selectables simultaneously.

use crate::coroutine::{self, Resumption, Suspension};
use crate::task::{self, Session, SessionWaker};

/// Permit promises to [Select] that read or write will not block current execution.
#[derive(Debug, PartialEq, Eq)]
pub struct Permit {
    primitive: usize,
    attendant: Option<usize>,
}

impl Permit {
    /// Constructs a permit with primitive value.
    pub const fn with_primitive(primitive: usize) -> Self {
        Permit { primitive, attendant: None }
    }

    /// Turns this permit to primitive value.
    pub fn into_primitive(self) -> usize {
        assert!(self.attendant.is_none(), "expect pritmive Permit");
        self.primitive
    }

    /// Constructs a permit with compound values.
    pub fn with_compound(primitive: usize, attendant: usize) -> Self {
        Permit { primitive, attendant: Some(attendant) }
    }

    /// Turns this permit to compound values.
    pub fn into_compound(self) -> (usize, usize) {
        let attendant = self.attendant.expect("expect compound Permit");
        (self.primitive, attendant)
    }
}

impl Default for Permit {
    fn default() -> Self {
        Permit::with_primitive(0)
    }
}

#[derive(Clone)]
enum Waker {
    Task(SessionWaker<(usize, Permit)>),
    Coroutine(Resumption<(usize, Permit)>),
}

impl Waker {
    fn new(parallel: bool) -> (Waiter, Waker) {
        if parallel {
            let (session, waker) = task::session();
            (Waiter::Task(session), Waker::Task(waker))
        } else {
            let (suspension, resumption) = coroutine::suspension();
            (Waiter::Coroutine(suspension), Waker::Coroutine(resumption))
        }
    }

    fn wake(self, index: usize, permit: Permit) -> bool {
        match self {
            Waker::Task(session) => session.wake((index, permit)),
            Waker::Coroutine(suspension) => suspension.resume((index, permit)),
        }
    }
}

enum Waiter {
    Task(Session<(usize, Permit)>),
    Coroutine(Suspension<(usize, Permit)>),
}

impl Waiter {
    fn wait(self) -> (usize, Permit) {
        match self {
            Waiter::Task(session) => session.wait(),
            Waiter::Coroutine(suspension) => suspension.suspend(),
        }
    }

    fn is_ready(&self) -> bool {
        match self {
            Waiter::Task(session) => session.is_ready(),
            Waiter::Coroutine(suspension) => suspension.is_ready(),
        }
    }
}

struct Witness<'a> {
    count: usize,
    identifier: Identifier,
    selectables: Enumerator<'a>,
}

impl<'a> Witness<'a> {
    fn new(count: usize, identifier: Identifier, selectables: Enumerator<'a>) -> Self {
        Witness { count, identifier, selectables }
    }
}

impl Drop for Witness<'_> {
    fn drop(&mut self) {
        for (_, selectable) in self.selectables.clone().take(self.count) {
            if let Some(selectable) = selectable {
                selectable.unwatch_permit(&self.identifier);
            }
        }
    }
}

struct WitnessWaiter<'a> {
    waiter: Waiter,
    _witness: Witness<'a>,
}

impl<'a> WitnessWaiter<'a> {
    fn new(count: usize, waiter: Waiter, identifier: Identifier, selectables: Enumerator<'a>) -> Self {
        Self { waiter, _witness: Witness::new(count, identifier, selectables) }
    }

    fn wait(self) -> (usize, Permit) {
        self.waiter.wait()
    }
}

/// [Selector] identifier.
pub struct Identifier {
    raw: *const (),
}

impl Identifier {
    fn new(raw: *const ()) -> Self {
        Self { raw }
    }

    fn equals(&self, other: &Identifier) -> bool {
        self.raw == other.raw
    }

    fn copy(&self) -> Self {
        Self { raw: self.raw }
    }
}

/// Selector waits permit application from [Selectable].
pub struct Selector {
    index: usize,
    waker: Waker,
    identifier: Identifier,
}

impl Selector {
    fn new(index: usize, waker: Waker, identifier: Identifier) -> Self {
        Selector { index, waker, identifier }
    }

    /// Applies a permit if no one applied before.
    pub fn apply(self, permit: Permit) -> bool {
        self.waker.wake(self.index, permit)
    }

    /// Identifies this selector as given identifier.
    pub fn identify(&self, identifier: &Identifier) -> bool {
        self.identifier.equals(identifier)
    }
}

/// Enumerate selectables in predictable but not sequential order.
#[derive(Clone)]
struct Enumerator<'a> {
    next: usize,
    end: usize,
    selectables: &'a [Option<&'a dyn Selectable>],
}

impl<'a> Enumerator<'a> {
    fn new(selectables: &'a [Option<&'a dyn Selectable>]) -> Self {
        let start = unsafe { crate::time::rand() as usize } % selectables.len();
        Enumerator { next: start, end: start + selectables.len(), selectables }
    }
}

impl<'a> Iterator for Enumerator<'a> {
    type Item = (usize, Option<&'a dyn Selectable>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            None
        } else {
            let index = self.next % self.selectables.len();
            self.next += 1;
            Some((index, self.selectables[index]))
        }
    }
}

/// Error for [Select::try_select] and [Selectable::select_permit].
pub enum TrySelectError {
    /// No avaiable permit now.
    WouldBlock,

    /// All permits exhausted.
    Completed,
}

/// Select candidate to read and/or write value in blocking or nonblocking.
pub trait Select<'a> {
    /// Returns all selectable candidates.
    fn selectables(&'a self) -> &[Option<&'a dyn Selectable>];

    /// Tries to select a permit for consumption.
    ///
    /// # Safety
    /// The returned permit must be consumed otherwise it leaked.
    unsafe fn try_select(&'a self) -> Result<(usize, Permit), TrySelectError> {
        let mut err = TrySelectError::Completed;
        for (index, selectable) in Enumerator::new(self.selectables()) {
            if let Some(selectable) = selectable {
                match selectable.select_permit() {
                    Ok(permit) => return Ok((index, permit)),
                    Err(TrySelectError::WouldBlock) => err = TrySelectError::WouldBlock,
                    Err(TrySelectError::Completed) => {},
                }
            }
        }
        Err(err)
    }

    /// Selects a permit for consumption.
    ///
    /// # Safety
    /// The returned permit must be consumed otherwise it leaked.
    unsafe fn select(&'a self) -> Option<(usize, Permit)> {
        match self.try_select() {
            Ok(selection) => return Some(selection),
            Err(TrySelectError::Completed) => return None,
            Err(TrySelectError::WouldBlock) => {},
        }
        let selectables = self.selectables();
        let parallel = selectables.iter().flatten().any(|s| s.parallel());
        let identifier = Identifier::new(&parallel as *const bool as *const ());
        let (waiter, waker) = Waker::new(parallel);
        let mut checked = 0;
        let enumerator = Enumerator::new(selectables);
        for (index, selectable) in enumerator.clone() {
            if let Some(selectable) = selectable {
                let selector = Selector::new(index, waker.clone(), identifier.copy());
                if selectable.watch_permit(selector) && waiter.is_ready() {
                    break;
                }
            }
            checked += 1;
        }
        drop(waker);
        let waiter = WitnessWaiter::new(checked, waiter, identifier, enumerator);
        Some(waiter.wait())
    }
}

impl<'a> Select<'a> for [Option<&'a dyn Selectable>] {
    fn selectables(&self) -> &[Option<&'a dyn Selectable>] {
        self
    }
}

/// [Select] candidate.
pub trait Selectable {
    /// Returns true if permit watching will contend with possible parallel executions.
    fn parallel(&self) -> bool;

    /// Select for avaiable permit.
    fn select_permit(&self) -> Result<Permit, TrySelectError>;

    /// Watches for available permit.
    ///
    /// If permit is avaiable now, applies it and returns true.
    fn watch_permit(&self, selector: Selector) -> bool;

    /// Unwatches possible applied selector.
    fn unwatch_permit(&self, identifier: &Identifier);
}

/// Writer that writes value with previously selected permit.
pub trait PermitWriter: Selectable {
    type Item;
    type Result;

    /// Consumes permit and writes given value. This operation must not block.
    fn consume_permit(&mut self, permit: Permit, value: Self::Item) -> Self::Result;
}

/// Reader that read value with previously selected permit.
pub trait PermitReader: Selectable {
    type Result;

    /// Consumes permit and reads value. This operation must not block.
    fn consume_permit(&mut self, permit: Permit) -> Self::Result;
}

impl<T> Selectable for &T
where
    T: Selectable,
{
    fn parallel(&self) -> bool {
        (**self).parallel()
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        (**self).select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        (**self).watch_permit(selector)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        (**self).unwatch_permit(identifier)
    }
}

impl<T> Selectable for &mut T
where
    T: Selectable,
{
    fn parallel(&self) -> bool {
        (**self).parallel()
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        (**self).select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        (**self).watch_permit(selector)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        (**self).unwatch_permit(identifier)
    }
}

/// [Selectable] for [never()].
#[non_exhaustive]
pub struct Never;

/// Constructs a selectable that is never ready to consume.
pub fn never() -> Never {
    Never
}

impl PermitReader for Never {
    type Result = std::convert::Infallible;

    fn consume_permit(&mut self, _permit: Permit) -> Self::Result {
        unreachable!("should never be consumed")
    }
}

impl Selectable for Never {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        Err(TrySelectError::Completed)
    }

    fn watch_permit(&self, _selector: Selector) -> bool {
        false
    }

    fn unwatch_permit(&self, _identifier: &Identifier) {}
}

/// [Selectable] for [ready].
pub struct Ready<T> {
    value: Option<T>,
}

/// Constructs a selectable that is immediately ready with given value.
pub fn ready<T>(value: T) -> Ready<T> {
    Ready { value: Some(value) }
}

impl<T> PermitReader for Ready<T> {
    type Result = T;

    fn consume_permit(&mut self, _permit: Permit) -> Self::Result {
        self.value.take().expect("value already consumed")
    }
}

impl<T> Selectable for Ready<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        match self.value.as_ref() {
            None => Err(TrySelectError::Completed),
            Some(_) => Ok(Permit::default()),
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        if self.value.is_some() {
            selector.apply(Permit::default());
            true
        } else {
            false
        }
    }

    fn unwatch_permit(&self, _identifier: &Identifier) {}
}

impl<T> Selectable for std::collections::VecDeque<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        if self.is_empty() {
            Err(TrySelectError::Completed)
        } else {
            Ok(Permit::default())
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        if self.is_empty() {
            false
        } else {
            selector.apply(Permit::default());
            true
        }
    }

    fn unwatch_permit(&self, _identifier: &Identifier) {}
}

impl<T> PermitReader for std::collections::VecDeque<T> {
    type Result = T;

    fn consume_permit(&mut self, _permit: Permit) -> Self::Result {
        self.pop_front().expect("all values consumed")
    }
}

impl<T> Selectable for Option<T> {
    fn parallel(&self) -> bool {
        false
    }

    fn select_permit(&self) -> Result<Permit, TrySelectError> {
        if self.is_none() {
            Err(TrySelectError::Completed)
        } else {
            Ok(Permit::default())
        }
    }

    fn watch_permit(&self, selector: Selector) -> bool {
        if self.is_none() {
            false
        } else {
            selector.apply(Permit::default());
            true
        }
    }

    fn unwatch_permit(&self, _identifier: &Identifier) {}
}

impl<T> PermitReader for Option<T> {
    type Result = T;

    fn consume_permit(&mut self, _permit: Permit) -> Self::Result {
        self.take().expect("all values consumed")
    }
}

#[cfg(test)]
mod tests {
    use ignore_result::Ignore;

    use crate::channel::prelude::*;
    use crate::channel::{parallel, serial};
    use crate::{coroutine, select, task};

    #[crate::test(crate = "crate")]
    fn never() {
        let mut never = select::never();
        select! {
            _ = <-never => unreachable!("never"),
            complete => {},
        }
    }

    #[test]
    fn select_ready() {
        let mut ready = select::ready(());
        select! {
            v = <-ready => assert_eq!(v, ()),
            complete => unreachable!("not completed"),
        }
        select! {
            _ = <-ready => unreachable!("ready consumed"),
            complete => {},
        }
    }

    #[crate::test(crate = "crate")]
    fn deque() {
        let mut values = vec![1, 2, 3];
        let mut deque: std::collections::VecDeque<_> = values.drain(..).collect();
        assert_eq!(values, vec![]);
        loop {
            select! {
                v = <-deque => values.push(v),
                complete => break,
            }
        }
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[crate::test(crate = "crate")]
    fn option() {
        let mut option = Some(5);
        loop {
            select! {
                v = <-option => assert_eq!(v, 5),
                complete => break,
            }
        }
        assert_eq!(option, None);
    }

    #[test]
    fn select_handover() {
        let (mut sender, mut receiver) = serial::bounded(1);

        select! {
            _ = <-receiver => panic!("empty"),
            _ = sender<-1  => {},
        }

        select! {
            r = <-receiver => assert_eq!(r, Some(1)),
            _ = sender<-2  => panic!("full"),
        }
    }

    #[crate::test(crate = "crate")]
    fn select_send() {
        let (mut signal_sender, mut signal_receiver) = serial::bounded(1);
        let (mut sender, mut receiver) = serial::bounded(1);
        let job = coroutine::spawn({
            let mut sender = sender.clone();
            move || {
                let mut sender_enabled = true;
                select! {
                    _ = <-receiver => panic!("empty"),
                    _ = sender<-1, if sender_enabled => {},
                }

                select! {
                    r = <-receiver => {
                        assert_eq!(r, Some(1));
                        sender_enabled = false;
                    },
                    _ = sender<-1 => panic!("full"),
                }

                signal_sender.send(()).unwrap();

                select! {
                    _ = sender<-5, if sender_enabled => panic!("not enabled"),
                    r = <-receiver => assert_eq!(r, Some(2)),
                }
            }
        });
        signal_receiver.recv().unwrap();
        sender.send(2).unwrap();
        job.join().unwrap();
    }

    #[test]
    #[should_panic(expected = "all selectables are disabled or completed")]
    fn select_disabled() {
        let mut ready = select::ready(());
        select! {
            _ = <-ready, if false => unreachable!("not enabled"),
        }
    }

    #[test]
    fn select_disabled_default() {
        let mut ready = select::ready(());
        select! {
            _ = <-ready, if false => unreachable!("not enabled"),
            default => {},
        }
    }

    #[crate::test(crate = "crate")]
    fn select_loop_default() {
        let (mut sender, mut receiver) = serial::bounded(1);
        let mut sent = false;
        let mut received = false;
        loop {
            select! {
                _ = <-receiver => received = true,
                _ = sender<-1, if !sent  => sent = true,
                default => {
                    assert!(sent);
                    assert!(received);
                    break;
                }
            }
        }
    }

    #[test]
    fn select_send_default() {
        let (mut sender, mut receiver) = serial::bounded(2);
        let mut sent = 0;
        let mut received = 0;
        loop {
            select! {
                _ = sender<-2 => {
                    sent += 1;
                },
                default => break,
            }
        }
        loop {
            select! {
                r = <-receiver => {
                    received += 1;
                    assert_eq!(r, Some(2));
                },
                default => break,
            }
        }
        assert_eq!(sent, 2);
        assert_eq!(received, 2);
    }

    #[crate::test(crate = "crate")]
    fn select_concurrent_recv() {
        let (mut sender, mut receiver) = parallel::bounded(5);
        let task1 = task::spawn({
            let mut receiver = receiver.clone();
            move || {
                select! {
                    r = <-receiver => return r.is_some(),
                }
            }
        });
        let task2 = task::spawn(move || {
            select! {
                r = <-receiver => return r.is_some(),
            }
        });

        crate::time::sleep(std::time::Duration::from_secs(1));
        sender.send(()).unwrap();
        drop(sender);

        let result1 = task1.join().unwrap();
        let result2 = task2.join().unwrap();

        assert_eq!(result1 & result2, false);
        assert_eq!(result1 | result2, true);
    }

    #[crate::test(crate = "crate")]
    fn select_concurrent_send() {
        let (mut signal_sender, mut signal_receiver) = parallel::bounded(1);
        let (mut sender, mut receiver) = parallel::bounded(1);
        let task1 = task::spawn({
            let mut sender = sender.clone();
            let mut signal = signal_sender.clone();
            move || {
                select! {
                    r = sender<-() => {
                        signal.send(()).ignore();
                        return r.is_ok()
                    }
                }
            }
        });
        let task2 = task::spawn(move || {
            select! {
                r = sender<-() => {
                    signal_sender.send(()).ignore();
                    return r.is_ok()
                }
            }
        });

        signal_receiver.recv();
        receiver.close();
        assert_eq!(receiver.recv(), Some(()));
        assert_eq!(receiver.recv(), None);
        assert!(receiver.is_drained());

        let result1 = task1.join().unwrap();
        let result2 = task2.join().unwrap();

        assert_eq!(result1 & result2, false);
        assert_eq!(result1 | result2, true);
    }
}
