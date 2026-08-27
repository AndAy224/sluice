//! The window: app state, event drain, layout.
//!
//! Four regions. Everything except the log is fixed-height; the log takes the
//! remainder, because at every moment other than the final verdict the log is
//! the main event.
//!
//! Nothing is hidden behind a "details" toggle. If the program knows it, it is
//! on screen or in the log.

pub mod banner;
pub mod devices;
pub mod logpane;
pub mod pipeline;
pub mod theme;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use egui::containers::{CentralPanel, Panel};
use egui::{RichText, Ui};

use crate::engine::telemetry::{
    self, default_log_dir, session_id, Event, Phase, Record, Sink, Telemetry,
};
use crate::engine::verdict::VerdictReport;
use crate::engine::win;
use crate::engine::{run_job, DeviceId, JobConfig};

use devices::DeviceState;
use logpane::LogPane;
use pipeline::PipelineState;

/// Repaint interval while a job runs. Idle costs nothing, so the app can sit
/// open on battery without burning it.
const FRAME: Duration = Duration::from_millis(33);

struct RunningJob {
    rx: Receiver<Record>,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    sink: Option<Sink>,
    log_path: PathBuf,
    outcome: Arc<std::sync::Mutex<Option<FinishedRun>>>,
}

pub struct App {
    card1: String,
    card2: String,
    dest_a: String,
    dest_b: String,
    dest_c: String,
    label: String,

    job: Option<RunningJob>,
    phase: Phase,
    devices: BTreeMap<DeviceId, DeviceState>,
    pipeline: PipelineState,
    log: LogPane,
    verdict: Option<VerdictReport>,
    elapsed: f64,
    /// Job-elapsed at the current phase's start, so an estimate divides this
    /// phase's bytes by this phase's seconds.
    phase_started: f64,
    status: Option<String>,
    /// Per-chunk detail. Off by default; on whenever something is being
    /// diagnosed.
    trace: bool,
    /// A --start that has not fired yet. Consumed on the first frame.
    pending_start: bool,
    /// The user asked to close while a job was running; exit once it stops.
    close_when_idle: bool,
    /// The last finished run, kept so a format can be recorded against it.
    finished: Option<FinishedRun>,
    /// The note being typed into the format confirmation.
    format_note: String,
    /// Whether the confirmation is open.
    confirming_format: bool,
    /// Set once a format has been recorded for the finished run, so the offer
    /// stops being made without tearing down the rest of the panel.
    format_recorded: bool,
    /// Raised when a job ends, consumed by the next frame, which has a `Context`
    /// to send a viewport command through.
    notify_finished: bool,
    /// Where this window's JSONL log is written.
    log_dir: PathBuf,
    /// The title currently on the window, so it is only re-sent when it changes.
    title: String,
    /// A config this build refused to read must not then be overwritten by one
    /// it would write. Saying so is worthless without this half.
    config_refused: bool,

    /// Everything currently mounted, for the picker. Refreshed on demand rather
    /// than per frame -- enumerating volumes touches every drive.
    volumes: Vec<VolumeChoice>,
    /// Identity of whatever each field currently points at, resolved live so a
    /// letter swap is visible *before* the button is pressed rather than after
    /// the job has run against the wrong drive.
    identity: BTreeMap<DeviceId, win::VolumeInfo>,
    /// A path changed; identities need re-resolving at the top of the frame.
    paths_dirty: bool,
    /// Slots the config remembers but whose drive is not plugged in, so the
    /// window can name what is missing instead of silently leaving a box blank.
    absent: BTreeMap<DeviceId, String>,
    /// Raised by the drive watcher when a letter appears or disappears.
    drives_changed: Arc<AtomicBool>,
}

/// How often the watcher asks which drive letters exist.
///
/// One syscall against the drive-letter table, so the interval is chosen for
/// how long somebody is willing to stand there after plugging a drive in, not
/// for cost.
const DRIVE_POLL: Duration = Duration::from_millis(900);

/// The slot names the config file uses, paired with their device.
const SLOT_NAMES: [(&str, DeviceId); 5] = [
    ("C1", DeviceId::Card1),
    ("C2", DeviceId::Card2),
    ("A", DeviceId::DestA),
    ("B", DeviceId::DestB),
    ("C", DeviceId::DestC),
];

/// A run that has ended, kept so its cards can be recorded as erased.
struct FinishedRun {
    session: String,
    session_dirs: Vec<PathBuf>,
    /// The session JSON files this run wrote. Not rebuilt from `session` — see
    /// `JobOutcome::session_json`.
    session_json: Vec<PathBuf>,
    cards: Vec<crate::engine::history::DeviceRecord>,
    authorised: bool,
    log_path: PathBuf,
}

