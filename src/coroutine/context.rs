use std::{mem, ptr};

use super::stack::{Stack, StackSize};

extern "C" {
    fn getcontext(ucp: *mut libc::ucontext_t) -> libc::c_int;
    fn setcontext(ucp: *const libc::ucontext_t) -> libc::c_int;
    fn swapcontext(oucp: *mut libc::ucontext_t, ucp: *const libc::ucontext_t) -> libc::c_int;
    fn makecontext(ucp: *mut libc::ucontext_t, func: extern "C" fn(*mut libc::c_void), argc: libc::c_int, ...);
}

pub struct Context {
    stack: Stack,
    context: libc::ucontext_t,
    // Seems that libc::ucontext_t does not list all possible members. Add padding to avoid crash.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    _padding: [u8; 256],
}

#[derive(Debug)]
pub struct Entry {
    pub f: extern "C" fn(*mut libc::c_void),
    pub arg: *mut libc::c_void,
    pub stack_size: StackSize,
}

unsafe impl Sync for Context {}

impl Context {
    pub fn empty() -> Context {
        unsafe { mem::zeroed() }
    }

    // Box Context to avoid potential self-referential members. Without boxing, linux will crash
    // unpredictable.
    pub fn new(entry: &Entry, returns: Option<&mut Context>) -> Box<Context> {
        let mut ctx = Box::new(Context::empty());
        let rc = unsafe { getcontext(&mut ctx.context) };
        if rc != 0 {
            panic!("getcontext returns {}", rc);
        }
        let stack = Stack::alloc(entry.stack_size);
        ctx.context.uc_stack.ss_sp = stack.base() as *mut libc::c_void;
        ctx.context.uc_stack.ss_size = stack.size();
        ctx.context.uc_link = match returns {
            Option::None => ptr::null_mut(),
            Option::Some(context) => &mut context.context,
        };
        ctx.stack = stack;
        unsafe { makecontext(&mut ctx.context, entry.f, 1, entry.arg) };
        ctx
    }

    pub fn resume(&self) {
        let rc = unsafe { setcontext(&self.context) };
        if rc != 0 {
            panic!("setcontext returns {}", rc);
        }
    }

    pub fn switch(&self, backup: &mut Context) {
        let rc = unsafe { swapcontext(&mut backup.context, &self.context) };
        if rc != 0 {
            panic!("swapcontext returns {}", rc);
        }
    }
}
