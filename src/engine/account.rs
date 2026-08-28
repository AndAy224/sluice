//! Accounting for a difference the camera is *supposed* to write.
//!
//! A body recording the same take to both card slots does not write identical
//! files, and it is not malfunctioning when it does not. Every difference
//! measured on real hardware traces to the **UMID** -- the SMPTE 330M Unique
//! Material Identifier, which identifies a material *instance*. Two cards are
//! two instances, so the standard requires it to differ.
//!
//! Measured on two clips from one Sony body:
//!
//! | | C0010 | C0011 |
//! |---|---|---|
//! | differing bytes | 771,896 | 259,827 |
//! | **inside video or audio samples** | **0** | **0** |
//! | inside the timed-metadata track | 771,895 | 259,825 |
//! | inside the top-level `meta` box | 1 | 2 |
//!
//! The picture and the sound are byte-identical on both cards; the difference is
//! the UMID, carried per frame in the metadata track and once more in the clip
//! XML embedded in the `meta` box. `ffmpeg` agrees: the HEVC and PCM streams hash
//! the same from either card.
//!
//! So the twin check is failing over an identifier, and refusing to authorise an
//! erase for a reason that has nothing to do with the footage.
//!
//! # What this proves, and what it does not
//!
//! It proves, positively: **not one differing byte lies inside a video or audio
//! sample.** That is measured against the container's own sample tables, and the
//! tables are taken only from a `moov` that is byte-identical on both cards, so
//! the two cards agree about which bytes are picture and sound before either is
//! excused anything.
//!
//! It does **not** prove the metadata is equivalent, and does not try to. What
//! the operator is told is exactly what was established: the picture and sound
//! are identical, and the rest of the difference is metadata.
//!
//! # Why it does not parse the vendor's metadata
//!
//! The difference sits inside a privately-defined SMPTE KLV record
//! (`060e2b34...7f010000`, byte 12 = `0x7f` meaning vendor-defined). Pinning
//! that key would narrow the excuse further, and it was considered. It is not
//! done, for two reasons. It would refuse these very clips -- the `meta` box
//! bytes are in no metadata sample at all. And it would put a vendor metadata
//! parser inside the path that authorises erasing originals, where a
//! misparse becomes a wrong verdict. Proving the essence untouched needs no
//! opinion about what the metadata means.
//!
//! # Failing closed
//!
//! Every gate below returns [`Outcome::Refused`], and a refusal is exactly
//! today's behaviour: the file keeps its `TwinMismatch` and the run keeps its
//! `Failed`. There is no partial credit, no "parse what I can", and no path that
//! rounds an unrecognised structure toward agreement.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use xxhash_rust::xxh64::Xxh64;

use super::unbuffered::{AlignedBuf, ChunkReader};

/// Containers the allowance may be attempted on.
///
/// Pinned because the whole justification is a claim about one container
/// format. A file with any other extension gets today's refusal, which is the
/// safe direction and the signal to measure before widening.
const PINNED_EXTENSIONS: &[&str] = &["mp4"];

/// Largest share of the file that may differ.
///
/// This is the number that matters: it is exactly the share of the file that
/// loses twin protection. Measured at 0.58% on a 50 Mbps clip. Two percent is
/// three times that.
///
/// A low-bitrate recording -- a Sony proxy is also `.MP4` and also carries a
/// timed-metadata track -- has the same metadata per frame against far fewer
/// picture bytes, so its excused share rises steeply. That configuration is
/// **unmeasured**, and an unproven claim is treated exactly like a disproven
/// one, so it refuses here rather than being waved through. The fix is to
/// measure a proxy pair and re-ground this number, not to raise it.
const MAX_DIFFERING_FRACTION: f64 = 0.02;

/// Largest `moov` read into memory while parsing. Sample tables grow with the
/// frame count; a long clip is a few MB. This bounds a hostile file.
const MAX_MOOV: u64 = 64 << 20;

