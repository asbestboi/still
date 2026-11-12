//! The image pipeline. Everything is resolution-independent: spatial
//! parameters are expressed in "units", where one unit is 1/1000 of the
//! image's long edge. That way the small preview and the full-size export
//! look the same.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub type Px = [f32; 3];

#[derive(Clone)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub px: Vec<Px>,
}

impl Img {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![[0.0; 3]; w * h] }
    }

    pub fn from_rgb32f(img: &image::Rgb32FImage) -> Self {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let px = img.as_raw().chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
        Self { w, h, px }
    }

    fn unit(&self) -> f32 {
        self.w.max(self.h) as f32 / 1000.0
    }

    #[inline]
    fn get(&self, x: isize, y: isize) -> Px {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        self.px[y * self.w + x]
    }

    /// Bilinear sample, clamped at the edges.
    #[inline]
    pub(crate) fn sample(&self, x: f32, y: f32) -> Px {
        let x = x - 0.5;
        let y = y - 0.5;
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (x0, y0) = (x0 as isize, y0 as isize);
        let a = self.get(x0, y0);
        let b = self.get(x0 + 1, y0);
        let c = self.get(x0, y0 + 1);
        let d = self.get(x0 + 1, y0 + 1);
        let mut o = [0.0; 3];
        for i in 0..3 {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bot = c[i] + (d[i] - c[i]) * fx;
            o[i] = top + (bot - top) * fy;
        }
        o
    }

    fn transpose(&self) -> Img {
        let (w, h) = (self.w, self.h);
        let mut out = Img::new(h, w);
        out.px.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
            for (y, p) in col.iter_mut().enumerate() {
                *p = self.px[y * w + x];
            }
        });
        out
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Crop aspect constraint. Variant names are kept stable for saved settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Aspect {
    #[default]
    Free,
    Original,
    Square,
    Portrait45,
    Classic32,
    Four3,
    Wide169,
    Cinema239,
    XPan,
}

impl Aspect {
    pub const ALL: [Aspect; 9] = [
        Aspect::Free,
        Aspect::Original,
        Aspect::Square,
        Aspect::Portrait45,
        Aspect::Classic32,
        Aspect::Four3,
        Aspect::Wide169,
        Aspect::Cinema239,
        Aspect::XPan,
    ];
    /// Whether the portrait / landscape switch means anything for this ratio.
    pub fn orientable(self) -> bool {
        !matches!(self, Aspect::Free | Aspect::Original | Aspect::Square)
    }
    pub fn label(self, portrait: bool) -> &'static str {
        match (self, portrait) {
            (Aspect::Free, _) => "Free",
            (Aspect::Original, _) => "Original",
            (Aspect::Square, _) => "Square",
            (Aspect::Portrait45, false) => "5:4",
            (Aspect::Portrait45, true) => "4:5",
            (Aspect::Classic32, false) => "3:2",
            (Aspect::Classic32, true) => "2:3",
            (Aspect::Four3, false) => "4:3",
            (Aspect::Four3, true) => "3:4",
            (Aspect::Wide169, false) => "16:9",
            (Aspect::Wide169, true) => "9:16",
            (Aspect::Cinema239, false) => "2.39:1",
            (Aspect::Cinema239, true) => "1:2.39",
            (Aspect::XPan, false) => "XPan",
            (Aspect::XPan, true) => "XPan ↕",
        }
    }
    /// Width / height in pixels for a frame of `w × h`; `None` means unconstrained.
    pub fn ratio(self, w: f32, h: f32, portrait: bool) -> Option<f32> {
        let r = match self {
            Aspect::Free => return None,
            Aspect::Original => return Some(w / h),
            Aspect::Square => return Some(1.0),
            Aspect::Portrait45 => 5.0 / 4.0,
            Aspect::Classic32 => 1.5,
            Aspect::Four3 => 4.0 / 3.0,
            Aspect::Wide169 => 16.0 / 9.0,
            Aspect::Cinema239 => 2.39,
            Aspect::XPan => 65.0 / 24.0,
        };
        Some(if portrait { 1.0 / r } else { r })
    }
}

/// The largest rect of pixel ratio `r` (w/h) centred on `c` that fits the unit frame.
pub fn fit_ratio(c: [f32; 4], r: f32, w: f32, h: f32) -> [f32; 4] {
    let (cx, cy) = ((c[0] + c[2]) * 0.5, (c[1] + c[3]) * 0.5);
    // in frame pixels
    let (mut cw, mut ch) = if w / h > r { (h * r, h) } else { (w, w / r) };
    // keep it no larger than needed to cover the current crop's area
    let cur = ((c[2] - c[0]) * w * (c[3] - c[1]) * h).max(1.0);
    let s = (cur / (cw * ch)).sqrt().min(1.0).max(0.35);
    cw *= s;
    ch *= s;
    let (nw, nh) = (cw / w, ch / h);
    let x0 = (cx - nw * 0.5).clamp(0.0, 1.0 - nw);
    let y0 = (cy - nh * 0.5).clamp(0.0, 1.0 - nh);
    [x0, y0, x0 + nw, y0 + nh]
}

