//! 单段 mmap Arena 实现。
//!
//! 两种后备：
//! - **匿名**（`new_anonymous`）：`MAP_ANONYMOUS | MAP_SHARED`，仅 fork 子进程或本进程多线程
//!   可见；分配器游标 `used` 内联在 `Arena` 结构里。
//! - **命名**（`create_named` / `attach_named`）：`shm_open` 后备，任意进程按名字 attach；
//!   段首放 [`ShmHeader`]，分配器游标 `used` 落在 header 内，**跨进程/跨映射共享**。
//!
//! ## 残留段回收
//!
//! 命名段是内核持久的：进程被 `SIGKILL`/崩溃/断电杀死时 [`Drop`] 不会执行，
//! `shm_unlink` 不会被调用，段残留在 `/dev/shm`。段头记录 `owner_pid`，
//! [`Arena::create_or_recover_named`] 与 [`Arena::cleanup_if_stale`] 通过
//! `kill(owner_pid, 0)` 存活检查识别并回收残留段（PostgreSQL postmaster 式启动自愈）。

use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::ffi::{CStr, CString};

use crate::error::ArenaError;

/// 段头 magic：ASCII `DLSM_ARN`。
const HEADER_MAGIC: u64 = 0x444C_534D_5F41_524E;
/// 段头格式版本。
const HEADER_VERSION: u32 = 1;
/// 段头占位大小（对齐到 cache line，数据区从此偏移开始）。
const HEADER_SIZE: usize = 64;

/// `state`：正常运行中（owner 存活时的值）。0=初始化中、2=干净关闭为保留值。
const STATE_ACTIVE: u8 = 1;

/// 命名段段首结构，位于 mmap 区 offset 0。
///
/// 布局必须稳定（`repr(C)`），因为不同进程、未来不同版本可能读它。
#[repr(C)]
struct ShmHeader {
    magic: u64,
    version: u32,
    _pad: u32,
    /// 整段 mmap 长度（含 header）。
    total_size: u64,
    /// 创建者选定的虚拟基址，供固定基址 attach 使用（本增量记录但暂不强制 remap）。
    base_addr: u64,
    /// 创建者进程 PID，残留段回收时做存活检查。
    owner_pid: u64,
    /// 每次创建的随机数，未来配合 pid 文件防 PID 复用误判（本增量仅记录）。
    boot_nonce: u64,
    /// 分配器游标——跨进程共享的真正状态。
    used: AtomicUsize,
    /// 生命周期状态（[`STATE_ACTIVE`] 等），保留供未来 warm-reuse / 崩溃区分。
    state: AtomicU8,
}

const _: () = {
    assert!(core::mem::size_of::<ShmHeader>() <= HEADER_SIZE);
};

/// 分配器游标 `used` 的存放位置。
enum UsedCursor {
    /// 匿名段：内联拥有。
    Inline(AtomicUsize),
    /// 命名段：指向 SHM header 内的 `used`。
    Header(NonNull<AtomicUsize>),
}

/// 段的后备与回收方式。
enum Backing {
    /// 匿名 mmap：drop 时仅 munmap。
    Anonymous {
        map_base: *mut c_void,
        map_len: usize,
    },
    /// 命名 mmap：drop 时 munmap；owner 额外 `shm_unlink`。
    Named {
        map_base: *mut c_void,
        map_len: usize,
        name: CString,
        owner: bool,
    },
}

/// 连续内存区 + 原子 bump 分配器。
///
/// 不支持单次分配的释放，只能整体 drop（与 Bw-Tree epoch GC 模型一致）。
pub struct Arena {
    /// 可用数据区起点（命名段为 mmap 基址 + [`HEADER_SIZE`]；匿名段即 mmap 基址）。
    base: NonNull<u8>,
    /// 可用容量（不含 header）。
    size: usize,
    used: UsedCursor,
    backing: Backing,
}

