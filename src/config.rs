//! Configuration `%LOCALAPPDATA%\Dock\config.json` and data folder.
//!
//! The config is global and shared across threads: `get()` returns an immutable snapshot
//! (`Arc<Config>`) that anyone (UI, shell thread) can hold without locking. Changes go
//! through `modify()` (read-modify-write with an atomic write: temp file + rename) and through
//! `poll_changed()` (re-read if the mtime changes, called by the UI thread every 2 s).
//!
//! Command-line options override the config ONLY through the `effective_*` functions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Position of the bar.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Top,
    #[default]
    Bottom,
}

/// What each dock entry shows: icon + label, icon only, or label only.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    #[default]
    IconText,
    Icon,
    Text,
}

/// An app pinned to the dock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PinnedApp {
    /// Grouping key (AUMID or normalized lowercase exe path).
    pub key: String,
    /// Display name.
    pub name: String,
    /// Launch target: exe path or `shell:AppsFolder\<AUMID>`.
    pub launch: String,
    /// Optional custom icon: file name relative to `%LOCALAPPDATA%\Dock\icons\`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Contents of `config.json`. Missing fields = defaults (`#[serde(default)]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub position: Position,
    pub autohide: bool,
    pub hide_native_taskbar: bool,
    pub pinned: Vec<PinnedApp>,
    /// Name overrides: key = lowercase exe file name (e.g. "code.exe") or AUMID;
    /// value = name to show.
    pub names: BTreeMap<String, String>,
    pub show_date: bool,
    pub show_volume_percent: bool,
    /// hosts the tray icons of other apps in the dock (`tray_host`). false = never started.
    pub show_tray: bool,
    /// Entry display: "icon_text" | "icon" | "text".
    pub display_mode: DisplayMode,
    /// Clock picture for GetTimeFormatEx (default "HH:mm").
    pub time_format: String,
    /// If true and `time_format` has no "s", ":ss" is appended.
    pub show_seconds: bool,
    /// Date picture for GetDateFormatEx (default "ddd d MMM"; empty = no date).
    pub date_format: String,
    /// GUIDs (lowercase, no braces) of the system tray icons NOT to show
    /// (they keep being answered and forwarded to Explorer).
    pub tray_hidden_guids: Vec<String>,
    /// Tray icons the user chose as "not in the bar" (they stay in the chevron popup).
    /// Per-icon key (`tray_host::item_key`): lowercase GUID without braces if the icon has one,
    /// otherwise `<lowercase exe name>#<uID>` (e.g. "discord.exe#1"). Default: empty.
    pub tray_hidden: Vec<String>,
    /// Relative order of the app entries that are NOT pinned (their keys). Pinned entries use the
    /// `pinned` array order. Apps missing from the list go last, in the order they were first seen.
    /// Written when the user drags an entry; capped at `APP_ORDER_CAP` keys.
    pub app_order: Vec<String>,
    /// Order of the tray icons (same key as `tray_hidden`). Unknown icons go last, in registration order.
    pub tray_order: Vec<String>,
    /// Order of the system modules: "notifications", "volume", "clock", "show_desktop", "power".
    /// Validated on use: missing modules are appended, unknown names are ignored.
    pub modules_order: Vec<String>,
    /// true once the `notif` thread has already asked for notification access once
    /// (`RequestAccessAsync`): the prompt is not shown again on every start.
    pub notif_prompted: bool,
}

/// Maximum number of keys kept in `app_order` / `tray_order` (the oldest, not-seen-recently ones fall off).
pub const APP_ORDER_CAP: usize = 64;
pub const TRAY_ORDER_CAP: usize = 128;

/// Default order of the system modules (names accepted by `modules_order`).
pub const DEFAULT_MODULES_ORDER: [&str; 5] = ["notifications", "volume", "clock", "show_desktop", "power"];

/// GUIDs of the system tray icons hidden by default: volume, network, power, location,
/// microphone (the dock draws/duplicates the first three).
pub const DEFAULT_TRAY_HIDDEN_GUIDS: [&str; 5] = [
    "7820ae73-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae74-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae75-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae77-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae82-23e3-4229-82c1-e41cb67d5b9c",
];

