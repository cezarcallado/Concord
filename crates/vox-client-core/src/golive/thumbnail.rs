//! The still picture of my live for hover previews (v1.1, see `vox_proto::stream_preview`).
//!
//! The frame comes from the capture that already runs (the sharer's preview tap, see
//! [`super::share_preview`]: a GPU downscale and a small readback, no second capture), copied once
//! on the capture thread. Everything else happens here, on a worker at low priority: a box
//! downscale to at most [`THUMB_MAX_W`] x [`THUMB_MAX_H`], BT.709 video-range NV12 to RGB, a JPEG
//! at [`THUMB_QUALITY`] (10-25 KB), and a 8 x 8 luma signature that tells whether the picture
//! changed enough to be worth sending again.

use vox_capture::Nv12Frame;

/// The picture fits inside this box (aspect kept, never enlarged).
pub const THUMB_MAX_W: u32 = 320;
pub const THUMB_MAX_H: u32 = 240;
/// JPEG quality: ~10-25 KB for 320 x 180 of a desktop or a game.
pub const THUMB_QUALITY: u8 = 65;
/// The app icon sent with the picture (pixels).
pub const ICON_SIDE: u32 = 32;

/// One NV12 frame owned (planes tightly packed: stride = width).
#[derive(Clone, PartialEq, Eq)]
pub struct Nv12Image {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

impl std::fmt::Debug for Nv12Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Nv12Image({}x{})", self.width, self.height)
    }
}

impl Nv12Image {
    /// Copy a borrowed capture frame (odd sizes are cut to even).
    pub fn copy_of(f: &Nv12Frame<'_>) -> Option<Self> {
        let (w, h) = (f.width & !1, f.height & !1);
        if w < 2 || h < 2 {
            return None;
        }
        let (ys, uvs) = (f.y_stride as usize, f.uv_stride as usize);
        let (wu, hu) = (w as usize, h as usize);
        if ys < wu || uvs < wu || f.y.len() < ys * (hu - 1) + wu || f.uv.len() < uvs * (hu / 2 - 1) + wu {
            return None;
        }
        let mut y = Vec::with_capacity(wu * hu);
        for r in 0..hu {
            y.extend_from_slice(&f.y[r * ys..r * ys + wu]);
        }
        let mut uv = Vec::with_capacity(wu * hu / 2);
        for r in 0..hu / 2 {
            uv.extend_from_slice(&f.uv[r * uvs..r * uvs + wu]);
        }
        Some(Self { width: w, height: h, y, uv })
    }
}

/// 8 x 8 mean luma of a picture: compared to tell "the screen changed" from capture noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    /// Changed enough to send again: a mean difference of more than 2 levels, or one cell (an
    /// eighth of the width and height) off by more than 20.
    pub fn differs(&self, other: &Signature) -> bool {
        let (mut sum, mut max) = (0u32, 0u8);
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            let d = a.abs_diff(*b);
            sum += d as u32;
            max = max.max(d);
        }
        sum > 2 * 64 || max > 20
    }
}

/// The 8 x 8 signature of `img` (luma only, a few thousand additions at most).
pub fn signature(img: &Nv12Image) -> Signature {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut out = [0u8; 64];
    for cy in 0..8 {
        for cx in 0..8 {
            let (x0, x1) = (cx * w / 8, ((cx + 1) * w / 8).max(cx * w / 8 + 1).min(w));
            let (y0, y1) = (cy * h / 8, ((cy + 1) * h / 8).max(cy * h / 8 + 1).min(h));
            // Every other pixel each way: plenty for a mean.
            let (mut sum, mut n) = (0u32, 0u32);
            for y in (y0..y1).step_by(2) {
                for x in (x0..x1).step_by(2) {
                    sum += img.y[y * w + x] as u32;
                    n += 1;
                }
            }
            out[cy * 8 + cx] = (sum / n.max(1)) as u8;
        }
    }
    Signature(out)
}

