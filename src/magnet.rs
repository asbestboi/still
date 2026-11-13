//! The layer magnet: while a layer is moved, scaled, stretched or turned on
//! the canvas, its edges are pulled flush against nearby layers, side by side
//! or lined up with theirs. Works in screen space on rotated rectangles, so
//! it holds for tilted layers too.
//!
//! With auto pivot, a layer the magnet catches also turns to its neighbour's
//! angle, and scaling a layer that touches a neighbour keeps the touching
//! side in place.

use egui::{Pos2, Vec2, vec2};

/// A rotated rectangle on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Box2 {
    pub c: Pos2,
    /// Half width and half height along its own axes.
    pub half: Vec2,
    /// Radians, clockwise on screen.
    pub rot: f32,
}

impl Box2 {
    /// Its x and y axes on screen.
    pub fn axes(&self) -> (Vec2, Vec2) {
        let (s, c) = self.rot.sin_cos();
        (vec2(c, s), vec2(-s, c))
    }

    fn to_local(self, p: Pos2) -> Vec2 {
        let (ax, ay) = self.axes();
        let d = p - self.c;
        vec2(d.dot(ax), d.dot(ay))
    }

    fn to_screen(self, v: Vec2) -> Pos2 {
        let (ax, ay) = self.axes();
        self.c + ax * v.x + ay * v.y
    }

    /// Half extents of `other` measured along this box's axes, when the two
    /// are aligned (rotations equal up to quarter turns).
    fn aligned_half(self, other: Box2) -> Option<Vec2> {
        let q = quarter_turns(other.rot - self.rot)?;
        Some(if q % 2 == 0 { other.half } else { vec2(other.half.y, other.half.x) })
    }

    /// Are the two (aligned) boxes touching, overlapping or within `dist`?
    fn near(self, other: Box2, other_half: Vec2, dist: f32) -> bool {
        let m = self.to_local(other.c);
        m.x.abs() <= self.half.x + other_half.x + dist && m.y.abs() <= self.half.y + other_half.y + dist
    }
}

/// How many quarter turns `a` (radians) is, if it is one within half a degree.
fn quarter_turns(a: f32) -> Option<i32> {
    let q = a / std::f32::consts::FRAC_PI_2;
    let r = q.round();
    ((q - r).abs() * 90.0 < 0.5).then_some(r.rem_euclid(4.0) as i32)
}

/// The angle closest to `rot` that lines up with `target` (up to quarter turns).
pub fn align_rotation(rot: f32, target: f32) -> f32 {
    let q = std::f32::consts::FRAC_PI_2;
    target + ((rot - target) / q).round() * q
}

/// What a snap changed, and the guides to draw for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Snap {
    pub center: Pos2,
    /// New rotation (radians) when auto pivot turned the layer.
    pub rot: Option<f32>,
    pub lines: Vec<[Pos2; 2]>,
}

/// The nearest offset within `dist` that puts one of `values` on one of `targets`.
fn best(pairs: &[(f32, f32)], dist: f32) -> Option<(f32, f32)> {
    pairs.iter().map(|&(v, t)| (t - v, t)).filter(|(d, _)| d.abs() < dist).min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))
}

/// A guide along the line `x = t` (or `y = t` when `!along_x`) in `n`'s
/// frame, long enough to cover both boxes.
fn guide(n: Box2, t: f32, along_x: bool, m: Vec2, mh: Vec2) -> [Pos2; 2] {
    if along_x {
        let (y0, y1) = ((m.y - mh.y).min(-n.half.y), (m.y + mh.y).max(n.half.y));
        [n.to_screen(vec2(t, y0)), n.to_screen(vec2(t, y1))]
    } else {
        let (x0, x1) = ((m.x - mh.x).min(-n.half.x), (m.x + mh.x).max(n.half.x));
        [n.to_screen(vec2(x0, t)), n.to_screen(vec2(x1, t))]
    }
}

