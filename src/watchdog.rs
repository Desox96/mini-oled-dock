//! Watchdog: the same exe launched with `--watchdog <pid> <heartbeat-event-name>`, plus the client
//! side in the dock (launch, heartbeat) and the named stop event used by `--restore`.
//!
//! PROTOCOL
//!  * Heartbeat event: name `Local\DockHeartbeat-<pid>` (auto-reset), created by the dock with
//!    `CreateEventW`; the dock signals it every 1 s (`TIMER_SECOND` -> `heartbeat()`).
//!  * The watchdog opens the dock process (`SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`) and
//!    the event, then waits (`WaitForMultipleObjects`) on the process and the event with a ~1.1 s timeout.
//!    If 6 consecutive heartbeats are missed it runs `taskbar::restore_force()` but stays alive as long as
//!    the dock lives (a stuck dock can recover; the dock itself stops hiding the taskbar
//!    if it finds the state file deleted). If the process ends with exit code != 0 (including
//!    abort/crash) it restores and exits; with code 0 it exits doing nothing (the dock has already
//!    restored). It does NOT relaunch the dock. Before restoring it waits up to 30 s for a
//!    `Shell_TrayWnd` (Explorer restarting).
//!  * Startup heartbeat event: `Local\DockHeartbeat-<pid>-startup` (auto-reset), signaled by
//!    `taskbar::init` while waiting for `Shell_TrayWnd`: it resets the count but does NOT move the
//!    watchdog from the startup tolerance (30) to the steady-state one (6), which starts at the first
//!    real heartbeat from `TIMER_SECOND`.
//!  * Single instance: named mutex `Local\DockInstance`, taken by `main` before anything else.
//!  * Stop event: fixed name `Local\DockStop`, created by the dock at startup (manual-reset); a
//!    `stop-watch` thread waits on it and, if it fires, posts `WM_DOCK_QUIT_REQUEST` to the window.
//!    `--restore` signals it with `signal_stop_to_instance()`.
//!
//! THREADING: `spawn`, `heartbeat`, `start_stop_watcher`, `shutdown` from the main/UI thread;
//! `run_watchdog` is the main of the watchdog process (no window, no hook, no COM).

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::thread::JoinHandle;

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    WPARAM,
};
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenEventW, OpenProcess,
    ProcessMemoryPriority, ResetEvent, SetEvent, SetProcessInformation, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, EVENT_MODIFY_STATE, INFINITE,
    MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_VERY_LOW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::log;
use crate::msg::WM_DOCK_QUIT_REQUEST;

/// Number of consecutive missed heartbeats after which the watchdog restores.
pub const MAX_MISSED_HEARTBEATS: u32 = 6;
/// Name of the stop event (manual-reset) shared by the dock and `--restore`.
pub const STOP_EVENT_NAME: &str = "Local\\DockStop";
/// Name of the dock's single-instance mutex.
const INSTANCE_MUTEX_NAME: &str = "Local\\DockInstance";
/// Maximum wait for `Shell_TrayWnd` before a restore by the watchdog.
const RESTORE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Timeout of each watchdog wait (slightly above the heartbeat period).
const WAIT_MS: u32 = 1100;
/// Before the FIRST heartbeat the dock may still be starting (waiting for Shell_TrayWnd, ...):
/// a much wider tolerance (~33 s).
const MAX_MISSED_BEFORE_FIRST: u32 = 30;
/// SYNCHRONIZE access right (waiting on kernel objects).
const SYNCHRONIZE: u32 = 0x0010_0000;

/// Raw handles (isize, 0 = absent) of the dock-side kernel objects.
static HEARTBEAT_EVENT: AtomicIsize = AtomicIsize::new(0);
static STARTUP_EVENT: AtomicIsize = AtomicIsize::new(0);
static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);
static SHUTDOWN_EVENT: AtomicIsize = AtomicIsize::new(0);

/// Name of the heartbeat event for the dock with `pid`: `Local\DockHeartbeat-<pid>`.
pub fn heartbeat_event_name(pid: u32) -> String {
    format!("Local\\DockHeartbeat-{pid}")
}

/// Name of the startup heartbeat event, derived from the heartbeat one.
fn startup_event_name(heartbeat_name: &str) -> String {
    format!("{heartbeat_name}-startup")
}

