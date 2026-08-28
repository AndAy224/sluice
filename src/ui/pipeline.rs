//! The pipeline monitor: the fan-out, live.
//!
//! Queue depth is channel occupancy, read straight off the bounded channels in
//! the copy pipeline. It is the most useful number in the program, because it
//! names *which device is applying backpressure* -- which is the actual answer
//! to "why is this slow" -- and it turns the architecture from a diagram into
//! something observable.
//!
//! Laid out as the mockup does: `who | bar | MB/s | queue | blocking`, with the
//! queue drawn as occupancy cells (`███░ 3/4`) so a full channel is visible as a
//! shape rather than as a fraction to be read.

use std::collections::{BTreeMap, VecDeque};

use egui::{RichText, Ui};

use crate::engine::telemetry::Phase;
use crate::engine::{DeviceId, DeviceKind};

use super::devices::DeviceState;
use super::theme;

/// Column widths, mirroring the mockup's grid.
const W_WHO: f32 = 128.0;
const W_RATE: f32 = 92.0;
const W_QUEUE: f32 = 86.0;

#[derive(Default)]
pub struct PipelineState {
    /// Per-destination `(depth, capacity)`.
    pub queues: BTreeMap<DeviceId, (usize, usize)>,
    pub current_file: Option<String>,
    pub current_size: u64,
    pub file_index: usize,
    pub file_total: usize,
    /// Which card the reader is currently pulling from.
    pub source: Option<DeviceId>,
    /// Bytes the session has to move, for the estimate.
    pub bytes_total: u64,
    /// `(phase seconds, bytes moved)` over the recent window.
    pub trail: VecDeque<(f64, u64)>,
}

/// Seconds of history the rate is measured over. Long enough to ride out the
/// 4 MiB chunk quantisation, short enough to notice a drive falling behind.
const RATE_WINDOW: f64 = 12.0;

impl PipelineState {
    /// The destination applying backpressure, if any: a full queue means the
    /// reader is blocked on that drive.
    pub fn blocking(&self) -> Option<DeviceId> {
        self.queues
            .iter()
            .filter(|(_, (depth, cap))| *cap > 0 && depth >= cap)
            .map(|(dev, _)| *dev)
            .next()
    }

    pub fn progress(&self) -> f32 {
        if self.file_total == 0 {
            0.0
        } else {
            self.file_index as f32 / self.file_total as f32
        }
    }

    /// Record where this phase has got to, for the rolling rate.
    pub fn note_progress(&mut self, phase_secs: f64, moved: u64) {
        self.trail.push_back((phase_secs, moved));
        while let Some(&(t, _)) = self.trail.front() {
            if phase_secs - t > RATE_WINDOW {
                self.trail.pop_front();
            } else {
                break;
            }
        }
    }

    /// Bytes per second over the last few seconds, not over the whole phase.
    ///
    /// Verify runs one hasher per copy and they do **not** finish together: the
    /// cards sit on fast internal disks and are done in well under a minute,
    /// while the USB drives grind on for several more. A phase average is then
    /// dominated by early finishers that are no longer moving anything, so the
    /// estimate reads a fraction of the truth and barely moves — observed live,
    /// stuck at 49 seconds for over two minutes with five minutes left to run.
    fn recent_rate(&self) -> Option<f64> {
        let (t0, b0) = *self.trail.front()?;
        let (t1, b1) = *self.trail.back()?;
        let dt = t1 - t0;
        let db = b1.saturating_sub(b0);
        (dt >= 2.0 && db > 0).then(|| db as f64 / dt)
    }

    /// Seconds remaining, from the recent rate.
    ///
    /// Deliberately based on bytes rather than file count: a night is a mix of
    /// 60 MB stills and multi-gigabyte clips, and counting files would swing the
    /// estimate wildly as the mix changes.
    pub fn eta_secs(&self, bytes_done: u64, elapsed: f64) -> Option<f64> {
        if self.bytes_total == 0 || bytes_done == 0 || elapsed < 2.0 {
            return None;
        }
        let remaining = self.bytes_total.saturating_sub(bytes_done);
        if remaining == 0 {
            return Some(0.0);
        }
        // The phase average is the fallback only until the window has filled.
        let rate = self.recent_rate().unwrap_or(bytes_done as f64 / elapsed);
        (rate > 0.0).then(|| remaining as f64 / rate)
    }

