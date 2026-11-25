//! 设置窗口：仿 macOS 系统设置（左侧分组导航 + 右侧圆角卡片分区）。
//!
//! 所有修改实时预览并立即持久化；交互元素具备 Hover / Active / Focus 反馈与 100ms 延迟
//! 自定义 Tooltip；支持快捷键录入（冲突校验）、Dock 固定项 / 托盘应用 / 取色排除名单管理、
//! 关于与一键更新。关闭窗口即销毁全部 DIB 与字体资源。

mod fields;
mod pages;

use std::collections::HashSet;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, DeleteObject, EndPaint, IntersectClipRect, InvalidateRect, RestoreDC,
    SaveDC, ScreenToClient, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VK_BACK, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_SPACE, VK_TAB, VK_UP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::App;
use crate::config::Config;
use crate::dock::icons::DOCK_ICON_SRC_SIZE;
use crate::dock::items;
use crate::hotkey;
use crate::startup;
use crate::topbar::tray_mirror::TrayItem;
use crate::ui::{
    accent_from_config, draw_app_badge, draw_button, draw_glyph,
    draw_launchpad_icon, draw_scrollbar, draw_text, draw_toggle, draw_tooltip, draw_wrapped_text,
    make_font, measure_text, measure_wrapped_text_height, palette, rgb, BtnStyle, ColorTextRenderer,
    DibTarget, Glyph, Palette, Rgba, Surface, TextAlign,
};
use crate::update::{self, CheckResult, UpdateBundle};
use crate::win32::{
    error_string, get_wheel_delta, get_x_lparam, get_y_lparam, hiword, loword, open_shell_path,
    pick_files, wide_static, window_dpi,
};

use fields::Field;
use pages::Row;

pub const CLASS_SETTINGS: &str = "WinDockFinderSettings";
const TIMER_TIP: usize = 301;
const LIST_ROW_DP: f32 = 36.0;
const DOCK_VISIBLE: usize = 4;
const TRAY_VISIBLE: usize = 4;
const EXCLUDE_VISIBLE: usize = 3;

// ---------- 页面 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    General,
    Dock,
    Topbar,
    Appearance,
    About,
}

const NAV: [(Page, &str); 5] = [
    (Page::General, "通用"),
    (Page::Dock, "Dock"),
    (Page::Topbar, "顶部栏"),
    (Page::Appearance, "外观"),
    (Page::About, "关于"),
];

impl Page {
    fn title(self) -> &'static str {
        NAV.iter().find(|(p, _)| *p == self).map(|(_, t)| *t).unwrap_or("")
    }
}

/// 全局快捷键归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeySlot {
    Dock,
    Topbar,
    Restore,
}

fn hotkey_slot(f: Field) -> HotkeySlot {
    match f {
        Field::HotkeyTopbar => HotkeySlot::Topbar,
        Field::HotkeyRestore => HotkeySlot::Restore,
        _ => HotkeySlot::Dock,
    }
}

// ---------- 命中 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    None,
    DragBar,
    Close,
    Nav(Page),
    Toggle(Field),
    Choice(Field, usize),
    Slider(Field),
    Hotkey(Field),
    Browse(Field),
    AutoDetect(Field),
    ClearPath(Field),
    Button(u32),
    Startup,
    DockRow(usize),
    DockUp,
    DockDown,
    DockRemove,
    DockAdd,
    DockReset,
    TrayRefresh,
    TrayRow(usize),
    ExcludeAdd,
    ExcludeRemove(usize),
    ScrollBar,
    // 关于
    UpdateBtn,
    UpdateToggle,
    Version(usize),
    Link(u8),
}

// ---------- 状态 ----------

#[derive(Default)]
struct UpdateState {
    status: String,
    failed: bool,
    busy: bool,
    /// 万分比 0..=10000
    progress: Option<u32>,
    bundle: Option<UpdateBundle>,
    expanded: HashSet<String>,
    scroll: i32,
}

struct Dlg {
    app: *mut App,
    hwnd: HWND,
    scale: f32,
    pal: Palette,
    dark: bool,
    target: DibTarget,
    font_title: HFONT,
    font_page: HFONT,
    font_nav: HFONT,
    font_section: HFONT,
    font_body: HFONT,
    font_sub: HFONT,
    font_btn: HFONT,
    font_tip: HFONT,
    page: Page,
    rows: Vec<Row>,
    scroll: i32,
    hover: Hit,
    hover_row: Option<usize>,
    pressed: Hit,
    focus: Option<Hit>,
    focus_from_keyboard: bool,
    color_text: Option<ColorTextRenderer>,
    tip_pending: Option<(Hit, RECT, String)>,
    tip_visible: Option<(RECT, String)>,
    capture: Option<Field>,
    capture_msg: Option<String>,
    drag_slider: Option<Field>,
    dock_sel: usize,
    dock_scroll: usize,
    tray_scroll: usize,
    exclude_scroll: usize,
    update: UpdateState,
}

/// 右侧内容区布局（行、卡片、滚动条与关于页专用区域）。
struct Layout {
    w: i32,
    h: i32,
    sidebar_w: i32,
    header_h: i32,
    close: RECT,
    nav: [RECT; 5],
    content: RECT,
    track: RECT,
    cards: Vec<RECT>,
    rows: Vec<RECT>,
    total_h: i32,
    max_scroll: i32,
    about: AboutLayout,
}

#[derive(Default, Clone, Copy)]
struct AboutLayout {
    hero: RECT,
    update: RECT,
    btn: RECT,
    auto_row: RECT,
    auto_toggle: RECT,
    links: [Option<RECT>; 3],
    log: RECT,
    log_view: RECT,
}

/// 单行控件的几何信息（按需填充）。
#[derive(Default)]
struct Geom {
    row: RECT,
    label: RECT,
    toggle: RECT,
    choice: Vec<RECT>,
    slider: Option<(RECT, f32)>,
    value: RECT,
    badge: RECT,
    edit: RECT,
    button: RECT,
    button2: Option<RECT>,
    button3: Option<RECT>,
    /// DockList：重置 / 添加 / 移除 / 下移 / 上移
    small: [Option<RECT>; 5],
    list: Option<(RECT, Vec<RECT>, Vec<RECT>)>,
}

enum ClItem {
    Header(usize, RECT),
    Section(RECT, usize, usize),
    Item(RECT, usize, usize, usize),
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn inset(r: RECT, v: i32) -> RECT {
    rc(r.left + v, r.top + v, r.right - v, r.bottom - v)
}

fn tray_items(app: &App) -> Vec<TrayItem> {
    app.topbars.first().map(|b| b.tray_items.clone()).unwrap_or_default()
}

fn dock_kind_label(item: &crate::config::DockItem) -> &'static str {
    match item.kind.as_str() {
        "uwp" => "UWP",
        "folder" => "文件夹",
        "file" => "文件",
        "winlaunchpad" => "启动台",
        _ => "应用",
    }
}

fn dock_label(item: &crate::config::DockItem) -> String {
    // 设置页也可能显示尚未保存的旧标签，按 Dock 的规则显示。
    if crate::win32::is_legacy_file_manager_label(&item.target, &item.label) {
        return crate::win32::FILE_MANAGER_NAME.into();
    }
    if !item.label.is_empty() {
        return item.label.clone();
    }
    if item.kind == "folder" || item.kind == "file" {
        return std::path::Path::new(&item.target)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "文件夹".into());
    }
    crate::monitor::exe_display_name(&items::resolve_item_exe(item).to_lowercase())
}

/// 固定项图标键（与 Dock 渲染保持一致）。
fn dock_icon_key(item: &crate::config::DockItem) -> String {
    if !item.custom_icon.is_empty() {
        return item.custom_icon.clone();
    }
    match item.kind.as_str() {
        "folder" | "file" => item.target.clone(),
        _ => items::resolve_item_exe(item),
    }
}

// ---------- 对外句柄 ----------

pub struct Settings {
    dlg: *mut Dlg,
    hwnd: HWND,
}

impl Settings {
    pub fn open(app: &mut App) -> Result<Settings, String> {
        unsafe {
            register_class();
            let ptr = app as *mut App;
            let scale = crate::win32::monitor_at_cursor()
                .map(|m| crate::win32::scale(m.dpi))
                .unwrap_or(1.0);
            let mut dlg = Box::new(Dlg::new(ptr, scale, &app.config));
            dlg.rows = pages::rows(Page::General);
            dlg.rebuild_fonts();
            let s = |v: f32| (v * scale).round() as i32;
            let cw = s(880.0);
            let ch = s(620.0);
            let work = crate::win32::monitor_at_cursor().map(|m| m.work).unwrap_or(RECT { left: 0, top: 0, right: 1920, bottom: 1080 });
            let x = work.left + ((work.right - work.left) - cw).max(0) / 2;
            let y = work.top + ((work.bottom - work.top) - ch).max(0) / 2;
            let dark = app.config.appearance.is_dark();
            let raw = Box::into_raw(dlg);
            // 取消 WS_EX_TOPMOST 与 WS_EX_TOOLWINDOW：不死压其他应用上方，支持 Alt+Tab 正常切换
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                wide_static(CLASS_SETTINGS),
                w!("WinDockFinder 设置"),
                WS_POPUP,
                x,
                y,
                cw,
                ch,
                app.hwnd,
                None,
                crate::win32::hinstance(),
                Some(raw as *const _),
            ) {
                Ok(h) => h,
                Err(e) => {
                    // 窗口未创建成功：过程不会收到 WM_DESTROY，避免释放未初始化资源，仅记录
                    let msg = error_string(e);
                    crate::log::log(&format!("设置窗口创建失败: {}", msg));
                    return Err(msg);
                }
            };
            enable_rounded(hwnd, dark);
            let _ = ShowWindow(hwnd, SW_SHOW);
            crate::win32::bring_to_front(hwnd);
            let s = Settings { dlg: raw, hwnd };
            s.invalidate();
            Ok(s)
        }
    }

    pub fn is_open(&self) -> bool {
        unsafe { !self.hwnd.0.is_null() && IsWindow(self.hwnd).as_bool() }
    }

    pub fn activate(&mut self) {
        if !self.is_open() {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            crate::win32::bring_to_front(self.hwnd);
        }
    }

    pub fn show_page(&mut self, page: Page) {
        if !self.is_open() {
            return;
        }
        unsafe {
            (*self.dlg).show_page(page);
        }
        self.invalidate();
    }

    /// 更新结果；返回 true 表示已启动安装，需要主进程立即退出。
    pub fn on_update_result(&mut self, result: CheckResult) -> bool {
        if !self.is_open() {
            return false;
        }
        let restart = unsafe { (*self.dlg).on_update_result(result) };
        self.invalidate();
        restart
    }

    pub fn on_update_progress(&mut self, progress: usize) {
        if !self.is_open() {
            return;
        }
        unsafe {
            let u = &mut (*self.dlg).update;
            if progress == usize::MAX {
                u.busy = false;
                u.progress = None;
                u.failed = true;
                u.status = "下载失败，可稍后重试".into();
            } else {
                u.progress = Some(progress as u32);
            }
        }
        self.invalidate();
    }

    pub fn invalidate(&self) {
        if self.is_open() {
            unsafe {
                let _ = InvalidateRect(self.hwnd, None, false);
            }
        }
    }

    pub fn destroy(&mut self) {
        if self.is_open() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        self.hwnd = HWND::default();
    }
}

fn register_class() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let hinst = crate::win32::hinstance();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(hinst, PCWSTR(1usize as *const u16)).unwrap_or_default(),
            lpszClassName: wide_static(CLASS_SETTINGS),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

/// 无边框弹窗启用 Windows 11 原生圆角、阴影与深浅边框模式同步。
fn enable_rounded(hwnd: HWND, dark: bool) {
    use windows::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    unsafe {
        let pref = DWMWCP_ROUND.0 as u32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const u32 as *const _, 4);
        let dark_val = u32::from(dark);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark_val as *const u32 as *const _, 4);
        let _ = DwmExtendFrameIntoClientArea(
            hwnd,
            &MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 1 },
        );
    }
}

// ---------- Dlg ----------

