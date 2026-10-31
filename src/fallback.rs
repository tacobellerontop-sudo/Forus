//! Decoders for formats Windows has no codec for (or whose codec isn't
//! installed): SVG, Photoshop, JPEG XL, camera RAW previews, PCX, macOS icons,
//! Krita/OpenRaster/Procreate/Sketch documents, and everything the `image`
//! crate reads (TGA, QOI, PNM, Radiance HDR, OpenEXR, farbfeld, ...). WIC
//! failures on common formats also get a second chance here.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use resvg::{tiny_skia, usvg};

use crate::decode::DecodedImage;

/// Formats we decode ourselves even when a WIC codec claims them, because
/// our decoder is more complete.
pub fn preferred(ext: &str) -> bool {
    matches!(ext, "svg" | "svgz" | "psd" | "psb" | "pcx" | "icns" | "kra" | "ora" | "procreate" | "sketch" | "emf" | "emz" | "wmf" | "wmz"
    )
}

/// Camera RAW formats: WIC decodes them fully when Microsoft's Raw Image
/// Extension is installed; otherwise we show the embedded preview.
pub const RAW_EXTENSIONS: &[&str] = &[
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef", "ptx",
    "srw", "x3f", "erf", "kdc", "dcr", "mrw", "mos", "mef", "3fr", "fff", "iiq", "rwl", "raw",
];

pub fn decode(path: &Path, ext: &str, max_dimension: u32) -> Result<DecodedImage, String> {
    match ext {
        "svg" | "svgz" => decode_svg(path, max_dimension),
        "psd" | "psb" => decode_psd(path, max_dimension),
        "jxl" => decode_jxl(path, max_dimension),
        "pcx" => crate::extra::pcx(path, max_dimension),
        "emf" | "emz" | "wmf" | "wmz" => crate::metafile::decode(path, max_dimension),
        "icns" => crate::extra::icns(path, max_dimension),
        "kra" | "ora" | "procreate" | "sketch" => crate::extra::zip_preview(path, ext, max_dimension),
        _ if RAW_EXTENSIONS.contains(&ext) => crate::extra::raw_preview(path, max_dimension),
        _ => decode_generic(path, max_dimension),
    }
}

// ---- SVG -------------------------------------------------------------------

fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            // Map CSS generic families to the first installed match.
            let pick = |db: &usvg::fontdb::Database, names: &[&str]| -> Option<String> {
                names
                    .iter()
                    .find(|n| db.faces().any(|f| f.families.iter().any(|(fam, _)| fam.eq_ignore_ascii_case(n))))
                    .map(|n| n.to_string())
            };
            if let Some(f) = pick(&db, &["Segoe UI", "Arial", "Tahoma", "Verdana", "DejaVu Sans", "Liberation Sans"]) {
                db.set_sans_serif_family(f);
            }
            if let Some(f) = pick(&db, &["Times New Roman", "Georgia", "Cambria", "DejaVu Serif", "Liberation Serif"]) {
                db.set_serif_family(f);
            }
            if let Some(f) = pick(&db, &["Consolas", "Cascadia Mono", "Courier New", "DejaVu Sans Mono"]) {
                db.set_monospace_family(f);
            }
            Arc::new(db)
        })
        .clone()
}

