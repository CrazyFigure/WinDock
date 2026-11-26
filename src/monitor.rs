//! 共享窗口状态跟踪：前台应用、最大化检测、全屏检测、运行应用列表。
//!
//! 事件驱动：`SetWinEventHook`（进程外）监听前台切换/显示/销毁/位置变化/最小化，
//! 变化时通知主窗口重算快照，后台不做高频轮询。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUERY_USER_NOTIFICATION_STATE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{process_image_raw, window_pid};
use crate::win32::{class_name, monitor_of_window, wide, window_text, MonitorInfo};

pub const WM_APP_MONITOR: u32 = WM_APP + 21;

#[derive(Clone, Debug)]
pub struct WinInfo {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    /// 进程映像完整路径（小写），UWP 为 WindowsApps 路径
    pub exe: String,
    /// 进程映像原始路径（保留大小写）
    pub exe_raw: String,
    /// 面向用户展示的应用名称（中文/正确大小写）
    pub display_name: String,
    pub minimized: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MaximizedWindow {
    pub monitor: isize,
    pub hwnd: isize,
    pub exe: String,
    pub rect: RECT,
}

#[derive(Clone, Debug, Default)]
pub struct MonitorState {
    pub foreground: isize,
    pub foreground_exe: String,
    pub foreground_name: String,
    pub foreground_title: String,
    /// 当前活跃显示器的最大化窗口所在显示器（句柄值）
    pub maximized_monitor: isize,
    /// 当前活跃显示器的最大化窗口句柄
    pub maximized_hwnd: isize,
    /// 当前活跃显示器的最大化窗口进程路径（小写）
    pub maximized_exe: String,
    /// 前台最大化窗口矩形（用于标题栏取色）
    pub maximized_rect: Option<RECT>,
    /// 各显示器按 Z 序最上层可见（未最小化）最大化窗口列表
    pub maximized_windows: Vec<MaximizedWindow>,
    pub fullscreen: bool,
    pub windows: Vec<WinInfo>,
}

impl MonitorState {
    /// 获取指定显示器最上层可见最大化窗口。
    pub fn maximized_on(&self, hmon: isize) -> Option<&MaximizedWindow> {
        self.maximized_windows.iter().find(|w| w.monitor == hmon)
    }

    /// 判断指定显示器当前是否存在最大化窗口。
    pub fn is_monitor_maximized(&self, hmon: isize) -> bool {
        self.maximized_on(hmon).is_some()
            || (self.maximized_monitor != 0 && self.maximized_monitor == hmon)
    }
}

// SAFETY: 状态仅在 UI 线程读写，句柄均以整数保存，可安全跨线程共享。
unsafe impl Send for MonitorState {}

/// 运行中的应用（按进程映像归并）。
#[derive(Clone, Debug)]
pub struct RuntimeApp {
    pub key: String,
    pub label: String,
    pub exe: String,
    pub windows: Vec<WinInfo>,
}

static STATE: Mutex<MonitorState> = Mutex::new(MonitorState {
    foreground: 0,
    foreground_exe: String::new(),
    foreground_name: String::new(),
    foreground_title: String::new(),
    maximized_monitor: 0,
    maximized_hwnd: 0,
    maximized_exe: String::new(),
    maximized_rect: None,
    maximized_windows: Vec::new(),
    fullscreen: false,
    windows: Vec::new(),
});
static HOOKS: Mutex<Vec<isize>> = Mutex::new(Vec::new());
static MAIN_HWND: Mutex<isize> = Mutex::new(0);
static LAST_POST: Mutex<Option<Instant>> = Mutex::new(None);
/// 应用名称内存缓存（key: 小写路径，严格限制上限 256 条防止常驻膨胀）。
static NAME_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// 判断是否为顶部栏、Dock、自绘菜单或浮窗等内部壳层窗口（设置窗口按普通应用窗口参与前景与列表跟踪）。
fn is_our_window(class: &str) -> bool {
    (class.starts_with("WinDockFinder") && class != crate::settings::CLASS_SETTINGS)
        || class == "SysShadow"
}

/// 真正代表桌面壁纸/桌面图标层的窗口类名。
fn is_explicit_desktop_class(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_IconView")
}

/// 托盘溢出气泡、原生右键菜单、工具提示等瞬态系统浮窗，不应使顶部栏退出最大化融合态。
fn is_transient_tray_or_popup_class(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "#32768"
            | "tooltips_class32"
            | "Windows.UI.Input.InputSite.WindowClass"
            | "Windows.UI.Composition.DesktopWindowContentBridge"
    ) || class.starts_with("NotifyIconOverflowWindow")
        || class.starts_with("TopLevelWindowForOverflowXamlIsland")
        || class.starts_with("Xaml_WindowedPopupClass")
        || class.contains("QWindowPopup")
}

