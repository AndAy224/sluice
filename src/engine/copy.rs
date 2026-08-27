//! The copy pipeline: one read of the source card, fanned out to every writer.
//!
//! ```text
//!                           ┌─► bounded chan ─► writer A ─► LaCie A
//!   source ─► reader thread ┼─► bounded chan ─► writer B ─► LaCie B
//!          (4 MiB chunks)   └─► bounded chan ─► writer C ─► laptop SSD
//!               │
//!               └─► xxHash64 (Sc, in flight)
//! ```
//!
//! Chunks travel as `Arc<Chunk>`, so fanning out to N writers copies a pointer
//! rather than 4 MiB. The bounded channels supply backpressure for free: when
//! the slowest drive falls behind, the reader blocks instead of ballooning
//! memory. A cap of 4 gives roughly 16 MiB in flight per destination.
//!
//! Two details that are load-bearing rather than incidental:
//!
//! * The source is read **unbuffered**. The design implies a normal read, but
//!   an unbuffered one makes `Sc` and the later `C1` two genuinely independent
//!   trips to the device -- which is the entire premise of the matrix row that
//!   detects an unrepeatable read. It also keeps a 91 GB card from evicting
//!   everything else from the page cache.
//! * `sync_all()` before close forces the data out of the OS dirty list, so the
//!   unbuffered verify read measures the drive rather than lazy writeback.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, SendTimeoutError, Sender};
use filetime::FileTime;
use serde::Serialize;

use super::reconcile::CopyItem;
use super::scan::Mtime;
use super::telemetry::{ByteMeter, Event, Level, Stage, Telemetry};
use super::unbuffered::{hex64_short, AlignedBuf, ChunkReader};
use super::DeviceId;

/// Channel depth per destination. Four 4 MiB chunks is ~16 MiB in flight.
const QUEUE_CAP: usize = 4;

/// How long a blocked send waits before re-checking the cancel flag.
const SEND_POLL: Duration = Duration::from_millis(100);

/// One unit of data in flight, with its aligned backing buffer.
pub struct Chunk {
    buf: AlignedBuf,
    len: usize,
}

impl Chunk {
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// The writer protocol.
enum Msg {
    Open(PathBuf, u64),
    Chunk(Arc<Chunk>),
    /// `sync_all`, then stamp the source mtime.
    Close(FileTime),
    /// Abandon the file in progress and delete the partial.
    Abandon,
    Stop,
}

/// A destination to write to.
#[derive(Debug, Clone)]
pub struct Destination {
    pub dev: DeviceId,
    /// The session folder, e.g. `D:\2026-03-14_shoot-01`.
    pub root: PathBuf,
}

/// A per-file failure on one destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyError {
    pub dev: DeviceId,
    pub rel: String,
    pub msg: String,
}

/// What the copy phase established.
#[derive(Debug, Default)]
pub struct CopyReport {
    /// `Sc` per file: the source hashed in flight. Absent for files resume
    /// skipped, which never passed through the reader at all.
    pub source_hashes: BTreeMap<String, u64>,
    /// Files already present on every destination with a matching size and
    /// mtime, so the write was skipped.
    pub skipped_resume: Vec<String>,
    pub errors: Vec<CopyError>,
    /// Files that failed once and succeeded on the retry.
    ///
    /// The data is good -- verify proves that independently -- so a recovered
    /// file does not fail the run. It is counted anyway, because a drive that
    /// needs retries is a drive beginning to fail, and that is only visible if
    /// somebody writes it down.
    pub retried: Vec<CopyError>,
    pub bytes_read: u64,
    pub cancelled: bool,
}

impl CopyReport {
    pub fn failed_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.errors.iter().map(|e| e.rel.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// How many files each destination needed a retry for.
    pub fn retries_by_device(&self) -> BTreeMap<DeviceId, usize> {
        let mut out = BTreeMap::new();
        for e in &self.retried {
            *out.entry(e.dev).or_insert(0) += 1;
        }
        out
    }
}

/// Absolute destination path for a relative entry.
///
/// Extended-prefixed when long: a session folder under a deep destination plus a
/// camera tree can pass `MAX_PATH`, where Win32 fails with an error that reads
/// like a missing directory.
pub fn dest_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    super::win::extended_path(&p)
}

/// Whether a destination already holds this file, for resume purposes.
///
/// Size and mtime only. That is deliberately weak evidence, and it is enough:
/// the verify phase hashes every file unconditionally afterwards, so a wrong
/// guess here costs a re-copy on the next run rather than a bad verdict. The
/// design's separate resume-hash path would only duplicate that work.
fn already_present(root: &Path, item: &CopyItem) -> bool {
    match fs::metadata(dest_path(root, &item.rel)) {
        // A cloud placeholder reports the full logical size and the original
        // mtime while holding none of the bytes, so it looks exactly like a
        // finished copy. Skipping it means the verify pass opens it, silently
        // hydrates it over the network, hashes what comes back and agrees --
        // and the same reasoning `win::is_cloud_placeholder` already applies to
        // the card ("reading it would download it rather than read a device")
        // was never applied to the drive the card is being traded for.
        Ok(m) => {
            !super::win::is_cloud_placeholder(m.file_attributes())
                && m.len() == item.size
                && Mtime::from_metadata(&m).matches(item.mtime)
        }
        Err(_) => false,
    }
}

