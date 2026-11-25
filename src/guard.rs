//! 极简 Watchdog 守护进程（`--guard --parent-pid <pid>`）。
//!
//! 只链接最小 kernel32/user32：不初始化 COM/GDI/WIC/Shell。持有主进程句柄
//! `WaitForSingleObject`，监听 UI 心跳（命名自动重置事件）与紧急恢复快捷键。
//! 正常路径零干预；仅在主进程崩溃、强杀、卡死超时或触发紧急快捷键时，
//! 立即读取恢复记录还原 Windows 任务栏与工作区。

use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, WAIT_EVENT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, OpenProcess, SYNCHRONIZATION_ACCESS_RIGHTS,
    PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Store;
use crate::hotkey::{self, HotkeySpec};
use crate::taskbar::{RecoveryRecord, WM_APP_TASKBAR_RESHOW};

/// EVENT_SYNCHRONIZE（0x00100000）：等待事件 signaling 的访问权。
const EVENT_SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);

pub const HEARTBEAT_INTERVAL_MS: u64 = 2000;
pub const HEARTBEAT_TIMEOUT_MS: u32 = 10_000;
const HOTKEY_ID: i32 = 7101;

pub fn heartbeat_event_name(pid: u32) -> String {
    format!("Local\\WinDockFinder-Heartbeat-{pid}")
}

pub struct Heartbeat {
    event: HANDLE,
}

// SAFETY: 事件句柄仅由心跳线程 SetEvent，可安全跨线程移动。
unsafe impl Send for Heartbeat {}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        if !self.event.is_invalid() && !self.event.0.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.event);
            }
        }
    }
}

impl Heartbeat {
    /// 主进程创建心跳事件并定期 SetEvent。
    pub fn create(pid: u32) -> Option<Self> {
        let name = crate::win32::wide(heartbeat_event_name(pid));
        unsafe {
            let event = CreateEventW(None, false, true, windows::core::PCWSTR(name.as_ptr())).ok()?;
            Some(Self { event })
        }
    }

    pub fn signal(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.event);
        }
    }

    pub fn spawn_thread(self) {
        std::thread::Builder::new()
            .name("heartbeat".into())
            .spawn(move || loop {
                self.signal();
                std::thread::sleep(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
            })
            .ok();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// 主进程正常退出（恢复记录已被主进程清除）
    CleanExit,
    /// 主进程异常退出
    Crash,
    /// 心跳超时
    Hang,
    /// 用户触发紧急恢复快捷键
    Hotkey,
}

struct GuardCtx {
    reason: Option<Reason>,
}

/// 守护进程主循环。返回进程退出码。
pub fn run(parent_pid: u32, hotkey: Option<HotkeySpec>) -> i32 {
    crate::log::init(&crate::win32::data_dir(), "guard");
    let store = Store::open();
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) else {
            // 主进程已不存在：若有残留记录则修复后退出。
            if let Some(rec) = RecoveryRecord::load(&store) {
                rec.restore();
                RecoveryRecord::clear(&store);
            }
            return 0;
        };
        // 等待主进程创建心跳事件（最多 3 秒）
        let name = crate::win32::wide(heartbeat_event_name(parent_pid));
        let mut heartbeat: Option<HANDLE> = None;
        for _ in 0..30 {
            if let Ok(h) = OpenEventW(EVENT_SYNCHRONIZE, BOOL::from(false), windows::core::PCWSTR(name.as_ptr())) {
                heartbeat = Some(h);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let hinst = crate::win32::hinstance();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(guard_wnd_proc),
            hInstance: hinst,
            lpszClassName: w!("WinDockFinderGuard"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("WinDockFinderGuard"),
            w!("WinDockFinderGuard"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        )
        .unwrap_or_default();
        let ctx = Box::new(GuardCtx { reason: None });
        let ctx_ptr = Box::into_raw(ctx) as isize;
        if !hwnd.is_invalid() {
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr);
            if let Some(spec) = hotkey {
                hotkey::register(hwnd, HOTKEY_ID, spec);
            }
        }

        let mut last_beat = std::time::Instant::now();
        let reason = loop {
            let handles: Vec<HANDLE> = {
                let mut v = vec![proc];
                if let Some(h) = heartbeat {
                    v.push(h);
                }
                v
            };
            let r = MsgWaitForMultipleObjects(Some(&handles), false, 250, QS_ALLINPUT);
            // 先把窗口消息（含热键）处理掉
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            let fired = (*(ctx_ptr as *mut GuardCtx)).reason;
            if fired == Some(Reason::Hotkey) {
                break Reason::Hotkey;
            }
            if r == WAIT_OBJECT_0 {
                // 主进程句柄 signaled：进程已退出
                break if RecoveryRecord::load(&store).is_some() {
                    Reason::Crash
                } else {
                    Reason::CleanExit
                };
            }
            if heartbeat.is_some() && r == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                // 心跳到达，重置计时
                last_beat = std::time::Instant::now();
                continue;
            }
            if last_beat.elapsed() > Duration::from_millis(HEARTBEAT_TIMEOUT_MS as u64) {
                break Reason::Hang;
            }
        };
        let _ = ctx_ptr;
        match reason {
            Reason::CleanExit => 0,
            Reason::Crash | Reason::Hang | Reason::Hotkey => {
                let rec = RecoveryRecord::load(&store);
                match rec {
                    Some(mut rec) => {
                        rec.abnormal_pause = true;
                        rec.save(&store);
                        rec.restore();
                        crate::log::log(&format!("guard: 触发任务栏恢复 ({reason:?})"));
                        if reason == Reason::Hang {
                            notify("WinDockFinder 无响应，已自动恢复 Windows 任务栏。");
                        } else if reason == Reason::Hotkey {
                            notify("已按紧急恢复快捷键恢复 Windows 任务栏。");
                        }
                        0
                    }
                    None => {
                        // 没有记录说明主进程已自行恢复
                        0
                    }
                }
            }
        }
    }
}

fn notify(text: &str) {
    let m = crate::win32::wide(text);
    unsafe {
        let _ = MessageBoxW(
            None,
            windows::core::PCWSTR(m.as_ptr()),
            w!("WinDockFinder 守护进程"),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

unsafe extern "system" fn guard_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_HOTKEY && wparam.0 as i32 == HOTKEY_ID {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if ptr != 0 {
            (*(ptr as *mut GuardCtx)).reason = Some(Reason::Hotkey);
        }
        return LRESULT(0);
    }
    if msg == WM_APP_TASKBAR_RESHOW {
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
