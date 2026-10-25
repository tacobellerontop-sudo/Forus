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
| JPEG, PNG, animated GIF, BMP, TIFF, ICO/CUR, WebP, JPEG XR, DDS (Windows codecs) | MP4, MOV, M4V, MKV, AVI, WMV, WebM, MPEG-TS, 3GP |
| SVG/SVGZ, Photoshop PSD/PSB, JPEG XL, OpenEXR, Radiance HDR, TGA, QOI, PBM/PGM/PPM/PAM, farbfeld (built-in decoders) | |
| HEIC/HEIF, AVIF, camera RAW: need the matching codec from the Microsoft Store | HEVC, VP9 and AV1 need their video extension from the Microsoft Store |

Forus also picks up any other image codec installed on the PC (Windows asks
each installed codec which extensions it handles), so third-party codecs work
automatically. If Windows can't decode a file, the built-in decoders get a
second try, and a damaged file shows an error instead of crashing the viewer.

Animated GIFs play with their own frame timing, offsets and disposal modes,
matching how browsers show them. EXIF orientation is applied automatically. Transparent images are drawn over a
checkerboard.

## Using it

Open a file with **Ctrl+O**, drag it onto the window, or pass it on the command
line (`forus.exe photo.jpg`). Forus then browses every photo and video in that
folder.

### Making Forus your default viewer

Forus registers itself for every format above when it first runs (per user, no
admin rights needed), so it appears in File Explorer's *Open with* menu. To
make it the default, press **D** in Forus, or run `forus.exe --set-default`.
Settings opens on Forus's page under *Default apps*; click **Set default**.
Windows doesn't allow apps to make themselves the default, so this click has
to be yours.

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
| Space | Play / pause videos and GIFs (next photo otherwise) |
| , / . | Step to the previous / next GIF frame |
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
| `src/decode.rs` | Background WIC decoding with EXIF orientation and GIF animation |
| `src/fallback.rs` | Built-in decoders: SVG (resvg), JPEG XL, EXR/HDR, TGA, QOI, PNM... |
| `src/psd.rs` | Reads the flattened composite image from PSD/PSB files |
| `src/assoc.rs` | File-type registration for *Open with* and *Default apps* |
| `src/video.rs` | Media Engine playback into a shared D3D11 texture |
| `src/files.rs` | Supported formats (incl. installed codecs) and Explorer-order folder listing |
| `assets/` | App icon (SVG sources and `.ico`) and Windows manifest |
