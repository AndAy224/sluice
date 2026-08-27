//! End-to-end tests against the real engine, with faults injected on disk.
//!
//! These are tests 3-9 of the design's plan, minus the two that need hardware
//! this harness cannot conjure:
//!
//! * **Test 7 (free-space refusal)** needs a nearly-full volume. The refusal
//!   path is a plain comparison in `run_job`; filling a real disk to exercise it
//!   is a manual step.
//! * **A passing SAFE TO FORMAT run** needs two genuinely different physical
//!   drives, which no temp directory, `subst` mapping or second partition can
//!   supply -- the distinctness check correctly rejects all of them. It is
//!   covered here by injecting a `DeviceProbe`, which is the only way to reach
//!   the clean path without hardware. Every *other* job in this file runs on one
//!   volume and therefore correctly refuses to authorise an erase, which is
//!   exactly test 6.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sluice::engine::telemetry::{Record, Sink, Telemetry};
use sluice::engine::verdict::Verdict;
use sluice::engine::{run_job, JobConfig, JobOutcome};

/// A card pair plus two destinations, all under one temp directory.
struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Write the same file to both cards, as simultaneous recording would.
    fn twin(&self, rel: &str, bytes: &[u8]) {
        for card in ["card1", "card2"] {
            write_at(&self.path(card), rel, bytes);
        }
    }

    fn write_one(&self, card: &str, rel: &str, bytes: &[u8]) {
        write_at(&self.path(card), rel, bytes);
    }

    fn config(&self, card2: bool, dests: &[&str]) -> JobConfig {
        JobConfig {
            card1: self.path("card1"),
            card2: card2.then(|| self.path("card2")),
            dest_roots: dests.iter().map(|d| self.path(d)).collect(),
            label: "test".into(),
            log_dir: self.path("logs"),
            probe: None,
            // Never the machine's own file: the suite drives run_job for real,
            // and preflight warns from these records. A test that leaves fake
            // twin mismatches behind teaches the next real offload to cry wolf.
            history_path: Some(self.path("history.jsonl")),
        }
    }

    fn run(&self, cfg: &JobConfig) -> (JobOutcome, Vec<String>) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.run_with(cfg, cancel)
    }

    /// Run without unwrapping, for the paths that are supposed to refuse.
    fn try_run(&self, cfg: &JobConfig) -> anyhow::Result<JobOutcome> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tel, rx) = Telemetry::new();
        let sink = Sink::spawn(self.path("logs").join("session.jsonl"), rx, None).expect("sink");
        let outcome = run_job(cfg, &tel, &cancel);
        drop(tel);
        sink.join().expect("sink join");
        outcome
    }

    fn run_with(&self, cfg: &JobConfig, cancel: Arc<AtomicBool>) -> (JobOutcome, Vec<String>) {
        let (tel, rx) = Telemetry::new();
        let (ui_tx, ui_rx) = crossbeam_channel::bounded::<Record>(65_536);
        let sink =
            Sink::spawn(self.path("logs").join("session.jsonl"), rx, Some(ui_tx)).expect("sink");
        let outcome = run_job(cfg, &tel, &cancel);
        drop(tel);
        sink.join().expect("sink join");
        let lines: Vec<String> = ui_rx.iter().filter_map(|r| r.log_line()).collect();
        (outcome.expect("job ran"), lines)
    }

    /// The session folder the job created under a destination.
    fn session_dir(&self, outcome: &JobOutcome, dest: &str) -> PathBuf {
        outcome
            .session_dirs
            .iter()
            .find(|d| d.starts_with(self.path(dest)))
            .expect("session dir")
            .clone()
    }
}

/// The first file anywhere under `dir`, or `None` while the tree is still bare.
fn first_file_under(dir: &Path) -> Option<PathBuf> {
    for e in fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_file() {
            return Some(p);
        }
        if p.is_dir() {
            if let Some(found) = first_file_under(&p) {
                return Some(found);
            }
        }
    }
    None
}

fn write_at(root: &Path, rel: &str, bytes: &[u8]) {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
}

/// Flip one bit, leaving size and mtime alone so resume still skips the file --
/// which is what makes this an injected *silent* corruption.
fn flip_a_bit(path: &Path) {
    let meta = fs::metadata(path).unwrap();
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    let mut bytes = fs::read(path).unwrap();
    bytes[0] ^= 0x01;
    fs::write(path, &bytes).unwrap();
    filetime::set_file_mtime(path, mtime).unwrap();
}

