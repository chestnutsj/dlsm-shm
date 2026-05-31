//! 验证 `ArenaError` 的稳定错误码与英文 Display。

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use dlsm_shm::ArenaError;

#[test]
fn error_codes_are_stable_and_in_shm_range() {
    // shm 区段 10000..=19999
    assert_eq!(ArenaError::ZeroSize.code(), 10003);
    assert_eq!(ArenaError::AlreadyExists.code(), 10007);
    assert_eq!(ArenaError::NotFound.code(), 10008);
    assert_eq!(ArenaError::BadMagic.code(), 10010);
    assert_eq!(
        ArenaError::OutOfMemory {
            requested: 256,
            available: 128
        }
        .code(),
        10004
    );
    assert_eq!(
        ArenaError::VersionMismatch {
            segment: 2,
            expected: 1
        }
        .code(),
        10011
    );
}

#[test]
fn display_messages_are_english() {
    assert_eq!(
        ArenaError::ZeroSize.to_string(),
        "arena size must be non-zero"
    );
    assert_eq!(
        ArenaError::OutOfMemory {
            requested: 256,
            available: 128
        }
        .to_string(),
        "out of memory: requested 256 bytes, available 128"
    );
    assert_eq!(
        ArenaError::VersionMismatch {
            segment: 2,
            expected: 1
        }
        .to_string(),
        "shm header version mismatch: segment=2, expected=1"
    );
}
