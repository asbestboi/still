//! Text layers: typed text in any font, rendered to pixels that the layer
//! machinery then places, scales, stretches, bends (perspective and mesh)
//! and blends like any other layer.
//!
//! A text layer stores only its `TextSpec`; the pixels are rendered on
//! demand (in the preview and export threads) and kept in the layer store
//! under a key derived from the spec, so editing the text re-renders it and
//! undo brings the old pixels back.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, VariableFont, point};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

/// What a text layer shows and how it is styled. Sizes are in em (the font size).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextSpec {
    pub text: String,
    /// Font file; empty for the built-in font.
    pub font: String,
    /// 100 (thin) to 900 (black). Fonts with a weight axis use it directly;
    /// others are emboldened (or not) to approximate it.
    pub weight: f32,
    /// Slant, -1..1 (0.25 looks like a usual italic).
    pub italic: f32,
    /// 0 left, 1 centre, 2 right.
    pub align: u8,
    /// Extra space between letters, in em.
    pub tracking: f32,
    /// Line height, as a multiple of the font's own.
    pub leading: f32,
    /// Colours are sRGB with alpha.
    pub fill: [u8; 4],
    pub gradient: bool,
    pub fill2: [u8; 4],
    /// Direction of the gradient, degrees (90: top to bottom).
    pub gradient_angle: f32,
    /// Outline width, em.
    pub outline: f32,
    pub outline_color: [u8; 4],
    /// Drop shadow opacity, 0..1.
    pub shadow: f32,
    pub shadow_color: [u8; 4],
    pub shadow_distance: f32,
    pub shadow_angle: f32,
    pub shadow_blur: f32,
    /// 0 straight, 1 arc, 2 wave.
    pub shape: u8,
    /// Arc: how far the line bends (1 = half a circle, 2 = a full circle,
    /// negative bends the other way). Wave: the height of the waves.
    pub bend: f32,
    /// Wave: how many waves run across the text.
    pub waves: f32,
}

impl Default for TextSpec {
    fn default() -> Self {
        TextSpec {
            text: "Text".into(),
            font: String::new(),
            weight: 650.0,
            italic: 0.0,
            align: 1,
            tracking: 0.0,
            leading: 1.15,
            fill: [255, 255, 255, 255],
            gradient: false,
            fill2: [255, 176, 64, 255],
            gradient_angle: 90.0,
            outline: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow: 0.0,
            shadow_color: [0, 0, 0, 255],
            shadow_distance: 0.06,
            shadow_angle: 45.0,
            shadow_blur: 0.05,
            shape: 0,
            bend: 0.5,
            waves: 1.0,
        }
    }
}

impl TextSpec {
    /// Key of its rendered pixels in the layer store.
    pub fn key(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(self).unwrap_or_default().hash(&mut h);
        format!("text:{:016x}", h.finish())
    }