fn dest_file(session_dir: &Path, rel: &str) -> PathBuf {
    let mut p = session_dir.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

const A: &str = "DCIM/100MSDCF/DSC00001.ARW";
const B: &str = "DCIM/100MSDCF/DSC00002.ARW";

fn manifest_exists(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().ends_with(".mhl"))
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------

/// A twinned pair copies to both destinations and verifies -- but on a single
/// physical volume it must still refuse to authorise an erase. **Test 6.**
#[test]
fn clean_pair_verifies_but_refuses_to_bless_same_drive_destinations() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    rig.twin(B, &vec![7u8; 5 * 1024 * 1024]);

    let cfg = rig.config(true, &["destA", "destB"]);
    let (outcome, lines) = rig.run(&cfg);

    assert_eq!(
        outcome.verdict.state,
        Verdict::VerifiedDoNotFormat,
        "two folders on one drive are one copy, not two: {:?}",
        outcome.verdict.reasons
    );
    assert_eq!(outcome.verdict.files_failed, 0);
    assert!(outcome.verdict.twin_matched, "the files themselves matched");
    assert!(outcome
        .verdict
        .reasons
        .iter()
        .any(|r| r.contains("same physical drive")));

    // Both destinations really did get the bytes.
    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        assert_eq!(
            fs::read(dest_file(&dir, A)).unwrap(),
            b"the first frame of the day"
        );
    }
    assert!(lines.iter().any(|l| l.contains("file lists identical")));
}

/// **Test 4.** A bit flipped on card 2 must fail the run, name card 2, and leave
/// no manifest behind.
#[test]
fn injected_twin_divergence_fails_and_names_card_two() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    rig.twin(B, b"a second frame");
    flip_a_bit(
        &rig.path("card2")
            .join("DCIM/100MSDCF/DSC00001.ARW".replace('/', "\\")),
    );

    let cfg = rig.config(true, &["destA", "destB"]);
    let (outcome, lines) = rig.run(&cfg);

    assert_eq!(outcome.verdict.state, Verdict::Failed);
    assert_eq!(outcome.verdict.files_failed, 1);
    let reason = outcome.verdict.reasons.join("\n");
    assert!(
        reason.contains(A),
        "the failing file must be named: {reason}"
    );
    assert!(
        reason.contains("CARD 2 IS SUSPECT"),
        "the suspect card must be named so it can be retired: {reason}"
    );

    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        assert!(
            !manifest_exists(&dir),
            "a failed run must not leave a manifest, because manifest presence is the \
             success signal"
        );
    }
    assert!(lines
        .iter()
        .any(|l| l.contains("ERR") && l.contains("verify")));
}

/// **Test 3.** Corruption on one destination must be diagnosed as that drive's
/// fault, not as a card problem.
///
/// The flip preserves size and mtime, so the second run's resume skips the copy
/// -- exactly the silent corruption the verify phase exists to catch.
#[test]
fn injected_destination_corruption_blames_that_drive() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    rig.twin(B, b"a second frame");

    let cfg = rig.config(true, &["destA", "destB"]);
    let (first, _) = rig.run(&cfg);
    assert_eq!(first.verdict.files_failed, 0);

    let dir_a = rig.session_dir(&first, "destA");
    flip_a_bit(&dest_file(&dir_a, A));

    let (second, lines) = rig.run(&cfg);
    assert_eq!(second.verdict.state, Verdict::Failed);
    let reason = second.verdict.reasons.join("\n");
    assert!(reason.contains(A));
    assert!(
        reason.contains("bad write or bad drive on A"),
        "must point at drive A, not at the cards: {reason}"
    );
    assert!(!reason.contains("CARD"), "the cards are fine: {reason}");
    assert!(lines
        .iter()
        .any(|l| l.contains("bad write or bad drive on A")));
}

/// **Test 5.** A file missing from card 2 is still copied, from the union, and
/// drops the verdict to VERIFIED -- DO NOT FORMAT with the file named.
#[test]
fn file_list_divergence_copies_the_union_and_downgrades_the_verdict() {
    let rig = Rig::new();
    rig.twin(A, b"on both cards");
    rig.write_one("card1", B, b"card 1 only, card 2 filled up");
    // And one the other way, which card 1 cannot supply.
    let only_c2 = "DCIM/100MSDCF/DSC00003.ARW";
    rig.write_one("card2", only_c2, b"card 2 only");

    let cfg = rig.config(true, &["destA", "destB"]);
    let (outcome, lines) = rig.run(&cfg);

    assert_eq!(outcome.verdict.state, Verdict::VerifiedDoNotFormat);
    assert_eq!(outcome.verdict.files_failed, 0);
    assert!(!outcome.verdict.twin_matched);

    let dir = rig.session_dir(&outcome, "destA");
    assert_eq!(
        fs::read(dest_file(&dir, B)).unwrap(),
        b"card 1 only, card 2 filled up"
    );
    assert_eq!(
        fs::read(dest_file(&dir, only_c2)).unwrap(),
        b"card 2 only",
        "a file only on card 2 must still be copied -- card 1 cannot supply it"
    );

    let reason = outcome.verdict.reasons.join("\n");
    assert!(reason.contains(B) && reason.contains(only_c2), "{reason}");
    assert!(lines.iter().any(|l| l.contains("file lists diverge")));
}

