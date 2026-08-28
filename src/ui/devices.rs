//! The device strip: five cards, one per device, each with a sparkline.
//!
//! Two things the mockup gets right and the prose spec does not:
//!
//! * A **2px top border in the pairing hue**, so the two cards read as a pair
//!   and the two destinations read as a pair before you have read anything.
//! * A **TWIN badge** naming the other half. Card 1's badge says `TWIN ①②`;
//!   dest A's says `TWIN AB`. The optional third destination has no badge and is
//!   dimmed, because it is a bonus rather than half of a guarantee.
//!
//! The volume serial is on every card, permanently. Two identical LaCie Rugged
//! drives swap letters between plug-ins, and the point of showing the serial is
//! that a swap becomes visible *before* you press the button rather than after
//! the job has run against the wrong drive.
//!
//! Sparklines are hand-drawn with `ui.painter()` -- about thirty lines, and it
//! avoids taking a plotting crate as a dependency that would drift out from
//! under us mid-project.

use std::collections::BTreeMap;

use egui::{Color32, CornerRadius, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::engine::telemetry::DeviceInfo;
use crate::engine::{DeviceId, DeviceKind};

use super::theme;

/// Sixty seconds at 2 Hz. Enough history to see a stall, cheap enough to redraw
/// every frame.
const BUCKETS: usize = 120;

/// Silence longer than this means the device has stopped, not that it is still
/// running at its last reported rate. `ByteMeter` ticks every 100 ms.
const STALE_AFTER: f64 = 0.75;

pub struct DeviceState {
    pub info: Option<DeviceInfo>,
    pub bytes: u64,
    /// What this device owes in the current phase, once the engine has said.
    ///
    /// `None` and `Some(0)` are different answers and the difference is
    /// visible: nobody has told us yet, against a drive that resume left with
    /// nothing to do. The first draws an empty bar, the second a full one.
    pub plan_bytes: Option<u64>,
    pub mbps: f32,
    /// Job-elapsed seconds at the last throughput sample.
    last_sample_at: f64,
    history: [f32; BUCKETS],
    head: usize,
    filled: usize,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            info: None,
            bytes: 0,
            plan_bytes: None,
            mbps: 0.0,
            last_sample_at: f64::NEG_INFINITY,
            history: [0.0; BUCKETS],
            head: 0,
            filled: 0,
        }
    }
}

impl DeviceState {
    pub fn record_throughput(&mut self, mbps: f32, at: f64) {
        self.mbps = mbps;
        self.last_sample_at = at;
        self.history[self.head] = mbps;
        self.head = (self.head + 1) % BUCKETS;
        self.filled = (self.filled + 1).min(BUCKETS);
    }

    /// The rate this device is moving *now*, averaged over about a second.
    ///
    /// Two corrections to the raw sample, both of which people saw as jitter.
    ///
    /// A single 100 ms tick holds either one 4 MiB chunk or two, so on a drive
    /// running at a perfectly steady rate consecutive samples swing by 2x. The
    /// mean over the last second is what somebody is actually trying to read.
    ///
    /// And a hasher that has finished simply stops emitting, leaving `mbps` at
    /// its final value forever — so a card done in the first minute went on
    /// showing a full bar for the rest of the pass, and kept inflating the scale
    /// every other bar was measured against.
    pub fn smoothed_mbps(&self, now: f64) -> f32 {
        if now - self.last_sample_at > STALE_AFTER {
            return 0.0;
        }
        let s = self.samples();
        let n = s.len().min(10);
        if n == 0 {
            return 0.0;
        }
        s[s.len() - n..].iter().sum::<f32>() / n as f32
    }

    /// Oldest-to-newest, for drawing left to right.
    pub fn samples(&self) -> Vec<f32> {
        if self.filled < BUCKETS {
            self.history[..self.filled].to_vec()
        } else {
            let (a, b) = self.history.split_at(self.head);
            b.iter().chain(a.iter()).copied().collect()
        }
    }

    pub fn peak(&self) -> f32 {
        self.samples().into_iter().fold(0.0, f32::max)
    }
}

/// All five slots, always. A device that is not mounted shows as an empty slot
/// rather than vanishing, so the shape of the session is constant.
pub const SLOTS: [DeviceId; 5] = [
    DeviceId::Card1,
    DeviceId::Card2,
    DeviceId::DestA,
    DeviceId::DestB,
    DeviceId::DestC,
];