impl Default for Config {
    fn default() -> Self {
        Config {
            position: Position::Bottom,
            autohide: true,
            hide_native_taskbar: true,
            pinned: Vec::new(),
            names: BTreeMap::new(),
            show_date: true,
            show_volume_percent: true,
            show_tray: true,
            display_mode: DisplayMode::IconText,
            time_format: "HH:mm".to_string(),
            show_seconds: false,
            date_format: "ddd d MMM".to_string(),
            tray_hidden_guids: DEFAULT_TRAY_HIDDEN_GUIDS.iter().map(|s| s.to_string()).collect(),
            tray_hidden: Vec::new(),
            app_order: Vec::new(),
            tray_order: Vec::new(),
            modules_order: DEFAULT_MODULES_ORDER.iter().map(|s| s.to_string()).collect(),
            notif_prompted: false,
        }
    }
}

static CURRENT: OnceLock<Mutex<Arc<Config>>> = OnceLock::new();

fn cell() -> &'static Mutex<Arc<Config>> {
    CURRENT.get_or_init(|| Mutex::new(Arc::new(Config::default())))
}

// ------------------------------------------------------------------ data folder

/// `%LOCALAPPDATA%\Dock` (does not create it). If `LOCALAPPDATA` is missing it uses the temp folder.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Dock")
}

/// Like `data_dir()` but creates the folder if missing; `None` if that fails.
pub fn ensure_data_dir() -> Option<PathBuf> {
    let d = data_dir();
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// `%LOCALAPPDATA%\Dock\config.json`.
pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// `%LOCALAPPDATA%\Dock\icons` (users' custom PNGs).
pub fn icons_dir() -> PathBuf {
    data_dir().join("icons")
}

/// `%LOCALAPPDATA%\Dock\cache` (PNG cache of extracted icons).
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

// -------------------------------------------------------------------- access

/// Current config snapshot (cheap: clones an `Arc`). Available from any thread.
/// Before `init()` it returns the defaults.
pub fn get() -> Arc<Config> {
    match cell().lock() {
        Ok(g) => Arc::clone(&g),
        Err(p) => Arc::clone(&p.into_inner()),
    }
}

pub(crate) fn set(c: Config) {
    let new = Arc::new(c);
    match cell().lock() {
        Ok(mut g) => *g = new,
        Err(p) => *p.into_inner() = new,
    }
}

/// mtime of `config.json` at our last read/write (`None` = file missing/unknown).
static LAST_MTIME: Mutex<Option<SystemTime>> = Mutex::new(None);
/// Serializes read-modify-writes (`modify`) and writes (`save`) across threads.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock_mtime() -> std::sync::MutexGuard<'static, Option<SystemTime>> {
    LAST_MTIME.lock().unwrap_or_else(|p| p.into_inner())
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Removes `//` line comments and `/* */` block comments (JSONC), leaving string literals intact
/// (escaped quotes included). Newlines are kept so serde error positions still match the file.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut it = src.chars().peekable();
    let mut in_str = false;
    while let Some(c) = it.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if it.peek() == Some(&'/') => {
                while let Some(&n) = it.peek() {
                    if n == '\n' {
                        break;
                    }
                    it.next();
                }
            }
            '/' if it.peek() == Some(&'*') => {
                it.next();
                let mut prev = '\0';
                for n in it.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Parses the config (JSONC). Tolerates a leading UTF-8 BOM (Notepad). Missing fields = defaults.
fn parse(text: &str) -> Result<Config, String> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    serde_json::from_str::<Config>(&strip_comments(t)).map_err(|e| e.to_string())
}

/// Marker on the first line of every file we write; a config without it is a legacy plain JSON
/// file and gets migrated to the commented template.
const TEMPLATE_MARK: &str = "// mini-oled-dock configuration";

fn js<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// Pretty JSON of a collection, continuation lines indented to sit under a 2-space key.
fn js_block<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string()).replace('\n', "\n  ")
}

