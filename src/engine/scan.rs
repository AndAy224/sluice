//! Card and destination enumeration.
//!
//! A whole-volume walk minus OS litter. The same code scans a card, its twin,
//! and a destination, because the resume check needs to know what is already on
//! a destination in exactly the same terms.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use std::os::windows::fs::MetadataExt;
use walkdir::WalkDir;

use super::win;

/// Directories that belong to an operating system rather than to the shoot.
///
/// Deliberately a short, explicit list. Sony writes `DCIM`, `PRIVATE`,
/// `MP_ROOT`, and `AVF_INFO`, all of which are real content that must be copied;
/// a broader heuristic such as "skip hidden or system" would eat them.
const SKIP_DIRS: &[&str] = &[
    "System Volume Information",
    "$RECYCLE.BIN",
    "RECYCLER",
    ".Trashes",
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
];

/// Files that belong to an operating system rather than to the shoot.
const SKIP_FILES: &[&str] = &["Thumbs.db", "desktop.ini", ".DS_Store", "Desktop.ini"];

fn is_litter_dir(name: &str) -> bool {
    SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name)) || name.eq_ignore_ascii_case("FOUND.000")
}

fn is_litter_file(name: &str) -> bool {
    SKIP_FILES.iter().any(|f| f.eq_ignore_ascii_case(name))
        // AppleDouble sidecars, left behind by a macOS machine touching the card.
        || name.starts_with("._")
}

/// Last-modification time, in a form that survives a round-trip through JSON and
/// back onto a destination file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mtime {
    pub secs: i64,
    pub nanos: u32,
}

impl Mtime {
    pub fn from_metadata(m: &fs::Metadata) -> Self {
        let ft = FileTime::from_last_modification_time(m);
        Self {
            secs: ft.unix_seconds(),
            nanos: ft.nanoseconds(),
        }
    }

    pub fn to_file_time(self) -> FileTime {
        FileTime::from_unix_time(self.secs, self.nanos)
    }

    /// UTC RFC-3339, which is what MHL wants for `lastmodificationdate`.
    pub fn to_rfc3339(self) -> String {
        match Utc.timestamp_opt(self.secs, self.nanos) {
            chrono::LocalResult::Single(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
            _ => DateTime::<Utc>::UNIX_EPOCH.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }

    /// Whether two timestamps are close enough to be the same file.
    ///
    /// exFAT stores modification times at 2-second granularity, so a byte-exact
    /// copy from one exFAT volume to another can legitimately differ by up to
    /// two seconds. Resume uses this; verification never does -- a hash is the
    /// only thing that decides whether a file is good.
    pub fn matches(self, other: Self) -> bool {
        (self.secs - other.secs).abs() <= 2
    }
}

/// One file, keyed by its path relative to the scan root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Forward-slash relative path, e.g. `DCIM/100MSDCF/DSC00001.ARW`.
    pub rel: String,
    pub size: u64,
    pub mtime: Mtime,
}

/// The result of walking one volume.
#[derive(Debug, Clone, Serialize)]
pub struct Scan {
    pub root: PathBuf,
    /// Keyed by relative path. `BTreeMap` so the copy order is deterministic and
    /// roughly sequential on the card, and so manifests come out stable.
    pub entries: BTreeMap<String, Entry>,
    pub total_bytes: u64,
    /// Paths skipped as OS litter, recorded so the log can account for every
    /// file present on the card.
    pub skipped: Vec<String>,
    /// Entries that could not be read at all. A card that throws errors during
    /// a metadata walk is not a card to format.
    pub errors: Vec<String>,
    /// Files whose bytes are not actually on this machine -- OneDrive or Dropbox
    /// placeholders. Recorded during the walk because the attribute is only
    /// available from the metadata call the walk already makes.
    pub placeholders: Vec<String>,
}

impl Scan {
    pub fn file_count(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, rel: &str) -> Option<&Entry> {
        self.entries.get(rel)
    }

