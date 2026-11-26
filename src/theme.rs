//! Colors, spacing and widget styling for the dark (default) and light themes.
//!
//! Everything that paints by hand should take its colors from [`pal`] so both themes stay
//! consistent; egui widgets pick theirs up from the [`egui::Style`] built here.

use eframe::egui;
use egui::{
    Color32, CornerRadius, FontId, Margin, Shadow, Stroke, TextStyle, Theme, ThemePreference,
    Visuals,
};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Tab strip and status bar (the darkest layer).
    pub crust: Color32,
    /// Toolbar and sidebar.
    pub mantle: Color32,
    /// Main content panels.
    pub base: Color32,
    /// Windows, popups and cards.
    pub raised: Color32,
    pub surface0: Color32,
    pub surface1: Color32,
    pub surface2: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    /// Text field background.
    pub input: Color32,

    pub text_strong: Color32,
    pub text: Color32,
    pub subtext: Color32,
    pub faint: Color32,

    pub accent: Color32,
    pub accent_hover: Color32,
    /// Text on an `accent` fill.
    pub on_accent: Color32,
    /// Selected row fill and its text.
    pub selection: Color32,
    pub sel_text: Color32,
    /// Hovered row fill.
    pub hover: Color32,
    /// Zebra stripe.
    pub stripe: Color32,

    pub green: Color32,
    pub red: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub purple: Color32,
    pub pink: Color32,
    pub teal: Color32,
    pub blue: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const DARK: Palette = Palette {
    dark: true,
    crust: rgb(12, 14, 19),
    mantle: rgb(17, 20, 27),
    base: rgb(22, 26, 34),
    raised: rgb(28, 33, 43),
    surface0: rgb(32, 37, 48),
    surface1: rgb(40, 47, 61),
    surface2: rgb(51, 59, 77),
    border: rgb(35, 41, 53),
    border_strong: rgb(55, 64, 82),
    input: rgb(14, 17, 23),

    text_strong: rgb(240, 243, 248),
    text: rgb(206, 212, 224),
    subtext: rgb(143, 152, 170),
    faint: rgb(98, 107, 125),

    accent: rgb(86, 136, 255),
    accent_hover: rgb(116, 158, 255),
    on_accent: rgb(255, 255, 255),
    selection: rgb(33, 52, 92),
    sel_text: rgb(240, 244, 255),
    hover: rgb(30, 35, 46),
    stripe: rgb(25, 29, 38),

    green: rgb(74, 203, 134),
    red: rgb(240, 98, 107),
    yellow: rgb(242, 196, 90),
    orange: rgb(245, 158, 75),
    purple: rgb(167, 139, 250),
    pink: rgb(244, 114, 182),
    teal: rgb(60, 198, 192),
    blue: rgb(96, 165, 250),
};

pub const LIGHT: Palette = Palette {
    dark: false,
    crust: rgb(226, 230, 237),
    mantle: rgb(241, 243, 247),
    base: rgb(252, 252, 254),
    raised: rgb(255, 255, 255),
    surface0: rgb(234, 237, 242),
    surface1: rgb(222, 227, 235),
    surface2: rgb(207, 214, 225),
    border: rgb(220, 224, 232),
    border_strong: rgb(194, 201, 213),
    input: rgb(255, 255, 255),

    text_strong: rgb(16, 19, 26),
    text: rgb(40, 45, 56),
    subtext: rgb(96, 104, 120),
    faint: rgb(140, 147, 161),

    accent: rgb(37, 99, 235),
    accent_hover: rgb(59, 118, 245),
    on_accent: rgb(255, 255, 255),
    selection: rgb(214, 227, 255),
    sel_text: rgb(18, 40, 100),
    hover: rgb(238, 241, 246),
    stripe: rgb(246, 247, 250),

    green: rgb(22, 150, 90),
    red: rgb(214, 58, 70),
    yellow: rgb(180, 130, 10),
    orange: rgb(210, 115, 25),
    purple: rgb(124, 92, 230),
    pink: rgb(214, 64, 140),
    teal: rgb(15, 145, 145),
    blue: rgb(37, 99, 235),
};

pub fn palette(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// The palette of the theme `ui` is painted with.
pub fn pal(ui: &egui::Ui) -> &'static Palette {
    palette(ui.visuals().dark_mode)
}

