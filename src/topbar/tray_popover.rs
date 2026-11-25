//! 顶部栏「展开按钮」紧凑图标收纳浮层：
//! 展示当前打开的应用图标与托盘图标，支持 100ms 自定义 Tooltip、点击固定到顶部栏、
//! 拖拽调整顶部栏顺序，以及右键快捷操作。关闭即释放 DIB 缓冲。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{AC_SRC_OVER, BLENDFUNCTION, HFONT};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::dock::icons::DOCK_ICON_SRC_SIZE;
use crate::topbar::tray_mirror::TrayItem;
use crate::ui::*;
use crate::win32::{dp, get_wheel_delta, get_x_lparam, get_y_lparam, wide_static};

pub const CLASS_POPOVER: &str = "WinDockFinderPopover";

const COLS: usize = 6;
const MAX_VISIBLE_ROWS: usize = 4;
const TIMER_TIP: usize = 401;

#[derive(Clone, Debug)]
pub struct PopoverDrag {
    pub from: usize,
    pub x: i32,
    pub y: i32,
    pub over: Option<usize>,
}

pub struct Popover {
    pub hwnd: HWND,
    pub target: DibTarget,
    pub scratch: DibTarget,
    pub font: HFONT,
    pub tip_font: HFONT,
    pub dpi: u32,
    pub w: i32,
    pub h: i32,
    pub card_top: i32,
    pub scroll: usize,
    pub hover_idx: Option<usize>,
    pub hover_since: Option<Instant>,
    pub tip_visible: bool,
    pub press: Option<(usize, i32, i32)>,
    pub drag: Option<PopoverDrag>,
    pub visible: bool,
    pub items: Vec<TrayItem>,
    pub pinned: Vec<String>,
    pub dark: bool,
    pub accent: (f32, f32, f32),
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

impl Popover {
    pub fn new(app_ptr: *mut crate::app::App, dpi: u32) -> Result<Popover, String> {
        if !REGISTERED.swap(true, Ordering::SeqCst) {
            unsafe {
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(popover_proc),
                    hInstance: crate::win32::hinstance(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    lpszClassName: wide_static(CLASS_POPOVER),
                    ..Default::default()
                };
                let _ = RegisterClassExW(&wc);
            }
        }
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                wide_static(CLASS_POPOVER),
                w!(""),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                crate::win32::hinstance(),
                Some(app_ptr as *const _),
            )
            .map_err(crate::win32::error_string)?;
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
            Ok(Popover {
                hwnd,
                target: DibTarget::default(),
                scratch: DibTarget::default(),
                font: make_font(dp(12, dpi), 400),
                tip_font: make_font(dp(12, dpi), 400),
                dpi,
                w: 0,
                h: 0,
                card_top: 0,
                scroll: 0,
                hover_idx: None,
                hover_since: None,
                tip_visible: false,
                press: None,
                drag: None,
                visible: false,
                items: Vec::new(),
                pinned: Vec::new(),
                dark: false,
                accent: (0.04, 0.52, 1.0),
            })
        }
    }

    pub fn cell_sz(&self) -> i32 {
        dp(42, self.dpi)
    }

    pub fn cell_gap(&self) -> i32 {
        dp(4, self.dpi)
    }

    pub fn pad(&self) -> i32 {
        dp(10, self.dpi)
    }

    /// 图标网格上方预留少许透明区，用于显示 100ms 悬停 Tooltip，不遮挡图标本身。
    fn tip_reserve(&self) -> i32 {
        dp(30, self.dpi)
    }

    fn total_rows(&self) -> usize {
        if self.items.is_empty() {
            1
        } else {
            (self.items.len() + COLS - 1) / COLS
        }
    }

    pub fn scroll_max(&self) -> usize {
        self.total_rows().saturating_sub(MAX_VISIBLE_ROWS)
    }

    /// 打开浮层：锚定在顶部栏 `<` 按钮正下方、右对齐，自动避让屏幕边界。
    pub fn open(&mut self, anchor: RECT, items: Vec<TrayItem>, pinned: Vec<String>, appearance: &Appearance) {
        self.items = items;
        self.pinned = pinned;
        self.scroll = 0;
        self.hover_idx = None;
        self.hover_since = None;
        self.tip_visible = false;
        self.press = None;
        self.drag = None;
        self.dark = appearance.is_dark();
        self.accent = accent_from_config(appearance);

        let cols = if self.items.is_empty() {
            4
        } else {
            self.items.len().clamp(3, COLS)
        };
        let rows = self.total_rows().min(MAX_VISIBLE_ROWS);
        let pad = self.pad();
        let cell = self.cell_sz();
        let gap = self.cell_gap();
        let card_w = pad * 2 + cols as i32 * cell + (cols as i32 - 1).max(0) * gap;
        let card_h = pad * 2 + rows as i32 * cell + (rows as i32 - 1).max(0) * gap;
        let tip_res = self.tip_reserve();

        self.w = card_w.max(dp(190, self.dpi));
        self.card_top = 0;
        self.h = card_h + tip_res;

        let mut left = anchor.right - self.w;
        let mut top = anchor.bottom + dp(4, self.dpi);
        unsafe {
            let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                ..Default::default()
            };
            let mon = windows::Win32::Graphics::Gdi::MonitorFromRect(&anchor, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
            let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
            left = left.clamp(mi.rcWork.left + 8, (mi.rcWork.right - self.w - 8).max(mi.rcWork.left + 8));
            if top + self.h > mi.rcWork.bottom - 8 {
                top = (anchor.top - self.h - 4).max(mi.rcWork.top + 8);
            }
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, left, top, self.w, self.h, SWP_NOACTIVATE | SWP_NOSENDCHANGING);
            self.visible = true;
            self.render();
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
        }
    }

    pub fn close(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TIP);
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
        }
        if self.visible {
            self.visible = false;
            self.hover_idx = None;
            self.hover_since = None;
            self.tip_visible = false;
            self.press = None;
            self.drag = None;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.target.release();
        self.scratch.release();
    }

    pub fn destroy(&mut self) {
        self.close();
        unsafe {
            let _ = SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            for f in [self.font, self.tip_font] {
                if !f.is_invalid() {
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(f.0));
                }
            }
        }
    }

    fn card_rect(&self) -> RECT {
        RECT {
            left: 0,
            top: self.card_top,
            right: self.w,
            bottom: self.h - self.tip_reserve(),
        }
    }

    pub fn cell_rect(&self, idx: usize) -> Option<RECT> {
        let start = self.scroll * COLS;
        let end = start + MAX_VISIBLE_ROWS * COLS;
        if idx < start || idx >= end || idx >= self.items.len() {
            return None;
        }
        let rel = idx - start;
        let r = rel / COLS;
        let c = rel % COLS;
        let pad = self.pad();
        let cell = self.cell_sz();
        let gap = self.cell_gap();
        let left = pad + c as i32 * (cell + gap);
        let top = self.card_top + pad + r as i32 * (cell + gap);
        Some(RECT {
            left,
            top,
            right: left + cell,
            bottom: top + cell,
        })
    }

    pub fn index_at(&self, x: i32, y: i32) -> Option<usize> {
        let start = self.scroll * COLS;
        let end = (start + MAX_VISIBLE_ROWS * COLS).min(self.items.len());
        for idx in start..end {
            if let Some(r) = self.cell_rect(idx) {
                if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                    return Some(idx);
                }
            }
        }
        None
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        if let Some((pi, px, py)) = self.press {
            if self.drag.is_none() {
                let dist = (((x - px).pow(2) + (y - py).pow(2)) as f32).sqrt();
                if dist >= 5.0 * (self.dpi as f32 / 96.0) {
                    self.drag = Some(PopoverDrag {
                        from: pi,
                        x,
                        y,
                        over: self.index_at(x, y),
                    });
                    self.tip_visible = false;
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_TIP);
                        let _ = SetCapture(self.hwnd);
                    }
                }
            }
        }
        if self.drag.is_some() {
            let over = self.index_at(x, y);
            if let Some(d) = self.drag.as_mut() {
                d.x = x;
                d.y = y;
                d.over = over;
            }
            self.render();
            return;
        }
        let hit = self.index_at(x, y);
        if hit != self.hover_idx {
            self.hover_idx = hit;
            self.tip_visible = false;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_TIP);
                if hit.is_some() {
                    self.hover_since = Some(Instant::now());
                    let _ = SetTimer(self.hwnd, TIMER_TIP, 100, None);
                } else {
                    self.hover_since = None;
                }
            }
            self.render();
        }
    }

    pub fn on_mouse_leave(&mut self) {
        if self.drag.is_none() {
            self.hover_idx = None;
            self.hover_since = None;
            self.tip_visible = false;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_TIP);
            }
            self.render();
        }
    }

    pub fn on_tip_timer(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TIP);
        }
        if self.hover_idx.is_some() && self.drag.is_none() && !self.tip_visible {
            self.tip_visible = true;
            self.render();
        }
    }

    /// 渲染紧凑图标网格、固定圆点、拖拽指示与底部 100ms Tooltip 气泡。
    pub fn render(&mut self) {
        if !self.visible || self.w <= 0 || self.h <= 0 {
            return;
        }
        let (w, h) = (self.w, self.h);
        if !self.target.ensure(w, h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else { return };
        let Some(mut sf) = self.target.surface() else { return };
        sf.clear_rect_transparent(w, h);

        let s = self.dpi as f32 / 96.0;
        let card = self.card_rect();
        let bg = if self.dark { rgb(0x222328, 0.96) } else { rgb(0xF8F9FC, 0.97) };
        let bd = if self.dark { (1.0, 1.0, 1.0, 0.16) } else { (0.0, 0.0, 0.0, 0.12) };
        let hover_bg = if self.dark { rgb(0x363842, 0.92) } else { rgb(0xE8ECF4, 0.95) };
        let press_bg = if self.dark { rgb(0x454856, 0.95) } else { rgb(0xDCE2EE, 0.98) };
        let fg = if self.dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) };
        let fg_dim = if self.dark { rgb(0x98989D, 1.0) } else { rgb(0x6E6E73, 1.0) };
        let accent_bg = (self.accent.0, self.accent.1, self.accent.2, if self.dark { 0.22 } else { 0.14 });
        let accent_bd = (self.accent.0, self.accent.1, self.accent.2, 0.65);

        sf.fill_rrect(&card, 12.0 * s, bg);
        sf.stroke_rrect(&card, 12.0 * s, 1.0, bd);

        if self.items.is_empty() {
            draw_transparent_text_aligned(&mut sf, &mut self.scratch, "暂无打开的应用", self.font, &card, fg_dim, TextAlign::Center, false, s);
        } else {
            let start = self.scroll * COLS;
            let end = (start + MAX_VISIBLE_ROWS * COLS).min(self.items.len());
            let app_icons = crate::app::app_icon_cache();
            let tray_icons = crate::app::tray_icon_cache();

            for idx in start..end {
                if self.drag.as_ref().is_some_and(|d| d.from == idx) {
                    continue;
                }
                let Some(r) = self.cell_rect(idx) else { continue };
                let item = &self.items[idx];
                let is_pinned = self.pinned.iter().any(|k| k == &item.key);
                let is_hover = self.hover_idx == Some(idx);
                let is_press = self.press.is_some_and(|(pi, _, _)| pi == idx) && self.drag.is_none();

                if is_press {
                    sf.fill_rrect(&r, 9.0 * s, press_bg);
                } else if is_hover {
                    sf.fill_rrect(&r, 9.0 * s, hover_bg);
                } else if is_pinned {
                    sf.fill_rrect(&r, 9.0 * s, accent_bg);
                }
                if is_pinned {
                    sf.stroke_rrect(&r, 9.0 * s, 1.0, accent_bd);
                }

                let cx = (r.left + r.right) as f32 * 0.5;
                let cy = (r.top + r.bottom) as f32 * 0.5;
                let icon_sz = 22.0 * s;
                draw_shelf_item_icon(&mut sf, &mut self.scratch, self.font, item, &app_icons, &tray_icons, cx, cy, icon_sz, self.dark, s);

                // 右上角固定指示小圆点
                if is_pinned {
                    let dx = r.right as f32 - 6.5 * s;
                    let dy = r.top as f32 + 6.5 * s;
                    sf.fill_circle(dx, dy, 2.8 * s, (self.accent.0, self.accent.1, self.accent.2, 0.98));
                }

                // 拖拽目标插入线
                if let Some(d) = &self.drag {
                    if d.over == Some(idx) && d.from != idx {
                        let line_x = if d.x < cx as i32 { r.left - 1 } else { r.right - 1 };
                        sf.fill_rrect(
                            &RECT {
                                left: line_x - 1,
                                top: r.top + dp(5, self.dpi),
                                right: line_x + 2,
                                bottom: r.bottom - dp(5, self.dpi),
                            },
                            1.5 * s,
                            (self.accent.0, self.accent.1, self.accent.2, 0.98),
                        );
                    }
                }
            }

            // 拖拽跟随幽灵图标
            if let Some(d) = &self.drag {
                if let Some(item) = self.items.get(d.from) {
                    draw_shelf_item_icon(
                        &mut sf,
                        &mut self.scratch,
                        self.font,
                        item,
                        &app_icons,
                        &tray_icons,
                        d.x as f32,
                        d.y as f32,
                        24.0 * s,
                        self.dark,
                        s,
                    );
                }
            }
        }

        // 100ms 自定义 Tooltip（绘制在底部预留透明区中，带指向角标）
        if self.tip_visible && self.drag.is_none() {
            if let Some(idx) = self.hover_idx {
                if let (Some(item), Some(anchor)) = (self.items.get(idx), self.cell_rect(idx)) {
                    let is_pinned = self.pinned.iter().any(|k| k == &item.key);
                    let short_name: String = item.name.chars().take(24).collect();
                    let tip_text = if is_pinned {
                        format!("{short_name} · 已固定")
                    } else {
                        format!("{short_name} · 点击固定")
                    };
                    let (tw, th) = measure_text(dc, &tip_text, self.tip_font);
                    let pad_x = dp(9, self.dpi);
                    let pad_y = dp(4, self.dpi);
                    let bw = (tw + pad_x * 2).min(w - 8);
                    let bh = th + pad_y * 2;
                    let cx = (anchor.left + anchor.right) / 2;
                    let left = (cx - bw / 2).clamp(4, (w - bw - 4).max(4));
                    let top = (card.bottom + dp(4, self.dpi)).min(h - bh - 2);
                    let box_r = RECT {
                        left,
                        top,
                        right: left + bw,
                        bottom: top + bh,
                    };
                    let tip_bg = rgb(0x181C24, 0.95);
                    sf.fill_rrect(&box_r, 6.0 * s, tip_bg);
                    sf.stroke_rrect(&box_r, 6.0 * s, 1.0, (1.0, 1.0, 1.0, 0.16));
                    let trc = RECT {
                        left: box_r.left + pad_x,
                        top: box_r.top,
                        right: box_r.right - pad_x,
                        bottom: box_r.bottom,
                    };
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &tip_text, self.tip_font, &trc, rgb(0xF8FAFC, 0.98), TextAlign::Center, false, s);
                }
            }
        }
        let _ = fg;

        drop(sf);
        unsafe {
            let mut wr = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut wr);
            let dst = POINT { x: wr.left, y: wr.top };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: 1,
            };
            let _ = UpdateLayeredWindow(self.hwnd, None, Some(&dst), Some(&size), dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }
}

