//! The format-safety state machine.
//!
//! Two claims that sound alike are not:
//!
//! * "everything copied correctly"
//! * "it is safe to erase the only other copies"
//!
//! The second is strictly stronger, and it is the one that authorises an
//! irreversible action. So there are three states, not two, and the middle one
//! exists specifically to say *the copy is good and you still must not format*.
//!
//! Every rule here fails toward not-formatting. An unproven claim is treated
//! exactly like a disproven one.

use std::collections::BTreeMap;

use serde::Serialize;

use super::reconcile::CardMode;
use super::verify::Diagnosis;
use super::win::Distinctness;
use super::DeviceId;

/// What this run proved.
///
/// §5 specifies three states. Three is right for the hardware rev 2 was designed
/// around -- a dual-slot camera and two LaCies -- and wrong for everybody else,
/// because it collapses two very different things into one banner:
///
/// * "you did everything your hardware allows, and it all checks out"
/// * "something here is wrong"
///
/// A photographer with one card gets the second banner every single night, for a
/// reason that will never change no matter what they do. A signal that always
/// fires is one people learn to click past, and then it is not there on the
/// night it means something.
///
/// So the middle state is split by *what was missing*. The safety property is
/// untouched: [`Verdict::authorises_erase`] is true for exactly one variant, and
/// the two new ones are a more honest way of saying no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// Every file matched across both cards and both destinations, manifests are
    /// written, and the destinations are proven to be different physical drives.
    SafeToFormat,
    /// Everything the hardware allows was proven, but only one card supplied the
    /// bytes -- so a card that returns the same wrong bytes on both reads cannot
    /// be ruled out.
    VerifiedOneSource,
    /// Everything verified, but the files exist in one place. Losing that drive
    /// loses the work.
    VerifiedOneCopy,
    /// The copy is good and something is actually wrong: a drive dropping
    /// writes, two destinations on one disk, identity that could not be proven.
    VerifiedDoNotFormat,
    /// A mismatch somewhere, with the specific diagnosis attached.
    Failed,
}

impl Verdict {
    /// Every state, so a test can enumerate them and a new one cannot be added
    /// without the exhaustive checks noticing.
    pub const ALL: [Verdict; 5] = [
        Self::SafeToFormat,
        Self::VerifiedOneSource,
        Self::VerifiedOneCopy,
        Self::VerifiedDoNotFormat,
        Self::Failed,
    ];

    /// The exact phrase on the banner. A tired person in a dim room reads this
    /// and nothing else, so it never varies.
    pub fn headline(self) -> &'static str {
        match self {
            Self::SafeToFormat => "SAFE TO FORMAT",
            Self::VerifiedOneSource => "VERIFIED — ONE SOURCE",
            Self::VerifiedOneCopy => "VERIFIED — ONE COPY",
            Self::VerifiedDoNotFormat => "VERIFIED — DO NOT FORMAT",
            Self::Failed => "FAILED",
        }
    }

    /// The one question this program exists to answer.
    ///
    /// Exactly one variant says yes. Every tier added for legibility is still a
    /// no, and this is the single place that decides.
    pub fn authorises_erase(self) -> bool {
        matches!(self, Self::SafeToFormat)
    }

    /// Whether every file was proven to have arrived intact.
    ///
    /// True for all four non-`Failed` states: the tiers below `SafeToFormat`
    /// differ in what could not be *ruled out*, not in whether the copy is good.
    pub fn copy_is_good(self) -> bool {
        !matches!(self, Self::Failed)
    }

    /// Whether this state means something is wrong, as opposed to something
    /// being structurally unprovable with the hardware present.
    ///
    /// The banner uses this to choose a colour: a run that did everything it
    /// could must not look like a run with a failing drive in it.
    pub fn something_is_wrong(self) -> bool {
        matches!(self, Self::Failed | Self::VerifiedDoNotFormat)
    }

    /// The process exit code for this verdict.
    ///
    /// A five-tier verdict that collapses to success/failure is one no wrapper
    /// can act on -- and the failure was not merely inconvenient. Until this
    /// existed only `Failed` was non-zero, so `sluice run ... && erase-card`
    /// succeeded on VERIFIED -- DO NOT FORMAT. The entire output of this program
    /// is a permission decision; the exit code has to carry it.
    ///
    /// `0` means, and only ever means, that erasing the originals is authorised.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::SafeToFormat => 0,
            Self::VerifiedOneSource => 10,
            Self::VerifiedOneCopy => 11,
            Self::VerifiedDoNotFormat => 12,
            Self::Failed => 20,
        }
    }

    /// The line under the headline: what to do about it.
    pub fn guidance(self) -> &'static str {
        match self {
            Self::SafeToFormat => {
                "Both cards and both drives agree. Spot-check a few frames, sleep on it, \
                 then format in the camera."
            }
            Self::VerifiedOneSource => {
                "Every file is on both drives and matches two independent reads. Only a \
                 second card written at the same time could rule out a card that misreads \
                 the same way twice, so keep the originals until the files are somewhere \
                 else as well."
            }
            Self::VerifiedOneCopy => {
                "Every file verified, but it exists in one place. Add a second destination \
                 drive before erasing anything."
            }
            Self::VerifiedDoNotFormat => {
                "The copy is good, but something below needs attention before these cards \
                 are the only thing you trust."
            }
            Self::Failed => "Do not erase anything. The detail below says what disagreed.",
        }
    }
}

