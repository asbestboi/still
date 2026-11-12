use crate::io::{self, Doc, ExportSettings, Format};
use crate::layers::{Blend, Layer};
use crate::masks::{self, BrushStroke, Local, Mask, Refine, Seed, Shape};
use crate::pipeline::{self, Aspect, Histogram, Img, Params};
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
    Preview { src: Arc<Img>, params: Params, generation: u64, frame: Frame, overlay: Option<usize> },
    Thumbs { src: Arc<Img>, looks: Vec<Params>, ver: u64 },
}

enum Msg {
    Preview { generation: u64, image: egui::ColorImage, hist: Histogram, ms: f32, frame: Frame, overlay: Option<egui::ColorImage> },
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
    LayerReady(Result<(PathBuf, f32), String>),
}

/// Mask overlay colour, read by the render thread.
static OVERLAY_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xFF2D28);
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

/// The mask as a translucent red wash, like a rubylith.
fn overlay_image(a: &[f32], w: usize, h: usize) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(w * h * 4);
    let c = OVERLAY_RGB.load(std::sync::atomic::Ordering::Relaxed);
    let (r, g, b) = ((c >> 16) as u8, (c >> 8) as u8, c as u8);
    for v in a {
        rgba.extend_from_slice(&[r, g, b, (v.clamp(0.0, 1.0) * 150.0) as u8]);
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

fn spawn_worker(ctx: egui::Context, jobs: Receiver<Job>, out: Sender<Msg>) {
    std::thread::spawn(move || {
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
            if let Some(Job::Preview { src, params, generation, frame, overlay }) = preview {
                let t = Instant::now();
                let (img, alpha) = pipeline::process_ext(&src, &params, overlay);
                let overlay = alpha.map(|a| overlay_image(&a, img.w, img.h));
                let hist = pipeline::histogram(&img);
                let image = to_color_image(&img);
                let ms = t.elapsed().as_secs_f32() * 1000.0;
                let _ = out.send(Msg::Preview { generation, image, hist, ms, frame, overlay });
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
    Looks,
    Adjust,
    Masks,
    Layers,
    Effects,
    Glitch,
    Crop,
}
const TABS: [(Tab, &str); 7] = [
    (Tab::Looks, "Looks"),
    (Tab::Adjust, "Adjust"),
    (Tab::Masks, "Masks"),
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
    last_sent: Option<(Params, Option<usize>)>,
    render_ms: f32,
    sent_at: Option<Instant>,
    tex_frame: Frame,
    overlay_tex: Option<egui::TextureHandle>,
    last_image: Option<Arc<egui::ColorImage>>,
    last_img_rect: Rect,
    mask_sel: Option<usize>,
    mask_drag: Option<MaskDrag>,
    show_overlay: bool,
    overlay_until: Option<Instant>,
    brush_size: f32,
    brush_feather: f32,
    brush_flow: f32,
    brush_erase: bool,
    picking_hue: bool,
    guide_thirds: bool,
    guide_golden: bool,
    prefs: io::Prefs,
    settings_open: bool,
    layer_sel: Option<usize>,
    layer_drag: Option<LayerDrag>,
    layers_loading: usize,
    crop_drag: Option<(Grip, [f32; 4], Pos2)>,

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
            sent_at: None,
            tex_frame: Frame::Output,
            overlay_tex: None,
            last_image: None,
            last_img_rect: Rect::NOTHING,
            mask_sel: None,
            mask_drag: None,
            show_overlay: false,
            overlay_until: None,
            brush_size: 0.45,
            brush_feather: 0.6,
            brush_flow: 1.0,
            brush_erase: false,
            picking_hue: false,
            guide_thirds: saved.guide_thirds,
            guide_golden: saved.guide_golden,
            prefs: saved.prefs.clone(),
            settings_open: false,
            layer_sel: None,
            layer_drag: None,
            layers_loading: 0,
            crop_drag: None,
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
        });
    }

    /// Push preferences into the parts of the app that read them globally.
    fn apply_prefs(&self) {
        k::set_reduce_motion(self.prefs.reduce_motion);
        OVERLAY_RGB.store(OVERLAY_COLORS[self.prefs.overlay_color.min(2)], std::sync::atomic::Ordering::Relaxed);
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

    // --- sharing looks -----------------------------------------------------

    fn export_looks(&mut self, looks: Vec<(String, Params)>, ctx: &egui::Context) {
        if self.dialog_open || looks.is_empty() {
            return;
        }
        self.dialog_open = true;
        let name = if looks.len() == 1 { io::look_file_name(&looks[0].0) } else { io::look_file_name("Still Looks") };
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let dlg = rfd::FileDialog::new()
                .set_title(if looks.len() == 1 { "Export Look" } else { "Export Looks" })
                .set_file_name(&name)
                .add_filter("Still Look", &[io::LOOK_EXT]);
            let msg = match dlg.save_file() {
                Some(mut p) => {
                    if !io::is_look_file(&p) {
                        p.set_extension(io::LOOK_EXT);
                    }
                    match io::write_looks(&p, &looks) {
                        Ok(()) => {
                            let what = if looks.len() == 1 { format!("“{}”", looks[0].0) } else { format!("{} looks", looks.len()) };
                            Msg::Info(format!("Exported {what}"), p.parent().map(|d| d.to_path_buf()))
                        }
                        Err(e) => Msg::Info(format!("Couldn't export — {e}"), None),
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
            let dlg = rfd::FileDialog::new().set_title("Import Looks").add_filter("Still Look", &[io::LOOK_EXT]);
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
            (0, Some(e)) => format!("Couldn't import — {e}"),
            (0, None) => return,
            (1, None) => "Imported 1 look".to_string(),
            (n, None) => format!("Imported {n} looks"),
            (n, Some(_)) => format!("Imported {n} looks, {} file{} skipped", errs.len(), if errs.len() == 1 { "" } else { "s" }),
        };
        self.toast = Some((msg, None, Instant::now()));
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
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
                .set_title("Open Photo")
                .add_filter("Images", &{
                    let mut v = vec!["jpg", "jpeg", "png", "tif", "tiff", "webp", "bmp", "gif", "tga", "exr", "hdr", "qoi", "pnm"];
                    v.extend_from_slice(io::RAW_EXTS);
                    v
                });
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
        // Doc isn't Clone (it owns big buffers) — share what the exporter needs.
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
                Msg::Preview { generation, image, hist, ms, frame, overlay } => {
                    if generation >= self.shown_gen {
                        self.shown_gen = generation;
                        self.tex_frame = frame;
                        self.overlay_tex = overlay.map(|o| match self.overlay_tex.take() {
                            Some(mut t) => {
                                t.set(o, egui::TextureOptions::LINEAR);
                                t
                            }
                            None => ctx.load_texture("mask_overlay", o, egui::TextureOptions::LINEAR),
                        });
                        self.last_image = Some(Arc::new(image.clone()));
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
                    self.layer_sel = None;
                    self.mask_sel = None;
                    self.committed = self.params.clone();
                    self.undo.clear();
                    self.redo.clear();
                    self.view = View { zoom: 1.0, pan: Vec2::ZERO, dragging: false };
                    self.request_thumbs();
                }
                Msg::Loaded(Err(e)) => {
                    self.loading = None;
                    self.toast = Some((format!("Couldn't open photo — {e}"), None, Instant::now()));
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
                Msg::LayerReady(r) => {
                    self.layers_loading = self.layers_loading.saturating_sub(1);
                    match r {
                        Ok((path, aspect)) => self.push_layer(path, aspect),
                        Err(e) => self.toast = Some((format!("Couldn't add photo — {e}"), None, Instant::now())),
                    }
                }
                Msg::Info(text, dir) => self.toast = Some((text, dir, Instant::now())),
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
                        Err(e) => self.toast = Some((format!("Export failed — {e}"), None, Instant::now())),
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
            Tab::Masks | Tab::Layers => Frame::Unbordered,
            _ => Frame::Output,
        }
    }

    fn selected_mask(&self) -> Option<usize> {
        self.mask_sel.filter(|i| *i < self.params.masks.len())
    }

    /// The red mask overlay shows while shaping a mask, briefly after adding
    /// one, while O is held, or always when switched on.
    fn overlay_wanted(&self, ctx: &egui::Context) -> Option<usize> {
        let i = self.selected_mask()?;
        if TABS[self.tab].0 != Tab::Masks {
            return None;
        }
        let text_focus = ctx.memory(|m| m.focused().is_some());
        let flash = self.overlay_until.map(|t| Instant::now() < t).unwrap_or(false);
        let key = !text_focus && ctx.input(|inp| inp.key_down(egui::Key::O));
        (self.show_overlay || self.mask_drag.is_some() || flash || key).then_some(i)
    }

    fn send_preview(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.overlay_until {
            let now = Instant::now();
            if now < t {
                ctx.request_repaint_after(t - now);
            }
        }
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
        let key = (params, overlay);
        if self.last_sent.as_ref() == Some(&key) {
            return;
        }
        let params = key.0.clone();
        self.generation += 1;
        self.last_sent = Some(key);
        self.sent_at = Some(Instant::now());
        let _ = self.jobs.send(Job::Preview { src: doc.preview.clone(), params, generation: self.generation, frame, overlay });
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
        if self.doc.is_some() && (sc(Modifiers::COMMAND, Key::E) || sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S)) {
            self.export_open = true;
        }
        if sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || sc(Modifiers::COMMAND, Key::Y) {
            self.do_redo();
        }
        if sc(Modifiers::COMMAND, Key::Z) {
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
        if self.settings_open && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.settings_open = false;
            self.persist();
        }
        if !text_focus {
            for (i, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7].into_iter().enumerate() {
                if ctx.input(|inp| inp.key_pressed(key) && inp.modifiers.is_none()) {
                    self.tab = i;
                }
            }
            if TABS[self.tab].0 == Tab::Layers
                && let Some(i) = self.selected_layer()
                && ctx.input(|inp| inp.key_pressed(Key::Delete) || inp.key_pressed(Key::Backspace))
            {
                self.params.layers.remove(i);
                self.layer_sel = None;
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
        }
    }

    fn handle_drop(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        let (looks, photos): (Vec<PathBuf>, Vec<PathBuf>) = dropped.into_iter().partition(|p| io::is_look_file(p));
        if !looks.is_empty() {
            self.import_look_files(&looks);
        }
        if self.doc.is_some() && TABS[self.tab].0 == Tab::Layers {
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
        ui.horizontal_centered(|ui| {
            if k::button(ui, "Open", Btn::Secondary, true).clicked() {
                self.pick_file(&ctx);
            }
            ui.add_space(4.0);
            let has_doc = self.doc.is_some();
            if k::button(ui, "Undo", Btn::Plain, !self.undo.is_empty()).on_hover_text("Ctrl+Z").clicked() {
                self.do_undo();
            }
            if k::button(ui, "Redo", Btn::Plain, !self.redo.is_empty()).on_hover_text("Ctrl+Shift+Z").clicked() {
                self.do_redo();
            }

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
            });
        });

        // centred title, independent of the button widths
        let painter = ui.painter();
        let (title, sub) = match (&self.doc, &self.loading) {
            (_, Some(name)) => (name.clone(), "Opening…".to_string()),
            (Some(d), None) => (d.name.clone(), d.source_desc.clone()),
            (None, None) => ("Still".to_string(), String::new()),
        };
        let cy = rect.center().y;
        if sub.is_empty() {
            painter.text(pos2(rect.center().x, cy), Align2::CENTER_CENTER, title, k::font_semibold(13.0), k::LABEL);
        } else {
            painter.text(pos2(rect.center().x, cy - 7.0), Align2::CENTER_CENTER, title, k::font_semibold(13.0), k::LABEL);
            painter.text(pos2(rect.center().x, cy + 9.0), Align2::CENTER_CENTER, sub, k::font(11.0), k::LABEL3);
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
        if cropping {
            self.view.zoom = 1.0;
            self.view.pan = Vec2::ZERO;
        }

        let tex = if compare { self.orig_tex.as_ref() } else { self.tex.as_ref() };
        let Some(tex) = tex.or(self.orig_tex.as_ref()) else { return };
        let (tex_id, tsize) = (tex.id(), tex.size_vec2());

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

        // --- drag to pan: 1:1, rubber-banded past the edges, momentum on release
        if resp.dragged() && self.view.zoom > 1.0 && self.mask_drag.is_none() && self.layer_drag.is_none() {
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

        let size = tsize * fit * zoom;
        let img_rect = Rect::from_center_size(rect.center() + vec2(px, py), size);
        painter.image(tex_id, img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        self.last_img_rect = img_rect;
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

        if self.view.zoom > 1.0 {
            ctx.set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
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

        if resp.drag_started()
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

        let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| clip.contains(*p));
        let active = self.crop_drag.map(|d| d.0).or_else(|| hover.and_then(|h| grip_at(r, h)));
        if let Some(g) = active {
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
        // rule of thirds, only while adjusting
        let grid_a = k::spring(&ctx, Id::unique("crop_grid"), if dragging { 1.0 } else { 0.0 }, 1.0, 0.25);
        let thirds = if self.guide_thirds { 1.0 } else { grid_a * 0.8 };
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
                self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(1400));
            }
            return;
        }
        if matches!(self.params.masks[i].shape, Shape::Brush { .. }) {
            self.mask_begin(o, ctx);
            self.mask_drag = None;
        }
        if img.contains(o) {
            let add = !ctx.input(|inp| inp.modifiers.alt);
            let map = self.mask_map(img);
            if let Shape::Object { seeds, .. } = &mut self.params.masks[i].shape {
                seeds.push(Seed { p: map.s2n(o), add });
                self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(1500));
            }
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
        let hover = ctx.input(|inp| inp.pointer.hover_pos()).filter(|p| clip.contains(*p));
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
                for sd in seeds {
                    let p = map.n2s(sd.p);
                    painter.circle_filled(p + vec2(0.0, 1.0), 8.0, shadow);
                    painter.circle_filled(p, 7.0, if sd.add { k::ACCENT } else { Color32::from_rgb(255, 69, 58) });
                    painter.circle_stroke(p, 7.0, Stroke::new(1.5, white));
                    painter.text(p, Align2::CENTER_CENTER, if sd.add { "+" } else { "−" }, k::font_semibold(11.0), white);
                }
                if hover.map(|h| img.contains(h)).unwrap_or(false) && !space_or_zoom_pan(&ctx) {
                    ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
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
        self.toast_view(ui, rect);
    }

    fn drop_hint(&self, ui: &Ui, rect: Rect) {
        let r = rect.shrink(14.0);
        ui.painter().rect_filled(r, CornerRadius::same(16), k::ACCENT.gamma_multiply(0.08));
        ui.painter().rect_stroke(r, CornerRadius::same(16), Stroke::new(2.0, k::ACCENT), egui::StrokeKind::Inside);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, "Drop to open", k::font_semibold(15.0), k::LABEL);
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
                let _ = std::process::Command::new("xdg-open").arg(d).spawn();
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

        let labels: Vec<&str> = TABS.iter().map(|t| t.1).collect();
        let prev = self.tab;
        k::segmented(ui, "tabs", &mut self.tab, &labels);
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
            if !has_doc && TABS[self.tab].0 == Tab::Looks {
                k::caption(ui, "Open a photo to preview looks.");
            }
            match TABS[self.tab].0 {
                Tab::Looks => self.tab_looks(ui),
                Tab::Adjust => self.tab_adjust(ui),
                Tab::Masks => self.tab_masks(ui),
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

    fn tab_looks(&mut self, ui: &mut Ui) {
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
                            if ui.button("Update with Current Look").clicked() {
                                overwrite = Some(idx - builtin_n);
                                ui.close();
                            }
                            if ui.button("Delete Look").clicked() {
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
            self.user_presets[i].1 = Params { masks: Vec::new(), layers: Vec::new(), ..self.params.clone() };
            changed = true;
        }
        if let Some(i) = delete {
            self.user_presets.remove(i);
            changed = true;
        }

        ui.add_space(8.0);
        k::section(ui, "Save Look", false, |ui| {
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
                    self.user_presets.push((self.new_preset.trim().to_string(), Params { masks: Vec::new(), layers: Vec::new(), ..self.params.clone() }));
                    self.new_preset.clear();
                    changed = true;
                }
            });
            if !self.user_presets.is_empty() {
                k::caption(ui, "Right-click a saved look to update, export or delete it.");
            }
        });

        let ctx = ui.ctx().clone();
        let mut import = false;
        let mut export_all = false;
        k::section(ui, "Share Looks", false, |ui| {
            k::caption(ui, "Looks are saved as small .stilllook files. Send one to a friend; they can import it here or drop it onto the window.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if k::button(ui, "Import…", Btn::Secondary, true).clicked() {
                    import = true;
                }
                if k::button(ui, "Export My Looks…", Btn::Secondary, !self.user_presets.is_empty()).clicked() {
                    export_all = true;
                }
            });
            ui.add_space(2.0);
            k::caption(ui, "Only the look is shared — never your photo, crop or masks.");
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

    fn tab_effects(&mut self, ui: &mut Ui) {
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
        let p = &mut self.params;
        k::caption(ui, "Glitches are placed by a seed — shuffle for a new arrangement.");
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
                let mut v = vec!["jpg", "jpeg", "png", "tif", "tiff", "webp", "bmp", "gif", "tga", "exr", "hdr", "qoi"];
                v.extend_from_slice(io::RAW_EXTS);
                v
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
        l.rotation = 0.0;
        l.x = (c[0] + c[2]) * 0.5;
        l.y = (c[1] + c[3]) * 0.5;
    }

    fn layer_begin(&mut self, o: Pos2, ctx: &egui::Context) {
        let map = self.mask_map(self.last_img_rect);
        // grab a handle of the selected layer first
        if let Some(i) = self.selected_layer() {
            let l = &self.params.layers[i];
            let (_, corners, knob) = layer_frame(l, &map);
            let kind = if knob.distance(o) < 14.0 {
                Some(2)
            } else if corners.iter().any(|c| c.distance(o) < 14.0) {
                Some(1)
            } else if layer_contains(l, &map, o) {
                Some(0)
            } else {
                None
            };
            if let Some(kind) = kind {
                self.layer_drag = Some(LayerDrag { kind, start: l.clone(), origin: o });
                return;
            }
        }
        // otherwise pick the topmost visible layer under the pointer and move it
        if let Some(i) = (0..self.params.layers.len()).rev().find(|&i| {
            let l = &self.params.layers[i];
            l.visible && layer_contains(l, &map, o)
        }) {
            self.layer_sel = Some(i);
            self.layer_drag = Some(LayerDrag { kind: 0, start: self.params.layers[i].clone(), origin: o });
        }
        let _ = ctx;
    }

    fn layer_click(&mut self, o: Pos2) {
        let map = self.mask_map(self.last_img_rect);
        let hit = (0..self.params.layers.len()).rev().find(|&i| {
            let l = &self.params.layers[i];
            l.visible && layer_contains(l, &map, o)
        });
        self.layer_sel = hit;
    }

    /// Outline + handles for the selected layer, and live dragging.
    fn layer_overlay(&mut self, ui: &mut Ui, resp: &egui::Response, clip: Rect, img: Rect) {
        let ctx = ui.ctx().clone();
        let map = self.mask_map(img);
        if let Some(d) = self.layer_drag.clone() {
            if let (Some(i), Some(p)) = (self.selected_layer(), ctx.input(|inp| inp.pointer.latest_pos())) {
                let shift = ctx.input(|inp| inp.modifiers.shift);
                let s = &d.start;
                let c = map.n2s([s.x, s.y]);
                let l = &mut self.params.layers[i];
                match d.kind {
                    0 => {
                        let f = map.frame();
                        let dl = p - d.origin;
                        l.x = s.x + dl.x / f.x;
                        l.y = s.y + dl.y / f.y;
                    }
                    1 => {
                        let r0 = (d.origin - c).length().max(1.0);
                        l.scale = (s.scale * (p - c).length() / r0).clamp(0.005, 20.0);
                    }
                    _ => {
                        let a0 = (d.origin - c).y.atan2((d.origin - c).x);
                        let a1 = (p - c).y.atan2((p - c).x);
                        let mut deg = s.rotation + (a1 - a0).to_degrees();
                        deg = (deg + 180.0).rem_euclid(360.0) - 180.0;
                        if shift {
                            deg = (deg / 15.0).round() * 15.0;
                        }
                        l.rotation = deg;
                    }
                }
            }
            if !resp.dragged() || resp.drag_stopped() {
                self.layer_drag = None;
            }
        }

        let Some(i) = self.selected_layer() else { return };
        let l = &self.params.layers[i];
        let painter = ui.painter_at(clip);
        let (c, corners, knob) = layer_frame(l, &map);
        let shadow = Color32::from_black_alpha(110);
        let mut outline = corners.to_vec();
        outline.push(corners[0]);
        painter.add(egui::Shape::line(outline.clone(), Stroke::new(3.0, Color32::from_black_alpha(70))));
        painter.add(egui::Shape::line(outline, Stroke::new(1.2, Color32::WHITE)));
        let top_mid = corners[0] + (corners[1] - corners[0]) * 0.5;
        painter.line_segment([top_mid, knob], Stroke::new(1.2, Color32::from_white_alpha(200)));
        let hover = ctx.input(|inp| inp.pointer.hover_pos()).filter(|p| clip.contains(*p));
        let active = self.layer_drag.as_ref().map(|d| d.kind);
        for p in corners {
            let big = active == Some(1) || hover.map(|h| h.distance(p) < 14.0).unwrap_or(false);
            let r = if big { 6.5 } else { 5.0 };
            painter.circle_filled(p + vec2(0.0, 1.0), r + 1.5, shadow);
            painter.circle_filled(p, r, Color32::WHITE);
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
                _ => egui::CursorIcon::Alias,
            });
        } else if let Some(h) = hover {
            if h.distance(knob) < 14.0 {
                ctx.set_cursor_icon(egui::CursorIcon::Alias);
            } else if corners.iter().any(|p| p.distance(h) < 14.0) {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            } else if layer_contains(l, &map, h) {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        // live size readout while transforming
        if active.is_some() {
            let label = format!("{:.0}%  ·  {:.0}°", l.scale * 100.0, l.rotation);
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
        k::caption(ui, "Place other photos on the canvas. Drag to move, corners to scale, the blue knob to rotate (Shift snaps). You can also drop files here.");
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

        enum Act {
            Select(usize),
            Toggle(usize),
            Duplicate(usize),
            Up(usize),
            Down(usize),
            Delete(usize),
        }
        let mut act = None;
        let sel = self.selected_layer();
        let n = self.params.layers.len();
        egui::Frame::NONE.fill(k::CARD).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            // top of the stack first, like any layers panel
            for i in (0..n).rev() {
                let l = &self.params.layers[i];
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
                let bg = if sel == Some(i) {
                    Color32::from_rgba_unmultiplied(10, 132, 255, 70)
                } else if resp.is_pointer_button_down_on() {
                    k::FILL_HI
                } else if resp.hovered() {
                    k::FILL
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(r, CornerRadius::same(8), bg);
                let dot = pos2(r.left() + 16.0, r.center().y);
                let dresp = ui.interact(Rect::from_center_size(dot, vec2(24.0, 24.0)), ui.scope_id().with(("layer_dot", i)), Sense::click());
                if l.visible {
                    ui.painter().circle_filled(dot, 5.0, k::ACCENT);
                } else {
                    ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.5, k::LABEL3));
                }
                let col = if l.visible { k::LABEL } else { k::LABEL3 };
                let name: String = l.name.chars().take(22).collect();
                ui.painter().text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, name, k::font(13.0), col);
                let tag = format!("{}{}", l.blend.label(), if l.opacity < 0.999 { format!(" · {:.0}%", l.opacity * 100.0) } else { String::new() });
                ui.painter().text(pos2(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, tag, k::font(11.0), k::LABEL3);
                if dresp.clicked() {
                    act = Some(Act::Toggle(i));
                } else if resp.clicked() {
                    act = Some(Act::Select(i));
                }
                resp.context_menu(|ui| {
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
            // the photo itself, always at the bottom
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            ui.painter().text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, "Photo", k::font(13.0), k::LABEL3);
            ui.painter().text(pos2(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, "Background", k::font(11.0), k::LABEL3);
        });
        let layers = &mut self.params.layers;
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
            None => {}
        }
        if n > 0 {
            k::caption(ui, "Right-click a layer to duplicate, reorder or delete it.");
        }
        ui.add_space(10.0);

        let Some(i) = self.selected_layer() else {
            if n > 0 {
                k::caption(ui, "Select a layer to edit it.");
            }
            return;
        };
        let mut fit = None;
        let l = &mut self.params.layers[i];
        let d = Layer::default();
        if k::section(ui, "Blending", l.opacity < 1.0 || l.blend != Blend::Normal || !l.graded, |ui| {
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
            k::switch_row(ui, "Apply Look to Layer", &mut l.graded);
            k::caption(ui, "On: the layer is graded with the photo. Off: it sits on top of the finished look, good for light leaks and textures (try Screen).");
            ui.add_space(4.0);
        }) {
            l.opacity = d.opacity;
            l.blend = d.blend;
            l.graded = d.graded;
        }
        if k::section(ui, "Transform", l.rotation != 0.0 || l.flip, |ui| {
            let mut pct = l.scale * 100.0;
            k::slider(ui, "Size (% of long edge)", &mut pct, 1.0..=300.0, 50.0, Fmt::Px);
            l.scale = pct / 100.0;
            k::slider(ui, "Rotation", &mut l.rotation, -180.0..=180.0, 0.0, Fmt::Deg);
            k::switch_row(ui, "Flip Horizontal", &mut l.flip);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if k::button(ui, "Fit", Btn::Secondary, true).clicked() {
                    fit = Some(false);
                }
                if k::button(ui, "Fill", Btn::Secondary, true).clicked() {
                    fit = Some(true);
                }
            });
            ui.add_space(4.0);
        }) {
            l.rotation = 0.0;
            l.flip = false;
        }
        if let Some(fill) = fit {
            self.fit_layer(i, fill);
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
        self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(1600));
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
        }
        let mut act = None;
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
                if m.enabled {
                    ui.painter().circle_filled(dot, 5.0, k::ACCENT);
                } else {
                    ui.painter().circle_stroke(dot, 5.0, Stroke::new(1.5, k::LABEL3));
                }
                let dresp = dresp.on_hover_text(if m.enabled { "Hide mask" } else { "Show mask" });
                let col = if m.enabled { k::LABEL } else { k::LABEL3 };
                ui.painter().text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, &m.name, k::font(13.0), col);
                let edited = !m.adj.is_identity();
                let tag = format!("{}{}", if m.invert { "Inverted · " } else { "" }, if edited { "Edited" } else { "No edits" });
                ui.painter().text(pos2(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, tag, k::font(11.0), k::LABEL3);
                if dresp.clicked() {
                    act = Some(Act::Toggle(i));
                } else if resp.clicked() {
                    act = Some(Act::Select(i));
                }
                resp.context_menu(|ui| {
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
                    self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(900));
                }
                self.mask_sel = Some(i);
                self.picking_hue = false;
            }
            Some(Act::Toggle(i)) => masks[i].enabled = !masks[i].enabled,
            Some(Act::Duplicate(i)) => {
                let mut m = masks[i].clone();
                m.name = format!("{} copy", m.name);
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
            None => {}
        }
        k::caption(ui, "Right-click a mask for more. Masks apply top to bottom.");
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
                    k::caption(ui, "Click the subject on the photo. Click more spots to add to it, Alt+click to take areas away.");
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
                        self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(900));
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
            k::switch_row(ui, "Show Overlay", &mut self.show_overlay);
            k::caption(ui, "Hold O to peek at the mask.");
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
                self.overlay_until = Some(Instant::now() + std::time::Duration::from_millis(1200));
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
                    let label = if self.picking_hue { "Click the Photo…" } else { "Pick from Photo" };
                    if k::button(ui, label, Btn::Secondary, true).clicked() {
                        self.picking_hue = !self.picking_hue;
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

        let a = &mut m.adj;
        if k::section(ui, "Adjustments", !a.is_identity(), |ui| {
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
        }) {
            *a = Local { tone_hue: 35.0, ..Local::default() };
        }

        ui.add_space(4.0);
        if k::button(ui, "Delete Mask", Btn::Plain, true).clicked() {
            self.params.masks.remove(i);
            self.mask_sel = None;
        }
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
            k::slider(ui, "Straighten", &mut p.straighten, -15.0..=15.0, 0.0, Fmt::Deg);
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
            k::caption(ui, "Guides show over the photo in every tab. They are never exported.");
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
                            k::switch_row(ui, "Keep Look When Opening a Photo", &mut self.prefs.keep_look);
                            k::caption(ui, "On: the next photo opens with your current look, handy for a series. Off: every photo starts clean.");
                            ui.add_space(12.0);

                            group(ui, "Looks");
                            ui.horizontal(|ui| {
                                if k::button(ui, "Import Looks…", Btn::Secondary, true).clicked() {
                                    import = true;
                                }
                                if k::button(ui, "Export My Looks…", Btn::Secondary, !self.user_presets.is_empty()).clicked() {
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
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
        if close {
            self.settings_open = false;
            self.persist();
        }
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
            let _ = ctx.run_ui(input, |ui| {
                let resp = ui.interact(ui.max_rect(), Id::unique("click_test"), Sense::click_and_drag());
                if resp.clicked() {
                    seen = click_pos(&resp);
                    origin_on_click = ui.input(|i| i.pointer.press_origin());
                }
            });
        }
        assert_eq!(seen, Some(at), "click position must be known when the click fires");
        assert_eq!(origin_on_click, None, "press_origin is gone on the click frame (the original bug)");
    }
}
