# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this program is

sluice copies camera cards to backup drives, verifies every byte off the physical
media, and issues a verdict on whether the originals are safe to erase. **Its
output authorises destroying the only other copy of somebody's work.** That single
fact decides most design arguments here: an unproven claim is treated exactly like
a disproven one, and every rule fails toward not-formatting.

`sluice-plan-rev2.md` is the design spec. The implementation deviates from it in
eight places, each documented under "Where it deviates from the design" in
`README.md` — read that section before assuming the spec is authoritative.

## Commands

```bash
cargo build --release              # the shipping binary; --locked in CI
cargo test                         # 240 unit + 26 integration
cargo clippy --all-targets -- -D warnings
cargo fmt --all

cargo test <substring>             # single test by name
cargo test --lib verdict::         # one module's unit tests
cargo test --test offload <name>   # one integration test
cargo test -- --nocapture          # see println! from tests
```

Run it:

```bash
cargo run -- doctor                # machine diagnosis; first thing when something breaks
cargo run -- run --card1 <dir> --dest <dir> --label x --log-dir <dir>
cargo run -- verify --drive D:\    # every session folder on a drive, one exit code
cargo run -- verify --manifest <path.mhl>
cargo run -- history [--sessions]  # what every card and drive has done
cargo run -- clean [--keep 30]     # prune session logs; never history.jsonl
cargo run --release -- selftest tail .        # the day-one unbuffered-read proof
```

Other selftests: `cache-bypass`, `cache-bypass-manual`, `format-verdict`,
`volumes`, `lid`.

`verify --drive` re-checks **every** manifest in a folder, not the first one
found: a shoot day that offloads three card pairs under one label writes three,
each listing only its own run. Exit 0 means every folder was opened and every
file matched; damage, files no manifest vouches for, and a cancelled sweep each
get their own non-zero code.

## Architecture

`src/engine/` is the whole program; `src/ui/` is a view over it. `engine::run_job`
is the entire engine API and runs headless — that is what keeps the UI cuttable and
what the integration tests drive.

### The phase pipeline

`scan` → `reconcile` → preflight → `copy` → `verify` → `verdict` → `mhl`

- **`scan`** walks a volume, minus OS litter, and records name hazards (case
  collisions, reserved device names, cloud placeholders) that preflight refuses on.
- **`reconcile`** compares the two cards and produces the copy list as their
  *union* — a file present only on card 2 has no card-1 source, so the reader picks
  a source per file. It also infers what the camera was doing (backup / relay /
  split), which is what turns "1,613 files have no twin" into an explanation.
- **`copy`** is one read of the source fanned out to N writer threads over bounded
  channels. Chunks travel as `Arc<Chunk>`, so fan-out copies a pointer. Each
  destination also gets a *syncer* thread; see "The flush is the copy phase's
  real cost" below before touching it.
- **`verify`** re-reads everything and calls `diagnose`.
- **`verdict`** turns the diagnoses into one of five states.

### The six hashes

The correctness core is `verify::diagnose`, which decides what a set of
disagreeing hashes *means*:

| | source |
|---|---|
| `Sc` | the copy's in-flight hash of the source |
| `C1`, `C2` | each card, re-read afterwards |
| `A`, `B`, `C` | each destination, read back |

`Sc` and `C1` are two genuinely independent trips to the device — that is why the
copy path reads unbuffered too, and it is what makes an unrepeatable read
detectable. `diagnose` only ever compares hashes for equality, so three distinct
values plus absence cover every equivalence class; the test enumerates all 4,096
tuples rather than sampling.

### The verdict is the safety boundary

Five states. `Verdict::authorises_erase()` is true for **exactly one** of them and
is the single place that decides. Faults (a drive dropping writes, two
destinations on one disk) decide the tier; structural gaps (one card, one
destination) are always *reported* even when a fault outranks them.

When touching `verdict.rs`, keep the exhaustive test in
`nothing_but_the_complete_arrangement_authorises_an_erase` passing — it asserts the
property over every combination of inputs rather than over the cases that existed
when it was written.

