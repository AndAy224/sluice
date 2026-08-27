//! The offload engine: everything below the UI.
//!
//! [`run_job`] sequences the phases and is the whole engine API. It is fully
//! drivable without a window, which is what keeps the UI cuttable scope.

pub mod build_info;
pub mod config;
pub mod copy;
pub mod history;
pub mod mhl;
pub mod recheck;
pub mod reconcile;
pub mod scan;
pub mod selftest;
pub mod telemetry;
pub mod unbuffered;
pub mod verdict;
pub mod verify;
pub mod win;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Serialize;

use copy::Destination;
use mhl::{CreatorInfo, HashEntry, PhaseTiming, ReconSummary, SessionLog};
use telemetry::{DeviceInfo, Level, Phase, Stage, Telemetry};
use verdict::{Assessment, FileFailure, VerdictReport};
use verify::VerifyTarget;
use win::Distinctness;

/// The copies a job can involve.
///
/// Two cards and two destinations are the load-bearing four; the laptop SSD is a
/// bonus third destination that costs nothing and covers the window after a card
/// is formatted, when the only two copies are same-model HDDs travelling in the
/// same bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum DeviceId {
    Card1,
    Card2,
    DestA,
    DestB,
    /// Optional laptop SSD.
    DestC,
}

impl DeviceId {
    pub const CARDS: [DeviceId; 2] = [Self::Card1, Self::Card2];
    pub const DESTS: [DeviceId; 3] = [Self::DestA, Self::DestB, Self::DestC];

    /// Short form, as it appears in log columns and the comparison matrix.
    pub fn label(self) -> &'static str {
        match self {
            Self::Card1 => "C1",
            Self::Card2 => "C2",
            Self::DestA => "A",
            Self::DestB => "B",
            Self::DestC => "C",
        }
    }

    /// Long form, as it appears on the device strip.
    pub fn title(self) -> &'static str {
        match self {
            Self::Card1 => "CARD 1",
            Self::Card2 => "CARD 2",
            Self::DestA => "DEST A",
            Self::DestB => "DEST B",
            Self::DestC => "DEST C",
        }
    }

    pub fn is_card(self) -> bool {
        matches!(self, Self::Card1 | Self::Card2)
    }

    pub fn is_dest(self) -> bool {
        !self.is_card()
    }

    /// Which twin pair this device belongs to, if any.
    ///
    /// The UI paints by this: both cards share one hue, both required
    /// destinations share another, and the optional third destination is on its
    /// own because it is a bonus rather than half of a pair.
    pub fn kind(self) -> DeviceKind {
        match self {
            Self::Card1 | Self::Card2 => DeviceKind::Card,
            Self::DestA | Self::DestB => DeviceKind::Dest,
            Self::DestC => DeviceKind::Aux,
        }
    }

    /// The badge naming this device's twin, e.g. `TWIN C1·C2` on either card.
    ///
    /// Kept to characters the embedded font actually has: the mockup's circled
    /// digits render as tofu boxes in egui's bundled faces, and a box is worse
    /// than plain text at 11pm.
    pub fn twin_badge(self) -> Option<&'static str> {
        match self.kind() {
            DeviceKind::Card => Some("TWIN C1·C2"),
            DeviceKind::Dest => Some("TWIN A·B"),
            DeviceKind::Aux => None,
        }
    }
}

/// The three roles a device can play, which is what the UI colours by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DeviceKind {
    /// One of the two cards the camera wrote simultaneously.
    Card,
    /// One of the two destinations a clean verdict depends on.
    Dest,
    /// The optional laptop SSD: a bonus layer, not part of a pair.
    Aux,
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What to offload, and to where.
#[derive(Debug, Clone)]
pub struct JobConfig {
    pub card1: PathBuf,
    /// Absent is legal and means no twin verification, so never SAFE TO FORMAT.
    pub card2: Option<PathBuf>,
    /// Destination volumes or folders. A session folder is created under each.
    pub dest_roots: Vec<PathBuf>,
    /// Appended to the date in the session folder name.
    pub label: String,
    pub log_dir: PathBuf,
    /// How the engine learns which physical disk a path is on.
    ///
    /// `None` uses the real `IOCTL_STORAGE_GET_DEVICE_NUMBER`. A test injects
    /// one so the clean SAFE TO FORMAT path can be exercised at all -- see
    /// [`win::DeviceProbe`], which explains why this is a constructor parameter
    /// and never a runtime switch.
    pub probe: Option<Arc<dyn win::DeviceProbe>>,
    /// Where the per-device history lives. `None` means the machine's own file
    /// under `%APPDATA%`, which is what every real run wants.
    ///
    /// Overridable so the integration suite, which drives this function for
    /// real, does not write test sessions into it. Those records are what
    /// preflight warns from, and a suite that leaves twenty fake twin mismatches
    /// behind teaches the next real offload to cry wolf about the developer's
    /// own system drive.
    pub history_path: Option<PathBuf>,
}

impl JobConfig {
    /// A job with the real device probe.
    pub fn new(card1: PathBuf, card2: Option<PathBuf>, dest_roots: Vec<PathBuf>) -> Self {
        Self {
            card1,
            card2,
            dest_roots,
            label: "session".into(),
            log_dir: telemetry::default_log_dir(),
            probe: None,
            history_path: None,
        }
    }

    fn device_probe(&self) -> Arc<dyn win::DeviceProbe> {
        self.probe
            .clone()
            .unwrap_or_else(|| Arc::new(win::RealDeviceProbe))
    }
}

