//! Unbuffered reads: `FILE_FLAG_NO_BUFFERING`.
//!
//! Correctness invariant 1 of the design: verification must read the *device*,
//! not the page cache. Hashing a file you just wrote, through the normal I/O
//! path, compares RAM to RAM and proves nothing at all.
//!
//! NO_BUFFERING imposes three constraints, all satisfied by aligning to 4096
//! (a superset of both 512e and 4Kn sector sizes):
//!
//!   * the buffer *address* must be sector-aligned -> [`AlignedBuf`]
//!   * the read *length* must be a sector multiple -> [`CHUNK`] is 4 MiB
//!   * the file *offset* must be a sector multiple -> sequential reads only
//!
//! The last one is why a short read before EOF is a hard error here rather than
//! something to loop past: it would leave the file offset unaligned and every
//! subsequent read would fail.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use xxhash_rust::xxh64::Xxh64;

/// Read granularity. A multiple of [`ALIGN`], so sequential reads keep the file
/// offset sector-aligned for free.
pub const CHUNK: usize = 4 * 1024 * 1024;

/// Alignment for both the buffer address and the read length. 4096 covers 512e
/// and 4Kn alike, so we never have to interrogate the device to stay legal.
pub const ALIGN: usize = 4096;

/// `windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING`
#[cfg(windows)]
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

/// A heap buffer whose address is [`ALIGN`]-aligned.
///
/// `Vec<u8>` will not do: the allocator hands back 8- or 16-byte alignment and
/// `ReadFile` rejects that under NO_BUFFERING.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the allocation is uniquely owned by this struct. No aliasing, no
// interior mutability, and `Drop` is the only thing that frees it.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// `len` must be a non-zero multiple of [`ALIGN`].
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "AlignedBuf length must be non-zero");
        assert!(
            len.is_multiple_of(ALIGN),
            "AlignedBuf length {len} is not a multiple of {ALIGN}"
        );
        let layout = Layout::from_size_align(len, ALIGN).expect("valid layout");
        // Zeroed rather than uninitialised: handing a `&[u8]` over uninitialised
        // memory to safe code is UB, and for a 4 MiB allocation the zero pages
        // come from the kernel anyway.
        //
        // SAFETY: `layout` has non-zero size.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        Self { ptr, len }
    }

    /// A fresh [`CHUNK`]-sized buffer, the unit the copy pipeline moves.
    pub fn chunk() -> Self {
        Self::new(CHUNK)
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, ALIGN).expect("valid layout");
        // SAFETY: `ptr` came from `alloc_zeroed` with this exact layout.
        unsafe { alloc::dealloc(self.ptr, layout) };
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialised bytes for our lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// Open a file for reading straight off the device.
///
/// Because std hands the caller buffer to `ReadFile` untouched, plain
/// `io::Read` works here and no raw syscalls are needed. The catch is that
/// anything interposing *its own* buffer (`BufReader`, `read_to_end`,
/// `read_exact`) reintroduces an unaligned pointer and breaks NO_BUFFERING.
/// Drive this handle through [`ChunkReader`], never directly.
pub fn open_unbuffered(path: &Path) -> Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        File::options()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)
            .with_context(|| format!("open unbuffered: {}", path.display()))
    }
    #[cfg(not(windows))]
    {
        // Placeholder for the Linux port in the backlog, where `O_DIRECT` is the
        // analogue. Reads here do NOT bypass the cache.
        File::open(path).with_context(|| format!("open: {}", path.display()))
    }
}

/// Sequential unbuffered reader over one file.
pub struct ChunkReader {
    file: File,
    len: u64,
    pos: u64,
}

impl ChunkReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_unbuffered(path)?;
        let len = file
            .metadata()
            .with_context(|| format!("stat: {}", path.display()))?
            .len();
        Ok(Self { file, len, pos: 0 })
    }

    /// File length as reported at open time.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read the next chunk into `buf`. Returns the byte count, or 0 at EOF.
    ///
    /// The tail is the interesting case. For a file whose length is not
    /// sector-aligned the final read still *requests* a full sector multiple.
    /// `ReadFile` is expected to return a short count; some stacks instead hand
    /// back the padded final sector. Both are handled by clamping to the known
    /// file length rather than trusting the returned count.
    pub fn next_chunk(&mut self, buf: &mut AlignedBuf) -> Result<usize> {
        assert_eq!(buf.len(), CHUNK, "chunk buffer must be exactly CHUNK bytes");
        if self.pos >= self.len {
            return Ok(0);
        }
        let remaining = self.len - self.pos;

        // Deliberately `read`, not `read_exact`: we want the raw count, so that
        // a short read mid-file is caught rather than silently stitched over.
        let got = self
            .file
            .read(&mut buf[..])
            .with_context(|| format!("unbuffered read at offset {}", self.pos))?;

        let n = if remaining >= CHUNK as u64 {
            if got != CHUNK {
                bail!(
                    "short unbuffered read: {got} of {CHUNK} bytes at offset {} with \
                     {remaining} bytes remaining. NO_BUFFERING requires the file offset \
                     stay sector-aligned, so this read cannot be resumed.",
                    self.pos
                );
            }
            CHUNK
        } else {
            let remaining = remaining as usize;
            if got < remaining {
                bail!(
                    "short tail read: {got} of {remaining} bytes at offset {}",
                    self.pos
                );
            }
            remaining
        };

        self.pos += n as u64;
        Ok(n)
    }
}