impl Dlg {
    fn new(app: *mut App, scale: f32, cfg: &Config) -> Self {
        let mut update = UpdateState::default();
        update.status = "检测可用的正式版本".into();
        update.expanded.insert(update::VERSION.to_string());
        let dark = cfg.appearance.is_dark();
        Self {
            app,
            hwnd: HWND::default(),
            scale,
            pal: palette(dark, accent_from_config(&cfg.appearance)),
            dark,
            target: DibTarget::default(),
            font_title: HFONT::default(),
            font_page: HFONT::default(),
            font_nav: HFONT::default(),
            font_section: HFONT::default(),
            font_body: HFONT::default(),
            font_sub: HFONT::default(),
            font_btn: HFONT::default(),
            font_tip: HFONT::default(),
            page: Page::General,
            rows: Vec::new(),
            scroll: 0,
            hover: Hit::None,
            hover_row: None,
            pressed: Hit::None,
            focus: None,
            focus_from_keyboard: false,
            color_text: None,
            tip_pending: None,
            tip_visible: None,
            capture: None,
            capture_msg: None,
            drag_slider: None,
            dock_sel: 0,
            dock_scroll: 0,
            tray_scroll: 0,
            exclude_scroll: 0,
            update,
        }
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    /// 测量用 DC（首次绘制前可能无效，测量结果回退为 0）。
    fn dc(&self) -> HDC {
        self.target.mem_dc.unwrap_or_default()
    }

    fn sync_theme(&mut self, cfg: &Config) {
        let next_dark = cfg.appearance.is_dark();
        let dark_changed = next_dark != self.dark;
        self.dark = next_dark;
        self.pal = palette(self.dark, accent_from_config(&cfg.appearance));
        if dark_changed && !self.hwnd.0.is_null() {
            enable_rounded(self.hwnd, self.dark);
        }
    }

    fn rebuild_fonts(&mut self) {
        self.destroy_fonts();
        let f = |px: f32, weight: i32| make_font((px * self.scale).round() as i32, weight);
        self.font_title = f(14.0, 600);
        self.font_page = f(16.0, 600);
        self.font_nav = f(13.5, 500);
        self.font_section = f(13.0, 600);
        self.font_body = f(13.0, 400);
        self.font_sub = f(11.5, 400);
        self.font_btn = f(13.0, 600);
        self.font_tip = f(12.0, 400);
    }

    fn destroy_fonts(&mut self) {
        unsafe {
            for f in [self.font_title, self.font_page, self.font_nav, self.font_section, self.font_body, self.font_sub, self.font_btn, self.font_tip] {
                if !f.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(f.0));
                }
            }
        }
        self.font_title = HFONT::default();
        self.font_page = HFONT::default();
        self.font_nav = HFONT::default();
        self.font_section = HFONT::default();
        self.font_body = HFONT::default();
        self.font_sub = HFONT::default();
        self.font_btn = HFONT::default();
        self.font_tip = HFONT::default();
    }

    /// WM_DESTROY：释放定时器、字体与 DIB（不触碰 App）。
    fn dispose(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TIP);
        }
        self.destroy_fonts();
        self.color_text = None;
        self.target.release();
    }

    fn show_page(&mut self, page: Page) {
        self.page = page;
        self.rows = pages::rows(page);
        self.scroll = 0;
        self.dock_scroll = 0;
        self.tray_scroll = 0;
        self.exclude_scroll = 0;
        self.hover = Hit::None;
        self.hover_row = None;
        self.pressed = Hit::None;
        self.focus = None;
        self.drag_slider = None;
        self.tip_pending = None;
        self.tip_visible = None;
        self.capture = None;
        self.capture_msg = None;
    }

    fn cancel_capture(&mut self, app: &mut App) {
        if self.capture.take().is_some() {
            self.capture_msg = None;
            app.suspend_hotkeys(false);
        }
    }

    fn commit(&mut self, app: &mut App, rebuild: bool, persist: bool) {
        self.sync_theme(&app.config);
        app.apply_config(rebuild);
        if persist {
            app.persist();
        }
    }

    // ---------- 布局 ----------

    fn row_height(&self, dc: HDC, row: &Row, width: i32) -> i32 {
        match row {
            Row::Section(_) => self.px(30.0),
            Row::Info(text) => self.px(22.0) + measure_wrapped_text_height(dc, text, self.font_sub, (width - self.px(32.0)).max(1)),
            Row::DockList | Row::TrayList => self.px(44.0 + 10.0) + self.px(LIST_ROW_DP) * DOCK_VISIBLE as i32,
            Row::Exclude => self.px(44.0 + 10.0) + self.px(LIST_ROW_DP) * EXCLUDE_VISIBLE as i32,
            _ => self.px(44.0),
        }
    }

    fn layout(&self, dc: HDC, _app: &App) -> Layout {
        let mut cr = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut cr);
        }
        let w = (cr.right - cr.left).max(1);
        let h = (cr.bottom - cr.top).max(1);
        let sidebar_w = self.px(196.0);
        let header_h = self.px(48.0);
        let close = rc(w - self.px(14.0) - self.px(28.0), (header_h - self.px(28.0)) / 2, w - self.px(14.0), (header_h + self.px(28.0)) / 2);
        let mut nav = [RECT::default(); 5];
        for (i, slot) in nav.iter_mut().enumerate() {
            let top = header_h + self.px(10.0) + i as i32 * (self.px(36.0) + self.px(4.0));
            *slot = rc(self.px(10.0), top, sidebar_w - self.px(10.0), top + self.px(36.0));
        }
        let content = rc(sidebar_w + self.px(20.0), header_h + self.px(14.0), w - self.px(24.0), h - self.px(14.0));
        let track = rc(w - self.px(15.0), content.top, w - self.px(8.0), content.bottom);

        let x0 = content.left;
        let x1 = content.right - self.px(2.0);
        let gap = self.px(10.0);
        let mut rows = Vec::with_capacity(self.rows.len());
        let mut cards: Vec<RECT> = Vec::new();
        let mut y = content.top - self.scroll;
        let mut card_top: Option<i32> = None;
        let mut card_bot = 0;
        let (mut first, mut prev_card, mut prev_section) = (true, false, false);
        for row in &self.rows {
            let is_card = matches!(
                row,
                Row::Toggle(_) | Row::Choice(_) | Row::Slider(_) | Row::Hotkey(_) | Row::Text(_) | Row::Button(..) | Row::Startup
            );
            let starts_block = !(is_card && prev_card);
            if starts_block && !first && !prev_section {
                y += gap;
            }
            let hgt = self.row_height(dc, row, x1 - x0);
            rows.push(rc(x0, y, x1, y + hgt));
            if is_card {
                if starts_block {
                    card_top = Some(y);
                }
                card_bot = y + hgt;
            } else {
                if let Some(t) = card_top.take() {
                    cards.push(rc(x0, t, x1, card_bot));
                }
                if !matches!(row, Row::Section(_)) {
                    cards.push(rc(x0, y, x1, y + hgt));
                }
            }
            y += hgt;
            first = false;
            prev_card = is_card;
            prev_section = matches!(row, Row::Section(_));
        }
        if let Some(t) = card_top.take() {
            cards.push(rc(x0, t, x1, card_bot));
        }
        let total_h = y + self.scroll - content.top + self.px(6.0);
        let max_scroll = (total_h - (content.bottom - content.top)).max(0);

        let mut about = AboutLayout::default();
        if self.page == Page::About {
            let p = |v: f32| self.px(v);
            let band = |top: f32, bottom: f32| rc(content.left, content.top + p(top), content.right - p(2.0), content.top + p(bottom));
            about.hero = band(0.0, 96.0);
            about.update = band(106.0, 212.0);
            about.btn = rc(about.update.right - p(16.0) - p(124.0), about.update.top + p(14.0), about.update.right - p(16.0), about.update.top + p(50.0));
            about.auto_row = rc(about.update.left, about.update.top + p(64.0), about.update.right, about.update.bottom);
            about.auto_toggle = rc(about.update.right - p(16.0) - p(44.0), about.auto_row.top + (p(48.0) - p(24.0)) / 2, about.update.right - p(16.0), about.auto_row.top + (p(48.0) + p(24.0)) / 2);
            let lw = p(120.0);
            let ly = about.update.bottom + p(10.0);
            for (i, slot) in about.links.iter_mut().enumerate() {
                let left = content.left + i as i32 * (lw + p(8.0));
                *slot = Some(rc(left, ly, left + lw, ly + p(30.0)));
            }
            about.log = rc(content.left, ly + p(30.0) + p(12.0), content.right - p(2.0), content.bottom);
            about.log_view = rc(about.log.left + p(12.0), about.log.top + p(40.0), about.log.right - p(12.0), about.log.bottom - p(8.0));
        }

        Layout { w, h, sidebar_w, header_h, close, nav, content, track, cards, rows, total_h, max_scroll, about }
    }

    fn list_rows(&self, list: RECT, n: usize, scroll: usize, visible: usize) -> Vec<RECT> {
        let lrh = self.px(LIST_ROW_DP);
        let mut v = Vec::new();
        for vi in 0..visible {
            let idx = scroll + vi;
            if idx >= n {
                break;
            }
            let top = list.top + vi as i32 * lrh;
            v.push(rc(list.left, top, list.right, (top + lrh - self.px(2.0)).min(list.bottom)));
        }
        v
    }

    fn geom(&self, dc: HDC, app: &App, l: &Layout, ri: usize) -> Geom {
        let cfg = &app.config;
        let row = l.rows[ri];
        let right = row.right;
        let rh = row.bottom - row.top;
        let label_w = |g: &mut Geom, r: i32| g.label.right = r;
        let mid_box = |_top: i32, h: f32| {
            let hh = self.px(h);
            rc(0, row.top + (rh - hh) / 2, 0, row.top + (rh + hh) / 2)
        };
        let mut g = Geom {
            row,
            label: rc(row.left + self.px(16.0), row.top, right - self.px(16.0), row.bottom),
            ..Default::default()
        };
        match &self.rows[ri] {
            Row::Toggle(_) | Row::Startup => {
                let b = mid_box(row.top, 24.0);
                g.toggle = rc(right - self.px(16.0) - self.px(44.0), b.top, right - self.px(16.0), b.bottom);
                let left = g.toggle.left - self.px(12.0);
                label_w(&mut g, left);
            }
            Row::Choice(f) => {
                if let Some((_, opts)) = f.get_choice(cfg) {
                    if *f == Field::AccentColor {
                        // macOS 风格彩色圆点选择器：每个圆点槽位 24dp×24dp，间距 6dp
                        let sz = self.px(24.0);
                        let gap = self.px(6.0);
                        let n = opts.len() as i32;
                        let total = n * sz + (n - 1).max(0) * gap;
                        let b = mid_box(row.top, 24.0);
                        let mut x = right - self.px(16.0) - total;
                        for _ in 0..opts.len() {
                            g.choice.push(rc(x, b.top, x + sz, b.bottom));
                            x += sz + gap;
                        }
                        label_w(&mut g, right - self.px(16.0) - total - self.px(12.0));
                    } else {
                        let mut total = 0;
                        let mut widths = Vec::new();
                        for (_, text) in opts {
                            let (tw, _) = measure_text(dc, text, self.font_sub);
                            let wd = (tw + self.px(18.0)).max(self.px(44.0));
                            widths.push(wd);
                            total += wd;
                        }
                        // 防止选项过多或文案较长时挤压左侧标题：超宽时按比例收敛
                        let max_total = (right - row.left - self.px(164.0)).max(self.px(160.0));
                        if total > max_total && total > 0 {
                            let ratio = max_total as f32 / total as f32;
                            total = 0;
                            for wd in &mut widths {
                                *wd = ((*wd as f32) * ratio).round().max(self.px(32.0) as f32) as i32;
                                total += *wd;
                            }
                        }
                        let b = mid_box(row.top, 26.0);
                        let mut x = right - self.px(16.0) - total;
                        for wd in widths {
                            g.choice.push(rc(x, b.top, x + wd, b.bottom));
                            x += wd;
                        }
                        label_w(&mut g, right - self.px(16.0) - total - self.px(12.0));
                    }
                }
            }
            Row::Slider(f) => {
                if let Some((v, min, max, _, _)) = f.get_num(cfg) {
                    g.value = rc(right - self.px(16.0) - self.px(72.0), row.top, right - self.px(16.0), row.bottom);
                    let tl = row.left + self.px(190.0);
                    let tr = g.value.left - self.px(12.0);
                    let mid = row.top + rh / 2;
                    g.slider = Some((rc(tl, mid - self.px(2.0), tr, mid + self.px(2.0)), ((v - min) / (max - min)).clamp(0.0, 1.0)));
                    label_w(&mut g, tl - self.px(12.0));
                }
            }
            Row::Hotkey(_) => {
                let b = mid_box(row.top, 28.0);
                g.badge = rc(right - self.px(16.0) - self.px(190.0), b.top, right - self.px(16.0), b.bottom);
                let left = g.badge.left - self.px(12.0);
                label_w(&mut g, left);
            }
            Row::Text(f) => {
                let b = mid_box(row.top, 28.0);
                g.button = rc(right - self.px(16.0) - self.px(72.0), b.top, right - self.px(16.0), b.bottom);
                let auto_b = rc(g.button.left - self.px(6.0) - self.px(78.0), b.top, g.button.left - self.px(6.0), b.bottom);
                g.button3 = Some(auto_b);
                let mut er = auto_b.left - self.px(8.0);
                if f.get_text(cfg).as_deref().is_some_and(|s| !s.is_empty()) {
                    g.button2 = Some(rc(er - self.px(24.0), b.top, er, b.bottom));
                    er -= self.px(24.0);
                }
                g.edit = rc(row.left + self.px(168.0), b.top, er, b.bottom);
                let left = g.edit.left - self.px(12.0);
                label_w(&mut g, left);
            }
            Row::Button(..) => {
                g.button = rc(right - self.px(16.0) - self.px(176.0), row.top + self.px(8.0), right - self.px(16.0), row.bottom - self.px(8.0));
                let left = g.button.left - self.px(12.0);
                label_w(&mut g, left);
            }
            Row::DockList => {
                // 头部行高 44：标题与按钮都在头部垂直居中
                let hdr_b = row.top + self.px(44.0);
                g.label.bottom = hdr_b;
                let hh = self.px(26.0);
                let top = row.top + (self.px(44.0) - hh) / 2;
                let mut x = right - self.px(12.0);
                for (i, wd) in [60.0, 60.0, 56.0, 28.0, 28.0].iter().enumerate() {
                    x -= self.px(*wd);
                    g.small[i] = Some(rc(x, top, x + self.px(*wd), top + hh));
                    x -= self.px(6.0);
                }
                g.label.right = x - self.px(8.0);
                let list = rc(row.left + self.px(8.0), hdr_b, right - self.px(8.0), row.bottom - self.px(8.0));
                let n = app.dock_items.items.len();
                g.list = Some((list, self.list_rows(list, n, self.dock_scroll, DOCK_VISIBLE), Vec::new()));
            }
            Row::TrayList => {
                let hdr_b = row.top + self.px(44.0);
                g.label.bottom = hdr_b;
                let hh = self.px(26.0);
                let top = row.top + (self.px(44.0) - hh) / 2;
                g.button = rc(right - self.px(12.0) - self.px(64.0), top, right - self.px(12.0), top + hh);
                g.label.right = g.button.left - self.px(12.0);
                let list = rc(row.left + self.px(8.0), hdr_b, right - self.px(8.0), row.bottom - self.px(8.0));
                let n = tray_items(app).len();
                g.list = Some((list, self.list_rows(list, n, self.tray_scroll, TRAY_VISIBLE), Vec::new()));
            }
            Row::Exclude => {
                let hdr_b = row.top + self.px(44.0);
                g.label.bottom = hdr_b;
                let hh = self.px(26.0);
                let top = row.top + (self.px(44.0) - hh) / 2;
                g.button = rc(right - self.px(12.0) - self.px(64.0), top, right - self.px(12.0), top + hh);
                g.label.right = g.button.left - self.px(12.0);
                let list = rc(row.left + self.px(8.0), hdr_b, right - self.px(8.0), row.bottom - self.px(8.0));
                let n = cfg.topbar.adaptive_color_exclude.len();
                let rows = self.list_rows(list, n, self.exclude_scroll, EXCLUDE_VISIBLE);
                let btns = rows
                    .iter()
                    .map(|r| {
                        let sz = self.px(20.0);
                        let cy = (r.top + r.bottom) / 2;
                        rc(r.right - self.px(12.0) - sz, cy - sz / 2, r.right - self.px(12.0), cy + sz / 2)
                    })
                    .collect();
                g.list = Some((list, rows, btns));
            }
            _ => {}
        }
        g
    }
    fn hit_test(&mut self, l: &Layout, dc: HDC, app: &App, x: i32, y: i32) -> Hit {
        self.hover_row = None;
        if inside(&l.close, x, y) {
            return Hit::Close;
        }
        for (i, r) in l.nav.iter().enumerate() {
            if inside(r, x, y) {
                return Hit::Nav(NAV[i].0);
            }
        }
        if self.page == Page::About {
            return self.about_hit(l, dc, x, y);
        }
        if inside(&l.track, x, y) {
            return Hit::ScrollBar;
        }
        if !inside(&l.content, x, y) {
            return if y < l.header_h { Hit::DragBar } else { Hit::None };
        }
        for (ri, r) in l.rows.iter().enumerate() {
            if !inside(r, x, y) {
                continue;
            }
            self.hover_row = Some(ri);
            let g = self.geom(dc, app, l, ri);
            return match &self.rows[ri] {
                Row::Toggle(f) => Hit::Toggle(*f),
                Row::Startup => Hit::Startup,
                Row::Choice(f) => {
                    let cur = f.get_choice(&app.config).map(|(i, _)| i).unwrap_or(0);
                    g.choice.iter().position(|c| inside(c, x, y)).map(|i| Hit::Choice(*f, i)).unwrap_or(Hit::Choice(*f, cur))
                }
                Row::Slider(f) => match g.slider {
                    Some((t, _)) if y >= t.top - self.px(10.0) && y <= t.bottom + self.px(10.0) => Hit::Slider(*f),
                    _ => Hit::None,
                },
                Row::Hotkey(f) => if inside(&g.badge, x, y) { Hit::Hotkey(*f) } else { Hit::None },
                Row::Text(f) => {
                    if inside(&g.button, x, y) {
                        Hit::Browse(*f)
                    } else if g.button3.map(|b| inside(&b, x, y)).unwrap_or(false) {
                        Hit::AutoDetect(*f)
                    } else if g.button2.map(|b| inside(&b, x, y)).unwrap_or(false) {
                        Hit::ClearPath(*f)
                    } else {
                        Hit::None
                    }
                }
                Row::Button(id, _) => Hit::Button(*id),
                Row::DockList => {
                    if let Some(b) = g.small[4] { if inside(&b, x, y) { return Hit::DockUp } }
                    if let Some(b) = g.small[3] { if inside(&b, x, y) { return Hit::DockDown } }
                    if let Some(b) = g.small[2] { if inside(&b, x, y) { return Hit::DockRemove } }
                    if let Some(b) = g.small[1] { if inside(&b, x, y) { return Hit::DockAdd } }
                    if let Some(b) = g.small[0] { if inside(&b, x, y) { return Hit::DockReset } }
                    match &g.list {
                        Some((_, rows, _)) => rows.iter().position(|r| inside(r, x, y)).map(|vi| Hit::DockRow(self.dock_scroll + vi)).unwrap_or(Hit::None),
                        None => Hit::None,
                    }
                }
                Row::TrayList => {
                    if inside(&g.button, x, y) {
                        return Hit::TrayRefresh;
                    }
                    match &g.list {
                        Some((_, rows, _)) => rows.iter().position(|r| inside(r, x, y)).map(|vi| Hit::TrayRow(self.tray_scroll + vi)).unwrap_or(Hit::None),
                        None => Hit::None,
                    }
                }
                Row::Exclude => {
                    if inside(&g.button, x, y) {
                        return Hit::ExcludeAdd;
                    }
                    match &g.list {
                        Some((_, rows, btns)) => {
                            if let Some(vi) = btns.iter().position(|b| inside(b, x, y)) {
                                return Hit::ExcludeRemove(self.exclude_scroll + vi);
                            }
                            let _ = rows;
                            Hit::None
                        }
                        None => Hit::None,
                    }
                }
                _ => Hit::None,
            };
        }
        Hit::None
    }

    fn about_hit(&mut self, l: &Layout, dc: HDC, x: i32, y: i32) -> Hit {
        let a = &l.about;
        if inside(&a.btn, x, y) {
            return Hit::UpdateBtn;
        }
        if inside(&a.auto_toggle, x, y) || inside(&a.auto_row, x, y) {
            return Hit::UpdateToggle;
        }
        for (i, r) in a.links.iter().enumerate() {
            if r.map(|r| inside(&r, x, y)).unwrap_or(false) {
                return Hit::Link(i as u8);
            }
        }
        if inside(&a.log_view, x, y) {
            let (items, _) = self.changelog(dc, a.log_view);
            for it in &items {
                if let ClItem::Header(ni, r) = it {
                    if inside(r, x, y) {
                        return Hit::Version(*ni);
                    }
                }
            }
        }
        if y < l.header_h {
            return Hit::DragBar;
        }
        Hit::None
    }

    /// 更新日志条目布局（可展开版本）。
    fn changelog(&self, dc: HDC, view: RECT) -> (Vec<ClItem>, i32) {
        let notes = update::bundled_notes();
        let notes = &notes[..notes.len().min(10)];
        let mut items = Vec::new();
        let mut y = view.top - self.update.scroll;
        for (ni, note) in notes.iter().enumerate() {
            let hh = self.px(34.0);
            items.push(ClItem::Header(ni, rc(view.left, y, view.right, y + hh)));
            y += hh;
            if self.update.expanded.contains(&note.version) {
                for (si, sec) in note.sections.iter().enumerate() {
                    items.push(ClItem::Section(rc(view.left + self.px(18.0), y, view.right - self.px(8.0), y + self.px(24.0)), ni, si));
                    y += self.px(24.0);
                    for (ii, text) in sec.items.iter().enumerate() {
                        let line = format!("• {text}");
                        let wd = view.right - view.left - self.px(24.0);
                        let th = measure_wrapped_text_height(dc, &line, self.font_body, wd).max(self.px(18.0));
                        items.push(ClItem::Item(rc(view.left + self.px(18.0), y, view.right - self.px(8.0), y + th), ni, si, ii));
                        y += th + self.px(2.0);
                    }
                    y += self.px(6.0);
                }
            }
            y += self.px(2.0);
        }
        (items, y + self.update.scroll - view.top)
    }

    // ---------- 焦点顺序 ----------

    fn focusables(&self, l: &Layout, dc: HDC, app: &App) -> Vec<Hit> {
        let mut v = vec![Hit::Close];
        v.extend(NAV.iter().map(|(p, _)| Hit::Nav(*p)));
        if self.page == Page::About {
            v.push(Hit::UpdateBtn);
            v.push(Hit::UpdateToggle);
            v.extend((0..3).map(Hit::Link));
            let n = update::bundled_notes().len().min(10);
            v.extend((0..n).map(Hit::Version));
            return v;
        }
        for (ri, row) in self.rows.iter().enumerate() {
            let g = self.geom(dc, app, l, ri);
            match row {
                Row::Toggle(f) => v.push(Hit::Toggle(*f)),
                Row::Startup => v.push(Hit::Startup),
                Row::Choice(f) => {
                    let cur = f.get_choice(&app.config).map(|(i, _)| i).unwrap_or(0);
                    v.push(Hit::Choice(*f, cur));
                }
                Row::Slider(f) => v.push(Hit::Slider(*f)),
                Row::Hotkey(f) => v.push(Hit::Hotkey(*f)),
                Row::Text(f) => v.push(Hit::Browse(*f)),
                Row::Button(id, _) => v.push(Hit::Button(*id)),
                Row::DockList => {
                    for h in [Hit::DockAdd, Hit::DockUp, Hit::DockDown, Hit::DockRemove, Hit::DockReset] {
                        v.push(h);
                    }
                    if let Some((_, rows, _)) = &g.list {
                        v.extend((0..rows.len()).map(|vi| Hit::DockRow(self.dock_scroll + vi)));
                    }
                }
                Row::TrayList => {
                    v.push(Hit::TrayRefresh);
                    if let Some((_, rows, _)) = &g.list {
                        v.extend((0..rows.len()).map(|vi| Hit::TrayRow(self.tray_scroll + vi)));
                    }
                }
                Row::Exclude => {
                    v.push(Hit::ExcludeAdd);
                    if let Some((_, _, btns)) = &g.list {
                        v.extend((0..btns.len()).map(|vi| Hit::ExcludeRemove(self.exclude_scroll + vi)));
                    }
                }
                _ => {}
            }
        }
        v
    }

    // ---------- 提示 ----------

    fn tip_for(&self, l: &Layout, app: &App, hit: Hit, g: &Geom) -> Option<(RECT, String)> {
        let cfg = &app.config;
        let in_list = |rows: &[RECT], i: usize, scroll: usize| rows.get(i - scroll).copied().unwrap_or(g.row);
        Some(match hit {
            Hit::Close => (l.close, "关闭设置（更改已自动保存）".to_string()),
            Hit::Toggle(f) => (g.toggle, f.tip().to_string()),
            Hit::Startup => (g.toggle, "开机后静默启动，可通过托盘图标或快捷键呼出".to_string()),
            Hit::Choice(f, i) => {
                if f == Field::AccentColor {
                    let anchor = g.choice.get(i).copied().unwrap_or(g.row);
                    let name = match i {
                        0 => "跟随 Windows 系统主题色",
                        1 => "强调色：天空蓝",
                        2 => "强调色：翠玉绿",
                        3 => "强调色：琥珀橙",
                        4 => "强调色：珊瑚红",
                        _ => "强调色：丁香紫",
                    };
                    (anchor, name.to_string())
                } else {
                    let anchor = g.choice.first().zip(g.choice.last()).map(|(a, b)| union(*a, *b)).unwrap_or(g.row);
                    (anchor, f.tip().to_string())
                }
            }
            Hit::Slider(f) => (g.slider.map(|(t, _)| t).unwrap_or(g.row), f.tip().to_string()),
            Hit::Hotkey(f) => (g.badge, format!("{}：点击后按下新的组合键；Esc 取消，Backspace 恢复默认", f.title())),
            Hit::Browse(f) => (g.button, f.tip().to_string()),
            Hit::AutoDetect(_) => (g.button3.unwrap_or(g.row), "自动搜索已安装的 WinLaunchpad".to_string()),
            Hit::ClearPath(_) => (g.button2.unwrap_or(g.row), "清除路径".to_string()),
            Hit::Button(id) => (
                g.button,
                match id {
                    1 => "打开 Windows 启动文件夹，可手动删除或恢复快捷方式",
                    9 => "仅恢复本页设置，不影响其他页面",
                    _ => "",
                }
                .to_string(),
            ),
            Hit::DockUp => (g.small[4].unwrap_or(g.row), "上移选中的固定项".to_string()),
            Hit::DockDown => (g.small[3].unwrap_or(g.row), "下移选中的固定项".to_string()),
            Hit::DockRemove => (g.small[2].unwrap_or(g.row), "从 Dock 移除选中的固定项".to_string()),
            Hit::DockAdd => (g.small[1].unwrap_or(g.row), "选择要固定到 Dock 的程序、快捷方式或文件夹".to_string()),
            Hit::DockReset => (g.small[0].unwrap_or(g.row), "恢复默认固定项（文件管理器与常见浏览器 / 编辑器）".to_string()),
            Hit::DockRow(i) => {
                let sorted = app.dock_items.sorted();
                let it = sorted.get(i)?;
                let rect = g.list.as_ref().map(|(_, r, _)| in_list(r, i, self.dock_scroll)).unwrap_or(g.row);
                (rect, format!("{} · {}", dock_label(it), it.target))
            }
            Hit::TrayRefresh => (g.button, "重新扫描系统托盘中的应用图标".to_string()),
            Hit::TrayRow(i) => {
                let rect = g.list.as_ref().map(|(_, r, _)| in_list(r, i, self.tray_scroll)).unwrap_or(g.row);
                let pinned = tray_items(app).get(i).map(|t| cfg.topbar.pinned_tray_keys.iter().any(|k| *k == t.key)).unwrap_or(false);
                (rect, if pinned { "已固定到顶部栏 · 点击收进折叠区".to_string() } else { "点击固定到顶部栏".to_string() })
            }
            Hit::ExcludeAdd => (g.button, "选择不参与最大化取色的程序（按文件名匹配）".to_string()),
            Hit::ExcludeRemove(i) => {
                let rect = g.list.as_ref().map(|(_, _, b)| in_list(b, i, self.exclude_scroll)).unwrap_or(g.row);
                (rect, "移除".to_string())
            }
            Hit::UpdateBtn => (l.about.btn, "优先检测 GitHub，失败时回退腾讯云 CNB；安装包经 Ed25519 验签后静默安装".to_string()),
            Hit::UpdateToggle => (l.about.auto_toggle, Field::AutoCheckUpdate.tip().to_string()),
            Hit::Link(2) => (l.about.links[2].unwrap_or(l.content), "打开配置、图标缓存与运行日志所在目录".to_string()),
            _ => return None,
        })
    }

    fn update_tip(&mut self, l: &Layout, app: &App, dc: HDC, hit: Hit) {
        let next = match self.hover_row {
            Some(ri) => {
                let g = self.geom(dc, app, l, ri);
                self.tip_for(l, app, hit, &g).map(|(r, t)| (hit, r, t))
            }
            None => self.tip_for(l, app, hit, &Geom::default()).map(|(r, t)| (hit, r, t)),
        }
        .filter(|(_, _, t)| !t.is_empty());
        let same = match (&self.tip_pending, &next) {
            (Some((a, ra, ta)), Some((b, rb, tb))) => a == b && *ra == *rb && ta == tb,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.tip_visible = None;
        self.tip_pending = next;
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TIP);
            if self.tip_pending.is_some() {
                let _ = SetTimer(self.hwnd, TIMER_TIP, 100, None);
            }
        }
    }

    /// 命中项所属行（用于键盘聚焦时滚动到可见区域）。
    fn hit_row(&self, hit: Hit) -> Option<usize> {
        self.rows.iter().position(|row| match (row, hit) {
            (Row::Toggle(a), Hit::Toggle(b)) => *a == b,
            (Row::Choice(a), Hit::Choice(b, _)) => *a == b,
            (Row::Slider(a), Hit::Slider(b)) => *a == b,
            (Row::Hotkey(a), Hit::Hotkey(b)) => *a == b,
            (Row::Text(a), Hit::Browse(b)) | (Row::Text(a), Hit::ClearPath(b)) => *a == b,
            (Row::Button(id, _), Hit::Button(b)) => *id == b,
            (Row::Startup, Hit::Startup) => true,
            (Row::DockList, Hit::DockRow(_) | Hit::DockUp | Hit::DockDown | Hit::DockRemove | Hit::DockAdd | Hit::DockReset) => true,
            (Row::TrayList, Hit::TrayRow(_) | Hit::TrayRefresh) => true,
            (Row::Exclude, Hit::ExcludeAdd | Hit::ExcludeRemove(_)) => true,
            _ => false,
        })
    }

    // ---------- 动作 ----------

    /// 点击 / 键盘激活；`dir` 为键盘调整方向（-1 / +1），鼠标为 0。
    fn activate(&mut self, app: &mut App, hit: Hit, dc: HDC, l: &Layout, dir: i32) {
        match hit {
            Hit::Close => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            Hit::Nav(p) => {
                self.show_page(p);
                if p == Page::Topbar {
                    app.refresh_tray();
                }
            }
            Hit::Toggle(f) => {
                let v = !f.get_bool(&app.config).unwrap_or(false);
                f.set_bool(&mut app.config, v);
                if matches!(f, Field::HideTaskbar | Field::TaskbarAutohide) {
                    app.sync_taskbar_setting();
                }
                self.commit(app, rebuild_field(f), true);
            }
            Hit::Startup => app.toggle_startup(),
            Hit::Choice(f, i) => {
                let next = if dir != 0 {
                    if let Some((cur, opts)) = f.get_choice(&app.config) {
                        let n = opts.len().max(1) as i32;
                        ((cur as i32 + dir).rem_euclid(n)) as usize
                    } else {
                        i
                    }
                } else {
                    i
                };
                f.set_choice(&mut app.config, next);
                if dir != 0 {
                    self.focus = Some(Hit::Choice(f, next));
                }
                self.commit(app, rebuild_field(f), true);
            }
            Hit::Slider(f) => {
                if let (Some((v, min, max, step, _)), false) = (f.get_num(&app.config), dir == 0) {
                    f.set_num(&mut app.config, (v + step * dir as f32).clamp(min, max));
                    self.commit(app, false, false);
                }
            }
            Hit::Hotkey(f) => {
                self.capture = Some(f);
                self.capture_msg = None;
                app.suspend_hotkeys(true);
                unsafe {
                    let _ = SetFocus(self.hwnd);
                }
            }
            Hit::Browse(f) => {
                let files = pick_files(self.hwnd, "程序 (*.exe)\0*.exe\0所有文件 (*.*)\0*.*", "选择 WinLaunchpad.exe");
                if let Some(p) = files.into_iter().next() {
                    f.set_text(&mut app.config, p.to_string_lossy().to_string());
                    app.config.general.winlaunchpad_auto_searched = true;
                    app.refresh_entries();
                    self.commit(app, false, true);
                }
            }
            Hit::AutoDetect(f) => {
                app.config.general.winlaunchpad_auto_searched = true;
                if let Some(found) = items::detect_winlaunchpad() {
                    f.set_text(&mut app.config, found);
                    app.refresh_entries();
                    self.commit(app, false, true);
                }
            }
            Hit::ClearPath(f) => {
                f.set_text(&mut app.config, String::new());
                app.config.general.winlaunchpad_auto_searched = true;
                app.refresh_entries();
                self.commit(app, false, true);
            }
            Hit::Button(id) => match id {
                1 => {
                    if let Some(dir) = startup::shortcut_path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
                        let _ = open_shell_path("open", &dir.to_string_lossy(), None);
                    }
                }
                2 => app.toggle_startup(),
                9 => self.reset_page(app),
                _ => {}
            },
            Hit::DockUp => self.move_dock(app, -1),
            Hit::DockDown => self.move_dock(app, 1),
            Hit::DockRemove => {
                let sorted = app.dock_items.sorted();
                if let Some(it) = sorted.get(self.dock_sel) {
                    let id = it.id.clone();
                    let mut di = app.dock_items.clone();
                    items::remove_pinned(&mut di, &id);
                    self.dock_sel = self.dock_sel.saturating_sub(1);
                    app.set_dock_items(di);
                }
            }
            Hit::DockAdd => {
                let files = pick_files(
                    self.hwnd,
                    "程序与快捷方式 (*.exe;*.lnk;*.bat;*.cmd)\0*.exe;*.lnk;*.bat;*.cmd\0所有文件 (*.*)\0*.*",
                    "选择要固定到 Dock 的项目",
                );
                let mut di = app.dock_items.clone();
                for f in files {
                    if let Some(it) = items::item_from_path(&f.to_string_lossy()) {
                        items::add_pinned(&mut di, it);
                    }
                }
                app.set_dock_items(di);
            }
            Hit::DockReset => {
                app.set_dock_items(items::seed_items());
                self.dock_sel = 0;
            }
            Hit::DockRow(i) => {
                self.dock_sel = i.min(app.dock_items.items.len().saturating_sub(1));
            }
            Hit::TrayRefresh => app.refresh_tray(),
            Hit::TrayRow(i) => {
                if let Some(t) = tray_items(app).get(i) {
                    let key = t.key.clone();
                    app.toggle_tray_pin(&key);
                }
            }
            Hit::ExcludeAdd => {
                let files = pick_files(self.hwnd, "程序 (*.exe)\0*.exe\0所有文件 (*.*)\0*.*", "选择不参与取色的程序");
                let mut changed = false;
                for f in files {
                    if let Some(n) = std::path::Path::new(&f).file_name().map(|n| n.to_string_lossy().to_lowercase()) {
                        let ex = &mut app.config.topbar.adaptive_color_exclude;
                        if !ex.contains(&n) {
                            ex.push(n);
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.commit(app, false, true);
                }
            }
            Hit::ExcludeRemove(i) => {
                if i < app.config.topbar.adaptive_color_exclude.len() {
                    app.config.topbar.adaptive_color_exclude.remove(i);
                    self.commit(app, false, true);
                }
            }
            Hit::UpdateBtn => self.start_update(app),
            Hit::UpdateToggle => {
                let v = !app.config.general.auto_check_update;
                Field::AutoCheckUpdate.set_bool(&mut app.config, v);
                self.commit(app, false, true);
            }
            Hit::Version(i) => {
                if let Some(n) = update::bundled_notes().get(i) {
                    if !self.update.expanded.remove(&n.version) {
                        self.update.expanded.insert(n.version.clone());
                    }
                    self.update.scroll = 0;
                }
            }
            Hit::Link(k) => {
                let target = match k {
                    0 => "https://github.com/CrazyFigure/WinDockFinder".to_string(),
                    1 => "https://cnb.cool/crazy-figure/WinDockFinder".to_string(),
                    _ => crate::win32::data_dir().to_string_lossy().to_string(),
                };
                let _ = open_shell_path("open", &target, None);
            }
            _ => {
                let _ = (dc, l);
            }
        }
    }

    fn move_dock(&mut self, app: &mut App, dir: i32) {
        let sorted = app.dock_items.sorted();
        let i = self.dock_sel;
        let Some(j) = i.checked_add_signed(dir as isize) else { return };
        if i >= sorted.len() || j >= sorted.len() {
            return;
        }
        let mut ids: Vec<String> = sorted.iter().map(|x| x.id.clone()).collect();
        ids.swap(i, j);
        let mut di = app.dock_items.clone();
        for (k, id) in ids.iter().enumerate() {
            if let Some(it) = di.items.iter_mut().find(|x| &x.id == id) {
                it.order = k as u32;
            }
        }
        self.dock_sel = j;
        app.set_dock_items(di);
    }

    fn reset_page(&mut self, app: &mut App) {
        let def = Config::default();
        let snapshot = |c: &Config| (c.dock.display_mode.clone(), c.topbar.display_mode.clone(), c.dock.enabled, c.topbar.enabled);
        let before = snapshot(&app.config);
        for row in self.rows.clone() {
            match row {
                Row::Toggle(f) | Row::Choice(f) | Row::Slider(f) | Row::Text(f) => f.set_from(&mut app.config, &def),
                Row::Hotkey(f) => {
                    if let Some(t) = f.get_text(&def) {
                        let _ = app.assign_hotkey(hotkey_slot(f), &t);
                    }
                }
                _ => {}
            }
        }
        if self.page == Page::Topbar {
            app.config.topbar.adaptive_color_exclude = def.topbar.adaptive_color_exclude;
        }
        if self.page == Page::General {
            app.sync_taskbar_setting();
        }
        self.commit(app, before != snapshot(&app.config), true);
    }

    fn start_update(&mut self, app: &mut App) {
        if self.update.busy {
            return;
        }
        if let Some(path) = self.update.bundle.as_ref().and_then(|b| b.local_path.clone()) {
            match update::start_installation(&path) {
                Ok(()) => {
                    self.update.status = "正在安装更新并重启".into();
                    self.update.busy = true;
                    app.quit();
                }
                Err(e) => {
                    crate::log::log(&format!("启动更新失败: {e}"));
                    self.update.status = "更新启动失败，可稍后重试".into();
                    self.update.failed = true;
                }
            }
            return;
        }
        self.update.failed = false;
        self.update.busy = true;
        if let Some(b) = self.update.bundle.clone() {
            self.update.progress = Some(0);
            self.update.status = "正在下载并验证签名".into();
            update::spawn_download(app.hwnd, b);
        } else {
            self.update.status = "正在检测新版本".into();
            app.check_update_manual();
        }
    }

    fn on_update_result(&mut self, result: CheckResult) -> bool {
        let u = &mut self.update;
        u.busy = false;
        match result {
            CheckResult::Latest => {
                u.status = "当前已是最新版本".into();
                u.failed = false;
                u.bundle = None;
                u.progress = None;
            }
            CheckResult::Available(b) => match &b.local_path {
                Some(path) => match update::start_installation(path) {
                    Ok(()) => {
                        u.status = "正在安装更新并重启".into();
                        u.busy = true;
                        return true;
                    }
                    Err(e) => {
                        crate::log::log(&format!("启动更新失败: {e}"));
                        u.status = "更新启动失败，可稍后重试".into();
                        u.failed = true;
                        u.progress = None;
                    }
                },
                None => {
                    u.status = format!("发现新版本 v{} · {}", b.version, b.channel.name());
                    u.failed = false;
                    u.bundle = Some(*b);
                    u.progress = None;
                }
            },
            CheckResult::Failed(e) => {
                crate::log::log(&format!("检查更新失败: {e}"));
                u.status = "检测失败，请检查网络后重试".into();
                u.failed = true;
                u.progress = None;
            }
        }
        false
    }

    /// 根据鼠标 x 设置滑块值（实时预览，不落盘）。
    fn set_slider(&mut self, app: &mut App, f: Field, l: &Layout, x: i32) {
        let Some(ri) = self.hover_row else { return };
        let g = self.geom(self.dc(), app, l, ri);
        let (Some((track, _)), Some((v, min, max, step, _))) = (g.slider, f.get_num(&app.config)) else { return };
        let kr = self.px(8.0) as f32;
        let width = ((track.right - track.left) as f32 - kr * 2.0).max(1.0);
        let frac = ((x - track.left) as f32 - kr) / width;
        let mut nv = min + frac.clamp(0.0, 1.0) * (max - min);
        nv = ((nv - min) / step).round() * step + min;
        nv = nv.clamp(min, max);
        if (nv - v).abs() > 1e-4 {
            f.set_num(&mut app.config, nv);
            self.commit(app, false, false);
        }
    }
}

