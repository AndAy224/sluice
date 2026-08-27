//! Stamps the binary with the commit it was built from.
//!
//! A verdict that authorises erasing the only other copy is worth tracing back
//! to an exact build. `--version`, every session log, and every manifest's
//! `<creatorinfo>` carry this, so a manifest found on a drive in two years names
//! the code that vouched for it.

use std::process::Command;

fn main() {
    // Re-run whenever anything that could change the answer changes.
    //
    // `.git/HEAD` alone is not enough, and the gap is not academic: emitting any
    // `rerun-if` instruction switches off Cargo's default "rescan the whole
    // package" behaviour, so with only HEAD declared, editing a source file
    // rebuilt the crate *without* rerunning this script. The stamp kept whatever
    // it said before -- a build from a modified tree still claiming to be the
    // clean commit it started from. That is precisely the claim the stamp exists
    // to make trustworthy, so the sources are declared too.
    for path in [
        "src",
        "tests",
        "assets",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        // Moves on commit, checkout and branch switch.
        ".git/HEAD",
        // Moves on `git add` and `git commit`, which is how a tree stops being
        // dirty without any file changing.
        ".git/index",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    embed_resources();

    println!("cargo:rustc-env=SLUICE_COMMIT={}", commit());
    println!("cargo:rustc-env=SLUICE_BUILD_DATE={}", build_date());
    // Cargo hands build scripts the real target triple; `std::env::consts::ARCH`
    // is only half of it and would call an i686 build "x86".
    println!(
        "cargo:rustc-env=SLUICE_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
}

/// Hand the linker the Explorer icon and the file properties.
///
/// `assets/sluice.res` is compiled *and committed*, rather than built here from
/// `sluice.rc`. Both alternatives cost more than they are worth: a build
/// dependency such as `winresource` would churn a lockfile this project pins
/// deliberately, and either route needs `rc.exe` from a versioned Windows SDK
/// path that every builder would then have to have. A checked-in `.res` links
/// on any machine with nothing but the toolchain, and the `.rc` and the
/// generator sit beside it so the blob is reproducible rather than mysterious.
///
/// Regenerate with `assets/mkicon.ps1`, then rc.exe — see `assets/sluice.rc`.
///
/// Scoped to the binary: a `.res` on the test harness' link line is at best
/// noise. Skipped rather than failed when absent, because an icon is not worth
/// a build.
fn embed_resources() {
    let res = std::path::Path::new("assets").join("sluice.res");
    if !res.is_file() {
        println!("cargo:warning=assets/sluice.res missing — building without an icon");
        return;
    }
    let abs = std::fs::canonicalize(&res).unwrap_or(res);
    // `canonicalize` yields a `\\?\` path, which link.exe does not accept here.
    let abs = abs.to_string_lossy().replace(r"\\?\", "");
    println!("cargo:rustc-link-arg-bin=sluice={abs}");
}

/// Short commit hash, with `+` appended when the tree had uncommitted changes.
///
/// A binary built from a dirty tree corresponds to no commit anybody can check
/// out, and that is exactly the build you least want to be mystified by later.
fn commit() -> String {
    let Some(sha) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if !s.is_empty() => format!("{sha}+"),
        _ => sha,
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Build date as `YYYY-MM-DD` UTC.
///
/// Honours `SOURCE_DATE_EPOCH` so a reproducible-build harness can pin it.
fn build_date() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    civil_date(secs.div_euclid(86_400))
}

/// Howard Hinnant's `civil_from_days`, so this needs no build dependency.
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
