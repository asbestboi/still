//! A small set of Apple-flavoured widgets for egui: springs, sliders,
//! segmented controls, switches, pill buttons and grouped sections.
//!
//! Principles followed (from Apple's fluid-interface guidance):
//! - feedback on pointer *down*, never only on release
//! - direct manipulation that tracks 1:1 while dragging
//! - critically-damped springs that start from the current on-screen value,
//!   so every transition is interruptible

use egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};

// ---------------------------------------------------------------------------
// Palette: Apple dark-mode system colours
// ---------------------------------------------------------------------------

pub const CANVAS: Color32 = Color32::from_rgb(10, 10, 11);
pub const WINDOW: Color32 = Color32::from_rgb(28, 28, 30);
pub const SIDEBAR: Color32 = Color32::from_rgb(22, 22, 24);
pub const CARD: Color32 = Color32::from_rgb(36, 36, 38);
pub const FILL: Color32 = Color32::from_rgb(48, 48, 52);
pub const FILL_HI: Color32 = Color32::from_rgb(72, 72, 76);
pub const SEPARATOR: Color32 = Color32::from_rgb(44, 44, 46);
pub const LABEL: Color32 = Color32::from_rgb(245, 245, 247);
pub const LABEL2: Color32 = Color32::from_rgb(152, 152, 159);
pub const LABEL3: Color32 = Color32::from_rgb(99, 99, 104);
pub const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn font_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn font_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("tabular".into()))
}

// ---------------------------------------------------------------------------
// Fonts: prefer a neo-grotesque close to SF Pro, from the system.
// ---------------------------------------------------------------------------

/// Bundled so the interface looks the same on every system (Adwaita Sans,
/// an Inter derivative, SIL Open Font License: see assets/fonts/OFL.txt).
/// It is also the default font of text layers.
pub static UI_FONT: &[u8] = include_bytes!("../assets/fonts/AdwaitaSans-Regular.ttf");

pub fn setup_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontData, FontDefinitions, VariationCoords};
    use std::sync::Arc;

    let mut defs = FontDefinitions::default();
    let fallbacks = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut regular = FontData::from_static(UI_FONT);
    regular.tweak.coords = VariationCoords::new([(b"wght", 420.0)]);
    defs.font_data.insert("ui".into(), Arc::new(regular));
    let mut semi = FontData::from_static(UI_FONT);
    semi.tweak.coords = VariationCoords::new([(b"wght", 620.0)]);
    defs.font_data.insert("ui-semibold".into(), Arc::new(semi));
    let regular_name = Some("ui".to_string());
    let semibold_name = Some("ui-semibold".to_string());

    let mut prop = fallbacks.clone();
    if let Some(r) = &regular_name {
        prop.insert(0, r.clone());
    }
    defs.families.insert(FontFamily::Proportional, prop.clone());

    let mut semi = fallbacks.clone();
    if let Some(s) = &semibold_name {
        semi.insert(0, s.clone());
    }
    defs.families.insert(FontFamily::Name("semibold".into()), semi);

    // numbers: use the monospace face so values don't jitter while dragging
    let mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    defs.families.insert(FontFamily::Name("tabular".into()), mono);

    ctx.set_fonts(defs);
}

pub fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = WINDOW;
        v.window_fill = CARD;
        v.extreme_bg_color = Color32::from_rgb(18, 18, 20);
        v.faint_bg_color = CARD;
        v.override_text_color = Some(LABEL);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_stroke = Stroke::new(1.0, Color32::from_white_alpha(18));
        v.popup_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(110) };
        v.window_shadow = v.popup_shadow;
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
            w.bg_stroke = Stroke::NONE;
        }
        v.widgets.inactive.weak_bg_fill = FILL;
        v.widgets.inactive.bg_fill = FILL;
        v.widgets.hovered.weak_bg_fill = FILL_HI;
        v.widgets.hovered.bg_fill = FILL_HI;
        v.widgets.active.weak_bg_fill = FILL_HI;
        v.widgets.active.bg_fill = FILL_HI;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEPARATOR);
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT);

        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(12.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.scroll.floating = true;
        style.spacing.scroll.bar_width = 6.0;
        style.text_styles.insert(egui::TextStyle::Body, font(13.0));
        style.text_styles.insert(egui::TextStyle::Button, font(13.0));
        style.text_styles.insert(egui::TextStyle::Small, font(11.0));
        style.text_styles.insert(egui::TextStyle::Heading, font_semibold(17.0));
        style.animation_time = 0.18;
    });
}

