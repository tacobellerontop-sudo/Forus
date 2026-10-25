//! Smaller format decoders: animated WebP/APNG, camera RAW previews, PCX,
//! macOS icons, and ZIP-based art files (Krita, OpenRaster, Procreate,
//! Sketch) that carry a flattened PNG of the artwork.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Duration;

use image::{AnimationDecoder, ImageFormat, ImageReader, RgbaImage};

use crate::decode::{AnimationFrame, DecodedImage, MAX_ANIMATION_BYTES};
use crate::fallback;

// ---- Animated WebP / APNG ----------------------------------------------------

/// Decodes all frames of an animated WebP or PNG. Returns `None` for still
/// images (and anything unusual) so the regular decoders handle them.
pub fn animated(path: &Path, ext: &str, max_dimension: u32) -> Option<Result<DecodedImage, String>> {
    let reader = BufReader::new(File::open(path).ok()?);
    let frames: image::Frames = match ext {
        "webp" => {
            let decoder = image::codecs::webp::WebPDecoder::new(reader).ok()?;
            if !decoder.has_animation() {
                return None;
            }
            decoder.into_frames()
        }
        "png" | "apng" => {
            let decoder = image::codecs::png::PngDecoder::new(reader).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            decoder.apng().ok()?.into_frames()
        }
        _ => return None,
    };

    let mut out: Vec<AnimationFrame> = Vec::new();
    let mut size = (0, 0);
    for frame in frames {
        let frame = match frame {
            Ok(f) => f,
            Err(e) if out.is_empty() => return Some(Err(e.to_string())),
            Err(_) => break, // play the frames we managed to read
        };
        let (num, den) = frame.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(0);
        let delay = if ms < 20 { Duration::from_millis(100) } else { Duration::from_millis(ms as u64) };
        let buffer = frame.into_buffer();
        size = buffer.dimensions();
        if size.0 > max_dimension || size.1 > max_dimension {
            return None;
        }
        if (out.len() + 1) * size.0 as usize * size.1 as usize * 4 > MAX_ANIMATION_BYTES {
            break;
        }
        out.push(AnimationFrame { pixels: fallback::premultiplied_bgra(buffer), delay });
    }
    if out.len() < 2 {
        return None;
    }
    let has_alpha = out.iter().any(|f| f.pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 255));
    let first = out.remove(0);
    Some(Ok(DecodedImage {
        width: size.0,
        height: size.1,
        pixels: first.pixels,
        source_size: size,
        orientation: 1,
        has_alpha,
        animation: out,
        first_delay: first.delay,
        scalable: false,
    }))
}

// ---- Camera RAW --------------------------------------------------------------

