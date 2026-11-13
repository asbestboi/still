use crate::io::{self, Doc, ExportSettings, Format};
use crate::layers::{Blend, Layer};
use crate::masks::{self, BrushStroke, Local, Mask, Refine, Seed, Shape};
use crate::pipeline::{self, Aspect, Histogram, Img, Params};
use crate::retouch::CloneStroke;
use crate::ui_kit::{self as k, Btn, Fmt};
use egui::{Align2, Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

enum Job {
    /// `overlay`: mask index and its wash colour. `draft`: render at half size (fast, while dragging).
    /// `logical_w`: width of the working preview, which the canvas lays out by.
    Preview { src: Arc<Img>, logical_w: usize, params: Params, generation: u64, frame: Frame, overlay: Option<(usize, u32)>, draft: bool },
    Thumbs { src: Arc<Img>, looks: Vec<Params>, ver: u64 },
}

enum Msg {
    /// `scale`: how many preview pixels one image pixel covers (2 for drafts).
    Preview { generation: u64, image: Arc<egui::ColorImage>, hist: Histogram, ms: f32, frame: Frame, overlay: Option<egui::ColorImage>, scale: f32, draft: bool },
    Thumbs { ver: u64, images: Vec<egui::ColorImage> },
    Loaded(Result<Doc, String>),
    Picked(PathBuf),
    ExportTo(PathBuf),
    Exported(Result<PathBuf, String>),
    DialogClosed,
    LooksImported(Vec<(String, Params)>, Vec<String>),
    Info(String, Option<PathBuf>),
    Repreviewed(PathBuf, Arc<Img>),
    LayerFiles(Vec<PathBuf>),
    SaveProjectTo(PathBuf),
    LayerReady(Result<(PathBuf, f32), String>),
    /// A sharper source for zoomed-in viewing: photo, long edge, image.
    DetailReady(PathBuf, u32, Arc<Img>),
}

/// Largest long edge used for the sharper zoomed-in preview.
const DETAIL_MAX: u32 = 4000;
/// How many recent files the start screen remembers.
const RECENT_MAX: usize = 8;
const OVERLAY_COLORS: [u32; 3] = [0xFF2D28, 0x30D158, 0x0A84FF];
const CANVAS_COLORS: [Color32; 3] = [k::CANVAS, Color32::from_rgb(28, 28, 30), Color32::from_rgb(72, 72, 76)];

fn to_color_image(img: &Img) -> egui::ColorImage {
    let mut rgb = Vec::with_capacity(img.w * img.h * 3);
    for p in &img.px {
        for c in p {
            rgb.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    egui::ColorImage::from_rgb([img.w, img.h], &rgb)
}

/// The picture over a grey checkerboard where masks have cut it out.
fn to_color_image_cutout(img: &Img, alpha: &[f32]) -> egui::ColorImage {
    let cell = (img.w.max(img.h) / 90).max(4);
    let mut rgb = Vec::with_capacity(img.w * img.h * 3);
    for (i, p) in img.px.iter().enumerate() {
        let (x, y) = (i % img.w, i / img.w);
        let check = if (x / cell + y / cell).is_multiple_of(2) { 0.42 } else { 0.30 };
        let a = alpha[i].clamp(0.0, 1.0);
        for c in p {
            rgb.push(((c.clamp(0.0, 1.0) * a + check * (1.0 - a)) * 255.0 + 0.5) as u8);
        }
    }
    egui::ColorImage::from_rgb([img.w, img.h], &rgb)
}

/// The mask as a translucent red wash, like a rubylith.
fn overlay_image(a: &[f32], w: usize, h: usize, c: u32) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(w * h * 4);
    let (r, g, b) = ((c >> 16) as u8, (c >> 8) as u8, c as u8);
    for v in a {
        rgba.extend_from_slice(&[r, g, b, (v.clamp(0.0, 1.0) * 150.0) as u8]);
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

fn spawn_worker(ctx: egui::Context, jobs: Receiver<Job>, out: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut cache = pipeline::StageCache::default();
        // the last preview source and its half-size copy for drafts
        let mut halves: Option<(Arc<Img>, Arc<Img>)> = None;
        while let Ok(first) = jobs.recv() {
            // coalesce: only the newest preview / thumbnail request matters
            let mut preview = None;
            let mut thumbs = None;
            let mut take = |j: Job| match j {
                Job::Preview { .. } => preview = Some(j),
                Job::Thumbs { .. } => thumbs = Some(j),
            };
            take(first);
            while let Ok(j) = jobs.try_recv() {
                take(j);
            }
            if let Some(Job::Preview { src, logical_w, params, generation, frame, overlay, draft }) = preview {
                let t = Instant::now();
                let full_w = logical_w as f32;
                let src = if draft {
                    match &halves {
                        Some((full, h)) if Arc::ptr_eq(full, &src) => h.clone(),
                        _ => {
                            let h = Arc::new(pipeline::half(&src));
                            halves = Some((src.clone(), h.clone()));
                            h
                        }
                    }
                } else {
                    src
                };
                let scale = full_w / src.w.max(1) as f32;
                let r = pipeline::process_cached(&src, &params, overlay.map(|o| o.0), &mut cache);
                let img = &r.img;
                let overlay = r.overlay.zip(overlay).map(|(a, (_, rgb))| overlay_image(&a, img.w, img.h, rgb));
                let hist = pipeline::histogram(img);
                let image = Arc::new(match &r.alpha {
                    Some(a) => to_color_image_cutout(img, a),
                    None => to_color_image(img),
                });
                let ms = t.elapsed().as_secs_f32() * 1000.0;
                let _ = out.send(Msg::Preview { generation, image, hist, ms, frame, overlay, scale, draft });
                ctx.request_repaint();
            }
            if let Some(Job::Thumbs { src, looks, ver }) = thumbs {
                use rayon::prelude::*;
                let images = looks.par_iter().map(|p| to_color_image(&pipeline::process(&src, p))).collect();
                let _ = out.send(Msg::Thumbs { ver, images });
                ctx.request_repaint();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Presets,
    Adjust,
    Masks,
    Retouch,
    Layers,
    Effects,
    Glitch,
    Crop,
}
const TABS: [(Tab, &str); 8] = [
    (Tab::Presets, "Presets"),
    (Tab::Adjust, "Adjust"),
    (Tab::Masks, "Masks"),
    (Tab::Retouch, "Retouch"),
    (Tab::Layers, "Layers"),
    (Tab::Effects, "Effects"),
    (Tab::Glitch, "Glitch"),
    (Tab::Crop, "Crop"),
];

/// What the preview texture shows, so overlays can be mapped onto it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Frame {
    /// The finished picture, border included.
    Output,
    /// The whole rotated frame, ignoring the crop (Crop tab).
    Uncropped,
    /// Cropped but without the border (Masks tab).
    Unbordered,
}

/// An in-progress drag on a mask.
#[derive(Clone)]
enum MaskDrag {
    Paint,
    Handle { id: u8, offset: Vec2, origin: Pos2, start: Shape },
}

/// An in-progress transform of a layer on the canvas.
#[derive(Clone)]
struct LayerDrag {
    /// 0 move, 1 scale (corner), 2 rotate
    kind: u8,
    start: Layer,
    origin: Pos2,
    /// Scaling: the point (in the layer's own frame, -1..1) that stays put.
    anchor: Vec2,
}

/// How close (screen pixels) the magnet starts pulling.
const MAGNET_PX: f32 = 10.0;

/// A layer's rectangle on screen, for the magnet.
fn layer_box(l: &Layer, map: &Map) -> crate::magnet::Box2 {
    let (w, h) = l.size(map.long());
    crate::magnet::Box2 { c: map.n2s([l.x, l.y]), half: vec2(w, h) * 0.5, rot: l.rotation.to_radians() }
}

/// Screen-space outline of a layer: centre, the four corners, and the rotate knob.
fn layer_frame(l: &Layer, map: &Map) -> (Pos2, [Pos2; 4], Pos2) {
    let c = map.n2s([l.x, l.y]);
    let (w, h) = l.size(map.long());
    let (s, co) = l.rotation.to_radians().sin_cos();
    let ax = vec2(co, s) * (w * 0.5);
    let ay = vec2(-s, co) * (h * 0.5);
    let corners = [c - ax - ay, c + ax - ay, c + ax + ay, c - ax + ay];
    let knob = c - ay - vec2(-s, co) * 26.0;
    (c, corners, knob)
}

/// Mid-edge stretch handles: (kind, position). 3 right, 4 left, 5 bottom, 6 top.
fn layer_edges(l: &Layer, map: &Map) -> [(u8, Pos2); 4] {
    let c = map.n2s([l.x, l.y]);
    let (w, h) = l.size(map.long());
    let (s, co) = l.rotation.to_radians().sin_cos();
    let ax = vec2(co, s) * (w * 0.5);
    let ay = vec2(-s, co) * (h * 0.5);
    [(3, c + ax), (4, c - ax), (5, c + ay), (6, c - ay)]
}

/// Snap `v` to the nearest of `targets` within `dist` screen pixels.
fn snap(v: f32, targets: &[f32], dist: f32) -> Option<f32> {
    targets.iter().copied().filter(|t| (t - v).abs() < dist).min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs()))
}

/// Is `p` inside the layer's (rotated) rectangle?
fn layer_contains(l: &Layer, map: &Map, p: Pos2) -> bool {
    let c = map.n2s([l.x, l.y]);
    let (w, h) = l.size(map.long());
    let (s, co) = l.rotation.to_radians().sin_cos();
    let d = p - c;
    let (u, v) = (d.x * co + d.y * s, -d.x * s + d.y * co);
    u.abs() <= w * 0.5 && v.abs() <= h * 0.5
}

/// Maps between normalised frame coordinates and the screen.
#[derive(Clone, Copy)]
struct Map {
    img: Rect,
    c: [f32; 4],
}

impl Map {
    fn n2s(&self, n: [f32; 2]) -> Pos2 {
        let c = self.c;
        pos2(
            self.img.left() + (n[0] - c[0]) / (c[2] - c[0]) * self.img.width(),
            self.img.top() + (n[1] - c[1]) / (c[3] - c[1]) * self.img.height(),
        )
    }
    fn s2n(&self, p: Pos2) -> [f32; 2] {
        let c = self.c;
        [
            c[0] + (p.x - self.img.left()) / self.img.width() * (c[2] - c[0]),
            c[1] + (p.y - self.img.top()) / self.img.height() * (c[3] - c[1]),
        ]
    }
    /// Size of the whole frame on screen.
    fn frame(&self) -> Vec2 {
        vec2(self.img.width() / (self.c[2] - self.c[0]), self.img.height() / (self.c[3] - self.c[1]))
    }
    /// Screen pixels per long-edge unit.
    fn long(&self) -> f32 {
        let f = self.frame();
        f.x.max(f.y)
    }
}

/// Brush size slider (0..1) → radius in long-edge units, perceptually spaced.
fn brush_radius(t: f32) -> f32 {
    0.004 * 75f32.powf(t.clamp(0.0, 1.0))
}

/// Screen positions of a shape's handles.
fn mask_handles(shape: &Shape, map: &Map) -> Vec<(u8, Pos2)> {
    match shape {
        Shape::Linear { a, b } => {
            let (a, b) = (map.n2s(*a), map.n2s(*b));
            vec![(0, a), (1, b), (2, a + (b - a) * 0.5)]
        }
        Shape::Radial { c, rx, ry, angle, .. } => {
            let (rx, ry) = (*rx, *ry);
            let cs = map.n2s(*c);
            let l = map.long();
            let ax = vec2(angle.cos(), angle.sin());
            let ay = vec2(-ax.y, ax.x);
            vec![(10, cs), (11, cs + ax * rx * l), (12, cs - ax * rx * l), (13, cs + ay * ry * l), (14, cs - ay * ry * l)]
        }
        _ => Vec::new(),
    }
}

/// New shape for a handle dragged to `p` (pointer + grab offset), `delta` since the grab.
fn drag_handle(start: &Shape, id: u8, p: Pos2, delta: Vec2, map: &Map) -> Shape {
    let f = map.frame();
    let dn = [delta.x / f.x, delta.y / f.y];
    let add = |q: [f32; 2]| [q[0] + dn[0], q[1] + dn[1]];
    match (start.clone(), id) {
        (Shape::Linear { b, .. }, 0) => Shape::Linear { a: map.s2n(p), b },
        (Shape::Linear { a, .. }, 1) => Shape::Linear { a, b: map.s2n(p) },
        (Shape::Linear { a, b }, _) => Shape::Linear { a: add(a), b: add(b) },
        (Shape::Radial { c, rx, ry, angle, feather }, 10) => Shape::Radial { c: add(c), rx, ry, angle, feather },
        (Shape::Radial { c, ry, feather, .. }, 11 | 12) => {
            let mut v = p - map.n2s(c);
            if id == 12 {
                v = -v;
            }
            Shape::Radial { c, rx: (v.length() / map.long()).max(0.005), ry, angle: v.y.atan2(v.x), feather }
        }
        (Shape::Radial { c, rx, angle, feather, .. }, _) => {
            let v = p - map.n2s(c);
            let ay = vec2(-angle.sin(), angle.cos());
            Shape::Radial { c, rx, ry: (v.dot(ay).abs() / map.long()).max(0.005), angle, feather }
        }
        (s, _) => s,
    }
}

/// Index of the object-selection point under `p`, if any.
fn seed_at(shape: &Shape, map: &Map, p: Pos2) -> Option<usize> {
    let Shape::Object { seeds, .. } = shape else { return None };
    seeds
        .iter()
        .enumerate()
        .map(|(i, s)| (i, map.n2s(s.p).distance(p)))
        .filter(|(_, d)| *d < 10.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Composition guides over `r`.
fn draw_guides(painter: &egui::Painter, r: Rect, thirds: f32, golden: bool) {
    if thirds > 0.01 {
        let st = Stroke::new(1.0, Color32::from_white_alpha((thirds * 110.0) as u8));
        for t in [1.0 / 3.0, 2.0 / 3.0] {
            let (x, y) = (r.min.x + r.width() * t, r.min.y + r.height() * t);
            painter.line_segment([pos2(x, r.min.y), pos2(x, r.max.y)], st);
            painter.line_segment([pos2(r.min.x, y), pos2(r.max.x, y)], st);
        }
    }
    if golden {
        let st = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 204, 0, 160));
        let phi = 0.618_034;
        for t in [1.0 - phi, phi] {
            let (x, y) = (r.min.x + r.width() * t, r.min.y + r.height() * t);
            painter.line_segment([pos2(x, r.min.y), pos2(x, r.max.y)], st);
            painter.line_segment([pos2(r.min.x, y), pos2(r.max.x, y)], st);
        }
    }
}

/// A guide line on the crop view for levelling the horizon: its centre as a
/// fraction of the uncropped preview, and its angle on screen (radians).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Horizon {
    c: [f32; 2],
    angle: f32,
}

impl Horizon {
    /// Centre and the two end handles on screen.
    fn points(&self, img: Rect) -> (Pos2, Pos2, Pos2) {
        let c = img.lerp_inside(vec2(self.c[0], self.c[1]));
        let d = vec2(self.angle.cos(), self.angle.sin()) * (img.width().min(img.height()) * 0.42).max(60.0);
        (c, c - d, c + d)
    }
    /// 0 centre, 1 / 2 ends.
    fn hit(&self, img: Rect, p: Pos2) -> Option<u8> {
        let (c, a, b) = self.points(img);
        [(0u8, c), (1, a), (2, b)].into_iter().map(|(id, q)| (id, q.distance(p))).filter(|h| h.1 < 14.0).min_by(|x, y| x.1.total_cmp(&y.1)).map(|h| h.0)
    }
}

/// Keep a line angle within (-90°, 90°]: a line has no direction.
fn line_angle(a: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, PI};
    let a = a.rem_euclid(PI);
    if a > FRAC_PI_2 { a - PI } else { a }
}

/// Which part of the crop rectangle is being dragged.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Grip {
    Move,
    Edge(i8, i8),
}

struct View {
    zoom: f32,
    pan: Vec2,
    dragging: bool,
}

pub struct App {
    doc: Option<Doc>,
    loading: Option<String>,
    params: Params,
    committed: Params,
    undo: Vec<Params>,
    redo: Vec<Params>,
    tab: usize,

    tex: Option<egui::TextureHandle>,
    orig_tex: Option<egui::TextureHandle>,
    hist: Histogram,
    generation: u64,
    shown_gen: u64,
    /// What the last preview job rendered: params, overlay, draft, detail edge (0 none), frame (0 none).
    last_sent: Option<(Params, Option<(usize, u32)>, bool, u32, Frame)>,
    /// Long edge the current zoom needs to look sharp, in source pixels.
    detail_need: f32,
    /// The sharper source for zooming in, and the edge being built.
    detail_src: Option<(PathBuf, u32, Arc<Img>)>,
    detail_req: Option<u32>,
    /// Zoom level and when it last changed, so the sharper source waits for the zoom to settle.
    detail_zoom: (f32, Instant),
    render_ms: f32,
    /// Duration of the last full-size render; slow ones get drafts while dragging.
    full_ms: f32,
    /// Preview pixels per texture pixel (2 while a draft is shown).
    tex_scale: f32,
    sent_at: Option<Instant>,
    tex_frame: Frame,
    overlay_tex: Option<egui::TextureHandle>,
    last_image: Option<Arc<egui::ColorImage>>,
    last_img_rect: Rect,
    mask_sel: Option<usize>,
    mask_drag: Option<MaskDrag>,
    show_overlay: bool,
    /// Keep the red overlay up while a mask is being shaped; it goes away
    /// once its adjustments are touched.
    overlay_hold: bool,
    brush_size: f32,
    brush_feather: f32,
    brush_flow: f32,
    brush_erase: bool,
    picking_hue: bool,
    /// Object selections: clicks place − (remove) points instead of + points.
    seed_remove: bool,
    guide_thirds: bool,
    guide_golden: bool,
    prefs: io::Prefs,
    settings_open: bool,
    /// The open project file, and the edit as it was last saved.
    project: Option<PathBuf>,
    saved_params: Option<Params>,
    pending_project: Option<Params>,
    layer_sel: Option<usize>,
    layer_drag: Option<LayerDrag>,
    /// Guides shown while a layer is snapped (screen x / y).
    snap_lines: (Option<f32>, Option<f32>),
    /// Guides along the edges the magnet pulled a layer onto.
    magnet_lines: Vec<[Pos2; 2]>,
    /// Layer being dragged in the list, for reordering.
    list_drag: Option<usize>,
    /// Inline rename in the mask / layer list: target, text, focus on next frame.
    renaming: Option<(Rename, String, bool)>,
    /// Recently opened photos and projects, newest first.
    recent: Vec<PathBuf>,
    layers_loading: usize,
    crop_drag: Option<(Grip, [f32; 4], Pos2)>,
    horizon: Option<Horizon>,
    horizon_drag: Option<(u8, Horizon, Pos2)>,
    /// Clone tool: where to copy from (normalised frame point), and the
    /// source-minus-brush offset, fixed by the first stroke after choosing it.
    clone_source: Option<[f32; 2]>,
    clone_offset: Option<[f32; 2]>,
    clone_painting: bool,
    /// The next click on the photo chooses the source.
    clone_pick_source: bool,
    clone_size: f32,
    clone_feather: f32,
    clone_opacity: f32,

    builtin: Vec<(String, Params)>,
    user_presets: Vec<(String, Params)>,
    thumbs: Vec<egui::TextureHandle>,
    thumbs_ver: u64,
    new_preset: String,

    view: View,

    export: ExportSettings,
    export_open: bool,
    exporting: bool,
    dialog_open: bool,
    toast: Option<(String, Option<PathBuf>, Instant)>,