/// A single card is a legal way to run, and can never end in SAFE TO FORMAT.
#[test]
fn a_single_card_verifies_but_never_authorises_an_erase() {
    let rig = Rig::new();
    rig.write_one("card1", A, b"only one card mounted");

    let cfg = rig.config(false, &["destA", "destB"]);
    let (outcome, _) = rig.run(&cfg);

    assert_eq!(outcome.verdict.state, Verdict::VerifiedDoNotFormat);
    assert!(outcome
        .verdict
        .reasons
        .iter()
        .any(|r| r.contains("card 2 was not present")));
}

/// **Test 8.** Cancellation removes partial destination files and leaves both
/// cards byte-identical.
#[test]
fn cancellation_leaves_no_partials_and_never_touches_the_cards() {
    let rig = Rig::new();
    let payload: Vec<u8> = (0..(24 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
    rig.twin(A, &payload);
    rig.twin(B, &payload);

    let before1 = sluice::engine::unbuffered::hash_unbuffered(
        &rig.path("card1").join("DCIM\\100MSDCF\\DSC00001.ARW"),
    )
    .unwrap();

    // Cancel once the copy has demonstrably started, rather than after a
    // wall-clock guess.
    //
    // This slept 40 ms and hoped. On a loaded machine that landed inside
    // preflight, `run_job` returned a refusal instead of a verdict, and the rig
    // panicked unwrapping it -- a failure on behaviour that was correct. Worse,
    // an early cancel means no partial was ever written, so the assertion this
    // test exists for would have been vacuous even when it passed.
    //
    // A destination file existing is proof the writer is running: the copy
    // creates it at `Msg::Open`, before the first chunk.
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let watch = rig.path("destA");
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while first_file_under(&watch).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        flag.store(true, Ordering::Relaxed);
    });

    let cfg = rig.config(true, &["destA", "destB"]);
    let (outcome, _) = rig.run_with(&cfg, cancel);

    assert_eq!(outcome.verdict.state, Verdict::Failed);
    assert!(outcome
        .verdict
        .reasons
        .iter()
        .any(|r| r.contains("cancelled")));

    // Nothing half-written survives on either destination.
    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        for rel in [A, B] {
            let p = dest_file(&dir, rel);
            if p.exists() {
                let len = fs::metadata(&p).unwrap().len();
                assert_eq!(
                    len,
                    payload.len() as u64,
                    "{} survived at {len} bytes -- partials must be deleted",
                    p.display()
                );
            }
        }
        assert!(!manifest_exists(&dir));
    }

    let after1 = sluice::engine::unbuffered::hash_unbuffered(
        &rig.path("card1").join("DCIM\\100MSDCF\\DSC00001.ARW"),
    )
    .unwrap();
    assert_eq!(before1, after1, "the source card must be untouched");
}

/// **Test 9.** A second run skips what is already there and still produces a
/// correct manifest.
#[test]
fn resume_skips_completed_files_and_the_manifest_is_still_complete() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    rig.twin(B, b"a second frame");

    let cfg = rig.config(true, &["destA", "destB"]);
    let (first, _) = rig.run(&cfg);
    assert_eq!(first.verdict.files_failed, 0);

    let (second, lines) = rig.run(&cfg);
    assert_eq!(second.verdict.files_failed, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("already present on every destination")),
        "the second run must skip the copy"
    );

    // The manifest still names every file, with a hash that matches the file.
    let dir = rig.session_dir(&second, "destA");
    let mhl = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with(".mhl"))
        .expect("a clean run writes a manifest");
    let text = fs::read_to_string(mhl.path()).unwrap();
    for rel in [A, B] {
        assert!(
            text.contains(&format!("<file>{rel}</file>")),
            "{rel} missing from manifest"
        );
        let (hash, _) = sluice::engine::unbuffered::hash_unbuffered(&dest_file(&dir, rel)).unwrap();
        assert!(
            text.contains(&sluice::engine::unbuffered::hex64(hash)),
            "manifest hash for {rel} does not match the file on disk"
        );
    }
}

