use std::os::fd::RawFd;

use strum::EnumDiscriminants;

use crate::fs::Metadata;

#[derive(Debug, EnumDiscriminants)]
#[strum_discriminants(name(OpCode))]
pub enum Operation {
    Open {
        path: *const libc::c_char,
        flags: i32,
        mode: u32,
    },
    Read {
        fd: RawFd,
        buf: *mut u8,
        len: usize,
        offset: isize,
    },
    Write {
        fd: RawFd,
        buf: *const u8,
        len: usize,
        offset: isize,
    },
    Fsync {
        fd: RawFd,
        only_data: bool,
    },
    #[allow(dead_code)]
    Cancel {
        user_data: usize,
    },
    Truncate {
        fd: RawFd,
        size: u64,
    },
    Stat {
        fd: RawFd,
        #[allow(dead_code)]
        path: *const libc::c_char,
        metadata: *mut Metadata,
    },
}

unsafe impl Send for Operation {}