/// Fewest samples the timed-metadata track must carry.
///
/// Below this there are too few frames to tell a structural, every-frame
/// difference from a one-off defect, so a very short clip refuses.
const MIN_META_SAMPLES: usize = 32;

/// What the two cards were found to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The picture and sound are byte-identical; the difference is metadata.
    Accounted(Accounted),
    /// Not established. The file keeps its twin mismatch.
    Refused(Refusal),
}

/// What was actually established, which is not the same claim for every kind of
/// file and must not be reported as though it were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// No differing byte lies inside a video or audio sample.
    EssenceUntouched,
    /// Every differing byte lies inside an identifier attribute value.
    IdentifiersOnly,
}

/// The measurement behind an [`Outcome::Accounted`], for the log and the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accounted {
    pub proof: Proof,
    pub file_len: u64,
    pub differing_bytes: u64,
    /// Bytes proven identical on both cards: picture and sound for a container,
    /// everything outside the identifiers for an index.
    pub matched_bytes: u64,
    /// Metadata samples for a container; identifier attributes for an index.
    pub sites: usize,
}

impl Accounted {
    pub fn differing_fraction(&self) -> f64 {
        if self.file_len == 0 {
            0.0
        } else {
            self.differing_bytes as f64 / self.file_len as f64
        }
    }
}

/// Why an allowance was not established. Every one of these means "refuse".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not a container this rule has been grounded on.
    NotPinnedContainer,
    /// Different lengths: a size conflict, handled elsewhere.
    LengthMismatch,
    /// The container could not be walked, or lacks the boxes this needs.
    Unparsable(&'static str),
    /// The two cards disagree about the file's structure, so they do not agree
    /// about which bytes are picture and sound.
    MoovDiffers,
    /// The sample tables were read from bytes that the verified read did not
    /// reproduce.
    MoovNotStable,
    /// Too few metadata samples to tell structure from a defect.
    TooFewSamples(usize),
    /// More of the file differs than metadata can account for.
    TooMuchDiffers { bytes: u64, len: u64 },
    /// The thing this exists to catch: the cards differ inside the footage.
    EssenceDiffers { bytes: u64 },
}

impl Refusal {
    /// One line for the log, in the operator's terms.
    pub fn describe(&self) -> String {
        match self {
            Self::NotPinnedContainer => "not a container this check has been grounded on".into(),
            Self::LengthMismatch => "the two cards hold different lengths".into(),
            Self::Unparsable(why) => format!("the container could not be read: {why}"),
            Self::MoovDiffers => {
                "the two cards describe the file differently, so they do not agree on which \
                 bytes are picture and sound"
                    .into()
            }
            Self::MoovNotStable => {
                "the structure read while parsing did not match the structure read off the \
                 device"
                    .into()
            }
            Self::TooFewSamples(n) => {
                format!("only {n} metadata samples -- too few to tell structure from a defect")
            }
            Self::TooMuchDiffers { bytes, len } => format!(
                "{bytes} of {len} bytes differ ({:.2}%), more than per-card metadata accounts \
                 for",
                100.0 * *bytes as f64 / *len as f64
            ),
            Self::EssenceDiffers { bytes } => format!(
                "{bytes} differing byte(s) fall inside the picture or sound -- this is a real \
                 mismatch"
            ),
        }
    }
}

/// The attributes whose values two cards are allowed to disagree about.
///
/// All three name the same thing: the identity of *this card's* copy. `umid` and
/// `umidRef` carry a SMPTE 330M Unique Material Identifier, which identifies a
/// material **instance** -- two cards are two instances, so the standard
/// requires them to differ. `mediaId` names the card itself.
///
/// Nothing else is allowed to differ, and this list is the whole of the excuse.
const IDENTIFIER_ATTRS: &[&str] = &["umidRef", "umid", "mediaId"];