impl JobConfig {
    /// `<dest>\2026-03-14_shoot-01`
    pub fn session_dir(&self, dest_root: &Path, date: &str) -> PathBuf {
        dest_root.join(format!("{date}_{}", sanitise(&self.label)))
    }
}

/// Strip anything that cannot go in a Windows path component.
fn sanitise(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if r#"<>:"/\|?*"#.contains(c) || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim().to_string();
    if trimmed.is_empty() {
        "session".into()
    } else {
        trimmed
    }
}

/// What the job established.
#[derive(Debug)]
pub struct JobOutcome {
    pub verdict: VerdictReport,
    pub session: String,
    pub session_dirs: Vec<PathBuf>,
    /// The session JSON files this run actually wrote, one per destination.
    ///
    /// Carried rather than rebuilt from [`Self::session`]. The two names differ
    /// precisely when a manifest for that session id was already on the drive,
    /// which means the file the old code would have addressed belongs to an
    /// *earlier* run -- so a format confirmation stamped the wrong session's
    /// record with these cards.
    pub session_json: Vec<PathBuf>,
    pub log_path: PathBuf,
    /// The cards this session read, so a later format confirmation can name
    /// exactly which ones were erased.
    pub cards: Vec<history::DeviceRecord>,
}

impl JobConfig {
    /// The history file this job reads from and appends to.
    fn history(&self) -> PathBuf {
        self.history_path
            .clone()
            .unwrap_or_else(history::history_path)
    }
}

