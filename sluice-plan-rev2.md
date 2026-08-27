# sluice — verified camera offload tool

**Design & implementation plan — rev 2**
Target: usable and field-tested before anything depends on it.

> **Rev 2 changes.** The camera now runs simultaneous dual-slot recording, and
> cards are formatted on demand in the field. This makes the tool
> load-bearing rather than convenient, and it adds a capability rev 1 didn't
> have: the two cards are independent physical copies of the same data, so
> verification can compare them against each other. See §4 and §5.

---

## 1. Purpose

A single-binary Windows GUI that copies a camera card to two destination drives
simultaneously, verifies every byte actually landed on the physical media,
cross-checks the source against its in-camera twin, and issues an explicit
verdict on whether the card pair is safe to erase.

The intended use runs three 256GB SD cards with the camera writing both slots
simultaneously. That yields 256GB of unique content before a format becomes
necessary, so formatting in the field is expected. Once a card is erased, this
program's verdict is the only thing standing between the day's work and
permanent loss.

### Design goals

| Goal | Why |
|---|---|
| Correct over fast | The bottleneck is 130 MB/s HDDs. Nothing else matters. |
| Zero decisions at 11pm | Pick the folders, press one button, read one phrase. |
| Verbose by default | The verdict is one phrase; everything behind it is inspectable in real time. These are not in tension — see §10. |
| Never touch the source | The program has no format or delete path, by construction. |
| The tool owns the verdict | You should never reason about "is this safe to erase" yourself. |
| Manifest outlives the tool | MHL v1, plain XML, readable by other software. |
| Single binary | No installer, no runtime, copy it to the laptop and go. |

### Non-goals for v1

Renaming templates, EXIF-driven folder structures, proxy generation, cloud
upload, LTO, multi-card queueing, macOS/Linux. All of these are out of scope.

**Explicit non-goal: the program never formats anything.** It renders a verdict.
The human acts on it, in daylight, the next morning.

---

## 2. Hardware prerequisites

**Two SD readers, or one dual-slot UHS-II reader.** This is a new requirement in
rev 2 and it needs buying before a full field test. Twin-card verification requires both
cards mounted at once; a single-slot reader would force a card swap mid-job and
turn the best feature into a nuisance. Two cheap UHS-II readers on separate USB
ports work as well as one dual-slot unit.

Everything else is already in hand: two LaCie Rugged USB-C 4TB (STFR4000800),
three 256GB SD cards, the laptop.

### Camera configuration

- Slots 1 and 2 set to **simultaneous** recording (not relay).
- Both cards in a pair should be the same capacity — the camera stops when the
  smaller fills.
- Lossless-compressed RAW. Uncompressed roughly doubles every number below for
  no visible benefit.

### Capacity model

| Per shooting day | Stills | Video | Day total |
|---|---|---|---|
| Light (400 frames, 15 min) | 22GB | 11GB | ~33GB |
| Typical (800 frames, 30 min) | 44GB | 22GB | ~66GB |
| Heavy (1,500 frames, 45 min) | 83GB | 34GB | ~117GB |

With dual-write, unique capacity before a forced format is **256GB**, not 768GB:
two cards live in the camera and the third is a spare, so when the pair fills,
the spare covers one slot and the other must come from an erase.

Three shooting days at the typical rate is ~200GB — that completes with no
format at all. At the heavy rate you format exactly once, in the field, after two
clean verified nights. Timelapse sequences are the thing that breaks this model:
a single 1,800-frame interval run is ~100GB in one night. If those are planned,
count them explicitly and consider a fourth card.

---

## 3. Correctness invariants

These are the properties the program exists to guarantee. Everything in the
architecture follows from them.

1. **Verification reads the device, not the page cache.** Hashing a file you
   just wrote, through the normal I/O path, compares RAM to RAM. Every
   verification read uses `FILE_FLAG_NO_BUFFERING`.
2. **Both cards are hashed.** The camera wrote each file twice, to two separate
   pieces of NAND behind two separate controllers. Agreement between them is a
   far stronger guarantee than re-reading one card and hoping an error isn't
   repeatable.
3. **The source is never modified.** No move, no delete, no format. The code
   path does not exist and will not be added.
4. **A file is not "done" until it is verified on both destinations *and*
   matched against its twin.** Partial success is reported as failure.
5. **Manifests are written last**, after verification passes, so the presence of
   a manifest means the copy was good.
