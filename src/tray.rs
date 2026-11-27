//! 本应用标准 Windows 托盘图标与右键菜单（含 TaskbarCreated 重建）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, LoadImageW,
    SetForegroundWindow, TrackPopupMenu, HICON, IMAGE_FLAGS, IMAGE_ICON, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, TPM_RIGHTBUTTON, TPM_RETURNCMD,
};

use crate::win32::wide;

pub const CMD_SETTINGS: usize = 1;
pub const CMD_TOGGLE_DOCK: usize = 2;
pub const CMD_TOGGLE_TOPBAR: usize = 3;
pub const CMD_TOGGLE_TASKBAR: usize = 4;
pub const CMD_STARTUP: usize = 5;
pub const CMD_EXIT: usize = 6;

pub struct Tray {
    pub hwnd: HWND,
    pub hicon: HICON,
    pub added: bool,
}

impl Drop for Tray {
    fn drop(&mut self) {
        if !self.hicon.is_invalid() && !self.hicon.0.is_null() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(self.hicon);
            }
        }
    }
}

impl Tray {
    pub fn new(hwnd: HWND, hinst: windows::Win32::Foundation::HINSTANCE) -> Tray {
        let sz = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let hicon = unsafe {
            LoadImageW(
                hinst,
                PCWSTR(1usize as *const u16),
                IMAGE_ICON,
                sz,
                sz,
                IMAGE_FLAGS(0x8000), // LR_DEFAULTCOLOR
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        Tray { hwnd, hicon, added: false }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut tip = [0u16; 128];
        for (i, c) in wide("WinDock").into_iter().take(127).enumerate() {
            tip[i] = c;
        }
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: crate::app::WM_APP_TRAY,
            hIcon: self.hicon,
            szTip: tip,
            ..Default::default()
        }
    }

    pub fn add(&mut self) {
        unsafe {
            if self.added {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data());
            } else {
                self.added = Shell_NotifyIconW(NIM_ADD, &self.data()).as_bool();
            }
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            self.added = false;
        }
    }

    /// 弹出右键菜单，返回所选命令 id（0 = 未选择）。
    pub fn show_menu(&self, dock_visible: bool, topbar_visible: bool, taskbar_hidden: bool, startup_on: bool, startup_disabled: bool) -> usize {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return 0 };
            let t = |s: String| -> Vec<u16> { wide(s) };
            let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, PCWSTR(t("WinDock 设置".into()).as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE_DOCK, PCWSTR(t(if dock_visible { "隐藏 Dock".into() } else { "显示 Dock".into() }).as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE_TOPBAR, PCWSTR(t(if topbar_visible { "隐藏顶部栏".into() } else { "显示顶部栏".into() }).as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE_TASKBAR, PCWSTR(t(if taskbar_hidden { "显示 Windows 任务栏".into() } else { "隐藏 Windows 任务栏".into() }).as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let startup_label = match (startup_on, startup_disabled) {
                (true, true) => "开机自启（已被任务管理器禁用）".to_string(),
                (true, false) => "√ 开机自启".to_string(),
                _ => "开机自启".to_string(),
            };
            let _ = AppendMenuW(menu, MF_STRING, CMD_STARTUP, PCWSTR(t(startup_label).as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, PCWSTR(t("退出 WinDock".into()).as_ptr()));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            // TPM_NONOTIFY | TPM_RETURNCMD：直接返回菜单 id
            let id = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | windows::Win32::UI::WindowsAndMessaging::TPM_NONOTIFY, pt.x, pt.y, 0, self.hwnd, None).0 as usize;
            let _ = DestroyMenu(menu);
            id
        }
    }
}
