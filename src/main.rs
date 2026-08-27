//! sluice entry point.
//!
//! Console subsystem on purpose: the headless harness prints here, and a crash
//! leaves something on screen instead of vanishing behind a closed window.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Result};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

use sluice::engine::telemetry::{Sink, Telemetry};
use sluice::engine::verdict::Verdict;
use sluice::engine::{selftest, win};

const USAGE: &str = "\
sluice -- verified camera offload

USAGE
  sluice
        Open the window. This is the ordinary way to run it.

  sluice --card1 <path> [--card2 <path>] --dest <path> [--dest <path>]
         [--label <name>] [--log-dir <path>] [--trace] [--start]
        The same window, opened with the night's setup already filled in, so
        a desktop shortcut can carry it. --start begins immediately and needs
        no clicks at all. After the first run the window remembers its own
        setup anyway, keyed on each drive's serial rather than its letter.

  sluice run --card1 <path> [--card2 <path>] --dest <path> [--dest <path>]
             [--label <name>] [--log-dir <path>] [--trace]
        Offload a card pair to every --dest, verify each copy against both
        cards, and print a format verdict. A session folder named
        <date>_<label> is created under each destination.

        Without --card2 there is no twin to check against, so the best
        available verdict is VERIFIED -- ONE SOURCE, which never authorises
        an erase. Ctrl-C cancels cleanly: partial destination files are
        removed and the cards are never touched.

        --trace adds per-chunk read lines and sync_all latency. Off by
        default because it roughly triples log volume.

  sluice verify --drive <dir>
        Re-check every sluice session folder on a drive, in one pass with one
        exit code. Also names folders that no manifest vouches for -- a run
        that did not verify cleanly leaves none behind, and nothing else
        ever surfaces that again.

  sluice verify --manifest <path.mhl> [--root <dir>]
        Re-check a drive against a manifest it was given when the copy was
        made. Needs no cards and no second destination -- this is how you
        find out months later whether a drive is still what it was.
        Reads both MHL v1.1 and ASC MHL v2, so a manifest from another
        tool works here too -- provided it records xxHash64, which is the
        only algorithm sluice computes. One using MD5 or SHA is reported
        as exactly that rather than as a parse error.
        --root defaults to whichever folder that
        dialect measures its paths from: the manifest's own folder for
        MHL v1, and the folder above for an ASC hash list, which lives one
        level down in ascmhl/.

  sluice doctor
        Everything about this machine that decides whether sluice can do its
        job: power and lid settings, every mounted volume with its physical
        disk number, whether a format verdict is even reachable here, and
        where the logs live. Read-only. This is the first thing to run when
        something does not work, and the first thing to paste into a bug
        report.

  sluice clean [--keep <n>] [--dry-run]
        Remove old session logs, keeping the newest n (default 30). Never
        touches history.jsonl, which is the durable record of what every
        card and drive has done.

  sluice licenses
        The licence of sluice and of the font compiled into this binary.

  sluice run ... exits 0 only when erasing the originals is authorised;
        10 = ONE SOURCE, 11 = ONE COPY, 12 = DO NOT FORMAT, 20 = FAILED,
        1 = refused before starting or an error.

  sluice --version

  sluice history [--sessions]
        What every card and drive has done across every session: retries,
        twin mismatches, and which cards were erased after which night.
        A card that misbehaved once is flagged here for good.

  sluice selftest tail <dir> [--file <path>]
        Test 1: files whose length is not sector-aligned hash the same
        unbuffered as buffered. Run this against the LaCie, with --file
        pointing at a real .ARW, before trusting anything else.

  sluice selftest cache-bypass <dir> [--size-mb <n>]
        Test 2, automatic half: unbuffered reads must run at device speed,
        not at cache speed. Default 512 MiB.

  sluice selftest cache-bypass-manual <dir>
        Test 2, manual half: flip a byte with a hex editor and confirm the
        unbuffered hash notices.

  sluice selftest format-verdict --dest <path> --dest <path>
        Can these two destinations authorise an erase? Reports each drive's
        identity and physical device number, and passes only if they are
        provably different drives. The automated suite covers the logic by
        injecting a probe; only this proves it on your actual hardware.
        Run it once with the real LaCies.

  sluice selftest volumes <path> [<path>...]
        Volume identity for each path, and whether each pair sits on
        distinct physical devices. Use this to watch two identical LaCies
        swap letters between plug-ins.

  sluice selftest lid
        What the active power scheme does when the lid closes. Keep-awake
        cannot override this.
