//! Mini OLED popup (notifications, audio, calendar, power, tray overflow): a layered window
//! SEPARATE from the bar, only one open at a time.
//!
//! WINDOW: class `DockPopup`, `WS_POPUP`, `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`
//! (WITHOUT `WS_EX_NOACTIVATE`: it needs focus for Esc and to know when the user clicks outside). Lives on the
//! UI thread, drawn with `surface::Surface` (no WM_PAINT). Created on first open and
//! reused (hidden on close).
//!
//! CLOSING: `WM_ACTIVATE` with `WA_INACTIVE` (click on another window/desktop, Alt+Tab), Esc,
//! `close()` from the bar (the bar is NOACTIVATE: clicking it does not deactivate the popup, so the UI
//! closes it explicitly), action performed, dock hidden. On every close
//! `WM_DOCK_POPUP_CLOSED` is posted to the dock window.
//!
//! TOGGLE: `toggle(kind)` on the same kind already open closes it; if the popup of that kind was closed
//! by `WA_INACTIVE` less than `TOGGLE_DEBOUNCE_MS` ago it is NOT reopened (it was the same click).
//!
//! THREADING: UI thread only. State is thread-local; no borrow across calls that
//! pump messages (`SetForegroundWindow`, `SetWindowPos`, `ShowWindow`, `UpdateLayeredWindow`).
//! State is ALWAYS taken with `try_borrow_mut` (`with`): an unexpected re-entry is a logged no-op,
//! never a panic (panic = abort).

use std::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow, IsWindow, KillTimer,
    LoadCursorW, PostMessageW, RegisterClassExW, SetCursor, SetTimer, ShowWindow, UnregisterClassW, HTCLIENT,
    IDC_ARROW, IDC_HAND, SW_HIDE, SW_SHOW, WA_INACTIVE, WM_ACTIVATE, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

// Child module via #[path]: this way main.rs does not need to declare it.
#[path = "popup_app.rs"]
pub(crate) mod popup_app;

use crate::audio::{OutputDevice, VolumeState};
use crate::log;
use crate::msg::{TIMER_POPUP_ANIM, TIMER_POPUP_TICK, WM_DOCK_POPUP_CLOSED};
use crate::render::Metrics;
use crate::surface::Surface;

/// A close due to inactivity followed within this time by `toggle` of the same kind = the same click.
pub const TOGGLE_DEBOUNCE_MS: u64 = 200;
/// Distance between the bar and the popup (logical px).
pub const POPUP_GAP: i32 = 6;
/// Minimum margin from the work area edges (logical px).
pub const POPUP_SCREEN_MARGIN: i32 = 8;
/// Duration of the opening fade (ms).
pub const FADE_MS: u64 = 100;

/// `WM_MOUSELEAVE` (defined here to avoid depending on the feature that exports it).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Fade timer step (ms).
const ANIM_STEP_MS: u32 = 15;

const CLASS_NAME: PCWSTR = w!("DockPopup");

/// Popup kind. `as usize` goes into wParam of `WM_DOCK_POPUP_CLOSED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupKind {
    Notifications = 1,
    Audio = 2,
    Calendar = 3,
    Power = 4,
    TrayOverflow = 5,
    AppMenu = 6,
}

/// Where to open the popup. SCREEN coordinates in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    /// Rectangle of the clicked bar item (the popup is horizontally centered on it).
    pub item: RECT,
    /// Bar rectangle (the popup sits above it, or below if `top`, at `POPUP_GAP`).
    pub bar: RECT,
    /// true if the bar is at the top (popup below the bar).
    pub top: bool,
    /// DPI of the bar's monitor.
    pub dpi: u32,
}

/// Data of the app item for the themed context menu (`open_app_menu`).
#[derive(Clone, Debug, Default)]
pub struct AppMenuInfo {
    /// Grouping key of the item (returned by `take_app_menu_key`).
    pub key: String,
    /// Name shown at the top of the menu.
    pub name: String,
    pub pinned: bool,
    /// Number of open windows (0 = "Close" disabled).
    pub windows: usize,
    /// true if the item has a launch target ("New window" enabled).
    pub can_launch: bool,
}

