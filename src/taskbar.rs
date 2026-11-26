//! Win11 多屏任务栏发现、显隐、防回弹 Hook、恢复记录读写。
//!
//! 安全策略：先创建双栏与守护进程并落盘 `taskbar_recovery.json`，再隐藏任务栏；
//! 退出/崩溃/卡死时按记录 100% 还原任务栏可见性与原始 `ABS_AUTOHIDE` 状态。

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::w;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_NAME_FORMAT, QueryFullProcessImageNameW};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT;
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, APPBARDATA, ABS_AUTOHIDE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, ShowWindowAsync,
    SW_HIDE, SW_SHOWNA, EVENT_OBJECT_SHOW,
};

use crate::config::Store;
use crate::log;
use crate::win32::class_name;

pub const WM_APP_TASKBAR_RESHOW: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 20;

/// 当前处于「隐藏态」的任务栏句柄（防回弹 Hook 依据）。
static HIDDEN: Mutex<Vec<isize>> = Mutex::new(Vec::new());
static HOOK: Mutex<Option<isize>> = Mutex::new(None);
static MAIN_HWND: Mutex<isize> = Mutex::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub session_id: String,
    pub main_pid: u32,
    pub explorer_pid: u32,
    pub original_autohide_state: u32,
    pub hidden_hwnds: Vec<isize>,
    pub hidden_at_unix: u64,
    #[serde(default)]
    pub abnormal_pause: bool,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 进程映像原始路径（保留大小写），失败返回空串。
pub fn process_image_raw(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        crate::win32::from_wide(&buf[..size as usize])
    }
}

/// 进程映像文件名（小写），失败返回空串。
pub fn process_image(pid: u32) -> String {
    process_image_raw(pid).to_lowercase()
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// 枚举本会话 Explorer 的全部任务栏窗口（主屏 Shell_TrayWnd + 副屏 Shell_SecondaryTrayWnd）。
pub fn find_taskbars() -> Vec<HWND> {
    let mut out = Vec::new();
    if let Some(main) = primary_taskbar() {
        out.push(main);
    }
    unsafe {
        let list = &mut out as *mut Vec<HWND> as isize;
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(Some(enum_secondary), LPARAM(list));
    }
    out
}

/// 原生通知区所在的主任务栏，确认它属于当前 Explorer。
pub fn primary_taskbar() -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None).ok()? };
    is_explorer_window(hwnd).then_some(hwnd)
}

unsafe extern "system" fn enum_secondary(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if class_name(hwnd) == "Shell_SecondaryTrayWnd" && is_explorer_window(hwnd) {
        let list = &mut *(lparam.0 as *mut Vec<HWND>);
        list.push(hwnd);
    }
    BOOL::from(true)
}

/// 校验窗口归属进程确为本会话 explorer.exe。
pub fn is_explorer_window(hwnd: HWND) -> bool {
    let img = process_image(window_pid(hwnd));
    img.ends_with("\\explorer.exe")
}

pub fn abm_state() -> u32 {
    unsafe {
        let mut data = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            ..Default::default()
        };
        SHAppBarMessage(ABM_GETSTATE, &mut data) as u32
    }
}

/// 设置任务栏自动隐藏（ABS_AUTOHIDE），返回是否改变。
pub fn set_autohide(enable: bool) -> bool {
    let cur = abm_state();
    let want = if enable { ABS_AUTOHIDE } else { 0 };
    if (cur & ABS_AUTOHIDE) == want {
        return false;
    }
    unsafe {
        let mut data = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            lParam: LPARAM(want as isize),
            ..Default::default()
        };
        SHAppBarMessage(ABM_SETSTATE, &mut data);
    }
    true
}

pub fn hide_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_HIDE);
    }
}

pub fn show_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_SHOWNA);
    }
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd).as_bool() && IsWindowVisible(hwnd).as_bool() }
}

impl RecoveryRecord {
    pub fn load(store: &Store) -> Option<Self> {
        let text = std::fs::read_to_string(store.recovery_path()).ok()?;
        match serde_json::from_str::<RecoveryRecord>(&text) {
            Ok(r) => Some(r),
            Err(e) => {
                log::log(&format!("恢复记录解析失败: {e}"));
                None
            }
        }
    }

    pub fn save(&self, store: &Store) {
        if let Ok(t) = serde_json::to_string_pretty(self) {
            crate::config::write_atomic(&store.recovery_path(), t.as_bytes(), false);
        }
    }

    pub fn clear(store: &Store) {
        let _ = std::fs::remove_file(store.recovery_path());
    }