fn is_desktop_class(class: &str) -> bool {
    is_explicit_desktop_class(class) || is_transient_tray_or_popup_class(class)
}

/// 常见 Windows 内置应用精准中文名映射。
fn known_system_name(file_lower: &str) -> Option<&'static str> {
    Some(match file_lower {
        "explorer.exe" => crate::win32::FILE_MANAGER_NAME,
        "systemsettings.exe" => "设置",
        "windowsterminal.exe" => "终端",
        "notepad.exe" => "记事本",
        "mspaint.exe" => "画图",
        "calc.exe" | "calculatorapp.exe" => "计算器",
        "taskmgr.exe" => "任务管理器",
        "snippingtool.exe" | "screenclippinghost.exe" => "截图工具",
        "cmd.exe" => "命令提示符",
        "powershell.exe" | "pwsh.exe" => "PowerShell",
        "regedit.exe" => "注册表编辑器",
        "control.exe" => "控制面板",
        "microsoft.photos.exe" | "photos.exe" => "照片",
        "msedge.exe" => "Microsoft Edge",
        "chrome.exe" => "Google Chrome",
        "code.exe" => "Visual Studio Code",
        "wechat.exe" | "weixin.exe" => "微信",
        "qq.exe" => "QQ",
        // 设置窗口作为普通应用展示统一短名称，避免回退为带副标题的 PE 文件描述
        "windockfinder.exe" => "WinDockFinder 设置",
        _ => return None,
    })
}

/// 清理 PE 版本描述/产品名里的商标后缀和无意义通用描述。
fn clean_pe_name(raw: &str) -> Option<String> {
    let mut s = raw
        .replace('®', "")
        .replace('™', "")
        .replace("(TM)", "")
        .replace("(R)", "")
        .trim()
        .to_string();
    if let Some(stripped) = s.strip_suffix(".exe").or_else(|| s.strip_suffix(".EXE")) {
        s = stripped.trim().to_string();
    }
    if s.is_empty() || s.chars().count() > 48 {
        return None;
    }
    let lower = s.to_lowercase();
    let generic = [
        "microsoft windows operating system",
        "windows operating system",
        "windows",
        "operating system",
        "application frame host",
        "applicationframehost",
        "host process",
        "host process for windows services",
        "electron",
        "windows shell experience host",
        "microsoft corporation",
        "cefsharp",
        "nwjs",
        "tauri",
    ];
    if generic.iter().any(|g| lower == *g) {
        return None;
    }
    Some(s)
}

