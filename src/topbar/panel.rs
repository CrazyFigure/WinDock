//! 顶部栏右侧自绘交互状态面板（输入法 / 网络 / 声音 / 电池 / 控制中心 / 日历）。
//!
//! 单一复用分层窗口，点击外部或按 Esc 立即隐藏并释放 DIB 缓冲；
//! 避免无意义跳转到系统设置或微软商店，所有常用调节均在原位弹窗内直接完成。

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows::core::{w, BSTR, GUID, HRESULT, Interface, PCWSTR, VARIANT};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{COLORREF, FreeLibrary, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, SYSTEMTIME, WPARAM};
use windows::Win32::Globalization::{GetLocaleInfoW, LOCALE_SLOCALIZEDDISPLAYNAME};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetMonitorInfoW, AC_SRC_OVER, BLENDFUNCTION, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH};
use windows::Win32::NetworkManagement::WiFi::{
    DOT11_BSS_TYPE, WLAN_AVAILABLE_NETWORK_LIST, WLAN_CONNECTION_ATTRIBUTES,
    WLAN_CONNECTION_MODE, WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO_LIST,
};
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_ALL,
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE, STGM_READ,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerEnumerate, PowerGetActiveScheme, PowerReadFriendlyName,
    PowerSetActiveScheme, ACCESS_SCHEME, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::Ime::{ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd, ImmReleaseContext, IME_CMODE_NATIVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardLayoutList, ReleaseCapture, SendInput, SetCapture, SetFocus,
    TrackMouseEvent, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, TME_LEAVE,
    TRACKMOUSEEVENT, VK_ESCAPE, VK_LWIN, VK_N,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Appearance;
use crate::ui::*;
use crate::win32::{dp, get_x_lparam, get_y_lparam, wide, wide_static};

pub const CLASS_PANEL: &str = "WinDockFinderStatusPanel";
pub const WM_APP_PANEL_DATA: u32 = WM_APP + 35;

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

static REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Ime,
    Network,
    Brightness,
    Volume,
    Battery,
    Control,
    Clock,
}

