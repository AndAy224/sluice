//! Remembered setup, keyed by volume serial rather than by drive letter.
//!
//! Two things at once, and the second is the reason this is in the engine and
//! not the UI:
//!
//! * **The 11pm goal.** The design asks for zero decisions at 11pm. Re-picking
//!   four paths every night is four decisions.
//! * **Letter-swapping stops mattering.** §7 is built around two identical LaCie
//!   Rugged drives trading `D:` and `G:` between plug-ins. The picker makes that
//!   *visible*; binding the remembered setup to the serial makes it *structural*.
//!   A remembered drive is found wherever Windows put it this time, and a drive
//!   that is not connected says so by name instead of silently resolving to
//!   whatever now holds its old letter.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::history::data_dir;
use super::win::{self, MountedVolume};

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// One remembered slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMemory {
    /// The volume this slot pointed at. The letter is deliberately *not* the
    /// identity -- it is the one part that changes between plug-ins.
    pub serial: u32,
    /// Shown when the drive is not connected, so the message can name it.
    pub label: String,
    /// Anything below the volume root, e.g. `Shoot`. Empty for a drive root.
    pub subpath: String,
}

impl SlotMemory {
    pub fn serial_hex(&self) -> String {
        format!("{:08X}", self.serial)
    }

    /// Rebuild the path from whatever letter this volume has now.
    pub fn resolve(&self, mounted: &[MountedVolume]) -> Option<PathBuf> {
        let vol = mounted.iter().find(|v| v.info.serial == self.serial)?;
        let mut p = PathBuf::from(&vol.info.root);
        if !self.subpath.is_empty() {
            for part in self.subpath.split('/') {
                p.push(part);
            }
        }
        Some(p)
    }

    /// What to say when the drive is nowhere to be found.
    pub fn absent_note(&self) -> String {
        format!(
            "last seen as {} ({}) — not connected",
            if self.label.is_empty() {
                "an unlabelled volume"
            } else {
                &self.label
            },
            self.serial_hex()
        )
    }
}

/// The remembered setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version of this file.
    ///
    /// Without it, a config written by a newer sluice and read by an older one
    /// failed to parse and was silently treated as "no memory yet" -- the setup
    /// quietly reset with nothing said. A version lets the older binary say so
    /// instead of pretending the file was never there.
    #[serde(default = "one")]
    pub version: u32,
    /// Keyed by slot: `C1`, `C2`, `A`, `B`, `C`.
    #[serde(default)]
    pub slots: BTreeMap<String, SlotMemory>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub trace: bool,
}

/// The schema version this build writes and understands.
pub const SCHEMA: u32 = 1;

/// A config written by a newer sluice than this build understands.
///
/// A distinct type rather than a message, so a caller can tell this apart from
/// "no config yet" without matching on prose -- which matters, because the two
/// demand opposite behaviour: one should be quietly replaced, the other must
/// not be touched.
#[derive(Debug, Clone, Copy)]
pub struct FutureSchema {
    pub found: u32,
    pub understood: u32,
}

impl std::fmt::Display for FutureSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "written by a newer version of sluice (schema {} vs {})",
            self.found, self.understood
        )
    }
}

impl std::error::Error for FutureSchema {}

impl Default for Config {
    /// Hand-written rather than derived: a derived `Default` would stamp version
    /// 0 on every fresh config, which is a version this program never wrote.
    fn default() -> Self {
        Self {
            version: SCHEMA,
            slots: BTreeMap::new(),
            label: String::new(),
            trace: false,
        }
    }
}

fn one() -> u32 {
    1
}

