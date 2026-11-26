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

pub const CLASS_TOPBAR: &str = "WinDockFinderTopBar";
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
        let brush: windows::Win32::Graphics::Gdi::HBRUSH =
            std::mem::transmute(windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH));
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(topbar_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: brush,
            lpszClassName: crate::win32::wide_static(CLASS_TOPBAR),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

#[derive(Clone, Debug)]
pub struct TopbarDrag {
    pub key: String,
    pub x: i32,
    pub insert_before: Option<String>,
    pub insert_x: i32,
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
    pub popover: Option<Popover>,
    pub mode: ColorMode,
    pub fused_from: Rgb,
    pub fused_to: Rgb,
    pub fused_t: f32,
    pub fused_target: Option<Rgb>,
    pub cur_fused: Rgb,
    pub dark_text: bool,
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
                w!("WinDockFinder 顶部栏"),
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
                popover: None,
                mode: ColorMode::Glass,
                fused_from: appearance.topbar_base_rgb(),
                fused_to: appearance.topbar_base_rgb(),
                fused_t: 1.0,
                fused_target: None,
                cur_fused: appearance.topbar_base_rgb(),
                dark_text: !appearance.is_dark(),
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

    fn setup_dwm(&mut self, appearance: &Appearance) {
        use windows::Win32::Graphics::Dwm::*;
        unsafe {
            let dark: u32 = if appearance.is_dark() { 1 } else { 0 };
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

    pub fn apply_config(&mut self, cfg: &TopbarCfg, appearance: &Appearance) {
        self.exclude = cfg.adaptive_color_exclude.clone();
        self.setup_dwm(appearance);
        self.height = dp(cfg.height_dp, self.dpi);
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

    /// 设置最大化融合目标色（None = 回到桌面统一纯净色）。
    pub fn set_fused_target(&mut self, target: Option<Rgb>, base: Rgb, instant: bool) {
        let same = match (target, self.fused_target) {
            (None, None) => self.mode == ColorMode::Glass && self.cur_fused == base,
            (Some(a), Some(b)) => self.mode == ColorMode::Fused && a == b,
            _ => false,
        };
        if same && self.fused_t >= 1.0 {
            return;
        }
        // 从非融合态进入最大化时以当前桌面统一色为过渡起点，避免沿用陈旧窗口色闪烁
        self.fused_from = if self.mode == ColorMode::Fused { self.cur_fused } else { base };
        self.fused_target = target;
        if let Some(t) = target {
            self.mode = ColorMode::Fused;
            self.fused_to = t;
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
            self.fused_to = base;
            self.cur_fused = base;
            self.fused_t = 1.0;
            self.dark_text = base.use_dark_text();
        }
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    /// 推进颜色过渡（150ms 平滑插值）；返回是否仍在过渡。
    pub fn tick_color(&mut self, dt_ms: f32, transition_ms: u32) -> bool {
        if self.fused_t >= 1.0 || self.mode != ColorMode::Fused {
            return false;
        }
        self.fused_t = (self.fused_t + dt_ms / transition_ms.max(1) as f32).min(1.0);
        self.cur_fused = Rgb::lerp(self.fused_from, self.fused_to, self.fused_t);
        self.dark_text = self.cur_fused.use_dark_text();
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
}

unsafe extern "system" fn topbar_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            crate::app::with_app(hwnd, |app| app.paint_topbar(hwnd));
            return LRESULT(0);
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
            // 仅响应系统主题变化，严格忽略 SPI_SETWORKAREA(0x002F) 避免 AppBar 自我触发无限重建闪烁
            if is_theme_setting_change(lparam.0) {
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

#[allow(dead_code)]
fn _unused(_: HWINEVENTHOOK, _: POINT) {}