### Two cards are not byte-identical, and that is correct

A Sony body recording the same take to both slots writes a different **UMID** to
each — the SMPTE 330M identifier for a material *instance*, and two cards are two
instances. Measured on real hardware, that single fact accounts for every
difference between a twin pair: the RTMD track carries the UMID once per frame,
the `meta` box carries it again in an embedded clip XML, and `MEDIAPRO.XML` /
`*M01.XML` carry it as attributes. The camera is not malfunctioning.

Without `account.rs`, sluice could therefore **never** reach SAFE TO FORMAT for
two-slot video — it failed on an identifier, and told the operator `CARD 2 IS
SUSPECT` about a healthy card.

`account::account` decides whether a twin difference is confined to metadata, and
dispatches on the file's own kind. Two things about it are load-bearing:

- **It proves, it does not tolerate.** For an `.mp4` it walks the container's
  sample tables and requires that *not one differing byte* lies inside a video or
  audio sample — a positive statement about the footage, not an assumption that
  the difference is harmless. The tables are read only from a `moov` that is
  byte-identical on both cards, and that `moov`'s digest must be reproduced by
  the unbuffered pass, so the parse is anchored to the bytes actually verified.
  For `.xml` it requires every differing byte to fall inside the value of a
  `umidRef` / `umid` / `mediaId` attribute, with both cards producing the same
  spans at the same offsets.
- **Anything else refuses**, and a refusal is exactly today's behaviour: the file
  keeps its `TwinMismatch` and the run keeps its `Failed`.

The rule is deliberately per-file-kind and never size-based. A size-based version
was written and **failed its own safety test**: `injected_twin_divergence_fails_
and_names_card_two` flips a bit in a 26-byte `.ARW`, and a "small files may
differ" rule preserved the corrupted copy and dropped it from the failures. A
real ARW is 25–80 MB, so that rule would have masked a flipped bit in a
photograph. Do not reintroduce a rule keyed on file size.

`PRIVATE/DATABASE/DATABASE.BIN` is the one file nothing can be proven about — 212
bytes differ across 143 runs of a 9.67 MB vendor index with no published
structure. It is not reasoned about: `account::CARD_INDEX_PATHS` names it
exactly, and the other card's copy is **kept** under `_twin/<card>/<rel>`,
verified back off each destination and entered in the manifest. Both cards are
then genuinely on both drives, so SAFE TO FORMAT is true rather than relaxed.

That list is **exact relative paths, never a size or a pattern**, and the
distinction is the whole safety argument: matching the path cannot reach a
photograph, a clip, or anything else that is somebody's work. Widen it one
observed filename at a time, on evidence.

`MAX_DIFFERING_FRACTION` is 2% against a measured 0.58%. A low-bitrate proxy is
also `.MP4` with the same per-frame metadata over far fewer picture bytes, so its
excused share climbs steeply — that configuration is **unmeasured** and refuses.
Measure a proxy pair before touching the number.

### Unbuffered reads are load-bearing

`FILE_FLAG_NO_BUFFERING` is what makes "verified off the physical media" true
rather than a re-read of the page cache. It requires 4096-byte buffer alignment,
sector-multiple lengths, and sector-aligned offsets. **Drive the handle through
`unbuffered::ChunkReader` only** — anything interposing its own buffer
(`BufReader`, `read_to_end`, `read_exact`) reintroduces an unaligned pointer and
silently breaks the guarantee. `ChunkReader::next_chunk` clamps to the known file
length rather than trusting the returned count, because some stacks hand back a
padded final sector.

This is also why network destinations cannot contribute to a verdict: over SMB the
flag is advisory. See `DriveType::verification_reaches_the_device`.

### The flush is the copy phase's real cost

Getting bytes onto a platter costs far more than writing them, and the shape of
the flush decides the throughput of the whole phase. Three things were measured
on a LaCie Rugged, 2 GiB each time, and each one is counter-intuitive enough
that changing it back looks like a simplification:

