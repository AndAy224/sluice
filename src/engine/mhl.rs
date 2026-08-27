//! MHL v1 emitter and the JSON session log.
//!
//! The manifest is the part of this program designed to outlive it. It is plain
//! XML in a published format, so another tool -- or future-you without this
//! binary -- can re-verify a drive months later. That is also why the hash is
//! xxHash64 rather than something stronger: MHL compatibility, not cryptography.
//!
//! Manifests are written **last**, after verification passes, so their presence
//! on a destination is itself the success signal.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::scan::Mtime;
use super::telemetry::{rfc3339, DeviceInfo};
use super::unbuffered::hex64;
use super::verdict::VerdictReport;
use super::verify::FileVerdict;

/// The tool identity written into every manifest and session log.
///
/// Carries the commit, not just the version: two builds of `0.1.0` can differ,
/// and a manifest found on a drive in two years should name the code that
/// vouched for it rather than a version number that was never bumped.
pub fn tool() -> String {
    super::build_info::stamp()
}

/// `<creatorinfo>`.
#[derive(Debug, Clone, Serialize)]
pub struct CreatorInfo {
    pub name: String,
    pub hostname: String,
    pub tool: String,
    pub start: DateTime<Utc>,
    pub finish: DateTime<Utc>,
}

impl CreatorInfo {
    pub fn new(start: DateTime<Utc>, finish: DateTime<Utc>) -> Self {
        Self {
            name: std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()),
            hostname: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
            tool: tool(),
            start,
            finish,
        }
    }
}

/// One `<hash>` block.
#[derive(Debug, Clone, Serialize)]
pub struct HashEntry {
    /// Forward-slash path relative to the directory holding the `.mhl`.
    pub rel: String,
    pub size: u64,
    pub mtime: Mtime,
    pub hash: u64,
    pub hashed_at: DateTime<Utc>,
}

/// Escape text for an XML element body or attribute.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render an MHL v1 document.
///
/// `version="1.1"` is correct despite the "v1" name: it is the schema revision
/// the ASC MHL v1 format carries.
pub fn render_mhl(creator: &CreatorInfo, entries: &[HashEntry]) -> String {
    let mut out = String::with_capacity(256 + entries.len() * 320);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<hashlist version=\"1.1\">\n");
    out.push_str("  <creatorinfo>\n");
    out.push_str(&format!("    <name>{}</name>\n", xml_escape(&creator.name)));
    out.push_str(&format!(
        "    <hostname>{}</hostname>\n",
        xml_escape(&creator.hostname)
    ));
    out.push_str(&format!("    <tool>{}</tool>\n", xml_escape(&creator.tool)));
    out.push_str(&format!(
        "    <startdate>{}</startdate>\n",
        rfc3339(creator.start)
    ));
    out.push_str(&format!(
        "    <finishdate>{}</finishdate>\n",
        rfc3339(creator.finish)
    ));
    out.push_str("  </creatorinfo>\n");

    for e in entries {
        out.push_str("  <hash>\n");
        out.push_str(&format!("    <file>{}</file>\n", xml_escape(&e.rel)));
        out.push_str(&format!("    <size>{}</size>\n", e.size));
        out.push_str(&format!(
            "    <lastmodificationdate>{}</lastmodificationdate>\n",
            e.mtime.to_rfc3339()
        ));
        out.push_str(&format!("    <xxhash64be>{}</xxhash64be>\n", hex64(e.hash)));
        out.push_str(&format!(
            "    <hashdate>{}</hashdate>\n",
            rfc3339(e.hashed_at)
        ));
        out.push_str("  </hash>\n");
    }
    out.push_str("</hashlist>\n");
    out
}

/// An element or attribute name with any XML namespace prefix stripped.
///
/// ASC MHL documents carry `xmlns="urn:ASC:MHL:v2.0"`, and some writers emit a
/// prefixed form. Matching on the local name reads both without a namespace
/// table.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

/// A manifest path as this program keys files: forward slashes, no `./` prefix.
fn normalise_rel(text: &str) -> String {
    text.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_string()
}

/// Whether a manifest's relative path stays inside the folder it describes.
///
/// The README invites manifests written by other tools, so this is untrusted
/// input arriving on somebody else's drive. Joining it componentwise onto the
/// manifest's own directory meant a `..` walked straight out: an entry of
/// `../outside.txt` was read and hashed, and the folder's INTACT verdict then
/// described a file that folder does not contain.
///
/// Only ordinary components are allowed — no `..`, no `.`, no drive prefix, no
/// root anchor, no empty segment.
fn is_contained_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let p = Path::new(rel);
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn parse_mtime(text: &str) -> Option<Mtime> {
    parse_rfc3339(text).ok().map(|d| Mtime {
        secs: d.timestamp(),
        nanos: 0,
    })
}

// ---------------------------------------------------------------------------
// ASC MHL
// ---------------------------------------------------------------------------

