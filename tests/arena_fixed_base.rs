//! 固定基址 + 只读 attach 测试（onstat 式观测的底座）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_ptr_alignment,
    missing_docs
)]

use core::alloc::Layout;
use core::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use dlsm_shm::{Arena, ArenaError};

fn unique_name(tag: &str) -> String {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    format!("/dlsm_test_{}_{}_{}", tag, std::process::id(), n)
}

#[test]
fn create_named_records_its_mapping_base() {
    let name = unique_name("fb_base");
    let owner = Arena::create_named(&name, 4096).unwrap();
    // 创建者本身就映射在固定基址处；数据区在 header 之后。
    assert_ne!(owner.fixed_base(), 0);
    assert!(owner.base_ptr().as_ptr() as usize > owner.fixed_base());
    // 段总长 = header + 可用容量，且覆盖数据区。
    assert!(owner.segment_len() >= owner.capacity());
}

#[test]
fn readonly_attach_same_process_hits_occupied_base() {
    // 同进程里创建者已占住固定基址，只读 attach 到同一基址必然失败。
    let name = unique_name("fb_occupied");
    let _owner = Arena::create_named(&name, 4096).unwrap();
    let err = Arena::attach_named_readonly(&name).expect_err("base occupied must fail");
    assert!(
        matches!(err, ArenaError::BaseAddrUnavailable),
        "got {err:?}"
    );
}

#[test]
fn readonly_attach_missing_returns_not_found() {
    let name = unique_name("fb_missing");
    let err = Arena::attach_named_readonly(&name).expect_err("missing must fail");
    assert!(matches!(err, ArenaError::NotFound), "got {err:?}");
}

/// 真·跨进程固定基址 attach：父进程写哨兵 + 一个**指向段内另一对象的绝对指针**；
/// 子进程释放 fork 继承的映射后，按 `header.base_addr` 以 `MAP_FIXED` 重新只读 attach，
/// 验证 (1) 映射落在相同基址，(2) 顺父进程写入的绝对指针能读到正确数据。
#[test]
fn forked_observer_attaches_at_fixed_base_and_resolves_absolute_pointer() {
    let name = unique_name("fb_fork");
    let owner = Arena::create_named(&name, 64 * 1024).unwrap();

    // 对象 B：放一个值。
    let b = owner.alloc(Layout::from_size_align(8, 8).unwrap()).unwrap();
    // SAFETY: b 指向段内 8 字节，独占写
    unsafe { b.as_ptr().cast::<u64>().write(0xCAFE_F00D_1234_5678) };

    // 对象 A：存 B 的**绝对地址**（固定基址下跨进程可解析）。
    let a = owner.alloc(Layout::from_size_align(8, 8).unwrap()).unwrap();
    // SAFETY: a 指向段内 8 字节，独占写
    unsafe { a.as_ptr().cast::<usize>().write(b.as_ptr() as usize) };
    let a_off = a.as_ptr() as usize - owner.base_ptr().as_ptr() as usize;

    let base = owner.fixed_base();
    let len = owner.segment_len();

    // SAFETY: fork 标准调用。
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        // 子进程：先释放 fork 继承的映射（腾出固定基址），不跑 owner 的 Drop。
        core::mem::forget(owner);
        // SAFETY: base/len 来自父进程成功的命名段映射；munmap 该区间释放继承映射。
        unsafe { libc::munmap(base as *mut c_void, len) };

        let code = match Arena::attach_named_readonly(&name) {
            Ok(view) => {
                // 必须映射在相同基址
                let same_base = view.fixed_base() == base;
                // 读对象 A 拿到 B 的绝对地址
                // SAFETY: view 在固定基址只读映射，a_off 在容量内
                let b_addr = unsafe { view.base_ptr().as_ptr().add(a_off).cast::<usize>().read() };
                // 顺绝对指针解引用读 B（固定基址使该绝对地址在本进程有效）
                // SAFETY: b_addr 是段内绝对地址，落在已映射只读区
                let b_val = unsafe { (b_addr as *const u64).read() };
                // 0 = 成功；1 = 校验失败
                i32::from(!(same_base && b_val == 0xCAFE_F00D_1234_5678))
            }
            Err(_) => 2,
        };
        // SAFETY: 立即退出，不回测试框架。
        unsafe { libc::_exit(code) };
    }

    let mut status = 0;
    // SAFETY: 标准 waitpid。
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let exit_code = libc::WEXITSTATUS(status);
    assert_eq!(exit_code, 0, "子进程固定基址 attach + 绝对指针解析应成功");
}