6. **The format verdict is a distinct, stricter state than "copied".** See §5.
7. **The machine cannot sleep mid-job.**

---

## 4. Architecture

### Phases

```
  SCAN ──► RECONCILE ──► PREFLIGHT ──► COPY ──► VERIFY ──► MANIFEST ──► VERDICT
```

**SCAN** — walk both cards, build file lists, sum bytes.

**RECONCILE** — *new in rev 2.* Compare the two card file lists. They should be
identical. Divergence is meaningful and is handled in §4.3.

**PREFLIGHT** — free space on all destinations, volume serials captured,
destination folders empty or resumable, keep-awake guard armed.

**COPY** — one read of card 1, fanned out to all writers.

**VERIFY** — concurrent unbuffered re-reads of card 1, card 2, and every
destination. Compare all.

**MANIFEST** — write MHL v1 + JSON session log into each destination root.

**VERDICT** — one of three states. See §5.

### 4.1 Copy pipeline

```
                          ┌─► bounded chan ─► writer A ─► LaCie A
  card 1 ─► reader thread ┼─► bounded chan ─► writer B ─► LaCie B
         (4 MiB chunks)   └─► bounded chan ─► writer C ─► laptop SSD (optional)
              │
              └─► xxHash64 (source-during-copy hash, in flight)
```

The reader streams `Arc<[u8]>` chunks so nothing is copied to fan out. Bounded
channels supply backpressure for free: when the slowest drive falls behind, the
reader blocks rather than ballooning memory. Cap of 4 gives ~16 MiB in flight
per destination.

Card 2 is not read during copy. It is read during verify.

Writer threads are long-lived and driven by a small protocol:

```rust
enum Msg {
    Open(PathBuf, u64),   // dest path, expected size
    Chunk(Arc<[u8]>),
    Close(FileTime),      // sync_all, then stamp mtime
    Stop,
}
```

`sync_all()` before close matters — it forces the data out of the OS's dirty
page list so the unbuffered verify read measures the drive, not lazy writeback.

**Optional third destination.** If the laptop has 200GB+ free, add it as a
writer. It costs nothing, needs no new hardware, and covers the window after a
card is formatted when the only two copies are same-model HDDs traveling in the
same bag. It is *not* required for a clean verdict — treat it as a bonus, and
recycle it day to day as space demands.

### 4.2 Verify pipeline

Four (or five) independent threads, each hashing one copy unbuffered, running
concurrently:

```
  card 1  ──► hash ──┐
  card 2  ──► hash ──┤
  LaCie A ──► hash ──┼──► comparison matrix ──► per-file verdict
  LaCie B ──► hash ──┤
  SSD     ──► hash ──┘
```

Card reads run at UHS-II speed (~250–300 MB/s) on separate USB ports and finish
well before the HDDs, which remain the long pole at 130 MB/s each.

### 4.3 The comparison matrix

Per file: `C1` and `C2` (the two cards, read during verify), `Sc` (card 1 hashed
in flight during the copy), and `A`, `B`, `C` (destinations).

| Condition | Diagnosis | Verdict |
|---|---|---|
| `C1 == C2 == A == B` | Clean | Pass; card pair erasable |
| `C1 != C2` | **One card is bad.** Identify which by comparing each to the destinations. | FAIL. Do not format either card. Retire the bad one for the rest of the shoot. |
| `C1 != Sc`, `C1 == C2` | Card 1 read is not repeatable — reader, cable, or contacts | FAIL. Re-run before concluding the card is bad. |
| `C1 == C2`, `A != C1` | Bad write or bad drive A | Retry file to A; recurrence means drive A is suspect |
| `A == B != C1 == C2` | Systematic — bus, cable, or RAM | Abort the job entirely |
| File on `C1`, absent on `C2` | Camera wrote one slot only (card full, or write error) | Copy proceeds; that file is **unverified against a twin** → no-format verdict |

The second row is the scenario that would otherwise silently destroy a day's
work, and simultaneous recording is what makes it detectable. It is the single
best reason rev 2 is a better design than rev 1.

**File-list divergence** deserves care rather than a hard failure. A card that
filled mid-session legitimately produces a shorter list. The rule: copy the
union of both cards, verify what can be verified, and let any unmatched file
suppress the format verdict for that session. Never let divergence silently
reduce the guarantee.

---

## 5. The format verdict

Rev 1 ended with VERIFIED or FAILED. Rev 2 needs a third state, because
"everything copied correctly" and "it is safe to erase the only other copies"
are different claims.