// ---------------------------------------------------------------------------
// Springs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SpringState {
    pos: f32,
    vel: f32,
}

/// Animate towards `target` with a spring described by Apple's two designer
/// parameters: `damping` (1.0 = critically damped) and `response` (seconds).
/// Always continues from the current on-screen value, so it is interruptible.
pub fn spring(ctx: &egui::Context, id: Id, target: f32, damping: f32, response: f32) -> f32 {
    let dt = ctx.input(|i| i.stable_dt).min(1.0 / 20.0);
    let mut s = ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(id, || SpringState { pos: target, vel: 0.0 }));
    let k = (std::f32::consts::TAU / response.max(0.01)).powi(2);
    let c = 4.0 * std::f32::consts::PI * damping / response.max(0.01);
    let steps = (dt / (1.0 / 240.0)).ceil().max(1.0) as usize;
    let h = dt / steps as f32;
    for _ in 0..steps {
        let a = -k * (s.pos - target) - c * s.vel;
        s.vel += a * h;
        s.pos += s.vel * h;
    }
    let scale = target.abs().max(1.0);
    if (s.pos - target).abs() < 0.0005 * scale && s.vel.abs() < 0.005 * scale {
        s.pos = target;
        s.vel = 0.0;
    } else {
        ctx.request_repaint();
    }
    ctx.data_mut(|d| d.insert_temp(id, s));
    s.pos
}

/// Jump a spring to a value immediately (e.g. while dragging 1:1).
pub fn spring_set(ctx: &egui::Context, id: Id, value: f32, velocity: f32) {
    ctx.data_mut(|d| d.insert_temp(id, SpringState { pos: value, vel: velocity }));
}

// ---------------------------------------------------------------------------
// Slider
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Fmt {
    /// -1..1 shown as -100..+100
    Signed,
    /// 0..1 shown as 0..100
    Amount,
    /// exposure stops
    Ev,
    /// degrees
    Deg,
    /// integer pixels
    Px,
    /// a plain number with two decimals
    Num,
}

fn format_value(v: f32, fmt: Fmt) -> String {
    match fmt {
        Fmt::Signed => {
            let n = (v as f64 * 100.0).round() as i64;
            if n > 0 { format!("+{n}") } else { format!("{n}") }
        }
        Fmt::Amount => format!("{}", (v as f64 * 100.0).round() as i64),
        Fmt::Ev => {
            if v.abs() < 0.005 { "0.00".into() } else { format!("{v:+.2}") }
        }
        Fmt::Deg => format!("{v:.1}°"),
        Fmt::Px => format!("{}", v.round() as i64),
        Fmt::Num => format!("{v:.2}"),
    }
}