/// 从 PE 资源读取 `FileDescription` / `ProductName`（优先中文语言表，其次当前 UI 语言与英文）。
fn read_pe_display_name(path: &str) -> Option<String> {
    if path.is_empty() || !Path::new(path).is_file() {
        return None;
    }
    let wpath = wide(path);
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), Some(&mut handle));
        if size == 0 || size > 4 * 1024 * 1024 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), 0, size, buf.as_mut_ptr() as *mut _).is_err() {
            return None;
        }
        let ui_lang = GetUserDefaultUILanguage();
        let mut langs: Vec<(u16, u16)> = Vec::new();
        let mut trans_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut trans_len = 0u32;
        if VerQueryValueW(
            buf.as_ptr() as *const _,
            PCWSTR(wide(r"\VarFileInfo\Translation").as_ptr()),
            &mut trans_ptr,
            &mut trans_len,
        )
        .as_bool()
            && !trans_ptr.is_null()
            && trans_len >= 4
        {
            let count = (trans_len as usize) / 4;
            let pairs = std::slice::from_raw_parts(trans_ptr as *const [u16; 2], count);
            for &[lang, cp] in pairs {
                langs.push((lang, cp));
            }
            langs.sort_by_key(|&(lang, _)| match lang {
                0x0804 => 0,
                0x0404 | 0x0C04 | 0x1004 => 1,
                l if l == ui_lang => 2,
                0x0409 => 3,
                _ => 4,
            });
        }
        for fallback in [(0x0804, 0x04B0), (0x0409, 0x04B0), (0x0409, 0x04E4), (0x0409, 0x0000)] {
            if !langs.contains(&fallback) {
                langs.push(fallback);
            }
        }
        for (lang, cp) in langs {
            for field in ["FileDescription", "ProductName"] {
                let query = format!(r"\StringFileInfo\{lang:04X}{cp:04X}\{field}");
                let mut str_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                let mut str_len = 0u32;
                if VerQueryValueW(
                    buf.as_ptr() as *const _,
                    PCWSTR(wide(&query).as_ptr()),
                    &mut str_ptr,
                    &mut str_len,
                )
                .as_bool()
                    && !str_ptr.is_null()
                    && str_len > 1
                {
                    let slice = std::slice::from_raw_parts(str_ptr as *const u16, str_len as usize);
                    let raw = crate::win32::from_wide(slice);
                    if let Some(name) = clean_pe_name(&raw) {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

/// 从磁盘目录尝试还原文件名的真实大小写；全小写英文名自动首字母大写。
fn stem_with_case(path: &str) -> String {
    let resolved = std::fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let chosen = resolved.as_deref().unwrap_or(path);
    let file = chosen.rsplit(['\\', '/']).next().unwrap_or(chosen);
    let stem = file
        .strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or_else(|| Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or(file));
    if stem.is_empty() {
        return "应用".to_string();
    }
    // 若磁盘名称仍全为小写 ASCII，做首字母大写美化
    if stem.chars().all(|c| !c.is_ascii_uppercase()) {
        let mut chars = stem.chars();
        if let Some(first) = chars.next() {
            return format!("{}{}", first.to_uppercase(), chars.as_str());
        }
    }
    stem.to_string()
}

/// 提取窗口标题中适合作为应用名的短称（仅用于 UWP 挂起/无独立描述时兜底）。
fn title_fallback(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() {
        return None;
    }
    let candidate = t
        .rsplit(" - ")
        .next()
        .or_else(|| t.rsplit(" — ").next())
        .unwrap_or(t)
        .trim();
    let chosen = if candidate.chars().count() <= 24 { candidate } else { t };
    let out: String = chosen.chars().take(28).collect();
    if out.eq_ignore_ascii_case("applicationframehost") {
        None
    } else {
        Some(out)
    }
}

/// 获取面向用户展示的应用名称（支持中文、正确大小写，并带内存上限缓存）。
pub fn app_display_name(exe_path: &str, title_hint: Option<&str>) -> String {
    let lower = exe_path.to_lowercase();
    let file_lower = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
    if file_lower == "applicationframehost.exe" {
        if let Some(t) = title_hint.and_then(title_fallback) {
            return t;
        }
        return "系统应用".to_string();
    }
    if let Some(known) = known_system_name(file_lower) {
        return known.to_string();
    }
    let cache = NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if !lower.is_empty() {
        if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&lower).cloned() {
            return hit;
        }
    }
    let mut name = read_pe_display_name(exe_path);
    if name.is_none() && lower.contains("\\windowsapps\\") {
        name = title_hint.and_then(title_fallback);
    }
    let resolved = name.unwrap_or_else(|| stem_with_case(exe_path));
    if !lower.is_empty() && !(lower.contains("\\windowsapps\\") && title_hint.is_some()) {
        let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= 256 {
            map.clear();
        }
        map.insert(lower, resolved.clone());
    }
    resolved
}

pub fn exe_display_name(exe: &str) -> String {
    app_display_name(exe, None)
}

/// 解析窗口对应的真实应用进程（穿透 UWP `ApplicationFrameHost` 找到内部 `CoreWindow` 进程）。
fn resolve_window_process(hwnd: HWND) -> (u32, String, String) {
    let pid = window_pid(hwnd);
    let exe_raw = process_image_raw(pid);
    let exe_lower = exe_raw.to_lowercase();
    let is_frame_host = exe_lower.ends_with("\\applicationframehost.exe")
        || exe_lower == "applicationframehost.exe"
        || class_name(hwnd) == "ApplicationFrameWindow";
    if !is_frame_host {
        return (pid, exe_lower, exe_raw);
    }

    struct ChildProbe {
        host_pid: u32,
        best_pid: u32,
        best_raw: String,
        fallback_pid: u32,
        fallback_raw: String,
    }
    unsafe extern "system" fn enum_child(child: HWND, lparam: LPARAM) -> BOOL {
        let probe = &mut *(lparam.0 as *mut ChildProbe);
        let cpid = window_pid(child);
        if cpid != 0 && cpid != probe.host_pid {
            let craw = process_image_raw(cpid);
            if !craw.is_empty() && !craw.to_lowercase().ends_with("\\applicationframehost.exe") {
                if class_name(child) == "Windows.UI.Core.CoreWindow" {
                    probe.best_pid = cpid;
                    probe.best_raw = craw;
                    return BOOL::from(false);
                }
                if probe.fallback_pid == 0 {
                    probe.fallback_pid = cpid;
                    probe.fallback_raw = craw;
                }
            }
        }
        BOOL::from(true)
    }

    let mut probe = ChildProbe {
        host_pid: pid,
        best_pid: 0,
        best_raw: String::new(),
        fallback_pid: 0,
        fallback_raw: String::new(),
    };
    unsafe {
        let _ = EnumChildWindows(hwnd, Some(enum_child), LPARAM(&mut probe as *mut ChildProbe as isize));
    }
    if probe.best_pid != 0 {
        return (probe.best_pid, probe.best_raw.to_lowercase(), probe.best_raw);
    }
    if probe.fallback_pid != 0 {
        return (probe.fallback_pid, probe.fallback_raw.to_lowercase(), probe.fallback_raw);
    }
    (pid, exe_lower, exe_raw)
}

/// 读取窗口的 DWM 可见矩形（回退 GetWindowRect）。
fn visible_window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut r);
        }
    }
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowCoverage {
    None,
    Maximized,
    Fullscreen,
}

