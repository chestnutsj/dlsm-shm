//! 命名共享内存 Arena（`shm_open` 后备）跨进程 attach 测试。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_ptr_alignment,
    missing_docs
)]

use core::alloc::Layout;
use core::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicU32, Ordering};

use dlsm_shm::{Arena, ArenaError};

/// 每个测试用唯一名字，避免 `/dev/shm` 残留互相干扰。
fn unique_name(tag: &str) -> String {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    format!("/dlsm_test_{}_{}_{}", tag, std::process::id(), n)
}

#[test]
fn named_segment_is_shared_between_handles() {
    let name = unique_name("share");
    let owner = Arena::create_named(&name, 64 * 1024).unwrap();
    let attached = Arena::attach_named(&name).unwrap();

    // owner 分配并写入哨兵值
    let layout = Layout::from_size_align(8, 8).unwrap();
    let p = owner.alloc(layout).unwrap();
    // SAFETY: p 指向 owner 段内 8 字节，独占写
    unsafe {
        p.as_ptr()
            .cast::<u64>()
            .write_volatile(0x0BAD_C0DE_DEAD_BEEF);
    }

    // 分配器状态(used)在共享 header 里：attached 看到同一偏移
    assert_eq!(attached.used(), owner.used());

    // owner 写入的字节，通过 attached 映射在相同偏移处可见
    let offset = p.as_ptr() as usize - owner.base_ptr().as_ptr() as usize;
    // SAFETY: 同一段的另一映射，offset 在容量内
    let seen = unsafe {
        attached
            .base_ptr()
            .as_ptr()
            .add(offset)
            .cast::<u64>()
            .read_volatile()
    };
    assert_eq!(seen, 0x0BAD_C0DE_DEAD_BEEF);
}

#[test]
fn allocator_state_is_coherent_across_handles() {
    let name = unique_name("alloc");
    let owner = Arena::create_named(&name, 64 * 1024).unwrap();
    let attached = Arena::attach_named(&name).unwrap();

    let layout = Layout::from_size_align(64, 8).unwrap();
    owner.alloc(layout).unwrap();
    // attached 经由共享 header 看到 owner 的分配
    assert_eq!(attached.used(), 64);

    // 从 attached 分配，owner 也应看到推进，且不与 owner 的分配重叠
    attached.alloc(layout).unwrap();
    assert_eq!(owner.used(), 128);
}

#[test]
fn attach_nonexistent_returns_not_found() {
    let name = unique_name("missing");
    let err = Arena::attach_named(&name).expect_err("attach to missing segment must fail");
    assert!(matches!(err, ArenaError::NotFound), "got {err:?}");
}

#[test]
fn create_twice_returns_already_exists() {
    let name = unique_name("dup");
    let _owner = Arena::create_named(&name, 4096).unwrap();
    let err = Arena::create_named(&name, 4096).expect_err("second create must fail");
    assert!(matches!(err, ArenaError::AlreadyExists), "got {err:?}");
}

#[test]
fn create_rejects_zero_size() {
    let name = unique_name("zero");
    let err = Arena::create_named(&name, 0).expect_err("zero size must error");
    assert!(matches!(err, ArenaError::ZeroSize));
}

