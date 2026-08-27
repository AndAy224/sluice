# sluice

Verified camera offload for Windows. Copies one or two camera cards to one or
more destination drives, verifies every byte off the physical media, cross-checks
the cards against each other where there are two, and issues an explicit verdict
on whether the originals are safe to erase.

Design: [sluice-plan-rev2.md](sluice-plan-rev2.md). This file records what is
built and what is not. Threat model: [SECURITY.md](SECURITY.md).

## Running it

```
sluice                                  open the window
sluice --card1 E:\ --card2 F:\ --dest D:\ --dest G:\ --label shoot-01 [--start]
                                        open it with the night's setup filled in
sluice run --card1 E:\ [--card2 F:\] --dest D:\ [--dest G:\] --label shoot-01
                                        the same job with no window
sluice verify --drive D:\             re-check every session folder on a drive
sluice verify --manifest <path.mhl>     is this drive still what it was?
sluice history [--sessions]             what every card and drive has done
sluice clean [--keep 30]                prune old session logs
sluice doctor                           why won't it work on this machine?
sluice licenses                         sluice's licence, and the font's
sluice --help                           everything else
```

`sluice run` exits `0` only when erasing the originals is authorised; see
**The verdict** below for the full code map.

Flags before a subcommand open the window pre-filled, so a desktop shortcut can
carry the setup; `--start` begins immediately. After the first run the window
remembers its own setup anyway — see **Remembering the setup** below.

## The verdict

This is the whole program. Everything else exists to make one of five sentences
true.

| Verdict | What was proven | Erase? |
|---|---|---|
| **SAFE TO FORMAT** | Two cards written simultaneously agree with each other and with two copies on two different physical drives. | yes |
| **VERIFIED — ONE SOURCE** | Every file is on two separate drives and matches two independent unbuffered reads of the card. Rules out a bad transfer and a bad destination; cannot rule out a card that returns the same wrong bytes twice. | no |
| **VERIFIED — ONE COPY** | Every byte verified, but it exists in one place. | no |
| **VERIFIED — DO NOT FORMAT** | The copy is good and something is wrong: a drive dropping writes, two destinations on one disk, identity that could not be proven. | no |
| **FAILED** | Something disagreed, with the specific diagnosis attached. | no |

Only the first authorises an erase, and that is decided in exactly one place
(`Verdict::authorises_erase`), tested over every state that exists rather than
over the states that existed when the test was written.

`sluice run` reports the verdict as its **exit code**, so a wrapper can act on it
without parsing anything:

| | |
|---|---|
| `0` | SAFE TO FORMAT — and nothing else ever exits 0 |
| `10` | VERIFIED — ONE SOURCE |
| `11` | VERIFIED — ONE COPY |
| `12` | VERIFIED — DO NOT FORMAT |
| `20` | FAILED |
| `1` | refused before starting, or an error |

This matters more than it looks. Until it existed only FAILED was non-zero, so
`sluice run … && erase-card` succeeded on a **do-not-format** verdict.

The three middle rows used to be one. Collapsing them was right for the hardware
rev 2 was designed around — a dual-slot camera and two LaCies — and wrong for
everybody else, because it says the same thing to a photographer who did
everything their hardware allows as to somebody whose drive is failing. A signal
that fires every single night is one people learn to click past, and then it is
not there on the night it means something. **The safety property is unchanged;
the two new tiers are a more honest way of saying no.**

### Choosing drives

Each row has a drive picker (`▾`), a folder browser (`…`), and the path itself.
The picker lists what is mounted **by identity rather than by letter**:

```
D:\   MT-A  ·  3A2F0D18  ·  exFAT  ·  disk 2  ·  3610 GB free
G:\   MT-B  ·  7C190B4E  ·  exFAT  ·  disk 4  ·  3610 GB free
Z:\   nas   ·  1C4409AA  ·  NTFS   ·  disk ?  ·  9100 GB free  —  network, cannot be verified off the device
```

That ordering is deliberate. Two identical LaCie Rugged drives swap letters
between plug-ins, so `D:` is the least trustworthy thing on the line and the
serial is the most. Card rows sort removable media to the top, destination rows
sort fixed drives to the top, and optical drives and empty reader slots are not
offered at all.

Things that used to be discoverable only after a twenty-minute run:

- Whatever each field points at is resolved **live** — label, filesystem, serial,
  physical disk, before anything starts.
