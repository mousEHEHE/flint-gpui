/// Lock a memory region to prevent it from being swapped to disk.
/// Best-effort: logs a warning on failure but does not return an error.
pub(crate) fn lock_memory(ptr: *const u8, len: usize) {
    #[cfg(unix)]
    lock_unix(ptr, len);
    #[cfg(windows)]
    lock_windows(ptr, len);
}

/// Unlock a previously locked memory region before freeing it.
/// Must be called with the same pointer and length passed to `lock_memory`.
pub(crate) fn unlock_memory(ptr: *const u8, len: usize) {
    #[cfg(unix)]
    unlock_unix(ptr, len);
    #[cfg(windows)]
    unlock_windows(ptr, len);
}

/// Prevent the process from writing core dumps on macOS and Linux.
/// Windows controls dumps through WER and the registry, so nothing is done there.
pub fn disable_core_dumps() {
    #[cfg(unix)]
    disable_core_dumps_unix();
}

#[cfg(unix)]
fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n <= 0 {
        4096
    } else {
        n as usize
    }
}

#[cfg(any(unix, windows))]
fn align_down(addr: usize, page: usize) -> usize {
    addr & !(page - 1)
}

#[cfg(any(unix, windows))]
fn align_up(addr: usize, page: usize) -> usize {
    (addr + page - 1) & !(page - 1)
}

#[cfg(unix)]
fn lock_unix(ptr: *const u8, len: usize) {
    let page = page_size();
    let start = align_down(ptr as usize, page);
    let end = align_up(ptr as usize + len, page);
    let ret = unsafe { libc::mlock(start as *const libc::c_void, end - start) };
    if ret != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "mlock failed; DEK memory page is not locked against swapping"
        );
    }
}

#[cfg(unix)]
fn unlock_unix(ptr: *const u8, len: usize) {
    let page = page_size();
    let start = align_down(ptr as usize, page);
    let end = align_up(ptr as usize + len, page);
    unsafe { libc::munlock(start as *const libc::c_void, end - start) };
}

#[cfg(windows)]
fn page_size_windows() -> usize {
    use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    let mut info = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as usize
}

#[cfg(windows)]
fn lock_windows(ptr: *const u8, len: usize) {
    use windows::Win32::System::Memory::VirtualLock;
    let page = page_size_windows();
    let start = align_down(ptr as usize, page);
    let end = align_up(ptr as usize + len, page);
    if let Err(e) = unsafe { VirtualLock(start as *const _, end - start) } {
        tracing::warn!(
            error = %e,
            "VirtualLock failed; DEK memory page is not locked against swapping"
        );
    }
}

#[cfg(windows)]
fn unlock_windows(ptr: *const u8, len: usize) {
    use windows::Win32::System::Memory::VirtualUnlock;
    let page = page_size_windows();
    let start = align_down(ptr as usize, page);
    let end = align_up(ptr as usize + len, page);
    let _ = unsafe { VirtualUnlock(start as *const _, end - start) };
}

#[cfg(unix)]
fn disable_core_dumps_unix() {
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rlim) };
    if ret != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "setrlimit(RLIMIT_CORE) failed; core dumps may not be suppressed"
        );
    }

    #[cfg(target_os = "linux")]
    mark_not_dumpable_linux();
}

/// A dump piped to a handler such as systemd-coredump ignores the size limit,
/// so the process is also marked as not dumpable.
#[cfg(target_os = "linux")]
fn mark_not_dumpable_linux() {
    let ret = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong) };
    if ret != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "prctl(PR_SET_DUMPABLE) failed; core dumps may not be suppressed"
        );
    }
}