    /// The same spec with values that would make rendering pathological
    /// pulled back (sliders accept any typed number).
    fn sane(&self) -> TextSpec {
        let f = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        TextSpec {
            text: self.text.chars().take(4000).collect(),
            weight: f(self.weight, 1.0, 1000.0, 400.0),
            italic: f(self.italic, -2.0, 2.0, 0.0),
            tracking: f(self.tracking, -1.0, 10.0, 0.0),
            leading: f(self.leading, 0.05, 10.0, 1.15),
            outline: f(self.outline, 0.0, 1.0, 0.0),
            shadow: f(self.shadow, 0.0, 1.0, 0.0),
            shadow_distance: f(self.shadow_distance, 0.0, 3.0, 0.0),
            shadow_blur: f(self.shadow_blur, 0.0, 1.0, 0.0),
            bend: f(self.bend, -4.0, 4.0, 0.0),
            waves: f(self.waves, 0.0, 40.0, 1.0),
            ..self.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

static FONT_DATA: LazyLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn font_bytes(path: &str) -> Option<Arc<Vec<u8>>> {
    if path.is_empty() {
        return None;
    }
    if let Some(b) = FONT_DATA.lock().ok().and_then(|m| m.get(path).cloned()) {
        return Some(b);
    }
    let bytes = Arc::new(std::fs::read(path).ok()?);
    if let Ok(mut m) = FONT_DATA.lock() {
        m.insert(path.to_string(), bytes.clone());
    }
    Some(bytes)
}

fn parse_font(bytes: Vec<u8>) -> Option<FontVec> {
    FontVec::try_from_vec(bytes.clone()).ok().or_else(|| FontVec::try_from_vec_and_index(bytes, 0).ok())
}

fn builtin() -> FontVec {
    FontVec::try_from_vec(crate::ui_kit::UI_FONT.to_vec()).expect("bundled font parses")
}

/// The spec's font at its weight, and the built-in font as a fallback for
/// characters the chosen one lacks. The flag says whether the weight was
/// applied by the font itself (a variable font with a weight axis).
fn fonts(spec: &TextSpec) -> (FontVec, FontVec, bool) {
    let mut main = font_bytes(&spec.font).and_then(|b| parse_font(b.to_vec())).unwrap_or_else(builtin);
    let mut fallback = builtin();
    let w = spec.weight.clamp(1.0, 1000.0);
    let real = main.variations().iter().any(|v| &v.tag == b"wght") && main.set_variation(b"wght", w);
    fallback.set_variation(b"wght", w);
    (main, fallback, real)
}

pub const FONT_EXTS: &[&str] = &["ttf", "otf", "ttc", "otc"];

/// Where fonts added in Still are kept.
pub fn user_font_dir() -> PathBuf {
    crate::io::config_dir().join("fonts")
}

/// Copy a font into Still's own font folder, so projects keep working if
/// the original moves. Returns its new path.
pub fn import_font(src: &Path) -> Result<String, String> {
    let bytes = std::fs::read(src).map_err(|e| e.to_string())?;
    if parse_font(bytes.clone()).is_none() {
        return Err("not a font Still can read".into());
    }
    let dir = user_font_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = src.file_name().ok_or("no file name")?;
    let dst = dir.join(name);
    if !dst.exists() || std::fs::read(&dst).ok().as_deref() != Some(&bytes[..]) {
        std::fs::write(&dst, &bytes).map_err(|e| e.to_string())?;
    }
    Ok(dst.to_string_lossy().to_string())
}

/// A readable name from a font file name: "NotoSans-BoldItalic" → "Noto Sans Bold Italic".
pub fn font_name(path: &str) -> String {
    if path.is_empty() {
        return "Adwaita Sans (built-in)".into();
    }
    let stem = Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in stem.chars() {
        let c = if c == '-' || c == '_' { ' ' } else { c };
        if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase()) {
            out.push(' ');
        }
        if !(c == ' ' && out.ends_with(' ')) {
            out.push(c);
        }
        prev = Some(c);
    }
    out.trim().to_string()
}

#[derive(Clone, Debug)]
pub struct FontChoice {
    pub name: String,
    pub path: String,
    /// 0 built-in, 1 added in Still, 2 installed on the system.
    pub source: u8,
}

fn font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 || out.len() > 6000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            font_files(&p, depth + 1, out);
        } else if p.extension().map(|x| FONT_EXTS.iter().any(|f| x.eq_ignore_ascii_case(f))).unwrap_or(false) {
            out.push(p);
        }
    }
}