/// Opens the themed context menu for an app item (same style as the popups). The choice reaches the
/// dock window as `WM_DOCK_APP_MENU_CMD` (wParam = `ID_MENU_*`), AFTER the popup closes.
/// If the menu is already open for the same item it closes it (toggle).
pub fn open_app_menu(anchor: Anchor, info: AppMenuInfo) {
    if !is_available() {
        return;
    }
    let now = now_ms();
    let same_key = APP_OPEN_KEY.with(|k| k.try_borrow().map(|g| *g == info.key).unwrap_or(false));
    if current() == Some(PopupKind::AppMenu) && same_key {
        close_inner("toggle");
        return;
    }
    // Closed by focus loss a moment ago on the same item = same click: do not reopen.
    let debounced = with(|st| match st.last_inactive {
        Some((PopupKind::AppMenu, t)) if now.saturating_sub(t) < TOGGLE_DEBOUNCE_MS => {
            st.last_inactive = None;
            same_key
        }
        _ => false,
    })
    .unwrap_or(false);
    if debounced {
        return;
    }
    APP_OPEN_KEY.with(|k| {
        if let Ok(mut g) = k.try_borrow_mut() {
            *g = info.key.clone();
        }
    });
    APP_INFO.with(|i| {
        if let Ok(mut g) = i.try_borrow_mut() {
            *g = Some(info);
        }
    });
    open(PopupKind::AppMenu, anchor);
}

/// Key of the app chosen last in the context menu (consumes it). UI thread.
pub fn take_app_menu_key() -> Option<String> {
    APP_CHOSEN_KEY.with(|k| k.try_borrow_mut().ok().and_then(|mut g| g.take()))
}

/// Data for the app menu content (consumed by `make_content`).
pub(crate) fn take_app_info() -> Option<AppMenuInfo> {
    APP_INFO.with(|i| i.try_borrow_mut().ok().and_then(|mut g| g.take()))
}

/// Stores the chosen key (the content sets it on click, before closing).
pub(crate) fn set_app_menu_key(key: String) {
    APP_CHOSEN_KEY.with(|k| {
        if let Ok(mut g) = k.try_borrow_mut() {
            *g = Some(key);
        }
    });
}

thread_local! {
    /// Key of the item for which the app menu is (or was) open.
    static APP_OPEN_KEY: RefCell<String> = const { RefCell::new(String::new()) };
    /// Info waiting to be handed to the content.
    static APP_INFO: RefCell<Option<AppMenuInfo>> = const { RefCell::new(None) };
    /// Key chosen by the user, waiting for `take_app_menu_key`.
    static APP_CHOSEN_KEY: RefCell<Option<String>> = const { RefCell::new(None) };
}

// ------------------------------------------------------- internal content contract

/// A content's reply to an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// Nothing to do.
    None,
    /// Redraw at the same size.
    Redraw,
    /// Recompute size and position, then redraw (e.g. notification list changed).
    Relayout,
    /// Close the popup (action performed).
    Close,
}

