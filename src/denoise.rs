//! Noise reduction: multi-scale DCT denoising.
//!
//! After Pierazzo, Morel & Facciolo, "Multi-Scale DCT Denoising" (IPOL 2017),
//! which builds on Yu & Sapiro, "DCT image denoising" (IPOL 2011):
//!
//! - The photo is split into brightness and two colour-difference channels by
//!   an orthonormal transform, so noise keeps its size in each of them.
//! - Each 8×8 patch goes into the DCT domain, where noise spreads thinly over
//!   every coefficient while the picture gathers in a few. Coefficients under
//!   3σ are dropped. Overlapping patches are averaged, the ones that came out
//!   simplest counting most, which keeps ringing away from edges.
//! - A second pass uses that result as a guide for a gentler Wiener filter,
//!   coefficient by coefficient.
//! - All of it runs on a pyramid of halved copies. A patch can't see a blotch
//!   bigger than itself; a coarser copy can, so each coarser result cleans the
//!   lowest frequencies of the finer one. Only the lowest: what the coarse
//!   filter did near its own finest scale would come back as ringing.
//!
//! The paper is told how strong the noise is, and takes it to be white. A
//! photo doesn't say, and its noise isn't white, so both are measured on the
//! photo itself, at every scale, channel and brightness: camera noise is
//! stronger in the shadows; demosaicing spreads it over neighbouring pixels,
//! so it is coarser than white noise; and a camera JPEG has often smoothed
//! its finest grain away already. Thresholds follow the noise's spectrum,
//! frequency by frequency, as in the Noise Clinic (Lebrun, Colom & Morel,
//! "Multiscale Image Blind Denoising", 2015). (darktable reaches a similar end
//! with noise profiles measured per camera and ISO.)
//!
//! Colour gets one more step: smoothing guided by brightness (He, Sun & Tang,
//! "Guided Image Filtering", 2010), for the broad blotches that are too smooth
//! for any threshold to tell from detail.

use crate::pipeline::Img;
use rayon::prelude::*;

/// Patch size.
const N: usize = 8;
/// Patches start every other pixel each way, in both passes. The paper slides
/// the final pass's by one; that measures 0.1 to 0.2 dB better, doesn't show,
/// and takes about three times as long.
const STEP_GUIDE: usize = 2;
const STEP_FINAL: usize = 2;
/// Brightness bins of the noise measurement.
const BINS: usize = 8;
/// How much of a coarser result is kept when it's put back into the finer
/// one: the blur (in coarse pixels) that leaves only its lowest frequencies.
const RECOMPOSE_BLUR: f32 = 1.0;

/// The amounts on the sliders, 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Amounts {
    /// Luminance noise.
    pub luma: f32,
    /// 0 smooths fine texture away, 1 keeps it (and its grain); 0.5 is neutral.
    pub detail: f32,
    /// Colour noise.
    pub color: f32,
}

impl Amounts {
    pub fn any(&self) -> bool {
        self.luma > 0.001 || self.color > 0.001
    }
}

/// Clean up `img` (sRGB-encoded, as the pipeline gets it from the file).
pub fn denoise(img: &Img, a: Amounts) -> Img {
    if !a.any() || img.w < 2 * N || img.h < 2 * N {
        return img.clone();
    }
    let luma = 2.0 * a.luma.clamp(0.0, 4.0);
    let color = 2.5 * a.color.clamp(0.0, 4.0);
    // Detail scales the noise level of the fine scales: 0 → ×2, 1 → ×½. Fine
    // means up to ~2/1000 of the long edge, so the preview and the full-size
    // export agree, and always at least the finest scale at hand.
    let detail = 2f32.powf((0.5 - a.detail.clamp(0.0, 1.0)) * 2.0);
    let fine = 1.0 + (img.w.max(img.h) as f32 / 1000.0).log2();

    let levels = level_count(img.w, img.h);
    let mut pyr = vec![split(img)];
    let mut clips = vec![clipped(img)];
    for l in 1..levels {
        let next = half(&pyr[l - 1]);
        clips.push(half_clip(&clips[l - 1], pyr[l - 1].w, pyr[l - 1].h));
        pyr.push(next);
    }
    let noises = measure_all(&pyr, &clips);

    // Colour also gets smoothed over a radius that grows with it, at the
    // scale where that radius comes to a few pixels (see `smooth_colour`).
    let radius = a.color.clamp(0.0, 4.0) * COLOR_RADIUS * img.w.max(img.h) as f32 / 1000.0;
    let smooth_at = (radius >= 2.0).then(|| ((radius / 2.0).log2().floor() as usize).min(levels - 1));

    let mut combined: Option<Planes> = None;
    for l in (0..levels).rev() {
        let reach = if l == 0 { 1.0 } else { (fine - l as f32).clamp(0.0, 1.0) };
        let noise = &noises[l];
        let mult = [luma * detail.powf(reach), color, color];
        let x = denoise_level(&pyr[l], noise, mult);
        let mut c = match combined {
            None => x,
            Some(c) => recompose(x, &c, mult),
        };
        if smooth_at == Some(l) {
            // the guide's own noise mustn't pass for edges: less of it once denoised
            let sy = noise.mean(0) * if luma > 0.0 { 0.7 } else { 2.0 };
            smooth_colour(&mut c, radius / (1 << l) as f32, GUIDE_EDGE * GUIDE_EDGE + sy * sy);
        }
        combined = Some(c);
    }
    join(&combined.expect("at least one level"))
}

/// The colour smoothing radius at full Color, in thousandths of the long edge.
const COLOR_RADIUS: f32 = 8.0;
/// The brightness step (as σ of the guide within the radius) under which a
/// colour edge counts as noise rather than an edge.
const GUIDE_EDGE: f32 = 0.025;

/// Smooth the colour channels over a box radius of `r` pixels, keeping the
/// colour edges that brightness shows: the guided filter (He, Sun & Tang,
/// 2010), brightness being the guide and `eps` how much brightness variance
/// makes an edge. It takes the colour blotches no threshold catches, which
/// are smooth enough to pass for detail: what camera JPEGs keep of their
/// noise, and what's left of a raw file's at high ISO. Lightroom's "Color
/// Smoothness" and darktable's colour-blended blurs are there for the same.
fn smooth_colour(p: &mut Planes, r: f32, eps: f32) {
    let (w, h) = (p.w, p.h);
    let [g, u, v] = &mut p.c;
    let g: &[f32] = g;
    let mean_g = box_blur(g, w, h, r);
    let var_g: Vec<f32> = {
        let gg: Vec<f32> = g.par_iter().map(|v| v * v).collect();
        box_blur(&gg, w, h, r).par_iter().zip(&mean_g).map(|(m2, m)| (m2 - m * m).max(0.0)).collect()
    };
    for c in [u, v] {
        let mean_c = box_blur(c, w, h, r);
        let gc: Vec<f32> = g.par_iter().zip(c.par_iter()).map(|(a, b)| a * b).collect();
        let mean_gc = box_blur(&gc, w, h, r);
        // per window: colour ≈ a·brightness + b
        let a: Vec<f32> = (0..w * h).into_par_iter().map(|i| (mean_gc[i] - mean_g[i] * mean_c[i]) / (var_g[i] + eps)).collect();
        let b: Vec<f32> = (0..w * h).into_par_iter().map(|i| mean_c[i] - a[i] * mean_g[i]).collect();
        let (a, b) = (box_blur(&a, w, h, r), box_blur(&b, w, h, r));
        *c = (0..w * h).into_par_iter().map(|i| a[i] * g[i] + b[i]).collect();
    }
}

