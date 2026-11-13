//! Local adjustments: a mask is a shape (gradient, ellipse, brush strokes or the
//! whole frame), optionally narrowed by a luminance or colour range, that
//! carries its own small set of adjustments.
//!
//! Positions are stored normalised to the rotated + straightened frame
//! (before cropping), so masks stay attached to the picture when the crop
//! changes, and lengths are in units of the frame's long edge, so a mask
//! renders identically at preview and export resolution.

use crate::pipeline::{Img, Px, blur, hue_rgb, luma, smoothstep, to_linear, to_srgb};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrushStroke {
    /// Points in normalised frame coordinates.
    pub pts: Vec<[f32; 2]>,
    /// Radius in long-edge units.
    pub radius: f32,
    /// 0 = hard edge, 1 = fully soft.
    pub feather: f32,
    pub flow: f32,
    pub erase: bool,
}

/// A click on the photo for an object selection: adds or removes a region.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    pub p: [f32; 2],
    pub add: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Shape {
    /// Full effect on `a`'s side, fading out to nothing at `b`.
    Linear { a: [f32; 2], b: [f32; 2] },
    /// Ellipse around `c`; radii in long-edge units, angle in radians.
    Radial { c: [f32; 2], rx: f32, ry: f32, angle: f32, feather: f32 },
    Brush { strokes: Vec<BrushStroke> },
    Everything,
    /// Grows from clicked points over similar colour, stopping at edges.
    Object { seeds: Vec<Seed>, threshold: f32, connected: bool, smooth: f32, feather: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Refine {
    #[default]
    None,
    Luminance,
    Color,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Local {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub temperature: f32,
    pub tint: f32,
    pub saturation: f32,
    pub sharpness: f32,
    pub tone: f32,
    pub tone_hue: f32,
}

impl Local {
    pub fn is_identity(&self) -> bool {
        let d = Local { tone_hue: self.tone_hue, ..Local::default() };
        *self == d
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mask {
    pub name: String,
    /// Label colour in the list (0 none, 1..=7 see the app's palette).
    pub color: u8,
    pub enabled: bool,
    pub shape: Shape,
    pub invert: bool,
    pub amount: f32,
    pub refine: Refine,
    pub lum_low: f32,
    pub lum_high: f32,
    pub lum_soft: f32,
    pub hue: f32,
    pub hue_range: f32,
    pub hue_soft: f32,
    pub adj: Local,
    /// The Effects tab only applies inside this mask.
    pub effects: bool,
    /// The Glitch tab only applies inside this mask.
    pub glitch: bool,
    /// Cut this area out: it becomes transparent.
    pub transparent: bool,
}

impl Default for Mask {
    fn default() -> Self {
        Mask {
            name: String::new(),
            color: 0,
            enabled: true,
            shape: Shape::Everything,
            invert: false,
            amount: 1.0,
            refine: Refine::None,
            lum_low: 0.6,
            lum_high: 1.0,
            lum_soft: 0.15,
            hue: 210.0,
            hue_range: 30.0,
            hue_soft: 25.0,
            adj: Local { tone_hue: 35.0, ..Local::default() },
            effects: false,
            glitch: false,
            transparent: false,
        }
    }
}

/// Maps pixels of the image being processed back into the frame the masks
/// were drawn on.
#[derive(Clone, Copy, Debug)]
pub struct Space {
    /// Frame size in pixels at the current resolution.
    pub fw: f32,
    pub fh: f32,
    /// Offset of the processed image within the frame (crop origin).
    pub ox: f32,
    pub oy: f32,
}

impl Space {
    pub fn long(&self) -> f32 {
        self.fw.max(self.fh)
    }
    /// Normalised frame point → long-edge units.
    pub fn to_l(&self, p: [f32; 2]) -> [f32; 2] {
        let l = self.long();
        [p[0] * self.fw / l, p[1] * self.fh / l]
    }
    /// Pixel (x, y) of the processed image → long-edge units.
    fn px_to_l(&self, x: usize, y: usize) -> [f32; 2] {
        let l = self.long();
        [(self.ox + x as f32 + 0.5) / l, (self.oy + y as f32 + 0.5) / l]
    }
}

fn shape_alpha(img: &Img, shape: &Shape, sp: &Space) -> Vec<f32> {
    let (w, h) = (img.w, img.h);
    let mut a = vec![0.0f32; w * h];
    match shape {
        Shape::Everything => a.fill(1.0),
        Shape::Linear { a: pa, b: pb } => {
            let (pa, pb) = (sp.to_l(*pa), sp.to_l(*pb));
            let d = [pb[0] - pa[0], pb[1] - pa[1]];
            let len2 = (d[0] * d[0] + d[1] * d[1]).max(1e-8);
            a.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                for (x, o) in row.iter_mut().enumerate() {
                    let p = sp.px_to_l(x, y);
                    let t = ((p[0] - pa[0]) * d[0] + (p[1] - pa[1]) * d[1]) / len2;
                    *o = 1.0 - smoothstep(0.0, 1.0, t);
                }
            });
        }
        Shape::Radial { c, rx, ry, angle, feather } => {
            let c = sp.to_l(*c);
            let (s, co) = angle.sin_cos();
            let (rx, ry) = (rx.max(1e-4), ry.max(1e-4));
            let inner = 1.0 - feather.clamp(0.01, 1.0);
            a.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                for (x, o) in row.iter_mut().enumerate() {
                    let p = sp.px_to_l(x, y);
                    let (dx, dy) = (p[0] - c[0], p[1] - c[1]);
                    let u = (dx * co + dy * s) / rx;
                    let v = (-dx * s + dy * co) / ry;
                    let d = (u * u + v * v).sqrt();
                    *o = 1.0 - smoothstep(inner, 1.0, d);
                }
            });
        }
        Shape::Brush { strokes } => {
            for st in strokes {
                stamp_stroke(&mut a, w, h, st, sp);
            }
        }
        Shape::Object { seeds, threshold, connected, smooth, feather } => {
            a = object_alpha(img, seeds, *threshold, *connected, *smooth, *feather, sp);
        }
    }
    a
}

// ---------------------------------------------------------------------------
// Object selection
// ---------------------------------------------------------------------------

/// Selections are grown on a fixed-size working copy, so the preview and the
/// full-resolution export select exactly the same region.
const WORK_EDGE: f32 = 640.0;

fn to_lab(c: Px) -> [f32; 3] {
    let (r, g, b) = (to_linear(c[0].clamp(0.0, 1.0)), to_linear(c[1].clamp(0.0, 1.0)), to_linear(c[2].clamp(0.0, 1.0)));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f32| if t > 0.008_856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

#[inline]
fn de(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Area-average resample to `w × h`.
fn shrink(img: &Img, w: usize, h: usize) -> Img {
    let mut out = Img { w, h, px: vec![[0.0; 3]; w * h] };
    out.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let y0 = y * img.h / h;
        let y1 = ((y + 1) * img.h / h).max(y0 + 1);
        for (x, o) in row.iter_mut().enumerate() {
            let x0 = x * img.w / w;
            let x1 = ((x + 1) * img.w / w).max(x0 + 1);
            let mut acc = [0.0; 3];
            for yy in y0..y1 {
                for s in &img.px[yy * img.w + x0..yy * img.w + x1] {
                    for c in 0..3 {
                        acc[c] += s[c];
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            *o = acc.map(|v| v / n);
        }
    });
    out
}

/// Recently grown selections, keyed by their inputs: tweaking a mask's own
/// adjustments re-renders without growing the region again.
static OBJECT_CACHE: std::sync::LazyLock<std::sync::Mutex<Vec<(u64, std::sync::Arc<Vec<f32>>)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

fn object_key(lab: &[[f32; 3]], w: usize, seeds: &[Seed], threshold: f32, connected: bool, sp: &Space, img_w: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (w, lab.len(), connected, threshold.to_bits(), img_w).hash(&mut h);
    (sp.fw.to_bits(), sp.fh.to_bits(), sp.ox.to_bits(), sp.oy.to_bits()).hash(&mut h);
    for s in seeds {
        (s.p[0].to_bits(), s.p[1].to_bits(), s.add).hash(&mut h);
    }
    for p in lab.iter().step_by(3) {
        (p[0].to_bits(), p[1].to_bits(), p[2].to_bits()).hash(&mut h);
    }
    h.finish()
}

/// Region growing as a bottleneck flood: the cost of reaching a pixel is the
/// largest single colour step on the best path from the click (so texture and
/// smooth shading don't add up, but a real edge blocks), combined with how far
/// its colour is from the clicked one (so slow drifts can't run away).
fn grow(lab: &[[f32; 3]], w: usize, h: usize, sx: usize, sy: usize, limit: f32, connected: bool) -> Vec<f32> {
    let n = w * h;
    // seed colour: mean of the 5×5 around the click
    let mut sc = [0.0; 3];
    let mut cnt = 0.0;
    for y in sy.saturating_sub(2)..(sy + 3).min(h) {
        for x in sx.saturating_sub(2)..(sx + 3).min(w) {
            let p = lab[y * w + x];
            for c in 0..3 {
                sc[c] += p[c];
            }
            cnt += 1.0;
        }
    }
    let sc = sc.map(|v| v / cnt);
    if !connected {
        return lab.par_iter().map(|p| de(*p, sc)).collect();
    }
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    const EDGE: f32 = 3.0;
    const DRIFT: f32 = 1.0;
    let mut cost = vec![f32::INFINITY; n];
    let mut heap = BinaryHeap::new();
    let s = sy * w + sx;
    cost[s] = 0.0;
    heap.push(Reverse((0u32, s as u32)));
    let cap = limit * 1.5;
    while let Some(Reverse((cb, i))) = heap.pop() {
        let i = i as usize;
        let c = f32::from_bits(cb);
        if c > cost[i] {
            continue;
        }
        let (x, y) = ((i % w) as isize, (i / w) as isize);
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                continue;
            }
            let j = ny as usize * w + nx as usize;
            let nc = c.max(de(lab[i], lab[j]) * EDGE).max(de(lab[j], sc) * DRIFT);
            if nc < cost[j] && nc <= cap {
                cost[j] = nc;
                heap.push(Reverse((nc.to_bits(), j as u32)));
            }
        }
    }
    cost
}

fn object_alpha(img: &Img, seeds: &[Seed], threshold: f32, connected: bool, smooth: f32, feather: f32, sp: &Space) -> Vec<f32> {
    let (w, h) = (img.w, img.h);
    if seeds.is_empty() {
        return vec![0.0; w * h];
    }
    let f = (w.max(h) as f32 / WORK_EDGE).max(1.0);
    let (ww, wh) = (((w as f32 / f).round() as usize).max(1), ((h as f32 / f).round() as usize).max(1));
    // a light blur first, so fine texture (foliage, fabric) reads as one surface
    let small = blur(&shrink(img, ww, wh), 0.8);
    let lab: Vec<[f32; 3]> = small.px.par_iter().map(|p| to_lab(*p)).collect();
    // threshold 0..1 → colour-difference budget
    let limit = if connected { 4.0 + threshold.powf(1.3) * 60.0 } else { 2.0 + threshold * 45.0 };
    let soft = limit * 0.3;

    let key = object_key(&lab, ww, seeds, threshold, connected, sp, w);
    let cached = OBJECT_CACHE.lock().ok().and_then(|c| c.iter().find(|e| e.0 == key).map(|e| e.1.clone()));
    let acc = match cached {
        Some(a) => a,
        None => {
            let mut acc = vec![0.0f32; ww * wh];
            for s in seeds {
                let px = (s.p[0] * sp.fw - sp.ox) / w as f32 * ww as f32;
                let py = (s.p[1] * sp.fh - sp.oy) / h as f32 * wh as f32;
                if px < 0.0 || py < 0.0 || px >= ww as f32 || py >= wh as f32 {
                    continue;
                }
                let cost = grow(&lab, ww, wh, px as usize, py as usize, limit, connected);
                acc.par_iter_mut().zip(cost.par_iter()).for_each(|(a, &c)| {
                    let v = 1.0 - smoothstep(limit, limit + soft, c);
                    *a = if s.add { a.max(v) } else { *a * (1.0 - v) };
                });
            }
            let acc = std::sync::Arc::new(acc);
            if let Ok(mut c) = OBJECT_CACHE.lock() {
                if c.len() >= 12 {
                    c.remove(0);
                }
                c.push((key, acc.clone()));
            }
            acc
        }
    };

    // smooth: fill pinholes and round off jaggies; feather: soften the edge
    let mut m = Img { w: ww, h: wh, px: acc.iter().map(|&v| [v; 3]).collect() };
    if smooth > 0.001 {
        m = blur(&m, 0.5 + smooth * 5.0);
        m.px.par_iter_mut().for_each(|p| {
            let v = smoothstep(0.3, 0.6, p[0]);
            *p = [v; 3];
        });
    }
    if feather > 0.001 {
        m = blur(&m, feather * 14.0);
    }
    // bilinear upsample to the image
    let (sx, sy) = (ww as f32 / w as f32, wh as f32 / h as f32);
    let mut a = vec![0.0f32; w * h];
    a.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            *o = m.sample((x as f32 + 0.5) * sx, (y as f32 + 0.5) * sy)[0].clamp(0.0, 1.0);
        }
    });
    a
}

