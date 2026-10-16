//! Native smoke tests for the patched Objective-C Blocks runtime boundary.

use block::ConcreteBlock;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn stack_and_heap_blocks_preserve_arguments_and_captures() {
    let captured = String::from("capture");
    let block = ConcreteBlock::new(move |value: usize| captured.len() + value);
    // The closure has no mutable state and accepts the declared argument type.
    assert_eq!(unsafe { block.call((5,)) }, 12);

    let heap = block.copy();
    assert_eq!(unsafe { heap.call((7,)) }, 14);
    let clone = heap.clone();
    drop(heap);
    assert_eq!(unsafe { clone.call((9,)) }, 16);
}

struct Capture(Arc<AtomicUsize>);

impl Capture {
    fn value(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn copying_and_cloning_release_the_capture_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(Arc::clone(&drops));
    let heap = ConcreteBlock::new(move || capture.value()).copy();
    let clone = heap.clone();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(heap);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    // The heap clone must retain its capture after the first reference drops.
    assert_eq!(unsafe { clone.call(()) }, 0);
    drop(clone);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