- A drive already used by another slot is flagged inside the dropdown —
  `— same drive as DEST A`.
- Two required destinations on one physical disk raise an amber line above the
  Offload button, not an explanation afterwards.
- **A network location says so on its own line.** See below for why that matters.

`Rescan` re-enumerates after plugging something in.

### What it refuses before starting

Rather than failing at 80% or, worse, succeeding wrongly:

- **A session folder that already holds someone else's work.** The folder is
  named from the date and the label alone, and the label is remembered and
  pre-filled — so a second card pair on the same day lands in the first pair's
  folder, and two camera bodies both number from `DSC00001.ARW`. Before this
  check the colliding frames were truncated in silence, the run verified its own
  files perfectly, and the verdict was SAFE TO FORMAT with the morning's work
  already gone. sluice now refuses, names every file, and tells you to change the
  label. Resume is untouched: a folder holding *this* run's files, byte for byte,
  is resumable rather than occupied.
- **Filenames Windows cannot store faithfully.** Two files differing only by
  capitalisation land on one NTFS path and the second silently overwrites the
  first — and verification then reports `Systematic`, which means *your bus, RAM
  or controller is corrupting data*, sending somebody off to buy hardware over a
  filename. Also reserved device names (`CON`, `NUL`, `COM1`), characters NTFS
  rejects, and components ending in a dot or a space, which Windows silently
  renames so the manifest no longer matches what landed. None of these come off a
  camera card; all of them come off a folder somebody picked by hand.
- **Cloud placeholders.** A OneDrive or Dropbox file whose bytes live in the
  cloud. Hashing one is a download, not a read, and "verified off the physical
  media" would be a false claim about it.
- **A destination that cannot be written.** Probed with a real file before the
  copy starts. An access-denied inside Documents, Pictures, Videos or Desktop
  names Controlled Folder Access and how to get past it, because Windows reports
  its own ransomware protection as a generic `os error 5` and that is the most
  likely way a first run fails on somebody else's machine.
- **The same card in both slots**, which would make the twin check compare a card
  against itself and report SAFE TO FORMAT having verified nothing.