/// Rasterise one stroke into a local buffer (max of its dabs, so a stroke
/// never builds up over itself), then composite it onto the mask.
pub(crate) fn stamp_stroke(a: &mut [f32], w: usize, h: usize, st: &BrushStroke, sp: &Space) {
    if st.pts.is_empty() {
        return;
    }
    let l = sp.long();
    let r = (st.radius * l).max(0.5);
    let hard = r * (1.0 - st.feather.clamp(0.0, 1.0));
    // stroke points in processed-image pixels
    let pts: Vec<[f32; 2]> = st.pts.iter().map(|p| [p[0] * sp.fw - sp.ox, p[1] * sp.fh - sp.oy]).collect();
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &pts {
        x0 = x0.min(p[0] - r);
        y0 = y0.min(p[1] - r);
        x1 = x1.max(p[0] + r);
        y1 = y1.max(p[1] + r);
    }
    let bx0 = x0.floor().max(0.0) as usize;
    let by0 = y0.floor().max(0.0) as usize;
    let bx1 = (x1.ceil().max(0.0) as usize).min(w);
    let by1 = (y1.ceil().max(0.0) as usize).min(h);
    if bx1 <= bx0 || by1 <= by0 {
        return;
    }
    let (bw, bh) = (bx1 - bx0, by1 - by0);
    let mut buf = vec![0.0f32; bw * bh];

    // resample the polyline into evenly spaced dabs
    let spacing = (r * 0.2).max(0.5);
    let mut dabs = vec![pts[0]];
    let mut carry = 0.0;
    for seg in pts.windows(2) {
        let (p, q) = (seg[0], seg[1]);
        let len = ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2)).sqrt();
        let mut t = spacing - carry;
        while t <= len {
            let f = t / len;
            dabs.push([p[0] + (q[0] - p[0]) * f, p[1] + (q[1] - p[1]) * f]);
            t += spacing;
        }
        carry = (carry + len) % spacing;
    }

    for d in dabs {
        let ix0 = ((d[0] - r).floor().max(bx0 as f32) as usize).max(bx0);
        let iy0 = ((d[1] - r).floor().max(by0 as f32) as usize).max(by0);
        let ix1 = ((d[0] + r).ceil().max(0.0) as usize).min(bx1);
        let iy1 = ((d[1] + r).ceil().max(0.0) as usize).min(by1);
        for y in iy0..iy1 {
            let dy = y as f32 + 0.5 - d[1];
            let row = &mut buf[(y - by0) * bw..(y - by0 + 1) * bw];
            for x in ix0..ix1 {
                let dx = x as f32 + 0.5 - d[0];
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < r {
                    let v = 1.0 - smoothstep(hard, r + 1e-3, dist);
                    let o = &mut row[x - bx0];
                    *o = o.max(v);
                }
            }
        }
    }

    let flow = st.flow.clamp(0.0, 1.0);
    for y in 0..bh {
        let dst = &mut a[(by0 + y) * w + bx0..(by0 + y) * w + bx1];
        for (o, s) in dst.iter_mut().zip(&buf[y * bw..(y + 1) * bw]) {
            let s = s * flow;
            *o = if st.erase { *o * (1.0 - s) } else { *o + (1.0 - *o) * s };
        }
    }
}

