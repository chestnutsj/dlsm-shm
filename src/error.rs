//! Arena 操作错误类型。
//!
//! 错误码区段 `10000..=19999`（本 crate 自带，独立于其它 crate）。消息一律英文。

use thiserror::Error;

/// Arena 创建或分配过程中的错误。
#[derive(Debug, Error)]
pub enum ArenaError {
    /// `mmap` 系统调用失败。
    #[error("mmap failed: {0}")]
    Mmap(#[source] std::io::Error),

    /// `munmap` 系统调用失败（仅在 drop 路径上被记录，不向用户返回）。
    #[error("munmap failed: {0}")]
    Munmap(#[source] std::io::Error),

    /// 请求大小为 0，无意义。
    #[error("arena size must be non-zero")]
    ZeroSize,

    /// 当前剩余容量不足以满足分配（包含 padding）。
    #[error("out of memory: requested {requested} bytes, available {available}")]
    OutOfMemory {
        /// 实际需要消耗的字节数（含对齐 padding）。
        requested: usize,
        /// 当前剩余可分配字节数。
        available: usize,
    },

    /// `shm_open` 系统调用失败（非"已存在"/"不存在"的其它错误）。
    #[error("shm_open failed: {0}")]
    ShmOpen(#[source] std::io::Error),

    /// `ftruncate` 系统调用失败。
    #[error("ftruncate failed: {0}")]
    Ftruncate(#[source] std::io::Error),

    /// 创建命名段时名字已存在（`O_EXCL`）。
    #[error("named segment already exists")]
    AlreadyExists,

    /// attach 命名段时名字不存在。
    #[error("named segment not found")]
    NotFound,

    /// 命名段名字非法（含 NUL，或无法转为 C 字符串）。
    #[error("invalid shm name")]
    InvalidName,

    /// 段头 magic 不匹配——不是 DLSM 段或已损坏。
    #[error("bad shm header magic (not a DLSM segment or corrupted)")]
    BadMagic,

    /// 段头版本与本二进制不一致。
    #[error("shm header version mismatch: segment={segment}, expected={expected}")]
    VersionMismatch {
        /// 段内记录的版本。
        segment: u32,
        /// 本二进制期望的版本。
        expected: u32,
    },

    /// 固定基址 attach 时目标基址在本进程地址空间已被占用。
    #[error("fixed base address is occupied in this process")]
    BaseAddrUnavailable,
}

impl ArenaError {
    /// 稳定数字错误码（区段 `10000..=19999`）：日志前缀 / 未来 `MySQL` errno 映射。
    #[must_use]
    pub fn code(&self) -> u32 {
        match self {
            Self::Mmap(_) => 10001,
            Self::Munmap(_) => 10002,
            Self::ZeroSize => 10003,
            Self::OutOfMemory { .. } => 10004,
            Self::ShmOpen(_) => 10005,
            Self::Ftruncate(_) => 10006,
            Self::AlreadyExists => 10007,
            Self::NotFound => 10008,
            Self::InvalidName => 10009,
            Self::BadMagic => 10010,
            Self::VersionMismatch { .. } => 10011,
            Self::BaseAddrUnavailable => 10012,
        }
    }
}