/// A finished picture.
#[derive(Debug, Clone)]
pub struct Thumb {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub sig: Signature,
}

/// The size `img` is shown at: inside [`THUMB_MAX_W`] x [`THUMB_MAX_H`], aspect kept, even.
pub fn thumb_size(w: u32, h: u32) -> (u32, u32) {
    if w <= THUMB_MAX_W && h <= THUMB_MAX_H {
        return (w.max(2) & !1, h.max(2) & !1);
    }
    let s = (THUMB_MAX_W as f64 / w as f64).min(THUMB_MAX_H as f64 / h as f64);
    (((w as f64 * s).round() as u32).max(2) & !1, ((h as f64 * s).round() as u32).max(2) & !1)
}

/// BT.709 video range YCbCr to RGB (what the capture's converter produces).
#[inline]
fn rgb709(y: f32, u: f32, v: f32) -> [u8; 3] {
    let c = 1.164_383_6 * (y - 16.0);
    let (d, e) = (u - 128.0, v - 128.0);
    let r = c + 1.792_741 * e;
    let g = c - 0.213_248_6 * d - 0.532_909_3 * e;
    let b = c + 2.112_402_3 * d;
    [r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8]
}

/// Downscale (box filter) and convert to packed RGB at [`thumb_size`].
pub fn to_rgb(img: &Nv12Image) -> (Vec<u8>, u32, u32) {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let (tw, th) = thumb_size(img.width, img.height);
    let (dw, dh) = (tw as usize, th as usize);
    let mut out = Vec::with_capacity(dw * dh * 3);
    for dy in 0..dh {
        let (y0, y1) = (dy * sh / dh, ((dy + 1) * sh / dh).max(dy * sh / dh + 1).min(sh));
        for dx in 0..dw {
            let (x0, x1) = (dx * sw / dw, ((dx + 1) * sw / dw).max(dx * sw / dw + 1).min(sw));
            let (mut ys, mut n) = (0u32, 0u32);
            for y in y0..y1 {
                let row = &img.y[y * sw + x0..y * sw + x1];
                ys += row.iter().map(|&p| p as u32).sum::<u32>();
                n += row.len() as u32;
            }
            // Chroma at half resolution: the block's chroma cells.
            let (cx0, cx1) = (x0 / 2, (x1.div_ceil(2)).max(x0 / 2 + 1).min(sw / 2));
            let (cy0, cy1) = (y0 / 2, (y1.div_ceil(2)).max(y0 / 2 + 1).min(sh / 2));
            let (mut us, mut vs, mut m) = (0u32, 0u32, 0u32);
            for cy in cy0..cy1 {
                for cx in cx0..cx1 {
                    let i = cy * sw + cx * 2;
                    us += img.uv[i] as u32;
                    vs += img.uv[i + 1] as u32;
                    m += 1;
                }
            }
            let m = m.max(1) as f32;
            out.extend_from_slice(&rgb709(ys as f32 / n.max(1) as f32, us as f32 / m, vs as f32 / m));
        }
    }
    (out, tw, th)
}

/// The JPEG of `img` and its signature.
pub fn encode(img: &Nv12Image) -> Option<Thumb> {
    let (rgb, w, h) = to_rgb(img);
    let jpeg = crate::images::encode_jpeg(&rgb, w, h, THUMB_QUALITY)?;
    Some(Thumb { jpeg, width: w, height: h, sig: signature(img) })
}

/// A PNG of an app icon at [`ICON_SIDE`] pixels (centred, never enlarged past its own size).
pub fn icon_png(icon: &vox_capture::RgbaImage) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let src = image::RgbaImage::from_raw(icon.width, icon.height, icon.data.clone())?;
    let img = if icon.width > ICON_SIDE || icon.height > ICON_SIDE { image::imageops::resize(&src, ICON_SIDE, ICON_SIDE, image::imageops::FilterType::Triangle) } else { src };
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8).ok()?;
    Some(out)
}

