//! Windows platform bits: keep-awake, volume identity, free space, sector size,
//! physical device identity, and the lid-close power policy.

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, PowerReadACValueIndex, PowerReadDCValueIndex, SetThreadExecutionState,
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SYSTEM_POWER_STATUS,
};
use windows_sys::Win32::System::SystemServices::{
    GUID_LIDCLOSE_ACTION, GUID_SYSTEM_BUTTON_SUBGROUP,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex};
use windows_sys::Win32::System::IO::DeviceIoControl;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn last_error(what: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::last_os_error()).context(what.to_string())
}

// ---------------------------------------------------------------------------
// Keep-awake
// ---------------------------------------------------------------------------

/// RAII guard over `SetThreadExecutionState`, held for the duration of a job.
///
/// **This blocks idle sleep only.** A lid close is a user-initiated power
/// transition and no process can veto it, so a laptop set to sleep on lid close
/// will still sleep mid-copy. That is a `powercfg` setting, not something this
/// program can fix -- see [`lid_policy`], which preflight reports on.
///
/// The execution state is per-*thread*, so this guard must live on a thread that
/// outlives the job. The orchestrator thread holds it.
pub struct KeepAwake {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl KeepAwake {
    pub fn arm() -> Result<Self> {
        // SAFETY: no pointers involved; the call is infallible apart from its
        // return value, which is 0 on failure.
        let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        if prev == 0 {
            return Err(last_error("SetThreadExecutionState(ES_SYSTEM_REQUIRED)"));
        }
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // Clearing the requirement flags while staying continuous is the
        // documented way to release without resetting the idle timer.
        //
        // SAFETY: as above.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

// ---------------------------------------------------------------------------
// Volume identity
// ---------------------------------------------------------------------------

/// Everything preflight captures about one mounted volume.
///
/// Two identical LaCie Rugged drives swap letters between plug-ins, so the
/// identity that matters is the serial and the physical device number, not `D:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeInfo {
    /// Mount point, e.g. `D:\`.
    pub root: String,
    pub label: String,
    /// `GetVolumeInformationW` serial. Identifies the *volume*, not the disk.
    pub serial: u32,
    pub filesystem: String,
    /// Logical bytes per sector. 4096 alignment is a superset, so this is
    /// reported for the device strip rather than used to size reads.
    pub sector_size: u32,
    /// `\\?\Volume{...}\`, when the mount point resolves to one.
    pub guid: Option<String>,
    /// Physical disk index. The only field that can prove two destinations are
    /// different *drives* rather than different partitions.
    pub device_number: Option<u32>,
}

impl VolumeInfo {
    /// Serial as the 8 hex digits shown on the device strip, e.g. `3A2F0D18`.
    pub fn serial_hex(&self) -> String {
        format!("{:08X}", self.serial)
    }

    /// The label, or an empty string when the volume has none.
    pub fn volume_label_or_none(&self) -> &str {
        &self.label
    }

    /// One-line identity for logs and the device strip.
    pub fn describe(&self) -> String {
        let label = if self.label.is_empty() {
            "(no label)"
        } else {
            &self.label
        };
        format!(
            "{} {} · {} · {}",
            self.root,
            label,
            self.filesystem,
            self.serial_hex()
        )
    }
}

/// The mount point containing `path`, e.g. `D:\` for `D:\foo\bar`.
pub fn volume_root(path: &Path) -> Result<String> {
    let raw = path.to_str().ok_or_else(|| {
        anyhow!(
            "path is not valid UTF-16-compatible text: {}",
            path.display()
        )
    })?;
    let mut buf = [0u16; 260];
    // SAFETY: `buf` is writable for its full length, which is what we pass.
    let ok = unsafe { GetVolumePathNameW(wide(raw).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return Err(last_error(&format!(
            "GetVolumePathNameW({})",
            path.display()
        )));
    }
    Ok(from_wide(&buf))
}

/// Capture the full identity of the volume containing `path`.
pub fn volume_info(path: &Path) -> Result<VolumeInfo> {
    volume_info_with(path, &RealDeviceProbe)
}

/// [`volume_info`] with the device probe injected. See [`DeviceProbe`].
pub fn volume_info_with(path: &Path, probe: &dyn DeviceProbe) -> Result<VolumeInfo> {
    let root = volume_root(path)?;
    let root_w = wide(&root);

    let mut label = [0u16; 256];
    let mut fs = [0u16; 256];
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    // SAFETY: every out-pointer is a live local of the length we declare.
    let ok = unsafe {
        GetVolumeInformationW(
            root_w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error(&format!("GetVolumeInformationW({root})")));
    }

    Ok(VolumeInfo {
        sector_size: logical_sector_size(&root).unwrap_or(0),
        guid: volume_guid(&root).ok(),
        device_number: probe.device_number(&root, path),
        root,
        label: from_wide(&label),
        serial,
        filesystem: from_wide(&fs),
    })
}

/// Logical bytes per sector for a mount point.
pub fn logical_sector_size(root: &str) -> Result<u32> {
    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;
    // SAFETY: four live u32 out-params.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide(root).as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 {
        return Err(last_error(&format!("GetDiskFreeSpaceW({root})")));
    }
    Ok(bytes_per_sector)
}

/// `\\?\Volume{GUID}\` for a mount point.
pub fn volume_guid(root: &str) -> Result<String> {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is writable for the length passed.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(wide(root).as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if ok == 0 {
        return Err(last_error(&format!(
            "GetVolumeNameForVolumeMountPointW({root})"
        )));
    }
    Ok(from_wide(&buf))
}

/// Physical disk index behind a mount point.
///
/// This is the check the format verdict rests on. Volume serials differ between
/// two partitions of a single disk, so serial-distinctness alone would happily
/// bless two folders on one drive as two independent copies.
pub fn physical_device_number(root: &str) -> Result<u32> {
    let letter = root
        .chars()
        .next()
        .ok_or_else(|| anyhow!("empty volume root"))?;
    if !letter.is_ascii_alphabetic() {
        bail!("volume root {root:?} is not a drive letter, cannot query a device number");
    }
    let device = format!(r"\\.\{}:", letter.to_ascii_uppercase());

    // Zero desired access is enough for this IOCTL and, unlike read access,
    // needs no elevation.
    //
    // SAFETY: a NUL-terminated wide path, null security attributes, null
    // template handle -- all valid arguments to CreateFileW.
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide(&device).as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error(&format!("CreateFileW({device})")));
    }
    let guard = HandleGuard(handle);