pub fn hue_sat(c: Px) -> (f32, f32) {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let d = mx - mn;
    if d < 1e-5 {
        return (0.0, 0.0);
    }
    let h = if mx == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if mx == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    };
    (h * 60.0, if mx > 0.0 { d / mx } else { 0.0 })
}

/// Final mask opacity per pixel, evaluated against the image as it stands
/// when masks are applied (so ranges see the global look).
pub fn alpha(img: &Img, m: &Mask, sp: &Space) -> Vec<f32> {
    let mut a = shape_alpha(img, &m.shape, sp);
    let amount = m.amount.clamp(0.0, 1.0);
    a.par_iter_mut().zip(img.px.par_iter()).for_each(|(o, px)| {
        let mut v = if m.invert { 1.0 - *o } else { *o };
        match m.refine {
            Refine::None => {}
            Refine::Luminance => {
                let l = luma(*px).clamp(0.0, 1.0);
                let s = m.lum_soft.max(0.001);
                let (lo, hi) = (m.lum_low.min(m.lum_high), m.lum_low.max(m.lum_high));
                v *= smoothstep(lo - s, lo, l) * (1.0 - smoothstep(hi, hi + s, l));
            }
            Refine::Color => {
                let (hue, sat) = hue_sat(*px);
                let dh = ((hue - m.hue).rem_euclid(360.0)).min((m.hue - hue).rem_euclid(360.0));
                v *= (1.0 - smoothstep(m.hue_range, m.hue_range + m.hue_soft.max(0.5), dh)) * smoothstep(0.03, 0.15, sat);
            }
        }
        *o = v * amount;
    });
    a
}

