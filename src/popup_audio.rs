//! AUDIO popup, top to bottom:
//!  1. master mute button (`VolumeToggleMute`) + volume slider (drag + wheel, `ShellRequest::VolumeSet`);
//!  2. output devices (`ListOutputs` on open, click = `SetDefaultOutput`);
//!  3. spatial sound formats of the default output (`ListSpatial` on open, click = `SetSpatialFormat`),
//!     only when the device supports spatial sound;
//!  4. per-app volume mixer (`ListSessions` on open; mute button = `ToggleSessionMute`, slider =
//!     `SetSessionVolume`), only when at least one session exists;
//!  5. "More volume settings" link (`ms-settings:apps-volume`).
//!
//! Initial state: the last known `VolumeState` passed by `popup::toggle` (via `DataEvent::Volume`).
//! Spatial and mixer data are read by the shell thread only on request (and on a default-device
//! change while this popup is open, see `audio::set_popup_open`); nothing outlives the popup.

use windows::Win32::Foundation::RECT;

use crate::audio::{self, AppSession, OutputDevice, SpatialState, VolumeState, WHEEL_STEP_PERCENT};
use crate::glyphs;
use crate::popup::{center_y, hline, launch, lift, row_plate, tick_ms, Content, DataEvent, HitMap, PopupKind, Reply};
use crate::shell_thread::{self, ShellRequest};
use crate::surface::{rect_wh, Font, Surface, PANEL_PAD, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};

const ID_MUTE: u32 = 1;
const ID_SLIDER: u32 = 2;
const ID_MIXER: u32 = 3;
/// Device rows have id `ID_DEVICE + index`.
const ID_DEVICE: u32 = 100;
/// Spatial format rows have id `ID_SPATIAL + index`.
const ID_SPATIAL: u32 = 200;
/// Mixer rows: mute button `ID_APP_MUTE + index`, slider `ID_APP_SLIDER + index`.
const ID_APP_MUTE: u32 = 300;
const ID_APP_SLIDER: u32 = 400;
/// End (exclusive) of the mixer slider id range.
const ID_END: u32 = 500;

/// Minimum interval between two `VolumeSet`/`SetSessionVolume` calls while dragging (ms).
const SET_THROTTLE_MS: u64 = 30;

/// Row height (logical px) of every list row (devices, spatial formats, mixer, link).
const ROW: i32 = 34;

pub(crate) struct AudioPopup {
    level: u8,
    muted: bool,
    available: bool,
    /// `None` until the first reply to `ListOutputs` arrives.
    outputs: Option<Vec<OutputDevice>>,
    /// `None` = not supported (or not read yet): section hidden.
    spatial: Option<SpatialState>,
    /// Mixer rows (empty = section hidden).
    sessions: Vec<AppSession>,
    dragging: bool,
    /// Mixer row whose slider is being dragged.
    drag_app: Option<usize>,
    last_set: u64,
    open_mixer: bool,
    // Cached geometry (physical px).
    hits: HitMap,
    width: i32,
    pad: i32,
    slider: RECT,
    knob_r: i32,
    pct_right: i32,
    sep1: i32,
    sep2: i32,
    y_label: i32,
    y_empty: i32,
    /// Separator and label y of the spatial section (when shown).
    spatial_sep: Option<(i32, i32)>,
    /// Separator and label y of the mixer section (when shown).
    mixer_sep: Option<(i32, i32)>,
    /// Name column of the mixer rows: x and width.
    app_name_x: i32,
    app_name_w: i32,
}

/// Creates the content (initial state).
pub(crate) fn new(volume: VolumeState) -> Box<dyn Content> {
    audio::set_popup_open(true);
    // Fresh data: the volume is re-read, the device list, spatial formats and sessions are requested.
    shell_thread::request(ShellRequest::VolumeRefresh);
    shell_thread::request(ShellRequest::ListOutputs);
    shell_thread::request(ShellRequest::ListSpatial);
    shell_thread::request(ShellRequest::ListSessions);
    Box::new(AudioPopup {
        level: volume.level.min(100),
        muted: volume.muted,
        available: volume.available,
        outputs: None,
        spatial: None,
        sessions: Vec::new(),
        dragging: false,
        drag_app: None,
        last_set: 0,
        open_mixer: false,
        hits: HitMap::default(),
        width: 0,
        pad: 0,
        slider: RECT::default(),
        knob_r: 7,
        pct_right: 0,
        sep1: 0,
        sep2: 0,
        y_label: 0,
        y_empty: 0,
        spatial_sep: None,
        mixer_sep: None,
        app_name_x: 0,
        app_name_w: 0,
    })
}