    jobs: Sender<Job>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        k::setup_fonts(&cc.egui_ctx);
        k::setup_style(&cc.egui_ctx);
        let (jobs, jobs_rx) = channel();
        let (tx, rx) = channel();
        spawn_worker(cc.egui_ctx.clone(), jobs_rx, tx.clone());
        let saved = io::load_saved();
        let mut app = Self {
            doc: None,
            loading: None,
            params: Params::default(),
            committed: Params::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            tab: 1,
            tex: None,
            orig_tex: None,
            hist: Histogram::default(),
            generation: 0,
            shown_gen: 0,
            last_sent: None,
            render_ms: 0.0,
            full_ms: 0.0,
            detail_need: 0.0,
            detail_src: None,
            detail_req: None,
            detail_zoom: (1.0, Instant::now()),
            tex_scale: 1.0,
            sent_at: None,
            tex_frame: Frame::Output,
            overlay_tex: None,
            last_image: None,
            last_img_rect: Rect::NOTHING,
            mask_sel: None,
            mask_drag: None,
            show_overlay: false,
            overlay_hold: false,
            brush_size: 0.45,
            brush_feather: 0.6,
            brush_flow: 1.0,
            brush_erase: false,
            picking_hue: false,
            seed_remove: false,
            guide_thirds: saved.guide_thirds,
            guide_golden: saved.guide_golden,
            prefs: saved.prefs.clone(),
            settings_open: false,
            project: None,
            saved_params: None,
            pending_project: None,
            layer_sel: None,
            layer_drag: None,
            snap_lines: (None, None),
            magnet_lines: Vec::new(),
            list_drag: None,
            renaming: None,
            recent: saved.recent.clone(),
            layers_loading: 0,
            crop_drag: None,
            horizon: None,
            horizon_drag: None,
            clone_source: None,
            clone_offset: None,
            clone_painting: false,
            clone_pick_source: false,
            clone_size: 0.35,
            clone_feather: 0.5,
            clone_opacity: 1.0,
            builtin: pipeline::builtin_presets(),
            user_presets: saved.presets,
            thumbs: Vec::new(),
            thumbs_ver: 0,
            new_preset: String::new(),
            view: View { zoom: 1.0, pan: Vec2::ZERO, dragging: false },
            export: saved.export,
            export_open: false,
            exporting: false,
            dialog_open: false,
            toast: None,
            jobs,
            tx,
            rx,
        };
        app.apply_prefs();
        if let Some(p) = initial {
            if io::is_look_file(&p) {
                app.import_look_files(&[p]);
            } else {
                app.open_path(p, &cc.egui_ctx);
            }
        }
        app
    }

    fn persist(&self) {
        io::store_saved(&io::Saved {
            presets: self.user_presets.clone(),
            export: self.export.clone(),
            guide_thirds: self.guide_thirds,
            guide_golden: self.guide_golden,
            prefs: self.prefs.clone(),
            recent: self.recent.clone(),
        });
    }

    /// Push preferences into the parts of the app that read them globally.
    fn apply_prefs(&self) {
        k::set_reduce_motion(self.prefs.reduce_motion);
    }

    /// Rebuild the working preview at the preferred size (Settings → Preview).
    fn rebuild_preview(&self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let (full, path, edge) = (doc.full.clone(), doc.path.clone(), self.prefs.preview_edge());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let img = Arc::new(io::downsized(&full, edge));
            let _ = tx.send(Msg::Repreviewed(path, img));
            ctx.request_repaint();
        });
    }

    // --- projects ------------------------------------------------------------

    fn unsaved(&self) -> bool {
        self.doc.is_some() && self.saved_params.as_ref() != Some(&self.params) && (self.project.is_some() || self.params != Params { seed: self.params.seed, ..Params::default() })
    }

    fn save_project(&mut self, path: PathBuf) {
        let Some(doc) = &self.doc else { return };
        match io::write_project(&path, &doc.path, &self.params) {
            Ok(()) => {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                self.toast = Some((format!("Saved {name}"), path.parent().map(|d| d.to_path_buf()), Instant::now()));
                self.project = Some(path.clone());
                self.saved_params = Some(self.params.clone());
                self.remember_recent(path);
            }
            Err(e) => self.toast = Some((format!("Couldn't save: {e}"), None, Instant::now())),
        }
    }

    /// Ctrl+S: save in place, or ask where the first time (or always, with `as_new`).
    fn save(&mut self, as_new: bool, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        if !as_new && let Some(p) = self.project.clone() {
            return self.save_project(p);
        }
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let stem = doc.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "photo".into());
        let name = format!("{stem}.{}", io::PROJECT_EXT);
        let dir = self.project.as_ref().and_then(|p| p.parent()).or_else(|| doc.path.parent()).map(|p| p.to_path_buf());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new().set_title("Save Project").set_file_name(&name).add_filter("Still Project", &[io::PROJECT_EXT]);
            if let Some(d) = dir {
                dlg = dlg.set_directory(d);
            }
            let _ = tx.send(match dlg.save_file() {
                Some(p) => Msg::SaveProjectTo(p),
                None => Msg::DialogClosed,
            });
            ctx.request_repaint();
        });
    }

    // --- sharing looks -----------------------------------------------------

    fn export_looks(&mut self, looks: Vec<(String, Params)>, ctx: &egui::Context) {
        if self.dialog_open || looks.is_empty() {
            return;
        }
        self.dialog_open = true;
        let name = if looks.len() == 1 { io::look_file_name(&looks[0].0) } else { io::look_file_name("Still Presets") };
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let dlg = rfd::FileDialog::new()
                .set_title(if looks.len() == 1 { "Export Preset" } else { "Export Presets" })
                .set_file_name(&name)
                .add_filter("Still Preset", &[io::LOOK_EXT]);
            let msg = match dlg.save_file() {
                Some(mut p) => {
                    if !io::is_look_file(&p) {
                        p.set_extension(io::LOOK_EXT);
                    }
                    match io::write_looks(&p, &looks) {
                        Ok(()) => {
                            let what = if looks.len() == 1 { format!("“{}”", looks[0].0) } else { format!("{} presets", looks.len()) };
                            Msg::Info(format!("Exported {what}"), p.parent().map(|d| d.to_path_buf()))
                        }
                        Err(e) => Msg::Info(format!("Couldn't export: {e}"), None),
                    }
                }
                None => Msg::DialogClosed,
            };
            let _ = tx.send(msg);
            let _ = tx.send(Msg::DialogClosed);
            ctx.request_repaint();
        });
    }

    fn pick_looks_to_import(&mut self, ctx: &egui::Context) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let dlg = rfd::FileDialog::new().set_title("Import Presets").add_filter("Still Preset", &io::dialog_exts(io::LOOK_EXTS));
            match dlg.pick_files() {
                Some(paths) => {
                    let (looks, errs) = read_look_files(&paths);
                    let _ = tx.send(Msg::LooksImported(looks, errs));
                }
                None => {}
            }
            let _ = tx.send(Msg::DialogClosed);
            ctx.request_repaint();
        });
    }

    fn import_look_files(&mut self, paths: &[PathBuf]) {
        let (looks, errs) = read_look_files(paths);
        self.add_imported(looks, errs);
    }

    fn add_imported(&mut self, looks: Vec<(String, Params)>, errs: Vec<String>) {
        let n = looks.len();
        for (name, p) in looks {
            // keep names unique so nothing gets silently shadowed
            let taken = |s: &str, me: &Self| me.builtin.iter().chain(me.user_presets.iter()).any(|(n, _)| n.eq_ignore_ascii_case(s));
            let base = if name.is_empty() { "Imported".to_string() } else { name };
            let mut name = base.clone();
            let mut i = 2;
            while taken(&name, self) {
                name = format!("{base} {i}");
                i += 1;
            }
            self.user_presets.push((name, p));
        }
        if n > 0 {
            self.persist();
            self.request_thumbs();
            self.tab = 0;
        }
        let msg = match (n, errs.first()) {
            (0, Some(e)) => format!("Couldn't import: {e}"),
            (0, None) => return,
            (1, None) => "Imported 1 preset".to_string(),
            (n, None) => format!("Imported {n} presets"),
            (n, Some(_)) => format!("Imported {n} presets, {} file{} skipped", errs.len(), if errs.len() == 1 { "" } else { "s" }),
        };
        self.toast = Some((msg, None, Instant::now()));
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        if io::is_project_file(&path) {
            match io::read_project(&path) {
                Ok((photo, params)) => {
                    self.pending_project = Some(params);
                    self.project = Some(path);
                    return self.open_path(photo, ctx);
                }
                Err(e) => {
                    self.toast = Some((format!("Couldn't open project: {e}"), None, Instant::now()));
                    return;
                }
            }
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.loading = Some(path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
        let edge = self.prefs.preview_edge();
        std::thread::spawn(move || {
            let r = io::load(&path, edge);
            let _ = tx.send(Msg::Loaded(r));
            ctx.request_repaint();
        });
    }

    /// Put a photo or project at the top of the Recent list on the start screen.
    fn remember_recent(&mut self, p: PathBuf) {
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(RECENT_MAX);
        self.persist();
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let dir = self.doc.as_ref().and_then(|d| d.path.parent().map(|p| p.to_path_buf()));
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new()
                .set_title("Open Photo or Project")
                .add_filter("Photos & Projects", &{
                    let mut v = vec![io::PROJECT_EXT];
                    v.extend_from_slice(io::IMAGE_EXTS);
                    v.extend_from_slice(io::RAW_EXTS);
                    io::dialog_exts(&v)
                })
                .add_filter("Still Project", &io::dialog_exts(&[io::PROJECT_EXT]));
            if let Some(d) = dir {
                dlg = dlg.set_directory(d);
            }
            let _ = tx.send(match dlg.pick_file() {
                Some(p) => Msg::Picked(p),
                None => Msg::DialogClosed,
            });
            ctx.request_repaint();
        });
    }

    fn pick_export_path(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let stem = doc.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "photo".into());
        let name = format!("{stem}{}.{}", self.export.suffix, self.export.format.ext());
        let dir = doc.path.parent().map(|p| p.to_path_buf());
        let fmt = self.export.format;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new()
                .set_title("Export Photo")
                .set_file_name(&name)
                .add_filter(fmt.label(), &[fmt.ext()]);
            if let Some(d) = dir {
                dlg = dlg.set_directory(d);
            }
            let _ = tx.send(match dlg.save_file() {
                Some(p) => Msg::ExportTo(p),
                None => Msg::DialogClosed,
            });
            ctx.request_repaint();
        });
    }

    fn start_export(&mut self, mut path: PathBuf, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        if path.extension().is_none() {
            path.set_extension(self.export.format.ext());
        }
        self.exporting = true;
        // Doc isn't Clone (it owns big buffers), so share what the exporter needs.
        let job_doc = Doc {
            path: doc.path.clone(),
            name: doc.name.clone(),
            full: doc.full.clone(),
            preview: doc.preview.clone(),
            thumb_src: doc.thumb_src.clone(),
            icc: doc.icc.clone(),
            exif: doc.exif.clone(),
            source_desc: String::new(),
        };
        let params = self.params.clone();
        let settings = self.export.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = io::export(&job_doc, &params, &settings, &path);
            let _ = tx.send(Msg::Exported(r));
            ctx.request_repaint();
        });
        self.persist();
    }

    fn all_looks(&self) -> Vec<(String, Params, bool)> {
        self.builtin
            .iter()
            .map(|(n, p)| (n.clone(), p.clone(), false))
            .chain(self.user_presets.iter().map(|(n, p)| (n.clone(), p.clone(), true)))
            .collect()
    }

    fn request_thumbs(&mut self) {
        if let Some(doc) = &self.doc {
            self.thumbs_ver += 1;
            let looks = self.all_looks().into_iter().map(|(_, p, _)| p).collect();
            let _ = self.jobs.send(Job::Thumbs { src: doc.thumb_src.clone(), looks, ver: self.thumbs_ver });
        }
    }

    // -----------------------------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Preview { generation, image, hist, ms, frame, overlay, scale, draft } => {
                    if generation >= self.shown_gen {
                        self.shown_gen = generation;
                        self.tex_frame = frame;
                        self.tex_scale = scale;
                        if !draft {
                            self.full_ms = ms;
                        }
                        self.overlay_tex = overlay.map(|o| match self.overlay_tex.take() {
                            Some(mut t) => {
                                t.set(o, egui::TextureOptions::LINEAR);
                                t
                            }
                            None => ctx.load_texture("mask_overlay", o, egui::TextureOptions::LINEAR),
                        });
                        self.last_image = Some(image.clone());
                        self.hist = hist;
                        self.render_ms = ms;
                        match &mut self.tex {
                            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                            None => self.tex = Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR)),
                        }
                    }
                }
                Msg::Thumbs { ver, images } => {
                    if ver == self.thumbs_ver {
                        self.thumbs = images
                            .into_iter()
                            .enumerate()
                            .map(|(i, im)| ctx.load_texture(format!("thumb{i}"), im, egui::TextureOptions::LINEAR))
                            .collect();
                    }
                }
                Msg::Loaded(Ok(doc)) => {
                    self.loading = None;
                    self.orig_tex = Some(ctx.load_texture("original", to_color_image(&doc.preview), egui::TextureOptions::LINEAR));
                    let portrait = doc.preview.h > doc.preview.w;
                    self.doc = Some(doc);
                    self.tex = None;
                    self.last_sent = None;
                    self.detail_src = None;
                    self.detail_req = None;
                    // keep the current look (handy for batches) unless asked not to; reset geometry
                    if !self.prefs.keep_look {
                        self.params = Params { seed: self.params.seed, ..Params::default() };
                    }
                    let d = Params::default();
                    self.params.rotate = d.rotate;
                    self.params.flip_h = d.flip_h;
                    self.params.straighten = d.straighten;
                    self.params.aspect = d.aspect;
                    self.params.aspect_portrait = portrait;
                    self.params.crop = d.crop;
                    // masks belong to a particular photo
                    self.params.masks.clear();
                    self.params.layers.clear();
                    self.params.layer_groups.clear();
                    self.params.retouch.clear();
                    self.layer_sel = None;
                    self.mask_sel = None;
                    match self.pending_project.take() {
                        Some(p) => {
                            self.params = p;
                            self.saved_params = Some(self.params.clone());
                        }
                        None => {
                            self.project = None;
                            self.saved_params = None;
                        }
                    }
                    self.committed = self.params.clone();
                    self.undo.clear();
                    self.redo.clear();
                    if let Some(p) = self.project.clone().or_else(|| self.doc.as_ref().map(|d| d.path.clone())) {
                        self.remember_recent(p);
                    }
                    self.view = View { zoom: 1.0, pan: Vec2::ZERO, dragging: false };
                    self.request_thumbs();
                }
                Msg::Loaded(Err(e)) => {
                    self.loading = None;
                    self.pending_project = None;
                    self.toast = Some((format!("Couldn't open photo: {e}"), None, Instant::now()));
                }
                Msg::Picked(p) => {
                    self.dialog_open = false;
                    self.open_path(p, ctx);
                }
                Msg::ExportTo(p) => {
                    self.dialog_open = false;
                    self.start_export(p, ctx);
                }
                Msg::DialogClosed => self.dialog_open = false,
                Msg::LooksImported(looks, errs) => self.add_imported(looks, errs),
                Msg::LayerFiles(paths) => self.load_layers(paths, ctx),
                Msg::SaveProjectTo(mut p) => {
                    self.dialog_open = false;
                    if !io::is_project_file(&p) {
                        p.set_extension(io::PROJECT_EXT);
                    }
                    self.save_project(p);
                }
                Msg::LayerReady(r) => {
                    self.layers_loading = self.layers_loading.saturating_sub(1);
                    match r {
                        Ok((path, aspect)) => self.push_layer(path, aspect),
                        Err(e) => self.toast = Some((format!("Couldn't add photo: {e}"), None, Instant::now())),
                    }
                }
                Msg::Info(text, dir) => self.toast = Some((text, dir, Instant::now())),
                Msg::DetailReady(path, edge, img) => {
                    if self.doc.as_ref().map(|d| d.path == path).unwrap_or(false) && self.detail_req == Some(edge) {
                        self.detail_src = Some((path, edge, img));
                    }
                }
                Msg::Repreviewed(path, img) => {
                    if let Some(doc) = &mut self.doc
                        && doc.path == path
                    {
                        self.orig_tex = Some(ctx.load_texture("original", to_color_image(&img), egui::TextureOptions::LINEAR));
                        doc.preview = img;
                        self.last_sent = None;
                    }
                }
                Msg::Exported(r) => {
                    self.exporting = false;
                    match r {
                        Ok(p) => {
                            self.export_open = false;
                            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                            self.toast = Some((format!("Exported {name}"), p.parent().map(|d| d.to_path_buf()), Instant::now()));
                        }
                        Err(e) => self.toast = Some((format!("Export failed: {e}"), None, Instant::now())),
                    }
                }
            }
        }
    }

    /// While the Crop tab is open the canvas shows the whole frame with the crop as an overlay.
    fn crop_mode(&self) -> bool {
        self.doc.is_some() && TABS[self.tab].0 == Tab::Crop
    }

    /// Size of the rotated frame the crop rect is expressed in (preview pixels).
    fn frame_dims(&self) -> (f32, f32) {
        let Some(d) = &self.doc else { return (1.0, 1.0) };
        let (w, h) = (d.preview.w as f32, d.preview.h as f32);
        if self.params.rotate % 2 == 1 { (h, w) } else { (w, h) }
    }

    fn render_frame(&self) -> Frame {
        match TABS[self.tab].0 {
            Tab::Crop => Frame::Uncropped,
            Tab::Masks | Tab::Retouch | Tab::Layers => Frame::Unbordered,
            _ => Frame::Output,
        }
    }

    fn selected_mask(&self) -> Option<usize> {
        self.mask_sel.filter(|i| *i < self.params.masks.len())
    }

    /// The red mask overlay shows while shaping a mask (from adding or
    /// selecting it until its adjustments are touched), while O is held, or
    /// always when switched on.
    fn overlay_wanted(&self, ctx: &egui::Context) -> Option<(usize, u32)> {
        let i = self.selected_mask()?;
        if TABS[self.tab].0 != Tab::Masks {
            return None;
        }
        let text_focus = ctx.memory(|m| m.focused().is_some());
        let key = !text_focus && ctx.input(|inp| inp.key_down(egui::Key::O));
        // a mask with its own label colour shows its wash in that colour
        let rgb = label_rgb(self.params.masks[i].color).unwrap_or(OVERLAY_COLORS[self.prefs.overlay_color.min(2)]);
        (self.show_overlay || self.mask_drag.is_some() || self.overlay_hold || key).then_some((i, rgb))
    }

    fn send_preview(&mut self, ctx: &egui::Context) {
        let overlay = self.overlay_wanted(ctx);
        let Some(doc) = &self.doc else { return };
        let frame = self.render_frame();
        let mut params = self.params.clone();
        if frame == Frame::Uncropped {
            params.crop = pipeline::FULL_CROP;
        }
        if frame != Frame::Output {
            params.border = 0.0;
        }
        // names and label colours don't change the picture
        for m in &mut params.masks {
            m.name.clear();
            m.color = 0;
        }
        params.layers = params.shown_layers().into_owned();
        params.layer_groups.clear();
        for l in &mut params.layers {
            l.name.clear();
            l.color = 0;
            l.locked = false;
            l.group = 0;
        }
        // While a slider or brush is held, slow renders are drafted at half size so
        // the preview keeps up; the full-size render follows on release.
        let interacting = ctx.input(|i| i.pointer.any_down()) && self.full_ms > 45.0;

        // Zoomed in past the preview's resolution: render from a sharper source
        // once it's ready (built in the background, then reused while zoomed).
        let src_long = doc.preview.w.max(doc.preview.h) as f32;
        let full_long = doc.full.width().max(doc.full.height()) as f32;
        let want = (self.detail_need > src_long * 1.25 && full_long > src_long * 1.1).then(|| {
            let e = ((self.detail_need / 800.0).ceil() * 800.0).min(full_long).min(DETAIL_MAX as f32);
            e as u32
        });
        if (self.view.zoom - self.detail_zoom.0).abs() > 1e-4 {
            self.detail_zoom = (self.view.zoom, Instant::now());
        }
        let settle = std::time::Duration::from_millis(250);
        let zoom_settled = self.detail_zoom.1.elapsed() > settle;
        if want.is_some() && !zoom_settled {
            ctx.request_repaint_after(settle);
        }
        let mut detail = None;
        match want {
            Some(e) => match &self.detail_src {
                Some((path, edge, img)) if *path == doc.path && *edge == e => detail = Some(img.clone()),
                _ if self.detail_req != Some(e) && zoom_settled => {
                    self.detail_req = Some(e);
                    let (full, path, tx, ctx) = (doc.full.clone(), doc.path.clone(), self.tx.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let img = Arc::new(io::downsized(&full, e));
                        let _ = tx.send(Msg::DetailReady(path, e, img));
                        ctx.request_repaint();
                    });
                }
                _ => {}
            },
            None => {
                // zoomed back out: let the big source go
                self.detail_src = None;
                self.detail_req = None;
            }
        }
        let detail = detail.filter(|_| !interacting);
        let detail_edge = if detail.is_some() { want.unwrap_or(0) } else { 0 };

        if let Some((p, o, was_draft, de, f)) = &self.last_sent
            && *p == params
            && *o == overlay
            && *de == detail_edge
            && *f == frame
            && (!*was_draft || interacting)
        {
            return;
        }
        self.generation += 1;
        self.last_sent = Some((params.clone(), overlay, interacting, detail_edge, frame));
        self.sent_at = Some(Instant::now());
        let src = detail.unwrap_or_else(|| doc.preview.clone());
        let logical_w = doc.preview.w;
        let _ = self.jobs.send(Job::Preview { src, logical_w, params, generation: self.generation, frame, overlay, draft: interacting });
    }

    /// Snapshot the parameters into the undo stack once an interaction ends.
    fn commit_history(&mut self, ctx: &egui::Context) {
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        if !pointer_down && self.params != self.committed {
            self.undo.push(std::mem::replace(&mut self.committed, self.params.clone()));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    fn do_undo(&mut self) {
        if let Some(p) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.params, p));
            self.committed = self.params.clone();
        }
    }

    fn do_redo(&mut self) {
        if let Some(p) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.params, p));
            self.committed = self.params.clone();
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let text_focus = ctx.memory(|m| m.focused().is_some());
        let sc = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        if sc(Modifiers::COMMAND, Key::O) {
            self.pick_file(ctx);
        }
        if self.doc.is_some() && sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S) {
            self.save(true, ctx);
        }
        if self.doc.is_some() && sc(Modifiers::COMMAND, Key::S) {
            self.save(false, ctx);
        }
        if self.doc.is_some() && sc(Modifiers::COMMAND, Key::E) {
            self.export_open = true;
        }
        // while typing (e.g. renaming a mask) undo belongs to the text field
        if !text_focus && (sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || sc(Modifiers::COMMAND, Key::Y)) {
            self.do_redo();
        }
        if !text_focus && sc(Modifiers::COMMAND, Key::Z) {
            self.do_undo();
        }
        if sc(Modifiers::COMMAND, Key::Num0) {
            self.view.zoom = 1.0;
            self.view.pan = Vec2::ZERO;
        }
        if sc(Modifiers::COMMAND, Key::Comma) {
            self.settings_open = !self.settings_open;
        }
        if self.export_open && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.export_open = false;
        }
        if self.picking_hue && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.picking_hue = false;
        }
        if self.settings_open && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.settings_open = false;
            self.persist();
        }
        if !text_focus {
            for (i, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8].into_iter().enumerate() {
                if ctx.input(|inp| inp.key_pressed(key) && inp.modifiers.is_none()) {
                    self.tab = i;
                }
            }
            if TABS[self.tab].0 == Tab::Layers
                && let Some(i) = self.selected_layer()
                && !self.params.layers[i].locked
                && ctx.input(|inp| inp.key_pressed(Key::Delete) || inp.key_pressed(Key::Backspace))
            {
                self.params.layers.remove(i);
                self.layer_sel = None;
            }
            // arrow keys nudge the selected layer by a screen pixel (Shift: ten)
            if TABS[self.tab].0 == Tab::Layers
                && let Some(i) = self.selected_layer()
                && !self.params.layers[i].locked
            {
                let f = self.mask_map(self.last_img_rect).frame();
                let step = if ctx.input(|inp| inp.modifiers.shift) { 10.0 } else { 1.0 };
                let mut d = Vec2::ZERO;
                for (key, v) in [(Key::ArrowLeft, vec2(-1.0, 0.0)), (Key::ArrowRight, vec2(1.0, 0.0)), (Key::ArrowUp, vec2(0.0, -1.0)), (Key::ArrowDown, vec2(0.0, 1.0))] {
                    if ctx.input(|inp| inp.key_pressed(key)) {
                        d += v * step;
                    }
                }
                if d != Vec2::ZERO && f.x > 0.0 && f.y > 0.0 {
                    let l = &mut self.params.layers[i];
                    l.x += d.x / f.x;
                    l.y += d.y / f.y;
                }
            }
            if TABS[self.tab].0 == Tab::Masks
                && let Some(i) = self.selected_mask()
            {
                if ctx.input(|inp| inp.key_pressed(Key::Delete) || inp.key_pressed(Key::Backspace)) {
                    self.params.masks.remove(i);
                    self.mask_sel = None;
                }
                if ctx.input(|inp| inp.key_pressed(Key::OpenBracket)) {
                    self.brush_size = (self.brush_size - 0.05).max(0.0);
                }
                if ctx.input(|inp| inp.key_pressed(Key::CloseBracket)) {
                    self.brush_size = (self.brush_size + 0.05).min(1.0);
                }
            }
            if TABS[self.tab].0 == Tab::Retouch {
                if ctx.input(|inp| inp.key_pressed(Key::OpenBracket)) {
                    self.clone_size = (self.clone_size - 0.05).max(0.0);
                }
                if ctx.input(|inp| inp.key_pressed(Key::CloseBracket)) {
                    self.clone_size = (self.clone_size + 0.05).min(1.0);
                }
                if self.clone_pick_source && ctx.input(|inp| inp.key_pressed(Key::Escape)) {
                    self.clone_pick_source = false;
                }
            }
        }
    }

    /// Ctrl+V: an image on the clipboard (a screenshot, an image copied in a
    /// browser) or copied image files become layers; with no photo open, the
    /// first pasted file opens.
    fn handle_paste(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (images, texts): (Vec<Arc<egui::ColorImage>>, Vec<String>) = ctx.input(|i| {
            let mut im = Vec::new();
            let mut tx = Vec::new();
            for e in &i.events {
                match e {
                    egui::Event::PasteImage(img) => im.push(img.clone()),
                    egui::Event::Paste(t) => tx.push(t.clone()),
                    _ => {}
                }
            }
            (im, tx)
        });
        let files: Vec<PathBuf> = texts.iter().flat_map(|t| pasted_paths(t)).collect();
        if !files.is_empty() {
            if self.doc.is_some() {
                self.load_layers(files, ctx);
            } else if let Some(p) = files.into_iter().next() {
                self.open_path(p, ctx);
            }
            return;
        }
        let Some(img) = images.into_iter().next() else { return };
        // a layer is a file on disk so projects can find it again
        let dir = io::config_dir().join("pasted");
        let (tx, ctx2, has_doc) = (self.tx.clone(), ctx.clone(), self.doc.is_some());
        std::thread::spawn(move || {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let path = dir.join(format!("pasted-{stamp}.png"));
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
            let r = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| {
                image::save_buffer(&path, &rgba, img.size[0] as u32, img.size[1] as u32, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())
            });
            let _ = match r {
                Ok(()) if has_doc => tx.send(Msg::LayerFiles(vec![path])),
                Ok(()) => tx.send(Msg::Picked(path)),
                Err(e) => tx.send(Msg::Info(format!("Couldn't paste the image: {e}"), None)),
            };
            ctx2.request_repaint();
        });
    }

    fn handle_drop(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        let (looks, photos): (Vec<PathBuf>, Vec<PathBuf>) = dropped.into_iter().partition(|p| io::is_look_file(p));
        if !looks.is_empty() {
            self.import_look_files(&looks);
        }
        if let Some(proj) = photos.iter().find(|p| io::is_project_file(p)).cloned() {
            self.open_path(proj, ctx);
        } else if self.doc.is_some() && !photos.is_empty() {
            // onto an open photo: place them on top as layers
            self.load_layers(photos, ctx);
        } else if let Some(p) = photos.into_iter().next() {
            self.open_path(p, ctx);
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        let c = k::WINDOW;
        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.shortcuts(&ctx);
        self.handle_drop(&ctx);
        self.handle_paste(&ctx);
        // a rename only lives as long as its list is on screen
        let list_shown = match self.renaming {
            Some((Rename::Mask(i), ..)) => TABS[self.tab].0 == Tab::Masks && i < self.params.masks.len(),
            Some((Rename::Layer(i), ..)) => TABS[self.tab].0 == Tab::Layers && i < self.params.layers.len(),
            Some((Rename::Group(id), ..)) => TABS[self.tab].0 == Tab::Layers && self.params.layer_groups.iter().any(|g| g.id == id),
            None => true,
        };
        if !list_shown || self.doc.is_none() {
            self.renaming = None;
        }

        egui::Panel::top("toolbar")
            .exact_size(52.0)
            .frame(egui::Frame::NONE.fill(k::WINDOW).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| self.toolbar(ui));

        egui::Panel::right("sidebar")
            .exact_size(320.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(k::SIDEBAR).inner_margin(egui::Margin { left: 14, right: 14, top: 14, bottom: 0 }))
            .show(ui, |ui| self.sidebar(ui));

        let canvas_bg = CANVAS_COLORS[self.prefs.canvas.min(2)];
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(canvas_bg)).show(ui, |ui| self.canvas(ui));

        self.export_sheet(&ctx);
        self.settings_sheet(&ctx);

        self.commit_history(&ctx);
        self.send_preview(&ctx);
    }

    fn on_exit(&mut self) {
        self.persist();
    }
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

impl App {
    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        // how far the buttons reach on each side, so the title can stay clear of them
        let (mut left_end, mut right_start) = (rect.left(), rect.right());
        ui.horizontal_centered(|ui| {
            if k::button(ui, "Open", Btn::Secondary, true).on_hover_text("Ctrl+O: a photo or a .still project").clicked() {
                self.pick_file(&ctx);
            }
            let has_doc = self.doc.is_some();
            if k::button(ui, "Save", Btn::Plain, has_doc).on_hover_text("Ctrl+S: save the edit as a .still project (Ctrl+Shift+S: Save As)").clicked() {
                self.save(false, &ctx);
            }
            ui.add_space(4.0);
            if k::button(ui, "Undo", Btn::Plain, !self.undo.is_empty()).on_hover_text("Ctrl+Z").clicked() {
                self.do_undo();
            }
            if k::button(ui, "Redo", Btn::Plain, !self.redo.is_empty()).on_hover_text("Ctrl+Shift+Z").clicked() {
                self.do_redo();
            }
            left_end = ui.min_rect().right();

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if k::button(ui, "Export", Btn::Primary, has_doc).on_hover_text("Ctrl+E").clicked() {
                    self.export_open = true;
                }
                ui.add_space(2.0);
                if k::button(ui, "Settings", Btn::Plain, true).on_hover_text("Ctrl+,").clicked() {
                    self.settings_open = true;
                }
                let modified = self.params != Params { seed: self.params.seed, ..Params::default() };
                if k::button(ui, "Revert", Btn::Plain, has_doc && modified).on_hover_text("Reset all edits").clicked() {
                    self.params = Params { seed: self.params.seed, ..Params::default() };
                }
                right_start = ui.min_rect().left();
            });
        });

        // centred title, independent of the button widths
        let painter = ui.painter();
        let (title, sub) = match (&self.doc, &self.loading) {
            (_, Some(name)) => (name.clone(), "Opening…".to_string()),
            (Some(d), None) => {
                let mut sub = d.source_desc.clone();
                if let Some(p) = &self.project {
                    sub = format!("{} · {sub}", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                }
                if self.unsaved() {
                    sub.push_str(" · Edited");
                }
                (d.name.clone(), sub)
            }
            (None, None) => ("Still".to_string(), String::new()),
        };
        let cy = rect.center().y;
        // centred, and cut short with an ellipsis before it reaches a button
        let cx = rect.center().x;
        let max_w = 2.0 * (cx - left_end).min(right_start - cx) - 32.0;
        if max_w < 24.0 {
            return;
        }
        let full = elided(ui, &title, k::font_semibold(13.0), k::LABEL, max_w);
        let title_g = full.clone();
        let rows: Vec<Arc<egui::Galley>> =
            if sub.is_empty() { vec![title_g] } else { vec![title_g, elided(ui, &sub, k::font(11.0), k::LABEL3, max_w)] };
        let ys: &[f32] = if rows.len() == 1 { &[0.0] } else { &[-7.0, 9.0] };
        for (g, dy) in rows.into_iter().zip(ys) {
            let pos = pos2(cx - g.size().x / 2.0, cy + dy - g.size().y / 2.0);
            painter.galley(pos, g, k::LABEL);
        }
        if full.elided {
            // the whole name on hover
            let r = Rect::from_center_size(pos2(cx, cy - if sub.is_empty() { 0.0 } else { 7.0 }), full.size());
            ui.interact(r, Id::unique("toolbar_title"), Sense::hover()).on_hover_text(title);
        }
    }
}

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

/// Apple's momentum projection (scroll-view deceleration).
/// Which grip of the crop rect `r` sits under `p` (corners win over edges).
fn grip_at(r: Rect, p: Pos2) -> Option<Grip> {
    let t = 14.0;
    if !r.expand(t).contains(p) {
        return None;
    }
    let x = if (p.x - r.left()).abs() < t { -1 } else if (p.x - r.right()).abs() < t { 1 } else { 0 };
    let y = if (p.y - r.top()).abs() < t { -1 } else if (p.y - r.bottom()).abs() < t { 1 } else { 0 };
    if x == 0 && y == 0 {
        return r.contains(p).then_some(Grip::Move);
    }
    Some(Grip::Edge(x, y))
}

/// Apply a drag of `d` (normalised to the frame) to the crop `c`, honouring an
/// optional pixel aspect `ratio` on a `w × h` frame.
fn drag_crop(c: [f32; 4], g: Grip, d: Vec2, ratio: Option<f32>, w: f32, h: f32) -> [f32; 4] {
    let mut r = [c[0] * w, c[1] * h, c[2] * w, c[3] * h];
    let (dx, dy) = (d.x * w, d.y * h);
    let min = 0.04 * w.max(h);
    match (g, ratio) {
        (Grip::Move, _) => {
            let (cw, ch) = (r[2] - r[0], r[3] - r[1]);
            let x0 = (r[0] + dx).clamp(0.0, w - cw);
            let y0 = (r[1] + dy).clamp(0.0, h - ch);
            r = [x0, y0, x0 + cw, y0 + ch];
        }
        (Grip::Edge(mx, my), None) => {
            match mx {
                -1 => r[0] = (r[0] + dx).clamp(0.0, r[2] - min),
                1 => r[2] = (r[2] + dx).clamp(r[0] + min, w),
                _ => {}
            }
            match my {
                -1 => r[1] = (r[1] + dy).clamp(0.0, r[3] - min),
                1 => r[3] = (r[3] + dy).clamp(r[1] + min, h),
                _ => {}
            }
        }
        (Grip::Edge(mx, my), Some(q)) if mx != 0 && my != 0 => {
            // corner: the opposite corner stays put
            let (sx, sy) = (mx as f32, my as f32);
            let ax = if mx > 0 { r[0] } else { r[2] };
            let ay = if my > 0 { r[1] } else { r[3] };
            let px = if mx > 0 { r[2] } else { r[0] } + dx;
            let py = if my > 0 { r[3] } else { r[1] } + dy;
            let maxw = if mx > 0 { w - ax } else { ax };
            let maxh = if my > 0 { h - ay } else { ay };
            let cw = ((px - ax) * sx).max((py - ay) * sy * q).max(min).min(maxw).min(maxh * q);
            let ch = cw / q;
            let (x0, y0) = (if mx > 0 { ax } else { ax - cw }, if my > 0 { ay } else { ay - ch });
            r = [x0, y0, x0 + cw, y0 + ch];
        }
        (Grip::Edge(mx, 0), Some(q)) => {
            // side edge: grows symmetrically in the other axis
            let ax = if mx > 0 { r[0] } else { r[2] };
            let px = if mx > 0 { r[2] } else { r[0] } + dx;
            let maxw = if mx > 0 { w - ax } else { ax };
            let cw = ((px - ax) * mx as f32).max(min).min(maxw).min(h * q);
            let ch = cw / q;
            let cy = ((r[1] + r[3]) * 0.5).clamp(ch * 0.5, h - ch * 0.5);
            let x0 = if mx > 0 { ax } else { ax - cw };
            r = [x0, cy - ch * 0.5, x0 + cw, cy + ch * 0.5];
        }
        (Grip::Edge(_, my), Some(q)) => {
            let ay = if my > 0 { r[1] } else { r[3] };
            let py = if my > 0 { r[3] } else { r[1] } + dy;
            let maxh = if my > 0 { h - ay } else { ay };
            let ch = ((py - ay) * my as f32).max(min).min(maxh).min(w / q);
            let cw = ch * q;
            let cx = ((r[0] + r[2]) * 0.5).clamp(cw * 0.5, w - cw * 0.5);
            let y0 = if my > 0 { ay } else { ay - ch };
            r = [cx - cw * 0.5, y0, cx + cw * 0.5, y0 + ch];
        }
    }
    [r[0] / w, r[1] / h, r[2] / w, r[3] / h].map(|v| v.clamp(0.0, 1.0))
}

