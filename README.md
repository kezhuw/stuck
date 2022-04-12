# stuck

[![crates.io](https://img.shields.io/crates/v/stuck?style=for-the-badge)](https://crates.io/crates/stuck)
[![docs.rs](https://img.shields.io/docsrs/stuck?style=for-the-badge)](https://docs.rs/stuck)
[![github-ci](https://img.shields.io/github/workflow/status/kezhuw/stuck/CI?style=for-the-badge)](https://github.com/kezhuw/stuck/actions)
[![mit-license](https://img.shields.io/github/license/kezhuw/stuck?style=for-the-badge)](LICENSE)

Stuck is a multi-threading scheduled task facility building on cooperative stackful coroutine.

## Examples
```rust
use std::time::Duration;

use stuck::channel::parallel;
use stuck::channel::prelude::*;
use stuck::{select, task, time};

#[stuck::main]
fn main() {
    let (mut task_sender, task_receiver) = parallel::bounded(1);
    let (mut response_sender, mut response_receiver) = parallel::bounded(1);

    task::spawn(move || {
        for value in task_receiver.into_iter() {
            time::sleep(Duration::from_secs(1));
            response_sender.send(value - 1).unwrap();
        }
    });

    let mut tasks = vec![6, 6, 6, 6];

    let mut sum = 0;
    while !response_receiver.is_drained() {
        select! {
            r = <-response_receiver => {
                if let Some(n) = r {
                    sum += n;
                }
            },
            _ = task_sender<-tasks.pop().unwrap(), if !tasks.is_empty() => {
                if tasks.is_empty() {
                    task_sender.close();
                }
            },
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