// ---------- 绘制 ----------

fn is_card_row(row: &Row) -> bool {
    matches!(row, Row::Toggle(_) | Row::Choice(_) | Row::Slider(_) | Row::Hotkey(_) | Row::Text(_) | Row::Button(..) | Row::Startup)
}

fn rebuild_field(f: Field) -> bool {
    matches!(f, Field::DockDisplayMode | Field::TopbarDisplayMode | Field::DockEnabled | Field::TopbarEnabled)
}

fn nav_glyph(page: Page) -> Glyph {
    match page {
        Page::Dock => Glyph::AppGrid,
        Page::Topbar => Glyph::ControlCenter,
        Page::Appearance => Glyph::Moon,
        _ => Glyph::Gear,
    }
}

/// macOS 系统设置风格的侧边栏彩色小图标底色。
fn nav_badge_color(page: Page) -> Rgba {
    match page {
        Page::General => rgb(0x8E8E93, 1.0),
        Page::Dock => rgb(0x007AFF, 1.0),
        Page::Topbar => rgb(0x5856D6, 1.0),
        Page::Appearance => rgb(0xAF52DE, 1.0),
        Page::About => rgb(0x32ADE6, 1.0),
    }
}

/// 强调色圆点选项对应色值（索引 0 为系统自动色，绘制四色点阵）。
fn accent_dot_color(idx: usize) -> Option<Rgba> {
    match idx {
        1 => Some(rgb(0x0A84FF, 1.0)),
        2 => Some(rgb(0x30D158, 1.0)),
        3 => Some(rgb(0xFF9F0A, 1.0)),
        4 => Some(rgb(0xFF453A, 1.0)),
        5 => Some(rgb(0xBF5AF2, 1.0)),
        _ => None,
    }
}