const LUT_N: usize = 4096;

/// Tabulated transfer curves: masks run per pixel per mask, and `powf` dominates.
struct Luts {
    lin: Vec<f32>,
    srgb: Vec<f32>,
}

static LUTS: std::sync::LazyLock<Luts> = std::sync::LazyLock::new(|| {
    let f = |g: fn(f32) -> f32| (0..=LUT_N).map(|i| g(i as f32 / LUT_N as f32)).collect();
    Luts { lin: f(to_linear), srgb: f(to_srgb) }
});

#[inline]
fn lut(t: &[f32], x: f32) -> f32 {
    let p = x.clamp(0.0, 1.0) * LUT_N as f32;
    let i = (p as usize).min(LUT_N - 1);
    let f = p - i as f32;
    t[i] + (t[i + 1] - t[i]) * f
}

#[inline]
fn fast_linear(x: f32) -> f32 {
    if x > 1.0 { to_linear(x) } else { lut(&LUTS.lin, x) }
}

#[inline]
fn fast_srgb(x: f32) -> f32 {
    if x > 1.0 { to_srgb(x) } else { lut(&LUTS.srgb, x) }
}

/// The local tone & colour adjustment for one display-referred pixel.
/// Per-mask constants, computed once rather than per pixel.
struct Prep {
    wb: [f32; 3],
    linear: bool,
    k: f32,
    tone_col: Px,
}

