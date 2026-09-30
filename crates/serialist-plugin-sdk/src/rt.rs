//! What a `#![no_std]` guest needs from a runtime: an allocator and a panic handler.
//! Only on wasm32 with the `rt` feature.

use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bindings::LogLevel;

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

// `memcpy`, `memcmp` and friends. On wasm32-wasip2, core leaves them to the C library
// (std links it); only those pure functions are taken from it, so no WASI import follows.
#[cfg(target_env = "p2")]
#[link(name = "c")]
unsafe extern "C" {}

/// The canonical ABI's allocator, which the host calls to pass lists and strings in. On
/// wasm32-wasip2 std provides it and wit-bindgen does not, so a `no_std` guest gets it
/// here. (On other wasm32 targets wit-bindgen links its own.)
///
/// # Safety
///
/// Called by the host with the canonical ABI's contract: `old_ptr` and `old_len` describe
/// an earlier allocation made with `align` (or `old_len` is 0), and `align` is a power
/// of two.
#[cfg(target_env = "p2")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(
    old_ptr: *mut u8,
    old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    use alloc::alloc::{Layout, alloc, realloc};

    // SAFETY: the canonical ABI's contract, above.
    let ptr = unsafe {
        if old_len == 0 {
            if new_len == 0 {
                return align as *mut u8;
            }
            alloc(Layout::from_size_align_unchecked(new_len, align))
        } else {
            realloc(
                old_ptr,
                Layout::from_size_align_unchecked(old_len, align),
                new_len,
            )
        }
    };
    if ptr.is_null() {
        // Out of memory. Trap without formatting anything.
        core::arch::wasm32::unreachable();
    }
    ptr
}

/// Longest panic message passed to the host; the rest is cut.
const MESSAGE_BYTES: usize = 512;

/// Set by the first panic, so a panic while reporting one just traps.
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Log the panic through the host (without allocating: the panic may be an allocation
/// failure) and trap. The host turns the trap into a `plugin_error` frame that shows the
/// message, then starts the plugin over from a fresh instance.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    if !PANICKING.swap(true, Ordering::Relaxed) {
        // The message first: the host shows the first line of it in a frame's summary.
        let mut message = Truncated::new();
        let _ = write!(message, "panicked: {}", info.message());
        if let Some(at) = info.location() {
            let _ = write!(message, " ({}:{}:{})", at.file(), at.line(), at.column());
        }
        crate::bindings::log(LogLevel::Error, message.as_str());
    }
    core::arch::wasm32::unreachable()
}

/// A fixed buffer that keeps the first [`MESSAGE_BYTES`] bytes written to it, cut at a
/// character boundary.
struct Truncated {
    buf: [u8; MESSAGE_BYTES],
    len: usize,
}

impl Truncated {
    fn new() -> Self {
        Self {
            buf: [0; MESSAGE_BYTES],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // Only whole characters are copied in, so this never fails.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("panicked")
    }
}

impl Write for Truncated {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let n = c.len_utf8();
            if self.len + n > MESSAGE_BYTES {
                return Err(fmt::Error);
            }
            c.encode_utf8(&mut self.buf[self.len..self.len + n]);
            self.len += n;
        }
        Ok(())
    }
}