    pub fn reset(&mut self) {
        self.queues.clear();
        self.current_file = None;
        self.current_size = 0;
        self.file_index = 0;
        self.file_total = 0;
        self.source = None;
        self.bytes_total = 0;
        self.trail.clear();
    }
}

pub fn show(
    ui: &mut Ui,
    state: &PipelineState,
    devices: &BTreeMap<DeviceId, DeviceState>,
    phase: Phase,
    elapsed: f64,
    now: f64,
) {
    theme::panel_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        match phase {
            Phase::Verify => verify_view(ui, devices, now),
            _ => copy_view(ui, state, devices, now),
        }
        // Seventeen minutes with no estimate is worse than it needs to be.
        if matches!(phase, Phase::Copy | Phase::Verify) {
            let moved = moved_in_phase(devices, phase);
            let note = match state.eta_secs(moved, elapsed) {
                Some(secs) => format!(
                    "{} of {} · {} elapsed · about {} left",
                    theme::bytes(moved).trim(),
                    theme::bytes(state.bytes_total).trim(),
                    humane_secs(elapsed),
                    humane_secs(secs)
                ),
                None => format!("{} elapsed", humane_secs(elapsed)),
            };
            ui.label(
                RichText::new(note)
                    .color(theme::DIM)
                    .font(theme::mono(theme::SMALL)),
            );
        }
    });
}

/// How far through its own work one device is.
///
/// A drive resume left nothing to do is **finished**, not stalled at zero, and
/// the two have to look different: on a resumed night one destination can have
/// the whole card already and the other none of it, and an empty bar next to a
/// filling one would read as the drive having failed to start.
fn fraction_done(d: &DeviceState) -> f32 {
    match d.plan_bytes {
        // The engine has not said yet.
        None => 0.0,
        // Nothing owed, so nothing outstanding.
        Some(0) => 1.0,
        Some(total) => (d.bytes as f32 / total as f32).clamp(0.0, 1.0),
    }
}

/// Bytes the current phase has moved so far.
///
/// The two phases count differently, and using the copy's rule for verify made
/// the estimate read a fraction of the truth. Copy fans **one** read out to
/// every destination, so each destination moves the whole session and the
/// furthest-along one is the progress. Verify re-reads every copy
/// independently — both cards and every destination — so its work is the sum of
/// all those streams, which on a two-card two-drive night is four times the
/// copy.
pub fn moved_in_phase(devices: &BTreeMap<DeviceId, DeviceState>, phase: Phase) -> u64 {
    match phase {
        Phase::Verify => devices.values().map(|s| s.bytes).sum(),
        _ => devices
            .iter()
            .filter(|(d, _)| d.is_dest())
            .map(|(_, s)| s.bytes)
            .max()
            .unwrap_or(0),
    }
}