pub const FULL_CROP: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// Pixel rect (x, y, w, h) for a normalised crop on a `w × h` frame.
pub fn crop_px(w: usize, h: usize, c: [f32; 4]) -> (usize, usize, usize, usize) {
    let x0 = ((c[0].clamp(0.0, 1.0) * w as f32).round() as usize).min(w - 1);
    let y0 = ((c[1].clamp(0.0, 1.0) * h as f32).round() as usize).min(h - 1);
    let x1 = ((c[2].clamp(0.0, 1.0) * w as f32).round() as usize).clamp(x0 + 1, w);
    let y1 = ((c[3].clamp(0.0, 1.0) * h as f32).round() as usize).clamp(y0 + 1, h);
    (x0, y0, x1 - x0, y1 - y0)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    // Geometry
    pub rotate: u8,
    pub flip_h: bool,
    pub straighten: f32,
    pub aspect: Aspect,
    pub aspect_portrait: bool,
    /// Normalised crop rect (x0, y0, x1, y1) on the rotated, straightened frame.
    pub crop: [f32; 4],

    // Light
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub fade: f32,

    // Color
    pub temperature: f32,
    pub tint: f32,
    pub saturation: f32,
    pub vibrance: f32,
    pub mono: bool,
    pub shadow_hue: f32,
    pub shadow_tone: f32,
    pub highlight_hue: f32,
    pub highlight_tone: f32,

    // Optics
    pub bloom: f32,
    pub bloom_radius: f32,
    pub bloom_threshold: f32,
    pub halation: f32,
    pub aberration: f32,
    pub softness: f32,
    pub sharpen: f32,
    pub vignette: f32,
    pub vignette_feather: f32,

    // Texture
    pub grain: f32,
    pub grain_size: f32,
    pub grain_color: f32,
    pub sensor_noise: f32,
    pub scanlines: f32,
    pub scanline_spacing: f32,
    pub posterize: f32,
    pub dither: bool,
    pub lowres: f32,
    pub lowres_smooth: bool,

    // Glitch
    pub sort_amount: f32,
    pub sort_low: f32,
    pub sort_high: f32,
    pub sort_vertical: bool,
    pub sort_reverse: bool,
    pub streaks: f32,
    pub streak_width: f32,
    pub slices: f32,
    pub slice_shift: f32,
    pub rgb_split: f32,
    pub rgb_angle: f32,
    pub blocks: f32,
    pub block_size: f32,

    // Frame
    pub border: f32,
    pub border_white: bool,

    pub seed: u32,

    /// Local adjustments, applied in order.
    pub masks: Vec<crate::masks::Mask>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            rotate: 0,
            flip_h: false,
            straighten: 0.0,
            aspect: Aspect::Free,
            aspect_portrait: false,
            crop: FULL_CROP,
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            fade: 0.0,
            temperature: 0.0,
            tint: 0.0,
            saturation: 0.0,
            vibrance: 0.0,
            mono: false,
            shadow_hue: 200.0,
            shadow_tone: 0.0,
            highlight_hue: 35.0,
            highlight_tone: 0.0,
            bloom: 0.0,
            bloom_radius: 0.4,
            bloom_threshold: 0.5,
            halation: 0.0,
            aberration: 0.0,
            softness: 0.0,
            sharpen: 0.0,
            vignette: 0.0,
            vignette_feather: 0.6,
            grain: 0.0,
            grain_size: 0.35,
            grain_color: 0.0,
            sensor_noise: 0.0,
            scanlines: 0.0,
            scanline_spacing: 0.3,
            posterize: 0.0,
            dither: true,
            lowres: 0.0,
            lowres_smooth: false,
            sort_amount: 0.0,
            sort_low: 0.45,
            sort_high: 1.0,
            sort_vertical: true,
            sort_reverse: false,
            streaks: 0.0,
            streak_width: 0.3,
            slices: 0.0,
            slice_shift: 0.3,
            rgb_split: 0.0,
            rgb_angle: 0.0,
            blocks: 0.0,
            block_size: 0.3,
            border: 0.0,
            border_white: true,
            seed: 1,
            masks: Vec::new(),
        }
    }
}

impl Params {
    /// Copy only the "look" from another set of params, keeping geometry & frame.
    pub fn with_look_from(&self, look: &Params) -> Params {
        let mut p = look.clone();
        p.rotate = self.rotate;
        p.flip_h = self.flip_h;
        p.straighten = self.straighten;
        p.aspect = self.aspect;
        p.aspect_portrait = self.aspect_portrait;
        p.crop = self.crop;
        p.border = self.border;
        p.border_white = self.border_white;
        p.masks = self.masks.clone();
        p
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    h
}

#[inline]
fn hashf(x: u32, y: u32, seed: u32) -> f32 {
    (hash(x, y, seed) >> 8) as f32 / (1u32 << 24) as f32
}

/// Small deterministic RNG for scattering glitch elements.
struct Rng(u64);
impl Rng {
    fn new(seed: u32, salt: u32) -> Self {
        Rng((seed as u64) << 32 | salt as u64 | 1)
    }
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next()
    }
}

