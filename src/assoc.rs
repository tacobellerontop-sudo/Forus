//! File-type registration so Forus can be chosen as the default photo/video
//! app in Windows Settings.
//!
//! Windows 10/11 don't let apps make themselves the default (the UserChoice
//! key is hash-protected), so Forus does what Windows asks of apps: it
//! registers its capabilities per-user (no admin rights needed) and then opens
//! Settings → Default apps → Forus, where one click on "Set default" makes it
//! the default for every type it registered.

use std::path::PathBuf;

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND, WIN32_ERROR};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{SHChangeNotify, ShellExecuteW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::files;

/// Name under `RegisteredApplications`; also used in the Settings deep link.
const APP_NAME: &str = "Forus";
const PROGID_IMAGE: &str = "Forus.Image";
const PROGID_VIDEO: &str = "Forus.Video";
const CAPABILITIES: &str = r"Software\Forus\Capabilities";

struct Paths {
    exe: String,
    exe_name: String,
}

fn paths() -> Option<Paths> {
    let exe: PathBuf = std::env::current_exe().ok()?;
    Some(Paths {
        exe_name: exe.file_name()?.to_string_lossy().into_owned(),
        exe: exe.to_string_lossy().into_owned(),
    })
}

fn open_command(exe: &str) -> String {
    format!("\"{exe}\" \"%1\"")
}

fn check(err: WIN32_ERROR) -> Result<()> {
    err.ok()
}

fn create_key(path: &str) -> Result<HKEY> {
    let mut key = HKEY::default();
    unsafe {
        check(RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        ))?;
    }
    Ok(key)
}

/// Writes a string value; `name` of `None` sets the key's default value.
fn set_string(path: &str, name: Option<&str>, value: &str) -> Result<()> {
    let key = create_key(path)?;
    let data: Vec<u8> = value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let name = name.map(HSTRING::from);
    let result = unsafe {
        check(RegSetValueExW(
            key,
            name.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null()),
            None,
            REG_SZ,
            Some(&data),
        ))
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// Writes an empty REG_NONE value (the form `OpenWithProgids` expects).
fn set_marker(path: &str, name: &str) -> Result<()> {
    let key = create_key(path)?;
    let result = unsafe { check(RegSetValueExW(key, &HSTRING::from(name), None, REG_NONE, None)) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn read_string(path: &str, name: Option<&str>) -> Option<String> {
    let name = name.map(HSTRING::from);
    let name_ptr = name.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null());
    let mut len = 0u32;
    unsafe {
        let path = HSTRING::from(path);
        check(RegGetValueW(HKEY_CURRENT_USER, &path, name_ptr, RRF_RT_REG_SZ, None, None, Some(&mut len))).ok()?;
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        check(RegGetValueW(
            HKEY_CURRENT_USER,
            &path,
            name_ptr,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut len),
        ))
        .ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

fn delete_tree(path: &str) {
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(path));
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(path));
    }
}

fn delete_value(path: &str, name: &str) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, KEY_SET_VALUE, &mut key).is_ok() {
            let r = RegDeleteValueW(key, &HSTRING::from(name));
            debug_assert!(r.is_ok() || r == ERROR_FILE_NOT_FOUND);
            let _ = RegCloseKey(key);
        }
    }
}

fn types() -> Vec<(String, &'static str)> {
    let images = files::image_extensions().iter().map(|e| (format!(".{e}"), PROGID_IMAGE));
    let videos = files::VIDEO_EXTENSIONS.iter().map(|e| (format!(".{e}"), PROGID_VIDEO));
    images.chain(videos).collect()
}

/// Whether this exe's registration is present and points at this exe.
pub fn is_registered() -> bool {
    let Some(p) = paths() else { return false };
    read_string(&format!(r"Software\Classes\{PROGID_IMAGE}\shell\open\command"), None).as_deref()
        == Some(open_command(&p.exe).as_str())
        && read_string(r"Software\RegisteredApplications", Some(APP_NAME)).is_some()
}

/// Registers Forus for every supported image and video type (per user).
pub fn register() -> Result<()> {
    let p = paths().ok_or_else(windows::core::Error::empty)?;
    let command = open_command(&p.exe);
    let icon = format!("\"{}\",0", p.exe);

    for (progid, description) in [(PROGID_IMAGE, "Image (Forus)"), (PROGID_VIDEO, "Video (Forus)")] {
        let base = format!(r"Software\Classes\{progid}");
        set_string(&base, None, description)?;
        set_string(&base, Some("FriendlyTypeName"), description)?;
        set_string(&format!(r"{base}\DefaultIcon"), None, &icon)?;
        set_string(&format!(r"{base}\shell\open"), Some("FriendlyAppName"), "Forus")?;
        set_string(&format!(r"{base}\shell\open\command"), None, &command)?;
    }

    // Lets "Open with" find the exe by name and know what it handles.
    let app = format!(r"Software\Classes\Applications\{}", p.exe_name);
    set_string(&app, Some("FriendlyAppName"), "Forus")?;
    set_string(&format!(r"{app}\DefaultIcon"), None, &icon)?;
    set_string(&format!(r"{app}\shell\open\command"), None, &command)?;

    // Capabilities: what Settings → Default apps lists for Forus.
    set_string(CAPABILITIES, Some("ApplicationName"), "Forus")?;
    set_string(
        CAPABILITIES,
        Some("ApplicationDescription"),
        "A fast, GPU-rendered photo and video viewer",
    )?;
    set_string(CAPABILITIES, Some("ApplicationIcon"), &icon)?;

    for (ext, progid) in types() {
        set_marker(&format!(r"Software\Classes\{ext}\OpenWithProgids"), progid)?;
        set_marker(&format!(r"{app}\SupportedTypes"), &ext)?;
        set_string(&format!(r"{CAPABILITIES}\FileAssociations"), Some(&ext), progid)?;
    }
    set_string(r"Software\RegisteredApplications", Some(APP_NAME), CAPABILITIES)?;

    notify_shell();
    Ok(())
}

/// Removes everything [`register`] wrote.
pub fn unregister() {
    for (ext, progid) in types() {
        delete_value(&format!(r"Software\Classes\{ext}\OpenWithProgids"), progid);
    }
    delete_tree(&format!(r"Software\Classes\{PROGID_IMAGE}"));
    delete_tree(&format!(r"Software\Classes\{PROGID_VIDEO}"));
    if let Some(p) = paths() {
        delete_tree(&format!(r"Software\Classes\Applications\{}", p.exe_name));
    }
    delete_tree(r"Software\Forus");
    delete_value(r"Software\RegisteredApplications", APP_NAME);
    notify_shell();
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Opens Settings on Forus's page in Default apps (Windows 11), or the Default
/// apps page itself on Windows 10.
pub fn open_default_apps_settings(hwnd: Option<HWND>) {
    let uri = HSTRING::from(format!("ms-settings:defaultapps?registeredAppUser={APP_NAME}"));
    unsafe {
        ShellExecuteW(hwnd, w!("open"), &uri, None, None, SW_SHOWNORMAL);
    }
}