/// One entry in the drive picker.
#[derive(Debug, Clone)]
struct VolumeChoice {
    root: String,
    display: String,
    drive_type: win::DriveType,
    device_number: Option<u32>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            card1: String::new(),
            card2: String::new(),
            dest_a: String::new(),
            dest_b: String::new(),
            dest_c: String::new(),
            label: "session".into(),
            job: None,
            phase: Phase::Idle,
            devices: BTreeMap::new(),
            pipeline: PipelineState::default(),
            log: LogPane::default(),
            verdict: None,
            elapsed: 0.0,
            phase_started: 0.0,
            status: None,
            trace: false,
            pending_start: false,
            close_when_idle: false,
            finished: None,
            format_note: String::new(),
            confirming_format: false,
            format_recorded: false,
            notify_finished: false,
            log_dir: default_log_dir(),
            title: "sluice".into(),
            config_refused: false,
            volumes: Vec::new(),
            identity: BTreeMap::new(),
            paths_dirty: true,
            absent: BTreeMap::new(),
            drives_changed: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Paths to open the window with already filled in.
#[derive(Debug, Clone, Default)]
pub struct Prefill {
    pub card1: String,
    pub card2: String,
    pub dests: Vec<String>,
    pub label: Option<String>,
    /// Where the JSONL forensic log goes, when the shortcut names it.
    ///
    /// The CLI honoured `--log-dir` and the window silently ignored it, writing
    /// to the default instead — the same class of bug as `--destination`
    /// quietly halving the number of copies.
    pub log_dir: Option<PathBuf>,
    pub trace: bool,
    /// Begin the offload as soon as the window opens, for a shortcut that
    /// carries the night's setup and needs no clicks at all.
    pub start: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, prefill: Prefill) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut app = Self {
            card1: prefill.card1,
            card2: prefill.card2,
            trace: prefill.trace,
            pending_start: prefill.start,
            log_dir: prefill.log_dir.unwrap_or_else(default_log_dir),
            ..Self::default()
        };
        if let Some(label) = prefill.label {
            app.label = label;
        }
        for (slot, path) in [&mut app.dest_a, &mut app.dest_b, &mut app.dest_c]
            .into_iter()
            .zip(prefill.dests)
        {
            *slot = path;
        }
        app.watch_drives(&cc.egui_ctx);
        app
    }

    /// Notice a drive arriving or leaving, without being asked.
    ///
    /// Pressing Rescan after plugging something in is a step nobody should have
    /// to know about, and the window is idle at exactly the moment it matters —
    /// it repaints only while a job runs, so the wake-up has to come from here.
    ///
    /// A thread rather than `WM_DEVICECHANGE` because winit owns the window and
    /// its message loop, and a message-only window of our own would not receive
    /// the broadcast anyway. Polling the drive-letter mask costs one syscall and
    /// no device I/O; see [`win::drive_letter_mask`].
    fn watch_drives(&self, ctx: &egui::Context) {
        let flag = Arc::clone(&self.drives_changed);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("sluice-drive-watch".into())
            .spawn(move || {
                let mut last = win::drive_letter_mask();
                loop {
                    std::thread::sleep(DRIVE_POLL);
                    let now = win::drive_letter_mask();
                    if now != last {
                        last = now;
                        flag.store(true, Ordering::Relaxed);
                        // The window is asleep when idle, which is precisely
                        // when a drive gets plugged in.
                        ctx.request_repaint();
                    }
                }
            });
        // Losing the watcher costs the convenience, never the correctness:
        // Rescan does the same work by hand.
        debug_assert!(spawned.is_ok(), "drive watcher failed to start");
    }

    fn running(&self) -> bool {
        self.job.is_some()
    }

    fn config(&self) -> Result<JobConfig, String> {
        if self.card1.trim().is_empty() {
            return Err("card 1 is required".into());
        }
        let dest_roots: Vec<PathBuf> = [&self.dest_a, &self.dest_b, &self.dest_c]
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| PathBuf::from(s.trim()))
            .collect();
        if dest_roots.is_empty() {
            return Err("at least one destination is required".into());
        }
        Ok(JobConfig {
            card1: PathBuf::from(self.card1.trim()),
            card2: (!self.card2.trim().is_empty()).then(|| PathBuf::from(self.card2.trim())),
            dest_roots,
            label: self.label.clone(),
            log_dir: self.log_dir.clone(),
            probe: None,
            history_path: None,
        })
    }

    fn start(&mut self) {
        let cfg = match self.config() {
            Ok(c) => c,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };

        self.verdict = None;
        self.phase = Phase::Scan;
        self.devices.clear();
        self.pipeline.reset();
        self.log.clear();
        self.status = None;

        let (tel, engine_rx) = Telemetry::with_trace(self.trace);
        let (ui_tx, ui_rx) = crossbeam_channel::bounded::<Record>(65_536);
        let log_path = telemetry::log_path(&cfg.log_dir, &session_id(chrono::Utc::now()));
        let sink = match Sink::spawn(log_path.clone(), engine_rx, Some(ui_tx)) {
            Ok(s) => s,
            Err(e) => {
                self.status = Some(format!("could not open the session log: {e:#}"));
                return;
            }
        };

        let cancel = crate::engine::cancel_flag();
        let job_cancel = Arc::clone(&cancel);
        let job_log_path = log_path.clone();
        let outcome_slot: Arc<std::sync::Mutex<Option<FinishedRun>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot = Arc::clone(&outcome_slot);
        let handle = std::thread::spawn(move || {
            // The verdict reaches the UI through the event stream; an error
            // reaches it as a log line, so nothing is swallowed here.
            match run_job(&cfg, &tel, &job_cancel) {
                Ok(outcome) => {
                    *slot.lock().unwrap() = Some(FinishedRun {
                        session: outcome.session,
                        session_dirs: outcome.session_dirs,
                        session_json: outcome.session_json,
                        cards: outcome.cards,
                        authorised: outcome.verdict.state.authorises_erase(),
                        log_path: job_log_path,
                    });
                }
                Err(e) => {
                    // A refusal -- a filename Windows cannot store, the same
                    // card in both slots, a full drive, another sluice already
                    // writing here -- never reaches the verdict phase, so it
                    // used to arrive as one ERR line in a scrolling log while
                    // the banner still read RUNNING. The banner is the only
                    // thing some people read. Give the refusal one.
                    let reasons = format!("{e:#}")
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect();
                    tel.emit(telemetry::Event::Verdict(VerdictReport::refused(reasons)));
                    tel.err(telemetry::Stage::Verdict, format!("job failed: {e:#}"));
                }
            }
            drop(tel);
        });

        // The setup is remembered only once a run actually begins, so a
        // half-typed path never becomes tomorrow night's default.
        self.save_config();

        self.finished = None;
        self.confirming_format = false;
        self.format_recorded = false;
        self.notify_finished = false;
        self.job = Some(RunningJob {
            rx: ui_rx,
            cancel,
            handle: Some(handle),
            sink: Some(sink),
            log_path,
            outcome: outcome_slot,
        });
    }

    fn cancel(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.status = Some("cancelling — partial destination files are being removed".into());
        }
    }

    /// Whether a cancel has been asked for and the job has not ended yet.
    fn cancelling(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|j| j.cancel.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Drain everything waiting, then decide whether the job has ended.
    fn drain(&mut self) {
        let (records, finished, drained) = {
            let Some(job) = &self.job else { return };
            let records: Vec<Record> = job.rx.try_iter().collect();
            let finished = job.handle.as_ref().map(|h| h.is_finished()).unwrap_or(true);
            (records, finished, job.rx.is_empty())
        };
        for record in &records {
            self.apply(record);
        }

        // Only tear down once the channel is also drained, or the last few
        // records -- the verdict among them -- would be lost on the way out.
        if !(finished && drained) {
            return;
        }
        let mut job = self.job.take().expect("checked above");
        if let Some(h) = job.handle.take() {
            let _ = h.join();
        }
        if let Some(sink) = job.sink.take() {
            if let Err(e) = sink.join() {
                self.status = Some(format!("session log: {e:#}"));
            }
        }
        let leftovers: Vec<Record> = job.rx.try_iter().collect();
        for record in &leftovers {
            self.apply(record);
        }
        self.finished = job.outcome.lock().ok().and_then(|mut o| o.take());
        self.status = Some(format!("log written to {}", job.log_path.display()));
        // Keep-awake exists so a 40-minute job can be walked away from, and
        // until now the job ended with an unchanged taskbar button. Note
        // `ES_DISPLAY_REQUIRED` is deliberately not asserted, so the screen may
        // well be asleep -- the taskbar is the only channel that survives that.
        self.notify_finished = true;
    }

    fn apply(&mut self, record: &Record) {
        self.elapsed = record.elapsed_ms as f64 / 1000.0;
        self.log.push(record);
        match &record.event {
            Event::Phase { phase } => {
                // Each phase measures its own work. Byte counters and the clock
                // used to run cumulatively across the whole job, so verify
                // began already "past" the copy's total and the estimate read
                // "about 0s left" from its first second to its last.
                if *phase != self.phase {
                    for d in self.devices.values_mut() {
                        d.bytes = 0;
                        // The denominator belongs to the phase as much as the
                        // numerator does. Carrying copy's over into verify
                        // would draw every row against the wrong total.
                        d.plan_bytes = 0;
                    }
                    self.pipeline.file_index = 0;
                    self.pipeline.trail.clear();
                    self.phase_started = self.elapsed;
                }
                self.phase = *phase;
                // Cancelling no longer ends the moment the writers stop: every
                // file already copied still has to be flushed and stamped, or a
                // resumed run would copy the whole card again. On a card of
                // stills that tail is not instant, and the standing message --
                // "partial destination files are being removed" -- would be
                // describing the opposite of what the drive is doing.
                if *phase == Phase::Flush && self.cancelling() {
                    self.status = Some(
                        "cancelling — finishing the files already copied so a resumed run can \
                         skip them"
                            .into(),
                    );
                }
            }
            Event::Device { id, info } => {
                self.devices.entry(*id).or_default().info = Some((**info).clone());
            }
            Event::DevicePlan { dev, bytes } => {
                self.devices.entry(*dev).or_default().plan_bytes = *bytes;
            }
            Event::Bytes { dev, delta } => {
                self.devices.entry(*dev).or_default().bytes += delta;
                // Sampled here rather than per frame: this is where the number
                // actually changes, and the estimate wants a rate measured over
                // real progress rather than over redraws.
                let moved = pipeline::moved_in_phase(&self.devices, self.phase);
                self.pipeline
                    .note_progress((self.elapsed - self.phase_started).max(0.0), moved);
            }
            Event::Throughput { dev, mbps } => {
                self.devices
                    .entry(*dev)
                    .or_default()
                    .record_throughput(*mbps, self.elapsed);
            }
            Event::Queue { dev, depth, cap } => {
                self.pipeline.queues.insert(*dev, (*depth, *cap));
            }
            Event::Plan { files, bytes } => {
                self.pipeline.file_total = *files;
                self.pipeline.bytes_total = *bytes;
            }
            Event::FileStart { idx, rel, size } => {
                self.pipeline.current_file =
                    Some(rel.rsplit('/').next().unwrap_or(rel).to_string());
                self.pipeline.current_size = *size;
                self.pipeline.file_index = *idx + 1;
            }
            Event::Verdict(v) => self.verdict = Some(v.clone()),
            Event::FileDone { .. } | Event::Log { .. } => {}
        }
    }

    /// `sluice 0.1.0 ............ session <date> · <label>`
    fn titlebar(&self, ui: &mut Ui) {
        egui::Frame::default()
            .fill(theme::PANEL)
            .inner_margin(egui::Margin::symmetric(13, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("sluice").color(theme::TEXT).strong());
                    ui.label(
                        RichText::new(env!("CARGO_PKG_VERSION"))
                            .color(theme::DIMMER)
                            .font(theme::mono(theme::SMALL)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "session {}  ·  {}",
                                chrono::Local::now().format("%Y-%m-%d"),
                                if self.label.trim().is_empty() {
                                    "unnamed"
                                } else {
                                    self.label.trim()
                                }
                            ))
                            .color(theme::DIM)
                            .font(theme::mono(theme::SMALL)),
                        );
                    });
                });
            });
    }

    /// Fill any empty slot from the remembered setup, resolving by serial.
    ///
    /// A slot the caller pre-filled on the command line wins: an explicit
    /// argument is a stronger statement than a memory.
    fn apply_config(&mut self) {
        let (cfg, refused) = crate::engine::config::Config::load_reporting();
        if let Some(msg) = refused {
            // Set before the early return below: a refused config has no slots,
            // so the message would otherwise never reach a surface.
            self.status = Some(msg);
            self.config_refused = true;
        }
        if cfg.slots.is_empty() {
            return;
        }
        if self.label == "session" && !cfg.label.is_empty() {
            self.label = cfg.label.clone();
        }
        let mounted = win::mounted_volumes();
        self.resolve_remembered(&cfg, &mounted);
    }

    /// Fill any empty slot from the remembered setup, and recompute what is
    /// still missing, against the volumes given.
    ///
    /// Split out of [`Self::apply_config`] so that Rescan can run it too. It
    /// only ran at startup, so a slot left empty because its drive was not
    /// plugged in stayed empty for the life of the window: plugging the drive
    /// in and pressing Rescan re-enumerated the picker and changed nothing
    /// else, and the only way through was to restart the program. Reported
    /// from live use.
    ///
    /// Takes the mounted list rather than asking for it, so one Rescan
    /// enumerates once — and so this is testable without the hardware.
    fn resolve_remembered(
        &mut self,
        cfg: &crate::engine::config::Config,
        mounted: &[crate::engine::win::MountedVolume],
    ) {
        let (found, absent) = cfg.resolve_all(mounted);

        for (slot, dev) in SLOT_NAMES {
            let field = self.slot_mut(dev);
            if !field.trim().is_empty() {
                continue;
            }
            if let Some(path) = found.get(slot) {
                *field = path.display().to_string();
            }
        }
        self.absent = absent
            .into_iter()
            .filter_map(|(slot, note)| {
                SLOT_NAMES
                    .iter()
                    .find(|(s, _)| *s == slot)
                    .map(|(_, dev)| (*dev, note))
            })
            .collect();
        self.paths_dirty = true;

        self.status = if self.absent.is_empty() {
            None
        } else {
            Some(format!(
                "{} remembered drive(s) not connected",
                self.absent.len()
            ))
        };
    }

    /// Remember the current setup, so tomorrow night needs no decisions.
    fn save_config(&self) {
        // A config this build refused to read must not be replaced by one it
        // would. Overwriting it destroys the newer sluice's remembered setup --
        // exactly the silent reset the version stamp exists to prevent.
        if self.config_refused {
            return;
        }
        let mut cfg = crate::engine::config::Config {
            label: self.label.clone(),
            trace: self.trace,
            ..Default::default()
        };
        for (slot, dev) in SLOT_NAMES {
            let path = self.slot(dev).trim();
            if !path.is_empty() {
                cfg.remember(slot, std::path::Path::new(path));
            }
        }
        // Best effort: failing to remember must never stop an offload.
        let _ = cfg.save();
    }

    fn slot(&self, dev: DeviceId) -> &String {
        match dev {
            DeviceId::Card1 => &self.card1,
            DeviceId::Card2 => &self.card2,
            DeviceId::DestA => &self.dest_a,
            DeviceId::DestB => &self.dest_b,
            DeviceId::DestC => &self.dest_c,
        }
    }

    fn slot_mut(&mut self, dev: DeviceId) -> &mut String {
        match dev {
            DeviceId::Card1 => &mut self.card1,
            DeviceId::Card2 => &mut self.card2,
            DeviceId::DestA => &mut self.dest_a,
            DeviceId::DestB => &mut self.dest_b,
            DeviceId::DestC => &mut self.dest_c,
        }
    }

    /// Rescan what is plugged in, and re-resolve the remembered setup against it.
    ///
    /// Both halves matter, and only the first was here. Re-enumerating refreshed
    /// the *picker*; the slot itself stayed empty and its "not connected" note
    /// stayed with it, so plugging in a missing drive and pressing Rescan looked
    /// like it did nothing until the program was restarted.
    fn refresh_volumes(&mut self) {
        let mounted = win::mounted_volumes();
        self.volumes = mounted
            .iter()
            .map(|v| VolumeChoice {
                root: v.info.root.clone(),
                display: v.describe(),
                drive_type: v.drive_type,
                device_number: v.info.device_number,
            })
            .collect();

        // A slot the operator typed is never overwritten, so this only fills
        // what is still blank — which is exactly what a missing drive left.
        let (cfg, _) = crate::engine::config::Config::load_reporting();
        if !cfg.slots.is_empty() {
            self.resolve_remembered(&cfg, &mounted);
        }
        self.paths_dirty = true;
    }

    /// Resolve what each field currently points at.
    ///
    /// This is the whole point of the picker: the identity is on screen before
    /// the button is pressed, so a letter swap between two identical LaCies is
    /// something you notice rather than something you discover afterwards.
    fn resolve_identity(&mut self) {
        self.identity.clear();
        for (id, path) in [
            (DeviceId::Card1, &self.card1),
            (DeviceId::Card2, &self.card2),
            (DeviceId::DestA, &self.dest_a),
            (DeviceId::DestB, &self.dest_b),
            (DeviceId::DestC, &self.dest_c),
        ] {
            let path = path.trim();
            if path.is_empty() {
                continue;
            }
            if let Ok(info) = win::volume_info(std::path::Path::new(path)) {
                self.identity.insert(id, info);
            }
        }
        self.paths_dirty = false;
    }

    /// Whether the two required destinations currently point at one physical
    /// drive. Answered before the run, not after it.
    fn destinations_collide(&self) -> Option<String> {
        let a = self.identity.get(&DeviceId::DestA)?;
        let b = self.identity.get(&DeviceId::DestB)?;
        match win::distinctness(a, b) {
            win::Distinctness::SameDevice => Some(format!(
                "DEST A and DEST B are the same physical drive ({} / {}) — that is one copy, \
                 not two, and the verdict will refuse to authorise a format",
                a.serial_hex(),
                b.serial_hex()
            )),
            win::Distinctness::Unproven(why) => Some(format!("unproven: {why}")),
            win::Distinctness::Distinct => None,
        }
    }

    /// One row per source and destination: label, path, a drive picker, a folder
    /// browser, and the live identity of whatever is currently selected.
    fn paths(&mut self, ui: &mut Ui) {
        let running = self.running();
        // Destructure so the picker can read `volumes` while a field is &mut.
        let App {
            card1,
            card2,
            dest_a,
            dest_b,
            dest_c,
            volumes,
            identity,
            paths_dirty,
            absent,
            ..
        } = self;

        // Which physical disk each slot is already pointing at, so the picker
        // can flag a choice that would land two slots on one drive.
        let taken: Vec<(DeviceId, u32)> = DeviceId::CARDS
            .iter()
            .chain(DeviceId::DESTS.iter())
            .filter_map(|d| {
                identity
                    .get(d)
                    .and_then(|i| i.device_number)
                    .map(|n| (*d, n))
            })
            .collect();

        let rows: [(DeviceId, &str, &mut String); 5] = [
            (DeviceId::Card1, "CARD 1", card1),
            (DeviceId::Card2, "CARD 2", card2),
            (DeviceId::DestA, "DEST A", dest_a),
            (DeviceId::DestB, "DEST B", dest_b),
            (DeviceId::DestC, "DEST C", dest_c),
        ];

        for (id, label, field) in rows {
            path_row(
                ui,
                id,
                label,
                field,
                volumes,
                identity,
                &taken,
                absent.get(&id).map(String::as_str),
                running,
                paths_dirty,
            );
        }
    }

    /// The other end of the forensic trail: which cards were actually erased.
    ///
    /// Offered only after a verdict that authorises one, and only after the
    /// fact -- the design puts a night's sleep and a human spot-check between
    /// the verdict and the erase, so this records what you did rather than
    /// doing it. sluice never formats anything.
    fn format_confirmation(&mut self, ui: &mut Ui) {
        let Some(finished) = &self.finished else {
            return;
        };
        if !finished.authorised || finished.cards.is_empty() || self.format_recorded {
            return;
        }

        if !self.confirming_format {
            ui.horizontal(|ui| {
                if ui
                    .button("Record format…")
                    .on_hover_text(
                        "After you have formatted the cards in-camera, record which ones. \
                         If a file turns up corrupt months later, this is how you \
                         reconstruct what happened.",
                    )
                    .clicked()
                {
                    self.confirming_format = true;
                }
                ui.label(
                    RichText::new("sluice never formats anything — this records what you did")
                        .color(theme::DIMMER)
                        .font(theme::mono(theme::SMALL)),
                );
            });
            return;
        }

        // Copied out before the closure, so the panel can take &mut self.
        let cards: Vec<String> = finished
            .cards
            .iter()
            .map(|c| {
                format!(
                    "{} ({})",
                    if c.label.is_empty() {
                        &c.slot
                    } else {
                        &c.label
                    },
                    c.serial_hex()
                )
            })
            .collect();
        let session = finished.session.clone();
        let jsons = finished.session_json.clone();
        let records = finished.cards.clone();

        theme::panel_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!("Record that you erased: {}", cards.join(", ")))
                    .color(theme::TEXT)
                    .font(theme::mono(theme::LOG)),
            );
            ui.horizontal(|ui| {
                ui.label(RichText::new("note").color(theme::DIM).size(theme::SMALL));
                ui.add(
                    egui::TextEdit::singleline(&mut self.format_note)
                        .desired_width(360.0)
                        .hint_text("e.g. next morning, spot-checked both drives")
                        .font(theme::mono(theme::LOG)),
                );
                if ui.button("Confirm").clicked() {
                    let result = crate::engine::history::record_format(
                        &session,
                        &jsons,
                        records.clone(),
                        &self.format_note,
                    );
                    self.status = Some(match result {
                        Ok(()) => format!("recorded: erased {}", cards.join(", ")),
                        Err(e) => format!("could not record the format: {e:#}"),
                    });
                    self.confirming_format = false;
                    self.format_note.clear();
                    // Deliberately does NOT clear `finished`. Clearing it took
                    // the export-bundle button away at the exact moment the
                    // record became complete -- and a bundle exported *after*
                    // the erase is the only one that proves the erase happened.
                    // `format_recorded` hides the offer instead, so the panel
                    // stops asking without taking anything else with it.
                    self.format_recorded = true;
                }
                if ui.button("Cancel").clicked() {
                    self.confirming_format = false;
                }
            });
        });
    }

    fn actions(&mut self, ui: &mut Ui) {
        let running = self.running();

        // A collision between the two required destinations is worth saying out
        // loud before the button is pressed, not only in the verdict afterwards.
        if !running {
            if let Some(warning) = self.destinations_collide() {
                ui.label(
                    RichText::new(format!("{} {warning}", theme::glyphs().warn))
                        .color(theme::WARN)
                        .font(theme::mono(theme::SMALL)),
                );
            }
        }

        ui.horizontal(|ui| {
            if running {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }
                ui.label(
                    RichText::new(format!("{}  {:.0}s", self.phase.label(), self.elapsed))
                        .color(theme::DEST)
                        .font(theme::mono(theme::LOG)),
                );
            } else {
                if ui.button("Offload").clicked() {
                    self.start();
                }
                ui.label(RichText::new("label").color(theme::DIM).size(theme::SMALL));
                ui.add_enabled(
                    true,
                    egui::TextEdit::singleline(&mut self.label)
                        .desired_width(140.0)
                        .font(theme::mono(theme::LOG)),
                );
            }
            if ui
                .add_enabled(!running, egui::Button::new("Rescan"))
                .on_hover_text("Re-enumerate mounted drives, after plugging something in")
                .clicked()
            {
                self.refresh_volumes();
            }
            ui.add_enabled(!running, egui::Checkbox::new(&mut self.trace, "--trace"))
                .on_hover_text(
                    "Per-chunk reads and sync_all latency. Off by default because it roughly \
                     triples log volume; on whenever something is being diagnosed.",
                );
            let can_export = self.finished.is_some();
            if can_export
                && ui
                    .button("export bundle")
                    .on_hover_text(
                        "Gather the session log, both manifests, the device history and                          system details into one folder -- what you would attach to a bug                          report against your own code in six months.",
                    )
                    .clicked()
            {
                if let Some(f) = self.finished.as_ref() {
                    let out = crate::engine::history::data_dir();
                    let result = crate::engine::history::export_bundle(
                        &f.session,
                        &f.session_dirs,
                        &f.log_path,
                        &out,
                    );
                    self.status = Some(match result {
                        Ok(p) => format!("bundle written to {}", p.display()),
                        Err(e) => format!("could not export the bundle: {e:#}"),
                    });
                }
            }
            if ui.button("copy filtered").clicked() {
                // The filtered view, so "ERR only, copy that" is one click.
                ui.ctx().copy_text(self.log.visible_text());
                self.status = Some(format!("{} rows copied", self.log.visible_count()));
            }
            if let Some(status) = &self.status {
                ui.label(
                    RichText::new(status)
                        .color(theme::DIMMER)
                        .font(theme::mono(theme::SMALL)),
                );
            }
        });
    }
}