/// 根据窗口矩形、显示器范围、Zoomed 状态与标题栏样式判定覆盖形态（纯函数，便于单测）。
fn classify_window_coverage(
    r: RECT,
    mon_rect: RECT,
    mon_work: RECT,
    is_zoomed: bool,
    has_caption: bool,
) -> WindowCoverage {
    let covers_full = r.left <= mon_rect.left + 1
        && r.right >= mon_rect.right - 1
        && r.top <= mon_rect.top + 1
        && r.bottom >= mon_rect.bottom - 1;
    let covers_work = (r.right - r.left) >= (mon_work.right - mon_work.left - 8)
        && (r.bottom - r.top) >= (mon_work.bottom - mon_work.top - 8);
    // 优先以 Windows 原生 IsZoomed 判定最大化，避免无边框最大化误判成独占全屏
    if is_zoomed {
        WindowCoverage::Maximized
    } else if covers_full && !has_caption {
        WindowCoverage::Fullscreen
    } else if covers_work {
        WindowCoverage::Maximized
    } else {
        WindowCoverage::None
    }
}

unsafe fn window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        4,
    );
    cloaked != 0
}

unsafe fn has_shell_def_view(hwnd: HWND) -> bool {
    FindWindowExW(hwnd, None, w!("SHELLDLL_DefView"), None)
        .is_ok_and(|child| !child.0.is_null())
}

/// 判断屏幕坐标点是否直接命中真正露出的 Windows 桌面壁纸/图标表面（排除被应用窗或自身遮挡的情况）。
pub unsafe fn is_desktop_surface_point(screen_x: i32, screen_y: i32) -> bool {
    let pt_hwnd = WindowFromPoint(windows::Win32::Foundation::POINT {
        x: screen_x,
        y: screen_y,
    });
    if pt_hwnd.0.is_null() {
        return false;
    }
    let root = GetAncestor(pt_hwnd, GA_ROOT);
    let root_cls = class_name(root);
    root_cls == "Progman"
        || (root_cls == "WorkerW"
            && (has_shell_def_view(root) || class_name(pt_hwnd) == "SysListView32"))
}

/// 计算两个矩形的重叠宽高（任意轴无重叠时返回 0, 0）。
fn rect_overlap_size(a: RECT, b: RECT) -> (i32, i32) {
    let w = (a.right.min(b.right) - a.left.max(b.left)).max(0);
    let h = (a.bottom.min(b.bottom) - a.top.max(b.top)).max(0);
    if w == 0 || h == 0 {
        (0, 0)
    } else {
        (w, h)
    }
}

struct BackdropScan {
    target: RECT,
    min_w: i32,
    min_h: i32,
    out: Vec<HWND>,
}

unsafe extern "system" fn enum_dock_backdrops(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = &mut *(lparam.0 as *mut BackdropScan);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || window_cloaked(hwnd) {
        return BOOL::from(true);
    }
    let class = class_name(hwnd);
    if is_our_window(&class) || is_transient_tray_or_popup_class(&class) {
        return BOOL::from(true);
    }
    // 到达桌面图标/壁纸层即终止（Win+D 置前时直接得出空集合；正常情况下方不会再有可见应用）
    if class == "Progman" || (class == "WorkerW" && has_shell_def_view(hwnd)) {
        let (ow, oh) = rect_overlap_size(visible_window_rect(hwnd), scan.target);
        if ow >= scan.min_w && oh >= scan.min_h {
            return BOOL::from(false);
        }
        return BOOL::from(true);
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    // 跳过穿透式透明辅助层及无标题工具浮窗，避免误将隐形辅助层当作背后应用
    if (ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0)) == (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) {
        return BOOL::from(true);
    }
    if (ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0)) != 0 && window_text(hwnd).trim().is_empty() {
        return BOOL::from(true);
    }
    let r = visible_window_rect(hwnd);
    let (ow, oh) = rect_overlap_size(r, scan.target);
    if ow >= scan.min_w && oh >= scan.min_h {
        scan.out.push(hwnd);
        // 若非分层窗口已完整遮盖 Dock 胶囊，无需继续遍历更深 Z 序窗口
        if (ex & WS_EX_LAYERED.0) == 0
            && r.left <= scan.target.left
            && r.right >= scan.target.right
            && r.top <= scan.target.top
            && r.bottom >= scan.target.bottom
        {
            return BOOL::from(false);
        }
    }
    BOOL::from(true)
}