/// Render an ASC MHL (MHL v2) hash list.
///
/// MHL v1.1 is what this program has always written, and it is what
/// [`render_mhl`] still produces. It is also, increasingly, not what other tools
/// read: the ASC took the format over and the v2 schema is what Silverstack,
/// ShotPut and the `ascmhl` reference implementation speak.
///
/// A manifest nobody else can open is only half a manifest, so both are written.
/// They describe the same files with the same xxHash64 values; the difference is
/// entirely in the spelling.
///
/// **What this implements**: a single-generation hash list plus its chain entry.
/// Not implemented: directory hashes, partial-file hashes, flattening, or
/// multi-generation verification history. A reader that needs those will say so
/// rather than silently misreading this.
pub fn render_ascmhl(creator: &CreatorInfo, root_name: &str, entries: &[HashEntry]) -> String {
    let mut out = String::with_capacity(256 + entries.len() * 320);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<hashlist version=\"2.0\" xmlns=\"urn:ASC:MHL:v2.0\">\n");
    out.push_str("  <creatorinfo>\n");
    out.push_str(&format!(
        "    <creationdate>{}</creationdate>\n",
        rfc3339(creator.finish)
    ));
    out.push_str(&format!(
        "    <hostname>{}</hostname>\n",
        xml_escape(&creator.hostname)
    ));
    out.push_str(&format!(
        "    <tool version=\"{}\">sluice</tool>\n",
        xml_escape(super::build_info::VERSION)
    ));
    out.push_str(&format!(
        "    <username>{}</username>\n",
        xml_escape(&creator.name)
    ));
    out.push_str("  </creatorinfo>\n");
    out.push_str("  <processinfo>\n");
    out.push_str("    <process action=\"original\">in-place</process>\n");
    out.push_str(&format!(
        "    <roothash>\n      <content>{}</content>\n    </roothash>\n",
        xml_escape(root_name)
    ));
    out.push_str("  </processinfo>\n");
    out.push_str("  <hashes>\n");
    for e in entries {
        out.push_str("    <hash>\n");
        out.push_str(&format!(
            "      <path size=\"{}\" lastmodificationdate=\"{}\">{}</path>\n",
            e.size,
            e.mtime.to_rfc3339(),
            xml_escape(&e.rel)
        ));
        out.push_str(&format!(
            "      <xxh64 action=\"original\" hashdate=\"{}\">{}</xxh64>\n",
            rfc3339(e.hashed_at),
            hex64(e.hash)
        ));
        out.push_str("    </hash>\n");
    }
    out.push_str("  </hashes>\n");
    out.push_str("</hashlist>\n");
    out
}

/// One hash list in an ASC MHL directory's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub sequence: u32,
    pub name: String,
    /// xxHash64 of the hash list's own bytes.
    pub hash: u64,
}

/// Render `ascmhl_chain.xml` over every generation the directory holds.
///
/// The chain is what makes an ASC MHL directory readable as a history rather
/// than as a pile of loose files, and it has to name *all* of them. An earlier
/// cut wrote a single hard-coded generation 1 and rewrote the file on every run,
/// so a second session into the same folder left two hash lists both numbered
/// 0001 and a chain vouching for only the later -- a directory invalid on its
/// own terms, unconditionally, even when not one media file collided.
pub fn render_ascmhl_chain(generations: &[Generation]) -> String {
    let mut out = String::with_capacity(256 + generations.len() * 160);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<chain xmlns=\"urn:ASC:MHL:v2.0\">\n");
    for g in generations {
        out.push_str("  <hashlist version=\"2.0\">\n");
        out.push_str(&format!(
            "    <sequencenumber>{}</sequencenumber>\n",
            g.sequence
        ));
        out.push_str(&format!("    <path>{}</path>\n", xml_escape(&g.name)));
        out.push_str(&format!("    <xxh64>{}</xxh64>\n", hex64(g.hash)));
        out.push_str("  </hashlist>\n");
    }
    out.push_str("</chain>\n");
    out
}

/// The generations already present in an `ascmhl/` directory, in sequence order.
///
/// Read from the files themselves rather than from the chain, so a chain that a
/// previous version wrote wrongly is corrected rather than believed.
pub fn existing_generations(dir: &Path) -> Vec<Generation> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Generation> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mhl"))
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            // `NNNN_<session>.mhl`
            let sequence = name.split('_').next()?.parse().ok()?;
            let bytes = fs::read(&p).ok()?;
            Some(Generation {
                sequence,
                hash: xxhash_rust::xxh64::xxh64(&bytes, 0),
                name,
            })
        })
        .collect();
    out.sort_by_key(|g| g.sequence);
    out
}

/// The `ascmhl/` directory inside a destination root.
pub fn ascmhl_dir(root: &Path) -> PathBuf {
    root.join("ascmhl")
}