fn system_font_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut v = Vec::new();
    if cfg!(windows) {
        v.push(env("WINDIR").unwrap_or_else(|| PathBuf::from("C:\\Windows")).join("Fonts"));
        if let Some(l) = env("LOCALAPPDATA") {
            v.push(l.join("Microsoft\\Windows\\Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        v.push("/System/Library/Fonts".into());
        v.push("/Library/Fonts".into());
        if let Some(h) = env("HOME") {
            v.push(h.join("Library/Fonts"));
        }
    } else {
        v.push("/usr/share/fonts".into());
        v.push("/usr/local/share/fonts".into());
        if let Some(h) = env("HOME") {
            v.push(h.join(".local/share/fonts"));
            v.push(h.join(".fonts"));
        }
        if let Some(d) = env("XDG_DATA_HOME") {
            v.push(d.join("fonts"));
        }
    }
    v
}

static SYSTEM_FONTS: OnceLock<Vec<FontChoice>> = OnceLock::new();

/// Every font to choose from: the built-in one, those added in Still, and
/// the ones installed on the computer (found once, by file name).
pub fn font_choices() -> Vec<FontChoice> {
    let mut v = vec![FontChoice { name: font_name(""), path: String::new(), source: 0 }];
    let mut user = Vec::new();
    font_files(&user_font_dir(), 0, &mut user);
    let mut user: Vec<FontChoice> = user.into_iter().map(|p| p.to_string_lossy().to_string()).map(|p| FontChoice { name: font_name(&p), path: p, source: 1 }).collect();
    user.sort_by_key(|a| a.name.to_lowercase());
    v.extend(user);
    let system = SYSTEM_FONTS.get_or_init(|| {
        let mut files = Vec::new();
        for d in system_font_dirs() {
            font_files(&d, 0, &mut files);
        }
        let mut seen = std::collections::HashSet::new();
        let mut list: Vec<FontChoice> = files
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .map(|p| FontChoice { name: font_name(&p), path: p, source: 2 })
            .filter(|c| seen.insert(c.name.to_lowercase()))
            .collect();
        list.sort_by_key(|a| a.name.to_lowercase());
        list
    });
    v.extend(system.iter().cloned());
    v
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

struct Placed {
    /// 0 the chosen font, 1 the fallback.
    font: u8,
    id: GlyphId,
    /// Left edge of the glyph's advance, along its line.
    x: f32,
    adv: f32,
    line: usize,
}

struct Layout {
    glyphs: Vec<Placed>,
    line_w: Vec<f32>,
    width: f32,
    ascent: f32,
    descent: f32,
    line_h: f32,
}

fn layout(spec: &TextSpec, fonts: [&FontVec; 2], em: f32) -> Layout {
    let scale = PxScale::from(em);
    let sf = [fonts[0].as_scaled(scale), fonts[1].as_scaled(scale)];
    let (ascent, descent, gap) = (sf[0].ascent(), sf[0].descent(), sf[0].line_gap());
    let line_h = (ascent - descent + gap) * spec.leading;
    let track = spec.tracking * em;
    let mut glyphs = Vec::new();
    let mut line_w = Vec::new();
    for (line, text) in spec.text.split('\n').enumerate() {
        let mut x = 0.0f32;
        let mut prev: Option<(u8, GlyphId)> = None;
        let mut any = false;
        for ch in text.chars().filter(|c| !c.is_control()) {
            let mut f = 0u8;
            let mut id = fonts[0].glyph_id(ch);
            if id.0 == 0 && !ch.is_whitespace() {
                let alt = fonts[1].glyph_id(ch);
                if alt.0 != 0 {
                    (f, id) = (1, alt);
                }
            }
            if let Some((pf, pid)) = prev
                && pf == f
            {
                x += sf[f as usize].kern(pid, id);
            }
            let adv = sf[f as usize].h_advance(id);
            glyphs.push(Placed { font: f, id, x, adv, line });
            x += adv + track;
            prev = Some((f, id));
            any = true;
        }
        line_w.push(if any { (x - track).max(0.0) } else { 0.0 });
    }
    let width = line_w.iter().copied().fold(0.0, f32::max);
    Layout { glyphs, line_w, width, ascent, descent, line_h }
}

/// Where a glyph's baseline centre lands and how much it turns, by the
/// spec's shape. Relative to the block's horizontal centre and its top.
fn place(spec: &TextSpec, lay: &Layout, g: &Placed, em: f32) -> ([f32; 2], f32) {
    let lw = lay.line_w.get(g.line).copied().unwrap_or(0.0);
    let off = match spec.align {
        0 => 0.0,
        1 => (lay.width - lw) * 0.5,
        _ => lay.width - lw,
    };
    let x = g.x + off + g.adv * 0.5 - lay.width * 0.5;
    let by = lay.ascent + g.line as f32 * lay.line_h;
    let lines = lay.line_w.len().max(1) as f32;
    let width = lay.width.max(em);
    match spec.shape {
        1 => {
            let theta = (spec.bend * std::f32::consts::PI).clamp(-std::f32::consts::TAU, std::f32::consts::TAU);
            if theta.abs() < 1e-3 {
                return ([x, by], 0.0);
            }
            let r = width / theta;
            let y_mid = lay.ascent + (lines - 1.0) * lay.line_h * 0.5;
            let yc = y_mid + r;
            let rho = yc - by;
            let phi = x / r;
            ([rho * phi.sin(), yc - rho * phi.cos()], phi)
        }
        2 => {
            let amp = spec.bend * em * 0.5;
            let k = std::f32::consts::TAU * spec.waves / width;
            let y = by - amp * (k * x).sin();
            let slope = -amp * k * (k * x).cos();
            ([x, y], slope.atan())
        }
        _ => ([x, by], 0.0),
    }
}

/// A glyph's coverage, rasterised upright at the origin (baseline-left).
struct Bitmap {
    x0: f32,
    y0: f32,
    w: usize,
    h: usize,
    cov: Vec<f32>,
}

impl Bitmap {
    fn sample(&self, x: f32, y: f32) -> f32 {
        let (x, y) = (x - self.x0 - 0.5, y - self.y0 - 0.5);
        if x < -1.0 || y < -1.0 || x > self.w as f32 || y > self.h as f32 {
            return 0.0;
        }
        let (fx, fy) = (x.floor(), y.floor());
        let (tx, ty) = (x - fx, y - fy);
        let (ix, iy) = (fx as isize, fy as isize);
        let at = |a: isize, b: isize| {
            if a < 0 || b < 0 || a >= self.w as isize || b >= self.h as isize { 0.0 } else { self.cov[b as usize * self.w + a as usize] }
        };
        let top = at(ix, iy) + (at(ix + 1, iy) - at(ix, iy)) * tx;
        let bot = at(ix, iy + 1) + (at(ix + 1, iy + 1) - at(ix, iy + 1)) * tx;
        top + (bot - top) * ty
    }
}

/// A glyph placed in the layout: its bitmap and the transform onto the page.
struct Shaped {
    bmp: Bitmap,
    pos: [f32; 2],
    angle: f32,
    /// The glyph's advance centre, the point it turns about.
    anchor: f32,
}

impl Shaped {
    /// Glyph-space point (baseline-left origin) → layout space.
    fn forward(&self, gx: f32, gy: f32, shear: f32) -> [f32; 2] {
        let lx = gx - shear * gy - self.anchor;
        let (s, c) = self.angle.sin_cos();
        [self.pos[0] + lx * c - gy * s, self.pos[1] + lx * s + gy * c]
    }
    fn backward(&self, x: f32, y: f32, shear: f32) -> [f32; 2] {
        let (dx, dy) = (x - self.pos[0], y - self.pos[1]);
        let (s, c) = self.angle.sin_cos();
        let (lx, gy) = (dx * c + dy * s, -dx * s + dy * c);
        [lx + self.anchor + shear * gy, gy]
    }
    fn bounds(&self, shear: f32) -> [f32; 4] {
        let b = &self.bmp;
        let (x1, y1) = (b.x0 + b.w as f32, b.y0 + b.h as f32);
        let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (x, y) in [(b.x0, b.y0), (x1, b.y0), (x1, y1), (b.x0, y1)] {
            let p = self.forward(x, y, shear);
            r = [r[0].min(p[0]), r[1].min(p[1]), r[2].max(p[0]), r[3].max(p[1])];
        }
        r
    }
}

fn shape_glyphs(spec: &TextSpec, fonts: [&FontVec; 2], em: f32, raster: bool) -> (Layout, Vec<Shaped>) {
    let lay = layout(spec, fonts, em);
    let scale = PxScale::from(em);
    let shaped: Vec<Shaped> = lay
        .glyphs
        .par_iter()
        .filter_map(|g| {
            let font = fonts[g.font as usize];
            let og = font.outline_glyph(g.id.with_scale_and_position(scale, point(0.0, 0.0)))?;
            let b = og.px_bounds();
            let (w, h) = ((b.width().ceil() as usize).max(1), (b.height().ceil() as usize).max(1));
            let mut cov = Vec::new();
            if raster {
                cov = vec![0.0f32; w * h];
                og.draw(|x, y, c| {
                    let (x, y) = (x as usize, y as usize);
                    if x < w && y < h {
                        cov[y * w + x] = c.clamp(0.0, 1.0);
                    }
                });
            }
            let (pos, angle) = place(spec, &lay, g, em);
            Some(Shaped { bmp: Bitmap { x0: b.min.x, y0: b.min.y, w, h, cov }, pos, angle, anchor: g.adv * 0.5 })
        })
        .collect();
    (lay, shaped)
}

/// Extra room around the letters for the outline, emboldening and shadow, in em.
fn pad_em(spec: &TextSpec, faux: f32) -> f32 {
    let shadow = if spec.shadow > 0.001 { spec.shadow_distance + spec.shadow_blur * 3.0 } else { 0.0 };
    spec.outline + faux + shadow + 0.04
}

/// Faux bold (in em) for fonts that can't take the weight themselves.
fn faux_bold(spec: &TextSpec, real: bool) -> f32 {
    if real { 0.0 } else { ((spec.weight - 450.0) / 450.0).clamp(0.0, 1.0) * 0.045 }
}

/// Ink bounds of the shaped glyphs (or an empty line's box), in layout pixels.
fn ink_bounds(spec: &TextSpec, lay: &Layout, shaped: &[Shaped], em: f32) -> [f32; 4] {
    let shear = spec.italic * 0.25;
    let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for g in shaped {
        let b = g.bounds(shear);
        r = [r[0].min(b[0]), r[1].min(b[1]), r[2].max(b[2]), r[3].max(b[3])];
    }
    if r[0] > r[2] {
        // nothing visible (empty or only spaces): a box the size of the line
        let w = lay.width.max(em * 0.5);
        r = [-w * 0.5, 0.0, w * 0.5, lay.ascent - lay.descent];
    }
    r
}

/// Width and height of the rendered text, in em. The layer's proportions.
pub fn measure(spec: &TextSpec) -> (f32, f32) {
    let spec = spec.sane();
    let (a, b, real) = fonts(&spec);
    let em = 100.0;
    let (lay, shaped) = shape_glyphs(&spec, [&a, &b], em, false);
    let r = ink_bounds(&spec, &lay, &shaped, em);
    let pad = pad_em(&spec, faux_bold(&spec, real)) * em;
    (((r[2] - r[0]) + 2.0 * pad) / em, ((r[3] - r[1]) + 2.0 * pad) / em)
}

/// Width over height of the rendered text.
#[cfg(test)]
pub fn aspect(spec: &TextSpec) -> f32 {
    let (w, h) = measure(spec);
    (w / h.max(1e-3)).clamp(1e-3, 1e3)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The long edge the text is rendered at, before limits on the font size.
const TARGET_EDGE: f32 = 2400.0;
const MAX_EDGE: f32 = 3200.0;

/// Squared distance transform (Felzenszwalb & Huttenlocher) of one line.
fn edt_1d(f: &[f64], d: &mut [f64]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut v = vec![0usize; n];
    let mut z = vec![0f64; n + 1];
    let mut k = 0usize;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    let sect = |q: usize, p: usize| ((f[q] + (q * q) as f64) - (f[p] + (p * p) as f64)) / (2.0 * q as f64 - 2.0 * p as f64);
    for q in 1..n {
        let mut s = sect(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = sect(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let p = v[k];
        *out = (q as f64 - p as f64).powi(2) + f[p];
    }
}

/// Distance (pixels) from each pixel to the nearest one at least half covered.
fn distance_outside(cov: &[f32], w: usize, h: usize) -> Vec<f32> {
    const FAR: f64 = 1e12;
    let mut g: Vec<f64> = cov.iter().map(|&c| if c >= 0.5 { 0.0 } else { FAR }).collect();
    // columns
    let cols: Vec<Vec<f64>> = (0..w)
        .into_par_iter()
        .map(|x| {
            let f: Vec<f64> = (0..h).map(|y| g[y * w + x]).collect();
            let mut d = vec![0.0; h];
            edt_1d(&f, &mut d);
            d
        })
        .collect();
    for (x, col) in cols.iter().enumerate() {
        for y in 0..h {
            g[y * w + x] = col[y];
        }
    }
    // rows
    g.par_chunks_mut(w).for_each(|row| {
        let f = row.to_vec();
        edt_1d(&f, row);
    });
    g.into_iter().map(|d| d.sqrt() as f32).collect()
}

/// Three box passes each way: close to a gaussian of `sigma`.
fn blur_alpha(a: &mut [f32], w: usize, h: usize, sigma: f32) {
    if sigma < 0.3 || w == 0 || h == 0 {
        return;
    }
    let r = ((sigma * sigma + 1.0).sqrt() * 0.95).round().max(1.0) as usize;
    let pass = |line: &mut [f32], r: usize| {
        let n = line.len();
        let src = line.to_vec();
        let at = |i: isize| src[i.clamp(0, n as isize - 1) as usize];
        let norm = 1.0 / (2 * r + 1) as f32;
        let mut acc: f32 = (-(r as isize)..=(r as isize)).map(at).sum();
        for (i, o) in line.iter_mut().enumerate() {
            *o = acc * norm;
            acc += at(i as isize + r as isize + 1) - at(i as isize - r as isize);
        }
    };
    for _ in 0..3 {
        a.par_chunks_mut(w).for_each(|row| pass(row, r));
    }
    let mut t = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            t[x * h + y] = a[y * w + x];
        }
    }
    for _ in 0..3 {
        t.par_chunks_mut(h).for_each(|col| pass(col, r));
    }
    for y in 0..h {
        for x in 0..w {
            a[y * w + x] = t[x * h + y];
        }
    }
}

fn rgba(c: [u8; 4]) -> [f32; 4] {
    c.map(|v| v as f32 / 255.0)
}

/// Render the text: transparent background, straight (not premultiplied) RGBA.
pub fn render(spec: &TextSpec) -> image::RgbaImage {
    let spec = spec.sane();
    let (fa, fb, real) = fonts(&spec);
    let faux = faux_bold(&spec, real);
    // the font size that puts the long edge near the target
    let (mw, mh) = measure(&spec);
    let long = mw.max(mh).max(1e-3);
    let mut em = (TARGET_EDGE / long).clamp(24.0, 420.0);
    if long * em > MAX_EDGE {
        em = MAX_EDGE / long;
    }
    let em = em.max(2.0);
    let (lay, shaped) = shape_glyphs(&spec, [&fa, &fb], em, true);
    let ink = ink_bounds(&spec, &lay, &shaped, em);
    let pad = pad_em(&spec, faux) * em;
    let w = ((ink[2] - ink[0] + 2.0 * pad).round() as usize).clamp(1, MAX_EDGE as usize);
    let h = ((ink[3] - ink[1] + 2.0 * pad).round() as usize).clamp(1, MAX_EDGE as usize);
    let (ox, oy) = (pad - ink[0], pad - ink[1]);
    let shear = spec.italic * 0.25;

    // letters, each drawn through its own transform, merged by coverage
    let pieces: Vec<(usize, usize, usize, Vec<f32>)> = shaped
        .par_iter()
        .map(|g| {
            let b = g.bounds(shear);
            let x0 = ((b[0] + ox).floor().max(0.0) as usize).min(w);
            let y0 = ((b[1] + oy).floor().max(0.0) as usize).min(h);
            let x1 = ((b[2] + ox).ceil().max(0.0) as usize + 1).min(w);
            let y1 = ((b[3] + oy).ceil().max(0.0) as usize + 1).min(h);
            let pw = x1.saturating_sub(x0);
            let mut cov = vec![0.0f32; pw * y1.saturating_sub(y0)];
            for y in y0..y1 {
                for x in x0..x1 {
                    let q = g.backward(x as f32 + 0.5 - ox, y as f32 + 0.5 - oy, shear);
                    cov[(y - y0) * pw + (x - x0)] = g.bmp.sample(q[0], q[1]);
                }
            }
            (x0, y0, pw, cov)
        })
        .collect();
    let mut fill = vec![0.0f32; w * h];
    for (x0, y0, pw, cov) in pieces {
        if pw == 0 {
            continue;
        }
        for (r, row) in cov.chunks(pw).enumerate() {
            let line = &mut fill[(y0 + r) * w + x0..(y0 + r) * w + x0 + pw];
            for (o, &c) in line.iter_mut().zip(row) {
                *o = o.max(c);
            }
        }
    }

    // faux bold and the outline grow out from the letters
    let need_dist = faux > 0.0 || spec.outline > 0.0;
    let dist = if need_dist { distance_outside(&fill, w, h) } else { Vec::new() };
    if faux > 0.0 {
        let r = faux * em;
        fill.par_iter_mut().zip(dist.par_iter()).for_each(|(f, &d)| *f = f.max((r + 0.5 - d).clamp(0.0, 1.0)));
    }
    let outline: Option<Vec<f32>> = (spec.outline > 0.0).then(|| {
        let r = (spec.outline + faux) * em;
        fill.par_iter().zip(dist.par_iter()).map(|(&f, &d)| f.max((r + 0.5 - d).clamp(0.0, 1.0))).collect()
    });

    // drop shadow: the whole shape, blurred and moved
    let shadow: Option<Vec<f32>> = (spec.shadow > 0.001).then(|| {
        let mut s = outline.clone().unwrap_or_else(|| fill.clone());
        blur_alpha(&mut s, w, h, spec.shadow_blur * em);
        let a = spec.shadow_angle.to_radians();
        let (dx, dy) = (a.cos() * spec.shadow_distance * em, a.sin() * spec.shadow_distance * em);
        let mut out = vec![0.0f32; w * h];
        out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for (x, o) in row.iter_mut().enumerate() {
                let (sx, sy) = (x as f32 - dx, y as f32 - dy);
                let (fx, fy) = (sx.floor(), sy.floor());
                let (tx, ty) = (sx - fx, sy - fy);
                let at = |a: f32, b: f32| {
                    if a < 0.0 || b < 0.0 || a >= w as f32 || b >= h as f32 { 0.0 } else { s[b as usize * w + a as usize] }
                };
                let top = at(fx, fy) + (at(fx + 1.0, fy) - at(fx, fy)) * tx;
                let bot = at(fx, fy + 1.0) + (at(fx + 1.0, fy + 1.0) - at(fx, fy + 1.0)) * tx;
                *o = top + (bot - top) * ty;
            }
        });
        out
    });

    // colour: shadow, then outline, then the fill (with its gradient)
    let (cf, cf2, co, cs) = (rgba(spec.fill), rgba(spec.fill2), rgba(spec.outline_color), rgba(spec.shadow_color));
    let ga = spec.gradient_angle.to_radians();
    let gdir = [ga.cos(), ga.sin()];
    let (iw, ih) = (ink[2] - ink[0], ink[3] - ink[1]);
    let extent = (iw * gdir[0].abs() + ih * gdir[1].abs()).max(1.0);
    let (icx, icy) = ((ink[0] + ink[2]) * 0.5 + ox, (ink[1] + ink[3]) * 0.5 + oy);
    let mut out = image::RgbaImage::new(w as u32, h as u32);
    let raw: &mut [u8] = &mut out;
    raw.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let i = y * w + x;
            let mut acc = [0.0f32; 4]; // premultiplied
            let mut over = |c: [f32; 3], a: f32| {
                if a <= 0.0 {
                    return;
                }
                for k in 0..3 {
                    acc[k] = c[k] * a + acc[k] * (1.0 - a);
                }
                acc[3] = a + acc[3] * (1.0 - a);
            };
            if let Some(s) = &shadow {
                over([cs[0], cs[1], cs[2]], s[i] * spec.shadow * cs[3]);
            }
            if let Some(o) = &outline {
                over([co[0], co[1], co[2]], o[i] * co[3]);
            }
            let f = fill[i];
            if f > 0.0 {
                let (col, a) = if spec.gradient {
                    let t = (((x as f32 + 0.5 - icx) * gdir[0] + (y as f32 + 0.5 - icy) * gdir[1]) / extent + 0.5).clamp(0.0, 1.0);
                    let m = |k: usize| cf[k] + (cf2[k] - cf[k]) * t;
                    ([m(0), m(1), m(2)], m(3))
                } else {
                    ([cf[0], cf[1], cf[2]], cf[3])
                };
                over(col, f * a);
            }
            let px = &mut row[x * 4..x * 4 + 4];
            if acc[3] > 1e-5 {
                for k in 0..3 {
                    px[k] = ((acc[k] / acc[3]).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
                px[3] = (acc[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
    });
    out
}

/// Recently rendered texts, oldest first, so edits don't pile up in memory.
static RECENT: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));
const KEEP: usize = 64;

/// Make sure the text's pixels are in the layer store; returns their key.
pub fn ensure(spec: &TextSpec) -> Option<String> {
    let key = spec.key();
    if !crate::layers::has(&key) {
        let img = render(spec);
        crate::layers::register(&key, &image::DynamicImage::ImageRgba8(img));
    }
    if let Ok(mut r) = RECENT.lock() {
        r.retain(|k| *k != key);
        r.push_back(key.clone());
        while r.len() > KEEP {
            if let Some(old) = r.pop_front() {
                crate::layers::forget(&old);
            }
        }
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(img: &image::RgbaImage) -> usize {
        img.pixels().filter(|p| p.0[3] > 128).count()
    }

    #[test]
    fn renders_text_with_the_measured_proportions() {
        let spec = TextSpec { text: "Still".into(), ..Default::default() };
        let img = render(&spec);
        assert!(ink(&img) > 1000, "letters were drawn");
        let a = aspect(&spec);
        let got = img.width() as f32 / img.height() as f32;
        assert!((a - got).abs() / a < 0.02, "aspect {a} vs rendered {got}");
        // two lines are taller than one
        let two = TextSpec { text: "Still\nLife".into(), ..Default::default() };
        assert!(measure(&two).1 > measure(&spec).1 * 1.6);
    }

    #[test]
    fn outline_shadow_and_weight_add_ink() {
        let base = TextSpec { text: "Hi".into(), weight: 400.0, ..Default::default() };
        let thin = ink(&render(&base));
        let bold = ink(&render(&TextSpec { weight: 900.0, ..base.clone() }));
        assert!(bold > thin, "heavier weight {bold} > {thin}");
        let outlined = render(&TextSpec { outline: 0.08, outline_color: [255, 0, 0, 255], ..base.clone() });
        assert!(outlined.pixels().any(|p| p.0[0] > 200 && p.0[1] < 40 && p.0[3] > 200), "red outline present");
        let shadowed = render(&TextSpec { shadow: 1.0, shadow_color: [0, 0, 255, 255], ..base });
        assert!(shadowed.pixels().any(|p| p.0[2] > 200 && p.0[0] < 40 && p.0[3] > 100), "blue shadow present");
    }

    #[test]
    fn arcs_and_waves_change_the_shape() {
        let flat = TextSpec { text: "A long line of text".into(), ..Default::default() };
        let arc = TextSpec { shape: 1, bend: 0.8, ..flat.clone() };
        let wave = TextSpec { shape: 2, bend: 1.0, waves: 2.0, ..flat.clone() };
        let (fw, fh) = measure(&flat);
        let (aw, ah) = measure(&arc);
        assert!(ah > fh * 1.5 && aw < fw, "an arc is taller and narrower: {aw}x{ah} vs {fw}x{fh}");
        assert!(measure(&wave).1 > fh * 1.2);
        assert!(ink(&render(&arc)) > 1000);
    }

    #[test]
    fn empty_and_odd_input_render_safely() {
        for t in ["", " ", "\n\n", "\u{1F600}", "ěščřžýáíé"] {
            let s = TextSpec { text: t.into(), ..Default::default() };
            let img = render(&s);
            assert!(img.width() >= 1 && img.height() >= 1);
        }
        let wild = TextSpec { tracking: 1e9, leading: -5.0, outline: 50.0, bend: f32::NAN, ..Default::default() };
        let img = render(&wild);
        assert!(img.width() <= MAX_EDGE as u32 && img.height() <= MAX_EDGE as u32);
    }

    #[test]
    fn font_names_read_well() {
        assert_eq!(font_name("/x/NotoSans-BoldItalic.ttf"), "Noto Sans Bold Italic");
        assert_eq!(font_name("/x/my_font.otf"), "my font");
    }
}