/// Hash a file by reading it off the device, bypassing the page cache.
///
/// Returns `(xxhash64, bytes_read)`.
pub fn hash_unbuffered(path: &Path) -> Result<(u64, u64)> {
    static NEVER: AtomicBool = AtomicBool::new(false);
    hash_unbuffered_cb(path, &NEVER, |_| {})
        .map(|r| r.expect("cannot be cancelled: the flag is permanently false"))
}

/// [`hash_unbuffered`] with cancellation and a per-chunk progress callback.
///
/// Returns `Ok(None)` if `cancel` was raised part-way through.
pub fn hash_unbuffered_cb(
    path: &Path,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(usize),
) -> Result<Option<(u64, u64)>> {
    let mut reader = ChunkReader::open(path)?;
    let mut buf = AlignedBuf::chunk();
    let mut hasher = Xxh64::new(0);
    let mut total: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = reader
            .next_chunk(&mut buf)
            .with_context(|| format!("hashing {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        on_bytes(n);
    }
    Ok(Some((hasher.digest(), total)))
}

/// Ordinary buffered hash, through the page cache.
///
/// This exists for the test harness only. It is precisely the measurement the
/// rest of this module refuses to trust: test 1 checks the two agree on quiet
/// data, and test 2 proves they *disagree* once the on-disk bytes change
/// underneath a warm cache.
pub fn hash_buffered(path: &Path) -> Result<(u64, u64)> {
    use std::io::BufReader;
    let file = File::open(path).with_context(|| format!("open: {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut hasher = Xxh64::new(0);
    let mut buf = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher.digest(), total))
}

/// MHL `xxhash64be`: the value as 16 hex digits, most significant nibble first.
pub fn hex64(h: u64) -> String {
    format!("{h:016x}")
}

/// The leading 8 hex digits, for inline log lines. Full value on click in the UI.
pub fn hex64_short(h: u64) -> String {
    format!("{:08x}", (h >> 32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn aligned_buf_is_aligned_and_zeroed() {
        for len in [ALIGN, CHUNK, 8 * ALIGN] {
            let buf = AlignedBuf::new(len);
            assert_eq!(buf.as_ptr() as usize % ALIGN, 0, "misaligned at len {len}");
            assert_eq!(buf.len(), len);
            assert!(buf.iter().all(|&b| b == 0));
        }
    }

    #[test]
    #[should_panic(expected = "not a multiple")]
    fn aligned_buf_rejects_unaligned_length() {
        let _ = AlignedBuf::new(4095);
    }

    /// Test 1, synthetic half: the sector tail.
    ///
    /// The riskiest assumption in the codebase is that `ReadFile` under
    /// NO_BUFFERING copes with a file whose length is not sector-aligned. If
    /// this fails, nothing downstream is worth writing.
    #[test]
    fn sector_tail_matches_buffered_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sizes = [
            0usize,
            1,
            ALIGN - 1,
            ALIGN,
            ALIGN + 1,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            2 * CHUNK + 1,
        ];
        for size in sizes {
            let path = dir.path().join(format!("tail_{size}.bin"));
            let data: Vec<u8> = (0..size)
                .map(|i| (i.wrapping_mul(31) % 251) as u8)
                .collect();
            let mut f = File::create(&path).expect("create");
            f.write_all(&data).expect("write");
            f.sync_all().expect("sync");
            drop(f);

            let (unbuf, unbuf_len) = hash_unbuffered(&path).expect("unbuffered hash");
            let (buf, buf_len) = hash_buffered(&path).expect("buffered hash");
            assert_eq!(unbuf_len, size as u64, "byte count mismatch at size {size}");
            assert_eq!(buf_len, size as u64);
            assert_eq!(unbuf, buf, "hash mismatch at size {size}");
        }
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex64(0xa1b2_c3d4_e5f6_0718), "a1b2c3d4e5f60718");
        assert_eq!(hex64(1), "0000000000000001");
        assert_eq!(hex64_short(0xa1b2_c3d4_e5f6_0718), "a1b2c3d4");
    }
}
