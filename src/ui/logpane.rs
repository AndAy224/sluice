//! The log pane: monospace, virtualised, sticky-to-bottom.
//!
//! Virtualisation is a correctness requirement, not an optimisation. 1,613 files
//! at several lines each, plus 10 Hz perf ticks over a twenty-minute run, is
//! well north of 50,000 rows. Rendering all of them drops the UI to single-digit
//! frames per second and -- far worse -- makes it look like the copy has
//! stalled at exactly the moment you are watching to see whether it has.

use std::collections::VecDeque;

use egui::{Color32, RichText, ScrollArea, Ui};

use crate::engine::telemetry::{Level, Record, Stage};

use super::theme;

/// In-memory cap. The full stream is on disk as JSONL regardless, so this only
/// bounds what can be scrolled back to, never what is recorded.
const MAX_ROWS: usize = 50_000;

pub struct LogRow {
    pub time: String,
    pub level: Level,
    pub stage: Stage,
    pub msg: String,
}

impl LogRow {
    fn matches(&self, filter: Option<Level>, search: &str) -> bool {
        if let Some(level) = filter {
            if self.level != level {
                return false;
            }
        }
        if search.is_empty() {
            return true;
        }
        let needle = search.to_ascii_lowercase();
        self.msg.to_ascii_lowercase().contains(&needle) || self.stage.label().contains(&needle)
    }
}

pub struct LogPane {
    rows: VecDeque<LogRow>,
    /// Sequence number of `rows.front()`. Lets the filtered index survive
    /// eviction without an O(n) fix-up on every append.
    first_seq: u64,
    next_seq: u64,
    /// Sequence numbers of rows passing the current filter.
    filtered: Vec<u64>,
    pub filter: Option<Level>,
    pub search: String,
    /// Cleared when the user scrolls up, so following the tail never fights a
    /// deliberate scroll back.
    pub stick_to_bottom: bool,
    /// Events the sink could not hand to this view. Counted since the sink was
    /// written and displayed nowhere until now.
    pub dropped: u64,
    dirty: bool,
}

impl Default for LogPane {
    fn default() -> Self {
        Self {
            rows: VecDeque::new(),
            first_seq: 0,
            next_seq: 0,
            filtered: Vec::new(),
            filter: None,
            search: String::new(),
            stick_to_bottom: true,
            dropped: 0,
            dirty: false,
        }
    }
}

