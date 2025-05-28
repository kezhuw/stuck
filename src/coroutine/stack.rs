use std::io::Error;
use std::ptr;
use std::sync::OnceLock;

use super::page_size;

/// StackSize specifies desired stack size for new task.
///
/// It defaults to what environment variable `STUCK_STACK_SIZE` specifies and
/// 32KiB in 32-bit systems and 1MiB in 64-bit systems in case of absent.
#[derive(Copy, Clone, Default, Debug)]
pub struct StackSize {
    size: isize,
}

impl StackSize {
    #[cfg(target_pointer_width = "64")]
    fn default_size() -> usize {
        // 1MiB
        Self::align_to_page_size(1024 * 1024)
    }

    #[cfg(target_pointer_width = "32")]
    fn default_size() -> usize {
        // 32KiB
        Self::align_to_page_size(32 * 1024)
    }

    fn global_size() -> usize {
        static STACK_SIZE: OnceLock<usize> = OnceLock::new();
        *STACK_SIZE.get_or_init(|| match std::env::var("STUCK_STACK_SIZE") {
            Err(_) => Self::default_size(),
            Ok(val) => match val.parse::<usize>() {
                Err(_) | Ok(0) => Self::default_size(),
                Ok(n) => Self::align_to_page_size(n),
            },
        })
    }

    fn align_to_page_size(size: usize) -> usize {
        let mask = page_size::get() - 1;
        (size + mask) & !mask
    }

    fn aligned_page_size(&self) -> usize {
        let size = match self.size {
            0 => Self::global_size(),
            1.. => Self::global_size() + Self::align_to_page_size(self.size as usize),
            _ => Self::align_to_page_size((-self.size) as usize),
        };
        Self::align_to_page_size(size.max(libc::MINSIGSTKSZ))
    }

    /// Specifies extra stack size in addition to default.
    pub fn with_extra_size(size: usize) -> StackSize {
        assert!(size <= isize::MAX as usize);
        StackSize { size: size as isize }
    }

    /// Specifies desired stack size.
    pub fn with_size(size: usize) -> StackSize {
        assert!(size <= isize::MAX as usize, "stack size is too large");
        StackSize { size: -(size.max(1) as isize) }
    }
}

pub(crate) struct Stack {
    base: *mut u8,
    size: libc::size_t,
}

mod libc {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos"))]
    pub const MAP_STACK: libc::c_int = 0;
    #[allow(unused_imports)]
    pub use libc::*;
}

impl Stack {
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    pub fn alloc(size: StackSize) -> Stack {
        let page_size = page_size::get();
        let stack_size = size.aligned_page_size();
        let alloc_size = stack_size + 2 * page_size;

        let flags = libc::MAP_STACK | libc::MAP_ANONYMOUS | libc::MAP_PRIVATE;
        let low = unsafe { libc::mmap(ptr::null_mut(), alloc_size, libc::PROT_NONE, flags, -1, 0) as *mut u8 };
        if low as *mut libc::c_void == libc::MAP_FAILED {
            panic!("failed to alloc stack with mmap: {:?}", Error::last_os_error())
        }

        let base = unsafe { low.add(page_size) };
        if unsafe { libc::mprotect(base as *mut libc::c_void, stack_size, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
            panic!("failed to make stack read and write: {:?}", Error::last_os_error())
        }
        Stack { base, size: stack_size }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        if self.base.is_null() {
            return;
        }
        let page_size = page_size::get();
        let alloc_size = self.size() + 2 * page_size;
        let low = unsafe { self.base.sub(page_size) };
        if unsafe { libc::munmap(low as *mut libc::c_void, alloc_size) } != 0 {
            panic!("failed to drop stack with munmap: {:?}", Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use super::*;

    fn read_stack(stack: &Stack) {
        let _ = *unsafe { stack.base().as_ref().unwrap() };
        let _ = *unsafe { stack.base().add(stack.size() - 1).as_ref().unwrap() };
    }

    fn write_stack(stack: &Stack) {
        *unsafe { stack.base().as_mut().unwrap() } = 0x11;
        *unsafe { stack.base().add(stack.size() - 1).as_mut().unwrap() } = 0x11;
    }

    #[test]
    fn stack_zeroed() {
        let _stack: Stack = unsafe { mem::zeroed() };
    }

    #[test]
    fn stack_default() {
        let stack = Stack::alloc(StackSize::default());
        assert!(stack.size() / page_size::get() > 0);
        assert_eq!(stack.size() % page_size::get(), 0);

        read_stack(&stack);
        write_stack(&stack);
    }

    #[test]
    fn stack_custom() {
        let stack = Stack::alloc(StackSize::with_size(20));
        assert!(stack.size() / page_size::get() > 0);
        assert_eq!(stack.size() % page_size::get(), 0);

        read_stack(&stack);
        write_stack(&stack);
    }

    #[test]
    fn stack_extra_size() {
        let stack = Stack::alloc(StackSize::with_extra_size(20));
        assert!(stack.size() / page_size::get() > 0);
        assert_eq!(stack.size() % page_size::get(), 0);

        read_stack(&stack);
        write_stack(&stack);
    }
}
