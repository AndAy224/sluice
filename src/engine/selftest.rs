//! Day-one proofs, runnable against real hardware.
//!
//! The unit tests in [`super::unbuffered`] cover the sector tail on whatever
//! volume `cargo test` happens to land on -- almost certainly the system SSD.
//! These run the same checks against a *named* drive, because the assumption in
//! question is about the device and its driver, not about the code.
//!
//! Two proofs live here:
//!
//! * [`tail`] -- test 1. Files whose length is not sector-aligned hash the same
//!   unbuffered as buffered. If this fails, nothing downstream is worth writing.
//! * [`cache_bypass`] -- test 2. Unbuffered reads come off the device rather
//!   than out of RAM. If this fails, verification is theater and every other
//!   guarantee in the program is worthless.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use super::unbuffered::{hash_buffered, hash_unbuffered, hex64, ALIGN, CHUNK};

/// Deterministic filler. Reproducible beats random for a diagnostic.
fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
}

fn write_file(path: &Path, size: u64, seed: u64) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    fill(&mut buf, seed);
    let mut left = size;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    // Force the bytes out of the OS dirty list, so a subsequent unbuffered read
    // measures the drive rather than lazy writeback. The copy pipeline does the
    // same thing per file for exactly this reason.
    f.sync_all().context("sync_all")?;
    Ok(())
}

/// A scratch directory that removes itself, so a failed run leaves no litter on
/// a destination drive.
struct Scratch(PathBuf);

impl Scratch {
    fn new(parent: &Path) -> Result<Self> {
        let dir = parent.join(".sluice-selftest");
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self(dir))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return f64::INFINITY;
    }
    bytes as f64 / 1.0e6 / secs
}

/// **Test 1** -- the sector tail, on a named drive.
///
/// Writes files whose lengths straddle every sector and chunk boundary that
/// matters, then checks the unbuffered hash against the buffered one. Also hashes
/// `real_file` if given: run this against an actual `.ARW` on an actual LaCie,
/// which is the case the design says to confirm before anything else matters.
pub fn tail(dir: &Path, real_file: Option<&Path>) -> Result<()> {
    let scratch = Scratch::new(dir)?;
    println!("test 1 -- sector-tail reads under FILE_FLAG_NO_BUFFERING");
    println!("  scratch: {}", scratch.path().display());

    let sizes: [u64; 12] = [
        0,
        1,
        (ALIGN - 1) as u64,
        ALIGN as u64,
        (ALIGN + 1) as u64,
        (CHUNK - 1) as u64,
        CHUNK as u64,
        (CHUNK + 1) as u64,
        // Multi-chunk, sector-aligned, but not chunk-aligned.
        (CHUNK + ALIGN) as u64,
        (2 * CHUNK + 1) as u64,
        // A 512-byte tail, which is what a 512e device would round to.
        6 * 1024 * 1024 + 512,
        // A tail in the middle of a sector, several chunks in.
        13 * 1024 * 1024 + 4095,
    ];

    let mut failures = 0usize;
    for (i, size) in sizes.iter().copied().enumerate() {
        let path = scratch.path().join(format!("tail_{size}.bin"));
        write_file(&path, size, 0x9E37_79B9_7F4A_7C15 ^ i as u64)?;

        let (unbuf, unbuf_len) = hash_unbuffered(&path)
            .with_context(|| format!("unbuffered hash of a {size}-byte file"))?;
        let (buf, buf_len) = hash_buffered(&path)?;

        let ok = unbuf == buf && unbuf_len == size && buf_len == size;
        if !ok {
            failures += 1;
        }
        println!(
            "  {:>12} B  unbuffered {}  buffered {}  {}",
            size,
            hex64(unbuf),
            hex64(buf),
            if ok { "ok" } else { "MISMATCH" }
        );
        let _ = fs::remove_file(&path);
    }

    if let Some(real) = real_file {
        let (unbuf, len) = hash_unbuffered(real)
            .with_context(|| format!("unbuffered hash of {}", real.display()))?;
        let (buf, _) = hash_buffered(real)?;
        let ok = unbuf == buf;
        if !ok {
            failures += 1;
        }
        println!(
            "  {:>12} B  unbuffered {}  buffered {}  {}   <- {}",
            len,
            hex64(unbuf),
            hex64(buf),
            if ok { "ok" } else { "MISMATCH" },
            real.display()
        );
    }

    if failures > 0 {
        bail!(
            "{failures} tail case(s) failed. The truncate-to-known-size fallback in \
             ChunkReader::next_chunk is not covering this device; do not build on it."
        );
    }
    println!("test 1 PASSED -- non-sector-aligned tails read correctly on this device");
    Ok(())
}