/// Serializes `c` as a commented JSONC file: an English guide above every field, current values
/// inline, so the comments survive every programmatic rewrite.
fn render(c: &Config) -> String {
    let mode = match c.display_mode {
        DisplayMode::IconText => "icon_text",
        DisplayMode::Icon => "icon",
        DisplayMode::Text => "text",
    };
    let pos = match c.position {
        Position::Top => "top",
        Position::Bottom => "bottom",
    };
    format!(
        r#"{TEMPLATE_MARK}
// Edit and save: changes are picked up within ~2 seconds, no restart needed.
// Comments (// and /* */) are allowed. If the file has a syntax error the previous
// settings are kept; if the dock must save a change (pin, tray icon) before you fix it,
// your file is first copied to config.json.bak so no edit is lost.
{{
  // Where the bar sits on screen: "top" or "bottom".
  "position": "{pos}",

  // What each entry shows: "icon_text" (icon + name), "icon" (icon only) or "text" (name only).
  // The indicator bar under each entry adapts to the chosen mode.
  "display_mode": "{mode}",

  // Hide the bar when the pointer leaves it, show it when you touch the screen edge.
  "autohide": {autohide},

  // Hide Windows' own taskbar while the dock runs (restored when the dock exits).
  "hide_native_taskbar": {hide_native},

  // Show the date next to the clock.
  "show_date": {show_date},

  // Clock format (Windows time picture, uses your system locale).
  //   H / HH = hour 0-23      h / hh = hour 1-12      m / mm = minutes
  //   s / ss = seconds        t / tt = AM/PM marker
  // Examples: "HH:mm" -> 14:05 (24h)    "h:mm tt" -> 2:05 PM (12h)
  "time_format": {time_format},

  // true = always show seconds (":ss" is appended if time_format has no "s").
  // The clock then refreshes every second instead of every minute.
  "show_seconds": {show_seconds},

  // Date format (Windows date picture). Empty string "" = no date.
  //   d / dd = day of month   ddd / dddd = short / full weekday name
  //   M / MM = month number   MMM / MMMM = short / full month name
  //   yy / yyyy = year
  // Examples: "ddd d MMM" -> Fri 23 Oct    "dd/MM/yyyy" -> 23/10/2026
  "date_format": {date_format},

  // Show the percentage next to the volume icon.
  "show_volume_percent": {show_volume},

  // Host other apps' tray icons inside the dock. All visible icons are shown (no limit);
  // the bar widens and narrows dynamically.
  "show_tray": {show_tray},

  // Tray icons you chose to keep out of the bar (they stay in the tray popup).
  // Normally managed from the tray popup. A key is the icon's GUID (lowercase, no braces)
  // or "<exe name lowercase>#<icon id>", e.g. "discord.exe#1".
  "tray_hidden": {tray_hidden},

  // Order of the tray icons in the bar (same keys as "tray_hidden"). Normally managed by
  // dragging an icon in the bar; icons not listed go last, in the order they registered.
  "tray_order": {tray_order},

  // System tray icons the dock never shows (volume, network, power, location, microphone).
  "tray_hidden_guids": {tray_guids},

  // Pinned apps, in order. Normally managed with Pin/Unpin in the app menu. Fields:
  //   "key"    grouping key: the AUMID, or the lowercase exe path
  //   "name"   label shown in the dock
  //   "launch" exe path, or "shell:AppsFolder\\<AUMID>" for Store/packaged apps
  //   "icon"   optional custom icon: PNG file name inside %LOCALAPPDATA%\Dock\icons\
  // List the AUMIDs of installed apps with:  Get-StartApps   (PowerShell)
  "pinned": {pinned},

  // Order of the open apps that are NOT pinned (their keys). Pinned apps follow the "pinned" order
  // above. Normally managed by dragging an entry in the bar; apps not listed go last, in the order
  // they were first seen. At most 64 keys are kept.
  "app_order": {app_order},

  // Order of the system buttons on the right of the bar. Normally managed by dragging them.
  // Names: "notifications", "volume", "clock", "show_desktop", "power". Missing names are appended
  // in the default order, unknown names are ignored.
  "modules_order": {modules_order},

  // Display-name overrides. Key = exe file name lowercase (e.g. "code.exe") or AUMID;
  // value = the name to show. Example: {{ "code.exe": "VS Code" }}
  "names": {names},

  // Internal: set once the notification-access prompt has been shown. Leave as is.
  "notif_prompted": {notif}
}}
"#,
        autohide = c.autohide,
        hide_native = c.hide_native_taskbar,
        show_date = c.show_date,
        time_format = js(&c.time_format),
        show_seconds = c.show_seconds,
        date_format = js(&c.date_format),
        show_volume = c.show_volume_percent,
        show_tray = c.show_tray,
        tray_hidden = js_block(&c.tray_hidden),
        tray_order = js_block(&c.tray_order),
        app_order = js_block(&c.app_order),
        modules_order = js_block(&c.modules_order),
        tray_guids = js_block(&c.tray_hidden_guids),
        pinned = js_block(&c.pinned),
        names = js_block(&c.names),
        notif = c.notif_prompted,
    )
}

