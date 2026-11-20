//! Finding a control by name: "where's the vignette?" The index lists every
//! slider, switch and button people look for, with where it lives and other
//! words they might type for it.

/// Where a control lives.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Where {
    /// A sidebar tab, by its label.
    Tab(&'static str),
    Settings,
    Toolbar,
    /// The bar at the bottom of the photo.
    Canvas,
    /// The Export sheet.
    Export,
}

#[derive(Debug)]
pub struct Place {
    /// The control's own label, as on screen.
    pub label: &'static str,
    pub at: Where,
    /// The section it sits in ("" when it's in none).
    pub section: &'static str,
    /// Other words for it.
    pub words: &'static str,
    /// Anything to know before it shows up (e.g. select a mask first).
    pub note: &'static str,
}

impl Place {
    /// "Effects › Optics"
    pub fn path(&self) -> String {
        let at = match self.at {
            Where::Tab(t) => t,
            Where::Settings => "Settings",
            Where::Toolbar => "Toolbar",
            Where::Canvas => "Bottom of the photo",
            Where::Export => "Export sheet",
        };
        if self.section.is_empty() { at.to_string() } else { format!("{at} › {}", self.section) }
    }
}

macro_rules! places {
    ($( $label:literal, $at:expr, $section:literal, $words:literal $(, note: $note:expr)? ;)*) => {
        &[$( Place { label: $label, at: $at, section: $section, words: $words, note: places!(@note $($note)?) }, )*]
    };
    (@note $n:expr) => { $n };
    (@note) => { "" };
}

use Where::{Canvas, Export, Settings, Tab, Toolbar};

const MASK: &str = "Select a mask in the list first.";