";

fn main() -> ExitCode {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("\nerror: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<u8> {
    let Some(first) = args.first() else {
        // No arguments is the ordinary case at 11pm: open the window.
        return sluice::ui::run().map(|()| 0).map_err(|e| anyhow!("{e}"));
    };

    match first.as_str() {
        "run" => run_cmd(&args[1..]),
        "verify" => verify_cmd(&args[1..]).map(|()| 0),
        "history" => history_cmd(&args[1..]).map(|()| 0),
        "doctor" => doctor_cmd().map(|()| 0),
        "clean" => clean_cmd(&args[1..]).map(|()| 0),
        "licenses" | "license" => licenses_cmd().map(|()| 0),
        "selftest" => selftest_cmd(&args[1..]).map(|()| 0),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" | "version" => {
            print!("{}", sluice::engine::build_info::long_version());
            Ok(0)
        }
        // Flags with no subcommand open the window with the paths filled in,
        // so a desktop shortcut can carry the night's setup.
        flag if flag.starts_with("--") => {
            // The CLI rejects a flag it does not know; this branch accepted
            // anything and dropped it. `sluice --card1 E:\ --destination D:\`
            // opened a window with no destination at all and said nothing.
            reject_unknown(
                args,
                &["--card1", "--card2", "--dest", "--label", "--log-dir"],
                &["--trace", "--start"],
            )?;
            let prefill = sluice::ui::Prefill {
                card1: flag_value(args, "--card1").unwrap_or_default().to_string(),
                card2: flag_value(args, "--card2").unwrap_or_default().to_string(),
                dests: flag_values(args, "--dest")
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                label: flag_value(args, "--label").map(str::to_string),
                log_dir: flag_value(args, "--log-dir").map(PathBuf::from),
                trace: args.iter().any(|a| a == "--trace"),
                start: args.iter().any(|a| a == "--start"),
            };
            sluice::ui::run_with(prefill)
                .map(|()| 0)
                .map_err(|e| anyhow!("{e}"))
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

/// Raised by the console handler so Ctrl-C cancels rather than killing the
/// process mid-write and stranding partial files on a destination.
static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();

unsafe extern "system" fn console_handler(_ctrl_type: u32) -> i32 {
    if let Some(flag) = CANCEL.get() {
        flag.store(true, Ordering::Relaxed);
    }
    // Handled: do not let the default handler terminate us. The job unwinds on
    // its own and cleans up after itself.
    1
}

fn run_cmd(args: &[String]) -> Result<u8> {
    reject_unknown(
        args,
        &["--card1", "--card2", "--dest", "--label", "--log-dir"],
        &["--trace"],
    )?;
    let card1 = PathBuf::from(
        flag_value(args, "--card1").ok_or_else(|| anyhow!("--card1 is required\n\n{USAGE}"))?,
    );
    let card2 = flag_value(args, "--card2").map(PathBuf::from);
    let dest_roots: Vec<PathBuf> = flag_values(args, "--dest")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if dest_roots.is_empty() {
        bail!("at least one --dest is required\n\n{USAGE}");
    }
    for p in [Some(&card1), card2.as_ref()].into_iter().flatten() {
        if !p.is_dir() {
            bail!("{} is not a directory", p.display());
        }
    }

    let cfg = sluice::engine::JobConfig {
        card1,
        card2,
        dest_roots,
        label: flag_value(args, "--label").unwrap_or("session").to_string(),
        log_dir: flag_value(args, "--log-dir")
            .map(PathBuf::from)
            .unwrap_or_else(sluice::engine::telemetry::default_log_dir),
        probe: None,
        history_path: None,
    };

    let cancel = sluice::engine::cancel_flag();
    let _ = CANCEL.set(Arc::clone(&cancel));
    // SAFETY: a plain function pointer with the documented signature.
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), 1);
    }

    let trace = args.iter().any(|a| a == "--trace");
    let (tel, rx) = Telemetry::with_trace(trace);
    let session = sluice::engine::telemetry::session_id(chrono::Utc::now());
    let log_path = sluice::engine::telemetry::log_path(&cfg.log_dir, &session);
    // Print every record as it arrives, so the console is the live log.
    let (print_tx, print_rx) = crossbeam_channel::bounded(65_536);
    let sink = Sink::spawn(log_path.clone(), rx, Some(print_tx))?;
    let printer = std::thread::spawn(move || {
        for rec in print_rx {
            if let Some(line) = rec.log_line() {
                println!("{line}");
            }
        }
    });

    let outcome = sluice::engine::run_job(&cfg, &tel, &cancel);
    drop(tel);
    sink.join()?;
    let _ = printer.join();

    let outcome = outcome?;
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("  {}", outcome.verdict.headline());
    println!("  {}", outcome.verdict.detail());
    for reason in &outcome.verdict.reasons {
        println!("  · {reason}");
    }
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    // The sink's own path, not a recomputed one: the session id is derived from
    // the clock, and the minute can roll between here and the job starting.
    println!("session {}   log {}", outcome.session, log_path.display());
    // Where the files actually went. It was on `JobOutcome` all along and never
    // printed, so a caller had to reconstruct the folder name from the date and
    // the label to find its own output.
    for dir in &outcome.session_dirs {
        println!("  -> {}", dir.display());
    }

    if outcome.verdict.state == Verdict::Failed {
        eprintln!("\nerror: the run did not verify cleanly");
    }
    // The verdict *is* the result, so it is what the process reports. 0 means
    // erasing the originals is authorised and nothing else does.
    Ok(outcome.verdict.state.exit_code())
}