impl Reply {
    /// The "stronger" reply of the two.
    fn merge(self, other: Reply) -> Reply {
        fn rank(r: Reply) -> u8 {
            match r {
                Reply::None => 0,
                Reply::Redraw => 1,
                Reply::Relayout => 2,
                Reply::Close => 3,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// Button other than the left one (tray overflow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Right,
    Middle,
}

/// Data arriving from outside while the popup is open (forwarded to the current content).
#[derive(Clone, Debug)]
pub(crate) enum DataEvent {
    Volume(VolumeState),
    AudioOutputs(Vec<OutputDevice>),
    NotifChanged,
    TrayChanged,
}

/// A popup content. Client coordinates of the popup, physical pixels. `hot`/`pressed` are the ids of the
/// controls being hovered/pressed (returned by `hit`). Internal to "popups": may be changed.
pub(crate) trait Content {
    fn kind(&self) -> PopupKind;
    /// Computes (and caches) the layout given the surface's metrics/fonts; returns the
    /// desired size (physical px).
    fn size(&mut self, s: &Surface) -> (i32, i32);
    /// Draws everything (panel included).
    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>);
    /// Control under the point.
    fn hit(&self, x: i32, y: i32) -> Option<u32>;
    /// true if control `id` shows the hand cursor.
    fn clickable(&self, _id: u32) -> bool {
        true
    }
    /// Left click completed on control `id`.
    fn click(&mut self, id: u32) -> Reply;
    /// Left button released after a press on `id` (always, even outside the control).
    fn release(&mut self, _id: u32) -> Reply {
        Reply::None
    }
    /// Right/middle click completed on control `id`.
    fn button(&mut self, _id: u32, _which: Button) -> Reply {
        Reply::None
    }
    /// Wheel (whole notches, positive = up) over the point `(x, y)`.
    fn wheel(&mut self, _notches: i32, _x: i32, _y: i32) -> Reply {
        Reply::None
    }
    /// Drag with the button held on `id` (volume slider).
    fn drag(&mut self, _id: u32, _x: i32, _y: i32) -> Reply {
        Reply::None
    }
    /// 1 s tick (`TIMER_POPUP_TICK`).
    fn tick(&mut self) -> Reply {
        Reply::None
    }
    /// External data updated.
    fn data(&mut self, _ev: &DataEvent) -> Reply {
        Reply::None
    }
    /// The popup is closing (e.g. `notif::set_popup_open(false)`).
    fn closing(&mut self) {}
    /// The popup is already hidden: run the actions to perform AFTER closing here.
    fn after_close(&mut self) {}
}

/// Clickable zones of a content (client coordinates, physical px). The last one added wins.
#[derive(Default)]
pub(crate) struct HitMap(Vec<(u32, RECT)>);

impl HitMap {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn add(&mut self, id: u32, rc: RECT) {
        self.0.push((id, rc));
    }

    pub(crate) fn at(&self, x: i32, y: i32) -> Option<u32> {
        self.0.iter().rev().find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom).map(|(id, _)| *id)
    }

    pub(crate) fn rect(&self, id: u32) -> Option<RECT> {
        self.0.iter().rev().find(|(i, _)| *i == id).map(|(_, r)| *r)
    }
}

// ------------------------------------------------------------------ state

/// Open popup.
struct Open {
    kind: PopupKind,
    content: Box<dyn Content>,
    /// `None` only while the surface is temporarily out for `present`.
    surf: Option<Surface>,
    anchor: Anchor,
    /// Top-left corner in screen coordinates.
    pos: (i32, i32),
    hot: Option<u32>,
    pressed: Option<u32>,
    tracking: bool,
    fade_start: u64,
    alpha: u8,
    /// Foreground window at open time (tick safety net).
    fg_at_open: HWND,
    /// true once the popup has been seen as the foreground window.
    was_active: bool,
    wheel_acc: i32,
}

struct State {
    hwnd: HWND,
    available: bool,
    registered: bool,
    open: Option<Open>,
    /// Last close due to `WA_INACTIVE`: (kind, time in ms) for the `toggle` debounce.
    last_inactive: Option<(PopupKind, u64)>,
    /// Last `VolumeState` received (even while the popup is closed).
    last_volume: VolumeState,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        hwnd: HWND::default(),
        available: false,
        registered: false,
        open: None,
        last_inactive: None,
        last_volume: VolumeState::default(),
    });
}

/// Runs `f` on the state. `None` if the state is already borrowed (re-entry): never a panic.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut g) => Some(f(&mut g)),
        Err(_) => {
            log!("popup: state already in use (re-entry), operation skipped");
            None
        }
    })
}

fn now_ms() -> u64 {
    // SAFETY: no preconditions.
    unsafe { GetTickCount64() }
}

// ------------------------------------------------------------------ public API

/// Registers the `DockPopup` class. Called by `main` after `ui::attach`. Error = no popups
/// (the UI falls back to the direct (non-popup) actions).
pub fn init() -> Result<(), String> {
    // SAFETY: no preconditions.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: HINSTANCE(module.0),
        // SAFETY: loads a system cursor (null hInstance), no pointers to manage.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialized; the class name is a static literal.
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err("RegisterClassExW(DockPopup) failed".into());
    }
    with(|st| {
        st.registered = true;
        st.available = true;
    });
    log!("popup::init: DockPopup class registered");
    Ok(())
}

/// true if `init` succeeded (class registered). false = the UI uses the direct (non-popup) actions
/// (bell = Win+N, volume = mute, clock = ms-settings:dateandtime, power = native menu
/// "Exit dock").
pub fn is_available() -> bool {
    STATE.with(|s| s.try_borrow().map(|g| g.available).unwrap_or(false))
}

/// Opens popup `kind` anchored to `anchor` (first closing any other popup), or closes it
/// if the same kind is already open (see TOGGLE in the module doc).
pub fn toggle(kind: PopupKind, anchor: Anchor) {
    #[derive(PartialEq)]
    enum Plan {
        Skip,
        Close,
        Open,
    }
    let now = now_ms();
    let plan = with(|st| {
        if !st.available {
            return Plan::Skip;
        }
        if st.open.as_ref().map(|o| o.kind) == Some(kind) {
            return Plan::Close;
        }
        if let Some((k, t)) = st.last_inactive {
            if k == kind && now.saturating_sub(t) < TOGGLE_DEBOUNCE_MS {
                st.last_inactive = None;
                return Plan::Skip;
            }
        }
        Plan::Open
    })
    .unwrap_or(Plan::Skip);
    match plan {
        Plan::Skip => {}
        Plan::Close => close_inner("toggle"),
        Plan::Open => open(kind, anchor),
    }
}

