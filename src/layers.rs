//! Photo layers: other images placed on the canvas, with opacity, blend mode
//! and a free transform.
//!
//! The edit only records *where* a layer is and *which file* it shows; the
//! decoded pixels live in a shared store (an RGBA mip pyramid), so undo
//! history stays light and the export re-reads files it hasn't seen yet.
//! Positions use the same frame coordinates as masks (normalised to the
//! rotated frame, sizes in long-edge units), so layers stay put when the
//! crop changes and look identical in preview and export.

use crate::masks::Space;
use crate::pipeline::Img;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Blend {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    SoftLight,
    Darken,
    Lighten,
    Difference,
    Add,
}

impl Blend {
    pub const ALL: [Blend; 9] = [
        Blend::Normal,
        Blend::Multiply,
        Blend::Screen,
        Blend::Overlay,
        Blend::SoftLight,
        Blend::Darken,
        Blend::Lighten,
        Blend::Difference,
        Blend::Add,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Blend::Normal => "Normal",
            Blend::Multiply => "Multiply",
            Blend::Screen => "Screen",
            Blend::Overlay => "Overlay",
            Blend::SoftLight => "Soft Light",
            Blend::Darken => "Darken",
            Blend::Lighten => "Lighten",
            Blend::Difference => "Difference",
            Blend::Add => "Add",
        }
    }
    #[inline]
    fn apply(self, b: f32, l: f32) -> f32 {
        match self {
            Blend::Normal => l,
            Blend::Multiply => b * l,
            Blend::Screen => 1.0 - (1.0 - b) * (1.0 - l),
            Blend::Overlay => {
                if b < 0.5 {
                    2.0 * b * l
                } else {
                    1.0 - 2.0 * (1.0 - b) * (1.0 - l)
                }
            }
            Blend::SoftLight => (1.0 - 2.0 * l) * b * b + 2.0 * l * b,
            Blend::Darken => b.min(l),
            Blend::Lighten => b.max(l),
            Blend::Difference => (b - l).abs(),
            Blend::Add => (b + l).min(1.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layer {
    pub name: String,
    /// Label colour in the list (0 none, 1..=7 see the app's palette).
    pub color: u8,
    /// Source image file.
    pub path: String,
    /// Width / height of the source.
    pub aspect: f32,
    pub visible: bool,
    /// Centre, normalised to the frame.
    pub x: f32,
    pub y: f32,
    /// Width in units of the frame's long edge.
    pub scale: f32,
    /// Height multiplier on top of the source's own proportions (stretching).
    pub stretch: f32,
    /// Degrees, clockwise.
    pub rotation: f32,
    pub flip: bool,
    pub opacity: f32,
    pub blend: Blend,
    /// Placed under the look (graded with the photo) or on top of it.
    pub graded: bool,
    /// Locked layers can't be picked, moved or changed on the canvas.
    pub locked: bool,
    /// The group (folder) it belongs to, 0 for none.
    pub group: u32,
    /// Sits behind the photo, showing through where masks cut it out.
    pub behind: bool,
    /// Perspective and mesh bending of the layer's rectangle.
    pub warp: Warp,
    /// A text layer: its pixels are drawn from this instead of a file.
    pub text: Option<crate::text::TextSpec>,
}

/// Bends a layer: first the mesh moves points within the layer, then the
/// perspective quad places the result. Offsets are in layer units (fractions
/// of its width and height), so they scale and turn with the layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Warp {
    /// Corner offsets: top-left, top-right, bottom-right, bottom-left.
    pub corners: [[f32; 2]; 4],
    /// Mesh cells across and down.
    pub cols: u8,
    pub rows: u8,
    /// Mesh point offsets, row by row, `(cols + 1) × (rows + 1)` of them
    /// (empty: no mesh bending).
    pub mesh: Vec<[f32; 2]>,
}

impl Default for Warp {
    fn default() -> Self {
        Warp { corners: [[0.0; 2]; 4], cols: 3, rows: 3, mesh: Vec::new() }
    }
}

/// A homography from the unit square onto a quad (p0 at (0,0), p1 at (1,0),
/// p2 at (1,1), p3 at (0,1)).
#[derive(Clone, Copy, Debug)]
pub struct Homography([f32; 8]);

impl Homography {
    pub fn square_to_quad(p: [[f32; 2]; 4]) -> Homography {
        let sx = p[0][0] - p[1][0] + p[2][0] - p[3][0];
        let sy = p[0][1] - p[1][1] + p[2][1] - p[3][1];
        let (dx1, dx2) = (p[1][0] - p[2][0], p[3][0] - p[2][0]);
        let (dy1, dy2) = (p[1][1] - p[2][1], p[3][1] - p[2][1]);
        let den = dx1 * dy2 - dx2 * dy1;
        let (g, h) = if den.abs() < 1e-9 || (sx.abs() < 1e-9 && sy.abs() < 1e-9) {
            (0.0, 0.0)
        } else {
            ((sx * dy2 - dx2 * sy) / den, (dx1 * sy - sx * dy1) / den)
        };
        Homography([
            p[1][0] - p[0][0] + g * p[1][0],
            p[3][0] - p[0][0] + h * p[3][0],
            p[0][0],
            p[1][1] - p[0][1] + g * p[1][1],
            p[3][1] - p[0][1] + h * p[3][1],
            p[0][1],
            g,
            h,
        ])
    }
    /// The mapping back from the quad to the unit square.
    pub fn invert(&self) -> Option<Homography> {
        let [a, b, c, d, e, f, g, h] = self.0;
        let r = [e - f * h, c * h - b, b * f - c * e, f * g - d, a - c * g, c * d - a * f, d * h - e * g, b * g - a * h, a * e - b * d];
        if r[8].abs() < 1e-9 || r.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let n = r[8];
        Some(Homography([r[0] / n, r[1] / n, r[2] / n, r[3] / n, r[4] / n, r[5] / n, r[6] / n, r[7] / n]))
    }
    pub fn map(&self, u: f32, v: f32) -> [f32; 2] {
        let [a, b, c, d, e, f, g, h] = self.0;
        let w = g * u + h * v + 1.0;
        let w = if w.abs() < 1e-6 { 1e-6f32.copysign(w) } else { w };
        [(a * u + b * v + c) / w, (d * u + e * v + f) / w]
    }
}

impl Warp {
    pub fn is_identity(&self) -> bool {
        self.corners.iter().all(|c| c[0] == 0.0 && c[1] == 0.0) && self.mesh.iter().all(|m| m[0] == 0.0 && m[1] == 0.0)
    }
    pub fn has_mesh(&self) -> bool {
        self.mesh.len() == self.points()
    }
    /// How many mesh points the grid has.
    pub fn points(&self) -> usize {
        (self.cols.max(1) as usize + 1) * (self.rows.max(1) as usize + 1)
    }
    /// The quad the perspective puts the layer in, in layer units (0..1 at rest).
    pub fn quad(&self) -> [[f32; 2]; 4] {
        let base = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let mut q = base;
        for i in 0..4 {
            q[i] = [base[i][0] + self.corners[i][0], base[i][1] + self.corners[i][1]];
        }
        q
    }
    /// A mesh point's resting place in the unit square.
    pub fn mesh_rest(&self, i: usize) -> [f32; 2] {
        let cols = self.cols.max(1) as usize;
        let rows = self.rows.max(1) as usize;
        [(i % (cols + 1)) as f32 / cols as f32, (i / (cols + 1)) as f32 / rows as f32]
    }
    /// Mesh offset at (u, v), bilinear within its cell.
    fn mesh_offset(&self, u: f32, v: f32) -> [f32; 2] {
        if !self.has_mesh() {
            return [0.0, 0.0];
        }
        let cols = self.cols.max(1) as usize;
        let rows = self.rows.max(1) as usize;
        let gx = (u.clamp(0.0, 1.0) * cols as f32).min(cols as f32 - 1e-4);
        let gy = (v.clamp(0.0, 1.0) * rows as f32).min(rows as f32 - 1e-4);
        let (i, j) = (gx as usize, gy as usize);
        let (fx, fy) = (gx - i as f32, gy - j as f32);
        let at = |x: usize, y: usize| self.mesh[y * (cols + 1) + x];
        let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i, j + 1), at(i + 1, j + 1));
        let mut o = [0.0; 2];
        for k in 0..2 {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bot = c[k] + (d[k] - c[k]) * fx;
            o[k] = top + (bot - top) * fy;
        }
        o
    }
    /// Where the layer point (u, v) ends up, in layer units (0..1 at rest).
    pub fn apply(&self, h: &Homography, u: f32, v: f32) -> [f32; 2] {
        let d = self.mesh_offset(u, v);
        h.map(u + d[0], v + d[1])
    }
    /// Start (or resize) the mesh grid, keeping the current bend where it can.
    pub fn set_grid(&mut self, cols: u8, rows: u8) {
        let (cols, rows) = (cols.clamp(1, 12), rows.clamp(1, 12));
        if self.has_mesh() && cols == self.cols && rows == self.rows {
            return;
        }
        let old = self.clone();
        self.cols = cols;
        self.rows = rows;
        let n = self.points();
        self.mesh = (0..n)
            .map(|i| {
                let r = self.mesh_rest(i);
                old.mesh_offset(r[0], r[1])
            })
            .collect();
    }
}