/// 真·跨进程：fork 出独立子进程 attach 同名段，子进程写、父进程读。
/// 验证命名 SHM 能被**无血缘关系外**之外的进程访问（这里用 fork 子进程作最小验证）。
#[test]
fn forked_process_attaches_and_shares_data() {
    let name = unique_name("fork");
    let owner = Arena::create_named(&name, 64 * 1024).unwrap();

    // 在段内放一个原子计数器，父子都通过 attach 看到它。
    let layout = Layout::new::<AtomicU64>();
    let counter_off = {
        let p = owner.alloc(layout).unwrap();
        // SAFETY: p 指向段内 8 字节，独占初始化
        unsafe { p.as_ptr().cast::<AtomicU64>().write(AtomicU64::new(0)) };
        p.as_ptr() as usize - owner.base_ptr().as_ptr() as usize
    };

    // SAFETY: fork 是标准 POSIX 调用；子进程只做最小工作后 _exit。
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        // 子进程：独立 attach，给计数器 +42，然后立即退出（不回到测试框架）。
        let child = Arena::attach_named(&name).expect("child attach");
        // SAFETY: counter_off 指向已初始化的 AtomicU64
        let c = unsafe {
            &*child
                .base_ptr()
                .as_ptr()
                .add(counter_off)
                .cast::<AtomicU64>()
        };
        c.fetch_add(42, Ordering::SeqCst);
        // 跳过 Drop（否则子进程会 shm_unlink/munmap 干扰父进程）；直接 _exit。
        unsafe { libc::_exit(0) };
    }

    // 父进程：等子进程结束，读计数器，应看到子进程写的 42。
    let mut status = 0;
    // SAFETY: 标准 waitpid。
    unsafe { libc::waitpid(pid, &mut status, 0) };

    let c = unsafe {
        &*owner
            .base_ptr()
            .as_ptr()
            .add(counter_off)
            .cast::<AtomicU64>()
    };
    assert_eq!(
        c.load(Ordering::SeqCst),
        42,
        "parent must observe child's cross-process write"
    );
}

/// 子进程创建命名段后**不 Drop 直接 `_exit`**，模拟崩溃残留（`owner_pid`=子进程，且已死）。
/// 返回段名供父进程恢复/清理。
fn spawn_leaked_segment(name: &str, size: usize) {
    // SAFETY: fork 标准调用。
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let a = Arena::create_named(name, size).expect("child create");
        // 故意泄漏：跳过 Drop（不 shm_unlink），段以子进程 pid 为 owner 残留。
        core::mem::forget(a);
        // SAFETY: 立即退出，不回测试框架，不跑析构。
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    // SAFETY: 等子进程死亡，确保 owner_pid 已不存活。
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

#[test]
fn create_or_recover_on_fresh_name_creates() {
    let name = unique_name("recov_fresh");
    let a = Arena::create_or_recover_named(&name, 64 * 1024).unwrap();
    assert_eq!(a.used(), 0);
    assert_eq!(a.capacity(), 64 * 1024);
}

#[test]
fn create_or_recover_reclaims_stale_segment() {
    let name = unique_name("recov_stale");
    spawn_leaked_segment(&name, 64 * 1024);

    // 残留段存在，但 owner 已死 → 应被回收并重建为全新段。
    let a = Arena::create_or_recover_named(&name, 64 * 1024).unwrap();
    assert_eq!(a.used(), 0, "reclaimed segment must be fresh");
}

#[test]
fn create_or_recover_refuses_when_owner_alive() {
    let name = unique_name("recov_live");
    // 本进程持有 owner（活着）。
    let _owner = Arena::create_named(&name, 4096).unwrap();
    let err =
        Arena::create_or_recover_named(&name, 4096).expect_err("must refuse while owner is alive");
    assert!(matches!(err, ArenaError::AlreadyExists), "got {err:?}");
}

#[test]
fn cleanup_if_stale_removes_dead_owner_segment() {
    let name = unique_name("clean_dead");
    spawn_leaked_segment(&name, 4096);

    let cleaned = Arena::cleanup_if_stale(&name).unwrap();
    assert!(cleaned, "stale segment with dead owner must be cleaned");
    // 清理后再 attach 应 NotFound
    assert!(matches!(
        Arena::attach_named(&name),
        Err(ArenaError::NotFound)
    ));
}

#[test]
fn cleanup_if_stale_keeps_live_segment() {
    let name = unique_name("clean_live");
    let _owner = Arena::create_named(&name, 4096).unwrap();
    let cleaned = Arena::cleanup_if_stale(&name).unwrap();
    assert!(!cleaned, "segment with live owner must not be cleaned");
    // 仍可 attach
    assert!(Arena::attach_named(&name).is_ok());
}

#[test]
fn cleanup_if_stale_on_missing_is_false() {
    let name = unique_name("clean_missing");
    let cleaned = Arena::cleanup_if_stale(&name).unwrap();
    assert!(!cleaned, "cleanup of a missing segment must return false");
}