#[inline]
pub(crate) fn luma(p: Px) -> f32 {
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

#[inline]
pub(crate) fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
pub(crate) fn to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

#[inline]
pub(crate) fn to_srgb(c: f32) -> f32 {
    let c = c.max(0.0);
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

pub(crate) fn hue_rgb(hue_deg: f32) -> Px {
    let h = (hue_deg.rem_euclid(360.0)) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    match h as u32 {
        0 => [1.0, x, 0.0],
        1 => [x, 1.0, 0.0],
        2 => [0.0, 1.0, x],
        3 => [0.0, x, 1.0],
        4 => [x, 0.0, 1.0],
        _ => [1.0, 0.0, x],
    }
}

/// One horizontal box-blur pass with a running sum.
fn box_blur_rows(img: &mut Img, r: usize) {
    if r == 0 {
        return;
    }
    let w = img.w;
    let norm = 1.0 / (2 * r + 1) as f32;
    img.px.par_chunks_mut(w).for_each(|row| {
        let src: Vec<Px> = row.to_vec();
        let at = |i: isize| src[i.clamp(0, w as isize - 1) as usize];
        let mut acc = [0.0f32; 3];
        for i in -(r as isize)..=(r as isize) {
            let p = at(i);
            for c in 0..3 {
                acc[c] += p[c];
            }
        }
        for x in 0..w {
            for c in 0..3 {
                row[x][c] = acc[c] * norm;
            }
            let add = at(x as isize + r as isize + 1);
            let sub = at(x as isize - r as isize);
            for c in 0..3 {
                acc[c] += add[c] - sub[c];
            }
        }
    });
}

/// Approximate gaussian blur (3 box passes in each direction).
pub fn blur(img: &Img, sigma: f32) -> Img {
    if sigma < 0.3 {
        return img.clone();
    }
    // Box radius for 3 passes approximating the given sigma.
    let r = ((sigma * sigma + 1.0).sqrt() * 0.95).round().max(1.0) as usize;
    // Very large radii: blur a downscaled copy and upsample (cheap and smooth).
    let factor = (r / 24).max(1);
    let mut work = if factor > 1 { downscale(img, factor) } else { img.clone() };
    let rr = (r / factor).max(1);
    for _ in 0..3 {
        box_blur_rows(&mut work, rr);
    }
    let mut t = work.transpose();
    for _ in 0..3 {
        box_blur_rows(&mut t, rr);
    }
    let work = t.transpose();
    if factor > 1 { upscale_to(&work, img.w, img.h) } else { work }
}

fn downscale(img: &Img, f: usize) -> Img {
    let w = (img.w / f).max(1);
    let h = (img.h / f).max(1);
    let mut out = Img::new(w, h);
    out.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, p) in row.iter_mut().enumerate() {
            let mut acc = [0.0; 3];
            let mut n = 0.0;
            for yy in y * f..((y + 1) * f).min(img.h) {
                for xx in x * f..((x + 1) * f).min(img.w) {
                    let s = img.px[yy * img.w + xx];
                    for c in 0..3 {
                        acc[c] += s[c];
                    }
                    n += 1.0;
                }
            }
            *p = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    });
    out
}

fn upscale_to(img: &Img, w: usize, h: usize) -> Img {
    let sx = img.w as f32 / w as f32;
    let sy = img.h as f32 / h as f32;
    let mut out = Img::new(w, h);
    out.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, p) in row.iter_mut().enumerate() {
            *p = img.sample((x as f32 + 0.5) * sx, (y as f32 + 0.5) * sy);
        }
    });
    out
}

