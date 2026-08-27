//! Re-verification from a manifest.
//!
//! The design calls the manifest "the part of this program designed to outlive
//! it" -- but a manifest nothing can read is a promise rather than a property.
//! This module is the other half: given a `.mhl` and the folder it describes,
//! re-hash every file off the device and say whether the drive is still good.
//!
//! It needs no cards, no second destination, and no session. Months later, at
//! home, "is this drive still what it was?" has an answer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;

use super::mhl::Manifest;
use super::scan;
use super::telemetry::{ByteMeter, Level, Stage, Telemetry};
use super::unbuffered::{hash_unbuffered_cb, hex64_short};
use super::DeviceId;

/// One file's re-check outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The bytes on the device still hash to what the manifest recorded.
    Matched,
    /// The file is there and has changed. Bit rot, or an edit.
    Changed { expected: u64, actual: u64 },
    /// The manifest lists it and the device does not have it.
    Missing,
    /// Present but unreadable -- which for this purpose is as bad as missing.
    Unreadable { error: String },
}

impl Outcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Matched)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Matched => "matches the manifest".into(),
            Self::Changed { expected, actual } => format!(
                "CHANGED -- manifest says {}, device holds {}",
                hex64_short(*expected),
                hex64_short(*actual)
            ),
            Self::Missing => "MISSING from the device".into(),
            Self::Unreadable { error } => format!("UNREADABLE -- {error}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileCheck {
    pub rel: String,
    pub outcome: Outcome,
}

/// What a re-check established.
#[derive(Debug, Default, Serialize)]
pub struct RecheckReport {
    pub files: Vec<FileCheck>,
    /// Files on the device that the manifest does not list.
    ///
    /// Not a failure -- a later session writes its own manifest alongside -- but
    /// worth naming, because an unlisted file is one nothing is vouching for.
    pub extras: Vec<String>,
    pub bytes_hashed: u64,
    pub cancelled: bool,
}

impl RecheckReport {
    pub fn matched(&self) -> usize {
        self.files.iter().filter(|f| f.outcome.is_ok()).count()
    }

    pub fn failures(&self) -> impl Iterator<Item = &FileCheck> {
        self.files.iter().filter(|f| !f.outcome.is_ok())
    }

    /// Whether every listed file is still exactly as recorded.
    pub fn intact(&self) -> bool {
        !self.cancelled && self.files.iter().all(|f| f.outcome.is_ok())
    }

    /// The one-line answer.
    pub fn headline(&self) -> String {
        if self.cancelled {
            return "CANCELLED — the re-check did not finish".into();
        }
        if self.intact() {
            format!("INTACT — all {} files match the manifest", self.files.len())
        } else {
            format!(
                "DAMAGED — {} of {} files no longer match the manifest",
                self.failures().count(),
                self.files.len()
            )
        }
    }
}

/// Re-hash everything a manifest lists, against the folder it describes.
///
/// `root` is normally the directory holding the `.mhl`, because that is what
/// MHL's relative paths are relative to.
pub fn verify_manifest(
    manifest: &Manifest,
    root: &Path,
    tel: &Telemetry,
    cancel: &AtomicBool,
) -> Result<RecheckReport> {
    let mut report = RecheckReport::default();
    // The device is the only destination involved, so borrow its label for the
    // throughput meter the UI already knows how to draw.
    let mut meter = ByteMeter::new(DeviceId::DestA);

    tel.info(
        Stage::Verify,
        format!(
            "re-checking {} files ({:.1} GB) against {}",
            manifest.entries.len(),
            manifest.total_bytes() as f64 / 1e9,
            root.display()
        ),
    );
    tel.info(
        Stage::Verify,
        format!(
            "manifest written by {} on {} at {}",
            manifest.creator.tool,
            manifest.creator.hostname,
            super::telemetry::rfc3339(manifest.creator.start)
        ),
    );

    let listed: BTreeSet<&str> = manifest.entries.iter().map(|e| e.rel.as_str()).collect();

    for entry in &manifest.entries {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let path = absolute(root, &entry.rel);
        let outcome = if !path.is_file() {
            Outcome::Missing
        } else {
            match hash_unbuffered_cb(&path, cancel, |n| meter.add(n, tel)) {
                Ok(Some((hash, len))) => {
                    report.bytes_hashed += len;
                    if hash == entry.hash {
                        Outcome::Matched
                    } else {
                        Outcome::Changed {
                            expected: entry.hash,
                            actual: hash,
                        }
                    }
                }
                Ok(None) => {
                    report.cancelled = true;
                    break;
                }
                Err(e) => Outcome::Unreadable {
                    error: format!("{e:#}"),
                },
            }
        };

        let name = entry.rel.rsplit('/').next().unwrap_or(&entry.rel);
        tel.log(
            if outcome.is_ok() {
                Level::Ok
            } else {
                Level::Err
            },
            Stage::Verify,
            format!("{name}  {}", outcome.describe()),
        );
        report.files.push(FileCheck {
            rel: entry.rel.clone(),
            outcome,
        });
    }
    meter.flush(tel);

    // Anything on the device the manifest does not vouch for.
    if let Ok(found) = scan::scan(root) {
        for rel in found.entries.keys() {
            if !listed.contains(rel.as_str()) && !is_sidecar(rel) {
                report.extras.push(rel.clone());
            }
        }
    }
    if !report.extras.is_empty() {
        tel.warn(
            Stage::Verify,
            format!(
                "{} file(s) on the device are not in this manifest -- nothing vouches for them",
                report.extras.len()
            ),
        );
    }

    tel.log(
        if report.intact() {
            Level::Ok
        } else {
            Level::Err
        },
        Stage::Verdict,
        report.headline(),
    );
    Ok(report)
}

