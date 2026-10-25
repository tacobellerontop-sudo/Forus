//! Direct3D 11 + Direct2D rendering.
//!
//! One D3D11 device is shared by Direct2D (drawing) and the Media Foundation
//! Media Engine (hardware video decode), so decoded video frames never leave
//! the GPU. Presentation uses a flip-model swap chain.

use windows::core::{w, Interface, Result, HSTRING};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows_numerics::Matrix3x2;

pub struct Fonts {
    pub body: IDWriteTextFormat,
    pub title: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
}

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub ctx: ID2D1DeviceContext,
    pub dwrite: IDWriteFactory,
    pub brush: ID2D1SolidColorBrush,
    pub checker: ID2D1BitmapBrush1,
    pub max_bitmap_size: u32,
    swap: IDXGISwapChain1,
    size: (u32, u32),
}

pub fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

pub fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

fn create_device() -> Result<ID3D11Device> {
    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let attempts = [
        (D3D_DRIVER_TYPE_HARDWARE, D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT),
        (D3D_DRIVER_TYPE_HARDWARE, D3D11_CREATE_DEVICE_BGRA_SUPPORT),
        (D3D_DRIVER_TYPE_WARP, D3D11_CREATE_DEVICE_BGRA_SUPPORT),
    ];
    let mut last_err = None;
    for (driver, flags) in attempts {
        let mut device = None;
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        match r {
            Ok(()) => return Ok(device.unwrap()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap())
}

impl Gfx {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let d3d = create_device()?;
            // The Media Engine uses the device from its own threads.
            if let Ok(mt) = d3d.GetImmediateContext()?.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let dxgi_device: IDXGIDevice = d3d.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;

            let (w, h) = client_size(hwnd);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swap = dxgi_factory.CreateSwapChainForHwnd(&d3d, hwnd, &desc, None, None)?;
            dxgi_factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;

            let factory: ID2D1Factory1 = D2D1CreateFactory(
                D2D1_FACTORY_TYPE_MULTI_THREADED,
                Some(&D2D1_FACTORY_OPTIONS::default()),
            )?;
            let device = factory.CreateDevice(&dxgi_device)?;
            let ctx = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            // Work in physical pixels everywhere; UI is scaled by DPI manually.
            ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);

            let brush = ctx.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 1.0), None)?;
            let checker = create_checker(&ctx)?;
            let max_bitmap_size = ctx.GetMaximumBitmapSize();
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let gfx = Self { d3d, ctx, dwrite, brush, checker, max_bitmap_size, swap, size: (w, h) };
            gfx.create_target()?;
            Ok(gfx)
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    fn create_target(&self) -> Result<()> {
        unsafe {
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            let target = self.ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.ctx.SetTarget(&target);
        }
        Ok(())
    }

    /// Resizes the swap chain to match the window's client area.
    pub fn sync_size(&mut self, hwnd: HWND) -> Result<()> {
        let size = client_size(hwnd);
        if size == self.size {
            return Ok(());
        }
        unsafe {
            self.ctx.SetTarget(None);
            self.swap.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        self.size = size;
        self.create_target()
    }

    /// Presents with vsync. Returns an error if the device was lost.
    pub fn present(&self) -> Result<()> {
        unsafe { self.swap.Present(1, DXGI_PRESENT(0)).ok() }
    }

    /// Uploads premultiplied BGRA pixels into a GPU bitmap.
    pub fn create_bitmap(&self, width: u32, height: u32, pixels: &[u8]) -> Result<ID2D1Bitmap1> {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        unsafe {
            self.ctx.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr() as *const _),
                width * 4,
                &props,
            )
        }
    }

    pub fn fonts(&self, scale: f32) -> Result<Fonts> {
        unsafe {
            let make = |family: &HSTRING, size: f32, weight: DWRITE_FONT_WEIGHT| {
                self.dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size * scale,
                    w!("en-us"),
                )
            };
            let ui = self.first_font(&["Segoe UI Variable Text", "Segoe UI", "Tahoma", "Arial"]);
            let icons = self.first_font(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]);
            let fonts = Fonts {
                body: make(&ui, 13.0, DWRITE_FONT_WEIGHT_NORMAL)?,
                title: make(&ui, 15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                icons: make(&icons, 16.0, DWRITE_FONT_WEIGHT_NORMAL)?,
            };
            fonts.icons.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fonts.icons.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(fonts)
        }
    }

    fn first_font(&self, families: &[&str]) -> HSTRING {
        let name = families.iter().find(|f| self.has_font(f)).unwrap_or(&families[0]);
        HSTRING::from(*name)
    }

    fn has_font(&self, family: &str) -> bool {
        unsafe {
            let mut collection = None;
            if self.dwrite.GetSystemFontCollection(&mut collection, false).is_err() {
                return false;
            }
            let Some(collection) = collection else { return false };
            let mut index = 0;
            let mut exists = Default::default();
            collection
                .FindFamilyName(&HSTRING::from(family), &mut index, &mut exists)
                .is_ok()
                && exists.as_bool()
        }
    }

    pub fn set_color(&self, c: D2D1_COLOR_F) {
        unsafe { self.brush.SetColor(&c) }
    }

    pub fn fill_rect(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        self.set_color(c);
        unsafe { self.ctx.FillRectangle(&r, &self.brush) }
    }

    pub fn fill_round(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        self.set_color(c);
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.ctx.FillRoundedRectangle(&rr, &self.brush) }
    }

    pub fn fill_ellipse(&self, cx: f32, cy: f32, radius: f32, c: D2D1_COLOR_F) {
        self.set_color(c);
        let e = D2D1_ELLIPSE {
            point: windows_numerics::Vector2 { X: cx, Y: cy },
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.ctx.FillEllipse(&e, &self.brush) }
    }

    /// Lays out text, returning the layout and its measured size.
    pub fn layout(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        max_width: f32,
    ) -> Result<(IDWriteTextLayout, f32, f32)> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, format, max_width.max(1.0), 10_000.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            Ok((layout, m.widthIncludingTrailingWhitespace, m.height))
        }
    }

    pub fn draw_layout(&self, layout: &IDWriteTextLayout, x: f32, y: f32, c: D2D1_COLOR_F) {
        self.set_color(c);
        unsafe {
            self.ctx.DrawTextLayout(
                windows_numerics::Vector2 { X: x, Y: y },
                layout,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            )
        }
    }

    pub fn draw_text(&self, text: &str, format: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        self.set_color(c);
        unsafe {
            self.ctx.DrawText(
                &wide,
                format,
                &r,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            )
        }
    }

    pub fn set_transform(&self, m: &Matrix3x2) {
        unsafe { self.ctx.SetTransform(m) }
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (((rc.right - rc.left).max(1)) as u32, ((rc.bottom - rc.top).max(1)) as u32)
}

/// A small two-tone tile used behind transparent images.
fn create_checker(ctx: &ID2D1DeviceContext) -> Result<ID2D1BitmapBrush1> {
    const N: u32 = 16;
    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let v = if (x / 8 + y / 8) % 2 == 0 { 0x3a } else { 0x2c };
            let i = ((y * N + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[v, v, v, 0xff]);
        }
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
    };
    unsafe {
        let bmp = ctx.CreateBitmap(D2D_SIZE_U { width: N, height: N }, Some(px.as_ptr() as _), N * 4, &props)?;
        let brush_props = D2D1_BITMAP_BRUSH_PROPERTIES1 {
            extendModeX: D2D1_EXTEND_MODE_WRAP,
            extendModeY: D2D1_EXTEND_MODE_WRAP,
            interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
        };
        ctx.CreateBitmapBrush(&bmp, Some(&brush_props), None)
    }
}