| State | Displayed | Conditions |
|---|---|---|
| Clean | **SAFE TO FORMAT** (green) | Every file matched across `C1`, `C2`, and both destinations; both manifests written; destinations report *different* volume serials |
| Copied but unproven | **VERIFIED — DO NOT FORMAT** (amber) | Destinations match card 1, but one or more files lacked a twin, or card 2 was absent |
| Failed | **FAILED** (red) | Any mismatch, with the specific diagnosis printed underneath |

The volume-serial check in row 1 is not paranoia: two identical LaCies can be
mounted such that both destination paths land on the same physical drive, and
that would produce a perfect-looking result with one copy. The program must
prove the two destinations are different devices before it blesses an erase.

### Field protocol on a SAFE TO FORMAT verdict

1. Spot-check by *opening* three or four RAWs and a video clip from **each**
   drive. A hash match proves the bytes; opening the file proves they were the
   right bytes and the folder structure is what you think it is.
2. Separate the drives physically — one in the camera bag, one in the room.
   Never both in the same vehicle compartment. Once a card is erased this stops
   being a nice-to-have and becomes the actual mitigation.
3. Format **in-camera**, oldest pair first, one card at a time. Never delete
   individual files.
4. **Do it the next morning, not the night of.** Formatting is the only
   irreversible action in the workflow and it should not happen at 11:30pm.

---

## 6. Module layout

```
sluice/
├─ Cargo.toml
└─ src/
   ├─ main.rs              entry point, eframe bootstrap
   ├─ ui/
   │  ├─ mod.rs            app state, event drain, layout
   │  ├─ theme.rs          visuals, embedded fonts, palette
   │  ├─ devices.rs        device strip + throughput sparklines
   │  ├─ pipeline.rs       live fan-out / backpressure monitor
   │  ├─ logpane.rs        virtualized log, filters, search
   │  └─ banner.rs         verdict banner
   └─ engine/
      ├─ mod.rs            Job, orchestration, phase sequencing
      ├─ telemetry.rs      Event enum, coalescing, JSONL writer
      ├─ scan.rs           card enumeration                     [written]
      ├─ reconcile.rs      twin-card file-list comparison       [new in rev 2]
      ├─ copy.rs           fan-out pipeline
      ├─ verify.rs         N-way concurrent verification + matrix
      ├─ verdict.rs        format-safety state machine          [new in rev 2]
      ├─ mhl.rs            MHL v1 emitter + JSON session log
      ├─ unbuffered.rs     aligned NO_BUFFERING reader          [written]
      └─ win.rs            keep-awake, free space, volume ID    [written]
```

### Status

| Module | State | Notes |
|---|---|---|
| `Cargo.toml` | drafted | Version pins need `cargo update`; egui API drifts fast |
| `win.rs` | drafted | KeepAwake, free_space, volume_id, find_by_serial |
| `unbuffered.rs` | drafted | AlignedBuf + hash_unbuffered |
| `scan.rs` | drafted | Whole-card walk minus OS litter; reusable for both cards |
| `reconcile.rs` | **to write** | Set comparison, union, divergence classification |
| `copy.rs` | **to write** | Reader + N writer threads, Msg protocol |
| `verify.rs` | **to write** | N concurrent hashers, comparison matrix |
| `verdict.rs` | **to write** | Three-state machine incl. volume-serial distinctness |
| `mhl.rs` | **to write** | XML emit with escaping, JSON log |
| `engine/mod.rs` | **to write** | Job type, phase driver, cancellation |
| `telemetry.rs` | **to write** | Event enum, 10 Hz coalescing, live JSONL sink |
| `ui/*` | **to write** | See §10. Most cuttable scope in the project. |

---

## 7. Windows specifics

### `FILE_FLAG_NO_BUFFERING`

Three constraints, all satisfied by aligning to 4096 (a superset of 512e and
4Kn):

- buffer address sector-aligned → custom `AlignedBuf` over `std::alloc`
- read length a multiple of sector size → 4 MiB chunks
- file offset a multiple of sector size → sequential reads only, never seek

**The one thing to test on day one:** the tail of a file whose length is not
sector-aligned. `ReadFile` is expected to return a short count at EOF rather
than erroring. This is relied upon widely but should be confirmed against a real
`.ARW` on a real LaCie before any of the rest matters. If it misbehaves, the
fallback is to read the final aligned block and truncate to the known file size.

