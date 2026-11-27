//! 顶部菜单栏窗口：多屏 AppBar、命中区域布局、原生菜单调度与焦点归还。

pub mod color_sync;
pub mod panel;
pub mod render;
pub mod status;
pub mod tray_mirror;
pub mod tray_popover;
pub(crate) mod tray_tooltip;
mod tray_popup;
mod tray_registry;

use std::time::Instant;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{HFONT, InvalidateRect};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABE_TOP, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Appearance, TopbarCfg};
use crate::topbar::color_sync::Rgb;
use crate::topbar::status::StatusSnapshot;
use crate::topbar::tray_mirror::TrayItem;
use crate::topbar::tray_popover::Popover;
use crate::ui::{make_font, DibTarget};
use crate::win32::{dp, MonitorInfo};

pub const CLASS_TOPBAR: &str = "WinDockTopBar";
pub const WM_APP_APPBAR: u32 = WM_APP + 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    ProductMenu,
    AppMenu,
    MenuFile,
    MenuWindow,
    MenuGo,
    TrayToggle,
    TrayIcon(String),
    Ime,
    Network,
    Brightness,
    Volume,
    Battery,
    Control,
    Clock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Glass,
    Fused,
}

static CLASSES_REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn register_classes(hinst: windows::Win32::Foundation::HINSTANCE) {
    if CLASSES_REGISTERED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    unsafe {
        // 使用空背景画刷，避免窗口重绘或激活切换时系统先刷出黑底闪烁
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(topbar_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
            lpszClassName: crate::win32::wide_static(CLASS_TOPBAR),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

#[derive(Clone, Debug)]
pub struct TopbarDrag {
    /// 被拖拽的托盘固定键
    pub key: String,
    /// 当前鼠标在顶部栏客户区的 X 坐标
    pub x: i32,
    /// 按下时槽位中心相对鼠标的偏移，保证起拖不跳变
    pub grab_dx: i32,
    /// 起拖前在可见托盘槽位中的索引（从左到右）
    pub from_idx: usize,
    /// 当前目标槽位索引（从左到右）
    pub target_idx: usize,
    /// 当前目标槽位矩形（用于渲染落位指示）
    pub target_rect: RECT,
}

/// 收集顶部栏当前可见的托盘固定图标槽位（按屏幕从左到右排列）。
pub fn tray_slots_sorted(hits: &[(RECT, Hit)]) -> Vec<(String, RECT)> {
    let mut slots: Vec<(String, RECT)> = hits
        .iter()
        .filter_map(|(r, h)| match h {
            Hit::TrayIcon(k) => Some((k.clone(), *r)),
            _ => None,
        })
        .collect();
    // hits 从右向左生成，按屏幕上从左到右排列，匹配 pinned_tray_keys 的视觉顺序（rev()）
    slots.sort_by_key(|(_, r)| r.left);
    slots
}

/// 根据拖拽图标中心 `preview_cx` 计算最近的可见目标槽位索引与矩形。
pub fn compute_tray_drag_target(slots: &[(String, RECT)], preview_cx: f32) -> Option<(usize, RECT)> {
    slots
        .iter()
        .enumerate()
        .min_by(|(_, (_, a)), (_, (_, b))| {
            let ca = (a.left + a.right) as f32 * 0.5;
            let cb = (b.left + b.right) as f32 * 0.5;
            (preview_cx - ca)
                .abs()
                .partial_cmp(&(preview_cx - cb).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, (_, r))| (idx, *r))
}

/// 计算拖拽过程中某个可见托盘槽位应移动到的让位槽位索引。
pub fn visual_tray_slot_index(orig_idx: usize, from_idx: usize, target_idx: usize) -> usize {
    if orig_idx == from_idx {
        return target_idx;
    }
    if target_idx > from_idx && orig_idx > from_idx && orig_idx <= target_idx {
        orig_idx - 1
    } else if target_idx < from_idx && orig_idx >= target_idx && orig_idx < from_idx {
        orig_idx + 1
    } else {
        orig_idx
    }
}

/// 根据视觉从左到右的最终目标索引 `target_idx` 重排可见固定图标并拼回不可见项。
pub fn reorder_pinned_tray_keys(
    ordered_visible: &[String],
    existing_pinned: &[String],
    key: &str,
    target_idx: usize,
) -> Vec<String> {
    let mut ordered = ordered_visible.to_vec();
    if let Some(old_pos) = ordered.iter().position(|k| k == key) {
        let item = ordered.remove(old_pos);
        let target = target_idx.min(ordered.len());
        ordered.insert(target, item);
    }
    // TopBar::layout 使用 pinned.iter().rev() 从右向左推入，因此 pinned 自身就是从左到右的视觉顺序
    let mut merged = ordered;
    for k in existing_pinned {
        if !merged.contains(k) {
            merged.push(k.clone());
        }
    }
    merged
}

pub struct TopBar {
    pub hwnd: HWND,
    pub monitor: MonitorInfo,
    pub dpi: u32,
    pub height: i32,
    pub appbar: bool,
    pub target: DibTarget,
    pub font: HFONT,
    pub font_bold: HFONT,
    pub scratch: DibTarget,
    pub hits: Vec<(RECT, Hit)>,
    pub hover: Option<Hit>,
    pub active_hit: Option<Hit>,
    pub press_icon: Option<(String, i32, i32)>,
    pub drag: Option<TopbarDrag>,
    pub tray_draw_x: Vec<(String, f32)>,
    pub tray_drop_settle: bool,
    pub last_drag_paint: Option<Instant>,
    pub popover: Option<Popover>,
    pub mode: ColorMode,
    pub fused_from: Rgb,
    pub fused_to: Rgb,
    pub fused_t: f32,
    pub fused_target: Option<Rgb>,
    pub cur_fused: Rgb,
    pub dark_text: bool,
    // 当前显示器桌面壁纸模糊条带及对应的基础色/文字极性缓存
    pub desktop_bgra: Vec<u32>,
    pub desktop_w: i32,
    pub desktop_h: i32,
    pub desktop_base: Rgb,
    pub desktop_dark_text: bool,
    pub fused_from_desktop: bool,
    // 当前已融合或待采样的外部最大化窗口句柄，以及是否需要重新取色
    pub fused_hwnd: isize,
    pub color_dirty: bool,
    pub active_app: String,
    pub status: StatusSnapshot,
    pub tray_items: Vec<TrayItem>,
    pub pinned: Vec<String>,
    pub visible: bool,
    pub hidden_by_fullscreen: bool,
    pub wanted_visible: bool,
    pub exclude: Vec<String>,
    pub tray_icon_dark: bool,
}

impl TopBar {
    pub fn new(
        app_ptr: *mut crate::app::App,
        monitor: MonitorInfo,
        cfg: &TopbarCfg,
        appearance: &Appearance,
    ) -> Result<TopBar, String> {
        register_classes(crate::win32::hinstance());
        unsafe {
            let height = dp(cfg.height_dp, monitor.dpi);
            // 顶部栏始终置顶且不抢夺前台应用焦点
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                crate::win32::wide_static(CLASS_TOPBAR),
                w!("WinDock 顶部栏"),
                WS_POPUP,
                monitor.rect.left,
                monitor.rect.top,
                monitor.width(),
                height,
                None,
                None,
                crate::win32::hinstance(),
                Some(app_ptr as *const _),
            )
            .map_err(crate::win32::error_string)?;
            let backdrop = crate::topbar::color_sync::desktop_topbar_backdrop(
                monitor.rect,
                height,
                &appearance.theme,
                appearance.opacity,
                false,
            );
            let dw = monitor.width().max(1);
            let dh = height.max(1);
            let desktop_bgra = backdrop.render_to_size(dw, dh);
            let base_rgb = backdrop.base_rgb;
            let desktop_dark_text = backdrop.dark_text;
            let mut bar = TopBar {
                hwnd,
                dpi: monitor.dpi,
                height,
                monitor,
                appbar: false,
                target: DibTarget::default(),
                font: make_font(dp(13, monitor.dpi), 400),
                font_bold: make_font(dp(13, monitor.dpi), 600),
                scratch: DibTarget::default(),
                hits: Vec::new(),
                hover: None,
                active_hit: None,
                press_icon: None,
                drag: None,
                tray_draw_x: Vec::new(),
                tray_drop_settle: false,
                last_drag_paint: None,
                popover: None,
                mode: ColorMode::Glass,
                fused_from: base_rgb,
                fused_to: base_rgb,
                fused_t: 1.0,
                fused_target: None,
                cur_fused: base_rgb,
                dark_text: desktop_dark_text,
                desktop_bgra,
                desktop_w: dw,
                desktop_h: dh,
                desktop_base: base_rgb,
                desktop_dark_text,
                fused_from_desktop: false,
                fused_hwnd: 0,
                color_dirty: false,
                active_app: "桌面".into(),
                status: StatusSnapshot::default(),
                tray_items: Vec::new(),
                pinned: Vec::new(),
                visible: false,
                hidden_by_fullscreen: false,
                wanted_visible: true,
                exclude: cfg.adaptive_color_exclude.clone(),
                tray_icon_dark: true,
            };
            bar.setup_dwm(appearance);
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
            Ok(bar)
        }
    }

    fn setup_dwm(&mut self, _appearance: &Appearance) {
        use windows::Win32::Graphics::Dwm::*;
        unsafe {
            let dark: u32 = if self.desktop_dark_text { 0 } else { 1 };
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const u32 as *const _, 4);
            let bt = DWMSBT_NONE.0;
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &bt as *const i32 as *const _, 4);
            let pref = DWMWCP_DONOTROUND.0 as u32;
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const u32 as *const _, 4);
            // 关闭系统默认窗口边框色，避免无边框顶部栏出现杂色细框
            let border: u32 = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_BORDER_COLOR, &border as *const u32 as *const _, 4);
        }
    }

    /// 重新采样并渲染当前显示器的桌面壁纸背景条带。
    pub fn refresh_desktop_backdrop(&mut self, appearance: &Appearance) {
        let backdrop = crate::topbar::color_sync::desktop_topbar_backdrop(
            self.monitor.rect,
            self.height,
            &appearance.theme,
            appearance.opacity,
            false,
        );
        let dw = self.monitor.width().max(1);
        let dh = self.height.max(1);
        self.desktop_w = dw;
        self.desktop_h = dh;
        self.desktop_base = backdrop.base_rgb;
        self.desktop_dark_text = backdrop.dark_text;
        self.desktop_bgra = backdrop.render_to_size(dw, dh);
        if self.mode == ColorMode::Glass {
            self.fused_from = self.desktop_base;
            self.fused_to = self.desktop_base;
            self.cur_fused = self.desktop_base;
            self.dark_text = self.desktop_dark_text;
        }
    }

    pub fn apply_config(&mut self, cfg: &TopbarCfg, appearance: &Appearance) {
        self.exclude = cfg.adaptive_color_exclude.clone();
        self.height = dp(cfg.height_dp, self.dpi);
        self.refresh_desktop_backdrop(appearance);
        self.setup_dwm(appearance);
        self.layout(cfg);
        if self.visible {
            self.register_appbar(true);
            unsafe {
                let _ = InvalidateRect(self.hwnd, None, false);
            }
        }
    }

    /// 注册/更新 AppBar 工作区预留；`re` 为 true 表示重新计算位置。
    pub fn register_appbar(&mut self, re: bool) {
        unsafe {
            let mut data = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                hWnd: self.hwnd,
                uCallbackMessage: WM_APP_APPBAR,
                uEdge: ABE_TOP,
                rc: RECT {
                    left: self.monitor.rect.left,
                    top: self.monitor.rect.top,
                    right: self.monitor.rect.right,
                    bottom: self.monitor.rect.top + self.height,
                },
                ..Default::default()
            };
            if !self.appbar {
                SHAppBarMessage(ABM_NEW, &mut data);
                self.appbar = true;
            }
            let _ = SHAppBarMessage(ABM_QUERYPOS, &mut data);
            data.rc.bottom = data.rc.top + self.height;
            let _ = SHAppBarMessage(ABM_SETPOS, &mut data);
            let _ = re;
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                self.monitor.rect.left,
                self.monitor.rect.top,
                self.monitor.width(),
                self.height,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        }
    }

    pub fn remove_appbar(&mut self) {
        if self.appbar {
            unsafe {
                let mut data = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    hWnd: self.hwnd,
                    ..Default::default()
                };
                SHAppBarMessage(ABM_REMOVE, &mut data);
            }
            self.appbar = false;
        }
    }

    pub fn show(&mut self) {
        if !self.visible {
            self.visible = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            }
        }
        self.register_appbar(false);
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.remove_appbar();
    }

    /// 全屏让位：仅 SW_HIDE，不撤销 AppBar（避免后台窗口重排抖动）。
    pub fn set_fullscreen_hidden(&mut self, hidden: bool) {
        if self.hidden_by_fullscreen == hidden {
            return;
        }
        self.hidden_by_fullscreen = hidden;
        if !self.wanted_visible {
            return;
        }
        unsafe {
            if hidden {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            } else {
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            }
        }
    }

    pub fn apply_wanted(&mut self) {
        if self.wanted_visible {
            if !self.hidden_by_fullscreen {
                self.show();
            } else {
                self.register_appbar(false);
            }
        } else {
            self.hide();
        }
    }

    pub fn destroy(&mut self) {
        self.remove_appbar();
        if let Some(mut p) = self.popover.take() {
            p.destroy();
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            for f in [self.font, self.font_bold] {
                if !f.is_invalid() {
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(f.0));
                }
            }
        }
        self.target.release();
        self.scratch.release();
    }

    // ---------- 取色融合 ----------

    /// 设置最大化融合目标色（None = 回到桌面壁纸贴合态）。
    pub fn set_fused_target(&mut self, target: Option<Rgb>, base: Rgb, instant: bool) {
        let same = match (target, self.fused_target) {
            (None, None) => self.mode == ColorMode::Glass && self.cur_fused == base,
            (Some(a), Some(b)) => self.mode == ColorMode::Fused && a == b,
            _ => false,
        };
        if same && self.fused_t >= 1.0 {
            return;
        }
        // 从非融合态进入最大化时记录桌面条带作像素级平滑过渡起点，避免沿用陈旧窗口色闪烁
        let was_glass = self.mode == ColorMode::Glass;
        self.fused_from = if self.mode == ColorMode::Fused { self.cur_fused } else { base };
        self.fused_target = target;
        if let Some(t) = target {
            self.mode = ColorMode::Fused;
            self.fused_to = t;
            self.fused_from_desktop = was_glass && !instant;
            self.fused_t = if instant { 1.0 } else { 0.0 };
            if instant {
                self.cur_fused = t;
                self.dark_text = t.use_dark_text();
            } else {
                self.cur_fused = self.fused_from;
                self.dark_text = t.use_dark_text();
            }
        } else {
            self.mode = ColorMode::Glass;
            self.fused_from_desktop = false;
            self.fused_to = base;
            self.cur_fused = base;
            self.fused_t = 1.0;
            self.dark_text = self.desktop_dark_text;
        }
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    /// 推进颜色过渡（150ms 平滑插值）；返回是否仍在过渡。
    pub fn tick_color(&mut self, dt_ms: f32, transition_ms: u32) -> bool {
        if self.fused_t >= 1.0 || self.mode != ColorMode::Fused {
            self.fused_from_desktop = false;
            return false;
        }
        self.fused_t = (self.fused_t + dt_ms / transition_ms.max(1) as f32).min(1.0);
        self.cur_fused = Rgb::lerp(self.fused_from, self.fused_to, self.fused_t);
        self.dark_text = self.cur_fused.use_dark_text();
        if self.fused_t >= 1.0 {
            self.fused_from_desktop = false;
        }
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
        self.fused_t < 1.0
    }

    // ---------- 布局 ----------

    /// 计算命中区域（左：产品菜单 + 前台应用；右：托盘/状态项/时钟）。
    pub fn layout(&mut self, cfg: &TopbarCfg) {
        let s = self.dpi as f32 / 96.0;
        let pad = (10.0 * s).round() as i32;
        let gap = (6.0 * s).round() as i32;
        let item_pad = (8.0 * s).round() as i32;
        let h = self.height;
        let mut hits: Vec<(RECT, Hit)> = Vec::new();
        // 左侧：macOS 风格 徽标 + 加粗当前应用名 + 文件 / 窗口 / 前往 菜单
        let mut x = pad;
        let logo = (16.0 * s).round() as i32;
        hits.push((
            RECT {
                left: (x - item_pad).max(2),
                top: 0,
                right: x + logo + item_pad,
                bottom: h,
            },
            Hit::ProductMenu,
        ));
        x += logo + item_pad + gap;
        if cfg.show_active_app {
            let app = self.active_app.clone();
            let w = self.measure(self.font_bold, &app).0;
            hits.push((
                RECT {
                    left: x - item_pad,
                    top: 0,
                    right: x + w + item_pad,
                    bottom: h,
                },
                Hit::AppMenu,
            ));
            x += w + item_pad * 2 + (4.0 * s).round() as i32;
        }
        for (label, hit) in [("文件", Hit::MenuFile), ("窗口", Hit::MenuWindow), ("前往", Hit::MenuGo)] {
            let w = self.measure(self.font, label).0;
            hits.push((
                RECT {
                    left: x - item_pad,
                    top: 0,
                    right: x + w + item_pad,
                    bottom: h,
                },
                hit,
            ));
            x += w + item_pad * 2 + (2.0 * s).round() as i32;
        }
        // 右侧（从右向左）
        let mut rx = self.monitor.width() - pad;
        let push_right = |rx: &mut i32, w: i32, hit: Hit, hits: &mut Vec<(RECT, Hit)>| {
            *rx -= w;
            hits.push((
                RECT {
                    left: *rx - item_pad,
                    top: 0,
                    right: *rx + w + item_pad,
                    bottom: h,
                },
                hit,
            ));
            *rx -= gap + item_pad;
        };
        let clock = self.status.clock.clone();
        let clock_w = self.measure(self.font, &clock).0;
        push_right(&mut rx, clock_w, Hit::Clock, &mut hits);
        let ctrl = (16.0 * s).round() as i32;
        push_right(&mut rx, ctrl, Hit::Control, &mut hits);
        if cfg.has_status("battery") {
            if let Some((pct, _)) = self.status.battery {
                let text = format!("{pct}%");
                push_right(&mut rx, (22.0 * s) as i32 + (4.0 * s) as i32 + self.measure(self.font, &text).0, Hit::Battery, &mut hits);
            }
        }
        // 从右向左排列：电量 → 亮度 → 音量，视觉顺序即为 音量 → 亮度 → 电量
        if cfg.has_status("brightness") {
            push_right(&mut rx, (16.0 * s) as i32, Hit::Brightness, &mut hits);
        }
        if cfg.has_status("volume") {
            if let Some((_, _)) = self.status.volume {
                push_right(&mut rx, (16.0 * s) as i32, Hit::Volume, &mut hits);
            }
        }
        if cfg.has_status("network") {
            push_right(&mut rx, (16.0 * s) as i32, Hit::Network, &mut hits);
        }
        if cfg.has_status("ime") && !self.status.ime.is_empty() {
            let ime = self.status.ime.clone();
            let w = self.measure(self.font, &ime).0 + (8.0 * s).round() as i32;
            push_right(&mut rx, w, Hit::Ime, &mut hits);
        }
        // 固定到顶部栏的托盘图标
        let icon = (16.0 * s).round() as i32;
        let pinned: Vec<String> = self.pinned.clone();
        for key in pinned.iter().rev() {
            if self.tray_items.iter().any(|t| &t.key == key) {
                push_right(&mut rx, icon, Hit::TrayIcon(key.clone()), &mut hits);
            }
        }
        push_right(&mut rx, (14.0 * s).round() as i32, Hit::TrayToggle, &mut hits);
        self.hits = hits;
        self.sync_drag_after_layout();
    }

    /// 布局重算后同步校验托盘拖拽状态，确保热更新或时钟变化后目标框不偏移。
    fn sync_drag_after_layout(&mut self) {
        let Some(cur_x) = self.drag.as_ref().map(|d| d.x) else { return };
        let still_exists = self
            .hits
            .iter()
            .any(|(_, h)| matches!(h, Hit::TrayIcon(k) if self.drag.as_ref().is_some_and(|d| &d.key == k)));
        if !still_exists {
            self.drag = None;
            self.tray_draw_x.clear();
            self.tray_drop_settle = false;
        } else {
            self.update_tray_drag(cur_x);
        }
    }

    fn measure(&mut self, font: HFONT, text: &str) -> (i32, i32) {
        if self.target.mem_dc.is_none() {
            let _ = self.target.ensure(64, 64);
        }
        match self.target.mem_dc {
            Some(dc) => crate::ui::measure_text(dc, text, font),
            None => (0, 0),
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| x >= r.left && x < r.right && y >= 0 && y < r.bottom).map(|(_, h)| h.clone())
    }

    pub fn rect_of(&self, hit: &Hit) -> Option<RECT> {
        self.hits.iter().find(|(_, h)| h == hit).map(|(r, _)| *r)
    }

    pub fn set_hover(&mut self, hit: Option<Hit>) -> bool {
        if self.hover != hit {
            self.hover = hit;
            unsafe {
                let _ = InvalidateRect(self.hwnd, None, false);
            }
            return true;
        }
        false
    }

    /// 进入顶部栏托盘图标拖拽排序模式，记录抓手偏移与初始位置。
    pub fn begin_tray_drag(&mut self, key: &str, press_x: i32, x: i32) -> bool {
        let slots = tray_slots_sorted(&self.hits);
        let Some(from_idx) = slots.iter().position(|(k, _)| k == key) else {
            return false;
        };
        let r = slots[from_idx].1;
        let slot_cx = (r.left + r.right) as f32 * 0.5;
        let half = ((r.right - r.left) as f32 * 0.5).max(1.0) as i32;
        let grab_dx = (slot_cx.round() as i32 - press_x).clamp(-half, half);
        self.tray_draw_x = slots
            .iter()
            .map(|(k, sr)| (k.clone(), (sr.left + sr.right) as f32 * 0.5))
            .collect();
        self.tray_drop_settle = false;
        self.last_drag_paint = None;
        self.hover = None;
        self.drag = Some(TopbarDrag {
            key: key.to_string(),
            x,
            grab_dx,
            from_idx,
            target_idx: from_idx,
            target_rect: r,
        });
        self.update_tray_drag(x);
        true
    }

    /// 根据当前鼠标坐标刷新顶部栏托盘图标拖拽预览与目标槽位。
    pub fn update_tray_drag(&mut self, x: i32) {
        let slots = tray_slots_sorted(&self.hits);
        let Some(d) = self.drag.as_mut() else { return };
        d.x = x;
        let Some(from_idx) = slots.iter().position(|(k, _)| k == &d.key) else { return };
        d.from_idx = from_idx;
        let preview_cx = (x + d.grab_dx) as f32;
        if let Some((target_idx, target_rect)) = compute_tray_drag_target(&slots, preview_cx) {
            d.target_idx = target_idx;
            d.target_rect = target_rect;
        }
    }

    /// 记录释放瞬间的拖拽图标位置，使图标在重排后平滑落入目标槽位。
    pub fn prepare_tray_drop_settle(&mut self, key: &str, drop_cx: f32) {
        if let Some(entry) = self.tray_draw_x.iter_mut().find(|(k, _)| k == key) {
            entry.1 = drop_cx;
        } else {
            self.tray_draw_x.push((key.to_string(), drop_cx));
        }
        self.tray_drop_settle = true;
    }

    /// 推进顶部栏托盘图标让位与落位平滑动画；返回是否仍需继续重绘。
    pub fn tick_tray_anim(&mut self, dt_ms: f32, reduce: bool) -> bool {
        if self.drag.is_none() && !self.tray_drop_settle {
            return false;
        }
        let slots = tray_slots_sorted(&self.hits);
        if slots.is_empty() {
            self.tray_draw_x.clear();
            self.tray_drop_settle = false;
            return false;
        }
        let mut still_moving = false;
        let mut changed = false;
        for (idx, (k, r)) in slots.iter().enumerate() {
            let target_cx = if let Some(d) = &self.drag {
                let vis = visual_tray_slot_index(idx, d.from_idx, d.target_idx);
                let tr = slots[vis].1;
                (tr.left + tr.right) as f32 * 0.5
            } else {
                (r.left + r.right) as f32 * 0.5
            };
            if let Some((_, cur)) = self.tray_draw_x.iter_mut().find(|(dk, _)| dk == k) {
                let next = crate::dock::layout::ease_to(
                    *cur,
                    target_cx,
                    dt_ms,
                    if reduce { 0.0 } else { 85.0 },
                    reduce,
                );
                if (next - *cur).abs() > 0.05 {
                    changed = true;
                }
                if (next - target_cx).abs() > 0.25 {
                    still_moving = true;
                }
                *cur = next;
            } else {
                self.tray_draw_x.push((k.clone(), target_cx));
                changed = true;
            }
        }
        if self.drag.is_none() && !still_moving {
            self.tray_drop_settle = false;
            self.tray_draw_x.clear();
            changed = true;
        }
        if changed {
            unsafe {
                let _ = InvalidateRect(self.hwnd, None, false);
            }
        }
        self.drag.is_some() || self.tray_drop_settle || still_moving
    }

    /// 获取托盘图标当前用于绘制的中心 X（拖拽让位或落位动画期间返回插值位置）。
    pub fn tray_draw_center(&self, key: &str) -> Option<f32> {
        if self.drag.is_some() || self.tray_drop_settle {
            self.tray_draw_x.iter().find(|(k, _)| k == key).map(|(_, x)| *x)
        } else {
            None
        }
    }
}