/// What every card and drive has done across every session.
fn history_cmd(args: &[String]) -> Result<()> {
    use sluice::engine::history;

    let entries = history::read_all()?;
    if entries.is_empty() {
        println!("no history yet — {}", history::history_path().display());
        return Ok(());
    }

    if args.iter().any(|a| a == "--sessions") {
        for e in &entries {
            match e {
                history::Entry::Session {
                    at,
                    session,
                    verdict,
                    failures,
                    devices,
                    ..
                } => println!(
                    "{}  session  {session}  {verdict}  [{}]{}",
                    sluice::engine::telemetry::local_stamp(*at),
                    // The cards this session read. Thrown away until now, three
                    // lines above a FORMAT branch that prints them -- and this
                    // is the documented after-the-fact recovery path.
                    devices
                        .iter()
                        .filter(|d| d.slot.starts_with('C'))
                        .map(|d| format!("{} {}", d.slot, d.serial_hex()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if *failures > 0 {
                        format!("  ({failures} failed)")
                    } else {
                        String::new()
                    }
                ),
                history::Entry::Format {
                    at,
                    session,
                    cards,
                    note,
                } => println!(
                    "{}  FORMAT   {session}  erased {}{}",
                    sluice::engine::telemetry::local_stamp(*at),
                    cards
                        .iter()
                        .map(|c| format!("{} ({})", c.label, c.serial_hex()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if note.is_empty() {
                        String::new()
                    } else {
                        format!("  — {note}")
                    }
                ),
            }
        }
        println!();
    }

    println!(
        "{:<10} {:<14} {:>8} {:>8} {:>6} {:>9}  LAST SEEN",
        "SERIAL", "LABEL", "SESSIONS", "RETRIES", "TWIN", "FORMATTED"
    );
    for d in history::summarise(&entries).values() {
        println!(
            "{:<10} {:<14} {:>8} {:>8} {:>6} {:>9}  {}",
            d.serial_hex(),
            if d.label.is_empty() { "-" } else { &d.label },
            d.sessions,
            d.retries,
            d.twin_mismatches,
            d.formatted,
            d.last_seen
                .map(sluice::engine::telemetry::local_date)
                .unwrap_or_else(|| "-".into())
        );
    }
    let flagged: Vec<String> = history::summarise(&entries)
        .values()
        .filter_map(|d| d.warning())
        .collect();
    if !flagged.is_empty() {
        println!();
        for w in flagged {
            println!("  ! {w}");
        }
    }
    println!();
    println!("{}", history::history_path().display());
    Ok(())
}

/// Re-check a drive against a manifest, months later, with no cards involved.
fn verify_cmd(args: &[String]) -> Result<()> {
    reject_unknown(args, &["--manifest", "--root", "--drive"], &[])?;

    // A shuttle drive holds a season, not a session. Requiring one --manifest
    // with a hand-typed session id meant thirty invocations and thirty exit
    // codes, which is why "how you find out months later whether a drive is
    // still what it was" did not get run and the manifests stayed decorative.
    if let Some(drive) = flag_value(args, "--drive") {
        return verify_drive(Path::new(drive));
    }

    let manifest = PathBuf::from(
        flag_value(args, "--manifest")
            .ok_or_else(|| anyhow!("--manifest or --drive is required\n\n{USAGE}"))?,
    );
    if !manifest.is_file() {
        bail!("{} is not a file", manifest.display());
    }
    let root = flag_value(args, "--root").map(PathBuf::from);

    let cancel = sluice::engine::cancel_flag();
    let _ = CANCEL.set(Arc::clone(&cancel));
    // SAFETY: a plain function pointer with the documented signature.
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), 1);
    }

    let (tel, rx) = Telemetry::new();
    let (print_tx, print_rx) = crossbeam_channel::bounded(65_536);
    let log_dir = sluice::engine::telemetry::default_log_dir();
    let log_path = sluice::engine::telemetry::log_path(
        &log_dir,
        &format!(
            "recheck-{}",
            sluice::engine::telemetry::session_id(chrono::Utc::now())
        ),
    );
    let sink = Sink::spawn(log_path.clone(), rx, Some(print_tx))?;
    let printer = std::thread::spawn(move || {
        for rec in print_rx {
            if let Some(line) = rec.log_line() {
                println!("{line}");
            }
        }
    });

    let report = sluice::engine::recheck::recheck_path(&manifest, root.as_deref(), &tel, &cancel);
    drop(tel);
    sink.join()?;
    let _ = printer.join();

    let report = report?;
    println!();
    println!("  {}", report.headline());
    for f in report.failures() {
        println!("  · {}: {}", f.rel, f.outcome.describe());
    }
    if !report.extras.is_empty() {
        println!(
            "  · {} file(s) present but not in the manifest",
            report.extras.len()
        );
    }
    println!();
    println!("log {}", log_path.display());

    // A cancelled re-check is not a mismatch. Reporting one as the other trains
    // people to read "damaged" as "I pressed Ctrl-C".
    if report.cancelled {
        bail!("cancelled — the re-check did not finish, so it proves nothing either way");
    }
    if !report.intact() {
        bail!("this copy no longer matches its manifest");
    }
    Ok(())
}

/// Re-check every session folder on a drive.
///
/// One headline and one exit code for the whole drive, so the answer to "is
/// this archive still good" is one command rather than thirty.
fn verify_drive(root: &Path) -> Result<()> {
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let sessions = sluice::engine::recheck::find_sessions(root);
    if sessions.is_empty() {
        println!("no sluice session folders found under {}", root.display());
        return Ok(());
    }

    let cancel = sluice::engine::cancel_flag();
    let _ = CANCEL.set(Arc::clone(&cancel));
    // SAFETY: a plain function pointer with the documented signature.
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), 1);
    }

    let log_dir = sluice::engine::telemetry::default_log_dir();
    let log_path = sluice::engine::telemetry::log_path(
        &log_dir,
        &format!(
            "recheck-{}",
            sluice::engine::telemetry::session_id(chrono::Utc::now())
        ),
    );

    println!(
        "{} session folder(s) under {}\n",
        sessions.len(),
        root.display()
    );

    // One invocation, one log. Spawning a sink per folder truncated the file
    // each time, so only the last folder's forensic record survived -- in the
    // command whose whole point is a record of what an archive drive held.
    let (tel, rx) = Telemetry::new();
    let sink = Sink::spawn(log_path.clone(), rx, None)?;

    let mut damaged = Vec::new();
    let mut unvouched = Vec::new();
    let mut intact = 0usize;
    let mut cancelled = false;

    for s in &sessions {
        // Hoisted above every `continue` below, so a cancel can never be
        // stepped over and no later folder can be reported as anything.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let name = s
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if s.manifests.is_empty() {
            // A run that did not verify cleanly deliberately leaves no manifest.
            // That is a designed outcome nothing has surfaced since.
            println!("  {name:<32} NO MANIFEST — nothing vouches for this folder");
            unvouched.push(name);
            continue;
        }

        // Every manifest in the folder, not the first one found: each lists only
        // the files its own run copied, so one INTACT proves one card pair.
        let mut failed: Vec<String> = Vec::new();
        let mut matched = 0usize;
        let mut unreadable: Option<String> = None;
        let mut folder_cancelled = false;
        // A file is unaccounted for only when *no* manifest here lists it, so
        // the residual is the intersection of the per-manifest extras.
        let mut residual: Option<std::collections::BTreeSet<String>> = None;

        for m in &s.manifests {
            match sluice::engine::recheck::recheck_path(m, None, &tel, &cancel) {
                Ok(r) => {
                    folder_cancelled |= r.cancelled;
                    matched += r.matched();
                    for f in r.failures() {
                        failed.push(format!("{}: {}", f.rel, f.outcome.describe()));
                    }
                    let extras: std::collections::BTreeSet<String> =
                        r.extras.iter().cloned().collect();
                    residual = Some(match residual {
                        Some(prev) => prev.intersection(&extras).cloned().collect(),
                        None => extras,
                    });
                }
                Err(e) => unreadable = Some(format!("{e:#}")),
            }
            if folder_cancelled {
                break;
            }
        }
        let residual = residual.unwrap_or_default();
        let n = s.manifests.len();
        let plural = if n > 1 {
            format!(", {n} manifests")
        } else {
            String::new()
        };

        if let Some(e) = unreadable {
            println!("  {name:<32} UNREADABLE — {e}");
            damaged.push(name);
        } else if folder_cancelled {
            // Not damage, and not proof either: say so rather than claiming one.
            println!("  {name:<32} NOT CHECKED — cancelled part-way");
            cancelled = true;
        } else if !failed.is_empty() {
            println!("  {name:<32} DAMAGED");
            for f in failed.iter().take(5) {
                println!("      · {f}");
            }
            damaged.push(name);
        } else if !residual.is_empty() {
            // Every file a manifest names is intact, but the folder holds files
            // no manifest names. Printing a bare INTACT over them is the false
            // all-clear this command exists to prevent.
            println!(
                "  {name:<32} INTACT ({matched} files{plural}) · {} file(s) no manifest vouches for",
                residual.len()
            );
            for r in residual.iter().take(5) {
                println!("      · {r}");
            }
            unvouched.push(name);
        } else if matched == 0 {
            // A manifest that lists nothing proves nothing.
            println!("  {name:<32} EMPTY MANIFEST — nothing vouches for this folder");
            unvouched.push(name);
        } else {
            println!("  {name:<32} INTACT  ({matched} files{plural})");
            intact += 1;
        }
    }

    drop(tel);
    sink.join()?;

    println!();
    println!(
        "  {intact} intact · {} damaged · {} unvouched",
        damaged.len(),
        unvouched.len()
    );
    println!("log {}", log_path.display());

    // Exit 0 has to mean "every folder on this drive was opened and every file
    // matched a manifest". Anything less gets a non-zero code and a reason.
    if cancelled {
        let done = intact + damaged.len() + unvouched.len();
        bail!(
            "cancelled — {done} of {} session folder(s) were checked; the rest were never opened",
            sessions.len()
        );
    }
    if !damaged.is_empty() {
        bail!(
            "{} session folder(s) no longer match their manifests",
            damaged.len()
        );
    }
    if !unvouched.is_empty() {
        bail!(
            "{} folder(s) hold files no manifest vouches for — this drive is not fully proven",
            unvouched.len()
        );
    }
    Ok(())
}