- **A card slot pointed at the root of a fixed disk.** `--card1 C:\` used to be
  accepted without a word, and would go on to copy and unbuffered-verify several
  hundred gigabytes of Windows. A folder on a fixed disk is still fine — that is a
  staging directory.
- **Arguments it does not understand.** `--destination` instead of `--dest`
  silently halved the number of copies; `--label --trace` produced a session
  folder literally named `--trace` with tracing still on.

### What it warns about

- **Network destinations.** Over SMB, `FILE_FLAG_NO_BUFFERING` is advisory: a
  verify read can be served from the redirector's cache or the server's, so it
  stops being independent evidence and becomes an expensive re-read of what was
  just written. A share is a perfectly good place to put files. It cannot vouch
  for a card, so a session using one never reaches SAFE TO FORMAT.
- **A destination on a slow connection.** Preflight writes 32 MiB and flushes it,
  which takes about a third of a second on a healthy drive, and reports the rate.
  Below the USB 2.0 ceiling it says so, names the likely cause and projects what
  the copy will cost at that speed — while a cable is still the fix.

  Measured, not asked. Windows can be interrogated for a nominal USB link speed,
  but that means walking volume → disk → parent hub → port through hub IOCTLs,
  and it answers the wrong question: a USB 3 drive behind a saturated hub is
  exactly as slow as a USB 2 one. Confirmed on real hardware — the same LaCie
  wrote at **22 MB/s** on one port and **103 MB/s** on another.
- **Battery.** Keep-awake blocks idle sleep and does nothing about a battery
  running out mid-copy.
- **The lid.** Read through `powrprof` rather than by parsing `powercfg` output —
  see the deviations below.
- **A camera that is not in backup mode.** Relay (fill card 1, then card 2) and
  RAW-to-one-slot/JPEG-to-the-other both produce cards that are *not* twins, so
  nearly every file comes out untwinned and the verdict correctly refuses. The
  run names the mode it looks like and the menu setting that would change it,
  because a wall of "no twin on card 2" for 1,613 files otherwise reads as a
  broken program.
- **Dual-slot readers that present both cards behind one bridge**, which makes
  the two cards one physical device. The warning names the fix: two readers, two
  ports.

### Remembering the setup

After a run, the setup is remembered in `%APPDATA%\sluice\config.json` — keyed
by **volume serial, never by drive letter**. Next time, the window fills itself
in and finds each drive wherever Windows has put it. A remembered drive that is
not connected says so by name, in amber, rather than leaving a blank box or
silently resolving to whatever now holds its old letter.

### Re-verifying, later

```
sluice verify --manifest D:\2026-03-14_shoot-01\sluice_20260314-221403.mhl
```

Re-hashes every file off the device and compares it to the manifest written when
the copy was made. No cards, no second destination, no session — this is how you
find out months later whether a drive is still what it was. Reports matched,
changed, missing and unreadable files, plus anything present that the manifest
does not vouch for. Exits non-zero if the copy no longer matches.

Reads **both MHL v1.1 and ASC MHL v2**, so a manifest written by another tool
works here too — provided it records xxHash64. sluice computes no other
algorithm, and a manifest using MD5 or SHA is reported as exactly that rather
than as a parse error.

```
sluice verify --drive D:\
```

Re-checks every session folder on a drive in one pass, with one exit code. It
also names folders that **no manifest vouches for**: a run that did not verify
cleanly deliberately leaves none behind, so an unvouched folder is a designed
outcome that nothing else ever surfaces again.

It re-checks **every** manifest in a folder, not the first one found. A shoot
day that offloads three card pairs under one label writes three manifests side
by side, each listing only its own run's files — so checking one and calling the
folder intact leaves the other two pairs unread. Files that *no* manifest in the
folder names are reported per file, not swallowed.

Exit 0 means every folder was opened and every file matched a manifest. Damage,
files nothing vouches for, and a sweep cancelled part-way each get their own
non-zero exit and a reason; a cancelled folder is reported as **not checked**
rather than as damaged.

### After a format

A verdict that authorises an erase offers **Record format…**, which captures
which cards you actually erased and when, into `%APPDATA%\sluice\history.jsonl`
and into the session log beside each copy. sluice never formats anything; this
records what you did, after the fact, which is where the design puts a night's
sleep and a human spot-check.

The same record accumulates per-device counters, so `sluice history` shows what
every card and drive has done: sessions, retried writes, twin mismatches, and
formats. **A card that produced a twin mismatch once is flagged for good**, and
preflight warns when it turns up again — the design says retire a suspect card,
and a second chance is not something to grant silently.

### Housekeeping

```
sluice clean [--keep 30] [--dry-run]
```

Removes old session logs, newest kept. **Never** touches `history.jsonl` — that
is the durable record of what every card and drive has done, and losing it loses
the suspect-card flags. `sluice doctor` reports the size of both.

Worth doing occasionally, and not for disk space: if the log volume fills, the
sink thread dies, the send failure is swallowed, and the job runs on to a verdict
with a truncated forensic record.

### When something goes wrong

```
sluice doctor
```

Power and lid settings, every mounted volume with its physical disk number,
whether a format verdict is even reachable on this machine, and where the logs
live. Read-only, safe at any time, and the first thing to paste into a bug
report. A panic writes `crash.log` next to the logs and says so on the way out.

`sluice run` is the same engine without a window, which is what the tests drive.
Add `--trace` for per-chunk reads and `sync_all` latency. Ctrl-C cancels
cleanly: partial destination files are removed and the cards are never touched.

Before trusting any of it on new hardware:

```
sluice selftest tail D:\ --file D:\some\real\DSC00001.ARW
sluice selftest cache-bypass D:\
sluice selftest volumes D:\ G:\        confirm the two LaCies are distinct drives
sluice selftest lid                    confirm a lid close will not kill a job
```

## Manifests

Every clean run leaves two manifests on every destination, describing the same
files with the same xxHash64 values:

```
sluice_20260314-221403.mhl        MHL v1.1 — what sluice re-verifies
ascmhl/0001_20260314-221403.mhl   ASC MHL v2 — what everything else reads
ascmhl/ascmhl_chain.xml
```

MHL v1.1 is what this program has always written. It is also, increasingly, not
what other tools read — the ASC took the format over, and v2 is what Silverstack,
ShotPut and the `ascmhl` reference implementation speak. A manifest nobody else
can open is only half a manifest.

The ASC side implements a **single-generation hash list plus its chain entry**.
Not implemented: directory hashes, partial-file hashes, flattening, or
multi-generation verification history. The v1.1 manifest remains the one whose
presence is the success signal, because it is the one sluice can re-verify
itself; a drive that cannot take the second small XML file gets a warning, not a
failed run.

## Getting it

```
cargo build --release
```

Self-contained: the CRT is statically linked, so there is no VC++ redistributable
to install. Copy `sluice.exe` to the laptop and go.

Every build stamps its own commit and date, visible in `sluice --version`, in
every session log, and in every manifest's `<creatorinfo>`. When a verdict is
later doubted the first useful question is *which build said so*, and the answer
belongs in the artifact rather than in somebody's memory of which exe they
copied. A trailing `+` on the commit means the binary was built from a tree with
uncommitted changes and corresponds to no commit anybody can check out.

CI publishes a SHA-256 alongside every binary. **On a public repository** it also
publishes an [artifact
attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations)
binding that digest to the commit and workflow that produced it:

```
gh attestation verify sluice.exe -R <owner>/<repo>
```

GitHub does not offer attestations for **user-owned private repositories** — the
API refuses with *"Feature not available"* — so on a private repo the workflow
skips that step, says so in the run summary, and the SHA-256 is the check that
remains. This was not a hypothetical: the step ran unconditionally and failed
every push for eleven runs while fmt, clippy, the tests, the release build and
the sector-tail selftest all passed each time, which is how a red CI stops being
read. Making the repository public turns the command above from an instruction
into a fact.

The binary carries its own icon and file properties, so it is findable in a
folder of executables and its Explorer properties say what it is rather than
nothing. `assets/sluice.res` is compiled from `sluice.rc` **and committed**: a
build dependency such as `winresource` would churn a lockfile this project pins
deliberately, and either route needs `rc.exe` from a versioned Windows SDK path
that every builder would then have to have. `build.rs` hands the ready-made
resource to the linker, and skips it with a warning rather than failing if it is
missing. Regenerate with `assets/mkicon.ps1`, which draws every size and
assembles the `.ico`.

The release workflow is wired for Authenticode signing and skips it cleanly when
no certificate is configured, publishing an unsigned binary and saying so rather
than failing. **The certificate is the one piece that cannot be automated** — put
a PFX in `SIGNING_CERT_PFX_BASE64` and its password in `SIGNING_CERT_PASSWORD`
and releases sign themselves. Until then SmartScreen will warn on a fresh laptop.

Licensing: sluice is MIT ([LICENSE](LICENSE)). JetBrains Mono is compiled into
the binary under the OFL, and `sluice licenses` prints both — so somebody who
receives only `sluice.exe` has received the terms with it. Third-party notices:
[NOTICE.md](NOTICE.md).

## Where it deviates from the design

Eight deliberate changes, each because the document as written is wrong,
underspecified, or would fail its own test plan.

1. **The reader takes a per-file source.** §4.1's "one read of card 1" cannot
   copy the union §4.3 requires: a file present only on card 2 has no card-1
   source. `reconcile.rs` chooses the source per file.

2. **The comparison matrix is an exhaustive classifier, not six rows.** The
   design's rows miss their own worst case: `Sc != C1 == C2` with `A == B == Sc`
   means both cards agree and the destinations faithfully recorded corrupt
   bytes. Row 3 reads that as a flaky reader and row 5 as a bus fault; both point
   at working hardware while the destinations are the broken thing. See
   `Diagnosis::SourceReadCorrupt` in `verify.rs`.

3. **Destination distinctness is proven by physical device number**
   (`IOCTL_STORAGE_GET_DEVICE_NUMBER`), not by volume serial. Two partitions of
   one disk have different serials, so §5's check would bless exactly the
   arrangement test 6 exists to catch. It needs no elevation, which is what makes
   the verdict reachable for an ordinary user.

4. **The lid-close policy is reported, because keep-awake cannot fix it.**
   `ES_SYSTEM_REQUIRED` blocks *idle* sleep; a lid close is user-initiated and no
   process can veto it. Test 12 fails on default power settings whatever the code
   does, so preflight reads the setting and prints the `powercfg` line that fixes
   it.

5. **The live JSONL is written to the laptop, not to a destination.** Test 13
   yanks a destination mid-copy; a forensic record that leaves with the cable is
   not a forensic record. Manifests still go to each destination at the end.

6. **Resume is "skip the write when size and mtime match", with no separate hash
   path.** Verify hashes everything unconditionally anyway, so a wrong guess
   costs a re-copy rather than a bad verdict.

7. **The copy reads the source unbuffered too.** That makes `Sc` and `C1` two
   genuinely independent trips to the device — the whole premise of the matrix
   row that detects an unrepeatable read — and stops a 91 GB card evicting the
   page cache.

8. **§5's three verdict states are five.** See **The verdict** above. Three
   states describe one hardware arrangement correctly and everything else
   misleadingly.

Smaller: the design's `HashSet` is named `Hashes` to avoid colliding with
`std::collections::HashSet`; guaranteed delivery applies to the JSONL rather than
to the UI channel, so a wedged window can never throttle a copy.

## The UI follows the mockup

[sluice-mockup.html](sluice-mockup.html) is the reference, and it carries one
idea the prose spec does not: **twin pairing is a hue.** Both cards are teal,
both destinations are periwinkle, the optional third destination is grey and
dimmed — so the relationship rev 2 exists to exploit, *these two things are
copies of each other*, is legible before a word is read. Cards carry a `TWIN
C1·C2` badge naming their other half. The pairing hues sit deliberately outside
green/amber/red, which stay reserved for state.

Colour is never the only channel. Roughly one man in twelve cannot separate the
green from the red, and the verdict band is the last thing read before an
irreversible decision — so the headline words, the glyph and the hue each carry
the answer alone, and a test asserts that the one state which authorises an erase
never shares a colour *or* a mark with any of the four that do not. The two
structural tiers deliberately take the destination hue rather than amber: a run
that did everything the hardware allows must not look like a run with a failing
drive in it.

Two places the implementation had to depart from the mockup's markup:

- **The font is vendored after all.** §10 asks for an embedded JetBrains Mono;
  I first argued egui's bundled Hack made that unnecessary. Running it disproved
  that: Hack has no block elements, no box drawing, and no dingbats, so the
  queue meter degraded from `███░ 3/4` to `###. 3/4`, `├─ writer A` to
  `|- writer A`, and the OK mark from `✓` to `+`. `assets/JetBrainsMono-Regular.ttf`
  (OFL, 264 KB) is now compiled into the binary.