Opening via `File::options().custom_flags(...)` means `std::io::Read` works
normally — no raw `ReadFile` calls needed, since std passes the buffer straight
through.

### Keep-awake

`SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` as an RAII guard
held for the job's duration, released on drop. Twenty lines that prevent the
most likely real-world data loss in the field.

### Volume identity

Two identical LaCie Rugged drives will swap letters between plug-ins. Capture
`GetVolumeInformationW` serial + label for each destination, display it
prominently, record it in the session log, and — new in rev 2 — **assert the two
destination serials differ** before issuing a format verdict.

### Free space

`GetDiskFreeSpaceExW` on all destinations during preflight. Refuse to start
rather than fail at 80%.

---

## 8. Output formats

### MHL v1 — `sluice_<session>.mhl` in each destination root

```xml
<?xml version="1.0" encoding="UTF-8"?>
<hashlist version="1.1">
  <creatorinfo>
    <name>Adam</name>
    <hostname>...</hostname>
    <tool>sluice 0.1.0</tool>
    <startdate>2026-03-14T22:14:03Z</startdate>
    <finishdate>2026-03-14T22:39:51Z</finishdate>
  </creatorinfo>
  <hash>
    <file>DCIM/100MSDCF/DSC00001.ARW</file>
    <size>62914560</size>
    <lastmodificationdate>2026-03-14T18:03:22Z</lastmodificationdate>
    <xxhash64be>a1b2c3d4e5f60718</xxhash64be>
    <hashdate>2026-03-14T22:31:12Z</hashdate>
  </hash>
</hashlist>
```

Forward slashes in paths, XML-escaped. `xxhash64be` is the u64 as 16 hex digits.
Standard format means other tools — and future-you without this binary — can
re-verify.

### JSON session log — `sluice_<session>.json`

Everything the MHL can't hold: volume serials and labels for both cards and all
destinations, per-phase durations, throughput, the full comparison matrix result
per file, the final verdict with its reasoning, and any failures with their
diagnosis. This is the forensic record if something surfaces at home.

**Rev 2 addition:** the log records which cards were formatted after which
session, entered by the user as a one-line confirmation. If a file is ever found
corrupt months later, this is how you reconstruct what happened.

---

## 9. Error handling

- **Per-file read/write error:** record, skip the file on all destinations,
  continue. The run is marked FAILED and the file is named.
- **Hash mismatch:** record with its diagnosis from §4.3. FAILED.
- **Twin mismatch (`C1 != C2`):** FAILED, and the verdict explicitly names which
  card is suspect so it can be retired for the rest of the shoot.
- **Systematic failure** (`A == B != C1`): abort the job.
- **No manifest is written for a failed run.** Manifest presence *is* the
  success signal.
- **Cancellation:** cooperative flag checked between chunks. Partial files on
  destinations are deleted on cancel; sources are untouched.
- **Resume:** if a destination file exists with matching size and mtime, hash it
  unbuffered and skip the copy if it matches. Makes an interrupted run cheap to
  restart.

---

## 10. UI

### Stance

Two audiences, same person at different moments. At 11:40pm after a long day you
need one unmissable verdict and nothing else. At every other moment — during the
build, during testing, and any time something looks wrong — you want to watch the
machine work in detail.

These aren't in tension if the layout is honest about which is which. The verdict
is a fixed banner that never scrolls and is never competing for attention. The
entire rest of the window is telemetry, and it is the default view, not a
disclosure triangle you have to go find.

**Nothing is hidden behind a "details" toggle.** If the program knows it, it's
on screen or in the log.

### Visual direction

Dark, near-black background, data-dense, no decorative chrome. egui's defaults
get replaced wholesale in `theme.rs`:

- Background `#0D1117`, panels `#161B22`, hairline separators at 1px
- One accent color for interactive elements; green/amber/red reserved
  exclusively for state, never decoration
- **Embedded monospace font** (JetBrains Mono or IBM Plex Mono, both OFL) shipped
  in the binary via `FontDefinitions` — no dependency on what's installed on the
  laptop, and no font substitution surprises at 11pm
- Fixed-width numeric columns, right-aligned, so figures don't jitter as they
  tick. This is the single biggest difference between a dashboard that reads as
  professional and one that reads as a toy
- Color is never the only signal — the verdict states carry distinct text, and
  log severity carries a glyph as well as a hue

### Layout