/// Mean over a box of radius `r` (fractional: the end pixels count by the
/// fraction), shrunk to fit at the edges.
fn box_blur(s: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    let mut t = vec![0.0; w * h];
    t.par_chunks_mut(w).zip(s.par_chunks(w)).for_each(|(o, row)| box_line(row, o, r));
    let mut tt = transpose(&t, w, h);
    let mut o = vec![0.0; w * h];
    o.par_chunks_mut(h).zip(tt.par_chunks(h)).for_each(|(o, col)| box_line(col, o, r));
    tt.clear();
    transpose(&o, h, w)
}

fn box_line(s: &[f32], o: &mut [f32], r: f32) {
    let n = s.len();
    let ri = r.floor() as usize;
    let f = (r - ri as f32) as f64;
    let mut pre = Vec::with_capacity(n + 1);
    pre.push(0.0f64);
    for v in s {
        pre.push(pre[pre.len() - 1] + *v as f64);
    }
    for (x, o) in o.iter_mut().enumerate() {
        let (lo, hi) = (x.saturating_sub(ri), (x + ri).min(n - 1));
        let mut sum = pre[hi + 1] - pre[lo];
        let mut wt = (hi - lo + 1) as f64;
        if x > ri {
            sum += f * s[x - ri - 1] as f64;
            wt += f;
        }
        if x + ri + 1 < n {
            sum += f * s[x + ri + 1] as f64;
            wt += f;
        }
        *o = (sum / wt) as f32;
    }
}

/// A `w`×`h` plane turned to `h`×`w`.
fn transpose(s: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut o = vec![0.0; w * h];
    o.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        for (y, v) in col.iter_mut().enumerate() {
            *v = s[y * w + x];
        }
    });
    o
}

/// Scales of the pyramid: halve while the long edge stays at 256 or more, so
/// the coarsest scale is about the same share of the picture at any size.
fn level_count(w: usize, h: usize) -> usize {
    let mut n = 1;
    while n < 5 && (w.max(h) >> n) >= 256 && (w.min(h) >> n) >= 2 * N {
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// Planes, colour split and the pyramid
// ---------------------------------------------------------------------------

/// Brightness and two colour differences, one plane each.
#[derive(Clone)]
struct Planes {
    w: usize,
    h: usize,
    c: [Vec<f32>; 3],
}

const S3: f32 = 0.577_350_26; // 1/√3
const S2: f32 = 0.707_106_77; // 1/√2
const S6: f32 = 0.408_248_3; // 1/√6

/// Orthonormal colour transform (Yu & Sapiro): white noise of σ in R, G and B
/// is white noise of σ in each of these.
fn split(img: &Img) -> Planes {
    let n = img.w * img.h;
    let (mut y, mut u, mut v) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    y.par_iter_mut().zip(u.par_iter_mut()).zip(v.par_iter_mut()).zip(img.px.par_iter()).for_each(|(((y, u), v), p)| {
        let [r, g, b] = *p;
        *y = (r + g + b) * S3;
        *u = (r - b) * S2;
        *v = (r - 2.0 * g + b) * S6;
    });
    Planes { w: img.w, h: img.h, c: [y, u, v] }
}

fn join(p: &Planes) -> Img {
    let mut img = Img::new(p.w, p.h);
    img.px.par_iter_mut().enumerate().for_each(|(i, px)| {
        let (y, u, v) = (p.c[0][i] * S3, p.c[1][i] * S2, p.c[2][i] * S6);
        *px = [(y + u + v).max(0.0), (y - 2.0 * v).max(0.0), (y - u + v).max(0.0)];
    });
    img
}

/// How much of each pixel is clipped (a channel at either end, where the
/// noise is cut off), 0 to 255: all or nothing at full size, then the share of
/// the area under each coarser pixel.
fn clipped(img: &Img) -> Vec<u8> {
    img.px.par_iter().map(|p| if p.iter().any(|&v| !(0.002..=0.998).contains(&v)) { 255 } else { 0 }).collect()
}

/// Half size, each pixel the mean of four. Unlike a smoother filter this keeps
/// white noise white, at half the σ, so every scale is denoised alike.
fn half(p: &Planes) -> Planes {
    let (w, h) = (p.w / 2, p.h / 2);
    let c = std::array::from_fn(|ch| {
        let s = &p.c[ch];
        let mut o = vec![0.0; w * h];
        o.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            let (a, b) = (&s[2 * y * p.w..], &s[(2 * y + 1) * p.w..]);
            for (x, o) in row.iter_mut().enumerate() {
                *o = (a[2 * x] + a[2 * x + 1] + b[2 * x] + b[2 * x + 1]) * 0.25;
            }
        });
        o
    });
    Planes { w, h, c }
}

fn half_clip(m: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (hw, hh) = (w / 2, h / 2);
    (0..hw * hh)
        .into_par_iter()
        .map(|i| {
            let (x, y) = (2 * (i % hw), 2 * (i / hw));
            let s = [m[y * w + x], m[y * w + x + 1], m[(y + 1) * w + x], m[(y + 1) * w + x + 1]].map(u32::from);
            ((s.iter().sum::<u32>() + 2) / 4) as u8
        })
        .collect()
}

/// Bilinear double size, `w`×`h` (the half was rounded down).
fn upsample(s: &[f32], sw: usize, sh: usize, w: usize, h: usize) -> Vec<f32> {
    let mut o = vec![0.0; w * h];
    let tap = |i: usize, n: usize| -> (usize, usize, f32) {
        // fine pixel centre i + ½ sits at (i + ½)/2 − ½ in coarse pixels
        let c = (i as f32 + 0.5) * 0.5 - 0.5;
        let f = c.floor();
        let a = (f.max(0.0) as usize).min(n - 1);
        let b = ((f + 1.0).max(0.0) as usize).min(n - 1);
        (a, b, c - f)
    };
    let xs: Vec<_> = (0..w).map(|x| tap(x, sw)).collect();
    o.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (y0, y1, fy) = tap(y, sh);
        let (r0, r1) = (&s[y0 * sw..(y0 + 1) * sw], &s[y1 * sw..(y1 + 1) * sw]);
        for (o, &(x0, x1, fx)) in row.iter_mut().zip(&xs) {
            let top = r0[x0] + (r0[x1] - r0[x0]) * fx;
            let bot = r1[x0] + (r1[x1] - r1[x0]) * fx;
            *o = top + (bot - top) * fy;
        }
    });
    o
}

/// Gaussian blur of one plane (small sigmas).
fn blur_plane(s: &mut [f32], w: usize, h: usize, sigma: f32) {
    let r = (sigma * 3.0).ceil() as isize;
    let k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let norm: f32 = k.iter().sum();
    let k: Vec<f32> = k.iter().map(|v| v / norm).collect();
    let mut t = vec![0.0; w * h];
    t.par_chunks_mut(w).zip(s.par_chunks(w)).for_each(|(o, row)| {
        for (x, o) in o.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let xx = (x as isize + j as isize - r).clamp(0, w as isize - 1) as usize;
                acc += row[xx] * kv;
            }
            *o = acc;
        }
    });
    s.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        o.fill(0.0);
        for (j, kv) in k.iter().enumerate() {
            let yy = (y as isize + j as isize - r).clamp(0, h as isize - 1) as usize;
            for (o, v) in o.iter_mut().zip(&t[yy * w..(yy + 1) * w]) {
                *o += v * kv;
            }
        }
    });
}