unsafe extern "system" fn topbar_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // 若主处理器正忙于模态或嵌套分发，交由默认过程校验绘制区，避免未 Validate 的黑底闪烁
            if !crate::app::handler_busy() {
                crate::app::with_app(hwnd, |app| app.paint_topbar(hwnd));
                return LRESULT(0);
            }
        }
        WM_MOUSEMOVE => {
            let x = crate::win32::get_x_lparam(lparam.0);
            let y = crate::win32::get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.topbar_mouse_move(hwnd, x, y));
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
            crate::app::with_app(hwnd, |app| app.topbar_mouse_leave(hwnd));
        }
        WM_LBUTTONDOWN => {
            let x = crate::win32::get_x_lparam(lparam.0);
            let y = crate::win32::get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.topbar_lbutton_down(hwnd, x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = crate::win32::get_x_lparam(lparam.0);
            let y = crate::win32::get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.topbar_lbutton_up(hwnd, x, y));
            return LRESULT(0);
        }
        WM_RBUTTONDOWN => {
            let x = crate::win32::get_x_lparam(lparam.0);
            let y = crate::win32::get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.topbar_click(hwnd, x, y, true));
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            crate::app::with_app(hwnd, |app| app.topbar_capture_lost(hwnd));
        }
        WM_APP_APPBAR => {
            // AppBar 回调：wparam 才是通知码（ABN_POSCHANGED=1 / ABN_FULLSCREENAPP=2）
            crate::app::with_app(hwnd, |app| app.topbar_appbar_notify(hwnd, wparam.0 as u32, lparam.0 != 0));
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            crate::app::with_app(hwnd, |app| app.topbar_dpi_changed(hwnd, lparam.0));
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => {
            crate::app::with_app(hwnd, |app| app.display_changed());
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            // 仅响应系统主题或壁纸变更，严格忽略 SPI_SETWORKAREA(0x002F) 避免 AppBar 自我触发无限重建闪烁
            if is_wallpaper_setting_change(wparam.0, lparam.0) {
                crate::topbar::color_sync::invalidate_wallpaper_cache();
                crate::app::with_app(hwnd, |app| app.on_theme_changed());
            } else if is_theme_setting_change(lparam.0) {
                crate::app::with_app(hwnd, |app| app.on_theme_changed());
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 判断 WM_SETTINGCHANGE 是否为系统浅/深主题或配色变化（而非工作区/任务栏尺寸变更）。
pub fn is_theme_setting_change(lparam: isize) -> bool {
    if lparam == 0 {
        return false;
    }
    let s = unsafe { windows::core::PCWSTR(lparam as *const u16).to_string().unwrap_or_default() };
    s.eq_ignore_ascii_case("ImmersiveColorSet") || s.eq_ignore_ascii_case("WindowsThemeElement")
}

/// 判断 WM_SETTINGCHANGE 是否为桌面壁纸切换事件。
pub fn is_wallpaper_setting_change(wparam: usize, lparam: isize) -> bool {
    if wparam as u32 == SPI_SETDESKWALLPAPER.0 {
        return true;
    }
    if lparam == 0 {
        return false;
    }
    let s = unsafe { windows::core::PCWSTR(lparam as *const u16).to_string().unwrap_or_default() };
    s.eq_ignore_ascii_case("Wallpaper") || s.eq_ignore_ascii_case("Control Panel\\Desktop")
}

#[allow(dead_code)]
fn _unused(_: HWINEVENTHOOK, _: POINT) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_drag_target_and_reorder_match_visual_slots() {
        let hits = vec![
            (RECT { left: 260, top: 0, right: 292, bottom: 30 }, Hit::TrayIcon("c".into())),
            (RECT { left: 230, top: 0, right: 262, bottom: 30 }, Hit::TrayIcon("b".into())),
            (RECT { left: 200, top: 0, right: 232, bottom: 30 }, Hit::TrayIcon("a".into())),
        ];
        let slots = tray_slots_sorted(&hits);
        let ordered_vis: Vec<String> = slots.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(ordered_vis, vec!["a", "b", "c"]);

        // 拖动最左 "a"(0) 向右跨过 "b"(1) 中心(246) 到达 248 -> 目标槽位 1，b 让位到 0
        let (target_idx, target_rect) = compute_tray_drag_target(&slots, 248.0).unwrap();
        assert_eq!(target_idx, 1);
        assert_eq!(target_rect.left, 230);
        assert_eq!(visual_tray_slot_index(1, 0, target_idx), 0);
        assert_eq!(visual_tray_slot_index(2, 0, target_idx), 2);

        // 重排后视觉顺序应为 [b, a, c]，与 pinned_tray_keys 前向顺序一致并保留隐藏项
        let existing = vec!["a".into(), "b".into(), "c".into(), "hidden".into()];
        let merged = reorder_pinned_tray_keys(&ordered_vis, &existing, "a", target_idx);
        assert_eq!(merged, vec!["b", "a", "c", "hidden"]);

        // 拖动最右 "c"(2) 向左跨过 "a"(0) 中心(216) 到达 210 -> 目标槽位 0
        let (target_left, _) = compute_tray_drag_target(&slots, 210.0).unwrap();
        assert_eq!(target_left, 0);
        assert_eq!(visual_tray_slot_index(0, 2, target_left), 1);
        assert_eq!(visual_tray_slot_index(1, 2, target_left), 2);
        let merged_left = reorder_pinned_tray_keys(&ordered_vis, &existing, "c", target_left);
        assert_eq!(merged_left, vec!["c", "a", "b", "hidden"]);
    }

    #[test]
    fn topbar_drag_sweep_preview_matches_drop_across_all_positions() {
        // 模拟 TopBar::layout push_right 产生的相邻 2px 重叠槽位（150% DPI）
        let hits = vec![
            (RECT { left: 538, top: 0, right: 586, bottom: 45 }, Hit::TrayIcon("k3".into())),
            (RECT { left: 493, top: 0, right: 541, bottom: 45 }, Hit::TrayIcon("k2".into())),
            (RECT { left: 448, top: 0, right: 496, bottom: 45 }, Hit::TrayIcon("k1".into())),
            (RECT { left: 403, top: 0, right: 451, bottom: 45 }, Hit::TrayIcon("k0".into())),
        ];
        let slots = tray_slots_sorted(&hits);
        let ordered_vis: Vec<String> = slots.iter().map(|(k, _)| k.clone()).collect();
        let existing = ordered_vis.clone();

        for from_idx in 0..4 {
            let drag_key = &ordered_vis[from_idx];
            for grab_dx in [-10, 0, 10] {
                for x in (380..=610).step_by(5) {
                    let (target_idx, target_rect) = compute_tray_drag_target(&slots, (x + grab_dx) as f32).unwrap();
                    assert_eq!(target_rect, slots[target_idx].1);

                    let mut occupied = [false; 4];
                    for orig in 0..4 {
                        if orig == from_idx {
                            continue;
                        }
                        let vis = visual_tray_slot_index(orig, from_idx, target_idx);
                        assert!(!occupied[vis], "顶部栏视觉槽位发生重叠");
                        occupied[vis] = true;
                    }
                    assert!(!occupied[target_idx], "顶部栏目标槽位未空出");

                    let new_pinned = reorder_pinned_tray_keys(&ordered_vis, &existing, drag_key, target_idx);
                    let final_pos = new_pinned.iter().position(|k| k == drag_key).unwrap();
                    assert_eq!(final_pos, target_idx, "顶部栏预览槽位 {target_idx} 与最终位置 {final_pos} 不一致");
                }
            }
        }
    }

    #[test]
    fn topbar_stateful_drag_clears_hover_animates_slots_and_settles() {
        let mon = crate::win32::primary_monitor().unwrap_or(MonitorInfo {
            hmonitor: windows::Win32::Graphics::Gdi::HMONITOR::default(),
            rect: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            work: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            dpi: 96,
            primary: true,
        });
        let cfg = TopbarCfg::default();
        let app_cfg = Appearance::default();
        let mut bar = TopBar::new(std::ptr::null_mut(), mon, &cfg, &app_cfg).expect("TopBar test window should create");
        let mk_item = |key: &str| TrayItem {
            key: key.into(),
            name: key.into(),
            native_tip: None,
            uia_key: None,
            registration: None,
            legacy_keys: Vec::new(),
            overflow: false,
            app_exe: None,
            app_hwnds: Vec::new(),
        };
        bar.tray_items = vec![mk_item("a"), mk_item("b"), mk_item("c")];
        // topbar.layout 按 pinned.iter().rev() 从右往左排，因此 pinned=["a","b","c"] 时屏幕从左到右正好为 ["a","b","c"]
        bar.pinned = vec!["a".into(), "b".into(), "c".into()];
        bar.layout(&cfg);
        bar.set_hover(Some(Hit::TrayIcon("a".into())));

        let slots = tray_slots_sorted(&bar.hits);
        assert_eq!(slots.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        let cx0 = (slots[0].1.left + slots[0].1.right) / 2;
        let cx2 = (slots[2].1.left + slots[2].1.right) / 2;

        // 按下 "a" 偏左 6px 起拖
        assert!(bar.begin_tray_drag("a", cx0 - 6, cx0 - 1));
        assert!(bar.hover.is_none(), "进入拖拽后应清除旧槽位悬停态");
        assert_eq!(bar.drag.as_ref().unwrap().grab_dx, 6);

        // 拖向 "c" 槽位中心并验证顶部栏离屏渲染无异常
        bar.update_tray_drag(cx2 - 6);
        assert_eq!(bar.drag.as_ref().unwrap().target_idx, 2);
        for _ in 0..30 {
            let _ = bar.tick_tray_anim(16.0, false);
        }
        assert!(crate::topbar::render::render_buffer(&mut bar, &cfg, &app_cfg, &crate::topbar::tray_mirror::TrayIconCache::default()).is_some());
        // "b" 应滑到原 "a" 槽位，"c" 应滑到原 "b" 槽位
        let cx1 = (slots[1].1.left + slots[1].1.right) as f32 * 0.5;
        assert!((bar.tray_draw_center("b").unwrap() - cx0 as f32).abs() < 1.0);
        assert!((bar.tray_draw_center("c").unwrap() - cx1).abs() < 1.0);

        // 释放并落位重排后，验证 TopBar::layout 实际布局与预览 ["b", "c", "a"] 100% 一致
        let d = bar.drag.take().unwrap();
        bar.prepare_tray_drop_settle(&d.key, (d.x + d.grab_dx) as f32);
        bar.pinned = reorder_pinned_tray_keys(
            &["a".into(), "b".into(), "c".into()],
            &bar.pinned,
            &d.key,
            d.target_idx,
        );
        bar.layout(&cfg);
        let after_slots = tray_slots_sorted(&bar.hits);
        assert_eq!(after_slots.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
        for _ in 0..30 {
            let _ = bar.tick_tray_anim(16.0, false);
        }
        assert!(!bar.tray_drop_settle);
        assert!(bar.tray_draw_center("a").is_none());

        bar.destroy();
    }

    #[test]
    fn topbar_tray_toggle_chevron_rotates_smoothly_with_popover_animation() {
        let mon = crate::win32::primary_monitor().unwrap_or(MonitorInfo {
            hmonitor: windows::Win32::Graphics::Gdi::HMONITOR::default(),
            rect: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            work: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            dpi: 96,
            primary: true,
        });
        let cfg = TopbarCfg::default();
        let app_cfg = Appearance::default();
        let mut bar = TopBar::new(std::ptr::null_mut(), mon, &cfg, &app_cfg)
            .expect("TopBar test window should create");
        bar.layout(&cfg);
        let toggle_rect = bar.rect_of(&Hit::TrayToggle).unwrap();
        let tray_icons = crate::topbar::tray_mirror::TrayIconCache::default();

        let snapshot_toggle_region = |bar: &TopBar| -> Vec<u32> {
            let w = bar.target.w as usize;
            let buf = unsafe {
                std::slice::from_raw_parts(bar.target.ptr as *const u32, w * bar.target.h as usize)
            };
            let mut region = Vec::new();
            for y in toggle_rect.top.max(0)..toggle_rect.bottom.min(bar.height) {
                for x in toggle_rect.left.max(0)..toggle_rect.right.min(bar.monitor.width()) {
                    region.push(buf[y as usize * w + x as usize]);
                }
            }
            region
        };

        // 1. 初始收起态：渲染左箭头 `<`
        assert!(crate::topbar::render::render_buffer(&mut bar, &cfg, &app_cfg, &tray_icons).is_some());
        let closed_px = snapshot_toggle_region(&bar);

        // 2. 展开首帧：箭头已开始向下方旋转，像素应不同于收起态
        let mut pop = Popover::new(std::ptr::null_mut(), bar.dpi).expect("Popover test window should create");
        let anchor = RECT { left: toggle_rect.left, top: 0, right: toggle_rect.right, bottom: bar.height };
        pop.open(anchor, Vec::new(), Vec::new(), HWND::default(), &app_cfg);
        bar.popover = Some(pop);
        assert!(crate::topbar::render::render_buffer(&mut bar, &cfg, &app_cfg, &tray_icons).is_some());
        let start_px = snapshot_toggle_region(&bar);
        assert_ne!(start_px, closed_px, "点击展开首帧旋转箭头应区别于收起态");

        // 3. 推进动画至完全展开到位：渲染下箭头 `v`，且不同于中途态
        for _ in 0..15 {
            if let Some(p) = bar.popover.as_mut() {
                let _ = p.tick(16.0, false);
            }
        }
        assert!(crate::topbar::render::render_buffer(&mut bar, &cfg, &app_cfg, &tray_icons).is_some());
        let open_px = snapshot_toggle_region(&bar);
        assert_ne!(open_px, start_px, "展开到位后箭头应与首帧过程态不同");
        assert_ne!(open_px, closed_px);

        bar.destroy();
    }
}