pub fn strip(ui: &mut Ui, devices: &BTreeMap<DeviceId, DeviceState>, now: f64) {
    const GAP: f32 = 8.0;
    /// Horizontal padding `panel_frame` adds inside each card, both sides.
    const FRAME_PAD: f32 = 24.0;

    let n = SLOTS.len() as f32;
    // `set_width` sizes the *content*, so the frame's own padding has to come
    // out of the budget or the fifth card falls off the right edge.
    let width = ((ui.available_width() - GAP * (n - 1.0) - FRAME_PAD * n) / n).max(120.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        for id in SLOTS {
            card(ui, id, devices.get(&id), width, now);
        }
    });
}

fn card(ui: &mut Ui, id: DeviceId, state: Option<&DeviceState>, width: f32, now: f64) {
    let hue = theme::device_colour(id);
    let present = state.and_then(|s| s.info.as_ref()).is_some();
    // The optional destination sits back visually whether or not it is in use.
    let dim_all = id.kind() == DeviceKind::Aux;

    theme::panel_frame().show(ui, |ui| {
        ui.set_width(width);
        // A cap as well as a floor. Without it the card grows to fit its
        // longest row and the strip runs off the edge of the window.
        ui.set_max_width(width);
        ui.vertical(|ui| {
            // The 2px pairing bar across the top of the card.
            let (bar, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 2.0), Sense::hover());
            ui.painter().rect_filled(
                bar,
                CornerRadius::ZERO,
                if dim_all {
                    hue.linear_multiply(0.5)
                } else {
                    hue
                },
            );
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(spaced(id.title()))
                        .color(if dim_all { theme::DIM } else { hue })
                        .font(theme::mono(theme::TINY)),
                );
                if let Some(badge) = id.twin_badge() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(badge)
                                .color(theme::DIMMER)
                                .font(theme::mono(theme::TINY)),
                        );
                    });
                }
            });
            ui.add_space(3.0);

            match state.and_then(|s| s.info.as_ref()) {
                Some(info) => {
                    let label = if info.volume.label.is_empty() {
                        "(no label)".to_string()
                    } else {
                        info.volume.label.clone()
                    };
                    row(ui, &label, &info.volume.serial_hex(), theme::DIMMER);
                    row(
                        ui,
                        &info.volume.filesystem,
                        &format!("{} B sec", info.volume.sector_size),
                        theme::TEXT,
                    );
                    row(
                        ui,
                        &format!("{} free", theme::bytes(info.free_bytes).trim()),
                        // Smoothed and idle-aware, matching the pipeline panel below.
                        // The raw sample swings 2x on a steady drive and never
                        // returns to zero when a device finishes, so the card and
                        // the panel showed two different numbers for one thing.
                        theme::mbps(state.map(|s| s.smoothed_mbps(now)).unwrap_or(0.0)).trim(),
                        theme::TEXT,
                    );
                }
                None => {
                    row(ui, "—", "not mounted", theme::DIMMER);
                    row(ui, "", "", theme::DIMMER);
                    row(ui, "", "", theme::DIMMER);
                }
            }

            ui.add_space(4.0);
            sparkline(
                ui,
                state.map(|s| s.samples()).unwrap_or_default(),
                state.map(|s| s.peak()).unwrap_or(0.0),
                if present { hue } else { theme::LINE },
            );
        });
    });
}

/// A left label and a right value on one line, as the mockup lays them out.
/// `label ............ value`, with the label giving way rather than the value.
///
/// Laid out right-to-left so the value is placed first and keeps its space; the
/// label then truncates into whatever is left. Two plain labels in a horizontal
/// layout instead set the row's width between them, and `set_width` on the card
/// is a *minimum* rather than a cap — so a long volume label pushed every card
/// wider than its share and the fifth fell off the right of the window. Caught
/// while screenshotting: DEST B was clipped and DEST C was not on screen at all.
fn row(ui: &mut Ui, left: &str, right: &str, right_colour: Color32) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(right)
                    .color(right_colour)
                    .font(theme::mono(theme::SMALL)),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(left)
                        .color(theme::DIM)
                        .font(theme::mono(theme::SMALL)),
                )
                .truncate(),
            );
        });
    });
}

/// Uppercase with hair spaces, standing in for CSS letter-spacing.
fn spaced(text: &str) -> String {
    text.to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect()
}