/// Luminance at quantile `q` (0..1), from a sparse sample of the image.
fn percentile_luma(img: &Img, q: f32) -> f32 {
    let step = (img.px.len() / 40_000).max(1);
    let mut l: Vec<f32> = img.px.iter().step_by(step).map(|p| luma(*p)).collect();
    if l.is_empty() {
        return 1.0;
    }
    let k = ((l.len() - 1) as f32 * q) as usize;
    *l.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Rotate, flip, straighten and crop; also returns where the crop sits in the
/// frame so masks can be mapped onto the result.
fn geometry(src: &Img, p: &Params) -> (Img, crate::masks::Space) {
    let mut img = rotate90(src, p.rotate % 4);
    if p.flip_h {
        img.px.par_chunks_mut(img.w).for_each(|row| row.reverse());
    }
    if p.straighten.abs() > 0.01 {
        img = straighten(&img, p.straighten.to_radians());
    }
    let (ox, oy, _, _) = crop_px(img.w, img.h, p.crop);
    let space = crate::masks::Space { fw: img.w as f32, fh: img.h as f32, ox: ox as f32, oy: oy as f32 };
    (crop(img, p.crop), space)
}

fn rotate90(src: &Img, turns: u8) -> Img {
    match turns {
        0 => src.clone(),
        2 => {
            let mut o = src.clone();
            o.px.reverse();
            o
        }
        _ => {
            // transpose then flip horizontally (cw) or vertically (ccw)
            let mut t = src.transpose();
            if turns == 1 {
                t.px.par_chunks_mut(t.w).for_each(|row| row.reverse());
            } else {
                let w = t.w;
                let rows: Vec<Vec<Px>> = t.px.chunks(w).rev().map(|r| r.to_vec()).collect();
                t.px = rows.concat();
            }
            t
        }
    }
}

fn straighten(img: &Img, a: f32) -> Img {
    let (w, h) = (img.w as f32, img.h as f32);
    let (s, c) = (a.abs().sin(), a.abs().cos());
    // zoom needed so the rotated image still covers the whole frame
    let zoom = ((w * c + h * s) / w).max((w * s + h * c) / h);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (sa, ca) = ((-a).sin(), (-a).cos());
    let mut out = Img::new(img.w, img.h);
    out.px.par_chunks_mut(img.w).enumerate().for_each(|(y, row)| {
        for (x, p) in row.iter_mut().enumerate() {
            let dx = (x as f32 + 0.5 - cx) / zoom;
            let dy = (y as f32 + 0.5 - cy) / zoom;
            let sx = cx + dx * ca - dy * sa;
            let sy = cy + dx * sa + dy * ca;
            *p = img.sample(sx, sy);
        }
    });
    out
}

fn crop(img: Img, c: [f32; 4]) -> Img {
    if c == FULL_CROP {
        return img;
    }
    let (ox, oy, cw, ch) = crop_px(img.w, img.h, c);
    let mut out = Img::new(cw, ch);
    out.px.par_chunks_mut(cw).enumerate().for_each(|(y, row)| {
        let s = (y + oy) * img.w + ox;
        row.copy_from_slice(&img.px[s..s + cw]);
    });
    out
}

/// Render at a lower resolution and blow it back up: blocky (nearest) or soft (bilinear).
/// The target size is relative to the long edge, so preview and export match.
fn low_res(img: &Img, p: &Params) -> Img {
    let long = img.w.max(img.h) as f32;
    let target = (1600f32.ln() * (1.0 - p.lowres) + 24f32.ln() * p.lowres).exp();
    let f = (long / target).max(1.0);
    if f < 1.05 {
        return img.clone();
    }
    let tw = ((img.w as f32 / f).round() as usize).max(1);
    let th = ((img.h as f32 / f).round() as usize).max(1);
    // area average into the small image
    let mut small = Img::new(tw, th);
    small.px.par_chunks_mut(tw).enumerate().for_each(|(y, row)| {
        let y0 = y * img.h / th;
        let y1 = ((y + 1) * img.h / th).max(y0 + 1);
        for (x, o) in row.iter_mut().enumerate() {
            let x0 = x * img.w / tw;
            let x1 = ((x + 1) * img.w / tw).max(x0 + 1);
            let mut acc = [0.0; 3];
            for yy in y0..y1 {
                for s in &img.px[yy * img.w + x0..yy * img.w + x1] {
                    for c in 0..3 {
                        acc[c] += s[c];
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            *o = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    });
    if p.lowres_smooth {
        return upscale_to(&small, img.w, img.h);
    }
    let mut out = Img::new(img.w, img.h);
    out.px.par_chunks_mut(img.w).enumerate().for_each(|(y, row)| {
        let sy = (y * th / img.h).min(th - 1);
        for (x, o) in row.iter_mut().enumerate() {
            *o = small.px[sy * tw + (x * tw / img.w).min(tw - 1)];
        }
    });
    out
}

// ---------------------------------------------------------------------------
// The full pipeline
// ---------------------------------------------------------------------------

pub fn process(src: &Img, p: &Params) -> Img {
    process_ext(src, p, None).0
}

/// Full render; also returns the opacity of mask `overlay` for on-screen display.
pub fn process_ext(src: &Img, p: &Params, overlay: Option<usize>) -> (Img, Option<Vec<f32>>) {
    let (mut img, space) = geometry(src, p);
    let u = img.unit();

    // --- Linear-light stage: exposure, white balance, bloom, halation -------
    let gain = 2f32.powf(p.exposure);
    let wb = [
        (1.0 + p.temperature * 0.30) * (1.0 + p.tint * 0.10),
        1.0 - p.tint * 0.18,
        (1.0 - p.temperature * 0.30) * (1.0 + p.tint * 0.10),
    ];
    img.px.par_iter_mut().for_each(|px| {
        for c in 0..3 {
            px[c] = to_linear(px[c]) * gain * wb[c];
        }
    });

    if p.bloom > 0.001 || p.halation > 0.001 {
        // The glow comes from the photo's own highlights: the threshold is relative
        // to its bright end, so a dull daylight frame or a flat RAW still blooms.
        let hi = percentile_luma(&img, 0.97).max(0.02);
        let t = hi * (0.25 + p.bloom_threshold * 0.85);
        let knee = (hi - t).max(hi * 0.25);
        let norm = 1.0 / hi.max(0.25);
        let mut bright = img.clone();
        bright.px.par_iter_mut().for_each(|px| {
            let m = smoothstep(t, t + knee, luma(*px)) * norm;
            for c in 0..3 {
                px[c] *= m;
            }
        });
        let bloom = (p.bloom > 0.001).then(|| blur(&bright, (8.0 + p.bloom_radius * 60.0) * u));
        let hal = (p.halation > 0.001).then(|| blur(&bright, 9.0 * u));
        let tint = [1.0, 0.22, 0.06];
        img.px.par_iter_mut().enumerate().for_each(|(i, px)| {
            if let Some(b) = &bloom {
                for c in 0..3 {
                    px[c] += b.px[i][c] * p.bloom * 0.9;
                }
            }
            if let Some(h) = &hal {
                // only the light that bleeds *past* the highlight edge
                let l = (luma(h.px[i]) - luma(bright.px[i]) * 0.6).max(0.0);
                for c in 0..3 {
                    px[c] += l * tint[c] * p.halation * 3.0;
                }
            }
        });
    }

    // --- Display-referred stage: tone & colour ----------------------------
    let bp = -p.blacks * 0.08;
    let wp = 1.0 - p.whites * 0.2;
    let k = (p.contrast * 1.1).exp();
    let fade_lo = p.fade * 0.22;
    let fade_hi = 1.0 - p.fade * 0.06;
    let sh_col = hue_rgb(p.shadow_hue);
    let hi_col = hue_rgb(p.highlight_hue);
    let sh_l = luma(sh_col);
    let hi_l = luma(hi_col);

    img.px.par_iter_mut().for_each(|px| {
        let mut c = [to_srgb(px[0]), to_srgb(px[1]), to_srgb(px[2])];

        // black / white points
        if bp != 0.0 || wp != 1.0 {
            for v in &mut c {
                *v = (*v - bp) / (wp - bp);
            }
        }

        // highlights / shadows (luminance-preserving hue)
        if p.shadows != 0.0 || p.highlights != 0.0 {
            let l = luma(c).clamp(0.0, 1.5);
            let s_mask = (1.0 - l).max(0.0).powi(2) * l.min(1.0) * 4.0;
            let h_mask = l.min(1.0).powi(2) * (1.0 - l.min(1.0)) * 4.0 + (l - 1.0).max(0.0);
            let nl = l + p.shadows * 0.3 * s_mask + p.highlights * 0.35 * h_mask;
            if l > 1e-4 {
                let r = (nl / l).max(0.0);
                for v in &mut c {
                    *v *= r;
                }
            } else {
                for v in &mut c {
                    *v += nl.max(0.0);
                }
            }
        }

        // contrast: symmetric power curve around mid grey
        if p.contrast != 0.0 {
            for v in &mut c {
                let x = v.clamp(0.0, 1.0);
                *v = if x < 0.5 { 0.5 * (2.0 * x).powf(k) } else { 1.0 - 0.5 * (2.0 * (1.0 - x)).powf(k) };
            }
        }

        // saturation & vibrance
        let l = luma(c);
        if p.saturation != 0.0 || p.vibrance != 0.0 {
            let mx = c[0].max(c[1]).max(c[2]);
            let mn = c[0].min(c[1]).min(c[2]);
            let sat = (mx - mn).clamp(0.0, 1.0);
            let f = (1.0 + p.saturation) * (1.0 + p.vibrance * (1.0 - sat).powi(2));
            for v in &mut c {
                *v = l + (*v - l) * f.max(0.0);
            }
        }
        if p.mono {
            c = [l, l, l];
        }

        // split toning
        if p.shadow_tone > 0.0 || p.highlight_tone > 0.0 {
            let l = luma(c).clamp(0.0, 1.0);
            let ws = (1.0 - l).powi(2) * p.shadow_tone * 0.35;
            let wh = l.powi(2) * p.highlight_tone * 0.35;
            for i in 0..3 {
                c[i] += (sh_col[i] - sh_l) * ws + (hi_col[i] - hi_l) * wh;
            }
        }

        // fade / matte
        if p.fade != 0.0 {
            for v in &mut c {
                *v = fade_lo + v.clamp(0.0, 1.0) * (fade_hi - fade_lo);
            }
        }

        for v in &mut c {
            *v = v.clamp(0.0, 1.0);
        }
        *px = c;
    });

    // --- Local adjustments -------------------------------------------------
    let mask_alpha = if p.masks.is_empty() { None } else { crate::masks::apply(&mut img, &p.masks, &space, u, overlay) };

    // --- Detail ------------------------------------------------------------
    if p.softness > 0.001 {
        let b = blur(&img, p.softness * 6.0 * u);
        let a = p.softness.min(1.0);
        img.px.par_iter_mut().zip(b.px.par_iter()).for_each(|(px, b)| {
            for c in 0..3 {
                px[c] += (b[c] - px[c]) * a;
            }
        });
    }
    if p.sharpen > 0.001 {
        let b = blur(&img, (0.7 * u).max(0.8));
        img.px.par_iter_mut().zip(b.px.par_iter()).for_each(|(px, b)| {
            for c in 0..3 {
                px[c] = (px[c] + (px[c] - b[c]) * p.sharpen * 2.0).clamp(0.0, 1.0);
            }
        });
    }

    // --- Resolution --------------------------------------------------------
    if p.lowres > 0.001 {
        img = low_res(&img, p);
    }

    // --- Glitch ------------------------------------------------------------
    if p.blocks > 0.001 {
        blocks(&mut img, p, u);
    }
    if p.sort_amount > 0.001 {
        pixel_sort(&mut img, p);
    }
    if p.slices > 0.001 {
        slices(&mut img, p, u);
    }
    if p.streaks > 0.001 {
        streaks(&mut img, p, u);
    }
    if p.rgb_split > 0.001 || p.aberration > 0.001 {
        img = channel_shift(&img, p, u);
    }

    // --- Texture -----------------------------------------------------------
    texture(&mut img, p, u);

    // --- Vignette ----------------------------------------------------------
    if p.vignette.abs() > 0.001 {
        vignette(&mut img, p);
    }

    // --- Frame -------------------------------------------------------------
    if p.border > 0.001 {
        img = border(&img, p);
    }
    (img, mask_alpha)
}

fn blocks(img: &mut Img, p: &Params, u: f32) {
    let bs = ((4.0 + p.block_size * 60.0) * u).round().max(2.0) as usize;
    let (w, h) = (img.w, img.h);
    let bw = w.div_ceil(bs);
    let bh = h.div_ceil(bs);
    // choose blocks, clustered in horizontal runs like real stream corruption
    let mut chosen = vec![false; bw * bh];
    let mut rng = Rng::new(p.seed, 77);
    let runs = (p.blocks * p.blocks * (bw * bh) as f32 * 0.08).ceil() as usize;
    for _ in 0..runs {
        let by = (rng.next() * bh as f32) as usize;
        let bx = (rng.next() * bw as f32) as usize;
        let len = 1 + (rng.next() * rng.next() * bw as f32 * 0.5) as usize;
        for i in 0..len {
            if bx + i < bw {
                chosen[by.min(bh - 1) * bw + bx + i] = true;
            }
        }
    }
    let src = img.clone();
    img.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let by = y / bs;
        for bx in 0..bw {
            if !chosen[by * bw + bx] {
                continue;
            }
            let (x0, y0) = (bx * bs, by * bs);
            let x1 = (x0 + bs).min(w);
            let y1 = (y0 + bs).min(h);
            // average of the block (computed per row: cheap enough, keeps it parallel)
            let mut acc = [0.0; 3];
            let mut n = 0.0;
            for yy in (y0..y1).step_by(2) {
                for xx in (x0..x1).step_by(2) {
                    let s = src.px[yy * w + xx];
                    for c in 0..3 {
                        acc[c] += s[c];
                    }
                    n += 1.0;
                }
            }
            let avg = [acc[0] / n, acc[1] / n, acc[2] / n];
            // a hint of the low-frequency gradient so it reads as compression
            let ty = (y - y0) as f32 / bs as f32 - 0.5;
            for x in x0..x1 {
                let tx = (x - x0) as f32 / bs as f32 - 0.5;
                let s = src.px[y * w + x];
                for c in 0..3 {
                    let q = avg[c] + (s[c] - avg[c]) * 0.15 + (tx + ty) * 0.02;
                    row[x][c] = (q * 12.0).round() / 12.0;
                }
            }
        }
    });
}

fn pixel_sort(img: &mut Img, p: &Params) {
    let mut work = if p.sort_vertical { img.transpose() } else { img.clone() };
    let (w, lo, hi) = (work.w, p.sort_low.min(p.sort_high), p.sort_high.max(p.sort_low));
    let density = p.sort_amount;
    let seed = p.seed;
    let rev = p.sort_reverse;
    work.px.par_chunks_mut(w).enumerate().for_each(|(line, row)| {
        // lines are picked in small coherent bands so the result stays calm
        let band = (line as u32) / 3;
        if hashf(band, 11, seed) > density {
            return;
        }
        let mut x = 0;
        while x < w {
            let l = luma(row[x]);
            if l >= lo && l <= hi {
                let start = x;
                while x < w && {
                    let l = luma(row[x]);
                    l >= lo && l <= hi
                } {
                    x += 1;
                }
                let span = &mut row[start..x];
                span.sort_by(|a, b| luma(*a).total_cmp(&luma(*b)));
                if rev {
                    span.reverse();
                }
            } else {
                x += 1;
            }
        }
    });
    *img = if p.sort_vertical { work.transpose() } else { work };
}

fn slices(img: &mut Img, p: &Params, u: f32) {
    let n = (p.slices * 14.0).ceil() as usize;
    let mut rng = Rng::new(p.seed, 31);
    let w = img.w;
    for _ in 0..n {
        let y0 = (rng.next() * img.h as f32) as usize;
        let hgt = ((rng.range(2.0, 40.0) * rng.next()) * u).max(1.0) as usize;
        let off = (rng.range(-1.0, 1.0) * p.slice_shift * w as f32 * 0.12) as isize;
        for y in y0..(y0 + hgt).min(img.h) {
            let row = &mut img.px[y * w..(y + 1) * w];
            let off = off.rem_euclid(w as isize) as usize;
            row.rotate_right(off);
        }
    }
}

fn streaks(img: &mut Img, p: &Params, u: f32) {
    let n = (p.streaks * 6.0).ceil() as usize;
    let mut rng = Rng::new(p.seed, 5);
    let (w, h) = (img.w, img.h);
    for s in 0..n {
        let bw = ((2.0 + p.streak_width * 30.0) * u * rng.range(0.5, 1.3)).max(1.0) as usize;
        let x0 = (rng.next() * (w.saturating_sub(bw)) as f32) as usize;
        let y_start = (rng.next() * rng.next() * h as f32 * 0.5) as usize;
        let y_end = (y_start + (rng.range(0.45, 1.0) * h as f32) as usize).min(h);
        // segments: each repeats one source row, with a slight colour cast
        let mut y = y_start;
        let mut seg = 0u32;
        while y < y_end {
            let len = ((rng.range(4.0, 60.0) * rng.next()) * u).max(1.0) as usize;
            // smear a row from elsewhere in the frame, so displaced content shows through
            let src_row = if rng.next() < 0.7 { (rng.next() * h as f32) as usize % h } else { y };
            // mostly faithful smears; an occasional segment drifts red or green
            let cast = if hashf(seg, s as u32 + 9, p.seed) < 0.18 {
                if rng.next() < 0.5 { [1.25, 0.9, 0.9] } else { [0.9, 1.15, 0.92] }
            } else {
                [1.0, 1.0, 1.0]
            };
            let dark = hashf(seg, s as u32, p.seed) < 0.06;
            for yy in y..(y + len).min(y_end) {
                for x in x0..(x0 + bw).min(w) {
                    // per-column jitter of the source column, like a broken readout
                    let jx = if hashf(x as u32, seg, p.seed) < 0.2 { x.saturating_sub(1) } else { x };
                    let mut c = img.px[src_row * w + jx];
                    for i in 0..3 {
                        c[i] = (c[i] * cast[i]).clamp(0.0, 1.0);
                        if dark {
                            c[i] *= 0.25;
                        }
                    }
                    img.px[yy * w + x] = c;
                }
            }
            y += len;
            seg += 1;
        }
    }
}

fn channel_shift(img: &Img, p: &Params, u: f32) -> Img {
    let a = p.rgb_angle.to_radians();
    let d = p.rgb_split * 12.0 * u;
    let (dx, dy) = (a.cos() * d, a.sin() * d);
    let k = p.aberration * 0.016;
    let (cx, cy) = (img.w as f32 / 2.0, img.h as f32 / 2.0);
    let mut out = Img::new(img.w, img.h);
    out.px.par_chunks_mut(img.w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (rx, ry) = (cx + (fx - cx) * (1.0 + k) + dx, cy + (fy - cy) * (1.0 + k) + dy);
            let (bx, by) = (cx + (fx - cx) * (1.0 - k) - dx, cy + (fy - cy) * (1.0 - k) - dy);
            let g = img.px[y * img.w + x][1];
            *o = [img.sample(rx, ry)[0], g, img.sample(bx, by)[2]];
        }
    });
    out
}

const BAYER4: [f32; 16] = [0., 8., 2., 10., 12., 4., 14., 6., 3., 11., 1., 9., 15., 7., 13., 5.];

fn texture(img: &mut Img, p: &Params, u: f32) {
    let do_post = p.posterize > 0.001;
    let do_scan = p.scanlines > 0.001;
    let do_noise = p.sensor_noise > 0.001;
    let do_grain = p.grain > 0.001;
    if !(do_post || do_scan || do_noise || do_grain) {
        return;
    }
    let levels = 2.0 + (1.0 - p.posterize).powi(2) * 30.0;
    let period = ((1.5 + p.scanline_spacing * 8.0) * u).max(2.0);
    let gsize = ((0.4 + p.grain_size * 3.0) * u).max(0.6);
    let nsize = (0.9 * u).max(1.0);
    let seed = p.seed;
    let w = img.w;

    // value noise sampled on a lattice of `size` pixels
    let vnoise = move |x: f32, y: f32, size: f32, salt: u32| -> f32 {
        let (gx, gy) = (x / size, y / size);
        let (x0, y0) = (gx.floor(), gy.floor());
        let (fx, fy) = (gx - x0, gy - y0);
        let (fx, fy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let (ix, iy) = (x0 as i32 as u32, y0 as i32 as u32);
        let n = |a: u32, b: u32| {
            // sum of two uniforms -> softer, more film-like distribution
            hashf(a, b, seed ^ salt) + hashf(a, b, seed.wrapping_add(salt).wrapping_mul(3)) - 1.0
        };
        let a = n(ix, iy);
        let b = n(ix + 1, iy);
        let c = n(ix, iy + 1);
        let d = n(ix + 1, iy + 1);
        let t = a + (b - a) * fx;
        let bt = c + (d - c) * fx;
        t + (bt - t) * fy
    };

    img.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let fy = y as f32;
        let scan = if do_scan {
            1.0 - p.scanlines * 0.45 * (0.5 + 0.5 * (std::f32::consts::TAU * fy / period).cos())
        } else {
            1.0
        };
        for (x, px) in row.iter_mut().enumerate() {
            let fx = x as f32;
            let mut c = *px;
            let l = luma(c).clamp(0.0, 1.0);
            if do_noise {
                let amt = p.sensor_noise * 0.16 * (0.35 + (1.0 - l));
                for i in 0..3 {
                    c[i] += vnoise(fx, fy, nsize, 100 + i as u32) * amt;
                }
            }
            if do_grain {
                let amt = p.grain * 0.15 * (0.3 + 3.0 * l * (1.0 - l));
                let g = vnoise(fx, fy, gsize, 1);
                for i in 0..3 {
                    let cg = if p.grain_color > 0.0 { vnoise(fx, fy, gsize, 2 + i as u32) } else { 0.0 };
                    c[i] += (g * (1.0 - p.grain_color) + cg * p.grain_color) * amt;
                }
            }
            for v in &mut c {
                *v *= scan;
            }
            if do_post {
                let t = if p.dither { BAYER4[(y % 4) * 4 + x % 4] / 16.0 - 0.47 } else { 0.0 };
                for v in &mut c {
                    *v = ((*v * (levels - 1.0) + t).round() / (levels - 1.0)).clamp(0.0, 1.0);
                }
            }
            for v in &mut c {
                *v = v.clamp(0.0, 1.0);
            }
            *px = c;
        }
    });
}

