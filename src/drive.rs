use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Ssd,
    Hdd,
    /// Could not determine type.
    /// On Windows this variant is never produced: any failed TRIM query maps to `Hdd`.
    /// On Linux/macOS it is treated as SSD (parallel I/O), because the rotational-flag
    /// path is reliable for physical drives and unknown results are rare there.
    Unknown,
}

impl DriveKind {
    fn label(self) -> &'static str {
        match self {
            DriveKind::Ssd => "SSD",
            DriveKind::Hdd => "HDD",
            DriveKind::Unknown => "unknown",
        }
    }
}

/// Which endpoints sit on spinning media. Kept per endpoint rather than
/// collapsed into one flag: SRC and DST are assumed to be independent
/// devices, so work that touches only one side (walking a tree, hashing that
/// side's files) can run at that drive's own pace. Only copying - which reads
/// SRC and writes DST in the same operation - is bound by the slower of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveProfile {
    pub src_hdd: bool,
    pub dst_hdd: bool,
}

impl DriveProfile {
    /// Every copy touches both endpoints, so concurrent copies would cause a
    /// seek storm as soon as either side is spinning media.
    pub fn serial_copies(self) -> bool {
        self.src_hdd || self.dst_hdd
    }

    /// Force serial behavior on both endpoints (CLI/test override).
    pub fn all_hdd(hdd: bool) -> Self {
        Self {
            src_hdd: hdd,
            dst_hdd: hdd,
        }
    }
}

/// Detect the drive kind of both endpoints. Returns the profile plus a log
/// line describing what it means for this run.
///
/// On Windows, any failed TRIM query (UNC path, access denied, unsupported
/// IOCTL) defaults to HDD. On other platforms, unknown types default to SSD.
pub fn probe(src: &Path, dst: &Path) -> (DriveProfile, String) {
    let src_kind = detect(src);
    let dst_kind = detect(dst);
    let profile = DriveProfile {
        src_hdd: src_kind == DriveKind::Hdd,
        dst_hdd: dst_kind == DriveKind::Hdd,
    };
    // Scanning is always per-endpoint and therefore always parallel; only the
    // copy phase is constrained. Say which, so the log reflects reality.
    let mode_label = if profile.serial_copies() {
        "parallel scan, serial copies"
    } else {
        "parallel I/O"
    };
    let msg = format!(
        "Drive detection: SRC={}, DST={} \u{2192} {mode_label}",
        src_kind.label(),
        dst_kind.label(),
    );
    (profile, msg)
}

// ── Windows: TRIM-support query via DeviceIoControl ──────────────────────────
//
// TRIM support is the correct discriminator: SSDs implement TRIM, HDDs don't.
// The seek-penalty IOCTL that sysinfo uses is unreliable on Windows: USB drives
// and virtual/encrypted volumes (VeraCrypt) answer it incorrectly regardless of
// actual media type.

#[cfg(windows)]
fn detect(path: &Path) -> DriveKind {
    match drive_letter(path).and_then(trim_support) {
        Some(true) => DriveKind::Ssd,
        _ => DriveKind::Hdd,
    }
}

#[cfg(windows)]
fn drive_letter(path: &Path) -> Option<char> {
    let s = path.to_string_lossy();
    let mut chars = s.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

#[cfg(windows)]
use std::mem;
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                DEVICE_TRIM_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
                STORAGE_PROPERTY_QUERY, StorageDeviceTrimProperty,
            },
        },
    },
    core::PCWSTR,
};

#[cfg(windows)]
fn open_volume(drive: char) -> windows::core::Result<HANDLE> {
    let path = format!("\\\\.\\{}:", drive);
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
}

#[cfg(windows)]
unsafe fn query_property<T>(
    handle: HANDLE,
    property_id: windows::Win32::System::Ioctl::STORAGE_PROPERTY_ID,
) -> windows::core::Result<T> {
    unsafe {
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: property_id,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0u8; 1],
        };
        let mut output: T = mem::zeroed();
        let mut returned = 0u32;
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const std::ffi::c_void),
            mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut output as *mut _ as *mut std::ffi::c_void),
            mem::size_of::<T>() as u32,
            Some(&mut returned),
            None,
        )?;
        Ok(output)
    }
}

/// Returns `Some(true)` = SSD, `Some(false)` = no TRIM (likely HDD), `None` = query failed.
#[cfg(windows)]
fn trim_support(drive: char) -> Option<bool> {
    let handle = open_volume(drive).ok()?;
    let result = unsafe {
        let r = query_property::<DEVICE_TRIM_DESCRIPTOR>(handle, StorageDeviceTrimProperty);
        let _ = CloseHandle(handle);
        r
    };
    Some(result.ok()?.TrimEnabled)
}

// ── Non-Windows: sysinfo rotational-flag detection ───────────────────────────
//
// On Linux sysinfo reads /sys/block/*/queue/rotational which is accurate.
// On macOS it uses IOKit. No heuristic overrides needed on either platform.

#[cfg(not(windows))]
fn detect(path: &Path) -> DriveKind {
    use sysinfo::{DiskKind, Disks};
    let disks = Disks::new_with_refreshed_list();
    let best = disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().components().count());
    match best {
        None => DriveKind::Unknown,
        Some(disk) => match disk.kind() {
            DiskKind::HDD => DriveKind::Hdd,
            DiskKind::SSD => DriveKind::Ssd,
            _ => DriveKind::Unknown,
        },
    }
}