/// 统一绘制收纳/顶部栏图标（优先动态托盘镜像与窗口实时图标，回退应用静态图标与首字母徽章）。
pub fn draw_shelf_item_icon(
    sf: &mut Surface,
    scratch: &mut DibTarget,
    font: HFONT,
    item: &TrayItem,
    app_icons: &crate::dock::icons::IconCache,
    tray_icons: &crate::topbar::tray_mirror::TrayIconCache,
    cx: f32,
    cy: f32,
    size: f32,
    dark: bool,
    s: f32,
) {
    if let Some(px) = item
        .uia_key
        .as_deref()
        .and_then(|k| tray_icons.get(k))
        .or_else(|| tray_icons.get(&format!("win:{}", item.key)))
        .or_else(|| tray_icons.get(&item.key))
    {
        sf.draw_icon_bilinear(&px, cx, cy, size, false, 0.0);
        return;
    }
    if let Some(exe) = &item.app_exe {
        if let Some(px) = app_icons.pixels(exe, DOCK_ICON_SRC_SIZE) {
            sf.draw_icon_bilinear(&px, cx, cy, size, false, 0.0);
            return;
        }
    }
    let ch: String = item
        .name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".into());
    let br = RECT {
        left: (cx - size * 0.48) as i32,
        top: (cy - size * 0.48) as i32,
        right: (cx + size * 0.48) as i32,
        bottom: (cy + size * 0.48) as i32,
    };
    sf.fill_rrect(&br, size * 0.25, if dark { rgb(0x48484A, 1.0) } else { rgb(0xDCE0E8, 1.0) });
    let trc = RECT { left: br.left, top: br.top - 1, right: br.right, bottom: br.bottom + 1 };
    draw_transparent_text_aligned(sf, scratch, &ch, font, &trc, if dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) }, TextAlign::Center, false, s);
}

unsafe extern "system" fn popover_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam.0 == TIMER_TIP {
                crate::app::with_app(hwnd, |app| app.popover_tip_timer(hwnd));
                return LRESULT(0);
            }
        }
        WM_MOUSEMOVE => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.popover_mouse_move(hwnd, x, y));
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
            crate::app::with_app(hwnd, |app| app.popover_mouse_leave(hwnd));
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let delta = get_wheel_delta(wparam.0);
            crate::app::with_app(hwnd, |app| app.popover_wheel(hwnd, delta));
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.popover_lbutton_down(hwnd, x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.popover_lbutton_up(hwnd, x, y));
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.popover_rclick(hwnd, x, y));
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                crate::app::with_app(hwnd, |app| app.close_popover());
                return LRESULT(0);
            }
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) == 0 {
                crate::app::with_app(hwnd, |app| app.close_popover());
            }
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            crate::app::with_app(hwnd, |app| app.popover_capture_lost(hwnd));
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
