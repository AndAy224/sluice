//! The engine-to-everything-else interface.
//!
//! Two consumers, with very different tolerances:
//!
//! * The **JSONL sink** is the forensic record. If something surfaces at home in
//!   six months, this file is how you reconstruct what happened. It must not
//!   lose a log line, and it must survive a yanked cable, so it is written to
//!   the laptop and flushed as it goes -- never to a destination drive, which is
//!   exactly the thing liable to disappear mid-job.
//! * The **UI** is a view. A dropped sparkline sample costs nothing.
//!
//! So the engine emits once, into a single channel drained by the sink thread,
//! and the sink fans out to the UI with `try_send`. High-frequency telemetry
//! (`Bytes`, `Queue`, `Throughput`) is droppable at the first hop too. The
//! engine therefore never blocks on the UI: a minimised, wedged, or absent
//! window cannot throttle a copy.
//!
//! The design describes guaranteed delivery to the UI. That is not quite what
//! happens here, deliberately: blocking a 20-minute copy on a GUI that has
//! stopped draining would trade the thing that matters for the thing that does
//! not. The guarantee lives on the JSONL side, which is where the design's own
//! reasoning puts the value.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::Serialize;

use super::verdict::VerdictReport;
use super::verify::{Diagnosis, Hashes};
use super::win::VolumeInfo;
use super::DeviceId;

/// Where the job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Phase {
    Idle,
    Scan,
    Reconcile,
    Preflight,
    Copy,
    Verify,
    Manifest,
    Verdict,
    Done,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Scan => "SCAN",
            Self::Reconcile => "RECONCILE",
            Self::Preflight => "PREFLIGHT",
            Self::Copy => "COPY",
            Self::Verify => "VERIFY",
            Self::Manifest => "MANIFEST",
            Self::Verdict => "VERDICT",
            Self::Done => "DONE",
        }
    }
}

/// Log severity. Carries a distinct glyph as well as a colour, because colour is
/// never the only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Level {
    Io,
    Perf,
    Info,
    Ok,
    Warn,
    Err,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Self::Io => "IO",
            Self::Perf => "PERF",
            Self::Info => "INFO",
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Err => "ERR",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Io => "·",
            Self::Perf => "~",
            Self::Info => "-",
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Err => "✗",
        }
    }
}

/// Which part of the engine spoke. Fixed-width column in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Stage {
    Scan,
    Recon,
    Pre,
    Power,
    Copy,
    Verify,
    Mhl,
    Verdict,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Recon => "recon",
            Self::Pre => "pre",
            Self::Power => "power",
            Self::Copy => "copy",
            Self::Verify => "verify",
            Self::Mhl => "mhl",
            Self::Verdict => "verdict",
        }
    }
}

/// What preflight captured about one device, for the device strip and the log.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub volume: VolumeInfo,
    pub free_bytes: u64,
    /// Present for cards, where the strip shows "used / capacity".
    pub total_bytes: Option<u64>,
}

/// Everything the engine has to say.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Phase {
        phase: Phase,
    },
    Device {
        id: DeviceId,
        info: Box<DeviceInfo>,
    },
    /// What the session has to move, emitted once reconciliation has decided.
    ///
    /// Without it the progress bar has no denominator and the estimate has no
    /// target -- both were silently reading zero before this existed.
    Plan {
        files: usize,
        bytes: u64,
    },
    FileStart {
        idx: usize,
        /// Forward-slash relative path, matching the manifest.
        rel: String,
        size: u64,
    },
    /// Coalesced to ~10 Hz per device by [`ByteMeter`], not one per 4 MiB chunk.
    Bytes {
        dev: DeviceId,
        delta: u64,
    },
    Queue {
        dev: DeviceId,
        depth: usize,
        cap: usize,
    },
    Throughput {
        dev: DeviceId,
        mbps: f32,
    },
    FileDone {
        idx: usize,
        rel: String,
        hashes: Hashes,
        diagnosis: Option<Diagnosis>,
        dur_ms: u64,
    },
    Log {
        level: Level,
        stage: Stage,
        msg: String,
    },
    Verdict(VerdictReport),
}

