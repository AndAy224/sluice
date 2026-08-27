//! The durable record: what happened, and which cards were erased afterwards.
//!
//! The design declares a `formatted_after` field and never fills it, which
//! leaves a hole exactly where it matters. If a file turns up corrupt at home in
//! January, the question is *which cards were erased after which session*, and
//! without an answer the trail stops at the night the tool said SAFE TO FORMAT.
//!
//! This is an append-only JSONL on the laptop, outliving any one session folder.
//! It also accumulates per-device counters, which gives §15's card-health
//! tracking for free: a card that produced a twin mismatch last month should
//! not be silently trusted in October.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where sluice keeps state that outlives a session.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sluice")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

/// One device as it appeared in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// `C1`, `C2`, `A`, `B`, `C`.
    pub slot: String,
    pub serial: u32,
    pub label: String,
}

impl DeviceRecord {
    pub fn serial_hex(&self) -> String {
        format!("{:08X}", self.serial)
    }
}

/// An append-only line in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// A completed offload, whatever its verdict.
    Session {
        at: DateTime<Utc>,
        session: String,
        verdict: String,
        devices: Vec<DeviceRecord>,
        /// Files that needed a retry, per slot.
        retries: BTreeMap<String, usize>,
        /// Serials of cards a twin mismatch pointed at.
        suspect_cards: Vec<u32>,
        failures: usize,
    },
    /// Cards actually erased after a session. The other end of the trail.
    Format {
        at: DateTime<Utc>,
        session: String,
        cards: Vec<DeviceRecord>,
        note: String,
    },
}

/// Append one line to the machine's history.
pub fn append(entry: &Entry) -> Result<()> {
    append_to(&history_path(), entry)
}

/// Append one line to a named history file, creating it and its directory.
///
/// The path is a parameter rather than always the machine's own file because
/// the integration suite drives `run_job` for real, and a test that records
/// twenty fake sessions against the developer's C: drive does more than make a
/// mess: the per-device counters are what preflight warns from, so the next real
/// offload opens by announcing that a card it has never seen is suspect. A
/// warning system trained on test data is a warning system nobody believes.
pub fn append_to(path: &Path, entry: &Entry) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let line = serde_json::to_string(entry).context("serialising a history entry")?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("appending to {}", path.display()))?;
    // Flushed to the device: this record exists to survive the thing that went
    // wrong, so a buffered write helps nobody -- and a sync that fails silently
    // helps even less, because the caller would go on believing the record is
    // durable.
    f.sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Read the machine's history.
pub fn read_all() -> Result<Vec<Entry>> {
    read_from(&history_path())
}

/// Read a named history file. A malformed line is skipped rather than fatal --
/// a truncated tail from a power cut must not cost the rest of the record.
pub fn read_from(path: &Path) -> Result<Vec<Entry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Entry>(&l).ok())
        .collect())
}

/// What the history knows about one physical device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeviceSummary {
    pub serial: u32,
    pub label: String,
    pub sessions: usize,
    pub retries: usize,
    /// Times a twin mismatch pointed at this card.
    pub twin_mismatches: usize,
    pub formatted: usize,
    pub last_seen: Option<DateTime<Utc>>,
}

impl DeviceSummary {
    pub fn serial_hex(&self) -> String {
        format!("{:08X}", self.serial)
    }

    /// Whether this device has misbehaved before.
    ///
    /// Deliberately triggered by a *single* prior twin mismatch. The design's
    /// rule is to retire a suspect card for the rest of the shoot; a card that
    /// disagreed with its twin once has already earned that, and a second
    /// chance is not something to grant silently.
    pub fn is_suspect(&self) -> bool {
        self.twin_mismatches > 0 || self.retries > 5
    }

    /// The warning preflight prints when this device shows up again.
    pub fn warning(&self) -> Option<String> {
        if !self.is_suspect() {
            return None;
        }
        let when = self
            .last_seen
            .map(super::telemetry::local_date)
            .unwrap_or_else(|| "an earlier session".into());
        let mut parts = Vec::new();
        if self.twin_mismatches > 0 {
            parts.push(format!("{} twin mismatch(es)", self.twin_mismatches));
        }
        if self.retries > 5 {
            parts.push(format!("{} retried writes", self.retries));
        }
        Some(format!(
            "{} ({}) has a history: {} -- last seen {when}. The design says retire a \
             suspect card, not give it another chance.",
            if self.label.is_empty() {
                "this device"
            } else {
                &self.label
            },
            self.serial_hex(),
            parts.join(", ")
        ))
    }
}

