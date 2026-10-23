//! Icon bitmaps: HICON/HBITMAP -> premultiplied pixel conversion, hand-written area (box)
//! filter resizing, PNG on disk via WIC (decode/encode) and cache.
//!
//! THREADING: ALL functions in this module that touch GDI/COM/WIC must be called from the
//! shell thread (COM STA already initialized). `IconBitmap`, `sanitize_key`, `resize_area`,
//! `cache_path`, `is_plain_icon_name` are pure/thread-safe and usable anywhere.
//!
//! FORMAT: `IconBitmap::pixels` is a row-major, top-down `Vec<u32>`, each pixel 0xAARRGGBB
//! PREMULTIPLIED (the same layout as the 32-bit DIB of `render`, so blending is a direct
//! read). On disk the PNGs are NOT premultiplied (converted on read/write).
//!
//! FILE I/O: every file is read into memory through `datadir` (no reparse point followed, size cap)
//! and decoded from memory; PNGs are encoded into memory and written with `datadir::write_atomic`.
//! WIC never opens a path itself.
//!
//! CUSTOM ICONS: `%ProgramData%\mini-oled-dock\icons\<name>`; the user can create files there, so
//! `datadir::read_icon` checks the handle (regular file, one link, final path inside the folder).
//!
//! DISK CACHE: `%ProgramData%\mini-oled-dock\cache\<sanitized-key>.png` (admin-only), always
//! `MASTER_PX` x `MASTER_PX` (64). The UI then asks for the final size
//! (24 logical * dpi/96) and `resize_area` does the downscale. Invalidated if the exe mtime is
//! newer than the cached PNG; if the exe mtime is unknown (UWP apps) the cache
//! expires after `CACHE_MAX_AGE_NO_MTIME` (7 days).

use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{HGLOBAL, SIZE};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_VendorMicrosoftBuiltIn, GUID_WICPixelFormat32bppBGRA,
    GUID_WICPixelFormat32bppPBGRA, IWICBitmapDecoder, IWICBitmapFrameEncode, IWICBitmapScaler, IWICBitmapSource,
    IWICImagingFactory, IWICStream, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{
    CoCreateInstance, IBindCtx, IStream, CLSCTX_INPROC_SERVER, STREAM_SEEK_CUR, STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfoExW, HICON, ICONINFOEXW};

use crate::log;

/// Side (px) of the master PNG in the disk cache.
pub const MASTER_PX: u32 = 64;

/// Cache expiry when the exe mtime is unknown (UWP packages).
const CACHE_MAX_AGE_NO_MTIME: Duration = Duration::from_secs(7 * 24 * 3600);
/// Maximum side (px) above which a loaded PNG is downscaled by WIC before the copy into RAM.
const LOAD_MAX_SIDE: u32 = 512;
/// Maximum accepted side for a source HICON/HBITMAP.
const MAX_SRC_SIDE: i32 = 2048;
/// Alpha threshold below which a pixel is considered "empty" for border cropping.
const CROP_ALPHA_MIN: u32 = 8;

/// Premultiplied RGBA image (see FORMAT in the module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IconBitmap {
    pub width: u32,
    pub height: u32,
    /// `width * height` premultiplied 0xAARRGGBB pixels, top-down.
    pub pixels: Vec<u32>,
}

impl IconBitmap {
    /// Empty (transparent) `w` x `h` bitmap.
    pub fn new(w: u32, h: u32) -> IconBitmap {
        IconBitmap { width: w, height: h, pixels: vec![0; (w as usize) * (h as usize)] }
    }

    /// true if it contains no pixels.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.pixels.is_empty()
    }

    /// true if at least one pixel has alpha != 0 (used to discard "empty" extractions).
    pub fn has_visible_pixels(&self) -> bool {
        self.pixels.iter().any(|p| p >> 24 != 0)
    }
}

/// Turns a grouping key into a valid file name: characters outside
/// `[A-Za-z0-9._-]` replaced by `_`, lowercase, max 120 characters.
/// E.g. `"C:\\Program Files\\x.exe"` -> `"c__program_files_x.exe"`.
pub fn sanitize_key(key: &str) -> String {
    let s: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c.to_ascii_lowercase() } else { '_' })
        .take(120)
        .collect();
    if s.is_empty() {
        "_".to_string()
    } else {
        s
    }
}

/// Path of the cache PNG for `key` (`<cache>/<sanitize_key(key)>.png`); `None` without a cache folder.
pub fn cache_path(key: &str) -> Option<PathBuf> {
    crate::datadir::cache_dir().map(|d| d.join(format!("{}.png", sanitize_key(key))))
}

