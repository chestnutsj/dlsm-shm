//! Arena 系统调用失败路径测试。

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use dlsm_shm::{Arena, ArenaError};

#[test]
fn mmap_failure_reports_io_error() {
    // 请求接近 usize::MAX 必然让 mmap 失败 (ENOMEM 或 EINVAL)。
    // 实际值留 1 页避免某些 libc 直接 EINVAL；usize::MAX - 4096 是经验值。
    let huge = usize::MAX - 4096;
    let err = Arena::new_anonymous(huge).expect_err("near-max size must fail");
    match err {
        ArenaError::Mmap(io) => {
            // 不强约束 errno; 只确保是真实的 OS 错误。
            assert!(io.raw_os_error().is_some(), "must carry an OS errno");
        }
        other => panic!("expected Mmap error, got {other:?}"),
    }
}
