//! SETTINGS popup (from "Settings" in the power popup): the dock's own options in one short list.
//!
//!  * Appearance: position, display mode, bar size, background tone, background opacity (segmented
//!    choices drawn by hand).
//!  * Behavior: auto-hide, clock seconds (switches).
//!  * Modules: one switch per system module (`config::MODULE_NAMES` mechanism) and, with the tray on,
//!    "Choose tray icons" (switches to the tray popup in chooser mode).
//!
//! Every click applies LIVE: `config::modify` rewrites `config.json` through the commented template
//! (the guide survives), then `WM_DOCK_CONFIG_APPLY` asks the dock window to rebuild the bar outside
//! this popup's wnd_proc. If the file cannot be written the change still applies for this session.
//! No timer, thread or system object: when the popup closes nothing of it keeps running.

use std::sync::Arc;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::{self, Background, BarSize, Config, DisplayMode, Position, OPACITY_STEPS};
use crate::log;
use crate::msg::WM_DOCK_CONFIG_APPLY;
use crate::popup::{self, center_y, lift, row_plate, toggle_switch, Content, HitMap, PopupKind, Reply};
use crate::surface::{rect_wh, Font, Surface, PRESS_ALPHA, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};

/// Popup width (logical px).
const WIDTH: i32 = 260;
/// Height of a control row / of a section label (logical px).
const ROW_H: i32 = 28;
const SECTION_H: i32 = 26;
/// Segmented control: height, horizontal text padding of a segment (logical px).
const SEG_H: i32 = 20;
const SEG_PAD: i32 = 7;
/// Segmented control alphas: track fill and outline, hovered segment.
const SEG_TRACK_ALPHA: u32 = 14;
const SEG_RIM_ALPHA: u32 = 40;
const SEG_HOT_ALPHA: u32 = 26;
/// Ids: item `i` -> `(i + 1) * ID_STRIDE`; segment `j` of a choice -> that + 1 + j.
const ID_STRIDE: u32 = 16;

/// A setting changed by a row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Position,
    Display,
    Size,
    Background,
    Opacity,
    AutoHide,
    Seconds,
    /// `Config::module_visible` name.
    Module(&'static str),
}

#[derive(Clone, Copy)]
enum Item {
    Section(&'static str),
    /// Label + segmented choice.
    Choice(Setting, &'static str),
    /// Label + switch (the whole row is clickable).
    Toggle(Setting, &'static str),
    /// "Choose tray icons" link.
    TrayChooser,
}

/// Rows of the popup for the config `c` (the tray link only while the tray is on).
fn items(c: &Config) -> Vec<Item> {
    let mut v = vec![
        Item::Section("Appearance"),
        Item::Choice(Setting::Position, "Position"),
        Item::Choice(Setting::Display, "Display"),
        Item::Choice(Setting::Size, "Size"),
        Item::Choice(Setting::Background, "Background"),
        Item::Choice(Setting::Opacity, "Opacity"),
        Item::Section("Behavior"),
        Item::Toggle(Setting::AutoHide, "Auto-hide"),
        Item::Toggle(Setting::Seconds, "Clock seconds"),
        Item::Section("Modules"),
    ];
    v.extend(config::MODULE_NAMES.iter().map(|n| Item::Toggle(Setting::Module(n), module_label(n))));
    if c.show_tray {
        v.push(Item::TrayChooser);
    }
    v
}

/// Row label of a `config::MODULE_NAMES` entry.
fn module_label(name: &str) -> &'static str {
    match name {
        "notifications" => "Notifications",
        "volume" => "Volume",
        "network" => "Network",
        "metrics" => "System metrics",
        "clock" => "Clock",
        "show_desktop" => "Show desktop",
        "tray" => "Tray icons",
        _ => "Module",
    }
}

/// Labels of the segments of a choice.
fn options(s: Setting) -> &'static [&'static str] {
    match s {
        Setting::Position => &["Top", "Bottom"],
        Setting::Display => &["Icon + name", "Icon", "Name"],
        Setting::Size => &["Small", "Medium", "Large"],
        Setting::Background => &["Black", "Dark"],
        Setting::Opacity => &["100%", "95%", "85%", "70%"],
        _ => &[],
    }
}

/// Selected segment of a choice (`None`: the value is not one of the segments, e.g. a hand-edited opacity).
fn selected(c: &Config, s: Setting) -> Option<usize> {
    Some(match s {
        Setting::Position => usize::from(c.position == Position::Bottom),
        Setting::Display => match c.display_mode {
            DisplayMode::IconText => 0,
            DisplayMode::Icon => 1,
            DisplayMode::Text => 2,
        },
        Setting::Size => match c.bar_size {
            BarSize::Small => 0,
            BarSize::Medium => 1,
            BarSize::Large => 2,
        },
        Setting::Background => usize::from(c.background == Background::Dark),
        Setting::Opacity => return OPACITY_STEPS.iter().position(|p| *p == c.bar_opacity),
        _ => return None,
    })
}

/// Sets segment `j` of choice `s`.
fn choose(c: &mut Config, s: Setting, j: usize) {
    match s {
        Setting::Position => c.position = if j == 0 { Position::Top } else { Position::Bottom },
        Setting::Display => {
            c.display_mode = [DisplayMode::IconText, DisplayMode::Icon, DisplayMode::Text][j.min(2)];
        }
        Setting::Size => c.bar_size = [BarSize::Small, BarSize::Medium, BarSize::Large][j.min(2)],
        Setting::Background => c.background = if j == 0 { Background::Black } else { Background::Dark },
        Setting::Opacity => c.bar_opacity = OPACITY_STEPS[j.min(OPACITY_STEPS.len() - 1)],
        _ => {}
    }
}

fn is_on(c: &Config, s: Setting) -> bool {
    match s {
        Setting::AutoHide => c.autohide,
        Setting::Seconds => c.show_seconds,
        Setting::Module(n) => c.module_visible(n),
        _ => false,
    }
}

fn flip(c: &mut Config, s: Setting) {
    match s {
        Setting::AutoHide => c.autohide = !c.autohide,
        Setting::Seconds => c.show_seconds = !c.show_seconds,
        Setting::Module(n) => {
            let on = c.module_visible(n);
            c.set_module_visible(n, !on);
        }
        _ => {}
    }
}

pub(crate) struct SettingsPopup {
    /// Config snapshot shown (re-read after every change).
    cfg: Arc<Config>,
    items: Vec<Item>,
    hits: HitMap,
    /// Top of each item and its height (physical px), same order as `items`.
    rows: Vec<(i32, i32)>,
    width: i32,
}

/// Creates the content from the current config.
pub(crate) fn new() -> Box<dyn Content> {
    let cfg = config::get();
    Box::new(SettingsPopup { items: items(&cfg), cfg, hits: HitMap::default(), rows: Vec::new(), width: 0 })
}

/// Id of item `i` (and of its segment `seg`, if any).
fn item_id(i: usize, seg: Option<usize>) -> u32 {
    (i as u32 + 1) * ID_STRIDE + seg.map_or(0, |j| j as u32 + 1)
}

/// `(item index, segment)` of an id.
fn decode(id: u32) -> (usize, Option<usize>) {
    let i = (id / ID_STRIDE).saturating_sub(1) as usize;
    let j = id % ID_STRIDE;
    (i, if j == 0 { None } else { Some(j as usize - 1) })
}

impl SettingsPopup {
    /// Segment widths of choice `s` (physical px).
    fn seg_widths(s: &Surface, set: Setting) -> Vec<i32> {
        let m = s.metrics();
        options(set).iter().map(|o| (s.text_width(o, Font::Small) + 2 * m.px(SEG_PAD)).max(m.px(30))).collect()
    }