    /// Absolute path of a relative entry under this root.
    pub fn absolute(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in rel.split('/') {
            p.push(part);
        }
        p
    }
}

/// Walk `root`, skipping OS litter.
pub fn scan(root: &Path) -> Result<Scan> {
    static NEVER: AtomicBool = AtomicBool::new(false);
    Ok(scan_cb(root, &NEVER, |_, _| {})?
        .expect("cannot be cancelled: the flag is permanently false"))
}

/// [`scan`] with cancellation and a progress callback of `(files, bytes)` so far.
pub fn scan_cb(
    root: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(usize, u64),
) -> Result<Option<Scan>> {
    if !root.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    let mut entries = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut errors = Vec::new();
    let mut placeholders = Vec::new();
    let mut total_bytes = 0u64;

    let walker = WalkDir::new(root).follow_links(false).into_iter();
    let mut walker = walker.filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        !(e.file_type().is_dir() && is_litter_dir(&name))
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Some(next) = walker.next() else { break };
        let entry = match next {
            Ok(e) => e,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = match relative(root, entry.path()) {
            Some(r) => r,
            None => {
                errors.push(format!(
                    "{} is not under {}",
                    entry.path().display(),
                    root.display()
                ));
                continue;
            }
        };
        if is_litter_file(&name) {
            skipped.push(rel);
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("{rel}: {e}"));
                continue;
            }
        };
        // A cloud placeholder is a file whose bytes live somewhere else. Reading
        // one is a download, not a read, so it is recorded here and refused in
        // preflight rather than silently hydrated mid-copy.
        if win::is_cloud_placeholder(meta.file_attributes()) {
            placeholders.push(rel.clone());
        }
        total_bytes += meta.len();
        entries.insert(
            rel.clone(),
            Entry {
                rel,
                size: meta.len(),
                mtime: Mtime::from_metadata(&meta),
            },
        );
        on_progress(entries.len(), total_bytes);
    }

    Ok(Some(Scan {
        root: root.to_path_buf(),
        entries,
        total_bytes,
        skipped,
        errors,
        placeholders,
    }))
}

// ---------------------------------------------------------------------------
// Name hazards
// ---------------------------------------------------------------------------

/// A file this program cannot copy faithfully, found before anything is copied.
///
/// None of these are camera-card problems -- a Sony writes `DSC00001.ARW` and
/// nothing else. They arise the moment somebody points a card slot at an
/// ordinary folder, which is what people do, and every one of them ends in
/// either silent data loss or a mid-run failure at 80%.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Hazard {
    /// Two source files whose paths differ only by case.
    ///
    /// NTFS is case-insensitive, so both copy to one destination path and the
    /// second silently overwrites the first. Verification then catches it --
    /// but as `Systematic`, a diagnosis that means "your bus, RAM or controller
    /// is corrupting data", because both destinations faithfully recorded the
    /// same wrong bytes. That sends somebody off to buy hardware over a
    /// filename.
    CaseCollision { lower: String, rels: Vec<String> },
    /// `CON`, `NUL`, `COM1` and friends. Windows resolves these to devices, not
    /// files, whatever the extension.
    ReservedName { rel: String, name: String },
    /// A character NTFS will not accept. Legal on the HFS+ or ext4 volume the
    /// files came from; not here.
    IllegalCharacter { rel: String, ch: char },
    /// A component ending in a dot or a space. Windows silently strips these on
    /// creation, so the file lands under a different name than the manifest
    /// records, and re-verification cannot find it.
    TrailingDotOrSpace { rel: String },
    /// A OneDrive or Dropbox placeholder: the bytes are not on this machine.
    CloudPlaceholder { rel: String },
}

impl Hazard {
    /// One line, naming the file and what is wrong with it.
    pub fn describe(&self) -> String {
        match self {
            Self::CaseCollision { rels, .. } => format!(
                "{} differ only by capitalisation, and Windows would keep only one of them",
                rels.join(" / ")
            ),
            Self::ReservedName { rel, name } => {
                format!("{rel}: {name} is a reserved device name on Windows")
            }
            Self::IllegalCharacter { rel, ch } => {
                format!("{rel}: Windows filenames cannot contain {ch:?}")
            }
            Self::TrailingDotOrSpace { rel } => format!(
                "{rel}: a name ending in a dot or a space is silently renamed by Windows, so \
                 the manifest would not match what lands"
            ),
            Self::CloudPlaceholder { rel } => format!(
                "{rel}: the bytes are not on this machine -- it is a cloud placeholder, and \
                 reading it would download it rather than read a device"
            ),
        }
    }
}

/// Names Windows resolves to devices rather than to files.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters NTFS rejects. `/` is the separator in a `rel` and so cannot occur.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '\\', '|', '?', '*'];

