//! macOS 风格自绘右键与下拉菜单窗口，替代系统原生 TrackPopupMenu。
//!
//! 特点：自适应深浅磨砂圆角卡片、宽松行高（34dp）、强调色/危险色悬停胶囊、
//! 键盘上下/回车/Esc 支持、工作区边界自动翻转，以及关闭即释放 DIB 缓冲。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetMonitorInfoW, MonitorFromPoint, AC_SRC_OVER, BLENDFUNCTION, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::ui::*;
use crate::win32::{dp, get_x_lparam, get_y_lparam, wide_static};

pub const CLASS_MENU: &str = "WinDockFinderContextMenu";

static REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub id: usize,
    pub label: String,
    pub checked: bool,
    pub danger: bool,
    pub separator: bool,
}

impl MenuItem {
    pub fn normal(id: usize, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            checked: false,
            danger: false,
            separator: false,
        }
    }

    pub fn checked(id: usize, label: impl Into<String>, checked: bool) -> Self {
        Self {
            id,
            label: label.into(),
            checked,
            danger: false,
            separator: false,
        }
    }

    pub fn danger(id: usize, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            checked: false,
            danger: true,
            separator: false,
        }
    }

    pub fn sep() -> Self {
        Self {
            id: 0,
            label: String::new(),
            checked: false,
            danger: false,
            separator: true,
        }
    }
}

pub struct ContextMenu {
    pub hwnd: HWND,
    pub visible: bool,
    prev_fg: HWND,
    dpi: u32,
    w: i32,
    h: i32,
    items: Vec<MenuItem>,
    item_rects: Vec<RECT>,
    hover: Option<usize>,
    pressed: Option<usize>,
    has_check_col: bool,
    dark: bool,
    accent: (f32, f32, f32),
    font: HFONT,
    target: DibTarget,
    scratch: DibTarget,
}

