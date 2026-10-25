//! Hardware-accelerated video playback with the Media Foundation Media Engine.
//!
//! The engine decodes on the GPU (DXVA) using our D3D11 device and copies each
//! frame into a texture that Direct2D draws directly, so frames stay in video
//! memory end to end.

use std::path::Path;

use windows::core::{implement, Interface, Result, BSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, S_OK, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::gfx::Gfx;
use crate::WM_APP_MEDIA;

/// Forwards Media Engine events (raised on MF worker threads) to the window.
#[implement(IMFMediaEngineNotify)]
struct Notify {
    hwnd: isize,
    generation: u32,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> Result<()> {
        let lparam = ((self.generation as isize) << 32) | event as isize;
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_APP_MEDIA, WPARAM(0), LPARAM(lparam));
        }
        Ok(())
    }
}

/// Splits the LPARAM posted by [`Notify`] into (generation, event).
pub fn unpack_event(lparam: LPARAM) -> (u32, u32) {
    ((lparam.0 >> 32) as u32, (lparam.0 & 0xffff_ffff) as u32)
}

struct Frame {
    texture: ID3D11Texture2D,
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
}

pub struct Video {
    engine: IMFMediaEngine,
    // Keeps the device manager alive for the engine's lifetime.
    _manager: IMFDXGIDeviceManager,
    frame: Option<Frame>,
    pub generation: u32,
    pub has_frame: bool,
}

impl Video {
    pub fn open(gfx: &Gfx, hwnd: HWND, path: &Path, generation: u32, volume: f64, muted: bool) -> Result<Self> {
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&gfx.d3d, token)?;

            let notify: IMFMediaEngineNotify = Notify { hwnd: hwnd.0 as isize, generation }.into();

            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.unwrap();
            attributes.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
            attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
            attributes.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;

            let factory: IMFMediaEngineClassFactory =
                CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_ALL)?;
            let engine = factory.CreateInstance(0, &attributes)?;
            engine.SetLoop(true)?;
            engine.SetVolume(volume)?;
            engine.SetMuted(muted)?;
            engine.SetAutoPlay(true)?;
            engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))?;

            Ok(Self { engine, _manager: manager, frame: None, generation, has_frame: false })
        }
    }

    /// Display size of the video (pixel aspect ratio applied), once known.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        unsafe {
            if !self.engine.HasVideo().as_bool() {
                return None;
            }
            let (mut w, mut h) = (0u32, 0u32);
            self.engine.GetNativeVideoSize(Some(&mut w), Some(&mut h)).ok()?;
            if w == 0 || h == 0 {
                return None;
            }
            let (mut ax, mut ay) = (0u32, 0u32);
            if self.engine.GetVideoAspectRatio(Some(&mut ax), Some(&mut ay)).is_ok() && ax > 0 && ay > 0 && ax != ay {
                w = ((w as u64 * ax as u64) / ay as u64).max(1) as u32;
            }
            Some((w, h))
        }
    }

    /// Pulls the newest decoded frame into our texture if one is ready.
    pub fn update(&mut self, gfx: &Gfx) -> Result<()> {
        let Some(size) = self.display_size() else { return Ok(()) };
        if self.frame.as_ref().map(|f| f.size) != Some(size) {
            self.frame = Some(create_frame(gfx, size)?);
            self.has_frame = false;
        }
        let frame = self.frame.as_ref().unwrap();
        unsafe {
            // OnVideoStreamTick returns S_FALSE when there is no new frame; the
            // projected wrapper folds that into Ok, so call through the vtable.
            let mut pts = 0i64;
            let hr = (Interface::vtable(&self.engine).OnVideoStreamTick)(Interface::as_raw(&self.engine), &mut pts);
            if hr == S_OK {
                let dst = RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
                self.engine.TransferVideoFrame(&frame.texture, None, &dst, None)?;
                self.has_frame = true;
            }
        }
        Ok(())
    }

    pub fn bitmap(&self) -> Option<(&ID2D1Bitmap1, (u32, u32))> {
        if !self.has_frame {
            return None;
        }
        self.frame.as_ref().map(|f| (&f.bitmap, f.size))
    }

    pub fn is_paused(&self) -> bool {
        unsafe { self.engine.IsPaused().as_bool() }
    }

    pub fn toggle_play(&self) {
        unsafe {
            if self.is_paused() {
                if self.is_ended() {
                    let _ = self.engine.SetCurrentTime(0.0);
                }
                let _ = self.engine.Play();
            } else {
                let _ = self.engine.Pause();
            }
        }
    }

    fn is_ended(&self) -> bool {
        unsafe { self.engine.IsEnded().as_bool() }
    }

    pub fn position(&self) -> f64 {
        unsafe { self.engine.GetCurrentTime() }
    }

    pub fn duration(&self) -> Option<f64> {
        let d = unsafe { self.engine.GetDuration() };
        (d.is_finite() && d > 0.0).then_some(d)
    }

    pub fn seek(&self, seconds: f64) {
        let mut t = seconds.max(0.0);
        if let Some(d) = self.duration() {
            t = t.min(d);
        }
        unsafe {
            let _ = self.engine.SetCurrentTime(t);
        }
    }

    /// Seeks to the nearest keyframe; cheap enough to call while scrubbing.
    pub fn seek_fast(&self, seconds: f64) {
        match self.engine.cast::<IMFMediaEngineEx>() {
            Ok(ex) => unsafe {
                let _ = ex.SetCurrentTimeEx(seconds.max(0.0), MF_MEDIA_ENGINE_SEEK_MODE_APPROXIMATE);
            },
            Err(_) => self.seek(seconds),
        }
    }

    pub fn set_volume(&self, volume: f64) {
        unsafe {
            let _ = self.engine.SetVolume(volume);
        }
    }

    pub fn set_muted(&self, muted: bool) {
        unsafe {
            let _ = self.engine.SetMuted(muted);
        }
    }

    pub fn looping(&self) -> bool {
        unsafe { self.engine.GetLoop().as_bool() }
    }

    pub fn set_looping(&self, looping: bool) {
        unsafe {
            let _ = self.engine.SetLoop(looping);
        }
    }

    /// Describes the last playback error, if any.
    pub fn error(&self) -> Option<String> {
        unsafe {
            let err = self.engine.GetError().ok()?;
            let code = err.GetErrorCode();
            let hr = err.GetExtendedErrorCode().map(|_| 0).unwrap_or_else(|e| e.code().0 as u32);
            let what = match MF_MEDIA_ENGINE_ERR(code as i32) {
                MF_MEDIA_ENGINE_ERR_SRC_NOT_SUPPORTED => {
                    "This video's format or codec isn't supported. HEVC, AV1 and VP9 videos may need \
                     their extension from the Microsoft Store."
                }
                MF_MEDIA_ENGINE_ERR_DECODE => "The video couldn't be decoded.",
                MF_MEDIA_ENGINE_ERR_NETWORK => "The file couldn't be read.",
                _ => "The video couldn't be played.",
            };
            Some(if hr != 0 { format!("{what} (0x{hr:08X})") } else { what.to_string() })
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        unsafe {
            let _ = self.engine.Shutdown();
        }
    }
}

fn create_frame(gfx: &Gfx, size: (u32, u32)) -> Result<Frame> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        gfx.d3d.CreateTexture2D(&desc, None, Some(&mut texture))?;
        let texture = texture.unwrap();
        let surface: IDXGISurface = texture.cast()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let bitmap = gfx.ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
        Ok(Frame { texture, bitmap, size })
    }
}
