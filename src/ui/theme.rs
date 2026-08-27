//! Visuals, palette, and type. Follows `sluice-mockup.html`.
//!
//! Three rules do most of the work:
//!
//! * **Twin pairing is the premise of the design, so it is a hue.** Both cards
//!   are teal, both destinations are periwinkle, the optional third destination
//!   is grey and dimmed. The relationship rev 2 exists to exploit -- these two
//!   things are copies of each other -- is legible before you read a word.
//! * Green, amber, and red are reserved **exclusively** for state. The pairing
//!   hues are deliberately chosen outside that range so nothing decorative ever
//!   borrows a state colour.
//! * Colour is never the only signal. Verdict states carry a distinct glyph and
//!   distinct words; log severity carries a glyph as well as a hue.
//!
//! Type is monospace throughout, including the numbers, so figures do not
//! jitter as they tick. egui embeds its own font, so nothing here depends on
//! what happens to be installed on the machine.

use egui::{Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle};

use crate::engine::telemetry::Level;
use crate::engine::{DeviceId, DeviceKind};

pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0E, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x12, 0x16, 0x1F);
pub const PANEL2: Color32 = Color32::from_rgb(0x17, 0x1C, 0x27);
pub const LINE: Color32 = Color32::from_rgb(0x23, 0x2A, 0x38);
pub const LINE_SOFT: Color32 = Color32::from_rgb(0x1B, 0x21, 0x2C);
pub const LOG_BG: Color32 = Color32::from_rgb(0x08, 0x0B, 0x10);

pub const TEXT: Color32 = Color32::from_rgb(0xC8, 0xD1, 0xDE);
pub const DIM: Color32 = Color32::from_rgb(0x6B, 0x76, 0x86);
pub const DIMMER: Color32 = Color32::from_rgb(0x46, 0x4F, 0x5E);

/// Twin pairing. Cards share a hue; destinations share a hue.
pub const CARD: Color32 = Color32::from_rgb(0x35, 0xD6, 0xC0);
pub const DEST: Color32 = Color32::from_rgb(0x8B, 0x93, 0xF8);
pub const AUX: Color32 = Color32::from_rgb(0x6B, 0x76, 0x86);

/// State colours. Never used for decoration.
pub const OK: Color32 = Color32::from_rgb(0x46, 0xC4, 0x6A);
pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xA7, 0x2C);
pub const ERR: Color32 = Color32::from_rgb(0xF2, 0x56, 0x5B);

pub const MONO: f32 = 13.0;
pub const LOG: f32 = 11.5;
pub const SMALL: f32 = 11.0;
pub const TINY: f32 = 9.5;
pub const VERDICT: f32 = 22.0;

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Symbols the UI draws, each resolved against the font actually embedded in
/// the binary.
///
/// egui ships Hack for monospace, which covers Latin-1, arrows, and box drawing
/// but not the dingbats block -- so a hard-coded `✓` renders as a tofu box, and
/// a box is worse than no glyph at all in a dim room at 11pm. Each symbol is
/// therefore declared as a preference plus an ASCII fallback, and the preference
/// is taken only if the font can actually draw it.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub ok: &'static str,
    pub err: &'static str,
    pub warn: &'static str,
    pub info: &'static str,
    pub io: &'static str,
    pub perf: &'static str,
    pub blocking: &'static str,
    pub branch: &'static str,
    pub branch_last: &'static str,
    pub queue_full: &'static str,
    pub queue_empty: &'static str,
    pub phase_idle: &'static str,
    pub phase_running: &'static str,
    pub phase_verify: &'static str,
}

/// What every symbol degrades to. Also what tests and the CLI see.
pub const ASCII_GLYPHS: Glyphs = Glyphs {
    ok: "+",
    err: "x",
    warn: "!",
    info: "-",
    io: ".",
    perf: "~",
    blocking: "<",
    branch: "|-",
    branch_last: "`-",
    queue_full: "#",
    queue_empty: ".",
    phase_idle: "o",
    phase_running: "-",
    phase_verify: "=",
};

static GLYPHS: std::sync::OnceLock<Glyphs> = std::sync::OnceLock::new();

/// The resolved symbol set. Falls back to pure ASCII before [`apply`] has run,
/// which is what unit tests get.
pub fn glyphs() -> &'static Glyphs {
    GLYPHS.get().unwrap_or(&ASCII_GLYPHS)
}

/// Resolve the symbol set against the live font atlas.
///
/// Must be called from inside a frame, not from `apply`: egui panics with
/// "No fonts available until first call to Context::run()" if the atlas is
/// touched during app construction. Cheap to call every frame -- the `OnceLock`
/// makes every call after the first a load.
pub fn ensure_glyphs(ctx: &Context) {
    if GLYPHS.get().is_some() {
        return;
    }
    resolve_glyphs(ctx);
}