/// Roll the history up per device serial.
pub fn summarise(entries: &[Entry]) -> BTreeMap<u32, DeviceSummary> {
    let mut out: BTreeMap<u32, DeviceSummary> = BTreeMap::new();
    for entry in entries {
        match entry {
            Entry::Session {
                at,
                devices,
                retries,
                suspect_cards,
                ..
            } => {
                // One device can fill two slots -- two folders on one volume, or
                // a destination that shares a disk with a card -- so a session
                // is counted once per *serial*, not once per slot.
                let mut counted: BTreeMap<u32, ()> = BTreeMap::new();
                for d in devices {
                    let s = out.entry(d.serial).or_default();
                    s.serial = d.serial;
                    if !d.label.is_empty() {
                        s.label = d.label.clone();
                    }
                    if counted.insert(d.serial, ()).is_none() {
                        s.sessions += 1;
                    }
                    s.retries += retries.get(&d.slot).copied().unwrap_or(0);
                    s.last_seen = Some(match s.last_seen {
                        Some(prev) if prev > *at => prev,
                        _ => *at,
                    });
                }
                for serial in suspect_cards {
                    let s = out.entry(*serial).or_default();
                    s.serial = *serial;
                    s.twin_mismatches += 1;
                }
            }
            Entry::Format { cards, .. } => {
                for d in cards {
                    let s = out.entry(d.serial).or_default();
                    s.serial = d.serial;
                    if !d.label.is_empty() {
                        s.label = d.label.clone();
                    }
                    s.formatted += 1;
                }
            }
        }
    }
    out
}

/// Record that cards were erased after a session, and patch the session logs
/// that sit beside the copies.
/// `session_json` is the list of files the run actually wrote, not a directory
/// list to rebuild names from: a run whose session id collided with one already
/// on the drive writes under a disambiguated name, and rebuilding would address
/// the earlier run's record instead of this one's.
pub fn record_format(
    session: &str,
    session_json: &[PathBuf],
    cards: Vec<DeviceRecord>,
    note: &str,
) -> Result<()> {
    let at = Utc::now();
    let summary = format!(
        "{} — erased {}{}",
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        cards
            .iter()
            .map(|c| format!("{} ({})", c.label, c.serial_hex()))
            .collect::<Vec<_>>()
            .join(", "),
        if note.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", note.trim())
        }
    );

    append(&Entry::Format {
        at,
        session: session.to_string(),
        cards,
        note: note.trim().to_string(),
    })?;

    // The session JSON sits beside the copies, so the answer travels with the
    // drive rather than only living on the laptop.
    for path in session_json {
        if let Err(e) = patch_formatted_after(path, &summary) {
            // Best effort: the durable record is the history file, and a
            // destination that has already been unplugged must not lose it.
            eprintln!("could not update {}: {e:#}", path.display());
        }
    }
    Ok(())
}

/// Collect everything about one session into a folder you can hand over.
///
/// §10 calls for a zip. A folder is the same thing without a zip dependency,
/// and Explorer will compress it in one right-click: what matters is that the
/// JSONL, both manifests, the session records, the device history and the
/// system details end up in one place rather than being gathered by hand from
/// three locations at a moment when something has already gone wrong.
pub fn export_bundle(
    session: &str,
    session_dirs: &[PathBuf],
    log_path: &std::path::Path,
    out_dir: &std::path::Path,
) -> Result<PathBuf> {
    let bundle = out_dir.join(format!("sluice-bundle_{session}"));
    fs::create_dir_all(&bundle).with_context(|| format!("creating {}", bundle.display()))?;

    // Gathered, then reported. An earlier cut counted every *attempt* and
    // swallowed every failure, so `system.txt` could claim four files in a
    // bundle that held two -- a false statement in the one file whose job is to
    // say what the bundle contains.
    let mut gathered: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut take = |from: &std::path::Path, to: PathBuf| {
        if !from.is_file() {
            return;
        }
        let name = to
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match fs::copy(from, &to) {
            Ok(_) => gathered.push(name),
            Err(e) => failed.push(format!("{}: {e}", from.display())),
        }
    };

    take(
        log_path,
        bundle.join(log_path.file_name().unwrap_or_default()),
    );
    // A panic writes here. It is the single most useful thing in the bundle when
    // there is one, and it is the reason the crash log lives beside the session
    // log rather than somewhere tidier.
    if let Some(logs) = log_path.parent() {
        let crash = logs.join("crash.log");
        take(&crash, bundle.join("crash.log"));
    }

    for (i, dir) in session_dirs.iter().enumerate() {
        // Both dialects: the v1 manifest sluice re-verifies, and the ASC MHL
        // directory other tools read. Leaving the ASC files out would hand
        // somebody a bundle missing half the evidence.
        let mut wanted = vec![
            super::mhl::mhl_path(dir, session),
            super::mhl::session_json_path(dir, session),
        ];
        if let Ok(entries) = fs::read_dir(super::mhl::ascmhl_dir(dir)) {
            wanted.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_file()));
        }
        for from in wanted {
            // Prefixed by destination index: every destination writes the same
            // filenames, and a bundle that overwrites them keeps only the last.
            let dest = bundle.join(format!(
                "dest{}_{}",
                i,
                from.file_name().unwrap_or_default().to_string_lossy()
            ));
            take(&from, dest);
        }
    }

    let hist = history_path();
    take(&hist, bundle.join("history.jsonl"));

    let mut system = format!(
        "tool:      {}\nsession:   {session}\nexported:  {}\nos:        {}\nhost:      {}\nuser:      {}\nfiles:     {}\n",
        super::mhl::tool(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        std::env::consts::OS,
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
        std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()),
        gathered.len(),
    );
    system.push_str("\ngathered:\n");
    for name in &gathered {
        system.push_str(&format!("  {name}\n"));
    }
    // Named, not hidden. A drive unplugged between the run and the export leaves
    // a manifest behind, and the person reading this bundle has to know that
    // rather than assume the gap means nothing was there.
    if !failed.is_empty() {
        system.push_str("\nCOULD NOT BE GATHERED:\n");
        for why in &failed {
            system.push_str(&format!("  {why}\n"));
        }
    }
    fs::write(bundle.join("system.txt"), system)
        .with_context(|| format!("writing {}", bundle.join("system.txt").display()))?;

    Ok(bundle)
}

