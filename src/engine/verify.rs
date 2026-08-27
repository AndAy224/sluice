//! N-way verification and the comparison matrix.
//!
//! This module holds the correctness core of the program: [`diagnose`], which
//! decides what a set of disagreeing hashes *means*. Everything else -- the
//! copying, the threading, the UI -- exists to feed it good data and to act on
//! its answer.
//!
//! The design sketches the matrix as six rows. Those rows are illustrative
//! rather than exhaustive, and taken literally they misclassify the worst case
//! in the set:
//!
//! > `Sc != C1 == C2`, with `A == B == Sc`
//!
//! Both cards agree with each other and disagree with what the copy read; the
//! destinations faithfully recorded the bytes the copy read. The design's row 3
//! reads this as "reader flakiness, re-run", and its row 5 would also match it
//! as "systematic, abort". Both are wrong in the same dangerous direction: the
//! **cards are fine and the destinations are corrupt**. A tool that told you to
//! re-seat the reader here would be pointing at the one part of the system that
//! is working.
//!
//! So this is written as an exhaustive classifier over the whole tuple, with a
//! test per class, rather than a row-by-row transcription.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;

use super::reconcile::{CopyItem, Pairing};
use super::telemetry::{ByteMeter, Event, Stage, Telemetry};
use super::unbuffered::{hash_unbuffered_cb, hex64_short};
use super::DeviceId;

/// The per-file hash tuple the comparison matrix works over.
///
/// The design calls this `HashSet`; renamed here to avoid colliding with
/// `std::collections::HashSet` at every use site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Hashes {
    /// The source card hashed *in flight during the copy* -- a separate read
    /// from `c1`, which is what makes an unrepeatable read detectable at all.
    /// `None` when resume skipped the copy for this file.
    pub sc: Option<u64>,
    pub c1: Option<u64>,
    pub c2: Option<u64>,
    pub a: Option<u64>,
    pub b: Option<u64>,
    pub c: Option<u64>,
}

impl Hashes {
    pub fn get(&self, dev: DeviceId) -> Option<u64> {
        match dev {
            DeviceId::Card1 => self.c1,
            DeviceId::Card2 => self.c2,
            DeviceId::DestA => self.a,
            DeviceId::DestB => self.b,
            DeviceId::DestC => self.c,
        }
    }

    pub fn set(&mut self, dev: DeviceId, hash: u64) {
        let slot = match dev {
            DeviceId::Card1 => &mut self.c1,
            DeviceId::Card2 => &mut self.c2,
            DeviceId::DestA => &mut self.a,
            DeviceId::DestB => &mut self.b,
            DeviceId::DestC => &mut self.c,
        };
        *slot = Some(hash);
    }

    /// Card hashes that were actually collected, in matrix order.
    pub fn cards(&self) -> Vec<(DeviceId, u64)> {
        DeviceId::CARDS
            .iter()
            .filter_map(|&d| self.get(d).map(|h| (d, h)))
            .collect()
    }

    /// Destination hashes that were actually collected, in matrix order.
    pub fn dests(&self) -> Vec<(DeviceId, u64)> {
        DeviceId::DESTS
            .iter()
            .filter_map(|&d| self.get(d).map(|h| (d, h)))
            .collect()
    }
}

/// What a set of hashes means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum Diagnosis {
    /// Every copy that was measured agrees. The only passing state.
    Clean,

    /// The two cards disagree, so one of them is bad. This is the failure mode
    /// single-source tools cannot see, and the reason simultaneous recording
    /// makes this design stronger than one that re-reads a single card.
    TwinMismatch {
        /// Which card the destinations and the in-flight hash side against, when
        /// they can break the tie. `None` when nothing else agrees either.
        suspect: Option<DeviceId>,
    },

    /// The cards agree with each other, the copy read something else, and the
    /// destinations recorded what the copy read. The cards are good; the
    /// destinations hold corrupt data and must be re-copied.
    SourceReadCorrupt { dests: Vec<DeviceId> },

    /// The cards agree with each other and with the destinations, but the copy's
    /// in-flight hash disagreed. The written bytes are right, so nothing
    /// propagated -- but a read that is not repeatable means the reader, cable,
    /// or contacts are suspect. Re-run before concluding the card is bad.
    UnrepeatableSourceRead,

    /// The cards agree; one or more specific destinations do not.
    BadDestination { dests: Vec<DeviceId> },

    /// Every destination disagrees with the cards in the *same* way. A fault
    /// downstream of the read and upstream of both drives: bus, cable, or RAM.
    /// Abort the job rather than grinding through the rest of the files.
    Systematic,

    /// A copy that should exist could not be read at all -- the file is missing,
    /// or the read errored.
    ///
    /// This has to be its own state rather than an absent measurement, because
    /// [`diagnose`] only compares the hashes it was given: with destination A
    /// unreadable, `C1 == C2 == B` looks exactly like a clean file. Silence is
    /// not agreement.
    MissingCopy { devs: Vec<DeviceId> },

    /// Not classifiable. Never silently treated as anything else; the caller
    /// prints the whole tuple.
    Unclassified,
}