/// One session folder found on a drive, and whether anything vouches for it.
#[derive(Debug, Clone)]
pub struct FoundSession {
    pub dir: PathBuf,
    /// Every MHL v1 manifest in the folder, in a stable order.
    ///
    /// Plural because a shoot day that offloads three card pairs into one
    /// folder is ordinary rather than exotic: the folder is named
    /// `<date>_<label>` alone, so each run writes its own `sluice_<id>.mhl`
    /// beside the last, and each lists only the files that run copied.
    /// Re-checking the first one found and calling the folder INTACT would
    /// leave the other two pairs unread and unmentioned.
    pub manifests: Vec<PathBuf>,
}

/// Every sluice session folder directly under `root`.
///
/// Session folders are `<date>_<label>` with a `sluice_<id>.mhl` at the top, so
/// this is a shallow walk rather than a scan of the whole drive.
///
/// Folders with *no* manifest are returned too, and they are the valuable half:
/// a run that did not verify cleanly deliberately leaves no manifest behind
/// ("manifest presence is the success signal"), so an unvouched folder is a
/// designed outcome that nothing ever surfaced again.
pub fn find_sessions(root: &Path) -> Vec<FoundSession> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<FoundSession> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|dir| {
            let mut manifests: Vec<PathBuf> = std::fs::read_dir(&dir)
                .ok()
                .map(|inner| {
                    inner
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .filter(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy())
                                .is_some_and(|n| n.starts_with("sluice_") && n.ends_with(".mhl"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            manifests.sort();
            // A directory that is neither a session folder nor holds one is not
            // this program's business.
            let looks_like_a_session = !manifests.is_empty()
                || dir.join(ASCMHL_DIR).is_dir()
                || dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .is_some_and(|n| looks_like_a_session_name(&n));
            looks_like_a_session.then_some(FoundSession { dir, manifests })
        })
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// `2026-03-14_shoot`: the exact shape `JobConfig::session_dir` writes.
///
/// Matching loosely is not free. A folder with no manifest is reported as one
/// nothing vouches for, which is a real signal about a real interrupted run --
/// and a sweep that cries wolf over `Lightroom Catalog` or `Take-3_selects` is
/// one people stop reading. `sanitise` guarantees a non-empty label, so a real
/// session name is always longer than the date and separator.
fn looks_like_a_session_name(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() > 11
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'_'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// The folder the manifest's relative paths are measured from.
///
/// MHL v1 paths are relative to the manifest's own directory, so a manifest
/// beside the files needs no thought. ASC MHL puts its hash lists one level down
/// in an `ascmhl/` folder, and its paths are relative to the *parent* of that
/// folder -- so taking the manifest's directory there reports every single file
/// as missing, on a drive that is perfectly intact. A false DAMAGED is not much
/// better than a false INTACT.
fn default_root(manifest_path: &Path) -> Result<PathBuf> {
    let dir = manifest_path
        .parent()
        .context("the manifest path has no parent directory")?;
    if dir
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(ASCMHL_DIR))
    {
        if let Some(parent) = dir.parent() {
            return Ok(parent.to_path_buf());
        }
    }
    Ok(dir.to_path_buf())
}

/// The ASC MHL folder name, as the spec fixes it.
const ASCMHL_DIR: &str = "ascmhl";

/// Files sluice itself puts on a destination, which no manifest vouches for and
/// which are not worth reporting as unaccounted-for.
///
/// Both manifest dialects live here, and so does the write probe -- a file
/// preflight creates and removes, which only survives a machine dying between
/// the two.
fn is_sidecar(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if rel
        .split('/')
        .any(|part| part.eq_ignore_ascii_case(ASCMHL_DIR))
    {
        return name.ends_with(".mhl") || name.ends_with(".xml");
    }
    name == ".sluice-write-probe"
        || (name.starts_with("sluice_") && (name.ends_with(".mhl") || name.ends_with(".json")))
}

/// Join a manifest entry onto the folder it describes, without leaving it.
///
/// `parse_mhl` refuses an entry that could escape, so the filter below is the
/// second lock on the same door rather than the first. It is here because this
/// is the function that does the joining: `PathBuf::push` treats a `..` as an
/// instruction and a drive prefix as a replacement, so a caller that ever
/// forgot to validate would get traversal rather than an error.
fn absolute(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            continue;
        }
        p.push(part);
    }
    super::win::extended_path(&p)
}

/// Load a manifest and re-check the folder it sits in.
pub fn recheck_path(
    manifest_path: &Path,
    root: Option<&Path>,
    tel: &Telemetry,
    cancel: &AtomicBool,
) -> Result<RecheckReport> {
    let manifest = super::mhl::parse_mhl(manifest_path)?;
    let root = match root {
        Some(r) => r.to_path_buf(),
        None => default_root(manifest_path)?,
    };
    verify_manifest(&manifest, &root, tel, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mhl::{parse_mhl_str, render_mhl, CreatorInfo, HashEntry};
    use crate::engine::scan::Mtime;
    use crate::engine::unbuffered::hash_unbuffered;
    use chrono::{DateTime, Utc};
    use std::fs;

    fn creator() -> CreatorInfo {
        CreatorInfo {
            name: "Adam".into(),
            hostname: "field-laptop".into(),
            tool: "sluice 0.1.0".into(),
            start: DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
                .unwrap()
                .with_timezone(&Utc),
            finish: DateTime::parse_from_rfc3339("2026-03-14T22:39:51Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    /// Build a real directory plus the manifest that describes it.
    fn rig(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for (rel, bytes) in files {
            let p = absolute(dir.path(), rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, bytes).unwrap();
            let (hash, _) = hash_unbuffered(&p).unwrap();
            entries.push(HashEntry {
                rel: rel.to_string(),
                size: bytes.len() as u64,
                mtime: Mtime {
                    secs: 1_757_629_443,
                    nanos: 0,
                },
                hash,
                hashed_at: creator().finish,
            });
        }
        let path = dir.path().join("sluice_20260314-221403.mhl");
        fs::write(&path, render_mhl(&creator(), &entries)).unwrap();
        (dir, path)
    }

    fn run(manifest: &Path) -> RecheckReport {
        let (tel, rx) = Telemetry::new();
        let cancel = AtomicBool::new(false);
        let r = recheck_path(manifest, None, &tel, &cancel).unwrap();
        drop(tel);
        let _ = rx.iter().count();
        r
    }

    /// Test 14's real form: the manifest is not merely well-formed, it can be
    /// read back and used to prove a drive.
    #[test]
    fn an_untouched_folder_reads_back_as_intact() {
        let (_dir, manifest) = rig(&[
            ("DCIM/100MSDCF/DSC00001.ARW", b"the first frame"),
            ("DCIM/100MSDCF/DSC00002.ARW", b"the second frame"),
        ]);
        let r = run(&manifest);
        assert!(r.intact(), "{:?}", r.files);
        assert_eq!(r.matched(), 2);
        assert!(r.extras.is_empty(), "the manifest itself is not a stray");
        assert!(r.headline().starts_with("INTACT"));
    }

    /// The point of the whole exercise: silent rot months later is detected.
    #[test]
    fn a_flipped_bit_is_reported_as_changed() {
        let (dir, manifest) = rig(&[("DCIM/X.ARW", b"the first frame")]);
        let victim = absolute(dir.path(), "DCIM/X.ARW");
        let mut bytes = fs::read(&victim).unwrap();
        bytes[0] ^= 0x01;
        fs::write(&victim, &bytes).unwrap();

        let r = run(&manifest);
        assert!(!r.intact());
        assert_eq!(r.failures().count(), 1);
        assert!(matches!(r.files[0].outcome, Outcome::Changed { .. }));
        assert!(r.files[0].outcome.describe().contains("CHANGED"));
        assert!(r.headline().starts_with("DAMAGED"));
    }

    #[test]
    fn a_deleted_file_is_reported_as_missing() {
        let (dir, manifest) = rig(&[("DCIM/X.ARW", b"gone soon"), ("DCIM/Y.ARW", b"still here")]);
        fs::remove_file(absolute(dir.path(), "DCIM/X.ARW")).unwrap();

        let r = run(&manifest);
        assert!(!r.intact());
        assert_eq!(r.files[0].outcome, Outcome::Missing);
        assert!(r.files[1].outcome.is_ok(), "the survivor still checks out");
    }

    /// An unlisted file is not a failure, but nothing vouches for it and it is
    /// named rather than passed over.
    #[test]
    fn a_file_the_manifest_does_not_list_is_reported_as_an_extra() {
        let (dir, manifest) = rig(&[("DCIM/X.ARW", b"listed")]);
        fs::write(absolute(dir.path(), "DCIM/STRAY.ARW"), b"unlisted").unwrap();

        let r = run(&manifest);
        assert!(r.intact(), "an extra does not damage what is listed");
        assert_eq!(r.extras, vec!["DCIM/STRAY.ARW"]);
    }

    /// Round-trip: what `render_mhl` writes, `parse_mhl_str` reads back
    /// identically. This is what makes the manifest a durable artifact rather
    /// than a write-only one.
    #[test]
    fn a_rendered_manifest_round_trips_through_the_parser() {
        let entries = vec![
            HashEntry {
                rel: "DCIM/100MSDCF/DSC00001.ARW".into(),
                size: 62_914_560,
                mtime: Mtime {
                    secs: 1_757_629_443,
                    nanos: 0,
                },
                hash: 0xa1b2_c3d4_e5f6_0718,
                hashed_at: creator().finish,
            },
            HashEntry {
                // Metacharacters must survive escaping and un-escaping.
                rel: "DCIM/a&b/<x>'y'.ARW".into(),
                size: 1,
                mtime: Mtime { secs: 0, nanos: 0 },
                hash: 1,
                hashed_at: creator().start,
            },
        ];
        let parsed = parse_mhl_str(&render_mhl(&creator(), &entries)).unwrap();

        assert_eq!(parsed.creator.name, "Adam");
        assert_eq!(parsed.creator.tool, "sluice 0.1.0");
        assert_eq!(parsed.creator.start, creator().start);
        assert_eq!(parsed.entries.len(), 2);
        for (got, want) in parsed.entries.iter().zip(&entries) {
            assert_eq!(got.rel, want.rel);
            assert_eq!(got.size, want.size);
            assert_eq!(got.hash, want.hash);
            assert_eq!(got.mtime.secs, want.mtime.secs);
        }
        assert_eq!(parsed.total_bytes(), 62_914_561);
    }

    #[test]
    fn something_that_is_not_a_manifest_is_rejected() {
        assert!(parse_mhl_str("<html><body>nope</body></html>").is_err());
    }

    // --- where a manifest's paths are measured from ------------------------

    /// The bug this exists to prevent: an ASC MHL hash list lives one level down
    /// in `ascmhl/`, and its paths are relative to the *parent* of that folder.
    /// Defaulting to the manifest's own directory reported every file as MISSING
    /// on a drive that was perfectly intact -- a false DAMAGED, which is not much
    /// better than a false INTACT.
    #[test]
    fn an_asc_manifest_measures_from_the_folder_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("2026-03-14_shoot");
        let asc = session.join("ascmhl");
        fs::create_dir_all(&asc).unwrap();
        let manifest = asc.join("0001_20260314-2214.mhl");
        fs::write(&manifest, b"placeholder").unwrap();

        assert_eq!(
            default_root(&manifest).unwrap(),
            session,
            "an ASC hash list is measured from the parent of ascmhl/"
        );
    }

    /// MHL v1 sits beside the files it describes, and must keep doing so.
    #[test]
    fn a_v1_manifest_measures_from_its_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("sluice_20260314-2214.mhl");
        fs::write(&manifest, b"placeholder").unwrap();
        assert_eq!(default_root(&manifest).unwrap(), dir.path());
    }

    /// sluice's own artifacts are not "files nothing vouches for". Before the
    /// ASC manifests were recognised, every single re-verification warned about
    /// two files it had written itself -- noise in exactly the place where noise
    /// is expensive.
    #[test]
    fn sluice_writes_nothing_it_would_then_report_as_unaccounted_for() {
        for rel in [
            "sluice_20260314-2214.mhl",
            "sluice_20260314-2214.json",
            "ascmhl/0001_20260314-2214.mhl",
            "ascmhl/ascmhl_chain.xml",
            ".sluice-write-probe",
        ] {
            assert!(is_sidecar(rel), "{rel} must not be reported as an extra");
        }
    }

    /// And it must not swallow a real file. A photograph that the manifest does
    /// not vouch for is the thing this report exists to surface.
    #[test]
    fn real_files_are_still_reported_as_extras() {
        for rel in [
            "DCIM/100MSDCF/DSC00001.ARW",
            "notes.mhl",
            "PRIVATE/M4ROOT/CLIP/C0001.MP4",
            "ascmhl_notes/DSC1.ARW",
        ] {
            assert!(!is_sidecar(rel), "{rel} must still be reported");
        }
    }

    // --- finding sessions on a drive ---------------------------------------

    /// A shuttle drive holds a season. Requiring one --manifest with a
    /// hand-typed session id is why "how you find out months later whether a
    /// drive is still what it was" did not get run.
    #[test]
    fn every_session_folder_on_a_drive_is_found() {
        let drive = tempfile::tempdir().unwrap();
        for name in ["2026-03-14_shoot", "2026-03-15_shoot"] {
            let dir = drive.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("sluice_20260314-2214.mhl"), b"<hashlist/>").unwrap();
        }
        // Not a session folder.
        fs::create_dir_all(drive.path().join("Lightroom Catalog")).unwrap();

        // Near misses that are somebody's ordinary folders, not sessions. A
        // sweep that reports these as "nothing vouches for this" is one people
        // stop reading.
        for noise in [
            "Lightroom Catalog",
            "2024-shoot_raw",
            "Sony-A7_b-roll_v2",
            "2024-06_export",
            "Take-3_selects",
            "Proj-2024_final",
        ] {
            fs::create_dir_all(drive.path().join(noise)).unwrap();
        }

        let found = find_sessions(drive.path());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| !f.manifests.is_empty()));
    }

    /// A shoot day that offloads three card pairs into one folder is ordinary:
    /// the folder is named for the date and label alone. Each run writes its
    /// own manifest listing only its own files, so re-checking the first one
    /// found and calling the folder INTACT leaves the rest unread.
    #[test]
    fn every_manifest_in_a_session_folder_is_kept() {
        let drive = tempfile::tempdir().unwrap();
        let dir = drive.path().join("2026-03-14_shoot");
        fs::create_dir_all(&dir).unwrap();
        for id in ["20260314-2214", "20260314-2337", "20260315-0102"] {
            fs::write(dir.join(format!("sluice_{id}.mhl")), b"<hashlist/>").unwrap();
        }

        let found = find_sessions(drive.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].manifests.len(), 3, "{:?}", found[0].manifests);
        // Stable order, so the console output does not shuffle between runs.
        let mut sorted = found[0].manifests.clone();
        sorted.sort();
        assert_eq!(sorted, found[0].manifests);
    }

    /// The valuable half: a run that did not verify cleanly deliberately leaves
    /// no manifest, so an unvouched folder is a designed outcome that nothing
    /// else ever surfaces again.
    #[test]
    fn a_folder_with_no_manifest_is_still_reported() {
        let drive = tempfile::tempdir().unwrap();
        let dir = drive.path().join("2026-03-14_interrupted");
        fs::create_dir_all(dir.join("DCIM")).unwrap();

        let found = find_sessions(drive.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].manifests.is_empty(),
            "it must be reported as unvouched, not skipped"
        );
    }

    #[test]
    fn an_empty_or_missing_drive_finds_nothing() {
        let drive = tempfile::tempdir().unwrap();
        assert!(find_sessions(drive.path()).is_empty());
        assert!(find_sessions(&drive.path().join("nope")).is_empty());
    }
}