/// A file this run would destroy: the destination already holds something at
/// that path which is not what is about to be written there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WouldOverwrite {
    pub rel: String,
    /// What is on the destination now.
    pub existing_size: u64,
    /// What this run would put there.
    pub incoming_size: u64,
    pub reason: Clash,
}

/// Why the file on the drive is not the file about to be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clash {
    /// A different length: plainly a different file.
    Size,
    /// The same length, a different modification time. Either a different file
    /// that happens to match in size, or -- worth saying out loud -- this run's
    /// own earlier copy, touched by something since it was written.
    Modified,
    /// A cloud placeholder: the name is on this drive, the bytes are not.
    Placeholder,
}

impl WouldOverwrite {
    /// What is in the way, in the operator's terms.
    ///
    /// Only a size difference is worth printing as two numbers. A placeholder
    /// matches on size *and* mtime, and a modified copy matches on size -- both
    /// printed as `(N B on the drive, N B incoming)`, which names no difference
    /// at all and reads like a bug in the check rather than a fact about the
    /// drive. Observed on real hardware, where the file in question had been
    /// touched after sluice wrote it.
    pub fn describe(&self) -> String {
        match self.reason {
            Clash::Placeholder => format!(
                "{} -- a cloud placeholder ({} B listed, none of it on this drive)",
                self.rel, self.existing_size
            ),
            Clash::Modified => format!(
                "{} -- same size ({} B), but the copy on the drive was modified after it was \
                 written",
                self.rel, self.existing_size
            ),
            Clash::Size => format!(
                "{} ({} B on the drive, {} B incoming)",
                self.rel, self.existing_size, self.incoming_size
            ),
        }
    }
}

/// Everything in this copy list that would silently replace a different file.
///
/// The design asks for this at §PREFLIGHT -- "destination folders empty or
/// resumable" -- and it was never built. Without it, a second card pair
/// offloaded on the same day under the same label lands in the first pair's
/// folder, and every colliding path is truncated by `File::create`. Two Sony
/// bodies both number from `DSC00001.ARW`, so the collision is ordinary rather
/// than exotic, and the run that causes it verifies its own files perfectly and
/// reports SAFE TO FORMAT. The morning's frames are gone and nothing said so.
///
/// The test for "the same file" is [`already_present`] itself, rather than a
/// second copy of its rules. The two have to agree exactly, because they
/// partition the same set: anything resume will not skip is something the
/// writer opens with `File::create`, and that truncates.
///
/// They drifted once, and it cost real bytes. `already_present` learned to
/// reject cloud placeholders -- a dehydrated file reports the full logical size
/// and the original mtime while holding none of the bytes -- and this function
/// did not. So a placeholder at the destination was *neither* skipped as
/// present *nor* refused as a clash: it was truncated, and deleted outright if
/// the run was then cancelled or hit a bad sector on the card. Calling
/// `already_present` makes that class of divergence unrepresentable.
pub fn would_overwrite(root: &Path, items: &[CopyItem]) -> Vec<WouldOverwrite> {
    let mut out = Vec::new();
    for item in items {
        let Ok(m) = fs::metadata(dest_path(root, &item.rel)) else {
            continue;
        };
        if m.is_dir() {
            // `File::create` can never win against a directory, so nothing is
            // destroyed here -- the copy fails loudly instead.
            continue;
        }
        if already_present(root, item) {
            continue;
        }
        let reason = if super::win::is_cloud_placeholder(m.file_attributes()) {
            Clash::Placeholder
        } else if m.len() == item.size {
            Clash::Modified
        } else {
            Clash::Size
        };
        out.push(WouldOverwrite {
            rel: item.rel.clone(),
            existing_size: m.len(),
            incoming_size: item.size,
            reason,
        });
    }
    out
}

/// Bytes this destination still has to receive, given what resume will skip.
///
/// Preflight uses this rather than the session total: a resumed run onto a drive
/// that already holds most of the night should not be refused for lacking room
/// it does not need.
pub fn bytes_needed(root: &Path, items: &[CopyItem]) -> u64 {
    items
        .iter()
        .filter(|i| !already_present(root, i))
        .map(|i| i.size)
        .sum()
}

