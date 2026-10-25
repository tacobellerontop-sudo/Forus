//! Background image decoding with WIC.
//!
//! Decoding a large JPEG/HEIC can take hundreds of milliseconds, so it runs on
//! a worker thread. The worker produces premultiplied BGRA pixels that the UI
//! thread uploads to a GPU bitmap.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::Foundation::{GENERIC_READ, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::WM_APP_DECODED;

/// Upper bound on decoded pixels; larger images are downscaled while decoding
/// to keep memory reasonable (still far beyond any screen).
const MAX_PIXELS: u64 = 120_000_000;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Size of the original file's image before any downscaling.
    pub source_size: (u32, u32),
    /// EXIF orientation (1-8).
    pub orientation: u16,
    pub has_alpha: bool,
}

pub struct Decoded {
    pub path: PathBuf,
    pub result: Result<Arc<DecodedImage>, String>,
}

struct Queue {
    jobs: Mutex<VecDeque<PathBuf>>,
    ready: Condvar,
}

pub struct Decoder {
    queue: Arc<Queue>,
    pub results: Receiver<Decoded>,
}

impl Decoder {
    pub fn new(hwnd: HWND, max_dimension: u32) -> Self {
        let queue = Arc::new(Queue { jobs: Mutex::new(VecDeque::new()), ready: Condvar::new() });
        let (tx, rx) = channel();
        let worker_queue = queue.clone();
        let hwnd = hwnd.0 as isize;
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || worker(worker_queue, tx, hwnd, max_dimension))
            .expect("spawn decoder thread");
        Self { queue, results: rx }
    }

    /// Queues a decode. Urgent jobs (the image the user is waiting for) jump
    /// ahead of prefetches.
    pub fn request(&self, path: PathBuf, urgent: bool) {
        let mut jobs = self.queue.jobs.lock().unwrap();
        if let Some(i) = jobs.iter().position(|p| *p == path) {
            if !urgent {
                return;
            }
            jobs.remove(i);
        }
        if urgent {
            jobs.push_front(path);
        } else {
            jobs.push_back(path);
        }
        self.queue.ready.notify_one();
    }

    /// Drops queued jobs that are no longer wanted.
    pub fn retain(&self, keep: impl Fn(&Path) -> bool) {
        self.queue.jobs.lock().unwrap().retain(|p| keep(p));
    }
}

fn worker(queue: Arc<Queue>, tx: Sender<Decoded>, hwnd: isize, max_dimension: u32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let factory: Option<IWICImagingFactory> =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
    loop {
        let path = {
            let mut jobs = queue.jobs.lock().unwrap();
            loop {
                if let Some(p) = jobs.pop_front() {
                    break p;
                }
                jobs = queue.ready.wait(jobs).unwrap();
            }
        };
        let result = match &factory {
            Some(f) => decode(f, &path, max_dimension).map(Arc::new).map_err(describe),
            None => Err("Windows Imaging Component is unavailable".into()),
        };
        if tx.send(Decoded { path, result }).is_err() {
            return;
        }
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as _)), WM_APP_DECODED, WPARAM(0), LPARAM(0));
        }
    }
}

fn describe(e: windows::core::Error) -> String {
    // WINCODEC_ERR_COMPONENTNOTFOUND: no codec installed for this format.
    if e.code().0 as u32 == 0x8898_2F50 {
        "No decoder is installed for this format. HEIC, AVIF and some RAW formats need \
         their codec extension from the Microsoft Store."
            .into()
    } else {
        let msg = e.message();
        if msg.is_empty() {
            format!("Error 0x{:08X}", e.code().0 as u32)
        } else {
            msg
        }
    }
}

fn decode(
    factory: &IWICImagingFactory,
    path: &Path,
    max_dimension: u32,
) -> windows::core::Result<DecodedImage> {
    unsafe {
        let decoder = factory.CreateDecoderFromFilename(
            &HSTRING::from(path.as_os_str()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let orientation = read_orientation(&frame).unwrap_or(1);

        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let source_size = (w, h);

        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        let mut source: IWICBitmapSource = converter.cast()?;

        // Fit within the GPU's maximum texture size and the memory budget.
        let mut scale = 1.0f64;
        let longest = w.max(h) as f64;
        if longest > max_dimension as f64 {
            scale = max_dimension as f64 / longest;
        }
        let pixels = w as f64 * h as f64 * scale * scale;
        if pixels > MAX_PIXELS as f64 {
            scale *= (MAX_PIXELS as f64 / pixels).sqrt();
        }
        if scale < 1.0 {
            let nw = ((w as f64 * scale).round() as u32).max(1);
            let nh = ((h as f64 * scale).round() as u32).max(1);
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(&source, nw, nh, WICBitmapInterpolationModeHighQualityCubic)?;
            source = scaler.cast()?;
            w = nw;
            h = nh;
        }

        let stride = w * 4;
        let mut pixels = vec![0u8; stride as usize * h as usize];
        source.CopyPixels(std::ptr::null(), stride, &mut pixels)?;
        let has_alpha = pixels.chunks_exact(4).any(|p| p[3] != 255);

        Ok(DecodedImage { width: w, height: h, pixels, source_size, orientation, has_alpha })
    }
}

fn read_orientation(frame: &IWICBitmapFrameDecode) -> Option<u16> {
    let reader = unsafe { frame.GetMetadataQueryReader().ok()? };
    let queries: [PCWSTR; 4] = [
        w!("System.Photo.Orientation"),
        w!("/app1/ifd/{ushort=274}"),
        w!("/ifd/{ushort=274}"),
        w!("/xmp/tiff:Orientation"),
    ];
    for query in queries {
        let mut value = PROPVARIANT::default();
        if unsafe { reader.GetMetadataByName(query, &mut value) }.is_ok() {
            let v = u32::try_from(&value)
                .ok()
                .or_else(|| value.to_string().trim().parse::<u32>().ok());
            if let Some(v @ 1..=8) = v {
                return Some(v as u16);
            }
        }
    }
    None
}
