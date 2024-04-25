use std::fs::File;
use std::io::{Error, Result};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::thread;

use ignore_result::Ignore;

use super::{OpCode, Operation, Request, Stopper};
use crate::channel::parallel;
use crate::channel::prelude::*;
use crate::select;

#[derive(Clone)]
pub(crate) struct Requester {
    main: parallel::Sender<Request>,
}

impl Requester {
    fn new(channel: parallel::Sender<Request>) -> Self {
        Self { main: channel }
    }

    pub fn send(&mut self, request: Request) {
        self.main.send(request).unwrap()
    }

    pub fn cancel(&mut self, _opcode: OpCode, _user_data: usize) {}
}

pub(crate) struct Poller {
    channel: parallel::Receiver<Request>,
}

impl Poller {
    pub fn new() -> (Self, Requester) {
        let (sender, receiver) = parallel::bounded(4096);
        (Self { channel: receiver }, Requester::new(sender))
    }

    pub fn start(self, parallelism: usize) -> Result<(Stopper, parallel::Receiver<()>)> {
        let (sender, receiver) = parallel::unbounded(0);
        let threads: Vec<_> = (1..=parallelism)
            .map(|i| {
                let requester = self.channel.clone();
                let closing = receiver.clone();
                let name = format!("stuck::io::poller({}/{})", i, parallelism);
                thread::Builder::new()
                    .name(name)
                    .spawn(move || Self::serve(requester, closing))
                    .expect("failed to spawn stuck::io::poller thread")
            })
            .collect();
        Ok((Stopper { closing: sender, threads }, receiver))
    }

    fn serve(mut requester: parallel::Receiver<Request>, mut closing: parallel::Receiver<()>) {
        loop {
            select! {
                r = <-requester => {
                    let Some(request) = r else {
                        break;
                    };
                    Self::process(request);
                },
                _ = <-closing, if !closing.is_drained() => {
                    requester.close();
                },
            }
        }
    }

    fn process(request: Request) {
        match request.operation {
            Operation::Open { path, flags, mode } => {
                let rc = unsafe { libc::open(path, flags, mode) };
                let rc = if rc < 0 { Err(Error::last_os_error()) } else { Ok(rc as i32) };
                request.result.send(rc).ignore();
            },
            Operation::Read { fd, buf, len, offset } => {
                let rc = if offset == -1 {
                    unsafe { libc::read(fd, buf as *mut libc::c_void, len) }
                } else {
                    unsafe { libc::pread(fd, buf as *mut libc::c_void, len, offset as libc::off_t) }
                };
                let rc = if rc < 0 { Err(Error::last_os_error()) } else { Ok(rc as i32) };
                request.result.send(rc).ignore();
            },
            Operation::Write { fd, buf, len, offset } => {
                let rc = if offset == -1 {
                    unsafe { libc::write(fd, buf as *const libc::c_void, len) }
                } else {
                    unsafe { libc::pwrite(fd, buf as *const libc::c_void, len, offset as libc::off_t) }
                };
                let rc = if rc < 0 { Err(Error::last_os_error()) } else { Ok(rc as i32) };
                request.result.send(rc).ignore();
            },
            Operation::Fsync { fd, only_data } => {
                let file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd)) };
                let rc = if only_data { file.sync_data() } else { file.sync_all() };
                request.result.send(rc.map(|_| 0)).ignore();
            },
            Operation::Truncate { fd, size } => {
                let file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd)) };
                let rc = file.set_len(size).map(|_| 0);
                request.result.send(rc).ignore();
            },
            #[cfg(not(target_os = "linux"))]
            Operation::Stat { fd, metadata, .. } => {
                let stat = unsafe { &mut (*metadata).stat as *mut _ };
                let rc = unsafe { libc::fstat(fd, stat) };
                let rc = if rc < 0 { Err(Error::last_os_error()) } else { Ok(rc as i32) };
                request.result.send(rc).ignore();
            },
            #[cfg(target_os = "linux")]
            Operation::Stat { .. } => unreachable!("async io stat on linux threads"),
            Operation::Cancel { .. } => unreachable!("async io cancel on pool threads"),
        }
    }
}