    let mut sdn = STORAGE_DEVICE_NUMBER {
        DeviceType: 0,
        DeviceNumber: 0,
        PartitionNumber: 0,
    };
    let mut returned: u32 = 0;
    // SAFETY: `sdn` is a live, correctly-sized output buffer for this IOCTL.
    let ok = unsafe {
        DeviceIoControl(
            guard.0,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            ptr::null(),
            0,
            &mut sdn as *mut _ as *mut c_void,
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error(&format!(
            "DeviceIoControl(IOCTL_STORAGE_GET_DEVICE_NUMBER, {device})"
        )));
    }
    Ok(sdn.DeviceNumber)
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live handle from CreateFileW, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// How the engine learns which physical disk a path is on.
///
/// Exists so the clean SAFE TO FORMAT path can be tested at all: it needs two
/// genuinely different physical drives, and no temp directory, `subst` mapping,
/// or second partition can supply one -- the distinctness check correctly
/// rejects every one of them. A test injects a probe that reports two temp
/// directories as different disks.
///
/// Deliberately a constructor parameter and **never a runtime flag, environment
/// variable, or config key**. A switch that could fake device distinctness in a
/// shipped binary is precisely the thing that could bless a bad format, so it
/// must not be reachable from one.
pub trait DeviceProbe: Send + Sync + std::fmt::Debug {
    /// The physical disk index behind a path.
    ///
    /// `root` is the path's mount point, which is what the real implementation
    /// queries; `path` is what was actually asked about, which is what lets a
    /// test tell two folders on one volume apart.
    fn device_number(&self, root: &str, path: &Path) -> Option<u32>;
}

/// The real probe: `IOCTL_STORAGE_GET_DEVICE_NUMBER`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDeviceProbe;

impl DeviceProbe for RealDeviceProbe {
    fn device_number(&self, root: &str, _path: &Path) -> Option<u32> {
        physical_device_number(root).ok()
    }
}

/// Whether two volumes sit on different physical drives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail")]
pub enum Distinctness {
    /// Proven different physical disks.
    Distinct,
    /// Proven the same disk or the same volume. Never bless a format on this.
    SameDevice,
    /// Neither could be proven. The verdict treats this exactly like
    /// `SameDevice`: it refuses to authorise an erase on an unproven claim.
    Unproven(String),
}

/// The §5 check: prove two destinations are different devices before blessing
/// an erase, because two identical LaCies can be mounted such that both paths
/// land on one drive and produce a perfect-looking result with a single copy.
pub fn distinctness(a: &VolumeInfo, b: &VolumeInfo) -> Distinctness {
    if let (Some(x), Some(y)) = (a.device_number, b.device_number) {
        return if x == y {
            Distinctness::SameDevice
        } else {
            Distinctness::Distinct
        };
    }

    // Without a device number we can still prove *sameness*, never difference:
    // two partitions of one disk have distinct volume GUIDs and serials.
    if a.guid.is_some() && a.guid == b.guid {
        return Distinctness::SameDevice;
    }
    if a.serial == b.serial {
        return Distinctness::SameDevice;
    }
    Distinctness::Unproven(format!(
        "no physical device number for {} or {}; distinct volume serials ({} / {}) \
         do not rule out two partitions of one disk",
        a.root,
        b.root,
        a.serial_hex(),
        b.serial_hex()
    ))
}

// ---------------------------------------------------------------------------
// Free space
// ---------------------------------------------------------------------------

/// Bytes available to this caller on the volume containing `path`.
pub fn free_space(path: &Path) -> Result<u64> {
    Ok(disk_space(path)?.0)
}

/// `(free to caller, total capacity)` for the volume containing `path`.
pub fn disk_space(path: &Path) -> Result<(u64, u64)> {
    let root = volume_root(path)?;
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: three live u64 out-params.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide(&root).as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return Err(last_error(&format!("GetDiskFreeSpaceExW({root})")));
    }
    Ok((free_to_caller, total))
}

// ---------------------------------------------------------------------------
// Long paths
// ---------------------------------------------------------------------------

/// Length past which a path is given the extended prefix. Under the classic
/// `MAX_PATH` of 260, with room for a filename.
const MAX_PATH_SAFE: usize = 240;

/// Prefix a path with `\\?\` once it is long enough to need it.
///
/// A session folder under a deep destination, plus a camera tree, can exceed 260
/// characters, and the Win32 API refuses past that with an error that reads like
/// a missing directory. The prefix also disables path normalisation, so it is
/// only ever applied to an absolute path -- prefixing a relative one would break
/// it rather than lengthen it.
pub fn extended_path(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if s.len() < MAX_PATH_SAFE || s.starts_with("\\\\?\\") || !p.is_absolute() {
        return p.to_path_buf();
    }
    match s.strip_prefix("\\\\") {
        Some(unc) => PathBuf::from(format!("\\\\?\\UNC\\{unc}")),
        None => PathBuf::from(format!("\\\\?\\{s}")),
    }
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

/// `ERROR_ALREADY_EXISTS`
const ERROR_ALREADY_EXISTS: u32 = 183;

/// A lock held for the length of a job, so two copies of sluice cannot write
/// into one session folder and interleave each other's files.
pub struct SingleInstance {
    handle: HANDLE,
}

// SAFETY: the handle is owned solely by this struct and closed once on drop.
unsafe impl Send for SingleInstance {}

impl SingleInstance {
    /// Lock keyed to one destination folder.
    ///
    /// Per-destination rather than global on purpose: the hazard is two jobs
    /// writing *the same* session folder, not two jobs existing. Offloading one
    /// card pair to the LaCies while another copy runs to a different drive is
    /// legitimate and stays allowed.
    ///
    /// The path is hashed because a mutex name cannot contain a backslash.
    pub fn for_destination(path: &Path) -> Result<Option<Self>> {
        let key = path.to_string_lossy().to_ascii_lowercase();
        let h = xxhash_rust::xxh64::xxh64(key.as_bytes(), 0);
        Self::acquire(&format!("sluice-dest-{h:016x}"))
    }

    /// Take a named lock. `Ok(None)` means another instance already holds it.
    ///
    /// Session-local rather than `Global\`, which is the right scope -- the
    /// hazard is two windows on one desktop, and `Global\` needs privileges this
    /// does not otherwise require.
    pub fn acquire(name: &str) -> Result<Option<Self>> {
        let name = wide(name);
        // SAFETY: a NUL-terminated wide name, null security attributes.
        let handle = unsafe { CreateMutexW(ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error("CreateMutexW"));
        }
        // SAFETY: reads the calling thread's last error, set by CreateMutexW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: a live handle from CreateMutexW, closed once.
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: owned by this struct, released and closed exactly once. Win32
        // mutex ownership is per-thread, and a job runs start to finish on the
        // thread that acquired this.
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Enumerating what is plugged in
// ---------------------------------------------------------------------------

/// What kind of thing a volume is, so the picker can put SD cards at the top of
/// a card slot and hard drives at the top of a destination slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DriveType {
    Removable,
    Fixed,
    Remote,
    CdRom,
    RamDisk,
    Unknown,
}

impl DriveType {
    fn from_raw(v: u32) -> Self {
        match v {
            2 => Self::Removable,
            3 => Self::Fixed,
            4 => Self::Remote,
            5 => Self::CdRom,
            6 => Self::RamDisk,
            _ => Self::Unknown,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Removable => "removable",
            Self::Fixed => "fixed",
            Self::Remote => "network",
            Self::CdRom => "optical",
            Self::RamDisk => "ram disk",
            Self::Unknown => "unknown",
        }
    }

    /// Cards are removable; nothing else is worth offering as a card.
    pub fn is_card_like(self) -> bool {
        matches!(self, Self::Removable)
    }

    /// A destination has to be something that will still be there tomorrow.
    pub fn is_dest_like(self) -> bool {
        matches!(self, Self::Fixed | Self::Remote)
    }

    /// Whether an unbuffered read of this volume actually reaches a device.
    ///
    /// This is the load-bearing assumption of the whole program. On a local
    /// disk, `FILE_FLAG_NO_BUFFERING` bypasses the page cache and the verify
    /// pass is a genuinely independent trip to the platter. Over SMB it is not:
    /// the flag is advisory to the redirector, and the bytes can be served from
    /// the client's cache or the server's, so the verify stops being independent
    /// evidence and becomes an expensive way to re-read what was just written.
    ///
    /// A network destination is still a perfectly good *place to put files*.
    /// What it cannot do is contribute to a verdict that authorises erasing the
    /// original.
    pub fn verification_reaches_the_device(self) -> bool {
        !matches!(self, Self::Remote)
    }
}

/// What kind of volume a path lives on.
///
/// Takes any path, not just a root: a destination is a session folder several
/// levels down, and the question "is this a network share" is about the volume
/// it sits on. Falls back to `Unknown` -- which is treated as local -- when the
/// volume cannot be resolved, because refusing to verify on a technicality is
/// worse than verifying something that turns out to be a share.
pub fn drive_type_of(path: &Path) -> DriveType {
    let Ok(root) = volume_root(path) else {
        return DriveType::Unknown;
    };
    // SAFETY: a NUL-terminated wide root path.
    DriveType::from_raw(unsafe { GetDriveTypeW(wide(&root).as_ptr()) })
}

/// Which drive letters exist right now, one bit per letter.
///
/// Deliberately the cheapest question that can be asked about the drives.
/// `GetLogicalDrives` reads the drive-letter table and touches no device, so it
/// can be polled continuously for nothing — unlike [`mounted_volumes`], which
/// queries every volume for label, filesystem, free space and device number,
/// and will spin up a sleeping disk to answer. Watch the free thing; do the
/// expensive one only when it changes.
///
/// A mask says *that* something changed, never what: an unplug and a replug
/// inside one poll interval is invisible. That is a fair trade for a check
/// which costs a single syscall.
pub fn drive_letter_mask() -> u32 {
    // SAFETY: no arguments, no out-parameters; returns a bitmask.
    unsafe { GetLogicalDrives() }
}

/// One mounted volume, as the picker lists it.
#[derive(Debug, Clone)]

pub struct MountedVolume {
    pub info: VolumeInfo,
    pub drive_type: DriveType,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl MountedVolume {
    /// The line the picker shows: identity first, letter second.
    ///
    /// Two identical LaCies swap letters between plug-ins, so `D:` is the least
    /// trustworthy thing on this line and the serial is the most.
    pub fn describe(&self) -> String {
        let label = if self.info.label.is_empty() {
            "(no label)"
        } else {
            &self.info.label
        };
        let disk = match self.info.device_number {
            Some(n) => format!("disk {n}"),
            None => "disk ?".into(),
        };
        // A network share looks exactly like a local drive on this line, and the
        // difference decides whether it can ever contribute to a format verdict.
        // Saying so at the moment of choosing is the entire point of listing
        // volumes by identity rather than by letter.
        let caveat = if self.drive_type.verification_reaches_the_device() {
            String::new()
        } else {
            format!(
                "  —  {}, cannot be verified off the device",
                self.drive_type.describe()
            )
        };
        format!(
            "{:<4} {label}  ·  {}  ·  {}  ·  {}  ·  {:.0} GB free{caveat}",
            self.info.root,
            self.info.serial_hex(),
            self.info.filesystem,
            disk,
            self.free_bytes as f64 / 1e9
        )
    }
}

/// Every mounted drive letter, with its identity.
///
/// Optical drives and empty reader slots are skipped: a volume that cannot be
/// interrogated is not one you can offload to or from.
pub fn mounted_volumes() -> Vec<MountedVolume> {
    // SAFETY: no arguments, returns a bitmask of present drive letters.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        // SAFETY: a NUL-terminated wide root path.
        let drive_type = DriveType::from_raw(unsafe { GetDriveTypeW(wide(&root).as_ptr()) });
        if matches!(drive_type, DriveType::CdRom | DriveType::Unknown) {
            continue;
        }
        // An empty card reader slot reports as removable but has no volume.
        let Ok(info) = volume_info(Path::new(&root)) else {
            continue;
        };
        let (free_bytes, total_bytes) = disk_space(Path::new(&root)).unwrap_or((0, 0));
        out.push(MountedVolume {
            info,
            drive_type,
            free_bytes,
            total_bytes,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Screen geometry
// ---------------------------------------------------------------------------

/// The primary monitor's work area in physical pixels: the desktop minus the
/// taskbar.
///
/// The window's default size was hardcoded and nothing ever asked how big the
/// screen was. A 1920x1080 laptop at Windows' default 150% scaling is a
/// 1280x720-*point* desktop, so a 1280x900 window ran a quarter of the way off
/// the bottom -- and the part that goes is the bottom panel, which is the
/// verdict banner. `banner.rs` puts it there specifically so it can never be
/// pushed off screen by a long log, and the window geometry undid that.
pub fn work_area() -> Option<(f32, f32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};

    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: SPI_GETWORKAREA writes a RECT into pvparam; the buffer is a live,
    // correctly sized RECT and no update flags are passed.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            (&mut r as *mut RECT).cast(),
            Default::default(),
        )
    };
    if ok == 0 {
        return None;
    }
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// The system DPI as a points-per-pixel scale, e.g. `1.5` at 150%.
///
/// Read before any window exists, so it cannot come from the viewport. Windows
/// reports 96 DPI as 100%.
pub fn system_scale() -> f32 {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, LOGPIXELSX};

    // SAFETY: a NULL hwnd asks for the screen DC, which is released below.
    unsafe {
        let dc = GetDC(std::ptr::null_mut());
        if dc.is_null() {
            return 1.0;
        }
        let dpi = GetDeviceCaps(dc, LOGPIXELSX as i32);
        ReleaseDC(std::ptr::null_mut(), dc);
        if dpi > 0 {
            dpi as f32 / 96.0
        } else {
            1.0
        }
    }
}

/// A window size that fits on this screen.
///
/// `scale` is the viewport's points-per-pixel; the work area comes back in
/// physical pixels and the requested size is in points. A margin is left so the
/// title bar and a shadow still have somewhere to be.
pub fn fit_to_screen(preferred: (f32, f32), scale: f32) -> (f32, f32) {
    fit_within(preferred, work_area(), scale)
}

/// The pure half, so a test can supply a screen this machine does not have.
///
/// A parameter rather than a runtime switch, per the rule that test seams are
/// constructor parameters: a shipped binary reaches this only through
/// [`fit_to_screen`], which always asks the real screen.
///
/// `screen` is `None` when the work-area query failed, and the only honest
/// answer then is to change nothing.
fn fit_within(preferred: (f32, f32), screen: Option<(f32, f32)>, scale: f32) -> (f32, f32) {
    let Some((pw, ph)) = screen else {
        return preferred;
    };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let (max_w, max_h) = (pw / scale - 16.0, ph / scale - 16.0);
    (
        preferred.0.min(max_w).max(640.0),
        preferred.1.min(max_h).max(480.0),
    )
}

// ---------------------------------------------------------------------------
// Elevation
// ---------------------------------------------------------------------------

/// Whether this process is running elevated.
///
/// Reported by `sluice doctor` and used nowhere else. Elevation is the first
/// thing people try when Windows denies them something, and it is worth being
/// able to say plainly that it changes nothing here: proving two destinations
/// are different physical drives works as an ordinary user, which is the only
/// privileged-looking thing this program does.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::HANDLE as WinHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: WinHandle = std::ptr::null_mut();
    // SAFETY: an out-parameter for a handle we close below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    // `TOKEN_ELEVATION` is a single DWORD.
    let mut elevation: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: the buffer matches the size passed, and the class returns a DWORD.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    };
    // SAFETY: opened above, closed exactly once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation != 0
}

// ---------------------------------------------------------------------------
// Cloud placeholders
// ---------------------------------------------------------------------------

/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` -- a OneDrive/Dropbox file whose
/// contents live in the cloud and are fetched on first read.
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` -- the older placeholder flavour.
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// `FILE_ATTRIBUTE_OFFLINE` -- HSM-style stub, same problem.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;

/// Whether a file's bytes are not actually on this machine.
///
/// Hashing a placeholder is not a read: it silently triggers a download of the
/// whole file, which can be gigabytes over somebody's hotel wifi, and what comes
/// back was never on the local device at all. "Verified off the physical media"
/// would be a false claim about such a file, so they are refused rather than
/// quietly hydrated.
pub fn is_cloud_placeholder(attrs: u32) -> bool {
    attrs
        & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_OFFLINE)
        != 0
}

// ---------------------------------------------------------------------------
// Writability
// ---------------------------------------------------------------------------

/// Why a destination could not be written to, in words that name the fix.
///
/// Windows reports Controlled Folder Access -- the ransomware protection that is
/// on by default and covers Documents, Pictures, Videos and Desktop -- as a
/// generic access-denied. A photographer pointing a destination at their
/// Pictures folder gets "Access is denied. (os error 5)" and no idea why, which
/// is the single most likely way a first run fails on somebody else's machine.
pub fn explain_write_failure(dir: &Path, err: &std::io::Error) -> String {
    let base = format!("cannot write to {}: {err}", dir.display());
    if err.kind() != std::io::ErrorKind::PermissionDenied {
        return base;
    }
    if in_a_protected_folder(dir) {
        format!(
            "{base}\n\nThis looks like a folder Windows protects by default. Controlled \
             Folder Access (Windows Security > Virus & threat protection > Ransomware \
             protection) blocks programs it does not know from writing to Documents, \
             Pictures, Videos and Desktop, and reports it as a plain access-denied.\n\n\
             Either allow sluice through it, or choose a destination outside those folders \
             -- which is the better answer anyway, since the point is a drive that can be \
             unplugged and carried somewhere else."
        )
    } else {
        format!(
            "{base}\n\nIf this drive is not read-only, check whether antivirus or a \
             folder-protection feature is blocking writes to it."
        )
    }
}

/// Whether a path sits under one of the user folders Controlled Folder Access
/// guards by default.
fn in_a_protected_folder(dir: &Path) -> bool {
    let Ok(profile) = std::env::var("USERPROFILE") else {
        return false;
    };
    let lower = dir.to_string_lossy().to_lowercase();
    let profile = profile.to_lowercase();
    ["documents", "pictures", "videos", "desktop", "music"]
        .iter()
        .any(|f| lower.starts_with(&format!("{profile}\\{f}")))
}

// ---------------------------------------------------------------------------
// How fast this connection actually is
// ---------------------------------------------------------------------------

/// Bytes written by [`measure_write_mbps`]. Big enough to outlast the burst a
/// drive absorbs at full speed, small enough to be free on a fast one: about
/// 1.5 s on USB 2, a fifth of a second on USB 3.
const SPEED_PROBE_BYTES: usize = 32 * 1024 * 1024;

/// The practical ceiling for sustained writes over USB 2.0.
///
/// The bus is 480 Mbit/s nominal, and real sustained writes land well under
/// this once protocol and filesystem overhead are paid. A USB 3 external disk
/// writes at 100 MB/s or better even when it is a spinning one, so the gap
/// either side of this number is wide rather than marginal.
const USB2_WRITE_CEILING: f64 = 50.0;

/// How fast this destination actually accepts data, in MB/s.
///
/// Measured rather than asked. Windows can be interrogated for a USB link
/// speed, but that means walking volume to disk to parent hub to port and
/// calling hub IOCTLs, and it answers the wrong question: a USB 3 drive behind
/// a saturated hub is exactly as slow as a USB 2 one. What matters is the rate
/// the copy is going to get.
///
/// Timed across the write *and* the flush. Without the flush a 32 MiB write
/// lands in the OS cache and returns instantly, which is the same reason the
/// copy path calls `sync_all` on every file.
pub fn measure_write_mbps(dir: &Path) -> Result<f64> {
    use std::io::Write;

    let path = dir.join(".sluice-speed-probe");
    let block = vec![0xA5u8; 1 << 20];
    let started = std::time::Instant::now();

    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&path)?;
        let mut left = SPEED_PROBE_BYTES;
        while left > 0 {
            let n = left.min(block.len());
            f.write_all(&block[..n])?;
            left -= n;
        }
        f.sync_all()
    })();
    let secs = started.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&path);
    result.map_err(|e| anyhow!("{}", explain_write_failure(dir, &e)))?;

    Ok(if secs > 0.0 {
        SPEED_PROBE_BYTES as f64 / 1.0e6 / secs
    } else {
        f64::INFINITY
    })
}