/// Send, yielding to the cancel flag rather than blocking forever.
///
/// Returns false if the job was cancelled or the writer is gone.
fn send_blocking(tx: &Sender<Msg>, msg: Msg, cancel: &AtomicBool) -> bool {
    let mut msg = msg;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        match tx.send_timeout(msg, SEND_POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(m)) => msg = m,
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

/// Run the copy phase, retrying once per destination for anything that failed.
///
/// A transient USB or drive glitch should cost a few seconds, not a 17-minute
/// re-run at midnight. The design asks for exactly this -- "retry file to A;
/// recurrence means drive A is suspect" -- and the retry is deliberately *once*:
/// a second failure is evidence about the drive rather than bad luck.
pub fn run_copy(
    items: &[CopyItem],
    dests: &[Destination],
    tel: &Telemetry,
    cancel: &Arc<AtomicBool>,
) -> Result<CopyReport> {
    run_copy_with(items, dests, tel, cancel, copy_pass)
}

/// [`run_copy`] with the pass runner injected.
///
/// A private seam, not a runtime switch: it lets the retry *orchestration* --
/// grouping by destination, telling a recovery apart from a second failure,
/// spotting two reads that disagree -- be tested deterministically, without
/// depending on a filesystem fault that obligingly clears itself mid-test.
fn run_copy_with<F>(
    items: &[CopyItem],
    dests: &[Destination],
    tel: &Telemetry,
    cancel: &Arc<AtomicBool>,
    pass_fn: F,
) -> Result<CopyReport>
where
    F: Fn(&[CopyItem], &[Destination], &Telemetry, &Arc<AtomicBool>) -> Result<CopyReport>,
{
    let mut report = pass_fn(items, dests, tel, cancel)?;
    if report.cancelled || report.errors.is_empty() {
        return Ok(report);
    }

    // A file that failed on A but landed on B only needs re-sending to A.
    let mut by_dev: BTreeMap<DeviceId, BTreeSet<String>> = BTreeMap::new();
    for e in &report.errors {
        by_dev.entry(e.dev).or_default().insert(e.rel.clone());
    }

    let first_pass_errors = std::mem::take(&mut report.errors);
    for (dev, rels) in by_dev {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let Some(dest) = dests.iter().find(|d| d.dev == dev) else {
            continue;
        };
        let retry_items: Vec<CopyItem> = items
            .iter()
            .filter(|i| rels.contains(&i.rel))
            .cloned()
            .collect();

        tel.warn(
            Stage::Copy,
            format!(
                "{} file(s) failed on {} — retrying once",
                retry_items.len(),
                dev.label()
            ),
        );

        let pass = pass_fn(&retry_items, std::slice::from_ref(dest), tel, cancel)?;
        report.bytes_read += pass.bytes_read;
        report.cancelled |= pass.cancelled;

        let failed_again: BTreeSet<&str> = pass.errors.iter().map(|e| e.rel.as_str()).collect();

        for (rel, hash) in &pass.source_hashes {
            match report.source_hashes.get(rel) {
                // Two reads of one file disagreeing is an unrepeatable source
                // read. Verify would catch it by comparing the destinations,
                // but saying so here points at the reader rather than the drive.
                Some(first) if first != hash => tel.err(
                    Stage::Copy,
                    format!(
                        "{rel}: the retry read different bytes than the first read \
                         ({} vs {}) — reader, cable, or contacts",
                        hex64_short(*first),
                        hex64_short(*hash)
                    ),
                ),
                Some(_) => {}
                None => {
                    report.source_hashes.insert(rel.clone(), *hash);
                }
            }
        }

        for e in first_pass_errors.iter().filter(|e| e.dev == dev) {
            if failed_again.contains(e.rel.as_str()) {
                continue;
            }
            tel.warn(
                Stage::Copy,
                format!("{} recovered on retry to {}", e.rel, dev.label()),
            );
            report.retried.push(e.clone());
        }
        report.errors.extend(pass.errors);
    }

    for (dev, n) in report.retries_by_device() {
        tel.warn(
            Stage::Copy,
            format!(
                "{n} file(s) needed a retry on {} — the data is verified, but treat that \
                 drive as suspect if it recurs",
                dev.label()
            ),
        );
    }

    report
        .errors
        .sort_by(|a, b| (a.rel.as_str(), a.dev).cmp(&(b.rel.as_str(), b.dev)));
    Ok(report)
}

/// One copy pass: read each item once, fan out to every destination.
fn copy_pass(
    items: &[CopyItem],
    dests: &[Destination],
    tel: &Telemetry,
    cancel: &Arc<AtomicBool>,
) -> Result<CopyReport> {
    if dests.is_empty() {
        anyhow::bail!("a copy needs at least one destination");
    }

    let mut senders: Vec<(DeviceId, Sender<Msg>)> = Vec::new();
    let mut receivers: Vec<(Destination, Receiver<Msg>)> = Vec::new();
    for d in dests {
        let (tx, rx) = bounded(QUEUE_CAP);
        senders.push((d.dev, tx));
        receivers.push((d.clone(), rx));
    }

    let report = std::thread::scope(|scope| -> Result<CopyReport> {
        let mut handles = Vec::new();
        for (dest, rx) in receivers {
            let tel = tel.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("sluice-writer-{}", dest.dev.label()))
                    .spawn_scoped(scope, move || writer_thread(dest, rx, tel))
                    .context("spawning a writer thread")?,
            );
        }

        let report = reader_loop(items, dests, &senders, tel, cancel);

        for (_, tx) in &senders {
            let _ = tx.send(Msg::Stop);
        }
        drop(senders);

        let mut report = report;
        for h in handles {
            match h.join() {
                Ok(errors) => report.errors.extend(errors),
                Err(_) => anyhow::bail!("a writer thread panicked"),
            }
        }
        report
            .errors
            .sort_by(|a, b| (a.rel.as_str(), a.dev).cmp(&(b.rel.as_str(), b.dev)));
        Ok(report)
    })?;

    Ok(report)
}

