//! 顶部栏「展开按钮」紧凑图标收纳浮层：
//! 按名称展示当前应用和托盘图标，支持点击固定到顶部栏和右键快捷操作。
//! 关闭即释放 DIB 缓冲。

use std::cmp;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Globalization::{CompareStringEx, CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, NORM_IGNORECASE, SORT_DIGITSASNUMBERS};
use windows::Win32::Graphics::Gdi::{AC_SRC_OVER, BLENDFUNCTION, HFONT};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::dock::icons::DOCK_ICON_SRC_SIZE;
use crate::topbar::tray_mirror::TrayItem;
use crate::ui::*;
use crate::win32::{dp, get_wheel_delta, get_x_lparam, get_y_lparam, wide_static};

pub const CLASS_POPOVER: &str = "WinDockPopover";

const COLS: usize = 6;
const MAX_VISIBLE_ROWS: usize = 4;

/// 使用 Windows 当前区域的名称顺序。同名图标用固定键打破平局，前台切换时也不会换位。
fn sort_items(items: &mut [TrayItem]) {
    let names: std::collections::HashMap<_, Vec<_>> = items.iter().map(|item| {
        (item.key.clone(), item.sort_name().encode_utf16().collect())
    }).collect();
    items.sort_by(|a, b| {
        let left = &names[&a.key];
        let right = &names[&b.key];
        let order = match unsafe { CompareStringEx(PCWSTR::null(), NORM_IGNORECASE | SORT_DIGITSASNUMBERS, left, right, None, None, LPARAM(0)) } {
            CSTR_LESS_THAN => cmp::Ordering::Less,
            CSTR_GREATER_THAN => cmp::Ordering::Greater,
            CSTR_EQUAL => cmp::Ordering::Equal,
            _ => left.cmp(right),
        };
        order.then_with(|| a.key.cmp(&b.key))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, name: &str) -> TrayItem {
        TrayItem {
            key: key.into(), name: name.into(), native_tip: Some("随时变化的托盘状态".into()),
            uia_key: None, registration: None, legacy_keys: Vec::new(), overflow: false,
            app_exe: None, app_hwnds: Vec::new(),
        }
    }

    #[test]
    fn names_keep_order_across_window_and_status_changes() {
        let mut items = [item("b", "Magpie - 2"), item("c", "App10"), item("a", "magpie - 1"), item("d", "app2")];
        sort_items(&mut items);
        assert_eq!(items.each_ref().map(|item| item.key.as_str()), ["d", "c", "a", "b"]);

        items.reverse();
        items[0].name = "Magpie - 3".into();
        items[0].native_tip = Some("Magpie\n3 个请求".into());
        sort_items(&mut items);
        assert_eq!(items.each_ref().map(|item| item.key.as_str()), ["d", "c", "a", "b"]);
    }

    #[test]
    fn drawer_easing_and_reversal_stay_monotonic_and_continuous() {
        assert!((eval_drawer_progress(0.0, false) - 0.0).abs() < 1e-6);
        assert!((eval_drawer_progress(1.0, false) - 1.0).abs() < 1e-6);
        assert!((eval_drawer_progress(0.0, true) - 0.0).abs() < 1e-6);
        assert!((eval_drawer_progress(1.0, true) - 1.0).abs() < 1e-6);

        // 展开前段比线性更快（ease-out），确保点击后迅速滑出不拖沓
        assert!(eval_drawer_progress(0.3, false) > 0.55);

        // 展开中途反向收起、收起中途反向展开均保持视觉进度连续无跳变
        for step in 1..10 {
            let t = step as f32 / 10.0;
            let e_open = eval_drawer_progress(t, false);
            let t_close = invert_drawer_progress(e_open, true);
            assert!((eval_drawer_progress(t_close, true) - e_open).abs() < 1e-4);

            let e_close = eval_drawer_progress(t, true);
            let t_reopen = invert_drawer_progress(e_close, false);
            assert!((eval_drawer_progress(t_reopen, false) - e_close).abs() < 1e-4);
        }

        // 向下滑出时：e=0 完全藏于顶部栏上方（card_top = -card_h），e=1 落位在 gap 处
        assert_eq!(compute_drawer_card_top(80, 4, 4, true, 0.0), -80);
        assert_eq!(compute_drawer_card_top(80, 4, 4, true, 1.0), 4);
        // 向上翻转时：e=0 完全藏于锚点下方（card_top = card_h + gap），e=1 落位在 0 处
        assert_eq!(compute_drawer_card_top(80, 4, 0, false, 0.0), 84);
        assert_eq!(compute_drawer_card_top(80, 4, 0, false, 1.0), 0);
    }

    #[test]
    fn popover_open_tick_reverse_and_close_animate_smoothly_with_clipping() {
        let mut p = Popover::new(std::ptr::null_mut(), 96).expect("Popover test window should create");
        let items: Vec<TrayItem> = (0..8).map(|i| item(&format!("k{i}"), &format!("App {i}"))).collect();
        let anchor = RECT { left: 1600, top: 0, right: 1630, bottom: 30 };
        let app = Appearance::default();

        // 1. 展开首帧即有探出过程，尚未完全到位且处于顶部栏下沿裁切滑出状态，且保持非激活悬浮窗样式
        p.open(anchor, items.clone(), vec!["k0".into()], HWND::default(), &app);
        assert!(p.is_open());
        assert!(p.is_animating());
        unsafe {
            let ex = GetWindowLongPtrW(p.hwnd, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0, "收纳浮层必须具备 WS_EX_NOACTIVATE 防止抢夺前景焦点");
            assert_eq!(
                SendMessageW(p.hwnd, WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)).0,
                MA_NOACTIVATE as isize
            );
            assert_ne!(GetForegroundWindow(), p.hwnd, "打开收纳浮层不应改变前台激活窗口");
        }
        assert!(p.visible_screen_rect().is_some());
        assert!(p.drawer.card_top < p.drawer.resting_card_top, "首帧卡片顶部({})应从顶部栏后方滑出(< {})", p.drawer.card_top, p.drawer.resting_card_top);
        assert!(p.drawer.card_top + p.drawer.card_h > 0, "首帧卡片底边应已探出顶部栏下沿");
        assert!(p.visual_progress() > 0.1 && p.visual_progress() < 0.5);
        let mut wr = RECT::default();
        unsafe {
            let _ = GetWindowRect(p.hwnd, &mut wr);
        }
        assert_eq!(wr.top, anchor.bottom, "向下滑出时浮层窗口顶边应紧贴顶部栏下沿");
        let open_stride = p.target.w as usize;
        let open_pixels = unsafe { std::slice::from_raw_parts(p.target.ptr as *const u32, open_stride * p.target.h as usize) };
        assert!((open_pixels[p.w as usize / 2] >> 24) > 200, "首帧顶部栏下沿第一行应显示已滑出的卡片内容");

        // 展开中途若托盘列表异步刷新新增条目，应保持平滑动画不回弹
        let mut expanded_items = items.clone();
        for i in 8..14 {
            expanded_items.push(item(&format!("k{i}"), &format!("App {i}")));
        }
        let _ = p.tick(16.0, false);
        let progress_before_refresh = p.visual_progress();
        p.update_items(expanded_items.clone());
        assert!(p.is_animating());
        assert!((p.visual_progress() - progress_before_refresh).abs() < 1e-6);

        // 首帧离屏像素校验：窗口顶部栏外第一行已有卡片底边像素，且整体在 160ms 内完整展开到位
        let mut prev_top = p.drawer.card_top;
        for _ in 0..12 {
            let _ = p.tick(16.0, false);
            assert!(p.drawer.card_top >= prev_top, "展开过程 card_top 应单调下移: {prev_top} -> {}", p.drawer.card_top);
            prev_top = p.drawer.card_top;
        }
        assert!(!p.is_animating());
        assert_eq!(p.drawer.card_top, p.drawer.resting_card_top);
        assert!((p.visual_progress() - 1.0).abs() < 1e-6);

        // 展开完成时顶部 gap_px 留白保持全透明，卡片本体区域首行具有不透明像素
        assert!(p.drawer.gap_px > 0);
        let stride = p.target.w as usize;
        let pixels = unsafe { std::slice::from_raw_parts(p.target.ptr as *const u32, stride * p.target.h as usize) };
        assert_eq!(pixels[p.w as usize / 2] >> 24, 0, "顶部栏与卡片间距首行应全透明");
        assert!((pixels[(p.drawer.resting_card_top as usize + 8) * stride + (p.w as usize / 2)] >> 24) > 200);

        // 2. 平滑收起，并在中途反向再次展开，验证连续接续
        assert!(p.begin_close(false));
        assert!(p.drawer.closing);
        assert!(!p.is_open());
        let _ = p.tick(24.0, false);
        let mid_top = p.drawer.card_top;
        assert!(mid_top < p.drawer.resting_card_top && p.visible);

        // 收起中途再次点击展开应从当前高度平滑反转展开，不重置回隐藏态
        p.open(anchor, expanded_items, vec!["k0".into()], HWND::default(), &app);
        assert!(p.is_open());
        assert!(!p.drawer.closing);
        assert!((p.drawer.card_top - mid_top).abs() <= 1, "反向展开不应跳变: mid={mid_top}, now={}", p.drawer.card_top);

        // 完整收起后应隐藏窗口并释放绘图缓冲
        assert!(p.begin_close(false));
        for _ in 0..10 {
            let (anim, _, closed) = p.tick(16.0, false);
            if closed {
                assert!(!anim);
                break;
            }
        }
        assert!(!p.visible);
        assert!(!p.drawer.closing);
        assert!(p.target.ptr.is_null());

        // 3. 减弱动态效果模式下应立即到位与立即收起
        let mut reduced = Appearance::default();
        reduced.reduce_motion = true;
        p.open(anchor, items, vec![], HWND::default(), &reduced);
        assert!(p.is_open());
        assert!(!p.is_animating());
        assert_eq!(p.drawer.card_top, p.drawer.resting_card_top);
        assert!(!p.begin_close(true));
        assert!(!p.visible);

        p.destroy();
    }
}

