use std::sync::mpsc;

use pretty_assertions::assert_eq;
use stuck::runtime::Runtime;
use stuck::{coroutine, task};

#[test]
fn test_runtime_spawn() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| 5);
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_task_spawn() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| task::spawn(|| 5).join().unwrap());
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_task_session() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| {
        let (session, waker) = task::session::<i32>();
        task::spawn(move || waker.wake(5));
        session.wait()
    });
    assert_eq!(5, five.join().unwrap());
}

#[test]
#[should_panic(expected = "session: no wakeup")]
fn test_task_session_no_wakeup() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| {
        let (session, waker) = task::session::<i32>();
        task::spawn(move || drop(waker));
        session.wait()
    });
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_task_session_waked_by_thread() {
    let runtime = Runtime::new();
    let (sender, receiver) = mpsc::sync_channel::<task::SessionWaker<i32>>(1);
    let five = runtime.spawn(move || {
        let (session, waker) = task::session::<i32>();
        sender.send(waker).unwrap();
        session.wait()
    });
    let waker = receiver.recv().unwrap();
    waker.wake(5);
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_coroutine_spawn() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| coroutine::spawn(|| 5).join().unwrap());
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_coroutine_suspension() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| {
        let (suspension, resumption) = coroutine::suspension::<i32>();
        coroutine::spawn(move || resumption.resume(5));
        suspension.suspend()
    });
    assert_eq!(5, five.join().unwrap());
}

#[test]
#[should_panic(expected = "suspend: no resumption")]
fn test_coroutine_suspension_no_wakeup() {
    let runtime = Runtime::new();
    let five = runtime.spawn(|| {
        let (suspension, resumption) = coroutine::suspension::<i32>();
        coroutine::spawn(move || drop(resumption));
        suspension.suspend()
    });
    assert_eq!(5, five.join().unwrap());
}

#[test]
fn test_example() {
    let runtime = Runtime::new();
    let twenty = runtime.spawn(|| {
        let five_coroutine = coroutine::spawn(|| 5);

        let (suspension, resumption) = coroutine::suspension::<i32>();
        coroutine::spawn(move || resumption.resume(5));

        let five_task = task::spawn(|| 5);

        let (session, waker) = task::session::<i32>();
        task::spawn(move || waker.wake(5));

        session.wait() + suspension.suspend() + five_coroutine.join().unwrap() + five_task.join().unwrap()
    });
    assert_eq!(20, twenty.join().unwrap());
}