/// Where a click landed. Valid on the frame `clicked()` fires, unlike
/// `pointer.press_origin()`, which egui has already cleared by then.
fn click_pos(resp: &egui::Response) -> Option<Pos2> {
    resp.interact_pointer_pos()
}

/// Holding Space hands the drag back to panning.
fn space_or_zoom_pan(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_down(egui::Key::Space))
}

fn project(velocity: f32, rate: f32) -> f32 {
    (velocity / 1000.0) * rate / (1.0 - rate)
}

fn rubberband(overshoot: f32, dim: f32) -> f32 {
    let c = 0.55;
    (overshoot * dim * c) / (dim + c * overshoot.abs())
}

impl App {
    fn canvas(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let resp = ui.interact(rect, Id::unique("canvas"), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());

        let Some(_doc) = &self.doc else {
            self.empty_state(ui, rect, hovering_files);
            return;
        };

        let compare_key = ctx.input(|i| i.key_down(egui::Key::Backslash));
        let compare_btn = ctx.data(|d| d.get_temp::<bool>(Id::unique("compare_btn")).unwrap_or(false));
        let compare = compare_key || compare_btn;

        let cropping = self.crop_mode() && !compare;
        let masking = TABS[self.tab].0 == Tab::Masks && !compare && self.selected_mask().is_some();
        let layering = TABS[self.tab].0 == Tab::Layers && !compare && !self.params.layers.is_empty();
        let retouching = TABS[self.tab].0 == Tab::Retouch && !compare;
        if cropping {
            self.view.zoom = 1.0;
            self.view.pan = Vec2::ZERO;
        }

        let tex = if compare { self.orig_tex.as_ref() } else { self.tex.as_ref() };
        let Some(tex) = tex.or(self.orig_tex.as_ref()) else { return };
        // a draft texture is smaller; lay it out at the full preview size
        let scale = if !compare && self.tex.is_some() { self.tex_scale } else { 1.0 };
        let (tex_id, tsize) = (tex.id(), tex.size_vec2() * scale);

        let pad = 36.0;
        let avail = rect.shrink(pad);
        let fit = (avail.width() / tsize.x).min(avail.height() / tsize.y).min(4.0);

        // --- input: zoom (pinch / wheel) about the pointer ------------------
        let (scroll, pinch, hover_pos) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
        if resp.hovered() && !self.export_open && !cropping {
            let factor = pinch * (scroll * 0.0018).exp();
            if (factor - 1.0).abs() > 1e-4 {
                let old = self.view.zoom;
                let new = (old * factor).clamp(1.0, 12.0);
                if let Some(p) = hover_pos {
                    let rel = p - rect.center();
                    self.view.pan = rel - (rel - self.view.pan) * (new / old);
                }
                self.view.zoom = new;
                if new <= 1.0001 {
                    self.view.pan = Vec2::ZERO;
                }
                let zid = Id::unique("zoom");
                k::spring_set(&ctx, zid, new, 0.0);
                k::spring_set(&ctx, Id::unique("panx"), self.view.pan.x, 0.0);
                k::spring_set(&ctx, Id::unique("pany"), self.view.pan.y, 0.0);
            }
        }
        if resp.double_clicked() && !cropping {
            if self.view.zoom > 1.01 {
                self.view.zoom = 1.0;
                self.view.pan = Vec2::ZERO;
            } else if let Some(p) = resp.interact_pointer_pos() {
                let new = 2.5;
                let rel = p - rect.center();
                self.view.pan = rel - rel * new;
                self.view.zoom = new;
            }
        }

        // bounds for panning at the target zoom
        let bounds = |zoom: f32| {
            let s = tsize * fit * zoom;
            vec2(((s.x - rect.width()) / 2.0 + pad).max(0.0), ((s.y - rect.height()) / 2.0 + pad).max(0.0))
        };

        // --- masks own the drag when it starts on a handle or with the brush
        let space_held = ctx.input(|i| i.key_down(egui::Key::Space));
        if masking && !space_held && self.tex_frame == Frame::Unbordered {
            let origin = ctx.input(|i| i.pointer.press_origin());
            if resp.drag_started()
                && let Some(o) = origin
            {
                self.mask_begin(o, &ctx);
            }
            // egui clears `press_origin` on the release frame, which is the frame a
            // click is reported on, so take the position from the response itself
            if resp.clicked()
                && let Some(o) = click_pos(&resp)
            {
                self.mask_click(o, &ctx);
            }
            if resp.secondary_clicked()
                && let Some(o) = click_pos(&resp)
            {
                self.mask_secondary_click(o);
            }
        }

        if layering && !space_held && self.tex_frame == Frame::Unbordered {
            if resp.drag_started()
                && let Some(o) = ctx.input(|i| i.pointer.press_origin())
            {
                self.layer_begin(o, &ctx);
            }
            if resp.clicked()
                && let Some(o) = click_pos(&resp)
            {
                self.layer_click(o);
            }
        }

        if retouching && !space_held && self.tex_frame == Frame::Unbordered {
            if resp.drag_started()
                && let Some(o) = ctx.input(|i| i.pointer.press_origin())
            {
                self.clone_begin(o, &ctx);
            }
            if resp.clicked()
                && let Some(o) = click_pos(&resp)
            {
                self.clone_click(o, &ctx);
            }
        }

        // --- drag to pan: 1:1, rubber-banded past the edges, momentum on release
        if resp.dragged() && self.view.zoom > 1.0 && self.mask_drag.is_none() && self.layer_drag.is_none() && !self.clone_painting {
            self.view.dragging = true;
            self.view.pan += resp.drag_delta();
        }
        let b = bounds(self.view.zoom);
        let mut shown_pan = self.view.pan;
        if self.view.dragging {
            for (v, lim, dim) in [(&mut shown_pan.x, b.x, rect.width()), (&mut shown_pan.y, b.y, rect.height())] {
                if v.abs() > lim {
                    let over = v.abs() - lim;
                    *v = v.signum() * (lim + rubberband(over, dim));
                }
            }
            k::spring_set(&ctx, Id::unique("panx"), shown_pan.x, 0.0);
            k::spring_set(&ctx, Id::unique("pany"), shown_pan.y, 0.0);
        }
        if resp.drag_stopped() && self.view.dragging {
            self.view.dragging = false;
            let v = ctx.input(|i| i.pointer.velocity());
            // project the resting point, then settle inside the bounds
            let target = shown_pan + vec2(project(v.x, 0.995), project(v.y, 0.995));
            self.view.pan = vec2(target.x.clamp(-b.x, b.x), target.y.clamp(-b.y, b.y));
            // hand the finger's velocity to the spring so there's no seam
            k::spring_set(&ctx, Id::unique("panx"), shown_pan.x, v.x);
            k::spring_set(&ctx, Id::unique("pany"), shown_pan.y, v.y);
        }
        if !self.view.dragging {
            self.view.pan = vec2(self.view.pan.x.clamp(-b.x, b.x), self.view.pan.y.clamp(-b.y, b.y));
        }

        let zoom = k::spring(&ctx, Id::unique("zoom"), self.view.zoom, 1.0, 0.35);
        let px = k::spring(&ctx, Id::unique("panx"), self.view.pan.x, 1.0, 0.45);
        let py = k::spring(&ctx, Id::unique("pany"), self.view.pan.y, 1.0, 0.45);

        // how many source pixels the long edge needs at the zoom we're heading to
        let src_long = self.doc.as_ref().map(|d| d.preview.w.max(d.preview.h) as f32).unwrap_or(0.0);
        self.detail_need = if cropping || compare { 0.0 } else { src_long * fit * self.view.zoom * ctx.pixels_per_point() };
        let size = tsize * fit * zoom;
        let img_rect = Rect::from_center_size(rect.center() + vec2(px, py), size);
        painter.image(tex_id, img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        self.last_img_rect = img_rect;
        // the hand for panning goes first, so a tool's own cursor (colour
        // picker, brush, object crosshair) can replace it
        if self.view.zoom > 1.0 && (resp.hovered() || resp.dragged()) {
            ctx.set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        }
        if masking
            && self.tex_frame == Frame::Unbordered
            && let Some(o) = &self.overlay_tex
        {
            painter.image(o.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        if cropping && self.tex_frame == Frame::Uncropped {
            self.crop_overlay(ui, &resp, rect, img_rect);
        } else if !cropping {
            let content = self.content_rect(img_rect);
            draw_guides(&ui.painter_at(img_rect), content, if self.guide_thirds { 1.0 } else { 0.0 }, self.guide_golden);
        }
        if masking && self.tex_frame == Frame::Unbordered {
            self.mask_overlay(ui, &resp, rect, img_rect);
        }
        if layering && self.tex_frame == Frame::Unbordered {
            self.layer_overlay(ui, &resp, rect, img_rect);
        }
        if retouching && self.tex_frame == Frame::Unbordered {
            self.clone_overlay(ui, &resp, rect, img_rect);
        }

        // status: a quiet activity indicator while a slow render is in flight
        let pending = self.generation != self.shown_gen;
        if pending && self.sent_at.map(|t| t.elapsed().as_millis() > 120).unwrap_or(false) {
            let c = pos2(rect.right() - 22.0, rect.top() + 22.0);
            let t = ctx.input(|i| i.time) as f32;
            for i in 0..8 {
                let a = i as f32 / 8.0 * std::f32::consts::TAU;
                let alpha = ((i as f32 / 8.0 + t * 1.2).fract() * 200.0) as u8;
                let d = vec2(a.cos(), a.sin());
                painter.line_segment([c + d * 4.0, c + d * 8.0], Stroke::new(2.0, Color32::from_white_alpha(alpha)));
            }
            ctx.request_repaint();
        }

        if compare {
            let r = Rect::from_center_size(pos2(rect.center().x, rect.top() + 26.0), vec2(80.0, 24.0));
            painter.rect_filled(r, CornerRadius::same(12), Color32::from_black_alpha(170));
            painter.text(r.center(), Align2::CENTER_CENTER, "Original", k::font_semibold(12.0), k::LABEL);
        }

        self.canvas_hud(ui, rect, fit, zoom);
        self.toast_view(ui, rect);
        if hovering_files {
            self.drop_hint(ui, rect);
        }
    }

    /// Crop rectangle drawn over the full frame: drag corners / edges to resize,
    /// inside to move. Tracks the pointer 1:1 from where it was grabbed.
    fn crop_overlay(&mut self, ui: &mut Ui, resp: &egui::Response, clip: Rect, img: Rect) {
        let ctx = ui.ctx().clone();
        let (fw, fh) = self.frame_dims();
        let ratio = self.params.aspect.ratio(fw, fh, self.params.aspect_portrait);
        let to_screen = |c: [f32; 4]| Rect::from_min_max(img.lerp_inside(vec2(c[0], c[1])), img.lerp_inside(vec2(c[2], c[3])));

        // the horizon line's handles sit on top of the crop, so they win
        if resp.drag_started()
            && let Some(o) = ctx.input(|i| i.pointer.press_origin())
            && let Some(h) = self.horizon
            && let Some(id) = h.hit(img, o)
        {
            self.horizon_drag = Some((id, h, o));
        }
        if let Some((id, start, origin)) = self.horizon_drag {
            if let Some(p) = ctx.input(|i| i.pointer.latest_pos())
                && let Some(h) = &mut self.horizon
            {
                if id == 0 {
                    let d = p - origin;
                    h.c = [(start.c[0] + d.x / img.width()).clamp(0.0, 1.0), (start.c[1] + d.y / img.height()).clamp(0.0, 1.0)];
                } else {
                    let (c, _, _) = start.points(img);
                    let v = p - c;
                    let mut a = line_angle(v.y.atan2(v.x));
                    // settle onto level when close
                    if a.abs() < 0.4f32.to_radians() {
                        a = 0.0;
                    }
                    h.angle = a;
                }
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.horizon_drag = None;
            }
        }
        if resp.drag_started()
            && self.horizon_drag.is_none()
            && let Some(o) = ctx.input(|i| i.pointer.press_origin())
            && let Some(g) = grip_at(to_screen(self.params.crop), o)
        {
            self.crop_drag = Some((g, self.params.crop, o));
        }
        if let Some((g, start, origin)) = self.crop_drag {
            if let Some(p) = ctx.input(|i| i.pointer.latest_pos()) {
                let d = p - origin;
                self.params.crop = drag_crop(start, g, vec2(d.x / img.width(), d.y / img.height()), ratio, fw, fh);
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.crop_drag = None;
            }
        }
        let dragging = self.crop_drag.is_some();

        // displayed rect follows the model through critically damped springs,
        // so preset changes glide while direct drags stay glued to the pointer
        let ids = ["crop0", "crop1", "crop2", "crop3"].map(Id::unique);
        let mut shown = [0.0; 4];
        for i in 0..4 {
            shown[i] = if dragging {
                k::spring_set(&ctx, ids[i], self.params.crop[i], 0.0);
                self.params.crop[i]
            } else {
                k::spring(&ctx, ids[i], self.params.crop[i], 1.0, 0.32)
            };
        }
        let r = to_screen(shown);

        let over_canvas = resp.hovered() || resp.dragged();
        let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| over_canvas && clip.contains(*p));
        let on_horizon = self.horizon_drag.is_some() || hover.zip(self.horizon).and_then(|(p, h)| h.hit(img, p)).is_some();
        let active = self.crop_drag.map(|d| d.0).or_else(|| hover.and_then(|h| grip_at(r, h)));
        if on_horizon {
            ctx.set_cursor_icon(if self.horizon_drag.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        } else if let Some(g) = active {
            ctx.set_cursor_icon(match g {
                Grip::Move => if dragging { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab },
                Grip::Edge(0, _) => egui::CursorIcon::ResizeVertical,
                Grip::Edge(_, 0) => egui::CursorIcon::ResizeHorizontal,
                Grip::Edge(x, y) if x == y => egui::CursorIcon::ResizeNwSe,
                Grip::Edge(_, _) => egui::CursorIcon::ResizeNeSw,
            });
        }

        let painter = ui.painter_at(clip);
        let shade = Color32::from_black_alpha(if dragging { 120 } else { 165 });
        for s in [
            Rect::from_min_max(img.min, pos2(img.max.x, r.min.y)),
            Rect::from_min_max(pos2(img.min.x, r.max.y), img.max),
            Rect::from_min_max(pos2(img.min.x, r.min.y), pos2(r.min.x, r.max.y)),
            Rect::from_min_max(pos2(r.max.x, r.min.y), pos2(img.max.x, r.max.y)),
        ] {
            painter.rect_filled(s, CornerRadius::ZERO, shade);
        }
        // rule of thirds: always there while cropping, brighter while adjusting
        let grid_a = k::spring(&ctx, Id::unique("crop_grid"), if dragging { 1.0 } else { 0.0 }, 1.0, 0.25);
        let thirds = if self.guide_thirds { 1.0 } else { 0.55 + grid_a * 0.45 };
        draw_guides(&painter, r, thirds, self.guide_golden);
        painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, Color32::from_white_alpha(210)), egui::StrokeKind::Inside);
        // corner brackets
        let (len, th) = (18.0f32.min(r.width() / 3.0).min(r.height() / 3.0), 3.0);
        let st = Stroke::new(th, Color32::WHITE);
        for (c, sx, sy) in [(r.left_top(), 1.0, 1.0), (r.right_top(), -1.0, 1.0), (r.left_bottom(), 1.0, -1.0), (r.right_bottom(), -1.0, -1.0)] {
            let c = c + vec2(sx * th / 2.0, sy * th / 2.0);
            painter.line_segment([c, c + vec2(sx * len, 0.0)], st);
            painter.line_segment([c, c + vec2(0.0, sy * len)], st);
        }
        // horizon line
        if let Some(h) = self.horizon {
            let (c, a, b) = h.points(img);
            let dir = (b - a).normalized() * (img.width() + img.height());
            let ip = ui.painter_at(img);
            let col = Color32::from_rgb(255, 214, 10);
            ip.line_segment([c - dir, c + dir], Stroke::new(3.0, Color32::from_black_alpha(90)));
            ip.line_segment([c - dir, c + dir], Stroke::new(1.5, col));
            let hot = hover.and_then(|p| h.hit(img, p)).or(self.horizon_drag.map(|d| d.0));
            for (id, q) in [(0u8, c), (1, a), (2, b)] {
                let rad = if id == 0 { 7.0 } else { 5.5 } + if hot == Some(id) { 1.5 } else { 0.0 };
                painter.circle_filled(q + vec2(0.0, 1.0), rad + 1.5, Color32::from_black_alpha(110));
                painter.circle_filled(q, rad, col);
                painter.circle_stroke(q, rad, Stroke::new(1.5, Color32::WHITE));
            }
            let label = format!("{:+.1}°", h.angle.to_degrees());
            let g = painter.layout_no_wrap(label, k::font_mono(11.0), k::LABEL);
            let pos = c + vec2(0.0, -22.0);
            let bg = Rect::from_center_size(pos, g.size() + vec2(14.0, 6.0));
            painter.rect_filled(bg, CornerRadius::same(9), Color32::from_black_alpha(170));
            painter.galley(bg.center() - g.size() / 2.0, g, k::LABEL);
        }

        // pixel size readout
        if dragging || hover.map(|h| r.contains(h)).unwrap_or(false) {
            if let Some(doc) = &self.doc {
                let (w, h) = (doc.full.width() as usize, doc.full.height() as usize);
                let (w, h) = if self.params.rotate % 2 == 1 { (h, w) } else { (w, h) };
                let (_, _, cw, ch) = pipeline::crop_px(w, h, self.params.crop);
                let label = format!("{cw} × {ch}");
                let pos = pos2(r.center().x, (r.top() - 16.0).max(img.top() + 14.0));
                let g = painter.layout_no_wrap(label, k::font_mono(11.5), k::LABEL);
                let bg = Rect::from_center_size(pos, g.size() + vec2(16.0, 6.0));
                painter.rect_filled(bg, CornerRadius::same(10), Color32::from_black_alpha(170));
                painter.galley(bg.center() - g.size() / 2.0, g, k::LABEL);
            }
        }
    }

    /// The picture inside the preview, excluding any border.
    fn content_rect(&self, img: Rect) -> Rect {
        if self.tex_frame != Frame::Output || self.params.border <= 0.001 {
            return img;
        }
        let (fw, fh) = self.frame_dims();
        let (_, _, cw, ch) = pipeline::crop_px(fw as usize, fh as usize, self.params.crop);
        let b = (self.params.border * 0.12 * cw.min(ch) as f32).round();
        let fx = b / (cw as f32 + 2.0 * b) * img.width();
        let fy = b / (ch as f32 + 2.0 * b) * img.height();
        img.shrink2(vec2(fx, fy))
    }

    fn mask_map(&self, img: Rect) -> Map {
        Map { img, c: self.params.crop }
    }

    fn mask_begin(&mut self, o: Pos2, ctx: &egui::Context) {
        let Some(i) = self.selected_mask() else { return };
        if self.picking_hue {
            return;
        }
        let map = self.mask_map(self.last_img_rect);
        let erase = self.brush_erase ^ ctx.input(|inp| inp.modifiers.alt);
        let (radius, feather, flow) = (brush_radius(self.brush_size), self.brush_feather, self.brush_flow);
        self.overlay_hold = true;
        let m = &mut self.params.masks[i];
        match &mut m.shape {
            Shape::Brush { strokes } => {
                strokes.push(BrushStroke { pts: vec![map.s2n(o)], radius, feather, flow, erase });
                self.mask_drag = Some(MaskDrag::Paint);
            }
            shape => {
                let hit = mask_handles(shape, &map)
                    .into_iter()
                    .map(|(id, p)| (id, p, p.distance(o)))
                    .filter(|h| h.2 < 14.0)
                    .min_by(|a, b| a.2.total_cmp(&b.2));
                if let Some((id, p, _)) = hit {
                    self.mask_drag = Some(MaskDrag::Handle { id, offset: p - o, origin: o, start: shape.clone() });
                }
            }
        }
    }

    /// A click (no drag): a brush dab, or sampling a colour for the range.
    fn mask_click(&mut self, o: Pos2, ctx: &egui::Context) {
        let Some(i) = self.selected_mask() else { return };
        let img = self.last_img_rect;
        if self.picking_hue {
            self.picking_hue = false;
            if let Some(im) = &self.last_image
                && img.contains(o)
            {
                let u = ((o.x - img.left()) / img.width() * im.size[0] as f32) as usize;
                let v = ((o.y - img.top()) / img.height() * im.size[1] as f32) as usize;
                let c = im.pixels[v.min(im.size[1] - 1) * im.size[0] + u.min(im.size[0] - 1)];
                let (hue, sat) = masks::hue_sat([c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0]);
                let m = &mut self.params.masks[i];
                if sat > 0.02 {
                    m.hue = hue;
                }
                m.refine = Refine::Color;
                self.overlay_hold = true;
            }
            return;
        }
        if matches!(self.params.masks[i].shape, Shape::Brush { .. }) {
            self.mask_begin(o, ctx);
            self.mask_drag = None;
        }
        if img.contains(o) {
            // Alt flips between adding and removing
            let add = !(self.seed_remove ^ ctx.input(|inp| inp.modifiers.alt));
            let map = self.mask_map(img);
            let hit = seed_at(&self.params.masks[i].shape, &map, o);
            if let Shape::Object { seeds, .. } = &mut self.params.masks[i].shape {
                // clicking an existing point takes it away again
                match hit {
                    Some(h) => {
                        seeds.remove(h);
                    }
                    None => seeds.push(Seed { p: map.s2n(o), add }),
                }
                self.overlay_hold = true;
            }
        }
    }

    /// Right-click: remove the object-selection point under the pointer.
    fn mask_secondary_click(&mut self, o: Pos2) {
        let Some(i) = self.selected_mask() else { return };
        let map = self.mask_map(self.last_img_rect);
        if let Some(h) = seed_at(&self.params.masks[i].shape, &map, o)
            && let Shape::Object { seeds, .. } = &mut self.params.masks[i].shape
        {
            seeds.remove(h);
            self.overlay_hold = true;
        }
    }

    /// Handles for the selected mask, and the brush cursor.
    fn mask_overlay(&mut self, ui: &mut Ui, resp: &egui::Response, clip: Rect, img: Rect) {
        let ctx = ui.ctx().clone();
        let Some(i) = self.selected_mask() else { return };
        let map = self.mask_map(img);

        if let Some(d) = self.mask_drag.clone() {
            if let Some(p) = ctx.input(|inp| inp.pointer.latest_pos()) {
                let m = &mut self.params.masks[i];
                match d {
                    MaskDrag::Paint => {
                        if let Shape::Brush { strokes } = &mut m.shape
                            && let Some(st) = strokes.last_mut()
                        {
                            let last = map.n2s(*st.pts.last().unwrap_or(&[0.0, 0.0]));
                            if last.distance(p) > 2.0 {
                                st.pts.push(map.s2n(p));
                            }
                        }
                    }
                    MaskDrag::Handle { id, offset, origin, start } => {
                        m.shape = drag_handle(&start, id, p + offset, p - origin, &map);
                    }
                }
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.mask_drag = None;
            }
        }

        let painter = ui.painter_at(clip);
        // only while the canvas itself is under the pointer, not a sheet or menu above it
        let over_canvas = resp.hovered() || resp.dragged();
        let hover = ctx.input(|inp| inp.pointer.hover_pos()).filter(|p| over_canvas && clip.contains(*p));
        if self.picking_hue {
            if hover.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            return;
        }
        let m = &self.params.masks[i];
        let white = Color32::WHITE;
        let shadow = Color32::from_black_alpha(110);
        let line = |pts: Vec<Pos2>, w: f32, a: u8, closed: bool| {
            let mk = |pts: Vec<Pos2>, st: Stroke| if closed { egui::Shape::closed_line(pts, st) } else { egui::Shape::line(pts, st) };
            painter.add(mk(pts.clone(), Stroke::new(w + 1.5, Color32::from_black_alpha(a / 3))));
            painter.add(mk(pts, Stroke::new(w, Color32::from_white_alpha(a))));
        };
        match &m.shape {
            Shape::Linear { a, b } => {
                let (a, b) = (map.n2s(*a), map.n2s(*b));
                let d = (b - a).normalized();
                let n = vec2(-d.y, d.x) * 6000.0;
                let ip = ui.painter_at(img);
                for (p, w, al) in [(a, 1.5, 230u8), (b, 1.0, 150), (a + (b - a) * 0.5, 1.0, 70)] {
                    ip.line_segment([p - n, p + n], Stroke::new(w + 1.5, Color32::from_black_alpha(al / 3)));
                    ip.line_segment([p - n, p + n], Stroke::new(w, Color32::from_white_alpha(al)));
                }
                ip.line_segment([a, b], Stroke::new(1.0, Color32::from_white_alpha(90)));
            }
            Shape::Radial { c, rx, ry, angle, feather } => {
                let cs = map.n2s(*c);
                let l = map.long();
                let ellipse = |s: f32| -> Vec<Pos2> {
                    (0..96)
                        .map(|k| {
                            let t = k as f32 / 96.0 * std::f32::consts::TAU;
                            let v = vec2(rx * s * t.cos(), ry * s * t.sin()) * l;
                            cs + vec2(v.x * angle.cos() - v.y * angle.sin(), v.x * angle.sin() + v.y * angle.cos())
                        })
                        .collect()
                };
                line(ellipse(1.0), 1.5, 230, true);
                line(ellipse(1.0 - feather.clamp(0.01, 1.0)), 1.0, 110, true);
            }
            Shape::Brush { .. } => {
                if let Some(h) = hover.filter(|h| img.contains(*h)) {
                    let r = brush_radius(self.brush_size) * map.long();
                    let erase = self.brush_erase ^ ctx.input(|inp| inp.modifiers.alt);
                    painter.circle_stroke(h, r, Stroke::new(2.5, Color32::from_black_alpha(70)));
                    painter.circle_stroke(h, r, Stroke::new(1.2, white));
                    painter.circle_stroke(h, r * (1.0 - self.brush_feather), Stroke::new(1.0, Color32::from_white_alpha(90)));
                    // minus sign while erasing
                    let sign = if erase { "−" } else { "+" };
                    painter.text(h, Align2::CENTER_CENTER, sign, k::font(12.0), white);
                    if !space_or_zoom_pan(&ctx) {
                        ctx.set_cursor_icon(egui::CursorIcon::None);
                    }
                }
            }
            Shape::Everything => {}
            Shape::Object { seeds, .. } => {
                let under = hover.and_then(|h| seed_at(&m.shape, &map, h));
                for (si, sd) in seeds.iter().enumerate() {
                    let p = map.n2s(sd.p);
                    // the point under the pointer shows that a click removes it
                    let hot = under == Some(si);
                    let r = if hot { 9.0 } else { 7.0 };
                    painter.circle_filled(p + vec2(0.0, 1.0), r + 1.0, shadow);
                    let fill = if hot { Color32::from_rgb(60, 60, 64) } else if sd.add { k::ACCENT } else { Color32::from_rgb(255, 69, 58) };
                    painter.circle_filled(p, r, fill);
                    painter.circle_stroke(p, r, Stroke::new(1.5, white));
                    let glyph = if hot { "×" } else if sd.add { "+" } else { "−" };
                    painter.text(p, Align2::CENTER_CENTER, glyph, k::font_semibold(if hot { 13.0 } else { 11.0 }), white);
                }
                if let Some(h) = hover.filter(|h| img.contains(*h))
                    && !space_or_zoom_pan(&ctx)
                {
                    if under.is_some() {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    } else {
                        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
                        // a small + or − next to the crosshair shows what a click places
                        let add = !(self.seed_remove ^ ctx.input(|inp| inp.modifiers.alt));
                        let tag = h + vec2(14.0, 14.0);
                        painter.circle_filled(tag, 7.0, if add { k::ACCENT } else { Color32::from_rgb(255, 69, 58) });
                        painter.text(tag, Align2::CENTER_CENTER, if add { "+" } else { "−" }, k::font_semibold(11.0), white);
                    }
                }
            }
        }

        let active = match &self.mask_drag {
            Some(MaskDrag::Handle { id, .. }) => Some(*id),
            _ => None,
        };
        let hs = mask_handles(&m.shape, &map);
        let hovered = hover.and_then(|h| hs.iter().filter(|(_, p)| p.distance(h) < 14.0).min_by(|a, b| a.1.distance(h).total_cmp(&b.1.distance(h))).map(|x| x.0));
        for (id, p) in hs {
            let pin = id == 2 || id == 10;
            let big = active == Some(id) || hovered == Some(id);
            let r = if pin { 7.0 } else { 5.0 } + if big { 1.5 } else { 0.0 };
            painter.circle_filled(p + vec2(0.0, 1.0), r + 1.5, shadow);
            if pin {
                painter.circle_filled(p, r, k::ACCENT);
                painter.circle_stroke(p, r, Stroke::new(2.0, white));
            } else {
                painter.circle_filled(p, r, white);
            }
        }
        if active.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if hovered.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    // --- clone tool ------------------------------------------------------------

    /// Ctrl or Alt (or the sidebar button) turns a click into choosing the source.
    fn clone_wants_source(&self, ctx: &egui::Context) -> bool {
        self.clone_pick_source || ctx.input(|i| i.modifiers.command || i.modifiers.alt)
    }

    fn clone_set_source(&mut self, o: Pos2) {
        let map = self.mask_map(self.last_img_rect);
        self.clone_source = Some(map.s2n(o));
        self.clone_offset = None;
        self.clone_pick_source = false;
    }

    /// Start a stroke at `o` (or choose the source).
    fn clone_begin(&mut self, o: Pos2, ctx: &egui::Context) {
        if !self.last_img_rect.contains(o) {
            return;
        }
        if self.clone_wants_source(ctx) {
            self.clone_set_source(o);
            return;
        }
        let Some(src) = self.clone_source else {
            self.toast = Some(("Ctrl+click the spot to copy from first".into(), None, Instant::now()));
            return;
        };
        let map = self.mask_map(self.last_img_rect);
        let at = map.s2n(o);
        // the first stroke fixes the distance between brush and source; later
        // strokes keep it, so the source travels with the brush
        let offset = *self.clone_offset.get_or_insert([src[0] - at[0], src[1] - at[1]]);
        self.params.retouch.push(CloneStroke {
            pts: vec![at],
            radius: brush_radius(self.clone_size),
            feather: self.clone_feather,
            opacity: self.clone_opacity,
            offset,
        });
        self.clone_painting = true;
    }

    /// A click without a drag: a single dab, or choosing the source.
    fn clone_click(&mut self, o: Pos2, ctx: &egui::Context) {
        if self.clone_wants_source(ctx) {
            if self.last_img_rect.contains(o) {
                self.clone_set_source(o);
            }
            return;
        }
        self.clone_begin(o, ctx);
        self.clone_painting = false;
    }

    /// Extends the stroke being painted, and draws the brush and source circles.
    fn clone_overlay(&mut self, ui: &mut Ui, resp: &egui::Response, clip: Rect, img: Rect) {
        let ctx = ui.ctx().clone();
        let map = self.mask_map(img);
        if self.clone_painting {
            if let Some(p) = ctx.input(|i| i.pointer.latest_pos())
                && let Some(st) = self.params.retouch.last_mut()
            {
                let last = map.n2s(*st.pts.last().unwrap_or(&[0.0, 0.0]));
                if last.distance(p) > 2.0 {
                    st.pts.push(map.s2n(p));
                }
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.clone_painting = false;
            }
        }

        let painter = ui.painter_at(clip);
        let white = Color32::WHITE;
        let ring = |c: Pos2, r: f32, col: Color32| {
            painter.circle_stroke(c, r, Stroke::new(2.5, Color32::from_black_alpha(80)));
            painter.circle_stroke(c, r, Stroke::new(1.2, col));
        };
        let cross = |c: Pos2, col: Color32| {
            for d in [vec2(6.0, 0.0), vec2(0.0, 6.0)] {
                painter.line_segment([c - d, c + d], Stroke::new(2.5, Color32::from_black_alpha(80)));
                painter.line_segment([c - d, c + d], Stroke::new(1.2, col));
            }
        };
        let source_col = Color32::from_rgb(48, 209, 88);
        let f = map.frame();
        let r = brush_radius(self.clone_size) * map.long();
        let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| clip.contains(*p) && img.contains(*p));

        // the source: where the brush copies from right now
        let source_at = |h: Option<Pos2>| -> Option<Pos2> {
            match (self.clone_offset, h, self.clone_source) {
                (Some(off), Some(h), _) => Some(h + vec2(off[0] * f.x, off[1] * f.y)),
                (None, _, Some(s)) => Some(map.n2s(s)),
                _ => None,
            }
        };
        let picking = self.clone_wants_source(&ctx);
        if let Some(s) = source_at(hover).filter(|_| !picking) {
            ring(s, r, source_col);
            cross(s, source_col);
        }
        if let Some(h) = hover {
            if space_or_zoom_pan(&ctx) {
                return;
            }
            if picking {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
                ring(h, r, source_col);
                painter.text(h + vec2(0.0, r + 12.0), Align2::CENTER_CENTER, "Copy from here", k::font(11.0), white);
            } else {
                ctx.set_cursor_icon(egui::CursorIcon::None);
                ring(h, r, white);
                ring(h, r * (1.0 - self.clone_feather), Color32::from_white_alpha(90));
                if self.clone_source.is_none() {
                    painter.text(h + vec2(0.0, r + 12.0), Align2::CENTER_CENTER, "Ctrl+click to choose a source", k::font(11.0), white);
                }
            }
        }
    }

    /// Floating capsule: hold-to-compare, zoom level, fit.
    fn canvas_hud(&mut self, ui: &mut Ui, rect: Rect, fit: f32, zoom: f32) {
        let ctx = ui.ctx().clone();
        let w = 236.0;
        let hud = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 34.0), vec2(w, 36.0));
        let painter = ui.painter_at(rect);
        // translucent material with a bright top edge
        painter.rect_filled(hud.translate(vec2(0.0, 4.0)).expand(2.0), CornerRadius::same(20), Color32::from_black_alpha(60));
        painter.rect_filled(hud, CornerRadius::same(18), Color32::from_rgba_unmultiplied(38, 38, 42, 225));
        k::stroke_rect(ui, hud, 18, Color32::from_white_alpha(16));