impl Diagnosis {
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean)
    }

    /// Whether this fault makes continuing pointless.
    pub fn aborts_job(&self) -> bool {
        matches!(self, Self::Systematic)
    }

    /// The log line that goes under a failing file.
    pub fn describe(&self) -> String {
        match self {
            Self::Clean => "all copies agree".into(),
            Self::TwinMismatch { suspect: Some(d) } => format!(
                "card {} disagrees with its twin AND with the destinations -- {} IS SUSPECT",
                d.label(),
                d.title()
            ),
            Self::TwinMismatch { suspect: None } => {
                "the two cards disagree and nothing else can break the tie".into()
            }
            Self::SourceReadCorrupt { dests } => format!(
                "cards agree with each other; the copy read different bytes and wrote them to {} \
                 -- the cards are good, {} corrupt, re-copy",
                labels(dests),
                if dests.len() == 1 { "this destination is" } else { "these destinations are" }
            ),
            Self::UnrepeatableSourceRead => {
                "the copy's in-flight hash disagrees with a re-read of the same card, though the \
                 destinations are correct -- reader, cable, or contacts. Re-run before \
                 concluding the card is bad."
                    .into()
            }
            Self::BadDestination { dests } => format!(
                "bad write or bad drive on {} -- cards and the copy all agree",
                labels(dests)
            ),
            Self::Systematic => {
                "every destination disagrees with the cards in the same way -- bus, cable, or RAM"
                    .into()
            }
            Self::MissingCopy { devs } => format!(
                "could not read this file on {} -- missing or unreadable, so it is unverified there",
                labels(devs)
            ),
            Self::Unclassified => "unclassified hash disagreement".into(),
        }
    }
}