/// Every RAW format embeds a full-size (or near) JPEG preview for cameras'
/// own screens. Without Microsoft's Raw Image Extension we show the largest
/// one, rotated by the RAW's own orientation tag.
pub fn raw_preview(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;

    // Candidate JPEG starts (SOI followed by another marker), largest first.
    let mut candidates: Vec<(u64, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() && candidates.len() < 64 {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            let reader = ImageReader::with_format(std::io::Cursor::new(&data[i..]), ImageFormat::Jpeg);
            if let Ok((w, h)) = reader.into_dimensions() {
                if w >= 160 && h >= 120 {
                    candidates.push((w as u64 * h as u64, i));
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));

    // The biggest "JPEG" may be the lossless-compressed sensor data, which
    // isn't a viewable image; fall through to the next one.
    for (_, start) in candidates {
        if let Ok(img) = image::load_from_memory_with_format(&data[start..], ImageFormat::Jpeg) {
            let mut decoded = fallback::from_rgba(img.into_rgba8(), max_dimension);
            decoded.orientation = tiff_orientation(&data).unwrap_or(1);
            return Ok(decoded);
        }
    }
    Err("This RAW file has no embedded preview. Install the Raw Image Extension from the Microsoft Store \
         to open it."
        .into())
}

/// Reads the Orientation tag (274) from a TIFF-structured RAW's first IFD.
fn tiff_orientation(data: &[u8]) -> Option<u16> {
    let little = match data.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b: [u8; 2] = data.get(o..o + 2)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(o..o + 4)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count.min(512)).find_map(|n| {
        let entry = ifd + 2 + n * 12;
        (u16_at(entry)? == 274).then(|| u16_at(entry + 8)).flatten()
    })
    .filter(|o| (1..=8).contains(o))
}

// ---- PCX ---------------------------------------------------------------------

pub fn pcx(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 128 || data[0] != 0x0A {
        return Err("Not a PCX file".into());
    }
    let le16 = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]) as usize;
    let bpp = data[3] as usize;
    let (w, h) = (le16(8).wrapping_sub(le16(4)) + 1, le16(10).wrapping_sub(le16(6)) + 1);
    let planes = data[65] as usize;
    let line = le16(66);
    if w == 0 || h == 0 || w > 65535 || h > 65535 || !matches!(bpp, 1 | 2 | 4 | 8) || !(1..=4).contains(&planes) {
        return Err("Unsupported PCX variant".into());
    }
    if line * 8 < w * bpp {
        return Err("Corrupt PCX header".into());
    }

    // Run-length decode every scanline (all planes of a line are contiguous).
    let total = line * planes * h;
    let mut raw = Vec::with_capacity(total);
    let mut pos = 128;
    while raw.len() < total && pos < data.len() {
        let b = data[pos];
        pos += 1;
        if data[2] == 1 && b & 0xC0 == 0xC0 {
            let v = *data.get(pos).ok_or("Truncated PCX data")?;
            pos += 1;
            raw.extend(std::iter::repeat_n(v, (b & 0x3F) as usize));
        } else {
            raw.push(b);
        }
    }
    raw.resize(total, 0);

    let header_palette = &data[16..64];
    let vga_palette = (bpp == 8 && planes == 1 && data.len() >= 769 && data[data.len() - 769] == 0x0C)
        .then(|| &data[data.len() - 768..]);

    let mut out = RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        let row = &raw[y * line * planes..(y + 1) * line * planes];
        for x in 0..w {
            let px = if bpp == 8 && planes >= 3 {
                let a = if planes == 4 { row[3 * line + x] } else { 255 };
                [row[x], row[line + x], row[2 * line + x], a]
            } else {
                // Palette index assembled from each plane's bits.
                let mut index = 0usize;
                for p in 0..planes {
                    let bit = x * bpp;
                    let byte = row[p * line + bit / 8];
                    let v = (byte >> (8 - bpp - bit % 8)) & ((1u16 << bpp) - 1) as u8;
                    index |= (v as usize) << (p * bpp);
                }
                match (vga_palette, bpp, planes) {
                    (Some(pal), ..) => [pal[index * 3], pal[index * 3 + 1], pal[index * 3 + 2], 255],
                    (None, 8, 1) => [index as u8, index as u8, index as u8, 255],
                    (None, 1, 1) => {
                        let v = if index == 0 { 0 } else { 255 };
                        [v, v, v, 255]
                    }
                    _ => {
                        let i = (index % 16) * 3;
                        [header_palette[i], header_palette[i + 1], header_palette[i + 2], 255]
                    }
                }
            };
            out.put_pixel(x as u32, y as u32, image::Rgba(px));
        }
    }
    Ok(fallback::from_rgba(out, max_dimension))
}

// ---- macOS icons ---------------------------------------------------------------

