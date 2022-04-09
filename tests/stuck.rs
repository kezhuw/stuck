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
fn test_twenty() {
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

#[stuck::test]
fn tcp_echo() {
    use std::io::{Read, Write};
    use std::net::SocketAddr;
    use std::str::FromStr;

    use stuck::net;

    let mut listener = net::TcpListener::bind(SocketAddr::from_str("127.0.0.1:0").unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    task::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        drop(listener);
        let mut buf: [u8; 16] = Default::default();
        loop {
            match stream.read(&mut buf).unwrap() {
                0 => break,
                n => stream.write_all(&buf[..n]).unwrap(),
            }
        }
    });

    let mut stream = net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
    let str = "Hi!";
    stream.write_all(str.as_bytes()).unwrap();
    stream.shutdown_write().unwrap();

    let mut buf = Vec::with_capacity(str.len());
    unsafe { buf.set_len(str.len()) };
    stream.read_exact(&mut buf).unwrap();
    drop(stream);

    let echo = std::str::from_utf8(&buf).unwrap();
    assert_eq!(str, echo);
}
