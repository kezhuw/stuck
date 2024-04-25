use std::io::Result;
use std::thread;

use ignore_result::Ignore;

use super::{pool, uring, OpCode, Operation, Request, Stopper};
use crate::channel::parallel;
use crate::channel::prelude::*;
use crate::net::Registry;
use crate::{select, task};

#[derive(Clone)]
pub(crate) struct Requester {
    uring: parallel::Sender<Request>,
    pool: pool::Requester,
}

impl Requester {
    fn new(uring: parallel::Sender<Request>, pool: pool::Requester) -> Self {
        Self { uring, pool }
    }

    pub fn send(&mut self, request: Request) {
        if matches!(request.operation, Operation::Truncate { .. }) {
            self.pool.send(request);
        } else {
            self.uring.send(request).unwrap();
        }
    }

    pub fn cancel(&mut self, opcode: OpCode, user_data: usize) {
        if opcode == OpCode::Truncate {
            return self.pool.cancel(opcode, user_data);
        }
        let operation = Operation::Cancel { user_data };
        let (session, waker) = task::session();
        self.uring.send(Request { result: waker, operation }).unwrap();
        session.wait().ignore();
    }
}

pub(crate) struct Poller {
    uring: parallel::Receiver<Request>,
    pool: pool::Poller,
}

impl Poller {
    pub fn new() -> (Self, Requester) {
        let (sender, receiver) = parallel::bounded(4096);
        let (poller, requester) = pool::Poller::new();
        (Self { uring: receiver, pool: poller }, Requester::new(sender, requester))
    }

    pub fn start(self, registry: &Registry) -> Result<Stopper> {
        let (mut stopper, closing) = self.pool.start(2)?;
        let (uring, readable) = uring::Uring::new(registry)?;
        let uring_thread = thread::Builder::new()
            .name("stuck::io::poller::uring".to_string())
            .spawn(move || Self::serve(uring, self.uring, closing, readable))
            .expect("failed to spawn stuck::io::poller::uring thread");
        stopper.threads.push(uring_thread);
        Ok(stopper)
    }

    fn serve(
        mut uring: uring::Uring,
        mut requester: parallel::Receiver<Request>,
        mut closing: parallel::Receiver<()>,
        mut readable: parallel::Receiver<()>,
    ) {
        uring.start().unwrap();
        while !(uring.is_empty() && requester.is_drained()) {
            select! {
                r = <-requester, if !requester.is_drained() => {
                    let Some(request) = r else {
                        continue;
                    };
                    uring.submit(request, &mut requester);
                }
                _ = <-closing, if !closing.is_drained() => requester.close(),
                _ = <-readable => uring.consume(),
            }
        }
    }
}
