//! File type detection and folder navigation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::StrCmpLogicalW;

/// Image formats Forus always recognises: the ones Windows decodes out of the
/// box, those our own fallback decoders handle (SVG, PSD, TGA, QOI, PNM,
/// HDR/EXR, JPEG XL...), and common ones that need a Store codec (HEIC, AVIF,
/// camera RAW) so the user gets a helpful message instead of nothing.
const KNOWN_IMAGE_EXTENSIONS: &[&str] = &[
    // Windows built-in codecs
    "jpg", "jpeg", "jpe", "jfif", "exif", "png", "apng", "bmp", "dib", "rle", "gif", "tif",
    "tiff", "ico", "cur", "jxr", "wdp", "hdp", "dds", "webp",
    // Fallback decoders bundled with Forus
    "svg", "svgz", "psd", "psb", "tga", "icb", "vda", "vst", "qoi", "pbm", "pgm", "ppm", "pnm", "pam",
    "hdr", "pic", "exr", "ff", "jxl", "pcx", "icns", "kra", "ora", "procreate", "sketch", "emf", "emz",
    "wmf", "wmz",
    // Need a codec extension from the Microsoft Store
    "heic", "heif", "hif", "avif", "avifs",
];

static IMAGE_EXTENSIONS: OnceLock<Vec<String>> = OnceLock::new();

/// Every image extension we can open: the known list plus whatever decoders
/// are installed on this PC (third-party WIC codecs included). Sorted.
pub fn image_extensions() -> &'static [String] {
    IMAGE_EXTENSIONS.get_or_init(|| {
        let mut all: BTreeSet<String> = KNOWN_IMAGE_EXTENSIONS
            .iter()
            .chain(crate::fallback::RAW_EXTENSIONS)
            .map(|e| e.to_string())
            .collect();
        all.extend(installed_wic_extensions());
        all.retain(|e| !VIDEO_EXTENSIONS.contains(&e.as_str()));
        all.into_iter().collect()
    })
}

/// Asks WIC for every installed decoder's file extensions. Needs COM.
fn installed_wic_extensions() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) =
            CoCreateInstance::<_, IWICImagingFactory>(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
        else {
            return out;
        };
        let Ok(components) =
            factory.CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32)
        else {
            return out;
        };
        loop {
            let mut item = [None];
            let mut fetched = 0u32;
            let _ = components.Next(&mut item, Some(&mut fetched));
            let Some(unknown) = item[0].take().filter(|_| fetched == 1) else { break };
            let Ok(info) = unknown.cast::<IWICBitmapCodecInfo>() else { continue };
            let mut len = 0u32;
            let _ = info.GetFileExtensions(&mut [], &mut len);
            let mut buf = vec![0u16; len as usize];
            if len == 0 || info.GetFileExtensions(&mut buf, &mut len).is_err() {
                continue;
            }
            let list = String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]);
            out.extend(
                list.split([',', ';'])
                    .map(|e| e.trim().trim_start_matches('*').trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric())),
            );
        }
    }
    out
}

/// Containers played through the Media Foundation Media Engine.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "3gp", "3g2", "ts", "m2ts", "mts", "mpg",
    "mpeg",
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
    if image_extensions().binary_search(&ext).is_ok() {
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
pub fn filter_spec<S: AsRef<str>>(exts: &[S]) -> String {
    exts.iter()
        .map(|e| format!("*.{}", e.as_ref()))
        .collect::<Vec<_>>()
        .join(";")
}
