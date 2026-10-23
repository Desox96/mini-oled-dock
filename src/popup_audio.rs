//! AUDIO popup: volume slider (drag + wheel, `ShellRequest::VolumeSet`),
//! mute button (`VolumeToggleMute`), list of output devices (`ListOutputs` on open,
//! click = `SetDefaultOutput`), "Volume mixer" link (`ms-settings:apps-volume`).
//! Initial state: the last known `VolumeState` passed by `popup::toggle` (via `DataEvent::Volume`).

use windows::Win32::Foundation::RECT;

use crate::audio::{OutputDevice, VolumeState, WHEEL_STEP_PERCENT};
use crate::glyphs;
use crate::popup::{center_y, hline, launch, lift, row_plate, tick_ms, Content, DataEvent, HitMap, PopupKind, Reply};
use crate::shell_thread::{self, ShellRequest};
use crate::surface::{rect_wh, Font, Surface, PANEL_PAD, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};

const ID_MUTE: u32 = 1;
const ID_SLIDER: u32 = 2;
const ID_MIXER: u32 = 3;
/// Device rows have id `ID_DEVICE + index`.
const ID_DEVICE: u32 = 100;

/// Minimum interval between two `VolumeSet` calls while dragging (ms).
const SET_THROTTLE_MS: u64 = 30;

pub(crate) struct AudioPopup {
    level: u8,
    muted: bool,
    available: bool,
    /// `None` until the first reply to `ListOutputs` arrives.
    outputs: Option<Vec<OutputDevice>>,
    dragging: bool,
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
}

/// Creates the content (initial state).
pub(crate) fn new(volume: VolumeState) -> Box<dyn Content> {
    // Fresh data: the volume is re-read and the device list is requested from the shell thread.
    shell_thread::request(ShellRequest::VolumeRefresh);
    shell_thread::request(ShellRequest::ListOutputs);
    Box::new(AudioPopup {
        level: volume.level.min(100),
        muted: volume.muted,
        available: volume.available,
        outputs: None,
        dragging: false,
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
    })
}

impl AudioPopup {
    /// Extremes (physical px) along which the center of the knob moves.
    fn track(&self) -> (i32, i32) {
        (self.slider.left + self.knob_r, self.slider.right - self.knob_r)
    }

    fn level_at(&self, x: i32) -> u8 {
        let (l, r) = self.track();
        let span = (r - l).max(1);
        (((x - l) * 100 + span / 2) / span).clamp(0, 100) as u8
    }

    fn send_level(&mut self) {
        self.last_set = tick_ms();
        shell_thread::request(ShellRequest::VolumeSet { percent: self.level });
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
        let mut y = row_top + btn + p(8);
        self.sep1 = y;
        y += p(1) + p(6);
        self.y_label = y;
        y += s.line_height(Font::Small) + p(4);
        let inset = p(8);
        let row = p(34);
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
        y += p(6);
        self.sep2 = y;
        y += p(1) + p(6);
        self.hits.add(ID_MIXER, rect_wh(inset, y, self.width - 2 * inset, row));
        y += row + inset;
        (self.width, y)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let icon = m.px(16);
        // Speaker button.
        if let Some(rc) = self.hits.rect(ID_MUTE) {
            let h = row_plate(s, rc, ID_MUTE, hot, pressed);
            let waves = glyphs::waves_for(self.level);
            let muted = self.muted || !self.available;
            let x = rc.left + ((rc.right - rc.left) - icon) / 2;
            let y = rc.top + ((rc.bottom - rc.top) - icon) / 2;
            s.glyph(x, y, icon, lift(TEXT_ALPHA, h), &|u, v| glyphs::speaker_shape(u, v, waves, muted));
        }
        // Slider.
        let knob_r = self.knob_r;
        let (tl, tr) = self.track();
        let cy = (self.slider.top + self.slider.bottom) / 2;
        let th = m.px(4);
        s.fill_rr(
            RECT { left: self.slider.left, top: cy - th / 2, right: self.slider.right, bottom: cy - th / 2 + th },
            th / 2,
            WHITE,
            60,
        );
        let kx = tl + (tr - tl) * self.level as i32 / 100;
        s.fill_rr(
            RECT { left: self.slider.left, top: cy - th / 2, right: kx, bottom: cy - th / 2 + th },
            th / 2,
            WHITE,
            230,
        );
        s.fill_rr(
            RECT { left: kx - knob_r, top: cy - knob_r, right: kx + knob_r, bottom: cy + knob_r },
            knob_r,
            WHITE,
            255,
        );
        // Percentage.
        let pct = format!("{}%", self.level);
        let tw = s.text_width(&pct, Font::Normal);
        let lh = s.line_height(Font::Normal);
        let btn_rc = self.hits.rect(ID_MUTE).unwrap_or_default();
        s.text(&pct, self.pct_right - tw, center_y(&btn_rc, lh), Font::Normal, WHITE, TEXT2_ALPHA);

        hline(s, m.px(TEXT_COL), self.width - m.px(TEXT_COL), self.sep1);
        s.text("Output", m.px(TEXT_COL), self.y_label, Font::Small, WHITE, TEXT3_ALPHA);

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
                let rc = rect_wh(0, self.y_empty, self.width, m.px(34));
                let txt = "No output device";
                let tw = s.text_width(txt, Font::Normal);
                s.text(txt, (self.width - tw) / 2, center_y(&rc, lh), Font::Normal, WHITE, TEXT2_ALPHA);
            }
            None => {
                let rc = rect_wh(0, self.y_empty, self.width, m.px(34));
                let txt = "Loading\u{2026}";
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

        hline(s, m.px(TEXT_COL), self.width - m.px(TEXT_COL), self.sep2);
        if let Some(rc) = self.hits.rect(ID_MIXER) {
            let h = row_plate(s, rc, ID_MIXER, hot, pressed);
            s.text("Volume mixer", rc.left + m.px(12), center_y(&rc, lh), Font::Normal, WHITE, lift(TEXT_ALPHA, h));
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
            i if i >= ID_DEVICE => {
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
            _ => Reply::None,
        }
    }

    fn drag(&mut self, id: u32, x: i32, _y: i32) -> Reply {
        if id != ID_SLIDER {
            return Reply::None;
        }
        let lvl = self.level_at(x);
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
            _ => Reply::None,
        }
    }

    fn after_close(&mut self) {
        if self.open_mixer {
            launch("ms-settings:apps-volume");
        }
    }
}
