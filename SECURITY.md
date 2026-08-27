# Threat model

sluice tells you when it is safe to erase the only other copy of a day's work.
That makes "what is this actually protecting against" a question worth answering
precisely, before somebody assumes an answer that is not true.

## What it defends against

**Accidental corruption, everywhere it can occur between the card and the drive.**

- A card that returns different bytes on two separate reads.
- A card whose bytes disagree with its twin, written at the same moment by the
  same camera.
- A transfer that corrupts data in flight — cable, controller, RAM, driver.
- A destination that accepts a write and stores something else.
- A destination that quietly loses a file later. Re-run `sluice verify
  --manifest` in a year and it will say so.
- The page cache lying to you about all of the above. Every hash that
  contributes to a verdict is read with `FILE_FLAG_NO_BUFFERING`, so it comes
  off the device rather than out of RAM.

This is the failure mode that actually destroys photographs, and it is what
every part of the design is aimed at.

## What it does not defend against

**A person or program deliberately trying to deceive it.**

xxHash64 is not a cryptographic hash. It is fast, it is what MHL specifies, and
it detects accidental corruption with effectively certainty at these file sizes.
It is trivially collidable *on purpose*. So:

- sluice cannot tell you a file was not tampered with.
- A manifest is not a signature. Anyone who can write to the drive can write a
  manifest that vouches for whatever they put there.
- Verification proves a drive matches the manifest that shipped with it. It does
  not prove either one is what the camera produced.

If you need tamper-evidence rather than corruption-detection, you need a signed
cryptographic digest, and sluice is the wrong tool.

**Anything after the verdict.** "Safe to format" means *two verified copies exist
on two different physical drives, right now*. It says nothing about what happens
to those drives afterwards. Two drives in one bag is one accident.

**A subtle bug in sluice itself.** The design names this as the top risk and
nothing displaces it. Mitigations: the comparison-matrix classifier is tested
exhaustively rather than by example; every rule fails toward not-formatting; and
an unproven claim is treated exactly like a disproven one. That is not the same
as a proof.

## Network behaviour

**sluice makes no network connections of any kind.** No telemetry, no update
check, no crash reporting, no HTTP client linked into the binary. `--version`
says so, and the dependency list in [NOTICE.md](NOTICE.md) is short enough to
check by hand.

This is deliberate and not merely an omission. The program reads every byte of
every photograph you give it, and the strongest promise it can make about that
is that the bytes have nowhere to go.

## Privilege

sluice runs as an ordinary user. It does not require, request, or benefit from
elevation — including for `IOCTL_STORAGE_GET_DEVICE_NUMBER`, which is what
proves two destinations are different physical drives.

It never writes to a card. The only paths it writes are the destinations you
choose and its own log and history under `%APPDATA%\sluice`.

## Reporting a problem

A wrong SAFE TO FORMAT verdict is the only critical bug class this program has:
every other defect costs time, and that one costs photographs. If you find a
case where it authorises an erase it should not, please open an issue with the
session bundle (**Export session bundle** in the window, or the JSONL under
`%APPDATA%\sluice\logs`), which carries the verdict, the device identities, and
the per-file comparison results.