/// Put the coarser result `c` back into `x` (one scale finer): where the
/// coarse filter found more noise in the low frequencies than the fine one,
/// take its word for it, for the lowest frequencies only.
fn recompose(mut x: Planes, c: &Planes, mult: [f32; 3]) -> Planes {
    let down = half(&x);
    for ((xc, (cc, dc)), m) in x.c.iter_mut().zip(c.c.iter().zip(&down.c)).zip(mult) {
        if m <= 0.0 {
            continue;
        }
        let mut d: Vec<f32> = cc.iter().zip(dc).map(|(a, b)| a - b).collect();
        blur_plane(&mut d, c.w, c.h, RECOMPOSE_BLUR);
        let up = upsample(&d, c.w, c.h, x.w, x.h);
        xc.par_iter_mut().zip(up.par_iter()).for_each(|(v, u)| *v += u);
    }
    x
}

// ---------------------------------------------------------------------------
// Measuring the noise
// ---------------------------------------------------------------------------

/// The noise of one scale: σ of each channel by brightness (bin centres at
/// (i + ½)/BINS), as the finest detail shows it; and its spectrum, how strong
/// it is at each DCT frequency against that, laid out as [`dct`] leaves the
/// coefficients. The spectrum is flat (all 1) for white noise; demosaicing,
/// JPEG and in-camera noise reduction tilt it towards the lower frequencies,
/// where a flat threshold would then let the noise through as blotches.
#[derive(Clone, Copy, Debug)]
struct Noise {
    bins: [[f32; BINS]; 3],
    shape: [Blk; 3],
}

const WHITE: Blk = [[1.0; N]; N];

/// σ by channel and brightness: the part of [`Noise`] the finest detail reads.
type Bins = [[f32; BINS]; 3];

fn bins_mean(b: &Bins, ch: usize) -> f32 {
    b[ch].iter().sum::<f32>() / BINS as f32
}

/// Measure the noise of every scale.
///
/// A scale too small or too clipped to measure follows the finer ones:
/// halved, as halving does to white noise, or by as much as the last halving
/// took off, for noise spread over neighbouring pixels.
///
/// How fast the noise falls from one scale to the next also bounds its
/// spectrum: noise that falls by r (½ when white) is about 2r as strong in a
/// scale's lower octave as its finest detail reads. Within that bound the
/// spectrum read off the picture stands (it can see more, such as noise
/// stronger across than down); past it, what it read is the picture's own
/// gradients. Where it can't be read, the bound stands in for it.
fn measure_all(pyr: &[Planes], clips: &[Vec<u8>]) -> Vec<Noise> {
    let mut read: Vec<(Bins, Option<[Blk; 3]>)> = Vec::with_capacity(pyr.len());
    for (p, clip) in pyr.iter().zip(clips) {
        let r = measure(p, clip).unwrap_or_else(|| match read.as_slice() {
            [] => ([[0.0; BINS]; 3], None),
            [(f, _)] => (f.map(|c| c.map(|v| v * 0.5)), None),
            [.., (ff, _), (f, _)] => {
                let k = (bins_mean(f, 0) / bins_mean(ff, 0).max(1e-6)).clamp(0.5, 1.0);
                (f.map(|c| c.map(|v| v * k)), None)
            }
        });
        read.push(r);
    }
    (0..read.len())
        .map(|l| {
            let bins = read[l].0;
            let shape = std::array::from_fn(|ch| {
                let fall = read.get(l + 1).map_or(0.5, |(c, _)| (bins_mean(c, ch) / bins_mean(&bins, ch).max(1e-6)).clamp(0.5, 1.0));
                let lower = 2.0 * fall;
                match &read[l].1 {
                    Some(s) => s[ch].map(|row| row.map(|v| v.clamp(SHAPE_MIN, lower * SHAPE_SLACK))),
                    None => {
                        let mut s: Blk = std::array::from_fn(|i| {
                            std::array::from_fn(|j| {
                                // the lower octave (up to 3 of 8) at `lower`, the upper at 1
                                let rho = ((i * i + j * j) as f32).sqrt();
                                1.0 + (lower - 1.0) * ((5.0 - rho) / 2.0).clamp(0.0, 1.0)
                            })
                        });
                        s[0][0] = 1.0;
                        s
                    }
                }
            });
            Noise { bins, shape }
        })
        .collect()
}

/// The least the spectrum is taken to be, and how far past the bound it may
/// read. The bound is loose: the picture's own fine texture reads as noise in
/// it, but noise spread over neighbouring pixels (as in every camera file)
/// often reads well past it. On synthetic tests, slack 1.5 costs 0.3 dB on
/// white noise and gains 3 dB on such noise against no spectrum at all.
const SHAPE_MIN: f32 = 0.7;
const SHAPE_SLACK: f32 = 1.5;

impl Noise {
    fn mean(&self, ch: usize) -> f32 {
        bins_mean(&self.bins, ch)
    }

    #[inline(always)]
    fn at(&self, ch: usize, brightness: f32) -> f32 {
        let t = (brightness * BINS as f32 - 0.5).clamp(0.0, (BINS - 1) as f32);
        let i = (t as usize).min(BINS - 2);
        let f = t - i as f32;
        let b = &self.bins[ch];
        b[i] + (b[i + 1] - b[i]) * f
    }
}

/// Measuring block, in pixels.
const BLOCK: usize = 16;
/// The 2×2 cells of a block.
const CELLS: usize = (BLOCK / 2) * (BLOCK / 2);
/// A pixel is measured while at most this share of it (of 255) is clipped:
/// a quarter takes its noise down by an eighth at most.
const CLIP_OK: u8 = 64;
/// Blocks a brightness bin needs to be measured on its own.
const MIN_BLOCKS: usize = 8;
/// Which of the blocks' readings stands for the noise: a low one, as most
/// blocks hold some texture on top of it.
const QUANTILE: f32 = 0.25;
/// What that reading comes to on pure noise, as a share of its σ.
const QUANTILE_BIAS: f32 = 0.885;