fn draw_about_icon(sf: &mut Surface, cx: f32, cy: f32, s: f32, c: Rgba) {
    let sw = (1.6 * s).max(1.1);
    sf.stroke_circle(cx, cy, 6.6 * s, sw, c);
    sf.fill_circle(cx, cy - 3.0 * s, 1.1 * s, c);
    sf.thick_line(cx, cy - 0.2 * s, cx, cy + 3.4 * s, sw, c);
}

fn sep_color(p: &Palette) -> Rgba {
    (p.separator.0, p.separator.1, p.separator.2, p.separator.3 * 0.7)
}

fn accent_alpha(p: &Palette, dark: bool, a: f32) -> Rgba {
    (p.accent.0, p.accent.1, p.accent.2, if dark { a * 1.8 } else { a })
}

fn draw_card(sf: &mut Surface, r: &RECT, s: f32, p: &Palette) {
    sf.fill_rrect(r, 10.0 * s, p.card);
    sf.stroke_rrect(r, 10.0 * s, s.max(1.0), sep_color(p));
}

/// 同步设置像素 Surface 与 GDI DC 的矩形裁剪区，彻底防止滚动时文字溢出覆盖页眉或邻近区块。
fn with_gdi_clip(sf: &mut Surface, dc: HDC, clip: RECT, f: impl FnOnce(&mut Surface)) {
    let prev_sf = sf.clip;
    let next_sf = match prev_sf {
        Some(p) => RECT {
            left: p.left.max(clip.left),
            top: p.top.max(clip.top),
            right: p.right.min(clip.right),
            bottom: p.bottom.min(clip.bottom),
        },
        None => clip,
    };
    sf.set_clip(Some(next_sf));
    let saved = unsafe { SaveDC(dc) };
    unsafe {
        let _ = IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom);
    }
    f(sf);
    if saved != 0 {
        unsafe {
            let _ = RestoreDC(dc, saved);
        }
    }
    sf.set_clip(prev_sf);
}

