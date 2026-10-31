//! Windows metafiles (EMF, WMF and their gzipped EMZ/WMZ forms), played by
//! GDI into an off-screen bitmap. They're vector art, so they're rendered
//! large and may be enlarged to fit the window like SVG.

use std::io::Read;
use std::path::Path;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{SetWinMetaFileBits, METAFILEPICT};

use crate::decode::DecodedImage;

/// Key at the start of an "Aldus placeable" WMF, which records its size.
const PLACEABLE_KEY: [u8; 4] = [0xD7, 0xCD, 0xC6, 0x9A];

pub fn decode(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let mut data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.starts_with(&[0x1F, 0x8B]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&data[..])
            .read_to_end(&mut out)
            .map_err(|_| "The compressed metafile is damaged")?;
        data = out;
    }
    let emf = Emf(load(&data).ok_or("Not a valid Windows metafile")?);

    let mut header = ENHMETAHEADER::default();
    let size = std::mem::size_of::<ENHMETAHEADER>() as u32;
    if unsafe { GetEnhMetaFileHeader(emf.0, size, Some(&mut header)) } == 0 {
        return Err("Not a valid Windows metafile".into());
    }
    // The frame is in 0.01 mm; show it at 96 dpi.
    let f = header.rclFrame;
    let (mut iw, mut ih) = ((f.right - f.left) as f32 * 96.0 / 2540.0, (f.bottom - f.top) as f32 * 96.0 / 2540.0);
    if !(iw >= 1.0 && ih >= 1.0) {
        let b = header.rclBounds;
        (iw, ih) = ((b.right - b.left + 1) as f32, (b.bottom - b.top + 1) as f32);
    }
    if !(iw >= 1.0 && ih >= 1.0) {
        return Err("The metafile is empty".into());
    }

    let longest = iw.max(ih);
    let target = longest.clamp(2048.0, 4096.0).min(max_dimension as f32);
    let mut scale = target / longest;
    const MAX_METAFILE_PIXELS: f32 = 24_000_000.0;
    let pixels = iw * ih * scale * scale;
    if pixels > MAX_METAFILE_PIXELS {
        scale *= (MAX_METAFILE_PIXELS / pixels).sqrt();
    }
    let (w, h) = (((iw * scale).round() as i32).max(1), ((ih * scale).round() as i32).max(1));
    let pixels = unsafe { play(&emf, w, h) }.ok_or("Windows couldn't draw the metafile")?;

    let mut img = crate::fallback::finish(w as u32, h as u32, pixels, ((iw.round() as u32).max(1), (ih.round() as u32).max(1)));
    img.scalable = true;
    Ok(img)
}

struct Emf(HENHMETAFILE);

impl Drop for Emf {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteEnhMetaFile(Some(self.0));
        }
    }
}

fn load(data: &[u8]) -> Option<HENHMETAFILE> {
    let emf = if data.get(40..44) == Some(b" EMF") {
        unsafe { SetEnhMetaFileBits(data) }
    } else if data.starts_with(&PLACEABLE_KEY) && data.len() > 22 {
        let le = |o: usize| i16::from_le_bytes([data[o], data[o + 1]]) as i32;
        let (left, top, right, bottom) = (le(6), le(8), le(10), le(12));
        let inch = u16::from_le_bytes([data[14], data[15]]).max(1) as i32;
        let pict = METAFILEPICT {
            mm: MM_ANISOTROPIC.0,
            // Extents in 0.01 mm.
            xExt: (right - left).abs() * 2540 / inch,
            yExt: (bottom - top).abs() * 2540 / inch,
            hMF: HMETAFILE::default(),
        };
        unsafe { SetWinMetaFileBits(&data[22..], None, Some(&pict)) }
    } else {
        unsafe { SetWinMetaFileBits(data, None, None) }
    };
    (!emf.is_invalid()).then_some(emf)
}

/// Plays the metafile onto white and returns opaque BGRA pixels.
unsafe fn play(emf: &Emf, w: i32, h: i32) -> Option<Vec<u8>> {
    let dc = CreateCompatibleDC(None);
    if dc.is_invalid() {
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let mut out = None;
    if let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
        let old = SelectObject(dc, bitmap.into());
        let len = w as usize * h as usize * 4;
        std::ptr::write_bytes(bits as *mut u8, 0xFF, len);
        SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
        if PlayEnhMetaFile(dc, emf.0, &RECT { left: 0, top: 0, right: w, bottom: h }).as_bool() {
            let _ = GdiFlush();
            let mut pixels = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
            // GDI leaves the alpha byte undefined.
            for p in pixels.as_chunks_mut::<4>().0 {
                p[3] = 255;
            }
            out = Some(pixels);
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
    }
    let _ = DeleteDC(dc);
    out
}