pub static PLACES: &[Place] = places![
    // --- Presets ---------------------------------------------------------------
    "Presets", Tab("Presets"), "", "looks styles filters film";
    "Save Preset", Tab("Presets"), "", "save look store my own new";
    "Import…", Tab("Presets"), "Share Presets", "import presets stillpreset friends load";
    "Export My Presets…", Tab("Presets"), "Share Presets", "share presets send export";

    // --- Adjust ----------------------------------------------------------------
    "Apply To", Tab("Adjust"), "", "mask local only part region selection different per mask adjustments";
    "Exposure", Tab("Adjust"), "Light", "brightness brighter darker lighten darken ev";
    "Contrast", Tab("Adjust"), "Light", "punch flat";
    "Highlights", Tab("Adjust"), "Light", "bright areas recover sky";
    "Shadows", Tab("Adjust"), "Light", "dark areas lift";
    "Whites", Tab("Adjust"), "Light", "white point levels";
    "Blacks", Tab("Adjust"), "Light", "black point levels crush";
    "Fade", Tab("Adjust"), "Light", "matte faded lifted blacks film";
    "Temperature", Tab("Adjust"), "Color", "white balance warm cool warmer cooler blue yellow";
    "Tint", Tab("Adjust"), "Color", "white balance green magenta";
    "Saturation", Tab("Adjust"), "Color", "colour colorful vivid desaturate";
    "Vibrance", Tab("Adjust"), "Color", "colour muted vivid";
    "Black & White", Tab("Adjust"), "Color", "monochrome mono grayscale greyscale bw b&w";
    "Split Tone", Tab("Adjust"), "", "split toning tint shadows highlights colour grade";
    "Shadow Hue", Tab("Adjust"), "Split Tone", "split toning shadows colour";
    "Highlight Hue", Tab("Adjust"), "Split Tone", "split toning highlights colour";
    "Sharpen", Tab("Adjust"), "Detail", "sharpness crisp detail clarity";
    "Soften", Tab("Adjust"), "Detail", "blur soft dreamy smooth";
    "Noise Reduction", Tab("Adjust"), "", "denoise grain grainy speckles high iso night low light clean";
    "Luminance", Tab("Adjust"), "Noise Reduction", "denoise grain grainy speckles high iso night luma brightness";
    "Detail", Tab("Adjust"), "Noise Reduction", "denoise texture keep fine grain";
    "Color", Tab("Adjust"), "Noise Reduction", "denoise chroma colour blotches mottling confetti speckles";

    // --- Effects ---------------------------------------------------------------
    "Bloom", Tab("Effects"), "Optics", "glow light dreamy";
    "Glow Threshold", Tab("Effects"), "Optics", "bloom glow which lights";
    "Halation", Tab("Effects"), "Optics", "red glow film cinestill";
    "Chromatic Aberration", Tab("Effects"), "Optics", "fringe fringing color edges lens";
    "Vignette", Tab("Effects"), "Optics", "dark corners edges darken lens";
    "Vignette Feather", Tab("Effects"), "Optics", "vignette soft corners";
    "Astigmatism", Tab("Effects"), "", "streaks stars cross lights rays flare night";
    "Grain", Tab("Effects"), "", "film grain noise texture";
    "CCD Noise", Tab("Effects"), "Sensor", "digital noise sensor";
    "Scanlines", Tab("Effects"), "Sensor", "crt lines tv monitor retro";
    "Downsample", Tab("Effects"), "Resolution", "low resolution pixelate pixel blocky lowres";
    "Smooth Upscale", Tab("Effects"), "Resolution", "low resolution soft";
    "Posterize", Tab("Effects"), "", "levels colours banding";
    "Ordered Dither", Tab("Effects"), "Posterize", "dithering bayer pattern";

    // --- Glitch ----------------------------------------------------------------
    "Shuffle", Tab("Glitch"), "", "main seed random new arrangement reshuffle";
    "Random", Tab("Glitch"), "Grain", "grain seed own pin arrangement copy paste use main";
    "Pixel Sort", Tab("Glitch"), "", "sorting pixels smear glitch";
    "Data Streaks", Tab("Glitch"), "", "streaks corrupt glitch lines";
    "Slice Shift", Tab("Glitch"), "", "slices offset tear glitch";
    "RGB Split", Tab("Glitch"), "", "channel shift rgb chromatic glitch";
    "Compression", Tab("Glitch"), "", "jpeg blocks artefacts artifacts glitch";

    // --- Masks -----------------------------------------------------------------
    "+ Linear", Tab("Masks"), "", "gradient graduated filter mask sky";
    "+ Radial", Tab("Masks"), "", "ellipse circle oval spot mask";
    "+ Brush", Tab("Masks"), "", "paint draw mask";
    "+ Object", Tab("Masks"), "", "subject select selection person mask";
    "+ Luminance", Tab("Masks"), "", "brightness range mask";
    "+ Color", Tab("Masks"), "", "colour range hue mask";
    "Invert", Tab("Masks"), "Shape", "mask reverse opposite", note: MASK;
    "Show Overlay", Tab("Masks"), "Shape", "red mask overlay see selection", note: MASK;
    "Range", Tab("Masks"), "", "mask luminance colour refine limit", note: MASK;
    "Make Transparent", Tab("Masks"), "Cut Out", "cut out transparent remove background erase png", note: MASK;
    "Adjustments", Tab("Masks"), "", "mask local edits exposure", note: MASK;
    "Delete Mask", Tab("Masks"), "", "remove mask", note: MASK;

    // --- Retouch ---------------------------------------------------------------
    "Clone", Tab("Retouch"), "", "clone stamp heal remove spot object dust retouch";

    // --- Layers ----------------------------------------------------------------
    "Add Photo…", Tab("Layers"), "", "layer overlay texture light leak picture image add";
    "Add Text", Tab("Layers"), "", "text title caption words typography letters write label";
    "Font", Tab("Layers"), "Text", "typeface typography text size weight bold italic", note: "Select a text layer in the list first.";
    "Align", Tab("Layers"), "Text", "text alignment left centre center right", note: "Select a text layer in the list first.";
    "Text Color", Tab("Layers"), "", "text colour outline shadow stroke", note: "Select a text layer in the list first.";
    "Gradient", Tab("Layers"), "Text Color", "text colour gradient", note: "Select a text layer in the list first.";
    "Text Shape", Tab("Layers"), "", "text arc wave curve bend", note: "Select a text layer in the list first.";
    "Handles", Tab("Layers"), "Transform", "perspective corners mesh warp bend distort", note: "Select a layer in the list first.";
    "Blending", Tab("Layers"), "", "blend mode multiply screen overlay opacity layer", note: "Select a layer in the list first.";
    "Behind Photo", Tab("Layers"), "Blending", "background replace sky layer underneath", note: "Select a layer in the list first.";
    "Apply Preset to Layer", Tab("Layers"), "Blending", "graded layer look", note: "Select a layer in the list first.";
    "Transform", Tab("Layers"), "", "layer position size rotation move scale", note: "Select a layer in the list first.";
    "Magnet", Tab("Layers"), "Transform", "snap snapping align layers", note: "Select a layer in the list first.";
    "Auto Pivot", Tab("Layers"), "Transform", "magnet rotate turn match", note: "Select a layer in the list first.";
    "Stretch", Tab("Layers"), "Transform", "fill canvas fit layer", note: "Select a layer in the list first.";

    // --- Crop ------------------------------------------------------------------
    "Crop", Tab("Crop"), "", "aspect ratio square 16:9 trim cut frame";
    "Straighten", Tab("Crop"), "Orientation", "rotate tilt level angle crooked";
    "Horizon Line", Tab("Crop"), "Orientation", "level straighten horizon";
    "Level Photo", Tab("Crop"), "Orientation", "horizon straighten";
    "Rotate Left", Tab("Crop"), "Orientation", "rotate turn 90";
    "Flip Horizontal", Tab("Crop"), "Orientation", "mirror flip";
    "Border", Tab("Crop"), "", "frame white black edge";
    "Rule of Thirds", Tab("Crop"), "Guides", "grid guides composition";
    "Golden Ratio", Tab("Crop"), "Guides", "grid guides composition spiral";

    // --- Settings --------------------------------------------------------------
    "Quality", Settings, "", "preview quality resolution sharp fast detailed";
    "Canvas", Settings, "", "background colour black gray grey";
    "Mask Overlay", Settings, "", "overlay colour red green blue";
    "Histogram", Settings, "", "rgb luminance graph";
    "Show Render Time", Settings, "", "speed ms performance";
    "Reduce Motion", Settings, "", "animation accessibility";
    "Keep Preset When Opening a Photo", Settings, "", "batch series same look workflow";
    "Native Wayland", Settings, "", "linux xwayland drag drop";
    "Import Presets…", Settings, "", "import presets";
    "Show Settings Folder", Settings, "", "config folder files location";
    "Pixel View", Settings, "", "pixels nearest sharp zoom pixel art";
    "Pixel Grid", Settings, "", "pixels grid zoom";
    "Check for Updates", Settings, "", "update new version upgrade";
    "Install Updates Automatically", Settings, "", "update auto upgrade";
    "New Seed Every Frame", Export, "", "animation gif frames grain glitch reseed";
    "Output Sharpening", Export, "", "sharpen export";
    "Keep EXIF Metadata", Export, "", "exif metadata camera info";

    // --- Toolbar and canvas ----------------------------------------------------
    "Photos", Toolbar, "", "browser folder mark cull trash sort keepers";
    "Open", Toolbar, "", "open file load photo project ctrl+o";
    "Save", Toolbar, "", "project save still ctrl+s";
    "Undo", Toolbar, "", "back ctrl+z";
    "Redo", Toolbar, "", "ctrl+shift+z";
    "Revert", Toolbar, "", "reset all start over";
    "Export", Toolbar, "", "save jpeg png tiff webp output ctrl+e";
    "Compare", Canvas, "", "before after original backslash", note: "Hold it, or hold \\.";
    "Fit", Canvas, "", "zoom fit ctrl+0";
];