impl Prep {
    fn new(l: &Local) -> Prep {
        let gain = 2f32.powf(l.exposure);
        Prep {
            wb: [
                gain * (1.0 + l.temperature * 0.30) * (1.0 + l.tint * 0.10),
                gain * (1.0 - l.tint * 0.18),
                gain * (1.0 - l.temperature * 0.30) * (1.0 + l.tint * 0.10),
            ],
            linear: l.exposure != 0.0 || l.temperature != 0.0 || l.tint != 0.0,
            k: (l.contrast * 1.1).exp(),
            tone_col: hue_rgb(l.tone_hue),
        }
    }
}

fn adjust(c: Px, l: &Local, pr: &Prep) -> Px {
    let mut c = c;
    if pr.linear {
        for i in 0..3 {
            c[i] = fast_srgb(fast_linear(c[i]) * pr.wb[i]);
        }
    }
    if l.shadows != 0.0 || l.highlights != 0.0 {
        let y = luma(c).clamp(0.0, 1.5);
        let s_mask = (1.0 - y).max(0.0).powi(2) * y.min(1.0) * 4.0;
        let h_mask = y.min(1.0).powi(2) * (1.0 - y.min(1.0)) * 4.0 + (y - 1.0).max(0.0);
        let ny = y + l.shadows * 0.3 * s_mask + l.highlights * 0.35 * h_mask;
        if y > 1e-4 {
            let r = (ny / y).max(0.0);
            for v in &mut c {
                *v *= r;
            }
        }
    }
    if l.contrast != 0.0 {
        let k = pr.k;
        for v in &mut c {
            let x = v.clamp(0.0, 1.0);
            *v = if x < 0.5 { 0.5 * (2.0 * x).powf(k) } else { 1.0 - 0.5 * (2.0 * (1.0 - x)).powf(k) };
        }
    }
    if l.saturation != 0.0 {
        let y = luma(c);
        for v in &mut c {
            *v = y + (*v - y) * (1.0 + l.saturation).max(0.0);
        }
    }
    if l.tone > 0.0 {
        // tint toward a colour while keeping luminance
        let y = luma(c);
        let tl = luma(pr.tone_col);
        for i in 0..3 {
            c[i] += (pr.tone_col[i] - tl) * l.tone * 0.5;
        }
        let ny = luma(c);
        if ny > 1e-4 {
            for v in &mut c {
                *v *= y / ny;
            }
        }
    }
    c.map(|v| v.clamp(0.0, 1.0))
}