/// Outcome of the automatic half of test 2.
pub struct CacheBypass {
    pub warm_buffered_mbps: f64,
    pub unbuffered_mbps: f64,
    pub ratio: f64,
    pub verdict: BypassVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassVerdict {
    /// Unbuffered reads are clearly slower than cache reads: the bypass works.
    Proven,
    /// The device is fast enough to rival RAM, so the timing gap cannot
    /// distinguish a cache hit from a device read. Says nothing either way.
    Inconclusive,
    /// A slow device keeping pace with cache reads. The bypass is not happening.
    Failed,
}

/// Above this rate no spinning disk, SD reader, or USB bridge is plausible, so a
/// small ratio means "the device is too fast to measure this way" rather than
/// "the reads came from RAM". Deliberately far above the 130 MB/s LaCie and the
/// ~300 MB/s UHS-II readers, which are the media the verdict actually rests on.
const IMPLAUSIBLE_DEVICE_MBPS: f64 = 1000.0;

/// Read a file through the page cache, discarding the bytes. Timing only.
fn drain_buffered(path: &Path) -> Result<u64> {
    use std::io::{BufReader, Read};
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    Ok(total)
}

/// Read a file off the device, discarding the bytes. Timing only.
fn drain_unbuffered(path: &Path) -> Result<u64> {
    use super::unbuffered::{AlignedBuf, ChunkReader};
    let mut reader = ChunkReader::open(path)?;
    let mut buf = AlignedBuf::chunk();
    let mut total = 0u64;
    loop {
        let n = reader.next_chunk(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    Ok(total)
}

/// **Test 2, automatic half** -- prove unbuffered reads reach the device.
///
/// A buffered re-read of a file that was just written is served out of RAM and
/// runs at several GB/s. An unbuffered read of the same file must run at device
/// speed. If the two rates are comparable, `FILE_FLAG_NO_BUFFERING` is not doing
/// anything and every hash this program reports is a measurement of memory.
///
/// The rates measured here are *raw reads with no hashing*. Timing a hashing
/// read instead would make the comparison worthless whenever the hash is the
/// bottleneck -- which it is in a debug build, where xxHash64 runs at a few
/// hundred MB/s and flattens both numbers to the same value.
///
/// Keep `size_mb` comfortably smaller than free RAM, or the buffered read stops
/// being a cache read and the comparison loses its meaning.
pub fn cache_bypass(dir: &Path, size_mb: u64) -> Result<CacheBypass> {
    let scratch = Scratch::new(dir)?;
    let path = scratch.path().join("cache_bypass.bin");
    let size = size_mb * 1024 * 1024;

    println!("test 2 -- cache bypass, {size_mb} MiB on {}", dir.display());
    write_file(&path, size, 0xDEAD_BEEF_CAFE_F00D)?;

    // One untimed pass to guarantee the cache is warm, then the timed one.
    drain_buffered(&path)?;
    let t0 = Instant::now();
    drain_buffered(&path)?;
    let warm = mbps(size, t0.elapsed().as_secs_f64());

    let t1 = Instant::now();
    drain_unbuffered(&path)?;
    let unbuffered = mbps(size, t1.elapsed().as_secs_f64());

    // Correctness alongside the timing: the two paths must agree on the bytes.
    let (warm_hash, _) = hash_buffered(&path)?;
    let (unbuf_hash, _) = hash_unbuffered(&path)?;
    if warm_hash != unbuf_hash {
        bail!(
            "buffered and unbuffered hashes of an unmodified file disagree \
             ({} vs {}). Something is wrong with the read path itself.",
            hex64(warm_hash),
            hex64(unbuf_hash)
        );
    }

    let ratio = if unbuffered > 0.0 {
        warm / unbuffered
    } else {
        f64::INFINITY
    };
    println!("  buffered (warm cache): {warm:>9.1} MB/s");
    println!("  unbuffered (device):   {unbuffered:>9.1} MB/s");
    println!("  ratio:                 {ratio:>9.1}x");

    let verdict = if ratio >= 2.0 {
        BypassVerdict::Proven
    } else if unbuffered > IMPLAUSIBLE_DEVICE_MBPS {
        BypassVerdict::Inconclusive
    } else {
        BypassVerdict::Failed
    };

    match verdict {
        BypassVerdict::Proven => {
            println!("test 2 (automatic half) PASSED -- unbuffered reads run at device speed");
        }
        BypassVerdict::Inconclusive => {
            println!(
                "test 2 (automatic half) INCONCLUSIVE -- this volume reads at \
                 {unbuffered:.0} MB/s, fast enough to rival RAM, so the timing gap \
                 cannot separate a cache hit from a device read."
            );
            println!(
                "  This is not a failure. Re-run against the LaCie (~130 MB/s), where a \
                 cache hit would stand out by roughly 50x, and use \
                 `selftest cache-bypass-manual` for the proof that does not depend on \
                 timing at all."
            );
        }
        BypassVerdict::Failed => {
            bail!(
                "unbuffered reads are running at cache speed: {unbuffered:.0} MB/s against \
                 {warm:.0} MB/s cached, only {ratio:.1}x apart, on a device too slow for \
                 that to be genuine. FILE_FLAG_NO_BUFFERING is not reaching the device on \
                 this volume, so verification here would compare RAM to RAM.\n\
                 Check that --size-mb is well under free RAM, so the buffered read really \
                 is a cache read."
            );
        }
    }

    Ok(CacheBypass {
        warm_buffered_mbps: warm,
        unbuffered_mbps: unbuffered,
        ratio,
        verdict,
    })
}

/// **Test 2, manual half** -- the hex-editor proof.
///
/// Writes a file, reports both hashes, and waits. Flip a byte in the file with a
/// hex editor while the process holds, then press Enter. The unbuffered hash
/// must change. A buffered hash that does *not* change is the failure mode this
/// whole design exists to defeat, made visible.
pub fn cache_bypass_manual(dir: &Path) -> Result<()> {
    let scratch = Scratch::new(dir)?;
    let path = scratch.path().join("flip_me.bin");
    write_file(&path, 8 * 1024 * 1024, 0x0123_4567_89AB_CDEF)?;

    let (buf_before, _) = hash_buffered(&path)?;
    let (unbuf_before, _) = hash_unbuffered(&path)?;
    println!("test 2 (manual half) -- hex-edit proof");
    println!("  file:     {}", path.display());
    println!("  buffered:   {}", hex64(buf_before));
    println!("  unbuffered: {}", hex64(unbuf_before));
    println!();
    println!("  Flip one byte in that file with a hex editor, save, then press Enter.");
    println!("  (The file is deleted when this command exits, so work quickly.)");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading stdin")?;

    let (buf_after, _) = hash_buffered(&path)?;
    let (unbuf_after, _) = hash_unbuffered(&path)?;
    println!(
        "  buffered:   {}  {}",
        hex64(buf_after),
        verdict_word(buf_before != buf_after)
    );
    println!(
        "  unbuffered: {}  {}",
        hex64(unbuf_after),
        verdict_word(unbuf_before != unbuf_after)
    );

    if unbuf_before == unbuf_after {
        bail!(
            "the unbuffered hash did not change after the file was modified. Either the \
             edit did not land, or unbuffered reads are being served from cache on this \
             volume. Do not trust verification here until this is resolved."
        );
    }
    println!("test 2 (manual half) PASSED -- the unbuffered read saw the change");
    Ok(())
}

/// **The hardware check a test seam cannot replace.**
///
/// The clean SAFE TO FORMAT path is covered automatically by injecting a device
/// probe, but that proves the *logic*. Only real hardware proves that these two
/// drives, in these two ports, on this laptop, report as different physical
/// devices -- and a format verdict rests entirely on that being true.
///
/// Run once with the actual LaCies before relying on the verdict.
pub fn format_verdict(dests: &[PathBuf]) -> Result<()> {
    use super::win::{self, Distinctness};

    if dests.len() < 2 {
        bail!(
            "two destinations are required -- a format verdict is a claim about two \
             drives, so there is nothing to check with one"
        );
    }
    println!("format-verdict check -- can these destinations authorise an erase?");
    println!();

    let mut infos = Vec::new();
    for d in dests {
        let info = win::volume_info(d)
            .with_context(|| format!("reading volume identity for {}", d.display()))?;
        println!("  {}", d.display());
        println!(
            "    label         {}",
            if info.label.is_empty() {
                "(none)"
            } else {
                &info.label
            }
        );
        println!("    serial        {}", info.serial_hex());
        println!("    filesystem    {}", info.filesystem);
        match info.device_number {
            Some(n) => println!("    phys. device  {n}"),
            None => println!("    phys. device  UNAVAILABLE"),
        }
        println!(
            "    free          {:.2} GB",
            win::free_space(d).unwrap_or(0) as f64 / 1e9
        );
        println!();
        infos.push(info);
    }

    let mut worst: Option<String> = None;
    for i in 0..infos.len() {
        for j in (i + 1)..infos.len() {
            let verdict = win::distinctness(&infos[i], &infos[j]);
            let line = match &verdict {
                Distinctness::Distinct => format!(
                    "  {} vs {}: DISTINCT — these two can authorise a format",
                    infos[i].root, infos[j].root
                ),
                Distinctness::SameDevice => format!(
                    "  {} vs {}: SAME DEVICE — one copy, not two",
                    infos[i].root, infos[j].root
                ),
                Distinctness::Unproven(why) => {
                    format!("  {} vs {}: UNPROVEN — {why}", infos[i].root, infos[j].root)
                }
            };
            println!("{line}");
            if !matches!(verdict, Distinctness::Distinct) {
                worst = Some(line);
            }
        }
    }
    println!();

    if let Some(line) = worst {
        bail!(
            "these destinations cannot authorise a format:\n{line}\n\
             Every run against them will end in VERIFIED — DO NOT FORMAT, which is \
             correct but means no card can be erased here."
        );
    }
    println!(
        "PASSED -- distinctness is provable on this hardware, so a clean run here can \
         reach SAFE TO FORMAT."
    );
    Ok(())
}

fn verdict_word(changed: bool) -> &'static str {
    if changed {
        "changed"
    } else {
        "UNCHANGED"
    }
}