#[derive(Clone, Debug)]
pub struct ImeLayoutItem {
    pub hkl: isize,
    pub name: String,
    pub is_chinese: bool,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct WifiNetItem {
    pub ssid: String,
    pub profile: String,
    pub quality: u32,
    pub secured: bool,
    pub connected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NetworkData {
    pub loading: bool,
    pub connected: bool,
    pub has_wifi: bool,
    pub iface_guid: Option<GUID>,
    pub active_name: String,
    pub ipv4: String,
    pub nets: Vec<WifiNetItem>,
}

#[derive(Clone, Debug)]
pub struct AudioDeviceItem {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct PowerSchemeItem {
    pub guid: GUID,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelHit {
    None,
    // 输入法
    ImeMode(bool),
    ImeLayout(usize),
    ImeSettings,
    // 网络
    NetRefresh,
    WifiNet(usize),
    NetSettings,
    // 亮度
    BrightSlider,
    // 声音
    VolMute,
    VolSlider,
    AudioDev(usize),
    SoundSettings,
    // 电池
    PowerScheme(usize),
    PowerSettings,
    // 控制中心
    CtrlTheme,
    CtrlNotify,
    CtrlLock,
    CtrlSleep,
    CtrlSettings,
    // 日历
    CalTitle,
    CalPickMonth(i32),
    CalPickYear(i32),
    CalPrev,
    CalToday,
    CalNext,
    CalNotify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalView {
    Days,
    Months,
    Years,
}

#[derive(Clone, Debug)]
pub enum PanelAction {
    None,
    Close,
    OpenSettings,
    NotificationCenter,
    SwitchKind(PanelKind),
    ToggleTheme,
    VolumeChanged(i32, bool),
    ImeChanged { close: bool },
    LockScreen,
    Sleep,
}

pub struct StatusPanel {
    pub hwnd: HWND,
    pub visible: bool,
    pub kind: PanelKind,
    dpi: u32,
    pub w: i32,
    pub h: i32,
    pub drawer: DrawerAnim,
    anchor: RECT,
    prev_fg: HWND,
    dark: bool,
    accent: (f32, f32, f32),
    hover: PanelHit,
    pressed: PanelHit,
    drag_slider: bool,
    drag_bright: bool,
    zones: Vec<(RECT, PanelHit)>,
    // 面板数据
    ime_layouts: Vec<ImeLayoutItem>,
    ime_native: bool,
    ime_cur_chinese: bool,
    net_data: NetworkData,
    net_token: u64,
    brightness: i32,
    bright_supported: bool,
    vol_level: i32,
    vol_mute: bool,
    audio_devs: Vec<AudioDeviceItem>,
    battery_pct: Option<(u8, bool, u32)>,
    power_saver: bool,
    power_schemes: Vec<PowerSchemeItem>,
    cal_year: i32,
    cal_month: i32,
    cal_view: CalView,
    today: (i32, i32, i32),
    // 绘图资源（关闭即释放）
    font: HFONT,
    font_bold: HFONT,
    font_sub: HFONT,
    font_big: HFONT,
    target: DibTarget,
    scratch: DibTarget,
}

impl Default for StatusPanel {
    fn default() -> Self {
        let now = local_ymd();
        Self {
            hwnd: HWND::default(),
            visible: false,
            kind: PanelKind::Control,
            dpi: 96,
            w: 0,
            h: 0,
            drawer: DrawerAnim::default(),
            anchor: RECT::default(),
            prev_fg: HWND::default(),
            dark: false,
            accent: (0.04, 0.52, 1.0),
            hover: PanelHit::None,
            pressed: PanelHit::None,
            drag_slider: false,
            drag_bright: false,
            zones: Vec::new(),
            ime_layouts: Vec::new(),
            ime_native: true,
            ime_cur_chinese: false,
            net_data: NetworkData::default(),
            net_token: 0,
            brightness: 80,
            bright_supported: true,
            vol_level: 50,
            vol_mute: false,
            audio_devs: Vec::new(),
            battery_pct: None,
            power_saver: false,
            power_schemes: Vec::new(),
            cal_year: now.0,
            cal_month: now.1,
            cal_view: CalView::Days,
            today: now,
            font: HFONT::default(),
            font_bold: HFONT::default(),
            font_sub: HFONT::default(),
            font_big: HFONT::default(),
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
            lpfnWndProc: Some(panel_proc),
            hInstance: crate::win32::hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: wide_static(CLASS_PANEL),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

impl StatusPanel {
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
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                wide_static(CLASS_PANEL),
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

    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        self.font = make_font(dp(13, self.dpi), 400);
        self.font_bold = make_font(dp(14, self.dpi), 600);
        self.font_sub = make_font(dp(11, self.dpi), 400);
        self.font_big = make_font(dp(22, self.dpi), 600);
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [self.font, self.font_bold, self.font_sub, self.font_big] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font = HFONT::default();
        self.font_bold = HFONT::default();
        self.font_sub = HFONT::default();
        self.font_big = HFONT::default();
    }

    /// 是否处于打开态（未进入收起动画）。
    pub fn is_open(&self) -> bool {
        self.visible && !self.drawer.closing
    }

    /// 是否仍有展开或收起动画需要逐帧推进。
    pub fn is_animating(&self) -> bool {
        self.drawer.is_animating(self.visible)
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

    pub fn open(
        &mut self,
        app_ptr: *mut crate::app::App,
        main_hwnd: HWND,
        kind: PanelKind,
        anchor: RECT,
        dpi: u32,
        prev_fg: HWND,
        appearance: &Appearance,
    ) {
        if !self.ensure_window(app_ptr, dpi) {
            return;
        }
        let was_visible = self.visible;
        let same_target = self.kind == kind && self.anchor == anchor;
        let same_closing = was_visible && same_target && self.drawer.closing;
        let keep_in_place = was_visible && same_target && !self.drawer.closing;
        self.kind = kind;
        self.anchor = anchor;
        if !prev_fg.0.is_null() && prev_fg != self.hwnd {
            self.prev_fg = prev_fg;
        }
        self.dark = appearance.is_dark();
        self.accent = accent_from_config(appearance);
        self.hover = PanelHit::None;
        self.pressed = PanelHit::None;
        self.drag_slider = false;
        self.drag_bright = false;
        unsafe {
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
        }
        self.cal_view = CalView::Days;

        // 统一接入顶部栏锚定抽屉展开动效，支持收起中途反向无跳变接续
        self.drawer
            .begin_open(was_visible, same_closing, keep_in_place, appearance.reduce_motion, true);
        self.refresh_data(main_hwnd);
        self.resize_and_position();

        self.visible = true;
        unsafe {
            self.render();
            if !was_visible {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
            }
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
        }
    }

    /// 启动平滑收起动画；若处于减弱动态效果模式则立即隐藏并释放缓冲。
    pub fn begin_close(&mut self, reduce: bool) -> bool {
        if !self.visible {
            return false;
        }
        self.hover = PanelHit::None;
        self.pressed = PanelHit::None;
        self.drag_slider = false;
        self.drag_bright = false;
        unsafe {
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
        }
        // 收起动作启动瞬间归还原外部前台窗口焦点，避免动画期间焦点落空
        crate::win32::restore_foreground_if_owned(self.hwnd, self.prev_fg);
        if !self.drawer.begin_close(reduce) {
            self.finish_close();
            return false;
        }
        self.render();
        true
    }

    /// 推进一帧状态面板展开/收起动画；返回 `(仍在动画, 进度有变化, 本帧刚完成收起)`。
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
        self.drag_slider = false;
        self.drag_bright = false;
        unsafe {
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
        }
        if self.visible {
            // 若面板关闭前仍持有焦点，先归还给原外部应用窗口，避免顶部栏色回落桌面色
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
        self.hover = PanelHit::None;
        self.pressed = PanelHit::None;
        self.zones.clear();
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
        self.destroy_fonts();
    }

    fn refresh_data(&mut self, main_hwnd: HWND) {
        match self.kind {
            PanelKind::Ime => {
                let (layouts, native, is_cn) = query_ime_layouts(self.prev_fg);
                self.ime_layouts = layouts;
                self.ime_native = native;
                self.ime_cur_chinese = is_cn;
            }
            PanelKind::Network => {
                self.net_data.loading = true;
                self.net_token = self.net_token.wrapping_add(1);
                spawn_network_query(main_hwnd, self.net_token);
            }
            PanelKind::Brightness => {
                if let Some(b) = read_system_brightness() {
                    self.brightness = b;
                    self.bright_supported = true;
                } else {
                    self.bright_supported = false;
                }
            }
            PanelKind::Volume => {
                if let Some((v, m)) = crate::topbar::status::volume() {
                    self.vol_level = v;
                    self.vol_mute = m;
                }
                self.audio_devs = query_audio_outputs();
            }
            PanelKind::Battery => {
                self.battery_pct = query_battery_detail();
                self.power_saver = query_power_saver();
                self.power_schemes = query_power_schemes();
            }
            PanelKind::Control => {
                if let Some((v, m)) = crate::topbar::status::volume() {
                    self.vol_level = v;
                    self.vol_mute = m;
                }
                self.net_data.connected = crate::topbar::status::network_connected();
            }
            PanelKind::Clock => {
                let now = local_ymd();
                self.today = now;
                self.cal_year = now.0;
                self.cal_month = now.1;
            }
        }
    }

    pub fn on_network_data(&mut self, token: u64, data: NetworkData) {
        if token != self.net_token {
            return;
        }
        self.net_data = data;
        if self.is_open() && self.kind == PanelKind::Network {
            self.resize_and_position();
            self.render();
        }
    }

    fn resize_and_position(&mut self) {
        let d = self.dpi;
        let (w, card_h) = match self.kind {
            PanelKind::Ime => {
                let rows = self.ime_layouts.len().clamp(1, 7) as i32;
                let extra = if self.ime_cur_chinese { dp(40, d) } else { 0 };
                (dp(260, d), dp(86, d) + extra + rows * dp(34, d))
            }
            PanelKind::Network => {
                let rows = self.net_data.nets.len().min(6) as i32;
                let list_h = if self.net_data.has_wifi {
                    rows.max(1) * dp(36, d)
                } else {
                    dp(30, d)
                };
                (dp(300, d), dp(140, d) + list_h)
            }
            PanelKind::Brightness => (dp(296, d), dp(96, d)),
            PanelKind::Volume => {
                let rows = self.audio_devs.len().clamp(1, 5) as i32;
                (dp(300, d), dp(132, d) + rows * dp(34, d))
            }
            PanelKind::Battery => {
                let rows = self.power_schemes.len().clamp(1, 4) as i32;
                (dp(286, d), dp(136, d) + rows * dp(34, d))
            }
            PanelKind::Control => (dp(296, d), dp(132, d)),
            PanelKind::Clock => (dp(292, d), dp(304, d)),
        };
        self.w = w;
        let want_gap = dp(4, d);
        unsafe {
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let mon = windows::Win32::Graphics::Gdi::MonitorFromRect(&self.anchor, MONITOR_DEFAULTTONEAREST);
            let _ = GetMonitorInfoW(mon, &mut mi);
            let bounds = compute_anchored_drawer_bounds(self.anchor, mi.rcWork, w, card_h, want_gap, 8, true);
            self.h = bounds.win_h;
            self.drawer
                .configure_bounds(card_h, bounds.gap_px, bounds.resting_card_top, bounds.open_down);
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                bounds.left,
                bounds.win_top,
                self.w,
                self.h,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        }
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) -> PanelAction {
        if !self.is_open() {
            return PanelAction::None;
        }
        if self.drag_slider {
            let act = self.apply_slider_x(x);
            self.render();
            return act;
        }
        if self.drag_bright {
            self.apply_bright_slider_x(x);
            self.render();
            return PanelAction::None;
        }
        let hit = self.hit_test(x, y);
        if hit != self.hover {
            self.hover = hit;
            self.render();
        }
        PanelAction::None
    }

    pub fn on_mouse_leave(&mut self) {
        if !self.is_open() {
            return;
        }
        if !self.drag_slider && !self.drag_bright && self.hover != PanelHit::None {
            self.hover = PanelHit::None;
            self.render();
        }
    }

    pub fn on_lbutton_down(&mut self, x: i32, y: i32) -> PanelAction {
        if !self.is_open() {
            return PanelAction::None;
        }
        let hit = self.hit_test(x, y);
        self.pressed = hit.clone();
        if hit == PanelHit::VolSlider {
            self.drag_slider = true;
            unsafe {
                let _ = SetCapture(self.hwnd);
            }
            let act = self.apply_slider_x(x);
            self.render();
            return act;
        }
        if hit == PanelHit::BrightSlider {
            self.drag_bright = true;
            unsafe {
                let _ = SetCapture(self.hwnd);
            }
            self.apply_bright_slider_x(x);
            self.render();
            return PanelAction::None;
        }
        self.render();
        PanelAction::None
    }

    pub fn on_lbutton_up(&mut self, main_hwnd: HWND, x: i32, y: i32) -> PanelAction {
        if !self.is_open() {
            return PanelAction::None;
        }
        if self.drag_slider || self.drag_bright {
            self.drag_slider = false;
            self.drag_bright = false;
            unsafe {
                let _ = ReleaseCapture();
            }
            self.pressed = PanelHit::None;
            self.render();
            return PanelAction::None;
        }
        let pressed = std::mem::replace(&mut self.pressed, PanelHit::None);
        let hit = self.hit_test(x, y);
        if pressed != PanelHit::None && pressed == hit {
            return self.activate(main_hwnd, hit);
        }
        self.render();
        PanelAction::None
    }

    fn slider_pct_from_x(&self, target: PanelHit, x: i32) -> Option<i32> {
        let (r, _) = self.zones.iter().find(|(_, h)| *h == target)?;
        let pad = dp(8, self.dpi) as f32;
        let w = ((r.right - r.left) as f32 - pad * 2.0).max(1.0);
        let frac = ((x - r.left) as f32 - pad) / w;
        Some((frac.clamp(0.0, 1.0) * 100.0).round() as i32)
    }

    fn apply_slider_x(&mut self, x: i32) -> PanelAction {
        let Some(pct) = self.slider_pct_from_x(PanelHit::VolSlider, x) else {
            return PanelAction::None;
        };
        if pct != self.vol_level || self.vol_mute {
            self.vol_level = pct;
            if pct > 0 {
                self.vol_mute = false;
            }
            set_system_volume(pct, self.vol_mute);
            return PanelAction::VolumeChanged(pct, self.vol_mute);
        }
        PanelAction::None
    }

    fn apply_bright_slider_x(&mut self, x: i32) {
        if !self.bright_supported {
            return;
        }
        let Some(pct) = self.slider_pct_from_x(PanelHit::BrightSlider, x) else {
            return;
        };
        if pct != self.brightness {
            self.brightness = pct;
            apply_brightness_async(pct);
        }
    }

    fn activate(&mut self, main_hwnd: HWND, hit: PanelHit) -> PanelAction {
        match hit {
            PanelHit::ImeMode(native) => {
                set_ime_native(self.prev_fg, native);
                self.ime_native = native;
                self.render();
                PanelAction::ImeChanged { close: false }
            }
            PanelHit::ImeLayout(i) => {
                if let Some(it) = self.ime_layouts.get(i) {
                    switch_keyboard_layout(self.prev_fg, it.hkl);
                }
                PanelAction::ImeChanged { close: true }
            }
            PanelHit::ImeSettings => {
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("ms-settings:regionlanguage"));
                PanelAction::Close
            }
            PanelHit::NetRefresh => {
                self.net_data.loading = true;
                self.net_token = self.net_token.wrapping_add(1);
                spawn_network_query(main_hwnd, self.net_token);
                self.render();
                PanelAction::None
            }
            PanelHit::WifiNet(i) => {
                if let (Some(guid), Some(net)) = (self.net_data.iface_guid, self.net_data.nets.get(i).cloned()) {
                    if !net.profile.is_empty() {
                        std::thread::spawn(move || {
                            connect_wifi(guid, &net.profile);
                        });
                        self.net_data.loading = true;
                        let token = self.net_token;
                        let mh = main_hwnd.0 as isize;
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(900));
                            spawn_network_query(HWND(mh as *mut _), token);
                        });
                        self.render();
                        return PanelAction::None;
                    } else {
                        let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("ms-availablenetworks:"));
                        return PanelAction::Close;
                    }
                }
                PanelAction::None
            }
            PanelHit::NetSettings => {
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("ms-settings:network-wifi"));
                PanelAction::Close
            }
            PanelHit::VolMute => {
                self.vol_mute = !self.vol_mute;
                set_system_volume(self.vol_level, self.vol_mute);
                self.render();
                PanelAction::VolumeChanged(self.vol_level, self.vol_mute)
            }
            PanelHit::AudioDev(i) => {
                if let Some(dev) = self.audio_devs.get(i).cloned() {
                    let _ = set_default_audio_endpoint(&dev.id);
                    for d in &mut self.audio_devs {
                        d.is_default = d.id == dev.id;
                    }
                    if let Some((v, m)) = crate::topbar::status::volume() {
                        self.vol_level = v;
                        self.vol_mute = m;
                    }
                    self.render();
                    return PanelAction::VolumeChanged(self.vol_level, self.vol_mute);
                }
                PanelAction::None
            }
            PanelHit::SoundSettings => {
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("ms-settings:sound"));
                PanelAction::Close
            }
            PanelHit::PowerScheme(i) => {
                if let Some(sc) = self.power_schemes.get(i) {
                    let guid = sc.guid;
                    unsafe {
                        let _ = PowerSetActiveScheme(HKEY::default(), Some(&guid));
                    }
                    for s in &mut self.power_schemes {
                        s.active = s.guid == guid;
                    }
                    self.render();
                }
                PanelAction::None
            }
            PanelHit::PowerSettings => {
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("ms-settings:powersleep"));
                PanelAction::Close
            }
            PanelHit::BrightSlider | PanelHit::VolSlider => PanelAction::None,
            PanelHit::CtrlTheme => PanelAction::ToggleTheme,
            PanelHit::CtrlNotify | PanelHit::CalNotify => PanelAction::NotificationCenter,
            PanelHit::CtrlLock => PanelAction::LockScreen,
            PanelHit::CtrlSleep => PanelAction::Sleep,
            PanelHit::CtrlSettings => PanelAction::OpenSettings,
            PanelHit::CalTitle => {
                self.cal_view = match self.cal_view {
                    CalView::Days => CalView::Months,
                    CalView::Months => CalView::Years,
                    CalView::Years => CalView::Days,
                };
                self.render();
                PanelAction::None
            }
            PanelHit::CalPickMonth(m) => {
                self.cal_month = m.clamp(1, 12);
                self.cal_view = CalView::Days;
                self.render();
                PanelAction::None
            }
            PanelHit::CalPickYear(yr) => {
                self.cal_year = yr.clamp(1900, 2999);
                self.cal_view = CalView::Months;
                self.render();
                PanelAction::None
            }
            PanelHit::CalPrev => {
                match self.cal_view {
                    CalView::Days => {
                        if self.cal_month <= 1 {
                            self.cal_year -= 1;
                            self.cal_month = 12;
                        } else {
                            self.cal_month -= 1;
                        }
                    }
                    CalView::Months => self.cal_year = (self.cal_year - 1).max(1900),
                    CalView::Years => self.cal_year = (self.cal_year - 12).max(1900),
                }
                self.render();
                PanelAction::None
            }
            PanelHit::CalNext => {
                match self.cal_view {
                    CalView::Days => {
                        if self.cal_month >= 12 {
                            self.cal_year += 1;
                            self.cal_month = 1;
                        } else {
                            self.cal_month += 1;
                        }
                    }
                    CalView::Months => self.cal_year = (self.cal_year + 1).min(2999),
                    CalView::Years => self.cal_year = (self.cal_year + 12).min(2999),
                }
                self.render();
                PanelAction::None
            }
            PanelHit::CalToday => {
                self.cal_year = self.today.0;
                self.cal_month = self.today.1;
                self.cal_view = CalView::Days;
                self.render();
                PanelAction::None
            }
            PanelHit::None => PanelAction::None,
        }
    }