fn patch_formatted_after(path: &std::path::Path, summary: &str) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    doc["formatted_after"] = serde_json::Value::String(summary.to_string());
    fs::write(path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(slot: &str, serial: u32, label: &str) -> DeviceRecord {
        DeviceRecord {
            slot: slot.into(),
            serial,
            label: label.into(),
        }
    }

    fn session(at: &str, suspects: Vec<u32>, retries: &[(&str, usize)]) -> Entry {
        Entry::Session {
            at: DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&Utc),
            session: "20260314-221403".into(),
            verdict: "SafeToFormat".into(),
            devices: vec![
                dev("C1", 0x1F3B9C04, "SONY_A"),
                dev("C2", 0x90C4A711, "SONY_B"),
                dev("A", 0x3A2F0D18, "MT-A"),
            ],
            retries: retries.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            suspect_cards: suspects,
            failures: 0,
        }
    }

    #[test]
    fn a_clean_history_flags_nobody() {
        let s = summarise(&[session("2026-03-14T22:14:03Z", vec![], &[])]);
        assert_eq!(s.len(), 3);
        assert!(s.values().all(|d| !d.is_suspect()));
        assert!(s[&0x1F3B9C04].warning().is_none());
        assert_eq!(s[&0x1F3B9C04].label, "SONY_A");
        assert_eq!(s[&0x1F3B9C04].sessions, 1);
    }

    /// One twin mismatch is enough. The design says retire a suspect card, and a
    /// second chance is not something to grant silently.
    #[test]
    fn a_single_twin_mismatch_marks_a_card_for_good() {
        let s = summarise(&[
            session("2026-03-14T22:14:03Z", vec![0x90C4A711], &[]),
            session("2026-03-15T22:14:03Z", vec![], &[]),
        ]);
        let card2 = &s[&0x90C4A711];
        assert!(card2.is_suspect());
        assert_eq!(card2.twin_mismatches, 1);
        let w = card2.warning().unwrap();
        assert!(w.contains("90C4A711"), "{w}");
        assert!(w.contains("twin mismatch"), "{w}");
        assert!(w.contains("retire"), "{w}");
        // The other card in the same sessions stays clean.
        assert!(!s[&0x1F3B9C04].is_suspect());
    }

    /// Two folders on one volume, or a destination sharing a disk with a card,
    /// put one serial in several slots. That is one session for that device,
    /// not three.
    #[test]
    fn one_device_filling_several_slots_counts_as_one_session() {
        let entry = Entry::Session {
            at: DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
                .unwrap()
                .with_timezone(&Utc),
            session: "s".into(),
            verdict: "Failed".into(),
            devices: vec![
                dev("C1", 0x30195459, "TEMP"),
                dev("C2", 0x30195459, "TEMP"),
                dev("A", 0x30195459, "TEMP"),
            ],
            retries: BTreeMap::new(),
            suspect_cards: vec![],
            failures: 0,
        };
        assert_eq!(summarise(&[entry])[&0x30195459].sessions, 1);
    }

    #[test]
    fn retries_accumulate_across_sessions_and_eventually_flag_a_drive() {
        let s = summarise(&[
            session("2026-03-14T22:14:03Z", vec![], &[("A", 3)]),
            session("2026-03-15T22:14:03Z", vec![], &[("A", 4)]),
        ]);
        let a = &s[&0x3A2F0D18];
        assert_eq!(a.retries, 7);
        assert!(
            a.is_suspect(),
            "seven retried writes over two nights is a pattern"
        );
        assert!(a.warning().unwrap().contains("retried writes"));
    }

    #[test]
    fn last_seen_tracks_the_most_recent_session_whatever_the_order() {
        let s = summarise(&[
            session("2026-03-15T22:14:03Z", vec![], &[]),
            session("2026-03-14T22:14:03Z", vec![], &[]),
        ]);
        assert_eq!(
            s[&0x1F3B9C04]
                .last_seen
                .unwrap()
                .format("%Y-%m-%d")
                .to_string(),
            "2026-03-15"
        );
    }

    #[test]
    fn a_format_entry_is_counted_against_the_cards_it_erased() {
        let s = summarise(&[
            session("2026-03-14T22:14:03Z", vec![], &[]),
            Entry::Format {
                at: DateTime::parse_from_rfc3339("2026-03-15T08:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                session: "20260314-221403".into(),
                cards: vec![dev("C1", 0x1F3B9C04, "SONY_A")],
                note: "next morning, spot-checked both drives".into(),
            },
        ]);
        assert_eq!(s[&0x1F3B9C04].formatted, 1);
        assert_eq!(s[&0x90C4A711].formatted, 0);
    }

    /// A power cut can truncate the last line. That must not cost the rest.
    #[test]
    fn a_truncated_tail_does_not_destroy_the_record() {
        let good = serde_json::to_string(&session("2026-03-14T22:14:03Z", vec![], &[])).unwrap();
        let text = format!("{good}\n{{\"kind\":\"session\",\"at\":\"2026-0");
        let parsed: Vec<Entry> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .collect();
        assert_eq!(parsed.len(), 1, "the intact line survives the broken one");
    }

    /// The format confirmation must land on the record of the run that was
    /// actually erased.
    ///
    /// It is addressed by the path the run wrote, never rebuilt from the
    /// session id — those two differ exactly when an earlier run's files are
    /// already in the folder, which is precisely when getting it wrong is
    /// worst: the earlier session's record would claim these cards were erased
    /// after *it*, which is how you reconstruct what happened to a frame that
    /// turns up corrupt months later.
    #[test]
    fn only_the_named_session_record_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let earlier = dir.path().join("sluice_20260314-2214.json");
        let mine = dir.path().join("sluice_20260314-2214-2.json");
        fs::write(&earlier, r#"{"session":"earlier"}"#).unwrap();
        fs::write(&mine, r#"{"session":"mine"}"#).unwrap();

        patch_formatted_after(&mine, "erased CARD 1 (3A2F0D18)").unwrap();

        assert!(fs::read_to_string(&mine).unwrap().contains("3A2F0D18"));
        assert!(
            !fs::read_to_string(&earlier).unwrap().contains("3A2F0D18"),
            "the earlier run's record must not be stamped with this run's cards"
        );
    }

    /// One folder holding everything about a session, rather than three
    /// locations to gather from at a moment when something has gone wrong.
    #[test]
    fn a_bundle_gathers_the_log_the_manifests_and_the_system_details() {
        let dir = tempfile::tempdir().unwrap();
        let session = "20260314-221403";

        let log = dir.path().join(format!("sluice_{session}.jsonl"));
        fs::write(
            &log, "{}
",
        )
        .unwrap();

        let mut dirs = Vec::new();
        for d in ["destA", "destB"] {
            let sd = dir.path().join(d);
            fs::create_dir_all(&sd).unwrap();
            fs::write(super::super::mhl::mhl_path(&sd, session), "<hashlist/>").unwrap();
            fs::write(super::super::mhl::session_json_path(&sd, session), "{}").unwrap();
            dirs.push(sd);
        }

        let out = dir.path().join("out");
        let bundle = export_bundle(session, &dirs, &log, &out).unwrap();

        let names: Vec<String> = fs::read_dir(&bundle)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();

        assert!(names.iter().any(|n| n.ends_with(".jsonl")), "{names:?}");
        assert!(names.iter().any(|n| n.starts_with("dest0_")), "{names:?}");
        // Both destinations' manifests share a filename, so they must be
        // prefixed or one would overwrite the other.
        assert!(names.iter().any(|n| n.starts_with("dest1_")), "{names:?}");
        assert_eq!(
            names.iter().filter(|n| n.ends_with(".mhl")).count(),
            2,
            "both manifests must survive: {names:?}"
        );
        assert!(names.iter().any(|n| n == "system.txt"), "{names:?}");

        let sys = fs::read_to_string(bundle.join("system.txt")).unwrap();
        assert!(sys.contains(session));
        assert!(sys.contains("sluice"));
    }

    #[test]
    fn entries_round_trip_through_json() {
        for entry in [
            session("2026-03-14T22:14:03Z", vec![0x90C4A711], &[("A", 2)]),
            Entry::Format {
                at: Utc::now(),
                session: "s".into(),
                cards: vec![dev("C1", 1, "X")],
                note: String::new(),
            },
        ] {
            let text = serde_json::to_string(&entry).unwrap();
            assert_eq!(serde_json::from_str::<Entry>(&text).unwrap(), entry);
        }
    }

    // --- the bundle says what it holds -------------------------------------

    /// A bundle is handed to somebody else at the moment something has already
    /// gone wrong. `system.txt` is the file that tells them what they have, so
    /// it must count what actually landed rather than what was attempted -- an
    /// earlier cut incremented on every try and swallowed every failure.
    #[test]
    fn the_bundle_counts_what_it_actually_gathered() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let session_dir = dir.path().join("dest").join("2026-03-14_shoot");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(crate::engine::mhl::ascmhl_dir(&session_dir)).unwrap();

        let log = logs.join("sluice_20260314-2214.jsonl");
        fs::write(&log, b"{}").unwrap();
        // A panic writes here, and it is the single most useful thing in the
        // bundle when there is one.
        fs::write(logs.join("crash.log"), b"panicked at ...").unwrap();
        fs::write(
            crate::engine::mhl::mhl_path(&session_dir, "20260314-2214"),
            b"<x/>",
        )
        .unwrap();
        fs::write(
            crate::engine::mhl::ascmhl_dir(&session_dir).join("0001_20260314-2214.mhl"),
            b"<x/>",
        )
        .unwrap();
        fs::write(
            crate::engine::mhl::ascmhl_dir(&session_dir).join("ascmhl_chain.xml"),
            b"<x/>",
        )
        .unwrap();

        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let bundle = export_bundle(
            "20260314-2214",
            std::slice::from_ref(&session_dir),
            &log,
            &out,
        )
        .unwrap();

        let names: Vec<String> = fs::read_dir(&bundle)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "crash.log"), "{names:?}");
        assert!(
            names.iter().any(|n| n.contains("ascmhl_chain.xml")),
            "the ASC manifests are half the evidence: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("0001_20260314-2214.mhl")),
            "{names:?}"
        );

        let system = fs::read_to_string(bundle.join("system.txt")).unwrap();
        assert_eq!(
            reported_count(&system),
            gathered_count(&bundle),
            "the count must match what is on disk: {system}"
        );
        assert!(system.contains("gathered:"), "{system}");
        assert!(
            !system.contains("COULD NOT BE GATHERED"),
            "nothing failed here: {system}"
        );
    }

    /// A destination unplugged between the run and the export leaves a manifest
    /// behind. The bundle must still be produced -- half the evidence beats none
    /// -- and its count must still match what is actually in it.
    ///
    /// Asserted as an invariant rather than against a fixed number, because
    /// `export_bundle` also picks up the machine's own history file and this
    /// test has no business caring whether the developer has ever run sluice.
    #[test]
    fn a_bundle_with_nothing_to_gather_still_reports_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let bundle = export_bundle(
            "20260314-2214",
            &[dir.path().join("unplugged")],
            &dir.path().join("no-such.jsonl"),
            &out,
        )
        .unwrap();
        let system = fs::read_to_string(bundle.join("system.txt")).unwrap();
        assert_eq!(reported_count(&system), gathered_count(&bundle), "{system}");
        assert!(
            !system.contains("COULD NOT BE GATHERED"),
            "absent is not the same as failed: {system}"
        );
    }

    /// Files in the bundle, not counting the manifest of the bundle itself.
    fn gathered_count(bundle: &std::path::Path) -> usize {
        fs::read_dir(bundle)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() != "system.txt")
            .count()
    }

    fn reported_count(system: &str) -> usize {
        system
            .lines()
            .find_map(|l| l.strip_prefix("files:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("system.txt must report a file count")
    }
}