/// Edit distance with adjacent swaps, stopping early past `max`.
fn distance(a: &[char], b: &[char], max: usize) -> usize {
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
        }
        if cur.iter().min().copied().unwrap_or(0) > max {
            return max + 1;
        }
        prev2 = std::mem::replace(&mut prev, cur);
    }
    prev[b.len()]
}

/// How well `token` matches `word`: 0 exact start, 1 inside, 2 with a typo, None not at all.
fn word_match(token: &str, word: &str) -> Option<u8> {
    if word.starts_with(token) {
        return Some(0);
    }
    if token.len() >= 3 && word.contains(token) {
        return Some(1);
    }
    let t: Vec<char> = token.chars().collect();
    if t.len() < 4 {
        return None;
    }
    let max = if t.len() >= 7 { 2 } else { 1 };
    let w: Vec<char> = word.chars().collect();
    // the whole word, or its start while the rest is still being typed
    let whole = distance(&t, &w, max) <= max;
    let start = w.len() > t.len() && distance(&t, &w[..t.len()], max) <= max;
    (whole || start).then_some(2)
}

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !(c.is_alphanumeric() || c == '&' || c == '+' || c == ':')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase())
}

/// The places matching `query`, best first. Every typed word has to match.
pub fn search(query: &str, linux: bool) -> Vec<&'static Place> {
    let tokens: Vec<String> = words(query).collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u32, usize, &Place)> = Vec::new();
    for (order, p) in PLACES.iter().enumerate() {
        if p.label == "Native Wayland" && !linux {
            continue;
        }
        let label: Vec<String> = words(p.label).collect();
        let tab = match p.at {
            Tab(t) => t,
            Settings => "settings",
            Toolbar => "toolbar",
            Canvas => "",
            Export => "export",
        };
        let other: Vec<String> = words(p.section).chain(words(tab)).chain(words(p.words)).collect();
        let mut score = 0u32;
        let mut ok = true;
        for t in &tokens {
            // the label counts more than the extra words
            let best_label = label.iter().filter_map(|w| word_match(t, w)).min().map(|m| m as u32);
            let best_other = other.iter().filter_map(|w| word_match(t, w)).min().map(|m| m as u32 + 4);
            match best_label.into_iter().chain(best_other).min() {
                Some(s) => score += s,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            hits.push((score, order, p));
        }
    }
    hits.sort_by_key(|h| (h.0, h.1));
    hits.into_iter().map(|h| h.2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(q: &str) -> &'static str {
        search(q, true).first().map(|p| p.label).unwrap_or("")
    }

    #[test]
    fn finds_by_name_other_words_and_typos() {
        assert_eq!(first("vignette"), "Vignette");
        assert_eq!(first("Vign"), "Vignette");
        assert_eq!(first("vinette"), "Vignette");
        assert_eq!(first("dark corners"), "Vignette");
        assert_eq!(first("white balance"), "Temperature");
        assert_eq!(first("contrst"), "Contrast");
        assert_eq!(first("remove background"), "Make Transparent");
        assert_eq!(first("noise"), "Noise Reduction");
        assert_eq!(first("denoise"), "Noise Reduction");
        assert_eq!(first("grain"), "Grain");
        assert_eq!(search("colour noise", true)[0].path(), "Adjust › Noise Reduction");
        assert_eq!(search("vignette", true)[0].path(), "Effects › Optics");
        assert!(search("zzzz", true).is_empty());
        assert!(search("   ", true).is_empty());
        assert!(search("wayland", false).is_empty());
    }
}
