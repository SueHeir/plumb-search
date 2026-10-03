//! Test builds only: an allocator that counts each thread's live heap
//! bytes, so tests can check that parsing hostile input stays within a
//! memory budget. Other threads' allocations do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn count(delta: isize) {
    // `try_with`: allocations can happen while a thread is being torn down.
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

// SAFETY: every call goes straight to the system allocator; the bookkeeping
// only touches const-initialized thread-locals, which never allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        count(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `f` on this thread and returns its result along with the most heap
/// memory, in bytes, it had allocated at any one time.
pub(crate) fn peak_bytes<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(base));
    let result = f();
    let peak = PEAK.with(Cell::get);
    (result, peak.saturating_sub(base).max(0) as usize)
}