fn reader_loop(
    items: &[CopyItem],
    dests: &[Destination],
    senders: &[(DeviceId, Sender<Msg>)],
    tel: &Telemetry,
    cancel: &AtomicBool,
) -> CopyReport {
    let mut report = CopyReport::default();
    let mut buf_owner = AlignedBuf::chunk();
    let mut meters: BTreeMap<DeviceId, ByteMeter> = BTreeMap::new();

    for (idx, item) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }

        // Resume: whichever destinations already hold this file sit this one out.
        let needed: Vec<usize> = (0..dests.len())
            .filter(|&i| !already_present(&dests[i].root, item))
            .collect();
        if needed.is_empty() {
            report.skipped_resume.push(item.rel.clone());
            tel.log(
                Level::Io,
                Stage::Copy,
                format!("skip  {}  already present on every destination", item.rel),
            );
            continue;
        }

        tel.emit(Event::FileStart {
            idx,
            rel: item.rel.clone(),
            size: item.size,
        });
        let started = Instant::now();

        let mut reader = match ChunkReader::open(&item.src) {
            Ok(r) => r,
            Err(e) => {
                record_read_error(&mut report, item, dests, &e.to_string(), tel);
                continue;
            }
        };

        let dest_path_for = |i: usize| dest_path(&dests[i].root, &item.rel);
        let mut opened: Vec<usize> = Vec::new();
        let mut aborted = false;
        for &i in &needed {
            if !send_blocking(
                &senders[i].1,
                Msg::Open(dest_path_for(i), item.size),
                cancel,
            ) {
                aborted = true;
                break;
            }
            opened.push(i);
        }
        if aborted {
            abandon(senders, &opened);
            report.cancelled = cancel.load(Ordering::Relaxed);
            break;
        }

        // --- stream the file ---------------------------------------------
        let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
        let mut read_failed = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                aborted = true;
                break;
            }
            let n = match reader.next_chunk(&mut buf_owner) {
                Ok(n) => n,
                Err(e) => {
                    read_failed = Some(e.to_string());
                    break;
                }
            };
            if n == 0 {
                break;
            }
            hasher.update(&buf_owner[..n]);
            if tel.tracing() {
                tel.trace(
                    Stage::Copy,
                    format!(
                        "read  {}  chunk {} B at offset {}",
                        short_name(&item.rel),
                        n,
                        report.bytes_read
                    ),
                );
            }
            report.bytes_read += n as u64;
            meters
                .entry(item.src_dev)
                .or_insert_with(|| ByteMeter::new(item.src_dev))
                .add(n, tel);

            // Move the filled buffer out and take a fresh one, so the writers
            // can hold their reference while the reader keeps going.
            let mut fresh = AlignedBuf::chunk();
            std::mem::swap(&mut buf_owner, &mut fresh);
            let chunk = Arc::new(Chunk { buf: fresh, len: n });

            for &i in &opened {
                if !send_blocking(&senders[i].1, Msg::Chunk(Arc::clone(&chunk)), cancel) {
                    aborted = true;
                    break;
                }
                let tx = &senders[i].1;
                tel.queue(dests[i].dev, tx.len(), QUEUE_CAP);
            }
            if aborted {
                break;
            }
        }

        if aborted {
            abandon(senders, &opened);
            report.cancelled = cancel.load(Ordering::Relaxed);
            break;
        }
        if let Some(msg) = read_failed {
            abandon(senders, &opened);
            record_read_error(&mut report, item, dests, &msg, tel);
            continue;
        }

        for &i in &opened {
            send_blocking(&senders[i].1, Msg::Close(item.mtime.to_file_time()), cancel);
        }

        let sc = hasher.digest();
        report.source_hashes.insert(item.rel.clone(), sc);
        for meter in meters.values_mut() {
            meter.flush(tel);
        }
        tel.log(
            Level::Ok,
            Stage::Copy,
            format!(
                "{}  {:.3}s  {}  xxh {}…",
                short_name(&item.rel),
                started.elapsed().as_secs_f64(),
                opened
                    .iter()
                    .map(|&i| format!("{} ok", dests[i].dev.label()))
                    .collect::<Vec<_>>()
                    .join("  "),
                hex64_short(sc)
            ),
        );
    }

    for meter in meters.values_mut() {
        meter.flush(tel);
    }
    report
}

