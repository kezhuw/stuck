# stuck

[![crates.io](https://img.shields.io/crates/v/stuck?style=for-the-badge)](https://crates.io/crates/stuck)
[![docs.rs](https://img.shields.io/docsrs/stuck?style=for-the-badge)](https://docs.rs/stuck)
[![github-ci](https://img.shields.io/github/workflow/status/kezhuw/stuck/CI?style=for-the-badge)](https://github.com/kezhuw/stuck/actions)
[![mit-license](https://img.shields.io/github/license/kezhuw/stuck?style=for-the-badge)](LICENSE)

Stuck is a multi-threading scheduled task facility building on cooperative stackful coroutine.

## Examples
```rust
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
```

See [tests](tests/stuck.rs) for more examples.

## LICENSE
[MIT](LICENSE)

## Inspiration
* [stp][]: The C++ counterpart that this library derives from.
* [skynet][]: A lightweight online game framework

[stp]: https://github.com/kezhuw/stp
[skynet]: https://github.com/cloudwu/skynet