impl Config {
    /// Load, treating anything unreadable as "no memory yet".
    ///
    /// A corrupt config must never stop an offload: the worst it should cost is
    /// re-picking the drives once.
    pub fn load() -> Self {
        Self::load_from(&config_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cfg.version > SCHEMA {
            return Err(anyhow::Error::new(FutureSchema {
                found: cfg.version,
                understood: SCHEMA,
            })
            .context(format!(
                "{}: refusing to read it rather than silently resetting your setup -- upgrade \
                 sluice, or delete the file to start fresh",
                path.display()
            )));
        }
        Ok(cfg)
    }

    /// Load, and say so when the file was *refused* rather than simply absent.
    ///
    /// [`load`](Self::load) collapses "no config yet", "corrupt" and "written by
    /// a newer sluice" into one silent default. Only the third is worth saying
    /// out loud -- and stamping a version achieved nothing at all while the one
    /// production caller discarded the error it was there to raise.
    pub fn load_reporting() -> (Self, Option<String>) {
        Self::load_reporting_from(&config_path())
    }

    /// The path-taking half, so a test can exercise the decision the shipped
    /// binary makes without writing to the user's real `%APPDATA%`.
    pub fn load_reporting_from(path: &Path) -> (Self, Option<String>) {
        match Self::load_from(path) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                // Only a refusal is worth saying out loud. "No config yet" and
                // "corrupt" both cost one re-pick and nothing more.
                let refused = e.downcast_ref::<FutureSchema>().is_some();
                (Self::default(), refused.then(|| format!("{e:#}")))
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Remember what a slot currently points at.
    ///
    /// Silently does nothing when the path has no identifiable volume: a memory
    /// that cannot be resolved later is worse than none.
    pub fn remember(&mut self, slot: &str, path: &Path) {
        if path.as_os_str().is_empty() {
            self.slots.remove(slot);
            return;
        }
        let Ok(info) = win::volume_info(path) else {
            return;
        };
        self.slots.insert(
            slot.to_string(),
            SlotMemory {
                serial: info.serial,
                label: info.label.clone(),
                subpath: subpath_of(path, &info.root),
            },
        );
    }

    /// What each slot resolves to right now, and what could not be found.
    ///
    /// Returns `(slot, resolved path)` for everything present, and
    /// `(slot, note)` for everything remembered but absent.
    pub fn resolve_all(
        &self,
        mounted: &[MountedVolume],
    ) -> (BTreeMap<String, PathBuf>, Vec<(String, String)>) {
        let mut found = BTreeMap::new();
        let mut absent = Vec::new();
        for (slot, mem) in &self.slots {
            match mem.resolve(mounted) {
                Some(p) => {
                    found.insert(slot.clone(), p);
                }
                None => absent.push((slot.clone(), mem.absent_note())),
            }
        }
        (found, absent)
    }
}

/// The part of `path` below its volume root, forward-slashed.
fn subpath_of(path: &Path, root: &str) -> String {
    let full = path.to_string_lossy();
    let trimmed = full
        .get(root.len()..)
        .unwrap_or("")
        .trim_start_matches(['\\', '/'])
        .trim_end_matches(['\\', '/']);
    trimmed.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_config_carries_the_current_schema() {
        assert_eq!(Config::default().version, SCHEMA);
    }

    /// A config from a newer sluice used to fail to parse and be treated as "no
    /// memory yet", so the setup reset with nothing said. Saying so is the whole
    /// point of stamping a version.
    #[test]
    fn a_config_from_the_future_is_refused_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            format!(
                r#"{{"version":{},"slots":{{}},"label":"x","trace":false}}"#,
                SCHEMA + 1
            ),
        )
        .unwrap();
        let err = Config::load_from(&path).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("newer version of sluice"), "{msg}");
    }

    /// The refusal has to reach the surface the operator is looking at.
    ///
    /// It did not: the only production loader was `load()`, which collapses "no
    /// config yet", "corrupt" and "written by a newer sluice" into one silent
    /// default -- so the version stamp changed nothing that anyone could see,
    /// and the next Offload overwrote the very file it had declined to read.
    #[test]
    fn a_refusal_is_reported_and_a_merely_missing_config_is_not() {
        let dir = tempfile::tempdir().unwrap();

        let future = dir.path().join("future.json");
        fs::write(
            &future,
            format!(r#"{{"version":{},"slots":{{}}}}"#, SCHEMA + 1),
        )
        .unwrap();
        let (cfg, refused) = Config::load_reporting_from(&future);
        assert!(cfg.slots.is_empty());
        let msg = refused.expect("a future config must be reported, not swallowed");
        assert!(msg.contains("newer version of sluice"), "{msg}");

        // The two that must stay quiet: nothing to say, and nothing worth
        // stopping an offload for.
        assert_eq!(
            Config::load_reporting_from(&dir.path().join("nope.json")).1,
            None
        );
        let corrupt = dir.path().join("corrupt.json");
        fs::write(&corrupt, b"{ this is not json").unwrap();
        assert_eq!(Config::load_reporting_from(&corrupt).1, None);

        // And a config this build understands is returned, not defaulted.
        let good = dir.path().join("good.json");
        fs::write(&good, r#"{"version":1,"slots":{},"label":"shoot"}"#).unwrap();
        let (cfg, refused) = Config::load_reporting_from(&good);
        assert_eq!(refused, None);
        assert_eq!(cfg.label, "shoot");
    }

    /// A config with no version at all is one this build wrote before versions
    /// existed. Read it, do not refuse it.
    #[test]
    fn a_config_without_a_version_is_assumed_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"slots":{},"label":"shoot","trace":false}"#).unwrap();
        let cfg = Config::load_from(&path).expect("must read");
        assert_eq!(cfg.version, SCHEMA);
        assert_eq!(cfg.label, "shoot");
    }
    use crate::engine::win::{DriveType, VolumeInfo};

    fn volume(root: &str, serial: u32, label: &str) -> MountedVolume {
        MountedVolume {
            info: VolumeInfo {
                root: root.into(),
                label: label.into(),
                serial,
                filesystem: "exFAT".into(),
                sector_size: 4096,
                guid: None,
                device_number: Some(serial),
            },
            drive_type: DriveType::Fixed,
            free_bytes: 3_610_000_000_000,
            total_bytes: 4_000_000_000_000,
        }
    }

    fn memory(serial: u32, label: &str, subpath: &str) -> SlotMemory {
        SlotMemory {
            serial,
            label: label.into(),
            subpath: subpath.into(),
        }
    }

    /// The whole point: the drive moved from D: to G: and the setup still finds
    /// it, because the memory is of the volume and not of the letter.
    #[test]
    fn a_remembered_drive_is_found_after_a_letter_swap() {
        let mem = memory(0x3A2F0D18, "MT-A", "");
        assert_eq!(
            mem.resolve(&[volume("D:\\", 0x3A2F0D18, "MT-A")]),
            Some(PathBuf::from("D:\\"))
        );
        // Same drive, different letter, next time it is plugged in.
        assert_eq!(
            mem.resolve(&[volume("G:\\", 0x3A2F0D18, "MT-A")]),
            Some(PathBuf::from("G:\\"))
        );
    }

    /// And the converse, which is the safety half: another drive now holding
    /// the old letter must not be picked up by mistake.
    #[test]
    fn a_different_drive_on_the_old_letter_is_not_mistaken_for_it() {
        let mem = memory(0x3A2F0D18, "MT-A", "");
        // D: is now the *other* LaCie.
        assert_eq!(mem.resolve(&[volume("D:\\", 0x7C190B4E, "MT-B")]), None);
        assert!(mem.absent_note().contains("MT-A"));
        assert!(mem.absent_note().contains("3A2F0D18"));
        assert!(mem.absent_note().contains("not connected"));
    }

    #[test]
    fn a_subfolder_below_the_volume_root_is_remembered_and_rebuilt() {
        let mem = memory(0x3A2F0D18, "MT-A", "Shoot/2026");
        assert_eq!(
            mem.resolve(&[volume("G:\\", 0x3A2F0D18, "MT-A")]),
            Some(PathBuf::from("G:\\").join("Shoot").join("2026"))
        );
    }

    #[test]
    fn subpath_extraction_handles_roots_and_nesting() {
        assert_eq!(subpath_of(Path::new("D:\\"), "D:\\"), "");
        assert_eq!(subpath_of(Path::new("D:\\Shoot"), "D:\\"), "Shoot");
        assert_eq!(
            subpath_of(Path::new("D:\\Shoot\\2026\\"), "D:\\"),
            "Shoot/2026"
        );
    }

    #[test]
    fn resolve_all_separates_what_is_present_from_what_is_missing() {
        let mut cfg = Config::default();
        cfg.slots.insert("A".into(), memory(0x3A2F0D18, "MT-A", ""));
        cfg.slots.insert("B".into(), memory(0x7C190B4E, "MT-B", ""));

        let (found, absent) = cfg.resolve_all(&[volume("G:\\", 0x3A2F0D18, "MT-A")]);
        assert_eq!(found.get("A"), Some(&PathBuf::from("G:\\")));
        assert!(!found.contains_key("B"));
        assert_eq!(absent.len(), 1);
        assert_eq!(absent[0].0, "B");
        assert!(absent[0].1.contains("MT-B"));
    }

    #[test]
    fn a_config_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = Config {
            label: "shoot-01".into(),
            trace: true,
            ..Default::default()
        };
        cfg.slots
            .insert("C1".into(), memory(0x1F3B9C04, "SONY_A", ""));
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), cfg);
    }

    /// A corrupt config must cost re-picking the drives once, never an offload.
    #[test]
    fn a_corrupt_config_reads_as_no_memory_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(Config::load_from(&path).is_err());
        // The public entry point swallows it.
        assert_eq!(
            Config::load_from(&path).unwrap_or_default(),
            Config::default()
        );
    }

    #[test]
    fn a_missing_config_is_simply_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load_from(&dir.path().join("nope.json")).unwrap_or_default(),
            Config::default()
        );
    }
}