fn resolve_glyphs(ctx: &Context) {
    let font = mono(MONO);
    let pick = |preferred: &'static str, fallback: &'static str| -> &'static str {
        if ctx.fonts_mut(|f| f.has_glyphs(&font, preferred)) {
            preferred
        } else {
            fallback
        }
    };
    let resolved = Glyphs {
        ok: pick("✓", ASCII_GLYPHS.ok),
        err: pick("✕", ASCII_GLYPHS.err),
        warn: pick("!", ASCII_GLYPHS.warn),
        info: pick("-", ASCII_GLYPHS.info),
        io: pick("·", ASCII_GLYPHS.io),
        perf: pick("~", ASCII_GLYPHS.perf),
        blocking: pick("◀", ASCII_GLYPHS.blocking),
        branch: pick("├─", ASCII_GLYPHS.branch),
        branch_last: pick("└─", ASCII_GLYPHS.branch_last),
        queue_full: pick("█", ASCII_GLYPHS.queue_full),
        queue_empty: pick("░", ASCII_GLYPHS.queue_empty),
        phase_idle: pick("○", ASCII_GLYPHS.phase_idle),
        phase_running: pick("◔", ASCII_GLYPHS.phase_running),
        phase_verify: pick("◕", ASCII_GLYPHS.phase_verify),
    };
    let _ = GLYPHS.set(resolved);
}

/// The severity mark for a log row, resolved against the embedded font.
pub fn level_glyph(level: Level) -> &'static str {
    let g = glyphs();
    match level {
        Level::Io => g.io,
        Level::Perf => g.perf,
        Level::Info => g.info,
        Level::Ok => g.ok,
        Level::Warn => g.warn,
        Level::Err => g.err,
    }
}

/// The pairing hue for a device.
pub fn device_colour(id: DeviceId) -> Color32 {
    match id.kind() {
        DeviceKind::Card => CARD,
        DeviceKind::Dest => DEST,
        DeviceKind::Aux => AUX,
    }
}

/// `(level tag colour, message colour)`.
///
/// The message is tinted too, not just the tag: a red line should read as red
/// from across the table without having to find the four-character label.
pub fn level_colours(level: Level) -> (Color32, Color32) {
    match level {
        Level::Io => (DIMMER, DIMMER),
        Level::Perf => (DEST, DIM),
        Level::Info => (DIM, DIM),
        Level::Ok => (OK, TEXT),
        Level::Warn => (WARN, WARN),
        Level::Err => (ERR, ERR),
    }
}

/// JetBrains Mono, shipped inside the binary.
///
/// egui already embeds Hack, which would satisfy "no dependency on what is
/// installed on the laptop" -- but Hack has no block elements, no box drawing,
/// and no dingbats, so the queue meter degrades from `███░ 3/4` to `###. 3/4`
/// and the OK mark from `✓` to `+`. JetBrains Mono covers all of them, and it is
/// OFL-licensed. 264 KB is a fair trade for the panel reading the way it was
/// designed to.
const JETBRAINS_MONO: &[u8] = include_bytes!("../../assets/JetBrainsMono-Regular.ttf");

fn install_fonts(ctx: &Context) {
    use egui::{FontData, FontDefinitions};
    use std::sync::Arc;

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "jetbrains-mono".to_owned(),
        Arc::new(FontData::from_static(JETBRAINS_MONO)),
    );
    // First in the monospace list, with egui's bundled faces left behind it as
    // a fallback for anything JetBrains Mono happens to lack.
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains-mono".to_owned());
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "jetbrains-mono".to_owned());
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &Context) {
    install_fonts(ctx);
    // The palette is committed to dark; following the OS theme would hand the
    // verdict banner a background it was not designed against.
    ctx.set_theme(egui::ThemePreference::Dark);
    // Glyphs are still resolved on the first frame: the font atlas does not
    // exist yet at construction time, and the fallbacks stay in place for
    // anything even JetBrains Mono is missing. See `ensure_glyphs`.

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, mono(VERDICT)),
            (TextStyle::Body, mono(MONO)),
            (TextStyle::Monospace, mono(MONO)),
            (TextStyle::Button, mono(SMALL)),
            (TextStyle::Small, mono(SMALL)),
        ]
        .into();

        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = LOG_BG;
        v.faint_bg_color = PANEL2;
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = DEST;
        v.selection.bg_fill = DEST.linear_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, DEST);

        // 1px hairlines, barely-there corners: nothing raised, nothing glossy.
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.bg_stroke = Stroke::new(1.0, LINE);
            w.corner_radius = CornerRadius::same(3);
        }
        v.widgets.noninteractive.bg_fill = PANEL;
        v.widgets.inactive.bg_fill = PANEL2;
        v.widgets.inactive.weak_bg_fill = PANEL2;
        v.widgets.hovered.bg_fill = PANEL2;
        v.widgets.hovered.weak_bg_fill = PANEL2;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, DIM);
        v.widgets.active.bg_fill = PANEL2;
        v.widgets.active.weak_bg_fill = PANEL2;
        v.widgets.active.bg_stroke = Stroke::new(1.0, DIM);

        style.spacing.item_spacing = egui::vec2(9.0, 5.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
    });
}

/// A flat panel with a hairline border.
pub fn panel_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(12, 10))
}

