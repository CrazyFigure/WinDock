//! Dock 悬停窗口预览浮窗：基于 DWM Thumbnail GPU 零拷贝合成实时预览窗口。
//!
//! 使用非分层 `WS_POPUP` 窗口配合 Win11 原生圆角与深浅模式适配；
//! 隐藏或关闭时立即注销全部缩略图句柄并释放 DIB 缓冲。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
    DwmSetWindowAttribute, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, InvalidateRect, HFONT, HGDIOBJ, PAINTSTRUCT,
    SRCCOPY,
};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::dock::icons::{IconCache, DOCK_ICON_SRC_SIZE};
use crate::ui::*;
use crate::win32::{dp, get_x_lparam, get_y_lparam, wide_static, MonitorInfo};

pub const CLASS_PREVIEW: &str = "WinDockFinderDockPreview";

static REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct CardItem {
    hwnd: isize,
    title: String,
    minimized: bool,
    card_rect: RECT,
    close_rect: RECT,
    thumb_rect: RECT,
    thumb_handle: isize,
}

pub enum PreviewClick {
    None,
    Activate(isize),
    Close(isize),
}

pub struct WindowPreview {
    pub hwnd: HWND,
    pub visible: bool,
    pub entry_idx: Option<usize>,
    pub mouse_in: bool,
    dpi: u32,
    w: i32,
    h: i32,
    dark: bool,
    accent: (f32, f32, f32),
    exe_key: String,
    cards: Vec<CardItem>,
    hover_card: Option<usize>,
    hover_close: Option<usize>,
    font: HFONT,
    font_sub: HFONT,
    target: DibTarget,
}

impl Default for WindowPreview {
    fn default() -> Self {
        Self {
            hwnd: HWND::default(),
            visible: false,
            entry_idx: None,
            mouse_in: false,
            dpi: 96,
            w: 0,
            h: 0,
            dark: false,
            accent: (0.04, 0.52, 1.0),
            exe_key: String::new(),
            cards: Vec::new(),
            hover_card: None,
            hover_close: None,
            font: HFONT::default(),
            font_sub: HFONT::default(),
            target: DibTarget::default(),
        }
    }
}