/// Reads the config from disk; if the file is missing it creates it with the defaults; if it is unreadable/invalid
/// it logs and uses the defaults WITHOUT overwriting the file. Sets the global snapshot and remembers
/// the mtime for `poll_changed`. Called once at startup from the main thread.
pub fn init() -> Arc<Config> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(c) => {
                crate::log!("config: read from {}", path.display());
                if !text.contains(TEMPLATE_MARK) {
                    // Legacy plain JSON: rewrite once with the commented guide.
                    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
                    match write_file(&c) {
                        Ok(()) => crate::log!("config: migrated to the commented format"),
                        Err(e) => crate::log!("config: migration failed: {e}"),
                    }
                }
                set(c);
            }
            Err(e) => {
                crate::log!("config: {} is invalid ({e}): using defaults, the file is left untouched", path.display());
                set(Config::default());
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::log!("config: missing, creating {} with defaults", path.display());
            if let Err(e) = save(&Config::default()) {
                crate::log!("config: creation failed: {e}");
            }
            set(Config::default());
        }
        Err(e) => {
            crate::log!("config: reading {} failed ({e}): using defaults", path.display());
            set(Config::default());
        }
    }
    *lock_mtime() = file_mtime(&path);
    get()
}

/// Checks the mtime of `config.json`; if it changed, re-reads, updates the global snapshot and
/// returns `true` (the UI must rebuild its entries). Does not block: one stat + read of a
/// small file. Called by the UI thread every 2 s (`TIMER_HOUSEKEEPING`).
///
/// If the new content is invalid (half-saved edit or syntax error) the previous
/// snapshot is kept, a message is logged once and `false` is returned; the file is re-read on the next
/// change. Returns `true` only if the parsed content differs from the snapshot.
pub fn poll_changed() -> bool {
    let path = config_path();
    let now = file_mtime(&path);
    {
        let mut last = lock_mtime();
        if *last == now {
            return false;
        }
        *last = now;
    }
    if now.is_none() {
        // File deleted: the snapshot is kept; it will be recreated on the next save.
        crate::log!("config: file removed, keeping the current configuration");
        return false;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            crate::log!("config: re-read failed ({e})");
            return false;
        }
    };
    match parse(&text) {
        Ok(c) => {
            if c == *get() {
                return false;
            }
            crate::log!("config: changed on disk, reloaded");
            set(c);
            true
        }
        Err(e) => {
            crate::log!("config: invalid change ({e}), keeping the previous one");
            false
        }
    }
}

/// Atomic read-modify-write of the config: applies `f` to a copy of the current snapshot,
/// writes the file (temp + rename) and, if that succeeds, updates the global snapshot.
/// Used by Pin/Unpin. Write errors -> `Err` (the snapshot stays unchanged).
pub fn modify<F: FnOnce(&mut Config)>(f: F) -> Result<(), String> {
    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = (*get()).clone();
    f(&mut c);
    write_file(&c)?;
    set(c);
    Ok(())
}

/// Atomically writes `c` to `config.json` (`.tmp` file in the same folder + rename).
/// Updates the global snapshot and the remembered mtime (so `poll_changed` does not re-read
/// our own write).
pub fn save(c: &Config) -> Result<(), String> {
    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    write_file(c)?;
    set(c.clone());
    Ok(())
}

/// If `config.json` exists but does not parse (the user is mid-edit or made a typo), copies it to
/// `config.json.bak` so the rewrite that follows does not destroy those edits.
fn backup_if_broken(path: &Path) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    if parse(&text).is_ok() {
        return Ok(());
    }
    let bak = path.with_extension("json.bak");
    std::fs::write(&bak, text.as_bytes()).map_err(|e| format!("backup to {}: {e}", bak.display()))?;
    crate::log!("config: {} has errors, copied to {} before rewriting", path.display(), bak.display());
    Ok(())
}