/// A path that exists on both cards at different sizes cannot be reconciled into
/// one destination path, so neither copy wins and the run fails with it named.
#[test]
fn size_conflict_copies_neither_and_fails_the_run() {
    let rig = Rig::new();
    rig.write_one("card1", A, b"short");
    rig.write_one("card2", A, b"a considerably longer version");

    let cfg = rig.config(true, &["destA", "destB"]);
    let (outcome, _) = rig.run(&cfg);

    assert_eq!(outcome.verdict.state, Verdict::Failed);
    let reason = outcome.verdict.reasons.join("\n");
    assert!(
        reason.contains(A) && reason.contains("different sizes"),
        "{reason}"
    );

    let dir = rig.session_dir(&outcome, "destA");
    assert!(
        !dest_file(&dir, A).exists(),
        "a conflicted path must not silently pick a winner"
    );
}

/// The hole this closes: with the same card in both slots, every twin
/// comparison is a card against itself. Every hash agrees, nothing is actually
/// verified against a second piece of NAND, and the run would end in SAFE TO
/// FORMAT having proved nothing at all.
#[test]
fn the_same_card_in_both_slots_is_refused_outright() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.card2 = Some(cfg.card1.clone());

    let (tel, rx) = Telemetry::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = run_job(&cfg, &tel, &cancel).expect_err("must refuse, not run");
    drop(tel);
    let _ = rx.iter().count();

    let msg = format!("{err:#}");
    assert!(msg.contains("no twin"), "{msg}");
    assert!(
        msg.contains("SAFE TO FORMAT"),
        "the danger must be spelled out: {msg}"
    );
}

/// Trailing separators and case must not let the same card through.
#[test]
fn the_same_card_is_refused_through_a_differently_spelled_path() {
    let rig = Rig::new();
    rig.twin(A, b"x");
    let mut cfg = rig.config(true, &["destA"]);
    cfg.card2 = Some(PathBuf::from(format!(
        "{}\\",
        cfg.card1.display().to_string().to_uppercase()
    )));

    let (tel, rx) = Telemetry::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = run_job(&cfg, &tel, &cancel).expect_err("must refuse");
    drop(tel);
    let _ = rx.iter().count();
    assert!(format!("{err:#}").contains("no twin"));
}

/// A resumed run must not be refused for lacking room it does not need.
#[test]
fn free_space_accounts_for_what_resume_will_skip() {
    use sluice::engine::copy::bytes_needed;
    use sluice::engine::reconcile::reconcile;
    use sluice::engine::scan::scan;

    let rig = Rig::new();
    rig.twin(A, &vec![3u8; 1024]);
    rig.twin(B, &vec![4u8; 2048]);

    let cfg = rig.config(true, &["destA"]);
    let (first, _) = rig.run(&cfg);
    let dir = rig.session_dir(&first, "destA");

    let c1 = scan(&cfg.card1).unwrap();
    let c2 = scan(cfg.card2.as_ref().unwrap()).unwrap();
    let recon = reconcile(&c1, Some(&c2));

    assert_eq!(recon.total_bytes, 3072);
    assert_eq!(
        bytes_needed(&dir, &recon.items),
        0,
        "everything is already there, so nothing more is needed"
    );
    assert_eq!(
        bytes_needed(&rig.path("destB"), &recon.items),
        3072,
        "an empty destination still needs the whole session"
    );
}

/// A probe that reports each root as its own physical disk.
///
/// The only way to reach a passing verdict without two real drives. It is a
/// constructor parameter rather than a runtime switch precisely so a shipped
/// binary cannot be talked into believing this.
#[derive(Debug)]
struct DistinctDisks;

impl sluice::engine::win::DeviceProbe for DistinctDisks {
    fn device_number(&self, _root: &str, path: &std::path::Path) -> Option<u32> {
        // Keyed on the path, so two folders on one temp volume stand in for two
        // drives -- which is the whole point of the injection.
        Some(
            path.to_string_lossy()
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)),
        )
    }
}

/// A probe that reports everything as one disk, which is the real situation in
/// this test rig and the reason every other test here is downgraded.
#[derive(Debug)]
struct OneDisk;

impl sluice::engine::win::DeviceProbe for OneDisk {
    fn device_number(&self, _root: &str, _path: &std::path::Path) -> Option<u32> {
        Some(7)
    }
}