fn ensure_class() {
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(preview_proc),
            hInstance: crate::win32::hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: wide_static(CLASS_PREVIEW),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

impl WindowPreview {
    fn ensure_window(&mut self, app_ptr: *mut crate::app::App, dpi: u32) -> bool {
        if !self.hwnd.0.is_null() && unsafe { IsWindow(self.hwnd).as_bool() } {
            if self.dpi != dpi {
                self.dpi = dpi;
                self.rebuild_fonts();
            }
            return true;
        }
        ensure_class();
        self.dpi = dpi;
        self.rebuild_fonts();
        unsafe {
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                wide_static(CLASS_PREVIEW),
                w!(""),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                crate::win32::hinstance(),
                None,
            ) else {
                return false;
            };
            let pref = DWMWCP_ROUND.0 as u32;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const u32 as *const _, 4);
            let _ = DwmExtendFrameIntoClientArea(
                hwnd,
                &MARGINS {
                    cxLeftWidth: 0,
                    cxRightWidth: 0,
                    cyTopHeight: 0,
                    cyBottomHeight: 1,
                },
            );
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
            self.hwnd = hwnd;
            true
        }
    }

    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        self.font = make_font(dp(12, self.dpi), 400);
        self.font_sub = make_font(dp(11, self.dpi), 400);
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [self.font, self.font_sub] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font = HFONT::default();
        self.font_sub = HFONT::default();
    }

    fn clear_thumbs(&mut self) {
        for c in &mut self.cards {
            if c.thumb_handle != 0 {
                unsafe {
                    let _ = DwmUnregisterThumbnail(c.thumb_handle);
                }
                c.thumb_handle = 0;
            }
        }
    }

    /// 打开或更新某个 Dock 条目的窗口缩略图浮窗。
    pub fn show_for(
        &mut self,
        app_ptr: *mut crate::app::App,
        entry_idx: usize,
        exe_key: &str,
        windows: &[(isize, String)],
        anchor_cx: i32,
        anchor_top: i32,
        mon: MonitorInfo,
        appearance: &Appearance,
        dock_dark: bool,
    ) {
        if windows.is_empty() {
            self.close();
            return;
        }
        if !self.ensure_window(app_ptr, mon.dpi) {
            return;
        }
        self.clear_thumbs();
        self.cards.clear();
        self.entry_idx = Some(entry_idx);
        self.exe_key = exe_key.to_string();
        self.dark = dock_dark;
        self.accent = accent_from_config(appearance);
        self.hover_card = None;
        self.hover_close = None;

        unsafe {
            let dark_val = u32::from(self.dark);
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark_val as *const u32 as *const _,
                4,
            );
        }

        let d = mon.dpi;
        let count = windows.len().min(5);
        let pad = dp(10, d);
        let gap = dp(8, d);
        let max_avail_w = (mon.work.right - mon.work.left - dp(24, d)).max(dp(240, d));
        let card_w = ((max_avail_w - pad * 2 - gap * (count as i32 - 1).max(0)) / count as i32)
            .clamp(dp(136, d), dp(208, d));
        let card_h = dp(148, d);
        let title_h = dp(28, d);
        let w = pad * 2 + count as i32 * card_w + (count as i32 - 1).max(0) * gap;
        let h = pad * 2 + card_h;

        self.w = w;
        self.h = h;

        for (i, (hwnd_raw, title)) in windows.iter().take(count).enumerate() {
            let cx0 = pad + i as i32 * (card_w + gap);
            let cy0 = pad;
            let card_rect = RECT {
                left: cx0,
                top: cy0,
                right: cx0 + card_w,
                bottom: cy0 + card_h,
            };
            let close_sz = dp(18, d);
            let close_rect = RECT {
                left: card_rect.right - dp(6, d) - close_sz,
                top: card_rect.top + (title_h - close_sz) / 2,
                right: card_rect.right - dp(6, d),
                bottom: card_rect.top + (title_h + close_sz) / 2,
            };
            let thumb_rect = RECT {
                left: card_rect.left + dp(6, d),
                top: card_rect.top + title_h,
                right: card_rect.right - dp(6, d),
                bottom: card_rect.bottom - dp(6, d),
            };
            let src = HWND(*hwnd_raw as *mut _);
            let minimized = unsafe { IsIconic(src).as_bool() };
            let mut thumb_handle = 0isize;
            // 对所有窗口（含最小化窗口）尝试注册 DWM 缩略图，只有源尺寸确实不可用时才回退
            if let Ok(th) = unsafe { DwmRegisterThumbnail(self.hwnd, src) } {
                if let Ok(sz) = unsafe { DwmQueryThumbnailSourceSize(th) } {
                    if sz.cx > 1 && sz.cy > 1 {
                        thumb_handle = th;
                    } else {
                        unsafe {
                            let _ = DwmUnregisterThumbnail(th);
                        }
                    }
                } else {
                    unsafe {
                        let _ = DwmUnregisterThumbnail(th);
                    }
                }
            }
            self.cards.push(CardItem {
                hwnd: *hwnd_raw,
                title: title.clone(),
                minimized,
                card_rect,
                close_rect,
                thumb_rect,
                thumb_handle,
            });
        }

        let left = (anchor_cx - w / 2).clamp(mon.work.left + 8, (mon.work.right - w - 8).max(mon.work.left + 8));
        let top = (anchor_top - h - dp(6, d)).max(mon.work.top + 8);

        self.visible = true;
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, left, top, w, h, SWP_NOACTIVATE | SWP_NOSENDCHANGING);
            self.sync_thumbnails();
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn sync_thumbnails(&self) {
        for c in &self.cards {
            if c.thumb_handle == 0 {
                continue;
            }
            let avail_w = (c.thumb_rect.right - c.thumb_rect.left).max(1);
            let avail_h = (c.thumb_rect.bottom - c.thumb_rect.top).max(1);
            let dest = if let Ok(src_sz) = unsafe { DwmQueryThumbnailSourceSize(c.thumb_handle) } {
                if src_sz.cx > 0 && src_sz.cy > 0 {
                    let k = (avail_w as f32 / src_sz.cx as f32).min(avail_h as f32 / src_sz.cy as f32);
                    let dw = (src_sz.cx as f32 * k).round().max(1.0) as i32;
                    let dh = (src_sz.cy as f32 * k).round().max(1.0) as i32;
                    let dl = c.thumb_rect.left + (avail_w - dw) / 2;
                    let dt = c.thumb_rect.top + (avail_h - dh) / 2;
                    RECT {
                        left: dl,
                        top: dt,
                        right: dl + dw,
                        bottom: dt + dh,
                    }
                } else {
                    c.thumb_rect
                }
            } else {
                c.thumb_rect
            };
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
                rcDestination: dest,
                opacity: 255,
                fVisible: true.into(),
                ..Default::default()
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(c.thumb_handle, &props);
            }
        }
    }

    pub fn close(&mut self) {
        self.clear_thumbs();
        self.cards.clear();
        self.entry_idx = None;
        self.mouse_in = false;
        self.hover_card = None;
        self.hover_close = None;
        if self.visible {
            self.visible = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.target.release();
    }

    pub fn destroy(&mut self) {
        self.close();
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
        self.destroy_fonts();
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.mouse_in = true;
        let mut hc = None;
        let mut hcl = None;
        for (i, c) in self.cards.iter().enumerate() {
            if x >= c.card_rect.left && x < c.card_rect.right && y >= c.card_rect.top && y < c.card_rect.bottom {
                hc = Some(i);
                if x >= c.close_rect.left && x < c.close_rect.right && y >= c.close_rect.top && y < c.close_rect.bottom {
                    hcl = Some(i);
                }
                break;
            }
        }
        if hc != self.hover_card || hcl != self.hover_close {
            self.hover_card = hc;
            self.hover_close = hcl;
            unsafe {
                let _ = InvalidateRect(self.hwnd, None, false);
            }
        }
    }

    pub fn on_mouse_leave(&mut self) {
        self.mouse_in = false;
        self.hover_card = None;
        self.hover_close = None;
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    pub fn on_lbutton_up(&mut self, x: i32, y: i32) -> PreviewClick {
        for c in &self.cards {
            if x >= c.close_rect.left && x < c.close_rect.right && y >= c.close_rect.top && y < c.close_rect.bottom {
                return PreviewClick::Close(c.hwnd);
            }
            if x >= c.card_rect.left && x < c.card_rect.right && y >= c.card_rect.top && y < c.card_rect.bottom {
                return PreviewClick::Activate(c.hwnd);
            }
        }
        PreviewClick::None
    }

    pub fn paint(&mut self, icons: &IconCache) {
        if !self.visible || self.w <= 0 || self.h <= 0 {
            return;
        }
        if !self.target.ensure_at_least(self.w, self.h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else { return };
        let Some(mut sf) = self.target.surface() else { return };
        let s = self.dpi as f32 / 96.0;
        let bg = if self.dark { rgb(0x202227, 1.0) } else { rgb(0xF3F5F9, 1.0) };
        let card = if self.dark { rgb(0x2B2E36, 1.0) } else { rgb(0xFFFFFF, 1.0) };
        let card_hov = if self.dark { rgb(0x353944, 1.0) } else { rgb(0xE9EEF6, 1.0) };
        let bd = if self.dark { (1.0, 1.0, 1.0, 0.14) } else { (0.0, 0.0, 0.0, 0.10) };
        let fg = if self.dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) };
        let fg_dim = if self.dark { rgb(0x98989D, 1.0) } else { rgb(0x6E6E73, 1.0) };
        let acc = (self.accent.0, self.accent.1, self.accent.2, 0.92);

        sf.clear(bg);

        for (i, c) in self.cards.iter().enumerate() {
            let hov = self.hover_card == Some(i);
            sf.fill_rrect(&c.card_rect, 9.0 * s, if hov { card_hov } else { card });
            sf.stroke_rrect(&c.card_rect, 9.0 * s, if hov { 1.4 } else { 1.0 }, if hov { acc } else { bd });

            let trc = RECT {
                left: c.card_rect.left + dp(8, self.dpi),
                top: c.card_rect.top + dp(3, self.dpi),
                right: c.close_rect.left - dp(4, self.dpi),
                bottom: c.card_rect.top + dp(27, self.dpi),
            };
            draw_text(dc, &c.title, self.font, &trc, fg, TextAlign::Left, true, false);

            // 关闭按钮
            let cl_hov = self.hover_close == Some(i);
            let cx = (c.close_rect.left + c.close_rect.right) as f32 * 0.5;
            let cy = (c.close_rect.top + c.close_rect.bottom) as f32 * 0.5;
            if cl_hov {
                sf.fill_circle(cx, cy, 8.0 * s, rgb(0xFF453A, 0.95));
            }
            let a = 3.3 * s;
            let xcol = if cl_hov { rgb(0xFFFFFF, 1.0) } else { fg_dim };
            sf.thick_line(cx - a, cy - a, cx + a, cy + a, 1.4 * s, xcol);
            sf.thick_line(cx - a, cy + a, cx + a, cy - a, 1.4 * s, xcol);

            // 无缩略图回退：绘制应用高清图标与标题
            if c.thumb_handle == 0 {
                let icx = (c.thumb_rect.left + c.thumb_rect.right) as f32 * 0.5;
                let icy = (c.thumb_rect.top + c.thumb_rect.bottom) as f32 * 0.5 - 8.0 * s;
                if let Some(px) = icons.pixels(&self.exe_key, DOCK_ICON_SRC_SIZE) {
                    sf.draw_icon_bilinear(&px, icx, icy, 36.0 * s, false, 0.0);
                }
                let hint = if c.minimized { "已最小化 · 点击恢复" } else { "点击切换到窗口" };
                let hrc = RECT {
                    left: c.thumb_rect.left,
                    top: (icy + 22.0 * s) as i32,
                    right: c.thumb_rect.right,
                    bottom: c.thumb_rect.bottom,
                };
                draw_text(dc, hint, self.font_sub, &hrc, fg_dim, TextAlign::Center, true, false);
            }
        }

        drop(sf);
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let wdc = BeginPaint(self.hwnd, &mut ps);
            let _ = BitBlt(wdc, 0, 0, self.w, self.h, dc, 0, 0, SRCCOPY);
            let _ = EndPaint(self.hwnd, &ps);
        }
    }
}

unsafe extern "system" fn preview_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            crate::app::with_app(hwnd, |app| app.paint_preview());
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.preview_mouse_move(x, y));
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            crate::app::with_app(hwnd, |app| app.preview_mouse_leave());
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.preview_lbutton_up(x, y));
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
