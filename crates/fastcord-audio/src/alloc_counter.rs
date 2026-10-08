//! Test-only global allocator that counts heap operations performed inside
//! audio data callbacks (see [`crate::audit`]). Every call is forwarded to the
//! system allocator unchanged.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Allocations, reallocations, and frees observed inside any callback scope,
/// on any thread, excluding [`expect_callback_allocations`] scopes.
static CALLBACK_HEAP_OPS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static THREAD_HEAP_OPS: Cell<u64> = const { Cell::new(0) };
    static EXPECTED: Cell<bool> = const { Cell::new(false) };
}

struct CountingAllocator;

fn note() {
    if crate::audit::in_audio_callback() {
        let _ = THREAD_HEAP_OPS.try_with(|count| count.set(count.get() + 1));
        if !EXPECTED.try_with(Cell::get).unwrap_or(false) {
            CALLBACK_HEAP_OPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees hold; the bookkeeping touches only an
// atomic and const-initialized thread-locals, neither of which allocates.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged; the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged; the caller upholds the contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        // SAFETY: `ptr` was allocated by `System` through this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: `ptr`/`layout` come from this allocator, which delegates to `System`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Heap operations made inside audio callbacks anywhere in the test process.
pub(crate) fn callback_heap_ops() -> u64 {
    CALLBACK_HEAP_OPS.load(Ordering::Relaxed)
}

/// Heap operations made inside callback scopes on the current thread.
pub(crate) fn thread_callback_heap_ops() -> u64 {
    THREAD_HEAP_OPS.with(Cell::get)
}

/// Runs `f` with the current thread's deliberate allocations excluded from the
/// process-wide count (used to prove the instrument itself works).
pub(crate) fn expect_callback_allocations<R>(f: impl FnOnce() -> R) -> R {
    EXPECTED.with(|flag| flag.set(true));
    let result = f();
    EXPECTED.with(|flag| flag.set(false));
    result
}

#[test]
fn instrument_detects_an_allocation_inside_a_callback_scope() {
    let before = thread_callback_heap_ops();
    expect_callback_allocations(|| {
        let _scope = crate::audit::CallbackScope::enter();
        let boxed = std::hint::black_box(Box::new([0u8; 64]));
        drop(boxed);
    });
    assert_eq!(thread_callback_heap_ops() - before, 2, "alloc + free");
    // Outside a scope nothing is attributed to callbacks.
    let _outside = std::hint::black_box(vec![1u8; 32]);
    assert_eq!(thread_callback_heap_ops() - before, 2);
}