pub struct Popover {
    pub hwnd: HWND,
    pub target: DibTarget,
    pub scratch: DibTarget,
    pub font: HFONT,
    pub dpi: u32,
    pub w: i32,
    pub h: i32,
    pub drawer: DrawerAnim,
    pub scroll: usize,
    pub hover_idx: Option<usize>,
    pub press: Option<usize>,
    pub visible: bool,
    pub items: Vec<TrayItem>,
    pub pinned: Vec<String>,
    pub dark: bool,
    pub accent: (f32, f32, f32),
    anchor: RECT,
    prev_fg: HWND,
    context_key: Option<String>,
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
            // 使用非激活悬浮窗样式，避免打开收纳格时抢走前台窗口焦点
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
                dpi,
                w: 0,
                h: 0,
                drawer: DrawerAnim::default(),
                scroll: 0,
                hover_idx: None,
                press: None,
                visible: false,
                items: Vec::new(),
                pinned: Vec::new(),
                dark: false,
                accent: (0.04, 0.52, 1.0),
                anchor: RECT::default(),
                prev_fg: HWND::default(),
                context_key: None,
            })
        }
    }

    pub fn cell_sz(&self) -> i32 {
        // 与顶部栏 30dp 固定图标格保持接近，四周留白紧凑
        dp(30, self.dpi)
    }

    pub fn cell_gap(&self) -> i32 {
        dp(3, self.dpi)
    }

    pub fn pad(&self) -> i32 {
        dp(7, self.dpi)
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

    /// 是否处于打开态（未进入收起动画）。
    pub fn is_open(&self) -> bool {
        self.visible && !self.drawer.closing
    }

    /// 是否仍有展开或收起动画需要逐帧推进。
    pub fn is_animating(&self) -> bool {
        self.drawer.is_animating(self.visible)
    }

    /// 返回给顶部栏收纳按钮箭头旋转使用的缓动进度（0 = 收起左箭头，1 = 展开下箭头）。
    pub fn visual_progress(&self) -> f32 {
        self.drawer.visual_progress(self.visible)
    }

    /// 返回收纳卡片在屏幕坐标系中的当前可见区域（不含顶部透明间距）。
    pub fn visible_screen_rect(&self) -> Option<RECT> {
        if !self.visible {
            return None;
        }
        let wr = crate::win32::window_rect(self.hwnd)?;
        self.drawer.visible_screen_rect(wr, self.w, self.h)
    }

    /// 打开浮层：锚定在顶部栏 `<` 按钮正下方、右对齐，自动避让屏幕边界并从顶部栏下沿平滑滑出。
    pub fn open(&mut self, anchor: RECT, mut items: Vec<TrayItem>, pinned: Vec<String>, prev_fg: HWND, appearance: &Appearance) {
        sort_items(&mut items);
        if !prev_fg.0.is_null() && prev_fg != self.hwnd {
            self.prev_fg = prev_fg;
        }
        let was_visible = self.visible;
        let same_closing = was_visible && self.drawer.closing;
        let keep_in_place = was_visible && !self.drawer.closing;
        self.items = items;
        self.pinned = pinned;
        self.scroll = 0;
        self.hover_idx = None;
        self.press = None;
        self.context_key = None;
        self.dark = appearance.is_dark();
        self.accent = accent_from_config(appearance);
        self.anchor = anchor;
        self.drawer.begin_open(was_visible, same_closing, keep_in_place, appearance.reduce_motion, true);

        self.resize_to_items();
        unsafe {
            self.visible = true;
            self.render();
            if !was_visible {
                // 不激活收纳浮层，保持原前台窗口的激活态与 Mica 材质表现
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            }
        }
    }

    /// 注册项后续增减时同步格子与窗口范围，避免留下旧的鼠标命中位置。
    pub fn update_items(&mut self, mut items: Vec<TrayItem>) {
        sort_items(&mut items);
        let layout_changed = !self.items.iter().map(|item| &item.key).eq(items.iter().map(|item| &item.key));
        self.items = items;
        if layout_changed {
            self.scroll = self.scroll.min(self.scroll_max());
            self.hover_idx = None;
            self.press = None;
            self.resize_to_items();
        }
        self.render();
    }

    fn resize_to_items(&mut self) {
        let anchor = self.anchor;
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

        self.w = if self.items.is_empty() {
            card_w.max(dp(140, self.dpi))
        } else {
            card_w
        };

        let want_gap = dp(4, self.dpi);
        unsafe {
            let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                ..Default::default()
            };
            let mon = windows::Win32::Graphics::Gdi::MonitorFromRect(&anchor, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
            let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
            let bounds = compute_anchored_drawer_bounds(anchor, mi.rcWork, self.w, card_h, want_gap, 8, true);
            self.h = bounds.win_h;
            self.drawer.configure_bounds(card_h, bounds.gap_px, bounds.resting_card_top, bounds.open_down);
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, bounds.left, bounds.win_top, self.w, self.h, SWP_NOACTIVATE | SWP_NOSENDCHANGING);
        }
    }

    /// 右键菜单接管焦点后继续高亮来源图标。
    pub fn set_context_key(&mut self, key: Option<String>) {
        self.context_key = key;
        self.render();
    }

    /// 触发平滑收起动画；若处于减弱动态效果模式则立即隐藏。返回是否仍需帧驱动继续推进。
    pub fn begin_close(&mut self, reduce: bool) -> bool {
        if !self.visible {
            return false;
        }
        self.hover_idx = None;
        self.press = None;
        self.context_key = None;
        // 收起动作启动瞬间即归还原应用前台焦点，避免关闭动画期间键盘操作延迟
        crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
        if !self.drawer.begin_close(reduce) {
            self.finish_close();
            return false;
        }
        self.render();
        true
    }

    /// 推进一帧展开/收起动画；返回 `(是否仍在动画, 视觉进度是否变化, 本帧是否刚完成收起)`。
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

    pub fn close(&mut self) {
        if self.visible {
            // 收纳格隐藏前若仍持有前台焦点，主动归还原窗口防止前台变空导致顶部栏转黑
            crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
            self.finish_close();
        } else {
            self.drawer.reset_closed();
            self.target.release();
            self.scratch.release();
        }
    }

    fn finish_close(&mut self) {
        self.visible = false;
        self.drawer.reset_closed();
        self.hover_idx = None;
        self.press = None;
        self.context_key = None;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.target.release();
        self.scratch.release();
    }

    pub fn destroy(&mut self) {
        self.close();
        unsafe {
            let _ = SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            if !self.font.is_invalid() {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.font.0));
            }
        }
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
        let top = self.drawer.card_top + pad + r as i32 * (cell + gap);
        Some(RECT {
            left,
            top,
            right: left + cell,
            bottom: top + cell,
        })
    }

    pub fn index_at(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || (self.w > 0 && x >= self.w) || (self.h > 0 && y >= self.h) {
            return None;
        }
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

    /// 按住移出原图标格时取消点击，防止跨格移动改变固定状态。
    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        let hit = self.index_at(x, y);
        let canceled = self.press.is_some() && self.press != hit;
        if canceled {
            self.press = None;
        }
        if canceled || hit != self.hover_idx {
            self.hover_idx = hit;
            self.render();
        }
    }

    pub fn on_mouse_leave(&mut self) {
        let was_hovering = self.hover_idx.take().is_some();
        let was_pressed = self.press.take().is_some();
        if was_hovering || was_pressed {
            self.render();
        }
    }

    /// 渲染紧凑图标网格、固定圆点以及悬停和按下反馈。
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

        sf.fill_rrect(&card, 10.0 * s, bg);
        sf.stroke_rrect(&card, 10.0 * s, 1.0, bd);

        if self.items.is_empty() {
            draw_transparent_text_aligned(&mut sf, &mut self.scratch, "暂无打开的应用", self.font, &card, fg_dim, TextAlign::Center, false, s);
        } else {
            let start = self.scroll * COLS;
            let end = (start + MAX_VISIBLE_ROWS * COLS).min(self.items.len());
            let app_icons = crate::app::app_icon_cache();
            let tray_icons = crate::app::tray_icon_cache();

            for idx in start..end {
                let Some(r) = self.cell_rect(idx) else { continue };
                if r.bottom <= 0 || r.top >= h {
                    continue;
                }
                let item = &self.items[idx];
                let is_pinned = self.pinned.iter().any(|k| k == &item.key);
                let is_hover = self.hover_idx == Some(idx);
                let is_press = self.press == Some(idx);
                let is_context = self.context_key.as_deref() == Some(item.key.as_str());

                if is_press || is_context {
                    sf.fill_rrect(&r, 7.0 * s, press_bg);
                } else if is_hover {
                    sf.fill_rrect(&r, 7.0 * s, hover_bg);
                } else if is_pinned {
                    sf.fill_rrect(&r, 7.0 * s, accent_bg);
                }
                if is_pinned || is_context {
                    sf.stroke_rrect(&r, 7.0 * s, 1.0, accent_bd);
                }

                let cx = (r.left + r.right) as f32 * 0.5;
                let cy = (r.top + r.bottom) as f32 * 0.5;
                let icon_sz = 18.0 * s;
                draw_shelf_item_icon(&mut sf, &mut self.scratch, self.font, item, &app_icons, &tray_icons, cx, cy, icon_sz, self.dark, s);

                // 右上角固定指示小圆点
                if is_pinned {
                    let dx = r.right as f32 - 4.5 * s;
                    let dy = r.top as f32 + 4.5 * s;
                    sf.fill_circle(dx, dy, 2.1 * s, (self.accent.0, self.accent.1, self.accent.2, 0.98));
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
        .and_then(|k| tray_icons.displayed_icon(k))
        .or_else(|| tray_icons.get(&format!("win:{}", item.key)))
        .or_else(|| tray_icons.get(&item.key))
    {
        sf.draw_icon_adaptive(&px, cx, cy, size, dark, false, 0.0);
        return;
    }
    if let Some(exe) = &item.app_exe {
        if let Some(px) = app_icons.pixels(exe, DOCK_ICON_SRC_SIZE) {
            sf.draw_icon_adaptive(&px, cx, cy, size, dark, false, 0.0);
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
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
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
        WM_RBUTTONDOWN => {
            crate::app::with_app(hwnd, |app| app.popover_rbutton_down());
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
                crate::app::with_app(hwnd, |app| app.popover_deactivated(hwnd, HWND(lparam.0 as *mut _)));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