/// Closes the open popup (no-op if none). `reason` is for the log only.
pub fn close(reason: &str) {
    close_inner(reason);
}

/// true if a popup is open (the UI suspends auto-hide).
pub fn is_open() -> bool {
    STATE.with(|s| s.try_borrow().map(|g| g.open.is_some()).unwrap_or(false))
}

/// Kind of the open popup (the UI keeps the plate on the matching item).
pub fn current() -> Option<PopupKind> {
    STATE.with(|s| s.try_borrow().ok().and_then(|g| g.open.as_ref().map(|o| o.kind)))
}

/// `WM_DOCK_VOLUME` reached the UI: updates the audio popup if open. Called by `ui`.
pub fn on_volume(st: VolumeState) {
    with(|s| s.last_volume = st);
    dispatch(DataEvent::Volume(st));
}

/// `ShellEvent::AudioOutputs` reached the UI. Called by `ui`.
pub fn on_audio_outputs(list: Vec<OutputDevice>) {
    if is_open() {
        dispatch(DataEvent::AudioOutputs(list));
    }
}

/// `WM_DOCK_NOTIF`: re-reads `notif::snapshot()` if the notifications popup is open. Called by `main`.
pub fn on_notif_changed() {
    dispatch(DataEvent::NotifChanged);
}

/// `WM_DOCK_TRAY_CHANGED`: updates the overflow popup if open. Called by `main`.
pub fn on_tray_changed() {
    dispatch(DataEvent::TrayChanged);
}

/// Destroys the popup window and the surface, unregisters the class. In `cleanup`, before
/// `ui::shutdown`. Idempotent.
pub fn shutdown() {
    close_inner("shutdown");
    let Some((hwnd, registered)) = with(|st| {
        let r = (st.hwnd, st.registered);
        st.hwnd = HWND::default();
        st.registered = false;
        st.available = false;
        r
    }) else {
        return;
    };
    if !hwnd.0.is_null() {
        // SAFETY: window created by this module on the UI thread.
        if let Err(e) = unsafe { DestroyWindow(hwnd) } {
            log!("popup: DestroyWindow failed: {e}");
        }
    }
    if registered {
        // SAFETY: class registered by `init` with this same module; no window survives.
        let inst = unsafe { GetModuleHandleW(PCWSTR::null()) }.map(|m| HINSTANCE(m.0)).ok();
        let _ = unsafe { UnregisterClassW(CLASS_NAME, inst) };
    }
}

// ------------------------------------------------------------------ open / close

/// Builds the content for `kind`.
fn make_content(kind: PopupKind, volume: VolumeState) -> Box<dyn Content> {
    match kind {
        PopupKind::Notifications => crate::popup_notif::new(),
        PopupKind::Audio => crate::popup_audio::new(volume),
        PopupKind::Calendar => crate::popup_calendar::new(),
        PopupKind::Power => crate::popup_power::new(),
        PopupKind::TrayOverflow => crate::popup_tray::new(),
        PopupKind::AppMenu => popup_app::new(take_app_info().unwrap_or_default()),
    }
}

/// Popup window, created on first request and reused.
fn ensure_window() -> Option<HWND> {
    let existing = with(|st| st.hwnd)?;
    // SAFETY: IsWindow accepts any handle, even null.
    if !existing.0.is_null() && unsafe { IsWindow(Some(existing)) }.as_bool() {
        return Some(existing);
    }
    // SAFETY: no preconditions.
    let module = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(m) => m,
        Err(e) => {
            log!("popup: GetModuleHandleW failed: {e}");
            return None;
        }
    };
    let owner = crate::hwnd();
    let owner = if owner.0.is_null() { None } else { Some(owner) };
    // SAFETY: class registered by `init`; the PCWSTRs are static literals; the owner is the dock
    // window (same thread) or none.
    let r = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("Dock popup"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            owner,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    };
    match r {
        Ok(h) => {
            with(|st| st.hwnd = h);
            Some(h)
        }
        Err(e) => {
            log!("popup: CreateWindowExW failed: {e}");
            with(|st| st.available = false);
            None
        }
    }
}