impl LogPane {
    pub fn push(&mut self, record: &Record) {
        let crate::engine::telemetry::Event::Log { level, stage, msg } = &record.event else {
            return;
        };
        let row = LogRow {
            time: crate::engine::telemetry::local_time(record.at),
            level: *level,
            stage: *stage,
            msg: msg.clone(),
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        if row.matches(self.filter, &self.search) {
            self.filtered.push(seq);
        }
        self.rows.push_back(row);

        while self.rows.len() > MAX_ROWS {
            self.rows.pop_front();
            self.first_seq += 1;
        }
        // Evicted rows fall off the front of the filtered view too. Amortised
        // O(evicted), not O(total).
        let first = self.first_seq;
        if let Some(cut) = self.filtered.iter().position(|s| *s >= first) {
            if cut > 0 {
                self.filtered.drain(..cut);
            }
        } else if !self.filtered.is_empty() && *self.filtered.last().unwrap() < first {
            self.filtered.clear();
        }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.filtered.clear();
        self.first_seq = self.next_seq;
    }

    fn row(&self, seq: u64) -> Option<&LogRow> {
        self.rows.get((seq - self.first_seq) as usize)
    }

    /// Recompute the filtered view. Only on a filter or search change, never
    /// per frame.
    fn refilter(&mut self) {
        self.filtered.clear();
        for (i, row) in self.rows.iter().enumerate() {
            if row.matches(self.filter, &self.search) {
                self.filtered.push(self.first_seq + i as u64);
            }
        }
        self.dirty = false;
    }

    pub fn visible_count(&self) -> usize {
        self.filtered.len()
    }

    pub fn total_count(&self) -> usize {
        self.rows.len()
    }

    /// The currently visible rows as plain text, for copy-to-clipboard.
    ///
    /// Honours the filter, so "show me only ERR, copy that" is one click and a
    /// short paste rather than fifty thousand lines.
    pub fn visible_text(&self) -> String {
        let mut out = String::new();
        for seq in &self.filtered {
            if let Some(row) = self.row(*seq) {
                out.push_str(&format!(
                    "{}  {:<5}  {:<8}  {}\n",
                    row.time,
                    row.level.label(),
                    row.stage.label(),
                    row.msg
                ));
            }
        }
        out
    }

    /// Rows currently held at each level, for the chip counts.
    fn count_of(&self, level: Option<Level>) -> usize {
        match level {
            None => self.rows.len(),
            Some(l) => self.rows.iter().filter(|r| r.level == l).count(),
        }
    }

    /// The filter chips and the live search box.
    ///
    /// Each chip carries its count, as the mockup does: `ERR 3` is a glance
    /// rather than a click, and `ERR 0` is the reassurance you actually want.
    pub fn controls(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let mut changed = false;
            let mut pick: Option<Option<Level>> = None;

            if chip(
                ui,
                "ALL",
                self.count_of(None),
                self.filter.is_none(),
                theme::DIM,
            ) {
                pick = Some(None);
            }
            for level in [Level::Io, Level::Perf, Level::Ok, Level::Warn, Level::Err] {
                let selected = self.filter == Some(level);
                let (tag, _) = theme::level_colours(level);
                if chip(ui, level.label(), self.count_of(Some(level)), selected, tag) {
                    // Clicking the active chip clears it, so ERR is a toggle.
                    pick = Some(if selected { None } else { Some(level) });
                }
            }
            if let Some(f) = pick {
                self.filter = f;
                changed = true;
            }

            ui.add_space(4.0);
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .desired_width(ui.available_width().min(260.0))
                    .hint_text("filter rows…")
                    .font(theme::mono(theme::LOG)),
            );
            changed |= response.changed();

            if changed {
                self.dirty = true;
            }
        });
        if self.dirty {
            self.refilter();
        }
    }

    /// The line under the log: what is held, what is capped, where it is going.
    pub fn footer(&self, ui: &mut Ui, log_path: Option<&std::path::Path>) {
        ui.horizontal(|ui| {
            let where_to = match log_path {
                Some(p) => format!(
                    " · streaming to {}",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
                None => String::new(),
            };
            ui.label(
                RichText::new(format!(
                    "{} rows · ring buffer {}{where_to}",
                    theme::thousands(self.rows.len() as u64),
                    theme::thousands(MAX_ROWS as u64),
                ))
                .color(theme::DIMMER)
                .font(theme::mono(theme::SMALL)),
            );
            // The sink counted these and nothing ever showed them. If this view
            // is missing lines, say so here rather than let somebody read an
            // incomplete log as a complete one — the file on disk has them all.
            if self.dropped > 0 {
                ui.label(
                    RichText::new(format!(
                        "· {} event(s) dropped from this view — the log file is complete",
                        theme::thousands(self.dropped)
                    ))
                    .color(theme::WARN)
                    .font(theme::mono(theme::SMALL)),
                );
            }
        });
    }

    /// Render only the rows actually on screen.
    ///
    /// Four fixed columns, as the mockup's grid lays them out: time, level,
    /// stage, message. The message is tinted to match its level, not just the
    /// level tag -- a red line should read as red without hunting for the label.
    pub fn show(&mut self, ui: &mut Ui) {
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace) * 1.15;
        let total = self.filtered.len();

        let output = egui::Frame::default()
            .fill(theme::LOG_BG)
            .stroke(egui::Stroke::new(1.0, theme::LINE))
            .corner_radius(egui::CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(10, 7))
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.stick_to_bottom)
                    .show_rows(ui, row_height, total, |ui, range| {
                        for i in range {
                            let Some(seq) = self.filtered.get(i).copied() else {
                                continue;
                            };
                            let Some(row) = self.row(seq) else { continue };
                            let (tag, msg) = theme::level_colours(row.level);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 9.0;
                                col(ui, 82.0, &row.time, theme::DIMMER);
                                col(
                                    ui,
                                    52.0,
                                    &format!(
                                        "{} {}",
                                        theme::level_glyph(row.level),
                                        row.level.label()
                                    ),
                                    tag,
                                );
                                col(ui, 58.0, row.stage.label(), theme::DIMMER);
                                ui.label(
                                    RichText::new(&row.msg)
                                        .color(msg)
                                        .font(theme::mono(theme::LOG)),
                                );
                            });
                        }
                    })
            })
            .inner;

        // If the user scrolls away from the bottom, stop dragging them back.
        let at_bottom =
            output.state.offset.y >= output.content_size.y - output.inner_rect.height() - 4.0;
        self.stick_to_bottom = at_bottom;
    }
}

