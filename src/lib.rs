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
//! with all other tasks.
//! * Use [task::session] to create facilities to wake waiting tasks.
//! * Use [task::JoinHandle] to join task result.
//!
//! ### Coroutine
//! * Use [coroutine::spawn] to spawn new coroutine in running task. The spawned coroutine will run
//! cooperatively with other coroutines belonging to same task.
//! * Use [coroutine::suspension] to create facilities to resume suspending coroutines.
//! * Use [coroutine::JoinHandle] to join coroutine result.
//!
//! ## Example
//! ```rust
//! use stuck::{coroutine, task};
//!
//! #[stuck::main]
//! fn main() {
//!     let twenty = task::spawn(|| {
//!         let five_coroutine = coroutine::spawn(|| 5);
//!
//!         let (suspension, resumption) = coroutine::suspension::<i32>();
//!         coroutine::spawn(move || resumption.resume(5));
//!
//!         let five_task = task::spawn(|| 5);
//!
//!         let (session, waker) = task::session::<i32>();
//!         task::spawn(move || waker.wake(5));
//!
//!         session.wait() + suspension.suspend() + five_coroutine.join().unwrap() + five_task.join().unwrap()
//!     });
//!     println!("twenty.join().unwrap(): {}", twenty.join().unwrap());
//! }
//! ```

pub mod coroutine;
mod error;
pub mod runtime;
pub mod task;
pub mod time;

pub use coroutine::stack::StackSize;
pub use error::JoinError;
#[cfg(not(test))]
pub use stuck_macros::main;
pub use stuck_macros::test;