| | 256 × 8 MiB | 2048 × 1 MiB |
|---|---|---|
| `sync_all` per file, inline (the original) | 44.0 MB/s | 11.1 MB/s |
| flush on a syncer thread, as files arrive | 44.9 MB/s | 12.0 MB/s |
| flush after the writing, stamp per file | 55.0 MB/s | 14.5 MB/s |
| **flush after the writing, stamp in a second pass** | **80.8 MB/s** | **69.5 MB/s** |

So, in order:

- **Flushing per file idles the drive.** `FlushFileBuffers` makes the device
  commit its cache — 80–150 ms — and a writer waiting on that sends nothing.
- **Moving it to a thread is not enough.** The flush then competes with the
  writes for the same drive, lazy writeback never gets ahead, and every flush
  still pays a full commit. `syncer_thread` therefore does *not* drain as files
  arrive: it collects the whole pass, then flushes. That looks like a missing
  optimisation and is the entire point.
- **Stamping the mtime immediately after a file's own flush costs 37 ms a file
  against 2 ms**, because the metadata write lands on a file the drive has just
  committed and forces a second commit. Hence two passes: flush everything,
  then stamp everything.

The stamp order is a safety property, not a performance one. Resume trusts size
and mtime, so nothing may wear the source mtime until its bytes are durable —
otherwise a power cut leaves a full-length, correctly stamped, half-written file
that resume skips and nothing re-copies. Two passes preserve that for the whole
destination at once. The stamps themselves are deliberately not flushed: losing
one means the file is copied again, which is the direction every rule here fails
in.

The flush queue is the one unbounded channel in the program, and has to be —
the syncer does not drain until the writer has finished, so any bound deadlocks
the copy on a card with more files than the bound.
`the_flush_queue_never_makes_a_writer_wait` exists to say so, and uses
`try_send` so a reintroduced bound fails the suite instead of hanging it.

`Phase::Flush` is separate from `Phase::Copy` because it moves no bytes and the
monitor measures bytes: inside `Copy` it read "about 0s left" for its whole
duration, which is the lie verify's denominator used to tell.

### What is *not* worth changing, measured

Two plausible-looking optimisations were tested and rejected, so they do not get
re-proposed:

- **Pipelining the read and the hash.** On an NVMe it is worth 35% (4.0 → 5.4
  GB/s), which is why it looks compelling. On the drives that actually gate a
  run it is worth 2%: a LaCie reads at 139 MB/s and hashes at 14 GB/s, so the
  hash is never the constraint. Chunk size is the same story — 4 MiB to 16 MiB
  buys 4% on the drive and costs 4× the in-flight memory.
- **Unbuffered destination writes.** Attractive because it would remove the
  page-cache copy and the flush both. Measured at 2 GiB: buffered plus a flush
  pass beats it at 8 MiB files (88.7 vs 78.0 MB/s) and ties at 1 MiB, so the
  alignment and set-length complexity buys nothing.

Beware benchmarking either of these at 512 MiB: everything fits in the write
cache and unbuffered looks far better than it is.

### Telemetry has two consumers with different guarantees

`engine::telemetry` emits once into one channel. The **JSONL sink** is the forensic
record and must not lose a line — it is written to the laptop, never to a
destination drive, because test 13 yanks a destination mid-copy. The **UI** is a
view, so UI sends are `try_send` while the sink send blocks: a wedged window can
never throttle a copy. Only `Bytes`, `Queue` and `Throughput` may ever be dropped
— see `Event::is_droppable`.

One caveat if you touch this: `Bytes` now feeds a **cumulative** UI counter, so a
drop no longer costs one sparkline sample — it skews progress and the estimate
permanently. The UI channel holds 65,536 events (~27 minutes) and the log header
reports `Sink::ui_dropped`, so it is visible rather than silent. Making it
self-healing means emitting an absolute total rather than a delta.

### What the monitor is measuring