Four regions. Everything except the log is fixed-height; the log takes the
remainder.

**1 — Device strip.** One card per device (C1, C2, A, B, optional SSD):

```
┌─ CARD 1 ──────────┐┌─ CARD 2 ──────────┐┌─ DEST A ──────────┐
│ E:\  SONY_A       ││ F:\  SONY_B       ││ D:\  MT-A         │
│ exFAT · 4096B sec ││ exFAT · 4096B sec ││ exFAT · 4096B sec │
│ 1F3B9C04          ││ 90C4A711          ││ 3A2F0D18          │
│ 118.4 GB / 238.4  ││ 118.4 GB / 238.4  ││ 3.61 TB free      │
│ 287 MB/s ▂▄▆█▆▅▄▃ ││ 291 MB/s ▁▃▅▇█▆▄▂ ││ 128 MB/s ▅▆▆▅▆▆▅▆ │
└───────────────────┘└───────────────────┘└───────────────────┘
```

Volume serial is displayed permanently on every card. A letter swap between two
identical LaCies becomes visible *before* you press the button rather than after
the job completes against the wrong drive.

Sparklines are a 60-second rolling window, hand-drawn with `ui.painter()` —
about twenty lines, and it avoids taking `egui_plot` as a dependency that will
drift out from under us mid-project.

**2 — Pipeline monitor.** The fan-out, live:

```
reader   ████████░░░░  312 MB/s   DSC01204.ARW   41.2 / 62.9 MB
   ├─ A  queue ███░  3/4   128 MB/s   ◀ BLOCKING
   ├─ B  queue █░░░  1/4   131 MB/s
   └─ C  queue ░░░░  0/4   612 MB/s
```

Queue depth is channel occupancy, read straight off the bounded crossbeam
channels. It's the most useful number in the program: it names which device is
applying backpressure, which is the actual answer to "why is this slow" — and it
turns the architecture in §4.1 from a diagram into something observable. During
verify the same region shows the N concurrent hashers with their independent
progress.

**3 — Log pane.** Monospace, virtualized, sticky-to-bottom, takes all remaining
vertical space. This is the main event.

**4 — Verdict banner.** Fixed footer, always visible, largest text on screen.

### Log format

Timestamped, severity-tagged, fixed columns:

```
22:14:03.114  INFO   scan      card 1: 1,613 files, 91.4 GB in 812 ms
22:14:03.119  INFO   scan      card 2: 1,613 files, 91.4 GB in 799 ms
22:14:03.121  OK     recon     file lists identical, 1,613 matched
22:14:03.204  INFO   pre       D:\ 3.61 TB free, need 91.4 GB — ok
22:14:03.205  INFO   pre       G:\ 3.61 TB free, need 91.4 GB — ok
22:14:03.207  WARN   pre       dest A and dest B are both LaCie STFR4000800
22:14:03.209  INFO   pre       serials differ (3A2F0D18 / 7C190B4E) — ok
22:14:03.210  INFO   power     ES_SYSTEM_REQUIRED asserted
22:14:03.988  IO     copy      open  DCIM/100MSDCF/DSC00001.ARW  62,914,560 B
22:14:04.512  PERF   copy      A 128.1 MB/s  B 131.4 MB/s  q 3/4,1/4
22:14:04.996  OK     copy      DSC00001.ARW  1.008s  A ok  B ok  xxh 8f2a91c4…
...
22:31:12.004  OK     verify    DSC01204.ARW  C1 a1b2c3d4 = C2 = A = B   ✓
22:31:14.887  ERR    verify    DSC01207.ARW  C1 5e9f… ≠ C2 71ab…
22:31:14.888  ERR    verify    → card 2 disagrees with card 1 AND both dests
22:31:14.889  ERR    verify    → CARD 2 (90C4A711) IS SUSPECT — do not format
```

Filter chips across the top: `ALL · IO · PERF · OK · WARN · ERR`, plus a search
box that filters live. `ERR` alone should be the first thing you click when
something goes wrong, and it should be a short list.

**Virtualization is mandatory, not a nicety.** 1,613 files at several lines each
plus 10 Hz perf ticks over a 20-minute run is well north of 50,000 rows.
`ScrollArea::show_rows` renders only what's visible; naive rendering will drop
the UI to single-digit FPS and, worse, make it look like the copy stalled.

In-memory ring buffer capped at 50,000 rows. The **full** stream goes to disk as
JSONL continuously, flushed as it's written, so a crash or a yanked power cable
still leaves a complete record of what happened up to the instant it died.

