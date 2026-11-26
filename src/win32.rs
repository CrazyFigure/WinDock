//! Win32 小工具：宽字符串、错误信息、路径、DPI、显示器、消息参数拆分等。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::core::{w, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_FROM_FLAGS,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

/// Windows 文件管理器在 Dock、顶部栏和设置页中的统一显示名。
pub const FILE_MANAGER_NAME: &str = "文件管理器";

/// 只迁移 Explorer 入口曾经使用过的默认名称，保留自定义名称。
pub fn is_legacy_file_manager_label(target: &str, label: &str) -> bool {
    target
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|file| file.eq_ignore_ascii_case("explorer.exe"))
        && matches!(label, "访达" | "应用管理器")
}

/// 转为以 0 结尾的 UTF-16。
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// 从以 0 结尾的缓冲区取字符串。
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 把静态字符串转为常驻的以 0 结尾 UTF-16 指针（窗口类名等需在调用后仍有效的场景，按内容缓存）。
pub fn wide_static(s: &'static str) -> PCWSTR {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, &'static [u16]>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    PCWSTR(map.entry(s).or_insert_with(|| Box::leak(wide(s).into_boxed_slice())).as_ptr())
}

/// PWSTR -> String，并释放 Shell 分配的内存。
pub fn take_pwstr(p: PWSTR) -> String {
    let s = if p.0.is_null() {
        String::new()
    } else {
        unsafe { p.to_string().unwrap_or_default() }
    };
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// 把系统错误码转成可读文本。
pub fn error_text(err: u32) -> String {
    unsafe {
        let mut buf: [u16; 1024] = [0; 1024];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            err,
            0,
            PWSTR(buf.as_mut_ptr()),
            buf.len() as u32,
            None,
        );
        if n == 0 {
            return format!("错误 {err}");
        }
        from_wide(&buf).trim().to_string()
    }
}

pub fn hresult_text(hr: HRESULT) -> String {
    if hr.is_ok() {
        return String::new();
    }
    let code = hr.0 as u32;
    if (code & 0xFFFF_0000) == 0x8007_0000 {
        error_text(code & 0xFFFF)
    } else {
        format!("HRESULT 0x{code:08X}")
    }
}

pub fn error_string(e: windows::core::Error) -> String {
    hresult_text(e.code())
}

pub fn last_error_text() -> String {
    error_text(unsafe { GetLastError().0 })
}

/// 当前模块实例句柄。
pub fn hinstance() -> HINSTANCE {
    HINSTANCE(unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(|m| m.0)
        .unwrap_or(std::ptr::null_mut()))
}

/// 当前可执行文件路径。
pub fn exe_path() -> PathBuf {
    unsafe {
        let mut buf = [0u16; 1024];
        let n = windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut buf);
        PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// %LOCALAPPDATA%
pub fn local_app_data() -> PathBuf {
    unsafe {
        match SHGetKnownFolderPath(&FOLDERID_LocalAppData, Default::default(), None) {
            Ok(p) => PathBuf::from(take_pwstr(p)),
            Err(_) => std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// 应用数据目录：%LOCALAPPDATA%\WinDockFinder
pub fn data_dir() -> PathBuf {
    local_app_data().join("WinDockFinder")
}

/// 读取窗口类名。
pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

/// 读取窗口标题。
pub fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

/// 取窗口 DPI（未就绪时回退 96）。
pub fn window_dpi(hwnd: HWND) -> u32 {
    let d = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    if d == 0 { 96 } else { d }
}

/// 显示器信息。
#[derive(Clone, Copy, Debug)]
pub struct MonitorInfo {
    pub hmonitor: HMONITOR,
    pub rect: RECT,
    pub work: RECT,
    pub primary: bool,
    pub dpi: u32,
}

impl MonitorInfo {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

pub fn monitor_info(hmonitor: HMONITOR) -> Option<MonitorInfo> {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
            return None;
        }
        let mut dx = 96u32;
        let mut dy = 96u32;
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
            hmonitor,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dx,
            &mut dy,
        );
        Some(MonitorInfo {
            hmonitor,
            rect: mi.rcMonitor,
            work: mi.rcWork,
            primary: mi.dwFlags & 1 != 0,
            dpi: dx,
        })
    }
}

/// 枚举所有显示器（按设备名顺序）。
pub fn all_monitors() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let ctx = &mut out as *mut Vec<MonitorInfo> as *mut _;
        let _ = windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(enum_cb),
            windows::Win32::Foundation::LPARAM(ctx as isize),
        );
    }
    out
}

unsafe extern "system" fn enum_cb(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<MonitorInfo>);
    if let Some(m) = monitor_info(hmonitor) {
        out.push(m);
    }
    BOOL::from(true)
}

/// 主显示器。
pub fn primary_monitor() -> Option<MonitorInfo> {
    let h = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTONEAREST) };
    monitor_info(h)
}