    fn hit_test(&self, x: i32, y: i32) -> PanelHit {
        if x < 0 || x >= self.w || y < 0 || y >= self.h {
            return PanelHit::None;
        }
        self.zones
            .iter()
            .rev()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, h)| h.clone())
            .unwrap_or(PanelHit::None)
    }

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
        self.zones.clear();

        let d = self.dpi;
        let s = d as f32 / 96.0;
        let dark = self.dark;
        let bg = if dark { rgb(0x222328, 0.96) } else { rgb(0xF7F8FB, 0.97) };
        let card_bg = if dark { rgb(0x2D2F36, 0.95) } else { rgb(0xFFFFFF, 0.96) };
        let hov_bg = if dark { rgb(0x3B3E48, 0.95) } else { rgb(0xEAEFF7, 0.98) };
        let bd = if dark { (1.0, 1.0, 1.0, 0.16) } else { (0.0, 0.0, 0.0, 0.12) };
        let sep = if dark { (1.0, 1.0, 1.0, 0.10) } else { (0.0, 0.0, 0.0, 0.08) };
        let fg = if dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) };
        let fg_dim = if dark { rgb(0x98989D, 1.0) } else { rgb(0x6E6E73, 1.0) };
        let acc = (self.accent.0, self.accent.1, self.accent.2, 1.0);
        let acc_soft = (self.accent.0, self.accent.1, self.accent.2, if dark { 0.24 } else { 0.14 });

        // 按抽屉偏移绘制卡片本体，超出顶部栏下沿（y < 0）的区域由 Surface 自动裁切
        let card = self.card_rect();
        sf.fill_rrect(&card, 14.0 * s, bg);
        sf.stroke_rrect(&card, 14.0 * s, 1.0, bd);

        let pad = dp(14, d);
        let mut y = card.top + pad;

        match self.kind {
            PanelKind::Ime => {
                let title_rc = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(22, d) };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "输入法", self.font_bold, &title_rc, fg, TextAlign::Left, false, s);
                y += dp(28, d);
                if self.ime_cur_chinese {
                    let seg_r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(32, d) };
                    sf.fill_rrect(&seg_r, 8.0 * s, card_bg);
                    let half = (seg_r.right - seg_r.left) / 2;
                    for (idx, (label, is_nat)) in [("中文模式", true), ("英文模式", false)].iter().enumerate() {
                        let r = RECT {
                            left: seg_r.left + idx as i32 * half + dp(3, d),
                            top: seg_r.top + dp(3, d),
                            right: seg_r.left + (idx as i32 + 1) * half - dp(3, d),
                            bottom: seg_r.bottom - dp(3, d),
                        };
                        let active = self.ime_native == *is_nat;
                        let hit = PanelHit::ImeMode(*is_nat);
                        if active {
                            sf.fill_rrect(&r, 6.0 * s, acc);
                        } else if self.hover == hit {
                            sf.fill_rrect(&r, 6.0 * s, hov_bg);
                        }
                        let tcol = if active { rgb(0xFFFFFF, 1.0) } else { fg };
                        draw_transparent_text_aligned(&mut sf, &mut self.scratch, label, self.font, &r, tcol, TextAlign::Center, false, s);
                        self.zones.push((r, hit));
                    }
                    y += dp(40, d);
                }
                for (i, it) in self.ime_layouts.iter().take(7).enumerate() {
                    let r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(32, d) };
                    let hit = PanelHit::ImeLayout(i);
                    if self.hover == hit {
                        sf.fill_rrect(&r, 7.0 * s, hov_bg);
                    } else if it.active {
                        sf.fill_rrect(&r, 7.0 * s, acc_soft);
                    }
                    if it.active {
                        let cx = r.left as f32 + 14.0 * s;
                        let cy = (r.top + r.bottom) as f32 * 0.5;
                        sf.thick_line(cx - 3.0 * s, cy, cx - 0.5 * s, cy + 2.5 * s, 1.6 * s, acc);
                        sf.thick_line(cx - 0.5 * s, cy + 2.5 * s, cx + 4.0 * s, cy - 2.5 * s, 1.6 * s, acc);
                    }
                    let tr = RECT { left: r.left + dp(26, d), top: r.top, right: r.right - dp(8, d), bottom: r.bottom };
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &it.name, self.font, &tr, if it.active { acc } else { fg }, TextAlign::Left, false, s);
                    self.zones.push((r, hit));
                    y += dp(34, d);
                }
                let foot = RECT { left: pad, top: card.bottom - dp(32, d), right: w - pad, bottom: card.bottom - dp(8, d) };
                sf.fill_rect(&RECT { left: pad, top: foot.top - dp(4, d), right: w - pad, bottom: foot.top - dp(3, d) }, sep);
                if self.hover == PanelHit::ImeSettings {
                    sf.fill_rrect(&foot, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "语言与输入法设置", self.font_sub, &foot, fg_dim, TextAlign::Center, false, s);
                self.zones.push((foot, PanelHit::ImeSettings));
            }
            PanelKind::Network => {
                let title_rc = RECT { left: pad, top: y, right: w - pad - dp(54, d), bottom: y + dp(24, d) };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "网络与 Wi-Fi", self.font_bold, &title_rc, fg, TextAlign::Left, false, s);
                let ref_r = RECT { left: w - pad - dp(50, d), top: y, right: w - pad, bottom: y + dp(24, d) };
                sf.fill_rrect(&ref_r, 6.0 * s, if self.hover == PanelHit::NetRefresh { hov_bg } else { card_bg });
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "刷新", self.font_sub, &ref_r, fg, TextAlign::Center, false, s);
                self.zones.push((ref_r, PanelHit::NetRefresh));
                y += dp(30, d);

                let status_r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(44, d) };
                sf.fill_rrect(&status_r, 9.0 * s, card_bg);
                let state_title = if self.net_data.connected {
                    if self.net_data.active_name.is_empty() { "已连接互联网".to_string() } else { self.net_data.active_name.clone() }
                } else {
                    "未连接网络".to_string()
                };
                let sub = if self.net_data.ipv4.is_empty() {
                    if self.net_data.loading { "正在刷新网络信息".to_string() } else { "无 IPv4 地址".to_string() }
                } else {
                    format!("IPv4: {}", self.net_data.ipv4)
                };
                draw_glyph(
                    &mut sf,
                    if self.net_data.connected { Glyph::Wifi } else { Glyph::WifiOff },
                    status_r.left as f32 + 20.0 * s,
                    (status_r.top + status_r.bottom) as f32 * 0.5,
                    16.0 * s,
                    if self.net_data.connected { acc } else { fg_dim },
                );
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, &state_title, self.font, &RECT { left: status_r.left + dp(38, d), top: status_r.top + dp(3, d), right: status_r.right - dp(8, d), bottom: status_r.top + dp(23, d) }, fg, TextAlign::Left, false, s);
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, &sub, self.font_sub, &RECT { left: status_r.left + dp(38, d), top: status_r.top + dp(22, d), right: status_r.right - dp(8, d), bottom: status_r.bottom - dp(3, d) }, fg_dim, TextAlign::Left, false, s);
                y += dp(52, d);

                if self.net_data.has_wifi {
                    for (i, net) in self.net_data.nets.iter().take(6).enumerate() {
                        let r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(34, d) };
                        let hit = PanelHit::WifiNet(i);
                        if self.hover == hit {
                            sf.fill_rrect(&r, 7.0 * s, hov_bg);
                        } else if net.connected {
                            sf.fill_rrect(&r, 7.0 * s, acc_soft);
                        }
                        let right_lbl = if net.connected { "已连接".to_string() } else { format!("{}%", net.quality) };
                        draw_transparent_text_aligned(&mut sf, &mut self.scratch, &net.ssid, self.font, &RECT { left: r.left + dp(10, d), top: r.top, right: r.right - dp(54, d), bottom: r.bottom }, if net.connected { acc } else { fg }, TextAlign::Left, false, s);
                        draw_transparent_text_aligned(&mut sf, &mut self.scratch, &right_lbl, self.font_sub, &RECT { left: r.right - dp(54, d), top: r.top, right: r.right - dp(8, d), bottom: r.bottom }, if net.connected { acc } else { fg_dim }, TextAlign::Right, false, s);
                        self.zones.push((r, hit));
                        y += dp(36, d);
                    }
                    if self.net_data.nets.is_empty() {
                        let er = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(30, d) };
                        let msg = if self.net_data.loading { "正在扫描附近 Wi-Fi" } else { "附近未发现可用 Wi-Fi" };
                        draw_transparent_text_aligned(&mut sf, &mut self.scratch, msg, self.font_sub, &er, fg_dim, TextAlign::Center, false, s);
                    }
                } else {
                    let er = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(30, d) };
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, "当前设备使用有线/非 WLAN 网络", self.font_sub, &er, fg_dim, TextAlign::Center, false, s);
                }

                let foot = RECT { left: pad, top: card.bottom - dp(32, d), right: w - pad, bottom: card.bottom - dp(8, d) };
                sf.fill_rect(&RECT { left: pad, top: foot.top - dp(4, d), right: w - pad, bottom: foot.top - dp(3, d) }, sep);
                if self.hover == PanelHit::NetSettings {
                    sf.fill_rrect(&foot, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "网络设置", self.font_sub, &foot, fg_dim, TextAlign::Center, false, s);
                self.zones.push((foot, PanelHit::NetSettings));
            }
            PanelKind::Brightness => {
                let title_rc = RECT { left: pad, top: y, right: w - pad - dp(48, d), bottom: y + dp(24, d) };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "屏幕亮度", self.font_bold, &title_rc, fg, TextAlign::Left, false, s);
                let pct_rc = RECT { left: w - pad - dp(48, d), top: y, right: w - pad, bottom: y + dp(24, d) };
                let pct_txt = if self.bright_supported { format!("{}%", self.brightness) } else { "不支持".to_string() };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, &pct_txt, self.font, &pct_rc, fg_dim, TextAlign::Right, false, s);
                y += dp(30, d);

                let row = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(40, d) };
                sf.fill_rrect(&row, 9.0 * s, card_bg);
                if self.bright_supported {
                    draw_glyph(&mut sf, Glyph::Sun, row.left as f32 + 19.0 * s, (row.top + row.bottom) as f32 * 0.5, 15.0 * s, fg);
                    let slider_r = RECT { left: row.left + dp(34, d), top: row.top, right: row.right - dp(8, d), bottom: row.bottom };
                    let frac = (self.brightness as f32 / 100.0).clamp(0.0, 1.0);
                    self.draw_slider(&mut sf, &slider_r, frac, dark, acc, s);
                    self.zones.push((slider_r, PanelHit::BrightSlider));
                } else {
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, "当前显示器不支持系统亮度调节", self.font_sub, &row, fg_dim, TextAlign::Center, false, s);
                }
            }
            PanelKind::Volume => {
                let title_rc = RECT { left: pad, top: y, right: w - pad - dp(44, d), bottom: y + dp(24, d) };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "声音输出", self.font_bold, &title_rc, fg, TextAlign::Left, false, s);
                let pct_rc = RECT { left: w - pad - dp(44, d), top: y, right: w - pad, bottom: y + dp(24, d) };
                let pct_txt = if self.vol_mute { "静音".to_string() } else { format!("{}%", self.vol_level) };
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, &pct_txt, self.font, &pct_rc, fg_dim, TextAlign::Right, false, s);
                y += dp(30, d);

                let vol_row = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(36, d) };
                sf.fill_rrect(&vol_row, 9.0 * s, card_bg);
                let mute_r = RECT { left: vol_row.left + dp(4, d), top: vol_row.top + dp(4, d), right: vol_row.left + dp(32, d), bottom: vol_row.bottom - dp(4, d) };
                if self.hover == PanelHit::VolMute {
                    sf.fill_rrect(&mute_r, 6.0 * s, hov_bg);
                }
                draw_glyph(&mut sf, if self.vol_mute || self.vol_level == 0 { Glyph::SpeakerMute } else { Glyph::Speaker }, (mute_r.left + mute_r.right) as f32 * 0.5, (mute_r.top + mute_r.bottom) as f32 * 0.5, 15.0 * s, if self.vol_mute { rgb(0xFF453A, 1.0) } else { fg });
                self.zones.push((mute_r, PanelHit::VolMute));

                let slider_r = RECT { left: mute_r.right + dp(6, d), top: vol_row.top, right: vol_row.right - dp(8, d), bottom: vol_row.bottom };
                self.draw_volume_slider(&mut sf, &slider_r, dark, acc, s);
                self.zones.push((slider_r, PanelHit::VolSlider));
                y += dp(44, d);

                for (i, dev) in self.audio_devs.iter().take(5).enumerate() {
                    let r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(32, d) };
                    let hit = PanelHit::AudioDev(i);
                    if self.hover == hit {
                        sf.fill_rrect(&r, 7.0 * s, hov_bg);
                    } else if dev.is_default {
                        sf.fill_rrect(&r, 7.0 * s, acc_soft);
                    }
                    if dev.is_default {
                        let cx = r.left as f32 + 13.0 * s;
                        let cy = (r.top + r.bottom) as f32 * 0.5;
                        sf.thick_line(cx - 3.0 * s, cy, cx - 0.5 * s, cy + 2.5 * s, 1.6 * s, acc);
                        sf.thick_line(cx - 0.5 * s, cy + 2.5 * s, cx + 4.0 * s, cy - 2.5 * s, 1.6 * s, acc);
                    }
                    let tr = RECT { left: r.left + dp(26, d), top: r.top, right: r.right - dp(8, d), bottom: r.bottom };
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &dev.name, self.font, &tr, if dev.is_default { acc } else { fg }, TextAlign::Left, false, s);
                    self.zones.push((r, hit));
                    y += dp(34, d);
                }

                let foot = RECT { left: pad, top: card.bottom - dp(32, d), right: w - pad, bottom: card.bottom - dp(8, d) };
                sf.fill_rect(&RECT { left: pad, top: foot.top - dp(4, d), right: w - pad, bottom: foot.top - dp(3, d) }, sep);
                if self.hover == PanelHit::SoundSettings {
                    sf.fill_rrect(&foot, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "声音设置", self.font_sub, &foot, fg_dim, TextAlign::Center, false, s);
                self.zones.push((foot, PanelHit::SoundSettings));
            }
            PanelKind::Battery => {
                let hero = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(68, d) };
                sf.fill_rrect(&hero, 10.0 * s, card_bg);
                if let Some((pct, charging, secs)) = self.battery_pct {
                    draw_battery_filled(&mut sf, hero.left as f32 + 28.0 * s, (hero.top + hero.bottom) as f32 * 0.5, 22.0 * s, pct, charging, fg);
                    let pct_str = format!("{pct}%");
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &pct_str, self.font_big, &RECT { left: hero.left + dp(56, d), top: hero.top + dp(8, d), right: hero.right - dp(12, d), bottom: hero.top + dp(38, d) }, fg, TextAlign::Left, false, s);
                    let mut desc = if charging { "正在接通电源充电".to_string() } else { "使用电池供电".to_string() };
                    if !charging && secs != u32::MAX && secs > 60 {
                        desc = format!("预计剩余 {} 小时 {} 分钟", secs / 3600, (secs % 3600) / 60);
                    }
                    if self.power_saver {
                        desc.push_str(" · 节电模式已开");
                    }
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &desc, self.font_sub, &RECT { left: hero.left + dp(56, d), top: hero.top + dp(38, d), right: hero.right - dp(12, d), bottom: hero.bottom - dp(8, d) }, fg_dim, TextAlign::Left, false, s);
                } else {
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, "当前设备未检测到电池", self.font, &hero, fg_dim, TextAlign::Center, false, s);
                }
                y += dp(78, d);

                for (i, sc) in self.power_schemes.iter().take(4).enumerate() {
                    let r = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(32, d) };
                    let hit = PanelHit::PowerScheme(i);
                    if self.hover == hit {
                        sf.fill_rrect(&r, 7.0 * s, hov_bg);
                    } else if sc.active {
                        sf.fill_rrect(&r, 7.0 * s, acc_soft);
                    }
                    if sc.active {
                        let cx = r.left as f32 + 13.0 * s;
                        let cy = (r.top + r.bottom) as f32 * 0.5;
                        sf.thick_line(cx - 3.0 * s, cy, cx - 0.5 * s, cy + 2.5 * s, 1.6 * s, acc);
                        sf.thick_line(cx - 0.5 * s, cy + 2.5 * s, cx + 4.0 * s, cy - 2.5 * s, 1.6 * s, acc);
                    }
                    let tr = RECT { left: r.left + dp(26, d), top: r.top, right: r.right - dp(8, d), bottom: r.bottom };
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, &sc.name, self.font, &tr, if sc.active { acc } else { fg }, TextAlign::Left, false, s);
                    self.zones.push((r, hit));
                    y += dp(34, d);
                }

                let foot = RECT { left: pad, top: card.bottom - dp(32, d), right: w - pad, bottom: card.bottom - dp(8, d) };
                sf.fill_rect(&RECT { left: pad, top: foot.top - dp(4, d), right: w - pad, bottom: foot.top - dp(3, d) }, sep);
                if self.hover == PanelHit::PowerSettings {
                    sf.fill_rrect(&foot, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "电源与电池设置", self.font_sub, &foot, fg_dim, TextAlign::Center, false, s);
                self.zones.push((foot, PanelHit::PowerSettings));
            }
            PanelKind::Control => {
                let tile_gap = dp(10, d);
                let tile_w = (w - pad * 2 - tile_gap) / 2;
                let tile_h = dp(56, d);
                // 仅保留外观主题与系统通知两个核心磁贴
                let tiles = [
                    (0, PanelHit::CtrlTheme, Glyph::Moon, "外观主题", if dark { "深色模式" } else { "浅色模式" }, dark),
                    (1, PanelHit::CtrlNotify, Glyph::ControlCenter, "系统通知", "打开通知中心", false),
                ];
                for (col, hit, glyph, title, sub, active) in tiles {
                    let tx = pad + col * (tile_w + tile_gap);
                    let tr = RECT { left: tx, top: y, right: tx + tile_w, bottom: y + tile_h };
                    let tbg = if self.hover == hit {
                        hov_bg
                    } else if active {
                        acc_soft
                    } else {
                        card_bg
                    };
                    sf.fill_rrect(&tr, 10.0 * s, tbg);
                    let icx = tr.left as f32 + 22.0 * s;
                    let icy = (tr.top + tr.bottom) as f32 * 0.5;
                    sf.fill_circle(icx, icy, 13.0 * s, if active { acc } else { (fg_dim.0, fg_dim.1, fg_dim.2, 0.18) });
                    draw_glyph(&mut sf, glyph, icx, icy, 13.5 * s, if active { rgb(0xFFFFFF, 1.0) } else { fg });
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, title, self.font, &RECT { left: tr.left + dp(42, d), top: tr.top + dp(8, d), right: tr.right - dp(6, d), bottom: tr.top + dp(29, d) }, fg, TextAlign::Left, false, s);
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, sub, self.font_sub, &RECT { left: tr.left + dp(42, d), top: tr.top + dp(28, d), right: tr.right - dp(6, d), bottom: tr.bottom - dp(6, d) }, fg_dim, TextAlign::Left, false, s);
                    self.zones.push((tr, hit));
                }
                y += tile_h + dp(12, d);

                // 底部快捷操作行（精简短标题防截断）
                let btn_gap = dp(8, d);
                let bw = (w - pad * 2 - btn_gap * 2) / 3;
                for (idx, (label, hit)) in [("锁定屏幕", PanelHit::CtrlLock), ("睡眠", PanelHit::CtrlSleep), ("应用设置", PanelHit::CtrlSettings)].into_iter().enumerate() {
                    let br = RECT {
                        left: pad + idx as i32 * (bw + btn_gap),
                        top: y,
                        right: pad + (idx as i32 + 1) * bw + idx as i32 * btn_gap,
                        bottom: y + dp(34, d),
                    };
                    sf.fill_rrect(&br, 8.0 * s, if self.hover == hit { hov_bg } else { card_bg });
                    draw_transparent_text_aligned(&mut sf, &mut self.scratch, label, self.font_sub, &br, fg, TextAlign::Center, false, s);
                    self.zones.push((br, hit));
                }
            }
            PanelKind::Clock => {
                // 头部年月（可点击切换 月/年 视图）+ 翻页/今天
                let hdr = RECT { left: pad, top: y, right: w - pad, bottom: y + dp(28, d) };
                let title_r = RECT { left: hdr.left, top: hdr.top + dp(1, d), right: hdr.right - dp(108, d), bottom: hdr.bottom - dp(1, d) };
                let title_txt = match self.cal_view {
                    CalView::Days => format!("{}年{}月 ▾", self.cal_year, self.cal_month),
                    CalView::Months => format!("{}年 ▾", self.cal_year),
                    CalView::Years => {
                        let start = year_page_start(self.cal_year);
                        format!("{} - {} ▴", start, start + 11)
                    }
                };
                if self.hover == PanelHit::CalTitle {
                    sf.fill_rrect(&title_r, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, &title_txt, self.font_bold, &RECT { left: title_r.left + dp(6, d), top: title_r.top, right: title_r.right - dp(4, d), bottom: title_r.bottom }, fg, TextAlign::Left, false, s);
                self.zones.push((title_r, PanelHit::CalTitle));

                let today_r = RECT { left: hdr.right - dp(102, d), top: hdr.top + dp(2, d), right: hdr.right - dp(58, d), bottom: hdr.bottom - dp(2, d) };
                sf.fill_rrect(&today_r, 6.0 * s, if self.hover == PanelHit::CalToday { hov_bg } else { card_bg });
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "今天", self.font_sub, &today_r, fg, TextAlign::Center, false, s);
                self.zones.push((today_r, PanelHit::CalToday));

                let prev_r = RECT { left: hdr.right - dp(52, d), top: hdr.top + dp(2, d), right: hdr.right - dp(28, d), bottom: hdr.bottom - dp(2, d) };
                sf.fill_rrect(&prev_r, 6.0 * s, if self.hover == PanelHit::CalPrev { hov_bg } else { card_bg });
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "‹", self.font_bold, &prev_r, fg, TextAlign::Center, false, s);
                self.zones.push((prev_r, PanelHit::CalPrev));

                let next_r = RECT { left: hdr.right - dp(24, d), top: hdr.top + dp(2, d), right: hdr.right, bottom: hdr.bottom - dp(2, d) };
                sf.fill_rrect(&next_r, 6.0 * s, if self.hover == PanelHit::CalNext { hov_bg } else { card_bg });
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "›", self.font_bold, &next_r, fg, TextAlign::Center, false, s);
                self.zones.push((next_r, PanelHit::CalNext));
                y += dp(34, d);

                match self.cal_view {
                    CalView::Days => {
                        let cell_w = (w - pad * 2) / 7;
                        let week_h = dp(22, d);
                        for (ci, wd) in ["一", "二", "三", "四", "五", "六", "日"].iter().enumerate() {
                            let wr = RECT { left: pad + ci as i32 * cell_w, top: y, right: pad + (ci as i32 + 1) * cell_w, bottom: y + week_h };
                            draw_transparent_text_aligned(&mut sf, &mut self.scratch, wd, self.font_sub, &wr, fg_dim, TextAlign::Center, false, s);
                        }
                        y += week_h + dp(4, d);

                        let cell_h = dp(32, d);
                        let cells = month_grid(self.cal_year, self.cal_month);
                        for (idx, (cy_val, cm_val, cd_val, in_month)) in cells.into_iter().enumerate() {
                            let r_idx = (idx / 7) as i32;
                            let c_idx = (idx % 7) as i32;
                            let cr = RECT {
                                left: pad + c_idx * cell_w,
                                top: y + r_idx * cell_h,
                                right: pad + (c_idx + 1) * cell_w,
                                bottom: y + (r_idx + 1) * cell_h,
                            };
                            let is_today = (cy_val, cm_val, cd_val) == self.today;
                            if is_today {
                                let cx = (cr.left + cr.right) as f32 * 0.5;
                                let cy = (cr.top + cr.bottom) as f32 * 0.5;
                                sf.fill_circle(cx, cy, 13.0 * s, acc);
                            }
                            let txt = cd_val.to_string();
                            let tcol = if is_today {
                                rgb(0xFFFFFF, 1.0)
                            } else if in_month {
                                fg
                            } else {
                                (fg_dim.0, fg_dim.1, fg_dim.2, 0.45)
                            };
                            draw_transparent_text_aligned(&mut sf, &mut self.scratch, &txt, self.font, &cr, tcol, TextAlign::Center, false, s);
                        }
                    }
                    CalView::Months => {
                        let gap = dp(8, d);
                        let cw = (w - pad * 2 - gap * 3) / 4;
                        let ch = dp(60, d);
                        y += dp(8, d);
                        for idx in 0..12 {
                            let m = idx + 1;
                            let r_idx = idx / 4;
                            let c_idx = idx % 4;
                            let cr = RECT {
                                left: pad + c_idx * (cw + gap),
                                top: y + r_idx * (ch + gap),
                                right: pad + c_idx * (cw + gap) + cw,
                                bottom: y + r_idx * (ch + gap) + ch,
                            };
                            let hit = PanelHit::CalPickMonth(m);
                            let selected = m == self.cal_month;
                            let is_cur_month = self.cal_year == self.today.0 && m == self.today.1;
                            let bg_c = if self.hover == hit {
                                hov_bg
                            } else if selected {
                                acc_soft
                            } else {
                                card_bg
                            };
                            sf.fill_rrect(&cr, 8.0 * s, bg_c);
                            if is_cur_month {
                                sf.stroke_rrect(&cr, 8.0 * s, 1.2 * s, acc);
                            }
                            draw_transparent_text_aligned(&mut sf, &mut self.scratch, &format!("{m}月"), self.font, &cr, if selected { acc } else { fg }, TextAlign::Center, false, s);
                            self.zones.push((cr, hit));
                        }
                    }
                    CalView::Years => {
                        let gap = dp(8, d);
                        let cw = (w - pad * 2 - gap * 3) / 4;
                        let ch = dp(60, d);
                        let start = year_page_start(self.cal_year);
                        y += dp(8, d);
                        for idx in 0..12 {
                            let yr = start + idx;
                            let r_idx = idx / 4;
                            let c_idx = idx % 4;
                            let cr = RECT {
                                left: pad + c_idx * (cw + gap),
                                top: y + r_idx * (ch + gap),
                                right: pad + c_idx * (cw + gap) + cw,
                                bottom: y + r_idx * (ch + gap) + ch,
                            };
                            let hit = PanelHit::CalPickYear(yr);
                            let selected = yr == self.cal_year;
                            let is_cur_year = yr == self.today.0;
                            let bg_c = if self.hover == hit {
                                hov_bg
                            } else if selected {
                                acc_soft
                            } else {
                                card_bg
                            };
                            sf.fill_rrect(&cr, 8.0 * s, bg_c);
                            if is_cur_year {
                                sf.stroke_rrect(&cr, 8.0 * s, 1.2 * s, acc);
                            }
                            draw_transparent_text_aligned(&mut sf, &mut self.scratch, &yr.to_string(), self.font, &cr, if selected { acc } else { fg }, TextAlign::Center, false, s);
                            self.zones.push((cr, hit));
                        }
                    }
                }

                let foot = RECT { left: pad, top: card.bottom - dp(34, d), right: w - pad, bottom: card.bottom - dp(8, d) };
                sf.fill_rect(&RECT { left: pad, top: foot.top - dp(4, d), right: w - pad, bottom: foot.top - dp(3, d) }, sep);
                if self.hover == PanelHit::CalNotify {
                    sf.fill_rrect(&foot, 6.0 * s, hov_bg);
                }
                draw_transparent_text_aligned(&mut sf, &mut self.scratch, "打开系统通知中心", self.font_sub, &foot, fg, TextAlign::Center, false, s);
                self.zones.push((foot, PanelHit::CalNotify));
            }
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

    fn draw_volume_slider(&self, sf: &mut Surface, area: &RECT, dark: bool, acc: Rgba, s: f32) {
        let frac = if self.vol_mute { 0.0 } else { (self.vol_level as f32 / 100.0).clamp(0.0, 1.0) };
        self.draw_slider(sf, area, frac, dark, acc, s);
    }

    fn draw_slider(&self, sf: &mut Surface, area: &RECT, frac: f32, dark: bool, acc: Rgba, s: f32) {
        let pad = 8.0 * s;
        let cy = (area.top + area.bottom) as f32 * 0.5;
        let th = (4.0 * s).round().max(3.0) as i32;
        let track = RECT {
            left: area.left + pad as i32,
            top: (cy - th as f32 * 0.5) as i32,
            right: area.right - pad as i32,
            bottom: (cy + th as f32 * 0.5) as i32,
        };
        let track_bg = if dark { rgb(0x48484D, 1.0) } else { rgb(0xDCE1EA, 1.0) };
        sf.fill_rrect(&track, th as f32 * 0.5, track_bg);
        let kx = track.left as f32 + frac.clamp(0.0, 1.0) * (track.right - track.left).max(1) as f32;
        let fill = RECT { left: track.left, top: track.top, right: kx.round() as i32, bottom: track.bottom };
        sf.fill_rrect(&fill, th as f32 * 0.5, acc);
        let kr = 7.5 * s;
        sf.fill_circle(kx, cy + 1.0 * s, kr, (0.0, 0.0, 0.0, 0.16));
        sf.fill_circle(kx, cy, kr, rgb(0xFFFFFF, 1.0));
        sf.stroke_circle(kx, cy, kr, 1.0, (0.0, 0.0, 0.0, 0.12));
    }
}