        let third = w / 3.0;
        // 1. Compare (press and hold)
        let r1 = Rect::from_min_size(hud.min, vec2(third, hud.height()));
        let c = ui.interact(r1, Id::unique("hud_compare"), Sense::click_and_drag());
        let held = c.is_pointer_button_down_on();
        ctx.data_mut(|d| d.insert_temp(Id::unique("compare_btn"), held));
        painter.text(
            r1.center(),
            Align2::CENTER_CENTER,
            "Compare",
            k::font(12.0),
            if held { k::ACCENT } else { k::LABEL },
        );
        c.on_hover_text("Hold to see the original  ( \\ )");

        // 2. zoom %
        let r2 = Rect::from_min_size(hud.min + vec2(third, 0.0), vec2(third, hud.height()));
        let ppp = ctx.pixels_per_point();
        let full_w = self.doc.as_ref().map(|d| d.full.width() as f32).unwrap_or(1.0);
        let prev_w = self.doc.as_ref().map(|d| d.preview.w as f32).unwrap_or(1.0);
        let pct = fit * zoom * ppp * prev_w / full_w * 100.0;
        painter.text(r2.center(), Align2::CENTER_CENTER, format!("{pct:.0}%"), k::font_mono(12.0), k::LABEL2);

        // 3. Fit
        let r3 = Rect::from_min_size(hud.min + vec2(2.0 * third, 0.0), vec2(third, hud.height()));
        let f = ui.interact(r3, Id::unique("hud_fit"), Sense::click());
        let col = if f.is_pointer_button_down_on() { k::LABEL3 } else if self.view.zoom > 1.0 { k::LABEL } else { k::LABEL3 };
        painter.text(r3.center(), Align2::CENTER_CENTER, "Fit", k::font(12.0), col);
        if f.clicked() {
            self.view.zoom = 1.0;
            self.view.pan = Vec2::ZERO;
        }
        for x in [hud.left() + third, hud.left() + 2.0 * third] {
            painter.line_segment([pos2(x, hud.top() + 10.0), pos2(x, hud.bottom() - 10.0)], Stroke::new(1.0, Color32::from_white_alpha(20)));
        }
    }

    fn empty_state(&mut self, ui: &mut Ui, rect: Rect, hovering: bool) {
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        let c = rect.center();
        if let Some(name) = &self.loading {
            painter.text(c, Align2::CENTER_CENTER, format!("Opening {name}…"), k::font(14.0), k::LABEL2);
            return;
        }
        let recent: Vec<PathBuf> = self.recent.iter().filter(|p| p.exists()).take(6).cloned().collect();
        // with a Recent list below, the card moves up to make room
        let list_h = if recent.is_empty() { 0.0 } else { 34.0 + recent.len() as f32 * 34.0 };
        let c = c - vec2(0.0, list_h / 2.0);
        let lift = k::spring(&ctx, Id::unique("drop_lift"), if hovering { 1.0 } else { 0.0 }, 1.0, 0.3);
        let card = Rect::from_center_size(c, vec2(380.0, 230.0) * (1.0 + lift * 0.03));
        painter.rect_stroke(
            card,
            CornerRadius::same(20),
            Stroke::new(1.5, k::lerp_color(Color32::from_white_alpha(22), k::ACCENT, lift)),
            egui::StrokeKind::Inside,
        );
        painter.text(c + vec2(0.0, -46.0), Align2::CENTER_CENTER, "Still", k::font_semibold(30.0), k::LABEL);
        painter.text(
            c + vec2(0.0, -12.0),
            Align2::CENTER_CENTER,
            if hovering { "Release to open" } else { "Drop a photo here, or open one to begin." },
            k::font(13.0),
            k::LABEL2,
        );
        let btn_rect = Rect::from_center_size(c + vec2(0.0, 34.0), vec2(140.0, 30.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(btn_rect).layout(egui::Layout::top_down(egui::Align::Center)));
        if k::button(&mut child, "Open Photo…", Btn::Primary, true).clicked() {
            self.pick_file(&ctx);
        }
        painter.text(
            c + vec2(0.0, 84.0),
            Align2::CENTER_CENTER,
            "RAW · JPEG · PNG · TIFF · WebP · EXR",
            k::font(11.0),
            k::LABEL3,
        );
        if !recent.is_empty() {
            self.recent_list(ui, &recent, pos2(c.x, card.bottom() + 18.0));
        }
        self.toast_view(ui, rect);
    }

    /// Recently opened files under the start card; click to open again.
    fn recent_list(&mut self, ui: &mut Ui, recent: &[PathBuf], top: Pos2) {
        let ctx = ui.ctx().clone();
        let w = 380.0;
        let painter = ui.painter().clone();
        let head = Rect::from_min_size(pos2(top.x - w / 2.0, top.y), vec2(w, 26.0));
        painter.text(pos2(head.left() + 12.0, head.center().y), Align2::LEFT_CENTER, "RECENT", k::font_semibold(11.0), k::LABEL3);
        let clear_r = Rect::from_min_max(pos2(head.right() - 60.0, head.top()), head.max);
        let clear = ui.interact(clear_r, Id::unique("recent_clear"), Sense::click());
        painter.text(pos2(head.right() - 12.0, head.center().y), Align2::RIGHT_CENTER, "Clear", k::font(11.0), if clear.hovered() { k::LABEL } else { k::LABEL3 });
        let mut open = None;
        let mut forget = None;
        for (n, p) in recent.iter().enumerate() {
            let r = Rect::from_min_size(pos2(head.left(), head.bottom() + 4.0 + n as f32 * 34.0), vec2(w, 32.0));
            let resp = ui.interact(r, Id::unique("recent_row").with(n), Sense::click());
            let bg = if resp.is_pointer_button_down_on() { k::FILL_HI } else if resp.hovered() { k::FILL } else { Color32::TRANSPARENT };
            painter.rect_filled(r, CornerRadius::same(8), bg);
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
            let kind = if io::is_project_file(p) { "Project" } else { "Photo" };
            let tag = painter.layout_no_wrap(kind.to_string(), k::font(11.0), k::LABEL3);
            let tag_x = r.right() - 12.0 - tag.size().x;
            painter.galley(pos2(tag_x, r.center().y - tag.size().y / 2.0), tag, k::LABEL3);
            let name_g = elided(ui, &name, k::font(13.0), k::LABEL, (tag_x - r.left() - 24.0) * 0.6);
            let name_w = name_g.size().x;
            painter.galley(pos2(r.left() + 12.0, r.center().y - name_g.size().y / 2.0), name_g, k::LABEL);
            let dir_g = elided(ui, &dir, k::font(11.0), k::LABEL3, tag_x - r.left() - 32.0 - name_w);
            painter.galley(pos2(r.left() + 20.0 + name_w, r.center().y - dir_g.size().y / 2.0), dir_g, k::LABEL3);
            let resp = resp.on_hover_text(p.to_string_lossy());
            if resp.clicked() {
                open = Some(p.clone());
            }
            resp.context_menu(|ui| {
                if ui.button("Remove from Recent").clicked() {
                    forget = Some(p.clone());
                }
            });
        }
        if let Some(p) = open {
            self.open_path(p, &ctx);
        }
        if let Some(p) = forget {
            self.recent.retain(|r| *r != p);
            self.persist();
        }
        if clear.clicked() {
            self.recent.clear();
            self.persist();
        }
    }

    fn drop_hint(&self, ui: &Ui, rect: Rect) {
        let r = rect.shrink(14.0);
        ui.painter().rect_filled(r, CornerRadius::same(16), k::ACCENT.gamma_multiply(0.08));
        ui.painter().rect_stroke(r, CornerRadius::same(16), Stroke::new(2.0, k::ACCENT), egui::StrokeKind::Inside);
        let project = ui.ctx().input(|i| i.raw.hovered_files.iter().any(|f| f.path.as_deref().map(io::is_project_file).unwrap_or(false)));
        let (title, sub) = if project {
            ("Drop to open the project", "")
        } else {
            ("Drop to add as a layer", "To edit a different photo instead, use Open (Ctrl+O).")
        };
        ui.painter().text(r.center() - vec2(0.0, 9.0), Align2::CENTER_CENTER, title, k::font_semibold(15.0), k::LABEL);
        ui.painter().text(r.center() + vec2(0.0, 13.0), Align2::CENTER_CENTER, sub, k::font(12.0), k::LABEL2);
    }

    fn toast_view(&mut self, ui: &mut Ui, rect: Rect) {
        let ctx = ui.ctx().clone();
        let Some((text, dir, at)) = self.toast.clone() else { return };
        let age = at.elapsed().as_secs_f32();
        let visible = age < 4.0;
        let t = k::spring(&ctx, Id::unique("toast"), if visible { 1.0 } else { 0.0 }, 1.0, 0.35);
        if !visible && t < 0.01 {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        let galley = ui.painter().layout_no_wrap(text.clone(), k::font(13.0), k::LABEL);
        let extra = if dir.is_some() { 64.0 } else { 0.0 };
        let size = vec2(galley.size().x + 36.0 + extra, 38.0);
        // emerges from the top edge and returns the same way
        let center = pos2(rect.center().x, rect.top() + 34.0 - (1.0 - t) * 16.0);
        let r = Rect::from_center_size(center, size);
        let painter = ui.painter_at(rect);
        let a = (t * 255.0) as u8;
        painter.rect_filled(r, CornerRadius::same(19), Color32::from_rgba_unmultiplied(44, 44, 48, (235.0 * t) as u8));
        painter.galley(pos2(r.left() + 18.0, r.center().y - galley.size().y / 2.0), galley, Color32::from_white_alpha(a));
        if let Some(d) = dir {
            let lr = Rect::from_min_max(pos2(r.right() - extra - 6.0, r.top()), r.max);
            let resp = ui.interact(lr, Id::unique("toast_reveal"), Sense::click());
            painter.text(lr.center(), Align2::CENTER_CENTER, "Show", k::font_semibold(13.0), k::ACCENT.gamma_multiply(t));
            if resp.clicked() {
                io::reveal(&d);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

macro_rules! modified {
    ($p:expr, $($f:ident),+) => {{ let d = Params::default(); false $( || $p.$f != d.$f )+ }};
}
macro_rules! reset {
    ($p:expr, $($f:ident),+) => {{ let d = Params::default(); $( $p.$f = d.$f; )+ }};
}

impl App {
    fn sidebar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.histogram(ui);
        ui.add_space(12.0);

        let prev = self.tab;
        // two rows of four: eight labels don't fit one segmented control
        let w = ui.available_width();
        let cw = (w - 6.0 * 3.0) / 4.0;
        for row in TABS.chunks(4).enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (j, (_, label)) in row.1.iter().enumerate() {
                    let i = row.0 * 4 + j;
                    if k::chip(ui, vec2(cw, 28.0), label, self.tab == i).clicked() {
                        self.tab = i;
                    }
                }
            });
            ui.add_space(2.0);
        }
        // content cross-fades and settles in when switching tabs
        let fid = Id::unique("tab_fade");
        if prev != self.tab {
            k::spring_set(&ctx, fid, 0.0, 0.0);
        }
        let fade = k::spring(&ctx, fid, 1.0, 1.0, 0.25);
        ui.add_space(12.0);

        let has_doc = self.doc.is_some();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.multiply_opacity(fade);
            ui.add_space((1.0 - fade) * 8.0);
            if !has_doc && TABS[self.tab].0 == Tab::Presets {
                k::caption(ui, "Open a photo to preview presets.");
            }
            match TABS[self.tab].0 {
                Tab::Presets => self.tab_presets(ui),
                Tab::Adjust => self.tab_adjust(ui),
                Tab::Masks => self.tab_masks(ui),
                Tab::Retouch => self.tab_retouch(ui),
                Tab::Layers => self.tab_layers(ui),
                Tab::Effects => self.tab_effects(ui),
                Tab::Glitch => self.tab_glitch(ui),
                Tab::Crop => self.tab_crop(ui),
            }
            ui.add_space(24.0);
        });
    }

    fn histogram(&self, ui: &mut Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 78.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(12), k::CARD);
        let inner = rect.shrink2(vec2(10.0, 10.0));
        let h = &self.hist;
        if h.l.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Histogram", k::font(11.0), k::LABEL3);
            return;
        }
        let n = h.l.len();
        let bw = inner.width() / n as f32;
        let rgb = [
            (&h.r, Color32::from_rgba_unmultiplied(255, 69, 58, 90)),
            (&h.g, Color32::from_rgba_unmultiplied(48, 209, 88, 80)),
            (&h.b, Color32::from_rgba_unmultiplied(10, 132, 255, 100)),
            (&h.l, Color32::from_rgba_unmultiplied(235, 235, 245, 60)),
        ];
        let luma = [(&h.l, Color32::from_rgba_unmultiplied(235, 235, 245, 150))];
        let chans: &[(&Vec<f32>, Color32)] = if self.prefs.histogram == 1 { &luma } else { &rgb };
        for &(vals, col) in chans {
            for (i, v) in vals.iter().enumerate() {
                let x = inner.left() + i as f32 * bw;
                let top = inner.bottom() - v * inner.height();
                painter.rect_filled(Rect::from_min_max(pos2(x, top), pos2(x + bw + 0.3, inner.bottom())), 0.0, col);
            }
        }
        if self.prefs.show_render_time {
            painter.text(
                pos2(rect.right() - 10.0, rect.top() + 8.0),
                Align2::RIGHT_TOP,
                format!("{:.0} ms", self.render_ms),
                k::font_mono(9.5),
                k::LABEL3,
            );
        }
    }

    fn tab_presets(&mut self, ui: &mut Ui) {
        let looks = self.all_looks();
        let w = ui.available_width();
        let gap = 10.0;
        let tw = (w - gap) / 2.0;
        let mut apply = None;
        let mut delete = None;
        let mut overwrite = None;
        let mut export_one: Option<(String, Params)> = None;
        let builtin_n = self.builtin.len();

        for row in looks.chunks(2).enumerate() {
            let (ri, items) = row;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (ci, (name, look, user)) in items.iter().enumerate() {
                    let idx = ri * 2 + ci;
                    let th = tw * 0.72;
                    let (rect, resp) = ui.allocate_exact_size(vec2(tw, th + 22.0), Sense::click());
                    let img_r = Rect::from_min_size(rect.min, vec2(tw, th));
                    let pressed = resp.is_pointer_button_down_on();
                    let p = k::spring(ui.ctx(), resp.id.with("press"), if pressed { 1.0 } else { 0.0 }, 1.0, 0.2);
                    let img_r = img_r.shrink(p * 3.0);
                    let painter = ui.painter();
                    painter.rect_filled(img_r, CornerRadius::same(10), k::CARD);
                    if let Some(t) = self.thumbs.get(idx) {
                        // cover-fit the thumbnail into the tile
                        let ts = t.size_vec2();
                        let (ta, ra) = (ts.x / ts.y, img_r.width() / img_r.height());
                        let uv = if ta > ra {
                            let f = ra / ta;
                            Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
                        } else {
                            let f = ta / ra;
                            Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
                        };
                        let mut mesh = egui::Mesh::with_texture(t.id());
                        mesh.add_rect_with_uv(img_r, uv, Color32::WHITE);
                        // rounded clip: draw image, then mask corners with the sidebar colour
                        painter.add(egui::Shape::mesh(mesh));
                        mask_corners(painter, img_r, 10.0, k::SIDEBAR);
                    }
                    let selected = self.params == self.params.with_look_from(look);
                    if selected {
                        painter.rect_stroke(img_r.expand(2.0), CornerRadius::same(12), Stroke::new(2.0, k::LABEL), egui::StrokeKind::Outside);
                    } else if resp.hovered() {
                        painter.rect_stroke(img_r, CornerRadius::same(10), Stroke::new(1.0, Color32::from_white_alpha(40)), egui::StrokeKind::Inside);
                    }
                    painter.text(
                        pos2(rect.center().x, rect.bottom() - 8.0),
                        Align2::CENTER_CENTER,
                        name,
                        if selected { k::font_semibold(12.0) } else { k::font(12.0) },
                        if selected { k::LABEL } else { k::LABEL2 },
                    );
                    if resp.clicked() {
                        apply = Some(look.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Export…").clicked() {
                            export_one = Some((name.clone(), look.clone()));
                            ui.close();
                        }
                        if *user {
                            if ui.button("Update with Current Edit").clicked() {
                                overwrite = Some(idx - builtin_n);
                                ui.close();
                            }
                            if ui.button("Delete Preset").clicked() {
                                delete = Some(idx - builtin_n);
                                ui.close();
                            }
                        }
                    });
                }
            });
            ui.add_space(8.0);
        }

        if let Some(look) = apply {
            self.params = self.params.with_look_from(&look);
        }
        let mut changed = false;
        if let Some(i) = overwrite {
            self.user_presets[i].1 = Params { masks: Vec::new(), layers: Vec::new(), retouch: Vec::new(), ..self.params.clone() };
            changed = true;
        }
        if let Some(i) = delete {
            self.user_presets.remove(i);
            changed = true;
        }

        ui.add_space(8.0);
        k::section(ui, "Save Preset", false, |ui| {
            k::caption(ui, "Saves every adjustment except crop, border, masks and layers.");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.new_preset)
                    .hint_text("Name")
                    .desired_width(ui.available_width() - 70.0)
                    .margin(egui::Margin::symmetric(8, 5));
                let r = ui.add(te);
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = !self.new_preset.trim().is_empty();
                if (k::button(ui, "Save", Btn::Secondary, ok).clicked() || enter) && ok {
                    self.user_presets.push((self.new_preset.trim().to_string(), Params { masks: Vec::new(), layers: Vec::new(), retouch: Vec::new(), ..self.params.clone() }));
                    self.new_preset.clear();
                    changed = true;
                }
            });
            if !self.user_presets.is_empty() {
                k::caption(ui, "Right-click a saved preset to update, export or delete it.");
            }
        });

        let ctx = ui.ctx().clone();
        let mut import = false;
        let mut export_all = false;
        k::section(ui, "Share Presets", false, |ui| {
            k::caption(ui, "Presets are saved as small .stillpreset files. Send one to a friend; they can import it here or drop it onto the window.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if k::button(ui, "Import…", Btn::Secondary, true).clicked() {
                    import = true;
                }
                if k::button(ui, "Export My Presets…", Btn::Secondary, !self.user_presets.is_empty()).clicked() {
                    export_all = true;
                }
            });
            ui.add_space(2.0);
            k::caption(ui, "Only the preset is shared, never your photo, crop or masks.");
        });
        if import {
            self.pick_looks_to_import(&ctx);
        }
        if export_all {
            self.export_looks(self.user_presets.clone(), &ctx);
        }
        if let Some(one) = export_one {
            self.export_looks(vec![one], &ctx);
        }
        if changed {
            self.persist();
            self.request_thumbs();
        }
    }

    fn tab_adjust(&mut self, ui: &mut Ui) {
        let p = &mut self.params;
        if k::section(ui, "Light", modified!(p, exposure, contrast, highlights, shadows, whites, blacks, fade), |ui| {
            k::slider(ui, "Exposure", &mut p.exposure, -3.0..=3.0, 0.0, Fmt::Ev);
            k::slider(ui, "Contrast", &mut p.contrast, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Highlights", &mut p.highlights, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Shadows", &mut p.shadows, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Whites", &mut p.whites, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Blacks", &mut p.blacks, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Fade", &mut p.fade, 0.0..=1.0, 0.0, Fmt::Amount);
        }) {
            reset!(p, exposure, contrast, highlights, shadows, whites, blacks, fade);
        }

        if k::section(ui, "Color", modified!(p, temperature, tint, saturation, vibrance, mono), |ui| {
            k::slider(ui, "Temperature", &mut p.temperature, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Tint", &mut p.tint, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Saturation", &mut p.saturation, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Vibrance", &mut p.vibrance, -1.0..=1.0, 0.0, Fmt::Signed);
            k::switch_row(ui, "Black & White", &mut p.mono);
        }) {
            reset!(p, temperature, tint, saturation, vibrance, mono);
        }

        if k::section(ui, "Split Tone", modified!(p, shadow_hue, shadow_tone, highlight_hue, highlight_tone), |ui| {
            hue_strip(ui);
            k::slider(ui, "Shadow Hue", &mut p.shadow_hue, 0.0..=360.0, 200.0, Fmt::Deg);
            k::slider(ui, "Shadow Amount", &mut p.shadow_tone, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Highlight Hue", &mut p.highlight_hue, 0.0..=360.0, 35.0, Fmt::Deg);
            k::slider(ui, "Highlight Amount", &mut p.highlight_tone, 0.0..=1.0, 0.0, Fmt::Amount);
        }) {
            reset!(p, shadow_hue, shadow_tone, highlight_hue, highlight_tone);
        }

        if k::section(ui, "Detail", modified!(p, sharpen, softness), |ui| {
            k::slider(ui, "Sharpen", &mut p.sharpen, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Soften", &mut p.softness, 0.0..=1.0, 0.0, Fmt::Amount);
        }) {
            reset!(p, sharpen, softness);
        }
    }

    /// "Apply To": the whole photo, or only inside one mask. The choice is
    /// stored on the mask (`pick` / `set`), so it survives reordering.
    fn apply_to(&mut self, ui: &mut Ui, id: &str, what: &str, pick: fn(&Mask) -> bool, set: fn(&mut Mask, bool)) {
        let masks = &mut self.params.masks;
        let current = masks.iter().position(pick);
        let mut choice = current;
        let shown = current.map(|i| masks[i].name.clone()).unwrap_or_else(|| "Whole Photo".into());
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            ui.label(egui::RichText::new("Apply To").color(k::LABEL2).size(13.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt(id).selected_text(shown).width(170.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, None, "Whole Photo");
                    for (i, m) in masks.iter().enumerate() {
                        ui.selectable_value(&mut choice, Some(i), &m.name);
                    }
                });
            });
        });
        if choice != current {
            for (i, m) in masks.iter_mut().enumerate() {
                set(m, choice == Some(i));
            }
        }
        match choice {
            None if masks.is_empty() => k::caption(ui, &format!("To use {what} on part of the photo, add a mask in the Masks tab first.")),
            Some(i) if !masks[i].enabled => k::caption(ui, "That mask is hidden, so these apply to the whole photo."),
            Some(_) => k::caption(ui, &format!("{what} only show inside the mask.")),
            None => {}
        }
        ui.add_space(8.0);
    }

    fn tab_effects(&mut self, ui: &mut Ui) {
        self.apply_to(ui, "fx_mask", "Effects", |m| m.effects, |m, on| m.effects = on);
        let p = &mut self.params;
        if k::section(ui, "Optics", modified!(p, bloom, bloom_radius, bloom_threshold, halation, aberration, vignette, vignette_feather), |ui| {
            k::slider(ui, "Bloom", &mut p.bloom, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Bloom Radius", &mut p.bloom_radius, 0.0..=1.0, 0.4, Fmt::Amount);
            k::slider(ui, "Glow Threshold", &mut p.bloom_threshold, 0.0..=1.0, 0.5, Fmt::Amount);
            k::slider(ui, "Halation", &mut p.halation, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Chromatic Aberration", &mut p.aberration, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Vignette", &mut p.vignette, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Vignette Feather", &mut p.vignette_feather, 0.0..=1.0, 0.6, Fmt::Amount);
        }) {
            reset!(p, bloom, bloom_radius, bloom_threshold, halation, aberration, vignette, vignette_feather);
        }

        if k::section(ui, "Astigmatism", modified!(p, astig, astig_length, astig_angle, astig_rays), |ui| {
            k::caption(ui, "Bright lights smear into streaks, like night lights seen through an astigmatic eye.");
            ui.add_space(2.0);
            k::slider(ui, "Amount", &mut p.astig, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Length", &mut p.astig_length, 0.0..=1.0, 0.4, Fmt::Amount);
            k::slider(ui, "Angle", &mut p.astig_angle, 0.0..=180.0, 90.0, Fmt::Deg);
            let mut r = (p.astig_rays.clamp(1, 3) - 1) as usize;
            if k::segmented(ui, "astig_rays", &mut r, &["Streak", "Cross", "Star"]) {
                p.astig_rays = r as u8 + 1;
            }
            ui.add_space(2.0);
            k::caption(ui, "Uses the Glow Threshold above to decide which lights streak.");
            ui.add_space(4.0);
        }) {
            reset!(p, astig, astig_length, astig_angle, astig_rays);
        }

        if k::section(ui, "Grain", modified!(p, grain, grain_size, grain_color), |ui| {
            k::slider(ui, "Amount", &mut p.grain, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Size", &mut p.grain_size, 0.0..=1.0, 0.35, Fmt::Amount);
            k::slider(ui, "Color", &mut p.grain_color, 0.0..=1.0, 0.0, Fmt::Amount);
        }) {
            reset!(p, grain, grain_size, grain_color);
        }

        if k::section(ui, "Sensor", modified!(p, sensor_noise, scanlines, scanline_spacing), |ui| {
            k::slider(ui, "CCD Noise", &mut p.sensor_noise, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Scanlines", &mut p.scanlines, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Line Spacing", &mut p.scanline_spacing, 0.0..=1.0, 0.3, Fmt::Amount);
        }) {
            reset!(p, sensor_noise, scanlines, scanline_spacing);
        }

        if k::section(ui, "Resolution", modified!(p, lowres, lowres_smooth), |ui| {
            k::slider(ui, "Downsample", &mut p.lowres, 0.0..=1.0, 0.0, Fmt::Amount);
            k::switch_row(ui, "Smooth Upscale", &mut p.lowres_smooth);
        }) {
            reset!(p, lowres, lowres_smooth);
        }

        if k::section(ui, "Posterize", modified!(p, posterize, dither), |ui| {
            k::slider(ui, "Amount", &mut p.posterize, 0.0..=1.0, 0.0, Fmt::Amount);
            k::switch_row(ui, "Ordered Dither", &mut p.dither);
        }) {
            reset!(p, posterize, dither);
        }
    }

    fn tab_glitch(&mut self, ui: &mut Ui) {
        self.apply_to(ui, "glitch_mask", "Glitches", |m| m.glitch, |m, on| m.glitch = on);
        let p = &mut self.params;
        k::caption(ui, "Glitches are placed by a seed. Shuffle for a new arrangement.");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if k::button(ui, "Shuffle", Btn::Secondary, true).clicked() {
                p.seed = p.seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 100_000 + 1;
            }
            ui.label(egui::RichText::new(format!("Seed {}", p.seed)).font(k::font_mono(11.5)).color(k::LABEL3));
        });
        ui.add_space(10.0);

        if k::section(ui, "Pixel Sort", modified!(p, sort_amount, sort_low, sort_high, sort_vertical, sort_reverse), |ui| {
            k::slider(ui, "Amount", &mut p.sort_amount, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Threshold Low", &mut p.sort_low, 0.0..=1.0, 0.45, Fmt::Amount);
            k::slider(ui, "Threshold High", &mut p.sort_high, 0.0..=1.0, 1.0, Fmt::Amount);
            k::switch_row(ui, "Vertical", &mut p.sort_vertical);
            k::switch_row(ui, "Reverse", &mut p.sort_reverse);
        }) {
            reset!(p, sort_amount, sort_low, sort_high, sort_vertical, sort_reverse);
        }

        if k::section(ui, "Data Streaks", modified!(p, streaks, streak_width), |ui| {
            k::slider(ui, "Amount", &mut p.streaks, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Width", &mut p.streak_width, 0.0..=1.0, 0.3, Fmt::Amount);
        }) {
            reset!(p, streaks, streak_width);
        }

        if k::section(ui, "Slice Shift", modified!(p, slices, slice_shift), |ui| {
            k::slider(ui, "Amount", &mut p.slices, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Offset", &mut p.slice_shift, 0.0..=1.0, 0.3, Fmt::Amount);
        }) {
            reset!(p, slices, slice_shift);
        }

        if k::section(ui, "RGB Split", modified!(p, rgb_split, rgb_angle), |ui| {
            k::slider(ui, "Distance", &mut p.rgb_split, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Angle", &mut p.rgb_angle, 0.0..=360.0, 0.0, Fmt::Deg);
        }) {
            reset!(p, rgb_split, rgb_angle);
        }

        if k::section(ui, "Compression", modified!(p, blocks, block_size), |ui| {
            k::slider(ui, "Amount", &mut p.blocks, 0.0..=1.0, 0.0, Fmt::Amount);
            k::slider(ui, "Block Size", &mut p.block_size, 0.0..=1.0, 0.3, Fmt::Amount);
        }) {
            reset!(p, blocks, block_size);
        }
    }

    // --- layers --------------------------------------------------------------

    fn selected_layer(&self) -> Option<usize> {
        self.layer_sel.filter(|i| *i < self.params.layers.len())
    }

    fn pick_layer_photos(&mut self, ctx: &egui::Context) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        let dir = self.doc.as_ref().and_then(|d| d.path.parent().map(|p| p.to_path_buf()));
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new().set_title("Add Photos as Layers").add_filter("Images", &{
                let mut v = io::IMAGE_EXTS.to_vec();
                v.extend_from_slice(io::RAW_EXTS);
                io::dialog_exts(&v)
            });
            if let Some(d) = dir {
                dlg = dlg.set_directory(d);
            }
            let picked = dlg.pick_files().unwrap_or_default();
            let _ = tx.send(Msg::DialogClosed);
            if !picked.is_empty() {
                let _ = tx.send(Msg::LayerFiles(picked));
            }
            ctx2.request_repaint();
        });
    }

    /// Decode photos off the UI thread and add each as a layer when ready.
    fn load_layers(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        // show where they land
        if !paths.is_empty()
            && let Some(i) = TABS.iter().position(|t| t.0 == Tab::Layers)
        {
            self.tab = i;
        }
        for path in paths {
            self.layers_loading += 1;
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = io::decode(&path).map(|img| {
                    let aspect = crate::layers::register(&path.to_string_lossy(), &img);
                    (path.clone(), aspect)
                });
                let _ = tx.send(Msg::LayerReady(r));
                ctx.request_repaint();
            });
        }
    }

    fn push_layer(&mut self, path: PathBuf, aspect: f32) {
        let (fw, fh) = self.frame_dims();
        let l = fw.max(fh);
        let c = self.params.crop;
        let (cw, ch) = ((c[2] - c[0]) * fw / l, (c[3] - c[1]) * fh / l);
        // fit inside the middle of the visible frame at about half its size
        let scale = (cw * 0.5).min(ch * 0.5 * aspect);
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Photo".into());
        self.params.layers.push(Layer {
            name,
            path: path.to_string_lossy().to_string(),
            aspect,
            x: (c[0] + c[2]) * 0.5,
            y: (c[1] + c[3]) * 0.5,
            scale,
            ..Layer::default()
        });
        self.layer_sel = Some(self.params.layers.len() - 1);
        self.tab = TABS.iter().position(|t| t.0 == Tab::Layers).unwrap_or(self.tab);
    }

    /// Fit (false) or fill (true) the visible canvas with the selected layer.
    fn fit_layer(&mut self, i: usize, fill: bool) {
        let (fw, fh) = self.frame_dims();
        let long = fw.max(fh);
        let c = self.params.crop;
        let (cw, ch) = ((c[2] - c[0]) * fw / long, (c[3] - c[1]) * fh / long);
        let l = &mut self.params.layers[i];
        let (a, b) = (cw, ch * l.aspect);
        l.scale = if fill { a.max(b) } else { a.min(b) };
        l.stretch = 1.0;
        l.rotation = 0.0;
        l.x = (c[0] + c[2]) * 0.5;
        l.y = (c[1] + c[3]) * 0.5;
    }

    /// Stretch the layer so it touches every side of the visible canvas.
    fn stretch_layer(&mut self, i: usize) {
        let (fw, fh) = self.frame_dims();
        let long = fw.max(fh);
        let c = self.params.crop;
        let (cw, ch) = ((c[2] - c[0]) * fw / long, (c[3] - c[1]) * fh / long);
        let l = &mut self.params.layers[i];
        l.rotation = 0.0;
        l.scale = cw;
        l.stretch = ch * l.aspect / cw.max(1e-4);
        l.x = (c[0] + c[2]) * 0.5;
        l.y = (c[1] + c[3]) * 0.5;
    }

    fn layer_begin(&mut self, o: Pos2, ctx: &egui::Context) {
        let map = self.mask_map(self.last_img_rect);
        // grab a handle of the selected layer first
        if let Some(i) = self.selected_layer().filter(|&i| !self.params.layers[i].locked) {
            let l = &self.params.layers[i];
            let (_, corners, knob) = layer_frame(l, &map);
            let edge = layer_edges(l, &map).into_iter().find(|(_, p)| p.distance(o) < 12.0).map(|e| e.0);
            let kind = if knob.distance(o) < 14.0 {
                Some(2)
            } else if corners.iter().any(|c| c.distance(o) < 14.0) {
                Some(1)
            } else if edge.is_some() {
                edge
            } else if layer_contains(l, &map, o) {
                Some(0)
            } else {
                None
            };
            if let Some(kind) = kind {
                // scaling a layer stuck to a neighbour keeps the touching side in place
                let anchor = if kind == 1 && self.prefs.magnet && self.prefs.magnet_pivot {
                    crate::magnet::touching_anchor(layer_box(l, &map), &self.magnet_targets(i, &map), 1.5)
                } else {
                    Vec2::ZERO
                };
                self.layer_drag = Some(LayerDrag { kind, start: l.clone(), origin: o, anchor });
                return;
            }
        }
        // otherwise pick the topmost visible layer under the pointer and move it
        if let Some(i) = (0..self.params.layers.len()).rev().find(|&i| self.layer_pickable(i) && layer_contains(&self.params.layers[i], &map, o)) {
            self.layer_sel = Some(i);
            self.layer_drag = Some(LayerDrag { kind: 0, start: self.params.layers[i].clone(), origin: o, anchor: Vec2::ZERO });
        }
        let _ = ctx;
    }

    fn layer_click(&mut self, o: Pos2) {
        let map = self.mask_map(self.last_img_rect);
        let hit = (0..self.params.layers.len()).rev().find(|&i| self.layer_pickable(i) && layer_contains(&self.params.layers[i], &map, o));
        self.layer_sel = hit;
    }

    /// The other layers the magnet pulls layer `i` toward: every one that is shown.
    fn magnet_targets(&self, i: usize, map: &Map) -> Vec<crate::magnet::Box2> {
        let shown = self.params.shown_layers();
        shown.iter().enumerate().filter(|(j, l)| *j != i && l.visible && l.opacity > 0.001).map(|(_, l)| layer_box(l, map)).collect()
    }

    /// Can the layer be picked on the canvas? Not when hidden (itself or its group) or locked.
    fn layer_pickable(&self, i: usize) -> bool {
        let l = &self.params.layers[i];
        let group_shown = self.params.layer_groups.iter().find(|g| g.id == l.group).map(|g| g.visible).unwrap_or(true);
        l.visible && !l.locked && group_shown
    }

    /// Outline + handles for the selected layer, and live dragging.
    fn layer_overlay(&mut self, ui: &mut Ui, resp: &egui::Response, clip: Rect, img: Rect) {
        let ctx = ui.ctx().clone();
        let map = self.mask_map(img);
        if let Some(d) = self.layer_drag.clone() {
            if let (Some(i), Some(p)) = (self.selected_layer(), ctx.input(|inp| inp.pointer.latest_pos())) {
                let (shift, no_snap) = ctx.input(|inp| (inp.modifiers.shift, inp.modifiers.command));
                let s = &d.start;
                let c = map.n2s([s.x, s.y]);
                let magnet = self.prefs.magnet && !no_snap;
                let pivot = self.prefs.magnet_pivot;
                let others = if magnet { self.magnet_targets(i, &map) } else { Vec::new() };
                let start_box = layer_box(s, &map);
                let l = &mut self.params.layers[i];
                let upright = (s.rotation.rem_euclid(90.0)).min(90.0 - s.rotation.rem_euclid(90.0)) < 0.01;
                self.snap_lines = (None, None);
                self.magnet_lines.clear();
                match d.kind {
                    0 => {
                        let dl = p - d.origin;
                        let mut nc = c + dl;
                        let mut rotation = s.rotation;
                        // the magnet pulls it flush against a neighbouring layer
                        let caught = magnet.then(|| crate::magnet::snap_move(crate::magnet::Box2 { c: nc, ..start_box }, &others, MAGNET_PX, pivot)).flatten();
                        if let Some(sn) = caught {
                            nc = sn.center;
                            if let Some(r) = sn.rot {
                                rotation = (r.to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
                            }
                            self.magnet_lines = sn.lines;
                        } else if upright && !no_snap {
                            // snap edges and centre to the canvas sides and centre
                            let (w, h) = s.size(map.long());
                            let (hw, hh) = if (s.rotation.rem_euclid(180.0) - 90.0).abs() < 1.0 { (h / 2.0, w / 2.0) } else { (w / 2.0, h / 2.0) };
                            let xs = [img.left(), img.center().x, img.right()];
                            let ys = [img.top(), img.center().y, img.bottom()];
                            let cand_x = [(nc.x - hw, -hw), (nc.x, 0.0), (nc.x + hw, hw)];
                            let cand_y = [(nc.y - hh, -hh), (nc.y, 0.0), (nc.y + hh, hh)];
                            if let Some((t, off)) = cand_x.iter().filter_map(|(v, off)| snap(*v, &xs, 8.0).map(|t| (t, *off))).next() {
                                nc.x = t - off;
                                self.snap_lines.0 = Some(t);
                            }
                            if let Some((t, off)) = cand_y.iter().filter_map(|(v, off)| snap(*v, &ys, 8.0).map(|t| (t, *off))).next() {
                                nc.y = t - off;
                                self.snap_lines.1 = Some(t);
                            }
                        }
                        let n = map.s2n(nc);
                        l.x = n[0];
                        l.y = n[1];
                        l.rotation = rotation;
                    }
                    3..=6 => {
                        // stretch one side; the opposite side stays where it is
                        let (w0, h0) = s.size(map.long());
                        let (sn, co) = s.rotation.to_radians().sin_cos();
                        let (ax, ay) = (vec2(co, sn), vec2(-sn, co));
                        let (axis, sign, len0) = match d.kind {
                            3 => (ax, 1.0, w0),
                            4 => (ax, -1.0, w0),
                            5 => (ay, 1.0, h0),
                            _ => (ay, -1.0, h0),
                        };
                        let mut len = (len0 + (p - d.origin).dot(axis) * sign).max(4.0);
                        // the magnet pulls the moving side onto a neighbour's edge
                        let caught = magnet
                            .then(|| {
                                let edge = c + axis * sign * (len - len0 / 2.0);
                                let half = if d.kind <= 4 { vec2(len, h0) } else { vec2(w0, len) } * 0.5;
                                let b = crate::magnet::Box2 { c: c + axis * sign * (len - len0) / 2.0, half, rot: start_box.rot };
                                crate::magnet::snap_edge(edge, axis * sign, b, &others, MAGNET_PX)
                            })
                            .flatten();
                        if let Some((dd, line)) = caught {
                            len = (len + dd).max(4.0);
                            self.magnet_lines.push(line);
                        } else if upright && !no_snap {
                            // snap the moving side to the canvas sides when upright
                            let edge = c + axis * sign * (len - len0 / 2.0);
                            let horizontal = axis.x.abs() > 0.5;
                            let (v, targets) = if horizontal { (edge.x, [img.left(), img.center().x, img.right()]) } else { (edge.y, [img.top(), img.center().y, img.bottom()]) };
                            if let Some(t) = snap(v, &targets, 8.0) {
                                len += (t - v) * if horizontal { axis.x.signum() * sign } else { axis.y.signum() * sign };
                                if horizontal {
                                    self.snap_lines.0 = Some(t);
                                } else {
                                    self.snap_lines.1 = Some(t);
                                }
                            }
                        }
                        let nc = c + axis * sign * (len - len0) / 2.0;
                        let (nw, nh) = if d.kind <= 4 { (len, h0) } else { (w0, len) };
                        l.scale = (nw / map.long()).clamp(0.002, 20.0);
                        l.stretch = (nh / (nw / s.aspect.max(1e-3))).clamp(0.01, 100.0);
                        let n = map.s2n(nc);
                        l.x = n[0];
                        l.y = n[1];
                    }
                    1 => {
                        // scale about the centre, or about the side touching a neighbour
                        let (ax, ay) = start_box.axes();
                        let fixed = c + ax * (d.anchor.x * start_box.half.x) + ay * (d.anchor.y * start_box.half.y);
                        let r0 = (d.origin - fixed).length().max(1.0);
                        let mut k = (p - fixed).length() / r0;
                        if magnet && let Some((ks, line)) = crate::magnet::snap_scale(start_box, d.anchor, k, &others, MAGNET_PX) {
                            k = ks;
                            self.magnet_lines.push(line);
                        }
                        l.scale = (s.scale * k).clamp(0.005, 20.0);
                        let b = crate::magnet::scaled(start_box, d.anchor, l.scale / s.scale.max(1e-6));
                        let n = map.s2n(b.c);
                        l.x = n[0];
                        l.y = n[1];
                    }
                    _ => {
                        let a0 = (d.origin - c).y.atan2((d.origin - c).x);
                        let a1 = (p - c).y.atan2((p - c).x);
                        let mut deg = s.rotation + (a1 - a0).to_degrees();
                        deg = (deg + 180.0).rem_euclid(360.0) - 180.0;
                        if shift {
                            deg = (deg / 15.0).round() * 15.0;
                        } else if magnet && let Some(r) = crate::magnet::snap_rotation(deg.to_radians(), &others, 3f32.to_radians()) {
                            // line up with a neighbouring layer's angle
                            deg = (r.to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
                        }
                        l.rotation = deg;
                    }
                }
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.layer_drag = None;
                self.snap_lines = (None, None);
                self.magnet_lines.clear();
            }
        }

        let Some(i) = self.selected_layer() else { return };
        let l = &self.params.layers[i];
        let painter = ui.painter_at(clip);
        let (c, corners, knob) = layer_frame(l, &map);
        let shadow = Color32::from_black_alpha(110);
        let mut outline = corners.to_vec();
        outline.push(corners[0]);
        if l.locked {
            // a locked layer only shows where it is, without handles
            painter.add(egui::Shape::dashed_line(&outline, Stroke::new(1.2, Color32::from_white_alpha(170)), 6.0, 4.0));
            return;
        }
        painter.add(egui::Shape::line(outline.clone(), Stroke::new(3.0, Color32::from_black_alpha(70))));
        painter.add(egui::Shape::line(outline, Stroke::new(1.2, Color32::WHITE)));
        let top_mid = corners[0] + (corners[1] - corners[0]) * 0.5;
        painter.line_segment([top_mid, knob], Stroke::new(1.2, Color32::from_white_alpha(200)));
        // only while the canvas itself is under the pointer, not a sheet or menu above it
        let over_canvas = resp.hovered() || resp.dragged();
        let hover = ctx.input(|inp| inp.pointer.hover_pos()).filter(|p| over_canvas && clip.contains(*p));
        let active = self.layer_drag.as_ref().map(|d| d.kind);
        for p in corners {
            let big = active == Some(1) || hover.map(|h| h.distance(p) < 14.0).unwrap_or(false);
            let r = if big { 6.5 } else { 5.0 };
            painter.circle_filled(p + vec2(0.0, 1.0), r + 1.5, shadow);
            painter.circle_filled(p, r, Color32::WHITE);
        }
        for (kind, p) in layer_edges(l, &map) {
            let big = active == Some(kind) || hover.map(|h| h.distance(p) < 12.0).unwrap_or(false);
            let r = if big { 5.5 } else { 4.0 };
            painter.circle_filled(p + vec2(0.0, 1.0), r + 1.5, shadow);
            painter.circle_filled(p, r, Color32::WHITE);
            painter.circle_stroke(p, r, Stroke::new(1.0, k::ACCENT));
        }
        let guide = Stroke::new(1.0, Color32::from_rgb(255, 214, 10));
        if let Some(x) = self.snap_lines.0 {
            painter.line_segment([pos2(x, img.top()), pos2(x, img.bottom())], guide);
        }
        if let Some(y) = self.snap_lines.1 {
            painter.line_segment([pos2(img.left(), y), pos2(img.right(), y)], guide);
        }
        for line in &self.magnet_lines {
            painter.line_segment(*line, Stroke::new(1.5, Color32::from_rgb(255, 214, 10)));
        }
        let kb = active == Some(2) || hover.map(|h| h.distance(knob) < 14.0).unwrap_or(false);
        painter.circle_filled(knob + vec2(0.0, 1.0), 8.5, shadow);
        painter.circle_filled(knob, if kb { 8.0 } else { 7.0 }, k::ACCENT);
        painter.circle_stroke(knob, if kb { 8.0 } else { 7.0 }, Stroke::new(2.0, Color32::WHITE));
        let _ = c;
        // cursor feedback
        if let Some(kind) = active {
            ctx.set_cursor_icon(match kind {
                0 => egui::CursorIcon::Grabbing,
                1 => egui::CursorIcon::ResizeNwSe,
                3 | 4 => egui::CursorIcon::ResizeHorizontal,
                5 | 6 => egui::CursorIcon::ResizeVertical,
                _ => egui::CursorIcon::Alias,
            });
        } else if let Some(h) = hover {
            if let Some((kind, _)) = layer_edges(l, &map).into_iter().find(|(_, p)| p.distance(h) < 12.0) {
                ctx.set_cursor_icon(if kind <= 4 { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::ResizeVertical });
            } else if h.distance(knob) < 14.0 {
                ctx.set_cursor_icon(egui::CursorIcon::Alias);
            } else if corners.iter().any(|p| p.distance(h) < 14.0) {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            } else if layer_contains(l, &map, h) {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        // live size readout while transforming
        if active.is_some() {
            let (w, h) = l.size(1.0);
            let label = if l.stretch != 1.0 {
                format!("{:.0} × {:.0}%  ·  {:.0}°", w * 100.0, h * 100.0, l.rotation)
            } else {
                format!("{:.0}%  ·  {:.0}°", l.scale * 100.0, l.rotation)
            };
            let g = painter.layout_no_wrap(label, k::font_mono(11.5), k::LABEL);
            let pos = pos2(img.center().x, img.top() + 18.0);
            let bg = Rect::from_center_size(pos, g.size() + vec2(16.0, 6.0));
            painter.rect_filled(bg, CornerRadius::same(10), Color32::from_black_alpha(170));
            painter.galley(bg.center() - g.size() / 2.0, g, k::LABEL);
        }
    }

    fn tab_layers(&mut self, ui: &mut Ui) {
        if self.doc.is_none() {
            k::caption(ui, "Open a photo first, then add more photos on top of it.");
            return;
        }
        let ctx = ui.ctx().clone();
        k::caption(ui, "Place other photos on the canvas. Drag to move, corners to scale, side handles to stretch, the blue knob to rotate. Edges snap to the canvas and, with the magnet, to other layers (hold Ctrl to place freely); arrow keys nudge. You can also drop photos onto the canvas or paste an image with Ctrl+V. Double-click a layer to rename it.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if k::button(ui, "Add Photo…", Btn::Secondary, true).clicked() {
                self.pick_layer_photos(&ctx);
            }
            if self.layers_loading > 0 {
                ui.spinner();
            }
        });
        ui.add_space(10.0);

        // keep each group's layers together, and the selection on the same layer
        let order = crate::layers::normalize_groups(&mut self.params.layers, &mut self.params.layer_groups);
        if let Some(s) = self.layer_sel {
            self.layer_sel = order.iter().position(|&o| o == s);
        }

        enum Act {
            Select(usize),
            Toggle(usize),
            Duplicate(usize),
            Up(usize),
            Down(usize),
            Delete(usize),
            /// layer, insert above this layer (None: bottom), group it lands in
            Move(usize, Option<usize>, u32),
            Rename(usize),
            Color(usize, u8),
            Lock(usize),
            NewGroup(usize),
            JoinGroup(usize, u32),
            GroupToggle(u32),
            GroupCollapse(u32),
            GroupRename(u32),
            GroupColor(u32, u8),
            Ungroup(u32),
            GroupDelete(u32),
        }
        /// A row of the list, top of the stack first.
        enum Entry {
            Group(usize),
            Layer(usize, bool),
        }
        let mut act = None;
        let mut renamed: Option<(Rename, Option<String>)> = None;
        let sel = self.selected_layer();
        let n = self.params.layers.len();
        let groups = self.params.layer_groups.clone();
        let mut entries = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        for i in (0..n).rev() {
            let g = self.params.layers[i].group;
            if g == 0 {
                entries.push(Entry::Layer(i, false));
                continue;
            }
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            let Some(gi) = groups.iter().position(|x| x.id == g) else { continue };
            entries.push(Entry::Group(gi));
            if !groups[gi].collapsed {
                entries.extend((0..n).rev().filter(|&j| self.params.layers[j].group == g).map(|j| Entry::Layer(j, true)));
            }
        }
        // where each row sits, for dragging: (rect, layer, group the row belongs to, top member if a header)
        let mut rows: Vec<(Rect, Option<usize>, u32, Option<usize>)> = Vec::new();
        egui::Frame::NONE.fill(k::CARD).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for e in &entries {
                match *e {
                    Entry::Group(gi) => {
                        let g = &groups[gi];
                        let members = self.params.layers.iter().filter(|l| l.group == g.id).count();
                        let top = (0..n).rev().find(|&j| self.params.layers[j].group == g.id);
                        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                        rows.push((r, None, 0, top));
                        let bg = if resp.is_pointer_button_down_on() { k::FILL_HI } else if resp.hovered() { k::FILL } else { Color32::TRANSPARENT };
                        ui.painter().rect_filled(r, CornerRadius::same(8), bg);
                        // disclosure triangle
                        let t = pos2(r.left() + 11.0, r.center().y);
                        let tri = if g.collapsed {
                            vec![t + vec2(-3.0, -5.0), t + vec2(4.0, 0.0), t + vec2(-3.0, 5.0)]
                        } else {
                            vec![t + vec2(-5.0, -3.0), t + vec2(5.0, -3.0), t + vec2(0.0, 4.0)]
                        };
                        ui.painter().add(egui::Shape::convex_polygon(tri, k::LABEL2, Stroke::NONE));
                        let dot = pos2(r.left() + 30.0, r.center().y);
                        let dresp = ui.interact(Rect::from_center_size(dot, vec2(22.0, 22.0)), ui.scope_id().with(("group_dot", g.id)), Sense::click());
                        let gc = label_color(g.color);
                        if g.visible {
                            ui.painter().circle_filled(dot, 5.0, gc.unwrap_or(k::LABEL2));
                        } else {
                            ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.5, gc.unwrap_or(k::LABEL3)));
                        }
                        let dresp = dresp.on_hover_text(if g.visible { "Hide group" } else { "Show group" });
                        let col = if g.visible { k::LABEL } else { k::LABEL3 };
                        let name_r = Rect::from_min_max(pos2(r.left() + 18.0, r.top()), r.max);
                        match &mut self.renaming {
                            Some((Rename::Group(id), buf, focus)) if *id == g.id => match rename_field(ui, name_r, buf, focus) {
                                RenameOutcome::Editing => {}
                                RenameOutcome::Commit(s) => renamed = Some((Rename::Group(g.id), Some(s))),
                                RenameOutcome::Cancel => renamed = Some((Rename::Group(g.id), None)),
                            },
                            _ => {
                                let tag = ui.painter().layout_no_wrap(format!("{members} layer{}", if members == 1 { "" } else { "s" }), k::font(11.0), k::LABEL3);
                                let tag_x = r.right() - 10.0 - tag.size().x;
                                ui.painter().galley(pos2(tag_x, r.center().y - tag.size().y / 2.0), tag, k::LABEL3);
                                let name = elided(ui, &g.name, k::font_semibold(13.0), col, tag_x - 10.0 - (r.left() + 44.0));
                                ui.painter().galley(pos2(r.left() + 44.0, r.center().y - name.size().y / 2.0), name, col);
                                if resp.double_clicked() && !dresp.hovered() {
                                    act = Some(Act::GroupRename(g.id));
                                }
                            }
                        }
                        if dresp.clicked() {
                            act = Some(Act::GroupToggle(g.id));
                        } else if resp.clicked() && act.is_none() {
                            act = Some(Act::GroupCollapse(g.id));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Rename").clicked() {
                                act = Some(Act::GroupRename(g.id));
                            }
                            if let Some(c) = color_menu(ui, g.color) {
                                act = Some(Act::GroupColor(g.id, c));
                            }
                            ui.separator();
                            if ui.button("Ungroup").clicked() {
                                act = Some(Act::Ungroup(g.id));
                            }
                            if ui.button("Delete Group and Layers").clicked() {
                                act = Some(Act::GroupDelete(g.id));
                            }
                        });
                    }
                    Entry::Layer(i, indented) => {
                        let l = &self.params.layers[i];
                        let ind = if indented { 16.0 } else { 0.0 };
                        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click_and_drag());
                        rows.push((r, Some(i), l.group, None));
                        if resp.drag_started() && self.renaming.is_none() {
                            self.list_drag = Some(i);
                        }
                        let bg = if self.list_drag == Some(i) {
                            k::FILL_HI
                        } else if sel == Some(i) {
                            Color32::from_rgba_unmultiplied(10, 132, 255, 70)
                        } else if resp.is_pointer_button_down_on() {
                            k::FILL_HI
                        } else if resp.hovered() {
                            k::FILL
                        } else {
                            Color32::TRANSPARENT
                        };
                        let rr = Rect::from_min_max(pos2(r.left() + ind, r.top()), r.max);
                        ui.painter().rect_filled(rr, CornerRadius::same(8), bg);
                        let dot = pos2(rr.left() + 16.0, r.center().y);
                        let dresp = ui.interact(Rect::from_center_size(dot, vec2(24.0, 24.0)), ui.scope_id().with(("layer_dot", i)), Sense::click());
                        if l.visible {
                            ui.painter().circle_filled(dot, 5.0, label_color(l.color).unwrap_or(k::ACCENT));
                        } else {
                            ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.5, label_color(l.color).unwrap_or(k::LABEL3)));
                        }
                        let col = if l.visible { k::LABEL } else { k::LABEL3 };
                        match &mut self.renaming {
                            Some((Rename::Layer(j), buf, focus)) if *j == i => match rename_field(ui, rr, buf, focus) {
                                RenameOutcome::Editing => {}
                                RenameOutcome::Commit(name) => renamed = Some((Rename::Layer(i), Some(name))),
                                RenameOutcome::Cancel => renamed = Some((Rename::Layer(i), None)),
                            },
                            _ => {
                                let tag = format!(
                                    "{}{}{}",
                                    if l.locked { "Locked · " } else { "" },
                                    l.blend.label(),
                                    if l.opacity < 0.999 { format!(" · {:.0}%", l.opacity * 100.0) } else { String::new() }
                                );
                                let tag = ui.painter().layout_no_wrap(tag, k::font(11.0), k::LABEL3);
                                let tag_x = r.right() - 10.0 - tag.size().x;
                                ui.painter().galley(pos2(tag_x, r.center().y - tag.size().y / 2.0), tag, k::LABEL3);
                                let name = elided(ui, &l.name, k::font(13.0), col, tag_x - 10.0 - (rr.left() + 32.0));
                                let full = name.elided;
                                ui.painter().galley(pos2(rr.left() + 32.0, r.center().y - name.size().y / 2.0), name, col);
                                let resp = if full && self.list_drag.is_none() { resp.clone().on_hover_text(&l.name) } else { resp.clone() };
                                if resp.double_clicked() && !dresp.hovered() {
                                    act = Some(Act::Rename(i));
                                }
                            }
                        }
                        if dresp.clicked() {
                            act = Some(Act::Toggle(i));
                        } else if resp.clicked() && act.is_none() {
                            act = Some(Act::Select(i));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Rename").clicked() {
                                act = Some(Act::Rename(i));
                            }
                            if let Some(c) = color_menu(ui, l.color) {
                                act = Some(Act::Color(i, c));
                            }
                            if ui.button(if l.locked { "Unlock" } else { "Lock" }).clicked() {
                                act = Some(Act::Lock(i));
                            }
                            ui.menu_button("Group", |ui| {
                                if ui.button("New Group").clicked() {
                                    act = Some(Act::NewGroup(i));
                                    ui.close();
                                }
                                for g in groups.iter().filter(|g| g.id != l.group) {
                                    if ui.button(format!("Move to {}", g.name)).clicked() {
                                        act = Some(Act::JoinGroup(i, g.id));
                                        ui.close();
                                    }
                                }
                                if l.group != 0 && ui.button("Remove from Group").clicked() {
                                    act = Some(Act::JoinGroup(i, 0));
                                    ui.close();
                                }
                            });
                            ui.separator();
                            if ui.button("Duplicate").clicked() {
                                act = Some(Act::Duplicate(i));
                            }
                            if i + 1 < n && ui.button("Bring Forward").clicked() {
                                act = Some(Act::Up(i));
                            }
                            if i > 0 && ui.button("Send Backward").clicked() {
                                act = Some(Act::Down(i));
                            }
                            if ui.button("Delete").clicked() {
                                act = Some(Act::Delete(i));
                            }
                        });
                    }
                }
            }
            // drag a row up or down to restack; an accent line shows where it lands
            if let Some(from) = self.list_drag {
                let y = ui.ctx().input(|inp| inp.pointer.latest_pos()).map(|p| p.y).unwrap_or(0.0);
                let slot = rows.iter().position(|row| y < row.0.center().y).unwrap_or(rows.len());
                if let Some(line_y) = rows.get(slot).map(|row| row.0.top() - 1.0).or_else(|| rows.last().map(|row| row.0.bottom() + 1.0)) {
                    let (l, rr) = (rows[0].0.left() + 6.0, rows[0].0.right() - 6.0);
                    ui.painter().line_segment([pos2(l, line_y), pos2(rr, line_y)], Stroke::new(2.0, k::ACCENT));
                }
                if !ui.ctx().input(|inp| inp.pointer.any_down()) {
                    // above a layer: join its group; above a group header: sit just above that group
                    let (above, group) = match rows.get(slot) {
                        Some((_, Some(j), g, _)) => (Some(*j), *g),
                        Some((_, None, _, top)) => (*top, 0),
                        None => (None, 0),
                    };
                    if above != Some(from) {
                        act = Some(Act::Move(from, above, group));
                    }
                    self.list_drag = None;
                }
            }
            // the photo itself, always at the bottom
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            ui.painter().text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, "Photo", k::font(13.0), k::LABEL3);
            ui.painter().text(pos2(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, "Background", k::font(11.0), k::LABEL3);
        });
        let next_id = self.params.layer_groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
        let layers = &mut self.params.layers;
        let lgroups = &mut self.params.layer_groups;
        match act {
            Some(Act::Select(i)) => self.layer_sel = Some(i),
            Some(Act::Toggle(i)) => layers[i].visible = !layers[i].visible,
            Some(Act::Duplicate(i)) => {
                let mut l = layers[i].clone();
                l.name = format!("{} copy", l.name);
                l.x += 0.03;
                l.y += 0.03;
                layers.insert(i + 1, l);
                self.layer_sel = Some(i + 1);
            }
            Some(Act::Up(i)) => {
                layers.swap(i, i + 1);
                self.layer_sel = Some(i + 1);
            }
            Some(Act::Down(i)) => {
                layers.swap(i, i - 1);
                self.layer_sel = Some(i - 1);
            }
            Some(Act::Delete(i)) => {
                layers.remove(i);
                self.layer_sel = None;
            }
            Some(Act::Move(from, above, group)) => {
                let mut l = layers.remove(from);
                l.group = group;
                let mut at = above.map(|a| a + 1).unwrap_or(0);
                if at > from {
                    at -= 1;
                }
                let at = at.min(layers.len());
                layers.insert(at, l);
                self.layer_sel = Some(at);
            }
            Some(Act::Rename(i)) => self.renaming = Some((Rename::Layer(i), layers[i].name.clone(), true)),
            Some(Act::Color(i, c)) => layers[i].color = c,
            Some(Act::Lock(i)) => layers[i].locked = !layers[i].locked,
            Some(Act::NewGroup(i)) => {
                lgroups.push(crate::layers::LayerGroup { id: next_id, name: format!("Group {}", lgroups.len() + 1), ..Default::default() });
                layers[i].group = next_id;
                self.renaming = Some((Rename::Group(next_id), format!("Group {}", lgroups.len()), true));
            }
            Some(Act::JoinGroup(i, g)) => layers[i].group = g,
            Some(Act::GroupToggle(id)) => {
                if let Some(g) = lgroups.iter_mut().find(|g| g.id == id) {
                    g.visible = !g.visible;
                }
            }
            Some(Act::GroupCollapse(id)) => {
                if let Some(g) = lgroups.iter_mut().find(|g| g.id == id) {
                    g.collapsed = !g.collapsed;
                }
            }
            Some(Act::GroupRename(id)) => {
                let name = lgroups.iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_default();
                self.renaming = Some((Rename::Group(id), name, true));
            }
            Some(Act::GroupColor(id, c)) => {
                if let Some(g) = lgroups.iter_mut().find(|g| g.id == id) {
                    g.color = c;
                }
            }
            Some(Act::Ungroup(id)) => {
                for l in layers.iter_mut().filter(|l| l.group == id) {
                    l.group = 0;
                }
            }
            Some(Act::GroupDelete(id)) => {
                layers.retain(|l| l.group != id);
                self.layer_sel = None;
            }
            None => {}
        }
        if let Some((target, name)) = renamed {
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                match target {
                    Rename::Layer(i) => {
                        if let Some(l) = self.params.layers.get_mut(i) {
                            l.name = name;
                        }
                    }
                    Rename::Group(id) => {
                        if let Some(g) = self.params.layer_groups.iter_mut().find(|g| g.id == id) {
                            g.name = name;
                        }
                    }
                    Rename::Mask(_) => {}
                }
            }
            self.renaming = None;
        }
        let order = crate::layers::normalize_groups(&mut self.params.layers, &mut self.params.layer_groups);
        if let Some(s) = self.layer_sel {
            self.layer_sel = order.iter().position(|&o| o == s);
        }
        if n > 0 {
            k::caption(ui, "Drag layers in the list to restack them. Right-click a layer to lock it or put it in a group.");
        }
        ui.add_space(10.0);

        let Some(i) = self.selected_layer() else {
            if n > 0 {
                k::caption(ui, "Select a layer to edit it.");
            }
            return;
        };
        if self.params.layers[i].locked {
            k::caption(ui, "This layer is locked, so it can't be moved or changed.");
            ui.add_space(4.0);
            if k::button(ui, "Unlock", Btn::Secondary, true).clicked() {
                self.params.layers[i].locked = false;
            }
            return;
        }
        let mut fit = None;
        let mut stretch_full = false;
        let has_cutout = self.params.masks.iter().any(|m| m.enabled && m.transparent);
        let magnet_before = (self.prefs.magnet, self.prefs.magnet_pivot);
        let l = &mut self.params.layers[i];
        let prefs = &mut self.prefs;
        let d = Layer::default();
        if k::section(ui, "Blending", l.opacity < 1.0 || l.blend != Blend::Normal || !l.graded || l.behind, |ui| {
            k::slider(ui, "Opacity", &mut l.opacity, 0.0..=1.0, 1.0, Fmt::Amount);
            ui.add_space(2.0);
            let w = ui.available_width();
            let cw = (w - 12.0) / 3.0;
            for row in Blend::ALL.chunks(3) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for b in row {
                        if k::chip(ui, vec2(cw, 28.0), b.label(), l.blend == *b).clicked() {
                            l.blend = *b;
                        }
                    }
                });
                ui.add_space(2.0);
            }
            ui.add_space(2.0);
            k::switch_row(ui, "Apply Preset to Layer", &mut l.graded);
            k::caption(ui, "On: the layer is graded with the photo. Off: it sits on top of the finished edit, good for light leaks and textures (try Screen).");
            ui.add_space(4.0);
            k::switch_row(ui, "Behind Photo", &mut l.behind);
            if l.behind && !has_cutout {
                k::caption(ui, "Nothing is cut out of the photo yet, so this layer is hidden. Cut out an area in the Masks tab (Cut Out → Make Transparent) to let it show through.");
            } else {
                k::caption(ui, "Puts the layer under the photo, where it shows through areas cut out with a mask. Use it to replace a sky or background.");
            }
            ui.add_space(4.0);
        }) {
            l.opacity = d.opacity;
            l.blend = d.blend;
            l.graded = d.graded;
            l.behind = d.behind;
        }
        if k::section(ui, "Transform", l.rotation != 0.0 || l.flip || l.stretch != 1.0, |ui| {
            k::slider(ui, "Position X", &mut l.x, -0.5..=1.5, 0.5, Fmt::Signed);
            k::slider(ui, "Position Y", &mut l.y, -0.5..=1.5, 0.5, Fmt::Signed);
            let mut pct = l.scale * 100.0;
            k::slider(ui, "Size (% of long edge)", &mut pct, 1.0..=300.0, 50.0, Fmt::Px);
            l.scale = pct / 100.0;
            k::slider(ui, "Stretch Vertically", &mut l.stretch, 0.1..=4.0, 1.0, Fmt::Amount);
            k::slider(ui, "Rotation", &mut l.rotation, -180.0..=180.0, 0.0, Fmt::Deg);
            k::switch_row(ui, "Flip Horizontal", &mut l.flip);
            ui.add_space(4.0);
            k::switch_row(ui, "Magnet", &mut prefs.magnet);
            k::caption(ui, "Pulls the layer flush against other layers as you move, scale, stretch or turn it. Hold Ctrl to place freely.");
            if prefs.magnet {
                k::switch_row(ui, "Auto Pivot", &mut prefs.magnet_pivot);
                k::caption(ui, "The layer turns to match the one it snaps to, and scaling keeps the side touching it in place.");
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if k::button(ui, "Fit", Btn::Secondary, true).clicked() {
                    fit = Some(false);
                }
                if k::button(ui, "Fill", Btn::Secondary, true).clicked() {
                    fit = Some(true);
                }
                if k::button(ui, "Stretch", Btn::Secondary, true).on_hover_text("Stretch to every side of the canvas").clicked() {
                    stretch_full = true;
                }
            });
            ui.add_space(4.0);
        }) {
            l.rotation = 0.0;
            l.flip = false;
            l.stretch = 1.0;
        }
        if (self.prefs.magnet, self.prefs.magnet_pivot) != magnet_before {
            self.persist();
        }
        if let Some(fill) = fit {
            self.fit_layer(i, fill);
        }
        if stretch_full {
            self.stretch_layer(i);
        }
        ui.add_space(4.0);
        if k::button(ui, "Delete Layer", Btn::Plain, true).clicked() {
            self.params.layers.remove(i);
            self.layer_sel = None;
        }
    }

    fn add_mask(&mut self, kind: u8) {
        let c = self.params.crop;
        let (fw, fh) = self.frame_dims();
        let l = fw.max(fh);
        let (cx, cy) = ((c[0] + c[2]) * 0.5, (c[1] + c[3]) * 0.5);
        let (cw, ch) = ((c[2] - c[0]) * fw / l, (c[3] - c[1]) * fh / l);
        let (shape, refine, name) = match kind {
            0 => (Shape::Linear { a: [cx, c[1] + (c[3] - c[1]) * 0.08], b: [cx, c[1] + (c[3] - c[1]) * 0.5] }, Refine::None, "Linear"),
            1 => (Shape::Radial { c: [cx, cy], rx: cw * 0.28, ry: ch * 0.32, angle: 0.0, feather: 0.6 }, Refine::None, "Radial"),
            2 => (Shape::Brush { strokes: Vec::new() }, Refine::None, "Brush"),
            3 => (Shape::Everything, Refine::Luminance, "Luminance"),
            5 => (Shape::Object { seeds: Vec::new(), threshold: 0.25, connected: true, smooth: 0.3, feather: 0.15 }, Refine::None, "Object"),
            _ => (Shape::Everything, Refine::Color, "Color"),
        };
        let n = self.params.masks.iter().filter(|m| m.name.starts_with(name)).count() + 1;
        self.params.masks.push(Mask { name: format!("{name} {n}"), shape, refine, ..Mask::default() });
        self.mask_sel = Some(self.params.masks.len() - 1);
        self.overlay_hold = true;
        self.picking_hue = false;
    }

    fn tab_masks(&mut self, ui: &mut Ui) {
        if self.doc.is_none() {
            k::caption(ui, "Open a photo to add masks.");
            return;
        }
        k::caption(ui, "Masks adjust part of the photo. Add one, shape it on the canvas, then adjust.");
        ui.add_space(8.0);

        // --- add -----------------------------------------------------------
        let w = ui.available_width();
        let mut add = None;
        for row in [&[("Linear", 0u8), ("Radial", 1), ("Brush", 2)][..], &[("Object", 5), ("Luminance", 3), ("Color", 4)][..]] {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let cw = (w - 6.0 * (row.len() - 1) as f32) / row.len() as f32;
                for (label, kind) in row {
                    if k::chip(ui, vec2(cw, 30.0), &format!("+ {label}"), false).clicked() {
                        add = Some(*kind);
                    }
                }
            });
            ui.add_space(2.0);
        }
        if let Some(kind) = add {
            self.add_mask(kind);
        }
        ui.add_space(10.0);

        // --- list ----------------------------------------------------------
        if self.params.masks.is_empty() {
            k::caption(ui, "No masks yet.");
            return;
        }
        enum Act {
            Select(usize),
            Toggle(usize),
            Duplicate(usize),
            Invert(usize),
            Up(usize),
            Down(usize),
            Delete(usize),
            Rename(usize),
            Color(usize, u8),
        }
        let mut act = None;
        let mut renamed = None;
        let sel = self.selected_mask();
        let n = self.params.masks.len();
        egui::Frame::NONE.fill(k::CARD).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, m) in self.params.masks.iter().enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
                let pressed = resp.is_pointer_button_down_on();
                let bg = if sel == Some(i) {
                    Color32::from_rgba_unmultiplied(10, 132, 255, 70)
                } else if pressed {
                    k::FILL_HI
                } else if resp.hovered() {
                    k::FILL
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(r, CornerRadius::same(8), bg);
                // visibility dot
                let dot = pos2(r.left() + 16.0, r.center().y);
                let dot_r = Rect::from_center_size(dot, vec2(24.0, 24.0));
                let dresp = ui.interact(dot_r, ui.scope_id().with(("mask_dot", i)), Sense::click());
                let dot_col = label_color(m.color).unwrap_or(k::ACCENT);
                if m.enabled {
                    ui.painter().circle_filled(dot, 5.0, dot_col);
                } else {
                    ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.5, label_color(m.color).unwrap_or(k::LABEL3)));
                }
                let dresp = dresp.on_hover_text(if m.enabled { "Hide mask" } else { "Show mask" });
                let col = if m.enabled { k::LABEL } else { k::LABEL3 };
                match &mut self.renaming {
                    Some((Rename::Mask(j), buf, focus)) if *j == i => match rename_field(ui, r, buf, focus) {
                        RenameOutcome::Editing => {}
                        RenameOutcome::Commit(name) => renamed = Some((i, Some(name))),
                        RenameOutcome::Cancel => renamed = Some((i, None)),
                    },
                    _ => {
                        let mut uses: Vec<&str> = Vec::new();
                        if m.invert {
                            uses.push("Inverted");
                        }
                        if m.transparent {
                            uses.push("Cut Out");
                        }
                        if !m.adj.is_identity() {
                            uses.push("Edited");
                        }
                        if m.effects {
                            uses.push("Effects");
                        }
                        if m.glitch {
                            uses.push("Glitch");
                        }
                        let tag = if uses.is_empty() { "No edits".to_string() } else { uses.join(" · ") };
                        let tag = ui.painter().layout_no_wrap(tag, k::font(11.0), k::LABEL3);
                        let tag_x = r.right() - 10.0 - tag.size().x;
                        ui.painter().galley(pos2(tag_x, r.center().y - tag.size().y / 2.0), tag, k::LABEL3);
                        let name = elided(ui, &m.name, k::font(13.0), col, tag_x - 10.0 - (r.left() + 32.0));
                        let full = name.elided;
                        ui.painter().galley(pos2(r.left() + 32.0, r.center().y - name.size().y / 2.0), name, col);
                        let resp = if full { resp.clone().on_hover_text(&m.name) } else { resp.clone() };
                        if resp.double_clicked() && !dresp.hovered() {
                            act = Some(Act::Rename(i));
                        }
                    }
                }
                if dresp.clicked() {
                    act = Some(Act::Toggle(i));
                } else if resp.clicked() && act.is_none() {
                    act = Some(Act::Select(i));
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        act = Some(Act::Rename(i));
                    }
                    if let Some(c) = color_menu(ui, m.color) {
                        act = Some(Act::Color(i, c));
                    }
                    ui.separator();
                    if ui.button("Duplicate").clicked() {
                        act = Some(Act::Duplicate(i));
                    }
                    if ui.button("Invert").clicked() {
                        act = Some(Act::Invert(i));
                    }
                    if i > 0 && ui.button("Move Up").clicked() {
                        act = Some(Act::Up(i));
                    }
                    if i + 1 < n && ui.button("Move Down").clicked() {
                        act = Some(Act::Down(i));
                    }
                    if ui.button("Delete").clicked() {
                        act = Some(Act::Delete(i));
                    }
                });
            }
        });
        let masks = &mut self.params.masks;
        match act {
            Some(Act::Select(i)) => {
                if self.mask_sel != Some(i) {
                    self.overlay_hold = true;
                }
                self.mask_sel = Some(i);
                self.picking_hue = false;
            }
            Some(Act::Toggle(i)) => masks[i].enabled = !masks[i].enabled,
            Some(Act::Duplicate(i)) => {
                let mut m = masks[i].clone();
                m.name = format!("{} copy", m.name);
                // one mask holds the Effects / Glitch tabs; the copy starts plain
                m.effects = false;
                m.glitch = false;
                masks.insert(i + 1, m);
                self.mask_sel = Some(i + 1);
            }
            Some(Act::Invert(i)) => masks[i].invert = !masks[i].invert,
            Some(Act::Up(i)) => {
                masks.swap(i, i - 1);
                self.mask_sel = Some(i - 1);
            }
            Some(Act::Down(i)) => {
                masks.swap(i, i + 1);
                self.mask_sel = Some(i + 1);
            }
            Some(Act::Delete(i)) => {
                masks.remove(i);
                self.mask_sel = None;
            }
            Some(Act::Rename(i)) => self.renaming = Some((Rename::Mask(i), masks[i].name.clone(), true)),
            Some(Act::Color(i, c)) => masks[i].color = c,
            None => {}
        }
        if let Some((i, name)) = renamed {
            if let Some(name) = name.filter(|n| !n.is_empty())
                && let Some(m) = self.params.masks.get_mut(i)
            {
                m.name = name;
            }
            self.renaming = None;
        }
        k::caption(ui, "Double-click a mask to rename it; right-click for colour and more. Masks apply top to bottom.");
        ui.add_space(10.0);

        // --- selected mask ---------------------------------------------------
        let Some(i) = self.selected_mask() else {
            k::caption(ui, "Select a mask to edit it.");
            return;
        };
        let m = &mut self.params.masks[i];

        if k::section(ui, "Shape", m.invert || m.amount < 1.0, |ui| {
            match &mut m.shape {
                Shape::Linear { .. } => {
                    k::caption(ui, "Drag the end points on the photo; the effect fades from the first line to the second. Drag the blue pin to move it.");
                }
                Shape::Radial { feather, .. } => {
                    k::caption(ui, "Drag the blue pin to move, the white handles to resize and rotate.");
                    ui.add_space(4.0);
                    k::slider(ui, "Feather", feather, 0.0..=1.0, 0.6, Fmt::Amount);
                }
                Shape::Brush { strokes } => {
                    let mut idx = self.brush_erase as usize;
                    if k::segmented(ui, "brush_mode", &mut idx, &["Paint", "Erase"]) {
                        self.brush_erase = idx == 1;
                    }
                    ui.add_space(4.0);
                    k::slider(ui, "Size", &mut self.brush_size, 0.0..=1.0, 0.45, Fmt::Amount);
                    k::slider(ui, "Feather", &mut self.brush_feather, 0.0..=1.0, 0.6, Fmt::Amount);
                    k::slider(ui, "Flow", &mut self.brush_flow, 0.05..=1.0, 1.0, Fmt::Amount);
                    ui.horizontal(|ui| {
                        k::caption(ui, &format!("{} stroke{}", strokes.len(), if strokes.len() == 1 { "" } else { "s" }));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if k::button(ui, "Clear", Btn::Plain, !strokes.is_empty()).clicked() {
                                strokes.clear();
                            }
                        });
                    });
                    k::caption(ui, "Paint on the photo. Hold Alt to erase, [ and ] to resize, Space to pan.");
                }
                Shape::Everything => {
                    k::caption(ui, "Covers the whole photo, narrowed by the range below.");
                }
                Shape::Object { seeds, threshold, connected, smooth, feather } => {
                    k::caption(ui, "Click the subject on the photo. + points add to the selection, − points mark where it should not go.");
                    ui.add_space(4.0);
                    let mut mi = self.seed_remove as usize;
                    if k::segmented(ui, "obj_points", &mut mi, &["+ Add", "− Remove"]) {
                        self.seed_remove = mi == 1;
                    }
                    k::caption(ui, "Alt+click places the other kind. Click a point (or right-click it) to delete it.");
                    ui.add_space(4.0);
                    let before = (*threshold, *connected, *smooth);
                    k::slider(ui, "Threshold", threshold, 0.0..=1.0, 0.25, Fmt::Amount);
                    let mut ci = (!*connected) as usize;
                    if k::segmented(ui, "obj_mode", &mut ci, &["Connected", "Anywhere"]) {
                        *connected = ci == 0;
                    }
                    ui.add_space(4.0);
                    k::slider(ui, "Smooth Edges", smooth, 0.0..=1.0, 0.3, Fmt::Amount);
                    k::slider(ui, "Feather", feather, 0.0..=1.0, 0.15, Fmt::Amount);
                    if before != (*threshold, *connected, *smooth) {
                        self.overlay_hold = true;
                    }
                    ui.horizontal(|ui| {
                        k::caption(ui, &format!("{} point{}", seeds.len(), if seeds.len() == 1 { "" } else { "s" }));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if k::button(ui, "Clear", Btn::Plain, !seeds.is_empty()).clicked() {
                                seeds.clear();
                            }
                            if k::button(ui, "Undo Point", Btn::Plain, !seeds.is_empty()).clicked() {
                                seeds.pop();
                            }
                        });
                    });
                    k::caption(ui, "Connected follows the object to its edges; Anywhere picks up the same colour all over.");
                }
            }
            ui.add_space(4.0);
            k::slider(ui, "Amount", &mut m.amount, 0.0..=1.0, 1.0, Fmt::Amount);
            k::switch_row(ui, "Invert", &mut m.invert);
            // reflects what's on screen: switching it off also ends the hold
            let mut visible = self.show_overlay || self.overlay_hold;
            if k::switch_row(ui, "Show Overlay", &mut visible).changed() {
                self.show_overlay = visible;
                self.overlay_hold = false;
            }
            k::caption(ui, "The red overlay stays up while you shape the mask and hides when you adjust. Hold O to peek.");
            ui.add_space(4.0);
        }) {
            m.invert = false;
            m.amount = 1.0;
        }

        let d = Mask::default();
        let range_mod = m.refine != Refine::None;
        if k::section(ui, "Range", range_mod, |ui| {
            let mut idx = match m.refine {
                Refine::None => 0,
                Refine::Luminance => 1,
                Refine::Color => 2,
            };
            if k::segmented(ui, "mask_refine", &mut idx, &["All", "Luminance", "Color"]) {
                m.refine = [Refine::None, Refine::Luminance, Refine::Color][idx];
                self.overlay_hold = true;
            }
            ui.add_space(4.0);
            match m.refine {
                Refine::None => k::caption(ui, "Limit the mask to a band of brightness or a colour."),
                Refine::Luminance => {
                    k::slider(ui, "From", &mut m.lum_low, 0.0..=1.0, d.lum_low, Fmt::Amount);
                    k::slider(ui, "To", &mut m.lum_high, 0.0..=1.0, d.lum_high, Fmt::Amount);
                    k::slider(ui, "Smoothness", &mut m.lum_soft, 0.0..=0.5, d.lum_soft, Fmt::Amount);
                }
                Refine::Color => {
                    k::slider(ui, "Hue", &mut m.hue, 0.0..=360.0, d.hue, Fmt::Deg);
                    hue_strip(ui);
                    k::slider(ui, "Range", &mut m.hue_range, 0.0..=180.0, d.hue_range, Fmt::Deg);
                    k::slider(ui, "Smoothness", &mut m.hue_soft, 0.0..=90.0, d.hue_soft, Fmt::Deg);
                    ui.add_space(2.0);
                    // turns blue while it waits for a click on the photo
                    let (label, kind) = if self.picking_hue { ("Click a Colour on the Photo…", Btn::Primary) } else { ("Pick from Photo", Btn::Secondary) };
                    if k::button(ui, label, kind, true).clicked() {
                        self.picking_hue = !self.picking_hue;
                    }
                    if self.picking_hue {
                        k::caption(ui, "Esc cancels.");
                    }
                }
            }
            ui.add_space(4.0);
        }) {
            m.refine = Refine::None;
            m.lum_low = d.lum_low;
            m.lum_high = d.lum_high;
            m.lum_soft = d.lum_soft;
            m.hue = d.hue;
            m.hue_range = d.hue_range;
            m.hue_soft = d.hue_soft;
        }

        if k::section(ui, "Cut Out", m.transparent, |ui| {
            k::switch_row(ui, "Make Transparent", &mut m.transparent);
            k::caption(ui, "Removes this area from the picture. To remove a background, select the subject, turn on Invert, then this.");
            k::caption(ui, "Export as PNG, TIFF or WebP to keep the transparency. JPEG fills it with white.");
            ui.add_space(4.0);
        }) {
            m.transparent = false;
        }

        let a = &mut m.adj;
        let adj_before = a.clone();
        let reset_adj = k::section(ui, "Adjustments", !a.is_identity(), |ui| {
            k::slider(ui, "Exposure", &mut a.exposure, -2.0..=2.0, 0.0, Fmt::Ev);
            k::slider(ui, "Contrast", &mut a.contrast, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Highlights", &mut a.highlights, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Shadows", &mut a.shadows, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Temperature", &mut a.temperature, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Tint", &mut a.tint, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Saturation", &mut a.saturation, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Sharpness", &mut a.sharpness, -1.0..=1.0, 0.0, Fmt::Signed);
            k::slider(ui, "Color", &mut a.tone, 0.0..=1.0, 0.0, Fmt::Amount);
            if a.tone > 0.0 {
                k::slider(ui, "Color Hue", &mut a.tone_hue, 0.0..=360.0, 35.0, Fmt::Deg);
                hue_strip(ui);
            }
            ui.add_space(4.0);
        });
        if reset_adj {
            *a = Local { tone_hue: 35.0, ..Local::default() };
        }
        // adjusting means the shape is done: let the picture show through
        if *a != adj_before {
            self.overlay_hold = false;
        }

        ui.add_space(4.0);
        if k::button(ui, "Delete Mask", Btn::Plain, true).clicked() {
            self.params.masks.remove(i);
            self.mask_sel = None;
        }
    }

    fn tab_retouch(&mut self, ui: &mut Ui) {
        if self.doc.is_none() {
            k::caption(ui, "Open a photo to retouch it.");
            return;
        }
        k::caption(ui, "Paint over something to cover it with another part of the photo.");
        ui.add_space(8.0);
        k::section(ui, "Clone", false, |ui| {
            k::caption(ui, "1. Ctrl+click the spot to copy from (or use the button).");
            k::caption(ui, "2. Paint over what you want to remove. The green circle shows what is being copied, and it follows the brush at the same distance.");
            ui.add_space(6.0);
            // the button grows while waiting for the click, so the status sits below it
            let (label, kind) = if self.clone_pick_source { ("Click Where to Copy From…", Btn::Primary) } else { ("Choose Source", Btn::Secondary) };
            if k::button(ui, label, kind, true).clicked() {
                self.clone_pick_source = !self.clone_pick_source;
            }
            k::caption(
                ui,
                match (self.clone_pick_source, self.clone_source.is_some()) {
                    (true, _) => "Esc cancels.",
                    (false, true) => "Source set. Ctrl+click again to choose another.",
                    (false, false) => "No source yet.",
                },
            );
            ui.add_space(6.0);
            k::slider(ui, "Size", &mut self.clone_size, 0.0..=1.0, 0.35, Fmt::Amount);
            k::slider(ui, "Feather", &mut self.clone_feather, 0.0..=1.0, 0.5, Fmt::Amount);
            k::slider(ui, "Opacity", &mut self.clone_opacity, 0.05..=1.0, 1.0, Fmt::Amount);
            let n = self.params.retouch.len();
            ui.horizontal(|ui| {
                k::caption(ui, &format!("{n} stroke{}", if n == 1 { "" } else { "s" }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if k::button(ui, "Clear", Btn::Plain, n > 0).clicked() {
                        self.params.retouch.clear();
                    }
                    if k::button(ui, "Undo Stroke", Btn::Plain, n > 0).clicked() {
                        self.params.retouch.pop();
                    }
                });
            });
            k::caption(ui, "[ and ] resize the brush. Hold Space to pan.");
            ui.add_space(4.0);
        });
    }

    fn tab_crop(&mut self, ui: &mut Ui) {
        let (fw, fh) = self.frame_dims();
        let p = &mut self.params;
        k::caption(ui, "Drag the corners or edges on the photo. Leave this tab to see the result.");
        ui.add_space(6.0);
        if k::section(ui, "Crop", modified!(p, aspect, crop) || p.crop != pipeline::FULL_CROP, |ui| {
            let w = ui.available_width();
            let cw = (w - 12.0) / 3.0;
            let mut refit = false;
            for row in Aspect::ALL.chunks(3) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for a in row {
                        let sel = p.aspect == *a;
                        let (r, resp) = ui.allocate_exact_size(vec2(cw, 30.0), Sense::click());
                        let pressed = resp.is_pointer_button_down_on();
                        let bg = if sel {
                            Color32::from_rgb(99, 99, 104)
                        } else if pressed {
                            k::FILL_HI
                        } else {
                            k::FILL
                        };
                        ui.painter().rect_filled(r, CornerRadius::same(8), bg);
                        ui.painter().text(
                            r.center(),
                            Align2::CENTER_CENTER,
                            a.label(p.aspect_portrait),
                            if sel { k::font_semibold(12.0) } else { k::font(12.0) },
                            if sel { k::LABEL } else { k::LABEL2 },
                        );
                        if resp.clicked() {
                            p.aspect = *a;
                            refit = true;
                        }
                    }
                });
                ui.add_space(2.0);
            }
            if p.aspect.orientable() {
                ui.add_space(4.0);
                let mut idx = p.aspect_portrait as usize;
                if k::segmented(ui, "crop_orient", &mut idx, &["Landscape", "Portrait"]) {
                    p.aspect_portrait = idx == 1;
                    refit = true;
                }
            }
            if refit && let Some(r) = p.aspect.ratio(fw, fh, p.aspect_portrait) {
                p.crop = pipeline::fit_ratio(p.crop, r, fw, fh);
            }
            ui.add_space(4.0);
        }) {
            p.crop = pipeline::FULL_CROP;
            p.aspect = Aspect::Free;
        }

        if k::section(ui, "Orientation", modified!(p, rotate, flip_h, straighten), |ui| {
            k::slider(ui, "Straighten", &mut p.straighten, -45.0..=45.0, 0.0, Fmt::Deg);
            ui.add_space(4.0);
            let mut line_on = self.horizon.is_some();
            if k::switch_row(ui, "Horizon Line", &mut line_on).changed() {
                self.horizon = line_on.then_some(Horizon { c: [0.5, 0.5], angle: 0.0 });
            }
            if let Some(h) = &mut self.horizon {
                k::caption(ui, "Drag the middle of the yellow line to move it up or down, and its ends to tilt it. Lay it along the horizon, or at any angle you want level, then press Level Photo.");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let tilted = h.angle.abs() > 0.01f32.to_radians();
                    if k::button(ui, "Level Photo", Btn::Primary, tilted).clicked() {
                        // turning the photo by the line's tilt brings the line level
                        p.straighten = (p.straighten - h.angle.to_degrees()).clamp(-45.0, 45.0);
                        h.angle = 0.0;
                    }
                    if k::button(ui, "Reset Line", Btn::Plain, *h != Horizon { c: [0.5, 0.5], angle: 0.0 }).clicked() {
                        *h = Horizon { c: [0.5, 0.5], angle: 0.0 };
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // the crop rotates with the photo
                if k::button(ui, "Rotate Left", Btn::Secondary, true).clicked() {
                    p.rotate = (p.rotate + 3) % 4;
                    let c = p.crop;
                    p.crop = [c[1], 1.0 - c[2], c[3], 1.0 - c[0]];
                    p.aspect_portrait = !p.aspect_portrait;
                }
                if k::button(ui, "Rotate Right", Btn::Secondary, true).clicked() {
                    p.rotate = (p.rotate + 1) % 4;
                    let c = p.crop;
                    p.crop = [1.0 - c[3], c[0], 1.0 - c[1], c[2]];
                    p.aspect_portrait = !p.aspect_portrait;
                }
            });
            ui.add_space(4.0);
            if k::switch_row(ui, "Flip Horizontal", &mut p.flip_h).changed() {
                let c = p.crop;
                p.crop = [1.0 - c[2], c[1], 1.0 - c[0], c[3]];
            }
        }) {
            reset!(p, rotate, flip_h, straighten);
        }

        if k::section(ui, "Border", modified!(p, border, border_white), |ui| {
            k::slider(ui, "Width", &mut p.border, 0.0..=1.0, 0.0, Fmt::Amount);
            let mut idx = if p.border_white { 0 } else { 1 };
            if k::segmented(ui, "border_col", &mut idx, &["White", "Black"]) {
                p.border_white = idx == 0;
            }
            ui.add_space(4.0);
        }) {
            reset!(p, border, border_white);
        }

        let guides_on = self.guide_thirds || self.guide_golden;
        if k::section(ui, "Guides", guides_on, |ui| {
            let a = k::switch_row(ui, "Rule of Thirds", &mut self.guide_thirds).changed();
            let b = k::switch_row(ui, "Golden Ratio", &mut self.guide_golden).changed();
            if a || b {
                self.persist();
            }
            k::caption(ui, "The rule-of-thirds grid always shows while cropping. These switches show guides in every tab. Guides are never exported.");
            ui.add_space(4.0);
        }) {
            self.guide_thirds = false;
            self.guide_golden = false;
            self.persist();
        }
    }
}