/// Moving `mov`: pull it flush against the closest of `others`.
pub fn snap_move(mov: Box2, others: &[Box2], dist: f32, pivot: bool) -> Option<Snap> {
    let mut found: Option<(f32, Snap)> = None;
    for &n in others {
        let (rot, turned) = if quarter_turns(mov.rot - n.rot).is_some() {
            (mov.rot, false)
        } else if pivot {
            (align_rotation(mov.rot, n.rot), true)
        } else {
            continue;
        };
        let cand = Box2 { rot, ..mov };
        let Some(h) = n.aligned_half(cand) else { continue };
        if !n.near(cand, h, dist) {
            continue;
        }
        let m = n.to_local(cand.c);
        let (w, hh) = (n.half.x, n.half.y);
        // side by side, edges lined up, or centres lined up
        let dx = best(&[(m.x - h.x, w), (m.x + h.x, -w), (m.x - h.x, -w), (m.x + h.x, w), (m.x, 0.0)], dist);
        let dy = best(&[(m.y - h.y, hh), (m.y + h.y, -hh), (m.y - h.y, -hh), (m.y + h.y, hh), (m.y, 0.0)], dist);
        if dx.is_none() && dy.is_none() {
            continue;
        }
        let score = dx.map(|d| d.0.abs()).unwrap_or(f32::MAX).min(dy.map(|d| d.0.abs()).unwrap_or(f32::MAX));
        if found.as_ref().is_some_and(|(s, _)| *s <= score) {
            continue;
        }
        let nm = m + vec2(dx.map(|d| d.0).unwrap_or(0.0), dy.map(|d| d.0).unwrap_or(0.0));
        let mut lines = Vec::new();
        if let Some((_, t)) = dx {
            lines.push(guide(n, t, true, nm, h));
        }
        if let Some((_, t)) = dy {
            lines.push(guide(n, t, false, nm, h));
        }
        found = Some((score, Snap { center: n.to_screen(nm), rot: turned.then_some(rot), lines }));
    }
    found.map(|f| f.1)
}

/// An edge of a moving box: a point on it and its outward normal. Returns how
/// far (along the normal) the edge must move to sit flush on the closest
/// parallel edge of `others`, and a guide. `mov` is the whole box as it is now.
pub fn snap_edge(edge: Pos2, normal: Vec2, mov: Box2, others: &[Box2], dist: f32) -> Option<(f32, [Pos2; 2])> {
    let mut found: Option<(f32, [Pos2; 2])> = None;
    for &n in others {
        let Some(h) = n.aligned_half(mov) else { continue };
        if !n.near(mov, h, dist) {
            continue;
        }
        let (ax, ay) = n.axes();
        // the neighbour axis parallel to the edge's normal
        let (u, along_x) = if normal.dot(ax).abs() > 0.5 { (ax * normal.dot(ax).signum(), true) } else { (ay * normal.dot(ay).signum(), false) };
        let ext = if along_x { n.half.x } else { n.half.y };
        let v = (edge - n.c).dot(u);
        let Some((d, t)) = best(&[(v, ext), (v, -ext)], dist) else { continue };
        if found.as_ref().is_some_and(|f| f.0.abs() <= d.abs()) {
            continue;
        }
        // the guide's line in the neighbour's frame (sign of u folded back in)
        let sign = if along_x { u.dot(ax) } else { u.dot(ay) };
        let moved = Box2 { c: mov.c + normal * d, ..mov };
        found = Some((d, guide(n, t * sign, along_x, n.to_local(moved.c), h)));
    }
    found
}