/// A bare frame filling the window background, for the outer panels.
pub fn bg_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(BG)
        .inner_margin(Margin::symmetric(13, 6))
}

/// The tiny letterspaced heading above each region.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    // egui has no letter-spacing, so it is faked the way the mockup renders:
    // uppercase with a hair space between characters.
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    ui.add_space(4.0);
    ui.label(egui::RichText::new(spaced).color(DIMMER).font(mono(TINY)));
    ui.add_space(2.0);
}

/// Right-align a number in a fixed-width column so it does not jitter as it
/// ticks. The single biggest difference between a dashboard that reads as
/// professional and one that reads as a toy.
pub fn num(value: impl std::fmt::Display, width: usize) -> String {
    format!("{:>width$}", value.to_string(), width = width)
}

/// Bytes as a fixed-width human figure.
pub fn bytes(n: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("kB", 1e3)];
    for (unit, scale) in UNITS {
        if n as f64 >= scale {
            return format!("{:>7.2} {unit}", n as f64 / scale);
        }
    }
    format!("{n:>7} B ")
}

/// Megabytes per second, fixed width. An idle device reads as a dash rather
/// than as a very confident zero.
pub fn mbps(v: f32) -> String {
    if v <= 0.0 {
        format!("{:>11}", "—")
    } else {
        format!("{v:>6.0} MB/s")
    }
}

/// Thousands separators, for file counts.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Queue occupancy as the mockup draws it: `███░ 3/4`.
pub fn queue_cells(depth: usize, cap: usize) -> String {
    let g = glyphs();
    let depth = depth.min(cap);
    format!(
        "{}{} {depth}/{cap}",
        g.queue_full.repeat(depth),
        g.queue_empty.repeat(cap - depth)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_fixed_width_so_columns_do_not_jitter() {
        assert_eq!(num(7, 5).len(), 5);
        assert_eq!(num(1613, 5).len(), 5);
        assert_eq!(num(7, 5), "    7");
        let widths: Vec<usize> = [999u64, 1_500, 1_500_000, 1_500_000_000, 4_000_000_000_000]
            .iter()
            .map(|n| bytes(*n).len())
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "byte figures must not change width: {widths:?}"
        );
    }

    /// The pairing hues must not collide with the state hues, or a teal card
    /// header would read as "something is fine" rather than as "this is a card".
    #[test]
    fn pairing_hues_are_disjoint_from_state_hues() {
        for pairing in [CARD, DEST, AUX] {
            for state in [OK, WARN, ERR] {
                assert_ne!(pairing, state);
            }
        }
        assert_ne!(CARD, DEST, "the two twin pairs must be distinguishable");
    }

    #[test]
    fn both_cards_share_a_hue_and_both_destinations_share_a_hue() {
        assert_eq!(
            device_colour(DeviceId::Card1),
            device_colour(DeviceId::Card2)
        );
        assert_eq!(
            device_colour(DeviceId::DestA),
            device_colour(DeviceId::DestB)
        );
        assert_ne!(
            device_colour(DeviceId::Card1),
            device_colour(DeviceId::DestA)
        );
        // The optional third destination is deliberately not part of a pair.
        assert_ne!(
            device_colour(DeviceId::DestC),
            device_colour(DeviceId::DestA)
        );
    }

    /// Rendered with the ASCII fallbacks, since no font is resolved under test.
    #[test]
    fn queue_cells_render_occupancy() {
        assert_eq!(queue_cells(0, 4), ".... 0/4");
        assert_eq!(queue_cells(3, 4), "###. 3/4");
        assert_eq!(queue_cells(4, 4), "#### 4/4");
        // A depth beyond capacity must not panic on a negative repeat.
        assert_eq!(queue_cells(9, 4), "#### 4/4");
    }

    /// Every preferred symbol must have a fallback that is plain ASCII, or the
    /// degradation just swaps one unrenderable glyph for another.
    #[test]
    fn every_fallback_is_ascii() {
        let g = ASCII_GLYPHS;
        for s in [
            g.ok,
            g.err,
            g.warn,
            g.info,
            g.io,
            g.perf,
            g.blocking,
            g.branch,
            g.branch_last,
            g.queue_full,
            g.queue_empty,
            g.phase_idle,
            g.phase_running,
            g.phase_verify,
        ] {
            assert!(s.is_ascii(), "{s:?} is not an ASCII fallback");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn severity_marks_are_distinguishable_without_colour() {
        let marks: Vec<&str> = [
            Level::Io,
            Level::Perf,
            Level::Info,
            Level::Ok,
            Level::Warn,
            Level::Err,
        ]
        .iter()
        .map(|l| level_glyph(*l))
        .collect();
        for i in 0..marks.len() {
            for j in (i + 1)..marks.len() {
                assert_ne!(marks[i], marks[j], "two levels share a mark");
            }
        }
    }

    #[test]
    fn an_idle_device_shows_a_dash_not_a_zero() {
        assert!(mbps(0.0).trim() == "—");
        assert!(mbps(128.4).contains("128"));
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(thousands(1613), "1,613");
        assert_eq!(thousands(999), "999");
    }
}