fn abandon(senders: &[(DeviceId, Sender<Msg>)], opened: &[usize]) {
    for &i in opened {
        // Best effort: on cancellation the writer may already be gone, and the
        // partial file is cleaned up by the writer's own teardown either way.
        let _ = senders[i].1.send_timeout(Msg::Abandon, SEND_POLL);
    }
}

fn record_read_error(
    report: &mut CopyReport,
    item: &CopyItem,
    dests: &[Destination],
    msg: &str,
    tel: &Telemetry,
) {
    tel.err(
        Stage::Copy,
        format!("{}: source read failed: {msg}", item.rel),
    );
    // The file is skipped on every destination, so record it against each: the
    // run is failed and the file is named, per the error-handling rules.
    for d in dests {
        report.errors.push(CopyError {
            dev: d.dev,
            rel: item.rel.clone(),
            msg: msg.to_string(),
        });
    }
}

fn short_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// One long-lived writer, driven by [`Msg`].
/// One destination's writer.
///
/// Deliberately takes no cancellation flag. Cancellation reaches a writer as a
/// message -- `Abandon` for the file in flight, `Stop` at the end, and a closed
/// channel if the reader dies without either -- so a second, racing route into
/// the same decision would buy nothing and could tear down a file the reader
/// still believes is open. The one place a flag could not help anyway is inside
/// `sync_all`, which cannot be interrupted.
fn writer_thread(dest: Destination, rx: Receiver<Msg>, tel: Telemetry) -> Vec<CopyError> {
    let mut errors = Vec::new();
    let mut meter = ByteMeter::new(dest.dev);
    let mut open: Option<(File, PathBuf)> = None;
    // Set when the current file has already failed, so the remaining chunks for
    // it are consumed without repeating the error.
    let mut poisoned = false;

    let rel_of = |path: &Path| -> String {
        path.strip_prefix(&dest.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };

    for msg in rx {
        match msg {
            Msg::Open(path, _size) => {
                poisoned = false;
                if let Some(parent) = path.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        errors.push(CopyError {
                            dev: dest.dev,
                            rel: rel_of(&path),
                            msg: format!("create directory: {e}"),
                        });
                        poisoned = true;
                        continue;
                    }
                }
                match File::create(&path) {
                    Ok(f) => open = Some((f, path)),
                    Err(e) => {
                        errors.push(CopyError {
                            dev: dest.dev,
                            rel: rel_of(&path),
                            msg: format!("create file: {e}"),
                        });
                        poisoned = true;
                    }
                }
            }
            Msg::Chunk(chunk) => {
                if poisoned {
                    continue;
                }
                let Some((file, path)) = open.as_mut() else {
                    continue;
                };
                if let Err(e) = file.write_all(chunk.bytes()) {
                    errors.push(CopyError {
                        dev: dest.dev,
                        rel: rel_of(path),
                        msg: format!("write: {e}"),
                    });
                    poisoned = true;
                    let path = path.clone();
                    open = None;
                    let _ = fs::remove_file(&path);
                    continue;
                }
                meter.add(chunk.len, &tel);
            }
            Msg::Close(mtime) => {
                meter.flush(&tel);
                let Some((file, path)) = open.take() else {
                    continue;
                };
                if poisoned {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                // Forces the bytes out of the OS dirty list, so the unbuffered
                // verify read measures the drive and not lazy writeback.
                let sync_started = Instant::now();
                let sync = file.sync_all();
                tel.trace(
                    Stage::Copy,
                    format!(
                        "{} sync_all {} took {:.1} ms",
                        dest.dev.label(),
                        rel_of(&path),
                        sync_started.elapsed().as_secs_f64() * 1000.0
                    ),
                );
                if let Err(e) = sync {
                    errors.push(CopyError {
                        dev: dest.dev,
                        rel: rel_of(&path),
                        msg: format!("sync_all: {e}"),
                    });
                    let _ = fs::remove_file(&path);
                    continue;
                }
                drop(file);
                if let Err(e) = filetime::set_file_mtime(&path, mtime) {
                    // The bytes are right; only the timestamp is not. That costs
                    // a needless re-copy on resume, nothing more.
                    tel.warn(
                        Stage::Copy,
                        format!(
                            "{} {}: could not set mtime: {e}",
                            dest.dev.label(),
                            rel_of(&path)
                        ),
                    );
                }
            }
            Msg::Abandon => {
                meter.flush(&tel);
                if let Some((file, path)) = open.take() {
                    drop(file);
                    // Partial destination files never survive; sources are never
                    // touched either way.
                    let _ = fs::remove_file(&path);
                }
            }
            Msg::Stop => break,
        }
    }

    // Cancellation mid-file leaves a handle open here.
    if let Some((file, path)) = open.take() {
        drop(file);
        let _ = fs::remove_file(&path);
    }
    meter.flush(&tel);
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::reconcile::Pairing;
    use crate::engine::unbuffered::{hash_unbuffered, CHUNK};
    use std::io::Read;

    /// `already_present` and `would_overwrite` partition one set: a file resume
    /// will not skip is a file the writer opens with `File::create`, and that
    /// truncates.
    ///
    /// They drifted once. `already_present` learned to reject cloud
    /// placeholders -- a dehydrated file reports the full logical size and the
    /// original mtime while holding none of the bytes -- and `would_overwrite`
    /// kept its own copy of the size-and-mtime rules. So a placeholder at the
    /// destination was neither skipped as present nor refused as a clash: it
    /// was truncated, and deleted outright if the run was then cancelled or hit
    /// a bad sector on the card.
    ///
    /// A placeholder cannot be synthesised here -- the attribute comes from a
    /// cloud filter driver, not from anything a test can set -- so what this
    /// pins is the agreement itself, over every case that can be built. The
    /// placeholder arm is covered by `would_overwrite` calling
    /// `already_present` rather than repeating it.
    #[test]
    fn resume_and_the_overwrite_guard_never_disagree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("card");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();

        let same = item(&src, "DCIM/SAME.ARW", b"0123456789");
        let resized = item(&src, "DCIM/RESIZED.ARW", b"0123456789");
        let restamped = item(&src, "DCIM/RESTAMPED.ARW", b"0123456789");
        let absent = item(&src, "DCIM/ABSENT.ARW", b"0123456789");

        // Land all three at the destination as byte-identical copies, mtime and
        // all: that is what a resumable folder looks like.
        for it in [&same, &resized, &restamped] {
            let d = dest_path(&dest, &it.rel);
            fs::create_dir_all(d.parent().unwrap()).unwrap();
            fs::copy(&it.src, &d).unwrap();
            let m = fs::metadata(&it.src).unwrap();
            filetime::set_file_mtime(&d, filetime::FileTime::from_last_modification_time(&m))
                .unwrap();
        }
        // Now make two of them into something else: a different file of a
        // different length, and one stamped at a wholly different time.
        fs::write(dest_path(&dest, &resized.rel), b"different").unwrap();
        filetime::set_file_mtime(
            dest_path(&dest, &restamped.rel),
            filetime::FileTime::from_unix_time(1_000_000, 0),
        )
        .unwrap();

        for it in [&same, &resized, &restamped, &absent] {
            let skipped = already_present(&dest, it);
            let refused = !would_overwrite(&dest, std::slice::from_ref(it)).is_empty();
            assert!(
                !(skipped && refused),
                "{}: skipped by resume and refused by preflight at once",
                it.rel
            );
            // The one that matters: every file the writer will open must have
            // been seen by the guard first, unless nothing is there at all.
            let exists = dest_path(&dest, &it.rel).exists();
            assert_eq!(
                refused,
                exists && !skipped,
                "{}: exists={exists} skipped={skipped} refused={refused}",
                it.rel
            );
        }
    }

    fn item(root: &Path, rel: &str, bytes: &[u8]) -> CopyItem {
        let src = dest_path(root, rel);
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, bytes).unwrap();
        let meta = fs::metadata(&src).unwrap();
        CopyItem {
            rel: rel.into(),
            src_dev: DeviceId::Card1,
            src,
            size: bytes.len() as u64,
            mtime: Mtime::from_metadata(&meta),
            pairing: Pairing::Twinned,
        }
    }

    fn run(items: &[CopyItem], roots: &[PathBuf]) -> (CopyReport, Telemetry) {
        let dests: Vec<Destination> = roots
            .iter()
            .zip([DeviceId::DestA, DeviceId::DestB, DeviceId::DestC])
            .map(|(root, dev)| Destination {
                dev,
                root: root.clone(),
            })
            .collect();
        let (tel, rx) = Telemetry::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let report = run_copy(items, &dests, &tel, &cancel).unwrap();
        drop(rx);
        (report, tel)
    }

    #[test]
    fn fans_one_source_out_to_two_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let b = dir.path().join("b");

        // Spans several chunks with a non-sector-aligned tail.
        let payload: Vec<u8> = (0..(2 * CHUNK + 1234)).map(|i| (i % 251) as u8).collect();
        let items = vec![
            item(&card, "DCIM/100MSDCF/DSC00001.ARW", &payload),
            item(&card, "DCIM/100MSDCF/DSC00002.ARW", b"small"),
        ];

        let (report, _tel) = run(&items, &[a.clone(), b.clone()]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.cancelled);
        assert_eq!(report.bytes_read, payload.len() as u64 + 5);

        for root in [&a, &b] {
            let copied = dest_path(root, "DCIM/100MSDCF/DSC00001.ARW");
            let mut got = Vec::new();
            File::open(&copied).unwrap().read_to_end(&mut got).unwrap();
            assert_eq!(got, payload, "bytes differ on {}", root.display());
        }

        // Sc must equal an independent unbuffered hash of the destination.
        let sc = report.source_hashes["DCIM/100MSDCF/DSC00001.ARW"];
        let (dest_hash, _) = hash_unbuffered(&dest_path(&a, "DCIM/100MSDCF/DSC00001.ARW")).unwrap();
        assert_eq!(sc, dest_hash);
    }

    #[test]
    fn preserves_mtime_so_resume_can_recognise_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let items = vec![item(&card, "DCIM/X.ARW", b"hello world")];

        run(&items, std::slice::from_ref(&a));
        let meta = fs::metadata(dest_path(&a, "DCIM/X.ARW")).unwrap();
        assert!(Mtime::from_metadata(&meta).matches(items[0].mtime));
    }

    /// Test 9: a second run must skip what is already there.
    #[test]
    fn resume_skips_files_already_present_on_every_destination() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let items = vec![
            item(&card, "DCIM/X.ARW", b"hello world"),
            item(&card, "DCIM/Y.ARW", b"second file"),
        ];

        let (first, _) = run(&items, std::slice::from_ref(&a));
        assert!(first.skipped_resume.is_empty());
        assert_eq!(first.source_hashes.len(), 2);

        let (second, _) = run(&items, std::slice::from_ref(&a));
        assert_eq!(second.skipped_resume.len(), 2);
        assert_eq!(second.bytes_read, 0, "resume must not re-read the source");
        assert!(
            second.source_hashes.is_empty(),
            "a skipped file has no in-flight hash, which diagnose() must tolerate"
        );
    }

    #[test]
    fn resume_still_writes_destinations_that_are_missing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let items = vec![item(&card, "DCIM/X.ARW", b"hello world")];

        run(&items, std::slice::from_ref(&a));
        let (report, _) = run(&items, &[a.clone(), b.clone()]);

        assert!(report.skipped_resume.is_empty(), "B still needs the file");
        assert!(dest_path(&b, "DCIM/X.ARW").exists());
        assert_eq!(report.source_hashes.len(), 1);
    }

    /// Test 8: cancelling must leave no partial files behind, and must not touch
    /// the source.
    #[test]
    fn cancellation_removes_partials_and_leaves_the_source_alone() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let payload: Vec<u8> = (0..(8 * CHUNK)).map(|i| (i % 251) as u8).collect();
        let items = vec![item(&card, "DCIM/BIG.ARW", &payload)];
        let (before, _) = hash_unbuffered(&items[0].src).unwrap();

        let dests = vec![Destination {
            dev: DeviceId::DestA,
            root: a.clone(),
        }];
        let (tel, rx) = Telemetry::new();
        let cancel = Arc::new(AtomicBool::new(true)); // cancelled before it starts
        let report = run_copy(&items, &dests, &tel, &cancel).unwrap();
        drop(rx);

        assert!(report.cancelled);
        assert!(
            !dest_path(&a, "DCIM/BIG.ARW").exists(),
            "a partial destination file must not survive a cancel"
        );
        let (after, _) = hash_unbuffered(&items[0].src).unwrap();
        assert_eq!(before, after, "the source must be byte-identical");
    }

    #[test]
    fn a_missing_source_file_is_recorded_against_every_destination() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let items = vec![CopyItem {
            rel: "DCIM/GONE.ARW".into(),
            src_dev: DeviceId::Card1,
            src: dir.path().join("card").join("GONE.ARW"),
            size: 10,
            mtime: Mtime { secs: 0, nanos: 0 },
            pairing: Pairing::Twinned,
        }];

        let (report, _) = run(&items, &[a, b]);
        assert_eq!(report.errors.len(), 2);
        assert_eq!(report.failed_paths(), vec!["DCIM/GONE.ARW"]);
    }

    #[test]
    fn copies_a_file_sourced_from_card_two() {
        let dir = tempfile::tempdir().unwrap();
        let card2 = dir.path().join("card2");
        let a = dir.path().join("a");
        let mut it = item(&card2, "DCIM/ONLY_C2.ARW", b"only on the second card");
        it.src_dev = DeviceId::Card2;
        it.pairing = Pairing::OnlyOnC2;

        let (report, _) = run(std::slice::from_ref(&it), std::slice::from_ref(&a));
        assert!(report.errors.is_empty());
        assert_eq!(
            fs::read(dest_path(&a, "DCIM/ONLY_C2.ARW")).unwrap(),
            b"only on the second card"
        );
    }

    /// A transient glitch must cost seconds, not a 17-minute re-run.
    ///
    /// The pass runner fails DEST A once and succeeds the second time, which is
    /// exactly what a USB hiccup looks like.
    #[test]
    fn a_file_that_fails_once_is_recovered_by_the_retry() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let items = vec![
            item(&card, "DCIM/X.ARW", b"hello"),
            item(&card, "DCIM/Y.ARW", b"world"),
        ];
        let dests = vec![
            Destination {
                dev: DeviceId::DestA,
                root: dir.path().join("a"),
            },
            Destination {
                dev: DeviceId::DestB,
                root: dir.path().join("b"),
            },
        ];
        let (tel, rx) = Telemetry::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let report = run_copy_with(&items, &dests, &tel, &cancel, |its, ds, _t, _c| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            let mut r = CopyReport {
                bytes_read: its.iter().map(|i| i.size).sum(),
                ..Default::default()
            };
            for i in its {
                r.source_hashes.insert(i.rel.clone(), 0xABCD);
            }
            if n == 0 {
                r.errors.push(CopyError {
                    dev: DeviceId::DestA,
                    rel: "DCIM/X.ARW".into(),
                    msg: "write: the device is not ready".into(),
                });
            }
            assert_eq!(
                ds.len(),
                if n == 0 { 2 } else { 1 },
                "the retry targets only the failing drive"
            );
            Ok(r)
        })
        .unwrap();
        drop(tel);

        assert!(
            report.errors.is_empty(),
            "a recovered file must not fail the run"
        );
        assert_eq!(report.retried.len(), 1, "but it must be counted");
        assert_eq!(report.retried[0].dev, DeviceId::DestA);
        assert_eq!(report.retries_by_device()[&DeviceId::DestA], 1);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "exactly one retry, never a loop"
        );

        let log: Vec<String> = rx.iter().filter_map(|r| r.log_line()).collect();
        assert!(log.iter().any(|l| l.contains("retrying once")));
        assert!(log.iter().any(|l| l.contains("recovered on retry")));
        assert!(
            log.iter()
                .any(|l| l.contains("treat that drive as suspect")),
            "a recovered file still has to be loud"
        );
    }

    /// Failing twice is evidence about the drive, not bad luck.
    #[test]
    fn a_file_that_fails_twice_still_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let items = vec![item(&card, "DCIM/X.ARW", b"hello")];
        let dests = vec![Destination {
            dev: DeviceId::DestA,
            root: dir.path().join("a"),
        }];
        let (tel, rx) = Telemetry::new();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = run_copy_with(&items, &dests, &tel, &cancel, |_its, _ds, _t, _c| {
            Ok(CopyReport {
                errors: vec![CopyError {
                    dev: DeviceId::DestA,
                    rel: "DCIM/X.ARW".into(),
                    msg: "write: the device is not ready".into(),
                }],
                ..Default::default()
            })
        })
        .unwrap();
        drop(tel);

        assert_eq!(report.errors.len(), 1);
        assert!(
            report.retried.is_empty(),
            "it never recovered, so nothing to count"
        );
        let _ = rx.iter().count();
    }

    /// Two reads of one file disagreeing is a reader fault, and saying so points
    /// at the cable rather than at the drive.
    #[test]
    fn a_retry_that_reads_different_bytes_is_called_out() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let items = vec![item(&card, "DCIM/X.ARW", b"hello")];
        let dests = vec![Destination {
            dev: DeviceId::DestA,
            root: dir.path().join("a"),
        }];
        let (tel, rx) = Telemetry::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let calls = std::sync::atomic::AtomicUsize::new(0);

        run_copy_with(&items, &dests, &tel, &cancel, |_its, _ds, _t, _c| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            let mut r = CopyReport::default();
            // A different hash on the second read of the same file.
            r.source_hashes.insert(
                "DCIM/X.ARW".to_string(),
                if n == 0 { 0x1111 } else { 0x2222 },
            );
            if n == 0 {
                r.errors.push(CopyError {
                    dev: DeviceId::DestA,
                    rel: "DCIM/X.ARW".into(),
                    msg: "write failed".into(),
                });
            }
            Ok(r)
        })
        .unwrap();
        drop(tel);

        let log: Vec<String> = rx.iter().filter_map(|r| r.log_line()).collect();
        assert!(
            log.iter()
                .any(|l| l.contains("read different bytes") && l.contains("contacts")),
            "got {log:#?}"
        );
    }

    /// A permanent obstruction on the real filesystem: the retry runs, fails
    /// again, and the error survives.
    #[test]
    fn a_permanently_blocked_destination_fails_after_its_retry() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let items = vec![item(&card, "DCIM/X.ARW", b"hello")];
        // A directory where the file needs to go: File::create can never win.
        fs::create_dir_all(dest_path(&a, "DCIM/X.ARW")).unwrap();

        let (report, _) = run(&items, std::slice::from_ref(&a));
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.retried.is_empty());
        assert_eq!(report.failed_paths(), vec!["DCIM/X.ARW"]);
    }

    #[test]
    fn empty_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("card");
        let a = dir.path().join("a");
        let items = vec![item(&card, "DCIM/EMPTY.ARW", b"")];
        let (report, _) = run(&items, std::slice::from_ref(&a));
        assert!(report.errors.is_empty());
        assert_eq!(
            fs::metadata(dest_path(&a, "DCIM/EMPTY.ARW")).unwrap().len(),
            0
        );
    }
}
