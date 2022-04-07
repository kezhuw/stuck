use stuck::{coroutine, task};

#[stuck::main]
fn main() {
    let twenty = task::spawn(|| {
        let five_coroutine = coroutine::spawn(|| 5);

        let (suspension, resumption) = coroutine::suspension::<i32>();
        coroutine::spawn(move || resumption.resume(5));

        let five_task = task::spawn(|| 5);

        let (session, waker) = task::session::<i32>();
        task::spawn(move || waker.wake(5));

        session.wait() + suspension.suspend() + five_coroutine.join().unwrap() + five_task.join().unwrap()
    });
    println!("twenty.join().unwrap(): {}", twenty.join().unwrap());
}