fn focus_ring(dlg: &Dlg, sf: &mut Surface, hit: Hit, r: RECT, s: f32) {
    if dlg.focus_from_keyboard && dlg.focus == Some(hit) && r.right > r.left && r.bottom > r.top {
        let rad = if matches!(hit, Hit::Toggle(_) | Hit::Startup | Hit::UpdateToggle) {
            (r.bottom - r.top) as f32 * 0.5 + 3.0 * s
        } else {
            9.0 * s
        };
        sf.stroke_rrect(&inset(r, -3), rad, (1.5 * s).max(1.0), dlg.pal.accent);
    }
}

fn paint(dlg: &mut Dlg, app: &App) {
    let mut cr = RECT::default();
    unsafe {
        let _ = GetClientRect(dlg.hwnd, &mut cr);
    }
    let w = (cr.right - cr.left).max(1);
    let h = (cr.bottom - cr.top).max(1);
    if !dlg.target.ensure_at_least(w, h) {
        return;
    }
    let Some(dc) = dlg.target.mem_dc else { return };
    let mut l = dlg.layout(dc, app);
    if dlg.scroll > l.max_scroll {
        dlg.scroll = l.max_scroll;
        l = dlg.layout(dc, app);
    }
    if dlg.page == Page::About && dlg.color_text.is_none() {
        dlg.color_text = ColorTextRenderer::new().ok();
    }
    let Some(mut sf) = dlg.target.surface() else { return };
    let s = dlg.scale;
    paint_frame(dlg, &l, &mut sf, dc);
    if dlg.page == Page::About {
        paint_about(dlg, app, &l, &mut sf, dc);
    } else {
        paint_rows(dlg, app, &l, &mut sf, dc);
    }
    sf.stroke_rrect(&rc(0, 0, w - 1, h - 1), 10.0 * s, 1.0, (0.0, 0.0, 0.0, 0.10));
    if let Some((anchor, text)) = &dlg.tip_visible {
        draw_tooltip(&mut sf, dc, dlg.font_tip, anchor, text, w, h, s);
    }
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let wdc = BeginPaint(dlg.hwnd, &mut ps);
        let _ = BitBlt(wdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
        let _ = EndPaint(dlg.hwnd, &ps);
    }
}

/// 侧栏、标题栏、关闭按钮与导航。
fn paint_frame(dlg: &Dlg, l: &Layout, sf: &mut Surface, dc: HDC) {
    let s = dlg.scale;
    let p = &dlg.pal;
    sf.clear(p.window);
    sf.fill_rect(&rc(0, 0, l.sidebar_w, l.h), p.sidebar);
    sf.fill_rect(&rc(l.sidebar_w, 0, l.sidebar_w + 1, l.h), p.separator);
    draw_text(dc, "WinDockFinder", dlg.font_title, &rc(dlg.px(16.0), 0, l.sidebar_w - dlg.px(8.0), l.header_h), p.text, TextAlign::Left, true, false);
    draw_text(dc, dlg.page.title(), dlg.font_page, &rc(l.sidebar_w + dlg.px(20.0), 0, l.close.left - dlg.px(12.0), l.header_h), p.text, TextAlign::Left, true, false);
    sf.fill_rect(&rc(l.sidebar_w + 1, l.header_h, l.w, l.header_h + 1), p.separator);

    let hov = dlg.hover == Hit::Close;
    let prs = dlg.pressed == Hit::Close;
    if hov || prs {
        sf.fill_rrect(&l.close, 7.0 * s, if prs { rgb(0xFEE2E2, 1.0) } else { p.control_bg });
    }
    let cx = (l.close.left + l.close.right) as f32 * 0.5;
    let cy = (l.close.top + l.close.bottom) as f32 * 0.5;
    let a = 4.4 * s;
    let sw = (1.6 * s).max(1.2);
    let cc = if hov || prs { rgb(0xDC2626, 1.0) } else { p.text_secondary };
    sf.thick_line(cx - a, cy - a, cx + a, cy + a, sw, cc);
    sf.thick_line(cx - a, cy + a, cx + a, cy - a, sw, cc);
    focus_ring(dlg, sf, Hit::Close, l.close, s);

    for (i, r) in l.nav.iter().enumerate() {
        let page = NAV[i].0;
        let active = dlg.page == page;
        let hov = dlg.hover == Hit::Nav(page);
        let prs = dlg.pressed == Hit::Nav(page);
        if active {
            sf.fill_rrect(r, 8.0 * s, accent_alpha(p, dlg.dark, 0.14));
        } else if prs {
            sf.fill_rrect(r, 8.0 * s, (p.text.0, p.text.1, p.text.2, 0.10));
        } else if hov {
            sf.fill_rrect(r, 8.0 * s, (p.text.0, p.text.1, p.text.2, 0.06));
        }
        // 仿 macOS 系统设置：左侧彩色圆角徽章底块 + 白色图标
        let cy = (r.top + r.bottom) as f32 * 0.5;
        let bsz = 22.0 * s;
        let icx = r.left as f32 + 10.0 * s + bsz * 0.5;
        let badge = rc(
            (icx - bsz * 0.5).round() as i32,
            (cy - bsz * 0.5).round() as i32,
            (icx + bsz * 0.5).round() as i32,
            (cy + bsz * 0.5).round() as i32,
        );
        sf.fill_rrect(&badge, 6.0 * s, nav_badge_color(page));
        let white = rgb(0xFFFFFF, 0.96);
        if page == Page::About {
            draw_about_icon(sf, icx, cy, s * 0.82, white);
        } else {
            draw_glyph(sf, nav_glyph(page), icx, cy, 12.5 * s, white);
        }
        let tc = if active { p.accent } else { p.text };
        draw_text(dc, NAV[i].1, dlg.font_nav, &rc(r.left + dlg.px(40.0), r.top, r.right - dlg.px(8.0), r.bottom), tc, TextAlign::Left, true, false);
        focus_ring(dlg, sf, Hit::Nav(page), *r, s);
    }
    let vr = rc(dlg.px(16.0), l.h - dlg.px(28.0), l.sidebar_w - dlg.px(8.0), l.h - dlg.px(8.0));
    let _ = draw_text(dc, &format!("v{}", update::VERSION), dlg.font_sub, &vr, p.text_secondary, TextAlign::Left, true, false);
}

fn paint_rows(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: HDC) {
    let s = dlg.scale;
    let p = &dlg.pal;
    with_gdi_clip(sf, dc, l.content, |sf| {
        for c in &l.cards {
            if c.bottom < l.content.top || c.top > l.content.bottom {
                continue;
            }
            draw_card(sf, c, s, p);
        }
        // 同一卡片内的行分隔线
        for ri in 0..l.rows.len().saturating_sub(1) {
            let (r, next) = (l.rows[ri], l.rows[ri + 1]);
            if is_card_row(&dlg.rows[ri]) && is_card_row(&dlg.rows[ri + 1]) && next.top == r.bottom && r.bottom >= l.content.top && r.bottom <= l.content.bottom {
                sf.fill_rect(&rc(r.left + dlg.px(16.0), r.bottom, r.right - dlg.px(16.0), r.bottom + 1), sep_color(p));
            }
        }
        for ri in 0..l.rows.len() {
            let r = l.rows[ri];
            if r.bottom < l.content.top || r.top > l.content.bottom {
                continue;
            }
            paint_row(dlg, app, l, sf, dc, ri);
        }
    });
    draw_scrollbar(
        sf,
        &l.track,
        l.total_h.max(0) as usize,
        (l.content.bottom - l.content.top).max(0) as usize,
        dlg.scroll.max(0) as usize,
        dlg.hover == Hit::ScrollBar,
        s,
    );
}

