//! Fixed-capacity buffers that one writer appends to while readers read what was
//! already written, with no lock and no copy.
//!
//! Soundness rests on three facts. There is exactly one [`AppendWriter`] per buffer
//! (created together, not `Clone`), so writes never race each other. The writer only
//! writes slots at or past the published length, and publishes new slots with a
//! `Release` store after writing them. Readers only form references to slots below a
//! length they loaded with `Acquire`, so they never see a slot being written and every
//! slot they see is initialised. Written slots are never touched again.

use std::alloc::{self, Layout};
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct AppendBuf<T: Copy> {
    ptr: NonNull<T>,
    cap: usize,
    len: AtomicUsize,
    _owns: PhantomData<T>,
}

// SAFETY: the buffer owns plain `Copy` data; shared access is read-only below the
// published length, and the unique writer only touches slots above it (module docs).
unsafe impl<T: Copy + Send + Sync> Send for AppendBuf<T> {}
// SAFETY: as above.
unsafe impl<T: Copy + Send + Sync> Sync for AppendBuf<T> {}

impl<T: Copy> AppendBuf<T> {
    /// An empty buffer of `cap` slots and the one writer allowed to fill it.
    pub fn new(cap: usize) -> (Arc<Self>, AppendWriter<T>) {
        let ptr = if cap == 0 || size_of::<T>() == 0 {
            NonNull::dangling()
        } else {
            let layout = Layout::array::<T>(cap).expect("capacity overflow");
            // SAFETY: the layout has a non-zero size.
            let raw = unsafe { alloc::alloc(layout) }.cast::<T>();
            NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout))
        };
        let buf = Arc::new(Self {
            ptr,
            cap,
            len: AtomicUsize::new(0),
            _owns: PhantomData,
        });
        let writer = AppendWriter {
            buf: Arc::clone(&buf),
            len: 0,
        };
        (buf, writer)
    }

    /// A full buffer holding exactly `items`.
    pub fn frozen(items: &[T]) -> Arc<Self> {
        let (buf, mut writer) = Self::new(items.len());
        writer.extend(items);
        buf
    }

    /// Slots written and published so far.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// Everything published so far.
    pub fn as_slice(&self) -> &[T] {
        let len = self.len();
        // SAFETY: slots below a length loaded with Acquire are initialised and are
        // never written again (module docs).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) }
    }

    /// Bytes this buffer allocated.
    pub fn heap_bytes(&self) -> usize {
        self.cap * size_of::<T>()
    }
}

impl<T: Copy> Drop for AppendBuf<T> {
    fn drop(&mut self) {
        if self.cap != 0 && size_of::<T>() != 0 {
            let layout = Layout::array::<T>(self.cap).expect("capacity overflow");
            // SAFETY: allocated in `new` with this layout; `T: Copy` needs no drop.
            unsafe { alloc::dealloc(self.ptr.as_ptr().cast(), layout) };
        }
    }
}

impl<T: Copy> fmt::Debug for AppendBuf<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppendBuf")
            .field("len", &self.len())
            .field("cap", &self.cap)
            .finish()
    }
}

/// The unique right to append to one [`AppendBuf`].
pub(crate) struct AppendWriter<T: Copy> {
    buf: Arc<AppendBuf<T>>,
    /// The writer's own copy of the published length.
    len: usize,
}

impl<T: Copy> AppendWriter<T> {
    pub fn remaining(&self) -> usize {
        self.buf.cap - self.len
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_full(&self) -> bool {
        self.len == self.buf.cap
    }

    /// Append as many of `items` as fit and publish them. Returns how many were written.
    pub fn extend(&mut self, items: &[T]) -> usize {
        let n = items.len().min(self.remaining());
        if n > 0 {
            // SAFETY: slots `len..len + n` are in bounds, unpublished (so no reader
            // holds a reference to them) and written only by this unique writer.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    items.as_ptr(),
                    self.buf.ptr.as_ptr().add(self.len),
                    n,
                );
            }
            self.len += n;
            self.buf.len.store(self.len, Ordering::Release);
        }
        n
    }

    /// Append one item. Returns `false` when the buffer is full.
    pub fn push(&mut self, item: T) -> bool {
        self.extend(std::slice::from_ref(&item)) == 1
    }

    /// Everything written so far.
    pub fn written(&self) -> &[T] {
        &self.buf.as_slice()[..self.len]
    }
}

impl<T: Copy> fmt::Debug for AppendWriter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppendWriter")
            .field("len", &self.len)
            .field("cap", &self.buf.cap)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
        let (buf, mut w) = AppendBuf::<u32>::new(4);
        assert!(buf.as_slice().is_empty());
        assert_eq!(w.extend(&[1, 2, 3]), 3);
        assert_eq!(buf.as_slice(), &[1, 2, 3]);
        assert!(w.push(4));
        assert!(!w.push(5));
        assert!(w.is_full());
        assert_eq!(w.written(), &[1, 2, 3, 4]);
        assert_eq!(AppendBuf::frozen(&[7u8, 8]).as_slice(), &[7, 8]);
        assert_eq!(AppendBuf::<u8>::new(0).0.as_slice(), &[] as &[u8]);
    }

    #[test]
    fn readers_see_a_growing_prefix() {
        let (buf, mut w) = AppendBuf::<u64>::new(100_000);
        let reader = std::thread::spawn(move || {
            let mut last = 0;
            while last < 100_000 {
                let s = buf.as_slice();
                assert!(s.len() >= last);
                for (i, v) in s.iter().enumerate() {
                    assert_eq!(*v, i as u64);
                }
                last = s.len();
            }
        });
        for i in 0..100_000u64 {
            w.push(i);
        }
        reader.join().unwrap();
    }
}