impl Event {
    /// Whether losing this event would cost the forensic record.
    ///
    /// The high-frequency three are droppable; everything else is not.
    fn is_droppable(&self) -> bool {
        matches!(
            self,
            Event::Bytes { .. } | Event::Queue { .. } | Event::Throughput { .. }
        )
    }
}

/// One timestamped event, as written to the JSONL and drained by the UI.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// Wall clock, so the record lines up with camera timestamps and with you
    /// remembering what time it was.
    pub at: DateTime<Utc>,
    /// Monotonic milliseconds since the job started, for ordering and durations.
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl Record {
    /// The fixed-column log line of §10.
    pub fn log_line(&self) -> Option<String> {
        let Event::Log { level, stage, msg } = &self.event else {
            return None;
        };
        Some(format!(
            "{}  {:<5}  {:<8}  {}",
            local_time(self.at),
            level.label(),
            stage.label(),
            msg
        ))
    }
}

struct Inner {
    start: Instant,
    tx: Sender<Record>,
    dropped: AtomicU64,
    trace: bool,
}

/// The handle every engine thread holds.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

impl Telemetry {
    /// Build a telemetry handle plus the receiver the sink thread drains.
    pub fn new() -> (Self, Receiver<Record>) {
        Self::with_trace(false)
    }

    /// As [`Telemetry::new`], with per-chunk tracing enabled.
    ///
    /// Off by default because it roughly triples log volume; on whenever
    /// something is being diagnosed.
    pub fn with_trace(trace: bool) -> (Self, Receiver<Record>) {
        let (tx, rx) = bounded(16_384);
        (
            Self {
                inner: Arc::new(Inner {
                    start: Instant::now(),
                    tx,
                    dropped: AtomicU64::new(0),
                    trace,
                }),
            },
            rx,
        )
    }

    /// Whether the per-chunk detail is wanted. Check this before doing work
    /// purely to build a trace message.
    pub fn tracing(&self) -> bool {
        self.inner.trace
    }

    /// An `IO`-level line that exists only under `--trace`.
    pub fn trace(&self, stage: Stage, msg: impl Into<String>) {
        if self.inner.trace {
            self.log(Level::Io, stage, msg);
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.inner.start.elapsed()
    }

    /// High-frequency events dropped because a consumer fell behind.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    pub fn emit(&self, event: Event) {
        let record = Record {
            at: Utc::now(),
            elapsed_ms: self.inner.start.elapsed().as_millis() as u64,
            event,
        };
        if record.event.is_droppable() {
            if let Err(TrySendError::Full(_)) = self.inner.tx.try_send(record) {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // Blocking, but only on a dedicated writer thread that does nothing
            // except drain this channel.
            let _ = self.inner.tx.send(record);
        }
    }

    pub fn log(&self, level: Level, stage: Stage, msg: impl Into<String>) {
        self.emit(Event::Log {
            level,
            stage,
            msg: msg.into(),
        });
    }

    pub fn info(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Info, stage, msg);
    }
    pub fn ok(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Ok, stage, msg);
    }
    pub fn warn(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Warn, stage, msg);
    }
    pub fn err(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Err, stage, msg);
    }
    pub fn io(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Io, stage, msg);
    }
    pub fn perf(&self, stage: Stage, msg: impl Into<String>) {
        self.log(Level::Perf, stage, msg);
    }

    pub fn phase(&self, phase: Phase) {
        self.emit(Event::Phase { phase });
    }

    pub fn queue(&self, dev: DeviceId, depth: usize, cap: usize) {
        self.emit(Event::Queue { dev, depth, cap });
    }
}

/// Per-device byte accumulator that emits at roughly 10 Hz.
///
/// Emitting one event per 4 MiB chunk would put ~30 events per second per device
/// on the channel during a copy and many times that during verify, all to move a
/// progress bar that repaints at 30 Hz.
pub struct ByteMeter {
    dev: DeviceId,
    pending: u64,
    window_bytes: u64,
    window_start: Instant,
}