impl Default for ContextMenu {
    fn default() -> Self {
        Self {
            hwnd: HWND::default(),
            visible: false,
            prev_fg: HWND::default(),
            dpi: 96,
            w: 0,
            h: 0,
            items: Vec::new(),
            item_rects: Vec::new(),
            hover: None,
            pressed: None,
            has_check_col: false,
            dark: false,
            accent: (0.04, 0.52, 1.0),
            font: HFONT::default(),
            target: DibTarget::default(),
            scratch: DibTarget::default(),
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
            lpfnWndProc: Some(menu_proc),
            hInstance: crate::win32::hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: wide_static(CLASS_MENU),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

impl ContextMenu {
    fn ensure_window(&mut self, app_ptr: *mut crate::app::App, dpi: u32) -> bool {
        if !self.hwnd.0.is_null() && unsafe { IsWindow(self.hwnd).as_bool() } {
            if self.dpi != dpi {
                self.dpi = dpi;
                self.rebuild_font();
            }
            return true;
        }
        ensure_class();
        self.dpi = dpi;
        self.rebuild_font();
        unsafe {
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                wide_static(CLASS_MENU),
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
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
            self.hwnd = hwnd;
            true
        }
    }

    fn rebuild_font(&mut self) {
        if !self.font.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
        self.font = make_font(dp(13, self.dpi), 400);
    }

    /// 在指定屏幕坐标打开自绘菜单（自动避让屏幕工作区边界）。
    pub fn open(
        &mut self,
        app_ptr: *mut crate::app::App,
        items: Vec<MenuItem>,
        x: i32,
        y: i32,
        prev_fg: HWND,
        appearance: &Appearance,
    ) {
        if items.is_empty() {
            self.close();
            return;
        }
        if !prev_fg.0.is_null() && prev_fg != self.hwnd {
            self.prev_fg = prev_fg;
        }
        let mon = unsafe {
            let pt = POINT { x, y };
            let hm = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(hm, &mut mi);
            mi
        };
        let dpi = crate::win32::monitor_at_cursor().map(|m| m.dpi).unwrap_or(96);
        if !self.ensure_window(app_ptr, dpi) {
            return;
        }
        self.items = items;
        self.hover = None;
        self.pressed = None;
        self.dark = appearance.is_dark();
        self.accent = accent_from_config(appearance);
        self.has_check_col = self.items.iter().any(|it| !it.separator && it.checked);

        let pad_x = dp(6, dpi);
        let pad_y = dp(6, dpi);
        let row_h = dp(34, dpi);
        let sep_h = dp(9, dpi);
        let text_pad_l = if self.has_check_col { dp(28, dpi) } else { dp(14, dpi) };
        let text_pad_r = dp(16, dpi);

        // 确保用于测量文字的临时 DC 可用
        if self.target.mem_dc.is_none() {
            let _ = self.target.ensure(64, 64);
        }
        let max_text_w = self
            .target
            .mem_dc
            .map(|dc| {
                self.items
                    .iter()
                    .filter(|it| !it.separator)
                    .map(|it| measure_text(dc, &it.label, self.font).0)
                    .max()
                    .unwrap_or(100)
            })
            .unwrap_or(120);

        let w = (pad_x * 2 + text_pad_l + max_text_w + text_pad_r).clamp(dp(168, dpi), dp(330, dpi));
        let mut cy = pad_y;
        self.item_rects.clear();
        for it in &self.items {
            if it.separator {
                self.item_rects.push(RECT {
                    left: pad_x,
                    top: cy,
                    right: w - pad_x,
                    bottom: cy + sep_h,
                });
                cy += sep_h;
            } else {
                self.item_rects.push(RECT {
                    left: pad_x,
                    top: cy,
                    right: w - pad_x,
                    bottom: cy + row_h,
                });
                cy += row_h;
            }
        }
        let h = cy + pad_y;
        self.w = w;
        self.h = h;

        // 避让工作区边缘：超出下边界时向上翻转，超出右边界时向左翻转
        let work = mon.rcWork;
        let margin = dp(6, dpi);
        let mut left = x;
        let mut top = y;
        if left + w > work.right - margin {
            left = (x - w).max(work.left + margin);
        }
        if left < work.left + margin {
            left = work.left + margin;
        }
        if top + h > work.bottom - margin {
            top = (y - h).max(work.top + margin);
        }
        if top < work.top + margin {
            top = work.top + margin;
        }

        self.visible = true;
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, left, top, w, h, SWP_NOACTIVATE | SWP_NOSENDCHANGING);
            self.render();
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
        }
    }

    /// 关闭菜单并立即释放 DIB 缓冲以控制常驻内存。
    pub fn close(&mut self) {
        if self.visible {
            self.visible = false;
            self.hover = None;
            self.pressed = None;
            // 若关闭时菜单仍持有前景焦点，先归还给原窗口，避免点击顶部栏等无激活区域后焦点掉到桌面
            crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.target.release();
        self.scratch.release();
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
        if !self.font.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            self.font = HFONT::default();
        }
    }

    fn hit_index(&self, x: i32, y: i32) -> Option<usize> {
        self.item_rects.iter().enumerate().find_map(|(i, r)| {
            if !self.items[i].separator && x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                Some(i)
            } else {
                None
            }
        })
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        let next = self.hit_index(x, y);
        if next != self.hover {
            self.hover = next;
            self.render();
        }
    }

    pub fn on_mouse_leave(&mut self) {
        if self.hover.is_some() || self.pressed.is_some() {
            self.hover = None;
            self.pressed = None;
            self.render();
        }
    }

    pub fn on_lbutton_down(&mut self, x: i32, y: i32) {
        let hit = self.hit_index(x, y);
        if hit != self.pressed {
            self.pressed = hit;
            self.render();
        }
    }

    /// 左键抬起命中项则返回对应命令 id。
    pub fn on_lbutton_up(&mut self, x: i32, y: i32) -> Option<usize> {
        let pressed = self.pressed.take();
        let hit = self.hit_index(x, y);
        if let Some(i) = hit {
            if pressed.is_none() || pressed == Some(i) {
                let id = self.items[i].id;
                self.close();
                return Some(id);
            }
        }
        self.render();
        None
    }