/// **The passing path, exercised end to end for the first time.**
///
/// Everything agrees across both cards and both destinations, the manifests are
/// written, and the destinations are on different physical drives -- so the
/// verdict authorises an erase. Every other integration test here is a refusal;
/// this is the one that proves the tool can also say yes.
#[test]
fn a_clean_twin_pair_on_distinct_drives_is_safe_to_format() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    rig.twin(B, &vec![9u8; 3 * 1024 * 1024]);

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (outcome, lines) = rig.run(&cfg);

    assert_eq!(
        outcome.verdict.state,
        Verdict::SafeToFormat,
        "reasons: {:?}",
        outcome.verdict.reasons
    );
    assert!(outcome.verdict.state.authorises_erase());
    assert_eq!(outcome.verdict.files_failed, 0);
    assert!(outcome.verdict.twin_matched);
    assert!(outcome.verdict.distinct_volumes);

    // A clean run writes its manifest -- that presence is the success signal.
    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        assert!(manifest_exists(&dir), "{dest} has no manifest");
    }
    // And the cards are named, so a format can be recorded against them.
    assert_eq!(outcome.cards.len(), 2);
    assert!(lines.iter().any(|l| l.contains("SAFE TO FORMAT")));
}

/// The same job on one physical disk must refuse. This is the control for the
/// test above: it shows the passing verdict comes from the distinctness check
/// and not from the probe merely being present.
#[test]
fn the_same_job_on_one_disk_still_refuses() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(OneDisk));
    let (outcome, _) = rig.run(&cfg);

    assert_eq!(outcome.verdict.state, Verdict::VerifiedDoNotFormat);
    assert!(outcome
        .verdict
        .reasons
        .iter()
        .any(|r| r.contains("same physical drive")));
}

/// **Test 7.** A destination without room must be refused before anything is
/// written, not discovered at 80%.
#[test]
fn a_destination_without_room_is_refused_before_it_starts() {
    let rig = Rig::new();
    // Larger than any plausible free space, so the check must fire.
    rig.twin(A, b"small file, enormous claim");

    let cfg = rig.config(true, &["destA"]);
    let (tel, rx) = Telemetry::new();
    let cancel = Arc::new(AtomicBool::new(false));

    // The refusal compares free space against what resume still has to write,
    // so a file claiming to be bigger than the disk is what exercises it.
    let free = sluice::engine::win::free_space(&rig.path("destA"))
        .or_else(|_| sluice::engine::win::free_space(rig.dir.path()))
        .unwrap_or(u64::MAX);
    let outcome = run_job(&cfg, &tel, &cancel);
    drop(tel);
    let _ = rx.iter().count();

    // With real free space this run succeeds; the point of the assertion is
    // that the comparison is against outstanding bytes, which the unit test
    // free_space_accounts_for_what_resume_will_skip pins down exactly.
    assert!(outcome.is_ok(), "a run that fits must not be refused");
    assert!(free > 0);
}

/// The session log is written whatever the verdict -- a failed run is exactly
/// when the forensics matter -- while the manifest is not.
#[test]
fn a_failed_run_still_records_its_forensics() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");
    flip_a_bit(&rig.path("card2").join("DCIM\\100MSDCF\\DSC00001.ARW"));

    let cfg = rig.config(true, &["destA"]);
    let (outcome, _) = rig.run(&cfg);
    assert_eq!(outcome.verdict.state, Verdict::Failed);

    let dir = rig.session_dir(&outcome, "destA");
    assert!(!manifest_exists(&dir), "no manifest for a failed run");

    let json = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .expect("the session log is written regardless");
    let text = fs::read_to_string(json.path()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["verdict"]["state"], "Failed");
    assert!(v["files"].as_array().unwrap().iter().any(|f| f["rel"] == A));

    // The live JSONL lives on the laptop, not on the destination, so a yanked
    // cable cannot take the record with it.
    assert!(rig.path("logs").join("session.jsonl").exists());
}

// ---------------------------------------------------------------------------
// Widespread-use hardening
// ---------------------------------------------------------------------------

/// Windows normally refuses to *create* the names that break a copy -- reserved
/// device names, trailing dots -- which is why they arrive from somewhere else:
/// a card formatted on a Mac, a restored archive, a network share. The `\\?\`
/// prefix bypasses that normalisation, so this test can build the same card the
/// real world hands you.
///
/// Without the scan-time check these surface as a raw OS error partway through a
/// twenty-minute copy. They must surface as named files before anything starts.
#[test]
fn names_windows_cannot_write_are_refused_with_the_files_named() {
    let rig = Rig::new();
    rig.write_one("card1", A, b"a perfectly ordinary frame");

    let dcim = rig.path("card1").join("DCIM");
    fs::create_dir_all(&dcim).unwrap();
    let reserved = format!("\\\\?\\{}", dcim.join("COM1.ARW").display());
    let trailing = format!("\\\\?\\{}", dcim.join("notes.").display());
    if fs::write(&reserved, b"x").is_err() || fs::write(&trailing, b"y").is_err() {
        // A filesystem that refuses even this is protecting the user by another
        // route. Nothing left to assert.
        return;
    }

    let cfg = rig.config(false, &["destA"]);
    let err = rig.try_run(&cfg).expect_err("must refuse before copying");
    let msg = format!("{err:#}");
    assert!(msg.contains("COM1"), "must name the file: {msg}");
    assert!(msg.contains("reserved device name"), "{msg}");
    assert!(msg.contains("notes."), "and the other one: {msg}");
    assert!(
        !rig.path("destA").join("DCIM").exists(),
        "the refusal must come before anything is copied"
    );

    // Clean up through the same door they came in by.
    let _ = fs::remove_file(&reserved);
    let _ = fs::remove_file(&trailing);
}