/// What applying the masks leaves behind besides the adjusted picture.
#[derive(Default)]
pub struct Applied {
    /// Opacity of the mask asked for, for drawing it on screen.
    pub overlay: Option<Vec<f32>>,
    /// Opacity of the picture (1 = kept), when any mask cuts areas out.
    pub cutout: Option<Vec<f32>>,
    /// The area of each mask that adjusts the picture, by index.
    pub areas: Vec<Option<Vec<f32>>>,
}

/// One mask's adjustments over its area `a`.
fn adjust_area(img: &mut Img, l: &Local, a: &[f32], u: f32) {
    let pr = Prep::new(l);
    img.px.par_iter_mut().zip(a.par_iter()).for_each(|(px, &w)| {
        if w > 1e-4 {
            let t = adjust(*px, l, &pr);
            for c in 0..3 {
                px[c] += (t[c] - px[c]) * w;
            }
        }
    });
    if l.sharpness.abs() > 0.001 {
        let b = if l.sharpness > 0.0 { blur(img, (0.8 * u).max(0.8)) } else { blur(img, 6.0 * u) };
        let s = l.sharpness;
        img.px.par_iter_mut().zip(b.px.par_iter()).zip(a.par_iter()).for_each(|((px, b), &w)| {
            for c in 0..3 {
                px[c] = if s > 0.0 {
                    (px[c] + (px[c] - b[c]) * s * 2.0 * w).clamp(0.0, 1.0)
                } else {
                    px[c] + (b[c] - px[c]) * (-s) * w
                };
            }
        });
    }
}

