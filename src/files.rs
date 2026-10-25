//! File type detection and folder navigation.

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::StrCmpLogicalW;

/// Formats decoded by WIC. HEIC/WebP/AVIF/JXL rely on the codec extensions
/// that ship with (or are installable from the Store on) Windows 11.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "bmp", "dib", "gif", "tif", "tiff", "ico", "webp",
    "heic", "heif", "avif", "jxl", "jxr", "wdp", "dds", "dng", "cr2", "cr3", "nef", "arw",
    "orf", "rw2",
];

/// Containers played through the Media Foundation Media Engine.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "3gp", "3g2", "ts", "m2ts", "mts",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Image,
    Video,
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = extension(path)?;
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else {
        None
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Sorts paths the way Explorer does ("img2" before "img10").
fn sort_logical(paths: &mut [PathBuf]) {
    let mut keyed: Vec<(Vec<u16>, PathBuf)> = paths
        .iter()
        .map(|p| (wide(p.file_name().unwrap_or_default()), p.clone()))
        .collect();
    keyed.sort_by(|a, b| {
        let r = unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) };
        r.cmp(&0)
    });
    for (slot, (_, p)) in paths.iter_mut().zip(keyed) {
        *slot = p;
    }
}

/// Lists the supported media in `dir`, sorted logically.
pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|e| e.path())
                .filter(|p| kind_of(p).is_some())
                .collect()
        })
        .unwrap_or_default();
    sort_logical(&mut files);
    files
}

/// Builds the navigation list for an opened path. A file opens its folder
/// positioned at that file; a folder opens at its first item.
pub fn playlist_for(path: &Path) -> (Vec<PathBuf>, usize) {
    if path.is_dir() {
        return (list_dir(path), 0);
    }
    let mut files = path.parent().map(list_dir).unwrap_or_default();
    let index = match files.iter().position(|p| same_path(p, path)) {
        Some(i) => i,
        None => {
            // Unknown extension or unreadable folder: still try to open it.
            files.insert(0, path.to_path_buf());
            0
        }
    };
    (files, index)
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// File-dialog filter spec, e.g. "*.jpg;*.png;...".
pub fn filter_spec(exts: &[&str]) -> String {
    exts.iter()
        .map(|e| format!("*.{e}"))
        .collect::<Vec<_>>()
        .join(";")
}
