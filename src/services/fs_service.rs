//! Read-side filesystem services: known locations and drive enumeration.

use std::path::PathBuf;

use sysinfo::Disks;

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub name: String,
    pub mount: PathBuf,
    pub total: u64,
    pub available: u64,
    /// Reserved for a distinct removable-drive icon in a future pass.
    #[allow(dead_code)]
    pub removable: bool,
}

pub fn list_drives() -> Vec<DriveInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut drives: Vec<DriveInfo> = disks
        .iter()
        .map(|disk| {
            let mount = disk.mount_point().to_path_buf();
            let label = disk.name().to_string_lossy().trim().to_string();
            let letter = mount.to_string_lossy().trim_end_matches('\\').to_string();
            let name = if label.is_empty() {
                format!("Local Disk ({letter})")
            } else {
                format!("{label} ({letter})")
            };
            DriveInfo {
                name,
                mount,
                total: disk.total_space(),
                available: disk.available_space(),
                removable: disk.is_removable(),
            }
        })
        .collect();
    drives.sort_by(|a, b| a.mount.cmp(&b.mount));
    drives.dedup_by(|a, b| a.mount == b.mount);
    drives
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
}

#[derive(Clone, Debug)]
pub struct Place {
    pub kind: PlaceKind,
    pub name: &'static str,
    pub path: PathBuf,
}

pub fn known_places() -> Vec<Place> {
    let mut places = Vec::new();
    let mut push = |kind, name: &'static str, path: Option<PathBuf>| {
        if let Some(path) = path
            && path.exists() {
                places.push(Place { kind, name, path });
            }
    };
    push(PlaceKind::Home, "Home", dirs::home_dir());
    push(PlaceKind::Desktop, "Desktop", dirs::desktop_dir());
    push(PlaceKind::Documents, "Documents", dirs::document_dir());
    push(PlaceKind::Downloads, "Downloads", dirs::download_dir());
    push(PlaceKind::Pictures, "Pictures", dirs::picture_dir());
    push(PlaceKind::Music, "Music", dirs::audio_dir());
    push(PlaceKind::Videos, "Videos", dirs::video_dir());
    places
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
}