// SAFETY: `base` 指向 mmap 区，并发推进通过 `used` 的原子操作隔离；裸指针与 CString
// 都是 Send/Sync 安全的元数据。Arena 只暴露线程安全 API。
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    /// 创建一段大小为 `size` 字节的匿名共享内存 Arena（仅 fork/多线程可见）。
    ///
    /// # Errors
    /// - [`ArenaError::ZeroSize`] 当 `size == 0`
    /// - [`ArenaError::Mmap`] 当 `mmap(2)` 失败
    pub fn new_anonymous(size: usize) -> Result<Self, ArenaError> {
        if size == 0 {
            return Err(ArenaError::ZeroSize);
        }

        // SAFETY: 参数为常量且符合 libc 文档；返回值下方做 MAP_FAILED 检查。
        let raw = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(ArenaError::Mmap(std::io::Error::last_os_error()));
        }

        // SAFETY: mmap 成功返回非空地址。
        let base = unsafe { NonNull::new_unchecked(raw.cast::<u8>()) };
        Ok(Self {
            base,
            size,
            used: UsedCursor::Inline(AtomicUsize::new(0)),
            backing: Backing::Anonymous {
                map_base: raw,
                map_len: size,
            },
        })
    }

    /// 创建一段**命名**共享内存 Arena（`shm_open` 后备），可用容量为 `size` 字节。
    ///
    /// 任意知道 `name` 的进程随后可用 [`Self::attach_named`] 挂载。分配器游标存于段头，
    /// 跨进程/跨映射共享。创建者拥有该段：drop 时 `shm_unlink`。
    ///
    /// `name` 不以 `/` 开头时会自动补一个（POSIX SHM 名要求）。
    ///
    /// # Errors
    /// - [`ArenaError::ZeroSize`] / [`ArenaError::InvalidName`]
    /// - [`ArenaError::AlreadyExists`] 当同名段已存在（无论 owner 死活）
    /// - [`ArenaError::ShmOpen`] / [`ArenaError::Ftruncate`] / [`ArenaError::Mmap`]
    pub fn create_named(name: &str, size: usize) -> Result<Self, ArenaError> {
        if size == 0 {
            return Err(ArenaError::ZeroSize);
        }
        let cname = to_shm_name(name)?;
        let fd = open_excl(&cname)?;
        build_fresh(fd, cname, size)
    }

    /// 创建命名段；若同名段残留且其 owner 进程已死，则**回收后重建**。
    ///
    /// 这是 `PostgreSQL` postmaster 式启动自愈：异常中断会把段留在 `/dev/shm`，
    /// 下次启动用本方法即可自动清理崩溃残留。owner 仍存活时返回 [`ArenaError::AlreadyExists`]
    /// （让调用方判定"已有实例运行"）。
    ///
    /// # Errors
    /// 同 [`Self::create_named`]；owner 存活时为 [`ArenaError::AlreadyExists`]。
    pub fn create_or_recover_named(name: &str, size: usize) -> Result<Self, ArenaError> {
        if size == 0 {
            return Err(ArenaError::ZeroSize);
        }
        let cname = to_shm_name(name)?;

        // 最多尝试几轮：每轮要么建成，要么发现残留→unlink→重试。
        for _ in 0..4 {
            match open_excl(&cname) {
                Ok(fd) => return build_fresh(fd, cname, size),
                Err(ArenaError::AlreadyExists) => match segment_is_stale(&cname) {
                    Ok(true) => {
                        // SAFETY: cname 合法；回收已死 owner 的残留段。
                        unsafe { libc::shm_unlink(cname.as_ptr()) };
                        // 继续重试创建
                    }
                    Ok(false) => return Err(ArenaError::AlreadyExists),
                    // 段在检查间隙消失（被别人清了）→ 重试创建。
                    Err(ArenaError::NotFound) => {}
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
        Err(ArenaError::AlreadyExists)
    }

    /// 若同名段存在且其 owner 进程已死，则 `shm_unlink` 之；返回是否清理了。
    ///
    /// 供 `dlsm-greenthread` 等上层在初始化/退出时调用清理崩溃残留。
    /// 段不存在返回 `Ok(false)`；owner 仍存活返回 `Ok(false)`（不动它）。
    ///
    /// # Errors
    /// 底层 `shm_open`/`fstat`/`mmap` 异常时返回对应 [`ArenaError`]。
    pub fn cleanup_if_stale(name: &str) -> Result<bool, ArenaError> {
        let cname = to_shm_name(name)?;
        match segment_is_stale(&cname) {
            Ok(true) => {
                // SAFETY: cname 合法；清理已死 owner 的残留段。
                unsafe { libc::shm_unlink(cname.as_ptr()) };
                Ok(true)
            }
            Ok(false) | Err(ArenaError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// 按名字 attach 一个已存在的命名共享内存 Arena。
    ///
    /// # Errors
    /// - [`ArenaError::InvalidName`] / [`ArenaError::NotFound`] / [`ArenaError::ShmOpen`]
    /// - [`ArenaError::Mmap`]
    /// - [`ArenaError::BadMagic`] / [`ArenaError::VersionMismatch`] 当段头不是合法 DLSM 段
    pub fn attach_named(name: &str) -> Result<Self, ArenaError> {
        let cname = to_shm_name(name)?;

        // SAFETY: cname 合法。
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::ENOENT) {
                ArenaError::NotFound
            } else {
                ArenaError::ShmOpen(e)
            });
        }

        // 取段大小。
        // SAFETY: stat 是 POD，fstat 写入它。
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(ArenaError::ShmOpen(e));
        }
        let total = usize::try_from(st.st_size).unwrap_or(0);
        if total < HEADER_SIZE {
            unsafe { libc::close(fd) };
            return Err(ArenaError::BadMagic);
        }

        // SAFETY: fd 指向 total 字节的段。
        let raw = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        // SAFETY: fd 有效。
        unsafe { libc::close(fd) };
        if raw == libc::MAP_FAILED {
            return Err(ArenaError::Mmap(std::io::Error::last_os_error()));
        }

        // 校验段头。magic/version 是普通整数字段，任何位模式读取都安全。
        let header = raw.cast::<ShmHeader>();
        // SAFETY: raw 指向 >= HEADER_SIZE 字节。
        let magic = unsafe { core::ptr::addr_of!((*header).magic).read() };
        if magic != HEADER_MAGIC {
            // SAFETY: 校验失败，回收本次映射。
            unsafe { libc::munmap(raw, total) };
            return Err(ArenaError::BadMagic);
        }
        // SAFETY: 同上。
        let version = unsafe { core::ptr::addr_of!((*header).version).read() };
        if version != HEADER_VERSION {
            unsafe { libc::munmap(raw, total) };
            return Err(ArenaError::VersionMismatch {
                segment: version,
                expected: HEADER_VERSION,
            });
        }

        Ok(Self::from_named_mapping(raw, total, cname, false))
    }

    /// 由已映射并校验过的命名段构造 `Arena`。
    fn from_named_mapping(raw: *mut c_void, total: usize, name: CString, owner: bool) -> Self {
        let header = raw.cast::<ShmHeader>();
        // SAFETY: header 指向已初始化的 ShmHeader；addr_of_mut 不解引用。
        let used_ptr = unsafe { NonNull::new_unchecked(core::ptr::addr_of_mut!((*header).used)) };
        // SAFETY: raw + HEADER_SIZE 仍在 [raw, raw+total) 内（total >= HEADER_SIZE）。
        let base = unsafe { NonNull::new_unchecked(raw.cast::<u8>().add(HEADER_SIZE)) };
        Self {
            base,
            size: total - HEADER_SIZE,
            used: UsedCursor::Header(used_ptr),
            backing: Backing::Named {
                map_base: raw,
                map_len: total,
                name,
                owner,
            },
        }
    }

    /// 以**只读 + 固定基址**方式 attach 一个命名段，返回 [`ArenaView`]（onstat 式观测）。
    ///
    /// 读取段头记录的 `base_addr`，用 `MAP_FIXED_NOREPLACE` 把整段映射到**相同虚拟基址**
    /// （`PROT_READ`），从而正确解析段内绝对指针；不加任何锁、不写任何字节，非侵入。
    ///
    /// # Errors
    /// - [`ArenaError::NotFound`] / [`ArenaError::ShmOpen`] / [`ArenaError::Mmap`]
    /// - [`ArenaError::BadMagic`] / [`ArenaError::VersionMismatch`]
    /// - [`ArenaError::BaseAddrUnavailable`] 当固定基址在本进程已被占用
    pub fn attach_named_readonly(name: &str) -> Result<ArenaView, ArenaError> {
        let cname = to_shm_name(name)?;

        // SAFETY: cname 合法。
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::ENOENT) {
                ArenaError::NotFound
            } else {
                ArenaError::ShmOpen(e)
            });
        }

        // SAFETY: stat 是 POD。
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(ArenaError::ShmOpen(e));
        }
        let total = usize::try_from(st.st_size).unwrap_or(0);
        if total < HEADER_SIZE {
            unsafe { libc::close(fd) };
            return Err(ArenaError::BadMagic);
        }

        // 第一步：临时只读映射读段头（拿 base_addr / 校验 magic·version）。
        // SAFETY: fd 指向 total 字节。
        let tmp = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                total,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if tmp == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(ArenaError::Mmap(e));
        }
        let header = tmp.cast::<ShmHeader>();
        // SAFETY: tmp 指向 >= HEADER_SIZE 字节；magic/version/base_addr 是普通整数字段。
        let magic = unsafe { core::ptr::addr_of!((*header).magic).read() };
        let version = unsafe { core::ptr::addr_of!((*header).version).read() };
        let base_addr = unsafe { core::ptr::addr_of!((*header).base_addr).read() };
        // SAFETY: 临时映射用完即弃。
        unsafe { libc::munmap(tmp, total) };

        if magic != HEADER_MAGIC {
            unsafe { libc::close(fd) };
            return Err(ArenaError::BadMagic);
        }
        if version != HEADER_VERSION {
            unsafe { libc::close(fd) };
            return Err(ArenaError::VersionMismatch {
                segment: version,
                expected: HEADER_VERSION,
            });
        }

        // 第二步：固定基址只读映射。MAP_FIXED_NOREPLACE 若目标已占用则失败（不覆盖）。
        let want = usize::try_from(base_addr).unwrap_or(0) as *mut c_void;
        // SAFETY: fd 有效；MAP_FIXED_NOREPLACE 保证不覆盖已有映射。
        let raw = unsafe {
            libc::mmap(
                want,
                total,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_FIXED_NOREPLACE,
                fd,
                0,
            )
        };
        // SAFETY: fd 有效，映射独立存活后可关闭。
        unsafe { libc::close(fd) };
        if raw == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                ArenaError::BaseAddrUnavailable
            } else {
                ArenaError::Mmap(e)
            });
        }
        if raw != want {
            // 内核未给到期望基址（理论上 NOREPLACE 要么给要么失败）；防御性处理。
            // SAFETY: 回收本次映射。
            unsafe { libc::munmap(raw, total) };
            return Err(ArenaError::BaseAddrUnavailable);
        }

        // SAFETY: raw + HEADER_SIZE 在 [raw, raw+total) 内。
        let base = unsafe { NonNull::new_unchecked(raw.cast::<u8>().add(HEADER_SIZE)) };
        let used_ptr = unsafe {
            NonNull::new_unchecked(core::ptr::addr_of_mut!((*raw.cast::<ShmHeader>()).used))
        };
        Ok(ArenaView {
            map_base: raw,
            map_len: total,
            base,
            size: total - HEADER_SIZE,
            used: used_ptr,
        })
    }

    /// 取分配器游标的原子引用（屏蔽内联/header 两种来源）。
    #[inline]
    fn used_atomic(&self) -> &AtomicUsize {
        match &self.used {
            UsedCursor::Inline(a) => a,
            // SAFETY: Header 指针在 Arena 存活期间始终指向有效映射内的 ShmHeader.used。
            UsedCursor::Header(p) => unsafe { p.as_ref() },
        }
    }

    /// 可用容量（字节，不含 header）。
    #[inline]
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// 当前已分配字节数。
    #[inline]
    pub fn used(&self) -> usize {
        self.used_atomic().load(Ordering::Acquire)
    }

    /// 剩余可分配字节数。
    #[inline]
    pub fn remaining(&self) -> usize {
        self.size - self.used()
    }

    /// 可用数据区起点。
    #[inline]
    pub fn base_ptr(&self) -> NonNull<u8> {
        self.base
    }

    /// 段的 mmap 基址（命名段即 `ShmHeader.base_addr`）。
    ///
    /// 观测进程按此基址以 `MAP_FIXED` attach（见 [`Self::attach_named_readonly`]），
    /// 从而正确解析段内绝对指针。
    #[inline]
    #[must_use]
    pub fn fixed_base(&self) -> usize {
        self.mapping().0 as usize
    }

    /// 段的 mmap 总长度（含 header）。
    #[inline]
    #[must_use]
    pub fn segment_len(&self) -> usize {
        self.mapping().1
    }

    /// 取底层 mmap 的 (基址, 长度)。
    #[inline]
    fn mapping(&self) -> (*mut c_void, usize) {
        match &self.backing {
            Backing::Anonymous { map_base, map_len }
            | Backing::Named {
                map_base, map_len, ..
            } => (*map_base, *map_len),
        }
    }

    /// 在 Arena 内按 `layout` 对齐 bump 分配一段字节。
    ///
    /// - `size == 0` 返回 `layout.align()` 对齐的占位指针，不消耗容量。
    /// - 多线程/多进程并发安全：`compare_exchange_weak` 循环推进共享游标。
    ///
    /// # Errors
    /// padding + size 超过剩余容量时返回 [`ArenaError::OutOfMemory`]。
    pub fn alloc(&self, layout: Layout) -> Result<NonNull<u8>, ArenaError> {
        if layout.size() == 0 {
            // SAFETY: align 为 2 的幂且 > 0，转指针仅作占位。
            return Ok(unsafe { NonNull::new_unchecked(layout.align() as *mut u8) });
        }

        let size = layout.size();
        let align = layout.align();
        debug_assert!(align.is_power_of_two());
        let mask = align - 1;
        let used = self.used_atomic();

        let mut current = used.load(Ordering::Relaxed);
        loop {
            let aligned = (current.wrapping_add(mask)) & !mask;
            debug_assert!(aligned >= current);

            debug_assert!(aligned.checked_add(size).is_some());
            let new_used = aligned.wrapping_add(size);

            if new_used > self.size || new_used < current {
                return Err(ArenaError::OutOfMemory {
                    requested: new_used.wrapping_sub(current),
                    available: self.size - current,
                });
            }

            match used.compare_exchange_weak(current, new_used, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    // SAFETY: aligned ∈ [0, size-size_req]，base + aligned 在数据区内。
                    let ptr = unsafe { self.base.as_ptr().add(aligned) };
                    // SAFETY: base 非空且偏移不回绕。
                    return Ok(unsafe { NonNull::new_unchecked(ptr) });
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl core::fmt::Debug for Arena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match &self.backing {
            Backing::Anonymous { .. } => "anonymous",
            Backing::Named { owner: true, .. } => "named(owner)",
            Backing::Named { owner: false, .. } => "named(attached)",
        };
        f.debug_struct("Arena")
            .field("kind", &kind)
            .field("capacity", &self.size)
            .field("used", &self.used())
            .finish_non_exhaustive()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        match &self.backing {
            Backing::Anonymous { map_base, map_len } => {
                // SAFETY: 来自构造时成功的 mmap；单次 drop 不会重复 unmap。
                let rc = unsafe { libc::munmap(*map_base, *map_len) };
                debug_assert_eq!(rc, 0, "munmap should not fail on a valid mapping");
            }
            Backing::Named {
                map_base,
                map_len,
                name,
                owner,
            } => {
                // SAFETY: 来自构造时成功的 mmap。
                let rc = unsafe { libc::munmap(*map_base, *map_len) };
                debug_assert_eq!(rc, 0, "munmap should not fail on a valid mapping");
                if *owner {
                    // SAFETY: name 是合法 C 字符串；owner 唯一负责 unlink。
                    unsafe { libc::shm_unlink(name.as_ptr()) };
                }
            }
        }
    }
}

/// 命名段的**只读固定基址视图**（onstat 式观测）。
///
/// 由 [`Arena::attach_named_readonly`] 产出：整段以 `PROT_READ` 映射在与写者相同的固定基址，
/// 因此段内绝对指针可直接解引用。无 `alloc`（不可写），drop 时仅 `munmap`（非 owner，不 unlink）。
///
/// # 安全前提
/// 视图存活期间，被观测段不应被 owner `shm_unlink` 后内核回收（正常运行中 owner 持有该段）。
/// 顺段内绝对指针遍历是安全的（Arena 从不释放单个对象，指针恒落在已映射区），但可能读到
/// 写者并发修改中的**瞬时不一致值**——这对实时状态快照可接受。
#[derive(Debug)]
pub struct ArenaView {
    map_base: *mut c_void,
    map_len: usize,
    base: NonNull<u8>,
    size: usize,
    used: NonNull<AtomicUsize>,
}

// SAFETY: 只读映射的元数据，跨线程传递安全；内容并发安全由"只读 + 写者单写"保证。
unsafe impl Send for ArenaView {}
unsafe impl Sync for ArenaView {}

impl ArenaView {
    /// 可用数据区起点（只读）。
    #[inline]
    #[must_use]
    pub fn base_ptr(&self) -> NonNull<u8> {
        self.base
    }

    /// 可用容量（不含 header）。
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// 映射基址（应等于写者的 [`Arena::fixed_base`]）。
    #[inline]
    #[must_use]
    pub fn fixed_base(&self) -> usize {
        self.map_base as usize
    }

    /// 写者当前已分配字节数（实时快照，可能瞬时不一致）。
    #[inline]
    #[must_use]
    pub fn used(&self) -> usize {
        // SAFETY: used 指向固定基址映射内的 ShmHeader.used，视图存活期间有效。
        unsafe { self.used.as_ref() }.load(Ordering::Acquire)
    }
}

impl Drop for ArenaView {
    fn drop(&mut self) {
        // SAFETY: map_base/map_len 来自成功的 mmap；视图非 owner，只 munmap 不 unlink。
        let rc = unsafe { libc::munmap(self.map_base, self.map_len) };
        debug_assert_eq!(rc, 0, "munmap should not fail on a valid mapping");
    }
}

/// `shm_open` 一个新命名段（`O_CREAT | O_EXCL`），返回 fd。
fn open_excl(cname: &CStr) -> Result<i32, ArenaError> {
    // SAFETY: cname 合法；O_EXCL 保证不复用旧段。
    let fd = unsafe {
        libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        Err(if e.raw_os_error() == Some(libc::EEXIST) {
            ArenaError::AlreadyExists
        } else {
            ArenaError::ShmOpen(e)
        })
    } else {
        Ok(fd)
    }
}

/// 在一个新建 fd 上 `ftruncate` + `mmap` + 写段头，构造拥有所有权的命名 Arena。
///
/// 任一步失败都会清理 fd 与命名段（`close` + `shm_unlink`）。
fn build_fresh(fd: i32, cname: CString, size: usize) -> Result<Arena, ArenaError> {
    let Some(total) = HEADER_SIZE.checked_add(size) else {
        // SAFETY: 失败清理。
        unsafe {
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
        }
        return Err(ArenaError::OutOfMemory {
            requested: size,
            available: usize::MAX - HEADER_SIZE,
        });
    };

    let Ok(total_off) = libc::off_t::try_from(total) else {
        // SAFETY: 失败清理。
        unsafe {
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
        }
        return Err(ArenaError::OutOfMemory {
            requested: total,
            available: usize::try_from(libc::off_t::MAX).unwrap_or(usize::MAX),
        });
    };

    // SAFETY: fd 有效；失败路径清理 fd 与命名段。
    if unsafe { libc::ftruncate(fd, total_off) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
        }
        return Err(ArenaError::Ftruncate(e));
    }

    // SAFETY: fd 指向已 ftruncate 到 total 的段。
    let raw = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    // SAFETY: fd 有效；mmap 后 fd 可关闭。
    unsafe { libc::close(fd) };
    if raw == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        // SAFETY: 创建失败，清理命名段。
        unsafe { libc::shm_unlink(cname.as_ptr()) };
        return Err(ArenaError::Mmap(e));
    }

    // 初始化段头。
    let header = raw.cast::<ShmHeader>();
    // SAFETY: raw 指向 total >= HEADER_SIZE 字节、新映射零页，独占初始化。
    unsafe {
        header.write(ShmHeader {
            magic: HEADER_MAGIC,
            version: HEADER_VERSION,
            _pad: 0,
            total_size: total as u64,
            base_addr: raw as u64,
            owner_pid: u64::from(std::process::id()),
            boot_nonce: gen_nonce(),
            used: AtomicUsize::new(0),
            state: AtomicU8::new(STATE_ACTIVE),
        });
    }

    Ok(Arena::from_named_mapping(raw, total, cname, true))
}