/// The number as it appears in the text field when typing a value.
fn edit_text(v: f32, fmt: Fmt) -> String {
    let shown = match fmt {
        Fmt::Signed | Fmt::Amount => v as f64 * 100.0,
        _ => v as f64,
    };
    let s = format!("{:.2}", shown);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// The largest number (as shown) that may be typed into a slider.
pub const TYPED_LIMIT: f64 = 1e9;

/// A typed value back to the slider's units. Accepts a comma for the decimal
/// point, and ignores `+`, `%` and `°`. Any finite number is allowed: typing
/// is how values beyond the slider's range are reached.
pub fn parse_typed(s: &str, fmt: Fmt) -> Option<f32> {
    let t: String = s.trim().chars().filter(|c| !matches!(c, '°' | '%' | '+' | ' ' | '\u{a0}')).map(|c| if c == ',' { '.' } else { c }).collect();
    let n: f64 = t.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    let n = n.clamp(-TYPED_LIMIT, TYPED_LIMIT);
    Some(match fmt {
        Fmt::Signed | Fmt::Amount => n / 100.0,
        _ => n,
    } as f32)
}

/// A small text field for typing a number. Returns `Some(text)` once Enter is
/// pressed or the field loses focus, `None` while editing; Escape cancels
/// (`Some(None)` from the outer option).
fn number_field(ui: &mut Ui, rect: Rect, id: Id, buf: &mut String, first: bool) -> Option<Option<String>> {
    let te = egui::TextEdit::singleline(buf)
        .id(id)
        .font(font_mono(11.5))
        .align(egui::Align2::RIGHT_CENTER)
        .margin(egui::Margin::symmetric(4, 2))
        .desired_width(rect.width());
    let out = ui.put(rect, te);
    if first {
        out.request_focus();
        // start with the whole number selected, so typing replaces it
        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id) {
            let n = buf.chars().count();
            st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
            st.store(ui.ctx(), id);
        }
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && (out.has_focus() || out.lost_focus()) {
        return Some(None);
    }
    if out.lost_focus() {
        return Some(Some(buf.clone()));
    }
    None
}

pub fn slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
    fmt: Fmt,
) -> Response {
    let (lo, hi) = (*range.start(), *range.end());
    let width = ui.available_width();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 40.0), Sense::click_and_drag());
    let id = resp.id;
    let ctx = ui.ctx().clone();

    // The number on the right: click it to type any value, also far beyond
    // the slider's range. Registered after the slider, so it takes the click.
    let shown = format_value(*value, fmt);
    let text_w = ui.painter().layout_no_wrap(shown.clone(), font_mono(11.5), LABEL).size().x;
    let val_rect = Rect::from_min_max(pos2(rect.right() - text_w.max(30.0) - 8.0, rect.top()), pos2(rect.right() + 2.0, rect.top() + 22.0));
    let edit_id = id.with("typed");
    let mut editing: Option<String> = ctx.data(|d| d.get_temp::<String>(edit_id));
    let mut first = false;
    // it also takes drags, or egui would hand a press here to the slider underneath
    let val_resp = ui.interact(val_rect, id.with("value"), Sense::click_and_drag());
    if editing.is_none() && val_resp.clicked() {
        editing = Some(edit_text(*value, fmt));
        first = true;
    }
    if val_resp.hovered() && editing.is_none() {
        ctx.set_cursor_icon(egui::CursorIcon::Text);
    }
    if let Some(mut buf) = editing {
        let field = Rect::from_min_max(pos2(rect.right() - 86.0, rect.top() + 1.0), pos2(rect.right() + 2.0, rect.top() + 21.0));
        match number_field(ui, field, edit_id.with("field"), &mut buf, first) {
            None => ctx.data_mut(|d| {
                d.insert_temp(edit_id, buf);
            }),
            Some(done) => {
                ctx.data_mut(|d| d.remove::<String>(edit_id));
                if let Some(v) = done.and_then(|t| parse_typed(&t, fmt))
                    && v != *value
                {
                    *value = v;
                    resp.mark_changed();
                }
            }
        }
    }
    let editing = ctx.data(|d| d.get_temp::<String>(edit_id)).is_some();

    let track = Rect::from_min_max(pos2(rect.left() + 2.0, rect.top() + 28.0), pos2(rect.right() - 2.0, rect.top() + 32.0));
    // a typed value past either end parks the knob at that end
    let to_x = |v: f32| track.left() + ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * track.width();
    let from_x = |x: f32| lo + ((x - track.left()) / track.width()).clamp(0.0, 1.0) * (hi - lo);

    let fine = ui.input(|i| i.modifiers.shift);
    if resp.double_clicked() {
        *value = default;
        resp.mark_changed();
    } else if resp.is_pointer_button_down_on() {
        if let Some(p) = resp.interact_pointer_pos() {
            if fine {
                let dx = ui.input(|i| i.pointer.delta().x);
                // fine steps keep a typed out-of-range value instead of snapping it back
                let v = *value + dx / track.width() * (hi - lo) * 0.1;
                *value = if (lo..=hi).contains(value) { v.clamp(lo, hi) } else { v };
            } else {
                // direct manipulation: the knob is glued to the pointer
                let nv = from_x(p.x);
                // soft detent at the default value
                let snap = (hi - lo) * 0.012;
                *value = if (nv - default).abs() < snap { default } else { nv };
            }
            resp.mark_changed();
        }
    }

    let active = resp.is_pointer_button_down_on();
    let modified = (*value - default).abs() > 1e-4;
    let beyond = *value < lo - 1e-6 || *value > hi + 1e-6;
    let painter = ui.painter_at(rect.expand(6.0));

    // label + value
    painter.text(
        pos2(rect.left() + 2.0, rect.top() + 11.0),
        Align2::LEFT_CENTER,
        label,
        font(13.0),
        if modified || active { LABEL } else { LABEL2 },
    );
    if !editing {
        if val_resp.hovered() {
            painter.rect_filled(val_rect.shrink2(vec2(2.0, 2.0)), CornerRadius::same(5), FILL);
        }
        painter.text(
            pos2(rect.right() - 2.0, rect.top() + 11.0),
            Align2::RIGHT_CENTER,
            format_value(*value, fmt),
            font_mono(11.5),
            // beyond the slider's range (typed in): shown in the accent colour
            if beyond { ACCENT } else if active { LABEL } else if modified { LABEL2 } else { LABEL3 },
        );
    }
    val_resp.on_hover_text_at_pointer("Click to type a value (any number, also beyond the slider)");

    // track
    let r = CornerRadius::same(2);
    painter.rect_filled(track, r, FILL);
    let anchor = default.clamp(lo, hi);
    let (a, b) = (to_x(anchor), to_x(*value));
    let fill = Rect::from_min_max(pos2(a.min(b), track.top()), pos2(a.max(b), track.bottom()));
    painter.rect_filled(fill, r, if active { LABEL } else { Color32::from_gray(200) });
    if anchor > lo && anchor < hi {
        let x = to_x(anchor);
        painter.line_segment([pos2(x, track.top() - 3.0), pos2(x, track.bottom() + 3.0)], Stroke::new(1.0, LABEL3));
    }

    // knob: grows on press (feedback on pointer down), springs back
    let grow = spring(&ctx, id.with("k"), if active { 1.0 } else { 0.0 }, 1.0, 0.25);
    let hover = resp.hovered() as i32 as f32;
    let radius = 6.5 + grow * 2.0 + hover * 0.5;
    let c = pos2(b, track.center().y);
    painter.circle_filled(c + vec2(0.0, 1.0), radius + 1.5, Color32::from_black_alpha(70));
    painter.circle_filled(c, radius, Color32::WHITE);

    if resp.hovered() || active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text_at_pointer("Double-click to reset · Shift for fine control")
}

