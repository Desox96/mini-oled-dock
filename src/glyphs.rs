//! Hand-drawn vector icons, shared by the bar (`render`) and the popups (`surface`/`popup_*`).
//!
//! Each shape is a PURE function `(u, v) -> bool` in the 16x16 logical space (u = x, v = y, origin
//! at the top left): true if the point is inside the shape. The caller evaluates it with 4x4
//! supersampling (`render::Canvas::vector`, `surface::Surface::glyph`), so there is no antialiasing here; the only state is the scale (`set_scale`), which makes strokes whole physical pixels.
//! No resources, no icon font, no GDI dependency.

use std::cell::Cell;

/// Nominal thickness of "flush" strokes (16x16 units): in physical pixels it becomes a whole number
/// (`stroke_dev`), so at 100% strokes are crisp at 1 px instead of smearing over 2.
const STROKE_NOMINAL: f32 = 1.3;

thread_local! {
    /// Current scale (physical pixels per 16x16 unit), set by the caller before evaluating the shape.
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

/// Sets the scale for the following shapes: `size` = side in physical pixels of the 16x16 square.
/// Called by `render::Canvas::vector` and `surface::Surface::glyph` (same thread that evaluates the shapes).
pub fn set_scale(size: i32) {
    if size > 0 {
        SCALE.with(|c| c.set(size as f32 / 16.0));
    }
}

fn scale() -> f32 {
    SCALE.with(|c| c.get())
}

/// Stroke thickness in physical pixels (integer, at least 1).
fn stroke_dev(s: f32) -> f32 {
    (STROKE_NOMINAL * s).round().max(1.0)
}

/// Half stroke thickness in 16x16 units.
fn half() -> f32 {
    let s = scale();
    stroke_dev(s) / (2.0 * s)
}

/// Moves the center line `c` (16x16 units) to where a whole-number-thickness stroke is crisp: at the pixel
/// center if the thickness is odd, on the boundary between two pixels if it is even.
fn snap(c: f32) -> f32 {
    let s = scale();
    if stroke_dev(s) as i32 % 2 == 1 {
        ((c * s).floor() + 0.5) / s
    } else {
        (c * s).round() / s
    }
}

/// Moves `c` to the nearest pixel edge (for the outer edges of filled areas).
fn edge(c: f32) -> f32 {
    let s = scale();
    (c * s).round() / s
}

/// Vertical symmetry axis of the glyphs (8.0, snapped to the stroke grid).
fn ax() -> f32 {
    snap(8.0)
}

/// Center of a dot snapped to the pixel center.
fn pix_center(c: f32) -> f32 {
    let s = scale();
    ((c * s).floor() + 0.5) / s
}

/// Distance from point `(px, py)` to segment `(ax, ay)-(bx, by)`.
pub fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    let t = if len2 > 0.0 { ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (wx - t * vx, wy - t * vy);
    (dx * dx + dy * dy).sqrt()
}

/// Polyline `pts` with half thickness `half`.
fn polyline(u: f32, v: f32, pts: &[(f32, f32)], half: f32) -> bool {
    pts.windows(2).any(|w| dist_seg(u, v, w[0].0, w[0].1, w[1].0, w[1].1) <= half)
}

/// Outline bell (same stroke as the other icons): dome, flaring sides,
/// rim, knob and small clapper arc.
pub fn bell_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (dx, dy) = (u - c, v - 6.6);
    let dome = v <= 6.6 && ((dx * dx + dy * dy).sqrt() - 3.4).abs() <= st;
    let rim_y = snap(11.6);
    let (l0, l1, l2) = (c - 3.4, c - 3.1, c - 4.8);
    let (r0, r1, r2) = (c + 3.4, c + 3.1, c + 4.8);
    let side_l = dist_seg(u, v, l0, 6.6, l1, 9.4) <= st || dist_seg(u, v, l1, 9.4, l2, rim_y) <= st;
    let side_r = dist_seg(u, v, r0, 6.6, r1, 9.4) <= st || dist_seg(u, v, r1, 9.4, r2, rim_y) <= st;
    let rim = dist_seg(u, v, l2, rim_y, r2, rim_y) <= st;
    let knob = dist_seg(u, v, c, 1.8, c, 2.6) <= st;
    let (cx, cy) = (u - c, v - (rim_y + 0.6));
    let clapper = v >= rim_y + 0.6 && ((cx * cx + cy * cy).sqrt() - 1.6).abs() <= st;
    dome || side_l || side_r || rim || knob || clapper
}

/// Center and radius of the notification badge in 16x16 space (the center is snapped to the pixel).
const BADGE_CX: f32 = 12.75;
const BADGE_CY: f32 = 3.25;
const BADGE_R: f32 = 2.25;
/// Empty ring around the badge and the diagonal bars (16x16 units).
const CUT_GAP: f32 = 1.25;

/// Notification badge dot (to be drawn on top of the bell).
pub fn badge_shape(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - pix_center(BADGE_CX), v - pix_center(BADGE_CY));
    dx * dx + dy * dy <= BADGE_R * BADGE_R
}

/// Bell with the empty ring for the badge (same technique as the bar of `bell_off_shape`).
pub fn bell_badge_shape(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - pix_center(BADGE_CX), v - pix_center(BADGE_CY));
    let gap = CUT_GAP.max(1.0 / scale());
    bell_shape(u, v) && (dx * dx + dy * dy).sqrt() > BADGE_R + gap
}