- **Every symbol still resolves against the font at runtime.** `theme::Glyphs`
  asks the live atlas whether it can draw each preferred character and falls back
  to ASCII if not. A tofu box is worse than a plain `+` in a dim room, and this
  keeps that impossible whatever the font turns out to hold.

The mockup's circled digits `①②` are the one thing dropped outright — absent
from JetBrains Mono too, so the twin badge reads `TWIN C1·C2`.

## State

Engine and UI are complete: **274 tests**, clippy clean, `cargo fmt` clean, CI on
`windows-latest`.

| Module | State |
|---|---|
| `unbuffered.rs`, `win.rs`, `scan.rs` (+ name hazards), `reconcile.rs` (+ card mode) | done |
| `copy.rs` (with retry), `verify.rs`, `verdict.rs` (five tiers), `mhl.rs` (both dialects), `telemetry.rs` | done |
| `recheck.rs` re-verification, `history.rs`, `config.rs`, `build_info.rs` | done |
| `engine/mod.rs` orchestration, headless CLI, `doctor`, `licenses` | done |
| `ui/*` — theme, layout, banner, log pane, device strip, pipeline monitor | done |

Design tests 1–9 and 14 are automated, including the passing SAFE TO FORMAT path
(via an injected device probe — see below). The classifier is tested
*exhaustively* rather than by example: `diagnose` only compares hashes for
equality, so three distinct values plus absence cover every equivalence class it
can distinguish, and all 4,096 tuples are enumerated. The verdict's safety
property is tested the same way — every combination of card count, destination
count, twinning and distinctness, asserting that only the complete arrangement
says yes.