/// Single instance: creates the named mutex and keeps it for the whole life of the process (the
/// kernel closes it on exit). Returns false if another dock is already running: in that case the caller
/// must exit WITHOUT touching the taskbar (the other instance handles it). If creation fails it
/// logs and carries on (true). Main thread, before `spawn`.
pub fn acquire_single_instance() -> bool {
    // SAFETY: HSTRING name valid for the call; no security attributes, not owned.
    match unsafe { CreateMutexW(None, false, &HSTRING::from(INSTANCE_MUTEX_NAME)) } {
        // The handle is NOT closed: it must stay open as long as the process lives.
        // SAFETY: GetLastError right after CreateMutexW on the same thread.
        Ok(_h) => (unsafe { GetLastError() }) != ERROR_ALREADY_EXISTS,
        Err(e) => {
            log!("watchdog: CreateMutexW(instance) failed: {e} (single-instance check skipped)");
            true
        }
    }
}

fn to_handle(raw: isize) -> HANDLE {
    HANDLE(raw as *mut c_void)
}

/// Closes the handle stored in `slot` (only once) and zeroes the slot.
fn close_slot(slot: &AtomicIsize) {
    let raw = slot.swap(0, Relaxed);
    if raw != 0 {
        // SAFETY: handle created by CreateEventW in this process; the swap guarantees a single close.
        let _ = unsafe { CloseHandle(to_handle(raw)) };
    }
}

// ----------------------------------------------------------- dock side

/// Creates the named events (heartbeat and stop) and launches `current_exe --watchdog <pid> <event>`
/// with `CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW` (fallback to `CREATE_NO_WINDOW` if
/// breakaway is denied). MUST be called BEFORE touching the taskbar. If it fails the dock can
/// continue, but `main` must log it and NOT hide the taskbar (no safety net).
pub fn spawn() -> Result<(), String> {
    // SAFETY: no preconditions.
    let pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    let ev_name = heartbeat_event_name(pid);

    // SAFETY: HSTRING name valid for the call; auto-reset, not signaled.
    let hb = unsafe { CreateEventW(None, false, false, &HSTRING::from(ev_name.as_str())) }
        .map_err(|e| format!("CreateEventW(heartbeat): {e}"))?;
    HEARTBEAT_EVENT.store(hb.0 as isize, Relaxed);

    // Startup heartbeat event (optional: without it, `startup_heartbeat` is a no-op).
    let st_name = startup_event_name(&ev_name);
    // SAFETY: valid HSTRING name; auto-reset, not signaled.
    match unsafe { CreateEventW(None, false, false, &HSTRING::from(st_name.as_str())) } {
        Ok(h) => STARTUP_EVENT.store(h.0 as isize, Relaxed),
        Err(e) => log!("watchdog: CreateEventW(startup) failed: {e}"),
    }

    // SAFETY: valid name; manual-reset, not signaled.
    let stop = match unsafe { CreateEventW(None, true, false, &HSTRING::from(STOP_EVENT_NAME)) } {
        Ok(h) => h,
        Err(e) => {
            close_slot(&HEARTBEAT_EVENT);
            close_slot(&STARTUP_EVENT);
            return Err(format!("CreateEventW(stop): {e}"));
        }
    };
    // SAFETY: GetLastError right after CreateEventW on the same thread.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if existed {
        log!("watchdog: stop event already exists (other instance?)");
    }
    // A possible leftover signal must not make this instance exit immediately.
    // SAFETY: handle just created.
    let _ = unsafe { ResetEvent(stop) };
    STOP_EVENT.store(stop.0 as isize, Relaxed);

    // Internal event (anonymous, manual-reset) to stop the stop-watch in `shutdown`.
    // SAFETY: no name, no security attributes.
    match unsafe { CreateEventW(None, true, false, None) } {
        Ok(h) => SHUTDOWN_EVENT.store(h.0 as isize, Relaxed),
        Err(e) => {
            close_slot(&HEARTBEAT_EVENT);
            close_slot(&STARTUP_EVENT);
            close_slot(&STOP_EVENT);
            return Err(format!("CreateEventW(shutdown): {e}"));
        }
    }

    let result = launch_watchdog(pid, &ev_name);
    if let Err(e) = result {
        close_slot(&HEARTBEAT_EVENT);
        close_slot(&STARTUP_EVENT);
        close_slot(&STOP_EVENT);
        close_slot(&SHUTDOWN_EVENT);
        return Err(e);
    }
    log!("watchdog: started (event {ev_name})");
    Ok(())
}