/// Paints the outside of a rounded rectangle's corners, giving a clean
/// rounded clip for textured meshes.
fn mask_corners(painter: &egui::Painter, r: Rect, rad: f32, bg: Color32) {
    let steps = 8;
    for (corner, dir) in [
        (r.left_top(), vec2(1.0, 1.0)),
        (r.right_top(), vec2(-1.0, 1.0)),
        (r.left_bottom(), vec2(1.0, -1.0)),
        (r.right_bottom(), vec2(-1.0, -1.0)),
    ] {
        let center = corner + dir * rad;
        let mut pts = vec![corner];
        for i in 0..=steps {
            let a = i as f32 / steps as f32 * std::f32::consts::FRAC_PI_2;
            let p = center + vec2(-dir.x * a.cos(), -dir.y * a.sin()) * rad;
            pts.push(p);
        }
        // fan triangles from the corner point
        let mut mesh = egui::Mesh::default();
        for p in &pts {
            mesh.colored_vertex(*p, bg);
        }
        for i in 1..pts.len() as u32 - 1 {
            mesh.add_triangle(0, i, i + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}

fn hue_strip(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let r = Rect::from_min_max(pos2(rect.left() + 2.0, rect.top()), pos2(rect.right() - 2.0, rect.bottom()));
    let mut mesh = egui::Mesh::default();
    let n = 36;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let x = r.left() + t * r.width();
        let c = egui::ecolor::Hsva::new(t, 0.65, 0.9, 1.0);
        let col: Color32 = c.into();
        mesh.colored_vertex(pos2(x, r.top()), col);
        mesh.colored_vertex(pos2(x, r.bottom()), col);
        if i > 0 {
            let b = (i as u32) * 2;
            mesh.add_triangle(b - 2, b - 1, b);
            mesh.add_triangle(b - 1, b, b + 1);
        }
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    ui.add_space(2.0);
}

// ---------------------------------------------------------------------------
// Export sheet
// ---------------------------------------------------------------------------

impl App {
    fn export_sheet(&mut self, ctx: &egui::Context) {
        let t = k::spring(ctx, Id::unique("sheet"), if self.export_open { 1.0 } else { 0.0 }, 1.0, 0.32);
        if t < 0.005 {
            return;
        }
        let Some(doc) = &self.doc else { return };
        let screen = ctx.content_rect();

        // scrim: dims the work behind and catches clicks
        egui::Area::new(Id::unique("scrim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(screen, Sense::click());
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha((130.0 * t) as u8));
                if resp.clicked() && !self.exporting {
                    self.export_open = false;
                }
            });

        let width = 440.0;
        // the sheet descends from the toolbar and returns the same way
        let top = screen.top() + 64.0 - (1.0 - t) * 28.0;
        let (full_w, full_h) = (doc.full.width(), doc.full.height());
        let scale = self.export.scale_for(full_w, full_h);
        let (ow, oh) = pipeline::output_size(
            (full_w as f32 * scale).round() as usize,
            (full_h as f32 * scale).round() as usize,
            &self.params,
        );
        let has_icc = doc.icc.is_some();
        let has_exif = doc.exif.is_some();
        let long_max = (full_w.max(full_h) as f32 * 2.0).max(1000.0);
        let mut do_export = false;
        let mut cancel = false;

        egui::Area::new(Id::unique("export_sheet"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos2(screen.center().x - width / 2.0, top))
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                egui::Frame::NONE
                    .fill(Color32::from_rgb(34, 34, 37))
                    .corner_radius(CornerRadius::same(16))
                    .stroke(Stroke::new(1.0, Color32::from_white_alpha(18)))
                    .shadow(egui::Shadow { offset: [0, 18], blur: 50, spread: 0, color: Color32::from_black_alpha(140) })
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.set_width(width - 40.0);
                        ui.label(egui::RichText::new("Export").font(k::font_semibold(17.0)));
                        ui.label(egui::RichText::new(&doc.name).size(12.0).color(k::LABEL3));
                        ui.add_space(14.0);

                        let s = &mut self.export;
                        field_label(ui, "Format");
                        let mut fi = Format::ALL.iter().position(|f| *f == s.format).unwrap_or(0);
                        if k::segmented(ui, "fmt", &mut fi, &["JPEG", "PNG", "TIFF", "WebP"]) {
                            s.format = Format::ALL[fi];
                        }
                        ui.add_space(8.0);
                        match s.format {
                            Format::Jpeg => {
                                k::slider(ui, "Quality", &mut s.quality, 0.5..=1.0, 0.92, Fmt::Amount);
                            }
                            Format::Png | Format::Tiff => {
                                field_label(ui, "Bit Depth");
                                let mut d = s.depth16 as usize;
                                if k::segmented(ui, "depth", &mut d, &["8-bit", "16-bit"]) {
                                    s.depth16 = d == 1;
                                }
                                ui.add_space(8.0);
                            }
                            Format::Webp => {
                                k::caption(ui, "Lossless, 8-bit.");
                                ui.add_space(8.0);
                            }
                        }

                        field_label(ui, "Size");
                        k::segmented(ui, "size", &mut s.size_mode, &["Original", "Long Edge", "Percent"]);
                        ui.add_space(4.0);
                        match s.size_mode {
                            1 => {
                                k::slider(ui, "Long Edge (px)", &mut s.long_edge, 256.0..=long_max, 3000.0, Fmt::Px);
                                s.long_edge = s.long_edge.round();
                            }
                            2 => {
                                k::slider(ui, "Scale (%)", &mut s.percent, 0.1..=2.0, 1.0, Fmt::Amount);
                            }
                            _ => {}
                        }
                        ui.add_space(8.0);

                        field_label(ui, "Output Sharpening");
                        k::segmented(ui, "sharp", &mut s.sharpen, &["Off", "Low", "Standard", "High"]);
                        ui.add_space(10.0);

                        let mut meta = s.keep_metadata && s.format.supports_metadata() && has_exif;
                        ui.add_enabled_ui(s.format.supports_metadata() && has_exif, |ui| {
                            if k::switch_row(ui, "Keep EXIF Metadata", &mut meta).changed() {
                                s.keep_metadata = meta;
                            }
                        });
                        let mut icc = s.keep_icc && has_icc && s.format != Format::Webp;
                        ui.add_enabled_ui(has_icc && s.format != Format::Webp, |ui| {
                            if k::switch_row(ui, "Embed Color Profile", &mut icc).changed() {
                                s.keep_icc = icc;
                            }
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("File Suffix").size(13.0).color(k::LABEL2));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut s.suffix)
                                        .desired_width(150.0)
                                        .margin(egui::Margin::symmetric(8, 4)),
                                );
                            });
                        });

                        ui.add_space(14.0);
                        let (sep, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                        ui.painter().rect_filled(sep, 0.0, k::SEPARATOR);
                        ui.add_space(10.0);

                        let depth = if s.format.supports_16() && s.depth16 { "16-bit" } else { "8-bit" };
                        let detail = match s.format {
                            Format::Jpeg => format!("JPEG {}", (s.quality * 100.0).round()),
                            f => format!("{} {depth}", f.label()),
                        };
                        let profile = if icc { "embedded profile" } else { "sRGB" };
                        ui.label(
                            egui::RichText::new(format!("{ow} × {oh} px  ·  {detail}  ·  {profile}"))
                                .font(k::font_mono(11.5))
                                .color(k::LABEL2),
                        );
                        ui.add_space(14.0);

                        ui.horizontal(|ui| {
                            if self.exporting {
                                ui.spinner();
                                ui.label(egui::RichText::new("Exporting…").color(k::LABEL2));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if k::button(ui, "Export…", Btn::Primary, !self.exporting).clicked() {
                                    do_export = true;
                                }
                                if k::button(ui, "Cancel", Btn::Secondary, !self.exporting).clicked() {
                                    cancel = true;
                                }
                            });
                        });
                    });
            });

        if cancel {
            self.export_open = false;
            self.persist();
        }
        if do_export {
            self.pick_export_path(ctx);
        }
    }
}

