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
        }
    }
}

impl Layer {
    /// Layer size in frame pixels for a frame whose long edge is `long`.
    pub fn size(&self, long: f32) -> (f32, f32) {
        let w = self.scale * long;
        (w, w / self.aspect.max(1e-3) * self.stretch.max(1e-3))
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
        if let Some(src) = source(&l.path) {
            draw(img, cover.as_deref_mut(), l, &src, sp);
        }
    }
}

/// Put the layers marked "behind" under the picture: they show only where
/// `alpha` (the picture's opacity) is below 1, and `alpha` grows to match.
pub fn composite_behind(img: &mut Img, alpha: &mut [f32], layers: &[Layer], graded: bool, sp: &Space) {
    let mut under: Option<(Img, Vec<f32>)> = None;
    for l in drawn(layers, graded, true) {
        if let Some(src) = source(&l.path) {
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

fn draw(img: &mut Img, cover: Option<&mut [f32]>, l: &Layer, src: &Source, sp: &Space) {
    let long = sp.long();
    let (lw, lh) = l.size(long);
    if lw < 0.5 || lh < 0.5 {
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
            let inv = if t[3] > 1e-5 { 1.0 / t[3] } else { 0.0 };
            let base = &mut row[x];
            match crow.as_deref_mut() {
                None => {
                    for ch in 0..3 {
                        let b = base[ch].clamp(0.0, 1.0);
                        let col = (t[ch] * inv).clamp(0.0, 1.0);
                        base[ch] = b + (blend.apply(b, col) - b) * a;
                    }
                }
                Some(crow) => {
                    // over a partly transparent picture: the blend only acts
                    // where there is picture; elsewhere the layer is itself
                    let a0 = crow[x];
                    let out = a0 + a * (1.0 - a0);
                    if out > 1e-5 {
                        for ch in 0..3 {
                            let b = base[ch].clamp(0.0, 1.0);
                            let col = (t[ch] * inv).clamp(0.0, 1.0);
                            let over = a * (a0 * blend.apply(b, col) + (1.0 - a0) * col) + (1.0 - a) * a0 * b;
                            base[ch] = over / out;
                        }
                    }
                    crow[x] = out;
                }
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
