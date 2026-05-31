//! 共享内存 Arena 分配器。
//!
//! 基于 `mmap(MAP_SHARED | MAP_ANONYMOUS)` 创建跨线程/跨进程可见的内存区，
//! 提供原子 bump 分配器与多区域管理（持久区 / 事务区 / 协程私有区）。
//!
//! 详细设计见 `docs/superpowers/specs/2026-05-22-bwtree-design.md`。

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod arena;
mod error;

pub use arena::{Arena, ArenaView};
pub use error::ArenaError;
