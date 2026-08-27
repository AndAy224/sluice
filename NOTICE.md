# Third-party notices

sluice itself is MIT-licensed; see [LICENSE](LICENSE).

## Bundled inside the binary

One thing is compiled into `sluice.exe` rather than merely linked, and its
licence has to travel with it:

| Component | Licence | Where |
|---|---|---|
| JetBrains Mono (Regular) | SIL Open Font License 1.1 | `assets/JetBrainsMono-Regular.ttf` |

The OFL requires that its text accompany any distribution of the font. A person
who receives only `sluice.exe` receives the font too, so the licence is embedded
in the binary and printed by:

```
sluice licenses
```

That command is the reason this is a solved problem rather than a paperwork one:
there is no way to hand somebody the font without also handing them the terms.

## Rust dependencies

Linked, not embedded. All are MIT, Apache-2.0, or dual MIT/Apache-2.0, which
carry no obligation beyond retaining the notices in the source distribution:

anyhow, chrono, crossbeam-channel, egui, eframe, filetime, quick-xml, rfd,
serde, serde_json, walkdir, windows-sys, xxhash-rust.

`cargo tree` gives the full transitive set for any given build. For a complete
machine-generated inventory:

```
cargo install cargo-about && cargo about generate about.hbs
```