/// 窗口所在显示器。
pub fn monitor_of_window(hwnd: HWND) -> Option<MonitorInfo> {
    let h = unsafe { MonitorFromWindow(hwnd, MONITOR_FROM_FLAGS(0)) };
    monitor_info(h)
}

/// 鼠标所在显示器。
pub fn monitor_at_cursor() -> Option<MonitorInfo> {
    let mut pt = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        monitor_info(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST))
    }
}

/// dp -> 像素。
#[inline]
pub fn dp(v: i32, dpi: u32) -> i32 {
    ((v as i64 * dpi as i64 + 48) / 96) as i32
}

#[inline]
pub fn scale(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

// ---------- lParam / wParam 拆分 ----------

pub fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}
pub fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}
pub fn get_x_lparam(l: isize) -> i32 {
    (l & 0xFFFF) as u16 as i16 as i32
}
pub fn get_y_lparam(l: isize) -> i32 {
    ((l >> 16) & 0xFFFF) as u16 as i16 as i32
}
pub fn get_wheel_delta(w: usize) -> i16 {
    hiword(w) as i16
}

/// 把前台窗口切到指定窗口（绕过前台锁定的常见技巧：先按一下 Alt）。
pub fn bring_to_front(hwnd: HWND) {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let fg = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if fg != hwnd {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
    }
}

/// 关闭自绘菜单/浮层前，若其仍持有前台焦点，则将焦点平稳归还给原外部窗口，
/// 避免因无焦点顶部栏触发窗口隐藏后前台句柄落空（NULL/Shell_TrayWnd）导致状态闪烁。
pub fn restore_foreground_if_owned(popup: HWND, prev_fg: HWND) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        if popup.0.is_null() || prev_fg.0.is_null() || prev_fg == popup {
            return;
        }
        if GetForegroundWindow() == popup
            && IsWindow(prev_fg).as_bool()
            && IsWindowVisible(prev_fg).as_bool()
            && !IsIconic(prev_fg).as_bool()
        {
            let _ = SetForegroundWindow(prev_fg);
        }
    }
}

/// 在资源管理器中打开路径（文件、目录、shell: 路径）。
pub fn open_shell_path(verb: &str, file: &str, params: Option<&str>) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        let wv = wide(verb);
        let wf = wide(file);
        let wp = params.map(wide);
        let r = ShellExecuteW(
            None,
            PCWSTR(wv.as_ptr()),
            PCWSTR(wf.as_ptr()),
            wp.as_ref().map(|v| PCWSTR(v.as_ptr())).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        r.0 as isize > 32
    }
}

/// 打开文件选择对话框（支持多选），返回所选路径；用户取消时为空。
pub fn pick_files(owner: HWND, filter: &str, title: &str) -> Vec<PathBuf> {
    use windows::core::PWSTR;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut buf = vec![0u16; 16384];
    let wfilter: Vec<u16> = format!("{filter}\0\0").encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(wfilter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(wide(title).as_ptr()),
        Flags: OFN_EXPLORER | OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    unsafe {
        if !GetOpenFileNameW(&mut ofn).as_bool() {
            return Vec::new();
        }
    }
    // 多选缓冲区：首个为目录，其后为文件名，以双 0 结尾；单选时仅一个完整路径
    let mut parts = Vec::new();
    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] == 0 {
            if i == start {
                break;
            }
            parts.push(from_wide(&buf[start..i]));
            start = i + 1;
        }
    }
    match parts.len() {
        0 => Vec::new(),
        1 => vec![PathBuf::from(&parts[0])],
        _ => {
            let dir = PathBuf::from(&parts[0]);
            parts[1..].iter().map(|name| dir.join(name)).collect()
        }
    }
}

/// 稳定的 FNV-1a 64 位哈希。
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 查询 Windows 系统浅/深色主题设置（优先 AppsUseLightTheme，缺失时回退 SystemUsesLightTheme）；
/// 仅在系统注册表键均无法读取时返回 None，以便调用方继续降级到壁纸亮度等兜底策略。
pub fn system_theme_dark() -> Option<bool> {
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, REG_VALUE_TYPE,
    };
    for value_name in [w!("AppsUseLightTheme"), w!("SystemUsesLightTheme")] {
        let mut data = 0u32;
        let mut size = 4u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
                value_name,
                RRF_RT_REG_DWORD,
                Some(&mut REG_VALUE_TYPE(0)),
                Some(&mut data as *mut u32 as *mut _),
                Some(&mut size),
            )
        };
        if r.is_ok() {
            return Some(data == 0);
        }
    }
    None
}

/// 系统深色模式（AppsUseLightTheme=0 即深色）。
pub fn system_uses_dark_theme() -> bool {
    system_theme_dark().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_foreground_handles_null_and_self_safely() {
        restore_foreground_if_owned(HWND::default(), HWND::default());
        let pseudo = HWND(0x1234isize as *mut _);
        restore_foreground_if_owned(pseudo, HWND::default());
        restore_foreground_if_owned(pseudo, pseudo);
    }
}