fn selftest_cmd(args: &[String]) -> Result<()> {
    let Some(which) = args.first() else {
        bail!("selftest needs a subcommand\n\n{USAGE}");
    };
    let rest = &args[1..];

    match which.as_str() {
        "tail" => {
            let dir = require_dir(rest.first())?;
            let file = flag_value(rest, "--file").map(PathBuf::from);
            if let Some(f) = &file {
                if !f.is_file() {
                    bail!("--file {} is not a file", f.display());
                }
            }
            selftest::tail(&dir, file.as_deref())
        }
        "cache-bypass" => {
            let dir = require_dir(rest.first())?;
            let size_mb = match flag_value(rest, "--size-mb") {
                Some(v) => v
                    .parse()
                    .map_err(|_| anyhow!("--size-mb {v:?} is not a number"))?,
                None => 512,
            };
            selftest::cache_bypass(&dir, size_mb).map(|_| ())
        }
        "cache-bypass-manual" => {
            let dir = require_dir(rest.first())?;
            selftest::cache_bypass_manual(&dir)
        }
        "format-verdict" => {
            let dests: Vec<PathBuf> = flag_values(rest, "--dest")
                .into_iter()
                .map(PathBuf::from)
                .collect();
            for d in &dests {
                if !d.is_dir() {
                    bail!("{} is not a directory", d.display());
                }
            }
            selftest::format_verdict(&dests)
        }
        "volumes" => volumes_cmd(rest),
        "lid" => lid_cmd(),
        other => bail!("unknown selftest {other:?}\n\n{USAGE}"),
    }
}

