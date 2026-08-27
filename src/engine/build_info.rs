//! Which build said so.
//!
//! Every verdict this program issues is a claim about somebody's only remaining
//! copy. When such a claim is doubted -- a file turns up corrupt, a manifest
//! disagrees with a drive -- the first useful question is *which build produced
//! this*, and the answer has to be in the artifact rather than in somebody's
//! memory of which exe they copied to the laptop.
//!
//! So the commit and build date are stamped into the binary by `build.rs` and
//! reproduced in `--version`, in every session log, and in every manifest's
//! `<creatorinfo>`.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short commit hash. `unknown` when built outside a git checkout; a trailing
/// `+` means the working tree had uncommitted changes, so the binary matches no
/// commit anybody can check out.
pub const COMMIT: &str = env!("SLUICE_COMMIT");

/// `YYYY-MM-DD`, UTC.
pub const BUILD_DATE: &str = env!("SLUICE_BUILD_DATE");

/// The target triple this was compiled for, e.g. `x86_64-pc-windows-msvc`.
pub const TARGET: &str = env!("SLUICE_TARGET");

/// `sluice 0.1.0 (a1b2c3d4e5f6 2026-08-26)` -- the identity that goes into
/// manifests and logs.
pub fn stamp() -> String {
    format!("sluice {VERSION} ({COMMIT} {BUILD_DATE})")
}

/// Whether this binary was built from a tree with uncommitted changes.
pub fn is_dirty() -> bool {
    COMMIT.ends_with('+')
}

/// The multi-line `--version` output.
pub fn long_version() -> String {
    let mut s = format!(
        "sluice {VERSION}\ncommit      {COMMIT}\nbuilt       {BUILD_DATE}\ntarget      {TARGET}\n"
    );
    if is_dirty() {
        s.push_str(
            "\nThis build came from a working tree with uncommitted changes, so it\n\
             corresponds to no commit that can be checked out.\n",
        );
    }
    s.push_str(
        "\nsluice makes no network connections of any kind. It has no telemetry,\n\
         no update check, and no HTTP client linked in.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The triple comes from Cargo rather than from `std::env::consts`, which
    /// only carries the architecture half.
    #[test]
    fn target_is_a_full_triple() {
        assert!(
            TARGET.matches('-').count() >= 2,
            "expected a target triple, got {TARGET}"
        );
    }

    #[test]
    fn stamp_names_a_build() {
        let s = stamp();
        assert!(s.starts_with("sluice "));
        assert!(s.contains(COMMIT), "{s} must name the commit");
        assert!(s.contains(BUILD_DATE), "{s} must name the build date");
    }

    /// The date comes from `build.rs`, and a silently broken civil-date
    /// conversion there would be invisible everywhere else.
    #[test]
    fn build_date_is_a_calendar_date() {
        let parts: Vec<&str> = BUILD_DATE.split('-').collect();
        assert_eq!(parts.len(), 3, "expected YYYY-MM-DD, got {BUILD_DATE}");
        let y: i32 = parts[0].parse().expect("year");
        let m: u32 = parts[1].parse().expect("month");
        let d: u32 = parts[2].parse().expect("day");
        assert!(
            (2024..2100).contains(&y),
            "implausible year in {BUILD_DATE}"
        );
        assert!((1..=12).contains(&m), "bad month in {BUILD_DATE}");
        assert!((1..=31).contains(&d), "bad day in {BUILD_DATE}");
    }
}