/// Measure the noise in each 16×16 block from the finest diagonal detail
/// (Haar HH, which a smooth picture hardly has), robustly (the median, so an
/// edge through the block doesn't count); then take a low reading over the
/// blocks of each brightness, since most blocks hold some texture too. Cells
/// with clipping are left out; `None` when too few blocks are left to tell.
/// Also reads the spectrum, when there's enough to read it from.
fn measure(p: &Planes, clip: &[u8]) -> Option<(Bins, Option<[Blk; 3]>)> {
    let w = p.w;
    let (bw, bh) = (p.w / BLOCK, p.h / BLOCK);
    let blocks: Vec<Block> = (0..bh)
        .into_par_iter()
        .flat_map_iter(|by| {
            (0..bw).filter_map(move |bx| {
                let (x0, y0) = (bx * BLOCK, by * BLOCK);
                let cells = || (y0..y0 + BLOCK).step_by(2).flat_map(|y| (x0..x0 + BLOCK).step_by(2).map(move |x| (x, y)));
                let ok = |(x, y): (usize, usize)| [y * w + x, y * w + x + 1, (y + 1) * w + x, (y + 1) * w + x + 1].iter().all(|&i| clip[i] <= CLIP_OK);
                let n = cells().filter(|&c| ok(c)).count();
                if n < CELLS / 2 {
                    return None;
                }
                let mut s = [0.0; 3];
                for (ch, s) in s.iter_mut().enumerate() {
                    let a = &p.c[ch];
                    let mut d = [0f32; CELLS];
                    for (d, (x, y)) in d.iter_mut().zip(cells().filter(|&c| ok(c))) {
                        *d = ((a[y * w + x] - a[y * w + x + 1] - a[(y + 1) * w + x] + a[(y + 1) * w + x + 1]) * 0.5).abs();
                    }
                    *s = *d[..n].select_nth_unstable_by((n - 1) / 2, f32::total_cmp).1 / 0.6745;
                }
                let sum: f32 = (y0..y0 + BLOCK).map(|y| p.c[0][y * w + x0..y * w + x0 + BLOCK].iter().sum::<f32>()).sum();
                Some(Block { x: x0, y: y0, m: sum / (BLOCK * BLOCK) as f32 * S3, s, whole: n == CELLS })
            })
        })
        .collect();
    if blocks.len() < MIN_BLOCKS {
        return None;
    }

    let bin_of = |m: f32| ((m.clamp(0.0, 0.9999) * BINS as f32) as usize).min(BINS - 1);
    let mut out = [[0.0; BINS]; 3];
    for (ch, out) in out.iter_mut().enumerate() {
        let mut all: Vec<f32> = blocks.iter().map(|b| b.s[ch]).collect();
        let overall = quantile(&mut all, QUANTILE) / QUANTILE_BIAS;
        let mut bins = [None; BINS];
        for (b, bin) in bins.iter_mut().enumerate() {
            let mut v: Vec<f32> = blocks.iter().filter(|s| bin_of(s.m) == b).map(|s| s.s[ch]).collect();
            if v.len() >= MIN_BLOCKS {
                *bin = Some(quantile(&mut v, QUANTILE) / QUANTILE_BIAS);
            }
        }
        // fill the gaps from the neighbours, then even out
        let filled: [f32; BINS] = std::array::from_fn(|b| {
            if let Some(v) = bins[b] {
                return v;
            }
            let lo = (0..b).rev().find_map(|i| bins[i].map(|v| (i, v)));
            let hi = (b + 1..BINS).find_map(|i| bins[i].map(|v| (i, v)));
            match (lo, hi) {
                (Some((i, a)), Some((j, c))) => a + (c - a) * (b - i) as f32 / (j - i) as f32,
                (Some((_, a)), None) => a,
                (None, Some((_, c))) => c,
                (None, None) => overall,
            }
        });
        for b in 0..BINS {
            let (l, r) = (filled[b.saturating_sub(1)], filled[(b + 1).min(BINS - 1)]);
            out[b] = (l + 2.0 * filled[b] + r) * 0.25;
        }
    }
    let shape = spectrum(p, &blocks, &Noise { bins: out, shape: [WHITE; 3] });
    Some((out, shape))
}

/// A measuring block: where it is, its brightness, the noise read in each
/// channel, and whether it's clear of clipping.
struct Block {
    x: usize,
    y: usize,
    m: f32,
    s: [f32; 3],
    whole: bool,
}

/// Blocks the spectrum needs to be read at all, and the most it reads.
const SPECTRUM_MIN: usize = 16;
const SPECTRUM_MAX: usize = 1024;

/// The noise's spectrum (see [`Noise`]), read where there's little but
/// noise: blocks clear of clipping whose every channel reads at most a little
/// over the estimate. Each 8×8 quarter's coefficients are taken against the
/// σ for its brightness; a low quantile of their squares over the blocks (the
/// lower quarter, which leaves out more of the picture's texture than the
/// median; 0.1015 for a unit normal) gives each frequency's strength.
fn spectrum(p: &Planes, blocks: &[Block], noise: &Noise) -> Option<[Blk; 3]> {
    let quiet: Vec<&Block> = blocks.iter().filter(|b| b.whole && (0..3).all(|ch| b.s[ch] <= 1.1 * noise.at(ch, b.m))).collect();
    if quiet.len() < SPECTRUM_MIN {
        return None;
    }
    let picked: Vec<&Block> = quiet.iter().copied().step_by(quiet.len().div_ceil(SPECTRUM_MAX)).collect();
    let z: Vec<[Blk; 3]> = picked
        .par_iter()
        .flat_map_iter(|b| {
            [(0, 0), (N, 0), (0, N), (N, N)].map(|(dx, dy)| {
                std::array::from_fn(|ch| {
                    let mut r = load::<[f32; N]>(&p.c[ch], p.w, b.x + dx, b.y + dy);
                    dct(&mut r);
                    let s = noise.at(ch, b.m).max(1e-6);
                    r.map(|row| row.map(|v| (v / s) * (v / s)))
                })
            })
        })
        .collect();
    Some(std::array::from_fn(|ch| {
        let mut shape = WHITE;
        let mut v = Vec::with_capacity(z.len());
        for (i, row) in shape.iter_mut().enumerate() {
            for (j, out) in row.iter_mut().enumerate() {
                v.clear();
                v.extend(z.iter().map(|t| t[ch][i][j]));
                *out = (quantile(&mut v, 0.25) / 0.1015).sqrt();
            }
        }
        shape[0][0] = 1.0;
        shape
    }))
}

/// The `q` quantile of `v` (reordering it); 0 when empty.
fn quantile(v: &mut [f32], q: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let k = ((v.len() - 1) as f32 * q) as usize;
    *v.select_nth_unstable_by(k, f32::total_cmp).1
}

// ---------------------------------------------------------------------------
// One scale: two-step DCT denoising
// ---------------------------------------------------------------------------

/// An 8×8 block, [row][column].
type Blk = [[f32; N]; N];

/// Eight floats side by side: one row of a block. Everything the transforms
/// do is written once against this, for plain Rust and for AVX2.
trait Lanes: Copy {
    fn load(r: &[f32; N]) -> Self;
    fn store(self, r: &mut [f32; N]);
    fn splat(v: f32) -> Self;
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    fn mul(self, o: Self) -> Self;
    /// self·m + a
    fn mul_add(self, m: Self, a: Self) -> Self;
    /// self·m − a
    fn mul_sub(self, m: Self, a: Self) -> Self;
    fn div(self, o: Self) -> Self;
    /// Zero the lanes under `t` in size; also returns how many lanes were kept.
    fn keep_over(self, t: Self) -> (Self, u32);
    fn sum(self) -> f32;
    fn first(self) -> f32;
    fn with_first(self, v: f32) -> Self;
    fn transpose(rows: &mut [Self; N]);
}