fn copy_view(
    ui: &mut Ui,
    state: &PipelineState,
    devices: &BTreeMap<DeviceId, DeviceState>,
    now: f64,
) {
    let source = state.source.unwrap_or(DeviceId::Card1);
    let reader_rate = devices
        .get(&source)
        .map(|d| d.smoothed_mbps(now))
        .unwrap_or(0.0);
    // Every row is drawn against its own work. They used to share one number —
    // the reader's position through the file list — which is true only while
    // nothing is skipped: the fan-out keeps the destinations within one queue
    // of each other, so one bar for all of them is honest on a fresh card. It
    // stops being honest the moment resume is involved, because each drive
    // then sits out a different set of files, and it was never honest for the
    // reader itself, which emits nothing for a file every destination already
    // holds — so a fully resumed run drew a bar stuck at zero while the phase
    // ran to completion.
    let progress_of = |dev: DeviceId| -> f32 { devices.get(&dev).map_or(0.0, fraction_done) };

    // --- the reader ---
    ui.horizontal(|ui| {
        fixed(ui, W_WHO, |ui| {
            ui.label(
                RichText::new(format!("reader · {}", source.label()))
                    .color(theme::TEXT)
                    .font(theme::mono(theme::LOG)),
            );
        });
        let bar_width = bar_width(ui);
        super::devices::meter(ui, progress_of(source), bar_width, theme::TEXT);
        fixed(ui, W_RATE, |ui| {
            ui.label(
                RichText::new(theme::mbps(reader_rate))
                    .color(theme::TEXT)
                    .font(theme::mono(theme::LOG)),
            );
        });
        fixed(ui, W_QUEUE, |_ui| {});
        let sub = match &state.current_file {
            Some(f) if state.file_total > 0 => format!(
                "{f}   {} / {}",
                theme::thousands(state.file_index as u64),
                theme::thousands(state.file_total as u64)
            ),
            Some(f) => f.clone(),
            None => "idle".into(),
        };
        // Truncate rather than wrap: a long filename must not push the row onto
        // a second line and make the monitor jump as files change.
        ui.add(
            egui::Label::new(
                RichText::new(sub)
                    .color(theme::DIM)
                    .font(theme::mono(theme::SMALL)),
            )
            .truncate(),
        );
    });

    // --- the writers ---
    let blocking = state.blocking();
    let entries: Vec<(DeviceId, (usize, usize))> =
        state.queues.iter().map(|(d, q)| (*d, *q)).collect();
    for (i, (dev, (depth, cap))) in entries.iter().enumerate() {
        let last = i + 1 == entries.len();
        let hue = theme::device_colour(*dev);
        let is_blocking = blocking == Some(*dev);
        let faded = dev.kind() == DeviceKind::Aux;
        ui.horizontal(|ui| {
            fixed(ui, W_WHO, |ui| {
                ui.label(
                    RichText::new(format!(
                        "  {} writer {}",
                        if last {
                            theme::glyphs().branch_last
                        } else {
                            theme::glyphs().branch
                        },
                        dev.label()
                    ))
                    .color(if faded { theme::AUX } else { hue })
                    .font(theme::mono(theme::LOG)),
                );
            });
            let bar_width = bar_width(ui);
            super::devices::meter(ui, progress_of(*dev), bar_width, hue);
            let rate = devices.get(dev).map(|d| d.mbps).unwrap_or(0.0);
            fixed(ui, W_RATE, |ui| {
                ui.label(
                    RichText::new(theme::mbps(rate))
                        .color(theme::TEXT)
                        .font(theme::mono(theme::LOG)),
                );
            });
            fixed(ui, W_QUEUE, |ui| {
                ui.label(
                    RichText::new(theme::queue_cells(*depth, *cap))
                        .color(if is_blocking { theme::WARN } else { hue })
                        .font(theme::mono(theme::LOG)),
                );
            });
            if is_blocking {
                // Names the drive everything else is waiting on. This is the
                // answer to "why is this slow".
                ui.label(
                    RichText::new(format!("{} BLOCKING", theme::glyphs().blocking))
                        .color(theme::WARN)
                        .font(theme::mono(theme::SMALL)),
                );
            }
        });
    }

    if entries.is_empty() {
        ui.label(
            RichText::new("  idle")
                .color(theme::DIMMER)
                .font(theme::mono(theme::SMALL)),
        );
    }
}

