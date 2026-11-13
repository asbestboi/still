//! Retouching by hand: the clone tool paints pixels copied from another part
//! of the photo, which is how a stray object is painted out.
//!
//! Strokes are stored in normalised coordinates of the rotated, straightened
//! frame (before cropping), like masks, and run on that whole frame, so a
//! stroke can copy from outside the crop and renders the same at any size.

use crate::masks::{BrushStroke, Space, stamp_stroke};
use crate::pipeline::Img;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloneStroke {
    /// Where the stroke was painted.
    pub pts: Vec<[f32; 2]>,
    /// Radius in long-edge units.
    pub radius: f32,
    /// 0 = hard edge, 1 = fully soft.
    pub feather: f32,
    pub opacity: f32,
    /// Source minus destination: each painted point copies from `p + offset`.
    pub offset: [f32; 2],
}

/// Paint every stroke, in order, onto the whole frame.
pub fn apply(img: &mut Img, strokes: &[CloneStroke]) {
    let (w, h) = (img.w, img.h);
    let sp = Space { fw: w as f32, fh: h as f32, ox: 0.0, oy: 0.0 };
    for st in strokes {
        let dab = BrushStroke { pts: st.pts.clone(), radius: st.radius, feather: st.feather, flow: st.opacity, erase: false };
        let mut a = vec![0.0f32; w * h];
        stamp_stroke(&mut a, w, h, &dab, &sp);
        // copy from the picture as it was before this stroke, so a stroke
        // never smears its own pixels along
        let src = img.clone();
        let (dx, dy) = (st.offset[0] * w as f32, st.offset[1] * h as f32);
        img.px.par_chunks_mut(w).zip(a.par_chunks(w)).enumerate().for_each(|(y, (row, arow))| {
            for (x, (px, &al)) in row.iter_mut().zip(arow).enumerate() {
                if al > 1e-4 {
                    let s = src.sample(x as f32 + 0.5 + dx, y as f32 + 0.5 + dy);
                    for c in 0..3 {
                        px[c] += (s[c] - px[c]) * al;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_from_the_offset_and_leaves_the_rest() {
        // left half black, right half white; clone white over a spot on the left
        let (w, h) = (100usize, 50usize);
        let px = (0..w * h).map(|i| if i % w < 50 { [0.0; 3] } else { [1.0; 3] }).collect();
        let mut img = Img { w, h, px };
        let st = CloneStroke { pts: vec![[0.2, 0.5]], radius: 0.05, feather: 0.0, opacity: 1.0, offset: [0.5, 0.0] };
        apply(&mut img, &[st]);
        assert!(img.px[25 * w + 20][0] > 0.99, "painted spot copies white");
        assert!(img.px[25 * w + 40][0] < 0.01, "outside the brush stays black");
        assert!(img.px[25 * w + 70][0] > 0.99, "the source is untouched");
    }
}