/// Run one offload, start to finish.
pub fn run_job(cfg: &JobConfig, tel: &Telemetry, cancel: &Arc<AtomicBool>) -> Result<JobOutcome> {
    let started_at = Utc::now();
    let session = telemetry::session_id(started_at);
    // Local, so the folder is filed under the night the shoot happened rather
    // than under tomorrow, and so a resume across midnight UTC still finds it.
    let date = telemetry::local_date(started_at);
    // Said once, so the log is self-describing. The clock in every line below is
    // the operator's; the `at` field stored beside it in the JSONL is UTC. A
    // record read in two years should not have to guess which.
    tel.info(
        Stage::Pre,
        format!(
            "session {session} — times below are local ({}); stored timestamps are UTC",
            telemetry::local_offset(started_at)
        ),
    );
    let mut timings: Vec<PhaseTiming> = Vec::new();
    let mut devices: BTreeMap<String, DeviceInfo> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();

    // A card selected twice would make the twin check compare a card against
    // itself: every hash would agree, the run would end in SAFE TO FORMAT, and
    // nothing would have been verified against a second piece of NAND. Caught
    // here by path, and again below by device identity.
    if let Some(c2) = &cfg.card2 {
        if same_location(&cfg.card1, c2) {
            bail!(
                "card 1 and card 2 are the same location ({}) -- there is no twin to check \
                 against, and a run like this would report SAFE TO FORMAT having verified \
                 nothing",
                cfg.card1.display()
            );
        }
    }

    // ---- SCAN ------------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Scan);
    // Cancellable, and it has to be. `scan_cb` was written for this, takes the
    // flag and a progress callback, and was never called -- the first cancel
    // read in the whole job was in the copy loop. So during the scan the Cancel
    // button did nothing, Ctrl-C did nothing, the window visibly refused to
    // close, and the status line said "cancelling -- partial destination files
    // are being removed" while nothing of the sort was happening. On a card with
    // 30,000 files that is a long time to be lying to someone.
    let Some(c1) = scan_card(&cfg.card1, DeviceId::Card1, tel, cancel)? else {
        return Err(cancelled());
    };
    tel.info(
        Stage::Scan,
        format!(
            "card 1: {} files, {:.1} GB",
            c1.file_count(),
            c1.total_bytes as f64 / 1e9
        ),
    );
    let c2 = match &cfg.card2 {
        Some(p) => {
            let Some(s) = scan_card(p, DeviceId::Card2, tel, cancel)? else {
                return Err(cancelled());
            };
            tel.info(
                Stage::Scan,
                format!(
                    "card 2: {} files, {:.1} GB",
                    s.file_count(),
                    s.total_bytes as f64 / 1e9
                ),
            );
            Some(s)
        }
        None => {
            tel.warn(
                Stage::Scan,
                "card 2 not supplied -- no twin verification, so this session cannot end in \
                 SAFE TO FORMAT",
            );
            None
        }
    };
    // Counted, not just logged. An unreadable directory takes its whole subtree
    // with it, so those files never enter the copy list and nothing downstream
    // can notice they are missing -- the verdict has to be told directly.
    let mut scan_errors = 0usize;
    for s in [Some(&c1), c2.as_ref()].into_iter().flatten() {
        for e in &s.errors {
            scan_errors += 1;
            errors.push(format!("scan: {e}"));
            tel.err(Stage::Scan, e.clone());
        }
    }
    // Names that cannot survive the trip to an NTFS volume. Every one of these
    // ends in either silent loss -- two files colliding into one -- or an
    // access-denied nineteen minutes in, and both are worse than being told now
    // which four files to rename. A camera card produces none of them; a folder
    // somebody picked by hand can produce all of them.
    let hazards: Vec<scan::Hazard> = [Some(&c1), c2.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(scan::hazards)
        .collect();
    if !hazards.is_empty() {
        for h in &hazards {
            tel.err(Stage::Scan, h.describe());
        }
        bail!(
            "{} file(s) cannot be copied to a Windows volume unchanged:\n{}\n\nRefusing to \
             start: copying these would either lose one of a colliding pair silently or fail \
             partway through.",
            hazards.len(),
            hazards
                .iter()
                .take(10)
                .map(|h| format!("  - {}", h.describe()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    timings.push(PhaseTiming {
        phase: "scan".into(),
        ms: t.elapsed().as_millis() as u64,
    });

    // ---- RECONCILE -------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Reconcile);
    let recon = reconcile::reconcile(&c1, c2.as_ref());
    let level = if recon.twins_complete() {
        Level::Ok
    } else {
        Level::Warn
    };
    tel.log(level, Stage::Recon, recon.summary());
    // A camera in relay or split mode produces a wall of untwinned files for a
    // reason that lives in a menu, not in this program. Saying which mode it
    // looks like turns a baffling refusal into an instruction.
    if let Some(mode) = recon.mode {
        tel.log(
            if mode.is_twinned() {
                Level::Info
            } else {
                Level::Warn
            },
            Stage::Recon,
            mode.describe(),
        );
    }
    tel.emit(telemetry::Event::Plan {
        files: recon.items.len(),
        bytes: recon.total_bytes,
    });
    for c in &recon.conflicts {
        tel.err(
            Stage::Recon,
            format!(
                "{}: {} B on card 1, {} B on card 2 -- copied from neither",
                c.rel, c.c1_size, c.c2_size
            ),
        );
    }
    timings.push(PhaseTiming {
        phase: "reconcile".into(),
        ms: t.elapsed().as_millis() as u64,
    });

    // ---- PREFLIGHT -------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Preflight);
    let _awake = match win::KeepAwake::arm() {
        Ok(g) => {
            tel.info(Stage::Power, "ES_SYSTEM_REQUIRED asserted");
            Some(g)
        }
        Err(e) => {
            tel.warn(Stage::Power, format!("could not block idle sleep: {e:#}"));
            None
        }
    };
    report_lid_policy(tel);
    // Keep-awake stops the machine idling to sleep. It does nothing about a
    // battery running out, and a 91 GB offload is not a battery-friendly
    // workload.
    if let Some(power) = win::power_status() {
        match power.warns_before_a_long_copy() {
            Some(w) => tel.warn(Stage::Power, w),
            None => tel.info(
                Stage::Power,
                match power.battery_percent {
                    Some(p) => format!("on mains power ({p}% battery)"),
                    None => "on mains power".into(),
                },
            ),
        }
    }

    let probe = cfg.device_probe();
    let mut card_volumes: Vec<(DeviceId, win::VolumeInfo)> = Vec::new();
    for (dev, path) in [
        (DeviceId::Card1, Some(&cfg.card1)),
        (DeviceId::Card2, cfg.card2.as_ref()),
    ] {
        if let Some(p) = path {
            if let Some(info) = capture_device(tel, dev, p, probe.as_ref()) {
                card_volumes.push((dev, info.volume.clone()));
                devices.insert(dev.label().into(), info);
            }
        }
    }
    // The device-identity half of the same guard. Two *folders* on one volume are
    // a legitimate if unusual setup, so this is not an abort -- but two cards on
    // one physical device means nothing was checked against a real twin, and that
    // must never reach a format verdict.
    let cards_distinct = (card_volumes.len() == 2)
        .then(|| win::distinctness(&card_volumes[0].1, &card_volumes[1].1));
    if cards_distinct == Some(Distinctness::SameDevice) {
        tel.warn(
            Stage::Pre,
            format!(
                "card 1 and card 2 are on the same physical device ({}) -- nothing here is \
                 checked against a real twin. Some dual-slot readers present both cards \
                 behind one bridge; two separate readers on two ports report separately",
                card_volumes[0].1.serial_hex()
            ),
        );
    }

    let mut session_dirs = Vec::new();
    let mut dest_locks: Vec<win::SingleInstance> = Vec::new();
    let mut dests: Vec<Destination> = Vec::new();
    let mut dest_volumes: Vec<(DeviceId, win::VolumeInfo)> = Vec::new();
    // Destinations whose bytes cannot be read back off a device. See
    // `DriveType::verification_reaches_the_device`.
    let mut unverifiable_dests: Vec<DeviceId> = Vec::new();
    for (i, root) in cfg.dest_roots.iter().enumerate() {
        let dev = *DeviceId::DESTS
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("at most {} destinations", DeviceId::DESTS.len()))?;
        let dir = cfg.session_dir(root, &date);
        // A bare "Access is denied. (os error 5)" is the most likely way a first
        // run fails on somebody else's machine: Windows protects Documents,
        // Pictures, Videos and Desktop by default and reports the block as a
        // generic permission error. Name the feature and the fix.
        fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("{}", win::explain_write_failure(root, &e)))?;
        win::probe_writable(&dir)?;

        // Over SMB, FILE_FLAG_NO_BUFFERING is advisory: a verify read can be
        // served from the redirector's cache or the server's, which makes it a
        // re-read of what was just written rather than independent evidence.
        // Still a fine place to put files; just not one that can vouch for a
        // card.
        let kind = win::drive_type_of(root);
        if !kind.verification_reaches_the_device() {
            unverifiable_dests.push(dev);
            tel.warn(
                Stage::Pre,
                format!(
                    "{} is a {} location -- files will be copied and hashed, but an \
                     unbuffered read there is advisory, so this copy cannot contribute \
                     evidence about the cards",
                    root.display(),
                    kind.describe()
                ),
            );
        }

        // §PREFLIGHT: "destination folders empty or resumable". A folder is
        // resumable when everything already in it that this run also carries is
        // byte-for-byte the same file; anything else means a previous session's
        // work is sitting where this one is about to write, and `File::create`
        // truncates without asking.
        //
        // The trigger is not exotic. The session folder is named from the date
        // and the label alone, the label is remembered and pre-filled, and two
        // camera bodies both number from DSC00001.ARW -- so a second card pair
        // on the same day is the ordinary case, not the corner case. The run
        // that overwrites verifies its own files perfectly and would report SAFE
        // TO FORMAT while the earlier frames are already gone.
        let clashes = copy::would_overwrite(&dir, &recon.items);
        if !clashes.is_empty() {
            for c in clashes.iter().take(10) {
                tel.err(
                    Stage::Pre,
                    format!(
                        "{} already holds something else here -- {}",
                        dir.display(),
                        c.describe()
                    ),
                );
            }
            // A placeholder needs different advice: the folder is not another
            // session's, and changing the label would not help.
            let advice = if clashes.iter().all(|c| c.reason == copy::Clash::Modified) {
                "Every one of those is the same size as the file about to replace it but was \
                 modified after it was written -- so this is most likely this session's own \
                 earlier copy, changed by something since. Re-check it with `sluice verify \
                 --drive` before letting anything overwrite it: a copy that changed on its own \
                 is the case this program exists to catch."
            } else if clashes.iter().any(|c| c.reason == copy::Clash::Placeholder) {
                "Some of those are cloud placeholders -- the names are on this drive but the \
                 bytes are not, so this run would replace a copy it cannot read, and would \
                 delete it outright if the run were then interrupted. Restore them (\"Always \
                 keep on this device\"), or offload to a folder that is not cloud-synced."
            } else {
                "That folder is another session's work -- most likely an earlier card from \
                 today, since the folder is named for the date and label only. Use a different \
                 --label (or a different session name in the window) and nothing is lost."
            };
            bail!(
                "{} already holds {} file(s) that this session would replace:\n{}\n\nRefusing \
                 to start. {advice}",
                dir.display(),
                clashes.len(),
                clashes
                    .iter()
                    .take(10)
                    .map(|c| format!("  - {}", c.describe()))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        let free =
            win::free_space(&dir).with_context(|| format!("free space on {}", root.display()))?;
        // What resume will actually write, not the whole session: demanding the
        // full total would refuse a resumed run onto a drive that has room for
        // everything still outstanding.
        let need = copy::bytes_needed(&dir, &recon.items);
        if free < need {
            bail!(
                "{} has {:.2} GB free, needs {:.2} GB -- refusing to start rather than fail at 80%",
                root.display(),
                free as f64 / 1e9,
                need as f64 / 1e9
            );
        }
        tel.info(
            Stage::Pre,
            format!(
                "{} {:.2} TB free, need {:.2} GB — ok",
                dir.display(),
                free as f64 / 1e12,
                need as f64 / 1e9
            ),
        );

        // How fast this drive actually is, while it is still cheap to act on.
        // A warning after a three-hour copy is a fact about a night that is
        // already gone; the same warning before it starts is a cable.
        //
        // Skipped for network locations, where throughput says nothing about a
        // bus and the copy is already flagged as unable to vouch for a card.
        if need > 0 && kind.verification_reaches_the_device() {
            match win::measure_write_mbps(&dir) {
                Ok(mbps) => {
                    let name = cfg
                        .dest_roots
                        .get(i)
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|| dev.title().to_string());
                    tel.info(
                        Stage::Pre,
                        format!("{} writes at {mbps:.0} MB/s", dir.display()),
                    );
                    if let Some(note) = win::slow_link_note(&name, mbps, need) {
                        tel.warn(Stage::Pre, note);
                    }
                }
                // Never fatal: probe_writable above already proved the drive
                // takes a file, so a failure here is the measurement's problem
                // rather than the destination's.
                Err(e) => tel.trace(Stage::Pre, format!("speed probe: {e:#}")),
            }
        }
        if let Some(info) = capture_device(tel, dev, &dir, probe.as_ref()) {
            dest_volumes.push((dev, info.volume.clone()));
            devices.insert(dev.label().into(), info);
        }
        // Two jobs writing one session folder would interleave files and leave a
        // tree neither could account for. Keyed to the folder, so an unrelated
        // offload to a different drive stays allowed.
        match win::SingleInstance::for_destination(&dir) {
            Ok(Some(guard)) => dest_locks.push(guard),
            Ok(None) => bail!(
                "another sluice offload is already writing {} -- two of them would \
                 interleave files and leave a tree neither could account for",
                dir.display()
            ),
            Err(e) => tel.warn(
                Stage::Pre,
                format!("could not lock {}: {e:#}", dir.display()),
            ),
        }

        session_dirs.push(dir.clone());
        dests.push(Destination { dev, root: dir });
    }
    if dests.is_empty() {
        bail!("a job needs at least one destination");
    }

    // A destination sharing a device with a card is a copy onto the source: it
    // eats the card's own free space and survives nothing the card does not.
    // Collected as well as logged. A warning twenty minutes before the banner
    // has scrolled away by the time anyone reads the verdict, and half the
    // redundancy sitting on the card you are being told to erase is not a
    // footnote.
    let mut dest_on_card_device: Vec<String> = Vec::new();
    for (cdev, cvol) in &card_volumes {
        for (ddev, dvol) in &dest_volumes {
            if win::distinctness(cvol, dvol) == Distinctness::SameDevice {
                let note = format!(
                    "{} and {} are on the same physical device ({})",
                    cdev.title(),
                    ddev.title(),
                    cvol.serial_hex()
                );
                tel.warn(
                    Stage::Pre,
                    format!("{note} -- that copy shares the fate of the card it came from"),
                );
                dest_on_card_device.push(note);
            }
        }
    }

    report_device_history(tel, &devices, &cfg.history());

    let distinctness = check_distinctness(tel, &dest_volumes);
    timings.push(PhaseTiming {
        phase: "preflight".into(),
        ms: t.elapsed().as_millis() as u64,
    });

    // ---- COPY ------------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Copy);
    let copy_report = copy::run_copy(&recon.items, &dests, tel, cancel)?;
    for e in &copy_report.errors {
        errors.push(format!("copy {}: {}: {}", e.dev.label(), e.rel, e.msg));
    }
    if !copy_report.skipped_resume.is_empty() {
        tel.info(
            Stage::Copy,
            format!(
                "{} file(s) already present on every destination, skipped",
                copy_report.skipped_resume.len()
            ),
        );
    }
    let copy_ms = t.elapsed().as_millis() as u64;
    timings.push(PhaseTiming {
        phase: "copy".into(),
        ms: copy_ms,
    });

    // ---- VERIFY ----------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Verify);
    let mut targets: Vec<VerifyTarget> = vec![VerifyTarget {
        dev: DeviceId::Card1,
        root: cfg.card1.clone(),
    }];
    if let Some(p) = &cfg.card2 {
        targets.push(VerifyTarget {
            dev: DeviceId::Card2,
            root: p.clone(),
        });
    }
    for d in &dests {
        targets.push(VerifyTarget {
            dev: d.dev,
            root: d.root.clone(),
        });
    }
    // Verify's workload is not the copy's. It re-reads *every* copy of every
    // file -- both cards and every destination -- so on a two-card, two-drive
    // night it is four times the bytes the copy moved, and it is the half
    // nobody has an intuition for.
    //
    // Without this the monitor kept the copy's denominator, which verify passes
    // within its first seconds, and then displayed "about 0s left" for the rest
    // of the pass. Counted per file against `expected_on`, so a relay-mode card
    // that genuinely does not hold a file is not counted as bytes to read.
    let verify_bytes: u64 = recon
        .items
        .iter()
        .map(|it| {
            let streams = targets
                .iter()
                .filter(|t| verify::expected_on(t.dev, it.pairing))
                .count() as u64;
            it.size * streams
        })
        .sum();
    tel.emit(telemetry::Event::Plan {
        files: recon.items.len(),
        bytes: verify_bytes,
    });
    tel.info(
        Stage::Verify,
        format!(
            "re-reading {:.1} GB across {} copies, unbuffered",
            verify_bytes as f64 / 1e9,
            targets.len()
        ),
    );

    let verify_report = verify::run_verify(
        &recon.items,
        &targets,
        &copy_report.source_hashes,
        tel,
        cancel,
    )?;
    for (dev, rel, msg) in &verify_report.errors {
        errors.push(format!("verify {}: {rel}: {msg}", dev.label()));
    }
    let verify_ms = t.elapsed().as_millis() as u64;
    timings.push(PhaseTiming {
        phase: "verify".into(),
        ms: verify_ms,
    });

    // ---- MANIFEST --------------------------------------------------------
    let t = Instant::now();
    tel.phase(Phase::Manifest);
    let failures: Vec<FileFailure> = verify_report
        .failures()
        .map(|f| FileFailure {
            rel: f.rel.clone(),
            diagnosis: f.diagnosis.clone(),
        })
        .collect();
    let aborted = verify_report
        .aborted
        .clone()
        .or_else(|| verify_report.cancelled.then(|| "cancelled".to_string()))
        .or_else(|| copy_report.cancelled.then(|| "cancelled".to_string()));

    let clean_run = failures.is_empty() && aborted.is_none() && recon.conflicts.is_empty();
    let finished_at = Utc::now();
    let creator = CreatorInfo::new(started_at, finished_at);
    // Session ids are second-granular, so two runs that finish inside the same
    // second share one -- and the second silently overwrote the first's
    // manifest, which is the file whose presence is the success signal. A
    // resumed run that finds everything already present takes well under a
    // second, so this is not hypothetical. The id in the log and the history
    // stays as it is; only the filenames move, and identically on every
    // destination so one session is not filed under two names on two drives.
    let file_session = unique_file_session(&session_dirs, &session);
    if file_session != session {
        tel.warn(
            Stage::Mhl,
            format!(
                "a manifest named for session {session} is already here -- writing this one as \
                 {file_session} rather than replacing it"
            ),
        );
    }
    let manifests_written = if clean_run {
        match write_manifests(
            &session_dirs,
            &file_session,
            &creator,
            &recon,
            &verify_report,
            tel,
        ) {
            Ok(()) => true,
            Err(e) => {
                tel.err(Stage::Mhl, format!("{e:#}"));
                errors.push(format!("manifest: {e:#}"));
                false
            }
        }
    } else {
        // Manifest presence is the success signal, so a failed run must not
        // leave one behind.
        tel.warn(
            Stage::Mhl,
            "run did not verify cleanly -- no manifest written",
        );
        false
    };
    timings.push(PhaseTiming {
        phase: "manifest".into(),
        ms: t.elapsed().as_millis() as u64,
    });

    // ---- VERDICT ---------------------------------------------------------
    tel.phase(Phase::Verdict);
    let assessment = Assessment {
        files_total: recon.items.len(),
        bytes_total: recon.total_bytes,
        failures,
        untwinned: recon
            .only_c1
            .iter()
            .chain(recon.only_c2.iter())
            .cloned()
            .collect(),
        conflicts: recon.conflicts.iter().map(|c| c.rel.clone()).collect(),
        had_card2: recon.had_card2,
        distinctness: Some(distinctness),
        cards_distinct,
        manifests_written,
        aborted,
        retries: copy_report.retries_by_device(),
        dest_count: dests.len(),
        unverifiable_dests,
        mode: recon.mode,
        scan_errors,
        dest_on_card_device,
    };
    let report = verdict::assess(&assessment);
    tel.log(
        match report.state {
            verdict::Verdict::SafeToFormat => Level::Ok,
            // The two structural tiers are informational, not warnings. Logging
            // them at WARN would put an amber line in the log of every night a
            // one-card photographer ever has, which is how a log stops being
            // read.
            verdict::Verdict::VerifiedOneSource | verdict::Verdict::VerifiedOneCopy => Level::Info,
            verdict::Verdict::VerifiedDoNotFormat => Level::Warn,
            verdict::Verdict::Failed => Level::Err,
        },
        Stage::Verdict,
        report.headline(),
    );
    tel.info(Stage::Verdict, report.state.guidance());
    for reason in &report.reasons {
        tel.info(Stage::Verdict, format!("→ {reason}"));
    }
    tel.perf(
        Stage::Verdict,
        format!(
            "copy {:.0}s · verify {:.0}s · {:.0}s total",
            copy_ms as f64 / 1000.0,
            verify_ms as f64 / 1000.0,
            tel.elapsed().as_secs_f64()
        ),
    );
    tel.emit(telemetry::Event::Verdict(report.clone()));

    // Captured before `devices` is moved into the session log.
    let device_records: Vec<history::DeviceRecord> = devices
        .iter()
        .map(|(slot, info)| history::DeviceRecord {
            slot: slot.clone(),
            serial: info.volume.serial,
            label: info.volume.label.clone(),
        })
        .collect();
    let suspect_cards = suspect_card_serials(&verify_report, &devices);

    // The session record goes out whatever the verdict, unlike the manifest: a
    // failed run is exactly when you most want the forensics.
    let log = SessionLog {
        tool: mhl::tool(),
        session: session.clone(),
        start: started_at,
        finish: finished_at,
        devices,
        phases: timings,
        reconciliation: ReconSummary {
            twinned: recon.twinned,
            only_c1: recon.only_c1.clone(),
            only_c2: recon.only_c2.clone(),
            conflicts: recon.conflicts.iter().map(|c| c.rel.clone()).collect(),
            had_card2: recon.had_card2,
        },
        files: verify_report.files.clone(),
        verdict: report.clone(),
        errors,
        formatted_after: None,
    };
    // Kept, rather than re-derived later from the session id: those two names
    // differ exactly when `unique_file_session` found an earlier run's files
    // here, so reconstructing the path is reconstructing the *previous* run's.
    let mut session_json = Vec::new();
    for dir in &session_dirs {
        let path = mhl::session_json_path(dir, &file_session);
        match mhl::write_session_json(&path, &log) {
            Ok(()) => session_json.push(path),
            Err(e) => tel.warn(Stage::Mhl, format!("session log: {e:#}")),
        }
    }

    // The durable record, outliving any one session folder. Best effort: a run
    // that verified must not be reported as failed because a log file could not
    // be appended to.
    let entry = history::Entry::Session {
        at: finished_at,
        session: session.clone(),
        verdict: format!("{:?}", report.state),
        devices: device_records.clone(),
        retries: copy_report
            .retries_by_device()
            .into_iter()
            .map(|(d, n)| (d.label().to_string(), n))
            .collect(),
        suspect_cards,
        failures: report.files_failed,
    };
    if let Err(e) = history::append_to(&cfg.history(), &entry) {
        tel.warn(
            Stage::Verdict,
            format!("could not write the history: {e:#}"),
        );
    }

    tel.phase(Phase::Done);
    let log_path = telemetry::log_path(&cfg.log_dir, &session);
    Ok(JobOutcome {
        verdict: report,
        session,
        session_dirs,
        session_json,
        log_path,
        cards: device_records
            .into_iter()
            .filter(|d| d.slot.starts_with('C'))
            .collect(),
    })
}

/// Serials of cards a twin mismatch pointed at this session.
///
/// These are what make the history worth keeping: a card that disagreed with its
/// twin once should be recognised the next time it is plugged in.
fn suspect_card_serials(
    report: &verify::VerifyReport,
    devices: &BTreeMap<String, DeviceInfo>,
) -> Vec<u32> {
    let mut out: Vec<u32> = report
        .failures()
        .filter_map(|f| match &f.diagnosis {
            verify::Diagnosis::TwinMismatch { suspect: Some(dev) } => {
                devices.get(dev.label()).map(|info| info.volume.serial)
            }
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Warn about any device the history has seen misbehave before.
fn report_device_history(tel: &Telemetry, devices: &BTreeMap<String, DeviceInfo>, path: &Path) {
    let entries = match history::read_from(path) {
        Ok(e) => e,
        Err(e) => {
            tel.warn(Stage::Pre, format!("could not read the history: {e:#}"));
            return;
        }
    };
    if entries.is_empty() {
        return;
    }
    let summary = history::summarise(&entries);
    for info in devices.values() {
        if let Some(warning) = summary.get(&info.volume.serial).and_then(|d| d.warning()) {
            tel.warn(Stage::Pre, warning);
        }
    }
}

/// A cancellation that reached us before there was anything to report.
fn cancelled() -> anyhow::Error {
    anyhow::anyhow!("cancelled during the scan -- nothing was copied")
}

/// Walk one card, honouring the cancel flag and reporting progress.
///
/// Also the one place that refuses a card slot pointed somewhere absurd. A card
/// is either a removable volume or a folder on one; the *root of a fixed disk*
/// never is. Without this, `--card1 C:\` was accepted without a word, cleared
/// the name hazards, passed a free-space check trivially against a 4 TB
/// destination, and went on to copy and unbuffered-verify several hundred
/// gigabytes of Windows. A folder on a fixed disk stays allowed -- that is a
/// staging directory, and it is what the tests use.
fn scan_card(
    path: &Path,
    dev: DeviceId,
    tel: &Telemetry,
    cancel: &AtomicBool,
) -> Result<Option<scan::Scan>> {
    if is_volume_root(path) {
        let kind = win::drive_type_of(path);
        if !kind.is_card_like() {
            bail!(
                "{} is the root of a {} volume, not a memory card. Point {} at a card \
                 reader, or at a folder -- copying an entire system or backup drive is \
                 not what this is for.",
                path.display(),
                kind.describe(),
                dev.title()
            );
        }
    }

    let mut last_report = 0usize;
    let scanned = scan::scan_cb(path, cancel, |files, bytes| {
        // Enough to show it is alive on a 30,000-file card, not enough to bury
        // the log.
        if files >= last_report + 2_000 {
            last_report = files;
            tel.info(
                Stage::Scan,
                format!(
                    "{}: {} files, {:.1} GB so far",
                    dev.title(),
                    files,
                    bytes as f64 / 1e9
                ),
            );
        }
    })
    .with_context(|| format!("scanning {}", path.display()))?;
    Ok(scanned)
}

/// Whether a path is a volume root such as `E:\`, rather than a folder on one.
fn is_volume_root(path: &Path) -> bool {
    win::volume_root(path).is_ok_and(|root| {
        let norm = |p: &str| p.trim_end_matches(['\\', '/']).to_ascii_lowercase();
        norm(&path.to_string_lossy()) == norm(&root)
    })
}

/// Whether two paths name the same place, allowing for trailing slashes and
/// case-insensitive Windows comparison.
fn same_location(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

fn capture_device(
    tel: &Telemetry,
    dev: DeviceId,
    path: &Path,
    probe: &dyn win::DeviceProbe,
) -> Option<DeviceInfo> {
    match win::volume_info_with(path, probe) {
        Ok(volume) => {
            let info = DeviceInfo {
                volume,
                free_bytes: win::free_space(path).unwrap_or(0),
                total_bytes: None,
            };
            tel.info(
                Stage::Pre,
                format!("{}: {}", dev.title(), info.volume.describe()),
            );
            tel.emit(telemetry::Event::Device {
                id: dev,
                info: Box::new(info.clone()),
            });
            Some(info)
        }
        Err(e) => {
            tel.warn(
                Stage::Pre,
                format!("{}: could not read volume identity: {e:#}", dev.title()),
            );
            None
        }
    }
}

/// Prove the two destinations are different physical drives.
///
/// Two identical LaCies can be mounted such that both destination paths land on
/// one drive, which produces a perfect-looking result with a single copy.
fn check_distinctness(tel: &Telemetry, dests: &[(DeviceId, win::VolumeInfo)]) -> Distinctness {
    if dests.len() < 2 {
        return Distinctness::Unproven("only one destination was supplied".into());
    }
    let (da, a) = &dests[0];
    let (db, b) = &dests[1];
    if a.label == b.label && !a.label.is_empty() {
        tel.warn(
            Stage::Pre,
            format!(
                "dest {} and dest {} carry the same label {:?}",
                da, db, a.label
            ),
        );
    }
    let d = win::distinctness(a, b);
    match &d {
        // Says what was actually checked. `win::distinctness` returns `Distinct`
        // on device-number inequality alone and never looks at serials, so the
        // old wording asserted a comparison the code does not make -- and on two
        // volumes sharing a serial it printed, verbatim, "serials differ
        // (30195459 / 30195459)". This is the line a user reads to believe the
        // distinctness claim.
        Distinctness::Distinct => tel.info(
            Stage::Pre,
            format!(
                "{} and {} are different physical devices (disk {} / disk {}) — ok",
                a.root,
                b.root,
                a.device_number
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                b.device_number
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
        ),
        Distinctness::SameDevice => tel.err(
            Stage::Pre,
            format!(
                "dest {da} and dest {db} are the SAME physical drive -- there would be only one copy"
            ),
        ),
        Distinctness::Unproven(why) => tel.warn(Stage::Pre, format!("distinctness unproven: {why}")),
    }
    d
}

fn report_lid_policy(tel: &Telemetry) {
    match win::lid_policy() {
        Ok(Some(policy)) => {
            if policy.ac.interrupts_job() || policy.dc.interrupts_job() {
                tel.warn(
                    Stage::Power,
                    format!(
                        "closing the lid will interrupt this job (on AC: {}, on battery: {}). \
                         Keep-awake blocks idle sleep only; a lid close cannot be vetoed. Fix: {}",
                        policy.ac.describe(),
                        policy.dc.describe(),
                        win::LID_FIX_COMMAND
                    ),
                );
            } else {
                tel.info(Stage::Power, "lid close action is 'do nothing' — ok");
            }
        }
        Ok(None) => tel.info(Stage::Power, "no lid-close setting on this machine"),
        Err(e) => tel.warn(
            Stage::Power,
            format!("could not read the lid policy: {e:#}"),
        ),
    }
}

/// A manifest basename that collides with nothing already in these folders.
///
/// Overwriting a previous run's manifest destroys the only record vouching for
/// the files that run copied -- and it does it silently, in the folder the
/// operator is about to trust. Reproduced live: two offloads into one session
/// folder inside the same second left one manifest, and the earlier card's
/// frames became files nothing vouched for.
fn unique_file_session(dirs: &[PathBuf], session: &str) -> String {
    let taken = |name: &str| {
        dirs.iter()
            .any(|d| mhl::mhl_path(d, name).exists() || mhl::session_json_path(d, name).exists())
    };
    if !taken(session) {
        return session.to_string();
    }
    (2..1000)
        .map(|n| format!("{session}-{n}"))
        .find(|c| !taken(c))
        .unwrap_or_else(|| format!("{session}-{}", std::process::id()))
}

fn write_manifests(
    dirs: &[PathBuf],
    session: &str,
    creator: &CreatorInfo,
    recon: &reconcile::Reconciliation,
    verify_report: &verify::VerifyReport,
    tel: &Telemetry,
) -> Result<()> {
    let hashed_at = Utc::now();
    let by_rel: BTreeMap<&str, &verify::FileVerdict> = verify_report
        .files
        .iter()
        .map(|f| (f.rel.as_str(), f))
        .collect();

    let mut entries = Vec::with_capacity(recon.items.len());
    for item in &recon.items {
        let Some(fv) = by_rel.get(item.rel.as_str()) else {
            bail!(
                "{} was never verified, refusing to put it in a manifest",
                item.rel
            );
        };
        // The manifest describes what is on the destination, so it records a
        // destination hash rather than a card's.
        let hash = fv
            .hashes
            .dests()
            .first()
            .map(|(_, h)| *h)
            .ok_or_else(|| anyhow::anyhow!("{} has no destination hash", item.rel))?;
        entries.push(HashEntry {
            rel: item.rel.clone(),
            size: item.size,
            mtime: item.mtime,
            hash,
            hashed_at,
        });
    }

    for dir in dirs {
        let path = mhl::mhl_path(dir, session);
        mhl::write_mhl(&path, creator, &entries)
            .with_context(|| format!("writing {}", path.display()))?;
        tel.ok(
            Stage::Mhl,
            format!("{} — {} entries", path.display(), entries.len()),
        );

        // The same files again, in the dialect the rest of the industry reads.
        // Best-effort: MHL v1.1 above is the manifest whose presence is the
        // success signal, because it is the one this program can re-verify
        // itself. A drive that cannot take a second small XML file is a problem
        // the verdict will already have noticed.
        match mhl::write_ascmhl(dir, session, creator, &entries) {
            Ok(p) => tel.ok(Stage::Mhl, format!("{} — ASC MHL v2", p.display())),
            Err(e) => tel.warn(
                Stage::Mhl,
                format!(
                    "could not write the ASC MHL copy to {}: {e:#} — the MHL v1 manifest \
                     above is unaffected",
                    dir.display()
                ),
            ),
        }
    }
    Ok(())
}

/// A fresh cancellation flag, raised from the UI or a Ctrl-C handler.
pub fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs whose session ids collide must not write over each other.
    ///
    /// Session ids are second-granular, so a resumed run that finds everything
    /// already present shares one with the run before it — and the second used
    /// to replace the first's manifest, the file whose presence is the success
    /// signal. Reproduced live before it was fixed.
    #[test]
    fn a_colliding_session_gets_its_own_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("destA");
        let b = dir.path().join("destB");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let dirs = vec![a.clone(), b.clone()];

        assert_eq!(unique_file_session(&dirs, "20260314-2214"), "20260314-2214");

        // An earlier run's manifest is already on one of the two drives.
        fs::write(mhl::mhl_path(&a, "20260314-2214"), "<hashlist/>").unwrap();
        let next = unique_file_session(&dirs, "20260314-2214");
        assert_eq!(next, "20260314-2214-2");
        // One name for every destination, so a single session is never filed
        // under two different names on two drives.
        assert!(!mhl::mhl_path(&b, &next).exists());

        // And it steps past whatever else is already there, in either dialect.
        fs::write(mhl::session_json_path(&b, &next), "{}").unwrap();
        assert_eq!(
            unique_file_session(&dirs, "20260314-2214"),
            "20260314-2214-3"
        );
    }

    #[test]
    fn device_labels_match_the_matrix() {
        assert_eq!(DeviceId::Card1.label(), "C1");
        assert_eq!(DeviceId::DestA.label(), "A");
        assert!(DeviceId::Card2.is_card());
        assert!(DeviceId::DestC.is_dest());
    }

    #[test]
    fn session_folders_are_date_prefixed() {
        let cfg = JobConfig {
            card1: "E:\\".into(),
            card2: None,
            dest_roots: vec!["D:\\".into()],
            label: "shoot-01".into(),
            log_dir: "C:\\logs".into(),
            probe: None,
            history_path: None,
        };
        assert_eq!(
            cfg.session_dir(Path::new("D:\\"), "2026-03-14"),
            Path::new("D:\\2026-03-14_shoot-01")
        );
    }

    #[test]
    fn labels_cannot_escape_into_path_syntax() {
        assert_eq!(sanitise("shoot/01"), "shoot-01");
        assert_eq!(sanitise("a:b*c?"), "a-b-c-");
        assert_eq!(sanitise("   "), "session");
        assert_eq!(sanitise(""), "session");
        assert_eq!(sanitise(".."), "session");
    }
}