impl App {
    fn settings_sheet(&mut self, ctx: &egui::Context) {
        let t = k::spring(ctx, Id::unique("settings_t"), if self.settings_open { 1.0 } else { 0.0 }, 1.0, 0.32);
        if t < 0.005 {
            return;
        }
        let screen = ctx.content_rect();
        let mut close = false;
        egui::Area::new(Id::unique("settings_scrim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(screen, Sense::click());
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha((130.0 * t) as u8));
                if resp.clicked() {
                    close = true;
                }
            });

        let width = 460.0;
        let top = screen.top() + 64.0 - (1.0 - t) * 28.0;
        let before = self.prefs.clone();
        let (mut import, mut export_all, mut reveal) = (false, false, false);
        let max_h = (screen.height() - 90.0).max(300.0);

        egui::Area::new(Id::unique("settings_sheet"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos2(screen.center().x - width / 2.0, top))
            .show(ctx, |ui| {
                ui.multiply_opacity(t);
                egui::Frame::NONE
                    .fill(Color32::from_rgb(34, 34, 37))
                    .corner_radius(CornerRadius::same(16))
                    .stroke(Stroke::new(1.0, Color32::from_white_alpha(18)))
                    .shadow(egui::Shadow { offset: [0, 18], blur: 50, spread: 0, color: Color32::from_black_alpha(140) })
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.set_width(width - 40.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Settings").font(k::font_semibold(17.0)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if k::button(ui, "Done", Btn::Primary, true).clicked() {
                                    close = true;
                                }
                            });
                        });
                        ui.add_space(10.0);
                        egui::ScrollArea::vertical().max_height(max_h - 70.0).auto_shrink([false, true]).show(ui, |ui| {
                            let p = &mut self.prefs;
                            group(ui, "Preview");
                            field_label(ui, "Quality");
                            k::segmented(ui, "pref_q", &mut p.preview_quality, &["Fast", "Balanced", "Detailed"]);
                            ui.add_space(2.0);
                            k::caption(ui, &format!(
                                "Edits are previewed at {} px on the long edge. Detailed is sharper when zoomed but slower; export always renders full resolution.",
                                p.preview_edge()
                            ));
                            ui.add_space(12.0);

                            group(ui, "Appearance");
                            field_label(ui, "Canvas");
                            k::segmented(ui, "pref_canvas", &mut p.canvas, &["Black", "Graphite", "Gray"]);
                            ui.add_space(8.0);
                            field_label(ui, "Mask Overlay");
                            k::segmented(ui, "pref_overlay", &mut p.overlay_color, &["Red", "Green", "Blue"]);
                            ui.add_space(8.0);
                            field_label(ui, "Histogram");
                            k::segmented(ui, "pref_hist", &mut p.histogram, &["RGB", "Luminance"]);
                            ui.add_space(6.0);
                            k::switch_row(ui, "Show Render Time", &mut p.show_render_time);
                            k::switch_row(ui, "Reduce Motion", &mut p.reduce_motion);
                            ui.add_space(12.0);

                            group(ui, "Guides");
                            k::switch_row(ui, "Rule of Thirds", &mut self.guide_thirds);
                            k::switch_row(ui, "Golden Ratio", &mut self.guide_golden);
                            ui.add_space(12.0);

                            group(ui, "Workflow");
                            k::switch_row(ui, "Keep Preset When Opening a Photo", &mut self.prefs.keep_look);
                            k::caption(ui, "On: the next photo opens with your current preset and adjustments, handy for a series. Off: every photo starts clean.");
                            ui.add_space(12.0);

                            if cfg!(target_os = "linux") {
                                group(ui, "System");
                                k::switch_row(ui, "Native Wayland", &mut self.prefs.native_wayland);
                                k::caption(ui, "Off (default): Still runs through XWayland, so you can drag photos onto the window. On: native Wayland, but drag and drop won't work. Applies after a restart.");
                                ui.add_space(12.0);
                            }

                            group(ui, "Presets");
                            ui.horizontal(|ui| {
                                if k::button(ui, "Import Presets…", Btn::Secondary, true).clicked() {
                                    import = true;
                                }
                                if k::button(ui, "Export My Presets…", Btn::Secondary, !self.user_presets.is_empty()).clicked() {
                                    export_all = true;
                                }
                            });
                            ui.add_space(12.0);

                            group(ui, "About");
                            k::caption(ui, &format!("Still {} · settings in {}", env!("CARGO_PKG_VERSION"), io::config_dir().display()));
                            if k::button(ui, "Show Settings Folder", Btn::Plain, true).clicked() {
                                reveal = true;
                            }
                        });
                    });
            });

        if self.prefs != before {
            self.apply_prefs();
            if self.prefs.preview_quality != before.preview_quality {
                self.rebuild_preview(ctx);
            }
            self.persist();
        }
        if import {
            self.settings_open = false;
            self.pick_looks_to_import(ctx);
        }
        if export_all {
            self.export_looks(self.user_presets.clone(), ctx);
        }
        if reveal {
            let dir = io::config_dir();
            let _ = std::fs::create_dir_all(&dir);
            io::reveal(&dir);
        }
        if close {
            self.settings_open = false;
            self.persist();
        }
    }
}