/// 按 Z 序查找真正位于 Dock 胶囊背后的可见外部应用窗口（纯桌面/Win+D 返回空）。
pub fn dock_backdrop_windows(cap_screen: RECT) -> Vec<HWND> {
    let cap_w = (cap_screen.right - cap_screen.left).max(1);
    let cap_h = (cap_screen.bottom - cap_screen.top).max(1);
    let mut scan = BackdropScan {
        target: cap_screen,
        min_w: (cap_w / 6).clamp(24, 96),
        min_h: (cap_h / 3).clamp(10, 24),
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(enum_dock_backdrops), LPARAM(&mut scan as *mut BackdropScan as isize));
    }
    scan.out
}

/// 判断窗口是否为可切换的普通顶层应用窗口（复用已获取的类名避免重复查询）。
fn is_app_window_with_class(hwnd: HWND, class: &str) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 || ex & WS_EX_NOACTIVATE.0 != 0 {
            return false;
        }
        if window_cloaked(hwnd) {
            return false;
        }
        if is_our_window(class) || is_desktop_class(class) {
            return false;
        }
        // 有所有者的弹窗/对话框不作为独立应用
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.is_invalid() && !owner.0.is_null() {
                return false;
            }
        }
        // 必须有标题且为可切换窗口
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        if style & WS_CAPTION.0 == 0
            && style & WS_SYSMENU.0 == 0
            && style & WS_THICKFRAME.0 == 0
            && (ex & WS_EX_APPWINDOW.0 == 0)
        {
            return false;
        }
        !window_text(hwnd).trim().is_empty()
    }
}

struct EnumScan {
    windows: Vec<WinInfo>,
    maximized: Vec<MaximizedWindow>,
    decided_monitors: Vec<isize>,
}

/// 窗口句柄若已无效、隐藏、最小化或处于其他虚拟桌面，不应继续作为瞬态保留目标。
unsafe fn is_live_foreground_candidate(hwnd: HWND) -> bool {
    !hwnd.0.is_null()
        && IsWindow(hwnd).as_bool()
        && IsWindowVisible(hwnd).as_bool()
        && !IsIconic(hwnd).as_bool()
        && !window_cloaked(hwnd)
}