// ---------------------------------------------------------------------------
// Segmented control
// ---------------------------------------------------------------------------

pub fn segmented(ui: &mut Ui, id_salt: &str, selected: &mut usize, labels: &[&str]) -> bool {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
    let id = ui.scope_id().with(id_salt);
    let n = labels.len().max(1);
    let seg_w = (rect.width() - 4.0) / n as f32;
    let mut changed = false;

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(8), FILL);

    // thumb, spring-animated from wherever it currently is
    let x = spring(ui.ctx(), id.with("thumb"), *selected as f32, 1.0, 0.32);
    let thumb = Rect::from_min_size(pos2(rect.left() + 2.0 + x * seg_w, rect.top() + 2.0), vec2(seg_w, rect.height() - 4.0));
    painter.rect_filled(thumb.translate(vec2(0.0, 1.0)), CornerRadius::same(7), Color32::from_black_alpha(60));
    painter.rect_filled(thumb, CornerRadius::same(7), Color32::from_rgb(99, 99, 104));

    for (i, l) in labels.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + 2.0 + i as f32 * seg_w, rect.top()), vec2(seg_w, rect.height()));
        let resp = ui.interact(r, id.with(i), Sense::click());
        let pressed = resp.is_pointer_button_down_on();
        if resp.clicked() && *selected != i {
            *selected = i;
            changed = true;
        }
        // separators between unselected neighbours (as in UISegmentedControl)
        if i > 0 && i != *selected && i != *selected + 1 {
            let sx = r.left();
            painter.line_segment([pos2(sx, r.top() + 8.0), pos2(sx, r.bottom() - 8.0)], Stroke::new(1.0, FILL_HI));
        }
        let sel = i == *selected;
        let color = if sel { LABEL } else if pressed { LABEL3 } else { LABEL2 };
        // narrow segments (e.g. seven tabs) get slightly smaller type rather than touching labels
        let size = if seg_w < 46.0 { 11.0 } else { 12.0 };
        painter.text(r.center(), Align2::CENTER_CENTER, *l, if sel { font_semibold(size) } else { font(size) }, color);
    }
    changed
}