What is still not proven:

- **The real hardware pairing.** The automated SAFE TO FORMAT test injects a
  `DeviceProbe`, which proves the *logic*. Only real drives prove that these two
  LaCies, in these two ports, report as different physical devices — and the
  verdict rests entirely on that. Run once:
  ```
  sluice selftest format-verdict --dest D:\ --dest G:\
  ```
  It is read-only and passes only if distinctness is provable.

  The probe is a constructor parameter and **never a runtime flag, environment
  variable or config key**. A switch that could fake device distinctness in a
  shipped binary is precisely the thing that could bless a bad format.

- **Tests 10–13** — real hardware: full card pair, letter swap, lid close, cable
  yank.
- **Test 15, the dress rehearsal** — §14 names "a subtle bug that passes tests"
  as the top risk, and this is still the only thing that addresses it.
- **A signing certificate.** The workflow is wired; the certificate is a manual
  step.
- **Non-English Windows.** The lid read no longer parses localised text, which
  was the one known locale bug, but nothing has actually been run on a non-English
  install.
- **Screen readers.** eframe is built with `default-features = false`, which
  compiles AccessKit out, so Narrator gets silence where the banner is. Enabling
  it does not currently resolve against the pinned lockfile, and unpinning the
  dependency graph is exactly the drift §14 names as a top risk — so it is
  deliberately not done yet. `sluice run` prints the identical headline, detail
  and reasons to an accessible surface in the meantime.