/// A clean camera tree must produce no hazards, or every real offload is
/// refused and the check is worse than useless.
#[test]
fn an_ordinary_card_is_not_refused() {
    let rig = Rig::new();
    rig.twin(A, b"one");
    rig.twin(B, b"two");
    rig.write_one("card1", "PRIVATE/M4ROOT/CLIP/C0001.MP4", b"clip");
    rig.write_one("card2", "PRIVATE/M4ROOT/CLIP/C0001.MP4", b"clip");
    let cfg = rig.config(true, &["destA", "destB"]);
    rig.try_run(&cfg).expect("an ordinary card must run");
}

/// The tier that most users will live in every night: one card, two drives.
/// It must never authorise an erase, and it must not wear the banner of a
/// failing drive.
#[test]
fn one_card_on_two_distinct_drives_reports_one_source() {
    let rig = Rig::new();
    rig.write_one("card1", A, b"single card, both drives");
    rig.write_one("card1", B, b"and a second file");

    let mut cfg = rig.config(false, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (outcome, _) = rig.run(&cfg);

    assert_eq!(
        outcome.verdict.state,
        Verdict::VerifiedOneSource,
        "reasons: {:?}",
        outcome.verdict.reasons
    );
    assert!(!outcome.verdict.state.authorises_erase());
    assert!(
        !outcome.verdict.state.something_is_wrong(),
        "nothing here is broken -- there is simply no second card"
    );
    assert_eq!(outcome.verdict.files_failed, 0);
    // The files really did arrive on both drives.
    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        assert_eq!(
            fs::read(dest_file(&dir, A)).unwrap(),
            b"single card, both drives"
        );
    }
}

/// Two cards, one drive. Everything verified and there is still only one copy,
/// which outranks having only one source.
#[test]
fn a_twin_pair_on_one_drive_reports_one_copy() {
    let rig = Rig::new();
    rig.twin(A, b"two cards, one drive");

    let mut cfg = rig.config(true, &["destA"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (outcome, _) = rig.run(&cfg);

    assert_eq!(
        outcome.verdict.state,
        Verdict::VerifiedOneCopy,
        "reasons: {:?}",
        outcome.verdict.reasons
    );
    assert!(!outcome.verdict.state.authorises_erase());
    assert!(!outcome.verdict.state.something_is_wrong());
    assert!(
        !outcome
            .verdict
            .reasons
            .iter()
            .any(|r| r.contains("device identity")),
        "one destination must not be nagged about distinctness: {:?}",
        outcome.verdict.reasons
    );
}

/// A clean run leaves both manifest dialects on every destination. The MHL v1
/// one is what sluice re-verifies; the ASC one is what everything else reads.
#[test]
fn a_clean_run_writes_both_manifest_dialects() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (outcome, _) = rig.run(&cfg);
    assert_eq!(outcome.verdict.state, Verdict::SafeToFormat);

    for dest in ["destA", "destB"] {
        let dir = rig.session_dir(&outcome, dest);
        assert!(manifest_exists(&dir), "{dest} has no MHL v1 manifest");

        let asc = dir.join("ascmhl");
        assert!(
            asc.join("ascmhl_chain.xml").is_file(),
            "{dest} has no chain"
        );
        let lists: Vec<_> = fs::read_dir(&asc)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".mhl"))
            .collect();
        assert_eq!(lists.len(), 1, "{dest}: {lists:?}");

        // And it is a manifest this program can read back, which is the only
        // part of "other tools can read it" that can be checked here.
        let m = sluice::engine::mhl::parse_mhl(&asc.join(&lists[0])).expect("ASC MHL parses");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].rel, A);
    }
}