/// `c` with the given opacity — for tinted badges and highlights that work over any background.
pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Install both themes and pick the active one. The app's own choice wins over the OS
/// appearance (egui would otherwise follow the system theme and drop our dark style).
pub fn apply(ctx: &egui::Context, dark: bool) {
    ctx.set_style_of(Theme::Dark, style(&DARK));
    ctx.set_style_of(Theme::Light, style(&LIGHT));
    ctx.set_theme(if dark {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });
}

fn style(p: &Palette) -> egui::Style {
    let mut s = egui::Style {
        visuals: visuals(p),
        ..Default::default()
    };
    s.text_styles = [
        (TextStyle::Small, FontId::proportional(10.5)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Heading, FontId::proportional(19.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 4.0);
    sp.button_padding = egui::vec2(10.0, 4.0);
    sp.interact_size = egui::vec2(40.0, 22.0);
    sp.menu_margin = Margin::same(6);
    sp.window_margin = Margin::same(16);
    sp.indent = 16.0;
    sp.icon_spacing = 6.0;
    sp.scroll.bar_width = 8.0;
    sp.scroll.floating_allocated_width = 0.0;
    s
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let r6 = CornerRadius::same(6);

    v.panel_fill = p.base;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0, p.border_strong);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 14],
        blur: 36,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 150 } else { 45 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 120 } else { 35 }),
    };
    v.faint_bg_color = p.stripe;
    v.extreme_bg_color = p.input;
    v.text_edit_bg_color = Some(p.input);
    v.code_bg_color = p.surface0;
    v.hyperlink_color = p.accent_hover;
    v.warn_fg_color = p.yellow;
    v.error_fg_color = p.red;
    v.weak_text_color = Some(p.subtext);
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.striped = false;

    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, if p.dark { p.accent_hover } else { p.accent });

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.base;
    w.noninteractive.weak_bg_fill = p.base;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r6;

    w.inactive.weak_bg_fill = p.surface0;
    w.inactive.bg_fill = p.surface1;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r6;

    w.hovered.weak_bg_fill = p.surface1;
    w.hovered.bg_fill = p.surface1;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text_strong);
    w.hovered.corner_radius = r6;
    w.hovered.expansion = 0.0;

    w.active.weak_bg_fill = p.surface2;
    w.active.bg_fill = p.surface2;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(2.0, p.text_strong);
    w.active.corner_radius = r6;
    w.active.expansion = 0.0;

    w.open.weak_bg_fill = p.surface1;
    w.open.bg_fill = p.base;
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.open.fg_stroke = Stroke::new(1.0, p.text_strong);
    w.open.corner_radius = r6;
    v
}

// ---------------------------------------------------------------------------
// Small painted widgets shared by the views
// ---------------------------------------------------------------------------

/// Frame for a panel with the given fill.
pub fn panel(fill: Color32, x: i8, y: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .inner_margin(Margin::symmetric(x, y))
}

/// A filled call-to-action button (Commit, Clone, Sign in …).
pub fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let p = pal(ui);
    let btn = egui::Button::new(egui::RichText::new(text).strong().color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(0.0, 28.0));
    ui.add_enabled(enabled, btn)
}

/// Width of a count pill for `text`.
pub fn pill_width(painter: &egui::Painter, text: &str) -> f32 {
    let g = painter.layout_no_wrap(text.to_string(), FontId::proportional(10.5), Color32::WHITE);
    (g.size().x + 10.0).max(18.0)
}

/// A rounded count pill (e.g. "3" changed files) starting at `left`. Returns its width.
pub fn paint_pill(
    painter: &egui::Painter,
    left: f32,
    cy: f32,
    text: &str,
    fill: Color32,
    fg: Color32,
) -> f32 {
    let g = painter.layout_no_wrap(text.to_string(), FontId::proportional(10.5), fg);
    let w = (g.size().x + 10.0).max(18.0);
    let rect = egui::Rect::from_min_size(egui::pos2(left, cy - 8.0), egui::vec2(w, 16.0));
    painter.rect_filled(rect, 8.0, fill);
    painter.galley(
        egui::pos2(rect.center().x - g.size().x / 2.0, cy - g.size().y / 2.0),
        g,
        fg,
    );
    w
}