impl Lanes for [f32; N] {
    #[inline(always)]
    fn load(r: &[f32; N]) -> Self {
        *r
    }
    #[inline(always)]
    fn store(self, r: &mut [f32; N]) {
        *r = self
    }
    #[inline(always)]
    fn splat(v: f32) -> Self {
        [v; N]
    }
    #[inline(always)]
    fn add(self, o: Self) -> Self {
        std::array::from_fn(|i| self[i] + o[i])
    }
    #[inline(always)]
    fn sub(self, o: Self) -> Self {
        std::array::from_fn(|i| self[i] - o[i])
    }
    #[inline(always)]
    fn mul(self, o: Self) -> Self {
        std::array::from_fn(|i| self[i] * o[i])
    }
    #[inline(always)]
    fn mul_add(self, m: Self, a: Self) -> Self {
        std::array::from_fn(|i| self[i] * m[i] + a[i])
    }
    #[inline(always)]
    fn mul_sub(self, m: Self, a: Self) -> Self {
        std::array::from_fn(|i| self[i] * m[i] - a[i])
    }
    #[inline(always)]
    fn div(self, o: Self) -> Self {
        std::array::from_fn(|i| self[i] / o[i])
    }
    #[inline(always)]
    fn keep_over(self, t: Self) -> (Self, u32) {
        let mut n = 0;
        let v = std::array::from_fn(|i| {
            let keep = self[i].abs() >= t[i];
            n += keep as u32;
            if keep { self[i] } else { 0.0 }
        });
        (v, n)
    }
    #[inline(always)]
    fn sum(self) -> f32 {
        self.iter().sum()
    }
    #[inline(always)]
    fn first(self) -> f32 {
        self[0]
    }
    #[inline(always)]
    fn with_first(mut self, v: f32) -> Self {
        self[0] = v;
        self
    }
    #[inline(always)]
    fn transpose(rows: &mut [Self; N]) {
        let t = *rows;
        for (i, row) in rows.iter_mut().enumerate() {
            *row = std::array::from_fn(|j| t[j][i]);
        }
    }
}

/// AVX2 lanes. Only used inside [`strip_avx`], once the CPU is known to have
/// AVX2 and FMA; there the intrinsics below inline into plain instructions.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct Avx(std::arch::x86_64::__m256);

#[cfg(target_arch = "x86_64")]
impl Lanes for Avx {
    // SAFETY (all): reached only from `strip_avx` (see above); loads and
    // stores stay inside one 8-float row.
    #[inline(always)]
    fn load(r: &[f32; N]) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_loadu_ps(r.as_ptr())) }
    }
    #[inline(always)]
    fn store(self, r: &mut [f32; N]) {
        unsafe { std::arch::x86_64::_mm256_storeu_ps(r.as_mut_ptr(), self.0) }
    }
    #[inline(always)]
    fn splat(v: f32) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_set1_ps(v)) }
    }
    #[inline(always)]
    fn add(self, o: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_add_ps(self.0, o.0)) }
    }
    #[inline(always)]
    fn sub(self, o: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_sub_ps(self.0, o.0)) }
    }
    #[inline(always)]
    fn mul(self, o: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_mul_ps(self.0, o.0)) }
    }
    #[inline(always)]
    fn mul_add(self, m: Self, a: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_fmadd_ps(self.0, m.0, a.0)) }
    }
    #[inline(always)]
    fn mul_sub(self, m: Self, a: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_fmsub_ps(self.0, m.0, a.0)) }
    }
    #[inline(always)]
    fn div(self, o: Self) -> Self {
        unsafe { Avx(std::arch::x86_64::_mm256_div_ps(self.0, o.0)) }
    }
    #[inline(always)]
    fn keep_over(self, t: Self) -> (Self, u32) {
        use std::arch::x86_64::*;
        unsafe {
            let size = _mm256_andnot_ps(_mm256_set1_ps(-0.0), self.0);
            let keep = _mm256_cmp_ps::<_CMP_GE_OQ>(size, t.0);
            (Avx(_mm256_and_ps(self.0, keep)), _mm256_movemask_ps(keep).count_ones())
        }
    }
    #[inline(always)]
    fn sum(self) -> f32 {
        use std::arch::x86_64::*;
        unsafe {
            let h = _mm_add_ps(_mm256_castps256_ps128(self.0), _mm256_extractf128_ps::<1>(self.0));
            let h = _mm_add_ps(h, _mm_movehl_ps(h, h));
            _mm_cvtss_f32(_mm_add_ss(h, _mm_shuffle_ps::<1>(h, h)))
        }
    }
    #[inline(always)]
    fn first(self) -> f32 {
        unsafe { std::arch::x86_64::_mm256_cvtss_f32(self.0) }
    }
    #[inline(always)]
    fn with_first(self, v: f32) -> Self {
        use std::arch::x86_64::*;
        unsafe { Avx(_mm256_blend_ps::<1>(self.0, _mm256_set1_ps(v))) }
    }
    #[inline(always)]
    fn transpose(r: &mut [Self; N]) {
        use std::arch::x86_64::*;
        unsafe {
            let t0 = _mm256_unpacklo_ps(r[0].0, r[1].0);
            let t1 = _mm256_unpackhi_ps(r[0].0, r[1].0);
            let t2 = _mm256_unpacklo_ps(r[2].0, r[3].0);
            let t3 = _mm256_unpackhi_ps(r[2].0, r[3].0);
            let t4 = _mm256_unpacklo_ps(r[4].0, r[5].0);
            let t5 = _mm256_unpackhi_ps(r[4].0, r[5].0);
            let t6 = _mm256_unpacklo_ps(r[6].0, r[7].0);
            let t7 = _mm256_unpackhi_ps(r[6].0, r[7].0);
            let u0 = _mm256_shuffle_ps::<0x44>(t0, t2);
            let u1 = _mm256_shuffle_ps::<0xEE>(t0, t2);
            let u2 = _mm256_shuffle_ps::<0x44>(t1, t3);
            let u3 = _mm256_shuffle_ps::<0xEE>(t1, t3);
            let u4 = _mm256_shuffle_ps::<0x44>(t4, t6);
            let u5 = _mm256_shuffle_ps::<0xEE>(t4, t6);
            let u6 = _mm256_shuffle_ps::<0x44>(t5, t7);
            let u7 = _mm256_shuffle_ps::<0xEE>(t5, t7);
            r[0] = Avx(_mm256_permute2f128_ps::<0x20>(u0, u4));
            r[1] = Avx(_mm256_permute2f128_ps::<0x20>(u1, u5));
            r[2] = Avx(_mm256_permute2f128_ps::<0x20>(u2, u6));
            r[3] = Avx(_mm256_permute2f128_ps::<0x20>(u3, u7));
            r[4] = Avx(_mm256_permute2f128_ps::<0x31>(u0, u4));
            r[5] = Avx(_mm256_permute2f128_ps::<0x31>(u1, u5));
            r[6] = Avx(_mm256_permute2f128_ps::<0x31>(u2, u6));
            r[7] = Avx(_mm256_permute2f128_ps::<0x31>(u3, u7));
        }
    }
}

/// ½·cos(kπ/16): the orthonormal DCT's weights.
const C1: f32 = 0.490_392_64;
const C2: f32 = 0.461_939_77;
const C3: f32 = 0.415_734_8;
const C5: f32 = 0.277_785_12;
const C6: f32 = 0.191_341_72;
const C7: f32 = 0.097_545_16;
/// 1/√8, the weight of the mean (and ½·cos(4π/16)).
const R8: f32 = 0.353_553_4;

