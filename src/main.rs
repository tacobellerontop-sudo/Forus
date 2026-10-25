//! Forus — a GPU-rendered photo and video viewer for Windows 11.

#![windows_subsystem = "windows"]

#[cfg(not(windows))]
compile_error!("Forus is a Windows application; build it for a Windows target.");

mod app;
mod assoc;
mod decode;
mod extra;
mod fallback;
mod files;
mod gfx;
mod psd;
mod titlebar;
mod video;

use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Posted by the decoder thread when a decode finishes.
pub const WM_APP_DECODED: u32 = WM_APP + 1;
/// Posted by the Media Engine callback; LPARAM packs (generation, event).
pub const WM_APP_MEDIA: u32 = WM_APP + 2;

fn main() {
    if let Err(e) = app::run() {
        app::fatal(&format!("Forus couldn't start.\n\n{}", e.message()));
    }
}