/// A folder of layers in the list. Its members always sit together in the stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerGroup {
    pub id: u32,
    pub name: String,
    pub color: u8,
    pub visible: bool,
    pub collapsed: bool,
}

impl Default for LayerGroup {
    fn default() -> Self {
        LayerGroup { id: 1, name: String::new(), color: 0, visible: true, collapsed: false }
    }
}

/// Tidy groups after the stack changed: forget unknown or empty groups and
/// gather each group's layers at the position of its topmost member.
/// Returns the new order as old indices (`order[new] == old`).
pub fn normalize_groups(layers: &mut Vec<Layer>, groups: &mut Vec<LayerGroup>) -> Vec<usize> {
    for l in layers.iter_mut() {
        if l.group != 0 && !groups.iter().any(|g| g.id == l.group) {
            l.group = 0;
        }
    }
    groups.retain(|g| layers.iter().any(|l| l.group == g.id));
    let n = layers.len();
    let mut top_first = Vec::with_capacity(n);
    let mut done = Vec::new();
    for i in (0..n).rev() {
        let g = layers[i].group;
        if g == 0 {
            top_first.push(i);
        } else if !done.contains(&g) {
            done.push(g);
            top_first.extend((0..n).rev().filter(|&j| layers[j].group == g));
        }
    }
    let order: Vec<usize> = top_first.into_iter().rev().collect();
    if order.iter().enumerate().any(|(k, &i)| k != i) {
        let old = std::mem::take(layers);
        *layers = order.iter().map(|&i| old[i].clone()).collect();
    }
    order
}