/// Which *other* slot is already pointing at this physical disk, if any.
///
/// A slot never collides with itself: re-picking the drive a row already has is
/// not a mistake.
fn colliding_slot(
    id: DeviceId,
    device: Option<u32>,
    taken: &[(DeviceId, u32)],
) -> Option<DeviceId> {
    let device = device?;
    taken
        .iter()
        .find(|(other, n)| *other != id && *n == device)
        .map(|(other, _)| *other)
}

#[allow(clippy::too_many_arguments)]
fn path_row(
    ui: &mut Ui,
    id: DeviceId,
    label: &str,
    value: &mut String,
    volumes: &[VolumeChoice],
    identity: &BTreeMap<DeviceId, win::VolumeInfo>,
    taken: &[(DeviceId, u32)],
    absent: Option<&str>,
    disabled: bool,
    dirty: &mut bool,
) {
    let hue = theme::device_colour(id);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(74.0, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_width(74.0);
                ui.label(
                    RichText::new(label)
                        .color(hue)
                        .font(theme::mono(theme::SMALL)),
                );
            },
        );

        // --- the drive picker -------------------------------------------
        // Ordered so the plausible thing for this slot comes first: removable
        // media for a card, fixed drives for a destination.
        let mut sorted: Vec<&VolumeChoice> = volumes.iter().collect();
        let wants_card = id.is_card();
        sorted.sort_by_key(|v| {
            let preferred = if wants_card {
                v.drive_type.is_card_like()
            } else {
                v.drive_type.is_dest_like()
            };
            (!preferred, v.root.clone())
        });

        let mut picked: Option<String> = None;
        egui::ComboBox::from_id_salt(("drive", id))
            .width(24.0)
            .selected_text("")
            .height(320.0)
            .show_ui(ui, |ui| {
                ui.set_min_width(430.0);
                if sorted.is_empty() {
                    ui.label(
                        RichText::new("nothing mounted — press Rescan")
                            .color(theme::DIMMER)
                            .font(theme::mono(theme::SMALL)),
                    );
                }
                for v in sorted {
                    let preferred = if wants_card {
                        v.drive_type.is_card_like()
                    } else {
                        v.drive_type.is_dest_like()
                    };
                    // Two slots on one physical disk is the mistake this whole
                    // program exists to refuse, so it is flagged at the moment
                    // of choosing rather than in the verdict twenty minutes on.
                    let collides = colliding_slot(id, v.device_number, taken);
                    let colour = match (collides, preferred) {
                        (Some(_), _) => theme::WARN,
                        (None, true) => theme::TEXT,
                        (None, false) => theme::DIM,
                    };
                    let text = match collides {
                        Some(other) => format!("{}   — same drive as {}", v.display, other.title()),
                        None => v.display.clone(),
                    };
                    let text = RichText::new(text)
                        .color(colour)
                        .font(theme::mono(theme::LOG));
                    if ui.selectable_label(value.trim() == v.root, text).clicked() {
                        picked = Some(v.root.clone());
                    }
                }
            });
        if let Some(root) = picked {
            *value = root;
            *dirty = true;
        }

        // --- the path itself ---------------------------------------------
        let meta_width = 320.0;
        let field_width = (ui.available_width() - meta_width - 36.0).max(140.0);
        let response = ui.add_enabled(
            !disabled,
            egui::TextEdit::singleline(value)
                .desired_width(field_width)
                .hint_text(if id == DeviceId::DestC {
                    "optional — laptop SSD"
                } else {
                    "pick a drive, or browse to a folder"
                })
                .font(theme::mono(theme::LOG)),
        );
        if response.changed() {
            *dirty = true;
        }

        // --- browse, for a subfolder rather than a drive root -------------
        if ui
            .add_enabled(!disabled, egui::Button::new("…"))
            .on_hover_text("Browse for a folder")
            .clicked()
        {
            let mut dialog = rfd::FileDialog::new().set_title(format!("{label} — choose a folder"));
            let current = std::path::Path::new(value.trim());
            if !value.trim().is_empty() && current.is_dir() {
                dialog = dialog.set_directory(current);
            }
            if let Some(dir) = dialog.pick_folder() {
                *value = dir.display().to_string();
                *dirty = true;
            }
        }

        // --- what that path actually is -----------------------------------
        let (meta, colour) = match identity.get(&id) {
            Some(info) => {
                let label = if info.volume_label_or_none().is_empty() {
                    "(no label)".to_string()
                } else {
                    info.volume_label_or_none().to_string()
                };
                let disk = match info.device_number {
                    Some(n) => format!("disk {n}"),
                    None => "disk ?".into(),
                };
                (
                    format!(
                        "{label} · {} · {} · {disk}",
                        info.filesystem,
                        info.serial_hex()
                    ),
                    theme::DIM,
                )
            }
            // A drive the config remembers but that is not plugged in says so
            // by name, rather than leaving an empty box to be puzzled over.
            None if value.trim().is_empty() => match absent {
                Some(note) => (note.to_string(), theme::WARN),
                None if id == DeviceId::DestC => ("optional".to_string(), theme::DIMMER),
                None => ("not selected".to_string(), theme::DIMMER),
            },
            None => ("path not found".to_string(), theme::ERR),
        };
        ui.add(
            egui::Label::new(
                RichText::new(meta)
                    .color(colour)
                    .font(theme::mono(theme::SMALL)),
            )
            .truncate(),
        );
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // The font atlas only exists once a frame is running, so the symbol set
        // is resolved here rather than at construction.
        theme::ensure_glyphs(ui.ctx());

        // A drive arrived or left. Held until the job ends rather than acted on
        // mid-copy: re-enumerating queries every volume, and the drive being
        // written to is one of them.
        if self.drives_changed.load(Ordering::Relaxed) && !self.running() {
            self.drives_changed.store(false, Ordering::Relaxed);
            let was_missing: Vec<DeviceId> = self.absent.keys().copied().collect();
            self.refresh_volumes();
            let arrived: Vec<&str> = was_missing
                .iter()
                .filter(|d| !self.absent.contains_key(d))
                .map(|d| d.title())
                .collect();
            if !arrived.is_empty() {
                self.status = Some(format!("{} connected", arrived.join(", ")));
            }
        }

        // Closing the window mid-job would leave a detached thread writing into
        // a destination with nobody left to clean up after it. Hold the close,
        // cancel, and let it go through once the job has actually stopped.
        if ui.ctx().input(|i| i.viewport().close_requested()) && self.running() {
            self.cancel();
            self.close_when_idle = true;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status =
                Some("cancelling before exit — removing partial destination files".into());
        }
        if self.close_when_idle && !self.running() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.volumes.is_empty() && !self.running() {
            self.refresh_volumes();
            self.apply_config();
        }
        if self.paths_dirty {
            self.resolve_identity();
        }
        if std::mem::take(&mut self.pending_start) {
            self.start();
        }
        self.drain();
        if std::mem::take(&mut self.notify_finished) {
            // Keep-awake exists so a 40-minute job can be walked away from, and
            // the job used to end with an unchanged taskbar button. Note that
            // `ES_DISPLAY_REQUIRED` is deliberately not asserted, so the screen
            // may well be asleep -- the taskbar is the only channel that
            // survives that. `Critical` rather than `Informational` because the
            // thing waiting to be read decides whether somebody erases a card.
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Critical,
                ));
        }
        // Derived from state every frame rather than stamped once when a job
        // ends. Stamping left the previous run's verdict in the title bar,
        // alt-tab and taskbar for the whole of the *next* job -- and a taskbar
        // reading SAFE TO FORMAT over a copy that has not finished is the one
        // sentence this program must never show at the wrong moment.
        let want = match self.verdict.as_ref().filter(|_| !self.running()) {
            Some(v) => format!("sluice — {}", v.headline()),
            None => "sluice".to_string(),
        };
        if want != self.title {
            self.title = want.clone();
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(want));
        }

        // Title bar.
        Panel::top("titlebar")
            .frame(egui::Frame::default().fill(theme::PANEL))
            .show(ui, |ui| self.titlebar(ui));

        // 1 — sources, destinations, and the device strip.
        Panel::top("top").frame(theme::bg_frame()).show(ui, |ui| {
            theme::section_label(ui, "Sources & destinations");
            self.paths(ui);
            ui.add_space(4.0);
            self.actions(ui);
            ui.add_space(2.0);
            theme::section_label(ui, "Devices");
            devices::strip(ui, &self.devices, self.elapsed);
            ui.add_space(2.0);
        });

        // 4 — the verdict banner: a fixed footer that never scrolls and is never
        // competing for attention. Claimed before the log, so a long log can
        // never push it off screen.
        Panel::bottom("verdict")
            .frame(theme::bg_frame())
            .show(ui, |ui| {
                banner::show(ui, self.verdict.as_ref(), self.phase, self.elapsed);
                self.format_confirmation(ui);
                ui.add_space(4.0);
            });

        // 2 — the pipeline monitor.
        Panel::top("pipeline")
            .frame(theme::bg_frame())
            .show(ui, |ui| {
                theme::section_label(ui, "Pipeline");
                pipeline::show(
                    ui,
                    &self.pipeline,
                    &self.devices,
                    self.phase,
                    // This phase's seconds, not the job's: dividing verify's
                    // bytes by a clock that already counted the copy made the
                    // rate look several times slower than the drives were.
                    (self.elapsed - self.phase_started).max(0.0),
                    self.elapsed,
                );
                ui.add_space(2.0);
            });

        // 3 — the log takes everything left. This is the main event at every
        // moment other than the final verdict.
        CentralPanel::default()
            .frame(theme::bg_frame())
            .show(ui, |ui| {
                theme::section_label(ui, "Log");
                self.log.controls(ui);
                ui.add_space(4.0);
                let log_path = self.job.as_ref().map(|j| j.log_path.clone());
                Panel::bottom("logfoot")
                    .frame(egui::Frame::default().inner_margin(egui::Margin::symmetric(0, 4)))
                    .show(ui, |ui| {
                        self.log.footer(ui, log_path.as_deref());
                    });
                self.log.dropped = self
                    .job
                    .as_ref()
                    .and_then(|j| j.sink.as_ref())
                    .map(|s| s.ui_dropped())
                    .unwrap_or(self.log.dropped);
                self.log.show(ui);
            });

        // Repaint while working, and not at all when idle, so the app can sit
        // open on battery without burning it.
        if self.running() {
            ui.ctx().request_repaint_after(FRAME);
        }
    }
}