/// 读取命名段的 `owner_pid`，判断该段是否残留（owner 已死或段头损坏）。
///
/// 段不存在返回 [`ArenaError::NotFound`]；段头 magic 不符视为损坏残留（`Ok(true)`）。
fn segment_is_stale(cname: &CStr) -> Result<bool, ArenaError> {
    match peek_owner_pid(cname) {
        Ok(pid) => Ok(!pid_is_alive(pid)),
        // magic 不符 = 不是合法 DLSM 段或半初始化崩溃残留 → 当作可回收。
        Err(ArenaError::BadMagic) => Ok(true),
        Err(e) => Err(e),
    }
}

/// 打开已存在命名段，读出段头 `owner_pid`（校验 magic）。不改动该段。
fn peek_owner_pid(cname: &CStr) -> Result<i64, ArenaError> {
    // SAFETY: cname 合法。
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::ENOENT) {
            ArenaError::NotFound
        } else {
            ArenaError::ShmOpen(e)
        });
    }

    // SAFETY: stat 是 POD。
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(ArenaError::ShmOpen(e));
    }
    let total = usize::try_from(st.st_size).unwrap_or(0);
    if total < HEADER_SIZE {
        unsafe { libc::close(fd) };
        return Err(ArenaError::BadMagic);
    }

    // SAFETY: fd 指向 total 字节。
    let raw = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    // SAFETY: fd 有效。
    unsafe { libc::close(fd) };
    if raw == libc::MAP_FAILED {
        return Err(ArenaError::Mmap(std::io::Error::last_os_error()));
    }

    let header = raw.cast::<ShmHeader>();
    // SAFETY: raw 指向 >= HEADER_SIZE 字节；magic/owner_pid 是普通整数字段。
    let magic = unsafe { core::ptr::addr_of!((*header).magic).read() };
    let owner_pid = unsafe { core::ptr::addr_of!((*header).owner_pid).read() };
    // SAFETY: 仅本次只读 peek，立即释放本映射。
    unsafe { libc::munmap(raw, total) };

    if magic != HEADER_MAGIC {
        return Err(ArenaError::BadMagic);
    }
    Ok(i64::try_from(owner_pid).unwrap_or(-1))
}

