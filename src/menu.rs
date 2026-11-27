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
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::ui::*;
use crate::win32::{dp, get_x_lparam, get_y_lparam, wide_static};

pub const CLASS_MENU: &str = "WinDockContextMenu";

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
    pub w: i32,
    pub h: i32,
    pub drawer: DrawerAnim,
    anchored: bool,
    anchor: RECT,
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
            drawer: DrawerAnim::default(),
            anchored: false,
            anchor: RECT::default(),
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
            // 使用非激活悬浮窗样式，弹出自绘菜单时不抢夺前台应用焦点（防止 Win11 Mica 窗口失焦变色）
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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

    /// 是否处于打开态（未进入收起动画）。
    pub fn is_open(&self) -> bool {
        self.visible && !self.drawer.closing
    }

    /// 是否仍有展开或收起动画需要逐帧推进。
    pub fn is_animating(&self) -> bool {
        self.drawer.is_animating(self.visible)
    }

    /// 当前是否有键盘或鼠标悬停高亮的菜单项。
    pub fn has_hover(&self) -> bool {
        self.hover.is_some()
    }

    /// 返回菜单卡片在屏幕坐标系中的当前可见区域（不含顶部透明间距）。
    pub fn visible_screen_rect(&self) -> Option<RECT> {
        if !self.visible {
            return None;
        }
        let wr = crate::win32::window_rect(self.hwnd)?;
        self.drawer.visible_screen_rect(wr, self.w, self.h)
    }

    fn card_rect(&self) -> RECT {
        let ch = if self.drawer.card_h > 0 { self.drawer.card_h } else { self.h };
        RECT {
            left: 0,
            top: self.drawer.card_top,
            right: self.w,
            bottom: self.drawer.card_top + ch,
        }
    }

    /// 在指定屏幕坐标打开自绘菜单（自动避让屏幕工作区边界，用于 Dock/托盘等非顶部栏右键菜单）。
    pub fn open(
        &mut self,
        app_ptr: *mut crate::app::App,
        items: Vec<MenuItem>,
        x: i32,
        y: i32,
        prev_fg: HWND,
        appearance: &Appearance,
    ) {
        self.open_impl(app_ptr, items, None, x, y, prev_fg, appearance);
    }

    /// 在顶部栏按钮下方打开自绘下拉菜单（与收纳浮层共享抽屉滑出/收起动效）。
    pub fn open_anchored(
        &mut self,
        app_ptr: *mut crate::app::App,
        items: Vec<MenuItem>,
        anchor: RECT,
        dpi: u32,
        align_right: bool,
        prev_fg: HWND,
        appearance: &Appearance,
    ) {
        self.open_impl(
            app_ptr,
            items,
            Some((anchor, dpi, align_right)),
            anchor.left,
            anchor.bottom,
            prev_fg,
            appearance,
        );
    }

    fn open_impl(
        &mut self,
        app_ptr: *mut crate::app::App,
        items: Vec<MenuItem>,
        anchored: Option<(RECT, u32, bool)>,
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
        let (mon, dpi) = if let Some((anchor_rect, bar_dpi, _)) = anchored {
            let mi = unsafe {
                let hm = windows::Win32::Graphics::Gdi::MonitorFromRect(&anchor_rect, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                let _ = GetMonitorInfoW(hm, &mut mi);
                mi
            };
            (mi, bar_dpi.max(96))
        } else {
            let mi = unsafe {
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
            (mi, dpi)
        };
        if !self.ensure_window(app_ptr, dpi) {
            return;
        }

        let was_visible = self.visible;
        let same_closing = matches!(
            anchored,
            Some((anchor_rect, _, _)) if was_visible && self.anchored && self.anchor == anchor_rect && self.drawer.closing
        );
        self.anchored = anchored.is_some();
        self.anchor = anchored.map(|(r, _, _)| r).unwrap_or_default();
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
        let card_h = cy + pad_y;
        self.w = w;

        // 顶部栏锚定菜单启用抽屉滑出动效；其余光标右键菜单保持原位立即显示
        let work = mon.rcWork;
        let margin = dp(6, dpi);
        let (left, win_top, win_h) = if let Some((anchor_rect, _, align_right)) = anchored {
            let bounds = compute_anchored_drawer_bounds(anchor_rect, work, w, card_h, dp(4, dpi), margin, align_right);
            self.drawer
                .begin_open(was_visible, same_closing, false, appearance.reduce_motion, true);
            self.drawer
                .configure_bounds(card_h, bounds.gap_px, bounds.resting_card_top, bounds.open_down);
            (bounds.left, bounds.win_top, bounds.win_h)
        } else {
            let mut left = x;
            let mut top = y;
            if left + w > work.right - margin {
                left = (x - w).max(work.left + margin);
            }
            if left < work.left + margin {
                left = work.left + margin;
            }
            if top + card_h > work.bottom - margin {
                top = (y - card_h).max(work.top + margin);
            }
            if top < work.top + margin {
                top = work.top + margin;
            }
            self.drawer.begin_open(false, false, false, true, false);
            self.drawer.configure_bounds(card_h, 0, 0, true);
            (left, top, card_h)
        };
        self.h = win_h;

        self.visible = true;
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, left, win_top, self.w, self.h, SWP_NOACTIVATE | SWP_NOSENDCHANGING);
            self.render();
            if !was_visible {
                // 保持原前台窗口激活状态，避免破坏背后窗口的 Mica/亚克力材质
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            }
        }
    }

    /// 触发平滑收起动画（仅顶部栏锚定菜单且未禁用动效时生效，否则立即隐藏）。
    pub fn begin_close(&mut self, reduce: bool) -> bool {
        if !self.visible {
            return false;
        }
        self.hover = None;
        self.pressed = None;
        crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
        if !self.drawer.begin_close(reduce) {
            self.finish_close();
            return false;
        }
        self.render();
        true
    }

    /// 推进一帧菜单抽屉展开/收起动画；返回 `(仍在动画, 进度有变化, 本帧刚完成收起)`。
    pub fn tick(&mut self, dt_ms: f32, reduce: bool) -> (bool, bool, bool) {
        if !self.visible {
            return (false, false, false);
        }
        let (animating, changed, just_closed) = self.drawer.tick(dt_ms, reduce);
        if just_closed {
            self.finish_close();
            return (false, true, true);
        }
        if changed {
            self.render();
        }
        (animating, changed, false)
    }

    /// 关闭菜单并立即释放 DIB 缓冲以控制常驻内存。
    pub fn close(&mut self) {
        if self.visible {
            // 若关闭时菜单仍持有前景焦点，先归还给原窗口，避免点击顶部栏等无激活区域后焦点掉到桌面
            crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
            self.finish_close();
        } else {
            self.anchored = false;
            self.drawer.reset_closed();
            self.target.release();
            self.scratch.release();
        }
    }

    fn finish_close(&mut self) {
        self.visible = false;
        self.anchored = false;
        self.drawer.reset_closed();
        self.hover = None;
        self.pressed = None;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
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
        if x < 0 || x >= self.w || y < 0 || y >= self.h {
            return None;
        }
        let ly = y - self.drawer.card_top;
        self.item_rects.iter().enumerate().find_map(|(i, r)| {
            if !self.items[i].separator && x >= r.left && x < r.right && ly >= r.top && ly < r.bottom {
                Some(i)
            } else {
                None
            }
        })
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        if !self.is_open() {
            return;
        }
        let next = self.hit_index(x, y);
        if next != self.hover {
            self.hover = next;
            self.render();
        }
    }

    pub fn on_mouse_leave(&mut self) {
        if !self.is_open() {
            return;
        }
        if self.hover.is_some() || self.pressed.is_some() {
            self.hover = None;
            self.pressed = None;
            self.render();
        }
    }

    pub fn on_lbutton_down(&mut self, x: i32, y: i32) {
        if !self.is_open() {
            return;
        }
        let hit = self.hit_index(x, y);
        if hit != self.pressed {
            self.pressed = hit;
            self.render();
        }
    }

    /// 左键抬起命中项则返回对应命令 id。
    pub fn on_lbutton_up(&mut self, x: i32, y: i32) -> Option<usize> {
        if !self.is_open() {
            return None;
        }
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
        if !self.is_open() {
            return None;
        }
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

        // 按抽屉垂直偏移绘制菜单卡片，超出顶部栏边缘的部分由 Surface 自动裁切
        let card = self.card_rect();
        let bg = if self.dark { rgb(0x24262C, 0.96) } else { rgb(0xFAFBFD, 0.97) };
        let bd = if self.dark { (1.0, 1.0, 1.0, 0.16) } else { (0.0, 0.0, 0.0, 0.12) };
        let fg_norm = if self.dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) };
        let fg_danger = if self.dark { rgb(0xFF6961, 1.0) } else { rgb(0xDC2626, 1.0) };
        let sep_col = if self.dark { (1.0, 1.0, 1.0, 0.11) } else { (0.0, 0.0, 0.0, 0.09) };
        let accent_bg = (self.accent.0, self.accent.1, self.accent.2, 0.96);
        let accent_press = (self.accent.0 * 0.85, self.accent.1 * 0.85, self.accent.2 * 0.85, 0.98);
        let danger_bg = rgb(0xFF453A, 0.96);
        let danger_press = rgb(0xDC2626, 0.98);

        sf.fill_rrect(&card, 10.0 * s, bg);
        sf.stroke_rrect(&card, 10.0 * s, 1.0, bd);

        let text_pad_l = if self.has_check_col { dp(28, self.dpi) } else { dp(14, self.dpi) };
        let text_pad_r = dp(14, self.dpi);

        for (i, it) in self.items.iter().enumerate() {
            let mut r = self.item_rects[i];
            r.top += self.drawer.card_top;
            r.bottom += self.drawer.card_top;
            if r.bottom <= 0 || r.top >= h {
                continue;
            }
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
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
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
            if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                crate::app::with_app(hwnd, |app| app.close_context_menu());
                return LRESULT(0);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_topbar_menu_animates_smoothly_and_cursor_menu_stays_immediate() {
        let mut menu = ContextMenu::default();
        let items = vec![
            MenuItem::normal(101, "新建文件管理器窗口"),
            MenuItem::normal(102, "打开终端"),
            MenuItem::sep(),
            MenuItem::danger(103, "关闭当前窗口"),
        ];
        let anchor = RECT { left: 120, top: 0, right: 170, bottom: 30 };
        let app = Appearance::default();

        // 1. 顶部栏锚定菜单：首帧即探出部分轮廓并贴紧顶部栏下沿裁切，且保持非激活悬浮窗样式
        menu.open_anchored(std::ptr::null_mut(), items.clone(), anchor, 96, false, HWND::default(), &app);
        assert!(menu.is_open());
        assert!(menu.anchored);
        assert!(menu.is_animating());
        unsafe {
            let ex = GetWindowLongPtrW(menu.hwnd, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0, "自绘菜单必须具备 WS_EX_NOACTIVATE 防止抢夺前景焦点");
            assert_eq!(
                SendMessageW(menu.hwnd, WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)).0,
                MA_NOACTIVATE as isize
            );
            assert_ne!(GetForegroundWindow(), menu.hwnd, "打开自绘菜单不应改变前台激活窗口");
        }
        assert!(menu.visible_screen_rect().is_some());
        assert!(menu.drawer.card_top < menu.drawer.resting_card_top);
        assert!(menu.drawer.card_top + menu.drawer.card_h > 0);
        let mut wr = RECT::default();
        unsafe {
            let _ = GetWindowRect(menu.hwnd, &mut wr);
        }
        assert_eq!(wr.top, anchor.bottom);
        let stride = menu.target.w as usize;
        let open_pixels = unsafe { std::slice::from_raw_parts(menu.target.ptr as *const u32, stride * menu.target.h as usize) };
        assert!((open_pixels[menu.w as usize / 2] >> 24) > 200, "首帧顶部栏下沿第一行应显示滑出的菜单卡片");

        // 展开完成后顶部间距透明，菜单项命中区域随 resting_card_top 对齐
        for _ in 0..14 {
            let _ = menu.tick(16.0, false);
        }
        assert!(!menu.is_animating());
        assert_eq!(menu.drawer.card_top, menu.drawer.resting_card_top);
        let pixels = unsafe { std::slice::from_raw_parts(menu.target.ptr as *const u32, stride * menu.target.h as usize) };
        assert_eq!(pixels[menu.w as usize / 2] >> 24, 0, "顶部留白首行应保持全透明");
        let first_y = menu.drawer.card_top + menu.item_rects[0].top + 8;
        assert_eq!(menu.hit_index(menu.w / 2, first_y), Some(0));

        // 2. 平滑收起中途拦截输入，再次点开同一按钮无缝反转接续
        assert!(menu.begin_close(false));
        assert!(menu.drawer.closing);
        assert!(!menu.is_open());
        assert_eq!(menu.on_lbutton_up(menu.w / 2, first_y), None);
        let _ = menu.tick(24.0, false);
        let mid_top = menu.drawer.card_top;
        assert!(mid_top < menu.drawer.resting_card_top && menu.visible);

        menu.open_anchored(std::ptr::null_mut(), items.clone(), anchor, 96, false, HWND::default(), &app);
        assert!(menu.is_open());
        assert!(!menu.drawer.closing);
        assert!((menu.drawer.card_top - mid_top).abs() <= 1);

        // 完整收起后隐藏窗口并释放 DIB
        assert!(menu.begin_close(false));
        for _ in 0..12 {
            let (anim, _, closed) = menu.tick(16.0, false);
            if closed {
                assert!(!anim);
                break;
            }
        }
        assert!(!menu.visible);
        assert!(!menu.anchored);
        assert!(menu.target.ptr.is_null());

        // 3. 减弱动态效果模式下锚定菜单直接到位且立即隐藏
        let mut reduced = Appearance::default();
        reduced.reduce_motion = true;
        menu.open_anchored(std::ptr::null_mut(), items.clone(), anchor, 96, false, HWND::default(), &reduced);
        assert!(menu.is_open());
        assert!(!menu.is_animating());
        assert_eq!(menu.drawer.card_top, menu.drawer.resting_card_top);
        assert!(!menu.begin_close(true));
        assert!(!menu.visible);

        // 4. 普通光标右键菜单保持非锚定立即显示与立即关闭
        menu.open(std::ptr::null_mut(), items, 400, 400, HWND::default(), &app);
        assert!(menu.is_open());
        assert!(!menu.anchored);
        assert!(!menu.is_animating());
        assert_eq!(menu.drawer.card_top, 0);
        assert!(!menu.begin_close(false));
        assert!(!menu.visible);

        menu.destroy();
    }
}