const TICK: Duration = Duration::from_millis(100);

impl ByteMeter {
    pub fn new(dev: DeviceId) -> Self {
        Self {
            dev,
            pending: 0,
            window_bytes: 0,
            window_start: Instant::now(),
        }
    }

    /// Record `n` bytes moved, emitting if the tick has elapsed.
    pub fn add(&mut self, n: usize, tel: &Telemetry) {
        self.pending += n as u64;
        self.window_bytes += n as u64;
        let elapsed = self.window_start.elapsed();
        if elapsed >= TICK {
            self.emit(tel, elapsed);
        }
    }

    /// Emit whatever is pending. Call at the end of a file or a phase, so the
    /// last partial tick is not silently lost.
    pub fn flush(&mut self, tel: &Telemetry) {
        let elapsed = self.window_start.elapsed();
        if self.pending > 0 {
            self.emit(tel, elapsed);
        }
    }

    fn emit(&mut self, tel: &Telemetry, elapsed: Duration) {
        tel.emit(Event::Bytes {
            dev: self.dev,
            delta: self.pending,
        });
        let secs = elapsed.as_secs_f32();
        if secs > 0.0 {
            tel.emit(Event::Throughput {
                dev: self.dev,
                mbps: self.window_bytes as f32 / 1.0e6 / secs,
            });
        }
        self.pending = 0;
        self.window_bytes = 0;
        self.window_start = Instant::now();
    }
}

/// Drains the engine channel, writes the JSONL, and forwards to the UI.
pub struct Sink {
    handle: JoinHandle<Result<()>>,
    ui_dropped: Arc<AtomicU64>,
}

impl Sink {
    /// Start the sink thread.
    ///
    /// `ui` may be `None` for a headless run, in which case records are written
    /// to disk and discarded.
    pub fn spawn(path: PathBuf, rx: Receiver<Record>, ui: Option<Sender<Record>>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating log directory {}", parent.display()))?;
        }
        let file = File::create(&path)
            .with_context(|| format!("creating session log {}", path.display()))?;
        let ui_dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&ui_dropped);

        let handle = std::thread::Builder::new()
            .name("sluice-jsonl".into())
            .spawn(move || -> Result<()> {
                let mut out = BufWriter::new(file);
                for record in rx {
                    // Serialise before forwarding, so a UI that has gone away
                    // cannot cost us the record.
                    match serde_json::to_string(&record) {
                        Ok(line) => {
                            out.write_all(line.as_bytes())?;
                            out.write_all(b"\n")?;
                            // Flush per record: a yanked power cable must still
                            // leave a complete account up to the instant it died.
                            out.flush()?;
                        }
                        Err(e) => {
                            let _ =
                                writeln!(out, "{{\"event\":\"serialise_error\",\"err\":{e:?}}}");
                        }
                    }
                    if let Some(ui) = &ui {
                        if ui.try_send(record).is_err() {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                out.flush()?;
                Ok(())
            })
            .context("spawning the JSONL sink thread")?;

        Ok(Self { handle, ui_dropped })
    }

    /// Records the UI could not keep up with. Cosmetic, but worth showing.
    pub fn ui_dropped(&self) -> u64 {
        self.ui_dropped.load(Ordering::Relaxed)
    }

    /// Wait for the sink to finish. The caller must have dropped every
    /// [`Telemetry`] handle first, or this blocks forever.
    pub fn join(self) -> Result<()> {
        match self.handle.join() {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the JSONL sink thread panicked"),
        }
    }
}

/// Where a session's live log goes.
///
/// On the laptop, never on a destination: test 13 yanks a destination mid-copy,
/// and the record of what happened must survive that.
pub fn default_log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sluice")
        .join("logs")
}

/// `sluice_<session>.jsonl` under the local log directory.
pub fn log_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("sluice_{session}.jsonl"))
}