fn volumes_cmd(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        bail!("selftest volumes needs at least one path");
    }
    let mut infos = Vec::new();
    for p in paths {
        let info = win::volume_info(Path::new(p))?;
        println!("{p}");
        println!("  root          {}", info.root);
        println!(
            "  label         {}",
            if info.label.is_empty() {
                "(none)"
            } else {
                &info.label
            }
        );
        println!("  filesystem    {}", info.filesystem);
        println!("  serial        {}", info.serial_hex());
        println!("  sector size   {} B", info.sector_size);
        println!(
            "  volume guid   {}",
            info.guid.as_deref().unwrap_or("(unavailable)")
        );
        match info.device_number {
            Some(n) => println!("  phys. device  {n}"),
            None => println!("  phys. device  (unavailable -- distinctness cannot be proven)"),
        }
        println!(
            "  free          {:.2} GB",
            win::free_space(Path::new(p))? as f64 / 1e9
        );
        println!();
        infos.push((p.clone(), info));
    }

    if infos.len() > 1 {
        println!("pairwise distinctness");
        for i in 0..infos.len() {
            for j in (i + 1)..infos.len() {
                let verdict = match win::distinctness(&infos[i].1, &infos[j].1) {
                    win::Distinctness::Distinct => {
                        "DISTINCT -- safe as two destinations".to_string()
                    }
                    win::Distinctness::SameDevice => {
                        "SAME DEVICE -- never use as two destinations".to_string()
                    }
                    win::Distinctness::Unproven(why) => format!("UNPROVEN -- {why}"),
                };
                println!("  {} vs {}: {verdict}", infos[i].0, infos[j].0);
            }
        }
    }
    Ok(())
}