/// Work area of the monitor containing `bar`.
fn work_area(bar: &RECT) -> RECT {
    // SAFETY: `bar` is valid; MONITOR_DEFAULTTONEAREST always returns a monitor.
    let mon = unsafe { MonitorFromRect(bar, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `mi` has the correct cbSize.
    if unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
        mi.rcWork
    } else {
        *bar
    }
}

/// Top-left corner position of a `w` x `h` popup.
fn compute_pos(a: &Anchor, w: i32, h: i32) -> (i32, i32) {
    let m = Metrics { dpi: a.dpi };
    let work = work_area(&a.bar);
    let margin = m.px(POPUP_SCREEN_MARGIN);
    let gap = m.px(POPUP_GAP);
    let cx = (a.item.left + a.item.right) / 2;
    let (lo, hi) = (work.left + margin, work.right - margin - w);
    let x = if hi < lo { lo } else { (cx - w / 2).clamp(lo, hi) };
    let y = if a.top { a.bar.bottom + gap } else { a.bar.top - gap - h };
    let (ylo, yhi) = (work.top + margin, work.bottom - margin - h);
    let y = if yhi < ylo { ylo } else { y.clamp(ylo, yhi) };
    (x, y)
}

fn open(kind: PopupKind, anchor: Anchor) {
    // Any other popup closes without a fade.
    close_inner("popup switch");
    let Some(hwnd) = ensure_window() else { return };
    let metrics = Metrics { dpi: anchor.dpi.max(96) };
    let surf = match Surface::new(metrics) {
        Ok(s) => s,
        Err(e) => {
            log!("popup: Surface::new failed: {e}");
            return;
        }
    };
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    let Some(volume) = with(|st| st.last_volume) else { return };
    let content = make_content(kind, volume);
    debug_assert_eq!(content.kind(), kind);
    let installed = with(|st| {
        st.last_inactive = None;
        st.open = Some(Open {
            kind,
            content,
            surf: Some(surf),
            anchor,
            pos: (0, 0),
            hot: None,
            pressed: None,
            tracking: false,
            fade_start: now_ms(),
            alpha: 0,
            fg_at_open: fg,
            was_active: false,
            wheel_acc: 0,
        });
    });
    if installed.is_none() {
        return;
    }
    if !render(true) {
        close_inner("initial render failed");
        return;
    }
    log!("popup: opening {kind:?}");
    // No borrow during ShowWindow / force_foreground: they can deliver messages to the popup.
    // SAFETY: `hwnd` is our popup window.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    crate::ui::force_foreground(hwnd);
    // SAFETY: `hwnd` is our window; the callbacks are in its wnd_proc.
    unsafe {
        if SetTimer(Some(hwnd), TIMER_POPUP_ANIM, ANIM_STEP_MS, None) == 0 {
            log!("popup: SetTimer(anim) failed");
        }
        if SetTimer(Some(hwnd), TIMER_POPUP_TICK, 1000, None) == 0 {
            log!("popup: SetTimer(tick) failed");
        }
    }
    with(|st| {
        if let Some(o) = st.open.as_mut() {
            o.fade_start = now_ms();
        }
    });
}

/// Closes the open popup. Idempotent.
/// Time (GetTickCount64) of the last popup close, for `closed_within`.
static LAST_CLOSE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// true if a popup closed within the last `ms` milliseconds.
pub fn closed_within(ms: u64) -> bool {
    // SAFETY: GetTickCount64 has no preconditions.
    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    let t = LAST_CLOSE_MS.load(std::sync::atomic::Ordering::Relaxed);
    t != 0 && now.saturating_sub(t) <= ms
}

fn close_inner(reason: &str) {
    let Some((taken, hwnd)) = with(|st| (st.open.take(), st.hwnd)) else { return };
    let Some(mut o) = taken else { return };
    log!("popup: closing {:?} ({reason})", o.kind);
    // SAFETY: GetTickCount64 has no preconditions.
    let tick = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    LAST_CLOSE_MS.store(tick, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: `hwnd` is our window (or null: then the calls just fail).
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_POPUP_ANIM);
        let _ = KillTimer(Some(hwnd), TIMER_POPUP_TICK);
    }
    if o.pressed.is_some() {
        // SAFETY: releases any mouse capture taken by this popup.
        let _ = unsafe { ReleaseCapture() };
    }
    o.content.closing();
    if !hwnd.0.is_null() {
        // SAFETY: `hwnd` is our window; the state is not borrowed.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
    o.content.after_close();
    let kind = o.kind;
    drop(o);
    // SAFETY: PostMessageW to the dock window; if it no longer exists it just fails.
    if let Err(e) = unsafe { PostMessageW(Some(crate::hwnd()), WM_DOCK_POPUP_CLOSED, WPARAM(kind as usize), LPARAM(0)) }
    {
        log!("popup: PostMessageW(WM_DOCK_POPUP_CLOSED) failed: {e}");
    }
}

/// Redraws the open popup; with `relayout` it first recomputes size and position. false if there
/// is nothing left to show or rendering fails.
fn render(relayout: bool) -> bool {
    // The surface leaves the state for the whole of `present`: no borrow during UpdateLayeredWindow.
    let prepared = with(|st| {
        let hwnd = st.hwnd;
        let o = st.open.as_mut()?;
        let mut surf = o.surf.take()?;
        // A previous failed resize leaves the surface without a DIB (0 x 0): retry the layout.
        if relayout || surf.size() == (0, 0) {
            let (w, h) = o.content.size(&surf);
            if let Err(e) = surf.resize(w.max(1), h.max(1)) {
                log!("popup: resize failed: {e}");
                // The surface goes back into the state: the next render retries instead of staying
                // frozen forever.
                o.surf = Some(surf);
                return None;
            }
            o.pos = compute_pos(&o.anchor, w.max(1), h.max(1));
        }
        o.content.paint(&mut surf, o.hot, o.pressed);
        Some((hwnd, surf, o.pos, o.alpha))
    })
    .flatten();
    let Some((hwnd, surf, pos, alpha)) = prepared else { return false };
    let ok = surf.present(hwnd, pos.0, pos.1, alpha);
    with(|st| {
        if let Some(o) = st.open.as_mut() {
            o.surf = Some(surf);
        }
    });
    ok
}

fn apply(reply: Reply) {
    match reply {
        Reply::None => {}
        Reply::Redraw => {
            render(false);
        }
        Reply::Relayout => {
            render(true);
        }
        Reply::Close => close_inner("action"),
    }
}

/// Forwards external data to the open content.
fn dispatch(ev: DataEvent) {
    let reply = with(|st| st.open.as_mut().map(|o| o.content.data(&ev))).flatten();
    if let Some(r) = reply {
        apply(r);
    }
}

// ------------------------------------------------------------------ window

fn lo_i16(v: isize) -> i32 {
    (v & 0xFFFF) as u16 as i16 as i32
}

fn hi_i16(v: isize) -> i32 {
    ((v >> 16) & 0xFFFF) as u16 as i16 as i32
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                on_inactive();
            } else {
                mark_active(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE.0 as usize {
                close_inner("Esc");
                return LRESULT(0);
            }
            // SAFETY: forwarding to the default handler with the same arguments.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, lo_i16(lp.0), hi_i16(lp.0));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            on_mouse_leave();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_lbutton_down(hwnd, lo_i16(lp.0), hi_i16(lp.0));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_lbutton_up(lo_i16(lp.0), hi_i16(lp.0));
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            on_other_button(Button::Right, lo_i16(lp.0), hi_i16(lp.0));
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            on_other_button(Button::Middle, lo_i16(lp.0), hi_i16(lp.0));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            on_wheel(hwnd, wp, lp);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lp.0 & 0xFFFF) as u32 == HTCLIENT && on_set_cursor(hwnd) {
                return LRESULT(1);
            }
            // SAFETY: forwarding to the default handler with the same arguments.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_POPUP_ANIM => on_anim(hwnd),
                TIMER_POPUP_TICK => on_tick(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        // The popup uses the bar's DPI, not its own.
        WM_DPICHANGED => LRESULT(0),
        WM_ERASEBKGND => LRESULT(1),
        // SAFETY: forwarding to the default handler with the same arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn mark_active(hwnd: HWND) {
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    if fg == hwnd {
        with(|st| {
            if let Some(o) = st.open.as_mut() {
                o.was_active = true;
            }
        });
    }
}

fn on_inactive() {
    let kind = with(|st| st.open.as_ref().map(|o| o.kind)).flatten();
    if let Some(k) = kind {
        with(|st| st.last_inactive = Some((k, now_ms())));
        close_inner("focus lost");
    }
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let r = with(|st| {
        let o = st.open.as_mut()?;
        let hit = o.content.hit(x, y);
        let mut reply = Reply::None;
        if let Some(id) = o.pressed {
            reply = o.content.drag(id, x, y);
            let nh = if hit == Some(id) { hit } else { None };
            if nh != o.hot {
                o.hot = nh;
                reply = reply.merge(Reply::Redraw);
            }
        } else if hit != o.hot {
            o.hot = hit;
            reply = Reply::Redraw;
        }
        let need = !o.tracking;
        o.tracking = true;
        Some((reply, need))
    })
    .flatten();
    let Some((reply, need_track)) = r else { return };
    if need_track {
        let mut t = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: `t` is initialized with the correct cbSize; `hwnd` is our window.
        if let Err(e) = unsafe { TrackMouseEvent(&mut t) } {
            log!("popup: TrackMouseEvent failed: {e}");
            with(|st| {
                if let Some(o) = st.open.as_mut() {
                    o.tracking = false;
                }
            });
        }
    }
    apply(reply);
}

fn on_mouse_leave() {
    let changed = with(|st| {
        let o = st.open.as_mut()?;
        o.tracking = false;
        if o.pressed.is_none() && o.hot.is_some() {
            o.hot = None;
            return Some(true);
        }
        Some(false)
    })
    .flatten();
    if changed == Some(true) {
        apply(Reply::Redraw);
    }
}

fn on_lbutton_down(hwnd: HWND, x: i32, y: i32) {
    let r = with(|st| {
        let o = st.open.as_mut()?;
        let hit = o.content.hit(x, y)?;
        o.pressed = Some(hit);
        o.hot = Some(hit);
        let reply = o.content.drag(hit, x, y).merge(Reply::Redraw);
        Some(reply)
    })
    .flatten();
    let Some(reply) = r else { return };
    // SAFETY: `hwnd` is our window; capture is released when the button is released.
    let _ = unsafe { SetCapture(hwnd) };
    apply(reply);
}

fn on_lbutton_up(x: i32, y: i32) {
    let r = with(|st| {
        let o = st.open.as_mut()?;
        let id = o.pressed.take()?;
        let hit = o.content.hit(x, y);
        o.hot = hit;
        let mut reply = o.content.release(id).merge(Reply::Redraw);
        if hit == Some(id) {
            reply = reply.merge(o.content.click(id));
        }
        Some(reply)
    })
    .flatten();
    // SAFETY: releases the capture taken in `on_lbutton_down` (harmless if there is none).
    let _ = unsafe { ReleaseCapture() };
    if let Some(reply) = r {
        apply(reply);
    }
}

fn on_other_button(which: Button, x: i32, y: i32) {
    let r = with(|st| {
        let o = st.open.as_mut()?;
        let id = o.content.hit(x, y)?;
        Some(o.content.button(id, which))
    })
    .flatten();
    if let Some(reply) = r {
        apply(reply);
    }
}

fn on_wheel(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    // The wheel carries SCREEN coordinates (not client).
    let mut pt = POINT { x: lo_i16(lp.0), y: hi_i16(lp.0) };
    // SAFETY: `hwnd` is our window, `pt` is writable.
    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
    let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let r = with(|st| {
        let o = st.open.as_mut()?;
        // High-resolution wheels: accumulate up to 120 (WHEEL_DELTA). A change of direction resets.
        if o.wheel_acc.signum() * delta.signum() < 0 {
            o.wheel_acc = 0;
        }
        o.wheel_acc += delta;
        let n = o.wheel_acc / 120;
        o.wheel_acc -= n * 120;
        if n == 0 {
            return None;
        }
        Some(o.content.wheel(n, pt.x, pt.y))
    })
    .flatten();
    if let Some(reply) = r {
        apply(reply);
    }
}

/// true if the cursor was set.
fn on_set_cursor(hwnd: HWND) -> bool {
    let mut pt = POINT::default();
    // SAFETY: `pt` is writable.
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return false;
    }
    // SAFETY: `hwnd` is our window, `pt` is writable.
    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
    let hand = with(|st| {
        let o = st.open.as_ref()?;
        let id = o.content.hit(pt.x, pt.y)?;
        Some(o.content.clickable(id))
    })
    .flatten()
    .unwrap_or(false);
    // SAFETY: loads a system cursor (null hInstance) and sets it.
    unsafe {
        let c = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).unwrap_or_default();
        SetCursor(Some(c));
    }
    true
}