impl Default for Layer {
    fn default() -> Self {
        Layer {
            name: String::new(),
            color: 0,
            path: String::new(),
            aspect: 1.0,
            visible: true,
            x: 0.5,
            y: 0.5,
            scale: 0.5,
            stretch: 1.0,
            rotation: 0.0,
            flip: false,
            opacity: 1.0,
            blend: Blend::Normal,
            graded: true,
            locked: false,
            group: 0,
            behind: false,
            warp: Warp::default(),
            text: None,
        }
    }
}

impl Layer {
    /// Layer size in frame pixels for a frame whose long edge is `long`.
    pub fn size(&self, long: f32) -> (f32, f32) {
        let w = self.scale * long;
        (w, w / self.aspect.max(1e-3) * self.stretch.max(1e-3))
    }

    /// Where the layer point (u, v) (0..1 across and down) lands, relative to
    /// the layer's centre, in frame pixels, before its rotation.
    pub fn local(&self, h: &Homography, u: f32, v: f32, long: f32) -> [f32; 2] {
        let (w, hh) = self.size(long);
        let q = self.warp.apply(h, u, v);
        [(q[0] - 0.5) * w, (q[1] - 0.5) * hh]
    }

    /// The bent outline of the layer, `n` points per side, clockwise from the
    /// top-left, relative to the centre in frame pixels, before rotation.
    pub fn outline(&self, long: f32, n: usize) -> Vec<[f32; 2]> {
        let h = Homography::square_to_quad(self.warp.quad());
        let n = n.max(1);
        let mut out = Vec::with_capacity(4 * n);
        for k in 0..n {
            out.push(self.local(&h, k as f32 / n as f32, 0.0, long));
        }
        for k in 0..n {
            out.push(self.local(&h, 1.0, k as f32 / n as f32, long));
        }
        for k in 0..n {
            out.push(self.local(&h, 1.0 - k as f32 / n as f32, 1.0, long));
        }
        for k in 0..n {
            out.push(self.local(&h, 0.0, 1.0 - k as f32 / n as f32, long));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Decoded sources
// ---------------------------------------------------------------------------

/// One level of the pyramid: premultiplied RGBA, 16 bits per channel.
struct Level {
    w: usize,
    h: usize,
    px: Vec<[u16; 4]>,
}

impl Level {
    /// Bilinear, premultiplied RGBA in 0..1 at pixel coordinates (x, y).
    #[inline]
    fn sample(&self, x: f32, y: f32) -> [f32; 4] {
        let x = (x - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (y - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let p = |xx: usize, yy: usize| self.px[yy * self.w + xx];
        let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
        let mut o = [0.0; 4];
        for i in 0..4 {
            let top = a[i] as f32 + (b[i] as f32 - a[i] as f32) * fx;
            let bot = c[i] as f32 + (d[i] as f32 - c[i] as f32) * fx;
            o[i] = (top + (bot - top) * fy) / 65535.0;
        }
        o
    }

    fn half(&self) -> Level {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut px = vec![[0u16; 4]; w * h];
        px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for (x, o) in row.iter_mut().enumerate() {
                let mut acc = [0u32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let s = self.px[(2 * y + dy).min(self.h - 1) * self.w + (2 * x + dx).min(self.w - 1)];
                    for i in 0..4 {
                        acc[i] += s[i] as u32;
                    }
                }
                *o = acc.map(|v| (v / 4) as u16);
            }
        });
        Level { w, h, px }
    }
}

pub struct Source {
    /// Finest first.
    levels: Vec<Level>,
}

/// Finest pyramid level, long edge. Enough for a layer covering a large print.
const MAX_EDGE: u32 = 3200;

static STORE: LazyLock<Mutex<HashMap<String, Arc<Source>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decode-ready image → pyramid, stored under `path`. Returns the aspect ratio.
pub fn register(path: &str, img: &image::DynamicImage) -> f32 {
    let (w, h) = (img.width(), img.height());
    let img = if w.max(h) > MAX_EDGE {
        let s = MAX_EDGE as f32 / w.max(h) as f32;
        img.resize_exact(((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1), image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let rgba = img.to_rgba16();
    let (lw, lh) = (rgba.width() as usize, rgba.height() as usize);
    let px: Vec<[u16; 4]> = rgba
        .pixels()
        .map(|p| {
            let a = p.0[3] as u32;
            [(p.0[0] as u32 * a / 65535) as u16, (p.0[1] as u32 * a / 65535) as u16, (p.0[2] as u32 * a / 65535) as u16, p.0[3]]
        })
        .collect();
    let mut levels = vec![Level { w: lw, h: lh, px }];
    while levels.last().map(|l| l.w.max(l.h) > 48).unwrap_or(false) {
        let next = levels.last().unwrap().half();
        levels.push(next);
    }
    if let Ok(mut s) = STORE.lock() {
        s.insert(path.to_string(), Arc::new(Source { levels }));
    }
    w as f32 / h.max(1) as f32
}

/// Drop a decoded source (text layers replace theirs as they're edited).
pub fn forget(key: &str) {
    if let Ok(mut s) = STORE.lock() {
        s.remove(key);
    }
}

/// Is a source with this key loaded?
pub fn has(key: &str) -> bool {
    STORE.lock().map(|s| s.contains_key(key)).unwrap_or(false)
}

/// The pixels a layer shows: its file, or its rendered text.
fn source_for(l: &Layer) -> Option<Arc<Source>> {
    match &l.text {
        Some(t) => {
            let key = crate::text::ensure(t)?;
            STORE.lock().ok().and_then(|s| s.get(&key).cloned())
        }
        None => source(&l.path),
    }
}

/// The decoded source for `path`, reading it from disk if it isn't loaded
/// yet (e.g. exporting after a restart).
fn source(path: &str) -> Option<Arc<Source>> {
    if let Some(s) = STORE.lock().ok().and_then(|s| s.get(path).cloned()) {
        return Some(s);
    }
    let img = crate::io::decode(std::path::Path::new(path)).ok()?;
    register(path, &img);
    STORE.lock().ok().and_then(|s| s.get(path).cloned())
}

// ---------------------------------------------------------------------------
// Compositing
// ---------------------------------------------------------------------------

fn drawn(layers: &[Layer], graded: bool, behind: bool) -> impl Iterator<Item = &Layer> {
    layers.iter().filter(move |l| l.visible && l.graded == graded && l.behind == behind && l.opacity > 0.001)
}

/// Draw every visible layer in front of the photo whose `graded` flag
/// matches, bottom to top. With `cover` (the picture's opacity, where masks
/// cut it out) the layers also fill it in.
pub fn composite(img: &mut Img, layers: &[Layer], graded: bool, sp: &Space, mut cover: Option<&mut [f32]>) {
    for l in drawn(layers, graded, false) {
        if let Some(src) = source_for(l) {
            draw(img, cover.as_deref_mut(), l, &src, sp);
        }
    }
}

/// Put the layers marked "behind" under the picture: they show only where
/// `alpha` (the picture's opacity) is below 1, and `alpha` grows to match.
pub fn composite_behind(img: &mut Img, alpha: &mut [f32], layers: &[Layer], graded: bool, sp: &Space) {
    let mut under: Option<(Img, Vec<f32>)> = None;
    for l in drawn(layers, graded, true) {
        if let Some(src) = source_for(l) {
            let (u, c) = under.get_or_insert_with(|| (Img::new(img.w, img.h), vec![0.0; img.w * img.h]));
            draw(u, Some(c), l, &src, sp);
        }
    }
    let Some((under, cu)) = under else { return };
    // the picture over what's behind it; `under` is drawn over transparency,
    // so its colours are already weighted by its coverage
    img.px.par_iter_mut().zip(alpha.par_iter_mut()).zip(under.px.par_iter().zip(cu.par_iter())).for_each(|((px, a), (u, &c))| {
        let out = *a + c * (1.0 - *a);
        if out > 1e-5 && c > 0.0 {
            for ch in 0..3 {
                px[ch] = (px[ch] * *a + u[ch] * (1.0 - *a)) / out;
            }
        }
        *a = out;
    });
}

/// Lay one premultiplied texel `t` over the picture with weight `a`. With
/// `crow` (the picture's opacity there) it also fills a partly transparent picture.
#[inline]
fn put(base: &mut [f32; 3], crow: Option<&mut f32>, t: [f32; 4], a: f32, blend: Blend) {
    let inv = if t[3] > 1e-5 { 1.0 / t[3] } else { 0.0 };
    match crow {
        None => {
            for ch in 0..3 {
                let b = base[ch].clamp(0.0, 1.0);
                let col = (t[ch] * inv).clamp(0.0, 1.0);
                base[ch] = b + (blend.apply(b, col) - b) * a;
            }
        }
        Some(c0) => {
            // over a partly transparent picture: the blend only acts
            // where there is picture; elsewhere the layer is itself
            let a0 = *c0;
            let out = a0 + a * (1.0 - a0);
            if out > 1e-5 {
                for ch in 0..3 {
                    let b = base[ch].clamp(0.0, 1.0);
                    let col = (t[ch] * inv).clamp(0.0, 1.0);
                    let over = a * (a0 * blend.apply(b, col) + (1.0 - a0) * col) + (1.0 - a) * a0 * b;
                    base[ch] = over / out;
                }
            }
            *c0 = out;
        }
    }
}

fn draw(img: &mut Img, cover: Option<&mut [f32]>, l: &Layer, src: &Source, sp: &Space) {
    if !l.warp.is_identity() {
        return draw_warped(img, cover, l, src, sp);
    }
    let long = sp.long();
    let (lw, lh) = l.size(long);
    if lw < 0.5 || lh < 0.5 || !lw.is_finite() || !lh.is_finite() {
        return;
    }
    // centre in this image's pixels
    let cx = l.x * sp.fw - sp.ox;
    let cy = l.y * sp.fh - sp.oy;
    let (s, c) = l.rotation.to_radians().sin_cos();
    let ex = (lw * c.abs() + lh * s.abs()) * 0.5 + 1.0;
    let ey = (lw * s.abs() + lh * c.abs()) * 0.5 + 1.0;
    let y0 = (cy - ey).floor().max(0.0) as usize;
    let y1 = ((cy + ey).ceil().max(0.0) as usize).min(img.h);
    let x0 = (cx - ex).floor().max(0.0) as usize;
    let x1 = ((cx + ex).ceil().max(0.0) as usize).min(img.w);
    if y1 <= y0 || x1 <= x0 {
        return;
    }
    // the coarsest level that still has at least one texel per output pixel
    let lev = src.levels.iter().rev().find(|v| v.w as f32 >= lw * 0.98).unwrap_or(&src.levels[0]);
    let (opacity, blend, flip, w) = (l.opacity.clamp(0.0, 1.0), l.blend, l.flip, img.w);
    let rows = img.px[y0 * w..y1 * w].par_chunks_mut(w);
    let covers: Vec<Option<&mut [f32]>> = match cover {
        Some(c) => c[y0 * w..y1 * w].chunks_mut(w).map(Some).collect(),
        None => (y0..y1).map(|_| None).collect(),
    };
    rows.zip(covers).enumerate().for_each(|(ry, (row, mut crow))| {
        let dy = (y0 + ry) as f32 + 0.5 - cy;
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let lu = dx * c + dy * s;
            let lv = -dx * s + dy * c;
            let (u, v) = (lu / lw + 0.5, lv / lh + 0.5);
            if !(-0.01..=1.01).contains(&u) || !(-0.01..=1.01).contains(&v) {
                continue;
            }
            // anti-aliased edge: coverage from the distance to the nearest side
            let edge = (u * lw).min((1.0 - u) * lw).min(v * lh).min((1.0 - v) * lh);
            let cover = (edge + 0.5).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let u = if flip { 1.0 - u } else { u };
            let t = lev.sample(u * lev.w as f32, v * lev.h as f32);
            let a = t[3] * cover * opacity;
            if a < 1e-4 {
                continue;
            }
            put(&mut row[x], crow.as_deref_mut().map(|c| &mut c[x]), t, a, blend);
        }
    });
}

/// One triangle of a bent layer, in picture pixels, with its texture
/// coordinates. `outer[k]`: edge k (point k to k+1) is the layer's outline,
/// so it gets an anti-aliased edge.
struct Tri {
    p: [[f32; 2]; 3],
    uv: [[f32; 2]; 3],
    outer: [bool; 3],
    y0: f32,
    y1: f32,
    x0: f32,
    x1: f32,
}

/// A layer bent by its perspective quad and mesh: drawn as a fine mesh of
/// triangles, each mapping its part of the source.
fn draw_warped(img: &mut Img, cover: Option<&mut [f32]>, l: &Layer, src: &Source, sp: &Space) {
    let long = sp.long();
    let (lw, lh) = l.size(long);
    if lw < 0.5 || lh < 0.5 || !lw.is_finite() || !lh.is_finite() {
        return;
    }
    let cx = l.x * sp.fw - sp.ox;
    let cy = l.y * sp.fh - sp.oy;
    let (s, c) = l.rotation.to_radians().sin_cos();
    let h = Homography::square_to_quad(l.warp.quad());
    // finer where the layer is bigger, so the bend stays smooth
    let n = ((lw.max(lh) / 10.0).ceil() as usize).clamp(8, 96);
    let mut verts = Vec::with_capacity((n + 1) * (n + 1));
    for j in 0..=n {
        for i in 0..=n {
            let p = l.local(&h, i as f32 / n as f32, j as f32 / n as f32, long);
            verts.push([cx + p[0] * c - p[1] * s, cy + p[0] * s + p[1] * c]);
        }
    }
    let at = |i: usize, j: usize| verts[j * (n + 1) + i];
    let uv = |i: usize, j: usize| [i as f32 / n as f32, j as f32 / n as f32];
    let mut tris = Vec::with_capacity(2 * n * n);
    let mut mk = |p: [[f32; 2]; 3], uv: [[f32; 2]; 3], outer: [bool; 3]| {
        let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[1][1] - p[0][1]) * (p[2][0] - p[0][0]);
        if area.abs() < 1e-6 || !area.is_finite() {
            return;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for q in &p {
            x0 = x0.min(q[0]);
            x1 = x1.max(q[0]);
            y0 = y0.min(q[1]);
            y1 = y1.max(q[1]);
        }
        tris.push(Tri { p, uv, outer, x0: x0 - 1.0, x1: x1 + 1.0, y0: y0 - 1.0, y1: y1 + 1.0 });
    };
    for j in 0..n {
        for i in 0..n {
            let (a, b, cc, d) = (at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
            let (ua, ub, uc, ud) = (uv(i, j), uv(i + 1, j), uv(i + 1, j + 1), uv(i, j + 1));
            mk([a, b, cc], [ua, ub, uc], [j == 0, i == n - 1, false]);
            mk([a, cc, d], [ua, uc, ud], [false, j == n - 1, i == 0]);
        }
    }
    if tris.is_empty() {
        return;
    }
    let by0 = tris.iter().map(|t| t.y0).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
    let by1 = (tris.iter().map(|t| t.y1).fold(f32::MIN, f32::max).ceil().max(0.0) as usize).min(img.h);
    if by1 <= by0 {
        return;
    }
    // triangles bucketed by bands of rows
    const BAND: usize = 16;
    let nb = (by1 - by0).div_ceil(BAND);
    let mut bands: Vec<Vec<u32>> = vec![Vec::new(); nb];
    for (k, t) in tris.iter().enumerate() {
        let a = ((t.y0.max(by0 as f32) as usize).saturating_sub(by0) / BAND).min(nb - 1);
        let b = ((t.y1.max(by0 as f32) as usize).saturating_sub(by0) / BAND).min(nb - 1);
        for band in &mut bands[a..=b] {
            band.push(k as u32);
        }
    }
    // the level matching the widest stretch of the bend
    let q = l.warp.quad();
    let len = |a: [f32; 2], b: [f32; 2]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    let stretch = len(q[0], q[1]).max(len(q[3], q[2])).max(1.0);
    let lev = src.levels.iter().rev().find(|v| v.w as f32 >= lw * stretch * 0.98).unwrap_or(&src.levels[0]);
    let (opacity, blend, flip, w) = (l.opacity.clamp(0.0, 1.0), l.blend, l.flip, img.w);
    let rows = img.px[by0 * w..by1 * w].par_chunks_mut(w);
    let covers: Vec<Option<&mut [f32]>> = match cover {
        Some(c) => c[by0 * w..by1 * w].chunks_mut(w).map(Some).collect(),
        None => (by0..by1).map(|_| None).collect(),
    };
    rows.zip(covers).enumerate().for_each(|(ry, (row, mut crow))| {
        let y = by0 + ry;
        let py = y as f32 + 0.5;
        for &k in &bands[ry / BAND] {
            let t = &tris[k as usize];
            if py < t.y0 || py > t.y1 {
                continue;
            }
            let p = &t.p;
            let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[1][1] - p[0][1]) * (p[2][0] - p[0][0]);
            let sgn = area.signum();
            let inv_area = 1.0 / area.abs();
            let lens = [0, 1, 2].map(|e| {
                let (a, b) = (p[e], p[(e + 1) % 3]);
                ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt().max(1e-6)
            });
            let xa = t.x0.floor().max(0.0) as usize;
            let xb = (t.x1.ceil().max(0.0) as usize).min(w);
            for x in xa..xb {
                let px = x as f32 + 0.5;
                let mut e = [0.0f32; 3];
                let mut inside = true;
                let mut cov = 1.0f32;
                for k in 0..3 {
                    let (a, b) = (p[k], p[(k + 1) % 3]);
                    e[k] = sgn * ((b[0] - a[0]) * (py - a[1]) - (b[1] - a[1]) * (px - a[0]));
                    if t.outer[k] {
                        let d = e[k] / lens[k];
                        if d < -0.5 {
                            inside = false;
                            break;
                        }
                        cov = cov.min(d + 0.5);
                    } else if e[k] < 0.0 {
                        inside = false;
                        break;
                    }
                }
                if !inside || cov <= 0.0 {
                    continue;
                }
                // barycentric weights: vertex k+2 is opposite edge k
                let (w2, w0, w1) = (e[0] * inv_area, e[1] * inv_area, e[2] * inv_area);
                let u = t.uv[0][0] * w0 + t.uv[1][0] * w1 + t.uv[2][0] * w2;
                let v = t.uv[0][1] * w0 + t.uv[1][1] * w1 + t.uv[2][1] * w2;
                let u = if flip { 1.0 - u } else { u };
                let tx = lev.sample(u.clamp(0.0, 1.0) * lev.w as f32, v.clamp(0.0, 1.0) * lev.h as f32);
                let a = tx[3] * cov.min(1.0) * opacity;
                if a < 1e-4 {
                    continue;
                }
                put(&mut row[x], crow.as_deref_mut().map(|c| &mut c[x]), tx, a, blend);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(w, h, image::Rgba(rgba)))
    }

    #[test]
    fn places_scales_and_blends() {
        register("test://red", &solid(40, 20, [255, 0, 0, 255]));
        let sp = Space { fw: 200.0, fh: 100.0, ox: 0.0, oy: 0.0 };
        let mut img = Img { w: 200, h: 100, px: vec![[0.5; 3]; 200 * 100] };
        // 0.25 of the long edge = 50 px wide, 25 px tall, centred at (60, 50)
        let l = Layer { path: "test://red".into(), aspect: 2.0, x: 0.3, y: 0.5, scale: 0.25, ..Layer::default() };
        composite(&mut img, std::slice::from_ref(&l), true, &sp, None);
        assert_eq!(img.px[50 * 200 + 60], [1.0, 0.0, 0.0]);
        assert_eq!(img.px[50 * 200 + 100], [0.5, 0.5, 0.5]);
        assert_eq!(img.px[30 * 200 + 60], [0.5, 0.5, 0.5]);
        // multiply at half opacity
        let mut img2 = Img { w: 200, h: 100, px: vec![[0.5; 3]; 200 * 100] };
        let m = Layer { blend: Blend::Multiply, opacity: 0.5, ..l.clone() };
        composite(&mut img2, &[m], true, &sp, None);
        let p = img2.px[50 * 200 + 60];
        assert!((p[0] - 0.5).abs() < 1e-3 && (p[1] - 0.25).abs() < 1e-3);
        // the other stage leaves it alone
        let mut img3 = Img { w: 200, h: 100, px: vec![[0.5; 3]; 200 * 100] };
        composite(&mut img3, &[l], false, &sp, None);
        assert_eq!(img3.px[50 * 200 + 60], [0.5, 0.5, 0.5]);
    }

    #[test]
    fn transparent_pixels_stay_transparent() {
        register("test://clear", &solid(10, 10, [255, 255, 255, 0]));
        let sp = Space { fw: 100.0, fh: 100.0, ox: 0.0, oy: 0.0 };
        let mut img = Img { w: 100, h: 100, px: vec![[0.2; 3]; 100 * 100] };
        let l = Layer { path: "test://clear".into(), aspect: 1.0, scale: 0.8, ..Layer::default() };
        composite(&mut img, &[l], true, &sp, None);
        assert!(img.px.iter().all(|p| *p == [0.2; 3]));
    }

    #[test]
    fn an_untouched_warp_draws_like_no_warp() {
        register("test://grad", &image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(32, 16, |x, y| image::Rgba([(x * 8) as u8, (y * 16) as u8, 90, 255]))));
        let sp = Space { fw: 120.0, fh: 80.0, ox: 0.0, oy: 0.0 };
        let l = Layer { path: "test://grad".into(), aspect: 2.0, x: 0.5, y: 0.5, scale: 0.6, rotation: 20.0, ..Layer::default() };
        let mut plain = Img { w: 120, h: 80, px: vec![[0.2; 3]; 120 * 80] };
        composite(&mut plain, std::slice::from_ref(&l), true, &sp, None);
        // a mesh at rest plus a perspective that only moves a corner by nothing
        let mut bent = l.clone();
        bent.warp.set_grid(3, 2);
        bent.warp.corners[1] = [1e-7, 0.0];
        let mut warped = Img { w: 120, h: 80, px: vec![[0.2; 3]; 120 * 80] };
        composite(&mut warped, &[bent], true, &sp, None);
        let diff: f32 = plain.px.iter().zip(&warped.px).map(|(a, b)| (0..3).map(|c| (a[c] - b[c]).abs()).sum::<f32>()).sum::<f32>() / plain.px.len() as f32;
        assert!(diff < 0.01, "mean difference {diff}");
    }

    #[test]
    fn perspective_and_mesh_move_the_picture() {
        register("test://white", &solid(10, 10, [255, 255, 255, 255]));
        let sp = Space { fw: 100.0, fh: 100.0, ox: 0.0, oy: 0.0 };
        let mut l = Layer { path: "test://white".into(), aspect: 1.0, x: 0.5, y: 0.5, scale: 0.5, ..Layer::default() };
        // pull the top corners together: a trapezoid, narrow at the top
        l.warp.corners[0] = [0.4, 0.0];
        l.warp.corners[1] = [-0.4, 0.0];
        let mut img = Img { w: 100, h: 100, px: vec![[0.0; 3]; 100 * 100] };
        composite(&mut img, &[l.clone()], true, &sp, None);
        assert!(img.px[27 * 100 + 30][0] < 0.05, "top-left corner pulled in");
        assert!(img.px[27 * 100 + 50][0] > 0.95, "top middle still covered");
        assert!(img.px[72 * 100 + 27][0] > 0.95, "bottom corner stays");
        // a mesh point pushed to the right drags the middle with it
        l.warp.corners = [[0.0; 2]; 4];
        l.warp.set_grid(2, 2);
        l.warp.mesh[4] = [0.0, 0.0];
        l.warp.mesh[3] = [0.3, 0.0];
        let mut img = Img { w: 100, h: 100, px: vec![[0.0; 3]; 100 * 100] };
        composite(&mut img, &[l], true, &sp, None);
        assert!(img.px[50 * 100 + 27][0] < 0.05, "left edge pulled in at the middle");
        assert!(img.px[26 * 100 + 27][0] > 0.95, "top-left corner untouched");
    }

    #[test]
    fn homography_maps_the_corners() {
        let q = [[0.1, 0.2], [0.9, 0.0], [1.2, 1.1], [-0.1, 0.8]];
        let h = Homography::square_to_quad(q);
        let inv = h.invert().unwrap();
        let back = inv.map(h.map(0.3, 0.7)[0], h.map(0.3, 0.7)[1]);
        assert!((back[0] - 0.3).abs() < 1e-4 && (back[1] - 0.7).abs() < 1e-4, "inverse: {back:?}");
        for (uv, p) in [([0.0, 0.0], q[0]), ([1.0, 0.0], q[1]), ([1.0, 1.0], q[2]), ([0.0, 1.0], q[3])] {
            let m = h.map(uv[0], uv[1]);
            assert!((m[0] - p[0]).abs() < 1e-4 && (m[1] - p[1]).abs() < 1e-4, "{uv:?} -> {m:?}, want {p:?}");
        }
    }

    #[test]
    fn stretch_changes_only_the_height() {
        let l = Layer { aspect: 2.0, scale: 0.5, stretch: 3.0, ..Layer::default() };
        let (w, h) = l.size(100.0);
        assert_eq!((w, h), (50.0, 75.0));
    }

    #[test]
    fn groups_gather_at_their_top_member_and_empty_ones_go() {
        let mk = |name: &str, group: u32| Layer { name: name.into(), group, ..Layer::default() };
        // bottom to top: a(g1) b c(g1) d(g9: unknown)
        let mut layers = vec![mk("a", 1), mk("b", 0), mk("c", 1), mk("d", 9)];
        let mut groups = vec![LayerGroup { id: 1, ..Default::default() }, LayerGroup { id: 2, ..Default::default() }];
        let order = normalize_groups(&mut layers, &mut groups);
        let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["b", "a", "c", "d"]);
        assert_eq!(order, [1, 0, 2, 3]);
        assert_eq!(layers[3].group, 0, "unknown group is dropped");
        assert_eq!(groups.len(), 1, "empty group 2 is removed");
        // already tidy: unchanged
        assert_eq!(normalize_groups(&mut layers, &mut groups), [0, 1, 2, 3]);
    }
}