/// The 8-point DCT down each lane (rows in, frequencies out), split into its
/// even and odd halves: 36 operations instead of 64 multiply-adds.
#[inline(always)]
fn dct8<L: Lanes>(x: &mut [L; N]) {
    let (c1, c2, c3, c5, c6, c7, r8) = (L::splat(C1), L::splat(C2), L::splat(C3), L::splat(C5), L::splat(C6), L::splat(C7), L::splat(R8));
    let (s0, s1, s2, s3) = (x[0].add(x[7]), x[1].add(x[6]), x[2].add(x[5]), x[3].add(x[4]));
    let (d0, d1, d2, d3) = (x[0].sub(x[7]), x[1].sub(x[6]), x[2].sub(x[5]), x[3].sub(x[4]));
    let (t0, t1, t2, t3) = (s0.add(s3), s1.add(s2), s0.sub(s3), s1.sub(s2));
    x[0] = t0.add(t1).mul(r8);
    x[4] = t0.sub(t1).mul(r8);
    x[2] = t2.mul_add(c2, t3.mul(c6));
    x[6] = t2.mul_sub(c6, t3.mul(c2));
    x[1] = d0.mul_add(c1, d1.mul_add(c3, d2.mul_add(c5, d3.mul(c7))));
    x[3] = d0.mul_sub(c3, d1.mul_add(c7, d2.mul_add(c1, d3.mul(c5))));
    x[5] = d0.mul_add(c5, d2.mul_add(c7, d3.mul_sub(c3, d1.mul(c1))));
    x[7] = d0.mul_add(c7, d2.mul_sub(c3, d1.mul_add(c5, d3.mul(c1))));
}

/// The inverse of [`dct8`] (the odd half's matrix is its own transpose).
#[inline(always)]
fn idct8<L: Lanes>(x: &mut [L; N]) {
    let (c1, c2, c3, c5, c6, c7, r8) = (L::splat(C1), L::splat(C2), L::splat(C3), L::splat(C5), L::splat(C6), L::splat(C7), L::splat(R8));
    let t0 = x[0].add(x[4]).mul(r8);
    let t1 = x[0].sub(x[4]).mul(r8);
    let t2 = x[2].mul_add(c2, x[6].mul(c6));
    let t3 = x[2].mul_sub(c6, x[6].mul(c2));
    let (e0, e1, e2, e3) = (t0.add(t2), t1.add(t3), t1.sub(t3), t0.sub(t2));
    let (y1, y3, y5, y7) = (x[1], x[3], x[5], x[7]);
    let o0 = y1.mul_add(c1, y3.mul_add(c3, y5.mul_add(c5, y7.mul(c7))));
    let o1 = y1.mul_sub(c3, y3.mul_add(c7, y5.mul_add(c1, y7.mul(c5))));
    let o2 = y1.mul_add(c5, y5.mul_add(c7, y7.mul_sub(c3, y3.mul(c1))));
    let o3 = y1.mul_add(c7, y5.mul_sub(c3, y3.mul_add(c5, y7.mul(c1))));
    (x[0], x[7]) = (e0.add(o0), e0.sub(o0));
    (x[1], x[6]) = (e1.add(o1), e1.sub(o1));
    (x[2], x[5]) = (e2.add(o2), e2.sub(o2));
    (x[3], x[4]) = (e3.add(o3), e3.sub(o3));
}

/// The orthonormal 2D DCT of a patch, in place. The coefficients come out
/// transposed ([horizontal][vertical] frequency), which nothing here minds:
/// the mean is still first, and [`idct`] takes them as they are.
#[inline(always)]
fn dct<L: Lanes>(r: &mut [L; N]) {
    dct8(r);
    L::transpose(r);
    dct8(r);
}

#[inline(always)]
fn idct<L: Lanes>(r: &mut [L; N]) {
    idct8(r);
    L::transpose(r);
    idct8(r);
}

/// Drop the coefficients under `t` (times the noise's `shape` at each
/// frequency), all but the mean; returns how many are left.
#[inline(always)]
fn hard<L: Lanes>(f: &mut [L; N], t: f32, shape: &[L; N]) -> u32 {
    let tt = L::splat(t);
    let dc = f[0].first();
    let mut kept = 0;
    for (v, s) in f.iter_mut().zip(shape) {
        let (k, n) = v.keep_over(tt.mul(*s));
        *v = k;
        kept += n;
    }
    f[0] = f[0].with_first(dc);
    kept - (dc.abs() >= t) as u32
}

/// Shrink each coefficient by g²/(g² + σ²), g being the guide's and σ² being
/// `s2` times the noise's `shape2` at each frequency, all but the mean;
/// returns the sum of the squared factors.
#[inline(always)]
fn wiener<L: Lanes>(f: &mut [L; N], g: &[L; N], s2: f32, shape2: &[L; N]) -> f32 {
    let s2v = L::splat(s2);
    let dc = f[0].first();
    let mut e = L::splat(0.0);
    for ((v, g), s) in f.iter_mut().zip(g).zip(shape2) {
        let g2 = g.mul(*g);
        let rho = g2.div(g2.add(s2v.mul(*s)));
        *v = v.mul(rho);
        e = rho.mul_add(rho, e);
    }
    let g2 = g[0].first() * g[0].first();
    let rho0 = g2 / (g2 + s2);
    f[0] = f[0].with_first(dc);
    e.sum() - rho0 * rho0
}

#[inline(always)]
fn mean<L: Lanes>(r: &[L; N]) -> f32 {
    r[1..].iter().fold(r[0], |a, v| a.add(*v)).sum() / (N * N) as f32
}

/// Where patches start along an edge of `n` pixels: every `step`, and flush
/// with the far end so every pixel is covered.
fn grid(n: usize, step: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (0..=n - N).step_by(step).collect();
    if v.last() != Some(&(n - N)) {
        v.push(n - N);
    }
    v
}

/// The patch at (x, y) of a plane.
#[inline(always)]
fn load<L: Lanes>(plane: &[f32], w: usize, x: usize, y: usize) -> [L; N] {
    std::array::from_fn(|r| {
        let o = (y + r) * w + x;
        L::load(plane[o..o + N].try_into().expect("8 floats"))
    })
}

/// Weighted sums of overlapping patches over rows `lo..hi`.
struct Acc {
    w: usize,
    lo: usize,
    hi: usize,
    num: [Vec<f32>; 3],
    den: Vec<f32>,
}

impl Acc {
    fn new(w: usize, lo: usize, hi: usize, active: [bool; 3]) -> Self {
        let n = w * (hi - lo);
        let num = std::array::from_fn(|c| if active[c] { vec![0.0; n] } else { Vec::new() });
        Self { w, lo, hi, num, den: vec![0.0; n] }
    }

    #[inline(always)]
    fn add<L: Lanes>(&mut self, x: usize, y: usize, wgt: f32, patches: &[[L; N]; 3], active: [bool; 3]) {
        let wv = L::splat(wgt);
        let rows = y.max(self.lo)..(y + N).min(self.hi);
        for yy in rows {
            let r = yy - y;
            let o = (yy - self.lo) * self.w + x;
            for c in 0..3 {
                if active[c] {
                    let d: &mut [f32; N] = (&mut self.num[c][o..o + N]).try_into().expect("8 floats");
                    patches[c][r].mul_add(wv, L::load(d)).store(d);
                }
            }
            let d: &mut [f32; N] = (&mut self.den[o..o + N]).try_into().expect("8 floats");
            L::load(d).add(wv).store(d);
        }
    }