### Event model

The engine↔UI interface. Worth pinning down now since `verify.rs` and
`reconcile.rs` both emit into it:

```rust
pub enum Event {
    Phase     { phase: Phase, at: Instant },
    Device    { id: DeviceId, info: DeviceInfo },   // label, serial, fs, sector, free
    FileStart { idx: usize, rel: PathBuf, size: u64 },
    Bytes     { dev: DeviceId, delta: u64 },        // coalesced to ~10 Hz
    Queue     { dev: DeviceId, depth: usize, cap: usize },
    Throughput{ dev: DeviceId, mbps: f32 },
    FileDone  { idx: usize, hashes: HashSet, dur: Duration },
    Log       { level: Level, stage: Stage, msg: String },
    Verdict   (Verdict),
}
```

**The engine never blocks on the UI channel.** High-frequency telemetry
(`Bytes`, `Queue`, `Throughput`) goes through `try_send` and is dropped on a full
channel — a stalled or minimized UI must never throttle the copy. `Log`,
`FileDone`, `Phase`, and `Verdict` are guaranteed delivery, and those are the
ones that also hit the JSONL sink. This split matters: dropped telemetry costs a
jittery sparkline, dropped log lines cost the forensic record.

Byte progress is coalesced inside the engine to ~10 Hz per device rather than
emitted per 4 MiB chunk. Repaint via `request_repaint_after(33ms)` while a job
runs and not at all when idle, so the app doesn't burn battery sitting open.

### Verbose levers

- **`--trace`** (and a UI toggle): adds per-chunk I/O lines, `sync_all` latency
  per file, aligned-buffer allocation counts, and per-read syscall timings. Off
  by default because it triples log volume; on whenever something is being
  diagnosed.
- **Full hashes on click.** Displayed truncated to 8 hex inline, full 16 on
  click-to-copy.
- **Export session bundle** — a zip of the JSONL, both MHLs, the device
  inventory, and system info. One button. This is what you'd attach to a bug
  report against your own code in six months.
- **Copy log to clipboard**, filtered or whole.

### Verdict banner

Fixed footer, never scrolls, largest element on screen:

```
╔══════════════════════════════════════════════════════════════╗
║  SAFE TO FORMAT                                              ║
║  1,613 files · 91.4 GB · twin-matched · 2 distinct volumes   ║
║  copy 8m12s · verify 8m31s · 17m03s total · avg 129 MB/s     ║
╚══════════════════════════════════════════════════════════════╝
```

It will be read by a tired person in a dim room who is about to make an
irreversible decision, and it should be legible from across the table. The
failure states get the same treatment with the specific diagnosis on the second
line — never a generic "verification failed."

## 11. Performance model

Assumptions: ~800 frames/day of a1 II lossless-compressed RAW plus 30 min video
≈ 66GB. UHS-II SD reads at ~250–300 MB/s. Each LaCie Rugged caps at 130 MB/s.

| Phase | Limiter | Time |
|---|---|---|
| Scan + reconcile | metadata only | <20 s |
| Copy | slower of two parallel writers, 130 MB/s | ~8.5 min |
| Verify | dest reads, concurrent, 130 MB/s | ~8.5 min |
| Manifest | negligible | <5 s |
| **Total** | | **~18 min** |

A heavy 117GB day comes to ~31 min. Adding card 2 to the verify phase costs
nothing in wall time — it finishes in ~4 min and overlaps the HDD reads
entirely. The strongest guarantee in the design is free.

xxHash64 runs at several GB/s; hashing is free relative to the disks. It's
chosen over BLAKE3 purely for MHL compatibility.

---

## 12. Test plan

Testing matters more than features here, and rev 2 raises the stakes: the tool's
verdict now authorizes an irreversible erase.

### Unit / synthetic

1. **Sector-tail read** — files of size 4095, 4096, 4097, and 8 MiB + 1 bytes,
   hashed unbuffered, compared against a normal buffered hash. Validates the
   riskiest assumption in the codebase.
2. **Cache bypass proof** — write a file, hash it unbuffered, then flip a byte on
   the destination with a hex editor while the OS still has it cached, and
   confirm a mismatch is reported. If it reports success, the verify is theater
   and everything else is worthless.
3. **Injected destination corruption** — flip one bit in dest A, confirm the
   diagnosis is "bad write, drive A" and not something vaguer.