/// Which sides of `mov` touch a neighbour, as a point in its own frame with
/// components -1, 0 or 1 (e.g. (-1, 0): its left side). Scaling then keeps
/// that point in place.
pub fn touching_anchor(mov: Box2, others: &[Box2], tol: f32) -> Vec2 {
    let (mx, my) = mov.axes();
    let mut a = Vec2::ZERO;
    for &n in others {
        let Some(h) = n.aligned_half(mov) else { continue };
        if !n.near(mov, h, tol) {
            continue;
        }
        let m = n.to_local(mov.c);
        let (ax, ay) = n.axes();
        // flush side by side, with the other extents overlapping
        let overlap_y = m.y.abs() < h.y + n.half.y - tol;
        let overlap_x = m.x.abs() < h.x + n.half.x - tol;
        let mut sides = Vec::new();
        if overlap_y && ((m.x - h.x) - n.half.x).abs() < tol {
            sides.push(-ax); // our side facing -x in n's frame touches n's right
        }
        if overlap_y && ((m.x + h.x) + n.half.x).abs() < tol {
            sides.push(ax);
        }
        if overlap_x && ((m.y - h.y) - n.half.y).abs() < tol {
            sides.push(-ay);
        }
        if overlap_x && ((m.y + h.y) + n.half.y).abs() < tol {
            sides.push(ay);
        }
        for s in sides {
            let (x, y) = (s.dot(mx).round(), s.dot(my).round());
            if a.x == 0.0 && x != 0.0 {
                a.x = x;
            }
            if a.y == 0.0 && y != 0.0 {
                a.y = y;
            }
        }
    }
    a
}

/// The box `start` scaled by `k` about `anchor` (a point in its own frame,
/// components -1..1): that point stays where it was.
pub fn scaled(start: Box2, anchor: Vec2, k: f32) -> Box2 {
    let (ax, ay) = start.axes();
    let p = start.c + ax * (anchor.x * start.half.x) + ay * (anchor.y * start.half.y);
    let half = start.half * k;
    Box2 { c: p - ax * (anchor.x * half.x) - ay * (anchor.y * half.y), half, rot: start.rot }
}

/// Scaling `start` by `k` about `anchor`: the nearby `k` that puts one of its
/// free sides flush on a neighbour's edge, and a guide.
pub fn snap_scale(start: Box2, anchor: Vec2, k: f32, others: &[Box2], dist: f32) -> Option<(f32, [Pos2; 2])> {
    let b = scaled(start, anchor, k);
    let (ax, ay) = b.axes();
    let mut found: Option<(f32, f32, [Pos2; 2])> = None;
    for (side, normal) in [(vec2(1.0, 0.0), ax), (vec2(-1.0, 0.0), -ax), (vec2(0.0, 1.0), ay), (vec2(0.0, -1.0), -ay)] {
        // how fast this side moves outward as k grows, in pixels per unit k
        let rate = if side.x != 0.0 { (side.x - anchor.x).abs() * start.half.x } else { (side.y - anchor.y).abs() * start.half.y };
        if rate < 1e-3 {
            continue;
        }
        let edge = b.c + ax * (side.x * b.half.x) + ay * (side.y * b.half.y);
        if let Some((d, line)) = snap_edge(edge, normal, b, others, dist)
            && found.as_ref().is_none_or(|f| d.abs() < f.0.abs())
        {
            found = Some((d, k + d / rate, line));
        }
    }
    found.filter(|f| f.1 > 0.0).map(|f| (f.1, f.2))
}