/// Open the window, optionally with the paths already filled in.
pub fn run_with(prefill: Prefill) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size((1280.0, 900.0)))
            .with_min_inner_size(window_size((980.0, 640.0)))
            .with_title("sluice")
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "sluice",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, prefill)))),
    )
}

/// The icon the window and the taskbar wear.
///
/// Raw RGBA rather than a PNG, so the window needs no image decoder at all —
/// 16 KB of bytes against a decoding dependency and a parsing surface, for a
/// picture whose only job is to be recognisable. The `.ico` beside it is what
/// Explorer reads, embedded by `build.rs`.
fn window_icon() -> egui::IconData {
    const SIDE: u32 = 64;
    let rgba = include_bytes!("../../assets/icon-64.rgba");
    debug_assert_eq!(rgba.len(), (SIDE * SIDE * 4) as usize);
    egui::IconData {
        rgba: rgba.to_vec(),
        width: SIDE,
        height: SIDE,
    }
}

/// A default window size that fits the screen it will open on.
///
/// The sizes below were hardcoded and nothing asked how big the desktop was. On
/// a 1080p laptop at Windows' default 150% scaling the desktop is 1280x720
/// *points*, so a 1280x900 window ran a quarter of the way off the bottom --
/// and the part that goes is the bottom panel, which is the verdict banner.
/// `banner.rs` puts it there precisely so a long log can never push it off
/// screen; the window geometry was quietly undoing that.
///
/// The scale is unknown before the window exists, so this uses the system DPI
/// rather than the viewport's. It only needs to be close enough to keep the
/// banner on screen.
fn window_size(preferred: (f32, f32)) -> [f32; 2] {
    let scale = crate::engine::win::system_scale();
    let (w, h) = crate::engine::win::fit_to_screen(preferred, scale);
    [w, h]
}