/// The user's custom icon for `key`, decoded: if `configured` (the pin's `icon` field) is a valid
/// plain file name (`is_plain_icon_name`), `icons\<configured>`; otherwise (or if that is missing)
/// `icons\<sanitize_key(key)>.png`. Shell thread (WIC).
///
/// SECURITY: the config is untrusted input (the user edits it; it used to be writable by any
/// medium process), so `configured` must be a plain name: absolute, UNC, `..` or ADS paths are
/// rejected. The file itself is read by `datadir::read_icon`, which refuses reparse points, hard
/// links and anything whose final path is not exactly inside the icons folder, and caps the size;
/// the bytes are then decoded from memory.
pub fn load_custom_icon(key: &str, configured: Option<&str>) -> Option<IconBitmap> {
    if let Some(c) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        if !is_plain_icon_name(c) {
            crate::log!("icons: custom icon '{c}' rejected: only a plain file name inside the icons folder is allowed");
        } else if let Some(b) = read_custom(c) {
            return Some(b);
        }
    }
    let by_key = format!("{}.png", sanitize_key(key));
    if is_plain_icon_name(&by_key) {
        read_custom(&by_key)
    } else {
        None
    }
}

/// Reads and decodes `icons\<name>`; a missing file is silent, anything else is logged.
fn read_custom(name: &str) -> Option<IconBitmap> {
    match crate::datadir::read_icon(name) {
        Ok(bytes) => {
            let b = decode_image_bytes(&bytes);
            if b.is_none() {
                log!("icons: custom icon '{name}' could not be decoded, using the extracted one");
            }
            b
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log!("icons: custom icon '{name}' refused: {e}");
            None
        }
    }
}

/// Windows reserved device names (with or without extension they never name a regular file).
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// true if `name` is a single, plain file name: one `Normal` path component, no separators
/// (`\` or `/`), no `.`/`..`, no drive/UNC/verbatim prefix, no `:` (drive or alternate data
/// stream), no wildcard/control characters, no trailing dot or space (Win32 strips them),
/// not a reserved device name, at most 255 characters. Pure.
pub fn is_plain_icon_name(name: &str) -> bool {
    use std::path::Component;
    if name.is_empty() || name.len() > 255 || name.ends_with(['.', ' ']) {
        return false;
    }
    if name.chars().any(|c| c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return false;
    }
    let mut comps = Path::new(name).components();
    if !matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None)) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    !RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

// ------------------------------------------------------------------ pixel: utilities

/// Premultiplies a "straight" 0xAARRGGBB pixel.
fn premultiply_px(p: u32) -> u32 {
    let a = p >> 24;
    match a {
        255 => p,
        0 => 0,
        _ => {
            let m = |c: u32| (c * a + 127) / 255;
            (a << 24) | (m((p >> 16) & 255) << 16) | (m((p >> 8) & 255) << 8) | m(p & 255)
        }
    }
}

/// Inverse of `premultiply_px` (for writing non-premultiplied PNGs).
fn unpremultiply_px(p: u32) -> u32 {
    let a = p >> 24;
    match a {
        255 => p,
        0 => 0,
        _ => {
            let u = |c: u32| ((c * 255 + a / 2) / a).min(255);
            (a << 24) | (u((p >> 16) & 255) << 16) | (u((p >> 8) & 255) << 8) | u(p & 255)
        }
    }
}

// ------------------------------------------------------------------ GDI -> IconBitmap

/// Size (width, height) of an HBITMAP, `None` if invalid or too large.
fn bitmap_size(hbm: HBITMAP) -> Option<(i32, i32)> {
    if hbm.0.is_null() {
        return None;
    }
    let mut bm = BITMAP::default();
    // SAFETY: `hbm` is a valid HBITMAP owned by the caller; `bm` is writable and as large
    // as declared in the `c` parameter.
    let n =
        unsafe { GetObjectW(HGDIOBJ(hbm.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void)) };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        return None;
    }
    let h = bm.bmHeight.abs();
    if bm.bmWidth > MAX_SRC_SIDE || h > MAX_SRC_SIDE {
        return None;
    }
    Some((bm.bmWidth, h))
}

