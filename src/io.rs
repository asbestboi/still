//! Loading and exporting.

use crate::pipeline::{self, Img, Params};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageReader, metadata::Orientation};
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const THUMB_EDGE: u32 = 220;

pub struct Doc {
    pub path: PathBuf,
    pub name: String,
    pub full: Arc<DynamicImage>,
    pub preview: Arc<Img>,
    pub thumb_src: Arc<Img>,
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    pub source_desc: String,
}

pub const RAW_EXTS: &[&str] = &[
    "rw2", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "pef", "srw", "rwl", "3fr", "iiq",
    "erf", "kdc", "dcr", "mos", "mef", "mrw", "ari",
];

/// Extensions for a file-dialog filter. The Linux dialogs match patterns
/// case-sensitively, and cameras name their files `DSC00747.ARW`, so each
/// extension is offered in lower and upper case.
pub fn dialog_exts(exts: &[&str]) -> Vec<String> {
    exts.iter().flat_map(|e| [e.to_lowercase(), e.to_uppercase()]).collect()
}

fn is_raw(path: &Path) -> bool {
    path.extension()
        .map(|e| RAW_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Develop a camera RAW into a 16-bit sRGB image (demosaic, white balance,
/// camera colour matrix). Tone is left neutral for the user to shape.
fn load_raw(path: &Path) -> Result<DynamicImage, String> {
    let path = path.to_path_buf();
    std::panic::catch_unwind(move || -> Result<DynamicImage, String> {
        let src = rawler::rawsource::RawSource::new(&path).map_err(|e| e.to_string())?;
        let params = rawler::decoders::RawDecodeParams::default();
        let raw = rawler::decode(&src, &params).map_err(|e| e.to_string())?;
        let dev = rawler::imgop::develop::RawDevelop::default();
        let inter = dev.develop_intermediate(&raw).map_err(|e| e.to_string())?;
        let mut img = inter.to_dynamic_image().ok_or_else(|| "RAW develop produced no image".to_string())?;
        // The sensor data is stored landscape; turn portrait shots upright.
        // rawler leaves `RawImage::orientation` at Normal, so read the EXIF tag.
        let orientation = rawler::get_decoder(&src)
            .and_then(|d| d.raw_metadata(&src, &params))
            .ok()
            .and_then(|m| m.exif.orientation)
            .and_then(|o| Orientation::from_exif(o as u8));
        if let Some(o) = orientation {
            img.apply_orientation(o);
        }
        Ok(img)
    })
    .map_err(|_| "this RAW format isn't supported".to_string())?
}

/// Open a photo, building a working preview with a long edge of `preview_edge`.
pub fn load(path: &Path, preview_edge: u32) -> Result<Doc, String> {
    if is_raw(path) {
        let full = load_raw(path)?;
        let ext = path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        let source_desc = format!("{} × {} · {ext} RAW", full.width(), full.height());
        let preview = Arc::new(downsized(&full, preview_edge));
        let thumb_src = Arc::new(downsized(&full, THUMB_EDGE));
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        return Ok(Doc { path: path.to_path_buf(), name, full: Arc::new(full), preview, thumb_src, icc: None, exif: None, source_desc });
    }
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let fmt = reader.format();
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let icc = decoder.icc_profile().ok().flatten();
    let mut exif = decoder.exif_metadata().ok().flatten();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut full = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    full.apply_orientation(orientation);
    // orientation is now baked into the pixels; clear it so exports aren't rotated twice
    if let Some(e) = exif.as_mut() {
        let _ = Orientation::remove_from_exif_chunk(e);
    }

    let depth = match full.color() {
        c if c.bytes_per_pixel() / c.channel_count() >= 4 => "32-bit float",
        c if c.bytes_per_pixel() / c.channel_count() == 2 => "16-bit",
        _ => "8-bit",
    };
    let fmt_name = fmt.map(|f| format!("{f:?}").to_uppercase()).unwrap_or_else(|| "IMAGE".into());
    let source_desc = format!(
        "{} × {} · {} {}{}",
        full.width(),
        full.height(),
        fmt_name,
        depth,
        if icc.is_some() { " · ICC" } else { "" }
    );

    let preview = Arc::new(downsized(&full, preview_edge));
    let thumb_src = Arc::new(downsized(&full, THUMB_EDGE));
    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    Ok(Doc { path: path.to_path_buf(), name, full: Arc::new(full), preview, thumb_src, icc, exif, source_desc })
}

/// Decode any supported image (RAW included), upright, keeping its alpha.
pub fn decode(path: &Path) -> Result<DynamicImage, String> {
    if is_raw(path) {
        return load_raw(path);
    }
    let reader = ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

pub fn downsized(img: &DynamicImage, edge: u32) -> Img {
    let (w, h) = (img.width(), img.height());
    let s = edge as f32 / w.max(h) as f32;
    if s >= 1.0 {
        return Img::from_rgb32f(&img.to_rgb32f());
    }
    let (nw, nh) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
    let small = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
    Img::from_rgb32f(&small.to_rgb32f())
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Format {
    Jpeg,
    Png,
    Tiff,
    Webp,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Jpeg, Format::Png, Format::Tiff, Format::Webp];
    pub fn label(self) -> &'static str {
        match self {
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::Tiff => "TIFF",
            Format::Webp => "WebP",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::Tiff => "tif",
            Format::Webp => "webp",
        }
    }
    pub fn supports_16(self) -> bool {
        matches!(self, Format::Png | Format::Tiff)
    }
    pub fn supports_metadata(self) -> bool {
        matches!(self, Format::Jpeg | Format::Png)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ExportSettings {
    pub format: Format,
    pub quality: f32,
    pub depth16: bool,
    /// 0 = original, 1 = long edge, 2 = percentage
    pub size_mode: usize,
    pub long_edge: f32,
    pub percent: f32,
    /// 0 off, 1 low, 2 standard, 3 high
    pub sharpen: usize,
    pub keep_metadata: bool,
    pub keep_icc: bool,
    pub suffix: String,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            format: Format::Jpeg,
            quality: 0.92,
            depth16: true,
            size_mode: 0,
            long_edge: 3000.0,
            percent: 1.0,
            sharpen: 0,
            keep_metadata: true,
            keep_icc: true,
            suffix: "-still".into(),
        }
    }
}

impl ExportSettings {
    pub fn scale_for(&self, w: u32, h: u32) -> f32 {
        match self.size_mode {
            1 => self.long_edge / w.max(h) as f32,
            2 => self.percent,
            _ => 1.0,
        }
    }
}

pub fn export(doc: &Doc, params: &Params, s: &ExportSettings, path: &Path) -> Result<PathBuf, String> {
    let full = &doc.full;
    let scale = s.scale_for(full.width(), full.height());
    let src = if (scale - 1.0).abs() > 1e-3 {
        let nw = ((full.width() as f32 * scale).round() as u32).max(1);
        let nh = ((full.height() as f32 * scale).round() as u32).max(1);
        full.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3).to_rgb32f()
    } else {
        full.to_rgb32f()
    };
    let mut img = pipeline::process(&Img::from_rgb32f(&src), params);
    drop(src);
    pipeline::output_sharpen(&mut img, [0.0, 0.25, 0.5, 0.9][s.sharpen.min(3)]);

    let (w, h) = (img.w as u32, img.h as u32);
    let deep = s.depth16 && s.format.supports_16();
    let tmp = path.with_extension(format!("{}.part", s.format.ext()));
    let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut out = BufWriter::new(file);

    let icc = if s.keep_icc { doc.icc.clone() } else { None };
    let exif = if s.keep_metadata { doc.exif.clone() } else { None };

    let result: Result<(), String> = (|| {
        match s.format {
            Format::Jpeg => {
                let q = (s.quality * 100.0).round().clamp(1.0, 100.0) as u8;
                let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q);
                if let Some(i) = icc {
                    let _ = enc.set_icc_profile(i);
                }
                if let Some(e) = exif {
                    let _ = enc.set_exif_metadata(e);
                }
                enc.write_image(&to_u8(&img), w, h, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())
            }
            Format::Png => {
                let mut enc = image::codecs::png::PngEncoder::new_with_quality(
                    &mut out,
                    image::codecs::png::CompressionType::Default,
                    image::codecs::png::FilterType::Adaptive,
                );
                if let Some(i) = icc {
                    let _ = enc.set_icc_profile(i);
                }
                if let Some(e) = exif {
                    let _ = enc.set_exif_metadata(e);
                }
                if deep {
                    enc.write_image(&to_u16_bytes(&img), w, h, image::ExtendedColorType::Rgb16)
                } else {
                    enc.write_image(&to_u8(&img), w, h, image::ExtendedColorType::Rgb8)
                }
                .map_err(|e| e.to_string())
            }
            Format::Tiff => {
                let mut enc = image::codecs::tiff::TiffEncoder::new(&mut out);
                if let Some(i) = icc {
                    let _ = enc.set_icc_profile(i);
                }
                if deep {
                    enc.write_image(&to_u16_bytes(&img), w, h, image::ExtendedColorType::Rgb16)
                } else {
                    enc.write_image(&to_u8(&img), w, h, image::ExtendedColorType::Rgb8)
                }
                .map_err(|e| e.to_string())
            }
            Format::Webp => image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .write_image(&to_u8(&img), w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| e.to_string()),
        }
    })();

    let result = result.and_then(|_| out.flush().map_err(|e| e.to_string()));
    drop(out);
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
            Ok(path.to_path_buf())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 8-bit quantisation with a tiny triangular dither, which prevents banding
/// in skies and smooth gradients.
fn to_u8(img: &Img) -> Vec<u8> {
    use rayon::prelude::*;
    let mut out = vec![0u8; img.w * img.h * 3];
    out.par_chunks_mut(img.w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..img.w {
            let p = img.px[y * img.w + x];
            for c in 0..3 {
                let n = tri_dither(x as u32, y as u32, c as u32);
                row[x * 3 + c] = (p[c] * 255.0 + n).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

fn to_u16_bytes(img: &Img) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.w * img.h * 6);
    for p in &img.px {
        for c in p {
            let v = (c.clamp(0.0, 1.0) * 65535.0).round() as u16;
            out.extend_from_slice(&v.to_ne_bytes());
        }
    }
    out
}

fn tri_dither(x: u32, y: u32, c: u32) -> f32 {
    let h = |s: u32| {
        let mut v = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ s.wrapping_mul(0xC2B2_AE3D);
        v ^= v >> 15;
        v = v.wrapping_mul(0x2C1B_3C6D);
        v ^= v >> 13;
        (v >> 8) as f32 / (1u32 << 24) as f32
    };
    h(c * 2 + 1) - h(c * 2 + 2)
}

// ---------------------------------------------------------------------------
// Persistence (presets & export settings) in ~/.config/still
// ---------------------------------------------------------------------------

pub fn config_dir() -> PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join(if cfg!(windows) { "Still" } else { "still" })
}

/// Show a folder in the system file manager.
pub fn reveal(dir: &Path) {
    let cmd = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(dir).spawn();
}

/// App preferences (the Settings sheet).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// 0 Fast, 1 Balanced, 2 Detailed
    pub preview_quality: usize,
    /// 0 Black, 1 Graphite, 2 Gray
    pub canvas: usize,
    /// 0 Red, 1 Green, 2 Blue
    pub overlay_color: usize,
    /// 0 RGB, 1 Luminance
    pub histogram: usize,
    pub show_render_time: bool,
    pub reduce_motion: bool,
    pub keep_look: bool,
    /// Linux: talk to Wayland directly. Off runs through XWayland, which is
    /// what makes drag and drop work (winit has no Wayland drag and drop yet).
    pub native_wayland: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            preview_quality: 1,
            canvas: 0,
            overlay_color: 0,
            histogram: 0,
            show_render_time: true,
            reduce_motion: false,
            keep_look: true,
            native_wayland: false,
        }
    }
}