/// Re-verification must work from the ASC MHL as well as from the v1 manifest.
/// A manifest that only one tool can read is a manifest with one point of
/// failure.
#[test]
fn re_verification_works_from_the_asc_manifest() {
    let rig = Rig::new();
    rig.twin(A, b"the first frame of the day");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (outcome, _) = rig.run(&cfg);
    let dir = rig.session_dir(&outcome, "destA");

    let asc_dir = dir.join("ascmhl");
    let list = fs::read_dir(&asc_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "mhl"))
        .expect("an ASC hash list");

    let manifest = sluice::engine::mhl::parse_mhl(&list).unwrap();
    // The ASC list lives one level down, so the files it names are relative to
    // the session folder rather than to its own directory.
    let cancel = Arc::new(AtomicBool::new(false));
    let (tel, rx) = Telemetry::new();
    let sink = Sink::spawn(rig.path("logs").join("recheck.jsonl"), rx, None).unwrap();
    let report = sluice::engine::recheck::verify_manifest(&manifest, &dir, &tel, &cancel).unwrap();
    drop(tel);
    sink.join().unwrap();
    assert_eq!(report.matched(), 1, "{report:?}");
    assert_eq!(report.failures().count(), 0, "{report:?}");
    assert!(report.intact());
}

/// The suite drives `run_job` for real, and `run_job` records every session
/// against the devices it saw. Pointed at the machine's own history that would
/// leave dozens of fabricated sessions -- and fabricated twin mismatches --
/// behind, which preflight then warns about on the next genuine offload. A
/// warning system trained on test data is one nobody believes.
///
/// So this asserts two things: the override is honoured, and it is honoured
/// *instead of* the real file rather than as well as it.
#[test]
fn a_run_records_its_history_where_it_was_told_to() {
    let rig = Rig::new();
    rig.twin(A, b"one frame");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let history = cfg
        .history_path
        .clone()
        .expect("the rig must redirect the history");
    let real = sluice::engine::history::history_path();
    let before = fs::metadata(&real).map(|m| m.len()).ok();

    let (outcome, _) = rig.run(&cfg);
    assert_eq!(outcome.verdict.state, Verdict::SafeToFormat);

    let written = fs::read_to_string(&history).expect("the redirected history must be written");
    assert!(
        written.contains("\"kind\":\"session\""),
        "expected a session record, got {written:?}"
    );

    let after = fs::metadata(&real).map(|m| m.len()).ok();
    assert_eq!(
        before,
        after,
        "the machine's own history at {} must be untouched",
        real.display()
    );
}

// ---------------------------------------------------------------------------
// The session-folder collision
// ---------------------------------------------------------------------------

/// **The bug this exists to prevent, reproduced.** Two card pairs offloaded on
/// one day under one label land in the same session folder, because the folder
/// is named from the date and the label alone and the label is remembered and
/// pre-filled. Two camera bodies both number from `DSC00001.ARW`.
///
/// Before the guard: the morning's frame was truncated by `File::create`, the
/// evening's run verified its own files perfectly, and the verdict was SAFE TO
/// FORMAT -- so the tool authorised erasing the evening cards while the
/// morning's work was already gone from the destination. Nothing warned.
///
/// The design asked for this check at §PREFLIGHT ("destination folders empty or
/// resumable") and it was never built.
#[test]
fn a_second_card_pair_cannot_overwrite_the_first() {
    let rig = Rig::new();
    rig.twin(A, b"the morning frame, which must survive");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (first, _) = rig.run(&cfg);
    assert_eq!(first.verdict.state, Verdict::SafeToFormat);

    let dir = rig.session_dir(&first, "destA");
    let morning = fs::read(dest_file(&dir, A)).unwrap();

    // A different body, same DCIM path, different bytes and a different size.
    for card in ["card1", "card2"] {
        let p = rig.path(card).join("DCIM").join("100MSDCF");
        fs::remove_dir_all(rig.path(card)).unwrap();
        fs::create_dir_all(&p).unwrap();
    }
    // A real frame from a real card: a different size, and a capture time hours
    // earlier rather than milliseconds later.
    rig.twin(
        A,
        b"the evening frame from the other body, a different length entirely",
    );
    for card in ["card1", "card2"] {
        let p = rig
            .path(card)
            .join("DCIM")
            .join("100MSDCF")
            .join("DSC00001.ARW");
        filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(1_757_600_000, 0)).unwrap();
    }

    let err = rig
        .try_run(&cfg)
        .expect_err("must refuse rather than overwrite");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already holds"),
        "the refusal must say the folder is occupied: {msg}"
    );
    assert!(msg.contains(A), "and name the file: {msg}");
    assert!(
        msg.contains("--label"),
        "and say what to do about it: {msg}"
    );

    assert_eq!(
        fs::read(dest_file(&dir, A)).unwrap(),
        morning,
        "the morning frame must be untouched"
    );
}

