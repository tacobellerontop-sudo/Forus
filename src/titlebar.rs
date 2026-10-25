//! The custom title bar: app icon, file name, a few tool buttons and
//! Windows 11-style caption buttons, all drawn with Direct2D.
//!
//! The window keeps its native frame (resize borders, shadow, rounded corners,
//! Snap); only the caption is replaced. Hit-testing reports the caption
//! buttons to Windows as HTMINBUTTON/HTMAXBUTTON/HTCLOSE, which is what makes
//! the Snap Layouts flyout appear when hovering the maximize button.

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap1, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC};

use crate::gfx::{color, rect, Fonts, Gfx};

const HEIGHT: f32 = 32.0;
const CAPTION_BUTTON_W: f32 = 46.0;
const TOOL_W: f32 = 40.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Minimize,
    Maximize,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Open,
    Previous,
    Next,
    Rotate,
    Fit,
    FullScreen,
    Help,
}

impl Tool {
    fn glyph(self) -> &'static str {
        // Segoe Fluent Icons / MDL2 code points.
        match self {
            Tool::Open => "\u{E8E5}",
            Tool::Previous => "\u{E76B}",
            Tool::Next => "\u{E76C}",
            Tool::Rotate => "\u{E7AD}",
            Tool::Fit => "\u{E9A6}",
            Tool::FullScreen => "\u{E740}",
            Tool::Help => "\u{E897}",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Open => "Open (Ctrl+O)",
            Tool::Previous => "Previous (←)",
            Tool::Next => "Next (→)",
            Tool::Rotate => "Rotate (R)",
            Tool::Fit => "Fit to window (F)",
            Tool::FullScreen => "Full screen (F11)",
            Tool::Help => "Keyboard shortcuts (F1)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TitleHit {
    /// Empty title bar: drags the window.
    Caption,
    Button(Button),
    Tool(Tool),
}

pub struct TitleBar {
    pub height: f32,
    scale: f32,
    buttons: Vec<(D2D_RECT_F, Button)>,
    tools: Vec<(D2D_RECT_F, Tool)>,
    title_right: f32,
}

impl TitleBar {
    pub fn layout(width: f32, scale: f32) -> Self {
        let height = (HEIGHT * scale).round();
        let bw = (CAPTION_BUTTON_W * scale).round();
        let mut x = width;
        let mut buttons = Vec::new();
        for b in [Button::Close, Button::Maximize, Button::Minimize] {
            buttons.push((rect(x - bw, 0.0, x, height), b));
            x -= bw;
        }
        x -= 8.0 * scale;
        let tw = (TOOL_W * scale).round();
        let mut tools = Vec::new();
        let groups: [&[Tool]; 3] = [
            &[Tool::Help, Tool::FullScreen],
            &[Tool::Fit, Tool::Rotate],
            &[Tool::Next, Tool::Previous, Tool::Open],
        ];
        // Drop tool groups that don't fit, keeping the caption draggable.
        for group in groups {
            if x - tw * (group.len() as f32) < 220.0 * scale {
                break;
            }
            for &t in group {
                tools.push((rect(x - tw, 0.0, x, height), t));
                x -= tw;
            }
            x -= 6.0 * scale;
        }
        Self { height, scale, buttons, tools, title_right: x }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<TitleHit> {
        if y < 0.0 || y >= self.height {
            return None;
        }
        let inside = |r: &D2D_RECT_F| x >= r.left && x < r.right;
        if let Some((_, b)) = self.buttons.iter().find(|(r, _)| inside(r)) {
            return Some(TitleHit::Button(*b));
        }
        if let Some((_, t)) = self.tools.iter().find(|(r, _)| inside(r)) {
            return Some(TitleHit::Tool(*t));
        }
        Some(TitleHit::Caption)
    }

    fn tool_rect(&self, tool: Tool) -> Option<D2D_RECT_F> {
        self.tools.iter().find(|(_, t)| *t == tool).map(|(r, _)| *r)
    }
}

pub struct TitleState<'a> {
    pub title: &'a str,
    pub icon: Option<&'a ID2D1Bitmap1>,
    pub active: bool,
    pub maximized: bool,
    pub hover: Option<TitleHit>,
    pub pressed: Option<TitleHit>,
    pub enabled: &'a dyn Fn(Tool) -> bool,
}

pub fn draw(g: &Gfx, fonts: &Fonts, bar: &TitleBar, width: f32, st: &TitleState) {
    let s = bar.scale;
    let fg = if st.active { 0.92 } else { 0.45 };
    g.fill_rect(rect(0.0, 0.0, width, bar.height), color(0.125, 0.125, 0.133, 1.0));

    // Icon and title.
    let icon = 16.0 * s;
    let mut x = 14.0 * s;
    if let Some(bmp) = st.icon {
        let y = ((bar.height - icon) / 2.0).round();
        unsafe {
            g.ctx.DrawBitmap(
                bmp,
                Some(&rect(x, y, x + icon, y + icon)),
                if st.active { 1.0 } else { 0.6 },
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
        x += icon + 12.0 * s;
    }
    let max_w = (bar.title_right - x - 8.0 * s).max(0.0);
    if max_w > 0.0 {
        if let Ok((layout, _, th)) = g.layout(st.title, &fonts.caption_text, max_w) {
            unsafe {
                use windows::Win32::Graphics::DirectWrite::*;
                let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                if let Ok(sign) = g.dwrite.CreateEllipsisTrimmingSign(&fonts.caption_text) {
                    let _ = layout.SetTrimming(&trimming, &sign);
                }
            }
            g.draw_layout(&layout, x, ((bar.height - th) / 2.0).round(), color(1.0, 1.0, 1.0, fg));
        }
    }

    // Tool buttons.
    for (r, tool) in &bar.tools {
        let enabled = (st.enabled)(*tool);
        let hit = Some(TitleHit::Tool(*tool));
        if enabled && (st.hover == hit || st.pressed == hit) {
            let a = if st.pressed == hit { 0.05 } else { 0.08 };
            let inset = 4.0 * s;
            g.fill_round(rect(r.left + 2.0 * s, inset, r.right - 2.0 * s, bar.height - inset), 4.0 * s, color(1.0, 1.0, 1.0, a));
        }
        let a = if enabled { fg } else { fg * 0.35 };
        g.draw_text(tool.glyph(), &fonts.icons, *r, color(1.0, 1.0, 1.0, a));
    }

    // Caption buttons.
    for (r, b) in &bar.buttons {
        let hit = Some(TitleHit::Button(*b));
        let hot = st.hover == hit || st.pressed == hit;
        let mut glyph_alpha = fg;
        if hot {
            let bg = match (b, st.pressed == hit) {
                (Button::Close, false) => color(0.769, 0.169, 0.110, 1.0),
                (Button::Close, true) => color(0.769, 0.169, 0.110, 0.9),
                (_, false) => color(1.0, 1.0, 1.0, 0.06),
                (_, true) => color(1.0, 1.0, 1.0, 0.04),
            };
            g.fill_rect(*r, bg);
            glyph_alpha = 1.0;
        }
        let glyph = match b {
            Button::Minimize => "\u{E921}",
            Button::Maximize if st.maximized => "\u{E923}",
            Button::Maximize => "\u{E922}",
            Button::Close => "\u{E8BB}",
        };
        g.draw_text(glyph, &fonts.caption_buttons, *r, color(1.0, 1.0, 1.0, glyph_alpha));
    }

    // Tooltip under a hovered tool.
    if let Some(TitleHit::Tool(tool)) = st.hover {
        if let (Some(r), Ok((layout, tw, th))) = (bar.tool_rect(tool), g.layout(tool.label(), &fonts.body, width)) {
            let pad = (8.0 * s, 5.0 * s);
            let w = tw + pad.0 * 2.0;
            let left = ((r.left + r.right) / 2.0 - w / 2.0).clamp(4.0 * s, width - w - 4.0 * s);
            let top = bar.height + 6.0 * s;
            g.fill_round(rect(left, top, left + w, top + th + pad.1 * 2.0), 4.0 * s, color(0.17, 0.17, 0.18, 0.97));
            g.draw_layout(&layout, left + pad.0, top + pad.1, color(1.0, 1.0, 1.0, 0.9));
        }
    }
}