fn lid_cmd() -> Result<()> {
    let Some(policy) = win::lid_policy()? else {
        println!("lid close action");
        println!("  no lid-close setting on this machine (desktop, or not exposed by the");
        println!("  active power scheme). Nothing to warn about here -- but re-run this on");
        println!("  the machine you will actually use, where the setting will exist.");
        return Ok(());
    };
    println!("lid close action");
    println!("  on AC       {}", policy.ac.describe());
    println!("  on battery  {}", policy.dc.describe());
    if policy.ac.interrupts_job() || policy.dc.interrupts_job() {
        println!();
        println!("  WARNING: closing the lid will interrupt a running job.");
        println!("  Keep-awake blocks idle sleep only -- a lid close is a user-initiated");
        println!("  power transition and no process can veto it. To fix:");
        println!();
        println!("    {}", win::LID_FIX_COMMAND);
    }
    Ok(())
}

/// Write a crash to a file and say where it went.
///
/// `panic = "unwind"` means a panicking worker thread is caught rather than
/// taking the process down, but nothing recorded it. On somebody else's machine
/// a crash that leaves no trace is a bug report that says "it stopped working",
/// which is not a bug report. This is also the only thing sluice ever asks a
/// user to send, and it goes next to the logs so the session bundle picks it up.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let dir = sluice::engine::telemetry::default_log_dir();
        let path = dir.join("crash.log");
        let mut report = String::new();
        report.push_str(&format!("{}\n", sluice::engine::build_info::stamp()));
        report.push_str(&format!("at    {}\n", chrono::Utc::now().to_rfc3339()));
        report.push_str(&format!(
            "thread {}\n",
            std::thread::current().name().unwrap_or("unnamed")
        ));
        report.push_str(&format!("{info}\n"));
        report.push_str("---\n");

        // Appended, not overwritten: a second panic during unwinding is exactly
        // the case where the first one mattered.
        let wrote = std::fs::create_dir_all(&dir).is_ok()
            && std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, report.as_bytes()))
                .is_ok();
        if wrote {
            eprintln!("\nsluice crashed. Details written to {}", path.display());
        }
        previous(info);
    }));
}