/// A session identifier: `20260314-221403`.
///
/// Seconds included deliberately. Two runs starting inside the same minute --
/// a fast failure and an immediate retry -- would otherwise share an id and the
/// second would overwrite the first's manifest.
pub fn session_id(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%Y%m%d-%H%M%S")
        .to_string()
}

/// The calendar date a session belongs to, in the operator's own timezone.
///
/// Local, deliberately, and this is a correctness matter rather than a
/// cosmetic one. The session folder is named from this. A 22:14 offload in
/// Shoot is the 12th in UTC, so a UTC date filed the night's work under
/// tomorrow -- disagreeing with the titlebar three inches above it, with the
/// callsheet, and with the camera's own file dates.
///
/// Worse, resume is keyed on that folder. A job interrupted at 17:50 and
/// restarted at 18:10 crossed midnight UTC, addressed a different folder, and
/// re-copied the entire card instead of resuming. 18:00 mountain is the start
/// of the offload window, so that was the normal case rather than a rare one.
///
/// Manifest timestamps stay UTC with a `Z`: the MHL and ASC MHL schemas require
/// it, and an instant is not a calendar date.
pub fn local_date(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// The local UTC offset as `+02:00`, for stamping a human-facing date.
pub fn local_offset(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local).format("%:z").to_string()
}

/// Wall-clock time of day, as the operator's own clock shows it.
///
/// Everything sluice *stores* is UTC — the JSONL `at`, the MHL dates — because
/// a forensic record has to be unambiguous years later. Everything it *shows*
/// is local, because the person reading it is looking at a wall clock and a
/// callsheet.
///
/// Mixing the two is worse than either, and it shipped: the session folder read
/// `2026-08-26_shoot` and its id `20260826-152315`, both local, while the log
/// line three inches below read `19:23:15`. Four hours apart, on one screen,
/// describing the same instant. Reported from a real run.
pub fn local_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%H:%M:%S%.3f")
        .to_string()
}

/// A local date and time, for a list somebody reads down.
pub fn local_stamp(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// One session log on disk.
#[derive(Debug, Clone)]
pub struct LogFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: Option<std::time::SystemTime>,
}

/// Every session log, newest first.
pub fn list_logs(dir: &Path) -> Vec<LogFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<LogFile> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|x| x == "jsonl")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("sluice_"))
        })
        .filter_map(|path| {
            let m = std::fs::metadata(&path).ok()?;
            Some(LogFile {
                bytes: m.len(),
                modified: m.modified().ok(),
                path,
            })
        })
        .collect();
    // Newest first, so "keep N" keeps the ones worth keeping.
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.path.cmp(&a.path)));
    out
}

/// Delete all but the newest `keep` session logs.
///
/// Logs are never rotated by the writer, and the sharp edge is not disk usage:
/// if the volume fills, the sink's `write_all` fails, the sink thread dies,
/// `Telemetry::emit` swallows the send error, and the job runs on to a verdict
/// with a truncated forensic record. Better a command that prunes them than a
/// user improvising in Explorer at 2am next to a `history.jsonl` that must not
/// be deleted.
///
/// Returns what was removed and how much it freed.
pub fn prune_logs(dir: &Path, keep: usize) -> (usize, u64) {
    let logs = list_logs(dir);
    let mut removed = 0usize;
    let mut freed = 0u64;
    for old in logs.into_iter().skip(keep) {
        if std::fs::remove_file(&old.path).is_ok() {
            removed += 1;
            freed += old.bytes;
        }
    }
    (removed, freed)
}