/// `CreateProcessW` of `current_exe --watchdog <pid> <event>`, with a fallback without breakaway.
fn launch_watchdog(pid: u32, ev_name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe_str = exe.to_string_lossy().into_owned();
    let cmdline = format!("\"{exe_str}\" --watchdog {pid} {ev_name}");
    let app = HSTRING::from(exe_str.as_str());

    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    for (flags, label) in
        [(CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW, "breakaway"), (CREATE_NO_WINDOW, "without breakaway")]
    {
        // CreateProcessW may modify the command line: mutable buffer for each attempt.
        let mut buf: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: `app` and `buf` (NUL-terminated, mutable) live for the whole call; `si` and `pi`
        // are valid structures; no inherited handles, environment and current folder of the parent.
        let r = unsafe {
            CreateProcessW(
                &app,
                Some(PWSTR(buf.as_mut_ptr())),
                None,
                None,
                false,
                flags as PROCESS_CREATION_FLAGS,
                None,
                None,
                &si,
                &mut pi,
            )
        };
        match r {
            Ok(()) => {
                // SAFETY: handles returned by CreateProcessW, not needed by the dock.
                unsafe {
                    let _ = CloseHandle(pi.hProcess);
                    let _ = CloseHandle(pi.hThread);
                }
                return Ok(());
            }
            Err(e) => {
                log!("watchdog: CreateProcessW ({label}) failed: {e}");
                if flags == CREATE_NO_WINDOW {
                    return Err(format!("CreateProcessW: {e}"));
                }
            }
        }
    }
    Err("CreateProcessW: no attempt succeeded".into())
}

/// Signals the heartbeat (`SetEvent`). Does not block; no-op if `spawn` did not succeed.
/// UI thread, `TIMER_SECOND`.
pub fn heartbeat() {
    let raw = HEARTBEAT_EVENT.load(Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: handle valid until `shutdown` closes it (same UI thread).
    let _ = unsafe { SetEvent(to_handle(raw)) };
}

/// Startup heartbeat (`taskbar::init`, waiting for `Shell_TrayWnd`): resets the watchdog count
/// without enabling the steady-state tolerance. Does not block; no-op if the event does not exist.
pub fn startup_heartbeat() {
    let raw = STARTUP_EVENT.load(Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: handle valid until `shutdown` closes it (same thread).
    let _ = unsafe { SetEvent(to_handle(raw)) };
}

/// Starts the `stop-watch` thread: waits on `STOP_EVENT_NAME` and posts `WM_DOCK_QUIT_REQUEST`
/// to `ui_hwnd`. Returns the handle (the thread exits when `shutdown` signals the internal stop).
pub fn start_stop_watcher(ui_hwnd: HWND) -> Option<JoinHandle<()>> {
    let stop = STOP_EVENT.load(Relaxed);
    let quit = SHUTDOWN_EVENT.load(Relaxed);
    if stop == 0 || quit == 0 {
        log!("watchdog: stop-watch not started (events missing)");
        return None;
    }
    // HANDLE/HWND are not Send: they are passed as integers.
    let ui = ui_hwnd.0 as isize;
    match std::thread::Builder::new().name("stop-watch".into()).stack_size(64 * 1024).spawn(move || {
        let handles = [to_handle(stop), to_handle(quit)];
        // SAFETY: the handles stay open until `shutdown` has joined this thread.
        let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if r == WAIT_OBJECT_0 {
            // SAFETY: PostMessageW is thread-safe; if the window is already gone it merely fails.
            let posted =
                unsafe { PostMessageW(Some(HWND(ui as *mut c_void)), WM_DOCK_QUIT_REQUEST, WPARAM(0), LPARAM(0)) };
            if let Err(e) = posted {
                log!("watchdog: PostMessageW(QUIT_REQUEST) failed: {e}");
            }
        } else if r == WAIT_FAILED {
            log!("watchdog: stop-watch wait failed: {:?}", windows::core::Error::from_thread());
        }
    }) {
        Ok(h) => Some(h),
        Err(e) => {
            log!("watchdog: stop-watch thread not started: {e}");
            None
        }
    }
}

/// Normal dock exit: stops the `stop-watch`, closes the handles. The watchdog sees the
/// process end with code 0 and leaves. Call AFTER `taskbar::shutdown()`.
pub fn shutdown(stop_watcher: Option<JoinHandle<()>>) {
    let quit = SHUTDOWN_EVENT.load(Relaxed);
    if quit != 0 {
        // SAFETY: valid handle (closed only further below).
        if let Err(e) = unsafe { SetEvent(to_handle(quit)) } {
            log!("watchdog: SetEvent(shutdown) failed: {e}");
        }
    }
    if let Some(h) = stop_watcher {
        if h.join().is_err() {
            log!("watchdog: stop-watch thread terminated with a panic");
        }
    }
    close_slot(&SHUTDOWN_EVENT);
    close_slot(&STOP_EVENT);
    close_slot(&HEARTBEAT_EVENT);
    close_slot(&STARTUP_EVENT);
}

// ------------------------------------------------------ `--restore` side

/// Opens `STOP_EVENT_NAME` and signals it. Returns true if an instance was running (event
/// existing), false otherwise.
pub fn signal_stop_to_instance() -> bool {
    // SAFETY: valid HSTRING name; if the event does not exist it fails (no instance).
    let ev = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(STOP_EVENT_NAME)) } {
        Ok(h) => h,
        Err(_) => return false,
    };
    // SAFETY: handle just opened with EVENT_MODIFY_STATE.
    let ok = match unsafe { SetEvent(ev) } {
        Ok(()) => true,
        Err(e) => {
            log!("watchdog: SetEvent(stop) failed: {e}");
            false
        }
    };
    // SAFETY: handle opened above, closed only once.
    let _ = unsafe { CloseHandle(ev) };
    ok
}