/// The licences of everything compiled into this binary.
///
/// The font is embedded rather than linked, and the OFL requires its text to
/// travel with it. Somebody who receives only `sluice.exe` has received the font
/// too, so the terms have to be reachable from the exe itself rather than from a
/// repository they may never see.
fn licenses_cmd() -> Result<()> {
    println!("{}\n", sluice::engine::build_info::stamp());
    println!("=== sluice ===\n");
    println!("{}", include_str!("../LICENSE"));
    println!("=== JetBrains Mono, embedded in this binary ===\n");
    println!("{}", include_str!("../assets/JetBrainsMono-OFL.txt"));
    Ok(())
}

/// Prune old session logs.
///
/// Never the history: `history.jsonl` is the durable record of what every card
/// and drive has done, and losing it loses the suspect-card flags. Only the
/// per-session JSONL, which is large, numerous, and superseded by the manifests
/// once a run has verified.
fn clean_cmd(args: &[String]) -> Result<()> {
    reject_unknown(args, &["--keep"], &["--dry-run"])?;
    let keep: usize = match flag_value(args, "--keep") {
        Some(v) => v
            .parse()
            .map_err(|_| anyhow!("--keep needs a number, got {v:?}"))?,
        None => 30,
    };
    let dry = args.iter().any(|a| a == "--dry-run");
    let dir = sluice::engine::telemetry::default_log_dir();
    let logs = sluice::engine::telemetry::list_logs(&dir);

    let total: u64 = logs.iter().map(|l| l.bytes).sum();
    println!(
        "{} session log(s) in {}, {}",
        logs.len(),
        dir.display(),
        human_bytes(total)
    );

    if logs.len() <= keep {
        println!("nothing to remove — keeping the newest {keep}");
        return Ok(());
    }

    let doomed: Vec<_> = logs.iter().skip(keep).collect();
    let freeing: u64 = doomed.iter().map(|l| l.bytes).sum();
    if dry {
        for l in &doomed {
            println!(
                "  would remove  {}  ({})",
                l.path.file_name().unwrap_or_default().to_string_lossy(),
                human_bytes(l.bytes)
            );
        }
        println!(
            "\n{} file(s), {} — re-run without --dry-run to remove them",
            doomed.len(),
            human_bytes(freeing)
        );
        return Ok(());
    }

    let (removed, freed) = sluice::engine::telemetry::prune_logs(&dir, keep);
    println!(
        "removed {removed} log(s), freed {} — kept the newest {keep}",
        human_bytes(freed)
    );
    println!("history.jsonl is never touched by this command");
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
        ("B", 1),
    ];
    for (label, scale) in UNITS {
        if n >= scale {
            return if scale == 1 {
                format!("{n} B")
            } else {
                format!("{:.1} {label}", n as f64 / scale as f64)
            };
        }
    }
    "0 B".into()
}