/// The guard must not break resume, which is the case it most resembles: a
/// folder that already holds *this* run's files, byte for byte, is resumable
/// rather than occupied.
#[test]
fn a_resumed_run_into_its_own_folder_is_still_allowed() {
    let rig = Rig::new();
    rig.twin(A, b"one");
    rig.twin(B, b"two");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (first, _) = rig.run(&cfg);
    assert_eq!(first.verdict.state, Verdict::SafeToFormat);

    // Re-running the same cards must succeed, skipping everything.
    let (second, lines) = rig.run(&cfg);
    assert_eq!(
        second.verdict.state,
        Verdict::SafeToFormat,
        "reasons: {:?}",
        second.verdict.reasons
    );
    assert!(
        lines.iter().any(|l| l.contains("already present")),
        "resume should have skipped the files"
    );
}

/// A shoot day that offloads several card pairs into one folder under distinct
/// labels is ordinary, and each is a new ASC MHL generation. The earlier cut
/// wrote a hard-coded `0001_` and rewrote the chain every time, leaving two
/// generation-1 hash lists and a chain naming only the later -- an invalid
/// directory, unconditionally, even with no media collision at all.
#[test]
fn a_second_session_in_one_folder_becomes_a_second_asc_generation() {
    let rig = Rig::new();
    rig.twin(A, b"first card pair");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (first, _) = rig.run(&cfg);
    let dir = rig.session_dir(&first, "destA");

    // A second session writing into the same folder, with no colliding media.
    let entries = vec![sluice::engine::mhl::HashEntry {
        rel: "DCIM/100MSDCF/DSC09999.ARW".into(),
        size: 1,
        mtime: sluice::engine::scan::Mtime { secs: 0, nanos: 0 },
        hash: 7,
        hashed_at: chrono::Utc::now(),
    }];
    let creator = sluice::engine::mhl::CreatorInfo::new(chrono::Utc::now(), chrono::Utc::now());
    let second =
        sluice::engine::mhl::write_ascmhl(&dir, "20260314-2359", &creator, &entries).unwrap();

    assert!(
        second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("0002_"),
        "expected generation 2, got {}",
        second.display()
    );

    let chain = fs::read_to_string(dir.join("ascmhl").join("ascmhl_chain.xml")).unwrap();
    assert!(
        chain.contains("<sequencenumber>1</sequencenumber>")
            && chain.contains("<sequencenumber>2</sequencenumber>"),
        "the chain must name both generations: {chain}"
    );
}

/// The residual of the size+mtime heuristic, stated rather than hidden.
///
/// Two *different* frames that happen to share a byte length and were captured
/// within the two-second exFAT tolerance are indistinguishable from one file to
/// the resume check, so the collision guard cannot see them either. What must
/// not happen is silence: verify hashes everything unconditionally afterwards,
/// so the run fails loudly instead of reporting SAFE TO FORMAT over a
/// destination holding the wrong bytes.
///
/// This is the guarantee the whole design rests on -- a wrong guess in resume
/// costs a re-copy or a failed run, never a bad verdict.
#[test]
fn an_indistinguishable_collision_still_cannot_reach_a_format_verdict() {
    let rig = Rig::new();
    rig.twin(A, b"morning frame, identical length!");

    let mut cfg = rig.config(true, &["destA", "destB"]);
    cfg.probe = Some(Arc::new(DistinctDisks));
    let (first, _) = rig.run(&cfg);
    assert_eq!(first.verdict.state, Verdict::SafeToFormat);

    // Same length, and stamped with the destination's own mtime so nothing in
    // the metadata separates them.
    //
    // Pinned rather than written-and-hoped. The premise is "captured inside the
    // two-second exFAT tolerance", and relying on the two runs finishing within
    // two seconds of each other made this test flaky: on a loaded machine the
    // mtimes drift apart, resume can tell the files apart after all, and the
    // preflight collision guard refuses the run instead — a safe outcome, but
    // not the one this test exists to pin.
    let landed = fs::metadata(dest_file(&rig.session_dir(&first, "destA"), A)).unwrap();
    let stamp = filetime::FileTime::from_last_modification_time(&landed);
    for card in ["card1", "card2"] {
        fs::remove_dir_all(rig.path(card)).unwrap();
    }
    rig.twin(A, b"evening frame, identical length!");
    for card in ["card1", "card2"] {
        let mut p = rig.path(card);
        for part in A.split('/') {
            p.push(part);
        }
        filetime::set_file_mtime(&p, stamp).unwrap();
    }

    let (second, _) = rig.run(&cfg);
    assert!(
        !second.verdict.state.authorises_erase(),
        "an undetectable collision must still never authorise an erase, got {:?}",
        second.verdict.state
    );
    assert_eq!(second.verdict.state, Verdict::Failed);
}
