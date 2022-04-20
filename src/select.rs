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

/// Select candidate to read and/or write value in blocking or nonblocking.
pub trait Select<'a> {
    /// Returns all selectable candidates.
    fn selectables(&'a self) -> &[Option<&'a dyn Selectable>];

    /// Tries to select a permit for consumption.
    ///
    /// # Safety
    /// The returned permit must be consumed otherwise it leaked.
    unsafe fn try_select(&'a self) -> Option<(usize, Permit)> {
        for (index, selectable) in Enumerator::new(self.selectables()) {
            if let Some(selectable) = selectable {
                if let Some(permit) = selectable.select_permit() {
                    return Some((index, permit));
                }
            }
        }
        None
    }

    /// Selects a permit for consumption.
    ///
    /// # Safety
    /// The returned permit must be consumed otherwise it leaked.
    unsafe fn select(&'a self) -> (usize, Permit) {
        if let Some(selection) = self.try_select() {
            return selection;
        }
        let selectables = self.selectables();
        let parallel = selectables.iter().flatten().any(|s| s.parallel());
        let identifier = Identifier::new(&parallel as *const bool as *const ());
        let (waiter, waker) = Waker::new(parallel);
        let mut checked = 0;
        let mut disabled = 0;
        let enumerator = Enumerator::new(selectables);
        for (index, selectable) in enumerator.clone() {
            if let Some(selectable) = selectable {
                let selector = Selector::new(index, waker.clone(), identifier.copy());
                match selectable.watch_permit(selector) {
                    None => disabled += 1,
                    Some(true) if waiter.is_ready() => break,
                    _ => {},
                }
            } else {
                disabled += 1;
            }
            checked += 1;
        }
        // Check disabled before drop useless waker otherwise we will get different panic.
        if disabled == selectables.len() {
            panic!("all select cases disabled with no `default`");
        }
        drop(waker);
        let waiter = WitnessWaiter::new(checked, waiter, identifier, enumerator);
        waiter.wait()
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
    fn select_permit(&self) -> Option<Permit>;

    /// Watches for available permit.
    ///
    /// # Returns
    /// * `None` if all permits has been consumed and there is no meaningful closed value. The case
    /// where this selectable is consulted will be treated as disabled.
    /// * `Some(true)` if a permit is avaiable and applied.
    /// * `Some(false)` no permit avaiable to consume.
    fn watch_permit(&self, selector: Selector) -> Option<bool>;

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

    fn select_permit(&self) -> Option<Permit> {
        (**self).select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> Option<bool> {
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

    fn select_permit(&self) -> Option<Permit> {
        (**self).select_permit()
    }

    fn watch_permit(&self, selector: Selector) -> Option<bool> {
        (**self).watch_permit(selector)
    }

    fn unwatch_permit(&self, identifier: &Identifier) {
        (**self).unwatch_permit(identifier)
    }
}

#[cfg(test)]
mod tests {
    use ignore_result::Ignore;

    use crate::channel::prelude::*;
    use crate::channel::{parallel, serial, SendError};
    use crate::{coroutine, select, task};

    #[test]
    fn select_closed() {
        let (mut sender, mut receiver) = serial::closed();
        assert!(sender.is_closed());
        assert!(receiver.is_drained());
        select! {
            r = <-receiver => assert_eq!(r, None),
        }
        select! {
            r = sender<-() => assert_eq!(r, Err(SendError::Closed(()))),
        }
    }

    #[test]
    fn select_ready() {
        let mut receiver = serial::ready(());
        select! {
            r = <-receiver => assert_eq!(r, Some(())),
        }
        select! {
            r = <-receiver => assert_eq!(r, None),
        }
        assert!(receiver.is_drained());
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
    #[should_panic(expected = "all select cases disabled with no `default`")]
    fn select_disabled() {
        let mut ready = serial::ready(());
        select! {
            _ = <-ready, if false => unreachable!("not enabled"),
        }
    }

    #[test]
    fn select_disabled_default() {
        let mut ready = serial::ready(());
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