/// During verify the same region shows the N concurrent hashers with their
/// independent progress.
fn verify_view(ui: &mut Ui, devices: &BTreeMap<DeviceId, DeviceState>, now: f64) {
    ui.label(
        RichText::new(format!(
            "verify · {} concurrent unbuffered hashers · page cache bypassed",
            devices.len()
        ))
        .color(theme::DIM)
        .font(theme::mono(theme::LOG)),
    );
    // These bars fill with the work each hasher has done, not with how fast it
    // is going. They were a rate meter scaled against the fastest rate any
    // device had ever reached, and on real hardware that is a category error:
    // the cards sit on internal disks and finish in seconds at several GB/s,
    // the drives are on USB at a fortieth of that. Measured over one run —
    // cards 3540 and 3703 MB/s, drives 121 and 41 — the two rows that decide
    // the verdict drew at 2.9% and 1.0% of the width, against a peak set by a
    // device that had stopped working four seconds in and would never emit
    // again. `smoothed_mbps` already zeroes a finished device's *numerator*;
    // nothing was zeroing the denominator it had left behind.
    //
    // Progress has none of that coupling: every row is measured against its own
    // work, a finished card sits honestly at full, and a slow drive advances at
    // its own pace. The rate is still on the row, as the number it always was.
    for (dev, dstate) in devices {
        let rate = dstate.smoothed_mbps(now);
        let progress = fraction_done(dstate);
        let hue = theme::device_colour(*dev);
        ui.horizontal(|ui| {
            fixed(ui, W_WHO, |ui| {
                ui.label(
                    RichText::new(format!("  {:<3}", dev.label()))
                        .color(hue)
                        .font(theme::mono(theme::LOG)),
                );
            });
            let bar_width = bar_width(ui);
            super::devices::meter(ui, progress, bar_width, hue);
            fixed(ui, W_RATE, |ui| {
                ui.label(
                    RichText::new(theme::mbps(rate))
                        .color(theme::TEXT)
                        .font(theme::mono(theme::LOG)),
                );
            });
            // Both numbers, because "6.2 GB" alone does not say how much is
            // left and a bar alone does not say how much has been read.
            let read = match dstate.plan_bytes {
                Some(total) if total > 0 => format!(
                    "{} of {}",
                    theme::bytes(dstate.bytes).trim(),
                    theme::bytes(total).trim()
                ),
                _ => theme::bytes(dstate.bytes).trim().to_string(),
            };
            ui.label(
                RichText::new(read)
                    .color(theme::DIM)
                    .font(theme::mono(theme::SMALL)),
            );
        });
    }
}

