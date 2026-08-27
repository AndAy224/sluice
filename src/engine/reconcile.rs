//! Twin-card file-list comparison.
//!
//! With the camera in simultaneous-recording mode, every file was written twice,
//! to two pieces of NAND behind two separate controllers. The two file lists
//! should therefore be identical, and divergence is information rather than an
//! error to paper over: a card that filled mid-session legitimately produces a
//! shorter list.
//!
//! The rule is: copy the **union** of both cards, verify whatever can be
//! verified, and let any file without a twin suppress the format verdict for the
//! whole session. Divergence must never silently reduce the guarantee.
//!
//! This module is a pure function over two [`Scan`]s. It touches no disk, which
//! is what makes the divergence cases cheap to test.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;

use super::scan::{Mtime, Scan};
use super::DeviceId;

/// How one relative path appears across the card pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum Pairing {
    /// On both cards at the same size. The normal case, and the only one that
    /// can be cross-checked against a twin.
    Twinned,
    /// On card 1 only -- the camera wrote one slot, because card 2 filled or
    /// errored. Copied, but unverifiable against a twin.
    OnlyOnC1,
    /// On card 2 only.
    OnlyOnC2,
    /// On both cards at different sizes. Two different files cannot share one
    /// destination path, so neither is copied and the file is named as failed.
    SizeConflict { c1: u64, c2: u64 },
}

impl Pairing {
    pub fn is_twinned(self) -> bool {
        matches!(self, Self::Twinned)
    }
}

/// One file to copy, with the card that supplies its bytes already chosen.
///
/// The source is per-file rather than "card 1", because a file present only on
/// card 2 still has to be copied and card 1 cannot supply it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyItem {
    pub rel: String,
    /// Card 1 whenever it holds the file, so the common case stays a single
    /// sequential read of one card.
    pub src_dev: DeviceId,
    pub src: PathBuf,
    pub size: u64,
    pub mtime: Mtime,
    pub pairing: Pairing,
}

/// A path that exists on both cards at two different sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub rel: String,
    pub c1_size: u64,
    pub c2_size: u64,
}

/// What the camera was actually doing with two cards.
///
/// rev 2 is built on one of these -- backup mode, where the camera writes both
/// slots simultaneously and the two cards are twins. The others are just as
/// common in the wild and they are *not* twins, so nearly every file comes out
/// untwinned and the verdict correctly refuses to authorise an erase.
///
/// Correctly, but bafflingly: a wall of "no twin on card 2" for 1,613 files
/// looks like a broken program rather than a camera set to relay. Naming the
/// mode turns a refusal into an explanation, and tells the user the one thing
/// that would change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CardMode {
    /// Both slots hold the same files. This is what rev 2 assumes.
    Backup,
    /// The two cards hold different files of the same kinds -- the camera filled
    /// one card and moved to the next.
    Relay,
    /// The two cards hold different *kinds* of file: RAW to one slot and JPEG or
    /// proxy to the other.
    SplitByType,
    /// Substantial overlap, but not complete. A card swapped mid-shoot, or a
    /// backup run that was interrupted.
    Mixed,
}

impl CardMode {
    /// Whether this mode can ever produce a twin-verified session.
    pub fn is_twinned(self) -> bool {
        matches!(self, Self::Backup)
    }

    /// One line for the log, saying what was seen and what it means.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Backup => {
                "both cards hold the same files -- backup mode, which is what \
                             twin verification needs"
            }
            Self::Relay => {
                "the cards hold different files of the same kinds, which is what \
                            relay mode looks like: the camera filled one card and moved to \
                            the next. Nothing here has a twin, so this cannot end in SAFE TO \
                            FORMAT. Set the camera to record to both slots simultaneously."
            }
            Self::SplitByType => {
                "the cards hold different kinds of file, which is what a \
                                  RAW-to-one-slot, JPEG-to-the-other setting looks like. \
                                  Nothing here has a twin, so this cannot end in SAFE TO \
                                  FORMAT. Set the camera to record the same thing to both \
                                  slots."
            }
            Self::Mixed => {
                "the cards overlap but do not match, which usually means one was \
                            swapped mid-shoot or a previous copy was interrupted. Only the \
                            overlapping files can be twin-verified."
            }
        }
    }
}

/// The outcome of comparing two card file lists.
#[derive(Debug, Clone, Serialize)]
pub struct Reconciliation {
    /// The union, in deterministic order, minus anything in `conflicts`.
    pub items: Vec<CopyItem>,
    pub only_c1: Vec<String>,
    pub only_c2: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// Files present on both cards at the same size.
    pub twinned: usize,
    /// Bytes to be copied, i.e. the sum over `items`.
    pub total_bytes: u64,
    /// False when card 2 was not mounted at all.
    pub had_card2: bool,
    /// What the camera appears to have been doing. `None` without a card 2.
    pub mode: Option<CardMode>,
}

