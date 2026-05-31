//! Arena 构造测试。

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use dlsm_shm::{Arena, ArenaError};

#[test]
fn creates_anonymous_arena_with_given_capacity() {
    let arena = Arena::new_anonymous(4096).expect("anonymous arena should be creatable");
    assert_eq!(arena.capacity(), 4096);
    assert_eq!(arena.used(), 0);
    assert_eq!(arena.remaining(), 4096);
}

#[test]
fn rejects_zero_size_anonymous_arena() {
    let err = Arena::new_anonymous(0).expect_err("zero size must error");
    assert!(matches!(err, ArenaError::ZeroSize));
    assert_eq!(err.to_string(), "arena size must be non-zero");
}