/// Everything in this scan that cannot be copied faithfully to a Windows volume.
///
/// Run before the copy, so the answer is "these four files, rename them" rather
/// than an access-denied nineteen minutes in.
pub fn hazards(scan: &Scan) -> Vec<Hazard> {
    let mut out = Vec::new();

    // --- case collisions ---------------------------------------------------
    //
    // Grouped rather than reported pairwise: three files colliding is one
    // problem with three names, not three problems.
    let mut by_lower: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rel in scan.entries.keys() {
        by_lower
            .entry(rel.to_lowercase())
            .or_default()
            .push(rel.clone());
    }
    for (lower, rels) in by_lower {
        if rels.len() > 1 {
            out.push(Hazard::CaseCollision { lower, rels });
        }
    }

    // --- per-name problems -------------------------------------------------
    for rel in scan.entries.keys() {
        for part in rel.split('/') {
            if part.ends_with('.') || part.ends_with(' ') {
                out.push(Hazard::TrailingDotOrSpace { rel: rel.clone() });
            }
            // The stem before the first dot is what Windows matches against, so
            // `NUL.ARW` is just as unusable as `NUL`.
            let stem = part.split('.').next().unwrap_or(part);
            if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
                out.push(Hazard::ReservedName {
                    rel: rel.clone(),
                    name: stem.to_string(),
                });
            }
            if let Some(ch) = part
                .chars()
                .find(|c| ILLEGAL.contains(c) || (*c as u32) < 0x20)
            {
                out.push(Hazard::IllegalCharacter {
                    rel: rel.clone(),
                    ch,
                });
            }
        }
    }

    for rel in &scan.placeholders {
        out.push(Hazard::CloudPlaceholder { rel: rel.clone() });
    }

    out
}

/// Forward-slash path of `path` relative to `root`.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for part in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&part.as_os_str().to_string_lossy());
    }
    Some(out)
}