impl Reconciliation {
    /// Whether every file in this session has a twin to be checked against.
    ///
    /// This is the gate on SAFE TO FORMAT. It is deliberately stricter than "the
    /// copy succeeded": a session can copy perfectly and still be unsafe to
    /// erase, because an untwinned file has only ever been read from one card.
    pub fn twins_complete(&self) -> bool {
        self.had_card2
            && self.only_c1.is_empty()
            && self.only_c2.is_empty()
            && self.conflicts.is_empty()
    }

    /// Files that will be copied but cannot be twin-verified.
    pub fn untwinned(&self) -> usize {
        self.only_c1.len() + self.only_c2.len()
    }

    /// One log line, in the shape the design's §10 sample uses.
    pub fn summary(&self) -> String {
        if !self.had_card2 {
            return format!(
                "card 2 absent, {} files from card 1 only -- no twin verification possible",
                self.items.len()
            );
        }
        if self.twins_complete() {
            return format!("file lists identical, {} matched", self.twinned);
        }
        let mut parts = vec![format!("{} matched", self.twinned)];
        if !self.only_c1.is_empty() {
            parts.push(format!("{} on card 1 only", self.only_c1.len()));
        }
        if !self.only_c2.is_empty() {
            parts.push(format!("{} on card 2 only", self.only_c2.len()));
        }
        if !self.conflicts.is_empty() {
            parts.push(format!("{} size conflicts", self.conflicts.len()));
        }
        format!("file lists diverge: {}", parts.join(", "))
    }
}

/// Compare two card scans and produce the copy work list.
///
/// `c2` is `None` when only one card was mounted, which is a legitimate way to
/// run the tool -- it just cannot end in SAFE TO FORMAT.
pub fn reconcile(c1: &Scan, c2: Option<&Scan>) -> Reconciliation {
    let mut rels: BTreeSet<&str> = c1.entries.keys().map(String::as_str).collect();
    if let Some(c2) = c2 {
        rels.extend(c2.entries.keys().map(String::as_str));
    }

    let mut items = Vec::with_capacity(rels.len());
    let mut only_c1 = Vec::new();
    let mut only_c2 = Vec::new();
    let mut conflicts = Vec::new();
    let mut twinned = 0usize;
    let mut total_bytes = 0u64;

    for rel in rels {
        let a = c1.get(rel);
        let b = c2.and_then(|s| s.get(rel));

        let item = match (a, b) {
            (Some(a), Some(b)) if a.size == b.size => {
                twinned += 1;
                CopyItem {
                    rel: rel.to_string(),
                    src_dev: DeviceId::Card1,
                    src: c1.absolute(rel),
                    size: a.size,
                    mtime: a.mtime,
                    pairing: Pairing::Twinned,
                }
            }
            (Some(a), Some(b)) => {
                // Same path, two different files. Copying either would pick a
                // winner silently, so copy neither and name it.
                conflicts.push(Conflict {
                    rel: rel.to_string(),
                    c1_size: a.size,
                    c2_size: b.size,
                });
                continue;
            }
            (Some(a), None) => {
                only_c1.push(rel.to_string());
                CopyItem {
                    rel: rel.to_string(),
                    src_dev: DeviceId::Card1,
                    src: c1.absolute(rel),
                    size: a.size,
                    mtime: a.mtime,
                    pairing: Pairing::OnlyOnC1,
                }
            }
            (None, Some(b)) => {
                only_c2.push(rel.to_string());
                let c2 = c2.expect("b came from c2");
                CopyItem {
                    rel: rel.to_string(),
                    // Card 1 cannot supply this file, so the reader has to be
                    // told, per item, where the bytes come from.
                    src_dev: DeviceId::Card2,
                    src: c2.absolute(rel),
                    size: b.size,
                    mtime: b.mtime,
                    pairing: Pairing::OnlyOnC2,
                }
            }
            (None, None) => unreachable!("rel came from one of the two scans"),
        };
        total_bytes += item.size;
        items.push(item);
    }

    let mode = c2
        .is_some()
        .then(|| detect_mode(twinned, &only_c1, &only_c2, &conflicts));

    Reconciliation {
        items,
        only_c1,
        only_c2,
        conflicts,
        twinned,
        total_bytes,
        had_card2: c2.is_some(),
        mode,
    }
}