/// Apply every enabled mask in order. Each mask's area is judged on `img`
/// as earlier masks left it.
pub fn apply(img: &mut Img, masks: &[Mask], sp: &Space, u: f32, overlay: Option<usize>) -> Applied {
    let mut out = Applied { areas: vec![None; masks.len()], ..Default::default() };
    for (i, m) in masks.iter().enumerate() {
        let want_overlay = overlay == Some(i);
        let adjusts = m.enabled && !m.adj.is_identity();
        let cuts = m.enabled && m.transparent;
        if !adjusts && !cuts && !want_overlay {
            continue;
        }
        let a = alpha(img, m, sp);
        if adjusts {
            adjust_area(img, &m.adj, &a, u);
        }
        if cuts {
            let keep = out.cutout.get_or_insert_with(|| vec![1.0; img.w * img.h]);
            keep.par_iter_mut().zip(a.par_iter()).for_each(|(k, &w)| *k *= 1.0 - w);
        }
        if want_overlay {
            out.overlay = Some(a.clone());
        }
        if adjusts {
            out.areas[i] = Some(a);
        }
    }
    out
}

/// Apply the masks' adjustments to another picture using areas already judged
/// by [`apply`] (on the photo alone, when layers are in the picture).
pub fn adjust_with(img: &mut Img, masks: &[Mask], applied: &Applied, u: f32) {
    for (m, a) in masks.iter().zip(&applied.areas) {
        if let Some(a) = a {
            adjust_area(img, &m.adj, a, u);
        }
    }
}

