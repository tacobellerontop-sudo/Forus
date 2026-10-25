# Forus

A fast, GPU-rendered photo and video viewer for Windows 11, written in Rust on
top of native Windows APIs: no runtime, no Electron, a single ~450 KB `.exe`.

## How it renders

- **Direct3D 11 + Direct2D:** every frame is drawn on the GPU into a
  flip-model DXGI swap chain with vsync. Zooming, panning and rotating are
  transforms applied when the bitmap is drawn, so they're instant at any size.
- **Photos:** decoded with the Windows Imaging Component on a background
  thread, uploaded once to a GPU bitmap. Neighbouring files are prefetched, so
  stepping through a folder is immediate. Filtering is high-quality cubic when
  downscaling and switches to nearest-neighbour at ≥ 400% so you can inspect
  pixels.
- **Videos:** played through the Media Foundation *Media Engine* on the same
  D3D11 device. Decoding is hardware-accelerated (DXVA), and frames go straight
  into a GPU texture that Direct2D draws, so video never round-trips through the
  CPU.
- **Windows 11 look:** dark title bar with Mica, rounded corners, per-monitor
  DPI awareness and Segoe Fluent Icons.

## Supported formats

| Photos | Videos |
| --- | --- |
| JPEG, PNG, GIF (first frame), BMP, TIFF, ICO, WebP, JPEG XR, DDS | MP4, MOV, M4V, MKV, AVI, WMV, WebM, MPEG-TS, 3GP |
| HEIC/HEIF, AVIF, JPEG XL, camera RAW: need the matching codec from the Microsoft Store | HEVC, VP9 and AV1 need their video extension from the Microsoft Store |

EXIF orientation is applied automatically. Transparent images are drawn over a
checkerboard.

## Using it

Open a file with **Ctrl+O**, drag it onto the window, or pass it on the command
line (`forus.exe photo.jpg`). To make it the default viewer, use *Open with →
Choose another app* in File Explorer. Forus then browses every photo and video
in that folder.

| Keys | Action |
| --- | --- |
| ← → / PgUp PgDn / mouse back & forward | Previous / next file |
| Home / End | First / last file |
| Mouse wheel, + −, ↑ ↓ | Zoom around the cursor |
| Drag | Pan when zoomed in |
| 0 or F | Fit to window |
| 1 | Actual size (100%) |
| R / Shift+R | Rotate clockwise / counter-clockwise |
| F11, Enter, double-click | Full screen |
| Space | Play / pause (next photo when viewing photos) |
| Shift+← → (Ctrl+Shift for 30 s) | Seek 5 s |
| ↑ ↓ while a video plays | Volume |
| M / L | Mute / toggle looping |
| I | Keep the info bar visible |
| E | Show the file in File Explorer |
| F1 or H | Show all shortcuts |
| Esc | Leave full screen, or quit |

The video bar at the bottom lets you click or drag to seek; scroll over the
speaker icon to change volume.

## Building

You need [Rust](https://rustup.rs) on Windows 10/11 with the MSVC toolchain:

```powershell
cargo build --release
.\target\release\forus.exe
```

Every push also builds `forus.exe` on GitHub Actions; download it from the
workflow run's artifacts.

Cross-compiling from Linux works with MinGW:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
```

## Code layout

| File | Responsibility |
| --- | --- |
| `src/app.rs` | Window, message loop, input, view math and UI drawing |
| `src/gfx.rs` | D3D11 device, swap chain, Direct2D/DirectWrite helpers |
| `src/decode.rs` | Background WIC decoding with EXIF orientation |
| `src/video.rs` | Media Engine playback into a shared D3D11 texture |
| `src/files.rs` | Supported formats and Explorer-order folder listing |