// ---------------------------------------------------------------------------
// Switch
// ---------------------------------------------------------------------------

pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = spring(ui.ctx(), resp.id, if *on { 1.0 } else { 0.0 }, 1.0, 0.3);
    let painter = ui.painter_at(rect.expand(3.0));
    let track_col = lerp_color(Color32::from_rgb(57, 57, 61), ACCENT, t);
    painter.rect_filled(rect, CornerRadius::same(11), track_col);
    let pressed = resp.is_pointer_button_down_on();
    // knob stretches a little on press, like iOS
    let kw = 18.0 + if pressed { 4.0 } else { 0.0 };
    let x = rect.left() + 2.0 + t * (rect.width() - 4.0 - kw);
    let knob = Rect::from_min_size(pos2(x, rect.top() + 2.0), vec2(kw, 18.0));
    painter.rect_filled(knob.translate(vec2(0.0, 1.0)), CornerRadius::same(9), Color32::from_black_alpha(60));
    painter.rect_filled(knob, CornerRadius::same(9), Color32::WHITE);
    resp
}

pub fn switch_row(ui: &mut Ui, label: &str, on: &mut bool) -> Response {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(egui::RichText::new(label).color(if *on { LABEL } else { LABEL2 }).size(13.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            out = Some(switch(ui, on));
        });
    });
    out.unwrap()
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Btn {
    Primary,
    Secondary,
    Plain,
}

pub fn button(ui: &mut Ui, text: &str, kind: Btn, enabled: bool) -> Response {
    let f = if kind == Btn::Primary { font_semibold(13.0) } else { font(13.0) };
    let galley = ui.painter().layout_no_wrap(text.to_string(), f, LABEL);
    let pad = if kind == Btn::Plain { 8.0 } else { 14.0 };
    let size = vec2(galley.size().x + pad * 2.0, 28.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let pressed = enabled && resp.is_pointer_button_down_on();
    let hovered = enabled && resp.hovered();
    // pressed state appears instantly on pointer-down; release eases back
    let p = spring(ui.ctx(), resp.id.with("p"), if pressed { 1.0 } else { 0.0 }, 1.0, 0.18);
    let r = rect.shrink(p * 0.6);
    let painter = ui.painter();
    let (bg, fg) = match kind {
        Btn::Primary => (lerp_color(ACCENT, Color32::from_rgb(0, 100, 210), p), Color32::WHITE),
        Btn::Secondary => (
            lerp_color(if hovered { FILL_HI } else { FILL }, Color32::from_rgb(90, 90, 96), p),
            LABEL,
        ),
        Btn::Plain => (
            if hovered || p > 0.01 { Color32::from_white_alpha((10.0 + p * 18.0) as u8) } else { Color32::TRANSPARENT },
            if kind == Btn::Plain { LABEL } else { LABEL },
        ),
    };
    let (bg, fg) = if enabled { (bg, fg) } else { (bg.gamma_multiply(0.5), LABEL3) };
    painter.rect_filled(r, CornerRadius::same(14), bg);
    let center = r.center();
    painter.galley(center - galley.size() / 2.0, galley, fg);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

// ---------------------------------------------------------------------------
// Grouped section ("inset grouped" card)
// ---------------------------------------------------------------------------

/// Returns true if "Reset" was clicked.
pub fn section(ui: &mut Ui, title: &str, modified: bool, add: impl FnOnce(&mut Ui)) -> bool {
    let id = ui.make_persistent_id(("section", title));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
    let mut reset = false;

    egui::Frame::NONE
        .fill(CARD)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin { left: 12, right: 12, top: 4, bottom: 6 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // header
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let open = state.is_open();
            let t = spring(ui.ctx(), id.with("chev"), if open { 1.0 } else { 0.0 }, 1.0, 0.3);
            paint_chevron(ui, pos2(rect.right() - 6.0, rect.center().y), t);
            ui.painter().text(
                pos2(rect.left(), rect.center().y),
                Align2::LEFT_CENTER,
                title,
                font_semibold(13.0),
                LABEL,
            );
            if modified {
                let dot = pos2(rect.left() + text_width(ui, title, font_semibold(13.0)) + 8.0, rect.center().y);
                ui.painter().circle_filled(dot, 2.5, ACCENT);
                // reset link placed left of the chevron
                let lr = Rect::from_min_size(pos2(rect.right() - 62.0, rect.top() + 6.0), vec2(40.0, 20.0));
                let r = ui.interact(lr, id.with("reset"), Sense::click());
                let col = if r.is_pointer_button_down_on() { ACCENT.gamma_multiply(0.6) } else { ACCENT };
                ui.painter().text(lr.right_center(), Align2::RIGHT_CENTER, "Reset", font(12.0), col);
                if r.clicked() {
                    reset = true;
                } else if resp.clicked() {
                    state.toggle(ui);
                }
            } else if resp.clicked() {
                state.toggle(ui);
            }
            state.show_body_unindented(ui, |ui| {
                ui.add_space(2.0);
                add(ui);
            });
        });
    ui.add_space(10.0);
    reset
}

fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
    ui.painter().layout_no_wrap(text.to_string(), font, LABEL).size().x
}