/// A file that did not verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileFailure {
    pub rel: String,
    pub diagnosis: Diagnosis,
}

/// Everything the state machine needs. Assembled by the orchestrator once the
/// verify phase has finished.
#[derive(Debug, Clone, Default)]
pub struct Assessment {
    pub files_total: usize,
    pub bytes_total: u64,
    pub failures: Vec<FileFailure>,
    /// Files copied but with no twin to check against.
    pub untwinned: Vec<String>,
    /// Paths that exist on both cards at different sizes.
    pub conflicts: Vec<String>,
    pub had_card2: bool,
    pub distinctness: Option<Distinctness>,
    /// Whether the two cards are on different physical devices. `None` when
    /// there is no card 2 to compare against.
    pub cards_distinct: Option<Distinctness>,
    /// Manifests landed on every destination. Their presence is the success
    /// signal, so a run that cannot write them is not a successful run.
    pub manifests_written: bool,
    /// Set when the job stopped early -- a systematic fault, or cancellation.
    pub aborted: Option<String>,
    /// Files per destination that failed once and succeeded on the retry.
    pub retries: BTreeMap<DeviceId, usize>,
    /// How many destinations this session wrote to. One copy is not two, however
    /// well it verified.
    pub dest_count: usize,
    /// Destinations whose bytes could not be read back off a device -- network
    /// shares, where `FILE_FLAG_NO_BUFFERING` is advisory and a read can be
    /// served from a cache on either end.
    pub unverifiable_dests: Vec<DeviceId>,
    /// What the camera appears to have been doing with two cards, so an
    /// untwinned session can explain itself rather than just refusing.
    pub mode: Option<CardMode>,
    /// Entries the card walk could not read at all.
    ///
    /// `scan.rs` already says "a card that throws errors during a metadata walk
    /// is not a card to format", and until now nothing acted on it: a `WalkDir`
    /// error takes its whole subtree with it, so those files are not untwinned,
    /// not missing, not anything -- they were never seen. The verdict has to
    /// know, or a dying card gets a green banner with ERR lines scrolled off
    /// above it.
    pub scan_errors: usize,
    /// Card/destination pairs found to be on one physical device.
    ///
    /// Detected in preflight and, until now, only warned about. A destination on
    /// a card's own disk means half the redundancy lives on the thing you are
    /// being told to erase.
    pub dest_on_card_device: Vec<String>,
}

/// Retries on one drive above which a glitch stops being a plausible
/// explanation.
///
/// One or two recovered files across a night is a USB hiccup; the data is on the
/// drive and verify proved it, so refusing the format would be superstition. Six
/// is a pattern, and a drive that drops writes repeatedly tonight is not one to
/// trust with the only copy of a day's work.
const RETRY_GLITCH_CEILING: usize = 5;

/// A verdict together with the reasoning that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerdictReport {
    pub state: Verdict,
    /// Why. Never empty for the two non-clean states -- the design forbids a
    /// bare "verification failed".
    pub reasons: Vec<String>,
    pub files_total: usize,
    pub files_failed: usize,
    pub bytes_total: u64,
    pub twin_matched: bool,
    pub distinct_volumes: bool,
}