fn vignette(img: &mut Img, p: &Params) {
    let (w, h) = (img.w as f32, img.h as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let norm = (cx * cx + cy * cy).sqrt();
    let inner = 1.0 - p.vignette_feather.clamp(0.05, 1.0);
    img.px.par_chunks_mut(img.w).enumerate().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let dx = (x as f32 - cx) / norm;
            let dy = (y as f32 - cy) / norm;
            let d = (dx * dx + dy * dy).sqrt() * 1.0;
            let m = smoothstep(inner * 0.7, 1.05, d);
            for v in px.iter_mut() {
                if p.vignette > 0.0 {
                    *v *= 1.0 - p.vignette * m;
                } else {
                    *v += (1.0 - *v) * (-p.vignette) * m;
                }
            }
        }
    });
}

fn border(img: &Img, p: &Params) -> Img {
    let b = (p.border * 0.12 * img.w.min(img.h) as f32).round() as usize;
    let col = if p.border_white { [0.96, 0.96, 0.95] } else { [0.02, 0.02, 0.02] };
    let (w, h) = (img.w + 2 * b, img.h + 2 * b);
    let mut out = Img { w, h, px: vec![col; w * h] };
    out.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        if y >= b && y < b + img.h {
            let sy = y - b;
            row[b..b + img.w].copy_from_slice(&img.px[sy * img.w..(sy + 1) * img.w]);
        }
    });
    out
}