    /// 独立恢复入口：按记录还原任务栏可见性与原始自动隐藏状态（不依赖主进程状态）。
    pub fn restore(&self) {
        unsafe {
            for &h in &self.hidden_hwnds {
                let hwnd = HWND(h as *mut _);
                if IsWindow(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_SHOWNA);
                }
            }
            // 记录中的任务栏可能已随 Explorer 重启失效：兜底显示当前所有任务栏。
            for hwnd in find_taskbars() {
                let _ = ShowWindowAsync(hwnd, SW_SHOWNA);
            }
        }
        set_autohide(self.original_autohide_state & ABS_AUTOHIDE != 0);
        HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// 安装防回弹 Hook：隐藏态下 Explorer 偷偷把任务栏设回可见时通知主窗口重隐藏。
pub fn install_hook(main_hwnd: HWND) {
    *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) = main_hwnd.0 as isize;
    let mut guard = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    unsafe {
        let h = SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            None,
            Some(on_show_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if !h.is_invalid() {
            *guard = Some(h.0 as isize);
        }
    }
}

pub fn uninstall_hook() {
    let mut guard = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = guard.take() {
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn on_show_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    let hidden = HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
    if hidden.contains(&(hwnd.0 as isize)) {
        let main = *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner());
        if main != 0 {
            let _ = PostMessageW(HWND(main as *mut _), WM_APP_TASKBAR_RESHOW, WPARAM(0), LPARAM(hwnd.0 as isize));
        }
    }
}

pub fn is_hidden(hwnd: HWND) -> bool {
    HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).contains(&(hwnd.0 as isize))
}

/// 任务栏控制器：负责隐藏/恢复、临时显示、防回弹熔断。
pub struct TaskbarController {
    pub store: Store,
    pub record: Option<RecoveryRecord>,
    /// 本会话是否处于异常暂停（上次未正常恢复），需用户手动重新开启。
    pub paused: bool,
    temp_show: bool,
    tray_peek: Option<TrayPeek>,
    rehide_count: u32,
    rehide_window_start: u64,
    pub original_state: u32,
    pub autohide_changed: bool,
}