    fn finish(mut self, active: [bool; 3]) -> [Vec<f32>; 3] {
        for (num, on) in self.num.iter_mut().zip(active) {
            if on {
                for (v, d) in num.iter_mut().zip(&self.den) {
                    *v /= d;
                }
            }
        }
        self.num
    }
}

/// One scale and how to denoise it.
struct Level<'a> {
    p: &'a Planes,
    noise: &'a Noise,
    /// Scales the measured noise of each channel; 0 leaves a channel alone.
    /// (The two colour channels are always on or off together.)
    mult: [f32; 3],
    active: [bool; 3],
    /// Where the patches of the first (guide) and second (final) pass start.
    guide_x: Vec<usize>,
    guide_y: Vec<usize>,
    final_x: Vec<usize>,
    final_y: Vec<usize>,
}

impl Level<'_> {
    /// σ of channel `ch` in a patch whose brightness channel averages `mean_y`.
    #[inline(always)]
    fn sigma(&self, ch: usize, mean_y: f32) -> f32 {
        (self.noise.at(ch, mean_y * S3) * self.mult[ch]).max(1e-6)
    }
}

/// Denoise rows `y0..y1` of a scale, with brightness (`Y`) and colour (`C`)
/// noise on or off. Each strip works out the guide it needs on its own (a few
/// rows past its edges), so strips run side by side.
#[inline(always)]
fn strip<L: Lanes, const Y: bool, const C: bool>(lv: &Level, y0: usize, y1: usize) -> [Vec<f32>; 3] {
    let (p, w) = (lv.p, lv.p.w);
    let on = [Y, C, C];
    // the final patches that touch these rows, and the rows those cover
    let fy: Vec<usize> = lv.final_y.iter().copied().filter(|&py| py + N > y0 && py < y1).collect();
    let (b0, b1) = (fy[0], fy[fy.len() - 1] + N);
    let gy: Vec<usize> = lv.guide_y.iter().copied().filter(|&py| py + N > b0 && py < b1).collect();
    let zero = [L::splat(0.0); N];
    let shape: [[L; N]; 3] = lv.noise.shape.map(|s| std::array::from_fn(|r| L::load(&s[r])));
    let shape2: [[L; N]; 3] = shape.map(|s| s.map(|v| v.mul(v)));

    // Step 1: hard thresholding, for a guide over rows b0..b1.
    let mut acc = Acc::new(w, b0, b1, on);
    for &py in &gy {
        for &px in &lv.guide_x {
            // (the thresholds first, so no patch is held across working them out)
            let m = mean::<L>(&load(&p.c[0], w, px, py));
            let t: [f32; 3] = std::array::from_fn(|c| if on[c] { 3.0 * lv.sigma(c, m) } else { 0.0 });
            let mut x = [zero; 3];
            let mut kept = 0;
            for c in 0..3 {
                if !on[c] {
                    continue;
                }
                x[c] = load(&p.c[c], w, px, py);
                dct(&mut x[c]);
                kept += hard(&mut x[c], t[c], &shape[c]);
                idct(&mut x[c]);
            }
            // patches that came out simple count more: less ringing near edges
            acc.add(px, py, 1.0 / (1.0 + kept as f32), &x, on);
        }
    }
    let guide = acc.finish(on);

    // Step 2: Wiener filtering against the guide, for rows y0..y1.
    let mut acc = Acc::new(w, y0, y1, on);
    for &py in &fy {
        let gr = py - b0;
        for &px in &lv.final_x {
            let m = mean::<L>(&load(if Y { &guide[0] } else { &p.c[0] }, w, px, if Y { gr } else { py }));
            let s2: [f32; 3] = std::array::from_fn(|c| if on[c] { lv.sigma(c, m).powi(2) } else { 0.0 });
            let mut x = [zero; 3];
            let mut e = 0.0;
            for c in 0..3 {
                if !on[c] {
                    continue;
                }
                let mut g = load(&guide[c], w, px, gr);
                dct(&mut g);
                x[c] = load(&p.c[c], w, px, py);
                dct(&mut x[c]);
                e += wiener(&mut x[c], &g, s2[c], &shape2[c]);
                idct(&mut x[c]);
            }
            acc.add(px, py, 1.0 / (1.0 + e), &x, on);
        }
    }
    acc.finish(on)
}