/// Reads an HBITMAP as a top-down 32-bit DIB (each u32 = B | G<<8 | R<<16 | A<<24, i.e.
/// 0xAARRGGBB). `h` is the number of rows to read. The HBITMAP must not be selected into a DC.
fn read_dib32(hbm: HBITMAP, w: i32, h: i32) -> Option<Vec<u32>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut buf = vec![0u32; (w as usize) * (h as usize)];
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: GetDC(None) returns the screen DC, released right below with ReleaseDC. `buf`
    // holds exactly w*h u32 = w*h*4 bytes, as much as GetDIBits writes for `h` 32-bit rows; `bmi` is valid.
    let lines = unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let n = GetDIBits(hdc, hbm, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, hdc);
        n
    };
    if lines == 0 {
        None
    } else {
        Some(buf)
    }
}

fn delete_bitmap(hbm: HBITMAP) {
    if !hbm.0.is_null() {
        // SAFETY: bitmap created by GetIconInfoExW and owned by us; it is not selected into any DC.
        let _ = unsafe { DeleteObject(HGDIOBJ(hbm.0)) };
    }
}

/// HICON -> premultiplied bitmap at the icon's native size (`GetIconInfoExW` +
/// `GetDIBits`; if the alpha channel is all 0 it uses the AND mask, otherwise it normalizes).
/// Shell thread. Does not destroy `icon` (ownership stays with the caller).
pub fn hicon_to_bitmap(icon: HICON) -> Option<IconBitmap> {
    if icon.is_invalid() {
        return None;
    }
    let mut info = ICONINFOEXW { cbSize: size_of::<ICONINFOEXW>() as u32, ..Default::default() };
    // SAFETY: `icon` is a valid HICON (the caller owns it or read it from a
    // window); `info` has cbSize set and lives for the whole call.
    if !unsafe { GetIconInfoExW(icon, &mut info) }.as_bool() {
        log!("icons::hicon_to_bitmap: GetIconInfoExW failed");
        return None;
    }
    let (color, mask) = (info.hbmColor, info.hbmMask);
    let out = if color.0.is_null() { monochrome_icon(mask) } else { color_icon(color, mask) };
    // GetIconInfoExW creates two new bitmaps: we must delete them ourselves.
    delete_bitmap(color);
    delete_bitmap(mask);
    out
}

/// Color icon: pixels from the color bitmap; if it has no real alpha, alpha comes from the AND mask.
fn color_icon(color: HBITMAP, mask: HBITMAP) -> Option<IconBitmap> {
    let (w, h) = bitmap_size(color)?;
    let mut px = read_dib32(color, w, h)?;
    if px.iter().any(|p| p >> 24 != 0) {
        // Real alpha (not premultiplied in GDI icons): premultiply.
        for p in px.iter_mut() {
            *p = premultiply_px(*p);
        }
    } else {
        // No alpha: AND mask (bit 1 = transparent). GetDIBits expands it to 0x000000/0xFFFFFF.
        let m = match bitmap_size(mask) {
            Some((mw, mh)) if mw == w && mh >= h => read_dib32(mask, mw, mh),
            _ => None,
        };
        match m {
            Some(m) => {
                for (p, mk) in px.iter_mut().zip(m.iter()) {
                    *p = if mk & 0x00FF_FFFF == 0 { 0xFF00_0000 | (*p & 0x00FF_FFFF) } else { 0 };
                }
            }
            None => {
                for p in px.iter_mut() {
                    *p |= 0xFF00_0000;
                }
            }
        }
    }
    Some(IconBitmap { width: w as u32, height: h as u32, pixels: px })
}

/// Monochrome icon (no color bitmap): the mask is twice as tall (AND on top, XOR below).
fn monochrome_icon(mask: HBITMAP) -> Option<IconBitmap> {
    let (w, h2) = bitmap_size(mask)?;
    let h = h2 / 2;
    if h <= 0 {
        return None;
    }
    let m = read_dib32(mask, w, h2)?;
    let n = (w as usize) * (h as usize);
    let mut px = vec![0u32; n];
    for i in 0..n {
        let and = m[i] & 0x00FF_FFFF != 0;
        let xor = m[n + i] & 0x00FF_FFFF != 0;
        px[i] = match (and, xor) {
            (false, false) => 0xFF00_0000,
            (false, true) => 0xFFFF_FFFF,
            (true, true) => 0xFFFF_FFFF, // "invert": approximated with white
            (true, false) => 0,
        };
    }
    Some(IconBitmap { width: w as u32, height: h as u32, pixels: px })
}

/// 32-bit HBITMAP (e.g. from `IShellItemImageFactory::GetImage`) -> premultiplied bitmap.
/// If the HBITMAP has no real alpha (all alpha pixels 0) it is assumed opaque. Does not destroy `bmp`.
pub fn hbitmap_to_bitmap(bmp: HBITMAP) -> Option<IconBitmap> {
    let (w, h) = bitmap_size(bmp)?;
    let mut px = read_dib32(bmp, w, h)?;
    if px.iter().any(|p| p >> 24 != 0) {
        for p in px.iter_mut() {
            *p = premultiply_px(*p);
        }
    } else {
        for p in px.iter_mut() {
            *p |= 0xFF00_0000;
        }
    }
    Some(IconBitmap { width: w as u32, height: h as u32, pixels: px })
}