impl TaskbarController {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            record: None,
            paused: false,
            temp_show: false,
            tray_peek: None,
            rehide_count: 0,
            rehide_window_start: 0,
            original_state: 0,
            autohide_changed: false,
        }
    }

    pub fn active(&self) -> bool {
        self.record.is_some()
    }

    /// 启动时检查上次残留记录并修复；若非正常退出则本会话暂停隐藏。
    pub fn recover_previous(&mut self, current_pid: u32) {
        let Some(rec) = RecoveryRecord::load(&self.store) else { return };
        if rec.main_pid == current_pid {
            return;
        }
        log::log(&format!("发现残留恢复记录（pid {}），执行任务栏修复", rec.main_pid));
        rec.restore();
        self.paused = rec.abnormal_pause;
        RecoveryRecord::clear(&self.store);
    }

    /// 标记异常暂停（供设置页提示）。
    pub fn mark_abnormal(&mut self) {
        if let Some(r) = &mut self.record {
            r.abnormal_pause = true;
            r.save(&self.store);
        }
    }

    /// 隐藏全部任务栏并落盘恢复记录。调用前必须已存在可见桌面入口。
    pub fn hide_all(&mut self, main_pid: u32, autohide: bool) -> bool {
        self.end_tray_peek();
        let bars = find_taskbars();
        if bars.is_empty() {
            log::log("未发现任何任务栏窗口，跳过隐藏");
            return false;
        }
        if self.record.is_none() {
            self.original_state = abm_state();
        }
        let explorer_pid = bars.first().map(|h| window_pid(*h)).unwrap_or(0);
        if autohide {
            self.autohide_changed = set_autohide(true) || self.autohide_changed;
        }
        let hidden: Vec<isize> = bars.iter().map(|h| h.0 as isize).collect();
        for &h in &bars {
            hide_window(h);
        }
        *HIDDEN.lock().unwrap_or_else(|e| e.into_inner()) = hidden.clone();
        self.record = Some(RecoveryRecord {
            session_id: format!("{}-{}", main_pid, now_unix()),
            main_pid,
            explorer_pid,
            original_autohide_state: self.original_state,
            hidden_hwnds: hidden,
            hidden_at_unix: now_unix(),
            abnormal_pause: false,
        });
        if let Some(r) = &self.record {
            r.save(&self.store);
        }
        self.paused = false;
        true
    }

    /// 恢复任务栏：撤销隐藏、还原原始自动隐藏状态、清除记录。
    pub fn restore_all(&mut self) {
        self.end_tray_peek();
        if let Some(rec) = self.record.take() {
            unsafe {
                for &h in &rec.hidden_hwnds {
                    let hwnd = HWND(h as *mut _);
                    if IsWindow(hwnd).as_bool() {
                        let _ = ShowWindowAsync(hwnd, SW_SHOWNA);
                    }
                }
            }
            if self.autohide_changed {
                set_autohide(rec.original_autohide_state & ABS_AUTOHIDE != 0);
                self.autohide_changed = false;
            }
        }
        HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).clear();
        RecoveryRecord::clear(&self.store);
        self.temp_show = false;
    }

    /// 临时显示/隐藏（不改变记录，临时显示期间防回弹放行）。
    pub fn set_temporary_show(&mut self, show: bool) {
        self.end_tray_peek();
        if self.record.is_none() {
            return;
        }
        self.temp_show = show;
        if show {
            for hwnd in find_taskbars() {
                show_window(hwnd);
            }
        } else {
            for hwnd in find_taskbars() {
                hide_window(hwnd);
            }
        }
    }

    pub fn temporary_show(&self) -> bool {
        self.temp_show
    }

    /// 仅为一次托盘交互暂停主任务栏的防回弹，其他任务栏继续隐藏。
    pub fn begin_tray_peek(&mut self) -> Option<TrayPeek> {
        if !self.active() || self.temp_show {
            return None;
        }
        let hwnd = primary_taskbar()?;
        let peek = TrayPeek { hwnd: hwnd.0 as isize, active: Arc::new(Mutex::new(true)) };
        self.tray_peek = Some(peek.clone());
        Some(peek)
    }

    /// 用户切换显隐或 Explorer 重启时，撤销后台借用，新的显隐选择优先。
    pub fn end_tray_peek(&mut self) {
        if let Some(peek) = self.tray_peek.take() {
            *peek.active.lock().unwrap_or_else(|error| error.into_inner()) = false;
        }
    }

    /// 防回弹：Explorer 把隐藏态任务栏设回可见时，带防抖地重新隐藏。
    pub fn run_rehide(&mut self) {
        if self.record.is_none() || self.temp_show {
            return;
        }
        let now = now_unix();
        if self.rehide_window_start == 0 || now.saturating_sub(self.rehide_window_start) > 10 {
            self.rehide_window_start = now;
            self.rehide_count = 0;
        }
        self.rehide_count += 1;
        if self.rehide_count > 60 {
            // 熔断：10 秒内超过 60 次说明系统持续对抗，放弃重隐藏并记录。
            log::log("任务栏防回弹触发熔断，暂时停止重隐藏");
            return;
        }
        for hwnd in find_taskbars() {
            if self.tray_peek.as_ref().is_some_and(|peek| peek.hwnd() == hwnd) {
                continue;
            }
            if is_visible(hwnd) {
                hide_window(hwnd);
            }
        }
    }

    /// Explorer 重启（TaskbarCreated）：重新发现任务栏，仍处于隐藏态则重应用。
    pub fn on_taskbar_created(&mut self) {
        self.end_tray_peek();
        let Some(rec) = self.record.as_mut() else { return };
        let bars = find_taskbars();
        rec.hidden_hwnds = bars.iter().map(|h| h.0 as isize).collect();
        rec.explorer_pid = bars.first().map(|h| window_pid(*h)).unwrap_or(0);
        rec.save(&self.store);
        *HIDDEN.lock().unwrap_or_else(|e| e.into_inner()) = rec.hidden_hwnds.clone();
        if !self.temp_show {
            for hwnd in bars {
                hide_window(hwnd);
            }
        }
    }
}

/// 一次后台托盘交互的任务栏借用。锁内只排队异步窗口操作，保证手动显隐后不会补发旧操作。
#[derive(Clone)]
pub struct TrayPeek {
    hwnd: isize,
    active: Arc<Mutex<bool>>,
}

impl TrayPeek {
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    pub fn set_visible(&self, visible: bool) -> bool {
        let active = self.active.lock().unwrap_or_else(|error| error.into_inner());
        if !*active {
            return false;
        }
        if visible {
            show_window(self.hwnd());
        } else {
            hide_window(self.hwnd());
        }
        true
    }
}

/// 独立紧急恢复：不依赖主进程，读取记录还原并清理。
pub fn standalone_restore(store: &Store) -> Result<(), String> {
    match RecoveryRecord::load(store) {
        Some(rec) => {
            rec.restore();
            RecoveryRecord::clear(store);
            log::log("--restore-taskbar: 已按记录恢复任务栏");
            Ok(())
        }
        None => {
            // 无记录时仍兜底显示全部任务栏并取消自动隐藏以外的影响。
            for hwnd in find_taskbars() {
                show_window(hwnd);
            }
            HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).clear();
            Ok(())
        }
    }
}

/// 等待任务栏恢复可见（更新流程用）。
pub fn wait_taskbar_visible(timeout_ms: u32) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if find_taskbars().iter().any(|h| is_visible(*h)) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}
