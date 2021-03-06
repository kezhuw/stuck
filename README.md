# stuck

[![crates.io](https://img.shields.io/crates/v/stuck)](https://crates.io/crates/stuck)
[![github-ci](https://github.com/kezhuw/stuck/actions/workflows/ci.yml/badge.svg?event=push)](https://github.com/kezhuw/stuck/actions)
[![codecov](https://codecov.io/gh/kezhuw/stuck/branch/master/graph/badge.svg?token=ZZ7LCNKQWQ)](https://codecov.io/gh/kezhuw/stuck)
[![docs.rs](https://img.shields.io/docsrs/stuck)](https://docs.rs/stuck)
[![mit-license](https://img.shields.io/github/license/kezhuw/stuck)](LICENSE)

Stuck is a multi-threading scheduled task facility building on cooperative stackful coroutine.

## Examples
```rust
use std::time::Duration;

use stuck::channel::parallel;
use stuck::channel::prelude::*;
use stuck::{select, task, time};

#[stuck::main]
fn main() {
    let (mut request_sender, request_receiver) = parallel::bounded(1);
    let (mut response_sender, mut response_receiver) = parallel::bounded(1);

    task::spawn(move || {
        for value in request_receiver.into_iter() {
            time::sleep(Duration::from_secs(1));
            response_sender.send(value - 1).unwrap();
        }
    });

    let mut tasks = vec![6, 6, 6, 6];

    let mut sum = 0;
    loop {
        select! {
            r = <-response_receiver => if let Some(n) = r {
                sum += n;
            },
            _ = request_sender<-tasks.pop().unwrap(), if !tasks.is_empty()  => if tasks.is_empty() {
                request_sender.close();
            },
            complete => break,
        }
    }
    println!("sum: {}", sum);
    assert_eq!(sum, 20);
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