    pub fn on_key_down(&mut self, vk: u32) -> Option<usize> {
        if vk == VK_ESCAPE.0 as u32 {
            self.close();
            return None;
        }
        if vk == VK_RETURN.0 as u32 {
            if let Some(i) = self.hover {
                if !self.items[i].separator {
                    let id = self.items[i].id;
                    self.close();
                    return Some(id);
                }
            }
            return None;
        }
        if vk == VK_UP.0 as u32 || vk == VK_DOWN.0 as u32 {
            let valid: Vec<usize> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, it)| !it.separator)
                .map(|(i, _)| i)
                .collect();
            if valid.is_empty() {
                return None;
            }
            let next = match self.hover.and_then(|h| valid.iter().position(|&idx| idx == h)) {
                Some(pos) => {
                    if vk == VK_UP.0 as u32 {
                        valid[(pos + valid.len() - 1) % valid.len()]
                    } else {
                        valid[(pos + 1) % valid.len()]
                    }
                }
                None => {
                    if vk == VK_UP.0 as u32 {
                        *valid.last().unwrap()
                    } else {
                        valid[0]
                    }
                }
            };
            self.hover = Some(next);
            self.render();
        }
        None
    }

    pub fn render(&mut self) {
        if !self.visible || self.w <= 0 || self.h <= 0 {
            return;
        }
        if !self.target.ensure(self.w, self.h) {
            return;
        }
        let Some(dc) = self.target.mem_dc else { return };
        let Some(mut sf) = self.target.surface() else { return };
        let (w, h) = (self.w, self.h);
        let s = self.dpi as f32 / 96.0;
        sf.clear_rect_transparent(w, h);

        let outer = RECT { left: 0, top: 0, right: w, bottom: h };
        let bg = if self.dark { rgb(0x24262C, 0.96) } else { rgb(0xFAFBFD, 0.97) };
        let bd = if self.dark { (1.0, 1.0, 1.0, 0.16) } else { (0.0, 0.0, 0.0, 0.12) };
        let fg_norm = if self.dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) };
        let fg_danger = if self.dark { rgb(0xFF6961, 1.0) } else { rgb(0xDC2626, 1.0) };
        let sep_col = if self.dark { (1.0, 1.0, 1.0, 0.11) } else { (0.0, 0.0, 0.0, 0.09) };
        let accent_bg = (self.accent.0, self.accent.1, self.accent.2, 0.96);
        let accent_press = (self.accent.0 * 0.85, self.accent.1 * 0.85, self.accent.2 * 0.85, 0.98);
        let danger_bg = rgb(0xFF453A, 0.96);
        let danger_press = rgb(0xDC2626, 0.98);

        sf.fill_rrect(&outer, 10.0 * s, bg);
        sf.stroke_rrect(&outer, 10.0 * s, 1.0, bd);

        let text_pad_l = if self.has_check_col { dp(28, self.dpi) } else { dp(14, self.dpi) };
        let text_pad_r = dp(14, self.dpi);

        for (i, it) in self.items.iter().enumerate() {
            let r = self.item_rects[i];
            if it.separator {
                let sy = (r.top + r.bottom) / 2;
                sf.fill_rect(
                    &RECT {
                        left: r.left + dp(8, self.dpi),
                        top: sy,
                        right: r.right - dp(8, self.dpi),
                        bottom: sy + 1,
                    },
                    sep_col,
                );
                continue;
            }

            let hov = self.hover == Some(i);
            let prs = self.pressed == Some(i);
            if hov || prs {
                let hl = if it.danger {
                    if prs { danger_press } else { danger_bg }
                } else if prs {
                    accent_press
                } else {
                    accent_bg
                };
                sf.fill_rrect(&r, 6.0 * s, hl);
            }

            let fg = if hov || prs {
                rgb(0xFFFFFF, 1.0)
            } else if it.danger {
                fg_danger
            } else {
                fg_norm
            };

            if it.checked {
                let cx = r.left as f32 + 13.0 * s;
                let cy = (r.top + r.bottom) as f32 * 0.5;
                sf.thick_line(cx - 3.2 * s, cy + 0.2 * s, cx - 0.6 * s, cy + 2.8 * s, 1.7 * s, fg);
                sf.thick_line(cx - 0.6 * s, cy + 2.8 * s, cx + 4.2 * s, cy - 2.6 * s, 1.7 * s, fg);
            }

            let trc = RECT {
                left: r.left + text_pad_l,
                top: r.top,
                right: r.right - text_pad_r,
                bottom: r.bottom,
            };
            draw_transparent_text_aligned(&mut sf, &mut self.scratch, &it.label, self.font, &trc, fg, TextAlign::Left, false, s);
        }

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

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.menu_mouse_move(x, y));
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
            crate::app::with_app(hwnd, |app| app.menu_mouse_leave());
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.menu_lbutton_down(x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.menu_lbutton_up(x, y));
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            crate::app::with_app(hwnd, |app| app.menu_key_down(wparam.0 as u32));
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            // 失去激活（点击窗外或切换窗口）即关闭菜单
            if (wparam.0 & 0xFFFF) == 0 {
                crate::app::with_app(hwnd, |app| app.close_context_menu());
            }
            return LRESULT(0);
        }
        WM_KILLFOCUS => {
            crate::app::with_app(hwnd, |app| app.close_context_menu());
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