/// A duration a tired person reads at a glance: `1 h 42 m`, not `6120 s`.
fn plain_duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{} h {:02} m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} m {:02} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

/// What to say about a destination slow enough to be worth re-plugging.
///
/// `None` when the rate is unremarkable — the point of the check is the night
/// somebody has quietly plugged 4 TB of shoot into a 2.0 port and is about to
/// wait three hours for what should take forty minutes. Phrased as the likely
/// cause rather than a certainty, because a tired drive, a long cable and a
/// shared hub all look the same from here, and all four have the same fix.
pub fn slow_link_note(label: &str, mbps: f64, remaining: u64) -> Option<String> {
    if !mbps.is_finite() || mbps <= 0.0 || mbps > USB2_WRITE_CEILING {
        return None;
    }
    let copy_secs = remaining as f64 / (mbps * 1.0e6);
    Some(format!(
        "{label} accepts data at {mbps:.0} MB/s, at or below the USB 2.0 ceiling -- most \
         likely a 2.0 port, hub or cable, since a USB 3 connection is several times faster. \
         At this rate the copy alone needs about {}, and the verify pass follows it. Moving \
         the drive to another port now costs seconds.",
        plain_duration(copy_secs)
    ))
}

/// Prove a destination is writable before spending twenty minutes finding out.
///
/// Creates and removes a small file. The alternative -- discovering it on the
/// first real write -- means a half-populated session folder and a user who now
/// has to work out which files made it.
pub fn probe_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(".sluice-write-probe");
    match std::fs::write(&probe, b"sluice") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => bail!("{}", explain_write_failure(dir, &e)),
    }
}