impl VerdictReport {
    /// A run that never got far enough to be assessed.
    ///
    /// Preflight refuses several things outright -- a filename Windows cannot
    /// store faithfully, the same card in both slots, a destination without room
    /// for what is left to write. None of those reach the verdict phase, so
    /// without this they produce no verdict at all and the banner keeps showing
    /// the phase it stopped in. A refusal is a `Failed` run in every sense that
    /// matters: nothing verified, and nothing may be erased.
    pub fn refused(reasons: Vec<String>) -> Self {
        Self {
            state: Verdict::Failed,
            reasons,
            files_total: 0,
            files_failed: 0,
            bytes_total: 0,
            twin_matched: false,
            distinct_volumes: false,
        }
    }

    pub fn headline(&self) -> &'static str {
        self.state.headline()
    }

    /// The banner's second line: what was actually established.
    pub fn detail(&self) -> String {
        let mut parts = vec![
            format!("{} files", thousands(self.files_total as u64)),
            format!("{:.1} GB", self.bytes_total as f64 / 1e9),
        ];
        if self.twin_matched {
            parts.push("twin-matched".into());
        }
        if self.distinct_volumes {
            parts.push("2 distinct volumes".into());
        }
        if self.files_failed > 0 {
            parts.push(format!("{} FAILED", self.files_failed));
        }
        parts.join(" · ")
    }
}

