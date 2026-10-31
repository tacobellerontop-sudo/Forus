//! Window, input handling, view state and drawing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, Error, Result, BOOL, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Matrix3x2;

use crate::assoc;
use crate::decode::{DecodedImage, Decoder};
use crate::files::{self, Kind};
use crate::gfx::{color, rect, Fonts, Gfx};
use crate::titlebar::{self, Button, TitleBar, TitleHit, TitleState, Tool};
use crate::video::{self, Video};
use crate::{WM_APP_DECODED, WM_APP_MEDIA};

const APP_NAME: &str = "Forus";
const BACKGROUND: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.075, g: 0.075, b: 0.078, a: 1.0 };
const ACCENT: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.30, g: 0.64, b: 1.0, a: 1.0 };
const MIN_ZOOM: f32 = 0.01;
const MAX_ZOOM: f32 = 64.0;
/// How long a zoom (wheel, keys, fit, actual size) takes to glide into place.
const ZOOM_ANIM: Duration = Duration::from_millis(180);
const UI_HIDE_AFTER: f32 = 2.0;
const UI_FADE: f32 = 0.3;
const TIMER_SIZEMOVE: usize = 1;

const D2DERR_RECREATE_TARGET: HRESULT = HRESULT(0x8899_000C_u32 as i32);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` against the app unless it is already borrowed (re-entrant
/// messages sent while handling another message fall through to
/// `DefWindowProc`).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut app| app.as_mut().map(f)))
}

pub fn fatal(message: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(message), &HSTRING::from(APP_NAME), MB_ICONERROR | MB_OK);
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;

        // Command-line management of file associations, for scripts/installers.
        let arg = std::env::args_os().nth(1);
        match arg.as_ref().and_then(|a| a.to_str()) {
            Some("--register") => return assoc::register(),
            Some("--unregister") => {
                assoc::unregister();
                return Ok(());
            }
            Some("--set-default") => {
                assoc::register()?;
                assoc::open_default_apps_settings(None);
                return Ok(());
            }
            _ => {}
        }

        // Keep the registration current (e.g. after the exe was moved) so
        // Forus is always offered in "Open with" and Default apps.
        std::thread::spawn(|| {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if !assoc::is_registered() {
                let _ = assoc::register();
            }
        });

        let instance = GetModuleHandleW(None)?;
        let class_name = w!("ForusViewerWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hIcon: LoadIconW(Some(instance.into()), PCWSTR(1 as _))
                .or_else(|_| LoadIconW(None, IDI_APPLICATION))?,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            &HSTRING::from(APP_NAME),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        apply_window_style(hwnd);

        // Size the window for the monitor it opened on.
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let (w, h) = ((1280.0 * scale) as i32, (820.0 * scale) as i32);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let work = mi.rcWork;
        let (w, h) = (w.min(work.right - work.left), h.min(work.bottom - work.top));
        let _ = SetWindowPos(
            hwnd,
            None,
            work.left + (work.right - work.left - w) / 2,
            work.top + (work.bottom - work.top - h) / 2,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        let mut app = App::new(hwnd)?;
        if let Some(arg) = arg {
            app.open(PathBuf::from(arg));
        }
        APP.with(|cell| *cell.borrow_mut() = Some(app));

        DragAcceptFiles(hwnd, true);
        let _ = ShowWindow(hwnd, SW_SHOW);

        let result = message_loop();
        APP.with(|cell| cell.borrow_mut().take());
        let _ = MFShutdown();
        CoUninitialize();
        result
    }
}

fn message_loop() -> Result<()> {
    let mut msg = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Render if needed, then decide how long we may sleep.
        let wait = with_app(|app| {
            let now = Instant::now();
            app.advance_animation(now);
            if app.dirty || app.animating(now) {
                app.render();
            }
            app.update_cursor_visibility();
            if app.animating(Instant::now()) {
                0
            } else {
                app.next_wakeup(Instant::now())
                    .map(|d| d.as_millis().min(u32::MAX as u128 - 1) as u32)
                    .unwrap_or(INFINITE)
            }
        })
        .unwrap_or(INFINITE);
        if wait > 0 {
            unsafe {
                MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCCALCSIZE if wparam.0 != 0 => {
            // Remove the native caption but keep the native left/right/bottom
            // resize borders, shadow and rounded corners: let Windows compute
            // the client area, then give the caption strip back to it.
            unsafe {
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                let result = DefWindowProcW(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top;
                // A maximized window hangs its frame off-screen; keep the
                // title bar on the monitor.
                if IsZoomed(hwnd).as_bool() {
                    params.rgrc[0].top += frame_thickness(hwnd);
                }
                return result;
            }
        }
        WM_GETMINMAXINFO => {
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            info.ptMinTrackSize = POINT { x: (360.0 * scale) as i32, y: (240.0 * scale) as i32 };
            return LRESULT(0);
        }
        _ => {}
    }
    if let Some(Some(result)) = with_app(|app| app.handle(msg, wparam, lparam)) {
        return result;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Height of the (invisible) top resize border, in pixels.
fn frame_thickness(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

fn apply_window_style(hwnd: HWND) {
    unsafe {
        let dark = BOOL::from(true);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        );
        let backdrop = DWMSBT_MAINWINDOW; // Mica title bar
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as _,
            std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        );
        let corners = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const _ as _,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

// ---------------------------------------------------------------------------

struct Picture {
    /// One bitmap per frame (a single entry for still images) and how long
    /// each frame stays on screen.
    frames: Vec<(ID2D1Bitmap1, Duration)>,
    frame: usize,
    /// When to show the next frame; `None` for still or paused images.
    next_frame_at: Option<Instant>,
    /// Bitmap size in pixels (may be smaller than the file if downscaled).
    pixels: (u32, u32),
    /// Size of the image in the file; zoom percentages refer to this.
    size: (u32, u32),
    orientation: u16,
    has_alpha: bool,
    /// Vector art: may be enlarged to fit the window.
    scalable: bool,
}

impl Picture {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.frames[self.frame].0
    }

    fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    fn toggle_pause(&mut self) -> bool {
        self.next_frame_at = match self.next_frame_at {
            Some(_) => None,
            None => Some(Instant::now() + self.frames[self.frame].1),
        };
        self.next_frame_at.is_none()
    }

    /// Pauses and moves `delta` frames.
    fn step(&mut self, delta: isize) {
        let n = self.frames.len() as isize;
        self.next_frame_at = None;
        self.frame = (self.frame as isize + delta).rem_euclid(n) as usize;
    }

    /// Advances to the frame due at `now`. Returns whether it changed.
    fn advance(&mut self, now: Instant) -> bool {
        let Some(mut due) = self.next_frame_at else { return false };
        if now < due {
            return false;
        }
        while now >= due {
            self.frame = (self.frame + 1) % self.frames.len();
            due += self.frames[self.frame].1;
            // After a long stall (window hidden, modal loop) don't fast-forward.
            if now.duration_since(due) > Duration::from_secs(1) {
                due = now + self.frames[self.frame].1;
            }
        }
        self.next_frame_at = Some(due);
        true
    }
}

enum Content {
    Empty,
    Image(Picture),
    Video(Video),
    Error(String),
}

#[derive(Default, Clone, Copy)]
struct View {
    /// `None` fits the content to the window.
    zoom: Option<f32>,
    pan: (f32, f32),
    /// Extra clockwise quarter turns chosen by the user.
    rotation: u8,
    /// A zoom in progress, gliding from this placement to the one above.
    anim: Option<ZoomAnim>,
}

#[derive(Clone, Copy)]
struct ZoomAnim {
    /// Scale and center that were on screen when the zoom started.
    scale: f32,
    cx: f32,
    cy: f32,
    start: Instant,
}

struct Drag {
    start: (f32, f32),
    pan: (f32, f32),
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    Play,
    Seek,
    Mute,
}

struct Controls {
    panel: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
    play: (f32, f32, f32),
    mute: (f32, f32, f32),
    bar: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
    time_x: f32,
}

/// How content sits in the window: scale and center in client pixels, plus
/// the displayed (post-rotation) size in content units.
#[derive(Clone, Copy)]
struct Placement {
    scale: f32,
    fit: f32,
    cx: f32,
    cy: f32,
    dw: f32,
    dh: f32,
}

struct App {
    hwnd: HWND,
    gfx: Gfx,
    fonts: Fonts,
    ui_scale: f32,
    decoder: Decoder,
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    pending: HashSet<PathBuf>,

    files: Vec<PathBuf>,
    index: usize,
    content: Content,
    /// The image path we're waiting on the decoder for.
    waiting_for: Option<PathBuf>,
    view: View,
    video_generation: u32,
    volume: f64,
    muted: bool,

    drag: Option<Drag>,
    scrub: Option<f64>,
    hover: Hit,
    fullscreen: Option<WINDOWPLACEMENT>,
    last_input: Instant,
    animate_until: Instant,
    toast: Option<(String, Instant)>,
    pin_info: bool,
    show_help: bool,
    dirty: bool,
    epoch: Instant,
    cursor_hidden: bool,
    tracking_leave: bool,
    tracking_nc_leave: bool,
    // Title bar state.
    title_hover: Option<TitleHit>,
    title_pressed: Option<TitleHit>,
    active: bool,
    app_icon: Option<ID2D1Bitmap1>,
}

fn is_device_lost(e: &Error) -> bool {
    let c = e.code();
    c == DXGI_ERROR_DEVICE_REMOVED || c == DXGI_ERROR_DEVICE_RESET || c == D2DERR_RECREATE_TARGET
}

impl App {
    fn new(hwnd: HWND) -> Result<Self> {
        let gfx = Gfx::new(hwnd)?;
        let ui_scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
        let fonts = gfx.fonts(ui_scale)?;
        let decoder = Decoder::new(hwnd, gfx.max_bitmap_size.min(16384));
        let app_icon = gfx.app_icon((16.0 * ui_scale).round() as u32).ok();
        let now = Instant::now();
        Ok(Self {
            hwnd,
            gfx,
            fonts,
            ui_scale,
            decoder,
            cache: HashMap::new(),
            pending: HashSet::new(),
            files: Vec::new(),
            index: 0,
            content: Content::Empty,
            waiting_for: None,
            view: View::default(),
            video_generation: 0,
            volume: 1.0,
            muted: false,
            drag: None,
            scrub: None,
            hover: Hit::None,
            fullscreen: None,
            last_input: now,
            animate_until: now,
            toast: None,
            pin_info: false,
            show_help: false,
            dirty: true,
            epoch: now,
            cursor_hidden: false,
            tracking_leave: false,
            tracking_nc_leave: false,
            title_hover: None,
            title_pressed: None,
            active: true,
            app_icon,
        })
    }

    // ---- title bar --------------------------------------------------------

    /// Height of the title bar; none in full screen.
    fn top(&self) -> f32 {
        if self.fullscreen.is_some() {
            0.0
        } else {
            self.title_bar().height
        }
    }

    fn title_bar(&self) -> TitleBar {
        TitleBar::layout(self.gfx.size().0 as f32, self.ui_scale)
    }

    fn title_hit(&self, x: f32, y: f32) -> Option<TitleHit> {
        if self.fullscreen.is_some() {
            return None;
        }
        self.title_bar().hit(x, y)
    }

    fn tool_enabled(&self, tool: Tool) -> bool {
        match tool {
            Tool::Previous | Tool::Next => self.files.len() > 1,
            Tool::Rotate | Tool::Fit => self.content_geometry().is_some(),
            _ => true,
        }
    }

    fn run_tool(&mut self, tool: Tool) {
        if !self.tool_enabled(tool) {
            return;
        }
        match tool {
            Tool::Open => self.open_dialog(),
            Tool::Previous => self.go(-1),
            Tool::Next => self.go(1),
            Tool::Rotate => self.rotate(true),
            Tool::Fit => self.fit(),
            Tool::FullScreen => self.toggle_fullscreen(),
            Tool::Help => self.show_help = !self.show_help,
        }
        self.dirty = true;
    }

    fn press_caption_button(&mut self, button: Button) {
        unsafe {
            match button {
                Button::Minimize => {
                    let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
                }
                Button::Maximize => {
                    let maximized = IsZoomed(self.hwnd).as_bool();
                    let _ = ShowWindow(self.hwnd, if maximized { SW_RESTORE } else { SW_MAXIMIZE });
                }
                Button::Close => {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    fn draw_title_bar(&self) {
        if self.fullscreen.is_some() {
            return;
        }
        let title = match self.current_path().and_then(|p| p.file_name()) {
            Some(name) if self.files.len() > 1 => {
                format!("{}  ·  {} / {}", name.to_string_lossy(), self.index + 1, self.files.len())
            }
            Some(name) => name.to_string_lossy().into_owned(),
            None => APP_NAME.to_string(),
        };
        let enabled = |t: Tool| self.tool_enabled(t);
        let state = TitleState {
            title: &title,
            icon: self.app_icon.as_ref(),
            active: self.active,
            maximized: unsafe { IsZoomed(self.hwnd) }.as_bool(),
            hover: self.title_hover,
            pressed: self.title_pressed,
            enabled: &enabled,
        };
        titlebar::draw(&self.gfx, &self.fonts, &self.title_bar(), self.gfx.size().0 as f32, &state);
    }

    /// WM_NCHITTEST: native borders, our caption, Windows' caption buttons.
    fn nc_hit_test(&self, lparam: LPARAM) -> LRESULT {
        let native = unsafe { DefWindowProcW(self.hwnd, WM_NCHITTEST, WPARAM(0), lparam) };
        if native.0 as u32 != HTCLIENT || self.fullscreen.is_some() {
            return native;
        }
        let mut p = POINT { x: get_x(lparam), y: get_y(lparam) };
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        let (x, y) = (p.x as f32, p.y as f32);
        let maximized = unsafe { IsZoomed(self.hwnd) }.as_bool();
        let frame = frame_thickness(self.hwnd);
        if !maximized && p.y < frame {
            let w = self.gfx.size().0 as i32;
            return LRESULT(if p.x < frame * 2 {
                HTTOPLEFT
            } else if p.x >= w - frame * 2 {
                HTTOPRIGHT
            } else {
                HTTOP
            } as isize);
        }
        LRESULT(match self.title_hit(x, y) {
            Some(TitleHit::Caption) => HTCAPTION,
            Some(TitleHit::Button(Button::Minimize)) => HTMINBUTTON,
            Some(TitleHit::Button(Button::Maximize)) => HTMAXBUTTON,
            Some(TitleHit::Button(Button::Close)) => HTCLOSE,
            Some(TitleHit::Tool(_)) | None => HTCLIENT,
        } as isize)
    }

    fn caption_button_for(code: usize) -> Option<Button> {
        match code as u32 {
            HTMINBUTTON => Some(Button::Minimize),
            HTMAXBUTTON => Some(Button::Maximize),
            HTCLOSE => Some(Button::Close),
            _ => None,
        }
    }

    // ---- navigation -------------------------------------------------------

    fn current_path(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    fn open(&mut self, path: PathBuf) {
        let (files, index) = files::playlist_for(&path);
        if files.is_empty() {
            self.toast("No photos or videos in that folder");
            return;
        }
        self.files = files;
        self.index = index;
        self.show_current();
    }

    fn go(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let n = self.files.len() as isize;
        self.index = (self.index as isize + delta).rem_euclid(n) as usize;
        self.show_current();
    }

    fn go_to(&mut self, index: usize) {
        if index < self.files.len() && index != self.index {
            self.index = index;
            self.show_current();
        }
    }

    fn show_current(&mut self) {
        let Some(path) = self.current_path().cloned() else { return };
        self.set_title(&path);
        self.waiting_for = None;
        self.scrub = None;
        self.poke();

        match files::kind_of(&path) {
            Some(Kind::Video) => {
                self.content = Content::Empty; // shut the old engine down first
                self.video_generation = self.video_generation.wrapping_add(1);
                self.content = match Video::open(&self.gfx, self.hwnd, &path, self.video_generation, self.volume, self.muted) {
                    Ok(v) => Content::Video(v),
                    Err(e) => Content::Error(format!("Couldn't open video: {}", e.message())),
                };
                self.view = View::default();
            }
            _ => {
                if let Some(img) = self.cache.get(&path).cloned() {
                    self.present_image(&img);
                } else {
                    if matches!(self.content, Content::Video(_) | Content::Error(_)) {
                        self.content = Content::Empty;
                    }
                    self.waiting_for = Some(path.clone());
                    self.pending.insert(path.clone());
                    self.decoder.request(path, true);
                }
            }
        }
        self.prefetch();
        self.dirty = true;
    }

    /// Paths worth keeping decoded: the current item and its neighbours.
    fn wanted(&self) -> Vec<PathBuf> {
        let n = self.files.len();
        if n == 0 {
            return Vec::new();
        }
        let mut v = vec![self.files[self.index].clone()];
        for d in [1, n - 1] {
            let p = &self.files[(self.index + d) % n];
            if !v.contains(p) {
                v.push(p.clone());
            }
        }
        v.retain(|p| files::kind_of(p) != Some(Kind::Video));
        v
    }

    fn prefetch(&mut self) {
        let wanted = self.wanted();
        self.cache.retain(|p, _| wanted.contains(p));
        self.decoder.retain(|p| wanted.iter().any(|w| w == p));
        self.pending.retain(|p| wanted.contains(p));
        for p in wanted {
            if !self.cache.contains_key(&p) && !self.pending.contains(&p) {
                self.pending.insert(p.clone());
                self.decoder.request(p, false);
            }
        }
    }

    fn on_decoded(&mut self) {
        while let Ok(done) = self.decoder.results.try_recv() {
            self.pending.remove(&done.path);
            let is_current = self.waiting_for.as_ref() == Some(&done.path);
            match done.result {
                Ok(img) => {
                    if self.wanted().contains(&done.path) {
                        self.cache.insert(done.path.clone(), img.clone());
                    }
                    if is_current {
                        self.waiting_for = None;
                        self.present_image(&img);
                    }
                }
                Err(msg) => {
                    if is_current {
                        self.waiting_for = None;
                        self.content = Content::Error(msg);
                        self.dirty = true;
                    }
                }
            }
        }
    }

    fn present_image(&mut self, img: &DecodedImage) {
        let frames: Result<Vec<_>> = std::iter::once((&img.pixels, img.first_delay))
            .chain(img.animation.iter().map(|f| (&f.pixels, f.delay)))
            .map(|(px, delay)| Ok((self.gfx.create_bitmap(img.width, img.height, px)?, delay)))
            .collect();
        self.content = match frames {
            Ok(frames) => Content::Image(Picture {
                next_frame_at: (frames.len() > 1).then(|| Instant::now() + frames[0].1),
                frames,
                frame: 0,
                pixels: (img.width, img.height),
                size: img.source_size,
                orientation: img.orientation,
                has_alpha: img.has_alpha,
                scalable: img.scalable,
            }),
            Err(e) => Content::Error(format!("Couldn't upload image to the GPU: {}", e.message())),
        };
        self.view = View::default();
        self.dirty = true;
    }

    fn set_title(&self, path: &Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(format!("{name} - {APP_NAME}")));
        }
    }

    fn open_dialog(&mut self) {
        let result: Result<PathBuf> = (|| unsafe {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            let images = files::image_extensions();
            let all: Vec<&str> =
                images.iter().map(String::as_str).chain(files::VIDEO_EXTENSIONS.iter().copied()).collect();
            let specs = [
                (HSTRING::from("Photos and videos"), HSTRING::from(files::filter_spec(&all))),
                (HSTRING::from("Photos"), HSTRING::from(files::filter_spec(images))),
                (HSTRING::from("Videos"), HSTRING::from(files::filter_spec(files::VIDEO_EXTENSIONS))),
                (HSTRING::from("All files"), HSTRING::from("*.*")),
            ];
            let filters: Vec<COMDLG_FILTERSPEC> = specs
                .iter()
                .map(|(n, s)| COMDLG_FILTERSPEC { pszName: PCWSTR(n.as_ptr()), pszSpec: PCWSTR(s.as_ptr()) })
                .collect();
            dialog.SetFileTypes(&filters)?;
            dialog.SetTitle(w!("Open a photo or video"))?;
            dialog.Show(Some(self.hwnd))?;
            let item = dialog.GetResult()?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = PathBuf::from(name.to_string().unwrap_or_default());
            CoTaskMemFree(Some(name.0 as _));
            Ok(path)
        })();
        if let Ok(path) = result {
            self.open(path);
        }
    }

    /// Registers Forus and opens its page in Settings → Default apps, where
    /// "Set default" makes it the default for all its types.
    fn make_default(&mut self) {
        match assoc::register() {
            Ok(()) => {
                assoc::open_default_apps_settings(Some(self.hwnd));
                self.toast("In Settings, click \"Set default\" for Forus");
            }
            Err(e) => self.toast(&format!("Couldn't register file types: {}", e.message())),
        }
    }

    fn reveal_in_explorer(&self) {
        let Some(path) = self.current_path() else { return };
        let args = HSTRING::from(format!("/select,\"{}\"", path.display()));
        unsafe {
            ShellExecuteW(Some(self.hwnd), w!("open"), w!("explorer.exe"), &args, None, SW_SHOWNORMAL);
        }
    }

    // ---- view math --------------------------------------------------------

    /// (logical width, height, orientation quarters, mirrored, may upscale)
    fn content_geometry(&self) -> Option<(f32, f32, u8, bool, bool)> {
        let (w, h, (q, flip), upscale) = match &self.content {
            Content::Image(p) => (p.size.0, p.size.1, exif_transform(p.orientation), p.scalable),
            Content::Video(v) => {
                let (_, size) = v.bitmap()?;
                (size.0, size.1, (0, false), true)
            }
            _ => return None,
        };
        Some((w as f32, h as f32, (q + self.view.rotation) % 4, flip, upscale))
    }

    /// Size of the content area (the client area below the title bar).
    fn window_size(&self) -> (f32, f32) {
        let (w, h) = self.gfx.size();
        (w as f32, (h as f32 - self.top()).max(1.0))
    }

    /// Center of the content area in client coordinates.
    fn view_center(&self) -> (f32, f32) {
        let (w, h) = self.window_size();
        (w / 2.0, self.top() + h / 2.0)
    }

    /// Where the content is drawn right now: the target placement, or a point
    /// on the way to it while a zoom animates.
    fn placement(&self) -> Option<Placement> {
        let mut p = self.target_placement()?;
        let Some(a) = self.view.anim else { return Some(p) };
        let t = a.start.elapsed().as_secs_f32() / ZOOM_ANIM.as_secs_f32();
        if t >= 1.0 {
            return Some(p);
        }
        let e = 1.0 - (1.0 - t).powi(3); // ease-out cubic
        let k = p.scale / a.scale;
        let r = k.powf(e); // scale changes geometrically, so speed feels even
        if (k - 1.0).abs() < 1e-3 {
            p.cx = a.cx + (p.cx - a.cx) * e;
            p.cy = a.cy + (p.cy - a.cy) * e;
        } else {
            // Zoom about the one point both placements agree on, so whatever
            // is under the cursor stays under it the whole way.
            let fx = (p.cx - a.cx * k) / (1.0 - k);
            let fy = (p.cy - a.cy * k) / (1.0 - k);
            p.cx = fx + (a.cx - fx) * r;
            p.cy = fy + (a.cy - fy) * r;
        }
        p.scale = a.scale * r;
        Some(p)
    }

    /// Starts gliding from `from` (what was on screen) to the current view.
    fn animate_view_from(&mut self, from: Option<Placement>) {
        self.view.anim = from.map(|p| ZoomAnim { scale: p.scale, cx: p.cx, cy: p.cy, start: Instant::now() });
        // A little extra so the final, settled frame is drawn too.
        self.animate_for(ZOOM_ANIM + Duration::from_millis(50));
        self.dirty = true;
    }

    /// Where the content settles once any zoom animation finishes.
    fn target_placement(&self) -> Option<Placement> {
        let (w, h, q, _, upscale) = self.content_geometry()?;
        let (dw, dh) = if q % 2 == 1 { (h, w) } else { (w, h) };
        let (ww, wh) = self.window_size();
        let mut fit = (ww / dw).min(wh / dh);
        if !upscale {
            fit = fit.min(1.0);
        }
        let scale = self.view.zoom.unwrap_or(fit);
        let (px, py) = self.clamp_pan(self.view.pan, scale, dw, dh);
        let (vx, vy) = self.view_center();
        Some(Placement { scale, fit, cx: vx + px, cy: vy + py, dw, dh })
    }

    fn clamp_pan(&self, pan: (f32, f32), scale: f32, dw: f32, dh: f32) -> (f32, f32) {
        let (ww, wh) = self.window_size();
        let mx = ((dw * scale - ww) / 2.0).max(0.0);
        let my = ((dh * scale - wh) / 2.0).max(0.0);
        (pan.0.clamp(-mx, mx), pan.1.clamp(-my, my))
    }

    fn content_matrix(&self, p: &Placement) -> Option<Matrix3x2> {
        let (w, h, q, flip, _) = self.content_geometry()?;
        let mut m = translation(-w / 2.0, -h / 2.0);
        if flip {
            m = mul(&m, &scaling(-1.0, 1.0));
        }
        m = mul(&m, &quarter_turns(q));
        m = mul(&m, &scaling(p.scale, p.scale));
        Some(mul(&m, &translation(p.cx, p.cy)))
    }

    fn zoom_at(&mut self, factor: f32, x: f32, y: f32) {
        let shown = self.placement();
        let Some(p) = self.target_placement() else { return };
        let mut new = (p.scale * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Snap back to "fit" when zooming out past it.
        if factor < 1.0 && new <= p.fit && p.scale > p.fit {
            new = p.fit;
        }
        let f = new / p.scale;
        let (vx, vy) = self.view_center();
        let (ox, oy) = (x - vx, y - vy);
        let pan = (ox - (ox - (p.cx - vx)) * f, oy - (oy - (p.cy - vy)) * f);
        self.view.zoom = if (new - p.fit).abs() < 1e-4 { None } else { Some(new) };
        self.view.pan = self.clamp_pan(pan, new, p.dw, p.dh);
        self.animate_view_from(shown);
        self.toast(&format!("{:.0}%", new * 100.0));
    }

    fn zoom_center(&mut self, factor: f32) {
        let (x, y) = self.view_center();
        self.zoom_at(factor, x, y);
    }

    fn set_actual_size(&mut self) {
        let shown = self.placement();
        if let Some(p) = self.target_placement() {
            self.view.zoom = if (p.fit - 1.0).abs() < 1e-4 { None } else { Some(1.0) };
            self.view.pan = (0.0, 0.0);
            self.animate_view_from(shown);
            self.toast("100%");
        }
    }

    fn fit(&mut self) {
        let shown = self.placement();
        self.view.zoom = None;
        self.view.pan = (0.0, 0.0);
        self.animate_view_from(shown);
        self.toast("Fit to window");
    }

    fn rotate(&mut self, clockwise: bool) {
        if self.content_geometry().is_none() {
            return;
        }
        self.view.rotation = (self.view.rotation + if clockwise { 1 } else { 3 }) % 4;
        self.view.zoom = None;
        self.view.pan = (0.0, 0.0);
        self.view.anim = None;
        self.dirty = true;
    }

    // ---- UI timing --------------------------------------------------------

    fn poke(&mut self) {
        self.last_input = Instant::now();
        self.dirty = true;
    }

    fn toast(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now() + Duration::from_millis(1200)));
        self.dirty = true;
    }

    fn video(&self) -> Option<&Video> {
        match &self.content {
            Content::Video(v) => Some(v),
            _ => None,
        }
    }

    /// Whether the chrome must stay visible regardless of idle time.
    fn ui_pinned(&self) -> bool {
        self.pin_info
            || self.show_help
            || self.scrub.is_some()
            || self.hover != Hit::None
            || matches!(self.content, Content::Empty | Content::Error(_))
            || self.video().is_some_and(|v| v.is_paused())
    }

    fn ui_alpha(&self, now: Instant) -> f32 {
        if self.ui_pinned() {
            return 1.0;
        }
        let idle = now.duration_since(self.last_input).as_secs_f32();
        (1.0 - (idle - UI_HIDE_AFTER) / UI_FADE).clamp(0.0, 1.0)
    }

    fn animating(&self, now: Instant) -> bool {
        let fading = {
            let a = self.ui_alpha(now);
            a > 0.0 && a < 1.0
        };
        let toast = self.toast.as_ref().is_some_and(|(_, until)| *until + Duration::from_secs_f32(UI_FADE) > now);
        let playing = self.video().is_some_and(|v| !v.is_paused());
        playing || fading || toast || self.animate_until > now || self.waiting_for.is_some()
    }

    /// When the next time-based change (UI fade-out, animation frame) will happen.
    fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        let fade = (!self.ui_pinned() && self.ui_alpha(now) > 0.0)
            .then(|| (self.last_input + Duration::from_secs_f32(UI_HIDE_AFTER)).saturating_duration_since(now));
        let frame = match &self.content {
            Content::Image(p) => p.next_frame_at.map(|t| t.saturating_duration_since(now)),
            _ => None,
        };
        match (fade, frame) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Moves animated images on to the frame due now.
    fn advance_animation(&mut self, now: Instant) {
        if let Content::Image(p) = &mut self.content {
            if p.advance(now) {
                self.dirty = true;
            }
        }
    }

    fn picture_mut(&mut self) -> Option<&mut Picture> {
        match &mut self.content {
            Content::Image(p) if p.is_animated() => Some(p),
            _ => None,
        }
    }

    fn animate_for(&mut self, d: Duration) {
        self.animate_until = self.animate_until.max(Instant::now() + d);
    }

    // ---- rendering --------------------------------------------------------

    fn render(&mut self) {
        self.dirty = false;
        if unsafe { IsIconic(self.hwnd) }.as_bool() {
            return;
        }
        if let Err(e) = self.try_render() {
            if is_device_lost(&e) {
                self.recreate_device();
            }
        }
    }

    fn recreate_device(&mut self) {
        self.content = Content::Empty;
        match Gfx::new(self.hwnd).and_then(|g| g.fonts(self.ui_scale).map(|f| (g, f))) {
            Ok((gfx, fonts)) => {
                self.app_icon = gfx.app_icon((16.0 * self.ui_scale).round() as u32).ok();
                self.gfx = gfx;
                self.fonts = fonts;
                self.show_current();
            }
            Err(e) => self.content = Content::Error(format!("Graphics device lost: {}", e.message())),
        }
    }

    fn try_render(&mut self) -> Result<()> {
        self.gfx.sync_size(self.hwnd)?;
        if let Content::Video(v) = &mut self.content {
            let _ = v.update(&self.gfx);
        }
        let now = Instant::now();
        unsafe {
            self.gfx.ctx.BeginDraw();
            self.gfx.ctx.Clear(Some(&BACKGROUND));
        }
        self.draw_content();
        self.draw_chrome(now);
        self.draw_title_bar();
        unsafe { self.gfx.ctx.EndDraw(None, None)? };
        self.gfx.present()
    }

    fn draw_content(&self) {
        let g = &self.gfx;
        let (bitmap, pixel_w, has_alpha) = match &self.content {
            Content::Image(p) => (p.bitmap(), p.pixels.0 as f32, p.has_alpha),
            Content::Video(v) => match v.bitmap() {
                Some((b, size)) => (b, size.0 as f32, false),
                None => return,
            },
            _ => return,
        };
        let (Some(p), Some((w, h, ..))) = (self.placement(), self.content_geometry()) else { return };
        let Some(m) = self.content_matrix(&p) else { return };

        if has_alpha {
            let (hw, hh) = (p.dw * p.scale / 2.0, p.dh * p.scale / 2.0);
            unsafe { g.ctx.FillRectangle(&rect(p.cx - hw, p.cy - hh, p.cx + hw, p.cy + hh), &g.checker) };
        }

        // Screen pixels per bitmap pixel decides the filter.
        let density = p.scale * w / pixel_w;
        let filter = if density < 1.0 {
            D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
        } else if density >= 4.0 {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            D2D1_INTERPOLATION_MODE_CUBIC
        };
        g.set_transform(&m);
        unsafe {
            g.ctx.DrawBitmap(bitmap, Some(&rect(0.0, 0.0, w, h)), 1.0, filter, None, None);
        }
        g.set_transform(&Matrix3x2::identity());
    }

    fn controls(&self) -> Option<Controls> {
        self.video()?;
        let s = self.ui_scale;
        let (w, h) = self.window_size();
        let h = h + self.top();
        let width = (w - 32.0 * s).min(860.0 * s);
        let left = (w - width) / 2.0;
        let panel = rect(left, h - 16.0 * s - 52.0 * s, left + width, h - 16.0 * s);
        let cy = (panel.top + panel.bottom) / 2.0;
        let r = 17.0 * s;
        let play = (panel.left + 10.0 * s + r, cy, r);
        let mute = (panel.right - 10.0 * s - r, cy, r);
        let time_w = 120.0 * s;
        let time_x = mute.0 - r - 8.0 * s - time_w;
        let bar = rect(play.0 + r + 14.0 * s, cy - 2.0 * s, time_x - 14.0 * s, cy + 2.0 * s);
        Some(Controls { panel, play, mute, bar, time_x })
    }

    fn hit_test(&self, x: f32, y: f32) -> Hit {
        if self.ui_alpha(Instant::now()) < 0.05 {
            return Hit::None;
        }
        let Some(c) = self.controls() else { return Hit::None };
        let inside = |(cx, cy, r): (f32, f32, f32)| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
        if inside(c.play) {
            Hit::Play
        } else if inside(c.mute) {
            Hit::Mute
        } else if x >= c.bar.left - 6.0 && x <= c.bar.right + 6.0 && y >= c.panel.top && y <= c.panel.bottom {
            Hit::Seek
        } else {
            Hit::None
        }
    }

    fn seek_to_x(&mut self, x: f32) {
        let (Some(c), Some(v)) = (self.controls(), self.video()) else { return };
        let Some(d) = v.duration() else { return };
        let t = ((x - c.bar.left) / (c.bar.right - c.bar.left)).clamp(0.0, 1.0) as f64 * d;
        v.seek_fast(t);
        self.scrub = Some(t);
        self.animate_for(Duration::from_millis(500));
        self.dirty = true;
    }

    fn draw_chrome(&self, now: Instant) {
        let g = &self.gfx;
        let s = self.ui_scale;
        let (w, h) = self.window_size();
        let top = self.top();
        let alpha = self.ui_alpha(now);
        let white = |a: f32| color(1.0, 1.0, 1.0, a);
        let panel_bg = |a: f32| color(0.11, 0.11, 0.12, 0.82 * a);

        match &self.content {
            Content::Empty if self.waiting_for.is_none() => {
                let lines = [
                    ("Open a photo or video", &self.fonts.title, 0.95),
                    ("Drop a file here, or press Ctrl+O  ·  F1 for shortcuts", &self.fonts.body, 0.6),
                    ("Press D to make Forus your default photo and video viewer", &self.fonts.body, 0.45),
                ];
                let mut y = top + h / 2.0 - 24.0 * s;
                for (text, font, a) in lines {
                    if let Ok((layout, tw, th)) = g.layout(text, font, w) {
                        g.draw_layout(&layout, (w - tw) / 2.0, y, white(a));
                        y += th + 8.0 * s;
                    }
                }
            }
            Content::Error(msg) => {
                let name = self.current_path().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                let max = (w - 64.0 * s).min(560.0 * s);
                let mut y = top + h / 2.0 - 30.0 * s;
                for (text, font, a) in [
                    (name.unwrap_or_default(), &self.fonts.title, 0.95),
                    (msg.clone(), &self.fonts.body, 0.65),
                ] {
                    if let Ok((layout, tw, th)) = g.layout(&text, font, max) {
                        g.draw_layout(&layout, (w - tw.min(max)) / 2.0, y, white(a));
                        y += th + 8.0 * s;
                    }
                }
            }
            _ => {}
        }

        let loading = self.waiting_for.is_some()
            || self.video().is_some_and(|v| v.bitmap().is_none());
        if loading {
            self.draw_spinner(w / 2.0, top + h / 2.0, now);
        }

        // Info pill (top left).
        if alpha > 0.0 && !self.files.is_empty() {
            let mut parts = Vec::new();
            // The title bar shows name and position; full screen has none.
            if top == 0.0 {
                if let Some(name) = self.current_path().and_then(|p| p.file_name()) {
                    parts.push(name.to_string_lossy().into_owned());
                }
                if self.files.len() > 1 {
                    parts.push(format!("{} / {}", self.index + 1, self.files.len()));
                }
            }
            if let Some((cw, ch, q, ..)) = self.content_geometry() {
                let (cw, ch) = if q % 2 == 1 { (ch, cw) } else { (cw, ch) };
                parts.push(format!("{} × {}", cw as u32, ch as u32));
            }
            if let Some(p) = self.placement() {
                parts.push(format!("{:.0}%", p.scale * 100.0));
            }
            if let Content::Image(p) = &self.content {
                if p.is_animated() {
                    parts.push(format!("frame {} / {}", p.frame + 1, p.frames.len()));
                }
            }
            let text = parts.join("   ·   ");
            if let Ok((layout, tw, th)) = g.layout(&text, &self.fonts.body, w - 64.0 * s) {
                let (x, y) = (16.0 * s, top + 16.0 * s);
                let pad = (12.0 * s, 7.0 * s);
                g.fill_round(rect(x, y, x + tw + pad.0 * 2.0, y + th + pad.1 * 2.0), 8.0 * s, panel_bg(alpha));
                g.draw_layout(&layout, x + pad.0, y + pad.1, white(0.92 * alpha));
            }
        }

        if alpha > 0.0 {
            self.draw_video_controls(alpha);
        }

        // Toast (top center).
        if let Some((text, until)) = &self.toast {
            let remaining = until.saturating_duration_since(now).as_secs_f32();
            let past = now.saturating_duration_since(*until).as_secs_f32();
            let a = if remaining > 0.0 { 1.0 } else { (1.0 - past / UI_FADE).max(0.0) };
            if a > 0.0 {
                if let Ok((layout, tw, th)) = g.layout(text, &self.fonts.title, w) {
                    let pad = (16.0 * s, 9.0 * s);
                    let x = (w - tw) / 2.0 - pad.0;
                    let y = top + 16.0 * s;
                    g.fill_round(rect(x, y, x + tw + pad.0 * 2.0, y + th + pad.1 * 2.0), 10.0 * s, panel_bg(a));
                    g.draw_layout(&layout, x + pad.0, y + pad.1, white(a));
                }
            }
        }

        if self.show_help {
            self.draw_help();
        }
    }

    fn draw_spinner(&self, cx: f32, cy: f32, now: Instant) {
        let s = self.ui_scale;
        let t = now.duration_since(self.epoch).as_secs_f32();
        for i in 0..8 {
            let angle = i as f32 / 8.0 * std::f32::consts::TAU;
            let phase = (t * 1.5 - i as f32 / 8.0).rem_euclid(1.0);
            let a = 0.15 + 0.75 * (1.0 - phase);
            self.gfx.fill_ellipse(
                cx + angle.cos() * 14.0 * s,
                cy + angle.sin() * 14.0 * s,
                2.6 * s,
                color(1.0, 1.0, 1.0, a),
            );
        }
    }

    fn draw_video_controls(&self, alpha: f32) {
        let (Some(c), Some(v)) = (self.controls(), self.video()) else { return };
        let g = &self.gfx;
        let s = self.ui_scale;
        let white = |a: f32| color(1.0, 1.0, 1.0, a * alpha);
        g.fill_round(c.panel, 12.0 * s, color(0.11, 0.11, 0.12, 0.82 * alpha));

        let button = |(cx, cy, r): (f32, f32, f32), glyph: &str, hot: bool| {
            if hot {
                g.fill_ellipse(cx, cy, r, white(0.12));
            }
            g.draw_text(glyph, &self.fonts.icons, rect(cx - r, cy - r, cx + r, cy + r), white(0.95));
        };
        // Segoe Fluent Icons: Play E768, Pause E769, Volume E767, Mute E74F.
        button(c.play, if v.is_paused() { "\u{E768}" } else { "\u{E769}" }, self.hover == Hit::Play);
        button(c.mute, if self.muted { "\u{E74F}" } else { "\u{E767}" }, self.hover == Hit::Mute);

        let duration = v.duration();
        let position = self.scrub.unwrap_or_else(|| v.position());
        let frac = duration.map(|d| (position / d).clamp(0.0, 1.0) as f32).unwrap_or(0.0);
        let hot = self.hover == Hit::Seek || self.scrub.is_some();
        let grow = if hot { 1.0 * s } else { 0.0 };
        let bar = rect(c.bar.left, c.bar.top - grow, c.bar.right, c.bar.bottom + grow);
        let radius = (bar.bottom - bar.top) / 2.0;
        g.fill_round(bar, radius, white(0.25));
        let fx = bar.left + (bar.right - bar.left) * frac;
        if frac > 0.0 {
            let mut accent = ACCENT;
            accent.a = alpha;
            g.fill_round(rect(bar.left, bar.top, fx, bar.bottom), radius, accent);
        }
        if hot {
            g.fill_ellipse(fx, (bar.top + bar.bottom) / 2.0, 7.0 * s, white(1.0));
        }

        let time = match duration {
            Some(d) => format!("{} / {}", fmt_time(position), fmt_time(d)),
            None => fmt_time(position),
        };
        if let Ok((layout, tw, th)) = g.layout(&time, &self.fonts.body, 200.0 * s) {
            let x = c.time_x + (120.0 * s - tw) / 2.0;
            g.draw_layout(&layout, x, (c.panel.top + c.panel.bottom - th) / 2.0, white(0.85));
        }
    }

    fn draw_help(&self) {
        const KEYS: &[(&str, &str)] = &[
            ("Ctrl+O, drop a file", "Open"),
            ("← → / PgUp PgDn", "Previous / next"),
            ("Home / End", "First / last"),
            ("Wheel, + −, ↑ ↓", "Zoom"),
            ("0 or F", "Fit to window"),
            ("1", "Actual size"),
            ("Drag", "Pan"),
            ("R / Shift+R", "Rotate"),
            ("F11, Enter, double-click", "Full screen"),
            ("Space", "Play / pause (video, GIF)"),
            (", / .", "Previous / next GIF frame"),
            ("Shift+← →", "Seek 5 s (Ctrl+Shift: 30 s)"),
            ("↑ ↓ (video)", "Volume"),
            ("M / L", "Mute / loop"),
            ("I", "Keep info visible"),
            ("E", "Show in File Explorer"),
            ("D", "Make Forus your default viewer"),
            ("Esc", "Exit full screen / quit"),
        ];
        let g = &self.gfx;
        let s = self.ui_scale;
        let (w, h) = self.window_size();
        let row_h = 26.0 * s;
        let key_w = 220.0 * s;
        let panel_w = (480.0 * s).min(w - 32.0 * s);
        let panel_h = row_h * KEYS.len() as f32 + 76.0 * s;
        let top = self.top();
        let x = (w - panel_w) / 2.0;
        let y = top + ((h - panel_h) / 2.0).max(16.0 * s);
        g.fill_rect(rect(0.0, top, w, top + h), color(0.0, 0.0, 0.0, 0.45));
        g.fill_round(rect(x, y, x + panel_w, y + panel_h), 14.0 * s, color(0.13, 0.13, 0.14, 0.97));
        g.draw_text(
            "Keyboard shortcuts",
            &self.fonts.title,
            rect(x + 24.0 * s, y + 20.0 * s, x + panel_w, y + 50.0 * s),
            color(1.0, 1.0, 1.0, 0.95),
        );
        let mut ry = y + 58.0 * s;
        for (key, action) in KEYS {
            g.draw_text(key, &self.fonts.body, rect(x + 24.0 * s, ry, x + 24.0 * s + key_w, ry + row_h), ACCENT);
            g.draw_text(
                action,
                &self.fonts.body,
                rect(x + 24.0 * s + key_w, ry, x + panel_w - 16.0 * s, ry + row_h),
                color(1.0, 1.0, 1.0, 0.85),
            );
            ry += row_h;
        }
    }

    // ---- input ------------------------------------------------------------

    fn toggle_fullscreen(&mut self) {
        unsafe {
            let hwnd = self.hwnd;
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            if let Some(placement) = self.fullscreen.take() {
                SetWindowLongW(hwnd, GWL_STYLE, (style | WS_OVERLAPPEDWINDOW.0) as i32);
                let _ = SetWindowPlacement(hwnd, &placement);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            } else {
                let mut placement =
                    WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                if GetWindowPlacement(hwnd, &mut placement).is_ok() && GetMonitorInfoW(mon, &mut mi).as_bool() {
                    SetWindowLongW(hwnd, GWL_STYLE, (style & !WS_OVERLAPPEDWINDOW.0) as i32);
                    let r = mi.rcMonitor;
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOP),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                    );
                    self.fullscreen = Some(placement);
                }
            }
        }
        self.dirty = true;
    }

    fn set_volume(&mut self, delta: f64) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.muted = false;
        if let Some(v) = self.video() {
            v.set_volume(self.volume);
            v.set_muted(false);
        }
        self.toast(&format!("Volume {:.0}%", self.volume * 100.0));
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(v) = self.video() {
            v.set_muted(self.muted);
        }
        self.toast(if self.muted { "Muted" } else { "Sound on" });
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY) {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let is_video = self.video().is_some();
        self.poke();

        if self.show_help && vk != VK_F11 {
            self.show_help = false;
            if matches!(vk, VK_ESCAPE | VK_F1 | VK_H) {
                return;
            }
        }

        match vk {
            VK_O if ctrl => self.open_dialog(),
            VK_W if ctrl => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            VK_LEFT | VK_RIGHT if shift && is_video => {
                let step = if ctrl { 30.0 } else { 5.0 };
                let dir = if vk == VK_LEFT { -1.0 } else { 1.0 };
                if let Some(v) = self.video() {
                    v.seek(v.position() + dir * step);
                }
                self.animate_for(Duration::from_millis(500));
            }
            VK_LEFT | VK_PRIOR | VK_BROWSER_BACK => self.go(-1),
            VK_RIGHT | VK_NEXT | VK_BROWSER_FORWARD => self.go(1),
            VK_HOME => self.go_to(0),
            VK_END => self.go_to(self.files.len().saturating_sub(1)),
            VK_SPACE => match self.video() {
                Some(v) => {
                    v.toggle_play();
                    self.animate_for(Duration::from_millis(300));
                }
                None => match self.picture_mut() {
                    Some(p) => {
                        let paused = p.toggle_pause();
                        self.toast(if paused { "Paused" } else { "Playing" });
                    }
                    None => self.go(1),
                },
            },
            VK_OEM_COMMA | VK_OEM_PERIOD => {
                if let Some(p) = self.picture_mut() {
                    p.step(if vk == VK_OEM_COMMA { -1 } else { 1 });
                    let text = format!("Frame {} / {}", p.frame + 1, p.frames.len());
                    self.toast(&text);
                }
            }
            VK_UP if is_video => self.set_volume(0.05),
            VK_DOWN if is_video => self.set_volume(-0.05),
            VK_UP | VK_OEM_PLUS | VK_ADD => self.zoom_center(1.25),
            VK_DOWN | VK_OEM_MINUS | VK_SUBTRACT => self.zoom_center(0.8),
            VK_0 | VK_NUMPAD0 | VK_F => self.fit(),
            VK_1 | VK_NUMPAD1 => self.set_actual_size(),
            VK_R => self.rotate(!shift),
            VK_M => self.toggle_mute(),
            VK_L => {
                if let Some(v) = self.video() {
                    let on = !v.looping();
                    v.set_looping(on);
                    self.toast(if on { "Loop on" } else { "Loop off" });
                }
            }
            VK_I | VK_TAB => {
                self.pin_info = !self.pin_info;
                self.toast(if self.pin_info { "Info stays visible" } else { "Info auto-hides" });
            }
            VK_E => self.reveal_in_explorer(),
            VK_D => self.make_default(),
            VK_F1 | VK_H | VK_OEM_2 => self.show_help = true,
            VK_F11 | VK_RETURN => self.toggle_fullscreen(),
            VK_ESCAPE => {
                if self.fullscreen.is_some() {
                    self.toggle_fullscreen();
                } else {
                    unsafe {
                        let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            _ => {}
        }
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let pt = || (get_x(lparam) as f32, get_y(lparam) as f32);
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe { BeginPaint(self.hwnd, &mut ps) };
                self.render();
                unsafe {
                    let _ = EndPaint(self.hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                self.dirty = true;
                if wparam.0 as u32 != SIZE_MINIMIZED {
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_NCHITTEST => Some(self.nc_hit_test(lparam)),
            WM_ACTIVATE => {
                self.active = (wparam.0 & 0xffff) as u32 != WA_INACTIVE;
                self.dirty = true;
                None
            }
            WM_NCMOUSEMOVE => {
                let hover = Self::caption_button_for(wparam.0).map(TitleHit::Button);
                if hover != self.title_hover {
                    self.title_hover = hover;
                    self.dirty = true;
                }
                if !self.tracking_nc_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    self.tracking_nc_leave = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
                }
                None
            }
            WM_NCMOUSELEAVE => {
                self.tracking_nc_leave = false;
                if matches!(self.title_hover, Some(TitleHit::Button(_))) || self.title_pressed.is_some() {
                    self.title_hover = None;
                    self.title_pressed = None;
                    self.dirty = true;
                }
                None
            }
            // Caption buttons are ours to draw; stop Windows drawing classic
            // ones, and act on release like native buttons do.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
                let button = Self::caption_button_for(wparam.0)?;
                self.title_pressed = Some(TitleHit::Button(button));
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_NCLBUTTONUP => {
                let button = Self::caption_button_for(wparam.0);
                let pressed = self.title_pressed.take();
                self.dirty = true;
                match button {
                    Some(b) => {
                        if pressed == Some(TitleHit::Button(b)) {
                            self.title_hover = None;
                            self.press_caption_button(b);
                        }
                        Some(LRESULT(0))
                    }
                    None => None,
                }
            }
            WM_ENTERSIZEMOVE => {
                // Keep video running while Windows runs its modal move loop.
                unsafe { SetTimer(Some(self.hwnd), TIMER_SIZEMOVE, 15, None) };
                None
            }
            WM_EXITSIZEMOVE => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_SIZEMOVE);
                }
                None
            }
            WM_TIMER if wparam.0 == TIMER_SIZEMOVE => {
                self.advance_animation(Instant::now());
                if self.dirty || self.animating(Instant::now()) {
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.ui_scale = (wparam.0 & 0xffff) as f32 / 96.0;
                if let Ok(f) = self.gfx.fonts(self.ui_scale) {
                    self.fonts = f;
                }
                self.app_icon = self.gfx.app_icon((16.0 * self.ui_scale).round() as u32).ok();
                let r = unsafe { &*(lparam.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = VIRTUAL_KEY(wparam.0 as u16);
                // Let Alt+F4 and friends through.
                if msg == WM_SYSKEYDOWN && vk != VK_F10 && vk != VK_RETURN {
                    return None;
                }
                self.on_key(vk);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
                let mut p = POINT { x: get_x(lparam), y: get_y(lparam) };
                unsafe {
                    let _ = ScreenToClient(self.hwnd, &mut p);
                }
                self.poke();
                if self.hit_test(p.x as f32, p.y as f32) == Hit::Mute {
                    self.set_volume(0.05 * delta as f64);
                } else {
                    self.zoom_at(1.2f32.powf(delta), p.x as f32, p.y as f32);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = pt();
                self.poke();
                unsafe { SetCapture(self.hwnd) };
                if let Some(hit @ TitleHit::Tool(_)) = self.title_hit(x, y) {
                    self.title_pressed = Some(hit);
                    self.dirty = true;
                    return Some(LRESULT(0));
                }
                if self.show_help {
                    self.show_help = false;
                    return Some(LRESULT(0));
                }
                match self.hit_test(x, y) {
                    Hit::Play => {
                        if let Some(v) = self.video() {
                            v.toggle_play();
                        }
                        self.animate_for(Duration::from_millis(300));
                    }
                    Hit::Mute => self.toggle_mute(),
                    Hit::Seek => self.seek_to_x(x),
                    Hit::None => self.drag = Some(Drag { start: (x, y), pan: self.placement_pan() }),
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = pt();
                if let Some(TitleHit::Tool(tool)) = self.title_hit(x, y) {
                    // A fast second click on a tool is just another click.
                    self.run_tool(tool);
                } else if self.hit_test(x, y) == Hit::None && y >= self.top() {
                    self.toggle_fullscreen();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = pt();
                let was_hidden = self.ui_alpha(Instant::now()) == 0.0;
                self.last_input = Instant::now();
                if !self.tracking_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    self.tracking_leave = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
                }
                if self.scrub.is_some() {
                    self.seek_to_x(x);
                } else if let Some(d) = &self.drag {
                    if self.view.zoom.is_some() {
                        self.view.pan = (d.pan.0 + x - d.start.0, d.pan.1 + y - d.start.1);
                        if let Some(p) = self.target_placement() {
                            self.view.pan = self.clamp_pan(self.view.pan, p.scale, p.dw, p.dh);
                        }
                        self.dirty = true;
                    }
                }
                let title_hover = self.title_hit(x, y).filter(|h| matches!(h, TitleHit::Tool(_)));
                if title_hover != self.title_hover {
                    self.title_hover = title_hover;
                    self.dirty = true;
                }
                let hover = self.hit_test(x, y);
                if hover != self.hover || was_hidden {
                    self.hover = hover;
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                if self.hover != Hit::None || matches!(self.title_hover, Some(TitleHit::Tool(_))) {
                    self.hover = Hit::None;
                    self.title_hover = None;
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                if let Some(TitleHit::Tool(tool)) = self.title_pressed.take() {
                    let (x, y) = pt();
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    if self.title_hit(x, y) == Some(TitleHit::Tool(tool)) {
                        self.run_tool(tool);
                    }
                    self.dirty = true;
                    return Some(LRESULT(0));
                }
                self.drag = None;
                if let (Some(t), Some(v)) = (self.scrub.take(), self.video()) {
                    v.seek(t); // land precisely where the scrub ended
                }
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_XBUTTONDOWN => {
                let button = ((wparam.0 >> 16) & 0xffff) as u16;
                self.go(if button == XBUTTON1 { -1 } else { 1 });
                Some(LRESULT(1))
            }
            WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
                let cursor = if self.fullscreen.is_some() && self.ui_alpha(Instant::now()) == 0.0 {
                    None
                } else if self.title_hover.is_some() {
                    Some(IDC_ARROW)
                } else if self.hover != Hit::None {
                    Some(IDC_HAND)
                } else if self.view.zoom.is_some() && self.content_geometry().is_some() {
                    Some(if self.drag.is_some() { IDC_SIZEALL } else { IDC_HAND })
                } else {
                    Some(IDC_ARROW)
                };
                unsafe {
                    let handle = cursor.and_then(|c| LoadCursorW(None, c).ok());
                    SetCursor(handle);
                }
                Some(LRESULT(1))
            }
            WM_DROPFILES => {
                let drop = HDROP(wparam.0 as _);
                let needed = unsafe { DragQueryFileW(drop, 0, None) } as usize;
                let mut buf = vec![0u16; needed + 1];
                let len = unsafe { DragQueryFileW(drop, 0, Some(&mut buf)) } as usize;
                unsafe { DragFinish(drop) };
                if len > 0 {
                    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
                    unsafe {
                        let _ = SetForegroundWindow(self.hwnd);
                    }
                    self.open(path);
                }
                Some(LRESULT(0))
            }
            WM_APP_DECODED => {
                self.on_decoded();
                self.prefetch();
                Some(LRESULT(0))
            }
            WM_APP_MEDIA => {
                self.on_media(lparam);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// Hides the cursor once the chrome fades out in full screen.
    fn update_cursor_visibility(&mut self) {
        let hide = self.fullscreen.is_some() && self.drag.is_none() && self.ui_alpha(Instant::now()) == 0.0;
        if hide && !self.cursor_hidden {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
                if WindowFromPoint(p) == self.hwnd {
                    SetCursor(None);
                    self.cursor_hidden = true;
                }
            }
        } else if !hide {
            self.cursor_hidden = false;
        }
    }

    /// The pan actually in effect (after clamping).
    fn placement_pan(&self) -> (f32, f32) {
        match self.target_placement() {
            Some(p) => {
                let (vx, vy) = self.view_center();
                (p.cx - vx, p.cy - vy)
            }
            None => (0.0, 0.0),
        }
    }

    fn on_media(&mut self, lparam: LPARAM) {
        let (generation, event) = video::unpack_event(lparam);
        let Content::Video(v) = &self.content else { return };
        if v.generation != generation {
            return; // stale event from a previous video
        }
        match MF_MEDIA_ENGINE_EVENT(event as i32) {
            MF_MEDIA_ENGINE_EVENT_ERROR => {
                let msg = v.error().unwrap_or_else(|| "The video couldn't be played.".into());
                self.content = Content::Error(msg);
            }
            MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA | MF_MEDIA_ENGINE_EVENT_FORMATCHANGE => {
                self.view = View::default();
            }
            _ => {}
        }
        self.animate_for(Duration::from_millis(500));
        self.dirty = true;
    }
}

// ---- helpers ---------------------------------------------------------------

fn get_x(lparam: LPARAM) -> i32 {
    (lparam.0 & 0xffff) as u16 as i16 as i32
}

fn get_y(lparam: LPARAM) -> i32 {
    ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32
}

fn fmt_time(seconds: f64) -> String {
    let t = seconds.max(0.0) as u64;
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// EXIF orientation → (clockwise quarter turns, mirror before turning).
fn exif_transform(orientation: u16) -> (u8, bool) {
    match orientation {
        2 => (0, true),
        3 => (2, false),
        4 => (2, true),
        5 => (3, true),
        6 => (1, false),
        7 => (1, true),
        8 => (3, false),
        _ => (0, false),
    }
}

fn translation(x: f32, y: f32) -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: x, M32: y }
}

fn scaling(x: f32, y: f32) -> Matrix3x2 {
    Matrix3x2 { M11: x, M12: 0.0, M21: 0.0, M22: y, M31: 0.0, M32: 0.0 }
}

/// Clockwise rotation by `q` quarter turns in y-down screen space.
fn quarter_turns(q: u8) -> Matrix3x2 {
    let (c, s) = match q % 4 {
        0 => (1.0, 0.0),
        1 => (0.0, 1.0),
        2 => (-1.0, 0.0),
        _ => (0.0, -1.0),
    };
    Matrix3x2 { M11: c, M12: s, M21: -s, M22: c, M31: 0.0, M32: 0.0 }
}

/// `a` then `b` (Direct2D uses row vectors).
fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}