/// [`strip`] for the channels that are on.
#[inline(always)]
fn strip_for<L: Lanes>(lv: &Level, y0: usize, y1: usize) -> [Vec<f32>; 3] {
    match (lv.active[0], lv.active[1]) {
        (true, true) => strip::<L, true, true>(lv, y0, y1),
        (true, false) => strip::<L, true, false>(lv, y0, y1),
        _ => strip::<L, false, true>(lv, y0, y1),
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn strip_avx(lv: &Level, y0: usize, y1: usize) -> [Vec<f32>; 3] {
    strip_for::<Avx>(lv, y0, y1)
}

/// Denoise one scale. `mult` scales the measured noise per channel; 0 leaves
/// a channel alone.
fn denoise_level(p: &Planes, noise: &Noise, mult: [f32; 3]) -> Planes {
    let lv = Level {
        p,
        noise,
        mult,
        active: mult.map(|m| m > 0.0),
        guide_x: grid(p.w, STEP_GUIDE),
        guide_y: grid(p.h, STEP_GUIDE),
        final_x: grid(p.w, STEP_FINAL),
        final_y: grid(p.h, STEP_FINAL),
    };
    #[cfg(target_arch = "x86_64")]
    let avx = is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma");
    let threads = rayon::current_num_threads().max(1);
    // about three strips a thread, to even out; not under 32 rows, as each
    // strip works out a few rows of guide past its edges
    let rows = (p.h / (3 * threads)).clamp(32, 256);
    let starts: Vec<usize> = (0..p.h).step_by(rows).collect();
    let parts: Vec<[Vec<f32>; 3]> = starts
        .par_iter()
        .map(|&y0| {
            let y1 = (y0 + rows).min(p.h);
            #[cfg(target_arch = "x86_64")]
            if avx {
                // SAFETY: the CPU has AVX2 and FMA (checked above).
                return unsafe { strip_avx(&lv, y0, y1) };
            }
            strip_for::<[f32; N]>(&lv, y0, y1)
        })
        .collect();

    let mut out = p.clone();
    for (&y0, part) in starts.iter().zip(parts) {
        for ((oc, pc), on) in out.c.iter_mut().zip(&part).zip(lv.active) {
            if on {
                oc[y0 * p.w..y0 * p.w + pc.len()].copy_from_slice(pc);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test card: soft gradients, hard edges, fine stripes, flat patches.
    fn card(w: usize, h: usize) -> Img {
        let mut img = Img::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
                let mut c = [0.15 + 0.6 * fx, 0.5 - 0.3 * fy, 0.3 + 0.4 * fx * fy];
                if (x / 40 + y / 40) % 2 == 0 && x < w / 2 {
                    c = [c[0] * 0.6, c[1] * 0.6 + 0.2, c[2] * 0.6];
                }
                if x > w * 3 / 4 && y < h / 2 {
                    let s = if (x / 3) % 2 == 0 { 0.12 } else { -0.12 };
                    c = [0.5 + s, 0.5 + s, 0.5 + s];
                }
                img.px[y * w + x] = c;
            }
        }
        img
    }

    fn gauss(seed: &mut u64) -> f32 {
        // Box–Muller on a small LCG: plenty for test noise
        let mut u = || {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((*seed >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        };
        let (a, b) = (u(), u());
        (-2.0 * a.ln()).sqrt() * (std::f32::consts::TAU * b).cos()
    }

    fn noisy(img: &Img, sigma: f32, seed: u64) -> Img {
        let mut s = seed;
        let mut o = img.clone();
        for p in &mut o.px {
            for v in p.iter_mut() {
                *v += sigma * gauss(&mut s);
            }
        }
        o
    }

    fn psnr(a: &Img, b: &Img) -> f32 {
        let mse: f64 = a.px.iter().zip(&b.px).map(|(p, q)| (0..3).map(|c| ((p[c] - q[c]) as f64).powi(2)).sum::<f64>()).sum::<f64>()
            / (a.px.len() * 3) as f64;
        (10.0 * (1.0 / mse).log10()) as f32
    }

    #[test]
    fn dct_round_trips() {
        let x: Blk = std::array::from_fn(|i| std::array::from_fn(|j| ((i * 8 + j) as f32 * 0.731).sin()));
        // the textbook transform, F = D·X·Dᵀ
        let d: Blk = std::array::from_fn(|k| {
            let a = if k == 0 { (1.0f64 / 8.0).sqrt() } else { 0.5 };
            std::array::from_fn(|n| (a * (std::f64::consts::PI * (2 * n + 1) as f64 * k as f64 / 16.0).cos()) as f32)
        });
        let want: Blk = std::array::from_fn(|k| {
            std::array::from_fn(|l| (0..N).flat_map(|r| (0..N).map(move |n| (r, n))).map(|(r, n)| d[k][r] * x[r][n] * d[l][n]).sum())
        });
        let check = |f: &Blk, y: &Blk| {
            for k in 0..N {
                for l in 0..N {
                    assert!((f[l][k] - want[k][l]).abs() < 1e-5, "coefficient {k},{l}");
                    assert!((y[k][l] - x[k][l]).abs() < 1e-5, "pixel {k},{l}");
                }
            }
        };
        fn both<L: Lanes>(x: &Blk, f: &mut Blk, y: &mut Blk) {
            let mut r: [L; N] = std::array::from_fn(|i| L::load(&x[i]));
            dct(&mut r);
            for (v, o) in r.iter().zip(f.iter_mut()) {
                v.store(o);
            }
            idct(&mut r);
            for (v, o) in r.iter().zip(y.iter_mut()) {
                v.store(o);
            }
        }
        let (mut f, mut y) = ([[0.0; N]; N], [[0.0; N]; N]);
        both::<[f32; N]>(&x, &mut f, &mut y);
        check(&f, &y);
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            #[target_feature(enable = "avx2,fma")]
            fn avx(x: &Blk, f: &mut Blk, y: &mut Blk) {
                both::<Avx>(x, f, y);
            }
            // SAFETY: the CPU has AVX2 and FMA.
            unsafe { avx(&x, &mut f, &mut y) };
            check(&f, &y);
        }
    }

    #[test]
    fn colour_split_round_trips() {
        let img = card(64, 48);
        let back = join(&split(&img));
        for (a, b) in img.px.iter().zip(&back.px) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn measures_white_noise() {
        // a flat grey picture, so all there is to measure is the noise
        let flat = Img { w: 512, h: 384, px: vec![[0.4, 0.45, 0.5]; 512 * 384], alpha: None };
        for sigma in [0.01, 0.04] {
            let img = noisy(&flat, sigma, 7);
            let p = split(&img);
            let clip = clipped(&img);
            let n = Noise { bins: measure(&p, &clip).expect("enough to measure").0, shape: [WHITE; 3] };
            for c in 0..3 {
                let got = n.at(c, 0.45);
                assert!((got / sigma - 1.0).abs() < 0.08, "channel {c}: measured {got}, actual {sigma}");
            }
            // one scale down it's half as strong
            let h = half(&p);
            let n = Noise { bins: measure(&h, &half_clip(&clip, p.w, p.h)).expect("enough to measure").0, shape: [WHITE; 3] };
            assert!((n.at(0, 0.45) / (sigma / 2.0) - 1.0).abs() < 0.12);
        }
    }

    #[test]
    fn measures_through_crushed_blacks() {
        // a dim channel, so the noise is cut off at 0 here and there (about 3%
        // of its pixels), as in the shadows of a raw file: still measured at
        // every scale, where needing whole blocks free of clipping finds none
        let mut img = noisy(&Img { w: 1024, h: 768, px: vec![[0.3, 0.25, 0.06]; 1024 * 768], alpha: None }, 0.03, 3);
        for p in &mut img.px {
            *p = p.map(|v| v.clamp(0.0, 1.0));
        }
        let mut p = split(&img);
        let mut clip = clipped(&img);
        for l in 0..4 {
            let n = Noise { bins: measure(&p, &clip).expect("enough to measure").0, shape: [WHITE; 3] };
            let want = 0.03 / (1 << l) as f32;
            let got = n.at(0, 0.2);
            assert!((got / want - 1.0).abs() < 0.15, "scale {l}: measured {got}, actual {want}");
            clip = half_clip(&clip, p.w, p.h);
            p = half(&p);
        }
        // nothing measurable at all
        let black = Img { w: 256, h: 256, px: vec![[0.0; 3]; 256 * 256], alpha: None };
        assert!(measure(&split(&black), &clipped(&black)).is_none());
    }

    #[test]
    fn clean_picture_is_left_nearly_alone() {
        let img = card(384, 256);
        let out = denoise(&img, Amounts { luma: 0.5, detail: 0.5, color: 0.5 });
        assert!(psnr(&img, &out) > 40.0, "{}", psnr(&img, &out));
    }

    #[test]
    fn removes_noise() {
        let clean = card(384, 256);
        let img = noisy(&clean, 0.06, 3);
        let before = psnr(&clean, &img);
        let out = denoise(&img, Amounts { luma: 0.5, detail: 0.5, color: 0.5 });
        let after = psnr(&clean, &out);
        assert!(after > before + 8.0, "{before} dB → {after} dB");
    }

    #[test]
    fn colour_only_leaves_brightness_alone() {
        let clean = card(256, 192);
        let img = noisy(&clean, 0.05, 5);
        let out = denoise(&img, Amounts { luma: 0.0, detail: 0.5, color: 1.0 });
        let (a, b, truth) = (split(&img), split(&out), split(&clean));
        // (where nothing had to be clipped at black)
        for ((x, y), p) in a.c[0].iter().zip(&b.c[0]).zip(&out.px) {
            if p.iter().all(|&v| v > 0.0) {
                assert!((x - y).abs() < 1e-4);
            }
        }
        // but the colour noise is gone
        let err = |p: &Planes| (1..3).map(|c| p.c[c].iter().zip(&truth.c[c]).map(|(v, t)| (v - t).powi(2)).sum::<f32>()).sum::<f32>();
        assert!(err(&b) < err(&a) * 0.2, "{} → {}", err(&a), err(&b));
    }

    #[test]
    fn off_is_untouched() {
        let img = noisy(&card(64, 48), 0.05, 1);
        let out = denoise(&img, Amounts { luma: 0.0, detail: 0.5, color: 0.0 });
        assert_eq!(img.px, out.px);
    }
}