/// `kill(pid, 0)` 存活探测：`ESRCH` 为已死，其余（含 `EPERM`）视为存活。
fn pid_is_alive(pid: i64) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if p <= 0 {
        return false;
    }
    // SAFETY: kill 信号 0 仅做存在性检查，不发送实际信号。
    if unsafe { libc::kill(p, 0) } == 0 {
        return true;
    }
    let e = std::io::Error::last_os_error();
    e.raw_os_error() != Some(libc::ESRCH)
}

/// 生成创建随机数（非加密用途，仅作 boot 标识）。
fn gen_nonce() -> u64 {
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    static CTR: AtomicU64 = AtomicU64::new(0);
    let t = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d
            .as_secs()
            .wrapping_mul(1_000_000_000)
            .wrapping_add(u64::from(d.subsec_nanos())),
        Err(_) => 0,
    };
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    t ^ (u64::from(std::process::id()) << 32) ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// 把用户名字规整为合法 POSIX SHM 名（以 `/` 开头、无内嵌 NUL）。
fn to_shm_name(name: &str) -> Result<CString, ArenaError> {
    let normalized = if name.starts_with('/') {
        name.to_owned()
    } else {
        format!("/{name}")
    };
    CString::new(normalized).map_err(|_| ArenaError::InvalidName)
}