/// A duration a tired person can read at a glance: `8m 12s`, not `492.3s`.
pub fn humane_secs(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Width left for the progress bar once the fixed columns and the trailing
/// text have taken their share.
fn bar_width(ui: &Ui) -> f32 {
    const TRAILING: f32 = 240.0;
    (ui.available_width() - W_RATE - W_QUEUE - TRAILING).clamp(60.0, 420.0)
}

/// A fixed-width column, so the grid lines up the way the mockup's CSS grid does.
fn fixed(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.spacing().interact_size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_width(width);
            add(ui);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // `DeviceState` keeps its sparkline fields private, so the struct-update
    // form is not available here.
    #[allow(clippy::field_reassign_with_default)]
    fn moved(pairs: &[(DeviceId, u64)]) -> BTreeMap<DeviceId, DeviceState> {
        pairs
            .iter()
            .map(|(d, b)| {
                let mut s = DeviceState::default();
                s.bytes = *b;
                (*d, s)
            })
            .collect()
    }

    /// The two phases count their work differently, and using the copy's rule
    /// for verify made the estimate read a quarter of the truth on a two-card,
    /// two-drive night — then, because the counters ran cumulatively, "about 0s
    /// left" for the whole pass.
    #[test]
    fn verify_counts_every_stream_and_copy_counts_the_furthest_destination() {
        let devs = moved(&[
            (DeviceId::Card1, 300),
            (DeviceId::Card2, 300),
            (DeviceId::DestA, 250),
            (DeviceId::DestB, 200),
        ]);

        // Copy fans one read out to both drives: the session is as far along as
        // the drive that has taken the most. Cards are the source, not work.
        assert_eq!(moved_in_phase(&devs, Phase::Copy), 250);

        // Verify re-reads all four copies independently, so every stream counts.
        assert_eq!(moved_in_phase(&devs, Phase::Verify), 1050);
    }

    /// Watched live: verify's estimate sat at 49 seconds for over two minutes.
    ///
    /// The hashers do not finish together. The cards are on fast internal disks
    /// and are done inside a minute; the USB drives grind on for several more.
    /// A phase average is then dominated by streams that stopped moving bytes
    /// long ago, so the estimate reads a fraction of the truth and barely moves.
    #[test]
    fn the_estimate_follows_the_recent_rate_not_the_phase_average() {
        let mut s = PipelineState {
            bytes_total: 4_000,
            ..Default::default()
        };
        // Two fast streams finish by t=10 having moved 1000 each; the slow pair
        // continues at 25/s between them.
        s.note_progress(0.0, 0);
        s.note_progress(10.0, 2_250);
        s.note_progress(20.0, 2_500);
        s.note_progress(30.0, 2_750);

        // 1250 left at the *recent* 25/s.
        let eta = s.eta_secs(2_750, 30.0).unwrap();
        assert!((eta - 50.0).abs() < 5.0, "got {eta}");

        // The phase average would have been wildly optimistic — this is the
        // number that used to be shown.
        let naive = (4_000.0 - 2_750.0) / (2_750.0 / 30.0);
        assert!(naive < 15.0, "naive average was {naive}");
    }

    /// A verify estimate divides this phase's bytes by this phase's seconds.
    #[test]
    fn a_verify_estimate_uses_the_verify_total() {
        // 12 GB copied, but four copies to re-read.
        let s = PipelineState {
            bytes_total: 48_000,
            ..Default::default()
        };
        // A quarter of the way through, after 10s of verifying.
        assert_eq!(s.eta_secs(12_000, 10.0), Some(30.0));
    }

    #[test]
    fn a_full_queue_names_the_blocking_device() {
        let mut s = PipelineState::default();
        s.queues.insert(DeviceId::DestA, (4, 4));
        s.queues.insert(DeviceId::DestB, (1, 4));
        assert_eq!(s.blocking(), Some(DeviceId::DestA));
    }

    #[test]
    fn nothing_blocks_when_every_queue_has_room() {
        let mut s = PipelineState::default();
        s.queues.insert(DeviceId::DestA, (3, 4));
        s.queues.insert(DeviceId::DestB, (0, 4));
        assert_eq!(s.blocking(), None);
    }

    #[test]
    fn a_zero_capacity_queue_is_not_reported_as_blocking() {
        let mut s = PipelineState::default();
        s.queues.insert(DeviceId::DestA, (0, 0));
        assert_eq!(s.blocking(), None);
    }

    #[test]
    fn progress_is_bounded_and_safe_before_the_total_is_known() {
        let mut s = PipelineState::default();
        assert_eq!(s.progress(), 0.0, "must not divide by zero before SCAN");
        s.file_total = 1613;
        s.file_index = 1613;
        assert_eq!(s.progress(), 1.0);
    }

    /// The estimate is bytes-based: a night mixes 60 MB stills with multi-GB
    /// clips, and counting files would swing wildly as the mix changes.
    #[test]
    fn the_estimate_comes_from_bytes_moved_per_second() {
        let s = PipelineState {
            bytes_total: 1000,
            ..Default::default()
        };
        // 250 bytes in 10s is 25 B/s, so the remaining 750 take 30s.
        assert_eq!(s.eta_secs(250, 10.0), Some(30.0));
        assert_eq!(s.eta_secs(1000, 10.0), Some(0.0), "finished means zero");
    }

    #[test]
    fn no_estimate_is_offered_before_there_is_evidence_for_one() {
        let mut s = PipelineState::default();
        assert_eq!(s.eta_secs(0, 0.0), None, "nothing known yet");
        s.bytes_total = 1000;
        assert_eq!(s.eta_secs(100, 0.5), None, "half a second is not a rate");
        assert_eq!(s.eta_secs(0, 30.0), None, "no bytes moved, no estimate");
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(humane_secs(45.0), "45s");
        assert_eq!(humane_secs(492.0), "8m 12s");
        assert_eq!(humane_secs(3725.0), "1h 02m");
        assert_eq!(
            humane_secs(-5.0),
            "0s",
            "a negative estimate must not print"
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = PipelineState::default();
        s.queues.insert(DeviceId::DestA, (4, 4));
        s.current_file = Some("x".into());
        s.file_total = 10;
        s.source = Some(DeviceId::Card2);
        s.reset();
        assert!(s.queues.is_empty());
        assert!(s.current_file.is_none());
        assert_eq!(s.file_total, 0);
        assert!(s.source.is_none());
    }
}
