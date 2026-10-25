# Forus

A fast, GPU-rendered photo and video viewer for Windows 11, written in Rust on
top of native Windows APIs: no runtime, no Electron, one self-contained `.exe`.

## Installing

Download `Forus-<version>-setup.exe` from the latest GitHub Actions run's
artifacts and run it. It installs for your account only (no administrator
prompt) into `%LOCALAPPDATA%\Programs\Forus`, adds Forus to the Start menu and
to *Apps & features*, and registers it for every supported photo and video
type. Its last page offers to make Forus your default viewer.

You can also just run `forus.exe` directly, without installing.

## How it renders

- **Direct3D 11 + Direct2D:** every frame is drawn on the GPU into a
  flip-model DXGI swap chain with vsync. Zooming, panning and rotating are
  transforms applied when the bitmap is drawn, so they're instant at any size.
- **Photos:** decoded on a background thread, uploaded once to a GPU bitmap.
  Neighbouring files are prefetched, so stepping through a folder is immediate.
  Filtering is high-quality cubic when downscaling and switches to
  nearest-neighbour at ≥ 400% so you can inspect pixels.
- **Videos:** played through the Media Foundation *Media Engine* on the same
  D3D11 device. Decoding is hardware-accelerated (DXVA), and frames go straight
  into a GPU texture that Direct2D draws, so video never round-trips through the
  CPU.
- **Windows 11 look:** a custom title bar drawn on the GPU, with the file name,
  quick tools (open, previous/next, rotate, fit, full screen, help) and Windows
  11 caption buttons. Hovering maximize still shows Snap Layouts, and the
  native resize borders, shadow and rounded corners are kept.

## Supported formats

| Photos | Videos |
| --- | --- |
| JPEG, PNG, GIF, BMP, TIFF, ICO/CUR, WebP, JPEG XR, DDS (Windows codecs) | MP4, MOV, M4V, MKV, AVI, WMV, WebM, MPEG-TS, 3GP |
| Animated GIF, WebP and PNG (APNG) | |
| SVG/SVGZ, Photoshop PSD/PSB, JPEG XL, OpenEXR, Radiance HDR, TGA, QOI, PCX, PBM/PGM/PPM/PAM, farbfeld, macOS ICNS | |
| Krita (.kra), OpenRaster (.ora), Procreate and Sketch documents (their flattened preview) | |
| Camera RAW (CR2, CR3, NEF, ARW, RAF, ORF, RW2, DNG, PEF and more) | |
| HEIC/HEIF, AVIF: need the matching codec from the Microsoft Store | HEVC, VP9 and AV1 need their video extension from the Microsoft Store |

Camera RAW files are fully decoded when Microsoft's free *Raw Image Extension*
is installed; without it Forus shows the full-size JPEG preview every camera
embeds in its RAW files.

Forus also picks up any other image codec installed on the PC (Windows asks
each installed codec which extensions it handles), so third-party codecs work
automatically. If Windows can't decode a file, the built-in decoders get a
second try, and a damaged file shows an error instead of crashing the viewer.

Animations play with their own frame timing and disposal modes, matching how
browsers show them. EXIF orientation is applied automatically. Transparent
images are drawn over a checkerboard.

## Using it

Open a file with **Ctrl+O** or the title bar's open button, drag it onto the
window, or pass it on the command line (`forus.exe photo.jpg`). Forus then
browses every photo and video in that folder.

### Making Forus your default viewer

Forus registers itself for every format above when it's installed or first
runs (per user, no admin rights needed), so it appears in File Explorer's
*Open with* menu. To make it the default, tick the box at the end of setup,
press **D** in Forus, or run `forus.exe --set-default`. Settings opens on
Forus's page under *Default apps*; click **Set default**. Windows doesn't allow
apps to make themselves the default, so this click has to be yours.

| Command | Effect |
| --- | --- |
| `forus.exe --set-default` | Register and open Forus's Default apps page |
| `forus.exe --register` | Register file types silently (for scripts) |
| `forus.exe --unregister` | Remove all of Forus's file-type registrations |

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
| Space | Play / pause videos and animations (next photo otherwise) |
| , / . | Step to the previous / next animation frame |
| Shift+← → (Ctrl+Shift for 30 s) | Seek 5 s |
| ↑ ↓ while a video plays | Volume |
| M / L | Mute / toggle looping |
| I | Keep the info bar visible |
| E | Show the file in File Explorer |
| D | Make Forus your default viewer |
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

To build the installer too, install [NSIS](https://nsis.sourceforge.io) and run:

```powershell
makensis /DVERSION=0.1.0 installer\forus.nsi   # writes target\Forus-0.1.0-setup.exe
```

Every push builds both `forus.exe` and the installer on GitHub Actions;
download them from the workflow run's artifacts.

Cross-compiling from Linux works with MinGW (and `apt install nsis` for the
installer):

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
makensis -DVERSION=0.1.0 -DEXE=../target/x86_64-pc-windows-gnu/release/forus.exe installer/forus.nsi
```

## Code layout

| File | Responsibility |
| --- | --- |
| `src/app.rs` | Window, message loop, input, view math and UI drawing |
| `src/titlebar.rs` | Custom title bar: layout, hit-testing and drawing |
| `src/gfx.rs` | D3D11 device, swap chain, Direct2D/DirectWrite helpers |
| `src/decode.rs` | Background decoding: WIC first, with EXIF orientation and GIF animation |
| `src/fallback.rs` | Built-in decoders: SVG (resvg), JPEG XL, EXR/HDR, TGA, QOI, PNM... |
| `src/extra.rs` | Animated WebP/APNG, camera RAW previews, PCX, ICNS, Krita/OpenRaster/Procreate/Sketch |
| `src/psd.rs` | Reads the flattened composite image from PSD/PSB files |
| `src/assoc.rs` | File-type registration for *Open with* and *Default apps* |
| `src/video.rs` | Media Engine playback into a shared D3D11 texture |
| `src/files.rs` | Supported formats (incl. installed codecs) and Explorer-order folder listing |
| `installer/forus.nsi` | Per-user NSIS installer and uninstaller |
| `assets/` | App icon (SVG sources and `.ico`) and Windows manifest |