/// 重新计算快照；返回是否发生变化（需要通知 Dock/顶部栏重排或取色）。
pub fn refresh() -> bool {
    let prev = STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut new_state = MonitorState::default();
    let mut scan = EnumScan {
        windows: Vec::new(),
        maximized: Vec::new(),
        decided_monitors: Vec::new(),
    };
    unsafe {
        let fg = GetForegroundWindow();
        let mut fg_ext: Option<(HWND, String, MonitorInfo, RECT, WindowCoverage)> = None;
        if fg.0.is_null() {
            // 自绘菜单/状态面板或原生浮窗刚关闭时的瞬态空前台：保留仍然可见的先前外部前景应用
            if is_live_foreground_candidate(HWND(prev.foreground as *mut _)) {
                new_state.foreground = prev.foreground;
                new_state.foreground_exe = prev.foreground_exe.clone();
                new_state.foreground_name = prev.foreground_name.clone();
                new_state.foreground_title = prev.foreground_title.clone();
                new_state.fullscreen = prev.fullscreen;
            } else {
                new_state.foreground_name = "桌面".to_string();
            }
        } else {
            let class = class_name(fg);
            if is_our_window(&class) || is_transient_tray_or_popup_class(&class) {
                // 点击 Finder/托盘触发自绘面板、右键菜单或系统托盘溢出弹窗时，保留先前外部应用状态
                if is_live_foreground_candidate(HWND(prev.foreground as *mut _)) {
                    new_state.foreground = prev.foreground;
                    new_state.foreground_exe = prev.foreground_exe.clone();
                    new_state.foreground_name = prev.foreground_name.clone();
                    new_state.foreground_title = prev.foreground_title.clone();
                    new_state.fullscreen = prev.fullscreen;
                } else {
                    new_state.foreground_name = "桌面".to_string();
                }
            } else if is_explicit_desktop_class(&class) {
                new_state.foreground = fg.0 as isize;
                new_state.foreground_name = "桌面".to_string();
            } else {
                let (_pid, exe_lower, exe_raw) = resolve_window_process(fg);
                let title = window_text(fg);
                new_state.foreground = fg.0 as isize;
                new_state.foreground_exe = exe_lower.clone();
                new_state.foreground_title = title.clone();
                new_state.foreground_name = app_display_name(&exe_raw, Some(&title));
                if let Some(mon) = monitor_of_window(fg) {
                    let r = visible_window_rect(fg);
                    let style = GetWindowLongPtrW(fg, GWL_STYLE) as u32;
                    let cov = classify_window_coverage(
                        r,
                        mon.rect,
                        mon.work,
                        IsZoomed(fg).as_bool(),
                        (style & WS_CAPTION.0) == WS_CAPTION.0,
                    );
                    if cov == WindowCoverage::Fullscreen {
                        new_state.fullscreen = true;
                    }
                    fg_ext = Some((fg, exe_lower, mon, r, cov));
                }
            }
            // 仅将 D3D 独占全屏(3) / 演示模式(4) 判定为全屏，排除 QUNS_BUSY(2) 防止误隐藏顶部栏
            if matches!(
                SHQueryUserNotificationState(),
                Ok(QUERY_USER_NOTIFICATION_STATE(3..=4))
            ) {
                new_state.fullscreen = true;
            }
        }

        let scan_ptr = &mut scan as *mut EnumScan as isize;
        let _ = EnumWindows(Some(enum_windows), LPARAM(scan_ptr));

        // 兜底：若前台最大化应用因特殊样式未被 enum_windows 计入，补充记录到对应显示器
        if let Some((fg_hwnd, exe_lower, mon, r, WindowCoverage::Maximized)) = fg_ext {
            let hmon = mon.hmonitor.0 as isize;
            if !IsIconic(fg_hwnd).as_bool() && !scan.decided_monitors.contains(&hmon) {
                scan.maximized.push(MaximizedWindow {
                    monitor: hmon,
                    hwnd: fg_hwnd.0 as isize,
                    exe: exe_lower,
                    rect: r,
                });
                scan.decided_monitors.push(hmon);
            }
        }

        let active_hmon = if new_state.foreground != 0 {
            monitor_of_window(HWND(new_state.foreground as *mut _)).map(|m| m.hmonitor.0 as isize)
        } else {
            None
        };
        let primary_max = active_hmon
            .and_then(|hm| scan.maximized.iter().find(|w| w.monitor == hm))
            .or_else(|| scan.maximized.first());
        if let Some(m) = primary_max {
            new_state.maximized_monitor = m.monitor;
            new_state.maximized_hwnd = m.hwnd;
            new_state.maximized_exe = m.exe.clone();
            new_state.maximized_rect = Some(m.rect);
        }
    }
    scan.windows.sort_by(|a, b| (a.hwnd.0 as isize).cmp(&(b.hwnd.0 as isize)));
    new_state.windows = scan.windows;
    new_state.maximized_windows = scan.maximized;
    let changed = {
        let mut cur = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let changed = cur.foreground != new_state.foreground
            || cur.foreground_exe != new_state.foreground_exe
            || cur.foreground_name != new_state.foreground_name
            || cur.maximized_monitor != new_state.maximized_monitor
            || cur.maximized_hwnd != new_state.maximized_hwnd
            || cur.maximized_exe != new_state.maximized_exe
            || cur.maximized_rect != new_state.maximized_rect
            || cur.maximized_windows != new_state.maximized_windows
            || cur.fullscreen != new_state.fullscreen
            || cur.windows.len() != new_state.windows.len()
            || cur
                .windows
                .iter()
                .zip(new_state.windows.iter())
                .any(|(a, b)| a.hwnd != b.hwnd || a.title != b.title || a.minimized != b.minimized || a.exe != b.exe);
        *cur = new_state;
        changed
    };
    changed
}