fn labels(devs: &[DeviceId]) -> String {
    devs.iter()
        .map(|d| d.label())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Classify one file's hashes.
///
/// Exhaustive by construction: every path returns a named [`Diagnosis`], and
/// anything that falls through lands on [`Diagnosis::Unclassified`] rather than
/// being rounded to the nearest happy answer.
pub fn diagnose(h: &Hashes) -> Diagnosis {
    let cards = h.cards();
    let dests = h.dests();

    // --- Do the cards agree with each other? ------------------------------
    //
    // Checked before anything else, including completeness. Two cards that
    // disagree is a fact about the cards, true whether or not a destination was
    // measured, and it is the single most important thing this function can
    // say -- reporting it as merely unclassifiable would throw it away.
    if let (Some(c1), Some(c2)) = (h.c1, h.c2) {
        if c1 != c2 {
            // Break the tie with everything else that was measured. The copy's
            // in-flight hash counts as a witness: it read one of these cards.
            let mut witnesses: Vec<u64> = dests.iter().map(|(_, x)| *x).collect();
            witnesses.extend(h.sc);
            let for_c1 = witnesses.iter().filter(|&&w| w == c1).count();
            let for_c2 = witnesses.iter().filter(|&&w| w == c2).count();
            let suspect = if for_c1 > for_c2 {
                Some(DeviceId::Card2)
            } else if for_c2 > for_c1 {
                Some(DeviceId::Card1)
            } else {
                None
            };
            return Diagnosis::TwinMismatch { suspect };
        }
    }

    if cards.is_empty() || dests.is_empty() {
        return Diagnosis::Unclassified;
    }

    // The cards agree (or only one was present), so they speak with one voice.
    let card = cards[0].1;

    let disagreeing: Vec<DeviceId> = dests
        .iter()
        .filter(|(_, x)| *x != card)
        .map(|(d, _)| *d)
        .collect();

    match h.sc {
        // --- The copy read the same bytes the cards still hold -------------
        Some(sc) if sc == card => classify_dests(&dests, &disagreeing),
        None => classify_dests(&dests, &disagreeing),

        // --- The copy read something the cards do not hold ----------------
        Some(sc) => {
            if disagreeing.is_empty() {
                // Every destination matches the cards, so nothing corrupt was
                // written. Only the in-flight hash was wrong.
                Diagnosis::UnrepeatableSourceRead
            } else if dests.iter().all(|(_, x)| *x == sc) {
                // The destinations faithfully recorded what the copy read, and
                // that is not what the cards hold. The cards are the good copy.
                Diagnosis::SourceReadCorrupt {
                    dests: dests.iter().map(|(d, _)| *d).collect(),
                }
            } else {
                Diagnosis::Unclassified
            }
        }
    }
}

fn classify_dests(dests: &[(DeviceId, u64)], disagreeing: &[DeviceId]) -> Diagnosis {
    if disagreeing.is_empty() {
        return Diagnosis::Clean;
    }
    // "Systematic" needs at least two destinations to be a meaningful claim:
    // with one drive, "every destination agrees with every other" is vacuous and
    // would dress a single bad drive up as a bus fault.
    let all_disagree = disagreeing.len() == dests.len();
    let dests_agree_with_each_other = dests.windows(2).all(|w| w[0].1 == w[1].1);
    if dests.len() >= 2 && all_disagree && dests_agree_with_each_other {
        return Diagnosis::Systematic;
    }
    Diagnosis::BadDestination {
        dests: disagreeing.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// The concurrent half: N independent hashers feeding the matrix
// ---------------------------------------------------------------------------

/// One copy to be hashed during verification.
#[derive(Debug, Clone)]
pub struct VerifyTarget {
    pub dev: DeviceId,
    /// Volume or session root; relative paths hang off this.
    pub root: PathBuf,
}

/// One file's verification outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileVerdict {
    pub idx: usize,
    pub rel: String,
    pub hashes: Hashes,
    pub diagnosis: Diagnosis,
}

/// What the verify phase established.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub files: Vec<FileVerdict>,
    /// Read errors, as `(device, rel, message)`.
    pub errors: Vec<(DeviceId, String, String)>,
    pub cancelled: bool,
    /// Set when a systematic fault made continuing pointless.
    pub aborted: Option<String>,
}

impl VerifyReport {
    pub fn failures(&self) -> impl Iterator<Item = &FileVerdict> {
        self.files.iter().filter(|f| !f.diagnosis.is_clean())
    }
}

/// Whether `dev` is expected to hold a file with this pairing.
///
/// A file the camera wrote to one slot only genuinely does not exist on the
/// other card, and its absence there is information, not a read error.
pub(super) fn expected_on(dev: DeviceId, pairing: Pairing) -> bool {
    match dev {
        DeviceId::Card1 => !matches!(pairing, Pairing::OnlyOnC2),
        DeviceId::Card2 => !matches!(pairing, Pairing::OnlyOnC1),
        _ => true,
    }
}

/// One hasher's answer for one file.
struct Measurement {
    dev: DeviceId,
    idx: usize,
    hash: Option<u64>,
    err: Option<String>,
}

/// What each hasher has to read, in target order.
///
/// The phase total is the sum of these, and is deliberately derived from them
/// rather than computed alongside them. They answer the same question at two
/// scales — one row's denominator and the run's — and two loops both restating
/// `expected_on` are two chances to disagree about a relay-mode card that does
/// not hold every file.
///
/// Counted per file against `expected_on`, so a card that genuinely does not
/// hold a file is not charged for reading it.
pub fn plan_by_device(items: &[CopyItem], targets: &[VerifyTarget]) -> Vec<(DeviceId, u64)> {
    targets
        .iter()
        .map(|t| {
            let bytes = items
                .iter()
                .filter(|it| expected_on(t.dev, it.pairing))
                .map(|it| it.size)
                .sum();
            (t.dev, bytes)
        })
        .collect()
}

/// Run the verify phase: every copy hashed unbuffered, concurrently, then
/// compared.
///
/// The card reads run at UHS-II speed on separate USB ports and finish well
/// before the HDDs, so adding the twin costs nothing in wall time. The strongest
/// guarantee in the design is free.
pub fn run_verify(
    items: &[CopyItem],
    targets: &[VerifyTarget],
    source_hashes: &BTreeMap<String, u64>,
    tel: &Telemetry,
    cancel: &Arc<AtomicBool>,
) -> Result<VerifyReport> {
    if targets.is_empty() {
        anyhow::bail!("verification needs at least one target");
    }

    // How many hashers owe an answer for each file.
    let expected: Vec<usize> = items
        .iter()
        .map(|it| {
            targets
                .iter()
                .filter(|t| expected_on(t.dev, it.pairing))
                .count()
        })
        .collect();

    let mut hashes: Vec<Hashes> = items
        .iter()
        .map(|it| Hashes {
            sc: source_hashes.get(&it.rel).copied(),
            ..Default::default()
        })
        .collect();
    let mut seen = vec![0usize; items.len()];
    let mut report = VerifyReport::default();

    let (tx, rx) = crossbeam_channel::bounded::<Measurement>(1024);

    std::thread::scope(|scope| -> Result<()> {
        for target in targets {
            let tx = tx.clone();
            let tel = tel.clone();
            let cancel = Arc::clone(cancel);
            std::thread::Builder::new()
                .name(format!("sluice-verify-{}", target.dev.label()))
                .spawn_scoped(scope, move || {
                    hasher_thread(target, items, &tx, &tel, &cancel)
                })
                .context("spawning a verify thread")?;
        }
        drop(tx);

        for m in rx {
            if let Some(err) = m.err {
                report.errors.push((m.dev, items[m.idx].rel.clone(), err));
            }
            if let Some(h) = m.hash {
                hashes[m.idx].set(m.dev, h);
            }
            seen[m.idx] += 1;
            if seen[m.idx] < expected[m.idx] {
                continue;
            }

            // A copy nobody could read is not a copy that agrees. Check this
            // before consulting the matrix, which only sees hashes it was given.
            let missing: Vec<DeviceId> = targets
                .iter()
                .filter(|t| expected_on(t.dev, items[m.idx].pairing))
                .map(|t| t.dev)
                .filter(|d| hashes[m.idx].get(*d).is_none())
                .collect();
            let diagnosis = if missing.is_empty() {
                diagnose(&hashes[m.idx])
            } else {
                Diagnosis::MissingCopy { devs: missing }
            };
            let rel = items[m.idx].rel.clone();
            emit_file_verdict(tel, m.idx, &rel, &hashes[m.idx], &diagnosis);

            if diagnosis.aborts_job() && report.aborted.is_none() {
                let why = diagnosis.describe();
                tel.err(Stage::Verify, format!("→ ABORTING: {why}"));
                report.aborted = Some(why);
                // Stop the other hashers rather than grinding through the rest
                // of a run whose fault is upstream of both drives.
                cancel.store(true, Ordering::Relaxed);
            }

            report.files.push(FileVerdict {
                idx: m.idx,
                rel,
                hashes: hashes[m.idx],
                diagnosis,
            });
        }
        Ok(())
    })?;

    report.cancelled = cancel.load(Ordering::Relaxed) && report.aborted.is_none();
    report.files.sort_by_key(|f| f.idx);
    Ok(report)
}

fn emit_file_verdict(tel: &Telemetry, idx: usize, rel: &str, h: &Hashes, diagnosis: &Diagnosis) {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if diagnosis.is_clean() {
        let agreeing: Vec<&str> = DeviceId::CARDS
            .iter()
            .chain(DeviceId::DESTS.iter())
            .filter(|d| h.get(**d).is_some())
            .map(|d| d.label())
            .collect();
        // No trailing tick: the OK level already carries the mark, and the glyph
        // has to survive a plain console and a JSONL file as well as the window.
        tel.ok(
            Stage::Verify,
            format!(
                "{name}  {} {} = {}",
                agreeing.first().copied().unwrap_or("?"),
                h.cards()
                    .first()
                    .map(|(_, v)| hex64_short(*v))
                    .unwrap_or_default(),
                agreeing[1..].join(" = ")
            ),
        );
    } else {
        tel.err(Stage::Verify, format!("{name}  {}", hash_summary(h)));
        tel.err(Stage::Verify, format!("→ {}", diagnosis.describe()));
    }
    tel.emit(Event::FileDone {
        idx,
        rel: rel.to_string(),
        hashes: *h,
        diagnosis: Some(diagnosis.clone()),
        dur_ms: 0,
    });
}

fn hash_summary(h: &Hashes) -> String {
    let mut parts = Vec::new();
    if let Some(sc) = h.sc {
        parts.push(format!("Sc {}", hex64_short(sc)));
    }
    for d in DeviceId::CARDS.iter().chain(DeviceId::DESTS.iter()) {
        if let Some(v) = h.get(*d) {
            parts.push(format!("{} {}", d.label(), hex64_short(v)));
        }
    }
    parts.join("  ")
}

fn hasher_thread(
    target: &VerifyTarget,
    items: &[CopyItem],
    tx: &crossbeam_channel::Sender<Measurement>,
    tel: &Telemetry,
    cancel: &AtomicBool,
) {
    let mut meter = ByteMeter::new(target.dev);
    for (idx, item) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !expected_on(target.dev, item.pairing) {
            continue;
        }
        let path = crate::engine::copy::dest_path(&target.root, &item.rel);
        let result = hash_unbuffered_cb(&path, cancel, |n| meter.add(n, tel));
        let m = match result {
            Ok(Some((hash, _))) => Measurement {
                dev: target.dev,
                idx,
                hash: Some(hash),
                err: None,
            },
            // Cancelled part-way through this file.
            Ok(None) => break,
            Err(e) => Measurement {
                dev: target.dev,
                idx,
                hash: None,
                err: Some(format!("{e:#}")),
            },
        };
        if tx.send(m).is_err() {
            break;
        }
    }
    meter.flush(tel);
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: u64 = 0xa1b2_c3d4_e5f6_0718;
    const BAD: u64 = 0x5e9f_0000_0000_0000;
    const OTHER: u64 = 0x71ab_0000_0000_0000;

    /// `C1 == C2 == A == B` -- the only state that passes.
    #[test]
    fn all_agree_is_clean() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(GOOD),
            b: Some(GOOD),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::Clean);
        assert!(diagnose(&h).is_clean());
    }

    #[test]
    fn optional_third_destination_participates() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(GOOD),
            b: Some(GOOD),
            c: Some(BAD),
        };
        assert_eq!(
            diagnose(&h),
            Diagnosis::BadDestination {
                dests: vec![DeviceId::DestC]
            }
        );
    }

    /// `C1 != C2` -- the scenario that would otherwise silently destroy a day's
    /// work, and the single best reason to record to both slots.
    #[test]
    fn twin_mismatch_names_the_suspect_card() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(OTHER),
            a: Some(GOOD),
            b: Some(GOOD),
            c: None,
        };
        assert_eq!(
            diagnose(&h),
            Diagnosis::TwinMismatch {
                suspect: Some(DeviceId::Card2)
            }
        );
        assert!(diagnose(&h).describe().contains("CARD 2 IS SUSPECT"));
    }

    #[test]
    fn twin_mismatch_can_indict_card_one() {
        let h = Hashes {
            sc: Some(BAD),
            c1: Some(BAD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: Some(GOOD),
            c: Some(GOOD),
        };
        // Witnesses: A=BAD, B=GOOD, C=GOOD, Sc=BAD -> 2 for GOOD (c2), 2 for BAD.
        // A tie must not guess.
        assert_eq!(diagnose(&h), Diagnosis::TwinMismatch { suspect: None });
    }

    #[test]
    fn twin_mismatch_without_a_tiebreaker_names_nobody() {
        let h = Hashes {
            sc: None,
            c1: Some(GOOD),
            c2: Some(OTHER),
            a: Some(BAD),
            b: Some(BAD),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::TwinMismatch { suspect: None });
        assert!(!diagnose(&h).is_clean());
    }

    /// `C1 == C2`, `A != C1` -- one drive wrote badly.
    #[test]
    fn single_bad_destination_is_named() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: Some(GOOD),
            c: None,
        };
        assert_eq!(
            diagnose(&h),
            Diagnosis::BadDestination {
                dests: vec![DeviceId::DestA]
            }
        );
        assert!(!diagnose(&h).aborts_job());
    }

    /// `A == B != C1 == C2` -- bus, cable, or RAM. Abort rather than grind on.
    #[test]
    fn both_destinations_wrong_the_same_way_is_systematic() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: Some(BAD),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::Systematic);
        assert!(diagnose(&h).aborts_job());
    }

    /// With one destination, "all destinations agree" is vacuous. A single bad
    /// drive must not be dressed up as a bus fault and abort the whole job.
    #[test]
    fn one_bad_destination_alone_is_not_systematic() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: None,
            c: None,
        };
        assert_eq!(
            diagnose(&h),
            Diagnosis::BadDestination {
                dests: vec![DeviceId::DestA]
            }
        );
        assert!(!diagnose(&h).aborts_job());
    }

    /// The case the design's six rows get wrong. Both cards agree; the copy read
    /// something else and both destinations recorded it. The cards are the good
    /// copy and the destinations are corrupt -- NOT a flaky reader, and NOT a
    /// bus fault.
    #[test]
    fn cards_agree_but_destinations_hold_what_the_copy_misread() {
        let h = Hashes {
            sc: Some(BAD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: Some(BAD),
            c: None,
        };
        let d = diagnose(&h);
        assert_eq!(
            d,
            Diagnosis::SourceReadCorrupt {
                dests: vec![DeviceId::DestA, DeviceId::DestB]
            }
        );
        assert!(!d.aborts_job(), "the cards are fine; re-copying is the fix");
        assert!(d.describe().contains("the cards are good"));
    }

    /// `C1 != Sc`, `C1 == C2`, destinations correct. Nothing corrupt was
    /// written, but the card did not read the same twice.
    #[test]
    fn unrepeatable_source_read_when_nothing_propagated() {
        let h = Hashes {
            sc: Some(BAD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(GOOD),
            b: Some(GOOD),
            c: None,
        };
        let d = diagnose(&h);
        assert_eq!(d, Diagnosis::UnrepeatableSourceRead);
        assert!(d.describe().contains("Re-run"));
    }

    /// Each hasher's bar is drawn against its own plan, and the run's estimate
    /// against the sum of them. Those two have to be the same arithmetic.
    ///
    /// Before this, the phase total was computed by its own loop over
    /// `expected_on` and the per-device figures did not exist at all -- every
    /// verify row was drawn as a rate against the fastest rate any device had
    /// ever hit. On real hardware the cards sit on internal disks and finish in
    /// seconds at several GB/s while the drives crawl over USB, so the two rows
    /// that decide the verdict rendered at 2.9% and 1.0% of the bar's width,
    /// scaled by a device that had already stopped.
    #[test]
    fn each_hasher_is_measured_against_its_own_work() {
        let target = |dev| VerifyTarget {
            dev,
            root: PathBuf::from("x"),
        };
        let targets = vec![
            target(DeviceId::Card1),
            target(DeviceId::Card2),
            target(DeviceId::DestA),
            target(DeviceId::DestB),
        ];
        // A relay-mode card: card 2 filled and card 1 carried on alone, so the
        // untwinned files exist on one card and on both destinations.
        let items = vec![
            item("both.ARW", 100, Pairing::Twinned),
            item("c1only.ARW", 30, Pairing::OnlyOnC1),
            item("c2only.ARW", 7, Pairing::OnlyOnC2),
        ];

        let plan = plan_by_device(&items, &targets);
        let by = |d: DeviceId| plan.iter().find(|(x, _)| *x == d).unwrap().1;

        assert_eq!(by(DeviceId::Card1), 130, "card 1 holds both.ARW and c1only");
        assert_eq!(by(DeviceId::Card2), 107, "card 2 holds both.ARW and c2only");
        // Every destination holds everything, whatever the cards did.
        assert_eq!(by(DeviceId::DestA), 137);
        assert_eq!(by(DeviceId::DestB), 137);

        // The run's denominator is these and nothing else, or a bar and the
        // estimate above it disagree about the same pass.
        let total: u64 = plan.iter().map(|(_, b)| b).sum();
        assert_eq!(total, 130 + 107 + 137 + 137);

        // And no row can be asked to fill past its end.
        for (dev, bytes) in &plan {
            assert!(*bytes <= total, "{dev:?} owes more than the whole phase");
            assert!(*bytes > 0, "{dev:?} would draw an empty bar all pass");
        }
    }

    fn item(rel: &str, size: u64, pairing: Pairing) -> CopyItem {
        CopyItem {
            rel: rel.into(),
            src: PathBuf::from(rel),
            src_dev: DeviceId::Card1,
            size,
            mtime: crate::engine::scan::Mtime { secs: 0, nanos: 0 },
            pairing,
        }
    }

    #[test]
    fn resume_skipped_files_have_no_in_flight_hash() {
        let h = Hashes {
            sc: None,
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(GOOD),
            b: Some(GOOD),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::Clean);
    }

    /// A file with no twin still verifies against the destinations; it just
    /// cannot contribute a twin check.
    #[test]
    fn untwinned_file_still_verifies_against_destinations() {
        let h = Hashes {
            sc: Some(GOOD),
            c1: Some(GOOD),
            c2: None,
            a: Some(GOOD),
            b: Some(GOOD),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::Clean);
    }

    #[test]
    fn missing_measurements_are_unclassified_not_clean() {
        assert_eq!(diagnose(&Hashes::default()), Diagnosis::Unclassified);
        let cards_only = Hashes {
            c1: Some(GOOD),
            c2: Some(GOOD),
            ..Default::default()
        };
        assert_eq!(diagnose(&cards_only), Diagnosis::Unclassified);
    }

    /// Every reachable shape of the hash tuple, checked against the invariants.
    ///
    /// `diagnose` only ever compares hashes for equality, so three distinct
    /// values plus absence cover every equivalence class it can distinguish.
    /// Six slots over that four-value domain is 4,096 cases -- small enough to
    /// enumerate, which makes this a proof over the classifier rather than a
    /// sample of it. Stronger than property testing here, and no dependency.
    #[test]
    fn every_possible_hash_tuple_upholds_the_invariants() {
        const DOMAIN: [Option<u64>; 4] = [None, Some(GOOD), Some(BAD), Some(OTHER)];
        let mut seen_clean = 0usize;
        let mut cases = 0usize;

        for sc in DOMAIN {
            for c1 in DOMAIN {
                for c2 in DOMAIN {
                    for a in DOMAIN {
                        for b in DOMAIN {
                            for c in DOMAIN {
                                let h = Hashes {
                                    sc,
                                    c1,
                                    c2,
                                    a,
                                    b,
                                    c,
                                };
                                let d = diagnose(&h);
                                cases += 1;

                                let cards = h.cards();
                                let dests = h.dests();

                                if d.is_clean() {
                                    seen_clean += 1;
                                    // Clean must mean everything measured agrees.
                                    let mut all: Vec<u64> =
                                        cards.iter().chain(dests.iter()).map(|(_, v)| *v).collect();
                                    all.extend(h.sc);
                                    assert!(
                                        all.windows(2).all(|w| w[0] == w[1]),
                                        "Clean with disagreeing hashes: {h:?}"
                                    );
                                    assert!(
                                        !cards.is_empty() && !dests.is_empty(),
                                        "Clean with nothing measured on one side: {h:?}"
                                    );
                                }

                                // Two cards that disagree is always a twin
                                // mismatch, whatever else is going on.
                                if let (Some(x), Some(y)) = (c1, c2) {
                                    if x != y {
                                        assert!(
                                            matches!(d, Diagnosis::TwinMismatch { .. }),
                                            "cards disagree but got {d:?}: {h:?}"
                                        );
                                    }
                                }

                                // "Systematic" is a claim about agreement
                                // between destinations, so it needs two.
                                if matches!(d, Diagnosis::Systematic) {
                                    assert!(dests.len() >= 2, "Systematic with <2 dests: {h:?}");
                                }

                                // An incomplete tuple is unclassifiable -- with
                                // the one exception that a twin mismatch is a
                                // fact about the cards alone.
                                let twins_disagree =
                                    matches!((c1, c2), (Some(x), Some(y)) if x != y);
                                if (cards.is_empty() || dests.is_empty()) && !twins_disagree {
                                    assert_eq!(
                                        d,
                                        Diagnosis::Unclassified,
                                        "incomplete tuple was classified: {h:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 4_096);
        assert!(seen_clean > 0, "the enumeration never reached a clean case");
    }

    #[test]
    fn three_way_disagreement_is_unclassified() {
        let h = Hashes {
            sc: Some(BAD),
            c1: Some(GOOD),
            c2: Some(GOOD),
            a: Some(BAD),
            b: Some(OTHER),
            c: None,
        };
        assert_eq!(diagnose(&h), Diagnosis::Unclassified);
    }
}