Each phase measures its own bytes against its own seconds; counters reset on the
`Phase` event. The two phases also count differently, which `pipeline::moved_in_phase`
encodes: **copy** fans one read out to every destination, so each destination
moves the whole session and the furthest-along one is the progress; **verify**
re-reads every copy independently, so its work is the *sum* of all streams and
its total is bytes × streams — four times the copy on a two-card, two-drive
night. The engine emits that total as a second `Event::Plan` at the phase change.

The estimate uses a rolling window, not the phase average: the hashers do not
finish together (cards on internal disks finish in under a minute, USB drives
take several), so an average is dominated by streams that stopped moving bytes.
Rates shown to the operator are smoothed and go to zero when a device falls
silent — a 100 ms tick holds either one 4 MiB chunk or two, so raw samples swing
2× on a steady drive, and a finished hasher simply stops emitting.

## Rules specific to this codebase

**Test seams are constructor parameters, never runtime switches.** `JobConfig`
carries `probe: Option<Arc<dyn DeviceProbe>>` and `history_path: Option<PathBuf>`.
Do not add an environment variable or config key that can influence a verdict — a
switch able to fake device distinctness in a shipped binary is precisely the thing
that could bless a bad format. Anything a test needs to redirect goes on
`JobConfig`.

**Predicates that partition one set must call each other, never restate each
other.** `copy::would_overwrite` decides "is this a different file, refuse the
run", and `copy::already_present` decides "is this the same file, skip it". They
cover the same set, so a file either is or is not skipped — and anything not
skipped gets opened with `File::create`, which truncates. They drifted once:
`already_present` learned that a dehydrated cloud placeholder is not a finished
copy, `would_overwrite` kept its own copy of the size-and-mtime rules, and a
OneDrive placeholder was then *neither* skipped nor refused. It was truncated,
and deleted outright if the run was cancelled or hit a bad sector. The guard now
calls the resume check. Keep it that way; a second copy of the rules is a second
chance to diverge, and `resume_and_the_overwrite_guard_never_disagree` exists to
say so.

The same shape applies elsewhere: **do not rebuild a path that was already
computed.** `record_format` reconstructed the session-JSON name from the session
id while the file on disk carried a disambiguated one, so it stamped an *earlier*
run's record. `JobOutcome::session_json` now carries the paths the run actually
wrote.

**Anything writing outside the session folder needs a seam.** `run_job` appends to
a per-device history that preflight then *warns* from. Tests must redirect it
(`history_path`), or the suite trains the warning system on fabricated data.

**Two manifest dialects.** MHL v1.1 (`sluice_<session>.mhl`) is the success signal
and the one sluice re-verifies; ASC MHL v2 (`ascmhl/`) exists so other tools can
read it. Their relative paths are measured from different folders — v1 from the
manifest's own directory, ASC from the folder *above* `ascmhl/`. See
`recheck::default_root`. Anything sluice writes to a destination must also be
recognised by `recheck::is_sidecar`, or every re-verification reports it as a file
nothing vouches for.

Three things follow from a manifest being the one artifact sluice **reads but did
not write**, and the README inviting manifests from other tools:

- **Its paths are untrusted input.** `mhl::is_contained_rel` refuses any entry
  that is not purely ordinary components. Before it, `../outside.txt` was read,
  hashed and reported on — no privilege boundary crossed, but an INTACT verdict
  about a folder was vouching for a file that folder does not contain.
- **`ascmhl/` may belong to somebody else.** `ascmhl_chain.xml` is a filename the
  spec fixes, and the format exists so several tools can share a folder.
  `mhl::foreign_ascmhl` makes sluice write nothing into a directory it did not
  create, rather than replacing another tool's chain with its own dialect.
- **A manifest is never overwritten.** Session ids are second-granular, so two
  offloads inside one second shared a name and the second replaced the first.
  `unique_file_session` writes alongside and warns.

**Windows-only by construction**, enforced by a `compile_error!` in `src/lib.rs`.
There is a `#[cfg(not(windows))]` branch in `unbuffered.rs` that does *not* bypass
the cache; it is an unreachable placeholder for a backlogged port.

