//! Arena 操作错误类型。
//!
//! 错误码区段 `10000..=19999`（见 [`dlsm_core::error`]）。消息一律英文。

dlsm_core::dlsm_error! {
    /// Arena 创建或分配过程中的错误。
    pub enum ArenaError {
        /// `mmap` 系统调用失败。
        10001 Mmap(#[source] std::io::Error) => "mmap failed: {0}",

        /// `munmap` 系统调用失败（仅在 drop 路径上被记录，不向用户返回）。
        10002 Munmap(#[source] std::io::Error) => "munmap failed: {0}",

        /// 请求大小为 0，无意义。
        10003 ZeroSize => "arena size must be non-zero",

        /// 当前剩余容量不足以满足分配（包含 padding）。
        10004 OutOfMemory {
            /// 实际需要消耗的字节数（含对齐 padding）。
            requested: usize,
            /// 当前剩余可分配字节数。
            available: usize,
        } => "out of memory: requested {requested} bytes, available {available}",

        /// `shm_open` 系统调用失败（非"已存在"/"不存在"的其它错误）。
        10005 ShmOpen(#[source] std::io::Error) => "shm_open failed: {0}",

        /// `ftruncate` 系统调用失败。
        10006 Ftruncate(#[source] std::io::Error) => "ftruncate failed: {0}",

        /// 创建命名段时名字已存在（`O_EXCL`）。
        10007 AlreadyExists => "named segment already exists",

        /// attach 命名段时名字不存在。
        10008 NotFound => "named segment not found",

        /// 命名段名字非法（含 NUL，或无法转为 C 字符串）。
        10009 InvalidName => "invalid shm name",

        /// 段头 magic 不匹配——不是 DLSM 段或已损坏。
        10010 BadMagic => "bad shm header magic (not a DLSM segment or corrupted)",

        /// 段头版本与本二进制不一致。
        10011 VersionMismatch {
            /// 段内记录的版本。
            segment: u32,
            /// 本二进制期望的版本。
            expected: u32,
        } => "shm header version mismatch: segment={segment}, expected={expected}",

        /// 固定基址 attach 时目标基址在本进程地址空间已被占用。
        10012 BaseAddrUnavailable => "fixed base address is occupied in this process",
    }
}
