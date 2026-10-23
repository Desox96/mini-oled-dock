//! Memory diagnostics and trimming.
//!
//! The dock is hidden almost all the time, so after the startup burst (WIC, shell icon
//! extraction, WinRT activation) it is worth giving freed heap blocks and resident pages back
//! to the OS. `trim` does that; `log_memory` writes one line with the numbers to `dock.log`.

use crate::log;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{GetProcessHeaps, HeapCompact, HEAP_FLAGS};
use windows::Win32::System::ProcessStatus::{
    EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetGuiResources, GetProcessHandleCount, GR_GDIOBJECTS, GR_USEROBJECTS,
};

/// Maximum number of process heaps handled by `trim`.
const MAX_HEAPS: usize = 64;

/// Snapshot of the process memory counters (bytes).
struct Counters {
    private: usize,
    working_set: usize,
    peak_working_set: usize,
}

fn counters() -> Option<Counters> {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    c.cb = size;
    // SAFETY: PROCESS_MEMORY_COUNTERS_EX starts with the PROCESS_MEMORY_COUNTERS fields; `cb`
    // is the real size of the struct, which stays writable for the whole call.
    let r = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size,
        )
    };
    match r {
        Ok(()) => Some(Counters {
            private: c.PrivateUsage,
            working_set: c.WorkingSetSize,
            peak_working_set: c.PeakWorkingSetSize,
        }),
        Err(e) => {
            log!("GetProcessMemoryInfo failed: {e}");
            None
        }
    }
}

/// Logs private bytes, working set, peak working set, handle count and GDI/USER object counts.
pub fn log_memory() {
    let Some(c) = counters() else { return };
    let mut handles = 0u32;
    // SAFETY: the pseudo handle is always valid and `handles` is writable for the call.
    if let Err(e) = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) } {
        log!("GetProcessHandleCount failed: {e}");
    }
    // SAFETY: plain query on the current process pseudo handle; returns 0 on failure.
    let (gdi, user) = unsafe {
        (GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS))
    };
    log!(
        "RAM private={} KB  ws={} KB  peak_ws={} KB  handles={handles}  gdi={gdi}  user={user}",
        c.private / 1024,
        c.working_set / 1024,
        c.peak_working_set / 1024
    );
}

/// Compacts every process heap (decommits coalesced free blocks) and empties the working set.
/// Call only while the dock is idle: the next show takes a few soft page faults.
pub fn trim(reason: &str) {
    let before = counters();
    let mut heaps = [HANDLE::default(); MAX_HEAPS];
    // SAFETY: the slice is writable and its length is the buffer capacity passed to the API.
    let n = unsafe { GetProcessHeaps(&mut heaps) } as usize;
    if n == 0 {
        log!("GetProcessHeaps returned no heaps: {}", windows::core::Error::from_thread());
    }
    for h in heaps.iter().take(n.min(MAX_HEAPS)) {
        // SAFETY: handle returned by GetProcessHeaps for this process; compaction is thread-safe
        // on serialized heaps and merely returns 0 on failure.
        let _ = unsafe { HeapCompact(*h, HEAP_FLAGS(0)) };
    }
    // SAFETY: pseudo handle of the current process has the required access rights.
    if let Err(e) = unsafe { EmptyWorkingSet(GetCurrentProcess()) } {
        log!("EmptyWorkingSet failed: {e}");
    }
    match (before, counters()) {
        (Some(b), Some(a)) => log!(
            "trim({reason}): heaps={n} private {} -> {} KB, ws {} -> {} KB",
            b.private / 1024,
            a.private / 1024,
            b.working_set / 1024,
            a.working_set / 1024
        ),
        _ => log!("trim({reason}): heaps={n}"),
    }
}