// ---------- 屏幕亮度读取与合并异步调节 ----------

const RPC_C_AUTHN_WINNT: u32 = 10;
const RPC_C_AUTHZ_NONE: u32 = 0;

static PENDING_BRIGHT: AtomicI32 = AtomicI32::new(-1);
static BRIGHT_RUNNING: AtomicBool = AtomicBool::new(false);

fn connect_wmi_root() -> Option<IWbemServices> {
    unsafe {
        let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        let svc = loc
            .ConnectServer(&BSTR::from("root\\wmi"), None, None, None, 0, None, None)
            .ok()?;
        let _ = CoSetProxyBlanket(
            &svc,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        );
        Some(svc)
    }
}

fn read_wmi_brightness() -> Option<i32> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let need_uninit = hr.is_ok();
        let res = (|| {
            let svc = connect_wmi_root()?;
            let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0);
            let en: IEnumWbemClassObject = svc
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from("SELECT CurrentBrightness FROM WmiMonitorBrightness"),
                    flags,
                    None,
                )
                .ok()?;
            let mut objs = [None];
            let mut ret = 0u32;
            if en.Next(1500, &mut objs, &mut ret).is_err() || ret == 0 {
                return None;
            }
            let obj = objs[0].as_ref()?;
            let mut val = VARIANT::default();
            obj.Get(w!("CurrentBrightness"), 0, &mut val, None, None).ok()?;
            u32::try_from(&val).ok().map(|v| (v as i32).clamp(0, 100))
        })();
        if need_uninit {
            CoUninitialize();
        }
        res
    }
}