Blocking a full field test: **a second card reader**. Without it there is no twin to
verify against, so the best available verdict is VERIFIED — ONE SOURCE and
nothing gets formatted.

## Hardening beyond the design

Things the design did not ask for, added because the tool's verdict authorises
destroying the only other copy:

- **Retry once per destination.** §4.3 asks for it and it was never built: a
  single transient write error used to fail the whole night. A recovered file
  does not fail the run — verify proves the bytes independently — but it is
  counted, and more than five retries on one drive blocks SAFE TO FORMAT,
  because at that point a glitch has stopped being a plausible explanation.
- **The same card in both slots is refused.** It made the twin check compare a
  card against itself: every hash agreed, nothing was verified against a second
  piece of NAND, and the run reported SAFE TO FORMAT having proved nothing.
- **The lid warning works outside English.** It used to match the literal string
  `Current AC Power Setting Index` in `powercfg` output. Windows translates that
  line, so on a French or Japanese machine it never matched, the function
  returned `Ok(None)`, and `None` is how this code says *this is a desktop, there
  is no lid*. Every non-English laptop silently lost the one warning that stops a
  lid close killing a 20-minute copy. Now read through `powrprof` as a number,
  which is a number in every locale.
- **A fault never hides a gap.** A failing drive decides the verdict tier, but
  the structural facts — eleven files with no twin, one destination — are still
  reported. An earlier cut let the fault branch swallow them.
- **A run that found nothing cannot authorise an erase.** `twin_matched` is
  vacuously true over an empty set and a zero-entry manifest writes without
  complaint, so two unreadable cards produced *"SAFE TO FORMAT · 0 files ·
  twin-matched · 2 distinct volumes"*. The same guard now covers cards whose scan
  threw errors — an unreadable directory takes its whole subtree with it, so those
  files are never seen and nothing else in the assessment can notice — and a
  destination sharing a physical device with a card, which the engine had
  detected and only warned about.
- **Session folders are dated in your timezone, not UTC.** A 22:14 offload in
  a late-evening offload was filed under tomorrow, disagreeing with the titlebar three inches
  above it. Worse, resume keys on that folder: a job interrupted at 17:50 and
  restarted at 18:10 crossed midnight UTC, addressed a different folder, and
  re-copied the whole card. Manifest timestamps stay UTC with a `Z`, because the
  schemas require it and an instant is not a calendar date.
- **ASC MHL generations are numbered.** Every run used to write `0001_` and
  rewrite the chain, so a second session in one folder left two generation-1 hash
  lists and a chain naming only the later — an invalid directory on its own
  terms, unconditionally, even with no media collision at all.
- **Preflight says what it checked.** The distinctness line read *"serials differ
  (30195459 / 30195459), physical devices differ — ok"*: `win::distinctness`
  decides on device numbers alone and never consults serials, so the line
  asserted a comparison the code does not make. It now names the disks it
  actually compared.
- **Cancel works during the scan.** `scan_cb` was written for this, takes the
  cancel flag and a progress callback, and was never called — the first time the
  job read the flag was in the copy loop. So on a 30,000-file card the Cancel
  button did nothing, Ctrl-C did nothing, the window visibly refused to close,
  and the status line read *"cancelling — partial destination files are being
  removed"* while nothing of the sort was happening.