/// Crossed-out bell ("Do not disturb" active): the same bell with a diagonal bar and an
/// empty halo around the bar (like the muted speaker). With `badge` the badge ring too.
pub fn bell_off_shape(u: f32, v: f32, badge: bool) -> bool {
    let st = half();
    let d = dist_seg(u, v, 2.0, 2.0, 14.0, 14.0);
    let base = if badge { bell_badge_shape(u, v) } else { bell_shape(u, v) };
    (base && d > st + 1.0) || d <= st
}

/// Outline speaker with `waves` waves (0..=3); if `muted`, no waves and a diagonal bar
/// with an empty halo around it.
pub fn speaker_shape(u: f32, v: f32, waves: u32, muted: bool) -> bool {
    let st = half();
    let (x_back, x_cone, x_tip) = (snap(2.0), snap(4.4), snap(8.0));
    let (y_top, y_bot) = (snap(6.2), snap(9.8));
    let pts = [
        (x_back, y_top),
        (x_cone, y_top),
        (x_tip, 3.2),
        (x_tip, 12.8),
        (x_cone, y_bot),
        (x_back, y_bot),
        (x_back, y_top),
    ];
    let mut shape = polyline(u, v, &pts, st);
    if muted {
        let d = dist_seg(u, v, 2.5, 2.5, 13.5, 13.5);
        return (shape && d > st + 1.0) || d <= st;
    }
    let (ddx, dy) = (u - (x_tip + 0.5), v - 8.0);
    for n in 0..waves {
        let r = 2.9 + 1.9 * n as f32;
        let d = (ddx * ddx + dy * dy).sqrt();
        if (d - r).abs() <= st && ddx > 0.0 && ddx >= dy.abs() * 0.9 {
            shape = true;
        }
    }
    shape
}

/// Number of speaker waves for a 0..=100 level.
pub fn waves_for(level: u8) -> u32 {
    match level {
        0 => 0,
        1..=33 => 1,
        34..=66 => 2,
        _ => 3,
    }
}

/// Power symbol (IEC 5009): ring open at the top + vertical stem.
pub fn power_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (cx, cy, r) = (c, 8.8, 5.2);
    let (dx, dy) = (u - cx, v - cy);
    let d = (dx * dx + dy * dy).sqrt();
    // Ring opening: a ~70 degree sector centered at the top (dy < 0).
    let gap = dy < 0.0 && dx.abs() < -dy * 0.72;
    let ring = (d - r).abs() <= st && !gap;
    let stem = dist_seg(u, v, c, 1.6, c, 8.2) <= st;
    ring || stem
}

/// "Show desktop": stylized 12x9 monitor (outline) with a stand, same stroke as the other icons.
pub fn show_desktop_shape(u: f32, v: f32) -> bool {
    let (l, t, r, b) = (edge(2.0), edge(2.2), edge(14.0), edge(11.2));
    let w = 2.0 * half();
    let inside_outer = u >= l && u <= r && v >= t && v <= b;
    let inside_inner = u > l + w && u < r - w && v > t + w && v < b - w;
    let frame = inside_outer && !inside_inner;
    let c = ax();
    let stem = (c - half()..=c + half()).contains(&u) && v > b && v <= edge(12.7);
    let base_y = edge(12.7);
    let base = (c - 3.0..=c + 3.0).contains(&u) && (base_y..=base_y + w).contains(&v);
    frame || stem || base
}

/// Chevron pointing up (tray overflow with the bar at the bottom).
pub fn chevron_up_shape(u: f32, v: f32) -> bool {
    let c = ax();
    polyline(u, v, &[(c - 4.0, 10.25), (c, 5.75), (c + 4.0, 10.25)], half())
}

/// Chevron pointing down (tray overflow with the bar at the top).
pub fn chevron_down_shape(u: f32, v: f32) -> bool {
    let c = ax();
    polyline(u, v, &[(c - 4.0, 5.75), (c, 10.25), (c + 4.0, 5.75)], half())
}

/// "Previous month" arrow of the calendar.
pub fn chevron_left_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(10.0, 4.0), (6.0, 8.0), (10.0, 12.0)], half())
}

/// "Next month" arrow of the calendar.
pub fn chevron_right_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(6.0, 4.0), (10.0, 8.0), (6.0, 12.0)], half())
}

/// Close X (dismissing a notification).
pub fn close_shape(u: f32, v: f32) -> bool {
    let st = half();
    dist_seg(u, v, 4.5, 4.5, 11.5, 11.5) <= st || dist_seg(u, v, 11.5, 4.5, 4.5, 11.5) <= st
}

/// Check mark (default audio device).
pub fn check_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)], half())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_hit_expected_points() {
        assert!(bell_shape(8.0, 11.6)); // rim
        assert!(!bell_shape(8.0, 8.0)); // empty interior
        assert!(badge_shape(12.75, 3.25));
        assert!(!bell_badge_shape(12.75, 3.25));
        assert!(!bell_shape(0.5, 0.5));
        assert!(power_shape(8.0, 4.0)); // stem
        assert!(!power_shape(8.0, 8.8)); // empty center
        assert!(show_desktop_shape(2.2, 6.0)); // left side of the monitor
        assert!(!show_desktop_shape(8.0, 7.0)); // empty screen
        assert!(chevron_up_shape(8.0, 6.0));
        assert!(close_shape(8.0, 8.0));
        assert!(check_shape(6.5, 11.5));
        assert_eq!(waves_for(0), 0);
        assert_eq!(waves_for(100), 3);
    }
}