/// Chevron that rotates from "›" (closed) to "⌄" (open).
fn paint_chevron(ui: &Ui, c: Pos2, t: f32) {
    let ang = t * std::f32::consts::FRAC_PI_2;
    let rot = |v: Vec2| vec2(v.x * ang.cos() - v.y * ang.sin(), v.x * ang.sin() + v.y * ang.cos());
    let a = c + rot(vec2(-2.0, -4.0));
    let b = c + rot(vec2(2.0, 0.0));
    let d = c + rot(vec2(-2.0, 4.0));
    let s = Stroke::new(1.6, LABEL3);
    ui.painter().line_segment([a, b], s);
    ui.painter().line_segment([b, d], s);
}

pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(11.0).color(LABEL3));
}

pub fn stroke_rect(ui: &Ui, rect: Rect, r: u8, color: Color32) {
    ui.painter().rect_stroke(rect, CornerRadius::same(r), Stroke::new(1.0, color), StrokeKind::Inside);
}

/// A selectable rounded chip (aspect presets, mask types). Highlights on press.
pub fn chip(ui: &mut Ui, size: egui::Vec2, label: &str, selected: bool) -> Response {
    let (r, resp) = ui.allocate_exact_size(size, Sense::click());
    let pressed = resp.is_pointer_button_down_on();
    let bg = if selected {
        Color32::from_rgb(99, 99, 104)
    } else if pressed {
        FILL_HI
    } else {
        FILL
    };
    ui.painter().rect_filled(r, CornerRadius::same(8), bg);
    ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        label,
        if selected { font_semibold(12.0) } else { font(12.0) },
        if selected { LABEL } else { LABEL2 },
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_values_go_beyond_the_slider() {
        assert_eq!(parse_typed("250", Fmt::Amount), Some(2.5));
        assert_eq!(parse_typed("-1000", Fmt::Signed), Some(-10.0));
        assert_eq!(parse_typed("+12,5", Fmt::Ev), Some(12.5));
        assert_eq!(parse_typed("720°", Fmt::Deg), Some(720.0));
        assert_eq!(parse_typed("1e4", Fmt::Px), Some(10000.0));
        assert_eq!(parse_typed("1e99", Fmt::Px), Some(TYPED_LIMIT as f32));
        assert_eq!(parse_typed("nan", Fmt::Px), None);
        assert_eq!(parse_typed("abc", Fmt::Px), None);
        assert_eq!(edit_text(0.25, Fmt::Amount), "25");
        assert_eq!(edit_text(1.5, Fmt::Ev), "1.5");
        assert_eq!(format_value(250.0, Fmt::Amount), "25000");
    }
}