// ---------------------------------------------------------------------------
// Lid-close policy
// ---------------------------------------------------------------------------

/// What the active power scheme does when the lid closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LidAction {
    DoNothing,
    Sleep,
    Hibernate,
    ShutDown,
    Unknown(u32),
}

impl LidAction {
    fn from_index(i: u32) -> Self {
        match i {
            0 => Self::DoNothing,
            1 => Self::Sleep,
            2 => Self::Hibernate,
            3 => Self::ShutDown,
            other => Self::Unknown(other),
        }
    }

    /// Whether this setting would interrupt a running job.
    pub fn interrupts_job(self) -> bool {
        !matches!(self, Self::DoNothing)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::DoNothing => "do nothing",
            Self::Sleep => "sleep",
            Self::Hibernate => "hibernate",
            Self::ShutDown => "shut down",
            Self::Unknown(_) => "unrecognised",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LidPolicy {
    /// On mains power, which is how an overnight offload should be running.
    pub ac: LidAction,
    /// On battery.
    pub dc: LidAction,
}

/// The `powercfg` command that fixes a lid-close setting which would kill a job.
///
/// Printed rather than run: changing somebody's power scheme without being asked
/// is not this program's business, and the GUIDs are locale-independent even
/// where the human-readable names are not.
pub const LID_FIX_COMMAND: &str = concat!(
    "powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS ",
    "5ca83367-6e45-459f-a27b-476b1d01c936 0 && powercfg /setactive SCHEME_CURRENT"
);

/// Read the active scheme's lid-close action.
///
/// Preflight surfaces this because [`KeepAwake`] cannot block a lid close, and a
/// laptop that sleeps when the lid shuts is the most likely way a night's
/// offload dies half-finished.
///
/// Read through `powrprof` rather than by parsing `powercfg /q` output. The
/// earlier implementation matched the literal string `Current AC Power Setting
/// Index`, which Windows translates: on a French or Japanese machine the line
/// never matched, the function returned `Ok(None)`, and `None` is how this
/// module says *this is a desktop, there is no lid*. Every non-English laptop
/// silently lost the one warning that stops a lid close killing a 20-minute
/// copy. The registry index is a number in every locale.
///
/// Returns `Ok(None)` on a machine that genuinely has no lid-close setting.
pub fn lid_policy() -> Result<Option<LidPolicy>> {
    // A NULL scheme GUID means "the active scheme", which is what preflight is
    // asking about. The root power key argument is reserved and must be null.
    //
    // SAFETY: three GUID pointers to statics, one out-parameter to a live u32,
    // and the documented null for the reserved key.
    let read = |ac: bool| -> Option<u32> {
        let mut index: u32 = 0;
        let rc = unsafe {
            let f = if ac {
                PowerReadACValueIndex
            } else {
                PowerReadDCValueIndex
            };
            f(
                ptr::null_mut(),
                ptr::null(),
                &GUID_SYSTEM_BUTTON_SUBGROUP,
                &GUID_LIDCLOSE_ACTION,
                &mut index,
            )
        };
        (rc == ERROR_SUCCESS).then_some(index)
    };

    match (read(true), read(false)) {
        (Some(ac), Some(dc)) => Ok(Some(LidPolicy {
            ac: LidAction::from_index(ac),
            dc: LidAction::from_index(dc),
        })),
        // Both absent is the honest "no lid" answer. One present and one absent
        // is a machine reporting something this code does not model, and
        // guessing the missing half would be inventing a reassurance.
        (None, None) => Ok(None),
        _ => bail!("the lid-close setting is only half readable on this machine"),
    }
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

/// Mains or battery, and how much battery is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PowerStatus {
    /// `true` when the machine is on mains power.
    pub on_mains: bool,
    /// Remaining charge, when the machine reports one. Desktops report none.
    pub battery_percent: Option<u8>,
}

impl PowerStatus {
    /// Whether starting a long copy on this power is asking for trouble.
    ///
    /// A 91 GB offload takes the better part of twenty minutes with the disks
    /// working the whole way. That is not a battery-friendly workload, and a
    /// machine that dies at 80% leaves a half-written destination and a card the
    /// user is now afraid to trust.
    pub fn warns_before_a_long_copy(&self) -> Option<String> {
        if self.on_mains {
            return None;
        }
        Some(match self.battery_percent {
            Some(p) if p < 40 => format!(
                "running on battery at {p}% -- a long offload can outlast it, and a machine \
                 that dies mid-copy leaves a half-written destination"
            ),
            Some(p) => format!(
                "running on battery ({p}%) -- mains power is one less thing that can end this \
                 run early"
            ),
            None => "running on battery -- mains power is one less thing that can end this run \
                     early"
                .into(),
        })
    }
}

/// Mains-or-battery, for preflight.
pub fn power_status() -> Option<PowerStatus> {
    let mut st = SYSTEM_POWER_STATUS::default();
    // SAFETY: one out-parameter to a live, correctly sized struct.
    if unsafe { GetSystemPowerStatus(&mut st) } == 0 {
        return None;
    }
    Some(PowerStatus {
        // 1 is "on mains"; 0 is battery and 255 is "unknown". Unknown is treated
        // as mains: warning a desktop user about a battery it does not have is
        // the kind of noise that teaches people to ignore warnings.
        on_mains: st.ACLineStatus != 0,
        // 255 is the documented "unknown" sentinel.
        battery_percent: (st.BatteryLifePercent <= 100).then_some(st.BatteryLifePercent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(root: &str, serial: u32, device: Option<u32>) -> VolumeInfo {
        VolumeInfo {
            root: root.into(),
            label: "MT".into(),
            serial,
            filesystem: "exFAT".into(),
            sector_size: 4096,
            guid: Some(format!("\\\\?\\Volume{{{serial:08x}}}\\")),
            device_number: device,
        }
    }

    #[test]
    fn different_disks_are_distinct() {
        let a = vol("D:\\", 0x3A2F0D18, Some(1));
        let b = vol("G:\\", 0x7C190B4E, Some(2));
        assert_eq!(distinctness(&a, &b), Distinctness::Distinct);
    }

    /// Test 6: two folders on one physical drive must never read as distinct,
    /// even though their volume serials differ.
    #[test]
    fn two_partitions_of_one_disk_are_not_distinct() {
        let a = vol("D:\\", 0x3A2F0D18, Some(1));
        let b = vol("E:\\", 0x7C190B4E, Some(1));
        assert_eq!(distinctness(&a, &b), Distinctness::SameDevice);
    }

    #[test]
    fn same_volume_is_not_distinct() {
        let a = vol("D:\\", 0x3A2F0D18, None);
        let b = vol("D:\\", 0x3A2F0D18, None);
        assert_eq!(distinctness(&a, &b), Distinctness::SameDevice);
    }

    /// Missing device numbers must fall through to Unproven, never to Distinct:
    /// the verdict refuses to authorise an erase on an unproven claim.
    #[test]
    fn missing_device_numbers_are_unproven_not_distinct() {
        let a = vol("D:\\", 0x3A2F0D18, None);
        let b = vol("G:\\", 0x7C190B4E, None);
        assert!(matches!(distinctness(&a, &b), Distinctness::Unproven(_)));
    }

    /// Short paths are left alone: the prefix disables normalisation, so
    /// applying it needlessly costs more than it buys.
    #[test]
    fn short_paths_are_untouched() {
        let p = Path::new(r"D:\2026-03-14_shoot\DCIM\DSC00001.ARW");
        assert_eq!(extended_path(p), p);
    }

    #[test]
    fn long_paths_get_the_extended_prefix() {
        let deep = format!(r"D:\{}\DSC00001.ARW", r"nested\".repeat(40));
        let out = extended_path(Path::new(&deep));
        assert!(
            out.to_string_lossy().starts_with(r"\\?\D:"),
            "{}",
            out.display()
        );
    }

    #[test]
    fn a_long_unc_path_uses_the_unc_form() {
        let deep = format!(r"\\server\share\{}\x.ARW", r"nested\".repeat(40));
        let out = extended_path(Path::new(&deep));
        assert!(
            out.to_string_lossy().starts_with(r"\\?\UNC\server"),
            "{}",
            out.display()
        );
    }

    #[test]
    fn an_already_prefixed_path_is_not_prefixed_twice() {
        let deep = format!(r"\\?\D:\{}\x.ARW", r"nested\".repeat(40));
        assert_eq!(extended_path(Path::new(&deep)), Path::new(&deep));
    }

    /// A relative path must never be prefixed: the prefix turns off
    /// normalisation, so the result would name nothing.
    #[test]
    fn relative_paths_are_never_prefixed() {
        let deep = format!(r"{}\x.ARW", r"nested\".repeat(40));
        assert_eq!(extended_path(Path::new(&deep)), Path::new(&deep));
    }

    /// The second acquire fails while the first guard is alive, and succeeds
    /// once it drops.
    #[test]
    fn the_instance_lock_excludes_a_second_holder() {
        let dest = Path::new(r"D:6-09-11_shoot-01");
        let first = SingleInstance::for_destination(dest).unwrap();
        assert!(first.is_some(), "the first caller takes the lock");
        assert!(
            SingleInstance::for_destination(dest).unwrap().is_none(),
            "a second caller for the same folder must be turned away"
        );
        // A different destination is a different lock: two offloads to two
        // drives at once is legitimate.
        assert!(
            SingleInstance::for_destination(Path::new(r"G:6-09-11_shoot-01"))
                .unwrap()
                .is_some(),
            "an unrelated destination must not be blocked"
        );
        drop(first);
        assert!(
            SingleInstance::for_destination(dest).unwrap().is_some(),
            "and the lock must be released on drop"
        );
    }

    /// The setting indices are a Windows-wide contract, not something this
    /// program can choose. `0` is the only one that leaves a running job alone.
    #[test]
    fn lid_action_indices_match_the_documented_values() {
        assert_eq!(LidAction::from_index(0), LidAction::DoNothing);
        assert_eq!(LidAction::from_index(1), LidAction::Sleep);
        assert_eq!(LidAction::from_index(2), LidAction::Hibernate);
        assert_eq!(LidAction::from_index(3), LidAction::ShutDown);
        assert_eq!(LidAction::from_index(9), LidAction::Unknown(9));

        assert!(!LidAction::DoNothing.interrupts_job());
        for a in [
            LidAction::Sleep,
            LidAction::Hibernate,
            LidAction::ShutDown,
            LidAction::Unknown(9),
        ] {
            assert!(
                a.interrupts_job(),
                "{a:?} must count as interrupting: an unrecognised setting is not a safe one"
            );
        }
    }

    /// Reading the real policy must never fail on a machine that has one. This
    /// runs on CI, which is a virtual machine with no lid, so the assertion is
    /// only that it answers rather than errors.
    #[test]
    fn lid_policy_answers_without_parsing_localised_text() {
        let policy = lid_policy().expect("reading the lid setting must not fail");
        if let Some(p) = policy {
            // Whatever it says, both halves must be populated.
            let _ = (p.ac.describe(), p.dc.describe());
        }
    }

    #[test]
    fn mains_power_never_warns() {
        let st = PowerStatus {
            on_mains: true,
            battery_percent: Some(3),
        };
        assert_eq!(
            st.warns_before_a_long_copy(),
            None,
            "a plugged-in laptop with a flat battery is still plugged in"
        );
    }

    #[test]
    fn low_battery_warns_about_outlasting_the_copy() {
        let st = PowerStatus {
            on_mains: false,
            battery_percent: Some(22),
        };
        let w = st.warns_before_a_long_copy().expect("must warn");
        assert!(w.contains("22%"), "the number is the point: {w}");
        assert!(w.contains("half-written"));
    }

    #[test]
    fn healthy_battery_still_mentions_mains() {
        let st = PowerStatus {
            on_mains: false,
            battery_percent: Some(95),
        };
        let w = st
            .warns_before_a_long_copy()
            .expect("must still mention it");
        assert!(w.contains("battery"), "{w}");
        assert!(!w.contains("half-written"), "95% is not an emergency: {w}");
    }

    /// A desktop reports "unknown" for a battery it does not have. Warning about
    /// it is the kind of noise that teaches people to ignore warnings.
    #[test]
    fn unknown_ac_line_status_is_treated_as_mains() {
        let st = PowerStatus {
            on_mains: true,
            battery_percent: None,
        };
        assert_eq!(st.warns_before_a_long_copy(), None);
    }

    // --- placeholders, writability, verifiability -------------------------

    /// The window must fit the screen it is on. A 1280x900 default on a 1080p
    /// laptop at 150% scaling ran a quarter off the bottom -- taking the verdict
    /// banner with it, which is the one thing the layout guarantees is visible.
    ///
    /// Pinned against a supplied screen rather than the runner's. The earlier
    /// version asserted only the 640x480 floor, which holds whether or not the
    /// clamp exists -- deleting the clamp left the suite green.
    #[test]
    fn a_window_is_clamped_to_the_work_area() {
        // A 1920x1080 work area at 150% is 1280x720 points, less a 16-point
        // margin so the title bar and shadow still have somewhere to be.
        assert_eq!(
            fit_within((1280.0, 900.0), Some((1920.0, 1080.0)), 1.5),
            (1264.0, 704.0)
        );
        // Never grows a window to fill the screen.
        assert_eq!(
            fit_within((900.0, 600.0), Some((3840.0, 2160.0)), 1.0),
            (900.0, 600.0)
        );
        // A screen too small for the floor loses to the floor: an edge off a
        // tiny screen beats a window clamped to nothing.
        let (w, h) = fit_within((1280.0, 900.0), Some((800.0, 600.0)), 1.0);
        assert!(w >= 640.0 && h >= 480.0, "clamped to nothing: {w}x{h}");
    }

    /// The watcher compares this mask against its previous value, so a mask
    /// that came back empty would mean a drive change is never noticed.
    #[test]
    fn the_drive_letter_mask_reports_the_system_drive() {
        let mask = drive_letter_mask();
        assert_ne!(mask, 0, "no drive letters at all");
        // Bit 2 is C:, which every machine that can run this test has.
        assert_ne!(mask & (1 << 2), 0, "C: missing from mask {mask:#x}");
    }

    // --- connection speed -------------------------------------------------

    /// The night somebody has quietly plugged 4 TB of shoot into a 2.0 port.
    ///
    /// 119 GB at 22 MB/s is ninety minutes of copy before the verify pass even
    /// starts; the same card on USB 3 is well under half an hour. Measured on
    /// real hardware: two LaCies read at 41.6 MB/s and wrote at ~22.
    #[test]
    fn a_usb_2_destination_is_named_with_what_it_will_cost() {
        let note = slow_link_note("F:\\", 22.0, 119_000_000_000).expect("must warn");
        assert!(note.contains("22 MB/s"), "{note}");
        assert!(note.contains("USB 2.0"), "{note}");
        // The number that makes it worth acting on.
        assert!(note.contains("1 h 30 m"), "{note}");
        assert!(note.contains("another port"), "{note}");
    }

    /// A fast drive is not worth a sentence. A warning that fires every night
    /// is one people learn to scroll past.
    #[test]
    fn an_ordinary_connection_says_nothing() {
        assert_eq!(slow_link_note("F:\\", 140.0, 119_000_000_000), None);
        assert_eq!(slow_link_note("F:\\", 51.0, 119_000_000_000), None);
    }

    /// A measurement that came back nonsense must not invent a warning.
    #[test]
    fn an_unusable_measurement_is_not_a_warning() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(slow_link_note("F:\\", bad, 1_000_000_000), None, "{bad}");
        }
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(plain_duration(45.0), "45 s");
        assert_eq!(plain_duration(605.0), "10 m 05 s");
        assert_eq!(plain_duration(6120.0), "1 h 42 m");
    }

    /// A screen query that fails must not shrink the window to nothing.
    #[test]
    fn an_unknown_screen_leaves_the_preferred_size_alone() {
        assert_eq!(fit_within((1280.0, 900.0), None, 1.5), (1280.0, 900.0));
    }

    /// A viewport reporting no scale yet must read as 100%, rather than
    /// dividing by zero and clamping the window to the floor.
    #[test]
    fn a_non_positive_scale_is_read_as_100_percent() {
        let screen = Some((1920.0, 1080.0));
        assert_eq!(
            fit_within((1280.0, 900.0), screen, 0.0),
            fit_within((1280.0, 900.0), screen, 1.0)
        );
    }

    #[test]
    fn ordinary_files_are_not_placeholders() {
        // FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_NORMAL
        assert!(!is_cloud_placeholder(0x20));
        assert!(!is_cloud_placeholder(0x80));
        assert!(!is_cloud_placeholder(0));
    }

    #[test]
    fn every_placeholder_flavour_is_caught() {
        for attr in [0x0040_0000u32, 0x0004_0000, 0x0000_1000] {
            assert!(
                is_cloud_placeholder(attr | 0x20),
                "{attr:#x} alongside ARCHIVE must still read as a placeholder"
            );
        }
    }

    /// The whole program rests on an unbuffered read reaching a device. Over SMB
    /// it does not, and that has to be a property of the drive type rather than
    /// something remembered at each call site.
    #[test]
    fn network_volumes_cannot_supply_verification_evidence() {
        assert!(!DriveType::Remote.verification_reaches_the_device());
        for t in [
            DriveType::Fixed,
            DriveType::Removable,
            DriveType::RamDisk,
            DriveType::CdRom,
            DriveType::Unknown,
        ] {
            assert!(t.verification_reaches_the_device(), "{t:?}");
        }
        // A network share is still a fine place to put files.
        assert!(DriveType::Remote.is_dest_like());
    }

    #[test]
    fn a_writable_directory_probes_clean_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        probe_writable(dir.path()).expect("a temp dir must be writable");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            left.is_empty(),
            "the probe file must be cleaned up: {left:?}"
        );
    }

    #[test]
    fn a_missing_directory_fails_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        assert!(probe_writable(&dir.path().join("nope")).is_err());
    }

    /// An access-denied inside a Windows-protected folder is the most likely
    /// first-run failure on somebody else's machine, and "os error 5" tells them
    /// nothing.
    #[test]
    fn access_denied_in_a_protected_folder_names_the_feature() {
        let Ok(profile) = std::env::var("USERPROFILE") else {
            return;
        };
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = explain_write_failure(&PathBuf::from(profile).join("Pictures"), &denied);
        assert!(
            msg.contains("Controlled Folder Access"),
            "must name the thing to turn off: {msg}"
        );
    }

    #[test]
    fn other_errors_are_not_dressed_up_as_folder_protection() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = explain_write_failure(Path::new("Q:\nowhere"), &missing);
        assert!(!msg.contains("Controlled Folder Access"), "{msg}");
    }
}
