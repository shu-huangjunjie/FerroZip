//! Fonts, colours and icons. Text uses Microsoft YaHei UI and icons come from
//! the system Segoe Fluent Icons / MDL2 font, so nothing is bundled.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, RichText, Stroke, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(0x1F, 0x6F, 0xEB);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xDC, 0xEA, 0xFD);
pub const FOLDER: Color32 = Color32::from_rgb(0xE8, 0xA8, 0x2E);
pub const MUTED: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
pub const DANGER: Color32 = Color32::from_rgb(0xD1, 0x34, 0x38);

/// Segoe Fluent Icons / Segoe MDL2 Assets code points.
pub mod icon {
    pub const OPEN: &str = "\u{E8E5}";
    pub const EXTRACT: &str = "\u{E896}";
    pub const NEW: &str = "\u{E7B8}";
    pub const TEST: &str = "\u{E9D5}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const INFO: &str = "\u{E946}";
    pub const BACK: &str = "\u{E72B}";
    pub const UP: &str = "\u{E74A}";
    pub const SEARCH: &str = "\u{E721}";
    pub const FOLDER: &str = "\u{E8B7}";
    pub const FILE: &str = "\u{E8A5}";
    pub const LOCK: &str = "\u{E72E}";
    pub const VIEW: &str = "\u{E8A7}";
    pub const CLOSE: &str = "\u{E711}";
    pub const ZIP: &str = "\u{F012}";
}

pub fn icons_family() -> FontFamily {
    FontFamily::Name("icons".into())
}

pub fn glyph(code: &str, size: f32) -> RichText {
    RichText::new(code).family(icons_family()).size(size)
}

fn read_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("Fonts");
    names.iter().find_map(|n| std::fs::read(dir.join(n)).ok())
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut ui_fonts = Vec::new();
    // Index 1 of msyh.ttc is "Microsoft YaHei UI".
    if let Some(data) = read_font(&["msyh.ttc"]) {
        fonts.font_data.insert("yahei".into(), FontData::from_blob(std::sync::Arc::new(data), 1).into());
        ui_fonts.push("yahei".to_string());
    } else if let Some(data) = read_font(&["simsun.ttc", "msjh.ttc", "meiryo.ttc", "malgun.ttf"]) {
        fonts.font_data.insert("cjk".into(), FontData::from_owned(data).into());
        ui_fonts.push("cjk".to_string());
    }
    if let Some(data) = read_font(&["segoeui.ttf"]) {
        fonts.font_data.insert("segoe".into(), FontData::from_owned(data).into());
        ui_fonts.insert(0, "segoe".to_string());
    }
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    for (i, f) in ui_fonts.iter().enumerate() {
        prop.insert(i, f.clone());
    }
    let mono = fonts.families.entry(FontFamily::Monospace).or_default();
    mono.extend(ui_fonts.iter().cloned());
    if let Some(data) = read_font(&["SegoeIcons.ttf", "segmdl2.ttf"]) {
        fonts.font_data.insert("icons".into(), FontData::from_owned(data).into());
        fonts.families.insert(icons_family(), vec!["icons".into()]);
    } else {
        fonts.families.insert(icons_family(), fonts.families[&FontFamily::Proportional].clone());
    }
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        let mut v = Visuals::light();
        v.selection.bg_fill = ACCENT_SOFT;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.panel_fill = Color32::from_rgb(0xF7, 0xF8, 0xFA);
        v.window_fill = Color32::WHITE;
        v.extreme_bg_color = Color32::WHITE;
        v.faint_bg_color = Color32::from_rgb(0xF6, 0xF8, 0xFB);
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(6);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xE3, 0xE6, 0xEA));
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(5);
        }
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xD0, 0xD5, 0xDD));
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xF3, 0xF5, 0xF8);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xE9, 0xF1, 0xFD);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xB8, 0xD2, 0xF8));
        style.visuals = v;
        // File-manager UI: clicks on a row must reach the row, not select label text.
        style.interaction.selectable_labels = false;
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 26.0;
        for (ts, size) in [
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Small, 12.0),
            (egui::TextStyle::Heading, 20.0),
            (egui::TextStyle::Monospace, 13.0),
        ] {
            if let Some(f) = style.text_styles.get_mut(&ts) {
                f.size = size;
            }
        }
    });
}

/// Window/taskbar icon drawn in code (a blue tile with a zipper).
pub fn app_icon() -> egui::IconData {
    let size = 64u32;
    egui::IconData { rgba: crate::icon_pixels::draw(size), width: size, height: size }
}