/// Decide the verdict.
///
/// Ordered most-severe first: a run cannot be "unproven" if it is also broken.
pub fn assess(a: &Assessment) -> VerdictReport {
    let mut reasons = Vec::new();
    let twin_matched = a.had_card2 && a.untwinned.is_empty() && a.conflicts.is_empty();
    let distinct_volumes = matches!(a.distinctness, Some(Distinctness::Distinct));

    // --- Failed -----------------------------------------------------------
    if let Some(why) = &a.aborted {
        reasons.push(format!("job aborted: {why}"));
    }
    for f in &a.failures {
        reasons.push(format!("{}: {}", f.rel, f.diagnosis.describe()));
    }
    for rel in &a.conflicts {
        reasons.push(format!(
            "{rel}: present on both cards at different sizes, copied from neither"
        ));
    }
    if !reasons.is_empty() {
        return VerdictReport {
            state: Verdict::Failed,
            reasons,
            files_total: a.files_total,
            files_failed: a.failures.len() + a.conflicts.len(),
            bytes_total: a.bytes_total,
            twin_matched,
            distinct_volumes,
        };
    }
    if !a.manifests_written {
        return VerdictReport {
            state: Verdict::Failed,
            // Manifest presence is the success signal. A verified copy nobody can
            // re-verify later is not a finished job.
            reasons: vec![
                "verification passed but the manifests could not be written, so this copy \
                 cannot be re-verified later"
                    .into(),
            ],
            files_total: a.files_total,
            files_failed: 0,
            bytes_total: a.bytes_total,
            twin_matched,
            distinct_volumes,
        };
    }

    // --- Something is actually wrong --------------------------------------
    //
    // Faults: hardware misbehaving, or an arrangement that defeats the point of
    // copying twice. Kept apart from the structural gaps below because a user
    // can *do* something about every one of them tonight.
    // Did this run actually check anything?
    //
    // `twin_matched` is vacuously true over an empty set, a manifest with no
    // entries writes without complaint, and `assess` then falls straight through
    // to the clean arm -- so two unreadable cards produced "SAFE TO FORMAT ·
    // 0 files · twin-matched · 2 distinct volumes". The path-equality half of
    // this guard was written in `run_job`; this is the half that was missing,
    // and it lives here so no caller can route around it.
    if a.files_total == 0 {
        reasons.push(
            "no files were found on the cards, so nothing was verified -- an empty result is \
             not a clean one, and a card that reads as empty is a card to investigate rather \
             than erase"
                .into(),
        );
    }
    for rel in &a.dest_on_card_device {
        reasons.push(format!(
            "{rel} -- a destination is on the same physical device as a card, so that copy \
             would not survive the card failing, and erasing the card takes it too"
        ));
    }
    if a.scan_errors > 0 {
        reasons.push(format!(
            "{} entr(y/ies) on the cards could not be read during the scan. Anything under an \
             unreadable directory was never seen at all, so it is neither copied nor missed -- \
             the file list this run verified may not be the whole card",
            a.scan_errors
        ));
    }
    for (dev, n) in &a.retries {
        if *n > RETRY_GLITCH_CEILING {
            reasons.push(format!(
                "drive {} dropped {n} writes that only succeeded on a retry — the bytes \
                 verified, but that many is a failing drive rather than a glitch",
                dev.label()
            ));
        }
    }
    if a.cards_distinct == Some(Distinctness::SameDevice) {
        reasons.push(
            "both cards are on one physical device, so nothing was checked against a real \
             twin -- one device failing would take both copies. Use two readers, on two \
             ports"
                .into(),
        );
    }
    // A destination that cannot supply evidence must not be able to *veto* the
    // verdict either. Two local drives plus a NAS is the standard team setup and
    // the window invites the third slot, so treating the NAS as a fault made
    // DO NOT FORMAT fire every single night on a night when nothing was wrong --
    // the exact alert fatigue the tier split exists to prevent.
    //
    // It is a fault only when it is load-bearing: when discounting it would
    // leave fewer than two destinations that can actually be checked.
    let verifiable_dests = a.dest_count.saturating_sub(a.unverifiable_dests.len());
    let mut unverifiable_notes: Vec<String> = Vec::new();
    for dev in &a.unverifiable_dests {
        let note = format!(
            "{} is a network location, where an unbuffered read is advisory: the bytes \
             checked may have come from a cache at either end rather than off the disk, so \
             it was copied and hashed but contributes no evidence about the cards",
            dev.label()
        );
        if verifiable_dests >= 2 && distinct_volumes {
            unverifiable_notes.push(note);
        } else {
            reasons.push(note);
        }
    }
    if a.dest_count >= 2 {
        match &a.distinctness {
            Some(Distinctness::Distinct) => {}
            Some(Distinctness::SameDevice) => reasons.push(
                "both destinations are on the same physical drive, so there is only one copy"
                    .into(),
            ),
            Some(Distinctness::Unproven(why)) => reasons.push(format!(
                "could not prove the destinations are different physical drives: {why}"
            )),
            None => reasons.push("destination device identity was never captured".into()),
        }
    }

    // --- Verified as far as the hardware present allows --------------------
    //
    // Nothing here is broken. These are facts about what was not present to
    // check against, and saying them in the same words as a failing drive is
    // what teaches people to stop reading the banner.
    //
    // They are gathered separately from the faults above but always *reported*:
    // a fault decides the tier, it does not get to hide the fact that eleven
    // files had no twin.
    let mut gaps: Vec<String> = unverifiable_notes;
    if a.dest_count < 2 {
        gaps.push(format!(
            "{} files verified byte for byte against the card, and written to one \
             destination. One drive is one failure away from nothing",
            thousands(a.files_total as u64)
        ));
    }
    if !twin_matched {
        gaps.push(match (a.had_card2, a.untwinned.len()) {
            (false, _) => format!(
                "card 2 was not present. {} files verified against two independent unbuffered \
                 reads of one card, which rules out a bad transfer and a bad destination -- \
                 but not a card that returns the same wrong bytes on every read",
                thousands(a.files_total as u64)
            ),
            (true, n) => format!(
                "{n} of {} files had no twin on card 2 and were checked against the \
                 destinations only: {}",
                thousands(a.files_total as u64),
                preview(&a.untwinned)
            ),
        });
        // A camera in relay or split mode produces a wall of untwinned files for
        // a reason that lives in a menu. Saying which menu is the difference
        // between an explanation and a mystery.
        if let Some(mode) = a.mode.filter(|m| !m.is_twinned()) {
            gaps.push(mode.describe().to_string());
        }
    }

    if !reasons.is_empty() {
        // A fault outranks every structural tier -- but the gaps go in the
        // report too, or a night with one flaky drive silently stops mentioning
        // that half the files had no twin.
        reasons.extend(gaps);
        return VerdictReport {
            state: Verdict::VerifiedDoNotFormat,
            reasons,
            files_total: a.files_total,
            files_failed: 0,
            bytes_total: a.bytes_total,
            twin_matched,
            distinct_volumes,
        };
    }

    if !gaps.is_empty() {
        // A run whose only remark is "the NAS could not be checked" proved
        // everything the format verdict needs: two verifiable drives, distinct,
        // twin-matched. Saying no there would be the alert fatigue this tier
        // system exists to avoid.
        if twin_matched
            && a.dest_count >= 2
            && a.files_total > 0
            && gaps.len() == a.unverifiable_dests.len()
        {
            let mut reasons = vec![format!(
                "{} files matched across card 1, card 2, and both verifiable destinations; \
                 manifests written; those destinations proven to be different physical drives",
                thousands(a.files_total as u64)
            )];
            reasons.extend(gaps);
            return VerdictReport {
                state: Verdict::SafeToFormat,
                reasons,
                files_total: a.files_total,
                files_failed: 0,
                bytes_total: a.bytes_total,
                twin_matched: true,
                distinct_volumes: true,
            };
        }
        // One copy is checked before one source: a single drive dying loses
        // everything, whereas a single card that read correctly twice has
        // probably read correctly.
        let state = if a.dest_count < 2 {
            Verdict::VerifiedOneCopy
        } else {
            Verdict::VerifiedOneSource
        };
        return VerdictReport {
            state,
            reasons: gaps,
            files_total: a.files_total,
            files_failed: 0,
            bytes_total: a.bytes_total,
            twin_matched,
            distinct_volumes,
        };
    }

    // --- Clean ------------------------------------------------------------
    VerdictReport {
        state: Verdict::SafeToFormat,
        reasons: vec![format!(
            "{} files matched across card 1, card 2, and both destinations; manifests \
             written; destinations proven to be different physical drives",
            thousands(a.files_total as u64)
        )],
        files_total: a.files_total,
        files_failed: 0,
        bytes_total: a.bytes_total,
        twin_matched: true,
        distinct_volumes: true,
    }
}

