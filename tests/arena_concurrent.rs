//! 多线程 bump 分配并发正确性测试。

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use core::alloc::Layout;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;

use dlsm_shm::Arena;

#[test]
fn concurrent_allocations_are_distinct_and_within_bounds() {
    const THREADS: usize = 8;
    const ALLOCS_PER_THREAD: usize = 256;
    const ALLOC_SIZE: usize = 32;
    const CAPACITY: usize = THREADS * ALLOCS_PER_THREAD * ALLOC_SIZE;

    let arena = Arc::new(Arena::new_anonymous(CAPACITY).unwrap());
    let barrier = Arc::new(Barrier::new(THREADS));
    let layout = Layout::from_size_align(ALLOC_SIZE, 8).unwrap();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let arena = Arc::clone(&arena);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut ptrs = Vec::with_capacity(ALLOCS_PER_THREAD);
                for _ in 0..ALLOCS_PER_THREAD {
                    ptrs.push(
                        arena
                            .alloc(layout)
                            .expect("under capacity must succeed")
                            .as_ptr() as usize,
                    );
                }
                ptrs
            })
        })
        .collect();

    let mut all = HashSet::with_capacity(THREADS * ALLOCS_PER_THREAD);
    let base = arena.base_ptr().as_ptr() as usize;
    for h in handles {
        for addr in h.join().unwrap() {
            assert!(addr >= base && addr + ALLOC_SIZE <= base + CAPACITY);
            assert_eq!(addr % 8, 0);
            assert!(all.insert(addr), "duplicate allocation address: {addr}");
        }
    }

    assert_eq!(all.len(), THREADS * ALLOCS_PER_THREAD);
    assert_eq!(arena.used(), CAPACITY);
    assert_eq!(arena.remaining(), 0);
}

#[test]
fn concurrent_allocations_under_pressure_report_oom_eventually() {
    // 多线程同时申请远超容量；最终至少一些请求应返回 OOM, 但已成功的分配仍互斥。
    const THREADS: usize = 4;
    const PER_THREAD: usize = 200;
    const ALLOC_SIZE: usize = 64;
    const CAPACITY: usize = THREADS * PER_THREAD * ALLOC_SIZE / 4; // 仅够 25% 请求

    let arena = Arc::new(Arena::new_anonymous(CAPACITY).unwrap());
    let layout = Layout::from_size_align(ALLOC_SIZE, 8).unwrap();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let arena = Arc::clone(&arena);
            thread::spawn(move || {
                let mut ok = Vec::new();
                let mut oom = 0usize;
                for _ in 0..PER_THREAD {
                    match arena.alloc(layout) {
                        Ok(p) => ok.push(p.as_ptr() as usize),
                        Err(_) => oom += 1,
                    }
                }
                (ok, oom)
            })
        })
        .collect();

    let mut all = HashSet::new();
    let mut total_oom = 0;
    for h in handles {
        let (ok, oom) = h.join().unwrap();
        total_oom += oom;
        for addr in ok {
            assert!(all.insert(addr), "duplicate addr {addr}");
        }
    }
    assert!(total_oom > 0, "pressure test must produce at least one OOM");
    assert!(arena.used() <= CAPACITY);
}