fn set_wmi_brightness(pct: i32) -> bool {
    let level = pct.clamp(0, 100) as u8;
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let need_uninit = hr.is_ok();
        let ok = (|| -> Option<bool> {
            let svc = connect_wmi_root()?;
            let mut cls: Option<IWbemClassObject> = None;
            svc.GetObject(
                &BSTR::from("WmiMonitorBrightnessMethods"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut cls),
                None,
            )
            .ok()?;
            let cls = cls?;
            let mut in_sig: Option<IWbemClassObject> = None;
            cls.GetMethod(w!("WmiSetBrightness"), 0, &mut in_sig, std::ptr::null_mut())
                .ok()?;
            let in_params = in_sig?.SpawnInstance(0).ok()?;
            // WMI COM 规范要求 CIM_UINT32 使用 VT_I4（传 VT_UI4 会触发 WBEM_E_TYPE_MISMATCH）
            in_params.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0).ok()?;
            // CIM_UINT8 优先使用 VT_UI1，兼容驱动回退 VT_I4
            if in_params.Put(w!("Brightness"), 0, &VARIANT::from(level), 0).is_err() {
                in_params.Put(w!("Brightness"), 0, &VARIANT::from(level as i32), 0).ok()?;
            }

            let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0);
            let en = svc
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from("SELECT * FROM WmiMonitorBrightnessMethods"),
                    flags,
                    None,
                )
                .ok()?;
            let _ = CoSetProxyBlanket(
                &en,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            );
            let mut any = false;
            loop {
                let mut objs = [None];
                let mut ret = 0u32;
                if en.Next(1500, &mut objs, &mut ret).is_err() || ret == 0 {
                    break;
                }
                if let Some(inst) = objs[0].as_ref() {
                    let mut pvar = VARIANT::default();
                    let got_path = inst.Get(w!("__RELPATH"), 0, &mut pvar, None, None).is_ok()
                        || inst.Get(w!("__PATH"), 0, &mut pvar, None, None).is_ok();
                    if got_path {
                        if let Ok(path) = BSTR::try_from(&pvar) {
                            match svc.ExecMethod(
                                &path,
                                &BSTR::from("WmiSetBrightness"),
                                WBEM_GENERIC_FLAG_TYPE(0),
                                None,
                                &in_params,
                                None,
                                None,
                            ) {
                                Ok(_) => any = true,
                                Err(e) => crate::log::log(&format!("WMI 调节亮度失败: {}", crate::win32::error_string(e))),
                            }
                        }
                    }
                }
            }
            Some(any)
        })()
        .unwrap_or(false);
        if need_uninit {
            CoUninitialize();
        }
        ok
    }
}