fn preview(items: &[String]) -> String {
    const MAX: usize = 3;
    if items.len() <= MAX {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..MAX].join(", "),
        items.len() - MAX
    )
}

fn thousands(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DeviceId;

    fn clean() -> Assessment {
        Assessment {
            files_total: 1613,
            bytes_total: 91_400_000_000,
            failures: Vec::new(),
            untwinned: Vec::new(),
            conflicts: Vec::new(),
            had_card2: true,
            distinctness: Some(Distinctness::Distinct),
            manifests_written: true,
            aborted: None,
            retries: BTreeMap::new(),
            cards_distinct: Some(Distinctness::Distinct),
            dest_count: 2,
            unverifiable_dests: Vec::new(),
            mode: Some(CardMode::Backup),
            scan_errors: 0,
            dest_on_card_device: Vec::new(),
        }
    }

    #[test]
    fn clean_run_authorises_a_format() {
        let r = assess(&clean());
        assert_eq!(r.state, Verdict::SafeToFormat);
        assert!(r.state.authorises_erase());
        assert_eq!(r.headline(), "SAFE TO FORMAT");
        assert_eq!(
            r.detail(),
            "1,613 files · 91.4 GB · twin-matched · 2 distinct volumes"
        );
    }

    /// Test 4: a bit flipped on card 2 must fail, name the card, and never
    /// reach a format verdict.
    #[test]
    fn twin_mismatch_fails_and_names_the_card() {
        let mut a = clean();
        a.failures.push(FileFailure {
            rel: "DCIM/100MSDCF/DSC01207.ARW".into(),
            diagnosis: Diagnosis::TwinMismatch {
                suspect: Some(DeviceId::Card2),
            },
        });
        let r = assess(&a);
        assert_eq!(r.state, Verdict::Failed);
        assert!(!r.state.authorises_erase());
        assert_eq!(r.files_failed, 1);
        assert!(r.reasons[0].contains("DSC01207.ARW"));
        assert!(r.reasons[0].contains("CARD 2 IS SUSPECT"));
    }

    /// Test 5: a file with no twin drops the verdict one notch but is not a
    /// failure -- the copy really did succeed.
    #[test]
    fn untwinned_file_drops_to_one_source() {
        let mut a = clean();
        a.untwinned.push("DCIM/100MSDCF/DSC01599.ARW".into());
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedOneSource);
        assert_eq!(r.headline(), "VERIFIED — ONE SOURCE");
        assert!(!r.state.authorises_erase());
        assert_eq!(r.files_failed, 0);
        assert!(r.reasons[0].contains("DSC01599.ARW"));
        assert!(!r.twin_matched);
    }

    /// Test 6: two destinations on one physical drive is a perfect-looking
    /// result with a single copy. It must never say SAFE TO FORMAT.
    #[test]
    fn same_physical_drive_refuses_to_authorise_a_format() {
        let mut a = clean();
        a.distinctness = Some(Distinctness::SameDevice);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(r.reasons.iter().any(|s| s.contains("same physical drive")));
        assert!(!r.distinct_volumes);
    }

    /// An unproven claim is treated exactly like a disproven one.
    #[test]
    fn unproven_distinctness_is_not_good_enough() {
        let mut a = clean();
        a.distinctness = Some(Distinctness::Unproven("no device number".into()));
        assert_eq!(assess(&a).state, Verdict::VerifiedDoNotFormat);

        let mut a = clean();
        a.distinctness = None;
        assert_eq!(assess(&a).state, Verdict::VerifiedDoNotFormat);
    }

    /// One card is the ordinary case for most photographers, and it must not
    /// wear the same banner as a failing drive -- but it must still never
    /// authorise an erase.
    #[test]
    fn absent_card2_verifies_but_never_authorises() {
        let mut a = clean();
        a.had_card2 = false;
        a.mode = None;
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedOneSource);
        assert!(!r.state.authorises_erase());
        assert!(!r.state.something_is_wrong(), "nothing here is broken");
        assert!(r.state.copy_is_good());
        assert!(
            r.reasons[0].contains("two independent unbuffered reads"),
            "the reason must say what *was* proven: {:?}",
            r.reasons
        );
        assert!(
            r.reasons[0].contains("same wrong bytes"),
            "and what was not: {:?}",
            r.reasons
        );
    }

    /// One drive is one failure away from nothing, whatever the cards did. That
    /// outranks having only one source, because a dead drive loses everything
    /// while a card that read correctly twice probably read correctly.
    #[test]
    fn a_single_destination_is_one_copy_even_with_two_cards() {
        let mut a = clean();
        a.dest_count = 1;
        a.distinctness = None;
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedOneCopy);
        assert!(!r.state.authorises_erase());
        assert!(!r.state.something_is_wrong());
        assert!(r.reasons[0].contains("one failure away"));
    }

    #[test]
    fn one_card_and_one_drive_reports_the_worse_of_the_two() {
        let mut a = clean();
        a.dest_count = 1;
        a.distinctness = None;
        a.had_card2 = false;
        assert_eq!(assess(&a).state, Verdict::VerifiedOneCopy);
    }

    /// A real fault outranks every structural tier: a drive dropping writes is
    /// not the same kind of news as not owning a second card.
    #[test]
    fn a_failing_drive_outranks_a_structural_gap() {
        let mut a = clean();
        a.had_card2 = false;
        a.retries.insert(DeviceId::DestA, RETRY_GLITCH_CEILING + 1);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(r.state.something_is_wrong());
    }

    /// A camera set to relay produces a wall of untwinned files for a reason
    /// that lives in a menu. The verdict has to say which menu.
    #[test]
    fn relay_mode_explains_itself_in_the_verdict() {
        let mut a = clean();
        a.untwinned = vec!["DCIM/A.ARW".into(), "DCIM/B.ARW".into()];
        a.mode = Some(CardMode::Relay);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedOneSource);
        assert!(
            r.reasons.iter().any(|s| s.contains("both slots")),
            "must name the fix: {:?}",
            r.reasons
        );
    }

    /// Backup mode is the expected case and must add no commentary, or every
    /// clean night grows a line nobody needs.
    #[test]
    fn backup_mode_adds_no_noise() {
        let mut a = clean();
        a.untwinned.push("DCIM/A.ARW".into());
        a.mode = Some(CardMode::Backup);
        assert_eq!(assess(&a).reasons.len(), 1);
    }

    /// A network destination cannot supply verification evidence, so it is a
    /// fault-tier reason rather than a structural one -- there is something the
    /// user can change tonight.
    #[test]
    fn a_network_destination_blocks_the_format_verdict() {
        let mut a = clean();
        a.unverifiable_dests.push(DeviceId::DestB);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(
            r.reasons.iter().any(|s| s.contains("network location")),
            "{:?}",
            r.reasons
        );
    }

    /// Two local drives plus a NAS is the standard team setup, and the window
    /// invites the third slot. The NAS cannot contribute evidence -- but the two
    /// local drives already proved everything the format verdict needs, so
    /// letting the NAS veto it would fire DO NOT FORMAT every night on a night
    /// when nothing is wrong.
    #[test]
    fn a_third_unverifiable_destination_does_not_veto_a_proven_pair() {
        let mut a = clean();
        a.dest_count = 3;
        a.unverifiable_dests.push(DeviceId::DestC);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::SafeToFormat, "reasons: {:?}", r.reasons);
        assert!(
            r.reasons.iter().any(|s| s.contains("network location")),
            "and it must still be reported: {:?}",
            r.reasons
        );
    }

    /// It is a fault when it is load-bearing: discount the NAS and there are not
    /// two checkable destinations left.
    #[test]
    fn an_unverifiable_destination_still_counts_when_it_is_load_bearing() {
        let mut a = clean();
        a.dest_count = 2;
        a.unverifiable_dests.push(DeviceId::DestB);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(!r.state.authorises_erase());
    }

    /// Zero is the permission. Everything else is a distinguishable refusal, so
    /// a wrapper can tell "you may erase" from "the copy is fine but do not".
    #[test]
    fn only_the_permission_exits_zero() {
        let mut codes = std::collections::BTreeSet::new();
        for v in Verdict::ALL {
            assert_eq!(
                v.exit_code() == 0,
                v.authorises_erase(),
                "{v:?} exit {} vs authorises {}",
                v.exit_code(),
                v.authorises_erase()
            );
            assert!(codes.insert(v.exit_code()), "{v:?} reuses an exit code");
        }
        assert_eq!(codes.len(), Verdict::ALL.len());
    }

    /// Distinctness is meaningless with one destination, and complaining about
    /// it there would put an unfixable warning on every single-drive run.
    #[test]
    fn one_destination_is_not_nagged_about_distinctness() {
        let mut a = clean();
        a.dest_count = 1;
        a.distinctness = None;
        let r = assess(&a);
        assert!(
            !r.reasons.iter().any(|s| s.contains("device identity")),
            "{:?}",
            r.reasons
        );
    }

    /// The safety invariant, over every combination of the inputs that decide a
    /// tier. Only the fully-proven arrangement may ever say yes.
    #[test]
    fn nothing_but_the_complete_arrangement_authorises_an_erase() {
        for had_card2 in [true, false] {
            for dest_count in [1usize, 2] {
                for untwinned in [0usize, 1] {
                    for distinct in [
                        Some(Distinctness::Distinct),
                        Some(Distinctness::SameDevice),
                        None,
                    ] {
                        let mut a = clean();
                        a.had_card2 = had_card2;
                        a.dest_count = dest_count;
                        a.untwinned = (0..untwinned).map(|i| format!("f{i}")).collect();
                        a.distinctness = distinct.clone();
                        let complete = had_card2
                            && dest_count == 2
                            && untwinned == 0
                            && distinct == Some(Distinctness::Distinct);
                        assert_eq!(
                            assess(&a).state.authorises_erase(),
                            complete,
                            "card2={had_card2} dests={dest_count} untwinned={untwinned} \
                             distinct={distinct:?}"
                        );
                    }
                }
            }
        }
    }

    /// Manifest presence is the success signal, so a run that cannot write one
    /// is not a successful run even though every byte verified.
    #[test]
    fn missing_manifest_fails_the_run() {
        let mut a = clean();
        a.manifests_written = false;
        let r = assess(&a);
        assert_eq!(r.state, Verdict::Failed);
        assert!(r.reasons[0].contains("cannot be re-verified later"));
    }

    #[test]
    fn a_systematic_abort_is_a_failure() {
        let mut a = clean();
        a.aborted = Some("every destination disagrees with the cards".into());
        let r = assess(&a);
        assert_eq!(r.state, Verdict::Failed);
        assert!(r.reasons[0].starts_with("job aborted"));
    }

    /// Failure outranks unprovenness: a broken run is never reported as merely
    /// unproven, however many other things are also wrong.
    #[test]
    fn failure_outranks_every_softer_problem() {
        let mut a = clean();
        a.had_card2 = false;
        a.distinctness = Some(Distinctness::SameDevice);
        a.failures.push(FileFailure {
            rel: "x.ARW".into(),
            diagnosis: Diagnosis::Systematic,
        });
        assert_eq!(assess(&a).state, Verdict::Failed);
    }

    #[test]
    fn size_conflicts_fail_the_run() {
        let mut a = clean();
        a.conflicts.push("DCIM/100MSDCF/DSC00042.ARW".into());
        let r = assess(&a);
        assert_eq!(r.state, Verdict::Failed);
        assert!(r.reasons[0].contains("different sizes"));
    }

    #[test]
    fn every_non_clean_verdict_carries_a_reason() {
        // The design forbids a bare "verification failed".
        for a in [
            {
                let mut a = clean();
                a.had_card2 = false;
                a
            },
            {
                let mut a = clean();
                a.manifests_written = false;
                a
            },
            {
                let mut a = clean();
                a.failures.push(FileFailure {
                    rel: "x".into(),
                    diagnosis: Diagnosis::Unclassified,
                });
                a
            },
        ] {
            let r = assess(&a);
            assert!(!r.reasons.is_empty(), "{:?} produced no reasoning", r.state);
            assert!(r.reasons.iter().all(|s| !s.is_empty()));
        }
    }

    /// Two cards on one physical device is not a twin. Without this the run
    /// would report SAFE TO FORMAT having compared a card against itself.
    #[test]
    fn cards_on_one_device_never_authorise_a_format() {
        let mut a = clean();
        a.cards_distinct = Some(Distinctness::SameDevice);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(
            r.reasons[0].contains("one physical device"),
            "{:?}",
            r.reasons
        );
        assert!(r.reasons[0].contains("real twin"));
    }

    /// A handful of recovered files is a glitch: the bytes verified, so refusing
    /// the format would be superstition.
    #[test]
    fn a_few_retries_do_not_block_the_format() {
        let mut a = clean();
        a.retries.insert(DeviceId::DestA, RETRY_GLITCH_CEILING);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::SafeToFormat, "{:?}", r.reasons);
    }

    /// Enough of them is a failing drive, and that must not authorise an erase.
    #[test]
    fn many_retries_block_the_format_and_name_the_drive() {
        let mut a = clean();
        a.retries.insert(DeviceId::DestA, RETRY_GLITCH_CEILING + 1);
        let r = assess(&a);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(r.reasons[0].contains("drive A"), "{:?}", r.reasons);
        assert!(r.reasons[0].contains("failing drive"));
        assert_eq!(r.files_failed, 0, "the files themselves were fine");
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1613), "1,613");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn preview_truncates_long_lists() {
        let items: Vec<String> = (0..10).map(|i| format!("f{i}")).collect();
        assert_eq!(preview(&items), "f0, f1, f2, and 7 more");
        assert_eq!(preview(&items[..2]), "f0, f1");
    }

    /// The hole this closes: `twin_matched` is vacuously true over an empty set,
    /// a zero-entry manifest writes fine, and the clean arm was reached with
    /// nothing verified at all. Two unreadable cards produced
    /// "SAFE TO FORMAT · 0 files · twin-matched · 2 distinct volumes".
    #[test]
    fn a_run_that_found_nothing_never_authorises_an_erase() {
        let mut a = clean();
        a.files_total = 0;
        a.bytes_total = 0;
        let r = assess(&a);
        assert!(!r.state.authorises_erase(), "got {:?}", r.state);
        assert_eq!(r.state, Verdict::VerifiedDoNotFormat);
        assert!(
            r.reasons.iter().any(|s| s.contains("nothing was verified")),
            "{:?}",
            r.reasons
        );
    }

    /// A card throwing errors during the metadata walk is, in `scan.rs`'s own
    /// words, not a card to format. A WalkDir error takes its whole subtree with
    /// it, so those files are not untwinned or missing -- they were never seen,
    /// and nothing else in the assessment can notice.
    #[test]
    fn unreadable_entries_on_a_card_block_the_format_verdict() {
        let mut a = clean();
        a.scan_errors = 2;
        let r = assess(&a);
        assert!(!r.state.authorises_erase());
        assert!(
            r.reasons.iter().any(|s| s.contains("could not be read")),
            "{:?}",
            r.reasons
        );
    }

    /// Half the redundancy living on the card you are being told to erase.
    /// Preflight detected this and only warned; the verdict had no term for it.
    #[test]
    fn a_destination_on_a_cards_own_disk_blocks_the_format_verdict() {
        let mut a = clean();
        a.dest_on_card_device
            .push("CARD 1 and DEST A are on the same physical device (3A2F0D18)".into());
        let r = assess(&a);
        assert!(!r.state.authorises_erase());
        assert!(
            r.reasons.iter().any(|s| s.contains("same physical device")),
            "{:?}",
            r.reasons
        );
    }

    /// Preflight refuses several things outright and never reaches an
    /// assessment. Those runs used to produce no verdict at all, so the window's
    /// banner kept showing the phase it stopped in while the reason scrolled
    /// past in the log. A refusal is a failure in every sense that matters.
    #[test]
    fn a_refusal_is_a_failed_verdict_carrying_its_reason() {
        let r = VerdictReport::refused(vec!["DCIM/COM1.ARW: reserved device name".into()]);
        assert_eq!(r.state, Verdict::Failed);
        assert!(!r.state.authorises_erase());
        assert!(!r.state.copy_is_good());
        assert_eq!(r.headline(), "FAILED");
        assert!(r.reasons[0].contains("COM1"));
    }
}