// ------------------------------------------------------------------ shared slider / speaker helpers

/// Extremes (physical px) along which the center of the knob of slider `rc` moves.
fn track(rc: &RECT, knob_r: i32) -> (i32, i32) {
    (rc.left + knob_r, rc.right - knob_r)
}

/// Level 0..=100 for pointer x on slider `rc`.
fn level_at(rc: &RECT, knob_r: i32, x: i32) -> u8 {
    let (l, r) = track(rc, knob_r);
    let span = (r - l).max(1);
    (((x - l) * 100 + span / 2) / span).clamp(0, 100) as u8
}

/// Track, filled part and knob of slider `rc` at `level`.
fn paint_slider(s: &mut Surface, rc: &RECT, knob_r: i32, level: u8) {
    let m = s.metrics();
    let (tl, tr) = track(rc, knob_r);
    let cy = (rc.top + rc.bottom) / 2;
    let th = m.px(4);
    let top = cy - th / 2;
    s.fill_rr(RECT { left: rc.left, top, right: rc.right, bottom: top + th }, th / 2, WHITE, 60);
    let kx = tl + (tr - tl) * level.min(100) as i32 / 100;
    s.fill_rr(RECT { left: rc.left, top, right: kx, bottom: top + th }, th / 2, WHITE, 230);
    s.fill_rr(RECT { left: kx - knob_r, top: cy - knob_r, right: kx + knob_r, bottom: cy + knob_r }, knob_r, WHITE, 255);
}

/// Speaker button (plate + glyph) for mute toggle `id` in `rc`.
fn paint_speaker(s: &mut Surface, rc: RECT, id: u32, hot: Option<u32>, pressed: Option<u32>, level: u8, muted: bool) {
    let icon = s.metrics().px(16);
    let h = row_plate(s, rc, id, hot, pressed);
    let waves = glyphs::waves_for(level);
    let x = rc.left + ((rc.right - rc.left) - icon) / 2;
    let y = rc.top + ((rc.bottom - rc.top) - icon) / 2;
    s.glyph(x, y, icon, lift(TEXT_ALPHA, h), &|u, v| glyphs::speaker_shape(u, v, waves, muted));
}

/// Percentage text right-aligned at `right`, vertically centered on `row`.
fn paint_pct(s: &mut Surface, level: u8, right: i32, row: &RECT, alpha: u32) {
    let pct = format!("{level}%");
    let tw = s.text_width(&pct, Font::Normal);
    let lh = s.line_height(Font::Normal);
    s.text(&pct, right - tw, center_y(row, lh), Font::Normal, WHITE, alpha);
}

// ------------------------------------------------------------------ layout (pure)

/// Logical-pixel layout inputs of the popup's list part (everything below the master row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListCounts {
    /// Device rows (0 = one "empty/loading" row).
    pub devices: usize,
    /// Spatial rows (0 = section hidden).
    pub spatial: usize,
    /// Mixer rows (0 = section hidden).
    pub apps: usize,
}

/// Height of a section header: separator gap (6 + 1 + 6) + label line + 4.
fn header_h(px: &dyn Fn(i32) -> i32, label_h: i32) -> i32 {
    px(6) + px(1) + px(6) + label_h + px(4)
}

/// Total popup height for `c` (the same arithmetic `size()` walks through), used by the tests and
/// handy to reason about the screen fit.
pub(crate) fn popup_height(px: &dyn Fn(i32) -> i32, label_h: i32, c: ListCounts) -> i32 {
    let pad = px(PANEL_PAD);
    let row = px(ROW);
    // Master row + gap; output header starts with the separator (no leading 6 px).
    let mut y = pad + px(32) + px(8);
    y += px(1) + px(6) + label_h + px(4);
    y += row * c.devices.max(1) as i32;
    if c.spatial > 0 {
        y += header_h(px, label_h) + row * c.spatial as i32;
    }
    if c.apps > 0 {
        y += header_h(px, label_h) + row * c.apps as i32;
    }
    // Bottom separator + link row + bottom inset.
    y += px(6) + px(1) + px(6) + row + px(8);
    y
}