fn on_anim(hwnd: HWND) {
    let done = with(|st| {
        let o = st.open.as_mut()?;
        let el = now_ms().saturating_sub(o.fade_start);
        let a = ((el * 255) / FADE_MS).min(255) as u8;
        o.alpha = a;
        Some(a == 255)
    })
    .flatten();
    let Some(done) = done else {
        // SAFETY: timer of our window; no popup open = nothing to animate.
        let _ = unsafe { KillTimer(Some(hwnd), TIMER_POPUP_ANIM) };
        return;
    };
    render(false);
    if done {
        // SAFETY: timer of our window.
        let _ = unsafe { KillTimer(Some(hwnd), TIMER_POPUP_ANIM) };
    }
}

fn on_tick(hwnd: HWND) {
    let r = with(|st| {
        let o = st.open.as_mut()?;
        Some((o.content.tick(), o.was_active, o.fg_at_open))
    })
    .flatten();
    let Some((reply, was_active, fg_at_open)) = r else { return };
    if !was_active {
        // Safety net: the foreground steal can fail and the popup would never receive WA_INACTIVE.
        // SAFETY: no preconditions.
        let fg = unsafe { GetForegroundWindow() };
        if fg == hwnd {
            with(|st| {
                if let Some(o) = st.open.as_mut() {
                    o.was_active = true;
                }
            });
        } else if fg != fg_at_open {
            close_inner("foreground changed");
            return;
        }
    }
    apply(reply);
}