fn paint_row(dlg: &Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: HDC, ri: usize) {
    let s = dlg.scale;
    let p = &dlg.pal;
    let cfg = &app.config;
    let row = &dlg.rows[ri];
    let g = dlg.geom(dc, app, l, ri);
    let hovered = dlg.hover_row == Some(ri) && dlg.hover != Hit::None;
    if hovered && is_card_row(row) {
        sf.fill_rrect(&inset(g.row, dlg.px(4.0)), 8.0 * s, p.card_hover);
    }
    let label = |text: &str| draw_text(dc, text, dlg.font_body, &g.label, p.text, TextAlign::Left, true, false);
    match row {
        Row::Section(title) => {
            draw_text(dc, title, dlg.font_section, &rc(g.label.left + dlg.px(2.0), g.row.top, g.label.right, g.row.bottom), p.text_secondary, TextAlign::Left, true, false);
        }
        Row::Info(text) => {
            draw_wrapped_text(dc, text, dlg.font_sub, &rc(g.label.left, g.row.top + dlg.px(11.0), g.label.right, g.row.bottom - dlg.px(11.0)), p.text_secondary);
        }
        Row::Toggle(f) => {
            label(f.title());
            draw_toggle(sf, &g.toggle, f.get_bool(cfg).unwrap_or(false), dlg.hover == Hit::Toggle(*f), dlg.pressed == Hit::Toggle(*f), s);
            focus_ring(dlg, sf, Hit::Toggle(*f), g.toggle, s);
        }
        Row::Startup => {
            let st = startup::state();
            let (text, col) = match st {
                startup::StartupState::DisabledBySystem => ("开机自启（已被任务管理器禁用）", rgb(0xD97706, 1.0)),
                _ => ("开机自启", p.text),
            };
            draw_text(dc, text, dlg.font_body, &g.label, col, TextAlign::Left, true, false);
            draw_toggle(sf, &g.toggle, st == startup::StartupState::Enabled, dlg.hover == Hit::Startup, dlg.pressed == Hit::Startup, s);
            focus_ring(dlg, sf, Hit::Startup, g.toggle, s);
        }
        Row::Choice(f) => {
            label(f.title());
            let (cur, opts) = f.get_choice(cfg).unwrap_or((0, &[]));
            if *f == Field::AccentColor {
                // macOS 风格彩色圆点选择器
                for (i, r) in g.choice.iter().enumerate() {
                    let hit = Hit::Choice(*f, i);
                    let active = i == cur;
                    let hov = dlg.hover == hit;
                    let cx = (r.left + r.right) as f32 * 0.5;
                    let cy = (r.top + r.bottom) as f32 * 0.5;
                    let rad = if hov { 9.2 * s } else { 8.4 * s };
                    if let Some(col) = accent_dot_color(i) {
                        sf.fill_circle(cx, cy, rad, col);
                        sf.stroke_circle(cx, cy, rad, s.max(1.0), (0.0, 0.0, 0.0, 0.14));
                    } else {
                        // 首项「跟随系统」：中性浅底 + 四色迷你点阵
                        sf.fill_circle(cx, cy, rad, p.control_bg);
                        sf.stroke_circle(cx, cy, rad, (1.1 * s).max(1.0), sep_color(p));
                        let off = 2.9 * s;
                        let dr = 2.2 * s;
                        sf.fill_circle(cx - off, cy - off, dr, rgb(0x0A84FF, 1.0));
                        sf.fill_circle(cx + off, cy - off, dr, rgb(0x30D158, 1.0));
                        sf.fill_circle(cx - off, cy + off, dr, rgb(0xFF9F0A, 1.0));
                        sf.fill_circle(cx + off, cy + off, dr, rgb(0xBF5AF2, 1.0));
                    }
                    if active {
                        // 选中状态：内部小白点 + 外围强调色高亮环
                        if accent_dot_color(i).is_some() {
                            sf.fill_circle(cx, cy, 3.0 * s, rgb(0xFFFFFF, 0.96));
                        }
                        sf.stroke_circle(cx, cy, rad + 2.6 * s, (1.6 * s).max(1.2), p.accent);
                    }
                }
                if !g.choice.is_empty() {
                    focus_ring(dlg, sf, Hit::Choice(*f, cur), g.choice[cur.min(g.choice.len() - 1)], s);
                }
            } else if let (Some(first), Some(last)) = (g.choice.first(), g.choice.last()) {
                sf.fill_rrect(&rc(first.left - dlg.px(2.0), first.top - dlg.px(2.0), last.right + dlg.px(2.0), last.bottom + dlg.px(2.0)), 8.0 * s, p.control_bg);
                for (i, r) in g.choice.iter().enumerate() {
                    let hit = Hit::Choice(*f, i);
                    let active = i == cur;
                    let hov = dlg.hover == hit;
                    let prs = dlg.pressed == hit;
                    if active {
                        sf.fill_rrect(r, 7.0 * s, p.card);
                        sf.stroke_rrect(r, 7.0 * s, s.max(1.0), sep_color(p));
                    } else if prs {
                        sf.fill_rrect(r, 7.0 * s, (p.text.0, p.text.1, p.text.2, 0.10));
                    } else if hov {
                        sf.fill_rrect(r, 7.0 * s, (p.text.0, p.text.1, p.text.2, 0.05));
                    }
                    let col = if active { p.accent } else { p.text_secondary };
                    draw_text(dc, opts.get(i).map(|o| o.1).unwrap_or(""), dlg.font_sub, r, col, TextAlign::Center, true, false);
                }
                focus_ring(dlg, sf, Hit::Choice(*f, cur), g.choice[cur.min(g.choice.len() - 1)], s);
            }
        }
        Row::Slider(f) => {
            label(f.title());
            if let Some(((v, _, _, _, fmt), (track, frac))) = f.get_num(cfg).zip(g.slider) {
                let txt = fmt(v);
                draw_text(dc, &txt, dlg.font_sub, &g.value, p.text_secondary, TextAlign::Right, true, false);
                let kr = 8.0 * s;
                let cy = (track.top + track.bottom) as f32 * 0.5;
                let kx = track.left as f32 + kr + frac * ((track.right - track.left) as f32 - kr * 2.0);
                sf.fill_rrect(&track, 2.0 * s, p.control_bg);
                sf.fill_rrect(&rc(track.left, track.top, kx.round().clamp(track.left as f32, track.right as f32) as i32, track.bottom), 2.0 * s, p.accent);
                sf.fill_circle(kx, cy + 1.0 * s, kr, (0.0, 0.0, 0.0, 0.15));
                sf.fill_circle(kx, cy, kr, p.card);
                sf.stroke_circle(kx, cy, kr, 1.0, (0.0, 0.0, 0.0, 0.10));
                focus_ring(dlg, sf, Hit::Slider(*f), rc(track.left, (cy - kr) as i32 - 2, track.right, (cy + kr) as i32 + 2), s);
            }
        }
        Row::Hotkey(f) => {
            label(f.title());
            let capturing = dlg.capture == Some(*f);
            sf.fill_rrect(&g.badge, 7.0 * s, if capturing { p.input_bg } else { p.control_bg });
            sf.stroke_rrect(&g.badge, 7.0 * s, if capturing { (1.4 * s).max(1.0) } else { s.max(1.0) }, if capturing { p.accent } else { p.separator });
            let (text, col) = if capturing {
                match &dlg.capture_msg {
                    Some(m) => (m.clone(), rgb(0xDC2626, 1.0)),
                    None => ("请按下组合键（Esc 取消）".into(), p.accent),
                }
            } else {
                (hotkey::parse(&f.get_text(cfg).unwrap_or_default()).map(|h| h.display_pretty()).unwrap_or_default(), p.text)
            };
            // 仅左右内缩，保留完整垂直高度，防止高 DPI 下字体上下被裁剪
            let badge_inner = rc(g.badge.left + dlg.px(10.0), g.badge.top, g.badge.right - dlg.px(10.0), g.badge.bottom);
            draw_text(dc, &text, dlg.font_body, &badge_inner, col, TextAlign::Center, true, false);
            focus_ring(dlg, sf, Hit::Hotkey(*f), g.badge, s);
        }
        Row::Text(f) => {
            label(f.title());
            let path = f.get_text(cfg).unwrap_or_default();
            sf.fill_rrect(&g.edit, 7.0 * s, p.input_bg);
            sf.stroke_rrect(&g.edit, 7.0 * s, s.max(1.0), p.separator);
            let (txt, col) = if path.is_empty() { ("未设置".to_string(), (p.text_secondary.0, p.text_secondary.1, p.text_secondary.2, 0.7)) } else { (path, p.text) };
            let edit_inner = rc(g.edit.left + dlg.px(10.0), g.edit.top, g.edit.right - dlg.px(10.0), g.edit.bottom);
            draw_text(dc, &txt, dlg.font_sub, &edit_inner, col, TextAlign::Left, true, true);
            if let Some(b3) = g.button3 {
                draw_button(sf, dc, &b3, "自动查找", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::AutoDetect(*f), dlg.pressed == Hit::AutoDetect(*f), s);
                focus_ring(dlg, sf, Hit::AutoDetect(*f), b3, s);
            }
            draw_button(sf, dc, &g.button, "浏览", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::Browse(*f), dlg.pressed == Hit::Browse(*f), s);
            if let Some(b) = g.button2 {
                let hov = dlg.hover == Hit::ClearPath(*f);
                let (cx, cy) = ((b.left + b.right) as f32 * 0.5, (b.top + b.bottom) as f32 * 0.5);
                if hov {
                    sf.fill_circle(cx, cy, 9.0 * s, (p.text.0, p.text.1, p.text.2, 0.08));
                }
                let a = 3.2 * s;
                let col = if hov { rgb(0xDC2626, 1.0) } else { p.text_secondary };
                sf.thick_line(cx - a, cy - a, cx + a, cy + a, (1.5 * s).max(1.0), col);
                sf.thick_line(cx - a, cy + a, cx + a, cy - a, (1.5 * s).max(1.0), col);
            }
            focus_ring(dlg, sf, Hit::Browse(*f), g.button, s);
        }
        Row::Button(id, text) => {
            if *id == 9 {
                label("重置当前页设置");
            }
            let hit = Hit::Button(*id);
            draw_button(sf, dc, &g.button, text, dlg.font_btn, BtnStyle::Secondary, dlg.hover == hit, dlg.pressed == hit, s);
            focus_ring(dlg, sf, hit, g.button, s);
        }
        Row::DockList => paint_dock_list(dlg, app, sf, dc, &g),
        Row::TrayList => paint_tray_list(dlg, app, sf, dc, &g),
        Row::Exclude => paint_exclude(dlg, app, sf, dc, &g),
        Row::About => {}
    }
}

fn paint_dock_list(dlg: &Dlg, app: &App, sf: &mut Surface, dc: HDC, g: &Geom) {
    let s = dlg.scale;
    let p = &dlg.pal;
    draw_text(dc, "Dock 固定项", dlg.font_body, &g.label, p.text, TextAlign::Left, true, false);
    for (i, text, hit) in [(0, "重置", Hit::DockReset), (1, "添加", Hit::DockAdd), (2, "移除", Hit::DockRemove)] {
        if let Some(r) = g.small[i] {
            draw_button(sf, dc, &r, text, dlg.font_sub, BtnStyle::Secondary, dlg.hover == hit, dlg.pressed == hit, s);
            focus_ring(dlg, sf, hit, r, s);
        }
    }
    for (i, text, hit) in [(3, "↓", Hit::DockDown), (4, "↑", Hit::DockUp)] {
        if let Some(r) = g.small[i] {
            draw_button(sf, dc, &r, text, dlg.font_btn, BtnStyle::Secondary, dlg.hover == hit, dlg.pressed == hit, s);
            focus_ring(dlg, sf, hit, r, s);
        }
    }
    let Some((list, rows, _)) = &g.list else { return };
    sf.fill_rrect(list, 8.0 * s, p.input_bg);
    let sorted = app.dock_items.sorted();
    if sorted.is_empty() {
        draw_text(dc, "暂无固定项：点击「添加」或把程序拖到 Dock 上", dlg.font_sub, list, p.text_secondary, TextAlign::Center, false, false);
        return;
    }
    with_gdi_clip(sf, dc, *list, |sf| {
        let isz = dlg.px(24.0);
        for (vi, r) in rows.iter().enumerate() {
            let idx = dlg.dock_scroll + vi;
            let Some(it) = sorted.get(idx) else { break };
            let rr = inset(*r, dlg.px(3.0));
            if idx == dlg.dock_sel {
                sf.fill_rrect(&rr, 7.0 * s, accent_alpha(p, dlg.dark, 0.12));
            } else if dlg.hover == Hit::DockRow(idx) {
                sf.fill_rrect(&rr, 7.0 * s, p.card_hover);
            }
            let icx = r.left as f32 + dlg.px(14.0) as f32 + isz as f32 * 0.5;
            let icy = (r.top + r.bottom) as f32 * 0.5;
            let half = isz as f32 * 0.5;
            if it.kind == "winlaunchpad" {
                draw_launchpad_icon(sf, icx, icy, isz as f32);
            } else {
                let key = dock_icon_key(it);
                app.icon_worker.request(&key, DOCK_ICON_SRC_SIZE);
                if let Some(px) = app.icons.pixels(&key, DOCK_ICON_SRC_SIZE) {
                    sf.draw_icon_bilinear(&px, icx, icy, isz as f32, false, 0.0);
                } else {
                    sf.fill_rrect(&rc((icx - half) as i32, (icy - half) as i32, (icx + half) as i32, (icy + half) as i32), 6.0 * s, p.control_bg);
                }
            }
            let tx = (icx + half) as i32 + dlg.px(10.0);
            draw_text(dc, &dock_label(it), dlg.font_body, &rc(tx, r.top, r.right - dlg.px(76.0), r.bottom), p.text, TextAlign::Left, true, false);
            draw_text(dc, dock_kind_label(it), dlg.font_sub, &rc(r.right - dlg.px(72.0), r.top, r.right - dlg.px(10.0), r.bottom), p.text_secondary, TextAlign::Right, true, false);
            focus_ring(dlg, sf, Hit::DockRow(idx), rr, s);
        }
    });
    draw_scrollbar(sf, &rc(list.right - dlg.px(4.0), list.top + 2, list.right - 1, list.bottom - 2), sorted.len(), DOCK_VISIBLE, dlg.dock_scroll, false, s);
}