/// Everything about this machine that decides whether sluice can do its job.
///
/// The point is to turn "it doesn't work" into a diagnosis without a
/// back-and-forth. Read-only, and safe to run at any time.
fn doctor_cmd() -> Result<()> {
    println!("{}", sluice::engine::build_info::long_version());

    if sluice::engine::build_info::is_dirty() {
        println!("!  built from a modified working tree\n");
    }

    println!("== machine ==");
    println!(
        "  user        {}",
        std::env::var("USERNAME").unwrap_or_else(|_| "?".into())
    );
    println!(
        "  host        {}",
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "?".into())
    );
    println!(
        "  cpus        {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    // Elevation is worth stating because it is the thing people reach for when
    // something is denied, and here it changes nothing.
    println!(
        "  elevated    {}  (not required -- device identity works without it)",
        if win::is_elevated() { "yes" } else { "no" }
    );

    println!("\n== power ==");
    match win::power_status() {
        Some(p) => {
            println!(
                "  source      {}",
                if p.on_mains { "mains" } else { "BATTERY" }
            );
            if let Some(pct) = p.battery_percent {
                println!("  battery     {pct}%");
            }
            if let Some(w) = p.warns_before_a_long_copy() {
                println!("  !  {w}");
            }
        }
        None => println!("  source      unknown"),
    }
    match win::lid_policy() {
        Ok(Some(policy)) => {
            println!("  lid on AC   {}", policy.ac.describe());
            println!("  lid on DC   {}", policy.dc.describe());
            if policy.ac.interrupts_job() || policy.dc.interrupts_job() {
                println!("  !  closing the lid will interrupt a running job. To fix:");
                println!("     {}", win::LID_FIX_COMMAND);
            }
        }
        Ok(None) => println!("  lid         no lid-close setting (desktop)"),
        Err(e) => println!("  lid         could not read: {e:#}"),
    }

    println!("\n== volumes ==");
    println!(
        "  {:<5} {:<14} {:<9} {:<8} {:<6} {:>10}  KIND",
        "ROOT", "LABEL", "SERIAL", "FS", "DISK", "FREE GB"
    );
    let mounted = win::mounted_volumes();
    for v in &mounted {
        println!(
            "  {:<5} {:<14} {:<9} {:<8} {:<6} {:>10.0}  {}{}",
            v.info.root,
            if v.info.label.is_empty() {
                "-"
            } else {
                &v.info.label
            },
            v.info.serial_hex(),
            v.info.filesystem,
            v.info
                .device_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into()),
            v.free_bytes as f64 / 1e9,
            v.drive_type.describe(),
            if v.drive_type.verification_reaches_the_device() {
                ""
            } else {
                "  (cannot be verified off the device)"
            }
        );
    }
    if mounted.is_empty() {
        println!("  none");
    }

    // The one thing that decides whether any pair of destinations can ever
    // authorise an erase.
    let disks: std::collections::BTreeSet<u32> = mounted
        .iter()
        .filter_map(|v| v.info.device_number)
        .collect();
    println!("\n== verdict capability ==");
    if mounted.iter().any(|v| v.info.device_number.is_none()) {
        println!("  !  some volumes report no physical device number. Two destinations");
        println!("     that cannot be told apart can never reach SAFE TO FORMAT.");
    }
    println!("  {} distinct physical disk(s) visible", disks.len());
    if disks.len() < 2 {
        println!("  !  a format verdict needs two destinations on two different disks.");
    }
    let removable = mounted
        .iter()
        .filter(|v| v.drive_type.is_card_like())
        .count();
    println!("  {removable} removable volume(s) -- a twin-verified session needs two");

    println!("\n== paths ==");
    let log_dir = sluice::engine::telemetry::default_log_dir();
    let logs = sluice::engine::telemetry::list_logs(&log_dir);
    let log_bytes: u64 = logs.iter().map(|l| l.bytes).sum();
    println!("  logs        {}", log_dir.display());
    println!(
        "              {} file(s), {}{}",
        logs.len(),
        human_bytes(log_bytes),
        if logs.len() > 100 {
            "  —  `sluice clean` prunes these"
        } else {
            ""
        }
    );
    let hist = sluice::engine::history::history_path();
    println!("  history     {}", hist.display());
    println!(
        "              {}  —  never removed by `sluice clean`",
        human_bytes(std::fs::metadata(&hist).map(|m| m.len()).unwrap_or(0))
    );
    println!(
        "  config      {}",
        sluice::engine::config::config_path().display()
    );
    Ok(())
}

fn require_dir(arg: Option<&String>) -> Result<PathBuf> {
    let Some(arg) = arg else {
        bail!("expected a directory argument");
    };
    let path = PathBuf::from(arg);
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

/// The value following a flag, refusing a value that is itself a flag.
///
/// `--label --trace` used to produce a session folder literally named
/// `--trace`, with tracing still on; a mistyped `--destination G:\` silently
/// halved the number of copies. Neither said anything.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    let v = args.get(i + 1).map(String::as_str)?;
    (!v.starts_with("--")).then_some(v)
}

/// Refuse arguments this program does not understand.
///
/// Silently ignoring an unknown flag is how `--destination` instead of `--dest`
/// turns two copies into one without a word.
fn reject_unknown(args: &[String], flags: &[&str], toggles: &[&str]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with("--") {
            i += 1;
            continue;
        }
        if toggles.contains(&a.as_str()) {
            i += 1;
        } else if flags.contains(&a.as_str()) {
            let value = args.get(i + 1);
            match value {
                Some(v) if !v.starts_with("--") => i += 2,
                _ => bail!("{a} needs a value"),
            }
        } else {
            bail!("unknown option {a}\n\n{USAGE}");
        }
    }
    Ok(())
}

/// Every occurrence of a repeatable flag, in order. `--dest D:\ --dest G:\`.
fn flag_values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == flag)
        .filter_map(|(i, _)| args.get(i + 1))
        .map(String::as_str)
        .collect()
}