    /// Applies `f` to the config: saved through the commented template, then the bar is rebuilt.
    fn commit(&mut self, f: impl Fn(&mut Config)) -> Reply {
        if let Err(e) = config::modify(&f) {
            // Not writable (or the write failed): the change still applies for this session.
            log!("settings: saving failed ({e}), applied in memory only");
            let mut c = (*config::get()).clone();
            f(&mut c);
            config::set(c);
        }
        // SAFETY: posting to the dock window; if it no longer exists the call just fails.
        if let Err(e) = unsafe { PostMessageW(Some(crate::hwnd()), WM_DOCK_CONFIG_APPLY, WPARAM(0), LPARAM(0)) } {
            log!("settings: PostMessageW(WM_DOCK_CONFIG_APPLY) failed: {e}");
        }
        self.cfg = config::get();
        self.items = items(&self.cfg);
        Reply::Relayout
    }
}

impl Content for SettingsPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Settings
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let inset = m.px(8);
        self.width = m.px(WIDTH);
        self.hits.clear();
        self.rows.clear();
        let mut y = inset;
        for (i, it) in self.items.iter().enumerate() {
            let h = match it {
                Item::Section(_) => m.px(SECTION_H) - if i == 0 { m.px(6) } else { 0 },
                _ => m.px(ROW_H),
            };
            let rc = rect_wh(inset, y, self.width - 2 * inset, h);
            match *it {
                Item::Section(_) => {}
                Item::Choice(set, _) => {
                    // Segments right-aligned on the switches' right edge.
                    let ws = Self::seg_widths(s, set);
                    let sh = m.px(SEG_H);
                    let mut x = rc.right - m.px(12) - ws.iter().sum::<i32>();
                    let sy = y + (h - sh) / 2;
                    for (j, w) in ws.iter().enumerate() {
                        self.hits.add(item_id(i, Some(j)), rect_wh(x, sy, *w, sh));
                        x += w;
                    }
                }
                Item::Toggle(..) | Item::TrayChooser => self.hits.add(item_id(i, None), rc),
            }
            self.rows.push((y, h));
            y += h;
        }
        (self.width, y + inset)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let col = m.px(TEXT_COL);
        let ln = s.line_height(Font::Normal);
        let lsm = s.line_height(Font::Small);
        for (i, it) in self.items.iter().enumerate() {
            let Some(&(y, h)) = self.rows.get(i) else { continue };
            let row = rect_wh(m.px(8), y, self.width - 2 * m.px(8), h);
            match *it {
                Item::Section(label) => {
                    // Dim label sitting on the bottom of its band, close to the rows it names.
                    s.text(label, col, y + h - lsm - m.px(4), Font::Small, WHITE, TEXT3_ALPHA);
                }
                Item::Choice(set, label) => {
                    s.text(label, col, center_y(&row, ln), Font::Normal, WHITE, TEXT_ALPHA);
                    let sel = selected(&self.cfg, set);
                    let n = options(set).len();
                    let (Some(first), Some(last)) =
                        (self.hits.rect(item_id(i, Some(0))), self.hits.rect(item_id(i, Some(n - 1))))
                    else {
                        continue;
                    };
                    let track = windows::Win32::Foundation::RECT { right: last.right, ..first };
                    let r = (track.bottom - track.top) / 2;
                    s.fill_rr(track, r, WHITE, SEG_TRACK_ALPHA);
                    s.stroke_rr(track, r, m.px(1).max(1), SEG_RIM_ALPHA);
                    for (j, opt) in options(set).iter().enumerate() {
                        let id = item_id(i, Some(j));
                        let Some(rc) = self.hits.rect(id) else { continue };
                        let ty = center_y(&rc, lsm);
                        let tx = rc.left + (rc.right - rc.left - s.text_width(opt, Font::Small)) / 2;
                        if sel == Some(j) {
                            s.fill_rr(rc, r, WHITE, 235);
                            s.text(opt, tx, ty, Font::Small, (0, 0, 0), 255);
                        } else {
                            let is_hot = hot == Some(id);
                            if is_hot {
                                let a = if pressed == Some(id) { PRESS_ALPHA } else { SEG_HOT_ALPHA };
                                s.fill_rr(rc, r, WHITE, a);
                            }
                            s.text(opt, tx, ty, Font::Small, WHITE, lift(TEXT2_ALPHA, is_hot));
                        }
                    }
                }
                Item::Toggle(set, label) => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    s.text(label, row.left + m.px(12), center_y(&row, ln), Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    let (sw, sh) = (m.px(32), m.px(18));
                    toggle_switch(s, row.right - m.px(12) - sw, row.top + (h - sh) / 2, is_on(&self.cfg, set));
                }
                Item::TrayChooser => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    let ty = center_y(&row, ln);
                    s.text("Choose tray icons", row.left + m.px(12), ty, Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    let cw = s.text_width("\u{203A}", Font::Normal);
                    s.text("\u{203A}", row.right - m.px(12) - cw, ty, Font::Normal, WHITE, lift(TEXT3_ALPHA, lit));
                }
            }
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn clickable(&self, id: u32) -> bool {
        // The segment already selected does nothing.
        match (self.items.get(decode(id).0), decode(id).1) {
            (Some(Item::Choice(set, _)), Some(j)) => selected(&self.cfg, *set) != Some(j),
            _ => true,
        }
    }

    fn click(&mut self, id: u32) -> Reply {
        let (i, seg) = decode(id);
        match (self.items.get(i).copied(), seg) {
            (Some(Item::Choice(set, _)), Some(j)) if selected(&self.cfg, set) != Some(j) => {
                self.commit(|c| choose(c, set, j))
            }
            (Some(Item::Toggle(set, _)), None) => self.commit(|c| flip(c, set)),
            (Some(Item::TrayChooser), None) => {
                popup::next_tray_chooser();
                Reply::Switch(PopupKind::TrayOverflow)
            }
            _ => Reply::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        assert_eq!(decode(item_id(0, None)), (0, None));
        assert_eq!(decode(item_id(5, Some(3))), (5, Some(3)));
        assert!(options(Setting::Opacity).len() == OPACITY_STEPS.len() && OPACITY_STEPS.len() < ID_STRIDE as usize);
    }

    #[test]
    fn choices_roundtrip() {
        let mut c = Config::default();
        for set in [Setting::Position, Setting::Display, Setting::Size, Setting::Background, Setting::Opacity] {
            for j in 0..options(set).len() {
                choose(&mut c, set, j);
                assert_eq!(selected(&c, set), Some(j));
            }
        }
        // A hand-edited opacity that is not a step selects nothing.
        c.bar_opacity = 60;
        assert_eq!(selected(&c, Setting::Opacity), None);
    }

    #[test]
    fn toggles_flip() {
        let mut c = Config::default();
        for set in [Setting::AutoHide, Setting::Seconds, Setting::Module("metrics"), Setting::Module("tray")] {
            let was = is_on(&c, set);
            flip(&mut c, set);
            assert_eq!(is_on(&c, set), !was);
        }
        assert!(!items(&c).iter().any(|i| matches!(i, Item::TrayChooser)));
    }
}