unsafe extern "system" fn enum_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = &mut *(lparam.0 as *mut EnumScan);
    let class = class_name(hwnd);
    // Win+D 等操作将桌面图标宿主提到窗口前方时，锁定对应显示器为非最大化桌面视图
    if (class == "Progman" || (class == "WorkerW" && has_shell_def_view(hwnd)))
        && IsWindowVisible(hwnd).as_bool()
        && !window_cloaked(hwnd)
    {
        if let Some(mon) = monitor_of_window(hwnd) {
            let r = visible_window_rect(hwnd);
            if (r.right - r.left) >= mon.width() - 2 && (r.bottom - r.top) >= mon.height() - 2 {
                let hmon = mon.hmonitor.0 as isize;
                if !scan.decided_monitors.contains(&hmon) {
                    scan.decided_monitors.push(hmon);
                }
            }
        }
        return BOOL::from(true);
    }

    if is_app_window_with_class(hwnd, &class) {
        let (_pid_val, exe, exe_raw) = resolve_window_process(hwnd);
        let title = window_text(hwnd);
        let display_name = app_display_name(&exe_raw, Some(&title));
        let minimized = IsIconic(hwnd).as_bool();
        if !minimized {
            if let Some(mon) = monitor_of_window(hwnd) {
                let hmon = mon.hmonitor.0 as isize;
                if !scan.decided_monitors.contains(&hmon) {
                    let r = visible_window_rect(hwnd);
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
                    match classify_window_coverage(
                        r,
                        mon.rect,
                        mon.work,
                        IsZoomed(hwnd).as_bool(),
                        (style & WS_CAPTION.0) == WS_CAPTION.0,
                    ) {
                        WindowCoverage::Maximized => {
                            scan.maximized.push(MaximizedWindow {
                                monitor: hmon,
                                hwnd: hwnd.0 as isize,
                                exe: exe.clone(),
                                rect: r,
                            });
                            scan.decided_monitors.push(hmon);
                        }
                        WindowCoverage::Fullscreen => {
                            scan.decided_monitors.push(hmon);
                        }
                        WindowCoverage::None => {}
                    }
                }
            }
        }
        scan.windows.push(WinInfo {
            hwnd,
            pid: _pid_val,
            title,
            exe,
            exe_raw,
            display_name,
            minimized,
        });
    }
    BOOL::from(true)
}