/// Work out what the camera was doing, from the shape of the overlap.
///
/// Deliberately a shape argument rather than a camera-model lookup: there is no
/// list of every camera and every menu setting, but "the two cards share almost
/// nothing and hold different file types" is the same fact whoever made the
/// body.
fn detect_mode(
    twinned: usize,
    only_c1: &[String],
    only_c2: &[String],
    conflicts: &[Conflict],
) -> CardMode {
    let total = twinned + only_c1.len() + only_c2.len() + conflicts.len();
    if total == 0 {
        // Two empty cards. Nothing was proven either way, and calling that
        // anything but backup mode would invent a problem.
        return CardMode::Backup;
    }

    // A handful of stragglers is a backup that caught the last frame on one card
    // only, not a different mode. Anything more is a real divergence.
    if twinned * 50 >= total * 49 {
        return CardMode::Backup;
    }
    if twinned > 0 {
        return CardMode::Mixed;
    }
    if only_c1.is_empty() || only_c2.is_empty() {
        // One card empty, or all the divergence on one side: not enough to call
        // it a mode.
        return CardMode::Mixed;
    }

    // Nothing in common. Whether that is relay or a type split comes down to
    // whether the two cards hold the same *kinds* of file.
    let e1 = extensions(only_c1);
    let e2 = extensions(only_c2);
    if e1.is_disjoint(&e2) {
        CardMode::SplitByType
    } else {
        CardMode::Relay
    }
}