/// Rasterizes an SVG well above screen size so it stays sharp when zoomed; the
/// logical size stays the SVG's own size so "100%" means what it should.
fn decode_svg(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let text_free = !data.windows(5).any(|w| w == b"<text");
    let options = usvg::Options {
        resources_dir: path.parent().map(Path::to_path_buf),
        // Loading system fonts takes a moment, so only do it when needed.
        fontdb: if text_free { Arc::new(usvg::fontdb::Database::new()) } else { fonts() },
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(&data, &options).map_err(|e| format!("Invalid SVG: {e}"))?;
    let (iw, ih) = (tree.size().width(), tree.size().height());

    let longest = iw.max(ih);
    let target = longest.clamp(4096.0, 8192.0).min(max_dimension as f32);
    let mut scale = target / longest;
    let pixels = iw * ih * scale * scale;
    const MAX_SVG_PIXELS: f32 = 40_000_000.0;
    if pixels > MAX_SVG_PIXELS {
        scale *= (MAX_SVG_PIXELS / pixels).sqrt();
    }
    let (w, h) = (((iw * scale).round() as u32).max(1), ((ih * scale).round() as u32).max(1));
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("SVG is too large")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(w as f32 / iw, h as f32 / ih),
        &mut pixmap.as_mut(),
    );

    let source_size = ((iw.round() as u32).max(1), (ih.round() as u32).max(1));
    // tiny-skia already renders premultiplied RGBA; just swizzle to BGRA.
    let mut pixels = pixmap.take();
    for p in pixels.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    let mut img = finish(w, h, pixels, source_size);
    img.scalable = true;
    Ok(img)
}

// ---- Photoshop -------------------------------------------------------------

fn decode_psd(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let rgba = crate::psd::decode(&data).map_err(|e| format!("Couldn't read this Photoshop file: {e}"))?;
    Ok(from_rgba(rgba, max_dimension))
}

// ---- JPEG XL and the image crate -------------------------------------------

fn decode_jxl(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Couldn't read this JPEG XL file: {e}"))?;
    let img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    Ok(from_dynamic(img, max_dimension))
}

fn decode_generic(path: &Path, max_dimension: u32) -> Result<DecodedImage, String> {
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(from_dynamic(img, max_dimension))
}

/// Converts any pixel layout to 8-bit RGBA. Floating-point (HDR/EXR) images
/// hold linear light, so they're encoded to sRGB rather than just clamped.
fn from_dynamic(img: DynamicImage, max_dimension: u32) -> DecodedImage {
    let rgba = match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            let f = img.into_rgba32f();
            let (w, h) = f.dimensions();
            let bytes = f
                .pixels()
                .flat_map(|p| {
                    let [r, g, b, a] = p.0;
                    [linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(b), (a.clamp(0.0, 1.0) * 255.0).round() as u8]
                })
                .collect();
            RgbaImage::from_raw(w, h, bytes).unwrap()
        }
        other => other.into_rgba8(),
    };
    from_rgba(rgba, max_dimension)
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

/// Converts straight-alpha RGBA to the premultiplied BGRA Direct2D draws.
pub fn premultiplied_bgra(rgba: RgbaImage) -> Vec<u8> {
    let mut pixels = rgba.into_raw();
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
        p.swap(0, 2);
    }
    pixels
}

/// Downscales oversized images and converts straight-alpha RGBA for display.
pub fn from_rgba(mut rgba: RgbaImage, max_dimension: u32) -> DecodedImage {
    let source_size = rgba.dimensions();
    let (w, h) = source_size;
    let mut scale = 1.0f64;
    if w.max(h) > max_dimension {
        scale = max_dimension as f64 / w.max(h) as f64;
    }
    let pixels = w as f64 * h as f64 * scale * scale;
    if pixels > crate::decode::MAX_PIXELS as f64 {
        scale *= (crate::decode::MAX_PIXELS as f64 / pixels).sqrt();
    }
    if scale < 1.0 {
        let nw = ((w as f64 * scale).round() as u32).max(1);
        let nh = ((h as f64 * scale).round() as u32).max(1);
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Lanczos3);
    }
    let (w, h) = rgba.dimensions();
    finish(w, h, premultiplied_bgra(rgba), source_size)
}

/// Builds the result from premultiplied BGRA pixels.
pub(crate) fn finish(width: u32, height: u32, pixels: Vec<u8>, source_size: (u32, u32)) -> DecodedImage {
    let has_alpha = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
    DecodedImage {
        width,
        height,
        pixels,
        source_size,
        orientation: 1,
        has_alpha,
        animation: Vec::new(),
        first_delay: Duration::ZERO,
        scalable: false,
    }
}