/// Combined area of the enabled masks that `pick` chooses (e.g. those that
/// hold the Effects tab), or None when none is chosen.
pub fn union_alpha(img: &Img, masks: &[Mask], sp: &Space, pick: fn(&Mask) -> bool) -> Option<Vec<f32>> {
    let mut out: Option<Vec<f32>> = None;
    for m in masks.iter().filter(|m| m.enabled && pick(m)) {
        let a = alpha(img, m, sp);
        match &mut out {
            None => out = Some(a),
            Some(o) => o.par_iter_mut().zip(a.par_iter()).for_each(|(o, &v)| *o = o.max(v)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize, v: f32) -> Img {
        Img { w, h, px: vec![[v, v, v]; w * h] }
    }
    fn sp(w: usize, h: usize) -> Space {
        Space { fw: w as f32, fh: h as f32, ox: 0.0, oy: 0.0 }
    }

    #[test]
    fn linear_fades_from_a_to_b() {
        let im = img(100, 100, 0.5);
        let m = Mask { shape: Shape::Linear { a: [0.5, 0.2], b: [0.5, 0.6] }, ..Mask::default() };
        let a = alpha(&im, &m, &sp(100, 100));
        assert!(a[10 * 100 + 50] > 0.99);
        assert!(a[80 * 100 + 50] < 0.01);
        assert!((a[40 * 100 + 50] - 0.5).abs() < 0.1);
    }

    #[test]
    fn radial_is_resolution_independent() {
        let m = Mask { shape: Shape::Radial { c: [0.5, 0.5], rx: 0.2, ry: 0.1, angle: 0.3, feather: 0.4 }, ..Mask::default() };
        let small = alpha(&img(200, 100, 0.5), &m, &sp(200, 100));
        let big = alpha(&img(800, 400, 0.5), &m, &sp(800, 400));
        // same normalised point in both
        for (x, y) in [(100, 50), (130, 55), (60, 30)] {
            let s = small[y * 200 + x];
            let b = big[(y * 4 + 2) * 800 + x * 4 + 2];
            assert!((s - b).abs() < 0.05, "{s} vs {b}");
        }
    }

    #[test]
    fn brush_paints_and_erases() {
        let im = img(100, 100, 0.5);
        let paint = BrushStroke { pts: vec![[0.2, 0.5], [0.8, 0.5]], radius: 0.05, feather: 0.0, flow: 1.0, erase: false };
        let erase = BrushStroke { pts: vec![[0.5, 0.2], [0.5, 0.8]], radius: 0.05, feather: 0.0, flow: 1.0, erase: true };
        let m = Mask { shape: Shape::Brush { strokes: vec![paint, erase] }, ..Mask::default() };
        let a = alpha(&im, &m, &sp(100, 100));
        assert!(a[50 * 100 + 30] > 0.99);
        assert!(a[50 * 100 + 50] < 0.01);
        assert!(a[20 * 100 + 30] < 0.01);
    }

    #[test]
    fn crop_offset_keeps_masks_on_the_picture() {
        // a mask pixel at frame (60, 40) must land at (10, 20) in a crop starting at (50, 20)
        let m = Mask { shape: Shape::Radial { c: [0.6, 0.4], rx: 0.02, ry: 0.02, angle: 0.0, feather: 0.1 }, ..Mask::default() };
        let full = Space { fw: 100.0, fh: 100.0, ox: 50.0, oy: 20.0 };
        let a = alpha(&img(40, 40, 0.5), &m, &full);
        assert!(a[20 * 40 + 10] > 0.9);
        assert!(a[5 * 40 + 35] < 0.01);
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;
    #[test]
    fn brush_on_wide_frame() {
        let (w, h) = (1800usize, 1350usize);
        let im = Img { w, h, px: vec![[0.5; 3]; w * h] };
        let st = BrushStroke { pts: vec![[0.1, 0.8], [0.3, 0.7], [0.5, 0.85], [0.8, 0.75]], radius: 0.04, feather: 0.6, flow: 1.0, erase: false };
        let m = Mask { shape: Shape::Brush { strokes: vec![st] }, ..Mask::default() };
        let a = alpha(&im, &m, &Space { fw: w as f32, fh: h as f32, ox: 0.0, oy: 0.0 });
        let at = |x: f32, y: f32| a[(y * h as f32) as usize * w + (x * w as f32) as usize];
        eprintln!("start {:.2} mid {:.2} off {:.2} sum {:.0}", at(0.1, 0.8), at(0.3, 0.7), at(0.5, 0.2), a.iter().sum::<f32>());
        assert!(at(0.1, 0.8) > 0.9 && at(0.3, 0.7) > 0.9);
    }

    #[test]
    fn object_selects_the_clicked_region_only() {
        // left half dark red square, right half light blue, with a gentle gradient in each
        let (w, h) = (200usize, 100usize);
        let px = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let g = y as f32 / h as f32 * 0.05;
                if x < 100 { [0.5 + g, 0.1, 0.1] } else { [0.5, 0.7 + g, 0.95] }
            })
            .collect();
        let im = Img { w, h, px };
        let shape = Shape::Object { seeds: vec![Seed { p: [0.25, 0.5], add: true }], threshold: 0.25, connected: true, smooth: 0.3, feather: 0.0 };
        let a = alpha(&im, &Mask { shape, ..Mask::default() }, &Space { fw: w as f32, fh: h as f32, ox: 0.0, oy: 0.0 });
        assert!(a[50 * w + 20] > 0.95 && a[5 * w + 90] > 0.9);
        assert!(a[50 * w + 150] < 0.05);
        // subtracting a click on the same region empties it
        let shape = Shape::Object {
            seeds: vec![Seed { p: [0.25, 0.5], add: true }, Seed { p: [0.3, 0.3], add: false }],
            threshold: 0.25,
            connected: true,
            smooth: 0.0,
            feather: 0.0,
        };
        let a = alpha(&im, &Mask { shape, ..Mask::default() }, &Space { fw: w as f32, fh: h as f32, ox: 0.0, oy: 0.0 });
        assert!(a[50 * w + 20] < 0.05);
    }
}
