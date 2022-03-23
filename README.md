# stuck

Stuck is a multi-threading scheduled task facility building on cooperative stackful coroutine.

## Examples
```rust
use stuck::runtime::Runtime;
use stuck::{coroutine, task};

fn main() {
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
    println!("twenty.join().unwrap(): {}", twenty.join().unwrap());
}
```

See [tests](tests/stuck.rs) for more examples.

## LICENSE
[MIT](LICENSE)

## Inspiration
* [stp][]: The C++ counterpart that this library derives from.
* [skynet][]: A lightweight online game framework

[stp]: https://github.com/kezhuw/stp
[skynet]: https://github.com/cloudwu/skynet