/// Lower-case extensions present in a set of relative paths.
fn extensions(rels: &[String]) -> BTreeSet<String> {
    rels.iter()
        .filter_map(|r| r.rsplit_once('.').map(|(_, e)| e.to_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::Entry;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn scan_of(root: &str, files: &[(&str, u64)]) -> Scan {
        let mut entries = BTreeMap::new();
        let mut total = 0;
        for (rel, size) in files {
            total += *size;
            entries.insert(
                rel.to_string(),
                Entry {
                    rel: rel.to_string(),
                    size: *size,
                    mtime: Mtime {
                        secs: 1_757_629_443,
                        nanos: 0,
                    },
                },
            );
        }
        Scan {
            root: PathBuf::from(root),
            entries,
            total_bytes: total,
            skipped: Vec::new(),
            errors: Vec::new(),
            placeholders: Vec::new(),
        }
    }

    const A: &str = "DCIM/100MSDCF/DSC00001.ARW";
    const B: &str = "DCIM/100MSDCF/DSC00002.ARW";
    const C: &str = "DCIM/100MSDCF/DSC00003.ARW";

    #[test]
    fn identical_lists_are_fully_twinned() {
        let c1 = scan_of("E:\\", &[(A, 100), (B, 200)]);
        let c2 = scan_of("F:\\", &[(A, 100), (B, 200)]);
        let r = reconcile(&c1, Some(&c2));

        assert_eq!(r.items.len(), 2);
        assert_eq!(r.twinned, 2);
        assert_eq!(r.total_bytes, 300);
        assert!(r.twins_complete());
        assert!(r.items.iter().all(|i| i.src_dev == DeviceId::Card1));
        assert_eq!(r.summary(), "file lists identical, 2 matched");
    }

    /// Test 5: a file missing from card 2 must still be copied, and must drop
    /// the session out of SAFE TO FORMAT.
    #[test]
    fn file_only_on_card1_is_copied_but_suppresses_the_format_verdict() {
        let c1 = scan_of("E:\\", &[(A, 100), (B, 200)]);
        let c2 = scan_of("F:\\", &[(A, 100)]);
        let r = reconcile(&c1, Some(&c2));

        assert_eq!(
            r.items.len(),
            2,
            "the union is copied, not the intersection"
        );
        assert_eq!(r.only_c1, vec![B]);
        assert_eq!(r.twinned, 1);
        assert!(!r.twins_complete());
        assert_eq!(r.untwinned(), 1);
    }

    /// The case the design's single-source reader could not have handled: card 1
    /// cannot supply a file it does not have.
    #[test]
    fn file_only_on_card2_is_sourced_from_card2() {
        let c1 = scan_of("E:\\", &[(A, 100)]);
        let c2 = scan_of("F:\\", &[(A, 100), (C, 300)]);
        let r = reconcile(&c1, Some(&c2));

        assert_eq!(r.items.len(), 2);
        let from_c2: Vec<&CopyItem> = r
            .items
            .iter()
            .filter(|i| i.src_dev == DeviceId::Card2)
            .collect();
        assert_eq!(from_c2.len(), 1);
        assert_eq!(from_c2[0].rel, C);
        assert_eq!(
            from_c2[0].src,
            Path::new("F:\\").join("DCIM/100MSDCF/DSC00003.ARW".replace('/', "\\"))
        );
        assert_eq!(r.only_c2, vec![C]);
        assert!(!r.twins_complete());
    }

    #[test]
    fn size_conflict_copies_neither_and_names_the_file() {
        let c1 = scan_of("E:\\", &[(A, 100)]);
        let c2 = scan_of("F:\\", &[(A, 999)]);
        let r = reconcile(&c1, Some(&c2));

        assert!(
            r.items.is_empty(),
            "a conflicted path must not pick a winner"
        );
        assert_eq!(
            r.conflicts,
            vec![Conflict {
                rel: A.into(),
                c1_size: 100,
                c2_size: 999
            }]
        );
        assert!(!r.twins_complete());
    }

    /// §5: "card 2 was absent" is a VERIFIED -- DO NOT FORMAT case, not a failure.
    #[test]
    fn absent_card2_copies_everything_but_is_never_twin_complete() {
        let c1 = scan_of("E:\\", &[(A, 100), (B, 200)]);
        let r = reconcile(&c1, None);

        assert_eq!(r.items.len(), 2);
        assert_eq!(r.twinned, 0);
        assert!(!r.had_card2);
        assert!(!r.twins_complete());
        assert!(r.summary().contains("card 2 absent"));
    }

    #[test]
    fn empty_cards_reconcile_cleanly() {
        let c1 = scan_of("E:\\", &[]);
        let c2 = scan_of("F:\\", &[]);
        let r = reconcile(&c1, Some(&c2));
        assert!(r.items.is_empty());
        assert!(
            r.twins_complete(),
            "nothing to copy is trivially consistent"
        );
    }

    // --- camera mode -------------------------------------------------------

    #[test]
    fn identical_cards_read_as_backup_mode() {
        let a = scan_of("E:\\", &[(A, 10), (B, 20), (C, 30)]);
        let b = scan_of("F:\\", &[(A, 10), (B, 20), (C, 30)]);
        let r = reconcile(&a, Some(&b));
        assert_eq!(r.mode, Some(CardMode::Backup));
        assert!(r.mode.unwrap().is_twinned());
    }

    /// The camera filled card 1 and moved to card 2. Same file types, no
    /// overlap: nothing has a twin, and the user needs to be told the setting
    /// rather than shown 1,613 warnings.
    #[test]
    fn disjoint_cards_of_the_same_type_read_as_relay() {
        let a = scan_of("E:\\", &[("DCIM/100MSDCF/DSC00001.ARW", 10)]);
        let b = scan_of("F:\\", &[("DCIM/100MSDCF/DSC00500.ARW", 10)]);
        let r = reconcile(&a, Some(&b));
        assert_eq!(r.mode, Some(CardMode::Relay));
        assert!(!r.mode.unwrap().is_twinned());
        assert!(r.mode.unwrap().describe().contains("relay mode"));
    }

    /// RAW to one slot, JPEG to the other.
    #[test]
    fn disjoint_cards_of_different_types_read_as_a_type_split() {
        let a = scan_of("E:\\", &[("DCIM/100MSDCF/DSC00001.ARW", 10)]);
        let b = scan_of("F:\\", &[("DCIM/100MSDCF/DSC00001.JPG", 4)]);
        let r = reconcile(&a, Some(&b));
        assert_eq!(r.mode, Some(CardMode::SplitByType));
        assert!(r.mode.unwrap().describe().contains("kinds of file"));
    }

    /// One frame that landed on a single card is a backup run, not a different
    /// mode. If this drifted, every real night would be reported as Mixed.
    #[test]
    fn a_single_straggler_is_still_backup_mode() {
        let mut pairs: Vec<(String, u64)> = (0..200)
            .map(|i| (format!("DCIM/100MSDCF/DSC{i:05}.ARW"), 10))
            .collect();
        let a = scan_of(
            "E:\\",
            &pairs
                .iter()
                .map(|(r, s)| (r.as_str(), *s))
                .collect::<Vec<_>>(),
        );
        pairs.pop();
        let b = scan_of(
            "F:\\",
            &pairs
                .iter()
                .map(|(r, s)| (r.as_str(), *s))
                .collect::<Vec<_>>(),
        );
        let r = reconcile(&a, Some(&b));
        assert_eq!(r.mode, Some(CardMode::Backup), "199 of 200 is a backup");
    }

    #[test]
    fn heavy_divergence_with_some_overlap_reads_as_mixed() {
        let a = scan_of("E:\\", &[(A, 10), ("DCIM/X1.ARW", 1), ("DCIM/X2.ARW", 1)]);
        let b = scan_of("F:\\", &[(A, 10), ("DCIM/Y1.ARW", 1), ("DCIM/Y2.ARW", 1)]);
        let r = reconcile(&a, Some(&b));
        assert_eq!(r.mode, Some(CardMode::Mixed));
    }

    #[test]
    fn one_card_has_no_mode_to_detect() {
        let a = scan_of("E:\\", &[(A, 10)]);
        assert_eq!(reconcile(&a, None).mode, None);
    }
}