/// Longest attribute value that may be excused. A basic UMID is 64 hex
/// characters; a `mediaId` is 32. This stops a crafted file declaring one
/// enormous "identifier" and swallowing the difference inside it.
const MAX_IDENTIFIER: usize = 128;

/// Largest XML this will read. Sony's clip and index files are hundreds of bytes
/// to a few kilobytes; this bounds a hostile one.
const MAX_XML: u64 = 1 << 20;

/// Offsets of `name="value"` spans for the allowlisted attributes.
///
/// Returns the **value** spans only, never the names, and refuses anything that
/// does not look like an identifier -- so the excused region cannot be made to
/// contain arbitrary data by writing arbitrary data into it.
fn identifier_spans(buf: &[u8]) -> Result<Vec<(usize, usize)>, &'static str> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for attr in IDENTIFIER_ATTRS {
        let pat: Vec<u8> = attr.bytes().chain(*b"=\"").collect();
        let mut from = 0usize;
        while let Some(p) = find(&buf[from..], &pat) {
            let at = from + p;
            // The name must stand alone: `xumid="` is not `umid="`.
            let standalone = at == 0
                || buf[at - 1].is_ascii_whitespace()
                || buf[at - 1] == b'<'
                || buf[at - 1] == b'/';
            let start = at + pat.len();
            let Some(q) = buf[start..].iter().position(|&c| c == b'"') else {
                return Err("an attribute value is never closed");
            };
            let end = start + q;
            if standalone {
                if end - start > MAX_IDENTIFIER {
                    return Err("an identifier attribute is longer than any identifier");
                }
                if !buf[start..end].iter().all(|c| c.is_ascii_alphanumeric()) {
                    return Err("an identifier attribute does not hold an identifier");
                }
                out.push((start, end));
            }
            from = end;
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Decide whether two cards' copies of an XML index differ only in identifiers.
///
/// Sony writes a per-card `umid`/`mediaId` into its clip and media indices, so
/// these files differ between two slots by construction. This permits exactly
/// that and nothing else: both cards must produce the *same* set of identifier
/// spans at the *same* offsets, and every differing byte must fall inside one.
fn account_xml(a: &Path, b: &Path) -> Result<Outcome> {
    let ma = std::fs::metadata(a)?;
    let mb = std::fs::metadata(b)?;
    if ma.len() != mb.len() {
        return Ok(Outcome::Refused(Refusal::LengthMismatch));
    }
    if ma.len() > MAX_XML {
        return Ok(Outcome::Refused(Refusal::Unparsable(
            "larger than any index",
        )));
    }
    // Read off the device, like everything else a verdict rests on.
    let (bytes_a, _) = read_all_unbuffered(a)?;
    let (bytes_b, _) = read_all_unbuffered(b)?;

    let spans_a = match identifier_spans(&bytes_a) {
        Ok(s) => s,
        Err(why) => return Ok(Outcome::Refused(Refusal::Unparsable(why))),
    };
    let spans_b = match identifier_spans(&bytes_b) {
        Ok(s) => s,
        Err(why) => return Ok(Outcome::Refused(Refusal::Unparsable(why))),
    };
    // The two cards must agree on where the identifiers are before either is
    // excused anything inside one.
    if spans_a != spans_b {
        return Ok(Outcome::Refused(Refusal::MoovDiffers));
    }
    if spans_a.is_empty() {
        return Ok(Outcome::Refused(Refusal::Unparsable(
            "no identifier attributes to account for",
        )));
    }

    let mut differing = 0u64;
    let mut outside = 0u64;
    for (k, (x, y)) in bytes_a.iter().zip(bytes_b.iter()).enumerate() {
        if x == y {
            continue;
        }
        differing += 1;
        if !spans_a.iter().any(|(s, e)| k >= *s && k < *e) {
            outside += 1;
        }
    }
    if outside > 0 {
        return Ok(Outcome::Refused(Refusal::EssenceDiffers { bytes: outside }));
    }
    Ok(Outcome::Accounted(Accounted {
        proof: Proof::IdentifiersOnly,
        file_len: ma.len(),
        differing_bytes: differing,
        matched_bytes: ma.len() - differing,
        sites: spans_a.len(),
    }))
}

fn read_all_unbuffered(path: &Path) -> Result<(Vec<u8>, u64)> {
    let mut r = ChunkReader::open(path)?;
    let len = r.len();
    let mut buf = AlignedBuf::chunk();
    let mut out = Vec::with_capacity(len as usize);
    loop {
        let n = r.next_chunk(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok((out, len))
}

/// A half-open byte range of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u64,
    end: u64,
}

impl Span {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        start < self.end && self.start < end
    }
}

/// Decide whether the difference between two cards' copies of one file is
/// confined to metadata.
///
/// `a` and `b` are the same relative path on each card. Reads both off the
/// device, unbuffered, in lockstep.
pub fn account(a: &Path, b: &Path) -> Result<Outcome> {
    let ext = a
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Dispatch on the file's own kind, and refuse anything with no rule. A
    // photograph is not an index and not a container: it gets no allowance,
    // which is why a flipped bit in an ARW still fails the run.
    if ext == "xml" {
        return account_xml(a, b);
    }
    if !PINNED_EXTENSIONS.iter().any(|p| ext == *p) {
        return Ok(Outcome::Refused(Refusal::NotPinnedContainer));
    }

    // --- structure, from a seeking pass over each card -------------------
    //
    // Buffered and seeking, because walking boxes means jumping. Nothing is
    // trusted from it: the `moov` bytes it parsed are digested, and the
    // unbuffered pass below has to reproduce that digest before any of this
    // counts.
    let (len_a, moov_a) = read_moov(a)?;
    let (len_b, moov_b) = read_moov(b)?;
    if len_a != len_b {
        return Ok(Outcome::Refused(Refusal::LengthMismatch));
    }
    let (Some(moov_a), Some(moov_b)) = (moov_a, moov_b) else {
        return Ok(Outcome::Refused(Refusal::Unparsable("no moov box")));
    };
    // The two cards must describe the file identically, or they do not agree
    // about which bytes are picture and sound and nothing below means anything.
    if moov_a.bytes != moov_b.bytes || moov_a.span != moov_b.span {
        return Ok(Outcome::Refused(Refusal::MoovDiffers));
    }
    let moov_digest = {
        let mut h = Xxh64::new(0);
        h.update(&moov_a.bytes);
        h.digest()
    };

    let tracks = match parse_tracks(&moov_a.bytes) {
        Ok(t) => t,
        Err(why) => return Ok(Outcome::Refused(Refusal::Unparsable(why))),
    };
    let essence: Vec<Span> = tracks
        .iter()
        .filter(|t| t.is_essence())
        .flat_map(|t| t.chunks.iter().copied())
        .collect();
    let meta_samples: usize = tracks
        .iter()
        .filter(|t| !t.is_essence())
        .map(|t| t.samples)
        .sum();
    if essence.is_empty() {
        return Ok(Outcome::Refused(Refusal::Unparsable(
            "no video or audio track to protect",
        )));
    }
    if meta_samples < MIN_META_SAMPLES {
        return Ok(Outcome::Refused(Refusal::TooFewSamples(meta_samples)));
    }
    let essence_bytes: u64 = essence.iter().map(|s| s.end - s.start).sum();

    // --- the verified pass: both cards, unbuffered, in lockstep ----------
    let budget = (len_a as f64 * MAX_DIFFERING_FRACTION) as u64;
    let scan = compare(a, b, moov_a.span, budget)?;
    match scan {
        Scan::TooMuch(bytes) => Ok(Outcome::Refused(Refusal::TooMuchDiffers {
            bytes,
            len: len_a,
        })),
        Scan::Done {
            runs,
            differing,
            moov_seen,
        } => {
            // The tables were parsed from bytes the device just reproduced.
            if moov_seen != moov_digest {
                return Ok(Outcome::Refused(Refusal::MoovNotStable));
            }
            // The whole point: does anything differ inside the footage?
            let mut in_essence = 0u64;
            for (start, len) in &runs {
                let end = start + *len as u64;
                for s in &essence {
                    if s.overlaps(*start, end) {
                        let lo = (*start).max(s.start);
                        let hi = end.min(s.end);
                        in_essence += hi - lo;
                    }
                }
            }
            if in_essence > 0 {
                return Ok(Outcome::Refused(Refusal::EssenceDiffers {
                    bytes: in_essence,
                }));
            }
            Ok(Outcome::Accounted(Accounted {
                proof: Proof::EssenceUntouched,
                file_len: len_a,
                differing_bytes: differing,
                matched_bytes: essence_bytes,
                sites: meta_samples,
            }))
        }
    }
}

enum Scan {
    Done {
        runs: Vec<(u64, u32)>,
        differing: u64,
        moov_seen: u64,
    },
    TooMuch(u64),
}

/// Read both files off the device in lockstep, recording where they differ and
/// digesting the `moov` region as it goes.
fn compare(a: &Path, b: &Path, moov: Span, budget: u64) -> Result<Scan> {
    let mut ra = ChunkReader::open(a)?;
    let mut rb = ChunkReader::open(b)?;
    let mut ba = AlignedBuf::chunk();
    let mut bb = AlignedBuf::chunk();
    let mut runs: Vec<(u64, u32)> = Vec::new();
    let mut differing = 0u64;
    let mut pos = 0u64;
    let mut moov_hash = Xxh64::new(0);

    loop {
        let na = ra.next_chunk(&mut ba)?;
        let nb = rb.next_chunk(&mut bb)?;
        if na == 0 && nb == 0 {
            break;
        }
        if na != nb {
            anyhow::bail!("equal-length files returned different read counts");
        }
        // Digest whatever part of this chunk is inside moov.
        let lo = moov.start.max(pos);
        let hi = moov.end.min(pos + na as u64);
        if lo < hi {
            moov_hash.update(&ba[(lo - pos) as usize..(hi - pos) as usize]);
        }

        let mut k = 0usize;
        while k < na {
            if ba[k] == bb[k] {
                k += 1;
                continue;
            }
            let start = k;
            while k < na && ba[k] != bb[k] {
                k += 1;
            }
            let run = (k - start) as u64;
            differing += run;
            if differing > budget {
                return Ok(Scan::TooMuch(differing));
            }
            runs.push((pos + start as u64, (k - start) as u32));
        }
        pos += na as u64;
    }
    Ok(Scan::Done {
        runs,
        differing,
        moov_seen: moov_hash.digest(),
    })
}

// ---------------------------------------------------------------------------
// ISO base media file format: only as much as this needs
// ---------------------------------------------------------------------------

struct Moov {
    span: Span,
    bytes: Vec<u8>,
}

/// One box header: `[u32 size][4cc type]`, with the 64-bit and to-EOF forms.
fn read_header(f: &mut BufReader<File>, at: u64, end: u64) -> Result<Option<(u64, [u8; 4], u64)>> {
    if end.saturating_sub(at) < 8 {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(at))?;
    let mut hdr = [0u8; 16];
    f.read_exact(&mut hdr[..8])?;
    let mut size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
    let ty: [u8; 4] = hdr[4..8].try_into().unwrap();
    let mut body = 8u64;
    if size == 1 {
        f.read_exact(&mut hdr[8..16])?;
        size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
        body = 16;
    } else if size == 0 {
        size = end - at;
    }
    if size < body {
        return Ok(None);
    }
    Ok(Some((size, ty, at + body)))
}

/// Walk the top level and return the file length plus the `moov` box.
fn read_moov(path: &Path) -> Result<(u64, Option<Moov>)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut f = BufReader::new(file);
    let mut at = 0u64;
    while let Some((size, ty, body)) = read_header(&mut f, at, len)? {
        if &ty == b"moov" {
            if size > MAX_MOOV {
                return Ok((len, None));
            }
            let mut bytes = vec![0u8; (at + size - body) as usize];
            f.seek(SeekFrom::Start(body))?;
            f.read_exact(&mut bytes)?;
            // The span is the *body*, exactly the bytes `bytes` holds, so the
            // digest taken here and the one taken during the verified pass
            // cover the same range. Including the header in one and not the
            // other makes every file look unstable.
            return Ok((
                len,
                Some(Moov {
                    span: Span {
                        start: body,
                        end: at + size,
                    },
                    bytes,
                }),
            ));
        }
        at += size;
    }
    Ok((len, None))
}