- **A cloud placeholder on a destination is no longer mistaken for a finished
  copy.** It reports full logical size and the original mtime while holding none
  of the bytes, so resume skipped it and the verify pass silently hydrated it over
  the network, hashed what came back, and agreed. The reasoning
  `is_cloud_placeholder` already applied to the card was never applied to the
  drive the card is being traded for.
- **A destination that cannot be verified no longer vetoes the verdict.** Two
  local drives plus a NAS is the standard team setup and the window invites the
  third slot — so treating the NAS as a fault fired DO NOT FORMAT every night on a
  night when nothing was wrong. It is a fault only when it is load-bearing: when
  discounting it would leave fewer than two checkable destinations. Either way it
  is still reported.
- **Recording a format no longer destroys the export button.** The design says to
  format the next morning; doing so meant the affordance was gone by the time you
  came back, and a bundle exported *after* the erase is the only one that proves
  the erase happened.
- **The window fits the screen.** Its size was hardcoded and nothing asked how
  big the desktop was — on a 1080p laptop at Windows' default 150% scaling the
  desktop is 1280×720 *points*, so a 1280×900 window ran a quarter of the way off
  the bottom. What goes off the bottom is the verdict banner, which the layout
  puts there precisely so a long log can never push it off screen.
- **A finished job asks for attention.** Keep-awake exists so a 40-minute copy
  can be walked away from, and the job used to end with an unchanged taskbar
  button. `ES_DISPLAY_REQUIRED` is deliberately not asserted — the screen is
  allowed to sleep — so the taskbar is the only channel that survives.
- **A destination inside a cloud-synced folder is refused, not truncated.** The
  resume check and the overwrite guard had drifted: resume learned that a
  dehydrated OneDrive file is *not* a finished copy, and the guard kept its own
  copy of the size-and-mtime rules and still read one as identical. A placeholder
  was therefore neither skipped nor refused — it was truncated, and deleted
  outright if the run was then cancelled or hit a bad sector. The guard now calls
  the resume check rather than restating it, so the two cannot disagree again.
- **Another tool's ASC MHL directory is left alone.** `ascmhl_chain.xml` is a
  filename the spec fixes, and ASC MHL exists so several tools can share a
  folder — so it was the one path sluice wrote that another vendor also owns.
  Rewriting it replaced their chain, in whatever algorithm they sealed it with,
  with sluice's. sluice now writes nothing into an `ascmhl/` directory it did not
  create, and says so; MHL v1.1 is unaffected.
- **The config format carries a schema version.** A config written by a newer
  sluice used to fail to parse and be treated as "no memory yet", silently
  resetting the setup. It now says so — *and* declines to overwrite the file it
  refused to read, which is the half that makes saying so worth anything.
- **Shown times are local; stored times are UTC.** The log clock was UTC while
  the session id and folder name beside it were local — on a real run in a UTC-4
  zone the log read `19:44` at `15:44`, three inches from a folder named for the
  local date. The JSONL `at` field and the MHL dates stay UTC-with-`Z`, because a
  forensic record has to be unambiguous years later; everything an operator reads
  goes through one local-time helper. Each run opens by saying which is which.
- **The verify pass has a real estimate.** It had one, and it was wrong from the
  first second: byte counters ran cumulatively across phases and the clock
  measured the whole job, so verify began already past the copy's total and
  displayed *about 0s left* for its entire duration. Each phase now measures its
  own bytes against its own seconds, and verify states its true workload — it
  re-reads **every** copy, so on a two-card two-drive night it is four times the
  bytes the copy moved. Measured: 13.4 GB copied in 605s, then 53.6 GB re-read
  in 322s.
- **The window honours `--log-dir`,** and rejects flags it does not know. The CLI
  did both; the window silently ignored the flag and wrote elsewhere, and would
  open with no destination at all if given `--destination` instead of `--dest`.
- **Closing the window mid-job** now cancels and waits, instead of leaving a
  detached thread writing.
- **A per-destination lock** stops two offloads interleaving one session folder,
  while still allowing an unrelated copy to a different drive.
- **A manifest is never overwritten by a later run.** Session ids are
  second-granular, and two offloads into one folder inside the same second shared
  one — the second silently replaced the first's manifest, and the earlier card's
  frames became files nothing vouched for. Reproduced, then fixed: a colliding
  run writes alongside and warns, rather than replacing.
- `\\?\` prefixing past `MAX_PATH`; free space checked against what resume will
  actually write.