impl AudioPopup {
    fn send_level(&mut self) {
        self.last_set = tick_ms();
        shell_thread::request(ShellRequest::VolumeSet { percent: self.level });
    }

    fn send_app_level(&mut self, idx: usize) {
        let Some(a) = self.sessions.get(idx) else { return };
        self.last_set = tick_ms();
        shell_thread::request(ShellRequest::SetSessionVolume { pid: a.pid, system: a.system, level: a.level });
    }

    fn spatial_len(&self) -> usize {
        self.spatial.as_ref().map_or(0, |s| s.options.len())
    }

    fn counts(&self) -> ListCounts {
        ListCounts {
            devices: self.outputs.as_ref().map_or(0, Vec::len),
            spatial: self.spatial_len(),
            apps: self.sessions.len(),
        }
    }
}

impl Content for AudioPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Audio
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        self.width = p(320);
        self.pad = p(PANEL_PAD);
        self.hits.clear();
        let row_top = self.pad;
        let btn = p(32);
        self.hits.add(ID_MUTE, rect_wh(self.pad, row_top, btn, btn));
        let pct_w = s.text_width("100%", Font::Normal);
        self.pct_right = self.width - p(TEXT_COL);
        let sl_left = self.pad + btn + p(8);
        let sl_right = self.pct_right - pct_w - p(8);
        let sl_h = p(24);
        self.slider = rect_wh(sl_left, row_top + (btn - sl_h) / 2, (sl_right - sl_left).max(p(40)), sl_h);
        self.hits.add(ID_SLIDER, self.slider);
        self.knob_r = p(7);
        let label_h = s.line_height(Font::Small);
        let mut y = row_top + btn + p(8);
        self.sep1 = y;
        y += p(1) + p(6);
        self.y_label = y;
        y += label_h + p(4);
        let inset = p(8);
        let row = p(ROW);
        match self.outputs.as_ref() {
            Some(list) if !list.is_empty() => {
                for i in 0..list.len() {
                    self.hits.add(ID_DEVICE + i as u32, rect_wh(inset, y, self.width - 2 * inset, row));
                    y += row;
                }
            }
            _ => {
                self.y_empty = y;
                y += row;
            }
        }
        // Spatial sound.
        self.spatial_sep = None;
        let n_spatial = self.spatial_len();
        if n_spatial > 0 {
            let sep = y + p(6);
            let label = sep + p(1) + p(6);
            self.spatial_sep = Some((sep, label));
            y = label + label_h + p(4);
            for i in 0..n_spatial {
                self.hits.add(ID_SPATIAL + i as u32, rect_wh(inset, y, self.width - 2 * inset, row));
                y += row;
            }
        }
        // Volume mixer.
        self.mixer_sep = None;
        if !self.sessions.is_empty() {
            let sep = y + p(6);
            let label = sep + p(1) + p(6);
            self.mixer_sep = Some((sep, label));
            y = label + label_h + p(4);
            let abtn = p(30);
            self.app_name_x = self.pad + abtn + p(8);
            self.app_name_w = p(92);
            let a_sl_left = self.app_name_x + self.app_name_w + p(8);
            for i in 0..self.sessions.len() {
                self.hits.add(ID_APP_MUTE + i as u32, rect_wh(self.pad, y + (row - abtn) / 2, abtn, abtn));
                self.hits.add(
                    ID_APP_SLIDER + i as u32,
                    rect_wh(a_sl_left, y + (row - sl_h) / 2, (sl_right - a_sl_left).max(p(40)), sl_h),
                );
                y += row;
            }
        }
        y += p(6);
        self.sep2 = y;
        y += p(1) + p(6);
        self.hits.add(ID_MIXER, rect_wh(inset, y, self.width - 2 * inset, row));
        y += row + inset;
        debug_assert_eq!(y, popup_height(&p, label_h, self.counts()));
        (self.width, y)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let icon = m.px(16);
        let lh = s.line_height(Font::Normal);
        // Master speaker button, slider and percentage.
        if let Some(rc) = self.hits.rect(ID_MUTE) {
            paint_speaker(s, rc, ID_MUTE, hot, pressed, self.level, self.muted || !self.available);
        }
        paint_slider(s, &self.slider, self.knob_r, self.level);
        let btn_rc = self.hits.rect(ID_MUTE).unwrap_or_default();
        paint_pct(s, self.level, self.pct_right, &btn_rc, TEXT2_ALPHA);

        let text_x0 = m.px(TEXT_COL);
        let text_x1 = self.width - m.px(TEXT_COL);
        hline(s, text_x0, text_x1, self.sep1);
        s.text(crate::i18n::tr("Output", "Uscita"), text_x0, self.y_label, Font::Small, WHITE, TEXT3_ALPHA);

        // Devices.
        match self.outputs.as_ref() {
            Some(list) if !list.is_empty() => {
                for (i, d) in list.iter().enumerate() {
                    let id = ID_DEVICE + i as u32;
                    let Some(rc) = self.hits.rect(id) else { continue };
                    let h = row_plate(s, rc, id, hot, pressed);
                    // Check mark in a 16 px slot at the text column (20), name at 20 + 16 + 8.
                    let x0 = rc.left + m.px(12);
                    if d.is_default {
                        s.glyph(x0, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, TEXT_ALPHA, &glyphs::check_shape);
                    }
                    // Form-factor icon after the check slot, name after the icon.
                    let ix = x0 + icon + m.px(6);
                    let kind = d.kind;
                    let ia = if d.is_default { TEXT_ALPHA } else { TEXT2_ALPHA };
                    s.glyph(ix, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, lift(ia, h), &|u, v| {
                        glyphs::device_shape(u, v, kind)
                    });
                    let tx = ix + icon + m.px(8);
                    let name = s.ellipsize(&d.name, Font::Normal, rc.right - m.px(10) - tx);
                    let alpha = if d.is_default { TEXT_ALPHA } else { TEXT2_ALPHA };
                    s.text(&name, tx, center_y(&rc, lh), Font::Normal, WHITE, lift(alpha, h));
                }
            }
            Some(_) => {
                let rc = rect_wh(0, self.y_empty, self.width, m.px(ROW));
                let txt = crate::i18n::tr("No output device", "Nessun dispositivo di uscita");
                let tw = s.text_width(txt, Font::Normal);
                s.text(txt, (self.width - tw) / 2, center_y(&rc, lh), Font::Normal, WHITE, TEXT2_ALPHA);
            }
            None => {
                let rc = rect_wh(0, self.y_empty, self.width, m.px(ROW));
                let txt = crate::i18n::tr("Loading\u{2026}", "Caricamento\u{2026}");
                let tw = s.text_width(txt, Font::Small);
                s.text(
                    txt,
                    (self.width - tw) / 2,
                    center_y(&rc, s.line_height(Font::Small)),
                    Font::Small,
                    WHITE,
                    TEXT3_ALPHA,
                );
            }
        }

        // Spatial sound: same check-mark style as the device rows (no form-factor icon).
        if let (Some((sep, label)), Some(sp)) = (self.spatial_sep, self.spatial.as_ref()) {
            hline(s, text_x0, text_x1, sep);
            s.text(crate::i18n::tr("Spatial sound", "Audio spaziale"), text_x0, label, Font::Small, WHITE, TEXT3_ALPHA);
            for (i, o) in sp.options.iter().enumerate() {
                let id = ID_SPATIAL + i as u32;
                let Some(rc) = self.hits.rect(id) else { continue };
                let h = row_plate(s, rc, id, hot, pressed);
                let selected = sp.current == Some(i);
                let x0 = rc.left + m.px(12);
                if selected {
                    s.glyph(x0, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, TEXT_ALPHA, &glyphs::check_shape);
                }
                let tx = x0 + icon + m.px(8);
                let name = s.ellipsize(o.kind.label(), Font::Normal, rc.right - m.px(10) - tx);
                let alpha = if selected { TEXT_ALPHA } else { TEXT2_ALPHA };
                s.text(&name, tx, center_y(&rc, lh), Font::Normal, WHITE, lift(alpha, h));
            }
        }

        // Volume mixer.
        if let Some((sep, label)) = self.mixer_sep {
            hline(s, text_x0, text_x1, sep);
            s.text(crate::i18n::tr("Volume mixer", "Mixer volume"), text_x0, label, Font::Small, WHITE, TEXT3_ALPHA);
            for (i, a) in self.sessions.iter().enumerate() {
                let (Some(brc), Some(src)) =
                    (self.hits.rect(ID_APP_MUTE + i as u32), self.hits.rect(ID_APP_SLIDER + i as u32))
                else {
                    continue;
                };
                paint_speaker(s, brc, ID_APP_MUTE + i as u32, hot, pressed, a.level, a.muted);
                let name = s.ellipsize(&a.name, Font::Normal, self.app_name_w);
                let alpha = if a.muted { TEXT3_ALPHA } else { TEXT2_ALPHA };
                s.text(&name, self.app_name_x, center_y(&brc, lh), Font::Normal, WHITE, alpha);
                paint_slider(s, &src, self.knob_r, a.level);
                paint_pct(s, a.level, self.pct_right, &brc, TEXT2_ALPHA);
            }
        }

        hline(s, text_x0, text_x1, self.sep2);
        if let Some(rc) = self.hits.rect(ID_MIXER) {
            let h = row_plate(s, rc, ID_MIXER, hot, pressed);
            s.text(
                crate::i18n::tr("More volume settings", "Altre impostazioni volume"),
                rc.left + m.px(12),
                center_y(&rc, lh),
                Font::Normal,
                WHITE,
                lift(TEXT_ALPHA, h),
            );
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        match id {
            ID_MUTE => {
                shell_thread::request(ShellRequest::VolumeToggleMute);
                self.muted = !self.muted;
                Reply::Redraw
            }
            ID_MIXER => {
                self.open_mixer = true;
                Reply::Close
            }
            i if (ID_DEVICE..ID_SPATIAL).contains(&i) => {
                let idx = (i - ID_DEVICE) as usize;
                let Some(list) = self.outputs.as_mut() else { return Reply::None };
                let Some(dev) = list.get(idx) else { return Reply::None };
                if dev.is_default {
                    return Reply::None;
                }
                let target = dev.id.clone();
                shell_thread::request(ShellRequest::SetDefaultOutput { id: target.clone() });
                for d in list.iter_mut() {
                    d.is_default = d.id == target;
                }
                Reply::Redraw
            }
            i if (ID_SPATIAL..ID_APP_MUTE).contains(&i) => {
                let idx = (i - ID_SPATIAL) as usize;
                let Some(sp) = self.spatial.as_mut() else { return Reply::None };
                let Some(opt) = sp.options.get(idx) else { return Reply::None };
                if sp.current == Some(idx) {
                    return Reply::None;
                }
                shell_thread::request(ShellRequest::SetSpatialFormat { subtype: opt.subtype.clone() });
                sp.current = Some(idx);
                Reply::Redraw
            }
            i if (ID_APP_MUTE..ID_APP_SLIDER).contains(&i) => {
                let Some(a) = self.sessions.get_mut((i - ID_APP_MUTE) as usize) else { return Reply::None };
                shell_thread::request(ShellRequest::ToggleSessionMute { pid: a.pid, system: a.system });
                a.muted = !a.muted;
                Reply::Redraw
            }
            _ => Reply::None,
        }
    }

    fn drag(&mut self, id: u32, x: i32, _y: i32) -> Reply {
        if (ID_APP_SLIDER..ID_END).contains(&id) {
            let idx = (id - ID_APP_SLIDER) as usize;
            let Some(rc) = self.hits.rect(id) else { return Reply::None };
            let lvl = level_at(&rc, self.knob_r, x);
            let Some(a) = self.sessions.get_mut(idx) else { return Reply::None };
            self.drag_app = Some(idx);
            if lvl == a.level && !(a.muted && lvl > 0) {
                return Reply::None;
            }
            a.level = lvl;
            if lvl > 0 {
                a.muted = false;
            }
            if tick_ms().saturating_sub(self.last_set) >= SET_THROTTLE_MS {
                self.send_app_level(idx);
            }
            return Reply::Redraw;
        }
        if id != ID_SLIDER {
            return Reply::None;
        }
        let lvl = level_at(&self.slider, self.knob_r, x);
        self.dragging = true;
        if lvl != self.level || self.muted {
            self.level = lvl;
            if lvl > 0 {
                self.muted = false;
            }
            if tick_ms().saturating_sub(self.last_set) >= SET_THROTTLE_MS {
                self.send_level();
            }
            return Reply::Redraw;
        }
        Reply::None
    }

    fn release(&mut self, id: u32) -> Reply {
        if id == ID_SLIDER {
            self.dragging = false;
            self.send_level();
        } else if (ID_APP_SLIDER..ID_END).contains(&id) {
            if let Some(idx) = self.drag_app.take() {
                self.send_app_level(idx);
                // Re-read the real state once the change is done.
                shell_thread::request(ShellRequest::ListSessions);
            }
        }
        Reply::None
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32) -> Reply {
        let delta = notches * WHEEL_STEP_PERCENT;
        shell_thread::request(ShellRequest::VolumeAdjust { delta_percent: delta });
        self.level = (self.level as i32 + delta).clamp(0, 100) as u8;
        if delta > 0 {
            self.muted = false;
        }
        Reply::Redraw
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        match ev {
            DataEvent::Volume(v) => {
                if !self.dragging {
                    self.level = v.level.min(100);
                }
                self.muted = v.muted;
                self.available = v.available;
                Reply::Redraw
            }
            DataEvent::AudioOutputs(list) => {
                self.outputs = Some(list.clone());
                Reply::Relayout
            }
            DataEvent::Spatial(st) => {
                self.spatial = st.clone();
                Reply::Relayout
            }
            DataEvent::AudioSessions(list) => {
                // Keep the local level of a row being dragged (the reply may predate the last set).
                let dragged = self.drag_app.and_then(|i| self.sessions.get(i)).map(|a| (a.pid, a.system, a.level));
                self.sessions = list.clone();
                if let Some((pid, system, level)) = dragged {
                    if let Some(a) = self.sessions.iter_mut().find(|a| a.pid == pid && a.system == system) {
                        a.level = level;
                    }
                }
                Reply::Relayout
            }
            _ => Reply::None,
        }
    }

    fn closing(&mut self) {
        audio::set_popup_open(false);
    }

    fn after_close(&mut self) {
        if self.open_mixer {
            launch("ms-settings:apps-volume");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(l: i32, r: i32) -> RECT {
        RECT { left: l, top: 0, right: r, bottom: 24 }
    }

    #[test]
    fn slider_levels() {
        let r = rc(100, 214); // track 107..207
        assert_eq!(track(&r, 7), (107, 207));
        assert_eq!(level_at(&r, 7, 0), 0);
        assert_eq!(level_at(&r, 7, 107), 0);
        assert_eq!(level_at(&r, 7, 157), 50);
        assert_eq!(level_at(&r, 7, 207), 100);
        assert_eq!(level_at(&r, 7, 999), 100);
    }

    #[test]
    fn height_grows_by_sections() {
        let px = |v: i32| v; // 96 dpi
        let lh = 16;
        let base = popup_height(&px, lh, ListCounts { devices: 2, spatial: 0, apps: 0 });
        let loading = popup_height(&px, lh, ListCounts { devices: 0, spatial: 0, apps: 0 });
        assert_eq!(base - loading, ROW);
        let with_spatial = popup_height(&px, lh, ListCounts { devices: 2, spatial: 3, apps: 0 });
        assert_eq!(with_spatial - base, 6 + 1 + 6 + lh + 4 + 3 * ROW);
        let full = popup_height(&px, lh, ListCounts { devices: 2, spatial: 3, apps: 4 });
        assert_eq!(full - with_spatial, 6 + 1 + 6 + lh + 4 + 4 * ROW);
        // IDs do not collide at the maximum counts.
        assert!(ID_DEVICE + 99 < ID_SPATIAL);
        assert!(ID_SPATIAL + 8 < ID_APP_MUTE);
        assert!(ID_APP_MUTE + audio::MAX_SESSIONS as u32 <= ID_APP_SLIDER);
        assert!(ID_APP_SLIDER + audio::MAX_SESSIONS as u32 <= ID_END);
    }
}