fn read_ddc_brightness() -> Option<i32> {
    for mon in crate::win32::all_monitors() {
        let mut count = 0u32;
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(mon.hmonitor, &mut count) }.is_err() || count == 0 {
            continue;
        }
        let mut list = vec![PHYSICAL_MONITOR::default(); count as usize];
        if unsafe { GetPhysicalMonitorsFromHMONITOR(mon.hmonitor, &mut list) }.is_ok() {
            let mut out = None;
            for pm in &list {
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                if unsafe { GetMonitorBrightness(pm.hPhysicalMonitor, &mut mn, &mut cur, &mut mx) } != 0 && mx > mn {
                    out = Some((((cur - mn) as f32 / (mx - mn) as f32) * 100.0).round() as i32);
                    break;
                }
            }
            unsafe {
                let _ = DestroyPhysicalMonitors(&list);
            }
            if out.is_some() {
                return out.map(|v| v.clamp(0, 100));
            }
        }
    }
    None
}

fn set_ddc_brightness(pct: i32) -> bool {
    let frac = (pct.clamp(0, 100) as f32) / 100.0;
    let mut any = false;
    for mon in crate::win32::all_monitors() {
        let mut count = 0u32;
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(mon.hmonitor, &mut count) }.is_err() || count == 0 {
            continue;
        }
        let mut list = vec![PHYSICAL_MONITOR::default(); count as usize];
        if unsafe { GetPhysicalMonitorsFromHMONITOR(mon.hmonitor, &mut list) }.is_ok() {
            for pm in &list {
                let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
                if unsafe { GetMonitorBrightness(pm.hPhysicalMonitor, &mut mn, &mut cur, &mut mx) } != 0 && mx >= mn {
                    let target = mn + ((mx - mn) as f32 * frac).round() as u32;
                    if unsafe { SetMonitorBrightness(pm.hPhysicalMonitor, target) } != 0 {
                        any = true;
                    }
                }
            }
            unsafe {
                let _ = DestroyPhysicalMonitors(&list);
            }
        }
    }
    any
}

/// 读取当前屏幕亮度（优先 WMI 笔记本内屏接口，回退 DDC/CI 外接显示器）。
fn read_system_brightness() -> Option<i32> {
    read_wmi_brightness().or_else(read_ddc_brightness)
}

/// 合并拖拽期间的亮度写入请求：仅保留单后台工作线程消化最新目标亮度，避免 COM 拥堵。
fn apply_brightness_async(pct: i32) {
    PENDING_BRIGHT.store(pct.clamp(0, 100), Ordering::SeqCst);
    if BRIGHT_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        std::thread::spawn(|| {
            loop {
                let val = PENDING_BRIGHT.swap(-1, Ordering::SeqCst);
                if val < 0 {
                    BRIGHT_RUNNING.store(false, Ordering::SeqCst);
                    // 双重检查：若释放标志瞬间又有新请求则继续消化
                    if PENDING_BRIGHT.load(Ordering::SeqCst) >= 0
                        && BRIGHT_RUNNING
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        continue;
                    }
                    break;
                }
                let wmi_ok = set_wmi_brightness(val);
                // 仅当 WMI 不可用或存在多屏时补充 DDC/CI 调节，避免单屏笔记本拖动时被 DDC 阻塞
                if !wmi_ok || crate::win32::all_monitors().len() > 1 {
                    let _ = set_ddc_brightness(val);
                }
            }
        });
    }
}

// ---------- 系统能力交互辅助 ----------

fn local_ymd() -> (i32, i32, i32) {
    unsafe {
        let st: SYSTEMTIME = GetLocalTime();
        (st.wYear as i32, st.wMonth as i32, st.wDay as i32)
    }
}

fn year_page_start(year: i32) -> i32 {
    year.div_euclid(12) * 12
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn days_in_month(y: i32, m: i32) -> i32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 30,
    }
}

/// Zeller/Sakamoto 公式：返回星期 0=周一 .. 6=周日。
fn weekday_mon0(y: i32, m: i32, d: i32) -> i32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let yy = if m < 3 { y - 1 } else { y };
    let sun0 = (yy + yy / 4 - yy / 100 + yy / 400 + T[(m - 1) as usize] + d) % 7;
    (sun0 + 6) % 7
}

/// 生成 6×7 = 42 格月历单元：(年, 月, 日, 是否当月)。
pub fn month_grid(year: i32, month: i32) -> Vec<(i32, i32, i32, bool)> {
    let lead = weekday_mon0(year, month, 1);
    let (py, pm) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
    let prev_days = days_in_month(py, pm);
    let cur_days = days_in_month(year, month);
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let mut out = Vec::with_capacity(42);
    for i in 0..42 {
        if i < lead {
            out.push((py, pm, prev_days - lead + i + 1, false));
        } else if i - lead < cur_days {
            out.push((year, month, i - lead + 1, true));
        } else {
            out.push((ny, nm, i - lead - cur_days + 1, false));
        }
    }
    out
}