struct Track {
    handler: [u8; 4],
    chunks: Vec<Span>,
    samples: usize,
}

impl Track {
    fn is_essence(&self) -> bool {
        &self.handler == b"vide" || &self.handler == b"soun"
    }
}

/// Boxes directly inside `buf`, which is one box's body.
fn children(buf: &[u8]) -> Vec<(&[u8], [u8; 4])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= buf.len() {
        let mut size = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        let ty: [u8; 4] = buf[at + 4..at + 8].try_into().unwrap();
        let mut body = 8usize;
        if size == 1 {
            if at + 16 > buf.len() {
                break;
            }
            size = u64::from_be_bytes(buf[at + 8..at + 16].try_into().unwrap()) as usize;
            body = 16;
        } else if size == 0 {
            size = buf.len() - at;
        }
        if size < body || at + size > buf.len() {
            break;
        }
        out.push((&buf[at + body..at + size], ty));
        at += size;
    }
    out
}

fn child<'a>(buf: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    children(buf)
        .into_iter()
        .find(|(_, t)| t == want)
        .map(|(b, _)| b)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
}

/// Turn `moov`'s sample tables into, per track, the byte spans its chunks
/// occupy.
///
/// Chunks are contiguous groups of samples, so a chunk is one span and the
/// per-sample detail is not needed: the question is only which bytes belong to
/// picture and sound.
fn parse_tracks(moov: &[u8]) -> Result<Vec<Track>, &'static str> {
    let mut out = Vec::new();
    for (trak, ty) in children(moov) {
        if &ty != b"trak" {
            continue;
        }
        let mdia = child(trak, b"mdia").ok_or("track without mdia")?;
        let hdlr = child(mdia, b"hdlr").ok_or("track without hdlr")?;
        let handler: [u8; 4] = hdlr.get(8..12).ok_or("short hdlr")?.try_into().unwrap();
        let minf = child(mdia, b"minf").ok_or("track without minf")?;
        let stbl = child(minf, b"stbl").ok_or("track without stbl")?;

        let stsz = child(stbl, b"stsz").ok_or("track without stsz")?;
        let uniform = be32(stsz, 4).ok_or("short stsz")?;
        let count = be32(stsz, 8).ok_or("short stsz")? as usize;

        let stsc = child(stbl, b"stsc").ok_or("track without stsc")?;
        let sc_n = be32(stsc, 4).ok_or("short stsc")? as usize;
        let mut sc: Vec<(usize, usize)> = Vec::with_capacity(sc_n);
        for i in 0..sc_n {
            let o = 8 + 12 * i;
            let first = be32(stsc, o).ok_or("short stsc")? as usize;
            let per = be32(stsc, o + 4).ok_or("short stsc")? as usize;
            sc.push((first, per));
        }

        let mut offsets: Vec<u64> = Vec::new();
        if let Some(stco) = child(stbl, b"stco") {
            let n = be32(stco, 4).ok_or("short stco")? as usize;
            for i in 0..n {
                offsets.push(be32(stco, 8 + 4 * i).ok_or("short stco")? as u64);
            }
        } else if let Some(co64) = child(stbl, b"co64") {
            let n = be32(co64, 4).ok_or("short co64")? as usize;
            for i in 0..n {
                offsets.push(be64(co64, 8 + 8 * i).ok_or("short co64")?);
            }
        } else {
            return Err("track without chunk offsets");
        }

        let mut chunks = Vec::with_capacity(offsets.len());
        let mut sample = 0usize;
        for (c, off) in offsets.iter().enumerate() {
            let mut per = 1usize;
            for &(first, p) in sc.iter().rev() {
                if c + 1 >= first {
                    per = p;
                    break;
                }
            }
            let mut len = 0u64;
            for _ in 0..per {
                if sample >= count {
                    break;
                }
                let sz = if uniform == 0 {
                    be32(stsz, 12 + 4 * sample).ok_or("short stsz table")? as u64
                } else {
                    uniform as u64
                };
                len += sz;
                sample += 1;
            }
            if len > 0 {
                chunks.push(Span {
                    start: *off,
                    end: off + len,
                });
            }
        }
        out.push(Track {
            handler,
            chunks,
            samples: count,
        });
    }
    if out.is_empty() {
        return Err("no tracks");
    }
    // Picture and sound must not overlap anything else, or "outside the essence"
    // means nothing.
    let ess: Vec<Span> = out
        .iter()
        .filter(|t| t.is_essence())
        .flat_map(|t| t.chunks.iter().copied())
        .collect();
    let rest: Vec<Span> = out
        .iter()
        .filter(|t| !t.is_essence())
        .flat_map(|t| t.chunks.iter().copied())
        .collect();
    for e in &ess {
        for r in &rest {
            if e.overlaps(r.start, r.end) {
                return Err("a metadata chunk overlaps picture or sound");
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half-open, and the ends matter: a differing byte at the first or last
    /// byte of a picture sample has to count as touching the picture.
    #[test]
    fn a_span_is_half_open_at_both_ends() {
        let s = Span { start: 10, end: 20 };
        assert!(
            s.overlaps(19, 25),
            "a run starting on the last byte touches"
        );
        assert!(
            !s.overlaps(20, 25),
            "a run starting one past the end does not"
        );
        assert!(
            !s.overlaps(0, 10),
            "a run ending on the first byte does not"
        );
        assert!(s.overlaps(0, 11), "a run ending one into it does");
        assert!(s.overlaps(0, 100), "a run swallowing it does");
    }

    fn xml_pair(a_body: &str, b_body: &str) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.xml");
        let b = dir.path().join("b.xml");
        std::fs::write(&a, a_body).unwrap();
        std::fs::write(&b, b_body).unwrap();
        account(&a, &b).unwrap()
    }

    /// The real shape: Sony's clip index, differing only in the UMID.
    #[test]
    fn an_index_differing_only_in_its_umid_is_accounted_for() {
        let a = r#"<?xml version="1.0"?><NonRealTimeMeta>
  <TargetMaterial umidRef="060A2B340101010501010D431300000081A9AA98801206C078F505FFFE42DF3D"/>
  <Duration value="372"/>
</NonRealTimeMeta>"#;
        let b = a.replace("801206C0", "801206CA");
        assert_eq!(a.len(), b.len());
        match xml_pair(a, &b) {
            Outcome::Accounted(x) => {
                assert_eq!(x.proof, Proof::IdentifiersOnly);
                assert_eq!(x.differing_bytes, 1, "only the one hex digit differs");
                assert_eq!(
                    x.matched_bytes,
                    a.len() as u64 - 1,
                    "everything that is not the identifier was proven identical"
                );
            }
            other => panic!("should account: {other:?}"),
        }
    }

    /// The whole point of the allowlist. A difference anywhere else is a
    /// difference, whatever else the file contains.
    #[test]
    fn a_difference_outside_an_identifier_is_refused() {
        let a = r#"<?xml version="1.0"?><M>
  <TargetMaterial umidRef="060A2B340101010501010D431300000081A9AA98801206C078F505FFFE42DF3D"/>
  <Duration value="372"/>
</M>"#;
        // The duration, not the identifier.
        let b = a.replace("372", "999");
        assert_eq!(a.len(), b.len());
        assert!(
            matches!(
                xml_pair(a, &b),
                Outcome::Refused(Refusal::EssenceDiffers { .. })
            ),
            "a changed duration is a real difference"
        );
    }

    /// An attribute whose name merely ends in an allowlisted one must not
    /// inherit the excuse.
    #[test]
    fn a_lookalike_attribute_name_earns_nothing() {
        let a = r#"<?xml version="1.0"?><M><X notumid="AAAA" umid="BBBB"/></M>"#;
        let b = r#"<?xml version="1.0"?><M><X notumid="ZZZZ" umid="BBBB"/></M>"#;
        assert_eq!(a.len(), b.len());
        assert!(
            matches!(
                xml_pair(a, b),
                Outcome::Refused(Refusal::EssenceDiffers { .. })
            ),
            "`notumid` is not `umid`"
        );
    }

    /// A crafted file must not be able to hide a payload inside a declared
    /// "identifier".
    #[test]
    fn an_identifier_that_is_not_an_identifier_is_refused() {
        let a = r#"<?xml version="1.0"?><M><X umid="hello, this is not hex"/></M>"#;
        let b = r#"<?xml version="1.0"?><M><X umid="hello, this is NOT hex"/></M>"#;
        assert_eq!(a.len(), b.len());
        assert!(
            matches!(xml_pair(a, b), Outcome::Refused(Refusal::Unparsable(_))),
            "a value with spaces and punctuation is not an identifier"
        );
    }

    #[test]
    fn an_xml_with_no_identifiers_gets_no_allowance() {
        let a = r#"<?xml version="1.0"?><M><Duration value="372"/></M>"#;
        let b = r#"<?xml version="1.0"?><M><Duration value="999"/></M>"#;
        assert!(matches!(
            xml_pair(a, b),
            Outcome::Refused(Refusal::Unparsable(_))
        ));
    }

    #[test]
    fn only_pinned_containers_are_attempted() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x.arw");
        let b = dir.path().join("y.arw");
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(&b, b"two").unwrap();
        assert_eq!(
            account(&a, &b).unwrap(),
            Outcome::Refused(Refusal::NotPinnedContainer),
            "a still is not a container this rule was grounded on"
        );
    }

    #[test]
    fn a_file_that_is_not_a_container_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x.mp4");
        let b = dir.path().join("y.mp4");
        std::fs::write(&a, vec![0u8; 4096]).unwrap();
        std::fs::write(&b, vec![1u8; 4096]).unwrap();
        assert!(
            matches!(account(&a, &b).unwrap(), Outcome::Refused(_)),
            "garbage named .mp4 must refuse, not parse"
        );
    }

    #[test]
    fn different_lengths_are_not_this_rules_business() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x.mp4");
        let b = dir.path().join("y.mp4");
        std::fs::write(&a, vec![0u8; 4096]).unwrap();
        std::fs::write(&b, vec![0u8; 8192]).unwrap();
        assert_eq!(
            account(&a, &b).unwrap(),
            Outcome::Refused(Refusal::LengthMismatch)
        );
    }

    #[test]
    fn every_refusal_says_something_specific() {
        for r in [
            Refusal::NotPinnedContainer,
            Refusal::LengthMismatch,
            Refusal::Unparsable("no moov box"),
            Refusal::MoovDiffers,
            Refusal::MoovNotStable,
            Refusal::TooFewSamples(3),
            Refusal::TooMuchDiffers {
                bytes: 10,
                len: 100,
            },
            Refusal::EssenceDiffers { bytes: 1 },
        ] {
            let d = r.describe();
            assert!(!d.is_empty(), "{r:?} has nothing to say");
            assert!(
                !d.to_lowercase().contains("error"),
                "{r:?} should say what is true, not that something errored"
            );
        }
    }
}