// ---------------------------------------------------- watchdog process side

/// Main of the `--watchdog <pid> <event>` mode: returns the process exit code.
/// See PROTOCOL in the module doc.
pub fn run_watchdog(dock_pid: u32, event_name: &str) -> i32 {
    // SAFETY: no preconditions; the PID is the one passed by the dock.
    let proc = match unsafe {
        OpenProcess(
            windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION.0),
            false,
            dock_pid,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            // Dock already gone (or inaccessible): better to restore, it is idempotent and protected.
            log!("watchdog: OpenProcess({dock_pid}) failed: {e}, restoring");
            crate::taskbar::restore_force_waiting(RESTORE_WAIT);
            return 1;
        }
    };
    // SAFETY: valid HSTRING name; only SYNCHRONIZE is needed to wait on the event.
    let ev = match unsafe {
        OpenEventW(windows::Win32::System::Threading::SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(event_name))
    } {
        Ok(h) => Some(h),
        Err(e) => {
            log!("watchdog: OpenEventW({event_name}) failed: {e} (no heartbeat check)");
            None
        }
    };
    // Startup heartbeat event (optional: if missing, only the real heartbeat is counted).
    let startup = ev.and_then(|_| {
        let name = startup_event_name(event_name);
        // SAFETY: valid HSTRING name; only SYNCHRONIZE is needed to wait on the event.
        unsafe {
            OpenEventW(
                windows::Win32::System::Threading::SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &HSTRING::from(name.as_str()),
            )
        }
        .ok()
    });
    log!("watchdog: watching dock pid={dock_pid}");

    lower_footprint();
    let restore = || {
        crate::taskbar::restore_force_waiting(RESTORE_WAIT);
        trim_working_set();
    };
    // Indices in the wait array: 0 = process, 1 = heartbeat (if present), 2 = startup (if present).
    let mut handles = vec![proc];
    handles.extend(ev);
    handles.extend(startup);
    let mut missed = 0u32;
    let mut seen_beat = false;
    // true after a restore due to missed heartbeats, until a heartbeat comes back (only one
    // restore per hang).
    let mut stalled = false;
    let code = loop {
        // Without the event only the process is awaited (no timeout: there are no heartbeats to count).
        let timeout = if ev.is_some() { WAIT_MS } else { INFINITE };
        // SAFETY: handles valid for the whole duration of the loop (closed only at the end).
        let r = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if r == WAIT_OBJECT_0 {
            let mut exit = 0u32;
            // SAFETY: `proc` opened with PROCESS_QUERY_LIMITED_INFORMATION; `exit` is writable.
            if let Err(e) = unsafe { GetExitCodeProcess(proc, &mut exit) } {
                log!("watchdog: GetExitCodeProcess failed: {e}, restoring");
                restore();
            } else if exit == 0 {
                log!("watchdog: the dock exited normally");
                // Exit with Explorer absent: restore_force kept the state file, so the
                // taskbar is still in autohide. Wait for Explorer and restore.
                if crate::taskbar::state_file_path().exists() {
                    log!("watchdog: state file still present after exit, restoring the taskbar");
                    restore();
                }
            } else {
                log!("watchdog: the dock ended with code 0x{exit:X}, restoring the taskbar");
                restore();
            }
            break 0;
        } else if r.0 == WAIT_OBJECT_0.0 + 1 && handles.len() >= 2 {
            if stalled {
                log!("watchdog: heartbeat resumed after the hang");
            }
            missed = 0;
            seen_beat = true;
            stalled = false;
        } else if r.0 == WAIT_OBJECT_0.0 + 2 && handles.len() == 3 {
            // Startup heartbeat: the dock is alive but not yet at steady state.
            missed = 0;
        } else if r == WAIT_TIMEOUT {
            missed += 1;
            let limit = if seen_beat { MAX_MISSED_HEARTBEATS } else { MAX_MISSED_BEFORE_FIRST };
            if missed >= limit && !stalled {
                log!("watchdog: {missed} missed heartbeats, restoring the taskbar (still waiting for the dock)");
                // Before restoring, the dock tray host must yield to Explorer. If it does not
                // respond (hung host) the dock must be terminated BEFORE the restore: a hung host
                // stays the first `Shell_TrayWnd` and apps, on the `TaskbarCreated` broadcast, would
                // register their icons with it (timing out) instead of with Explorer.
                if crate::tray_host::yield_foreign_hosts() {
                    terminate_dock(dock_pid);
                    // SAFETY: `proc` opened with SYNCHRONIZE, valid until closed at the end.
                    let w = unsafe { WaitForSingleObject(proc, 2000) };
                    if w != WAIT_OBJECT_0 {
                        log!("watchdog: dock not yet terminated after 2 s ({w:?}), restoring anyway");
                    }
                }
                restore();
                stalled = true;
            }
        } else {
            log!("watchdog: wait failed ({:?}), restoring", windows::core::Error::from_thread());
            restore();
            break 1;
        }
    };
    // SAFETY: handles opened above, closed only once.
    unsafe {
        let _ = CloseHandle(proc);
        for h in ev.into_iter().chain(startup) {
            let _ = CloseHandle(h);
        }
    }
    code
}

