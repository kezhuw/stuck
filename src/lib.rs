#![allow(clippy::needless_doctest_main)]

//! # Multi-threading task facility building on cooperative stackful coroutine
//! `stuck` provides a multi-threading runtime to serve lightweight concurrent tasks where you can
//! use [coroutine::suspension] to cooperatively resume suspending coroutines.
//!
//! ## Usage
//! Construct an [runtime::Runtime] to [runtime::Runtime::spawn] initial task.
//!
//! ### Task
//! * Use [task::spawn] to spawn new task in running task. The spawned task will run concurrent
//!   with all other tasks.
//! * Use [task::session] to create facilities to wake waiting tasks.
//! * Use [task::JoinHandle] to join task result.
//!
//! ### Coroutine
//! * Use [coroutine::spawn] to spawn new coroutine in running task. The spawned coroutine will run
//!   cooperatively with other coroutines belonging to same task.
//! * Use [coroutine::suspension] to create facilities to resume suspending coroutines.
//! * Use [coroutine::JoinHandle] to join coroutine result.
//!
//! ## Example
//! ```rust
//! use std::time::Duration;
//!
//! use stuck::channel::parallel;
//! use stuck::channel::prelude::*;
//! use stuck::{select, task, time};
//!
//! #[stuck::main]
//! fn main() {
//!     let (mut request_sender, request_receiver) = parallel::bounded(1);
//!     let (mut response_sender, mut response_receiver) = parallel::bounded(1);
//!
//!     task::spawn(move || {
//!         for value in request_receiver.into_iter() {
//!             time::sleep(Duration::from_secs(1));
//!             response_sender.send(value - 1).unwrap();
//!         }
//!     });
//!
//!     let mut tasks = vec![6, 6, 6, 6];
//!
//!     let mut sum = 0;
//!     loop {
//!         select! {
//!             r = <-response_receiver => if let Some(n) = r {
//!                 sum += n;
//!             },
//!             _ = request_sender<-tasks.pop().unwrap(), if !tasks.is_empty()  => if tasks.is_empty() {
//!                 request_sender.close();
//!             },
//!             complete => break,
//!         }
//!     }
//!     println!("sum: {}", sum);
//!     assert_eq!(sum, 20);
//! }
//! ```

pub mod channel;
pub mod coroutine;
mod error;
pub mod fs;
mod io;
pub mod net;
pub mod runtime;
pub mod select;
mod select_macro;
pub mod task;
pub mod time;

pub use coroutine::stack::StackSize;
pub use error::JoinError;
#[cfg(not(test))]
pub use stuck_macros::main;
pub use stuck_macros::test;