/// Why this `ascmhl/` directory looks like another tool's, if it does.
///
/// ASC MHL exists precisely so several tools can share a folder, and
/// `ascmhl_chain.xml` is a filename the spec fixes -- which makes it the one
/// path this program writes that another vendor also owns. Rewriting it
/// replaces their chain with sluice's dialect: their hash lists survive on
/// disk, but the record of what sealed them, in whatever algorithm they sealed
/// it with, does not. Worse, the replacement re-hashes each list from its
/// *current* bytes, so a hash list that rotted after the other tool sealed it
/// is silently re-blessed and the only evidence of the rot is gone.
///
/// Those bytes are somebody's evidence, and this program does not overwrite
/// evidence. If anything here was not written by sluice, sluice writes nothing
/// here at all -- MHL v1.1 is unaffected, and it is the manifest whose presence
/// is the success signal.
fn foreign_ascmhl(dir: &Path) -> Option<String> {
    for e in fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == ASCMHL_CHAIN {
            // Our own rendering carries xxh64 and nothing else.
            let text = fs::read_to_string(e.path()).unwrap_or_default();
            if let Some(tag) = ["<c4>", "<md5>", "<sha1>", "<sha256>", "<xxh128>", "<xxh3>"]
                .into_iter()
                .find(|t| text.contains(t))
            {
                let algo = tag.trim_matches(|c| c == '<' || c == '>');
                return Some(format!(
                    "{name} records {algo} hashes, which sluice never writes"
                ));
            }
            continue;
        }
        // `NNNN_<session>.mhl` is what `write_ascmhl` names its hash lists.
        let ours = name.ends_with(".mhl")
            && name
                .split_once('_')
                .is_some_and(|(seq, _)| seq.len() == 4 && seq.bytes().all(|b| b.is_ascii_digit()));
        if !ours {
            return Some(format!("{name} was not written by sluice"));
        }
    }
    None
}

/// The chain filename the ASC MHL spec fixes.
const ASCMHL_CHAIN: &str = "ascmhl_chain.xml";

/// Write the ASC MHL directory: one hash list and its chain.
///
/// Best-effort by construction. MHL v1.1 remains the manifest whose presence is
/// the success signal, because that is the one this program can re-verify
/// itself; ASC MHL is written so *other* tools can. A failure here is reported
/// and does not fail the run.
pub fn write_ascmhl(
    root: &Path,
    session: &str,
    creator: &CreatorInfo,
    entries: &[HashEntry],
) -> Result<PathBuf> {
    let dir = ascmhl_dir(root);
    if let Some(why) = foreign_ascmhl(&dir) {
        anyhow::bail!(
            "{} holds an ASC MHL directory another tool wrote ({why}). Leaving it untouched: \
             rewriting {ASCMHL_CHAIN} would replace that tool's chain with sluice's, and those \
             bytes are the only record of what it sealed. The MHL v1.1 manifest is written as \
             usual and is the one sluice re-verifies.",
            dir.display()
        );
    }
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    // Continue the directory's history rather than clobbering it. A shoot day
    // that offloads three card pairs into one folder is ordinary, and each is a
    // new generation.
    let mut generations = existing_generations(&dir);
    let sequence = generations.last().map(|g| g.sequence + 1).unwrap_or(1);

    let name = format!("{sequence:04}_{session}.mhl");
    let body = render_ascmhl(creator, &root_name, entries);
    write_synced(&dir.join(&name), body.as_bytes())?;

    // The chain vouches for each hash list by its own bytes, so a hash list
    // edited after the fact stops matching.
    generations.push(Generation {
        sequence,
        hash: xxhash_rust::xxh64::xxh64(body.as_bytes(), 0),
        name: name.clone(),
    });
    let chain = render_ascmhl_chain(&generations);
    write_synced(&dir.join(ASCMHL_CHAIN), chain.as_bytes())?;
    Ok(dir.join(name))
}

/// `sluice_<session>.mhl` in a destination root.
pub fn mhl_path(root: &Path, session: &str) -> PathBuf {
    root.join(format!("sluice_{session}.mhl"))
}

/// `sluice_<session>.json` in a destination root.
pub fn session_json_path(root: &Path, session: &str) -> PathBuf {
    root.join(format!("sluice_{session}.json"))
}