/// Shows the largest PNG-encoded image inside an .icns file (all icons made
/// since OS X 10.7 store their big sizes as PNG).
pub fn icns(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.get(0..4) != Some(b"icns") {
        return Err("Not a macOS icon file".into());
    }
    let mut best: Option<(u32, &[u8])> = None;
    let mut pos = 8;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if len < 8 || pos + len > data.len() {
            break;
        }
        let body = &data[pos + 8..pos + len];
        if body.starts_with(b"\x89PNG\r\n\x1a\n") && body.len() > 24 {
            let width = u32::from_be_bytes(body[16..20].try_into().unwrap());
            if best.is_none_or(|(w, _)| width > w) {
                best = Some((width, body));
            }
        }
        pos += len;
    }
    let (_, png) = best.ok_or("This icon only uses legacy image formats, which aren't supported")?;
    let img = image::load_from_memory_with_format(png, ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(fallback::from_rgba(img.into_rgba8(), max_dimension))
}

// ---- ZIP-based art files -------------------------------------------------------

/// Krita, OpenRaster, Procreate and Sketch documents are ZIP archives that
/// include a flattened render of the artwork; show that.
pub fn zip_preview(path: &Path, ext: &str, max_dimension: u32) -> Result<DecodedImage, String> {
    let wanted: &[&str] = match ext {
        "kra" | "ora" => &["mergedimage.png", "Thumbnails/thumbnail.png", "preview.png"],
        "procreate" => &["QuickLook/Thumbnail.png"],
        "sketch" => &["previews/preview.png"],
        _ => &[],
    };
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let entries = zip_entries(&data).ok_or("This file isn't a valid document archive")?;
    for name in wanted {
        if let Some(entry) = entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)) {
            let png = zip_read(&data, entry).ok_or("Couldn't read the document's preview")?;
            let img = image::load_from_memory(&png).map_err(|e| e.to_string())?;
            return Ok(fallback::from_rgba(img.into_rgba8(), max_dimension));
        }
    }
    Err("This document doesn't contain a flattened preview (save it with \"maximize compatibility\")".into())
}

struct ZipEntry {
    name: String,
    method: u16,
    compressed: usize,
    local_header: usize,
}

fn zip_entries(data: &[u8]) -> Option<Vec<ZipEntry>> {
    let le16 = |o: usize| -> Option<usize> { Some(u16::from_le_bytes(data.get(o..o + 2)?.try_into().ok()?) as usize) };
    let le32 = |o: usize| -> Option<usize> { Some(u32::from_le_bytes(data.get(o..o + 4)?.try_into().ok()?) as usize) };
    // End of central directory: within the last 64 KiB (comment max) + 22.
    let search_from = data.len().saturating_sub(65_557);
    let eocd = (search_from..data.len().saturating_sub(21)).rev().find(|&i| data[i..i + 4] == *b"PK\x05\x06")?;
    let count = le16(eocd + 10)?;
    let mut pos = le32(eocd + 16)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if data.get(pos..pos + 4)? != b"PK\x01\x02" {
            return None;
        }
        let name_len = le16(pos + 28)?;
        let skip = name_len + le16(pos + 30)? + le16(pos + 32)?;
        entries.push(ZipEntry {
            method: le16(pos + 10)? as u16,
            compressed: le32(pos + 20)?,
            local_header: le32(pos + 42)?,
            name: String::from_utf8_lossy(data.get(pos + 46..pos + 46 + name_len)?).into_owned(),
        });
        pos += 46 + skip;
    }
    Some(entries)
}

fn zip_read(data: &[u8], entry: &ZipEntry) -> Option<Vec<u8>> {
    let h = entry.local_header;
    if data.get(h..h + 4)? != b"PK\x03\x04" {
        return None;
    }
    let name_len = u16::from_le_bytes(data.get(h + 26..h + 28)?.try_into().ok()?) as usize;
    let extra_len = u16::from_le_bytes(data.get(h + 28..h + 30)?.try_into().ok()?) as usize;
    let start = h + 30 + name_len + extra_len;
    let body = data.get(start..start + entry.compressed)?;
    match entry.method {
        0 => Some(body.to_vec()),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(body).read_to_end(&mut out).ok()?;
            Some(out)
        }
        _ => None,
    }
}