impl Prefs {
    pub const PREVIEW_EDGES: [u32; 3] = [1200, 1800, 2600];
    pub fn preview_edge(&self) -> u32 {
        Self::PREVIEW_EDGES[self.preview_quality.min(2)]
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Saved {
    #[serde(default)]
    pub presets: Vec<(String, Params)>,
    #[serde(default)]
    pub export: ExportSettings,
    #[serde(default)]
    pub guide_thirds: bool,
    #[serde(default)]
    pub guide_golden: bool,
    #[serde(default)]
    pub prefs: Prefs,
}

pub fn load_saved() -> Saved {
    std::fs::read_to_string(config_dir().join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn store_saved(s: &Saved) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let tmp = dir.join("settings.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, dir.join("settings.json"));
        }
    }
}

// ---------------------------------------------------------------------------
// Sharing looks
// ---------------------------------------------------------------------------

/// File extension for shared looks. The file is plain JSON, so it survives
/// email, chat apps and cloud drives, and stays readable.
pub const LOOK_EXT: &str = "stilllook";
const LOOK_FORMAT: &str = "still-look";

#[derive(Serialize, Deserialize)]
struct LookFile {
    format: String,
    version: u32,
    looks: Vec<SharedLook>,
}

#[derive(Serialize, Deserialize)]
struct SharedLook {
    name: String,
    params: Params,
}

/// Only the look travels: crop, rotation, border and masks belong to a photo.
fn look_only(p: &Params) -> Params {
    Params::default().with_look_from(p)
}

pub fn write_looks(path: &Path, looks: &[(String, Params)]) -> Result<(), String> {
    let file = LookFile {
        format: LOOK_FORMAT.into(),
        version: 1,
        looks: looks.iter().map(|(n, p)| SharedLook { name: n.clone(), params: look_only(p) }).collect(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("{LOOK_EXT}.part"));
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn read_looks(path: &Path) -> Result<Vec<(String, Params)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: LookFile = serde_json::from_str(&text).map_err(|_| "not a Still look file".to_string())?;
    if file.format != LOOK_FORMAT {
        return Err("not a Still look file".into());
    }
    if file.looks.is_empty() {
        return Err("the file has no looks".into());
    }
    Ok(file.looks.into_iter().map(|l| (l.name.trim().chars().take(60).collect(), look_only(&l.params))).collect())
}

pub fn is_look_file(path: &Path) -> bool {
    path.extension().map(|e| e.eq_ignore_ascii_case(LOOK_EXT)).unwrap_or(false)
}

/// A file name for a look: keeps it readable, drops characters that trip up file systems.
pub fn look_file_name(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '-' }).collect();
    let clean = clean.trim();
    format!("{}.{LOOK_EXT}", if clean.is_empty() { "Look" } else { clean })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_round_trip_without_photo_specific_settings() {
        let dir = std::env::temp_dir().join(format!("still-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.stilllook");
        let mut p = pipeline::builtin_presets().into_iter().find(|(n, _)| n == "MGS4").unwrap().1;
        p.rotate = 1;
        p.crop = [0.1, 0.1, 0.9, 0.9];
        p.masks.push(crate::masks::Mask::default());
        write_looks(&path, &[("Field / Night".into(), p.clone())]).unwrap();
        let back = read_looks(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "Field / Night");
        let q = &back[0].1;
        assert_eq!(q.grain, p.grain);
        assert_eq!(q.saturation, p.saturation);
        assert_eq!(q.rotate, 0);
        assert_eq!(q.crop, pipeline::FULL_CROP);
        assert!(q.masks.is_empty());
        std::fs::write(&path, "{\"hello\": 1}").unwrap();
        assert!(read_looks(&path).is_err());
        assert_eq!(look_file_name("Field / Night"), "Field - Night.stilllook");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn projects_round_trip_and_survive_being_moved() {
        let root = std::env::temp_dir().join(format!("still-proj-{}", std::process::id()));
        let a = root.join("a");
        std::fs::create_dir_all(a.join("pics")).unwrap();
        let photo = a.join("pics/p.png");
        image::RgbImage::new(4, 4).save(&photo).unwrap();
        let layer = a.join("pics/l.png");
        image::RgbImage::new(4, 4).save(&layer).unwrap();
        let mut params = pipeline::builtin_presets().remove(3).1;
        params.crop = [0.1, 0.2, 0.8, 0.9];
        params.masks.push(crate::masks::Mask::default());
        params.layers.push(crate::layers::Layer { path: layer.to_string_lossy().to_string(), stretch: 2.0, ..Default::default() });
        let proj = a.join("edit.still");
        write_project(&proj, &photo, &params).unwrap();
        let (p, back) = read_project(&proj).unwrap();
        assert_eq!(absolute(&p), absolute(&photo));
        assert_eq!(back, Params { layers: back.layers.clone(), ..params.clone() });
        // move the whole folder: paths resolve relative to the project
        let b = root.join("b");
        std::fs::rename(&a, &b).unwrap();
        let (p, back) = read_project(&b.join("edit.still")).unwrap();
        assert!(p.starts_with(&b) && p.exists());
        assert!(Path::new(&back.layers[0].path).starts_with(&b));
        assert!(read_project(&b.join("pics/p.png")).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}

// ---------------------------------------------------------------------------
// Project files
// ---------------------------------------------------------------------------

/// A saved edit: which photo, and everything done to it (look, crop, masks,
/// layers). Paths are stored both absolute and relative to the project file,
/// so a folder with the project and its photos can be moved or shared.
pub const PROJECT_EXT: &str = "still";
const PROJECT_FORMAT: &str = "still-project";

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: String,
    version: u32,
    photo: String,
    #[serde(default)]
    photo_relative: Option<String>,
    params: Params,
    /// Relative paths of layer sources, in the same order as `params.layers`.
    #[serde(default)]
    layers_relative: Vec<Option<String>>,
}

pub fn is_project_file(path: &Path) -> bool {
    path.extension().map(|e| e.eq_ignore_ascii_case(PROJECT_EXT)).unwrap_or(false)
}

/// `target` relative to `base_dir`, when they share a root.
fn relative_to(target: &Path, base_dir: &Path) -> Option<String> {
    let t: Vec<_> = target.components().collect();
    let b: Vec<_> = base_dir.components().collect();
    let common = t.iter().zip(&b).take_while(|(a, c)| a == c).count();
    if common == 0 {
        return None;
    }
    let mut rel = PathBuf::new();
    for _ in common..b.len() {
        rel.push("..");
    }
    for c in &t[common..] {
        rel.push(c.as_os_str());
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn absolute(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn write_project(path: &Path, photo: &Path, params: &Params) -> Result<(), String> {
    let dir = absolute(path.parent().unwrap_or(Path::new(".")));
    let photo_abs = absolute(photo);
    let file = ProjectFile {
        format: PROJECT_FORMAT.into(),
        version: 1,
        photo: photo_abs.to_string_lossy().to_string(),
        photo_relative: relative_to(&photo_abs, &dir),
        layers_relative: params.layers.iter().map(|l| relative_to(&absolute(Path::new(&l.path)), &dir)).collect(),
        params: params.clone(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("{PROJECT_EXT}.part"));
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Read a project: the photo to open and the edit to apply to it.
pub fn read_project(path: &Path) -> Result<(PathBuf, Params), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: ProjectFile = serde_json::from_str(&text).map_err(|_| "not a Still project".to_string())?;
    if file.format != PROJECT_FORMAT {
        return Err("not a Still project".into());
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    // prefer the original location; fall back to the same place relative to the project
    let resolve = |abs: &str, rel: Option<&String>| -> Option<PathBuf> {
        let a = PathBuf::from(abs);
        if a.exists() {
            return Some(a);
        }
        rel.map(|r| dir.join(r)).filter(|p| p.exists())
    };
    let photo = resolve(&file.photo, file.photo_relative.as_ref())
        .ok_or_else(|| format!("photo not found: {}", Path::new(&file.photo).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))?;
    let mut params = file.params;
    for (i, l) in params.layers.iter_mut().enumerate() {
        if let Some(p) = resolve(&l.path, file.layers_relative.get(i).and_then(|r| r.as_ref())) {
            l.path = p.to_string_lossy().to_string();
        }
    }
    Ok((photo, params))
}