/// Flat toolbar button: accent icon + label, background only on hover. `badge` adds a count
/// pill; `selected` keeps the hover background (for toggles).
pub fn ghost_button(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    badge: usize,
    selected: bool,
) -> egui::Response {
    let p = pal(ui);
    let enabled = ui.is_enabled();
    let painter = ui.painter().clone();
    let ph = Color32::PLACEHOLDER;
    let icon_g = (!icon.is_empty())
        .then(|| painter.layout_no_wrap(icon.to_string(), FontId::proportional(15.0), ph));
    let label_g = (!label.is_empty())
        .then(|| painter.layout_no_wrap(label.to_string(), FontId::proportional(13.0), ph));
    let badge_text = if badge > 99 {
        "99+".to_string()
    } else {
        badge.to_string()
    };
    let (pad, gap) = (9.0, 6.0);
    let mut w = pad * 2.0;
    if let Some(g) = &icon_g {
        w += g.size().x;
    }
    if let Some(g) = &label_g {
        if icon_g.is_some() {
            w += gap;
        }
        w += g.size().x;
    }
    if badge > 0 {
        w += gap + pill_width(&painter, &badge_text);
    }
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if enabled && resp.is_pointer_button_down_on() {
            p.surface2
        } else if enabled && (resp.hovered() || selected) {
            p.surface1
        } else {
            Color32::TRANSPARENT
        };
        painter.rect_filled(rect, 7.0, bg);
        let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.4) };
        let cy = rect.center().y;
        let mut x = rect.left() + pad;
        if let Some(g) = icon_g {
            let gw = g.size().x;
            painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g, dim(p.accent_hover));
            x += gw + gap;
        }
        if let Some(g) = label_g {
            let gw = g.size().x;
            let c = if resp.hovered() && enabled {
                p.text_strong
            } else {
                p.text
            };
            painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g, dim(c));
            x += gw + gap;
        }
        if badge > 0 {
            paint_pill(&painter, x, cy, &badge_text, dim(p.accent), p.on_accent);
        }
    }
    resp
}

/// iOS-style segmented control. Returns true when the selection changed.
pub fn segmented(ui: &mut egui::Ui, options: &[&str], selected: &mut usize) -> bool {
    let p = pal(ui);
    let painter = ui.painter().clone();
    let font = FontId::proportional(13.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|o| {
            painter
                .layout_no_wrap(o.to_string(), font.clone(), p.text)
                .size()
                .x
                + 28.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, 30.0), egui::Sense::hover());
    painter.rect_filled(rect, 8.0, p.surface0);
    let mut changed = false;
    let mut x = rect.left() + 2.0;
    for (i, (o, w)) in options.iter().zip(&widths).enumerate() {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), egui::vec2(*w, 26.0));
        let r = ui.interact(
            seg,
            ui.scope_id().with(("segment", i)),
            egui::Sense::click(),
        );
        let on = *selected == i;
        if on {
            painter.rect_filled(seg, 6.0, p.surface2);
        } else if r.hovered() {
            painter.rect_filled(seg, 6.0, p.surface1);
        }
        painter.text(
            seg.center(),
            egui::Align2::CENTER_CENTER,
            *o,
            font.clone(),
            if on { p.text_strong } else { p.subtext },
        );
        if r.clicked() && !on {
            *selected = i;
            changed = true;
        }
        x += w;
    }
    changed
}

/// Small disclosure triangle (▸ closed / ▾ open) painted as a shape so it never depends on fonts.
pub fn paint_caret(painter: &egui::Painter, center: egui::Pos2, open: bool, color: Color32) {
    let s = 3.5;
    let pts = if open {
        vec![
            center + egui::vec2(-s, -s * 0.55),
            center + egui::vec2(s, -s * 0.55),
            center + egui::vec2(0.0, s * 0.75),
        ]
    } else {
        vec![
            center + egui::vec2(-s * 0.55, -s),
            center + egui::vec2(-s * 0.55, s),
            center + egui::vec2(s * 0.75, 0.0),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A stable color for a name (avatars, initials).
pub fn name_color(name: &str) -> Color32 {
    let p = &DARK;
    let colors = [
        p.blue, p.green, p.orange, p.purple, p.pink, p.teal, p.yellow, p.red,
    ];
    let h = name
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    colors[h as usize % colors.len()]
}

/// A round avatar with the first letter of `name`.
pub fn paint_avatar(painter: &egui::Painter, center: egui::Pos2, radius: f32, name: &str) {
    let c = name_color(name);
    painter.circle_filled(center, radius, c);
    let letter = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        letter,
        FontId::proportional(radius * 1.15),
        Color32::from_gray(18),
    );
}

/// Label for a Cmd (macOS) / Ctrl (elsewhere) shortcut, e.g. `shortcut("T")` → "⌘T" or "Ctrl+T".
pub fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

/// Branch glyph that exists in egui's bundled icon font (U+2442 "⑂" does not render on macOS).
pub const BRANCH: &str = "⎇";