/// Marks this process' pages as the first to be repurposed by the memory manager and trims what is
/// resident. Memory priority does not affect CPU scheduling, so restore latency is unchanged.
fn lower_footprint() {
    let info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY_VERY_LOW };
    // SAFETY: pseudo-handle of the current process; `info` is a valid struct of the stated size.
    let r = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            &info as *const MEMORY_PRIORITY_INFORMATION as *const c_void,
            std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    };
    if let Err(e) = r {
        log!("watchdog: SetProcessInformation(memory priority) failed: {e}");
    }
    trim_working_set();
}

/// Empties the working set of the watchdog process (it sleeps almost all the time).
fn trim_working_set() {
    // SAFETY: pseudo-handle of the current process, always valid.
    if let Err(e) = unsafe { EmptyWorkingSet(GetCurrentProcess()) } {
        log!("watchdog: EmptyWorkingSet failed: {e}");
    }
}

/// Terminates the dock process (`TerminateProcess(.., 1)`): used when its tray host does not respond
/// and cannot yield to Explorer. Watchdog process only, before the taskbar restore.
fn terminate_dock(pid: u32) {
    // SAFETY: no preconditions; if the process is already gone or inaccessible it returns Err.
    match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        Ok(h) => {
            // SAFETY: handle just opened with PROCESS_TERMINATE.
            match unsafe { TerminateProcess(h, 1) } {
                Ok(()) => log!("watchdog: dock pid={pid} terminated (hung tray host, before the restore)"),
                Err(e) => log!("watchdog: TerminateProcess({pid}) failed: {e}"),
            }
            // SAFETY: handle opened above, closed only once.
            let _ = unsafe { CloseHandle(h) };
        }
        Err(e) => log!("watchdog: OpenProcess(TERMINATE, {pid}) failed: {e}"),
    }
}