/// Open the window.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size((1280.0, 860.0)))
            .with_min_inner_size(window_size((980.0, 620.0)))
            .with_title("sluice")
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "sluice",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, Prefill::default())))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(root: &str, serial: u32, label: &str) -> win::MountedVolume {
        win::MountedVolume {
            info: win::VolumeInfo {
                root: root.into(),
                label: label.into(),
                serial,
                filesystem: "exFAT".into(),
                sector_size: 4096,
                guid: None,
                device_number: Some(serial),
            },
            drive_type: win::DriveType::Fixed,
            free_bytes: 3_610_000_000_000,
            total_bytes: 4_000_000_000_000,
        }
    }

    fn remembering(pairs: &[(&str, u32, &str)]) -> crate::engine::config::Config {
        let mut cfg = crate::engine::config::Config::default();
        for (slot, serial, label) in pairs {
            cfg.slots.insert(
                (*slot).to_string(),
                crate::engine::config::SlotMemory {
                    serial: *serial,
                    label: (*label).to_string(),
                    subpath: String::new(),
                },
            );
        }
        cfg
    }

    /// Plug the missing drive in, press Rescan, and it resolves.
    ///
    /// Reported from live use: it did not. Rescan re-enumerated the picker and
    /// nothing else, so a slot left empty because its drive was absent at
    /// startup stayed empty and kept its "not connected" note, and the only way
    /// through was to restart the program.
    #[test]
    fn rescan_resolves_a_drive_that_was_missing_at_startup() {
        let cfg = remembering(&[("A", 0x0129D1EE, "MT-A"), ("B", 0x0129D190, "MT-B")]);
        let mut app = App::default();

        // Startup with only one of the two plugged in.
        app.resolve_remembered(&cfg, &[mounted("G:\\", 0x0129D190, "MT-B")]);
        assert_eq!(app.dest_b, "G:\\");
        assert!(app.dest_a.is_empty(), "the absent drive fills nothing");
        assert!(app.absent.contains_key(&DeviceId::DestA));
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("not connected"));

        // The operator plugs it in and presses Rescan.
        app.resolve_remembered(
            &cfg,
            &[
                mounted("F:\\", 0x0129D1EE, "MT-A"),
                mounted("G:\\", 0x0129D190, "MT-B"),
            ],
        );
        assert_eq!(app.dest_a, "F:\\", "the slot must fill without a restart");
        assert!(
            app.absent.is_empty(),
            "and the note must clear: {:?}",
            app.absent
        );
        assert_eq!(app.status, None);
    }

    /// A drive that comes back on a different letter still resolves, because
    /// the memory is keyed on the serial rather than on `F:`.
    #[test]
    fn a_drive_that_returns_on_another_letter_still_resolves() {
        let cfg = remembering(&[("A", 0x0129D1EE, "MT-A")]);
        let mut app = App::default();
        app.resolve_remembered(&cfg, &[]);
        assert!(app.dest_a.is_empty());

        app.resolve_remembered(&cfg, &[mounted("K:\\", 0x0129D1EE, "MT-A")]);
        assert_eq!(app.dest_a, "K:\\");
    }

    /// A path the operator typed is theirs. Rescan must never overwrite it.
    #[test]
    fn rescan_never_overwrites_a_typed_path() {
        let cfg = remembering(&[("A", 0x0129D1EE, "MT-A")]);
        let mut app = App {
            dest_a: "Z:\\somewhere-else".into(),
            ..Default::default()
        };
        app.resolve_remembered(&cfg, &[mounted("F:\\", 0x0129D1EE, "MT-A")]);
        assert_eq!(app.dest_a, "Z:\\somewhere-else");
    }

    #[test]
    fn config_requires_a_card_and_a_destination() {
        let mut app = App::default();
        assert!(app.config().is_err());
        app.card1 = "E:\\".into();
        assert!(app.config().is_err(), "a card alone is not enough");
        app.dest_a = "D:\\".into();
        assert!(app.config().is_ok());
    }

    #[test]
    fn blank_destinations_are_dropped_not_passed_through() {
        let app = App {
            card1: "E:\\".into(),
            dest_a: "D:\\".into(),
            dest_b: "   ".into(),
            dest_c: "G:\\".into(),
            ..App::default()
        };
        let cfg = app.config().unwrap();
        assert_eq!(cfg.dest_roots.len(), 2);
        assert_eq!(cfg.dest_roots[1], PathBuf::from("G:\\"));
        assert!(
            cfg.card2.is_none(),
            "an empty card 2 means no twin, not an empty path"
        );
    }

    #[test]
    fn applying_events_updates_the_visible_state() {
        let mut app = App::default();
        let (tel, rx) = Telemetry::new();

        tel.phase(Phase::Copy);
        tel.emit(Event::Throughput {
            dev: DeviceId::DestA,
            mbps: 128.0,
        });
        tel.emit(Event::Bytes {
            dev: DeviceId::DestA,
            delta: 4096,
        });
        tel.emit(Event::Queue {
            dev: DeviceId::DestA,
            depth: 4,
            cap: 4,
        });
        tel.emit(Event::FileStart {
            idx: 11,
            rel: "DCIM/100MSDCF/DSC01204.ARW".into(),
            size: 62_914_560,
        });
        tel.ok(telemetry::Stage::Copy, "DSC01204.ARW copied");
        drop(tel);

        for record in rx.iter() {
            app.apply(&record);
        }

        assert_eq!(app.phase, Phase::Copy);
        assert_eq!(app.devices[&DeviceId::DestA].mbps, 128.0);
        assert_eq!(app.devices[&DeviceId::DestA].bytes, 4096);
        assert_eq!(app.pipeline.blocking(), Some(DeviceId::DestA));
        assert_eq!(app.pipeline.current_file.as_deref(), Some("DSC01204.ARW"));
        assert_eq!(app.pipeline.file_index, 12, "index is 1-based for display");
        assert_eq!(app.log.total_count(), 1, "only Log events become log rows");
    }

    /// Picking a drive another slot already uses is the mistake the format
    /// verdict exists to refuse. The picker says so at the moment of choosing.
    #[test]
    fn the_picker_flags_a_drive_another_slot_already_uses() {
        let taken = [(DeviceId::DestA, 2u32), (DeviceId::Card1, 5)];
        assert_eq!(
            colliding_slot(DeviceId::DestB, Some(2), &taken),
            Some(DeviceId::DestA)
        );
        assert_eq!(
            colliding_slot(DeviceId::DestB, Some(5), &taken),
            Some(DeviceId::Card1),
            "a destination on the same disk as a card is just as wrong"
        );
        assert_eq!(colliding_slot(DeviceId::DestB, Some(9), &taken), None);
    }

    #[test]
    fn a_slot_never_collides_with_itself() {
        let taken = [(DeviceId::DestA, 2u32)];
        assert_eq!(
            colliding_slot(DeviceId::DestA, Some(2), &taken),
            None,
            "re-picking the drive this row already has is not a mistake"
        );
    }

    #[test]
    fn a_drive_with_no_device_number_cannot_be_shown_as_colliding() {
        let taken = [(DeviceId::DestA, 2u32)];
        assert_eq!(colliding_slot(DeviceId::DestB, None, &taken), None);
    }

    /// The picker sorts plausible choices to the top of each slot.
    #[test]
    fn cards_prefer_removable_media_and_destinations_prefer_fixed_drives() {
        use crate::engine::win::DriveType;
        assert!(DriveType::Removable.is_card_like());
        assert!(!DriveType::Fixed.is_card_like());
        assert!(DriveType::Fixed.is_dest_like());
        assert!(!DriveType::Removable.is_dest_like());
        // Optical drives are neither, and are not offered at all.
        assert!(!DriveType::CdRom.is_card_like() && !DriveType::CdRom.is_dest_like());
    }

    /// The progress bar and the estimate both had no denominator until the
    /// plan event existed -- they silently read zero.
    #[test]
    fn the_plan_gives_progress_a_denominator() {
        let mut app = App::default();
        let (tel, rx) = Telemetry::new();
        tel.emit(Event::Plan {
            files: 1613,
            bytes: 91_400_000_000,
        });
        drop(tel);
        for record in rx.iter() {
            app.apply(&record);
        }
        assert_eq!(app.pipeline.file_total, 1613);
        assert_eq!(app.pipeline.bytes_total, 91_400_000_000);
        assert_eq!(app.pipeline.progress(), 0.0, "nothing done yet");
    }

    #[test]
    fn the_verdict_survives_the_drain() {
        let mut app = App::default();
        let (tel, rx) = Telemetry::new();
        let report = crate::engine::verdict::assess(&crate::engine::verdict::Assessment {
            files_total: 3,
            had_card2: true,
            manifests_written: true,
            distinctness: Some(crate::engine::win::Distinctness::Distinct),
            ..Default::default()
        });
        tel.emit(Event::Verdict(report.clone()));
        drop(tel);
        for record in rx.iter() {
            app.apply(&record);
        }
        assert_eq!(app.verdict.map(|v| v.state), Some(report.state));
    }

    /// A preflight refusal -- a filename Windows cannot store, the same card in
    /// both slots, a full drive -- never reaches the verdict phase. It used to
    /// arrive as one ERR line in a scrolling log while the banner still read
    /// RUNNING, and the banner is the only thing some people read.
    #[test]
    fn a_refused_run_still_reaches_the_banner() {
        let mut app = App::default();
        let (tel, rx) = Telemetry::new();

        let refusal = anyhow::anyhow!(
            "1 file(s) cannot be copied to a Windows volume unchanged:\n  - DCIM/COM1.ARW: \
             COM1 is a reserved device name on Windows"
        );
        let reasons: Vec<String> = format!("{refusal:#}")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        tel.emit(Event::Verdict(VerdictReport::refused(reasons)));
        drop(tel);
        for record in rx.iter() {
            app.apply(&record);
        }

        let v = app.verdict.expect("a refusal must produce a banner");
        assert_eq!(v.state, crate::engine::verdict::Verdict::Failed);
        assert!(!v.state.authorises_erase());
        assert!(
            v.reasons.iter().any(|r| r.contains("COM1")),
            "the reason has to survive the trip: {:?}",
            v.reasons
        );
    }
}