// ------------------------------------------------------------------ helpers for the contents

/// Hover/pressed plate of a row (none if `id` is not hovered). Returns true if `id` is
/// hovered: the caller brightens text/glyphs with `lift`.
pub(crate) fn row_plate(s: &mut Surface, rc: RECT, id: u32, hot: Option<u32>, pressed: Option<u32>) -> bool {
    if hot != Some(id) {
        return false;
    }
    let a = if pressed == Some(id) { crate::surface::PRESS_ALPHA } else { crate::surface::HOVER_ALPHA };
    let r = s.metrics().px(crate::surface::ROW_RADIUS);
    s.plate(rc, r, a);
    true
}

/// Alpha of a row's text/glyph: under the mouse it goes up one step (max 255).
pub(crate) fn lift(base: u32, hot: bool) -> u32 {
    if hot {
        (base + 25).min(255)
    } else {
        base
    }
}

/// Switch shared by the popups (32x18, knob 12, margin 3), top-left corner `(x, y)`.
/// On: white track and black knob. Off: dim track with a 1 px outline and gray knob.
pub(crate) fn toggle_switch(s: &mut Surface, x: i32, y: i32, on: bool) {
    let m = s.metrics();
    let (w, h, knob, inset) = (m.px(32), m.px(18), m.px(12), m.px(3));
    let track = crate::surface::rect_wh(x, y, w, h);
    let ky = y + (h - knob) / 2;
    if on {
        s.fill_rr(track, h / 2, crate::surface::WHITE, 235);
        s.fill_rr(crate::surface::rect_wh(x + w - inset - knob, ky, knob, knob), knob / 2, (0, 0, 0), 255);
    } else {
        s.fill_rr(track, h / 2, crate::surface::WHITE, 40);
        s.stroke_rr(track, h / 2, m.px(1).max(1), 70);
        s.fill_rr(crate::surface::rect_wh(x + inset, ky, knob, knob), knob / 2, crate::surface::WHITE, 160);
    }
}

/// Horizontal separator (1 logical px thick) between `x0` and `x1` at ordinate `y`.
pub(crate) fn hline(s: &mut Surface, x0: i32, x1: i32, y: i32) {
    let t = s.metrics().px(1).max(1);
    s.fill_rr(RECT { left: x0, top: y, right: x1, bottom: y + t }, 0, crate::surface::WHITE, crate::surface::SEP_ALPHA);
}

/// Y coordinate to vertically center a line of height `lh` inside `rc`.
pub(crate) fn center_y(rc: &RECT, lh: i32) -> i32 {
    rc.top + ((rc.bottom - rc.top) - lh) / 2
}

/// Opens `target` (e.g. `ms-settings:...`) on the shell thread.
pub(crate) fn launch(target: &str) {
    crate::shell_thread::request(crate::shell_thread::ShellRequest::Launch { target: target.to_string(), args: None });
}

/// Monotonic milliseconds (for deadlines and content throttling).
pub(crate) fn tick_ms() -> u64 {
    now_ms()
}