/// Run `f` with this thread at a low priority (Windows), so the encode never competes with the
/// capture, the encoder or a game. The priority is put back afterwards.
pub fn with_low_priority<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_LOWEST};
        // SAFETY: the pseudo handle of the current thread.
        let before = unsafe { GetThreadPriority(GetCurrentThread()) };
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
        let out = f();
        unsafe { SetThreadPriority(GetCurrentThread(), before) };
        out
    }
    #[cfg(not(windows))]
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1280 x 720 "desktop": a dark background, a lighter window, some text-like stripes.
    fn desktop(shift: usize) -> Nv12Image {
        let (w, h) = (1280usize, 720usize);
        let mut y = vec![40u8; w * h];
        for r in 100..600 {
            for c in 200..1100 {
                y[r * w + c] = if (r / 6 + c / 9 + shift) % 7 == 0 { 30 } else { 200 };
            }
        }
        let mut uv = vec![128u8; w * h / 2];
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                uv[r * w + c * 2] = 128 + ((c / 40) % 3) as u8 * 20;
                uv[r * w + c * 2 + 1] = 128 - ((r / 30) % 2) as u8 * 30;
            }
        }
        Nv12Image { width: w as u32, height: h as u32, y, uv }
    }

    #[test]
    fn copies_strided_frames() {
        let (w, h, stride) = (6u32, 4u32, 8u32);
        let y: Vec<u8> = (0..(stride * h) as u8).collect();
        let uv: Vec<u8> = (100..100 + (stride * h / 2) as u8).collect();
        let f = Nv12Frame { width: w, height: h, timestamp_us: 0, y: &y, y_stride: stride, uv: &uv, uv_stride: stride };
        let img = Nv12Image::copy_of(&f).unwrap();
        assert_eq!((img.width, img.height), (6, 4));
        assert_eq!(&img.y[..6], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(&img.y[6..12], &[8, 9, 10, 11, 12, 13]);
        assert_eq!(&img.uv[6..12], &[108, 109, 110, 111, 112, 113]);
        // Too short a buffer: refused, never read past the end.
        let f = Nv12Frame { width: 16, height: 16, timestamp_us: 0, y: &y, y_stride: 16, uv: &uv, uv_stride: 16 };
        assert!(Nv12Image::copy_of(&f).is_none());
    }

    #[test]
    fn sizes_fit_the_box() {
        assert_eq!(thumb_size(1920, 1080), (320, 180));
        assert_eq!(thumb_size(640, 360), (320, 180));
        assert_eq!(thumb_size(1080, 1920), (134, 240));
        assert_eq!(thumb_size(300, 200), (300, 200), "never enlarged");
        assert_eq!(thumb_size(3, 3), (2, 2));
    }

    #[test]
    fn colours_survive_the_conversion() {
        // Video-range white, black and a red.
        assert_eq!(rgb709(235.0, 128.0, 128.0), [255, 255, 255]);
        assert_eq!(rgb709(16.0, 128.0, 128.0), [0, 0, 0]);
        let red = rgb709(63.0, 102.0, 240.0);
        assert!(red[0] > 240 && red[1] < 20 && red[2] < 20, "{red:?}");
    }

    #[test]
    fn a_desktop_makes_a_small_jpeg_and_changes_are_seen() {
        let a = desktop(0);
        let t = encode(&a).unwrap();
        assert_eq!((t.width, t.height), (320, 180));
        assert!(t.jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(t.jpeg.len() < 32 * 1024, "{} bytes", t.jpeg.len());
        let back = image::load_from_memory(&t.jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (320, 180));
        // The same picture with a little capture noise: not a change.
        let mut noisy = a.clone();
        for (i, p) in noisy.y.iter_mut().enumerate() {
            *p = p.saturating_add((i % 3) as u8);
        }
        assert!(!signature(&a).differs(&signature(&noisy)));
        // A window moved / new content: a change.
        let mut moved = a.clone();
        for r in 100..600 {
            for c in 0..300 {
                moved.y[r * 1280 + c] = 230;
            }
        }
        assert!(signature(&a).differs(&signature(&moved)));
        // A fully different screen.
        let dark = Nv12Image { y: vec![16; a.y.len()], ..a.clone() };
        assert!(signature(&a).differs(&signature(&dark)));
    }

    /// RGB to BT.709 video-range NV12 (what the capture hands over), for measurements.
    fn nv12_of(img: &image::RgbImage) -> Nv12Image {
        let (w, h) = (img.width() & !1, img.height() & !1);
        let mut y = Vec::with_capacity((w * h) as usize);
        let mut uv = Vec::with_capacity((w * h / 2) as usize);
        let px = |x: u32, yy: u32| { let p = img.get_pixel(x, yy).0; (p[0] as f32, p[1] as f32, p[2] as f32) };
        for r in 0..h {
            for c in 0..w {
                let (rr, g, b) = px(c, r);
                y.push((16.0 + 0.183 * rr + 0.614 * g + 0.062 * b).round() as u8);
            }
        }
        for r in (0..h).step_by(2) {
            for c in (0..w).step_by(2) {
                let (rr, g, b) = px(c, r);
                uv.push((128.0 - 0.101 * rr - 0.339 * g + 0.439 * b).round() as u8);
                uv.push((128.0 + 0.439 * rr - 0.399 * g - 0.040 * b).round() as u8);
            }
        }
        Nv12Image { width: w, height: h, y, uv }
    }

    /// Sizes and encode times of real pictures (`VOX_THUMB_SAMPLES`: paths separated by `;`), as
    /// the capture would hand them over at 640 x 360 (the tile's preview) or full size. Run by
    /// hand: `cargo test -p vox-client-core thumbnail -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_sample_pictures() {
        let list = std::env::var("VOX_THUMB_SAMPLES").unwrap_or_default();
        // Worst case first: fine detail everywhere (foliage / a busy game scene).
        let busy = image::RgbImage::from_fn(1920, 1080, |x, y| {
            let n = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).rotate_left(13).wrapping_mul(2_246_822_519) >> 24;
            image::Rgb([((x / 7) as u8).wrapping_add(n as u8 / 3), ((y / 5) as u8).wrapping_add(n as u8 / 4), (n as u8) / 2 + 60])
        });
        let busy_path = std::env::temp_dir().join("vox-thumb-busy.png");
        busy.save(&busy_path).unwrap();
        let busy_path = busy_path.to_string_lossy().to_string();
        for path in std::iter::once(busy_path.as_str()).chain(list.split(';').filter(|p| !p.is_empty())) {
            let img = image::open(path).unwrap().to_rgb8();
            for (label, src) in [("full", img.clone()), ("640", image::imageops::resize(&img, 640, 640 * img.height() / img.width(), image::imageops::FilterType::Triangle))] {
                let nv = nv12_of(&src);
                let t = std::time::Instant::now();
                let th = encode(&nv).unwrap();
                println!("{path} [{label} {}x{}]: {}x{} jpeg {} bytes in {} us", nv.width, nv.height, th.width, th.height, th.jpeg.len(), t.elapsed().as_micros());
            }
        }
    }

    #[test]
    fn icons_become_small_pngs() {
        let icon = vox_capture::RgbaImage { width: 48, height: 48, data: vec![200; 48 * 48 * 4] };
        let png = icon_png(&icon).unwrap();
        let back = image::load_from_memory(&png).unwrap();
        assert_eq!((back.width(), back.height()), (ICON_SIDE, ICON_SIDE));
        assert!(png.len() < vox_proto::STREAM_PREVIEW_MAX_ICON);
        let small = vox_capture::RgbaImage { width: 16, height: 16, data: vec![9; 16 * 16 * 4] };
        assert_eq!(image::load_from_memory(&icon_png(&small).unwrap()).unwrap().width(), 16);
        assert_eq!(with_low_priority(|| 41 + 1), 42);
    }
}