fn paint_tray_list(dlg: &Dlg, app: &App, sf: &mut Surface, dc: HDC, g: &Geom) {
    let s = dlg.scale;
    let p = &dlg.pal;
    draw_text(dc, "托盘应用", dlg.font_body, &g.label, p.text, TextAlign::Left, true, false);
    draw_button(sf, dc, &g.button, "刷新", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::TrayRefresh, dlg.pressed == Hit::TrayRefresh, s);
    focus_ring(dlg, sf, Hit::TrayRefresh, g.button, s);
    let Some((list, rows, _)) = &g.list else { return };
    sf.fill_rrect(list, 8.0 * s, p.input_bg);
    let items = tray_items(app);
    if items.is_empty() {
        draw_text(dc, "暂未发现托盘图标", dlg.font_sub, list, p.text_secondary, TextAlign::Center, false, false);
        return;
    }
    with_gdi_clip(sf, dc, *list, |sf| {
        let isz = dlg.px(20.0);
        for (vi, r) in rows.iter().enumerate() {
            let idx = dlg.tray_scroll + vi;
            let Some(it) = items.get(idx) else { break };
            let pinned = app.config.topbar.pinned_tray_keys.iter().any(|k| *k == it.key);
            let rr = inset(*r, dlg.px(3.0));
            if dlg.hover == Hit::TrayRow(idx) {
                sf.fill_rrect(&rr, 7.0 * s, p.card_hover);
            }
            let icx = r.left as f32 + dlg.px(14.0) as f32 + isz as f32 * 0.5;
            let icy = (r.top + r.bottom) as f32 * 0.5;
            let half = isz as f32 * 0.5;
            sf.fill_rrect(&rc((icx - half) as i32, (icy - half) as i32, (icx + half) as i32, (icy + half) as i32), 5.0 * s, p.control_bg);
            let live_px = it
                .uia_key
                .as_deref()
                .and_then(|k| app.tray_icons.get(k))
                .or_else(|| app.tray_icons.get(&format!("win:{}", it.key)))
                .or_else(|| app.tray_icons.get(&it.key));
            if let Some(px) = live_px {
                sf.draw_icon_bilinear(&px, icx, icy, isz as f32, false, 0.0);
            } else if let Some(exe) = &it.app_exe {
                app.icon_worker.request(exe, DOCK_ICON_SRC_SIZE);
                if let Some(px) = app.icons.pixels(exe, DOCK_ICON_SRC_SIZE) {
                    sf.draw_icon_bilinear(&px, icx, icy, isz as f32, false, 0.0);
                }
            }
            let pill = rc(r.right - dlg.px(74.0), (icy - dlg.px(11.0) as f32) as i32, r.right - dlg.px(10.0), (icy + dlg.px(11.0) as f32) as i32);
            sf.fill_rrect(&pill, 6.0 * s, if pinned { accent_alpha(p, dlg.dark, 0.14) } else { p.control_bg });
            draw_text(dc, if pinned { "已固定" } else { "收进折叠" }, dlg.font_sub, &pill, if pinned { p.accent } else { p.text_secondary }, TextAlign::Center, true, false);
            let name: String = it.name.chars().take(48).collect();
            draw_text(dc, &name, dlg.font_body, &rc((icx + half) as i32 + dlg.px(10.0), r.top, pill.left - dlg.px(8.0), r.bottom), p.text, TextAlign::Left, true, false);
            focus_ring(dlg, sf, Hit::TrayRow(idx), rr, s);
        }
    });
    draw_scrollbar(sf, &rc(list.right - dlg.px(4.0), list.top + 2, list.right - 1, list.bottom - 2), items.len(), TRAY_VISIBLE, dlg.tray_scroll, false, s);
}

fn paint_exclude(dlg: &Dlg, app: &App, sf: &mut Surface, dc: HDC, g: &Geom) {
    let s = dlg.scale;
    let p = &dlg.pal;
    draw_text(dc, "不参与取色的应用", dlg.font_body, &g.label, p.text, TextAlign::Left, true, false);
    draw_button(sf, dc, &g.button, "添加", dlg.font_sub, BtnStyle::Secondary, dlg.hover == Hit::ExcludeAdd, dlg.pressed == Hit::ExcludeAdd, s);
    focus_ring(dlg, sf, Hit::ExcludeAdd, g.button, s);
    let Some((list, rows, btns)) = &g.list else { return };
    sf.fill_rrect(list, 8.0 * s, p.input_bg);
    let ex = &app.config.topbar.adaptive_color_exclude;
    if ex.is_empty() {
        draw_text(dc, "未设置排除项：所有最大化窗口都会参与取色", dlg.font_sub, list, p.text_secondary, TextAlign::Center, false, false);
        return;
    }
    with_gdi_clip(sf, dc, *list, |sf| {
        for (vi, r) in rows.iter().enumerate() {
            let idx = dlg.exclude_scroll + vi;
            let Some(name) = ex.get(idx) else { break };
            draw_text(dc, name, dlg.font_body, &rc(r.left + dlg.px(14.0), r.top, btns[vi].left - dlg.px(8.0), r.bottom), p.text, TextAlign::Left, true, false);
            let b = btns[vi];
            let hit = Hit::ExcludeRemove(idx);
            let hov = dlg.hover == hit;
            let (cx, cy) = ((b.left + b.right) as f32 * 0.5, (b.top + b.bottom) as f32 * 0.5);
            if hov {
                sf.fill_circle(cx, cy, 10.0 * s, rgb(0xFEE2E2, if dlg.dark { 0.25 } else { 1.0 }));
            }
            let a = 3.4 * s;
            let col = if hov { rgb(0xDC2626, 1.0) } else { p.text_secondary };
            sf.thick_line(cx - a, cy - a, cx + a, cy + a, (1.5 * s).max(1.0), col);
            sf.thick_line(cx - a, cy + a, cx + a, cy - a, (1.5 * s).max(1.0), col);
            focus_ring(dlg, sf, hit, b, s);
        }
    });
    draw_scrollbar(sf, &rc(list.right - dlg.px(4.0), list.top + 2, list.right - 1, list.bottom - 2), ex.len(), EXCLUDE_VISIBLE, dlg.exclude_scroll, false, s);
}

fn paint_about(dlg: &mut Dlg, app: &App, l: &Layout, sf: &mut Surface, dc: HDC) {
    let s = dlg.scale;
    let p = &dlg.pal;
    let a = &l.about;
    let u = &dlg.update;
    let px = |v: f32| (v * s).round() as i32;
    let left = |v: i32, top: i32, bottom: i32, right: i32| rc(v, top, right, bottom);

    // 头部
    sf.fill_rrect(&a.hero, 14.0 * s, accent_alpha(p, dlg.dark, 0.10));
    draw_app_badge(sf, (a.hero.left + px(56.0)) as f32, (a.hero.top + px(48.0)) as f32, 64.0 * s, s);
    let tx = a.hero.left + px(104.0);
    draw_text(dc, "WinDockFinder", dlg.font_page, &rc(tx, a.hero.top + px(14.0), a.hero.right - px(16.0), a.hero.top + px(50.0)), p.text, TextAlign::Left, true, false);
    draw_text(dc, &format!("v{}  ·  CrazyFigure", update::VERSION), dlg.font_section, &rc(tx, a.hero.top + px(50.0), a.hero.right - px(16.0), a.hero.top + px(70.0)), p.accent, TextAlign::Left, true, false);
    draw_text(dc, "macOS 风格 Dock 与顶部菜单栏 for Windows 11", dlg.font_body, &rc(tx, a.hero.top + px(70.0), a.hero.right - px(16.0), a.hero.top + px(88.0)), p.text_secondary, TextAlign::Left, true, false);
    let _ = left;

    // 更新卡片
    draw_card(sf, &a.update, s, p);
    draw_text(dc, "软件更新", dlg.font_section, &rc(a.update.left + px(16.0), a.update.top + px(12.0), a.btn.left - px(12.0), a.update.top + px(34.0)), p.text, TextAlign::Left, true, false);
    let status = match u.progress {
        Some(pr) if u.busy => format!("正在下载 {}%", pr / 100),
        _ => u.status.clone(),
    };
    let scol = if u.failed { rgb(0xDC2626, 1.0) } else { p.text_secondary };
    draw_text(dc, &status, dlg.font_body, &rc(a.update.left + px(16.0), a.update.top + px(36.0), a.btn.left - px(12.0), a.update.top + px(58.0)), scol, TextAlign::Left, true, false);
    if u.busy {
        sf.fill_rrect(&a.btn, 8.0 * s, p.control_bg);
        draw_text(dc, "处理中", dlg.font_btn, &a.btn, p.text_secondary, TextAlign::Center, true, false);
    } else {
        draw_button(sf, dc, &a.btn, if u.bundle.is_some() { "一键更新" } else { "检测更新" }, dlg.font_btn, BtnStyle::Primary, dlg.hover == Hit::UpdateBtn, dlg.pressed == Hit::UpdateBtn, s);
    }
    focus_ring(dlg, sf, Hit::UpdateBtn, a.btn, s);
    sf.fill_rect(&rc(a.update.left + px(16.0), a.auto_row.top, a.update.right - px(16.0), a.auto_row.top + 1), sep_color(p));
    if dlg.hover == Hit::UpdateToggle {
        sf.fill_rrect(&rc(a.auto_row.left + 1, a.auto_row.top + 1, a.update.right - 1, a.update.bottom - 1), 9.0 * s, p.card_hover);
    }
    draw_text(dc, Field::AutoCheckUpdate.title(), dlg.font_body, &rc(a.auto_row.left + px(16.0), a.auto_row.top, a.auto_toggle.left - px(12.0), a.auto_row.bottom), p.text, TextAlign::Left, true, false);
    draw_toggle(sf, &a.auto_toggle, app.config.general.auto_check_update, dlg.hover == Hit::UpdateToggle, dlg.pressed == Hit::UpdateToggle, s);
    focus_ring(dlg, sf, Hit::UpdateToggle, a.auto_toggle, s);

    for (i, text) in ["GitHub 仓库", "腾讯云 CNB", "本地配置目录"].iter().enumerate() {
        if let Some(r) = a.links[i] {
            let hit = Hit::Link(i as u8);
            draw_button(sf, dc, &r, text, dlg.font_btn, BtnStyle::Secondary, dlg.hover == hit, dlg.pressed == hit, s);
            focus_ring(dlg, sf, hit, r, s);
        }
    }

    // 更新日志
    draw_card(sf, &a.log, s, p);
    draw_text(dc, "更新日志", dlg.font_section, &rc(a.log.left + px(16.0), a.log.top + px(10.0), a.log.right - px(16.0), a.log.top + px(34.0)), p.text, TextAlign::Left, true, false);
    let (_, raw_total) = dlg.changelog(dc, a.log_view);
    let max_log_scroll = (raw_total - (a.log_view.bottom - a.log_view.top)).max(0);
    if dlg.update.scroll > max_log_scroll {
        dlg.update.scroll = max_log_scroll;
    }
    let (items, total) = dlg.changelog(dc, a.log_view);
    let notes = update::bundled_notes();
    let notes = &notes[..notes.len().min(10)];
    let expanded_set = &dlg.update.expanded;
    let mut emoji_lines: Vec<(RECT, String)> = Vec::new();
    with_gdi_clip(sf, dc, a.log_view, |sf| {
        for it in &items {
            match *it {
                ClItem::Header(ni, r) => {
                    let Some(n) = notes.get(ni) else { continue };
                    if r.bottom < a.log_view.top || r.top > a.log_view.bottom {
                        continue;
                    }
                    let expanded = expanded_set.contains(&n.version);
                    if dlg.hover == Hit::Version(ni) {
                        sf.fill_rrect(&rc(r.left + px(4.0), r.top + 1, r.right - px(4.0), r.bottom - 1), 6.0 * s, p.card_hover);
                    }
                    draw_text(dc, &format!("v{}", n.version), dlg.font_section, &rc(r.left + px(8.0), r.top, r.left + px(110.0), r.bottom), p.accent, TextAlign::Left, true, false);
                    if !n.date.is_empty() {
                        draw_text(dc, &n.date, dlg.font_sub, &rc(r.left + px(112.0), r.top, r.right - px(28.0), r.bottom), p.text_secondary, TextAlign::Left, true, false);
                    }
                    let cx = r.right as f32 - px(14.0) as f32;
                    let cy = (r.top + r.bottom) as f32 * 0.5;
                    let sw = (1.6 * s).max(1.1);
                    if expanded {
                        sf.thick_line(cx - 3.2 * s, cy - 1.5 * s, cx, cy + 2.0 * s, sw, p.text_secondary);
                        sf.thick_line(cx, cy + 2.0 * s, cx + 3.2 * s, cy - 1.5 * s, sw, p.text_secondary);
                    } else {
                        sf.thick_line(cx - 1.5 * s, cy - 3.2 * s, cx + 2.0 * s, cy, sw, p.text_secondary);
                        sf.thick_line(cx + 2.0 * s, cy, cx - 1.5 * s, cy + 3.2 * s, sw, p.text_secondary);
                    }
                    focus_ring(dlg, sf, Hit::Version(ni), r, s);
                }
                ClItem::Section(r, ni, si) => {
                    if r.bottom < a.log_view.top || r.top > a.log_view.bottom {
                        continue;
                    }
                    if let Some(sec) = notes.get(ni).and_then(|n| n.sections.get(si)) {
                        draw_text(dc, &sec.title, dlg.font_section, &rc(r.left + px(4.0), r.top, r.right, r.bottom), p.text, TextAlign::Left, true, false);
                    }
                }
                ClItem::Item(r, ni, si, ii) => {
                    if r.bottom < a.log_view.top || r.top > a.log_view.bottom {
                        continue;
                    }
                    if let Some(t) = notes.get(ni).and_then(|n| n.sections.get(si)).and_then(|sc| sc.items.get(ii)) {
                        emoji_lines.push((r, format!("• {t}")));
                    }
                }
            }
        }
    });
    if !emoji_lines.is_empty() {
        let pairs: Vec<(RECT, &str)> = emoji_lines.iter().map(|(r, t)| (*r, t.as_str())).collect();
        let d2d_ok = dlg
            .color_text
            .as_mut()
            .is_some_and(|ct| ct.draw_wrapped_lines(dc, &a.log_view, px(13.0), p.text_secondary, &pairs));
        if !d2d_ok {
            with_gdi_clip(sf, dc, a.log_view, |_| {
                for (r, txt) in &pairs {
                    draw_wrapped_text(dc, txt, dlg.font_body, r, p.text_secondary);
                }
            });
        }
    }
    draw_scrollbar(
        sf,
        &rc(a.log_view.right + px(2.0), a.log_view.top, a.log_view.right + px(6.0), a.log_view.bottom),
        total.max(0) as usize,
        (a.log_view.bottom - a.log_view.top).max(0) as usize,
        dlg.update.scroll.max(0) as usize,
        false,
        s,
    );
}