/// Turning a layer: snap its angle to a neighbour's (up to quarter turns)
/// within `tol` radians.
pub fn snap_rotation(rot: f32, others: &[Box2], tol: f32) -> Option<f32> {
    others.iter().map(|n| align_rotation(rot, n.rot)).filter(|a| (a - rot).abs() < tol).min_by(|a, b| (a - rot).abs().total_cmp(&(b - rot).abs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn bx(x: f32, y: f32, w: f32, h: f32, deg: f32) -> Box2 {
        Box2 { c: pos2(x, y), half: vec2(w / 2.0, h / 2.0), rot: deg.to_radians() }
    }

    #[test]
    fn moves_flush_next_to_a_neighbour() {
        let n = bx(100.0, 100.0, 100.0, 60.0, 0.0);
        // 4 px right of the neighbour's right edge (x = 150)
        let m = bx(184.0, 110.0, 60.0, 40.0, 0.0);
        let s = snap_move(m, &[n], 8.0, false).unwrap();
        assert!((s.center.x - 180.0).abs() < 1e-3, "{:?}", s.center);
        assert!(s.rot.is_none());
        assert!(!s.lines.is_empty());
        // far away: nothing
        assert!(snap_move(bx(300.0, 300.0, 60.0, 40.0, 0.0), &[n], 8.0, false).is_none());
    }

    #[test]
    fn follows_a_tilted_neighbour() {
        let n = bx(0.0, 0.0, 100.0, 50.0, 30.0);
        // place a box near the tilted neighbour's right side, along its x axis
        let (ax, _) = n.axes();
        let m = Box2 { c: pos2(0.0, 0.0) + ax * (50.0 + 20.0 + 5.0), half: vec2(20.0, 10.0), rot: n.rot };
        let s = snap_move(m, &[n], 8.0, false).unwrap();
        let local = n.to_local(s.center);
        assert!((local.x - 70.0).abs() < 1e-3, "{local:?}");
    }

    #[test]
    fn auto_pivot_turns_to_match() {
        let n = bx(0.0, 0.0, 100.0, 50.0, 20.0);
        let (ax, _) = n.axes();
        let m = Box2 { c: pos2(0.0, 0.0) + ax * 74.0, half: vec2(20.0, 10.0), rot: 5f32.to_radians() };
        assert!(snap_move(m, &[n], 8.0, false).is_none(), "not aligned, no pivot: no snap");
        let s = snap_move(m, &[n], 8.0, true).unwrap();
        assert!((s.rot.unwrap() - n.rot).abs() < 1e-5);
        assert!((n.to_local(s.center).x - 70.0).abs() < 1e-3);
        // a quarter turn off also lines up, without spinning the layer around
        assert!((align_rotation(100f32.to_radians(), 20f32.to_radians()) - 110f32.to_radians()).abs() < 1e-5);
    }

    #[test]
    fn scaling_keeps_the_touching_side_and_snaps() {
        let n = bx(100.0, 100.0, 100.0, 100.0, 0.0);
        // flush against the neighbour's right side
        let m = bx(170.0, 100.0, 40.0, 40.0, 0.0);
        let a = touching_anchor(m, &[n], 1.0);
        assert_eq!(a, vec2(-1.0, 0.0));
        let b = scaled(m, a, 2.0);
        assert!((b.c.x - b.half.x - 150.0).abs() < 1e-3, "the left side stays on the neighbour");
        // the bottom side lands near the neighbour's bottom (y = 150): snaps to it
        let k = (50.0 + 47.0) / 40.0; // bottom at 100 + 48.5 when anchored at the left middle
        let (ks, _) = snap_scale(m, a, k, &[n], 8.0).unwrap();
        let b = scaled(m, a, ks);
        assert!((b.c.y + b.half.y - 150.0).abs() < 1e-3, "{b:?}");
    }

    #[test]
    fn stretching_edge_snaps_to_a_parallel_edge() {
        let n = bx(100.0, 100.0, 100.0, 100.0, 0.0);
        let m = bx(200.0, 100.0, 40.0, 40.0, 0.0);
        // its left edge at x = 180 moving left toward the neighbour's right (150)
        let edge = pos2(153.0, 100.0);
        let (d, _) = snap_edge(edge, vec2(-1.0, 0.0), Box2 { c: pos2(176.5, 100.0), half: vec2(23.5, 20.0), ..m }, &[n], 8.0).unwrap();
        assert!((d - 3.0).abs() < 1e-3, "moves 3 px further out: {d}");
    }

    #[test]
    fn rotation_snaps_to_neighbours() {
        let n = bx(0.0, 0.0, 10.0, 10.0, 30.0);
        let r = snap_rotation(32f32.to_radians(), &[n], 4f32.to_radians()).unwrap();
        assert!((r - 30f32.to_radians()).abs() < 1e-5);
        assert!(snap_rotation(50f32.to_radians(), &[n], 4f32.to_radians()).is_none());
    }
}