/// RFC-3339 UTC with a `Z`, as the manifests want it.
pub fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn droppable_set_is_exactly_the_high_frequency_three() {
        assert!(Event::Bytes {
            dev: DeviceId::DestA,
            delta: 1
        }
        .is_droppable());
        assert!(Event::Queue {
            dev: DeviceId::DestA,
            depth: 1,
            cap: 4
        }
        .is_droppable());
        assert!(Event::Throughput {
            dev: DeviceId::DestA,
            mbps: 1.0
        }
        .is_droppable());

        assert!(!Event::Phase { phase: Phase::Copy }.is_droppable());
        assert!(!Event::Log {
            level: Level::Err,
            stage: Stage::Verify,
            msg: String::new()
        }
        .is_droppable());
        assert!(!Event::FileStart {
            idx: 0,
            rel: "x".into(),
            size: 1
        }
        .is_droppable());
    }

    /// The clock the operator reads must be the operator's own clock, and the
    /// same one the session id and folder name use.
    ///
    /// It was not: log lines were stamped UTC while the id beside them was
    /// local. Found on a real run in a UTC-4 zone — the log read 19:44 at
    /// 15:44, four hours out, three inches from a folder named for the local
    /// date. Asserted as agreement between the two rather than against a
    /// literal, which would only hold in the zone it was written in.
    #[test]
    fn the_log_clock_and_the_session_id_agree() {
        let at = DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
            .unwrap()
            .with_timezone(&Utc);

        let shown = local_time(at);
        assert_eq!(
            shown,
            at.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        );

        // `session_id` ends in HHMMSS on the same clock. If either side ever
        // goes back to UTC alone, these stop matching.
        let hhmmss: String = shown.chars().filter(char::is_ascii_digit).take(6).collect();
        assert!(
            session_id(at).ends_with(&hhmmss),
            "session id {} disagrees with the log clock {shown}",
            session_id(at)
        );

        // And what is *stored* stays UTC, so the record is unambiguous later.
        assert!(rfc3339(at).ends_with('Z'), "{}", rfc3339(at));
    }

    #[test]
    fn log_lines_use_fixed_columns() {
        let (tel, rx) = Telemetry::new();
        tel.ok(Stage::Recon, "file lists identical, 1,613 matched");
        let rec = rx.recv().unwrap();
        let line = rec.log_line().unwrap();
        assert!(
            line.contains("OK     recon     file lists identical"),
            "got {line}"
        );
    }

    #[test]
    fn non_log_events_have_no_log_line() {
        let (tel, rx) = Telemetry::new();
        tel.phase(Phase::Copy);
        assert!(rx.recv().unwrap().log_line().is_none());
    }

    #[test]
    fn byte_meter_coalesces_and_flushes_the_tail() {
        let (tel, rx) = Telemetry::new();
        let mut meter = ByteMeter::new(DeviceId::DestA);
        // Well under the tick, so nothing should be emitted yet.
        for _ in 0..10 {
            meter.add(4 * 1024 * 1024, &tel);
        }
        assert!(
            rx.try_recv().is_err(),
            "10 chunks inside one tick must not emit 10 events"
        );

        meter.flush(&tel);
        let Event::Bytes { delta, .. } = rx.recv().unwrap().event else {
            panic!("expected a Bytes event");
        };
        assert_eq!(delta, 10 * 4 * 1024 * 1024, "the tail must not be lost");
    }

    #[test]
    fn droppable_events_do_not_block_a_full_channel() {
        let (tel, _rx) = Telemetry::new();
        // Overfill well past the 16,384 capacity. Without dropping, this hangs.
        for _ in 0..20_000 {
            tel.emit(Event::Bytes {
                dev: DeviceId::DestA,
                delta: 1,
            });
        }
        assert!(
            tel.dropped() > 0,
            "a full channel must shed droppable telemetry"
        );
    }

    #[test]
    fn sink_writes_one_json_object_per_line_and_forwards_to_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let (tel, rx) = Telemetry::new();
        let (ui_tx, ui_rx) = bounded(64);
        let sink = Sink::spawn(path.clone(), rx, Some(ui_tx)).unwrap();

        tel.info(Stage::Scan, "card 1: 1,613 files");
        tel.phase(Phase::Copy);
        drop(tel);
        sink.join().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("at").is_some() && v.get("event").is_some());
        }
        assert_eq!(ui_rx.len(), 2, "the UI sees the same records");
    }

    #[test]
    fn session_ids_and_timestamps_are_stable() {
        let at = DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
            .unwrap()
            .with_timezone(&Utc);
        // Local, not UTC -- see `local_date`. Asserted as a correspondence
        // rather than as a literal, because a literal would only hold in the
        // timezone it was written in.
        assert_eq!(
            session_id(at),
            at.with_timezone(&chrono::Local)
                .format("%Y%m%d-%H%M%S")
                .to_string()
        );
        assert_eq!(session_id(at).len(), 15);
        // Manifest timestamps stay UTC with a Z, whatever the operator's clock
        // says. The schemas require it, and an instant is not a calendar date.
        assert_eq!(rfc3339(at), "2026-03-14T22:14:03Z");
        assert_eq!(
            log_path(Path::new("C:\\logs"), &session_id(at)),
            Path::new(&format!("C:\\logs\\sluice_{}.jsonl", session_id(at)))
        );
    }

    /// The folder a night's work is filed under is the operator's date, not
    /// UTC's. A 22:14 offload in Shoot is the 12th in UTC, and filing it
    /// under tomorrow disagrees with the titlebar, the callsheet and the
    /// camera's own file dates -- and, worse, moves the folder that resume
    /// keys on.
    #[test]
    fn the_session_date_is_the_operators_date() {
        let at = DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            local_date(at),
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        );
        // And it agrees with the session id, so a folder never carries one date
        // while the manifest inside it carries another.
        assert!(
            session_id(at).starts_with(&local_date(at).replace('-', "")),
            "session {} must sit inside {}",
            session_id(at),
            local_date(at)
        );
    }

    #[test]
    fn the_offset_is_reported_so_a_date_is_unambiguous() {
        let at = DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let off = local_offset(at);
        assert!(
            off.len() == 6 && (off.starts_with('+') || off.starts_with('-')),
            "expected +HH:MM, got {off}"
        );
    }

    // --- log retention ------------------------------------------------------

    /// Logs are never rotated by the writer. The sharp edge is not disk usage:
    /// if the volume fills, the sink's write fails, the sink thread dies, the
    /// send error is swallowed, and the job runs on to a verdict with a
    /// truncated forensic record.
    #[test]
    fn pruning_keeps_the_newest_and_removes_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6 {
            let p = dir.path().join(format!("sluice_2026031{i}-2214.jsonl"));
            std::fs::write(&p, vec![b'x'; 100]).unwrap();
            // Distinct mtimes so "newest" is well defined.
            filetime::set_file_mtime(
                &p,
                filetime::FileTime::from_unix_time(1_757_600_000 + i as i64 * 60, 0),
            )
            .unwrap();
        }
        assert_eq!(list_logs(dir.path()).len(), 6);

        let (removed, freed) = prune_logs(dir.path(), 2);
        assert_eq!(removed, 4);
        assert_eq!(freed, 400);

        let left = list_logs(dir.path());
        assert_eq!(left.len(), 2);
        // The two newest survived.
        assert!(left[0].path.to_string_lossy().contains("20260315"));
        assert!(left[1].path.to_string_lossy().contains("20260314"));
    }

    /// history.jsonl is the durable record of what every card and drive has
    /// done, and losing it loses the suspect-card flags. Pruning must never
    /// touch it, nor anything else that is not a session log.
    #[test]
    fn pruning_touches_nothing_but_session_logs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("history.jsonl"), b"{}").unwrap();
        std::fs::write(dir.path().join("crash.log"), b"panic").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hi").unwrap();
        std::fs::write(dir.path().join("sluice_20260314-2214.jsonl"), b"{}").unwrap();

        assert_eq!(
            list_logs(dir.path()).len(),
            1,
            "only the session log counts"
        );
        let (removed, _) = prune_logs(dir.path(), 0);
        assert_eq!(removed, 1);
        for keep in ["history.jsonl", "crash.log", "notes.txt"] {
            assert!(dir.path().join(keep).exists(), "{keep} must survive");
        }
    }

    #[test]
    fn keeping_more_than_exist_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sluice_20260314-2214.jsonl"), b"{}").unwrap();
        assert_eq!(prune_logs(dir.path(), 30), (0, 0));
    }
}