**Colour is never the only signal.** The verdict band's words, glyph and hue each
carry the answer alone, and a test asserts the state that authorises an erase never
shares a colour *or* a mark with one that does not.

**Stored timestamps are UTC; shown timestamps are local.** The JSONL `at` field
and the MHL dates stay UTC-with-`Z`, because a forensic record has to be
unambiguous years later and the MHL schemas require it. Everything an operator
reads goes through `telemetry::local_time` / `local_stamp` / `local_date` — never
format a stored `DateTime<Utc>` directly. Mixing them is worse than either: log
lines read `19:44` at `15:44`, three inches from a folder named for the local
date. Each run states its offset in its first line.

## Testing notes

Some things cannot be tested the obvious way, and the workarounds are load-bearing:

- **Real device distinctness** cannot be synthesised — no temp dir, `subst` mapping
  or second partition produces two physical devices, and the check correctly
  rejects all of them. The passing SAFE TO FORMAT path is reached only by injecting
  a `DeviceProbe`. Only real hardware proves the real thing:
  `sluice selftest format-verdict --dest D:\ --dest G:\`. **This has now been run**
  on two LaCie drives — distinct disks, unelevated, SAFE TO FORMAT reached for
  real, a single flipped bit caught on one drive with the other clean, and two
  destinations on one disk refused. What is still unproven is the **card-reader
  path**: every run so far used fixed disks standing in for cards, and a real SD
  card through a reader adds bridge behaviour under `FILE_FLAG_NO_BUFFERING`,
  unrepeatable reads and sector errors that no simulation reproduces.
- **A test must construct its premise, never hope for it.** Two flakes here were
  the same mistake in different clothes, and both failed only under load — which
  is to say, on somebody else's CI rather than on this desk.

  `Mtime::matches` has a two-second tolerance, so a test whose premise is "these
  two files look identical to resume" must *stamp* the mtimes
  (`filetime::set_file_mtime`) rather than write them quickly. And a test that
  has to act *during* a phase must observe that phase — the cancellation test
  slept 40 ms and hoped it landed inside the copy; under load it landed in
  preflight, `run_job` correctly returned a refusal instead of a verdict, and the
  rig panicked unwrapping it. It now waits for a destination file to exist, which
  the writer creates at `Msg::Open` before the first chunk.

  Note the second failure mode in that one: an early cancel means no partial was
  ever written, so the assertion the test exists for was **vacuous even when it
  passed**. A suite that passes on retry can hide a regression in the thing this
  program exists to guarantee.
- **Case-colliding filenames** cannot be created on NTFS — the kernel merges them
  before the scan sees them — so they are covered at the unit level only.
- **Reserved names and trailing dots** *can* be created through the `\\?\` prefix,
  which bypasses Win32 name normalisation. `tests/offload.rs` uses this to build
  the cards the real world delivers.

## Workflow gotchas on this machine

- `core.autocrlf=true`. Files come out of a checkout as CRLF; edits written as LF
  diff cleanly, but a script matching on exact text must normalise first.
- Bash heredocs here collapse doubled backslashes, which silently corrupts Rust
  string literals and `\\?\` paths. Use the Write/Edit tools for anything
  containing backslashes.
- PowerShell 5.1 reads UTF-8-without-BOM as ANSI; a `Get-Content`/`Set-Content`
  round-trip mangles non-ASCII. Use `[IO.File]::ReadAllText`/`WriteAllText` with
  `UTF8Encoding($false)`.
- `strings`, `gh` and `python` are not installed. Inspect the binary by reading
  bytes in PowerShell; CI status has to be checked in a browser.
- **Windows locks `target/release/sluice.exe` while the GUI is running**, so
  `cargo build --release` fails until the window is closed. Debug builds and
  `cargo test` use different artifacts and are unaffected — do those first, and
  ask for the window to be closed before rebuilding the shipping binary.
- Commit *before* the final `cargo build --release`, or the build stamp reads
  `<commit>+` and says it came from a modified tree that matches no commit.