/// A fixed-width cell, so the log reads as columns rather than as ragged text.
fn col(ui: &mut Ui, width: f32, text: &str, colour: Color32) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.spacing().interact_size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_width(width);
            ui.label(
                RichText::new(text)
                    .color(colour)
                    .font(theme::mono(theme::LOG)),
            );
        },
    );
}

/// A filter chip carrying its count: `ERR 3`.
fn chip(ui: &mut Ui, text: &str, count: usize, selected: bool, colour: Color32) -> bool {
    let label = RichText::new(format!("{text}  {count}"))
        .color(if selected { colour } else { theme::DIM })
        .font(theme::mono(theme::SMALL));
    ui.selectable_label(selected, label).clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::telemetry::Event;

    /// Built directly rather than round-tripped through a channel: the eviction
    /// test pushes 50,000 of these, and a channel apiece makes it take a minute.
    fn record(level: Level, stage: Stage, msg: &str) -> Record {
        Record {
            at: chrono::Utc::now(),
            elapsed_ms: 0,
            event: Event::Log {
                level,
                stage,
                msg: msg.to_string(),
            },
        }
    }

    #[test]
    fn ignores_non_log_events() {
        let mut pane = LogPane::default();
        pane.push(&Record {
            at: chrono::Utc::now(),
            elapsed_ms: 0,
            event: Event::Phase {
                phase: crate::engine::telemetry::Phase::Copy,
            },
        });
        assert_eq!(pane.total_count(), 0);
    }

    #[test]
    fn filters_by_level_and_search() {
        let mut pane = LogPane::default();
        pane.push(&record(Level::Ok, Stage::Copy, "DSC00001.ARW copied"));
        pane.push(&record(Level::Err, Stage::Verify, "DSC00002.ARW mismatch"));
        pane.push(&record(Level::Ok, Stage::Verify, "DSC00003.ARW ok"));
        assert_eq!(pane.visible_count(), 3);

        pane.filter = Some(Level::Err);
        pane.refilter();
        assert_eq!(pane.visible_count(), 1, "ERR alone should be a short list");

        pane.filter = None;
        pane.search = "dsc00003".into();
        pane.refilter();
        assert_eq!(pane.visible_count(), 1, "search is case-insensitive");
    }

    /// The ring buffer must bound memory without corrupting the filtered view,
    /// which indexes by sequence number rather than by position.
    #[test]
    fn eviction_keeps_the_filtered_view_consistent() {
        let mut pane = LogPane::default();
        for i in 0..(MAX_ROWS + 500) {
            let level = if i % 100 == 0 {
                Level::Err
            } else {
                Level::Info
            };
            pane.push(&record(level, Stage::Copy, &format!("line {i}")));
        }
        assert_eq!(pane.total_count(), MAX_ROWS);
        assert_eq!(pane.visible_count(), MAX_ROWS);

        // Every surviving sequence number must still resolve to a real row.
        for seq in pane.filtered.clone() {
            assert!(pane.row(seq).is_some(), "seq {seq} dangled after eviction");
        }
        let newest = pane.row(*pane.filtered.last().unwrap()).unwrap();
        assert_eq!(newest.msg, format!("line {}", MAX_ROWS + 499));
    }

    #[test]
    fn filtered_view_survives_eviction_of_every_match() {
        let mut pane = LogPane::default();
        pane.push(&record(Level::Err, Stage::Copy, "the only error"));
        pane.filter = Some(Level::Err);
        pane.refilter();
        assert_eq!(pane.visible_count(), 1);

        for i in 0..(MAX_ROWS + 10) {
            pane.push(&record(Level::Info, Stage::Copy, &format!("line {i}")));
        }
        assert_eq!(pane.visible_count(), 0, "the matching row was evicted");
        assert_eq!(pane.total_count(), MAX_ROWS);
    }

    #[test]
    fn clear_resets_without_dangling_sequences() {
        let mut pane = LogPane::default();
        pane.push(&record(Level::Ok, Stage::Copy, "one"));
        pane.clear();
        assert_eq!(pane.total_count(), 0);
        assert_eq!(pane.visible_count(), 0);
        pane.push(&record(Level::Ok, Stage::Copy, "two"));
        assert_eq!(pane.visible_count(), 1);
        assert!(pane.row(*pane.filtered.last().unwrap()).is_some());
    }
}