// ------------------------------------------------------------------ resizing

/// Rectangle (x, y, w, h) of pixels with alpha >= `CROP_ALPHA_MIN`; `None` if there are none.
fn opaque_bbox(src: &IconBitmap) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = (src.width as usize, src.height as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        let row = &src.pixels[y * w..(y + 1) * w];
        for (x, p) in row.iter().enumerate() {
            if p >> 24 >= CROP_ALPHA_MIN {
                x0 = x0.min(x);
                x1 = x1.max(x + 1);
                y0 = y0.min(y);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 > x0 && y1 > y0 {
        Some((x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

/// True when `resize_area(src, size_px)` would return `src` unchanged: already `size_px` square
/// and nothing to crop (no transparent border, or fully transparent).
pub fn resize_is_identity(src: &IconBitmap, size_px: u32) -> bool {
    let (w, h) = (src.width as usize, src.height as usize);
    size_px > 0
        && src.width == size_px
        && src.height == size_px
        && src.pixels.len() == w * h
        && opaque_bbox(src).is_none_or(|b| b == (0, 0, w, h))
}

/// Resizes to `size_px` x `size_px` (square side) with an area (box) filter using
/// fractional weights, on premultiplied data. The source is first cropped to its
/// transparent borders and centered in a square (jumbo icons often have an empty margin).
/// If the magnification is > 1 (source smaller than the destination) it uses bilinear
/// interpolation. `size_px == 0` or an empty/inconsistent source -> empty bitmap.
/// Pure, usable from any thread.
pub fn resize_area(src: &IconBitmap, size_px: u32) -> IconBitmap {
    let (sw, sh) = (src.width as usize, src.height as usize);
    if size_px == 0 || src.is_empty() || src.pixels.len() != sw * sh {
        return IconBitmap::default();
    }
    let (cx, cy, cw, ch) = opaque_bbox(src).unwrap_or((0, 0, sw, sh));
    let side = cw.max(ch);
    // Offset of the crop inside the virtual `side` x `side` square.
    let (ox, oy) = ((side - cw) / 2, (side - ch) / 2);
    let n = size_px as usize;
    let scale = side as f32 / n as f32; // source pixels per destination pixel

    // Premultiplied pixel as [a, r, g, b] in f32; outside the crop = transparent.
    let fetch = |vx: isize, vy: isize| -> [f32; 4] {
        let (x, y) = (vx - ox as isize, vy - oy as isize);
        if x < 0 || y < 0 || x >= cw as isize || y >= ch as isize {
            return [0.0; 4];
        }
        let p = src.pixels[(cy + y as usize) * sw + cx + x as usize];
        [(p >> 24) as f32, ((p >> 16) & 255) as f32, ((p >> 8) & 255) as f32, (p & 255) as f32]
    };

    let mut out = IconBitmap::new(size_px, size_px);
    for dy in 0..n {
        for dx in 0..n {
            let acc: [f32; 4] = if scale >= 1.0 {
                // Area filter: weighted average of the source pixels covered by [dx*s, (dx+1)*s).
                let (fx0, fx1) = (dx as f32 * scale, (dx + 1) as f32 * scale);
                let (fy0, fy1) = (dy as f32 * scale, (dy + 1) as f32 * scale);
                let mut sum = [0.0f32; 4];
                let (ix0, ix1) = (fx0.floor() as isize, (fx1.ceil() as isize).max(fx0.floor() as isize + 1));
                let (iy0, iy1) = (fy0.floor() as isize, (fy1.ceil() as isize).max(fy0.floor() as isize + 1));
                for sy in iy0..iy1 {
                    let wy = (fy1.min((sy + 1) as f32) - fy0.max(sy as f32)).max(0.0);
                    for sx in ix0..ix1 {
                        let wx = (fx1.min((sx + 1) as f32) - fx0.max(sx as f32)).max(0.0);
                        let wgt = wx * wy;
                        if wgt <= 0.0 {
                            continue;
                        }
                        let v = fetch(sx, sy);
                        for k in 0..4 {
                            sum[k] += v[k] * wgt;
                        }
                    }
                }
                let area = scale * scale;
                [sum[0] / area, sum[1] / area, sum[2] / area, sum[3] / area]
            } else {
                // Magnification: bilinear at the center of the destination pixel.
                let sxf = (dx as f32 + 0.5) * scale - 0.5;
                let syf = (dy as f32 + 0.5) * scale - 0.5;
                // Destination pixels whose centre falls in the square padding stay transparent;
                // inside the crop the taps are clamped to it (edge extension, no faded fringe).
                let inside = |c: f32, lo: usize, len: usize| c >= lo as f32 && c < (lo + len) as f32;
                if !inside(sxf + 0.5, ox, cw) || !inside(syf + 0.5, oy, ch) {
                    [0.0; 4]
                } else {
                    let (x0, y0) = (sxf.floor(), syf.floor());
                    let (tx, ty) = (sxf - x0, syf - y0);
                    let (x0, y0) = (x0 as isize, y0 as isize);
                    let (lx, hx) = (ox as isize, (ox + cw) as isize - 1);
                    let (ly, hy) = (oy as isize, (oy + ch) as isize - 1);
                    let (xa, xb) = (x0.clamp(lx, hx), (x0 + 1).clamp(lx, hx));
                    let (ya, yb) = (y0.clamp(ly, hy), (y0 + 1).clamp(ly, hy));
                    let (p00, p10) = (fetch(xa, ya), fetch(xb, ya));
                    let (p01, p11) = (fetch(xa, yb), fetch(xb, yb));
                    let mut v = [0.0f32; 4];
                    for k in 0..4 {
                        let top = p00[k] + (p10[k] - p00[k]) * tx;
                        let bot = p01[k] + (p11[k] - p01[k]) * tx;
                        v[k] = top + (bot - top) * ty;
                    }
                    v
                }
            };
            let a = (acc[0] + 0.5).clamp(0.0, 255.0) as u32;
            let c = |v: f32| ((v + 0.5).clamp(0.0, 255.0) as u32).min(a);
            out.pixels[dy * n + dx] = (a << 24) | (c(acc[1]) << 16) | (c(acc[2]) << 8) | c(acc[3]);
        }
    }
    out
}

// ------------------------------------------------------------------ WIC: PNG

fn wic_factory() -> Result<IWICImagingFactory, String> {
    // SAFETY: COM is initialized on the shell thread (STA); CLSID/IID match the requested type.
    unsafe { CoCreateInstance::<_, IWICImagingFactory>(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("CoCreateInstance(WICImagingFactory): {e}"))
}

/// First frame of `decoder` -> premultiplied bitmap (downscaled by WIC if > `LOAD_MAX_SIDE`).
fn decoder_to_bitmap(factory: &IWICImagingFactory, decoder: &IWICBitmapDecoder) -> Result<IconBitmap, String> {
    // SAFETY: COM calls on valid interfaces; the buffers passed have the declared size
    // (stride * height); no pointer outlives the function.
    unsafe {
        let frame = decoder.GetFrame(0).map_err(|e| format!("GetFrame: {e}"))?;
        let conv = factory.CreateFormatConverter().map_err(|e| format!("CreateFormatConverter: {e}"))?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .map_err(|e| format!("FormatConverter::Initialize: {e}"))?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).map_err(|e| format!("GetSize: {e}"))?;
        if w == 0 || h == 0 {
            return Err("empty image".into());
        }
        // Very large images (4K custom PNGs): WIC downscales them before copying into RAM.
        let mut source: IWICBitmapSource = conv.cast().map_err(|e| format!("cast: {e}"))?;
        if w.max(h) > LOAD_MAX_SIDE {
            let k = LOAD_MAX_SIDE as f32 / w.max(h) as f32;
            let (nw, nh) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
            let scaler: IWICBitmapScaler =
                factory.CreateBitmapScaler().map_err(|e| format!("CreateBitmapScaler: {e}"))?;
            scaler
                .Initialize(&source, nw, nh, WICBitmapInterpolationModeFant)
                .map_err(|e| format!("Scaler::Initialize: {e}"))?;
            source = scaler.cast().map_err(|e| format!("cast: {e}"))?;
            (w, h) = (nw, nh);
        }
        let stride = w * 4;
        let mut pixels = vec![0u32; (w as usize) * (h as usize)];
        // SAFETY: the u8 view covers exactly the w*h*4 bytes of `pixels`; u32 has no invalid bit
        // patterns and u8 alignment is weaker. BGRA little-endian bytes equal 0xAARRGGBB.
        let bytes = std::slice::from_raw_parts_mut(pixels.as_mut_ptr().cast::<u8>(), pixels.len() * 4);
        source.CopyPixels(std::ptr::null(), stride, bytes).map_err(|e| format!("CopyPixels: {e}"))?;
        Ok(IconBitmap { width: w, height: h, pixels })
    }
}

/// Encodes `bmp` as a PNG in memory (WIC PNG encoder, non-premultiplied 32bppBGRA) and returns the
/// file bytes. Requires COM on the calling thread.
fn encode_png(bmp: &IconBitmap) -> Result<Vec<u8>, String> {
    let factory = wic_factory()?;
    let bytes: Vec<u8> = bmp.pixels.iter().flat_map(|&p| unpremultiply_px(p).to_le_bytes()).collect();
    // SAFETY: COM calls on valid interfaces; `bytes` holds width*height*4 bytes as declared
    // to WritePixels (stride = width*4, lines = height). `out` is writable for the `len` bytes
    // passed to Read. The HGLOBAL stream owns (and frees) its memory.
    unsafe {
        let stream: IStream =
            CreateStreamOnHGlobal(HGLOBAL::default(), true).map_err(|e| format!("CreateStreamOnHGlobal: {e}"))?;
        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
            .map_err(|e| format!("CreateEncoder: {e}"))?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).map_err(|e| format!("Encoder::Initialize: {e}"))?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).map_err(|e| format!("CreateNewFrame: {e}"))?;
        let frame = frame.ok_or_else(|| "CreateNewFrame: null frame".to_string())?;
        frame.Initialize(None::<&IPropertyBag2>).map_err(|e| format!("Frame::Initialize: {e}"))?;
        frame.SetSize(bmp.width, bmp.height).map_err(|e| format!("SetSize: {e}"))?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt).map_err(|e| format!("SetPixelFormat: {e}"))?;
        if fmt != GUID_WICPixelFormat32bppBGRA {
            return Err("the PNG encoder does not accept 32-bit BGRA".into());
        }
        frame.WritePixels(bmp.height, bmp.width * 4, &bytes).map_err(|e| format!("WritePixels: {e}"))?;
        frame.Commit().map_err(|e| format!("Frame::Commit: {e}"))?;
        encoder.Commit().map_err(|e| format!("Encoder::Commit: {e}"))?;
        let mut len = 0u64;
        stream.Seek(0, STREAM_SEEK_CUR, Some(&mut len)).map_err(|e| format!("Seek: {e}"))?;
        let len = u32::try_from(len).map_err(|_| "PNG too large".to_string())?;
        stream.Seek(0, STREAM_SEEK_SET, None).map_err(|e| format!("Seek: {e}"))?;
        let mut out = vec![0u8; len as usize];
        let mut read = 0u32;
        stream.Read(out.as_mut_ptr().cast(), len, Some(&mut read)).ok().map_err(|e| format!("Read: {e}"))?;
        out.truncate(read as usize);
        Ok(out)
    }
}

/// true if `bmp` has a consistent, non-empty pixel buffer.
fn encodable(bmp: &IconBitmap) -> bool {
    !bmp.is_empty() && bmp.pixels.len() == (bmp.width as usize) * (bmp.height as usize)
}

/// Writes `bmp` as a PNG file (encoded in memory, then `datadir::write_atomic`: new temp file +
/// rename). Creates missing folders. Used by `--preview` (a folder chosen by the user). Shell thread.
pub fn save_png(path: &Path, bmp: &IconBitmap) -> bool {
    if !encodable(bmp) {
        return false;
    }
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log!("icons::save_png: create_dir_all({}): {e}", dir.display());
            return false;
        }
    }
    let res =
        encode_png(bmp).and_then(|bytes| crate::datadir::write_atomic(path, &bytes, crate::datadir::FileAcl::Inherit));
    if let Err(e) = &res {
        log!("icons::save_png({}): {e}", path.display());
    }
    res.is_ok()
}

// ------------------------------------------------------------------ disk cache

/// Looks up `key` in the disk cache. Valid only if `exe_mtime` (mtime of the exe/package, if
/// known) is NOT newer than the cached PNG; if unknown, only if the PNG is under 7 days old.
/// Returns the already-loaded master PNG (`MASTER_PX`). Shell thread.
pub fn cache_load(key: &str, exe_mtime: Option<SystemTime>) -> Option<IconBitmap> {
    let path = cache_path(key)?;
    let png_mtime = file_mtime(&path)?;
    match exe_mtime {
        Some(m) if m > png_mtime => return None,
        Some(_) => {}
        None => {
            if SystemTime::now().duration_since(png_mtime).map(|d| d > CACHE_MAX_AGE_NO_MTIME).unwrap_or(false) {
                return None;
            }
        }
    }
    let bytes = crate::datadir::read_file(&path, crate::datadir::CACHE_MAX_BYTES, true)
        .map_err(|e| {
            if e.kind() != std::io::ErrorKind::NotFound {
                log!("icons::cache_load({}): {e}", path.display());
            }
        })
        .ok()?;
    let bmp = decode_image_bytes(&bytes)?;
    if bmp.width != MASTER_PX || bmp.height != MASTER_PX {
        log!("icons::cache_load: {} is not {MASTER_PX}x{MASTER_PX}, discarded", path.display());
        return None;
    }
    Some(bmp)
}

/// Saves the master `bmp` (see `MASTER_PX`) in the cache for `key`. Errors are only logged.
/// Shell thread.
pub fn cache_store(key: &str, bmp: &IconBitmap) {
    if !crate::datadir::writable() || !encodable(bmp) {
        return;
    }
    let Some(path) = cache_path(key) else { return };
    // The cache folder is admin-only (created and verified by `datadir`): the PNG inherits its DACL.
    let res =
        encode_png(bmp).and_then(|bytes| crate::datadir::write_atomic(&path, &bytes, crate::datadir::FileAcl::Inherit));
    if let Err(e) = res {
        log!("icons::cache_store: saving '{key}' failed: {e}");
    }
}

/// mtime of a file (None if missing/unreadable). Useful for `cache_load`.
pub fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, p: u32) -> IconBitmap {
        IconBitmap { width: w, height: h, pixels: vec![p; (w * h) as usize] }
    }

    #[test]
    fn resize_down_keeps_solid_colour() {
        let s = solid(256, 256, 0xFF80_4020);
        let r = resize_area(&s, 24);
        assert_eq!((r.width, r.height), (24, 24));
        assert!(r.pixels.iter().all(|&p| p == 0xFF80_4020));
    }

    #[test]
    fn resize_up_keeps_solid_colour() {
        let r = resize_area(&solid(16, 16, 0x8040_2010), 64);
        assert!(r.pixels.iter().all(|&p| p == 0x8040_2010));
    }

    #[test]
    fn transparent_border_is_cropped() {
        let mut s = IconBitmap::new(100, 100);
        for y in 40..60 {
            for x in 40..60 {
                s.pixels[y * 100 + x] = 0xFFFF_FFFF;
            }
        }
        let r = resize_area(&s, 10);
        assert!(r.pixels.iter().all(|&p| p == 0xFFFF_FFFF));
    }

    #[test]
    fn channels_never_exceed_alpha() {
        let mut s = IconBitmap::new(8, 8);
        for (i, p) in s.pixels.iter_mut().enumerate() {
            *p = premultiply_px(0xFF00_0000 | ((i as u32 * 7919) & 0xFF_FFFF) | (((i as u32 * 37) & 0xFF) << 24));
        }
        let r = resize_area(&s, 5);
        for p in r.pixels {
            let a = p >> 24;
            assert!((p >> 16) & 255 <= a && (p >> 8) & 255 <= a && p & 255 <= a);
        }
    }

    #[test]
    fn zero_size_is_empty() {
        assert!(resize_area(&solid(4, 4, 0xFFFF_FFFF), 0).is_empty());
    }

    #[test]
    fn premultiply_roundtrip() {
        let p = 0x80FF_8000;
        let q = unpremultiply_px(premultiply_px(p));
        assert_eq!(q >> 24, 0x80);
        assert!(((q >> 16) & 255).abs_diff(0xFF) <= 2 && ((q >> 8) & 255).abs_diff(0x80) <= 2);
    }

    #[test]
    fn plain_icon_names() {
        for ok in ["discord.png", "my icon.png", "a.b.c.png", "x", "ÿ-ünicode.png"] {
            assert!(is_plain_icon_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "..\\x.png",
            "../x.png",
            "sub\\x.png",
            "sub/x.png",
            "C:\\Windows\\x.png",
            "C:x.png",
            "\\\\server\\share\\x.png",
            "\\\\?\\C:\\x.png",
            "\\x.png",
            "x.png:stream",
            "x.png::$DATA",
            "x.png.",
            "x.png ",
            "*.png",
            "a\u{0}b.png",
            "CON",
            "nul.png",
            "com1.txt",
        ] {
            assert!(!is_plain_icon_name(bad), "{bad:?}");
        }
        assert!(!is_plain_icon_name(&"a".repeat(256)));
    }

    #[test]
    fn png_signature_only() {
        assert!(is_png(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]));
        assert!(!is_png(&[0xFF, 0xD8, 0xFF, 0xE0, b'j', b'p', b'g']));
        assert!(!is_png(b"GIF89a"));
        assert!(!is_png(&[0x89, b'P', b'N']));
        // A non-PNG never reaches WIC (no COM needed for the refusal).
        assert!(decode_image_bytes(b"BM not a png").is_none());
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_key("C:\\Program Files\\x.exe"), "c__program_files_x.exe");
    }

    #[test]
    fn png_roundtrip_via_wic() {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        // SAFETY: COM for the test thread only.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut s = IconBitmap::new(64, 64);
        for y in 0..64usize {
            for x in 0..64usize {
                s.pixels[y * 64 + x] =
                    premultiply_px(0x8000_0000 | ((x as u32 * 4) << 16) | ((y as u32 * 4) << 8) | 0x40);
            }
        }
        let path = std::env::temp_dir().join("dock_icons_test_roundtrip.png");
        assert!(save_png(&path, &s));
        let bytes = crate::datadir::read_file(&path, crate::datadir::CACHE_MAX_BYTES, true).expect("read");
        let back = decode_image_bytes(&bytes).expect("decode");
        let _ = std::fs::remove_file(&path);
        assert_eq!((back.width, back.height), (64, 64));
        for (a, b) in s.pixels.iter().zip(back.pixels.iter()) {
            assert_eq!(a >> 24, b >> 24);
            for sh in [0, 8, 16] {
                assert!(((a >> sh) & 255).abs_diff((b >> sh) & 255) <= 2);
            }
        }
        if hr.is_ok() {
            // SAFETY: paired with the CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

// ------------------------------------------------------------------ notification icons

/// PNG file signature (first 8 bytes).
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// true if `bytes` starts with the PNG signature. Pure.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&PNG_SIGNATURE)
}

/// Decodes an in-memory PNG via WIC -> premultiplied bitmap. PNG ONLY: the bytes come from the user
/// (custom icons), the cache or other apps (notification logos), and the dock is elevated, so the
/// decoder is chosen explicitly (`CreateDecoder(PNG)`, preferring the built-in Microsoft one) instead
/// of letting WIC probe every installed codec (HEIF, WebP, RAW, third-party). Anything else is refused.
/// Requires COM initialized on the calling thread (MTA or STA).
pub fn decode_image_bytes(bytes: &[u8]) -> Option<IconBitmap> {
    if !is_png(bytes) {
        log!("icons: in-memory image refused: not a PNG");
        return None;
    }
    let r = (|| -> Result<IconBitmap, String> {
        let factory = wic_factory()?;
        // SAFETY: `stream` is initialized on the `bytes` buffer, which lives until the end of the function
        // (the decoder is only used in here).
        unsafe {
            let stream: IWICStream = factory.CreateStream().map_err(|e| format!("CreateStream: {e}"))?;
            stream.InitializeFromMemory(bytes).map_err(|e| format!("InitializeFromMemory: {e}"))?;
            let decoder = factory
                .CreateDecoder(&GUID_ContainerFormatPng, &GUID_VendorMicrosoftBuiltIn)
                .map_err(|e| format!("CreateDecoder(PNG): {e}"))?;
            decoder
                .Initialize(&stream, WICDecodeMetadataCacheOnDemand)
                .map_err(|e| format!("Decoder::Initialize: {e}"))?;
            decoder_to_bitmap(&factory, &decoder)
        }
    })();
    match r {
        Ok(b) if b.has_visible_pixels() => Some(b),
        Ok(_) => None,
        Err(e) => {
            log!("icons: in-memory image decoding failed: {e}");
            None
        }
    }
}

/// Icon of an app from the `shell:AppsFolder\<AUMID>` catalog (`IShellItemImageFactory`, `px` x `px`).
/// Requires COM initialized on the calling thread. `None` if the AUMID does not exist or the icon is empty.
pub fn appsfolder_icon(aumid: &str, px: i32) -> Option<IconBitmap> {
    let path = HSTRING::from(format!("shell:AppsFolder\\{aumid}"));
    // SAFETY: COM initialized by the caller; `path` lives for the call.
    let item: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(&path, None::<&IBindCtx>) }.ok()?;
    // SAFETY: valid `item`; the returned HBITMAP is ours and is deleted below.
    let hbm = unsafe { item.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY) }.ok()?;
    let b = hbitmap_to_bitmap(hbm).filter(IconBitmap::has_visible_pixels);
    // SAFETY: bitmap owned by us, not selected into any DC.
    let _ = unsafe { DeleteObject(HGDIOBJ(hbm.0)) };
    b
}
