use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Contents of `.kobo/version`: a comma-separated line such as
/// `N418170000001,4.1.15,4.38.21908,4.1.15,4.1.15,00000000-0000-0000-0000-000000000390`
/// (serial, kernel, firmware, …, model id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub serial: String,
    pub firmware: Option<String>,
    pub model_id: Option<String>,
}

impl VersionInfo {
    pub fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.trim().split(',').map(str::trim).collect();
        let serial = fields.first().filter(|s| !s.is_empty())?.to_string();
        let field = |s: Option<&&str>| s.filter(|s| !s.is_empty()).map(|s| s.to_string());
        Some(Self {
            serial,
            firmware: field(fields.get(2)),
            model_id: if fields.len() >= 6 { field(fields.last()) } else { None },
        })
    }
}

/// A mounted Kobo.
#[derive(Debug, Clone)]
pub struct KoboDevice {
    pub root: PathBuf,
    pub version: Option<VersionInfo>,
}

impl KoboDevice {
    /// Treats `root` as a Kobo mount point if it holds `.kobo/KoboReader.sqlite`.
    pub fn at(root: &Path) -> Result<Self> {
        if !root.join(".kobo").join("KoboReader.sqlite").is_file() {
            return Err(Error::NotAKobo(root.to_path_buf()));
        }
        let version = fs::read_to_string(root.join(".kobo").join("version"))
            .ok()
            .and_then(|s| VersionInfo::parse(&s));
        Ok(Self { root: root.to_path_buf(), version })
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join(".kobo").join("KoboReader.sqlite")
    }

    /// The annotation `source` tag for this device, e.g. `kobo:N418170000001`.
    pub fn source_tag(&self) -> String {
        match &self.version {
            Some(v) => format!("kobo:{}", v.serial),
            None => "kobo:unknown".to_string(),
        }
    }
}

/// Filesystems a Kobo shows up as over USB. Restricting the scan to these
/// avoids stat-ing network mounts, which can hang.
const REMOVABLE_FS: &[&str] = &["vfat", "msdos", "exfat", "fuseblk"];

/// Finds mounted Kobos by scanning `/proc/self/mounts`.
///
/// M0 stand-in for the udisks2 D-Bus watcher described in SPEC.md.
pub fn detect() -> Vec<KoboDevice> {
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    candidate_roots(&mounts)
        .into_iter()
        .filter(|root| seen.insert(root.clone()))
        .filter_map(|root| KoboDevice::at(&root).ok())
        .collect()
}

fn candidate_roots(mounts: &str) -> Vec<PathBuf> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let mount_point = fields.next()?;
            let fs_type = fields.next()?;
            REMOVABLE_FS
                .contains(&fs_type)
                .then(|| PathBuf::from(unescape_mount(mount_point)))
        })
        .collect()
}

/// `/proc/mounts` escapes space, tab, newline and backslash as octal (`\040`).
fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let octal = &bytes[i + 1..i + 4];
            if octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = octal.iter().fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                if let Ok(value) = u8::try_from(value) {
                    out.push(value);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_line() {
        let v = VersionInfo::parse(
            "N418170000001,4.1.15,4.38.21908,4.1.15,4.1.15,00000000-0000-0000-0000-000000000390\n",
        )
        .unwrap();
        assert_eq!(v.serial, "N418170000001");
        assert_eq!(v.firmware.as_deref(), Some("4.38.21908"));
        assert_eq!(v.model_id.as_deref(), Some("00000000-0000-0000-0000-000000000390"));
        assert!(VersionInfo::parse("").is_none());
        assert_eq!(VersionInfo::parse("N1").unwrap().firmware, None);
    }

    #[test]
    fn finds_removable_mounts_and_unescapes_paths() {
        let mounts = "\
proc /proc proc rw 0 0
/dev/sda1 /run/media/me/KOBOeReader vfat rw,nosuid 0 0
/dev/sdb1 /run/media/me/My\\040Kobo exfat rw 0 0
server:/x /mnt/nfs nfs4 rw 0 0
";
        assert_eq!(
            candidate_roots(mounts),
            vec![
                PathBuf::from("/run/media/me/KOBOeReader"),
                PathBuf::from("/run/media/me/My Kobo"),
            ]
        );
    }
}