pub fn snapshot() -> MonitorState {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 按进程映像归并运行中的应用（按窗口句柄稳定顺序）。
pub fn running_apps() -> Vec<RuntimeApp> {
    let snap = snapshot();
    let mut apps: Vec<RuntimeApp> = Vec::new();
    for w in snap.windows {
        if w.exe.is_empty() {
            continue;
        }
        if let Some(app) = apps.iter_mut().find(|a| a.key == w.exe) {
            app.windows.push(w);
        } else {
            apps.push(RuntimeApp {
                key: w.exe.clone(),
                label: w.display_name.clone(),
                exe: if w.exe_raw.is_empty() { w.exe.clone() } else { w.exe_raw.clone() },
                windows: vec![w],
            });
        }
    }
    apps
}

pub fn install(main_hwnd: HWND) {
    *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) = main_hwnd.0 as isize;
    let mut hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    if !hooks.is_empty() {
        return;
    }
    unsafe {
        let ranges: [(u32, u32); 4] = [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        ];
        for (a, b) in ranges {
            let h = SetWinEventHook(a, b, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
            if !h.is_invalid() {
                hooks.push(h.0 as isize);
            }
        }
        for ev in [EVENT_OBJECT_DESTROY, EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_MOVESIZEEND] {
            let h = SetWinEventHook(ev, ev, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
            if !h.is_invalid() {
                hooks.push(h.0 as isize);
            }
        }
    }
    refresh();
}

pub fn uninstall() {
    let mut hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    for h in hooks.drain(..) {
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    // 仅处理顶层窗口事件，避免网页或编辑器内子控件产生海量噪音
    if GetAncestor(hwnd, GA_ROOT) != hwnd {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE {
        let fg = GetForegroundWindow();
        if hwnd != fg {
            return;
        }
    }
    let class = class_name(hwnd);
    if is_our_window(&class)
        || (class == "Static" && window_pid(hwnd) == std::process::id())
        || (event != EVENT_SYSTEM_FOREGROUND && is_desktop_class(&class))
    {
        // 遮罩、提示和原生托盘显隐都不改变运行应用列表，避免重复刷新 Dock。
        return;
    }
    // 前台切换与最小化/还原事件（含 Win+D 切换桌面与还原）立即投递不丢帧，其余高频移动事件保留 35ms 节流
    let urgent = matches!(
        event,
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND
    );
    if !urgent {
        let mut last = LAST_POST.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if t.elapsed().as_millis() < 35 {
                return;
            }
        }
        *last = Some(Instant::now());
    }
    let main = *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner());
    if main != 0 {
        let _ = PostMessageW(HWND(main as *mut _), WM_APP_MONITOR, WPARAM(0), LPARAM(0));
    }
}

/// 前台窗口所在显示器（无则主屏）。
pub fn foreground_monitor() -> Option<MonitorInfo> {
    let snap = snapshot();
    if snap.foreground == 0 {
        return None;
    }
    monitor_of_window(HWND(snap.foreground as *mut _))
        .or_else(crate::win32::primary_monitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_names_and_host_cleanup() {
        assert_eq!(app_display_name(r"C:\Windows\SystemApps\SystemSettings.exe", None), "设置");
        assert_eq!(app_display_name(r"C:\Windows\explorer.exe", None), "文件管理器");
        assert_eq!(app_display_name(r"C:\Windows\System32\ApplicationFrameHost.exe", Some("设置")), "设置");
        assert_eq!(app_display_name(r"C:\Windows\System32\applicationframehost.exe", None), "系统应用");
        assert_eq!(clean_pe_name("Microsoft Windows Operating System"), None);
        assert_eq!(clean_pe_name("Application Frame Host"), None);
        assert_eq!(clean_pe_name("Visual Studio Code™"), Some("Visual Studio Code".into()));
    }

    #[test]
    fn desktop_and_transient_class_classification() {
        assert!(is_explicit_desktop_class("Progman"));
        assert!(is_explicit_desktop_class("WorkerW"));
        assert!(!is_explicit_desktop_class("Shell_TrayWnd"));

        assert!(is_transient_tray_or_popup_class("Shell_TrayWnd"));
        assert!(is_transient_tray_or_popup_class("Windows.UI.Input.InputSite.WindowClass"));
        assert!(is_transient_tray_or_popup_class("Windows.UI.Composition.DesktopWindowContentBridge"));
        assert!(is_transient_tray_or_popup_class("TopLevelWindowForOverflowXamlIsland"));
        assert!(is_transient_tray_or_popup_class("Xaml_WindowedPopupClass"));
        assert!(is_transient_tray_or_popup_class("#32768"));
        assert!(!is_transient_tray_or_popup_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn window_coverage_and_per_monitor_state() {
        let mon = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let work = RECT { left: 0, top: 30, right: 1920, bottom: 1080 };
        let normal = RECT { left: 100, top: 100, right: 900, bottom: 700 };
        let zoomed_rect = RECT { left: 0, top: 30, right: 1920, bottom: 1080 };
        assert_eq!(classify_window_coverage(normal, mon, work, false, true), WindowCoverage::None);
        assert_eq!(classify_window_coverage(zoomed_rect, mon, work, true, false), WindowCoverage::Maximized);
        assert_eq!(classify_window_coverage(mon, mon, work, false, false), WindowCoverage::Fullscreen);
        assert_eq!(classify_window_coverage(zoomed_rect, mon, work, false, true), WindowCoverage::Maximized);

        let mut st = MonitorState::default();
        st.maximized_windows.push(MaximizedWindow {
            monitor: 101,
            hwnd: 202,
            exe: r"C:\App\edge.exe".into(),
            rect: zoomed_rect,
        });
        assert!(st.is_monitor_maximized(101));
        assert!(!st.is_monitor_maximized(999));
        assert_eq!(st.maximized_on(101).unwrap().hwnd, 202);
    }

    #[test]
    fn rect_overlap_calculates_intersection_and_disjoint() {
        let a = RECT { left: 100, top: 200, right: 500, bottom: 260 };
        let b = RECT { left: 300, top: 220, right: 700, bottom: 280 };
        let disjoint = RECT { left: 600, top: 200, right: 800, bottom: 260 };
        assert_eq!(rect_overlap_size(a, b), (200, 40));
        assert_eq!(rect_overlap_size(a, disjoint), (0, 0));
    }

    #[test]
    fn settings_window_is_treated_as_regular_app_window() {
        assert!(!is_our_window(crate::settings::CLASS_SETTINGS));
        assert!(!is_desktop_class(crate::settings::CLASS_SETTINGS));
        assert!(is_our_window("WinDockFinderTopBar"));
        assert!(is_our_window("WinDockFinderDockOverlay"));
        assert!(is_our_window("WinDockFinderDockTrigger"));
        assert!(is_our_window("WinDockFinderDockPreview"));
        assert!(is_our_window("WinDockFinderContextMenu"));
        assert!(is_our_window("WinDockFinderStatusPanel"));
        assert!(is_our_window("WinDockFinderPopover"));
        assert!(is_our_window("WinDockFinderMain"));
        assert!(is_our_window("SysShadow"));
        assert_eq!(
            app_display_name(r"C:\Program Files\WinDockFinder\WinDockFinder.exe", Some("WinDockFinder 设置")),
            "WinDockFinder 设置"
        );
    }
}
