use std::{mem, ptr};

use super::stack::{Stack, StackSize};

#[cfg(not(any(target_pointer_width = "64", target_pointer_width = "32")))]
compile_error!("only 32-bit and 64-bit systems are supported");

#[allow(improper_ctypes)] // suppress "`extern` block uses type `u128`, which is not FFI-safe"
extern "C" {
    fn getcontext(ucp: *mut libc::ucontext_t) -> libc::c_int;
    fn setcontext(ucp: *const libc::ucontext_t) -> libc::c_int;
    fn swapcontext(oucp: *mut libc::ucontext_t, ucp: *const libc::ucontext_t) -> libc::c_int;
    fn makecontext(ucp: *mut libc::ucontext_t, func: extern "C" fn(u32, u32), argc: libc::c_int, ...);
}

#[repr(C, align(16))]
pub struct Context {
    stack: Stack,
    context: libc::ucontext_t,
    // macOS and its siblings embed mcontext inside ucontext while libc crate did not include them.
    // See following links for details.
    //
    // * https://github.com/rust-lang/libc/issues/2812
    // * https://github.com/rust-lang/libc/pull/2817
    // * https://github.com/rust-lang/libc/pull/3312
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos"))]
    _mcontext: libc::__darwin_mcontext64,
}

#[derive(Debug)]
pub struct Entry {
    pub f: extern "C" fn(u32, u32),
    pub arg1: u32,
    pub arg2: u32,
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
        unsafe {
            makecontext(&mut ctx.context, entry.f, 2, entry.arg1, entry.arg2);
        }
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