4. **Injected twin divergence** — copy a card pair to disk images, flip a bit on
   the simulated card 2, confirm the verdict is FAILED, card 2 named as suspect,
   and no manifest written.
5. **File-list divergence** — delete a file from simulated card 2, confirm the
   union is copied, the verdict drops to VERIFIED — DO NOT FORMAT, and the
   unmatched file is named.
6. **Same-drive destinations** — point dest A and dest B at two folders on the
   *same* physical drive, confirm the verdict refuses to say SAFE TO FORMAT.
7. **Free space refusal** — point at a nearly-full volume, confirm preflight
   refuses.
8. **Cancellation** — cancel mid-file, confirm partial destination files are
   removed and both cards are byte-identical to before.
9. **Resume** — kill the process mid-run, restart, confirm completed files skip
   and the final manifest is correct.

### Field-realistic

10. **Full card pair, real hardware** — fill a 256GB pair with real frames,
    run the whole job to both LaCies, time it against the model in §11.
11. **Letter-swap** — unplug everything, replug in a different order, confirm
    the UI shows the swap before you press the button.
12. **Lid close** — start a job, close the laptop lid, confirm it keeps running.
13. **Cable yank** — pull dest B mid-copy. Confirm a clean specific failure, and
    that dest A plus both cards are unharmed.
14. **Manifest round-trip** — verify the emitted MHL against an independent
    tool, and re-verify a destination a week later from the manifest alone.

### Dress rehearsal

15. Two consecutive days of ordinary local shooting with dual-slot recording on,
    offloaded exactly as they will be in Shoot, on the actual laptop, with the
    actual readers and cables, at night, tired. **Including a real format of a
    real card pair on the second morning.** This is the test that finds the
    problems that matter, and it is the only one that exercises the full loop.

---

## 13. Scope and fallback

**Scope-cut rule.** The engine is finished and testable from a headless harness
before any UI work starts, so everything after that point is UI and is cuttable
in priority order: pipeline monitor first, then sparklines, then filters. The
log pane's virtualization and the verdict banner are not cuttable -- one is a
performance floor, the other is the entire point. If time runs out and the
device strip is still a plain text row, ship it that way.

**Hard rule:** robocopy + rclone remains the documented fallback, printed and in
the bag. If anything about sluice feels wrong in the field, fall back without
hesitation -- and if you are on the fallback, do not format anything. The manual
path verifies copies; it does not verify twins, and it should not be trusted to
authorize an erase.

---

## 14. Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| NO_BUFFERING tail-read misbehaves | Medium | Test 1 on day one; truncate-to-size fallback |
| egui API drift vs. pinned versions | High | `cargo update` early, fix once, commit `Cargo.lock` |
| Only one card reader available | Low | Without it there is no twin, so no erase can ever be authorised |
| Subtle bug that passes tests | Low | **This is now the top risk.** See below. |
| UI work crowds out testing | Medium | The engine is finished and headless-testable before any UI work; scope-cut rule in §13 |
| Log rendering stalls the UI | Medium | Virtualize from the first commit, not as an optimization pass |
| Over-engineering | High | Non-goals in §1 are binding |

**On the top risk.** In rev 1, cards were retained throughout, so total software
failure was a recoverable inconvenience. That safety net is gone by design.
Three things replace it: twin-card verification catches the failure mode that
single-source tools cannot see; the format verdict is deliberately stricter than
the copy result and refuses on any ambiguity; and the field protocol in §5 puts a
night's sleep and a human spot-check between the verdict and the erase. The
optional laptop SSD copy is a fourth, cheap layer for the most recent day.

None of that makes new code trustworthy on its own. Test 15 does — running the
complete loop, format included, on work you can afford to lose, before relying on it.
Until test 15 has been run, dual-write still happens but nothing gets formatted,
and the work completes on 256GB of unique capacity. Per §2, that is very likely
enough anyway.

---

## 15. Backlog

- Persist destination serials in config; resolve letter-from-serial at startup
- Verify-from-manifest mode (re-check a drive at home, months later)
- Card-health tracking: log per-card read error and mismatch counts across
  sessions, flag a card that misbehaves twice
- Third/fourth destination, NAS target
- EXIF-driven session naming and folder templates
- Linux build (only `win.rs` and the NO_BUFFERING layer are platform-specific;
  `O_DIRECT` is the analogue)
- Integration with a longer-term backup flow once the drives are back