/// Image files named by pasted text: plain paths or `file://` URIs, one per line.
fn pasted_paths(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let p = match l.strip_prefix("file://") {
                // file:///home/… on Unix, file:///C:/… on Windows
                Some(rest) => {
                    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
                    let decoded = percent_decode(rest);
                    if cfg!(windows) { decoded.trim_start_matches('/').to_string() } else { decoded }
                }
                None => l.to_string(),
            };
            let p = PathBuf::from(p);
            (p.is_file() && io::is_openable(&p)).then_some(p)
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Label colours for masks and layers; index 0 means no colour.
const LABELS: [(&str, u32); 7] = [
    ("Red", 0xFF453A),
    ("Orange", 0xFF9F0A),
    ("Yellow", 0xFFD60A),
    ("Green", 0x30D158),
    ("Blue", 0x0A84FF),
    ("Purple", 0xBF5AF2),
    ("Gray", 0x98989D),
];

fn label_rgb(c: u8) -> Option<u32> {
    (c as usize).checked_sub(1).and_then(|i| LABELS.get(i)).map(|l| l.1)
}

fn label_color(c: u8) -> Option<Color32> {
    label_rgb(c).map(|v| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Single-line text cut to `max_w` with an ellipsis.
fn elided(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(12.0));
    ui.painter().layout_job(job)
}

/// "Change Color" submenu for a list row's context menu; returns the picked label.
fn color_menu(ui: &mut Ui, current: u8) -> Option<u8> {
    let mut pick = None;
    ui.menu_button("Change Color", |ui| {
        for c in 0..=LABELS.len() as u8 {
            let name = if c == 0 { "None" } else { LABELS[c as usize - 1].0 };
            let (r, resp) = ui.allocate_exact_size(vec2(130.0, 24.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(5), k::FILL_HI);
            }
            let dot = pos2(r.left() + 12.0, r.center().y);
            match label_color(c) {
                Some(col) => ui.painter().circle_filled(dot, 5.5, col),
                None => ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.2, k::LABEL3)),
            };
            ui.painter().text(pos2(r.left() + 26.0, r.center().y), Align2::LEFT_CENTER, name, k::font(13.0), k::LABEL);
            if current == c {
                ui.painter().text(pos2(r.right() - 8.0, r.center().y), Align2::RIGHT_CENTER, "✓", k::font(12.0), k::LABEL2);
            }
            if resp.clicked() {
                pick = Some(c);
                ui.close();
            }
        }
    });
    pick
}