/// Size of the processed result for a source of `w × h`, without rendering it.
pub fn output_size(w: usize, h: usize, p: &Params) -> (usize, usize) {
    let (mut w, mut h) = if p.rotate % 2 == 1 { (h, w) } else { (w, h) };
    let (_, _, cw, ch) = crop_px(w, h, p.crop);
    (w, h) = (cw, ch);
    if p.border > 0.001 {
        let b = (p.border * 0.12 * w.min(h) as f32).round() as usize;
        w += 2 * b;
        h += 2 * b;
    }
    (w, h)
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

pub const HIST_BINS: usize = 128;

#[derive(Clone, Default)]
pub struct Histogram {
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
    pub l: Vec<f32>,
}

pub fn histogram(img: &Img) -> Histogram {
    let mut r = vec![0u32; HIST_BINS];
    let mut g = vec![0u32; HIST_BINS];
    let mut b = vec![0u32; HIST_BINS];
    let mut l = vec![0u32; HIST_BINS];
    let bin = |v: f32| ((v.clamp(0.0, 1.0) * (HIST_BINS - 1) as f32).round()) as usize;
    for p in img.px.iter().step_by(2) {
        r[bin(p[0])] += 1;
        g[bin(p[1])] += 1;
        b[bin(p[2])] += 1;
        l[bin(luma(*p))] += 1;
    }
    // normalise against a robust peak so a single spike doesn't flatten the rest
    let norm = |v: Vec<u32>| -> Vec<f32> {
        let mut s: Vec<u32> = v.clone();
        s.sort_unstable();
        let peak = s[HIST_BINS - 3].max(1) as f32;
        v.into_iter().map(|x| (x as f32 / peak).min(1.0).sqrt()).collect()
    };
    Histogram { r: norm(r), g: norm(g), b: norm(b), l: norm(l) }
}

/// Sharpening applied once at output resolution.
pub fn output_sharpen(img: &mut Img, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let b = blur(img, 0.8);
    img.px.par_iter_mut().zip(b.px.par_iter()).for_each(|(px, b)| {
        for c in 0..3 {
            px[c] = (px[c] + (px[c] - b[c]) * amount).clamp(0.0, 1.0);
        }
    });
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

pub fn builtin_presets() -> Vec<(String, Params)> {
    let d = Params::default;
    let mut v: Vec<(String, Params)> = Vec::new();

    v.push(("Original".into(), d()));

    v.push(("Overcast".into(), Params {
        temperature: -0.22,
        contrast: -0.12,
        fade: 0.35,
        saturation: -0.35,
        highlights: -0.25,
        shadow_tone: 0.35,
        shadow_hue: 195.0,
        grain: 0.22,
        ..d()
    }));

    v.push(("Camcorder".into(), Params {
        sharpen: 0.55,
        sensor_noise: 0.35,
        aberration: 0.35,
        highlights: -0.35,
        shadows: 0.15,
        saturation: -0.18,
        temperature: -0.12,
        scanlines: 0.12,
        vignette: 0.18,
        ..d()
    }));

    v.push(("Night CCD".into(), Params {
        exposure: -0.35,
        contrast: 0.35,
        blacks: -0.3,
        temperature: -0.2,
        sensor_noise: 0.6,
        halation: 0.35,
        bloom: 0.2,
        grain: 0.3,
        vignette: 0.3,
        ..d()
    }));

    v.push(("Streak".into(), Params {
        sharpen: 0.4,
        sensor_noise: 0.25,
        aberration: 0.25,
        saturation: -0.2,
        temperature: -0.15,
        highlights: -0.2,
        streaks: 0.17,
        streak_width: 0.25,
        seed: 7,
        ..d()
    }));

    v.push(("Silver".into(), Params {
        mono: true,
        contrast: 0.3,
        fade: 0.12,
        grain: 0.45,
        grain_size: 0.45,
        highlight_tone: 0.12,
        highlight_hue: 40.0,
        vignette: 0.2,
        ..d()
    }));

    v.push(("Bleach".into(), Params {
        saturation: -0.55,
        contrast: 0.45,
        highlights: -0.2,
        fade: 0.08,
        sharpen: 0.25,
        ..d()
    }));

    v.push(("Red Room".into(), Params {
        halation: 0.6,
        bloom: 0.25,
        temperature: 0.25,
        tint: 0.12,
        contrast: 0.2,
        highlight_tone: 0.3,
        highlight_hue: 8.0,
        grain: 0.2,
        ..d()
    }));

    v.push(("Sorted".into(), Params {
        sort_amount: 0.3,
        sort_low: 0.5,
        sort_high: 1.0,
        contrast: 0.15,
        saturation: -0.25,
        ..d()
    }));

    v.push(("Signal".into(), Params {
        rgb_split: 0.25,
        slices: 0.35,
        slice_shift: 0.2,
        scanlines: 0.2,
        saturation: -0.3,
        temperature: -0.15,
        fade: 0.15,
        ..d()
    }));

    v.push(("Matte".into(), Params {
        fade: 0.5,
        contrast: -0.15,
        saturation: -0.2,
        vibrance: 0.15,
        shadow_tone: 0.25,
        shadow_hue: 210.0,
        highlight_tone: 0.15,
        highlight_hue: 30.0,
        ..d()
    }));

    // Muted, sickly olive-ochre haze: everything looks like it's seen through dust and fever.
    v.push(("Pathologic".into(), Params {
        exposure: -0.15,
        temperature: 0.45,
        tint: -0.4,
        saturation: -0.6,
        contrast: -0.15,
        highlights: -0.35,
        fade: 0.3,
        shadow_tone: 0.6,
        shadow_hue: 70.0,
        highlight_tone: 0.75,
        highlight_hue: 55.0,
        softness: 0.25,
        lowres: 0.35,
        lowres_smooth: true,
        vignette: 0.4,
        vignette_feather: 0.8,
        grain: 0.12,
        ..d()
    }));

    // Night-time city at 640×480: crushed blacks, cold steel teal, chunky texels.
    v.push(("Deus Ex".into(), Params {
        exposure: -0.75,
        contrast: 0.35,
        blacks: -0.35,
        highlights: -0.3,
        temperature: -0.15,
        tint: -0.12,
        saturation: -0.6,
        shadow_tone: 0.6,
        shadow_hue: 170.0,
        highlight_tone: 0.25,
        highlight_hue: 190.0,
        lowres: 0.5,
        lowres_smooth: false,
        posterize: 0.12,
        dither: false,
        vignette: 0.3,
        ..d()
    }));

    // Battlefield filter: drained olive-sepia, hard contrast, heavy film grain.
    v.push(("MGS4".into(), Params {
        saturation: -0.75,
        contrast: 0.45,
        highlights: -0.15,
        blacks: -0.1,
        temperature: 0.35,
        tint: -0.35,
        shadow_tone: 0.55,
        shadow_hue: 75.0,
        highlight_tone: 0.75,
        highlight_hue: 55.0,
        bloom: 0.15,
        bloom_radius: 0.5,
        grain: 0.55,
        grain_size: 0.4,
        sharpen: 0.2,
        vignette: 0.3,
        ..d()
    }));

    v
}