/// Write a manifest, flushing and syncing so it is genuinely on the device.
pub fn write_mhl(path: &Path, creator: &CreatorInfo, entries: &[HashEntry]) -> Result<()> {
    let body = render_mhl(creator, entries);
    write_synced(path, body.as_bytes())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Everything the MHL cannot hold. The forensic record, if something surfaces
/// at home months later.
#[derive(Debug, Clone, Serialize)]
pub struct SessionLog {
    pub tool: String,
    pub session: String,
    pub start: DateTime<Utc>,
    pub finish: DateTime<Utc>,
    /// Keyed by device label: `C1`, `C2`, `A`, `B`, `C`.
    pub devices: BTreeMap<String, DeviceInfo>,
    pub phases: Vec<PhaseTiming>,
    pub reconciliation: ReconSummary,
    /// The full comparison-matrix result for every file.
    pub files: Vec<FileVerdict>,
    pub verdict: VerdictReport,
    pub errors: Vec<String>,
    /// Which cards were formatted after this session, entered by the user as a
    /// one-line confirmation. If a file is ever found corrupt months later, this
    /// is how you reconstruct what happened.
    pub formatted_after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseTiming {
    pub phase: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconSummary {
    pub twinned: usize,
    pub only_c1: Vec<String>,
    pub only_c2: Vec<String>,
    pub conflicts: Vec<String>,
    pub had_card2: bool,
}

// ---------------------------------------------------------------------------
// Reading a manifest back
// ---------------------------------------------------------------------------

/// A manifest read back off disk.
///
/// Without this the manifest is write-only, and "another tool -- or future-you
/// without this binary -- can re-verify" stays an aspiration. Everything here
/// exists so [`super::recheck`] can answer "is this drive still good?" months
/// later, with no cards in the room.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub creator: CreatorInfo,
    pub entries: Vec<HashEntry>,
}

impl Manifest {
    /// Total bytes the manifest claims are present.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing timestamp {s:?}"))?
        .with_timezone(&Utc))
}

/// Read an MHL document.
pub fn parse_mhl(path: &Path) -> Result<Manifest> {
    let text =
        fs::read_to_string(path).with_context(|| format!("reading manifest {}", path.display()))?;
    parse_mhl_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Read an MHL document from a string.
pub fn parse_mhl_str(xml: &str) -> Result<Manifest> {
    use quick_xml::events::Event as XmlEvent;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut element = String::new();
    // The parser is flat rather than recursive: MHL nests only one level, and a
    // "which element did the text belong to" tracker is enough to read it.
    let mut in_hash = false;

    let (mut name, mut hostname, mut tool) = (String::new(), String::new(), String::new());
    let (mut start, mut finish) = (None, None);
    let mut entries: Vec<HashEntry> = Vec::new();
    let mut cur: Option<PartialEntry> = None;

    loop {
        match reader.read_event_into(&mut buf).context("malformed XML")? {
            XmlEvent::Start(e) => {
                element = local_name(e.name().as_ref());
                if element == "hash" {
                    in_hash = true;
                    cur = Some(PartialEntry::default());
                }
                // ASC MHL puts on attributes what MHL v1 puts in child elements:
                // `<path size="123" lastmodificationdate="...">rel</path>` and
                // `<xxh64 hashdate="...">hex</xxh64>`. Reading them here is what
                // lets one parser serve both dialects.
                if let Some(p) = cur.as_mut() {
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        let Ok(value) = attr.unescape_value() else {
                            continue;
                        };
                        p.set_attr(&element, &key, value.as_ref());
                    }
                }
            }
            XmlEvent::End(e) => {
                let ended = local_name(e.name().as_ref());
                if ended == "hash" {
                    in_hash = false;
                    if let Some(p) = cur.take() {
                        entries.push(p.finish()?);
                    }
                }
                element.clear();
            }
            XmlEvent::Text(e) => {
                let text = e.unescape().context("un-escaping text")?.into_owned();
                if text.trim().is_empty() {
                    continue;
                }
                if in_hash {
                    if let Some(p) = cur.as_mut() {
                        p.set(&element, &text);
                    }
                } else {
                    match element.as_str() {
                        "name" => name = text,
                        "hostname" => hostname = text,
                        "tool" => tool = text,
                        "startdate" => start = Some(parse_rfc3339(&text)?),
                        "finishdate" => finish = Some(parse_rfc3339(&text)?),
                        _ => {}
                    }
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if entries.is_empty() && start.is_none() {
        bail!("no <hashlist> content found -- is this an MHL file?");
    }
    let epoch = DateTime::<Utc>::UNIX_EPOCH;
    Ok(Manifest {
        creator: CreatorInfo {
            name,
            hostname,
            tool,
            start: start.unwrap_or(epoch),
            finish: finish.unwrap_or(epoch),
        },
        entries,
    })
}

#[derive(Default)]
struct PartialEntry {
    rel: Option<String>,
    size: Option<u64>,
    mtime: Option<Mtime>,
    hash: Option<u64>,
    hashed_at: Option<DateTime<Utc>>,
    /// A hash algorithm this manifest carries that sluice cannot check.
    other_algorithm: Option<String>,
}

impl PartialEntry {
    fn set(&mut self, element: &str, text: &str) {
        match element {
            // MHL v1 spells the path `<file>`; ASC MHL spells it `<path>`.
            "file" | "path" => self.rel = Some(normalise_rel(text)),
            "size" => self.size = text.parse().ok(),
            "lastmodificationdate" => self.mtime = parse_mtime(text),
            // `xxhash64be` is MHL v1. ASC MHL names the algorithm directly, and
            // renders the same 64-bit value as the same 16 hex digits.
            "xxhash64be" | "xxh64" => self.hash = u64::from_str_radix(text.trim(), 16).ok(),
            // Recorded, not parsed. sluice verifies xxHash64 only, and a
            // manifest from a rental house using MD5 used to abort with
            // "<hash> with no xxHash64 value" -- a message that names the wrong
            // problem and reads like a corrupt file.
            "md5" | "sha1" | "sha256" | "xxh3" | "xxh128" | "c4" => {
                self.other_algorithm = Some(element.to_string())
            }
            "hashdate" => self.hashed_at = parse_rfc3339(text).ok(),
            _ => {}
        }
    }

    /// The ASC MHL half: the same facts, carried as attributes.
    fn set_attr(&mut self, element: &str, key: &str, value: &str) {
        match (element, key) {
            ("path", "size") => self.size = value.parse().ok(),
            ("path", "lastmodificationdate") => self.mtime = parse_mtime(value),
            ("xxh64", "hashdate") => self.hashed_at = parse_rfc3339(value).ok(),
            _ => {}
        }
    }

    fn finish(self) -> Result<HashEntry> {
        let rel = self
            .rel
            .ok_or_else(|| anyhow::anyhow!("<hash> with no <file>"))?;
        if !is_contained_rel(&rel) {
            bail!(
                "{rel}: this entry points outside the folder the manifest describes. A manifest \
                 is the one thing sluice reads that it did not write, so its paths are not \
                 trusted -- and a re-check that wandered outside the folder would be vouching \
                 for something else entirely."
            );
        }
        Ok(HashEntry {
            size: self
                .size
                .ok_or_else(|| anyhow::anyhow!("{rel}: <hash> with no <size>"))?,
            hash: self.hash.ok_or_else(|| match &self.other_algorithm {
                Some(alg) => anyhow::anyhow!(
                    "this manifest records {} hashes; sluice verifies xxHash64 only, so it \
                     cannot check this copy. The files are listed and their sizes are \
                     readable -- what is missing is a hash sluice can recompute. (First \
                     affected entry: {rel})",
                    alg.to_uppercase()
                ),
                None => anyhow::anyhow!("{rel}: <hash> with no xxHash64 value"),
            })?,
            mtime: self.mtime.unwrap_or(Mtime { secs: 0, nanos: 0 }),
            hashed_at: self.hashed_at.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
            rel,
        })
    }
}

pub fn write_session_json(path: &Path, log: &SessionLog) -> Result<()> {
    let body = serde_json::to_vec_pretty(log).context("serialising the session log")?;
    write_synced(path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::Event as XmlEvent;
    use quick_xml::Reader;

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

    fn entry(rel: &str) -> HashEntry {
        HashEntry {
            rel: rel.into(),
            size: 62_914_560,
            mtime: Mtime {
                secs: 1_757_629_443,
                nanos: 0,
            },
            hash: 0xa1b2_c3d4_e5f6_0718,
            hashed_at: DateTime::parse_from_rfc3339("2026-03-14T22:31:12Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn renders_the_documented_shape() {
        let xml = render_mhl(&creator(), &[entry("DCIM/100MSDCF/DSC00001.ARW")]);
        for expected in [
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<hashlist version=\"1.1\">",
            "<tool>sluice 0.1.0</tool>",
            "<startdate>2026-03-14T22:14:03Z</startdate>",
            "<finishdate>2026-03-14T22:39:51Z</finishdate>",
            "<file>DCIM/100MSDCF/DSC00001.ARW</file>",
            "<size>62914560</size>",
            "<xxhash64be>a1b2c3d4e5f60718</xxhash64be>",
            "<hashdate>2026-03-14T22:31:12Z</hashdate>",
        ] {
            assert!(xml.contains(expected), "missing {expected:?} in:\n{xml}");
        }
    }

    /// Test 14, the half that can be automated: the emitted XML must parse, and
    /// the values must come back out intact.
    #[test]
    fn round_trips_through_an_independent_xml_parser() {
        let entries = [
            entry("DCIM/100MSDCF/DSC00001.ARW"),
            entry("PRIVATE/M4ROOT/CLIP/C0001.MP4"),
        ];
        let xml = render_mhl(&creator(), &entries);

        let mut reader = Reader::from_str(&xml);
        let mut files = Vec::new();
        let mut hashes = Vec::new();
        let mut current = String::new();
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf).expect("well-formed XML") {
                XmlEvent::Start(e) => {
                    current = String::from_utf8_lossy(e.name().as_ref()).into_owned()
                }
                // Without this, the indentation between </file> and <size> is a
                // Text event still attributed to "file".
                XmlEvent::End(_) => current.clear(),
                XmlEvent::Text(e) => {
                    let text = e.unescape().unwrap().into_owned();
                    match current.as_str() {
                        "file" => files.push(text),
                        "xxhash64be" => hashes.push(text),
                        _ => {}
                    }
                }
                XmlEvent::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        assert_eq!(
            files,
            vec![
                "DCIM/100MSDCF/DSC00001.ARW",
                "PRIVATE/M4ROOT/CLIP/C0001.MP4"
            ]
        );
        assert_eq!(hashes, vec!["a1b2c3d4e5f60718"; 2]);
    }

    #[test]
    fn escapes_xml_metacharacters_in_paths() {
        let xml = render_mhl(&creator(), &[entry("DCIM/a&b/<x>'\"y\".ARW")]);
        assert!(xml.contains("<file>DCIM/a&amp;b/&lt;x&gt;&apos;&quot;y&quot;.ARW</file>"));
        assert!(!xml.contains("<x>"), "raw metacharacters must not survive");

        // And it must still parse.
        let mut reader = Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf).expect("well-formed XML") {
                XmlEvent::Eof => break,
                _ => buf.clear(),
            }
        }
    }

    #[test]
    fn hashes_are_sixteen_hex_digits() {
        let mut e = entry("x.ARW");
        e.hash = 1;
        let xml = render_mhl(&creator(), &[e]);
        assert!(xml.contains("<xxhash64be>0000000000000001</xxhash64be>"));
    }

    #[test]
    fn empty_manifest_is_still_valid() {
        let xml = render_mhl(&creator(), &[]);
        assert!(xml.contains("</hashlist>"));
        let mut reader = Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf).expect("well-formed XML") {
                XmlEvent::Eof => break,
                _ => buf.clear(),
            }
        }
    }

    #[test]
    fn manifest_paths_follow_the_session_naming() {
        let root = Path::new("D:\\2026-03-14_shoot-01");
        assert_eq!(
            mhl_path(root, "20260314-2214"),
            root.join("sluice_20260314-2214.mhl")
        );
        assert_eq!(
            session_json_path(root, "20260314-2214"),
            root.join("sluice_20260314-2214.json")
        );
    }

    #[test]
    fn writes_and_reads_back_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = mhl_path(dir.path(), "20260314-2214");
        write_mhl(&path, &creator(), &[entry("DCIM/X.ARW")]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("<file>DCIM/X.ARW</file>"));
    }

    // --- ASC MHL -----------------------------------------------------------

    /// The point of writing a second dialect is that other tools can read it.
    /// The point of *this* test is that we can read our own, which is the only
    /// part we can check without those tools present.
    #[test]
    fn ascmhl_round_trips_through_the_parser() {
        let creator = creator();
        let entries = sample_entries();
        let xml = render_ascmhl(&creator, "2026-03-14_shoot-01", &entries);

        assert!(xml.contains("urn:ASC:MHL:v2.0"), "{xml}");
        assert!(xml.contains("<hashes>"));

        let back = parse_mhl_str(&xml).expect("our own ASC MHL must parse");
        assert_eq!(back.entries.len(), entries.len());
        for (a, b) in back.entries.iter().zip(entries.iter()) {
            assert_eq!(a.rel, b.rel);
            assert_eq!(a.size, b.size, "size lives in an attribute here");
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.mtime.secs, b.mtime.secs);
        }
    }

    /// Both dialects describe the same files with the same hashes. If they ever
    /// disagree, one of the two manifests on a drive is lying.
    #[test]
    fn both_dialects_describe_the_same_files() {
        let creator = creator();
        let entries = sample_entries();
        let v1 = parse_mhl_str(&render_mhl(&creator, &entries)).unwrap();
        let v2 = parse_mhl_str(&render_ascmhl(&creator, "root", &entries)).unwrap();
        let key = |m: &Manifest| -> Vec<(String, u64, u64)> {
            m.entries
                .iter()
                .map(|e| (e.rel.clone(), e.size, e.hash))
                .collect()
        };
        assert_eq!(key(&v1), key(&v2));
    }

    /// A hand-written ASC MHL from another tool, in the shape the reference
    /// implementation emits: namespaced, size as an attribute, `xxh64` rather
    /// than `xxhash64be`.
    #[test]
    fn a_foreign_ascmhl_document_parses() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hashlist version="2.0" xmlns="urn:ASC:MHL:v2.0">
  <creatorinfo>
    <creationdate>2026-03-14T22:14:03Z</creationdate>
    <hostname>othermachine</hostname>
    <tool version="0.4.0">ascmhl</tool>
  </creatorinfo>
  <hashes>
    <hash>
      <path size="4096" lastmodificationdate="2026-03-14T21:00:00Z">DCIM/100MSDCF/DSC00001.ARW</path>
      <xxh64 action="original" hashdate="2026-03-14T22:14:03Z">a1b2c3d4e5f60718</xxh64>
    </hash>
  </hashes>
</hashlist>
"#;
        let m = parse_mhl_str(xml).expect("a foreign ASC MHL must parse");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].rel, "DCIM/100MSDCF/DSC00001.ARW");
        assert_eq!(m.entries[0].size, 4096);
        assert_eq!(m.entries[0].hash, 0xa1b2_c3d4_e5f6_0718);
        assert_eq!(m.creator.hostname, "othermachine");
    }

    /// `./DCIM/x.ARW` and `DCIM/x.ARW` are the same file. Other writers use the
    /// prefixed form, and a re-verification that cannot match them would report
    /// every file as both missing and extra.
    #[test]
    fn dot_slash_prefixes_are_normalised_away() {
        let xml = r#"<hashlist version="2.0" xmlns="urn:ASC:MHL:v2.0">
  <hashes>
    <hash>
      <path size="1">./DCIM/x.ARW</path>
      <xxh64>0000000000000001</xxh64>
    </hash>
  </hashes>
</hashlist>"#;
        let m = parse_mhl_str(xml).unwrap();
        assert_eq!(m.entries[0].rel, "DCIM/x.ARW");
    }

    /// The chain vouches for the hash list by its bytes, so a hash list edited
    /// after the fact stops matching its own chain.
    #[test]
    fn the_chain_names_the_hashlist_and_its_hash() {
        let body = render_ascmhl(&creator(), "root", &sample_entries());
        let h = xxhash_rust::xxh64::xxh64(body.as_bytes(), 0);
        let chain = render_ascmhl_chain(&[Generation {
            sequence: 1,
            name: "0001_20260314-2214.mhl".into(),
            hash: h,
        }]);
        assert!(chain.contains("<sequencenumber>1</sequencenumber>"));
        assert!(chain.contains("0001_20260314-2214.mhl"));
        assert!(chain.contains(&hex64(h)));
    }

    /// A shoot day that offloads three card pairs into one folder is ordinary.
    /// Each is a new generation, and the chain has to name all of them -- the
    /// earlier cut wrote a hard-coded generation 1 every time and rewrote the
    /// chain, leaving a directory that was invalid on its own terms.
    #[test]
    fn a_second_session_becomes_a_second_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let first = write_ascmhl(root, "20260314-2214", &creator(), &sample_entries()).unwrap();
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0001_"),
            "{}",
            first.display()
        );

        let second = write_ascmhl(root, "20260314-2351", &creator(), &sample_entries()).unwrap();
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0002_"),
            "a second session must not reuse generation 1: {}",
            second.display()
        );

        let chain = fs::read_to_string(ascmhl_dir(root).join("ascmhl_chain.xml")).unwrap();
        assert!(
            chain.contains("<sequencenumber>1</sequencenumber>"),
            "{chain}"
        );
        assert!(
            chain.contains("<sequencenumber>2</sequencenumber>"),
            "the chain must keep the earlier generation: {chain}"
        );
        assert!(chain.contains("0001_20260314-2214.mhl"), "{chain}");
        assert!(chain.contains("0002_20260314-2351.mhl"), "{chain}");

        // And both hash lists are still readable.
        assert_eq!(parse_mhl(&first).unwrap().entries.len(), 2);
        assert_eq!(parse_mhl(&second).unwrap().entries.len(), 2);
    }

    /// Generations are read from the files rather than from the chain, so a
    /// A manifest is the one thing sluice reads that it did not write, and the
    /// README invites manifests from other tools — so it arrives on somebody
    /// else's drive and its paths are untrusted.
    ///
    /// Demonstrated before the fix: an entry of `../outside-the-session.txt`
    /// was read and hashed, and reported on, from outside the folder the
    /// manifest describes.
    #[test]
    fn a_manifest_entry_cannot_point_outside_its_own_folder() {
        for escape in [
            "../outside.txt",
            "DCIM/../../outside.txt",
            "C:/Windows/win.ini",
            "/etc/passwd",
            "..",
            "",
        ] {
            let xml = format!(
                r#"<?xml version="1.0"?><hashlist version="1.1"><hash>
                   <file>{escape}</file><size>11</size>
                   <xxhash64be>0000000000000000</xxhash64be></hash></hashlist>"#
            );
            let err = parse_mhl_str(&xml)
                .expect_err("must refuse an entry that leaves the folder: {escape}");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("outside the folder") || msg.contains("no <file>"),
                "{escape} gave: {msg}"
            );
        }
    }

    /// And ordinary entries still parse, including nested ones.
    #[test]
    fn ordinary_relative_entries_still_parse() {
        let xml = r#"<?xml version="1.0"?><hashlist version="1.1"><hash>
             <file>DCIM/100MSDCF/DSC00001.ARW</file><size>11</size>
             <xxhash64be>0000000000000001</xxhash64be></hash></hashlist>"#;
        let m = parse_mhl_str(xml).expect("a normal entry must parse");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].rel, "DCIM/100MSDCF/DSC00001.ARW");
    }

    /// `ascmhl_chain.xml` is a filename the ASC MHL spec fixes, which makes it
    /// the one path this program writes that another vendor's tool also owns.
    /// ASC MHL exists precisely so several tools can share a folder, so meeting
    /// one is ordinary rather than exotic -- and rewriting their chain replaces
    /// what they sealed, in whatever algorithm they sealed it with, with
    /// sluice's. Those bytes are somebody's evidence.
    #[test]
    fn another_tools_ascmhl_directory_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let asc = ascmhl_dir(dir.path());
        fs::create_dir_all(&asc).unwrap();
        let theirs =
            br#"<?xml version="1.0"?><chain><hashlist><c4>c4abc</c4></hashlist></chain>"#.to_vec();
        fs::write(asc.join(ASCMHL_CHAIN), &theirs).unwrap();

        let err = write_ascmhl(dir.path(), "20260314-2214", &creator(), &sample_entries())
            .expect_err("must refuse to write into another tool's directory");
        assert!(format!("{err:#}").contains("another tool"), "{err:#}");
        assert_eq!(
            fs::read(asc.join(ASCMHL_CHAIN)).unwrap(),
            theirs,
            "their chain must be byte-for-byte untouched"
        );
    }

    /// A hash list this program's naming never produces means the directory is
    /// somebody else's, even when no chain file gives it away.
    #[test]
    fn an_unrecognised_hash_list_makes_the_directory_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let asc = ascmhl_dir(dir.path());
        fs::create_dir_all(&asc).unwrap();
        fs::write(asc.join("Silverstack_2026-03-14.mhl"), b"<hashlist/>").unwrap();

        assert!(write_ascmhl(dir.path(), "20260314-2214", &creator(), &sample_entries()).is_err());
    }

    /// And sluice's own directory is still continued, or a second card pair on
    /// the same day would stop being recorded as a new generation.
    #[test]
    fn sluices_own_ascmhl_directory_is_not_treated_as_foreign() {
        let dir = tempfile::tempdir().unwrap();
        write_ascmhl(dir.path(), "20260314-2214", &creator(), &sample_entries()).unwrap();
        let second =
            write_ascmhl(dir.path(), "20260314-2351", &creator(), &sample_entries()).unwrap();
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0002_"),
            "{}",
            second.display()
        );
    }

    /// chain a previous version wrote wrongly is corrected rather than believed.
    #[test]
    fn generations_are_read_from_the_directory_not_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let asc = ascmhl_dir(dir.path());
        fs::create_dir_all(&asc).unwrap();
        fs::write(asc.join("0001_a.mhl"), b"one").unwrap();
        fs::write(asc.join("0003_c.mhl"), b"three").unwrap();
        // A chain that disagrees with what is actually there.
        fs::write(asc.join("ascmhl_chain.xml"), b"<chain/>").unwrap();

        let gens = existing_generations(&asc);
        assert_eq!(gens.len(), 2, "{gens:?}");
        assert_eq!(gens[0].sequence, 1);
        assert_eq!(gens[1].sequence, 3);

        // The next write continues past the highest, and does not reuse 2.
        let next =
            write_ascmhl(dir.path(), "20260314-2359", &creator(), &sample_entries()).unwrap();
        assert!(
            next.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0004_"),
            "{}",
            next.display()
        );
    }

    #[test]
    fn writing_an_ascmhl_directory_produces_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let written =
            write_ascmhl(dir.path(), "20260314-2214", &creator(), &sample_entries()).unwrap();
        assert!(written.exists(), "{}", written.display());
        assert!(ascmhl_dir(dir.path()).join("ascmhl_chain.xml").exists());
        // And the hash list it wrote must be readable by our own parser.
        let m = parse_mhl(&written).unwrap();
        assert_eq!(m.entries.len(), 2);
    }

    fn sample_entries() -> Vec<HashEntry> {
        vec![
            HashEntry {
                rel: "DCIM/100MSDCF/DSC00001.ARW".into(),
                size: 91_400_000,
                mtime: Mtime {
                    secs: 1_757_629_443,
                    nanos: 0,
                },
                hash: 0xa1b2_c3d4_e5f6_0718,
                hashed_at: DateTime::parse_from_rfc3339("2026-03-14T22:14:03Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
            HashEntry {
                rel: "PRIVATE/M4ROOT/CLIP/C0001.MP4".into(),
                size: 4_096,
                mtime: Mtime {
                    secs: 1_757_629_445,
                    nanos: 0,
                },
                hash: 1,
                hashed_at: DateTime::parse_from_rfc3339("2026-03-14T22:14:04Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
        ]
    }
}