// ---------- 窗口过程 ----------

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        let param = (*cs).lpCreateParams;
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, param as isize);
        if !param.is_null() {
            (*(param as *mut Dlg)).hwnd = hwnd;
        }
        return LRESULT(1);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dlg;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    if msg == WM_CREATE {
        (*ptr).scale = window_dpi(hwnd) as f32 / 96.0;
        (*ptr).rebuild_fonts();
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        (*ptr).dispose();
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr));
        return LRESULT(0);
    }
    let dlg = &mut *ptr;
    let app = &mut *dlg.app;
    crate::app::hold_app(|| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(dlg, app, hwnd, msg, wparam, lparam)));
        match result {
            Ok(v) => v,
            Err(_) => {
                crate::log::log("设置窗口处理消息时 panic");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    })
}

fn handle(dlg: &mut Dlg, app: &mut App, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(dlg, app);
                LRESULT(0)
            }
            WM_ACTIVATE | WM_KILLFOCUS if dlg.capture.is_some() && (msg == WM_KILLFOCUS || loword(wparam.0) == 0) => {
                dlg.cancel_capture(app);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = *(lparam.0 as *const RECT);
                dlg.scale = hiword(wparam.0) as f32 / 96.0;
                dlg.rebuild_fonts();
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_TIP {
                    let _ = KillTimer(hwnd, TIMER_TIP);
                    if let Some((_, r, text)) = &dlg.tip_pending {
                        dlg.tip_visible = Some((*r, text.clone()));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let dc = dlg.dc();
                let (x, y) = (get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                let l = dlg.layout(dc, app);
                if let Some(f) = dlg.drag_slider {
                    dlg.set_slider(app, f, &l, x);
                }
                let hit = dlg.hit_test(&l, dc, app, x, y);
                dlg.update_tip(&l, app, dc, hit);
                if hit != dlg.hover {
                    dlg.hover = hit;
                    if hit != Hit::None {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                } else if dlg.drag_slider.is_some() {
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                dlg.hover = Hit::None;
                dlg.hover_row = None;
                dlg.tip_pending = None;
                dlg.tip_visible = None;
                let _ = KillTimer(hwnd, TIMER_TIP);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let dc = dlg.dc();
                let mut pt = POINT { x: get_x_lparam(lparam.0), y: get_y_lparam(lparam.0) };
                let _ = ScreenToClient(hwnd, &mut pt);
                let l = dlg.layout(dc, app);
                let step = if get_wheel_delta(wparam.0) > 0 { -1 } else { 1 };
                if dlg.page == Page::About {
                    if inside(&l.about.log_view, pt.x, pt.y) {
                        let (_, total) = dlg.changelog(dc, l.about.log_view);
                        let max = (total - (l.about.log_view.bottom - l.about.log_view.top)).max(0);
                        dlg.update.scroll = (dlg.update.scroll + step * dlg.px(60.0)).clamp(0, max);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                if let Some(ri) = dlg.hover_row {
                    let g = dlg.geom(dc, app, &l, ri);
                    if let Some((list, _, _)) = &g.list {
                        if inside(list, pt.x, pt.y) {
                            let (n, cur) = match &dlg.rows[ri] {
                                Row::DockList => (app.dock_items.items.len(), &mut dlg.dock_scroll),
                                Row::TrayList => (tray_items(app).len(), &mut dlg.tray_scroll),
                                Row::Exclude => (app.config.topbar.adaptive_color_exclude.len(), &mut dlg.exclude_scroll),
                                _ => (0, &mut dlg.dock_scroll),
                            };
                            let visible = match &dlg.rows[ri] {
                                Row::Exclude => EXCLUDE_VISIBLE,
                                _ => DOCK_VISIBLE,
                            };
                            let max = n.saturating_sub(visible) as i32;
                            *cur = (*cur as i32 + step).clamp(0, max) as usize;
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                    }
                }
                dlg.scroll = (dlg.scroll + step * dlg.px(72.0)).clamp(0, l.max_scroll);
                let hit = dlg.hit_test(&l, dc, app, pt.x, pt.y);
                dlg.hover = hit;
                dlg.update_tip(&l, app, dc, hit);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let dc = dlg.dc();
                let (x, y) = (get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                let l = dlg.layout(dc, app);
                let hit = dlg.hit_test(&l, dc, app, x, y);
                dlg.tip_visible = None;
                dlg.focus_from_keyboard = false;
                if hit == Hit::DragBar {
                    let _ = ReleaseCapture();
                    let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
                    return LRESULT(0);
                }
                dlg.pressed = hit;
                if hit != Hit::None {
                    dlg.focus = Some(hit);
                    let _ = SetCapture(hwnd);
                }
                if let Hit::Slider(f) = hit {
                    dlg.drag_slider = Some(f);
                    dlg.set_slider(app, f, &l, x);
                }
                if hit == Hit::ScrollBar {
                    let view = l.content.bottom - l.content.top;
                    let mid = (l.track.top + l.track.bottom) / 2;
                    dlg.scroll = (dlg.scroll + if y < mid { -view } else { view }).clamp(0, l.max_scroll);
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let pressed = dlg.pressed;
                let was_drag = dlg.drag_slider.take().is_some();
                let _ = ReleaseCapture();
                let dc = dlg.dc();
                let (x, y) = (get_x_lparam(lparam.0), get_y_lparam(lparam.0));
                let l = dlg.layout(dc, app);
                let hit = dlg.hit_test(&l, dc, app, x, y);
                dlg.pressed = Hit::None;
                if was_drag {
                    app.persist();
                } else if pressed != Hit::None && pressed == hit {
                    dlg.activate(app, hit, dc, &l, 0);
                }
                dlg.update_tip(&l, app, dc, hit);
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                dlg.pressed = Hit::None;
                if dlg.drag_slider.take().is_some() {
                    app.persist();
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN if dlg.capture.is_some() => {
                let vk = wparam.0 as u32;
                let f = dlg.capture.unwrap_or(Field::HotkeyDock);
                let is = |k: VIRTUAL_KEY| k.0 as u32 == vk;
                if is(VK_ESCAPE) {
                    dlg.cancel_capture(app);
                } else if is(VK_BACK) {
                    let def = f.get_text(&Config::default()).unwrap_or_default();
                    match app.assign_hotkey(hotkey_slot(f), &def) {
                        Ok(()) => dlg.cancel_capture(app),
                        Err(e) => dlg.capture_msg = Some(e),
                    }
                } else if !hotkey::is_modifier(vk) {
                    match hotkey::spec_from_current(vk) {
                        Some(spec) => match app.assign_hotkey(hotkey_slot(f), &spec.display()) {
                            Ok(()) => dlg.cancel_capture(app),
                            Err(e) => dlg.capture_msg = Some(format!("{e}（Esc 取消）")),
                        },
                        None => dlg.capture_msg = Some("请至少包含 Ctrl / Alt / Win 之一（Esc 取消）".into()),
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wparam.0 as u32;
                let is = |k: VIRTUAL_KEY| k.0 as u32 == vk;
                let dc = dlg.dc();
                let l = dlg.layout(dc, app);
                if is(VK_ESCAPE) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                if is(VK_TAB) {
                    let items = dlg.focusables(&l, dc, app);
                    if !items.is_empty() {
                        let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                        let pos = dlg.focus.and_then(|f| items.iter().position(|h| *h == f));
                        let next = match pos {
                            Some(p) => items[(p + if shift { items.len() - 1 } else { 1 }) % items.len()],
                            None => items[0],
                        };
                        dlg.focus = Some(next);
                        dlg.focus_from_keyboard = true;
                        if let Some(ri) = dlg.hit_row(next) {
                            let r = l.rows[ri];
                            if r.top < l.content.top {
                                dlg.scroll -= l.content.top - r.top;
                            } else if r.bottom > l.content.bottom {
                                dlg.scroll += r.bottom - l.content.bottom;
                            }
                            dlg.scroll = dlg.scroll.clamp(0, l.max_scroll);
                        }
                    }
                } else if is(VK_LEFT) || is(VK_RIGHT) || is(VK_UP) || is(VK_DOWN) {
                    let dir = if is(VK_LEFT) || is(VK_UP) { -1 } else { 1 };
                    if let Some(h) = dlg.focus {
                        if matches!(h, Hit::Choice(_, _) | Hit::Slider(_)) {
                            dlg.activate(app, h, dc, &l, dir);
                        }
                    }
                } else if is(VK_RETURN) || is(VK_SPACE) {
                    if let Some(h) = dlg.focus {
                        dlg.hover_row = dlg.hit_row(h);
                        dlg.activate(app, h, dc, &l, 0);
                    }
                } else if is(VK_PRIOR) || is(VK_NEXT) {
                    let view = (l.content.bottom - l.content.top).max(1);
                    let step = if is(VK_PRIOR) { -view } else { view };
                    if dlg.page == Page::About {
                        let (_, total) = dlg.changelog(dc, l.about.log_view);
                        let max = (total - view).max(0);
                        dlg.update.scroll = (dlg.update.scroll + step).clamp(0, max);
                    } else {
                        dlg.scroll = (dlg.scroll + step).clamp(0, l.max_scroll);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_fields_map_to_slots() {
        assert_eq!(hotkey_slot(Field::HotkeyDock), HotkeySlot::Dock);
        assert_eq!(hotkey_slot(Field::HotkeyTopbar), HotkeySlot::Topbar);
        assert_eq!(hotkey_slot(Field::HotkeyRestore), HotkeySlot::Restore);
    }

    #[test]
    fn dock_item_labels_follow_kind() {
        let mut it = items::new_item("app", r"C:\Windows\notepad.exe", "记事本");
        assert_eq!(dock_kind_label(&it), "应用");
        assert_eq!(dock_label(&it), "记事本");
        assert_eq!(dock_icon_key(&it).to_lowercase(), r"c:\windows\notepad.exe");
        it.label.clear();
        it.kind = "folder".into();
        it.target = r"C:\Users\me\Documents".into();
        assert_eq!(dock_kind_label(&it), "文件夹");
        assert_eq!(dock_label(&it), "Documents");
    }

    #[test]
    fn pages_cover_all_documented_rows() {
        for page in [Page::General, Page::Dock, Page::Topbar, Page::Appearance] {
            assert!(!pages::rows(page).is_empty());
        }
        assert!(pages::rows(Page::General).iter().any(|r| matches!(r, Row::Hotkey(Field::HotkeyRestore))));
        assert_eq!(pages::rows(Page::About), vec![Row::About]);
    }
}
