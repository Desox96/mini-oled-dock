//! Asynchronous log with rotation.
//!
//! Callers (`log!`) format the line and send it to an unbounded channel: they never block.
//! A dedicated `log` thread writes it to stdout (ONLY if `enable_stdout`: the exe uses the windows
//! subsystem, so it has no console) and (append, flush on every line) to
//! `%ProgramData%\mini-oled-dock\dock.log` (admin-only folder, see `datadir`; opened without
//! following reparse points). When the file exceeds `MAX_LOG_BYTES` (2 MB) it is
//! renamed to `dock.log.1` (replacing the previous one) and a new file is opened.
//! If there is no usable data folder (or the file cannot be opened), lines go to the debugger output
//! (`OutputDebugStringW`, visible with DebugView) besides stdout. The watchdog process uses the same
//! file with the `wd` tag in its lines.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use windows::Win32::System::SystemInformation::GetTickCount64;

/// Log file rotation threshold.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Start tick (ms), for relative timestamps.
static START_TICK: AtomicU64 = AtomicU64::new(0);
/// Channel to the `log` thread; `None` = shut down. Initialized once by `start`.
static LOG_TX: OnceLock<Sender<Option<String>>> = OnceLock::new();
/// true = the `log` thread also writes to stdout (only with an attached console).
static STDOUT_ON: AtomicBool = AtomicBool::new(false);
/// Process role tag ("" for the dock, "wd" for the watchdog), prepended to lines.
static TAG: OnceLock<&'static str> = OnceLock::new();
/// true = synchronous mode (watchdog): no thread, each line is appended straight to the file.
static SYNC: AtomicBool = AtomicBool::new(false);

/// Enables the log copy on stdout (call only after attaching a console).
pub fn enable_stdout() {
    STDOUT_ON.store(true, Relaxed);
}

/// Prints a user message to stdout (CLI mode); errors ignored (no console).
/// Buffer for `say` messages when there is no console (CLI modes launched from UAC/Explorer).
static SAY_BUF: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// From now on `say` accumulates messages instead of writing them to stdout (no console attached).
pub fn buffer_say() {
    if let Ok(mut b) = SAY_BUF.lock() {
        *b = Some(String::new());
    }
}

/// Returns and clears the messages accumulated by `say` after `buffer_say`.
pub fn take_say_buffer() -> String {
    SAY_BUF.lock().ok().and_then(|mut b| b.take()).unwrap_or_default()
}

pub fn say(msg: &str) {
    if let Ok(mut b) = SAY_BUF.lock() {
        if let Some(buf) = b.as_mut() {
            // No console: the message is accumulated and shown in a window at the end of the command.
            buf.push_str(msg);
            buf.push('\n');
            return;
        }
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}

fn now_ms() -> u64 {
    // SAFETY: GetTickCount64 has no preconditions.
    unsafe { GetTickCount64() }
}

/// Path of the log file (`<data root>\dock.log`); `None` if the data folder is not usable or not
/// initialized (`datadir::init`). Never initializes anything (safe from the panic hook).
pub fn log_path() -> Option<PathBuf> {
    crate::datadir::log_path()
}

/// Formats and sends the line: `Sender::send` on an unbounded channel never blocks the caller.
/// Before `start` (or after `stop`) the line is discarded. Use the `log!` macro.
pub fn log_line(args: std::fmt::Arguments<'_>) {
    let ms = now_ms().saturating_sub(START_TICK.load(Relaxed));
    let sync = SYNC.load(Relaxed);
    if !sync && LOG_TX.get().is_none() {
        return;
    }
    let tag = TAG.get().copied().unwrap_or("");
    // Integer timestamp: avoids pulling in the float formatting code.
    let (s, m) = (ms / 1000, ms % 1000);
    let line = if tag.is_empty() { format!("[{s:5}.{m:03}] {args}") } else { format!("[{s:5}.{m:03}] [{tag}] {args}") };
    if sync {
        // Watchdog: rare lines, so open-append-close is cheaper than a dedicated thread.
        write_sync(&line);
    } else if let Some(tx) = LOG_TX.get() {
        // If the log thread already ended the error is ignored.
        let _ = tx.send(Some(line));
    }
}

/// `log!("...", args)`: like `format!`, but writes to the async log.
#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::logger::log_line(format_args!($($a)*)) };
}

/// Writes a line to the log file (append) IMMEDIATELY and synchronously, bypassing the channel.
/// Only for the panic hook (with `panic = "abort"` the `log` thread would not have time to drain).
pub fn write_sync(line: &str) {
    match log_path().and_then(|p| crate::datadir::open_append(&p)) {
        Some(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        None => crate::datadir::debug_out(line),
    }
}

fn open_log_file() -> Option<(std::fs::File, u64)> {
    let path = log_path()?;
    let mut f = crate::datadir::open_append(&path)?;
    let mut len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len >= MAX_LOG_BYTES {
        drop(f);
        rotate(&path);
        f = crate::datadir::open_append(&path)?;
        len = f.metadata().map(|m| m.len()).unwrap_or(0);
    }
    Some((f, len))
}

/// Renames `dock.log` to `dock.log.1` (the old `.1` is replaced). Delete and rename act on the names
/// themselves (never on a link target), inside the admin-only data folder.
fn rotate(path: &std::path::Path) {
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    let old = PathBuf::from(old);
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, &old);
}

/// Starts the `log` thread. `tag` = "" for the dock, "wd" for the watchdog. Must be called once,
/// before any `log!`. Returns the handle to pass to `stop` (`None` in synchronous `wd` mode).
pub fn start(tag: &'static str) -> Option<JoinHandle<()>> {
    START_TICK.store(now_ms(), Relaxed);
    let _ = TAG.set(tag);
    if tag == "wd" {
        // The watchdog logs a handful of lines: no thread, no channel.
        SYNC.store(true, Relaxed);
        return None;
    }
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handle = std::thread::Builder::new()
        .name("log".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut file = open_log_file();
            while let Ok(Some(line)) = rx.recv() {
                // If stdout is closed or slow we do not want a panic: errors ignored.
                if STDOUT_ON.load(Relaxed) {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "{line}");
                    let _ = out.flush();
                }
                let mut rotate_now = false;
                if let Some((f, len)) = file.as_mut() {
                    if writeln!(f, "{line}").is_ok() {
                        let _ = f.flush();
                        *len += line.len() as u64 + 2;
                        rotate_now = *len >= MAX_LOG_BYTES;
                    }
                } else {
                    crate::datadir::debug_out(&line);
                }
                if rotate_now {
                    drop(file.take()); // closes the file before renaming it
                    file = open_log_file();
                }
            }
        })
        .ok()?;
    LOG_TX.set(tx).ok()?;
    Some(handle)
}

/// Flushes and shuts down the `log` thread (the last thing before exiting).
pub fn stop(handle: Option<JoinHandle<()>>) {
    if let (Some(tx), Some(h)) = (LOG_TX.get(), handle) {
        let _ = tx.send(None);
        let _ = h.join();
    }
}