/// A 60-second rolling window: a line in the device hue with a faint fill under
/// it, exactly as the mockup's canvas draws.
fn sparkline(ui: &mut Ui, samples: Vec<f32>, peak: f32, colour: Color32) {
    let size = Vec2::new(ui.available_width(), 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), theme::LOG_BG);

    // A baseline hairline, so an idle device reads as idle rather than as blank.
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() - 0.5),
            egui::pos2(rect.right(), rect.bottom() - 0.5),
        ],
        Stroke::new(1.0, theme::LINE_SOFT),
    );
    if samples.len() < 2 || peak <= 0.0 {
        return;
    }

    let n = samples.len();
    let dx = rect.width() / (BUCKETS - 1) as f32;
    let scale = |v: f32| rect.bottom() - (v / peak).clamp(0.0, 1.0) * (rect.height() - 3.0) - 1.5;

    let points: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| {
            // Right-aligned, so the newest sample is always at the right edge
            // and history scrolls left as the window fills.
            let x = (rect.right() - (n - 1 - i) as f32 * dx).max(rect.left());
            egui::pos2(x, scale(*v))
        })
        .collect();

    // Fill under the curve first, then the curve on top of it.
    let mut fill = points.clone();
    fill.push(egui::pos2(points[n - 1].x, rect.bottom()));
    fill.push(egui::pos2(points[0].x, rect.bottom()));
    painter.add(egui::Shape::convex_polygon(
        fill,
        colour.linear_multiply(0.10),
        Stroke::NONE,
    ));
    painter.add(egui::Shape::line(points, Stroke::new(1.6, colour)));
}

/// A progress or occupancy bar; used by the pipeline monitor too.
pub fn meter(ui: &mut Ui, fraction: f32, width: f32, colour: Color32) -> Rect {
    let size = Vec2::new(width, 7.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), theme::LINE_SOFT);
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    painter.rect_filled(filled, CornerRadius::same(2), colour);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_oldest_to_newest_before_it_wraps() {
        let mut s = DeviceState::default();
        for v in [10.0, 20.0, 30.0] {
            s.record_throughput(v, 0.0);
        }
        assert_eq!(s.samples(), vec![10.0, 20.0, 30.0]);
        assert_eq!(s.peak(), 30.0);
        assert_eq!(s.mbps, 30.0);
    }

    /// A hasher that has finished stops emitting, and its last rate would
    /// otherwise stand forever — a full bar for a device doing nothing, and an
    /// inflated scale for every other bar drawn against it.
    #[test]
    fn a_device_that_stopped_reporting_reads_as_idle() {
        let mut s = DeviceState::default();
        s.record_throughput(400.0, 10.0);
        assert!(s.smoothed_mbps(10.1) > 0.0, "still working");
        assert_eq!(s.smoothed_mbps(30.0), 0.0, "it finished 20 seconds ago");
    }

    /// One 100 ms tick holds either one 4 MiB chunk or two, so a drive at a
    /// perfectly steady rate reports samples alternating by 2x. What is drawn
    /// should not.
    #[test]
    fn the_shown_rate_is_averaged_over_recent_samples() {
        let mut s = DeviceState::default();
        for i in 0..10 {
            s.record_throughput(if i % 2 == 0 { 20.0 } else { 60.0 }, i as f64 * 0.1);
        }
        let shown = s.smoothed_mbps(0.9);
        assert!((shown - 40.0).abs() < 0.01, "got {shown}");
    }

    #[test]
    fn history_stays_oldest_to_newest_after_wrapping() {
        let mut s = DeviceState::default();
        for i in 0..(BUCKETS + 3) {
            s.record_throughput(i as f32, 0.0);
        }
        let samples = s.samples();
        assert_eq!(samples.len(), BUCKETS);
        assert_eq!(
            *samples.last().unwrap(),
            (BUCKETS + 2) as f32,
            "the newest sample must be last so it draws at the right edge"
        );
        assert_eq!(*samples.first().unwrap(), 3.0);
        assert!(
            samples.windows(2).all(|w| w[0] < w[1]),
            "the ring must not scramble order when it wraps"
        );
    }

    #[test]
    fn an_idle_device_has_no_peak_and_draws_nothing() {
        let s = DeviceState::default();
        assert!(s.samples().is_empty());
        assert_eq!(s.peak(), 0.0);
    }

    /// Every slot is always shown, so an unmounted card is a visible gap rather
    /// than a strip that silently has one fewer entry.
    #[test]
    fn all_five_slots_are_always_present() {
        assert_eq!(SLOTS.len(), 5);
        assert!(SLOTS.contains(&DeviceId::Card2));
        assert!(SLOTS.contains(&DeviceId::DestC));
    }

    #[test]
    fn only_paired_devices_carry_a_twin_badge() {
        assert_eq!(DeviceId::Card1.twin_badge(), Some("TWIN C1·C2"));
        assert_eq!(DeviceId::Card2.twin_badge(), Some("TWIN C1·C2"));
        assert_eq!(DeviceId::DestA.twin_badge(), Some("TWIN A·B"));
        assert_eq!(DeviceId::DestB.twin_badge(), Some("TWIN A·B"));
        assert_eq!(
            DeviceId::DestC.twin_badge(),
            None,
            "the optional destination is a bonus, not half of a guarantee"
        );
    }
}
