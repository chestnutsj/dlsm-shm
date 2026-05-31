//! 单线程 bump 分配器测试。

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use core::alloc::Layout;

use dlsm_shm::{Arena, ArenaError};

#[test]
fn alloc_returns_pointer_within_arena() {
    let arena = Arena::new_anonymous(4096).unwrap();
    let layout = Layout::from_size_align(64, 8).unwrap();

    let p = arena
        .alloc(layout)
        .expect("first 64-byte alloc must succeed");

    let base = arena.base_ptr().as_ptr() as usize;
    let addr = p.as_ptr() as usize;
    assert!(addr >= base, "allocation must lie at or above base");
    assert!(
        addr + 64 <= base + arena.capacity(),
        "must lie within arena"
    );
    assert_eq!(addr % 8, 0, "must satisfy alignment");
}

#[test]
fn alloc_advances_used_offset() {
    let arena = Arena::new_anonymous(4096).unwrap();
    let layout = Layout::from_size_align(64, 8).unwrap();

    arena.alloc(layout).unwrap();
    assert_eq!(arena.used(), 64);
    assert_eq!(arena.remaining(), 4096 - 64);

    arena.alloc(layout).unwrap();
    assert_eq!(arena.used(), 128);
}

#[test]
fn alloc_returns_distinct_pointers() {
    let arena = Arena::new_anonymous(4096).unwrap();
    let layout = Layout::from_size_align(16, 8).unwrap();

    let p1 = arena.alloc(layout).unwrap();
    let p2 = arena.alloc(layout).unwrap();
    assert_ne!(p1, p2);
    assert!(
        (p2.as_ptr() as usize) >= (p1.as_ptr() as usize) + 16,
        "p2 must not overlap p1"
    );
}

#[test]
fn alloc_respects_larger_alignment() {
    let arena = Arena::new_anonymous(4096).unwrap();
    // 先吃 1 字节制造非 64 对齐起点
    arena.alloc(Layout::from_size_align(1, 1).unwrap()).unwrap();

    let layout = Layout::from_size_align(64, 64).unwrap();
    let p = arena.alloc(layout).unwrap();
    assert_eq!(p.as_ptr() as usize % 64, 0);
}

#[test]
fn alloc_returns_oom_when_exceeding_capacity() {
    let arena = Arena::new_anonymous(128).unwrap();
    let big = Layout::from_size_align(256, 8).unwrap();

    let err = arena.alloc(big).expect_err("256 > 128 must OOM");
    match err {
        ArenaError::OutOfMemory {
            requested,
            available,
        } => {
            assert_eq!(requested, 256);
            assert_eq!(available, 128);
        }
        other => panic!("expected OutOfMemory, got {other:?}"),
    }

    // OOM 不应推进 used
    assert_eq!(arena.used(), 0);
}

#[test]
fn alloc_zero_size_returns_dangling_in_capacity() {
    // 与 std 的 Allocator 约定一致: size=0 不消耗容量, 返回 align 对齐的占位地址。
    let arena = Arena::new_anonymous(64).unwrap();
    let layout = Layout::from_size_align(0, 16).unwrap();

    let p = arena
        .alloc(layout)
        .expect("zero-size alloc returns sentinel");
    assert_eq!(p.as_ptr() as usize % 16, 0);
    assert_eq!(arena.used(), 0, "zero-size alloc must not consume capacity");
}

#[test]
fn map_anonymous_pages_are_zero_initialized() {
    let arena = Arena::new_anonymous(4096).unwrap();
    let layout = Layout::from_size_align(128, 8).unwrap();
    let p = arena.alloc(layout).unwrap();
    // SAFETY: alloc 返回的指针指向 128 字节的 mmap 区，未被写入。
    let slice = unsafe { core::slice::from_raw_parts(p.as_ptr(), 128) };
    assert!(slice.iter().all(|&b| b == 0));
}

#[test]
fn alloc_huge_layout_returns_oom_not_panic() {
    let arena = Arena::new_anonymous(64).unwrap();
    // Layout::from_size_align 拒绝 usize::MAX, 但允许 isize::MAX 大小 (align=1)。
    // 这个值远超 64 字节容量, 必然走 new_used > self.size 分支。
    let layout = Layout::from_size_align(isize::MAX as usize, 1).unwrap();
    let err = arena.alloc(layout).expect_err("isize::MAX must OOM");
    assert!(matches!(err, ArenaError::OutOfMemory { .. }));
}

#[test]
fn alloc_oom_due_to_padding_reports_padded_request() {
    // capacity=72, 先吃 1 字节, 再申请 align=64/size=64 -> 需要 padding 63 + 64 = 127, OOM
    let arena = Arena::new_anonymous(72).unwrap();
    arena.alloc(Layout::from_size_align(1, 1).unwrap()).unwrap();

    let err = arena
        .alloc(Layout::from_size_align(64, 64).unwrap())
        .expect_err("padding + size > remaining must OOM");
    assert!(matches!(err, ArenaError::OutOfMemory { .. }));
}
