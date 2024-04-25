use std::collections::hash_map::{self, HashMap};
use std::io::{Error, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use io_uring::{opcode, types, IoUring};

use super::{Operation, Request};
use crate::channel::parallel;
use crate::channel::prelude::*;
use crate::net::Registry;

impl Request {
    pub fn uring_entry(&self) -> io_uring::squeue::Entry {
        let entry = match self.operation {
            Operation::Open { path, flags, mode } => {
                opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path).flags(flags).mode(mode as libc::mode_t).build()
            },
            Operation::Read { fd, buf, len, offset } => {
                opcode::Read::new(types::Fd(fd), buf, len as u32).offset(offset as u64).build()
            },
            Operation::Write { fd, buf, len, offset } => {
                opcode::Write::new(types::Fd(fd), buf, len as u32).offset(offset as u64).build()
            },
            Operation::Fsync { fd, only_data } => {
                let flags = if only_data { types::FsyncFlags::DATASYNC } else { types::FsyncFlags::empty() };
                opcode::Fsync::new(types::Fd(fd)).flags(flags).build()
            },
            Operation::Stat { fd, path, metadata } => {
                let stat = unsafe { &(*metadata).stat as *const _ as *mut _ };
                opcode::Statx::new(types::Fd(fd), path, stat)
                    .flags(libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_SYNC_AS_STAT | libc::AT_EMPTY_PATH)
                    .mask(libc::STATX_ALL)
                    .build()
            },
            Operation::Truncate { .. } => unreachable!("no truncate support on linux uring"),
            Operation::Cancel { user_data } => opcode::AsyncCancel::new(user_data as u64).build(),
        };
        entry.user_data(self.result.user_data() as u64)
    }
}

struct State {
    entries: Vec<io_uring::squeue::Entry>,
    requests: HashMap<usize, Request>,
}

impl State {
    fn new() -> Self {
        Self { entries: Vec::with_capacity(1024), requests: HashMap::with_capacity(4096) }
    }

    fn push(&mut self, request: Request) {
        if let Operation::Cancel { user_data } = request.operation {
            match self.requests.entry(user_data) {
                hash_map::Entry::Vacant(_) => {
                    // target request already completed.
                    request.result.wake(Ok(0));
                    return;
                },
                hash_map::Entry::Occupied(entry) => {
                    if let Some(i) = self.entries.iter().position(|entry| entry.get_user_data() == user_data as u64) {
                        // target request is not submitted.
                        //
                        // Wake it first to maximize concurrency and avoid spuriously wakeup.
                        request.result.wake(Ok(0));
                        // io_uring guarantees no completion orderness, this way the submission order
                        // does not matter either.
                        self.entries.swap_remove(i);
                        let (_, cancelled) = entry.remove_entry();
                        cancelled.result.wake(Err(Error::from_raw_os_error(libc::ECANCELED)));
                        return;
                    }
                },
            }
        }
        let entry = request.uring_entry();
        self.requests.insert(request.result.user_data(), request);
        self.entries.push(entry);
    }
}

pub struct Uring {
    state: State,
    uring: IoUring,
    eventfd: OwnedFd,
}

impl Uring {
    pub fn new(registry: &Registry) -> Result<(Self, parallel::Receiver<()>)> {
        let eventfd = unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)) };
        let (_token, readable) = registry.register_reader(&eventfd.as_raw_fd())?;
        let uring = IoUring::builder()
            .dontfork()
            .setup_clamp()
            .setup_r_disabled()
            .setup_single_issuer()
            .setup_coop_taskrun()
            .setup_submit_all()
            .build(4096)?;
        Ok((Self { uring, eventfd, state: State::new() }, readable))
    }

    pub fn is_empty(&self) -> bool {
        self.state.requests.is_empty()
    }

    pub fn start(&self) -> Result<()> {
        let submitter = self.uring.submitter();
        submitter.register_eventfd_async(self.eventfd.as_raw_fd())?;
        submitter.register_enable_rings()?;
        Ok(())
    }

    pub fn submit(&mut self, request: Request, requester: &mut parallel::Receiver<Request>) {
        let mut submission = self.uring.submission();
        let max = submission.capacity() - submission.len();
        self.state.push(request);
        while self.state.entries.len() < max {
            let Ok(request) = requester.try_recv() else {
                break;
            };
            self.state.push(request);
        }
        unsafe {
            submission.push_multiple(&self.state.entries).unwrap();
        }
        self.state.entries.clear();
        drop(submission);
        self.uring.submit().unwrap();
        // Some submissions could complete inline, there are no notifications
        // for those submissions.
        self.consume();
    }

    pub fn consume(&mut self) {
        // Consume readable event first, so we could wake spuriously but never miss
        // anything.
        let mut buf = [0u8; 0];
        _ = unsafe { libc::read(self.eventfd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, 8) };
        for entry in self.uring.completion() {
            let user_data = entry.user_data() as usize;
            let request = self.state.requests.remove(&user_data).expect("no uring request found");
            let result = entry.result();
            let result = if result < 0 { Err(Error::from_raw_os_error(-result)) } else { Ok(result) };
            request.result.wake(result);
        }
    }
}