/// 模拟 Win+N 组合键呼出 Windows 11 系统通知中心。
pub fn trigger_notification_center() {
    let vk_win = VK_LWIN.0;
    let vk_n = VK_N.0;
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk_win), ..Default::default() },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk_n), ..Default::default() },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk_n), dwFlags: KEYEVENTF_KEYUP, ..Default::default() },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk_win), dwFlags: KEYEVENTF_KEYUP, ..Default::default() },
            },
        },
    ];
    unsafe {
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn query_ime_layouts(fg: HWND) -> (Vec<ImeLayoutItem>, bool, bool) {
    unsafe {
        let thread = if fg.0.is_null() {
            windows::Win32::System::Threading::GetCurrentThreadId()
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let active_hkl = GetKeyboardLayout(thread);
        let active_lang = active_hkl.0 as u32 & 0xFFFF;
        let is_cn = matches!(active_lang, 0x0804 | 0x0404 | 0x0C04 | 0x1004);
        let mut native = true;
        if is_cn {
            let ime = ImmGetDefaultIMEWnd(fg);
            if !ime.is_invalid() {
                let ctx = ImmGetContext(ime);
                if !ctx.is_invalid() {
                    let mut conv = windows::Win32::UI::Input::Ime::IME_CONVERSION_MODE(0);
                    if ImmGetConversionStatus(ctx, Some(&mut conv), None).as_bool() {
                        native = conv.0 & IME_CMODE_NATIVE.0 != 0;
                    }
                    let _ = ImmReleaseContext(ime, ctx);
                }
            }
        }
        let count = GetKeyboardLayoutList(None);
        let mut list = vec![HKL::default(); count.max(0) as usize];
        if count > 0 {
            let _ = GetKeyboardLayoutList(Some(&mut list));
        }
        let mut items = Vec::new();
        for hkl in list {
            let lang = hkl.0 as u32 & 0xFFFF;
            let mut buf = [0u16; 96];
            let n = GetLocaleInfoW(lang, LOCALE_SLOCALIZEDDISPLAYNAME, Some(&mut buf));
            let name = if n > 0 {
                crate::win32::from_wide(&buf)
            } else {
                format!("键盘 ({lang:04X})")
            };
            items.push(ImeLayoutItem {
                hkl: hkl.0 as isize,
                name,
                is_chinese: matches!(lang, 0x0804 | 0x0404 | 0x0C04 | 0x1004),
                active: hkl == active_hkl,
            });
        }
        (items, native, is_cn)
    }
}

fn switch_keyboard_layout(fg: HWND, hkl: isize) {
    if fg.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(fg, WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(hkl));
    }
}

fn set_ime_native(fg: HWND, native: bool) {
    if fg.0.is_null() {
        return;
    }
    const WM_IME_CONTROL_MSG: u32 = 0x0283;
    const IMC_SETCONVERSIONMODE: usize = 0x0002;
    unsafe {
        let ime = ImmGetDefaultIMEWnd(fg);
        if !ime.is_invalid() {
            let mode = if native { IME_CMODE_NATIVE.0 as isize } else { 0 };
            let mut res = 0usize;
            let _ = SendMessageTimeoutW(ime, WM_IME_CONTROL_MSG, WPARAM(IMC_SETCONVERSIONMODE), LPARAM(mode), SMTO_ABORTIFHUNG, 150, Some(&mut res));
        }
    }
}

fn set_system_volume(pct: i32, mute: bool) {
    unsafe {
        if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            if let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
                if let Ok(ep) = dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
                    let _ = ep.SetMasterVolumeLevelScalar((pct.clamp(0, 100) as f32) / 100.0, std::ptr::null());
                    let _ = ep.SetMute(mute, std::ptr::null());
                }
            }
        }
    }
}

fn query_audio_outputs() -> Vec<AudioDeviceItem> {
    let mut out = Vec::new();
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return out;
        };
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(crate::win32::take_pwstr)
            .unwrap_or_default();
        let Ok(col) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let count = col.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(dev) = col.Item(i) else { continue };
            let id = dev.GetId().ok().map(crate::win32::take_pwstr).unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|store| store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok())
                .map(|v| v.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "音频输出设备".into());
            out.push(AudioDeviceItem {
                is_default: id == default_id,
                id,
                name,
            });
        }
    }
    out
}

#[repr(C)]
struct PolicyConfigVtbl {
    query_interface: unsafe extern "system" fn(*mut std::ffi::c_void, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    _reserved: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, ERole) -> HRESULT,
}

#[repr(C)]
struct RawPolicyConfig {
    vtbl: *const PolicyConfigVtbl,
}

fn set_default_audio_endpoint(device_id: &str) -> bool {
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    unsafe {
        let Ok(raw) = CoCreateInstance::<_, windows::core::IUnknown>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) else {
            return false;
        };
        let mut iface: *mut std::ffi::c_void = std::ptr::null_mut();
        if raw.query(&IID_POLICY_CONFIG, &mut iface).is_err() || iface.is_null() {
            return false;
        }
        let pc = &*(iface as *const RawPolicyConfig);
        let wid = wide(device_id);
        let mut ok = true;
        for role in [eConsole, eMultimedia, eCommunications] {
            if ((*pc.vtbl).set_default_endpoint)(iface, PCWSTR(wid.as_ptr()), role).is_err() {
                ok = false;
            }
        }
        ((*pc.vtbl).release)(iface);
        ok
    }
}

fn query_battery_detail() -> Option<(u8, bool, u32)> {
    unsafe {
        let mut st = SYSTEM_POWER_STATUS::default();
        GetSystemPowerStatus(&mut st).ok()?;
        if st.BatteryFlag & 128 != 0 || st.BatteryLifePercent == 255 {
            return None;
        }
        Some((st.BatteryLifePercent.min(100), st.ACLineStatus == 1, st.BatteryLifeTime))
    }
}

fn query_power_saver() -> bool {
    unsafe {
        let mut st = SYSTEM_POWER_STATUS::default();
        GetSystemPowerStatus(&mut st).is_ok() && st.SystemStatusFlag == 1
    }
}

fn query_power_schemes() -> Vec<PowerSchemeItem> {
    let mut out = Vec::new();
    unsafe {
        let mut active_ptr: *mut GUID = std::ptr::null_mut();
        let active_guid = if PowerGetActiveScheme(HKEY::default(), &mut active_ptr).0 == 0 && !active_ptr.is_null() {
            let g = *active_ptr;
            let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(active_ptr as *mut _));
            Some(g)
        } else {
            None
        };
        let mut idx = 0u32;
        loop {
            let mut guid = GUID::default();
            let mut sz = std::mem::size_of::<GUID>() as u32;
            let err = PowerEnumerate(
                HKEY::default(),
                None,
                None,
                ACCESS_SCHEME,
                idx,
                Some(&mut guid as *mut GUID as *mut u8),
                &mut sz,
            );
            if err.0 != 0 {
                break;
            }
            let mut buf = [0u16; 128];
            let mut bsz = (buf.len() * 2) as u32;
            let name = if PowerReadFriendlyName(
                HKEY::default(),
                Some(&guid),
                None,
                None,
                Some(buf.as_mut_ptr() as *mut u8),
                &mut bsz,
            )
            .0 == 0
            {
                let s = crate::win32::from_wide(&buf);
                if s.is_empty() { "电源计划".into() } else { s }
            } else {
                "电源计划".into()
            };
            out.push(PowerSchemeItem {
                active: active_guid == Some(guid),
                guid,
                name,
            });
            idx += 1;
        }
    }
    out
}

// ---------- 动态加载 wlanapi.dll 进行网络枚举与连接 ----------

type FnWlanOpenHandle = unsafe extern "system" fn(u32, *const std::ffi::c_void, *mut u32, *mut windows::Win32::Foundation::HANDLE) -> u32;
type FnWlanCloseHandle = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, *const std::ffi::c_void) -> u32;
type FnWlanEnumInterfaces = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, *const std::ffi::c_void, *mut *mut WLAN_INTERFACE_INFO_LIST) -> u32;
type FnWlanQueryInterface = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, *const GUID, u32, *const std::ffi::c_void, *mut u32, *mut *mut std::ffi::c_void, *mut u32) -> u32;
type FnWlanGetAvailableNetworkList = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, *const GUID, u32, *const std::ffi::c_void, *mut *mut WLAN_AVAILABLE_NETWORK_LIST) -> u32;
type FnWlanConnect = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, *const GUID, *const WLAN_CONNECTION_PARAMETERS, *const std::ffi::c_void) -> u32;
type FnWlanFreeMemory = unsafe extern "system" fn(*mut std::ffi::c_void);

struct WlanApi {
    module: HMODULE,
    open_handle: FnWlanOpenHandle,
    close_handle: FnWlanCloseHandle,
    enum_interfaces: FnWlanEnumInterfaces,
    query_interface: FnWlanQueryInterface,
    get_networks: FnWlanGetAvailableNetworkList,
    connect: FnWlanConnect,
    free_memory: FnWlanFreeMemory,
}

impl Drop for WlanApi {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}

impl WlanApi {
    fn load() -> Option<Self> {
        unsafe {
            let module = LoadLibraryW(w!("wlanapi.dll")).ok()?;
            let sym = |name: &[u8]| -> Option<*const ()> {
                GetProcAddress(module, windows::core::PCSTR(name.as_ptr())).map(|f| f as *const ())
            };
            Some(Self {
                module,
                open_handle: std::mem::transmute(sym(b"WlanOpenHandle\0")?),
                close_handle: std::mem::transmute(sym(b"WlanCloseHandle\0")?),
                enum_interfaces: std::mem::transmute(sym(b"WlanEnumInterfaces\0")?),
                query_interface: std::mem::transmute(sym(b"WlanQueryInterface\0")?),
                get_networks: std::mem::transmute(sym(b"WlanGetAvailableNetworkList\0")?),
                connect: std::mem::transmute(sym(b"WlanConnect\0")?),
                free_memory: std::mem::transmute(sym(b"WlanFreeMemory\0")?),
            })
        }
    }
}

fn spawn_network_query(main_hwnd: HWND, token: u64) {
    let raw_hwnd = main_hwnd.0 as isize;
    std::thread::Builder::new()
        .name("panel-net".into())
        .spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let data = query_network_sync();
            unsafe {
                CoUninitialize();
            }
            let payload = Box::new((token, data));
            unsafe {
                let _ = PostMessageW(
                    HWND(raw_hwnd as *mut _),
                    WM_APP_PANEL_DATA,
                    WPARAM(0),
                    LPARAM(Box::into_raw(payload) as isize),
                );
            }
        })
        .ok();
}

