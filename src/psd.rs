//! Reads the flattened composite image that Photoshop stores at the end of
//! every PSD/PSB file. Layers are skipped entirely, which makes this fast and
//! immune to layer features (ZIP-compressed layers, adjustment layers, smart
//! objects) a viewer doesn't need.

use image::RgbaImage;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or("File is truncated")?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

/// Decodes PackBits run-length data into exactly `out.len()` bytes.
fn unpack_bits(mut src: &[u8], out: &mut [u8]) -> Result<(), String> {
    let mut o = 0;
    while o < out.len() {
        let (&n, rest) = src.split_first().ok_or("Corrupt RLE data")?;
        src = rest;
        let n = n as i8;
        if n >= 0 {
            let len = n as usize + 1;
            if src.len() < len || o + len > out.len() {
                return Err("Corrupt RLE data".into());
            }
            out[o..o + len].copy_from_slice(&src[..len]);
            src = &src[len..];
            o += len;
        } else if n != -128 {
            let len = (1 - n as isize) as usize;
            let (&b, rest) = src.split_first().ok_or("Corrupt RLE data")?;
            src = rest;
            if o + len > out.len() {
                return Err("Corrupt RLE data".into());
            }
            out[o..o + len].fill(b);
            o += len;
        }
    }
    Ok(())
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

pub fn decode(data: &[u8]) -> Result<RgbaImage, String> {
    let mut r = Reader { data, pos: 0 };
    if r.bytes(4)? != b"8BPS" {
        return Err("Not a Photoshop file".into());
    }
    let version = r.u16()?;
    let large = match version {
        1 => false,
        2 => true, // PSB
        _ => return Err("Unknown Photoshop file version".into()),
    };
    r.bytes(6)?;
    let channels = r.u16()? as usize;
    let height = r.u32()? as usize;
    let width = r.u32()? as usize;
    let depth = r.u16()?;
    let mode = r.u16()?;
    if width == 0 || height == 0 || channels == 0 {
        return Err("Photoshop file has no image".into());
    }
    if !matches!(depth, 1 | 8 | 16 | 32) {
        return Err(format!("Unsupported bit depth {depth}"));
    }

    let palette_len = r.u32()? as usize;
    let palette = r.bytes(palette_len)?;
    let resources_len = r.u32()? as usize;
    r.bytes(resources_len)?;

    // Layer section: only its layer count matters. A negative count means the
    // first extra channel of the composite holds its transparency.
    let layers_len = if large { r.u64()? as usize } else { r.u32()? as usize };
    let layers_end = r.pos.checked_add(layers_len).ok_or("File is truncated")?;
    let mut composite_has_alpha = false;
    if layers_len >= if large { 10 } else { 6 } {
        let _layer_info_len = if large { r.u64()? } else { r.u32()? as u64 };
        composite_has_alpha = (r.u16()? as i16) < 0;
    }
    r.pos = layers_end;

    let compression = r.u16()?;
    let row_bytes = if depth == 1 { width.div_ceil(8) } else { width * depth as usize / 8 };
    let plane = row_bytes * height;
    let mut planes = vec![0u8; plane * channels];
    match compression {
        0 => planes.copy_from_slice(r.bytes(plane * channels)?),
        1 => {
            let rows = height * channels;
            let mut counts = Vec::with_capacity(rows);
            for _ in 0..rows {
                counts.push(if large { r.u32()? as usize } else { r.u16()? as usize });
            }
            for (row, count) in planes.chunks_exact_mut(row_bytes).zip(counts) {
                unpack_bits(r.bytes(count)?, row)?;
            }
        }
        _ => return Err("This Photoshop file's composite image uses ZIP compression, which isn't supported".into()),
    }

    // Normalised 0..255 sample of channel `c` at pixel `i`.
    let sample = |c: usize, i: usize| -> u8 {
        let p = &planes[c * plane..(c + 1) * plane];
        let (y, x) = (i / width, i % width);
        let row = &p[y * row_bytes..(y + 1) * row_bytes];
        match depth {
            1 => {
                if row[x / 8] & (0x80 >> (x % 8)) != 0 { 0 } else { 255 }
            }
            8 => row[x],
            16 => row[x * 2], // high byte
            _ => linear_to_srgb(f32::from_be_bytes(row[x * 4..x * 4 + 4].try_into().unwrap())),
        }
    };

    let colour_channels = match mode {
        3 => 3, // RGB
        4 => 4, // CMYK
        9 => return Err("Lab colour Photoshop files aren't supported".into()),
        _ => 1, // bitmap, grayscale, indexed, duotone, multichannel
    };
    if channels < colour_channels {
        return Err("Photoshop file is missing colour channels".into());
    }
    let alpha = (composite_has_alpha && channels > colour_channels).then_some(colour_channels);

    let mut out = vec![0u8; width * height * 4];
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let rgb = match mode {
            3 => [sample(0, i), sample(1, i), sample(2, i)],
            4 => {
                // CMYK is stored inverted: 255 means no ink.
                let k = sample(3, i) as u32;
                let f = |c: u8| (c as u32 * k / 255) as u8;
                [f(sample(0, i)), f(sample(1, i)), f(sample(2, i))]
            }
            2 if palette.len() >= 768 => {
                let idx = sample(0, i) as usize;
                [palette[idx], palette[256 + idx], palette[512 + idx]]
            }
            _ => {
                let g = sample(0, i);
                [g, g, g]
            }
        };
        let a = alpha.map(|c| sample(c, i)).unwrap_or(255);
        px.copy_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
    }
    RgbaImage::from_raw(width as u32, height as u32, out).ok_or_else(|| "Bad image size".into())
}