/// Atomic write + update of the remembered mtime. Call with `WRITE_LOCK` held.
fn write_file(c: &Config) -> Result<(), String> {
    let dir = ensure_data_dir().ok_or_else(|| "data folder could not be created".to_string())?;
    let json = render(c);
    let path = config_path();
    // Never lose unparsed user edits: refuse to write if they cannot be saved aside.
    backup_if_broken(&path)?;
    let tmp = dir.join("config.json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    // std::fs::rename on Windows replaces the destination (MOVEFILE_REPLACE_EXISTING).
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename in {}: {e}", path.display()));
    }
    *lock_mtime() = file_mtime(&path);
    Ok(())
}

// ------------------------------------------------------------------ pin / unpin

impl Config {
    /// Index of the pinned entry with this key (case-insensitive comparison).
    pub fn pinned_index(&self, key: &str) -> Option<usize> {
        self.pinned.iter().position(|p| p.key.eq_ignore_ascii_case(key))
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pinned_index(key).is_some()
    }
}

/// Appends `app` to the pins and saves. If the key is already pinned it does nothing (Ok).
pub fn pin(app: PinnedApp) -> Result<(), String> {
    if get().is_pinned(&app.key) {
        return Ok(());
    }
    modify(|c| {
        if !c.is_pinned(&app.key) {
            c.pinned.push(app);
        }
    })
}

/// Removes the pin with this key and saves. If it does not exist nothing is written (Ok).
pub fn unpin(key: &str) -> Result<(), String> {
    if !get().is_pinned(key) {
        return Ok(());
    }
    modify(|c| c.pinned.retain(|p| !p.key.eq_ignore_ascii_case(key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_partial_uses_defaults() {
        let c = parse("{\"position\":\"top\",\"pinned\":[{\"key\":\"a\"}]}").unwrap();
        assert_eq!(c.position, Position::Top);
        assert!(c.autohide && c.show_date);
        assert_eq!(c.pinned[0].key, "a");
        assert!(c.pinned[0].icon.is_none());
    }

    #[test]
    fn parse_bom_and_invalid() {
        assert!(parse("\u{feff}{}").is_ok());
        assert!(parse("{ not json").is_err());
    }

    #[test]
    fn comments_are_stripped_outside_strings() {
        let t = "// hi\n{ /* b */ \"names\": {\"a\": \"x // y \\\" /* z */\"} } // end";
        let c = parse(t).unwrap();
        assert_eq!(c.names["a"], "x // y \" /* z */");
    }

    #[test]
    fn template_roundtrip() {
        let mut c = Config::default();
        c.pinned.push(PinnedApp {
            key: "k".into(),
            name: "N".into(),
            launch: "shell:AppsFolder\\X".into(),
            icon: None,
        });
        c.names.insert("code.exe".into(), "VS Code".into());
        c.show_seconds = true;
        c.display_mode = DisplayMode::Text;
        c.position = Position::Top;
        let r = render(&c);
        assert!(r.contains("Get-StartApps"));
        assert_eq!(parse(&r).unwrap(), c);
        assert_eq!(parse(&render(&Config::default())).unwrap(), Config::default());
    }

    #[test]
    fn order_fields_roundtrip() {
        let mut c = Config::default();
        c.app_order = vec!["a".into(), "b".into()];
        c.tray_order = vec!["x#1".into()];
        c.modules_order = vec!["power".into(), "clock".into()];
        let r = render(&c);
        assert!(r.contains("\"modules_order\""));
        assert_eq!(parse(&r).unwrap(), c);
        assert_eq!(Config::default().modules_order.len(), 5);
    }

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.pinned.push(PinnedApp { key: "k".into(), name: "N".into(), launch: "L".into(), icon: Some("i.png".into()) });
        c.names.insert("code.exe".into(), "VS Code".into());
        assert_eq!(parse(&serde_json::to_string_pretty(&c).unwrap()).unwrap(), c);
    }
}

// ---------------------------------------------------- effective options (CLI > config)

/// Effective position: `--top`/`--bottom` if present, otherwise the config.
pub fn effective_position() -> Position {
    match crate::args().top {
        Some(true) => Position::Top,
        Some(false) => Position::Bottom,
        None => get().position,
    }
}

/// Effective auto-hide: `--no-autohide` forces false, otherwise the config.
pub fn effective_autohide() -> bool {
    !crate::args().no_autohide && get().autohide
}

/// Hide the native taskbar: `--keep-taskbar` forces false, otherwise the config.
pub fn effective_hide_native_taskbar() -> bool {
    !crate::args().keep_taskbar && get().hide_native_taskbar
}