fn query_network_sync() -> NetworkData {
    let mut data = NetworkData {
        loading: false,
        connected: crate::topbar::status::network_connected(),
        ..Default::default()
    };

    // 读取首个活跃非环回 IPv4 地址
    unsafe {
        let mut sz = 16 * 1024u32;
        let mut buf = vec![0u8; sz as usize];
        let r = GetAdaptersAddresses(
            AF_INET.0 as u32,
            GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST,
            None,
            Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
            &mut sz,
        );
        if r == 0 {
            let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !cur.is_null() {
                let ad = &*cur;
                if ad.OperStatus.0 == 1 && ad.IfType != 24 {
                    let mut ua = ad.FirstUnicastAddress;
                    while !ua.is_null() {
                        let addr = (*ua).Address.lpSockaddr;
                        if !addr.is_null() && (*addr).sa_family == AF_INET {
                            let sin = &*(addr as *const SOCKADDR_IN);
                            let ip = sin.sin_addr.S_un.S_addr.to_le_bytes();
                            if ip[0] != 169 {
                                data.ipv4 = format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
                                if data.active_name.is_empty() && !ad.FriendlyName.0.is_null() {
                                    data.active_name = ad.FriendlyName.to_string().unwrap_or_default();
                                }
                                break;
                            }
                        }
                        ua = (*ua).Next;
                    }
                    if !data.ipv4.is_empty() {
                        break;
                    }
                }
                cur = ad.Next;
            }
        }
    }

    // 动态加载 wlanapi 扫描可用 Wi-Fi
    if let Some(wlan) = WlanApi::load() {
        unsafe {
            let mut ver = 0u32;
            let mut client = windows::Win32::Foundation::HANDLE::default();
            if (wlan.open_handle)(2, std::ptr::null(), &mut ver, &mut client) == 0 {
                let mut if_list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
                if (wlan.enum_interfaces)(client, std::ptr::null(), &mut if_list) == 0 && !if_list.is_null() {
                    let count = (*if_list).dwNumberOfItems;
                    if count > 0 {
                        data.has_wifi = true;
                        let iface = &(*if_list).InterfaceInfo[0];
                        let guid = iface.InterfaceGuid;
                        data.iface_guid = Some(guid);

                        let mut conn_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                        let mut conn_sz = 0u32;
                        if (wlan.query_interface)(client, &guid, 7 /* wlan_intf_opcode_current_connection */, std::ptr::null(), &mut conn_sz, &mut conn_ptr, std::ptr::null_mut()) == 0 && !conn_ptr.is_null() {
                            let attr = &*(conn_ptr as *const WLAN_CONNECTION_ATTRIBUTES);
                            let ssid_len = (attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength as usize).min(32);
                            let ssid = String::from_utf8_lossy(&attr.wlanAssociationAttributes.dot11Ssid.ucSSID[..ssid_len]).to_string();
                            if !ssid.is_empty() {
                                data.active_name = ssid;
                                data.connected = true;
                            }
                            (wlan.free_memory)(conn_ptr);
                        }

                        let mut net_list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
                        if (wlan.get_networks)(client, &guid, 0, std::ptr::null(), &mut net_list) == 0 && !net_list.is_null() {
                            let n = (*net_list).dwNumberOfItems as usize;
                            let base = (*net_list).Network.as_ptr();
                            let mut seen = std::collections::HashSet::new();
                            for i in 0..n {
                                let net = &*base.add(i);
                                let slen = (net.dot11Ssid.uSSIDLength as usize).min(32);
                                if slen == 0 {
                                    continue;
                                }
                                let ssid = String::from_utf8_lossy(&net.dot11Ssid.ucSSID[..slen]).to_string();
                                if !seen.insert(ssid.clone()) {
                                    continue;
                                }
                                let profile = crate::win32::from_wide(&net.strProfileName);
                                let connected = (net.dwFlags & 1) != 0;
                                data.nets.push(WifiNetItem {
                                    ssid,
                                    profile,
                                    quality: net.wlanSignalQuality,
                                    secured: net.bSecurityEnabled.as_bool(),
                                    connected,
                                });
                            }
                            data.nets.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.quality.cmp(&a.quality)));
                            (wlan.free_memory)(net_list as *mut _);
                        }
                    }
                    (wlan.free_memory)(if_list as *mut _);
                }
                let _ = (wlan.close_handle)(client, std::ptr::null());
            }
        }
    }
    data
}

fn connect_wifi(guid: GUID, profile: &str) {
    let Some(wlan) = WlanApi::load() else { return };
    unsafe {
        let mut ver = 0u32;
        let mut client = windows::Win32::Foundation::HANDLE::default();
        if (wlan.open_handle)(2, std::ptr::null(), &mut ver, &mut client) == 0 {
            let wp = wide(profile);
            let params = WLAN_CONNECTION_PARAMETERS {
                wlanConnectionMode: WLAN_CONNECTION_MODE(0), // wlan_connection_mode_profile
                strProfile: PCWSTR(wp.as_ptr()),
                pDot11Ssid: std::ptr::null_mut(),
                pDesiredBssidList: std::ptr::null_mut(),
                dot11BssType: DOT11_BSS_TYPE(1), // dot11_BSS_type_infrastructure
                dwFlags: 0,
            };
            let _ = (wlan.connect)(client, &guid, &params, std::ptr::null());
            let _ = (wlan.close_handle)(client, std::ptr::null());
        }
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.panel_mouse_move(x, y));
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
            crate::app::with_app(hwnd, |app| app.panel_mouse_leave());
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.panel_lbutton_down(x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = get_x_lparam(lparam.0);
            let y = get_y_lparam(lparam.0);
            crate::app::with_app(hwnd, |app| app.panel_lbutton_up(x, y));
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                crate::app::with_app(hwnd, |app| app.close_panel());
                return LRESULT(0);
            }
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) == 0 {
                crate::app::with_app(hwnd, |app| app.close_panel());
            }
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            crate::app::with_app(hwnd, |app| app.panel_capture_lost());
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_grid_aligns_leap_and_weekdays() {
        let g = month_grid(2024, 2);
        assert_eq!(g.len(), 42);
        // 2024-02-01 是星期四（周一为 0 时索引为 3）
        assert_eq!(g[3], (2024, 2, 1, true));
        // 2024 闰年 2 月有 29 天，第 3+28 = 31 格为 29 日
        assert_eq!(g[31], (2024, 2, 29, true));
        assert_eq!(g[32], (2024, 3, 1, false));
        assert_eq!(year_page_start(2026), 2016);
        assert_eq!(year_page_start(2028), 2028);
    }

    #[test]
    fn wmi_brightness_roundtrip_preserves_current_level() {
        // 本机支持 WMI 内屏亮度时验证写入接口真正可用（原样写回当前亮度，不改变屏幕视觉）
        if let Some(cur) = read_wmi_brightness() {
            assert!(set_wmi_brightness(cur), "WmiSetBrightness 应能成功写入当前亮度 {cur}");
        }
    }

    #[test]
    fn status_panel_drawer_opens_ticks_reverses_and_closes_smoothly() {
        let mut panel = StatusPanel::default();
        let anchor_clock = RECT { left: 1760, top: 0, right: 1890, bottom: 30 };
        let anchor_ctrl = RECT { left: 1710, top: 0, right: 1750, bottom: 30 };
        let app = Appearance::default();

        // 1. 打开日历状态面板：首帧即从顶部栏下沿探出，保持窗口顶边紧贴顶部栏下沿
        panel.open(std::ptr::null_mut(), HWND::default(), PanelKind::Clock, anchor_clock, 96, HWND::default(), &app);
        assert!(panel.is_open());
        assert!(panel.is_animating());
        assert!(panel.drawer.card_top < panel.drawer.resting_card_top);
        assert!(panel.drawer.card_top + panel.drawer.card_h > 0);
        let mut wr = RECT::default();
        unsafe {
            let _ = GetWindowRect(panel.hwnd, &mut wr);
        }
        assert_eq!(wr.top, anchor_clock.bottom);
        let stride = panel.target.w as usize;
        let open_pixels = unsafe { std::slice::from_raw_parts(panel.target.ptr as *const u32, stride * panel.target.h as usize) };
        assert!((open_pixels[panel.w as usize / 2] >> 24) > 200, "首帧顶部栏下沿第一行应显示已滑出的面板内容");

        // 推进动画至完全展开到位：顶部留白透明，底栏区域按钮（打开系统通知中心）可准确命中
        let mut prev_top = panel.drawer.card_top;
        for _ in 0..14 {
            let _ = panel.tick(16.0, false);
            assert!(panel.drawer.card_top >= prev_top);
            prev_top = panel.drawer.card_top;
        }
        assert!(!panel.is_animating());
        assert_eq!(panel.drawer.card_top, panel.drawer.resting_card_top);
        assert!(panel.drawer.gap_px > 0);
        let pixels = unsafe { std::slice::from_raw_parts(panel.target.ptr as *const u32, stride * panel.target.h as usize) };
        assert_eq!(pixels[panel.w as usize / 2] >> 24, 0, "顶部间距首行应全透明");
        assert!((pixels[(panel.drawer.resting_card_top as usize + 12) * stride + (panel.w as usize / 2)] >> 24) > 200);
        assert_eq!(panel.hit_test(panel.w / 2, panel.h - 18), PanelHit::CalNotify);

        // 2. 平滑收起期间拦截点击，并在中途点击同一按钮时反向无缝展开
        assert!(panel.begin_close(false));
        assert!(panel.drawer.closing);
        assert!(!panel.is_open());
        assert!(matches!(panel.on_lbutton_down(panel.w / 2, panel.h - 18), PanelAction::None));
        let _ = panel.tick(24.0, false);
        let mid_top = panel.drawer.card_top;
        assert!(mid_top < panel.drawer.resting_card_top && panel.visible);

        panel.open(std::ptr::null_mut(), HWND::default(), PanelKind::Clock, anchor_clock, 96, HWND::default(), &app);
        assert!(panel.is_open());
        assert!(!panel.drawer.closing);
        assert!((panel.drawer.card_top - mid_top).abs() <= 1, "反向展开不应跳变: mid={mid_top}, now={}", panel.drawer.card_top);

        // 3. 切换到另一面板类型（Control）应重新播放滑出动画；同位置重复刷新 Control 保持原地不重播
        panel.open(std::ptr::null_mut(), HWND::default(), PanelKind::Control, anchor_ctrl, 96, HWND::default(), &app);
        assert!(panel.is_animating());
        assert!(panel.drawer.card_top < panel.drawer.resting_card_top);
        for _ in 0..14 {
            let _ = panel.tick(16.0, false);
        }
        assert!(!panel.is_animating());
        let mut dark_app = Appearance::default();
        dark_app.theme = "dark".into();
        panel.open(std::ptr::null_mut(), HWND::default(), PanelKind::Control, anchor_ctrl, 96, HWND::default(), &dark_app);
        assert!(!panel.is_animating(), "同面板切换主题应原地刷新不重复滑出");
        assert_eq!(panel.drawer.card_top, panel.drawer.resting_card_top);

        // 4. 完整收起后应隐藏窗口并释放绘图缓冲
        assert!(panel.begin_close(false));
        for _ in 0..12 {
            let (anim, _, closed) = panel.tick(16.0, false);
            if closed {
                assert!(!anim);
                break;
            }
        }
        assert!(!panel.visible);
        assert!(!panel.drawer.closing);
        assert!(panel.target.ptr.is_null());

        // 5. 减弱动态效果模式下应立即到位与立即隐藏
        let mut reduced = Appearance::default();
        reduced.reduce_motion = true;
        panel.open(std::ptr::null_mut(), HWND::default(), PanelKind::Brightness, anchor_ctrl, 96, HWND::default(), &reduced);
        assert!(panel.is_open());
        assert!(!panel.is_animating());
        assert_eq!(panel.drawer.card_top, panel.drawer.resting_card_top);
        assert!(!panel.begin_close(true));
        assert!(!panel.visible);

        panel.destroy();
    }
}