/// Scan a destination, tolerating one that does not exist yet.
pub fn scan_destination(root: &Path) -> Result<Scan> {
    if !root.exists() {
        return Ok(Scan {
            root: root.to_path_buf(),
            entries: BTreeMap::new(),
            total_bytes: 0,
            skipped: Vec::new(),
            errors: Vec::new(),
            placeholders: Vec::new(),
        });
    }
    scan(root).with_context(|| format!("scanning destination {}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn touch(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn walks_camera_tree_and_skips_os_litter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "DCIM/100MSDCF/DSC00001.ARW", 100);
        touch(root, "DCIM/100MSDCF/DSC00002.ARW", 200);
        touch(root, "PRIVATE/M4ROOT/CLIP/C0001.MP4", 300);
        touch(root, "AVF_INFO/AVIN0001.INP", 10);
        touch(root, "Thumbs.db", 5);
        touch(root, "DCIM/._DSC00001.ARW", 5);
        touch(root, "System Volume Information/tracking.log", 999);

        let scan = scan(root).unwrap();
        let keys: Vec<&str> = scan.entries.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "AVF_INFO/AVIN0001.INP",
                "DCIM/100MSDCF/DSC00001.ARW",
                "DCIM/100MSDCF/DSC00002.ARW",
                "PRIVATE/M4ROOT/CLIP/C0001.MP4",
            ],
            "camera directories must survive; only OS litter is dropped"
        );
        assert_eq!(scan.total_bytes, 610);
        assert!(scan.errors.is_empty());
        // Thumbs.db and the AppleDouble sidecar are accounted for, not silently
        // vanished; the System Volume Information *directory* is pruned whole.
        assert_eq!(scan.skipped.len(), 2);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "DCIM/100MSDCF/DSC00001.ARW", 1);
        let scan = scan(dir.path()).unwrap();
        assert!(scan.entries.contains_key("DCIM/100MSDCF/DSC00001.ARW"));
        assert_eq!(
            scan.absolute("DCIM/100MSDCF/DSC00001.ARW"),
            dir.path()
                .join("DCIM")
                .join("100MSDCF")
                .join("DSC00001.ARW")
        );
    }

    #[test]
    fn missing_destination_scans_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan_destination(&dir.path().join("not-created-yet")).unwrap();
        assert_eq!(scan.file_count(), 0);
        assert_eq!(scan.total_bytes, 0);
    }

    #[test]
    fn mtime_tolerates_exfat_two_second_granularity() {
        let a = Mtime {
            secs: 1_757_629_443,
            nanos: 0,
        };
        assert!(a.matches(Mtime {
            secs: 1_757_629_444,
            nanos: 0
        }));
        assert!(a.matches(Mtime {
            secs: 1_757_629_441,
            nanos: 0
        }));
        assert!(!a.matches(Mtime {
            secs: 1_757_629_446,
            nanos: 0
        }));
    }

    #[test]
    fn mtime_formats_as_rfc3339_utc() {
        let m = Mtime {
            secs: 1_757_629_443,
            nanos: 0,
        };
        let s = m.to_rfc3339();
        assert!(
            s.ends_with('Z'),
            "MHL wants a Z-suffixed UTC timestamp, got {s}"
        );
        assert_eq!(s, "2025-09-11T22:24:03Z");
    }

    // --- name hazards ------------------------------------------------------

    /// A clean camera card must produce no hazards at all. If this ever starts
    /// firing, every real offload gets a warning it has to learn to ignore.
    #[test]
    fn an_ordinary_camera_card_has_no_hazards() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "DCIM/100MSDCF/DSC00001.ARW", 10);
        touch(dir.path(), "DCIM/100MSDCF/DSC00002.ARW", 10);
        touch(dir.path(), "PRIVATE/M4ROOT/CLIP/C0001.MP4", 10);
        assert!(hazards(&scan(dir.path()).unwrap()).is_empty());
    }

    /// The bug this exists to prevent: two names differing only by case land on
    /// one NTFS path, one silently overwrites the other, and verification blames
    /// the hardware.
    #[test]
    fn case_only_collisions_are_found_and_grouped() {
        let dir = tempfile::tempdir().unwrap();
        // Created through a case-insensitive filesystem, so build the scan
        // directly -- the point is the classifier, not the walk.
        let scan = scan_of(
            dir.path(),
            &["DCIM/IMG_0001.ARW", "DCIM/img_0001.arw", "DCIM/o.ARW"],
        );
        let h = hazards(&scan);
        assert_eq!(h.len(), 1, "one collision, not one per file: {h:?}");
        match &h[0] {
            Hazard::CaseCollision { lower, rels } => {
                assert_eq!(lower, "dcim/img_0001.arw");
                assert_eq!(rels.len(), 2);
            }
            other => panic!("expected a collision, got {other:?}"),
        }
        assert!(h[0].describe().contains("capitalisation"));
    }

    #[test]
    fn reserved_device_names_are_caught_with_or_without_an_extension() {
        for name in ["NUL", "NUL.ARW", "com1.mp4", "AUX"] {
            let scan = scan_of(Path::new("X:\\"), &[&format!("DCIM/{name}")]);
            let h = hazards(&scan);
            assert!(
                h.iter().any(|x| matches!(x, Hazard::ReservedName { .. })),
                "{name} must be flagged, got {h:?}"
            );
        }
    }

    #[test]
    fn ordinary_names_that_merely_start_with_a_reserved_word_are_left_alone() {
        let scan = scan_of(Path::new("X:\\"), &["DCIM/CONCERT.ARW", "DCIM/NULLS.MP4"]);
        assert!(hazards(&scan).is_empty(), "CONCERT is not CON");
    }

    #[test]
    fn characters_ntfs_rejects_are_named_individually() {
        let scan = scan_of(Path::new("X:\\"), &["DCIM/12:30 take.mov"]);
        let h = hazards(&scan);
        match h.as_slice() {
            [Hazard::IllegalCharacter { ch, .. }] => assert_eq!(*ch, ':'),
            other => panic!("expected one illegal character, got {other:?}"),
        }
        assert!(h[0].describe().contains("cannot contain"));
    }

    /// It is the *component* that must not end in a dot or a space -- a folder
    /// called `Take one ` is as unusable as a file called `notes.`, and both are
    /// silently renamed rather than rejected.
    #[test]
    fn trailing_dots_and_spaces_are_caught() {
        let scan = scan_of(
            Path::new("X:\\"),
            &["DCIM/Take one /DSC1.ARW", "DCIM/notes.", "DCIM/DSC2.ARW"],
        );
        let mut h = hazards(&scan);
        h.retain(|x| matches!(x, Hazard::TrailingDotOrSpace { .. }));
        assert_eq!(h.len(), 2, "{h:?}");
        assert!(h[0].describe().contains("silently renamed"));
    }

    /// A dot inside a name, which is every file ever, must not trip it.
    #[test]
    fn ordinary_extensions_are_not_trailing_dots() {
        let scan = scan_of(Path::new("X:\\"), &["DCIM/DSC00001.ARW", "A.B/C.D.E"]);
        assert!(hazards(&scan).is_empty());
    }

    #[test]
    fn cloud_placeholders_are_reported_from_the_walk() {
        let mut scan = scan_of(Path::new("X:\\"), &["DCIM/DSC00001.ARW"]);
        scan.placeholders.push("DCIM/DSC00001.ARW".into());
        let h = hazards(&scan);
        assert_eq!(h.len(), 1);
        assert!(h[0].describe().contains("cloud placeholder"), "{:?}", h[0]);
    }

    /// A `Scan` assembled by hand, so the classifier can be tested on names the
    /// host filesystem would not let us create.
    fn scan_of(root: &Path, rels: &[&str]) -> Scan {
        let mut entries = BTreeMap::new();
        for rel in rels {
            entries.insert(
                (*rel).to_string(),
                Entry {
                    rel: (*rel).to_string(),
                    size: 1,
                    mtime: Mtime { secs: 0, nanos: 0 },
                },
            );
        }
        Scan {
            root: root.to_path_buf(),
            entries,
            total_bytes: rels.len() as u64,
            skipped: Vec::new(),
            errors: Vec::new(),
            placeholders: Vec::new(),
        }
    }
}