/// Which list row is being renamed.
#[derive(Clone, Copy, PartialEq)]
enum Rename {
    Mask(usize),
    Layer(usize),
    Group(u32),
}

/// What happened to an inline rename this frame.
enum RenameOutcome {
    Editing,
    Commit(String),
    Cancel,
}

/// The text field over a row's name while renaming it.
fn rename_field(ui: &mut Ui, r: Rect, buf: &mut String, focus: &mut bool) -> RenameOutcome {
    let tr = Rect::from_min_max(pos2(r.left() + 26.0, r.top() + 4.0), pos2(r.right() - 6.0, r.bottom() - 4.0));
    let te = ui.put(tr, egui::TextEdit::singleline(buf).font(k::font(13.0)).margin(egui::Margin::symmetric(5, 3)).char_limit(80));
    if *focus {
        te.request_focus();
        *focus = false;
    }
    if te.lost_focus() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            RenameOutcome::Cancel
        } else {
            RenameOutcome::Commit(buf.trim().to_string())
        }
    } else {
        RenameOutcome::Editing
    }
}

/// A small uppercase group heading inside a sheet.
fn group(ui: &mut Ui, title: &str) {
    ui.label(egui::RichText::new(title.to_uppercase()).font(k::font_semibold(11.0)).color(k::LABEL3));
    ui.add_space(4.0);
}

fn read_look_files(paths: &[PathBuf]) -> (Vec<(String, Params)>, Vec<String>) {
    let mut looks = Vec::new();
    let mut errs = Vec::new();
    for p in paths {
        match io::read_looks(p) {
            Ok(mut l) => looks.append(&mut l),
            Err(e) => errs.push(format!("{}: {e}", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
        }
    }
    (looks, errs)
}

fn field_label(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(k::LABEL2));
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(c: [f32; 4], w: f32, h: f32) -> f32 {
        (c[2] - c[0]) * w / ((c[3] - c[1]) * h)
    }

    #[test]
    fn corner_drag_keeps_ratio_and_stays_inside() {
        let (w, h) = (1800.0, 1350.0);
        let c = pipeline::fit_ratio(pipeline::FULL_CROP, 1.0, w, h);
        assert!((ratio(c, w, h) - 1.0).abs() < 1e-3);
        for d in [vec2(-0.3, 0.1), vec2(0.5, 0.5), vec2(-2.0, -2.0), vec2(0.05, -0.4)] {
            for g in [Grip::Edge(1, 1), Grip::Edge(-1, -1), Grip::Edge(1, 0), Grip::Edge(0, -1), Grip::Move] {
                let r = drag_crop(c, g, d, Some(1.0), w, h);
                assert!((ratio(r, w, h) - 1.0).abs() < 1e-2, "{g:?} {d:?} {r:?}");
                assert!(r.iter().all(|v| (0.0..=1.0).contains(v)));
                assert!(r[2] > r[0] && r[3] > r[1]);
            }
        }
    }

    #[test]
    fn free_drag_and_rotation() {
        let c = drag_crop(pipeline::FULL_CROP, Grip::Edge(-1, 0), vec2(0.25, 0.0), None, 100.0, 100.0);
        assert_eq!(c, [0.25, 0.0, 1.0, 1.0]);
        // rotating right then left returns the same rect
        let r = [1.0 - c[3], c[0], 1.0 - c[1], c[2]];
        let back = [r[1], 1.0 - r[2], r[3], 1.0 - r[0]];
        assert_eq!(back, c);
    }

    /// Drive egui with a real press + release and check the click position
    /// is available on the frame the click is reported.
    #[test]
    fn click_position_survives_release_frame() {
        let ctx = egui::Context::default();
        let at = pos2(120.0, 80.0);
        let mut seen = None;
        let mut origin_on_click = Some(Pos2::ZERO);
        let frames = [
            vec![egui::Event::PointerMoved(at)],
            vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }],
            vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }],
            vec![],
        ];
        for events in frames {
            let input = egui::RawInput { events, screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let resp = ui.interact(ui.max_rect(), Id::unique("click_test"), Sense::click_and_drag());
                if resp.clicked() {
                    seen = click_pos(&resp);
                    origin_on_click = ui.input(|i| i.pointer.press_origin());
                }
            });
            // nothing renders here; drop the font texture upload egui asks for
            out.textures_delta.clear();
        }
        assert_eq!(seen, Some(at), "click position must be known when the click fires");
        assert_eq!(origin_on_click, None, "press_origin is gone on the click frame (the original bug)");
    }

    // ------------------------------------------------------------------
    // End-to-end: the real app, driven with real pointer events.
    // ------------------------------------------------------------------

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("still-e2e-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_photo(dir: &std::path::Path) -> PathBuf {
        let img = image::RgbImage::from_fn(900, 600, |x, y| image::Rgb([(x / 4) as u8, (y / 3) as u8, 120]));
        let p = dir.join("photo.png");
        img.save(&p).unwrap();
        p
    }

    fn test_layer_png(dir: &std::path::Path) -> PathBuf {
        let img = image::RgbaImage::from_pixel(200, 100, image::Rgba([250, 160, 30, 255]));
        let p = dir.join("layer.png");
        img.save(&p).unwrap();
        p
    }

    /// Step the app until `done` holds (background threads need real time).
    fn settle(h: &mut egui_kittest::Harness<'_, App>, done: impl Fn(&App) -> bool) {
        for _ in 0..600 {
            h.step();
            if done(h.state()) {
                h.step();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let a = h.state();
        panic!("app never settled: doc {} frame {:?} gen {}/{} rect {:?} tab {}", a.doc.is_some(), a.tex_frame, a.shown_gen, a.generation, a.last_img_rect, a.tab);
    }

    fn rendered(a: &App) -> bool {
        a.generation > 0 && a.shown_gen == a.generation && a.last_img_rect.is_positive()
    }

    fn drag(h: &mut egui_kittest::Harness<'_, App>, from: Pos2, to: Pos2) {
        h.hover_at(from);
        h.step();
        h.drag_at(from);
        h.step();
        for k in 1..=12 {
            h.hover_at(from + (to - from) * (k as f32 / 12.0));
            h.step();
        }
        h.drop_at(to);
        h.step();
    }

    fn open_with_layer(tag: &str) -> egui_kittest::Harness<'static, App> {
        let dir = temp_dir(tag);
        let photo = test_photo(&dir);
        let layer = test_layer_png(&dir);
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(move |cc| App::new(cc, Some(photo)));
        settle(&mut h, |a| a.doc.is_some());
        h.state_mut().load_layers(vec![layer], &egui::Context::default());
        settle(&mut h, |a| !a.params.layers.is_empty());
        settle(&mut h, |a| a.tex_frame == Frame::Unbordered && rendered(a));
        h
    }

    #[test]
    fn e2e_layer_moves_after_adding() {
        let mut h = open_with_layer("move");
        let (x0, y0) = (h.state().params.layers[0].x, h.state().params.layers[0].y);
        let map = h.state().mask_map(h.state().last_img_rect);
        let c = map.n2s([x0, y0]);
        drag(&mut h, c, c + vec2(120.0, 60.0));
        let l = &h.state().params.layers[0];
        assert!(l.x > x0 + 0.05 && l.y > y0 + 0.05, "layer did not move: {x0},{y0} -> {},{}", l.x, l.y);

        // deselect, then drag it again: picking it up by clicking on it must work too
        h.state_mut().layer_sel = None;
        h.step();
        let (x1, y1) = (h.state().params.layers[0].x, h.state().params.layers[0].y);
        let c = map.n2s([x1, y1]);
        drag(&mut h, c, c - vec2(80.0, 0.0));
        let l = &h.state().params.layers[0];
        assert!(l.x < x1 - 0.03, "second move failed: {x1} -> {}", l.x);
        assert!((l.y - y1).abs() < 0.01);
    }

    #[test]
    fn e2e_stretch_and_snap_to_canvas_sides() {
        let mut h = open_with_layer("stretch");
        let img = h.state().last_img_rect;
        let map = h.state().mask_map(img);
        let before = h.state().params.layers[0].clone();
        let (w0, h0) = before.size(map.long());
        // grab the right-hand stretch handle and pull it 100 px outwards
        let right = layer_edges(&before, &map)[0].1;
        let left_before = right.x - w0;
        drag(&mut h, right, right + vec2(100.0, 0.0));
        let l = h.state().params.layers[0].clone();
        let (w1, h1) = l.size(map.long());
        assert!((w1 - (w0 + 100.0)).abs() < 2.0, "width {w0} -> {w1}");
        assert!((h1 - h0).abs() < 1.0, "height should not change: {h0} -> {h1}");
        let left_after = map.n2s([l.x, l.y]).x - w1 / 2.0;
        assert!((left_after - left_before).abs() < 1.0, "opposite side must stay put");

        // drag the right side to within a few pixels of the canvas edge: it snaps exactly
        let right = layer_edges(&l, &map)[0].1;
        let target = pos2(img.right() - 5.0, right.y);
        drag(&mut h, right, target);
        let l = h.state().params.layers[0].clone();
        let (w2, _) = l.size(map.long());
        let right_edge = map.n2s([l.x, l.y]).x + w2 / 2.0;
        assert!((right_edge - img.right()).abs() < 0.5, "should snap to the canvas side: {right_edge} vs {}", img.right());

        // Stretch fills every side of the canvas
        h.state_mut().stretch_layer(0);
        let l = h.state().params.layers[0].clone();
        let (w3, h3) = l.size(map.long());
        let c = map.n2s([l.x, l.y]);
        assert!((c.x - w3 / 2.0 - img.left()).abs() < 1.0 && (c.x + w3 / 2.0 - img.right()).abs() < 1.0);
        assert!((c.y - h3 / 2.0 - img.top()).abs() < 1.0 && (c.y + h3 / 2.0 - img.bottom()).abs() < 1.0);
    }

    #[test]
    fn e2e_magnet_pulls_layers_together() {
        let mut h = open_with_layer("magnet");
        {
            let a = h.state_mut();
            a.prefs.magnet = true;
            a.prefs.magnet_pivot = false;
            let mut b = a.params.layers[0].clone();
            a.params.layers[0].x = 0.3;
            a.params.layers[0].y = 0.5;
            a.params.layers[0].scale = 0.2;
            b.x = 0.75;
            b.y = 0.52;
            b.scale = 0.12;
            a.params.layers.push(b);
            a.layer_sel = Some(1);
        }
        h.step();
        let img = h.state().last_img_rect;
        let map = h.state().mask_map(img);
        let right_of = |l: &Layer| map.n2s([l.x, l.y]).x + l.size(map.long()).0 / 2.0;
        let left_of = |l: &Layer| map.n2s([l.x, l.y]).x - l.size(map.long()).0 / 2.0;
        let a_right = right_of(&h.state().params.layers[0]);

        // drag B so its left side ends 6 px short of A's right side: pulled flush
        let b = h.state().params.layers[1].clone();
        let from = map.n2s([b.x, b.y]);
        let to = from + vec2(a_right + 6.0 - left_of(&b), 0.0);
        drag(&mut h, from, to);
        let b = h.state().params.layers[1].clone();
        assert!((left_of(&b) - a_right).abs() < 0.5, "flush: {} vs {a_right}", left_of(&b));

        // with Ctrl held, the magnet lets go
        h.state_mut().params.layers[1].x += 0.1;
        let b = h.state().params.layers[1].clone();
        let from = map.n2s([b.x, b.y]);
        let to = from + vec2(a_right + 6.0 - left_of(&b), 0.0);
        h.event(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        drag(&mut h, from, to);
        h.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        h.step();
        let b = h.state().params.layers[1].clone();
        assert!((left_of(&b) - (a_right + 6.0)).abs() < 1.0, "Ctrl places freely: {}", left_of(&b));

        // auto pivot: a slightly turned layer turns square to its neighbour when caught
        h.state_mut().prefs.magnet_pivot = true;
        h.state_mut().params.layers[1].rotation = 7.0;
        h.state_mut().params.layers[1].x += 0.1;
        h.step();
        let b = h.state().params.layers[1].clone();
        let from = map.n2s([b.x, b.y]);
        let to = from + vec2(a_right + 5.0 - left_of(&b), 0.0);
        drag(&mut h, from, to);
        let b = h.state().params.layers[1].clone();
        assert!(b.rotation.abs() < 1e-3, "turned to match: {}", b.rotation);
        assert!((left_of(&b) - a_right).abs() < 0.5, "and flush: {} vs {a_right}", left_of(&b));

        // auto pivot: scaling keeps the side touching the neighbour in place
        let (_, corners, _) = layer_frame(&b, &map);
        let w0 = b.size(map.long()).0;
        drag(&mut h, corners[2], corners[2] + vec2(60.0, 40.0));
        let b = h.state().params.layers[1].clone();
        assert!(b.size(map.long()).0 > w0 + 20.0, "it grew");
        assert!((left_of(&b) - a_right).abs() < 0.5, "the touching side stayed: {} vs {a_right}", left_of(&b));
    }

    #[test]
    fn e2e_project_save_and_reopen() {
        let mut h = open_with_layer("project");
        h.state_mut().add_mask(1);
        h.state_mut().params.masks[0].adj.exposure = 0.7;
        h.state_mut().params.layers[0].stretch = 1.7;
        h.state_mut().params.crop = [0.1, 0.1, 0.9, 0.85];
        h.step();
        assert!(h.state().unsaved());
        let proj = temp_dir("project").join("edit.still");
        h.state_mut().save_project(proj.clone());
        h.step();
        assert!(!h.state().unsaved(), "saving clears the edited marker");
        let edit = h.state().params.clone();

        // a fresh app opening the project gets the photo and the whole edit back
        let p2 = proj.clone();
        let mut h2 = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(move |cc| App::new(cc, Some(p2)));
        settle(&mut h2, |a| a.doc.is_some());
        assert_eq!(h2.state().params, edit);
        assert_eq!(h2.state().project.as_deref(), Some(proj.as_path()));
        assert!(!h2.state().unsaved());
    }

    #[derive(Debug)]
    struct TestDrop(PathBuf);
    impl egui::DroppedFile for TestDrop {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn e2e_dropped_photo_becomes_a_layer() {
        let dir = temp_dir("drop");
        let photo = test_photo(&dir);
        let layer = test_layer_png(&dir);
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(move |cc| App::new(cc, None));
        h.step();
        // dropped on the empty window: opens the photo
        h.input_mut().dropped_files.push(Arc::new(TestDrop(photo)));
        settle(&mut h, |a| a.doc.is_some());
        assert!(h.state().params.layers.is_empty());
        // dropped on an open photo, from any tab: placed on top as a layer
        h.state_mut().tab = 1;
        h.input_mut().dropped_files.push(Arc::new(TestDrop(layer)));
        settle(&mut h, |a| !a.params.layers.is_empty());
        assert!(TABS[h.state().tab].0 == Tab::Layers);
        assert_eq!(h.state().doc.as_ref().unwrap().name, "photo.png", "the photo stays open");
    }

    #[test]
    fn e2e_rename_and_colour_skip_the_render() {
        let mut h = open_with_layer("rename");
        h.state_mut().tab = TABS.iter().position(|t| t.0 == Tab::Masks).unwrap();
        h.state_mut().add_mask(1);
        h.state_mut().overlay_hold = false;
        settle(&mut h, |a| a.tex_frame == Frame::Unbordered && rendered(a));
        settle(&mut h, rendered);
        let generation = h.state().generation;

        h.state_mut().renaming = Some((Rename::Mask(0), String::new(), true));
        h.step();
        h.event(egui::Event::Text("Sky".into()));
        h.step();
        h.key_press(egui::Key::Enter);
        h.step();
        h.step();
        assert_eq!(h.state().params.masks[0].name, "Sky");
        assert!(h.state().renaming.is_none());

        // renaming with an empty name keeps the old one
        h.state_mut().renaming = Some((Rename::Mask(0), String::new(), true));
        h.step();
        h.key_press(egui::Key::Enter);
        h.step();
        h.step();
        assert_eq!(h.state().params.masks[0].name, "Sky");

        h.state_mut().params.masks[0].color = 3;
        h.state_mut().params.layers[0].name = "Sun flare".into();
        h.state_mut().params.layers[0].color = 5;
        h.step();
        h.step();
        assert_eq!(h.state().generation, generation, "names and colours must not re-render the photo");
    }

    #[test]
    fn pasted_text_finds_image_files() {
        let dir = temp_dir("paste-text");
        let photo = test_photo(&dir);
        let spaced = dir.join("my photo.png");
        std::fs::copy(&photo, &spaced).unwrap();
        let text = format!("file://{}\n{}\nnot a file\nfile://{}", photo.display(), dir.join("missing.png").display(), spaced.display().to_string().replace(' ', "%20"));
        let found = pasted_paths(&text);
        if !cfg!(windows) {
            assert_eq!(found, vec![photo, spaced]);
        }
    }

    #[test]
    fn e2e_paste_image_lock_and_group() {
        let mut h = open_with_layer("paste");
        h.state_mut().tab = 1;
        let img = egui::ColorImage::new([40, 30], vec![Color32::from_rgb(10, 200, 30); 1200]);
        h.event(egui::Event::PasteImage(Arc::new(img)));
        settle(&mut h, |a| a.params.layers.len() == 2);
        assert!(TABS[h.state().tab].0 == Tab::Layers);
        assert!(std::path::Path::new(&h.state().params.layers[1].path).is_file(), "pasted image is kept on disk");
        let _ = std::fs::remove_file(&h.state().params.layers[1].path);

        // a locked layer can't be picked on the canvas; clicks fall through to the one below
        settle(&mut h, rendered);
        let map = h.state().mask_map(h.state().last_img_rect);
        for l in &mut h.state_mut().params.layers {
            l.x = 0.5;
            l.y = 0.5;
        }
        h.state_mut().params.layers[1].locked = true;
        h.state_mut().layer_click(map.n2s([0.5, 0.5]));
        assert_eq!(h.state().layer_sel, Some(0));

        // groups: a hidden group's layers can't be picked either
        h.state_mut().params.layer_groups.push(crate::layers::LayerGroup { id: 7, visible: false, ..Default::default() });
        h.state_mut().params.layers[0].group = 7;
        h.state_mut().layer_click(map.n2s([0.5, 0.5]));
        assert_eq!(h.state().layer_sel, None);
        h.step();
        h.step();
    }

    #[test]
    fn e2e_recent_files_and_sharper_zoom() {
        let dir = temp_dir("detail");
        let big = image::RgbImage::from_fn(3000, 2000, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let path = dir.join("big.png");
        big.save(&path).unwrap();
        let p2 = path.clone();
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(move |cc| App::new(cc, Some(p2)));
        settle(&mut h, |a| a.doc.is_some());
        assert_eq!(h.state().recent.first(), Some(&std::fs::canonicalize(&path).unwrap()));
        settle(&mut h, rendered);
        let preview_w = h.state().doc.as_ref().unwrap().preview.w;
        let shown = |a: &App| a.tex.as_ref().map(|t| t.size()[0]).unwrap_or(0);
        assert_eq!(shown(h.state()), preview_w);
        // zoom in far: once settled, the texture comes from a sharper source but lays out the same
        h.state_mut().view.zoom = 6.0;
        let r0 = h.state().last_img_rect;
        settle(&mut h, |a| shown(a) > preview_w && rendered(a));
        assert!(h.state().tex_scale < 1.0);
        let _ = r0;
        // and back out: the normal preview returns
        h.state_mut().view.zoom = 1.0;
        settle(&mut h, |a| shown(a) == preview_w && rendered(a));
        assert!(h.state().detail_src.is_none());
    }

    fn click(h: &mut egui_kittest::Harness<'_, App>, at: Pos2, modifiers: egui::Modifiers) {
        h.hover_at(at);
        h.step();
        h.event_modifiers(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers }, modifiers);
        h.step();
        h.event_modifiers(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers }, modifiers);
        h.step();
        h.step();
    }

    fn open_on(tag: &str, tab: Tab) -> egui_kittest::Harness<'static, App> {
        let dir = temp_dir(tag);
        let photo = test_photo(&dir);
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(move |cc| App::new(cc, Some(photo)));
        settle(&mut h, |a| a.doc.is_some());
        h.state_mut().tab = TABS.iter().position(|t| t.0 == tab).unwrap();
        let want = if tab == Tab::Crop { Frame::Uncropped } else { Frame::Unbordered };
        settle(&mut h, move |a| a.tex_frame == want && rendered(a));
        h
    }

    #[test]
    fn e2e_colour_picker_keeps_its_crosshair_when_zoomed() {
        let mut h = open_on("picker", Tab::Masks);
        h.state_mut().add_mask(4);
        h.state_mut().picking_hue = true;
        h.state_mut().view.zoom = 2.5;
        for _ in 0..30 {
            h.step();
        }
        let c = h.state().last_img_rect.center();
        h.hover_at(c);
        h.step();
        h.step();
        assert_eq!(h.output().platform_output.cursor_icon, egui::CursorIcon::Crosshair);
    }

    #[test]
    fn e2e_object_points_add_remove_and_delete() {
        let mut h = open_on("object", Tab::Masks);
        h.state_mut().add_mask(5);
        h.step();
        let r = h.state().last_img_rect;
        let (a, b) = (r.lerp_inside(vec2(0.3, 0.5)), r.lerp_inside(vec2(0.7, 0.5)));
        let seeds = |h: &egui_kittest::Harness<'_, App>| match &h.state().params.masks[0].shape {
            Shape::Object { seeds, .. } => seeds.clone(),
            _ => unreachable!(),
        };
        click(&mut h, a, egui::Modifiers::NONE);
        assert_eq!(seeds(&h).len(), 1);
        assert!(seeds(&h)[0].add);
        // − mode places a point that marks where the selection should not go
        h.state_mut().seed_remove = true;
        click(&mut h, b, egui::Modifiers::NONE);
        assert_eq!(seeds(&h).len(), 2);
        assert!(!seeds(&h)[1].add);
        // clicking a point deletes it
        click(&mut h, a, egui::Modifiers::NONE);
        let left = seeds(&h);
        assert_eq!(left.len(), 1);
        assert!(!left[0].add, "the + point was removed, the − point stays");
    }

    #[test]
    fn e2e_overlay_stays_until_adjusting() {
        let mut h = open_on("overlay", Tab::Masks);
        h.state_mut().add_mask(1);
        std::thread::sleep(std::time::Duration::from_millis(1800));
        h.step();
        let ctx = egui::Context::default();
        assert!(h.state().overlay_wanted(&ctx).is_some(), "still showing long after adding");
        h.state_mut().overlay_hold = false; // what touching an adjustment does
        assert!(h.state().overlay_wanted(&ctx).is_none());
    }

    #[test]
    fn e2e_clone_tool_copies_from_the_source() {
        let mut h = open_on("clone", Tab::Retouch);
        let r = h.state().last_img_rect;
        let (src, dst) = (r.lerp_inside(vec2(0.8, 0.3)), r.lerp_inside(vec2(0.2, 0.6)));
        // Ctrl+click picks the source
        click(&mut h, src, egui::Modifiers::COMMAND);
        assert!(h.state().clone_source.is_some());
        assert!(h.state().params.retouch.is_empty());
        drag(&mut h, dst, dst + vec2(60.0, 0.0));
        let st = &h.state().params.retouch;
        assert_eq!(st.len(), 1);
        assert!(st[0].pts.len() > 3);
        let map = h.state().mask_map(r);
        let (ns, nd) = (map.s2n(src), map.s2n(dst));
        assert!((st[0].offset[0] - (ns[0] - nd[0])).abs() < 0.01 && (st[0].offset[1] - (ns[1] - nd[1])).abs() < 0.01);
        // the rendered preview changed where it was painted: the test photo's red
        // grows to the right, so copying from the right makes the spot redder
        settle(&mut h, rendered);
        let im = h.state().last_image.clone().unwrap();
        let at = |p: Pos2| {
            let u = ((p.x - r.left()) / r.width() * im.size[0] as f32) as usize;
            let v = ((p.y - r.top()) / r.height() * im.size[1] as f32) as usize;
            im.pixels[v * im.size[0] + u]
        };
        let painted = at(dst + vec2(30.0, 0.0));
        let below = at(dst + vec2(30.0, 120.0));
        assert!(painted.r() as i32 > below.r() as i32 + 60, "painted {painted:?} vs untouched {below:?}");
    }

    #[test]
    fn e2e_horizon_line_tilts_and_levels() {
        let mut h = open_on("horizon", Tab::Crop);
        h.state_mut().horizon = Some(Horizon { c: [0.5, 0.5], angle: 0.0 });
        h.step();
        let img = h.state().last_img_rect;
        let (_, _, end) = h.state().horizon.unwrap().points(img);
        drag(&mut h, end, end + vec2(0.0, 40.0));
        let a = h.state().horizon.unwrap().angle;
        assert!(a.to_degrees() > 5.0, "dragging the right end down tilts the line: {}", a.to_degrees());
        assert_eq!(h.state().params.crop, pipeline::FULL_CROP, "the crop did not move");
        // the middle moves it up and down
        let (c, _, _) = h.state().horizon.unwrap().points(img);
        drag(&mut h, c, c - vec2(0.0, 80.0));
        assert!(h.state().horizon.unwrap().c[1] < 0.45);
    }
}
