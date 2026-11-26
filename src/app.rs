//! 主实例：单实例锁、IPC 转发、全局状态机与生命周期协调。
//!
//! 生命周期顺序（安全前置门禁）：创建双栏窗口 → 守护进程握手 → 落盘恢复记录 → 隐藏任务栏。
//! 退出顺序：撤销 AppBar → 恢复任务栏 → 通知守护进程 → 清除恢复标记。

use std::cell::Cell;
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIIF_INFO, NIF_INFO, NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Config, DockItem, DockItems, Store};
use crate::dock::icons::{IconCache, IconResult, IconWorker, DOCK_ICON_SRC_SIZE};
use crate::dock::items::{self, FuncKind};
use crate::dock::preview::{PreviewClick, WindowPreview};
use crate::dock::render::{render, RenderCtx};
use crate::dock::Dock;
use crate::guard::Heartbeat;
use crate::hotkey;
use crate::menu::{ContextMenu, MenuItem};
use crate::monitor::{self, MonitorState};
use crate::taskbar::{self, TaskbarController, WM_APP_TASKBAR_RESHOW};
use crate::topbar::color_sync::{self, Rgb};
use crate::topbar::panel::{NetworkData, PanelAction, PanelKind, StatusPanel};
use crate::topbar::status;
use crate::topbar::tray_mirror::{self, TrayIconCache, TrayInvocation, TrayItem, TrayRefreshRequest, TraySnapshot};
use crate::topbar::tray_popover::Popover;
use crate::topbar::tray_tooltip::{TipTarget, TrayTooltip, TIMER_TRAY_TIP};
use crate::topbar::{Hit, TopBar, TopbarDrag};
use crate::tray::Tray;
use crate::ui::DibTarget;
use crate::win32::{all_monitors, class_name, monitor_at_cursor, wide, wide_static, window_dpi, MonitorInfo};

pub const CLASS_MAIN: &str = "WinDockFinderMain";
pub const MUTEX_NAME: &str = "Local\\WinDockFinder-SingleInstance";
pub const WM_APP_IPC: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;
pub const WM_APP_COLOR: u32 = WM_APP + 32;
pub const WM_APP_UPDATE: u32 = WM_APP + 50;

pub const IPC_SETTINGS: usize = 1;
pub const IPC_TOGGLE_DOCK: usize = 2;
pub const IPC_TOGGLE_TOPBAR: usize = 3;
pub const IPC_RESTORE_TASKBAR: usize = 4;

const TIMER_FRAME: usize = 1;
const TIMER_SLOW: usize = 2;
const TIMER_EDGE: usize = 3;
const TIMER_TRAY_CONTEXT: usize = 4;
const TIMER_TRAY_FLASH: usize = 6;
const HK_DOCK: i32 = 7001;
const HK_TOPBAR: i32 = 7002;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 85 MB 软水位：超过即触发降载。
const SOFT_WATERMARK: u64 = 85_000_000;

// ---------- 菜单命令 ----------
const M_SETTINGS: usize = 1;
const M_TOGGLE_DOCK: usize = 2;
const M_TOGGLE_TOPBAR: usize = 3;
const M_TOGGLE_TASKBAR: usize = 4;
const M_EXIT: usize = 5;
const M_ABOUT: usize = 6;
const M_LOCK: usize = 7;
const M_SLEEP: usize = 8;
const M_RESTART: usize = 9;
const M_SHUTDOWN: usize = 10;
const M_TASKMGR: usize = 11;
const M_STARTUP: usize = 13;
const M_APP_OPEN: usize = 100;
const M_APP_NEW: usize = 101;
const M_APP_ADMIN: usize = 102;
const M_APP_LOCATION: usize = 103;
const M_APP_PIN: usize = 104;
const M_APP_UNPIN: usize = 105;
const M_APP_QUIT: usize = 106;
const M_TRASH_OPEN: usize = 111;
const M_TRASH_EMPTY: usize = 112;
const M_WINDOW_BASE: usize = 200;
const M_WINDOW_LAST: usize = M_WINDOW_BASE + 63;
const M_FG_MIN: usize = 300;
const M_FG_MAX: usize = 301;
const M_FG_CENTER: usize = 302;
const M_FG_CLOSE: usize = 303;
const M_FG_LEFT_HALF: usize = 304;
const M_FG_RIGHT_HALF: usize = 305;
const M_SHOW_DESKTOP: usize = 306;
const M_NEW_FINDER: usize = 307;
const M_OPEN_TERMINAL: usize = 308;
const M_OPEN_BASE: usize = 310;
const M_OPEN_LAST: usize = M_OPEN_BASE + 5;

static APP_PTR: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
thread_local! {
    /// with_app 重入深度：模态菜单/DestroyWindow 期间同步派发的嵌套消息直接忽略，避免 `&mut` 别名。
    static HANDLER_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub fn app_ptr() -> *mut App {
    APP_PTR.load(Ordering::SeqCst) as *mut App
}

/// 当前是否正处于 App 事件处理（嵌套消息应被忽略）。
pub fn handler_busy() -> bool {
    HANDLER_DEPTH.with(|d| d.get()) > 0
}

/// 通过窗口 GWLP_USERDATA 取回 App 并执行回调。
pub fn with_app(hwnd: HWND, f: impl FnOnce(&mut App)) {
    if HANDLER_DEPTH.with(|d| d.get()) > 0 {
        return;
    }
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if ptr.is_null() {
        return;
    }
    hold_app(|| f(unsafe { &mut *ptr }));
}

/// 在已持有 `&mut App` 的自定义窗口过程中执行回调，期间忽略经 with_app 派发的嵌套消息。
pub fn hold_app<R>(f: impl FnOnce() -> R) -> R {
    HANDLER_DEPTH.with(|d| d.set(1));
    let r = f();
    HANDLER_DEPTH.with(|d| d.set(0));
    r
}

pub fn tray_icon_cache() -> Arc<TrayIconCache> {
    let ptr = app_ptr();
    if ptr.is_null() {
        return Arc::new(TrayIconCache::default());
    }
    unsafe { (*ptr).tray_icons.clone() }
}

pub fn app_icon_cache() -> Arc<IconCache> {
    let ptr = app_ptr();
    if ptr.is_null() {
        return Arc::new(IconCache::default());
    }
    unsafe { (*ptr).icons.clone() }
}

// ---------- 单实例 ----------

pub enum Instance {
    Primary(HANDLE),
    Secondary(HWND),
}

pub fn find_main_window() -> Option<HWND> {
    unsafe { FindWindowW(wide_static(CLASS_MAIN), None).ok().filter(|h| !h.0.is_null()) }
}

pub fn acquire_instance() -> Result<Instance, String> {
    let name = wide(MUTEX_NAME);
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())).map_err(crate::win32::error_string)? };
    if unsafe { windows::Win32::Foundation::GetLastError() } == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(Instance::Secondary(find_main_window().unwrap_or_default()))
    } else {
        Ok(Instance::Primary(handle))
    }
}

/// 把命令转发给已有主实例；返回是否送达。
pub fn forward(hwnd: HWND, cmd: usize) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut result: usize = 0;
    let r = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_APP_IPC,
            WPARAM(cmd),
            LPARAM(1),
            SMTO_ABORTIFHUNG | SMTO_NOTIMEOUTIFNOTHUNG,
            3000,
            Some(&mut result),
        )
    };
    r.0 != 0 && result == 1
}

// ---------- 菜单上下文 ----------

fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn fill_buf(buf: &mut [u16], s: &str) {
    let wide_s: Vec<u16> = s.encode_utf16().take(buf.len() - 1).collect();
    buf[..wide_s.len()].copy_from_slice(&wide_s);
    buf[wide_s.len()] = 0;
}

fn is_desktop_class(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Shell_IconView")
        || class.starts_with("NotifyIconOverflowWindow")
        || class.starts_with("TopLevelWindowForOverflowXamlIsland")
        || class.starts_with("WinDockFinder")
}

#[derive(Clone, Default)]
pub enum MenuCtx {
    #[default]
    None,
    DockEntry(usize),
    Foreground(isize),
    ShelfKey(String),
}

/// entries 结构是否变化（决定是否 relayout）。
fn entries_same_structure(a: &crate::dock::items::EntryList, b: &crate::dock::items::EntryList) -> bool {
    a.apps.len() == b.apps.len()
        && a.right.len() == b.right.len()
        && a.apps.iter().zip(b.apps.iter()).all(|(x, y)| {
            x.id == y.id && x.icon_key == y.icon_key && x.windows.is_empty() == y.windows.is_empty()
        })
        && a.right.iter().zip(b.right.iter()).all(|(x, y)| x.kind == y.kind)
}

fn entries_equal(a: &crate::dock::items::EntryList, b: &crate::dock::items::EntryList) -> bool {
    entries_same_structure(a, b)
        && a.apps.iter().zip(b.apps.iter()).all(|(x, y)| x.label == y.label)
}

struct TrayClick {
    item: TrayItem,
    anchor: Option<RECT>,
    popover: Option<HWND>,
}

enum TrayWork {
    // 图标采样中的用户点击排在其后，避免两个后台线程同时操作同一个原生托盘。
    Refresh { canceled: Arc<AtomicBool>, reading_overflow: bool, queued_click: Option<TrayClick> },
    Click { right: bool, screen: POINT, popover: Option<HWND>, queued_click: Option<TrayClick> },
}

struct TrayContext {
    popover: HWND,
    menu: Option<HWND>,
}

enum TraySeed {
    Loading(Option<HWND>),
    Ready,
}

/// 顶部栏与收纳浮层都是无边框窗口，使用实际图标矩形确定屏幕锚点。
fn tray_anchor(hwnd: HWND, icon: RECT) -> Option<RECT> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut bounds).ok()? };
    Some(RECT {
        left: bounds.left + icon.left,
        top: bounds.top + icon.top,
        right: bounds.left + icon.right,
        bottom: bounds.top + icon.bottom,
    })
}

/// 用户正在操作原生菜单时，延后定期采样，避免 Explorer 重新处理托盘显隐。
fn foreground_has_menu() -> bool {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let found = unsafe { GetGUIThreadInfo(0, &mut info).is_ok() };
    found && (info.flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE)).0 != 0
}

pub struct App {
    pub hwnd: HWND,
    pub mutex: HANDLE,
    pub store: Store,
    pub config: Config,
    pub dock_items: DockItems,
    pub icons: Arc<IconCache>,
    pub icon_worker: Arc<IconWorker>,
    pub tray_icons: Arc<TrayIconCache>,
    pub text_scratch: DibTarget,
    pub tray: Tray,
    pub taskbar: TaskbarController,
    pub heartbeat: Option<Heartbeat>,
    pub guard_handle: Option<HANDLE>,
    pub docks: Vec<Dock>,
    pub topbars: Vec<TopBar>,
    pub settings: Option<crate::settings::Settings>,
    pub context_menu: ContextMenu,
    pub status_panel: StatusPanel,
    pub window_preview: WindowPreview,
    pub preview_leave_at: Option<Instant>,
    pub snap: MonitorState,
    pub dock_visible: bool,
    pub topbar_visible: bool,
    pub dock_revealed: bool,
    pub mouse_in_dock: bool,
    pub edge_since: Option<Instant>,
    pub hide_at: Option<Instant>,
    pub trash_full: bool,
    pub last_frame: Instant,
    pub frame_on: bool,
    pub edge_timer_on: bool,
    pub slow_ticks: u64,
    pub last_clock: String,
    pub last_ime: String,
    pub tray_raw_items: Vec<TrayItem>,
    tray_snapshot_generation: u64,
    pub last_tray_refresh: Option<Instant>,
    last_tray_details: Option<Instant>,
    tray_details_pending: bool,
    tray_work: Option<TrayWork>,
    tray_seed: TraySeed,
    tray_context: Option<TrayContext>,
    tray_tip: TrayTooltip,
    pub temp_show_until: Option<Instant>,
    pub display_dirty_at: Option<Instant>,
    pub cursor_monitor: isize,
    pub fused_hwnd: isize,
    pub color_token: u64,
    pub color_busy: bool,
    pub last_beat: Instant,
    pub menu_ctx: MenuCtx,
    pub update_checked: bool,
    pub exiting: bool,
}

// ---------- 启动 ----------

pub fn run(open_settings: bool) -> i32 {
    let data = crate::win32::data_dir();
    crate::log::init(&data, "main");
    unsafe {
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }
    let hinst = crate::win32::hinstance();
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            lpszClassName: wide_static(CLASS_MAIN),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        let msg_id = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.store(msg_id, Ordering::SeqCst);
    }
    let mutex = match acquire_instance() {
        Ok(Instance::Primary(h)) => Some(h),
        Ok(Instance::Secondary(other)) => {
            let _ = forward(other, IPC_SETTINGS);
            return 0;
        }
        Err(e) => {
            crate::log::log(&format!("单实例锁失败: {e}"));
            None
        }
    };
    let hwnd = match unsafe {
        CreateWindowExW(WS_EX_TOOLWINDOW, wide_static(CLASS_MAIN), w!("WinDockFinder"), WS_POPUP, 0, 0, 0, 0, None, None, hinst, None)
    } {
        Ok(h) => h,
        Err(e) => {
            crate::log::log(&format!("主窗口创建失败: {}", crate::win32::error_string(e)));
            return 1;
        }
    };

    let store = Store::open();
    let mut config = store.load_config();
    // 首次或原路径失效时自动搜索已安装的 WinLaunchpad
    let wlp_invalid = !config.general.winlaunchpad_path.is_empty()
        && !std::path::Path::new(&config.general.winlaunchpad_path).is_file();
    if (config.general.winlaunchpad_path.is_empty() && !config.general.winlaunchpad_auto_searched) || wlp_invalid {
        if let Some(found) = items::detect_winlaunchpad() {
            config.general.winlaunchpad_path = found;
            config.general.winlaunchpad_auto_searched = true;
            let _ = store.save_config(&config);
        } else if wlp_invalid {
            config.general.winlaunchpad_path.clear();
            config.general.winlaunchpad_auto_searched = true;
            let _ = store.save_config(&config);
        } else {
            config.general.winlaunchpad_auto_searched = true;
            let _ = store.save_config(&config);
        }
    }
    // 首次升级或首次初始化时默认开启开机自启，之后遵循用户手动开关状态
    if !config.general.startup_initialized {
        if crate::startup::state() == crate::startup::StartupState::Off {
            let _ = crate::startup::set_enabled(true);
        }
        config.general.startup_initialized = true;
        let _ = store.save_config(&config);
    }
    let mut dock_items = store.load_dock_items();
    if !store.dock_items_path().exists() {
        dock_items = items::seed_items();
        let _ = store.save_dock_items(&dock_items);
    }
    let mut taskbar = TaskbarController::new(Store::open());
    taskbar.recover_previous(std::process::id());
    monitor::refresh();
    let icon_worker = IconWorker::start(hwnd, store.icons_dir());
    let app = Box::new(App {
        hwnd,
        mutex: mutex.unwrap_or_default(),
        store,
        config,
        dock_items,
        icons: Arc::new(IconCache::default()),
        icon_worker,
        tray_icons: Arc::new(TrayIconCache::default()),
        text_scratch: DibTarget::default(),
        tray: Tray::new(hwnd, hinst),
        taskbar,
        heartbeat: None,
        guard_handle: None,
        docks: Vec::new(),
        topbars: Vec::new(),
        settings: None,
        context_menu: ContextMenu::default(),
        status_panel: StatusPanel::default(),
        window_preview: WindowPreview::default(),
        preview_leave_at: None,
        snap: monitor::snapshot(),
        dock_visible: true,
        topbar_visible: true,
        dock_revealed: false,
        mouse_in_dock: false,
        edge_since: None,
        hide_at: None,
        trash_full: false,
        last_frame: Instant::now(),
        frame_on: false,
        edge_timer_on: false,
        slow_ticks: 0,
        last_clock: String::new(),
        last_ime: String::new(),
        tray_raw_items: Vec::new(),
        tray_snapshot_generation: 0,
        last_tray_refresh: None,
        last_tray_details: None,
        tray_details_pending: false,
        tray_work: None,
        tray_seed: TraySeed::Ready,
        tray_context: None,
        tray_tip: TrayTooltip::default(),
        temp_show_until: None,
        display_dirty_at: None,
        cursor_monitor: 0,
        fused_hwnd: 0,
        color_token: 0,
        color_busy: false,
        last_beat: Instant::now(),
        menu_ctx: MenuCtx::None,
        update_checked: false,
        exiting: false,
    });
    let ptr = Box::into_raw(app);
    APP_PTR.store(ptr as isize, Ordering::SeqCst);
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        (*ptr).init();
    }
    if open_settings {
        with_app(hwnd, |app| app.open_settings(None));
    }

    unsafe {
        let mut msg = MSG::default();
        loop {
            // BOOL > 0：取到消息；0：WM_QUIT；-1：出错
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
            (*ptr).beat();
        }
    }
    // 无论通过 WM_CLOSE 还是菜单退出，此处 hwnd 可能已销毁，直接通过 ptr 调用幂等 shutdown
    unsafe {
        (*ptr).shutdown();
    }
    APP_PTR.store(0, Ordering::SeqCst);
    if let Some(mutex) = mutex {
        unsafe {
            let _ = CloseHandle(mutex);
        }
    }
    unsafe {
        drop(Box::from_raw(ptr));
    }
    0
}

impl App {
    fn init(&mut self) {
        self.heartbeat = Heartbeat::create(std::process::id());
        // 与窗口监控、顶部栏和守护进程初始化并行准备首屏图标。
        if self.config.topbar.enabled && tray_mirror::spawn_seed(self.hwnd, self.tray_icons.clone()) {
            self.tray_seed = TraySeed::Loading(None);
        }
        monitor::install(self.hwnd);
        taskbar::install_hook(self.hwnd);
        self.snap = monitor::snapshot();
        self.register_hotkeys();
        self.rebuild_bars();
        // 首次启动立刻刷新顶部栏时钟/电量/音量/网络/输入法状态
        self.refresh_status(true);
        self.trash_full = items::recycle_bin_full();
        self.tray.add();
        self.spawn_guard();
        // 安全前置门禁：双栏已创建、守护进程握手后才隐藏任务栏
        self.apply_taskbar_hide();
        self.sync_shelf_items();
        self.refresh_tray();
        self.sync_edge_timer();
        unsafe {
            SetTimer(self.hwnd, TIMER_SLOW, 1000, None);
        }
    }

    fn beat(&mut self) {
        if self.last_beat.elapsed() >= Duration::from_millis(1000) {
            self.last_beat = Instant::now();
            if let Some(h) = &self.heartbeat {
                h.signal();
            }
        }
    }

    fn spawn_guard(&mut self) {
        let mut cmd = std::process::Command::new(crate::win32::exe_path());
        cmd.arg("--guard")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .arg("--hotkey")
            .arg(&self.config.general.emergency_restore_hotkey)
            .creation_flags(CREATE_NO_WINDOW);
        match cmd.spawn() {
            Ok(child) => {
                self.guard_handle = Some(HANDLE(child.into_raw_handle()));
                // 等待守护进程握手（窗口出现），最多 3 秒
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(3) {
                    if unsafe { FindWindowW(w!("WinDockFinderGuard"), None) }.is_ok() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
                crate::log::log("守护进程握手超时");
            }
            Err(e) => crate::log::log(&format!("守护进程启动失败: {e}")),
        }
    }

    fn register_hotkeys(&mut self) {
        hotkey::unregister(self.hwnd, HK_DOCK);
        hotkey::unregister(self.hwnd, HK_TOPBAR);
        let g = self.config.general.clone();
        for (id, text) in [(HK_DOCK, &g.toggle_dock_hotkey), (HK_TOPBAR, &g.toggle_topbar_hotkey)] {
            let Some(spec) = hotkey::parse(text) else { continue };
            if hotkey::register(self.hwnd, id, spec) {
                continue;
            }
            // 冲突回退：叠加全部修饰键再试一次
            let all = hotkey::HotkeySpec {
                mods: spec.mods
                    | MOD_CONTROL.0 as u32
                    | MOD_ALT.0 as u32
                    | MOD_SHIFT.0 as u32,
                vk: spec.vk,
            };
            if !hotkey::register(self.hwnd, id, all) {
                crate::log::log(&format!("全局快捷键被占用，已停用: {text}"));
            }
        }
    }

    /// 隐藏任务栏：仅在存在可见桌面入口且守护进程可用时执行。
    fn apply_taskbar_hide(&mut self) {
        if self.exiting || self.taskbar.paused || !self.config.general.hide_taskbar_on_start {
            return;
        }
        let has_entry = (self.topbar_visible && self.config.topbar.enabled && !self.topbars.is_empty())
            || (self.dock_visible && self.config.dock.enabled && !self.docks.is_empty());
        if !has_entry {
            crate::log::log("无可见桌面入口，跳过隐藏任务栏");
            return;
        }
        if self.guard_handle.is_none() {
            crate::log::log("守护进程不可用，跳过隐藏任务栏");
            return;
        }
        self.taskbar.hide_all(std::process::id(), self.config.general.taskbar_autohide);
    }

    fn shutdown(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_FRAME);
            let _ = KillTimer(self.hwnd, TIMER_SLOW);
            let _ = KillTimer(self.hwnd, TIMER_EDGE);
        }
        self.frame_on = false;
        self.edge_timer_on = false;
        self.context_menu.destroy();
        self.status_panel.destroy();
        self.window_preview.destroy();
        if let Some(s) = self.settings.as_mut() {
            s.destroy();
        }
        self.settings = None;
        for bar in &mut self.topbars {
            if let Some(p) = bar.popover.as_mut() {
                Self::silence(p.hwnd);
            }
            Self::silence(bar.hwnd);
            bar.destroy();
        }
        self.topbars.clear();
        for dock in &mut self.docks {
            Self::silence(dock.hwnd_overlay);
            Self::silence(dock.hwnd_trigger);
            dock.destroy();
        }
        self.docks.clear();
        self.tray.remove();
        // 撤销 AppBar → 恢复任务栏 → 清除恢复记录
        self.taskbar.restore_all();
        taskbar::uninstall_hook();
        monitor::uninstall();
        hotkey::unregister(self.hwnd, HK_DOCK);
        hotkey::unregister(self.hwnd, HK_TOPBAR);
        if let Some(h) = self.guard_handle.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        let _ = self.store.save_config(&self.config);
        let _ = self.store.save_dock_items(&self.dock_items);
    }

    fn silence(hwnd: HWND) {
        unsafe {
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }

    pub fn quit(&mut self) {
        // 先执行幂等清理（撤销 AppBar、恢复任务栏、保存配置），再销毁主窗口触发 PostQuitMessage
        self.shutdown();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    // ---------- 双栏创建/重建 ----------

    /// 顶部栏显示器集合。
    fn topbar_monitors(&self) -> Vec<MonitorInfo> {
        let all = all_monitors();
        let Some(first) = all.first().copied() else { return Vec::new() };
        let primary = all.iter().find(|m| m.primary).copied().unwrap_or(first);
        if self.config.topbar.display_mode == "all" { all } else { vec![primary] }
    }

    fn dock_monitors(&self) -> Vec<MonitorInfo> {
        let all = all_monitors();
        let Some(first) = all.first().copied() else { return Vec::new() };
        match self.config.dock.display_mode.as_str() {
            "all" => all,
            "cursor" => vec![monitor_at_cursor().unwrap_or(
                all.iter().find(|m| m.primary).copied().unwrap_or(first),
            )],
            _ => vec![all.iter().find(|m| m.primary).copied().unwrap_or(first)],
        }
    }

    fn rebuild_bars(&mut self) {
        self.close_popover();
        self.tray_tip.clear(self.hwnd);
        self.context_menu.close();
        self.status_panel.close();
        self.window_preview.close();
        for bar in &mut self.topbars {
            if let Some(p) = bar.popover.as_mut() {
                Self::silence(p.hwnd);
            }
            Self::silence(bar.hwnd);
            bar.destroy();
        }
        self.topbars.clear();
        for dock in &mut self.docks {
            Self::silence(dock.hwnd_overlay);
            Self::silence(dock.hwnd_trigger);
            dock.destroy();
        }
        self.docks.clear();

        if self.config.topbar.enabled {
            let mons = self.topbar_monitors();
            let label = self.active_app_label();
            let pure_dark_text = !self.config.appearance.is_dark();
            for m in mons {
                if let Ok(mut bar) = TopBar::new(app_ptr(), m, &self.config.topbar, &self.config.appearance) {
                    bar.pinned = self.config.topbar.pinned_tray_keys.clone();
                    bar.active_app = label.clone();
                    bar.dark_text = pure_dark_text;
                    bar.wanted_visible = self.topbar_visible;
                    bar.layout(&self.config.topbar);
                    if self.topbar_visible {
                        bar.show();
                    }
                    self.topbars.push(bar);
                }
            }
        }

        if self.config.dock.enabled {
            let mons = self.dock_monitors();
            self.cursor_monitor = mons.first().map(|m| m.hmonitor.0 as isize).unwrap_or(0);
            for m in mons {
                if let Ok(mut dock) = Dock::new(app_ptr(), m, &self.config.dock, &self.config.appearance) {
                    dock.register_drop();
                    self.docks.push(dock);
                }
            }
            self.refresh_entries();
        }
        self.sync_shelf_items();
        self.on_state_changed();
        self.sync_edge_timer();
        self.ensure_frame();
    }

    // ---------- Dock 条目 ----------

    pub fn refresh_entries(&mut self) {
        if self.docks.is_empty() {
            return;
        }
        let cfg = &self.config.dock;
        let apps = monitor::running_apps();
        let mut list = items::build_entries(&self.dock_items, &apps, cfg.show_running_apps);
        if !cfg.show_folders {
            list.right.retain(|f| !matches!(f.kind, FuncKind::Folder(_)));
        }
        // 已设置 WinLaunchpad 且未自定义固定项时，启动台居最左，文件管理器紧随其后。
        if !self.config.general.winlaunchpad_path.is_empty()
            && !self.dock_items.items.iter().any(|i| i.kind == "winlaunchpad")
        {
            list.apps.insert(
                0,
                items::Entry {
                    id: "special:launchpad".into(),
                    pinned: None,
                    running_exe: None,
                    label: "启动台".into(),
                    icon_key: items::launchpad_icon_key().into(),
                    windows: Vec::new(),
                },
            );
            if let Some(exp_pos) = list.apps.iter().skip(1).position(|e| {
                e.running_exe
                    .as_deref()
                    .is_some_and(|p| p.ends_with("\\explorer.exe"))
                    || e.icon_key.to_lowercase().contains("explorer.exe")
                    || e.label == crate::win32::FILE_MANAGER_NAME
            }) {
                let real_pos = exp_pos + 1;
                if real_pos != 1 {
                    let exp = list.apps.remove(real_pos);
                    list.apps.insert(1, exp);
                }
            }
        }
        if cfg.show_trash {
            list.right.push(items::FuncEntry {
                kind: FuncKind::Trash,
                label: "回收站".into(),
                icon_key: items::trash_icon_key().into(),
            });
        }
        list.right.push(items::FuncEntry {
            kind: FuncKind::Settings,
            label: "设置".into(),
            icon_key: items::settings_icon_key().into(),
        });
        let appearance = &self.config.appearance;
        let mut changed = false;
        for dock in &mut self.docks {
            if entries_equal(&dock.entries, &list) {
                continue;
            }
            let structural = !entries_same_structure(&dock.entries, &list);
            dock.set_entries(list.clone());
            if structural {
                dock.relayout(cfg, appearance);
            }
            changed = true;
        }
        if changed {
            self.ensure_frame();
        }
    }

    fn pinned_item(&self, sorted_index: usize) -> Option<DockItem> {
        self.dock_items.sorted().get(sorted_index).map(|i| (*i).clone())
    }

    // ---------- 状态变化 ----------

    fn active_app_label(&self) -> String {
        if !self.snap.foreground_name.is_empty() {
            return self.snap.foreground_name.clone();
        }
        let fg = HWND(self.snap.foreground as *mut _);
        if fg.0.is_null() || self.snap.foreground_exe.is_empty() || is_desktop_class(&class_name(fg)) {
            return "桌面".into();
        }
        monitor::app_display_name(&self.snap.foreground_exe, Some(&self.snap.foreground_title))
    }

    fn on_state_changed(&mut self) {
        let label = self.active_app_label();
        let fullscreen = self.snap.fullscreen;
        let cfg = &self.config.topbar;
        let pure_dark_text = !self.config.appearance.is_dark();
        for bar in &mut self.topbars {
            bar.active_app = label.clone();
            bar.status.ime = if cfg.has_status("ime") {
                status::ime_label(HWND(self.snap.foreground as *mut _))
            } else {
                String::new()
            };
            bar.status.network = status::network_connected();
            bar.set_fullscreen_hidden(fullscreen);
            bar.layout(&self.config.topbar);
            invalidate(bar.hwnd);
        }
        if let Some(first) = self.topbars.first() {
            self.last_ime = first.status.ime.clone();
        }
        self.sync_edge_timer();
        // 最大化窗口自动取色融合
        let cfg = &self.config.topbar;
        let fg = self.snap.foreground;
        let want_fused = cfg.adaptive_maximized_color
            && self.snap.maximized_monitor != 0
            && !fullscreen
            && !color_sync::is_excluded(&self.snap.foreground_exe, &cfg.adaptive_color_exclude);
        if want_fused {
            if self.fused_hwnd != fg {
                self.fused_hwnd = fg;
                self.spawn_color_sample(true);
            }
        } else {
            let had_fused = self.fused_hwnd != 0 || self.topbars.iter().any(|b| b.mode == crate::topbar::ColorMode::Fused);
            self.fused_hwnd = 0;
            self.color_token += 1; // 使在途采样失效
            for bar in &mut self.topbars {
                bar.set_fused_target(None, false);
                bar.dark_text = pure_dark_text;
                invalidate(bar.hwnd);
            }
            if had_fused {
                self.ensure_frame();
            }
        }
        if !self.docks.is_empty() {
            // 仅当存在自动隐藏/智能避让 Dock 状态需要推进时启动帧调度
            for i in 0..self.docks.len() {
                let (want, _) = self.dock_state(i);
                if self.docks[i].want_visible != want {
                    self.ensure_frame();
                    break;
                }
            }
        }
    }

    fn spawn_color_sample(&mut self, initial_delay: bool) {
        if self.color_busy || self.snap.foreground == 0 {
            return;
        }
        self.color_busy = true;
        self.color_token += 1;
        let token = self.color_token;
        let target = self.snap.foreground as isize;
        let main = self.hwnd.0 as isize;
        let hmon = self.snap.maximized_monitor;
        let (mon_rect, bar_h) = self
            .topbars
            .iter()
            .find(|b| b.monitor.hmonitor.0 as isize == hmon)
            .or_else(|| self.topbars.first())
            .map(|b| (Some(b.monitor.rect), b.height))
            .unwrap_or((None, 30));
        std::thread::Builder::new()
            .name("caption-color".into())
            .spawn(move || {
                if initial_delay {
                    std::thread::sleep(Duration::from_millis(60));
                }
                let rgb = color_sync::caption_color(HWND(target as *mut _), mon_rect, bar_h);
                let payload = Box::new((token, rgb));
                unsafe {
                    let _ = PostMessageW(
                        HWND(main as *mut _),
                        WM_APP_COLOR,
                        WPARAM(0),
                        LPARAM(Box::into_raw(payload) as isize),
                    );
                }
            })
            .ok();
    }

    fn on_color(&mut self, lparam: isize) {
        self.color_busy = false;
        let (token, rgb) = *unsafe { Box::from_raw(lparam as *mut (u64, Rgb)) };
        if token != self.color_token || self.fused_hwnd == 0 {
            if self.fused_hwnd != 0 {
                self.spawn_color_sample(false);
            }
            return;
        }
        let hmon = self.snap.maximized_monitor;
        let first = self.topbars.iter().all(|b| b.mode != crate::topbar::ColorMode::Fused);
        for bar in &mut self.topbars {
            if bar.monitor.hmonitor.0 as isize == hmon {
                bar.set_fused_target(Some(rgb), false);
            } else {
                bar.set_fused_target(None, false);
            }
            invalidate(bar.hwnd);
        }
        let _ = first;
        self.ensure_frame();
    }

    fn refresh_status(&mut self, full: bool) {
        let cfg = &self.config.topbar;
        let fg = HWND(self.snap.foreground as *mut _);
        {
            let mut new_clock = String::new();
            for bar in &mut self.topbars {
                let mut st = status::snapshot(
                    &cfg.clock_format,
                    fg,
                    cfg.has_status("ime"),
                    cfg.has_status("network"),
                    full && cfg.has_status("volume"),
                    cfg.has_status("battery"),
                );
                if !full {
                    st.volume = bar.status.volume;
                    st.battery = bar.status.battery;
                }
                new_clock = st.clock.clone();
                bar.status = st;
                bar.layout(&self.config.topbar);
                invalidate(bar.hwnd);
            }
            self.last_clock = new_clock;
        }
    }

    // ---------- 任务栏 ----------

    fn toggle_taskbar(&mut self) {
        if self.taskbar.active() {
            let show = !self.taskbar.temporary_show();
            self.taskbar.set_temporary_show(show);
            self.temp_show_until = None;
            if show {
                self.refresh_tray();
            }
        } else {
            self.taskbar.paused = false;
            self.apply_taskbar_hide();
        }
    }

    pub fn toggle_startup(&mut self) {
        match crate::startup::state() {
            crate::startup::StartupState::Enabled | crate::startup::StartupState::DisabledBySystem => {
                if let Err(e) = crate::startup::set_enabled(false) {
                    crate::log::log(&format!("关闭开机启动失败: {e}"));
                }
            }
            crate::startup::StartupState::Off => {
                if let Err(e) = crate::startup::set_enabled(true) {
                    crate::log::log(&format!("开启开机启动失败: {e}"));
                }
            }
        }
    }

    // ---------- 托盘镜像 ----------

    pub fn refresh_tray(&mut self) {
        if self.topbars.is_empty() || self.tray_work.is_some() || self.snap.fullscreen
            || self.tray_context.is_some() || matches!(self.tray_seed, TraySeed::Loading(_))
        {
            return;
        }
        let peek = self.taskbar.begin_tray_peek();
        let canceled = Arc::new(AtomicBool::new(false));
        let reading_overflow = self.tray_details_pending;
        let pinned = self.topbars.iter().flat_map(|bar| bar.tray_items.iter().filter_map(|item| {
            bar.pinned.contains(&item.key).then(|| item.uia_key.clone()).flatten()
        })).collect();
        let request = TrayRefreshRequest { peek, canceled: canceled.clone(), overflow: reading_overflow, pinned };
        if tray_mirror::spawn_refresh(self.hwnd, self.tray_icons.clone(), request) {
            self.last_tray_refresh = Some(Instant::now());
            if reading_overflow {
                self.tray_details_pending = false;
                self.last_tray_details = self.last_tray_refresh;
            }
            self.tray_work = Some(TrayWork::Refresh { canceled, reading_overflow, queued_click: None });
        } else {
            self.taskbar.end_tray_peek();
        }
    }

    /// 后台采样退还主任务栏后，立即处理这段时间收到的图标点击。
    fn on_tray_refresh(&mut self, snapshot: TraySnapshot) {
        let Some(TrayWork::Refresh { queued_click, .. }) = self.tray_work.take() else { return };
        self.taskbar.end_tray_peek();
        self.apply_tray_snapshot(snapshot);
        let (active, _) = self.tray_icons.advance_flash_frames();
        if active {
            unsafe { let _ = SetTimer(self.hwnd, TIMER_TRAY_FLASH, 85, None); }
            self.repaint_tray_frames();
        }
        if let Some(click) = queued_click {
            self.invoke_tray(click);
        } else if self.tray_details_pending {
            self.refresh_tray();
        }
    }

    fn repaint_tray_frames(&mut self) {
        for bar in &self.topbars {
            if !bar.pinned.is_empty() {
                invalidate(bar.hwnd);
            }
        }
    }

    /// 首次点开收纳时，等一次轻量注册快照，让首屏同时包含窗口和托盘条目。
    fn on_tray_seed(&mut self, snapshot: TraySnapshot) {
        self.apply_tray_snapshot(snapshot);
        if let TraySeed::Loading(Some(hwnd)) = std::mem::replace(&mut self.tray_seed, TraySeed::Ready) {
            if let Some(index) = self.topbar_index(hwnd) {
                self.open_popover(index);
            }
        }
        self.refresh_tray();
    }

    /// 同步可见托盘的扫描结果，保留当前未展开区域的缓存。
    fn apply_tray_snapshot(&mut self, snapshot: TraySnapshot) {
        if snapshot.apply(&mut self.tray_raw_items, &mut self.tray_snapshot_generation) {
            self.sync_shelf_items();
        }
    }

    fn redraw_shelf_views(&mut self) {
        for bar in &mut self.topbars {
            invalidate(bar.hwnd);
            if let Some(p) = bar.popover.as_mut() {
                if p.visible {
                    p.render();
                }
            }
        }
        if let Some(s) = &self.settings {
            s.invalidate();
        }
    }

    /// 同步顶部栏与收纳浮层中的应用/托盘图标列表。
    pub fn sync_shelf_items(&mut self) {
        let apps = monitor::running_apps();
        let items = tray_mirror::build_shelf_items(&apps, &self.tray_raw_items);
        for exe in items.iter().filter_map(|item| item.app_exe.as_deref()).filter(|exe| !exe.is_empty()) {
            self.icon_worker.request(exe, DOCK_ICON_SRC_SIZE);
        }
        // 旧版保存在提示文字下的固定项，在其注册身份唯一时迁移到稳定键。
        let mut pins = Vec::new();
        for saved in &self.config.topbar.pinned_tray_keys {
            let mut matches = items.iter().filter(|item| item.legacy_keys.contains(saved));
            let key = if items.iter().any(|item| &item.key == saved) {
                saved
            } else {
                matches.next().filter(|_| matches.next().is_none()).map_or(saved, |item| &item.key)
            };
            if !pins.contains(key) {
                pins.push(key.clone());
            }
        }
        if pins != self.config.topbar.pinned_tray_keys {
            self.config.topbar.pinned_tray_keys = pins.clone();
            let _ = self.store.save_config(&self.config);
        }
        for bar in &mut self.topbars {
            bar.tray_items = items.clone();
            bar.pinned.clone_from(&pins);
            if let Some(p) = bar.popover.as_mut() {
                if p.visible {
                    p.pinned.clone_from(&pins);
                    p.update_items(items.clone());
                }
            }
            bar.layout(&self.config.topbar);
            invalidate(bar.hwnd);
        }
        if let Some(s) = &self.settings {
            s.invalidate();
        }
        self.refresh_tray_tip();
    }

    // ---------- Dock 显隐状态机 ----------

    fn dock_state(&self, i: usize) -> (bool, bool) {
        let Some(dock) = self.docks.get(i) else { return (false, false) };
        let cfg = &self.config.dock;
        let base = self.dock_visible && cfg.enabled && !self.snap.fullscreen;
        if !base {
            return (false, false);
        }
        let hidden_by_mode = match cfg.hide_mode.as_str() {
            "always" => false,
            "auto" => true,
            _ => self.snap.maximized_monitor == dock.monitor.hmonitor.0 as isize,
        };
        if !hidden_by_mode {
            return (true, false);
        }
        (self.dock_revealed, true)
    }

    /// 推进边缘触发/离开延迟；返回是否仍在等待。
    fn eval_reveal(&mut self) -> bool {
        let cfg = &self.config.dock;
        let mut waiting = false;
        if let Some(t) = self.edge_since {
            if t.elapsed().as_millis() as u64 >= cfg.show_delay_ms as u64 {
                if !self.dock_revealed {
                    self.dock_revealed = true;
                }
            } else {
                waiting = true;
            }
        }
        if self.dock_revealed && !self.mouse_in_dock && self.edge_since.is_none() {
            let due = match self.hide_at {
                Some(t) => t.elapsed().as_millis() as u64 >= cfg.hide_delay_ms as u64,
                None => true,
            };
            if due {
                self.dock_revealed = false;
                self.hide_at = None;
            } else {
                waiting = true;
            }
        }
        waiting
    }

    fn toggle_dock(&mut self) {
        self.dock_visible = !self.dock_visible;
        self.dock_revealed = false;
        self.edge_since = None;
        self.mouse_in_dock = false;
        self.window_preview.close();
        self.sync_edge_timer();
        self.ensure_frame();
    }

    fn toggle_topbar(&mut self) {
        self.topbar_visible = !self.topbar_visible;
        self.sync_topbar_visibility();
    }

    fn sync_topbar_visibility(&mut self) {
        for bar in &mut self.topbars {
            bar.wanted_visible = self.topbar_visible && self.config.topbar.enabled;
            bar.apply_wanted();
            invalidate(bar.hwnd);
        }
    }

    /// 仅当存在隐藏模式 Dock 时才启动 80ms 边缘轮询，否则关闭以保持零唤醒空闲。
    fn sync_edge_timer(&mut self) {
        let need = !self.exiting
            && self.dock_visible
            && self.config.dock.enabled
            && !self.docks.is_empty()
            && (self.config.dock.hide_mode != "always" || self.window_preview.visible);
        if need && !self.edge_timer_on {
            self.edge_timer_on = true;
            unsafe {
                let _ = SetTimer(self.hwnd, TIMER_EDGE, 80, None);
            }
        } else if !need && self.edge_timer_on {
            self.edge_timer_on = false;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_EDGE);
            }
        }
    }

    /// 80ms 轻量边缘/悬停检测：解决高速移动漏过 1px 触发窗、bottom_offset 缝隙早退及预览窗悬停桥接。
    fn on_edge_timer(&mut self) {
        if self.exiting {
            return;
        }
        let pt = cursor_pos();
        let edge_px = self.config.dock.trigger_edge_px.max(4);
        let mut on_edge = false;
        let mut in_dock_zone = self.context_menu.visible || self.status_panel.visible || self.window_preview.mouse_in;

        for (i, dock) in self.docks.iter().enumerate() {
            let mon = dock.monitor;
            let in_mon_x = pt.x >= mon.rect.left && pt.x < mon.rect.right;
            let (_, hidden_mode) = self.dock_state(i);
            if hidden_mode && in_mon_x && pt.y >= mon.rect.bottom - edge_px && pt.y <= mon.rect.bottom {
                on_edge = true;
            }
            if dock.window_visible {
                let cap = dock.capsule_rect();
                let sl = dock.win_rect.left + cap.left;
                let sr = dock.win_rect.left + cap.right;
                let st = dock.win_rect.top + cap.top - crate::win32::dp(12, dock.dpi);
                // 桥接到底部屏幕边缘，防止移过 bottom_offset 缝隙时 Dock 立刻缩回
                if pt.x >= sl && pt.x <= sr && pt.y >= st && pt.y <= mon.rect.bottom {
                    in_dock_zone = true;
                }
            }
        }

        if on_edge {
            if self.edge_since.is_none() {
                self.edge_since = Some(Instant::now());
            }
            self.hide_at = None;
            self.ensure_frame();
        } else if self.edge_since.is_some() {
            self.edge_since = None;
        }

        if in_dock_zone {
            self.mouse_in_dock = true;
            self.hide_at = None;
        } else if self.mouse_in_dock {
            self.mouse_in_dock = false;
            if self.dock_revealed {
                self.hide_at = Some(Instant::now());
                self.ensure_frame();
            }
        }

        self.eval_window_preview(pt);
    }

    fn eval_window_preview(&mut self, pt: POINT) {
        if !self.config.dock.show_window_preview || self.context_menu.visible || self.status_panel.visible {
            if self.window_preview.visible {
                self.window_preview.close();
            }
            return;
        }
        // 查找当前鼠标悬停且有打开窗口的 Dock 槽位
        let mut target_dock: Option<(usize, usize)> = None;
        for (di, dock) in self.docks.iter().enumerate() {
            if !dock.mouse_in || dock.drag.is_some() {
                continue;
            }
            if let Some(hi) = dock.hover {
                if let Some(entry) = dock.entries.apps.get(hi) {
                    if !entry.windows.is_empty() {
                        if dock.hover_since.is_some_and(|t| t.elapsed().as_millis() >= 220) {
                            target_dock = Some((di, hi));
                        }
                    }
                }
            }
        }

        if let Some((di, hi)) = target_dock {
            self.preview_leave_at = None;
            if !self.window_preview.visible || self.window_preview.entry_idx != Some(hi) {
                let (exe, windows, anchor_cx, anchor_top, mon) = {
                    let dock = &self.docks[di];
                    let entry = &dock.entries.apps[hi];
                    let cap = dock.capsule_rect();
                    let slot = dock.layout.slots[hi];
                    let cx = dock.win_rect.left + cap.left + slot.center_x.round() as i32;
                    let top = dock.win_rect.top + cap.top + (slot.center_y - slot.size * 0.5).round() as i32;
                    (entry.icon_key.clone(), entry.windows.clone(), cx, top, dock.monitor)
                };
                let ptr = app_ptr();
                let appearance = &self.config.appearance;
                self.window_preview.show_for(ptr, hi, &exe, &windows, anchor_cx, anchor_top, mon, appearance);
                self.docks[di].dirty = true;
                self.ensure_frame();
                self.sync_edge_timer();
            }
            return;
        }

        if self.window_preview.visible {
            let mut wr = RECT::default();
            let in_preview = unsafe { GetWindowRect(self.window_preview.hwnd, &mut wr).is_ok() }
                && pt.x >= wr.left
                && pt.x < wr.right
                && pt.y >= wr.top
                && pt.y < wr.bottom;
            if in_preview {
                self.preview_leave_at = None;
            } else {
                match self.preview_leave_at {
                    None => self.preview_leave_at = Some(Instant::now()),
                    Some(t) if t.elapsed().as_millis() >= 220 => {
                        self.preview_leave_at = None;
                        self.window_preview.close();
                        for d in &mut self.docks {
                            d.dirty = true;
                        }
                        self.ensure_frame();
                        self.sync_edge_timer();
                    }
                    _ => {}
                }
            }
        }
    }

    // ---------- 帧驱动 ----------

    fn ensure_frame(&mut self) {
        if self.frame_on || self.exiting {
            return;
        }
        self.frame_on = true;
        self.last_frame = Instant::now();
        unsafe {
            SetTimer(self.hwnd, TIMER_FRAME, 16, None);
        }
    }

    fn on_frame(&mut self) {
        if self.exiting {
            return;
        }
        let dt = (self.last_frame.elapsed().as_secs_f32() * 1000.0).clamp(0.0, 50.0);
        self.last_frame = Instant::now();
        let mut animating = self.eval_reveal();
        let n = self.docks.len();
        for i in 0..n {
            let (want, hidden) = self.dock_state(i);
            let cfg = &self.config.dock;
            let appearance = &self.config.appearance;
            let a = {
                let dock = &mut self.docks[i];
                dock.set_want_visible(want);
                dock.update_trigger(hidden);
                dock.tick(dt, cfg, appearance)
            };
            let need_draw = a || self.docks[i].dirty;
            animating |= a;
            if need_draw {
                self.render_dock(i);
            }
        }
        let transition = self.config.topbar.color_transition_ms;
        for bar in &mut self.topbars {
            animating |= bar.tick_color(dt, transition);
        }
        if !animating {
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_FRAME);
            }
            self.frame_on = false;
        }
    }

    fn render_dock(&mut self, i: usize) {
        let cfg = &self.config.dock;
        let appearance = &self.config.appearance;
        let icons = self.icons.clone();
        let worker = self.icon_worker.clone();
        let trash = self.trash_full;
        let suppress_tooltip = self.window_preview.visible;
        let scratch = &mut self.text_scratch;
        let font = self.docks[i].tooltip_font;
        let dock = &mut self.docks[i];
        let mut ctx = RenderCtx {
            cfg,
            appearance,
            icons: &icons,
            worker: &worker,
            text_scratch: scratch,
            tooltip_font: font,
            trash_full: trash,
            suppress_tooltip,
        };
        render(dock, &mut ctx);
    }

    fn on_slow(&mut self) {
        self.slow_ticks += 1;
        // 守护进程意外退出：先恢复任务栏，本会话暂停隐藏
        if let Some(h) = self.guard_handle {
            unsafe {
                if WaitForSingleObject(h, 0) == WAIT_OBJECT_0 {
                    crate::log::log("守护进程已退出，恢复任务栏并暂停隐藏");
                    let _ = CloseHandle(h);
                    self.guard_handle = None;
                    self.taskbar.restore_all();
                    self.taskbar.paused = true;
                }
            }
        }
        // 临时显示任务栏到期
        if let Some(t) = self.temp_show_until {
            if t <= Instant::now() {
                self.temp_show_until = None;
                self.taskbar.set_temporary_show(false);
            }
        }
        // 时钟按分钟刷新；每 1 秒轻量校对输入法，每 3 秒轻量校对音量/电量
        let clock = status::clock_string(&self.config.topbar.clock_format);
        let cur_ime = if self.config.topbar.has_status("ime") {
            status::ime_label(HWND(self.snap.foreground as *mut _))
        } else {
            String::new()
        };
        if clock != self.last_clock || self.slow_ticks % 3 == 0 {
            self.refresh_status(true);
            self.last_ime = cur_ime;
        } else if cur_ime != self.last_ime {
            self.last_ime = cur_ime.clone();
            for bar in &mut self.topbars {
                bar.status.ime = cur_ime.clone();
                bar.layout(&self.config.topbar);
                invalidate(bar.hwnd);
            }
        }
        if self.slow_ticks % 5 == 0 {
            let full = items::recycle_bin_full();
            if full != self.trash_full {
                self.trash_full = full;
                for d in &mut self.docks {
                    d.dirty = true;
                }
                self.ensure_frame();
            }
        }
        // 跟随鼠标屏
        if self.config.dock.display_mode == "cursor" {
            let cur = monitor_at_cursor().map(|m| m.hmonitor.0 as isize).unwrap_or(0);
            if cur != 0 && cur != self.cursor_monitor {
                self.rebuild_bars();
            }
        }
        // 最大化融合态每 10 秒低频复查一次标题栏颜色（低开销）
        if self.slow_ticks % 10 == 0
            && self.fused_hwnd != 0
            && self.config.topbar.adaptive_maximized_color
            && !self.snap.fullscreen
            && !self.color_busy
        {
            self.spawn_color_sample(false);
        }
        // 显示器/主题变更（防抖）
        if let Some(t) = self.display_dirty_at {
            if t.elapsed() >= Duration::from_millis(600) {
                self.display_dirty_at = None;
                self.snap = monitor::snapshot();
                self.rebuild_bars();
            }
        }
        // 托盘镜像与运行应用动态图标低频校对（仅顶部栏可见时触发，保持零高开销轮询）
        if !self.topbars.is_empty() && self.topbar_visible {
            let popover_open = self.topbars.iter().any(|b| b.popover.as_ref().map(|p| p.visible).unwrap_or(false));
            let taskbar_open = !self.taskbar.active() || self.taskbar.temporary_show();
            if self.slow_ticks % 3 == 0 {
                let mut jobs: Vec<(String, Vec<isize>)> = Vec::new();
                if let Some(bar) = self.topbars.first() {
                    for it in &bar.tray_items {
                        if it.app_hwnds.is_empty() {
                            continue;
                        }
                        let pinned = self.config.topbar.pinned_tray_keys.iter().any(|k| k == &it.key);
                        if pinned || popover_open {
                            jobs.push((it.key.clone(), it.app_hwnds.clone()));
                        }
                    }
                }
                if !jobs.is_empty() {
                    tray_mirror::spawn_live_window_icon_refresh(self.hwnd, jobs, self.tray_icons.clone());
                }
            }
            // 顶部固定的图标与收纳面板中的图标也需要跟随原生状态更新。
            let tracks_visible_icons = popover_open || !self.config.topbar.pinned_tray_keys.is_empty();
            let samples_notifications = self.topbars.first().is_some_and(|bar| bar.tray_items.iter().any(|item| {
                !item.overflow && item.uia_key.is_some() && bar.pinned.contains(&item.key)
            }));
            let poll_seconds = if taskbar_open { 2 } else if samples_notifications { 3 } else { 6 };
            let due = self.last_tray_refresh.is_none_or(|last| last.elapsed() >= Duration::from_secs(poll_seconds));
            if (tracks_visible_icons || taskbar_open) && due && !foreground_has_menu()
                && !(self.frame_on && self.docks.iter().any(Dock::animating))
            {
                self.refresh_tray();
            }
        }
        // 内存软水位降载
        if self.slow_ticks % 10 == 0 && Self::memory_bytes() > SOFT_WATERMARK {
            crate::log::log("内存超过 85MB 软水位，执行降载");
            let gen = crate::dock::icons::current_generation();
            self.icons.trim_to(crate::dock::icons::ICON_BUDGET_BYTES / 2, gen);
        }
        // 低频自动检测更新（启动 30 秒后一次）
        if !self.update_checked && self.slow_ticks >= 30 && self.config.general.auto_check_update {
            self.update_checked = true;
            crate::update::spawn_check(self.hwnd);
        }
    }

    fn memory_bytes() -> u64 {
        use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        unsafe {
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).is_ok() {
                c.WorkingSetSize as u64
            } else {
                0
            }
        }
    }

    // ---------- 设置 ----------

    pub fn open_settings(&mut self, page: Option<crate::settings::Page>) {
        if let Some(s) = self.settings.as_mut().filter(|s| s.is_open()) {
            s.activate();
            if let Some(p) = page {
                s.show_page(p);
            }
            return;
        }
        self.settings = None;
        match crate::settings::Settings::open(self) {
            Ok(mut s) => {
                if let Some(p) = page {
                    s.show_page(p);
                }
                self.settings = Some(s);
            }
            Err(e) => crate::log::log(&format!("设置窗口创建失败: {e}")),
        }
    }

    /// 关闭设置窗口（设置窗口 WM_CLOSE 入口）。
    pub fn close_settings(&mut self) {
        if let Some(mut s) = self.settings.take() {
            s.destroy();
        }
    }

    /// 设置页实时预览：应用当前配置（不落盘）。`rebuild` 表示显示器集合或启用状态变化需重建双栏。
    pub fn apply_config(&mut self, rebuild: bool) {
        if rebuild {
            self.rebuild_bars();
            return;
        }
        let cfg = self.config.clone();
        for dock in &mut self.docks {
            dock.apply_appearance(&cfg.appearance);
            dock.relayout(&cfg.dock, &cfg.appearance);
        }
        for bar in &mut self.topbars {
            bar.pinned = cfg.topbar.pinned_tray_keys.clone();
            bar.apply_config(&cfg.topbar, &cfg.appearance);
            invalidate(bar.hwnd);
        }
        self.on_state_changed();
        self.sync_edge_timer();
        self.ensure_frame();
    }

    /// 落盘配置与 Dock 固定项。
    pub fn persist(&mut self) {
        let _ = self.store.save_config(&self.config);
        let _ = self.store.save_dock_items(&self.dock_items);
    }

    pub fn set_dock_items(&mut self, items: DockItems) {
        self.dock_items = items;
        let _ = self.store.save_dock_items(&self.dock_items);
        self.refresh_entries();
    }

    pub fn check_update_manual(&mut self) {
        self.update_checked = true;
        crate::update::spawn_check(self.hwnd);
    }

    // ---------- 设置辅助 ----------

    /// 设置窗口录入快捷键期间暂停主进程全局热键（否则组合键会被 RegisterHotKey 吞掉）。
    pub fn suspend_hotkeys(&mut self, on: bool) {
        hotkey::unregister(self.hwnd, HK_DOCK);
        hotkey::unregister(self.hwnd, HK_TOPBAR);
        if !on {
            self.register_hotkeys();
        }
    }

    /// 绑定全局快捷键并立即生效；冲突时保持原绑定并返回原因。
    pub fn assign_hotkey(&mut self, slot: crate::settings::HotkeySlot, text: &str) -> Result<(), String> {
        use crate::settings::HotkeySlot;
        let spec = hotkey::parse(text).ok_or("快捷键格式无效")?;
        let g = self.config.general.clone();
        let same = |s: &str| hotkey::parse(s).map(|o| o == spec).unwrap_or(false);
        let duplicate = match slot {
            HotkeySlot::Dock => same(&g.toggle_topbar_hotkey) || same(&g.emergency_restore_hotkey),
            HotkeySlot::Topbar => same(&g.toggle_dock_hotkey) || same(&g.emergency_restore_hotkey),
            HotkeySlot::Restore => same(&g.toggle_dock_hotkey) || same(&g.toggle_topbar_hotkey),
        };
        if duplicate {
            return Err("与其他快捷键重复".into());
        }
        match slot {
            HotkeySlot::Dock | HotkeySlot::Topbar => {
                let (id, cur) = if slot == HotkeySlot::Dock {
                    (HK_DOCK, &g.toggle_dock_hotkey)
                } else {
                    (HK_TOPBAR, &g.toggle_topbar_hotkey)
                };
                hotkey::unregister(self.hwnd, id);
                if !hotkey::register(self.hwnd, id, spec) {
                    if let Some(old) = hotkey::parse(cur) {
                        hotkey::register(self.hwnd, id, old);
                    }
                    return Err("快捷键已被其他程序占用".into());
                }
                if slot == HotkeySlot::Dock {
                    self.config.general.toggle_dock_hotkey = text.to_string();
                } else {
                    self.config.general.toggle_topbar_hotkey = text.to_string();
                }
            }
            HotkeySlot::Restore => {
                // 紧急恢复热键由守护进程持有：更新配置后重启守护进程
                self.config.general.emergency_restore_hotkey = text.to_string();
                if let Some(h) = self.guard_handle.take() {
                    unsafe {
                        let _ = TerminateProcess(h, 0);
                        let _ = CloseHandle(h);
                    }
                }
                self.spawn_guard();
            }
        }
        let _ = self.store.save_config(&self.config);
        Ok(())
    }

    // ---------- IPC / 消息入口 ----------

    fn on_ipc(&mut self, cmd: usize) -> LRESULT {
        match cmd {
            IPC_SETTINGS => self.open_settings(None),
            IPC_TOGGLE_DOCK => self.toggle_dock(),
            IPC_TOGGLE_TOPBAR => self.toggle_topbar(),
            IPC_RESTORE_TASKBAR => {
                self.taskbar.restore_all();
                self.taskbar.paused = true;
            }
            _ => return LRESULT(0),
        }
        LRESULT(1)
    }

    fn on_hotkey(&mut self, id: i32) {
        match id {
            HK_DOCK => self.toggle_dock(),
            HK_TOPBAR => self.toggle_topbar(),
            _ => {}
        }
    }

    fn on_icon(&mut self, lparam: isize) {
        let result = *unsafe { Box::from_raw(lparam as *mut IconResult) };
        if let Some(px) = result.pixels {
            self.icons.insert(&result.key, result.size, px, result.w, result.h, crate::dock::icons::current_generation());
            self.ensure_frame();
        }
        if let Some(s) = &self.settings {
            s.invalidate();
        }
    }

    fn on_taskbar_created(&mut self) {
        self.tray.add();
        self.taskbar.on_taskbar_created();
        // Explorer 重建会注销旧托盘按钮，等待新 Explorer 发布实际条目。
        self.tray_raw_items.clear();
        self.sync_shelf_items();
        for bar in &mut self.topbars {
            // Explorer 重启后 AppBar 注册失效，需重新 ABM_NEW
            bar.appbar = false;
            bar.apply_wanted();
            invalidate(bar.hwnd);
        }
        if self.taskbar.active() && !self.taskbar.temporary_show() {
            // 新任务栏窗口仍需隐藏
            self.taskbar.hide_all(std::process::id(), self.config.general.taskbar_autohide);
        }
        self.refresh_tray();
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_FRAME => self.on_frame(),
            TIMER_SLOW => self.on_slow(),
            TIMER_EDGE => self.on_edge_timer(),
            TIMER_TRAY_CONTEXT => self.track_tray_context(),
            TIMER_TRAY_TIP => {
                if matches!(self.tray_work, Some(TrayWork::Click { right: true, .. })) {
                    self.tray_tip.clear(self.hwnd);
                } else {
                    self.tray_tip.on_timer(self.hwnd, self.config.appearance.is_dark());
                }
            }
            TIMER_TRAY_FLASH => {
                let (active, changed) = self.tray_icons.advance_flash_frames();
                if !active {
                    unsafe { let _ = KillTimer(self.hwnd, TIMER_TRAY_FLASH); }
                }
                if changed || !active {
                    self.repaint_tray_frames();
                }
            }
            _ => {}
        }
    }

    // ---------- 菜单 ----------

    fn popup(&mut self, items: Vec<MenuItem>, ctx: MenuCtx) {
        let pt = cursor_pos();
        self.popup_at(items, ctx, pt.x, pt.y);
    }

    /// 在指定屏幕坐标弹出 macOS 风格自绘菜单。
    fn popup_at(&mut self, items: Vec<MenuItem>, ctx: MenuCtx, x: i32, y: i32) {
        self.close_popover();
        self.status_panel.close();
        self.window_preview.close();
        self.menu_ctx = ctx;
        let ptr = app_ptr();
        let appearance = self.config.appearance.clone();
        self.context_menu.open(ptr, items, x, y, &appearance);
    }

    pub fn close_context_menu(&mut self) {
        self.context_menu.close();
    }

    pub fn menu_mouse_move(&mut self, x: i32, y: i32) {
        self.context_menu.on_mouse_move(x, y);
    }

    pub fn menu_mouse_leave(&mut self) {
        self.context_menu.on_mouse_leave();
    }

    pub fn menu_lbutton_down(&mut self, x: i32, y: i32) {
        self.context_menu.on_lbutton_down(x, y);
    }

    pub fn menu_lbutton_up(&mut self, x: i32, y: i32) {
        if let Some(id) = self.context_menu.on_lbutton_up(x, y) {
            self.on_menu(id);
        }
    }

    pub fn menu_key_down(&mut self, vk: u32) {
        if let Some(id) = self.context_menu.on_key_down(vk) {
            self.on_menu(id);
        }
    }

    fn product_menu(&mut self) {
        let pt = cursor_pos();
        self.product_menu_at(pt.x, pt.y);
    }

    fn product_menu_at(&mut self, x: i32, y: i32) {
        let taskbar_label = if self.taskbar.active() && self.taskbar.temporary_show() {
            "重新隐藏 Windows 任务栏"
        } else if self.taskbar.active() {
            "暂时显示 Windows 任务栏"
        } else {
            "隐藏 Windows 任务栏"
        };
        let v = vec![
            MenuItem::normal(M_SETTINGS, "WinDockFinder设置"),
            MenuItem::normal(M_ABOUT, "关于 WinDockFinder"),
            MenuItem::sep(),
            MenuItem::normal(M_TOGGLE_DOCK, if self.dock_visible { "隐藏 Dock" } else { "显示 Dock" }),
            MenuItem::normal(M_TOGGLE_TOPBAR, if self.topbar_visible { "隐藏顶部栏" } else { "显示顶部栏" }),
            MenuItem::normal(M_TOGGLE_TASKBAR, taskbar_label),
            MenuItem::sep(),
            MenuItem::normal(M_LOCK, "锁定屏幕"),
            MenuItem::normal(M_SLEEP, "睡眠"),
            MenuItem::danger(M_RESTART, "重新启动"),
            MenuItem::danger(M_SHUTDOWN, "关机"),
            MenuItem::sep(),
            MenuItem::normal(M_TASKMGR, "任务管理器"),
            MenuItem::danger(M_EXIT, "退出 WinDockFinder"),
        ];
        self.popup_at(v, MenuCtx::None, x, y);
    }

    // ---------- 消息入口 ----------

    pub fn on_monitor_event(&mut self) {
        monitor::refresh();
        self.snap = monitor::snapshot();
        self.refresh_entries();
        self.sync_shelf_items();
        self.on_state_changed();
        self.ensure_frame();
    }

    pub fn on_update(&mut self, lparam: isize) {
        let result = *unsafe { Box::from_raw(lparam as *mut crate::update::CheckResult) };
        // 设置窗口打开时由「关于与更新」页接管结果；返回 true 表示需要立即退出以应用更新
        if let Some(s) = self.settings.as_mut().filter(|s| s.is_open()) {
            if s.on_update_result(result) {
                self.quit();
            }
            return;
        }
        match result {
            crate::update::CheckResult::Available(b) => {
                self.notify_tray(&format!("新版本 {} 可用，打开「关于与更新」一键升级。", b.version));
            }
            crate::update::CheckResult::Failed(e) => crate::log::log(&format!("自动检查更新失败: {e}")),
            crate::update::CheckResult::Latest => {}
        }
    }

    fn on_update_progress(&mut self, progress: usize) {
        if let Some(s) = self.settings.as_mut().filter(|s| s.is_open()) {
            s.on_update_progress(progress);
        }
    }

    fn notify_tray(&self, text: &str) {
        let mut info = [0u16; 256];
        fill_buf(&mut info, text);
        let mut title = [0u16; 64];
        fill_buf(&mut title, "WinDockFinder");
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            szInfo: info,
            szInfoTitle: title,
            dwInfoFlags: NIIF_INFO,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    pub fn on_tray(&mut self, msg: u32) {
        if msg != WM_RBUTTONUP && msg != WM_LBUTTONUP {
            return;
        }
        use crate::startup::StartupState;
        let startup_st = crate::startup::state();
        let startup_item = match startup_st {
            StartupState::Enabled => MenuItem::checked(M_STARTUP, "开机自启", true),
            StartupState::DisabledBySystem => MenuItem::normal(M_STARTUP, "开机自启（已被任务管理器禁用）"),
            StartupState::Off => MenuItem::normal(M_STARTUP, "开机自启"),
        };
        let taskbar_label = if self.taskbar.active() && self.taskbar.temporary_show() {
            "重新隐藏 Windows 任务栏"
        } else if self.taskbar.active() {
            "暂时显示 Windows 任务栏"
        } else {
            "隐藏 Windows 任务栏"
        };
        let v = vec![
            MenuItem::normal(M_SETTINGS, "WinDockFinder设置"),
            MenuItem::sep(),
            MenuItem::normal(M_TOGGLE_DOCK, if self.dock_visible { "隐藏 Dock" } else { "显示 Dock" }),
            MenuItem::normal(M_TOGGLE_TOPBAR, if self.topbar_visible { "隐藏顶部栏" } else { "显示顶部栏" }),
            MenuItem::normal(M_TOGGLE_TASKBAR, taskbar_label),
            MenuItem::sep(),
            startup_item,
            MenuItem::sep(),
            MenuItem::danger(M_EXIT, "退出 WinDockFinder"),
        ];
        self.popup(v, MenuCtx::None);
    }

    pub fn on_menu(&mut self, id: usize) {
        let ctx = self.menu_ctx.clone();
        match id {
            M_SETTINGS => self.open_settings(None),
            M_ABOUT => self.open_settings(Some(crate::settings::Page::About)),
            M_TOGGLE_DOCK => self.toggle_dock(),
            M_TOGGLE_TOPBAR => self.toggle_topbar(),
            M_TOGGLE_TASKBAR => self.toggle_taskbar(),
            M_EXIT => self.quit(),
            M_LOCK => {
                let _ = crate::win32::open_shell_path("open", "rundll32.exe", Some("user32.dll,LockWorkStation"));
            }
            M_SLEEP => {
                unsafe {
                    let _ = windows::Win32::System::Power::SetSuspendState(false, true, false);
                }
            }
            M_RESTART => {
                let _ = crate::win32::open_shell_path("open", "shutdown.exe", Some("/r /t 0"));
            }
            M_SHUTDOWN => {
                let _ = crate::win32::open_shell_path("open", "shutdown.exe", Some("/s /t 0"));
            }
            M_TASKMGR => {
                let _ = crate::win32::open_shell_path("open", "taskmgr.exe", None);
            }
            M_STARTUP => self.toggle_startup(),
            M_TRASH_OPEN => items::open_recycle_bin(),
            M_TRASH_EMPTY => {
                items::empty_recycle_bin();
                self.trash_full = items::recycle_bin_full();
                for d in &mut self.docks {
                    d.dirty = true;
                }
                self.ensure_frame();
            }
            M_APP_OPEN..=M_APP_QUIT => self.dock_menu_action(id, ctx),
            M_WINDOW_BASE..=M_WINDOW_LAST => {
                if let MenuCtx::DockEntry(i) = ctx {
                    let hwnd = self.docks.first().and_then(|d| d.entries.apps.get(i)).and_then(|e| e.windows.get(id - M_WINDOW_BASE)).map(|w| w.0);
                    if let Some(h) = hwnd {
                        items::activate_window(HWND(h as *mut _));
                    }
                }
            }
            M_FG_MIN..=M_FG_RIGHT_HALF => {
                if let MenuCtx::Foreground(h) = ctx {
                    self.foreground_action(HWND(h as *mut _), id);
                }
            }
            M_SHOW_DESKTOP => {
                // 最小化当前已枚举的所有普通应用窗口，等效显示桌面
                let snap = monitor::snapshot();
                for w in snap.windows {
                    if !w.minimized {
                        unsafe {
                            let _ = ShowWindow(w.hwnd, SW_MINIMIZE);
                        }
                    }
                }
            }
            M_NEW_FINDER => {
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some("shell:MyComputerFolder"));
            }
            M_OPEN_TERMINAL => {
                if !crate::win32::open_shell_path("open", "wt.exe", None) {
                    let _ = crate::win32::open_shell_path("open", "powershell.exe", None);
                }
            }
            M_OPEN_BASE..=M_OPEN_LAST => {
                let shell = [
                    "shell:MyComputerFolder",
                    "shell:Desktop",
                    "shell:Downloads",
                    "shell:Personal",
                    "shell:RecycleBinFolder",
                    "ms-settings:",
                ][(id - M_OPEN_BASE) % 6];
                let _ = crate::win32::open_shell_path("open", "explorer.exe", Some(shell));
            }
            _ => {}
        }
    }

    fn foreground_action(&mut self, hwnd: HWND, id: usize) {
        unsafe {
            match id {
                M_FG_MIN => {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
                M_FG_MAX => {
                    let _ = ShowWindow(hwnd, if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
                }
                M_FG_CENTER => {
                    if IsZoomed(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let mut r = RECT::default();
                    if GetWindowRect(hwnd, &mut r).is_ok() {
                        if let Some(mon) = crate::win32::monitor_of_window(hwnd) {
                            let w = r.right - r.left;
                            let h = r.bottom - r.top;
                            let x = mon.work.left + ((mon.work.right - mon.work.left - w) / 2).max(0);
                            let y = mon.work.top + ((mon.work.bottom - mon.work.top - h) / 2).max(0);
                            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                        }
                    }
                }
                M_FG_LEFT_HALF | M_FG_RIGHT_HALF => {
                    if IsZoomed(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    if let Some(mon) = crate::win32::monitor_of_window(hwnd) {
                        let half_w = (mon.work.right - mon.work.left) / 2;
                        let h = mon.work.bottom - mon.work.top;
                        let x = if id == M_FG_LEFT_HALF { mon.work.left } else { mon.work.left + half_w };
                        let _ = SetWindowPos(hwnd, None, x, mon.work.top, half_w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
                _ => {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    fn dock_menu_action(&mut self, id: usize, ctx: MenuCtx) {
        let MenuCtx::DockEntry(i) = ctx else { return };
        let Some(e) = self.docks.first().and_then(|d| d.entries.apps.get(i)).cloned() else { return };
        if e.id == "special:launchpad" {
            if id == M_APP_OPEN {
                items::toggle_launchpad(&self.config.general.winlaunchpad_path);
            }
            return;
        }
        let item = e.pinned.and_then(|p| self.pinned_item(p));
        match id {
            M_APP_OPEN | M_APP_NEW => {
                if let Some(it) = &item {
                    items::launch_item(it);
                } else if let Some(exe) = &e.running_exe {
                    let _ = crate::win32::open_shell_path("open", exe, None);
                }
            }
            M_APP_ADMIN => {
                if let Some(it) = &item {
                    items::run_as_admin(it);
                }
            }
            M_APP_LOCATION => {
                if let Some(it) = &item {
                    items::open_file_location(it);
                }
            }
            M_APP_PIN => {
                let target = item.as_ref().map(|it| it.target.clone()).or(e.running_exe.clone());
                if let Some(t) = target {
                    if let Some(it) = items::item_from_path(&t) {
                        items::add_pinned(&mut self.dock_items, it);
                        self.after_items_change();
                    }
                }
            }
            M_APP_UNPIN => {
                items::remove_pinned(&mut self.dock_items, &e.id);
                self.after_items_change();
            }
            _ => {
                for w in &e.windows {
                    items::close_window(HWND(w.0 as *mut _));
                }
            }
        }
    }

    fn after_items_change(&mut self) {
        let _ = self.store.save_dock_items(&self.dock_items);
        self.refresh_entries();
    }

    fn foreground_window(&self) -> Option<HWND> {
        let h = HWND(self.snap.foreground as *mut _);
        if h.0.is_null() || is_desktop_class(&class_name(h)) {
            None
        } else {
            Some(h)
        }
    }

    // ---------- Dock 事件 ----------

    fn dock_index(&self, hwnd: HWND) -> Option<usize> {
        self.docks.iter().position(|d| d.hwnd_overlay == hwnd || d.hwnd_trigger == hwnd)
    }

    pub fn dock_mouse_move(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.dock_index(hwnd) else { return };
        // Dock 放大动画优先于后台托盘采样，右键和悬停详情仍照常处理。
        if let Some(TrayWork::Refresh { canceled, reading_overflow: false, .. }) = &self.tray_work {
            canceled.store(true, Ordering::Relaxed);
        }
        self.mouse_in_dock = true;
        self.hide_at = None;
        self.edge_since = None;
        {
            let dock = &mut self.docks[i];
            let hit = dock.hit(x, y);
            let inside = dock.in_capsule(x, y);
            if hit != dock.hover {
                dock.hover = hit;
                dock.hover_since = hit.map(|_| Instant::now());
                if hit.is_none() {
                    dock.tooltip_visible = false;
                }
                dock.dirty = true;
            }
            dock.mouse = (x, y);
            dock.mouse_in = true;
            // 按住固定应用拖动超过阈值后自动启动拖拽排序/拖出移除
            if dock.drag.is_none() {
                if let Some((pi, px, py)) = dock.press {
                    let dist = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
                    let is_pinned = dock.entries.apps.get(pi).and_then(|e| e.pinned).is_some();
                    if is_pinned && dist >= 6.0 * dock.dpi_scale() {
                        dock.drag = Some(crate::dock::DragState {
                            from: pi,
                            x,
                            y,
                            over: if inside { hit } else { None },
                        });
                        dock.tooltip_visible = false;
                        dock.dirty = true;
                    }
                }
            }
            if let Some(d) = dock.drag.as_mut() {
                d.x = x;
                d.y = y;
                d.over = if inside { hit } else { None };
                dock.dirty = true;
            }
        }
        self.ensure_frame();
    }

    pub fn dock_mouse_leave(&mut self, hwnd: HWND) {
        let Some(i) = self.dock_index(hwnd) else { return };
        {
            let dock = &mut self.docks[i];
            dock.mouse_in = false;
            dock.hover = None;
            dock.tooltip_visible = false;
            dock.hover_since = None;
            dock.dirty = true;
            if dock.drag.is_none() {
                dock.press = None;
            }
        }
        if !self.window_preview.mouse_in && !self.context_menu.visible {
            self.mouse_in_dock = false;
            if self.dock_revealed {
                self.hide_at = Some(Instant::now());
            }
        }
        self.ensure_frame();
    }

    pub fn dock_lbutton_down(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.dock_index(hwnd) else { return };
        self.window_preview.close();
        {
            let dock = &mut self.docks[i];
            dock.press = dock.hit(x, y).map(|h| (h, x, y));
            dock.dirty = true;
        }
        self.ensure_frame();
    }

    pub fn dock_lbutton_up(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.dock_index(hwnd) else { return };
        let (drag, press, hit) = {
            let dock = &mut self.docks[i];
            let hit = dock.hit(x, y);
            (dock.drag.take(), dock.press.take(), hit)
        };
        if let Some(d) = drag.clone() {
            let below = y > self.docks[i].capsule_rect().bottom as f32 + 8.0 * self.docks[i].dpi_scale();
            let over_trash = d.over.map(|o| matches!(self.docks[i].func_kind(o), Some(FuncKind::Trash))).unwrap_or(false);
            let n = self.docks[i].entries.apps.len();
            let from = self.docks[i].entries.apps.get(d.from).cloned();
            if let Some(e) = from.filter(|e| e.pinned.is_some()) {
                let over_entry = d.over.filter(|o| *o < n).and_then(|o| self.docks[i].entries.apps.get(o).cloned());
                let after = match d.over {
                    Some(o) if o < n => {
                        let dock = &self.docks[i];
                        let cap = dock.capsule_rect();
                        x > cap.left as f32 + dock.layout.slots.get(o).map(|s| s.center_x).unwrap_or(0.0)
                    }
                    _ => false,
                };
                if over_trash || (below && d.over.is_none()) {
                    items::remove_pinned(&mut self.dock_items, &e.id);
                    self.after_items_change();
                } else if let Some(oe) = over_entry {
                    let before_id = if after {
                        self.docks[i].entries.apps.get(d.over.unwrap_or(0) + 1).filter(|x| x.pinned.is_some()).map(|x| x.id.clone())
                    } else {
                        Some(oe.id.clone())
                    };
                    items::reorder(&mut self.dock_items, &e.id, before_id.as_deref());
                    self.after_items_change();
                } else if d.over.is_some() {
                    // 放到功能区左侧：移动到固定项末尾
                    let last = self.docks[i].entries.apps.iter().rev().find(|x| x.pinned.is_some()).map(|x| x.id.clone());
                    items::reorder(&mut self.dock_items, &e.id, last.as_deref());
                    self.after_items_change();
                }
            }
        } else if let Some((pi, _, _)) = press {
            if hit == Some(pi) {
                self.activate_entry(pi);
            }
        }
        self.ensure_frame();
    }

    pub fn dock_rbutton_up(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.dock_index(hwnd) else { return };
        let hit = self.docks[i].hit(x, y);
        match hit {
            None => self.product_menu(),
            Some(h) => {
                let n = self.docks[i].entries.apps.len();
                if h < n {
                    self.dock_app_menu(h);
                } else if matches!(self.docks[i].func_kind(h), Some(FuncKind::Trash)) {
                    self.popup(
                        vec![
                            MenuItem::normal(M_TRASH_OPEN, "打开回收站"),
                            MenuItem::danger(M_TRASH_EMPTY, "清空回收站"),
                        ],
                        MenuCtx::None,
                    );
                } else {
                    self.run_func(h - n);
                }
            }
        }
    }

    fn dock_app_menu(&mut self, i: usize) {
        let Some(e) = self.docks.first().and_then(|d| d.entries.apps.get(i)).cloned() else { return };
        if e.id == "special:launchpad" {
            self.popup(vec![MenuItem::normal(M_APP_OPEN, "打开启动台")], MenuCtx::DockEntry(i));
            return;
        }
        let mut v = vec![MenuItem::normal(M_APP_OPEN, "打开")];
        if e.pinned.is_some() {
            v.push(MenuItem::normal(M_APP_NEW, "新建窗口"));
            v.push(MenuItem::normal(M_APP_ADMIN, "以管理员身份运行"));
            v.push(MenuItem::normal(M_APP_LOCATION, "打开所在位置"));
        }
        v.push(MenuItem::sep());
        if e.pinned.is_some() {
            v.push(MenuItem::danger(M_APP_UNPIN, "从 Dock 中移除"));
        } else {
            v.push(MenuItem::normal(M_APP_PIN, "固定到 Dock"));
        }
        if !e.windows.is_empty() {
            v.push(MenuItem::sep());
            v.push(MenuItem::danger(M_APP_QUIT, format!("退出「{}」", e.label)));
        }
        self.popup(v, MenuCtx::DockEntry(i));
    }

    pub fn dock_capture_lost(&mut self, hwnd: HWND) {
        let Some(i) = self.dock_index(hwnd) else { return };
        self.docks[i].drag = None;
        self.docks[i].press = None;
        self.ensure_frame();
    }

    pub fn dock_edge_hovered(&mut self, _hwnd: HWND) {
        if self.edge_since.is_none() {
            self.edge_since = Some(Instant::now());
        }
        self.ensure_frame();
    }

    pub fn dock_edge_left(&mut self, _hwnd: HWND) {
        self.edge_since = None;
        if self.dock_revealed && !self.mouse_in_dock {
            self.hide_at = Some(Instant::now());
        }
        self.ensure_frame();
    }

    pub fn dock_external_drag(&mut self, hwnd: HWND, _x: i32, _y: i32, _files: Option<Vec<String>>) {
        if self.dock_index(hwnd).is_some() {
            self.mouse_in_dock = true;
            self.hide_at = None;
            self.ensure_frame();
        }
    }

    pub fn dock_external_drag_leave(&mut self, hwnd: HWND) {
        if self.dock_index(hwnd).is_some() {
            self.mouse_in_dock = false;
            if self.dock_revealed {
                self.hide_at = Some(Instant::now());
            }
            self.ensure_frame();
        }
    }

    pub fn dock_external_drop(&mut self, hwnd: HWND, sx: i32, sy: i32, files: Vec<String>) {
        self.mouse_in_dock = false;
        let Some(i) = self.dock_index(hwnd) else { return };
        let over_trash = {
            let dock = &self.docks[i];
            let x = (sx - dock.win_rect.left) as f32;
            let y = (sy - dock.win_rect.top) as f32;
            dock.hit(x, y).map(|h| matches!(dock.func_kind(h), Some(FuncKind::Trash))).unwrap_or(false)
        };
        if files.is_empty() {
            return;
        }
        if over_trash {
            if items::move_to_recycle_bin(&files) {
                self.trash_full = true;
            }
        } else {
            for f in &files {
                if let Some(it) = items::item_from_path(f) {
                    items::add_pinned(&mut self.dock_items, it);
                }
            }
            self.after_items_change();
        }
        self.ensure_frame();
    }

    fn activate_entry(&mut self, i: usize) {
        let Some(dock) = self.docks.first() else { return };
        let n = dock.entries.apps.len();
        if i >= n {
            self.run_func(i - n);
            return;
        }
        let Some(e) = dock.entries.apps.get(i).cloned() else { return };
        if e.id == "special:launchpad" {
            items::toggle_launchpad(&self.config.general.winlaunchpad_path);
            return;
        }
        match e.windows.len() {
            1 => {
                let target = HWND(e.windows[0].0 as *mut _);
                let is_fg = unsafe { GetForegroundWindow() == target && !IsIconic(target).as_bool() };
                if is_fg {
                    unsafe {
                        let _ = ShowWindow(target, SW_MINIMIZE);
                    }
                } else {
                    items::activate_window(target);
                }
            }
            0 => {
                // 未运行时触发启动弹跳动效并打开应用
                for d in &mut self.docks {
                    d.trigger_bounce(i);
                }
                self.ensure_frame();
                if let Some(p) = e.pinned.and_then(|p| self.pinned_item(p)) {
                    items::launch_item(&p);
                } else if let Some(exe) = &e.running_exe {
                    let _ = crate::win32::open_shell_path("open", exe, None);
                }
            }
            _ => {
                let mut v: Vec<MenuItem> = e
                    .windows
                    .iter()
                    .enumerate()
                    .map(|(k, w)| MenuItem::normal(M_WINDOW_BASE + k, w.1.chars().take(64).collect::<String>()))
                    .collect();
                v.push(MenuItem::sep());
                v.push(MenuItem::danger(M_APP_QUIT, format!("退出「{}」", e.label)));
                self.popup(v, MenuCtx::DockEntry(i));
            }
        }
    }

    fn run_func(&mut self, k: usize) {
        let Some(kind) = self.docks.first().and_then(|d| d.entries.right.get(k)).map(|f| f.kind.clone()) else { return };
        match kind {
            FuncKind::Folder(idx) => {
                if let Some(it) = self.pinned_item(idx) {
                    items::open_folder(&it.target);
                }
            }
            FuncKind::Trash => items::open_recycle_bin(),
            FuncKind::Settings => self.open_settings(None),
            FuncKind::Launchpad => items::toggle_launchpad(&self.config.general.winlaunchpad_path),
        }
    }

    // ---------- 顶部栏事件 ----------

    fn topbar_index(&self, hwnd: HWND) -> Option<usize> {
        self.topbars.iter().position(|b| b.hwnd == hwnd)
    }

    fn popover_owner(&self, hwnd: HWND) -> Option<usize> {
        self.topbars.iter().position(|b| b.popover.as_ref().map(|p| p.hwnd) == Some(hwnd))
    }

    pub fn paint_topbar(&mut self, hwnd: HWND) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        let t = &self.config.topbar;
        let a = &self.config.appearance;
        let icons = &self.tray_icons;
        let bar = &mut self.topbars[i];
        crate::topbar::render::paint(hwnd, bar, t, a, icons);
    }

    pub fn topbar_mouse_move(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        let bar = &mut self.topbars[i];
        if let Some((pkey, px, py)) = bar.press_icon.clone() {
            if bar.drag.is_none() {
                let dist = (((x - px).pow(2) + (y - py).pow(2)) as f32).sqrt();
                if dist >= 5.0 * (bar.dpi as f32 / 96.0) {
                    let (ins_before, ins_x) = Self::compute_topbar_insert(bar, &pkey, x);
                    bar.drag = Some(TopbarDrag {
                        key: pkey,
                        x,
                        insert_before: ins_before,
                        insert_x: ins_x,
                    });
                    invalidate(bar.hwnd);
                    self.tray_tip.clear(self.hwnd);
                    return;
                }
            }
        }
        if let Some(d) = bar.drag.as_ref() {
            let key = d.key.clone();
            let (ins_before, ins_x) = Self::compute_topbar_insert(bar, &key, x);
            if let Some(d) = bar.drag.as_mut() {
                d.x = x;
                d.insert_before = ins_before;
                d.insert_x = ins_x;
            }
            invalidate(bar.hwnd);
            return;
        }
        let hit = bar.hit_test(x, y);
        bar.set_hover(hit);
        self.update_tray_tip(hwnd, x, y);
    }

    /// 根据当前命中的格子提取实际托盘提示，而非替换成窗口的显示名称。
    fn update_tray_tip(&mut self, hwnd: HWND, x: i32, y: i32) {
        let mut hidden_icon = false;
        let target = if self.tray_context.is_some() {
            None
        } else if let Some(index) = self.topbar_index(hwnd) {
            let bar = &self.topbars[index];
            match bar.hit_test(x, y) {
                Some(hit @ Hit::TrayIcon(_)) if bar.press_icon.is_none() && bar.drag.is_none() => {
                    let Hit::TrayIcon(key) = &hit else { return };
                    bar.tray_items.iter().find(|item| &item.key == key).and_then(|item| {
                        hidden_icon = item.overflow;
                        Some(TipTarget { owner: hwnd, key: key.clone(), anchor: tray_anchor(hwnd, bar.rect_of(&hit)?)?,
                            dpi: bar.dpi, text: item.hint().to_string() })
                    })
                }
                _ => None,
            }
        } else {
            self.popover_owner(hwnd).and_then(|index| self.topbars[index].popover.as_ref()).and_then(|p| {
                if !p.visible || p.press.is_some() { return None; }
                let index = p.index_at(x, y)?;
                let item = p.items.get(index)?;
                hidden_icon = item.overflow;
                Some(TipTarget { owner: hwnd, key: item.key.clone(), anchor: tray_anchor(hwnd, p.cell_rect(index)?)?,
                    dpi: p.dpi, text: item.hint().to_string() })
            })
        };
        if hidden_icon && target.as_ref().is_some_and(|target| !self.tray_tip.tracks(target))
            && self.last_tray_details.is_none_or(|last| last.elapsed() >= Duration::from_secs(3))
        {
            self.tray_details_pending = true;
            if let Some(TrayWork::Refresh { canceled, reading_overflow: false, .. }) = &self.tray_work {
                canceled.store(true, Ordering::Relaxed);
            }
            self.refresh_tray();
        }
        self.tray_tip.track(self.hwnd, target, self.config.appearance.is_dark());
    }

    fn refresh_tray_tip(&mut self) {
        let Some(owner) = self.tray_tip.owner() else { return };
        let mut point = POINT::default();
        let mut rect = RECT::default();
        if unsafe { GetCursorPos(&mut point).is_ok() && GetWindowRect(owner, &mut rect).is_ok() } {
            self.update_tray_tip(owner, point.x - rect.left, point.y - rect.top);
        }
    }

    fn compute_topbar_insert(bar: &TopBar, drag_key: &str, x: i32) -> (Option<String>, i32) {
        let mut slots: Vec<(String, RECT)> = bar
            .hits
            .iter()
            .filter_map(|(r, h)| match h {
                Hit::TrayIcon(k) => Some((k.clone(), *r)),
                _ => None,
            })
            .collect();
        // hits 从右向左生成，按屏幕上从左到右排列，匹配 pinned_tray_keys 的视觉顺序（rev()）
        slots.sort_by_key(|(_, r)| r.left);
        // 找到最近插入间隙：在顶部栏中，pinned 列表按 rev() 从右往左排，即最左边的图标在 pinned 末尾，最右边的在 pinned 开头
        if slots.is_empty() {
            return (None, x);
        }
        // 用视觉位置判断落在第几个图标右侧/左侧
        // 视觉上第 idx 个槽位在 pinned 中的顺序是逆序的；若要放在视觉左侧（视觉更靠左 = pinned 更靠后），实际上直接按视觉索引排布最直观：
        // 构造当前视觉从左到右的键序，移除 drag_key 后按 x 插入，再反转即新的 pinned_tray_keys！
        let mut best_x = slots[0].1.left;
        let mut min_dist = (x - slots[0].1.left).abs();
        let mut insert_visual_idx = 0usize;
        for (idx, (_, r)) in slots.iter().enumerate() {
            let d_l = (x - r.left).abs();
            if d_l < min_dist {
                min_dist = d_l;
                best_x = r.left;
                insert_visual_idx = idx;
            }
            let d_r = (x - r.right).abs();
            if d_r < min_dist {
                min_dist = d_r;
                best_x = r.right;
                insert_visual_idx = idx + 1;
            }
        }
        // 将视觉插槽映射回 pinned_tray_keys 的 "before_key" 标识（使用特殊标记或直接通过 reorder_topbar_visual 重排）
        let _ = drag_key;
        (Some(format!("__vis_idx:{insert_visual_idx}")), best_x)
    }

    pub fn topbar_mouse_leave(&mut self, hwnd: HWND) {
        if self.tray_tip.owner() == Some(hwnd) {
            self.tray_tip.clear(self.hwnd);
        }
        let Some(i) = self.topbar_index(hwnd) else { return };
        if self.topbars[i].drag.is_none() {
            self.topbars[i].set_hover(None);
        }
    }

    pub fn topbar_lbutton_down(&mut self, hwnd: HWND, x: i32, y: i32) {
        self.tray_tip.clear(self.hwnd);
        let Some(i) = self.topbar_index(hwnd) else { return };
        if let Some(Hit::TrayIcon(key)) = self.topbars[i].hit_test(x, y) {
            self.topbars[i].press_icon = Some((key, x, y));
            unsafe {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
            }
            return;
        }
        self.topbar_click(hwnd, x, y, false);
    }

    pub fn topbar_lbutton_up(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        let press = self.topbars[i].press_icon.take();
        let drag = self.topbars[i].drag.take();
        unsafe {
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == hwnd {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            }
        }
        if let Some(d) = drag {
            if let Some(vis_str) = d.insert_before.as_deref().and_then(|s| s.strip_prefix("__vis_idx:")) {
                if let Ok(vis_idx) = vis_str.parse::<usize>() {
                    self.reorder_topbar_by_visual(i, &d.key, vis_idx);
                }
            }
            invalidate(hwnd);
            return;
        }
        if let Some((pkey, _, _)) = press {
            if matches!(self.topbars[i].hit_test(x, y), Some(Hit::TrayIcon(ref k)) if k == &pkey) {
                if let Some(item) = self.topbars[i].tray_items.iter().find(|item| item.key == pkey).cloned() {
                    self.invoke_tray_native(item, None);
                }
            }
        }
    }

    pub fn topbar_capture_lost(&mut self, hwnd: HWND) {
        if let Some(i) = self.topbar_index(hwnd) {
            self.topbars[i].press_icon = None;
            if self.topbars[i].drag.take().is_some() {
                invalidate(hwnd);
            }
        }
    }

    /// 顶部栏和收纳格共用原生托盘交互。普通窗口仍有自己的窗口菜单。
    fn invoke_tray_native(&mut self, item: TrayItem, anchor: Option<RECT>) {
        self.invoke_tray(TrayClick { item, anchor, popover: None });
    }

    fn invoke_tray(&mut self, click: TrayClick) {
        self.tray_tip.clear(self.hwnd);
        let TrayClick { item, anchor, popover } = click;
        let right = anchor.is_some();
        let screen = anchor.map_or(POINT::default(), |icon| POINT { x: icon.left, y: icon.bottom });
        if !right && !item.app_hwnds.is_empty() {
            self.invoke_window_shelf_item(&item, false, screen);
            return;
        }
        if let Some(work) = &mut self.tray_work {
            let queued_click = match work {
                TrayWork::Refresh { canceled, queued_click, .. } => {
                    canceled.store(true, Ordering::Relaxed);
                    queued_click
                }
                TrayWork::Click { queued_click, .. } => queued_click,
            };
            *queued_click = Some(TrayClick { item, anchor, popover });
            return;
        }
        let peek = self.taskbar.begin_tray_peek();
        if tray_mirror::spawn_invocation(self.hwnd, item.clone(), anchor, peek, self.tray_icons.clone()) {
            self.tray_work = Some(TrayWork::Click { right, screen, popover, queued_click: None });
        } else {
            self.taskbar.end_tray_peek();
            self.invoke_window_shelf_item(&item, right, screen);
            if popover.is_some() {
                self.close_popover();
            }
        }
    }

    fn on_tray_invoked(&mut self, result: TrayInvocation) {
        let Some(TrayWork::Click { right, screen, popover, queued_click }) = self.tray_work.take() else { return };
        self.taskbar.end_tray_peek();
        self.apply_tray_snapshot(result.snapshot);
        if let Some(click) = queued_click {
            self.invoke_tray(click);
            return;
        }
        // 普通窗口才回退到窗口菜单。
        if result.ordinary_window {
            self.invoke_window_shelf_item(&result.target, right, screen);
        }
        if popover.is_some_and(|owner| self.tray_context.as_ref().is_some_and(|ctx| ctx.popover == owner)) {
            if result.ordinary_window {
                self.close_popover();
            } else if let (Some(ctx), Some(menu)) = (&mut self.tray_context, result.menu) {
                ctx.menu = Some(HWND(menu as *mut _));
                // 原生菜单拥有自己的消息循环。只在它打开时检查显隐，子菜单展开期间保留收纳格。
                unsafe { let _ = SetTimer(self.hwnd, TIMER_TRAY_CONTEXT, 45, None); }
                self.track_tray_context();
            } else if let Some(ctx) = self.tray_context.take() {
                if let Some(index) = self.popover_owner(ctx.popover) {
                    if let Some(p) = self.topbars[index].popover.as_mut() {
                        p.set_context_key(None);
                    }
                }
            }
        }
    }

    /// 普通窗口沿用系统菜单或前置、最小化切换。
    fn invoke_window_shelf_item(&self, item: &TrayItem, right: bool, screen: POINT) {
        if let Some(&first_h) = item.app_hwnds.first() {
            let target = HWND(first_h as *mut _);
            if !unsafe { IsWindow(target).as_bool() } {
                return;
            }
            if right {
                self.show_native_window_system_menu(target, screen.x, screen.y);
            } else {
                let is_fg = unsafe { GetForegroundWindow() == target && !IsIconic(target).as_bool() };
                if is_fg {
                    unsafe {
                        let _ = ShowWindow(target, SW_MINIMIZE);
                    }
                } else {
                    items::activate_window(target);
                }
            }
        }
    }

    fn show_native_window_system_menu(&self, target: HWND, sx: i32, sy: i32) {
        unsafe {
            let hmenu = GetSystemMenu(target, false);
            if hmenu.0.is_null() {
                return;
            }
            let _ = SetForegroundWindow(self.hwnd);
            let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN;
            let cmd = TrackPopupMenu(hmenu, flags, sx, sy, 0, self.hwnd, None);
            if cmd.0 > 0 {
                let _ = PostMessageW(target, WM_SYSCOMMAND, WPARAM(cmd.0 as usize), LPARAM(0));
            }
        }
    }

    fn reorder_topbar_by_visual(&mut self, bar_idx: usize, key: &str, visual_idx: usize) {
        let mut vis_keys: Vec<(String, i32)> = self.topbars[bar_idx]
            .hits
            .iter()
            .filter_map(|(r, h)| match h {
                Hit::TrayIcon(k) => Some((k.clone(), r.left)),
                _ => None,
            })
            .collect();
        vis_keys.sort_by_key(|(_, l)| *l);
        let mut ordered: Vec<String> = vis_keys.into_iter().map(|(k, _)| k).collect();
        let Some(old_pos) = ordered.iter().position(|k| k == key) else { return };
        let item = ordered.remove(old_pos);
        let target = if visual_idx > old_pos {
            (visual_idx - 1).min(ordered.len())
        } else {
            visual_idx.min(ordered.len())
        };
        ordered.insert(target, item);
        // topbar 从右向左渲染 pinned.iter().rev()，所以把可见区顺序反转后拼回
        let new_visible_pinned: Vec<String> = ordered.into_iter().rev().collect();
        let mut hidden_pinned: Vec<String> = self
            .config
            .topbar
            .pinned_tray_keys
            .iter()
            .filter(|k| !new_visible_pinned.contains(k))
            .cloned()
            .collect();
        let mut merged = new_visible_pinned;
        merged.append(&mut hidden_pinned);
        self.apply_pinned_tray_keys(merged);
    }

    fn apply_pinned_tray_keys(&mut self, keys: Vec<String>) {
        self.config.topbar.pinned_tray_keys = keys.clone();
        let _ = self.store.save_config(&self.config);
        for bar in &mut self.topbars {
            bar.pinned = keys.clone();
            bar.layout(&self.config.topbar);
            if let Some(p) = bar.popover.as_mut() {
                p.pinned = keys.clone();
                p.render();
            }
            invalidate(bar.hwnd);
        }
        if let Some(s) = &self.settings {
            s.invalidate();
        }
    }

    pub fn topbar_appbar_notify(&mut self, hwnd: HWND, code: u32, fullscreen: bool) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        match code {
            // ABN_POSCHANGED (1)：仅当窗口实际偏离显示器顶部时才重新定位，绝不盲目触发 ABM_SETPOS 循环
            1 => {
                let bar = &self.topbars[i];
                let mut wr = RECT::default();
                let _ = unsafe { GetWindowRect(bar.hwnd, &mut wr) };
                if wr.left != bar.monitor.rect.left || wr.top != bar.monitor.rect.top || (wr.bottom - wr.top) != bar.height {
                    self.topbars[i].register_appbar(false);
                }
            }
            // ABN_FULLSCREENAPP (2)：系统通知某应用进入/退出独占全屏
            2 if self.config.topbar.fullscreen_yield => {
                self.topbars[i].set_fullscreen_hidden(fullscreen);
            }
            _ => {}
        }
    }

    /// 系统浅/深主题变化时仅更新外观与重绘，不销毁重建 AppBar，彻底杜绝闪烁与画面抖动。
    pub fn on_theme_changed(&mut self) {
        self.apply_config(false);
    }

    /// 设置页切换「启动时隐藏任务栏」后实时应用生效。
    pub fn sync_taskbar_setting(&mut self) {
        if self.config.general.hide_taskbar_on_start {
            self.taskbar.paused = false;
            self.apply_taskbar_hide();
        } else {
            self.taskbar.restore_all();
        }
    }

    pub fn topbar_dpi_changed(&mut self, hwnd: HWND, lparam: isize) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        let suggested = unsafe { &*(lparam as *const RECT) };
        let w = suggested.right - suggested.left;
        let h = suggested.bottom - suggested.top;
        unsafe {
            let _ = SetWindowPos(hwnd, None, suggested.left, suggested.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        let bar = &mut self.topbars[i];
        bar.dpi = window_dpi(hwnd);
        bar.apply_config(&self.config.topbar, &self.config.appearance);
    }

    pub fn display_changed(&mut self) {
        self.display_dirty_at = Some(Instant::now());
    }

    pub fn topbar_click(&mut self, hwnd: HWND, x: i32, y: i32, right: bool) {
        let Some(i) = self.topbar_index(hwnd) else { return };
        let Some(hit) = self.topbars[i].hit_test(x, y) else { return };
        let anchor_pt = {
            let bar = &self.topbars[i];
            let mut wr = RECT::default();
            let _ = unsafe { GetWindowRect(bar.hwnd, &mut wr) };
            let r = bar.rect_of(&hit).unwrap_or_default();
            (wr.left + r.left, wr.bottom + 2)
        };
        match hit {
            Hit::ProductMenu => self.product_menu_at(anchor_pt.0, anchor_pt.1),
            Hit::AppMenu => {
                match self.foreground_window() {
                    Some(fg) => {
                        let zoomed = unsafe { IsZoomed(fg).as_bool() };
                        self.popup_at(
                            vec![
                                MenuItem::normal(M_FG_MIN, "最小化"),
                                MenuItem::normal(M_FG_MAX, if zoomed { "还原" } else { "最大化" }),
                                MenuItem::normal(M_FG_CENTER, "居中"),
                                MenuItem::sep(),
                                MenuItem::danger(M_FG_CLOSE, "关闭窗口"),
                            ],
                            MenuCtx::Foreground(fg.0 as isize),
                            anchor_pt.0,
                            anchor_pt.1,
                        );
                    }
                    None => self.popup_at(
                        vec![
                            MenuItem::normal(M_NEW_FINDER, "打开文件管理器"),
                            MenuItem::normal(M_SETTINGS, "WinDockFinder设置"),
                        ],
                        MenuCtx::None,
                        anchor_pt.0,
                        anchor_pt.1,
                    ),
                }
            }
            Hit::MenuFile => {
                let mut v = vec![
                    MenuItem::normal(M_NEW_FINDER, "新建文件管理器窗口"),
                    MenuItem::normal(M_OPEN_TERMINAL, "打开终端"),
                ];
                let ctx = if let Some(fg) = self.foreground_window() {
                    v.push(MenuItem::sep());
                    v.push(MenuItem::danger(M_FG_CLOSE, "关闭当前窗口"));
                    MenuCtx::Foreground(fg.0 as isize)
                } else {
                    MenuCtx::None
                };
                self.popup_at(v, ctx, anchor_pt.0, anchor_pt.1);
            }
            Hit::MenuWindow => {
                let (mut v, ctx) = match self.foreground_window() {
                    Some(fg) => {
                        let zoomed = unsafe { IsZoomed(fg).as_bool() };
                        (
                            vec![
                                MenuItem::normal(M_FG_MIN, "最小化"),
                                MenuItem::normal(M_FG_MAX, if zoomed { "还原" } else { "最大化" }),
                                MenuItem::normal(M_FG_CENTER, "居中显示"),
                                MenuItem::sep(),
                                MenuItem::normal(M_FG_LEFT_HALF, "贴靠左半屏"),
                                MenuItem::normal(M_FG_RIGHT_HALF, "贴靠右半屏"),
                                MenuItem::sep(),
                            ],
                            MenuCtx::Foreground(fg.0 as isize),
                        )
                    }
                    None => (Vec::new(), MenuCtx::None),
                };
                v.push(MenuItem::normal(M_SHOW_DESKTOP, "最小化全部窗口（显示桌面）"));
                self.popup_at(v, ctx, anchor_pt.0, anchor_pt.1);
            }
            Hit::MenuGo => {
                self.popup_at(
                    vec![
                        MenuItem::normal(M_OPEN_BASE, "此电脑"),
                        MenuItem::normal(M_OPEN_BASE + 1, "桌面"),
                        MenuItem::normal(M_OPEN_BASE + 2, "下载"),
                        MenuItem::normal(M_OPEN_BASE + 3, "文档"),
                        MenuItem::sep(),
                        MenuItem::normal(M_OPEN_BASE + 4, "回收站"),
                        MenuItem::normal(M_OPEN_BASE + 5, "Windows 设置"),
                    ],
                    MenuCtx::None,
                    anchor_pt.0,
                    anchor_pt.1,
                );
            }
            Hit::TrayToggle => {
                if self.topbars[i].popover.as_ref().is_some_and(|p| p.visible) {
                    self.close_popover();
                } else {
                    self.open_popover(i);
                }
            }
            Hit::TrayIcon(key) => {
                let bar = &self.topbars[i];
                if let Some(item) = bar.tray_items.iter().find(|item| item.key == key).cloned() {
                    let anchor = if right {
                        let Some(anchor) = bar.rect_of(&Hit::TrayIcon(key)).and_then(|icon| tray_anchor(hwnd, icon)) else { return };
                        Some(anchor)
                    } else {
                        None
                    };
                    self.invoke_tray_native(item, anchor);
                }
            }
            Hit::Ime => self.toggle_status_panel(i, PanelKind::Ime, hit),
            Hit::Network => self.toggle_status_panel(i, PanelKind::Network, hit),
            Hit::Brightness => self.toggle_status_panel(i, PanelKind::Brightness, hit),
            Hit::Volume => self.toggle_status_panel(i, PanelKind::Volume, hit),
            Hit::Battery => self.toggle_status_panel(i, PanelKind::Battery, hit),
            Hit::Control => self.toggle_status_panel(i, PanelKind::Control, hit),
            Hit::Clock => self.toggle_status_panel(i, PanelKind::Clock, hit),
        }
    }

    fn set_topbar_active(&mut self, bi: Option<usize>, hit: Option<Hit>) {
        for (idx, bar) in self.topbars.iter_mut().enumerate() {
            let next = if Some(idx) == bi { hit.clone() } else { None };
            if bar.active_hit != next {
                bar.active_hit = next;
                invalidate(bar.hwnd);
            }
        }
    }

    fn toggle_status_panel(&mut self, bi: usize, kind: PanelKind, hit: Hit) {
        if self.status_panel.visible && self.status_panel.kind == kind {
            self.status_panel.close();
            self.set_topbar_active(None, None);
            return;
        }
        self.close_popover();
        self.context_menu.close();
        self.window_preview.close();
        self.set_topbar_active(Some(bi), Some(hit.clone()));
        let (anchor, dpi) = {
            let bar = &self.topbars[bi];
            let mut wr = RECT::default();
            let _ = unsafe { GetWindowRect(bar.hwnd, &mut wr) };
            let r = bar.rect_of(&hit).unwrap_or_default();
            (
                RECT {
                    left: wr.left + r.left,
                    top: wr.top + r.top,
                    right: wr.left + r.right,
                    bottom: wr.top + r.bottom,
                },
                bar.dpi,
            )
        };
        let prev_fg = HWND(self.snap.foreground as *mut _);
        let ptr = app_ptr();
        let main_hwnd = self.hwnd;
        let appearance = self.config.appearance.clone();
        self.status_panel.open(ptr, main_hwnd, kind, anchor, dpi, prev_fg, &appearance);
    }

    pub fn close_panel(&mut self) {
        self.status_panel.close();
        if !self.topbars.iter().any(|b| b.popover.as_ref().is_some_and(|p| p.visible)) {
            self.set_topbar_active(None, None);
        }
    }

    pub fn panel_mouse_move(&mut self, x: i32, y: i32) {
        let act = self.status_panel.on_mouse_move(x, y);
        self.handle_panel_action(act);
    }

    pub fn panel_mouse_leave(&mut self) {
        self.status_panel.on_mouse_leave();
    }

    pub fn panel_lbutton_down(&mut self, x: i32, y: i32) {
        let act = self.status_panel.on_lbutton_down(x, y);
        self.handle_panel_action(act);
    }

    pub fn panel_lbutton_up(&mut self, x: i32, y: i32) {
        let act = self.status_panel.on_lbutton_up(self.hwnd, x, y);
        self.handle_panel_action(act);
    }

    pub fn panel_capture_lost(&mut self) {
        self.close_panel();
    }

    fn on_panel_data(&mut self, lparam: isize) {
        let (token, data) = *unsafe { Box::from_raw(lparam as *mut (u64, NetworkData)) };
        self.status_panel.on_network_data(token, data);
    }

    fn handle_panel_action(&mut self, act: PanelAction) {
        match act {
            PanelAction::None => {}
            PanelAction::OpenSettings => self.open_settings(None),
            PanelAction::LockScreen => self.on_menu(M_LOCK),
            PanelAction::Sleep => self.on_menu(M_SLEEP),
            PanelAction::VolumeChanged(v, m) => {
                for bar in &mut self.topbars {
                    bar.status.volume = Some((v, m));
                    invalidate(bar.hwnd);
                }
            }
            PanelAction::ImeChanged => {
                let cur = status::ime_label(HWND(self.snap.foreground as *mut _));
                self.last_ime = cur.clone();
                for bar in &mut self.topbars {
                    bar.status.ime = cur.clone();
                    bar.layout(&self.config.topbar);
                    invalidate(bar.hwnd);
                }
            }
            PanelAction::ToggleTheme => {
                let next = if self.config.appearance.is_dark() { "light" } else { "dark" };
                self.config.appearance.theme = next.into();
                let _ = self.store.save_config(&self.config);
                self.apply_config(false);
                if self.status_panel.visible {
                    let prev_fg = HWND(self.snap.foreground as *mut _);
                    let dpi = self.topbars.first().map(|b| b.dpi).unwrap_or(96);
                    let anchor = self.topbars.first().and_then(|b| {
                        let mut wr = RECT::default();
                        let _ = unsafe { GetWindowRect(b.hwnd, &mut wr) };
                        b.rect_of(&Hit::Control).map(|r| RECT {
                            left: wr.left + r.left,
                            top: wr.top + r.top,
                            right: wr.left + r.right,
                            bottom: wr.top + r.bottom,
                        })
                    }).unwrap_or_default();
                    let ptr = app_ptr();
                    let main_hwnd = self.hwnd;
                    let appearance = self.config.appearance.clone();
                    self.status_panel.open(ptr, main_hwnd, PanelKind::Control, anchor, dpi, prev_fg, &appearance);
                }
            }
            PanelAction::SwitchKind(kind) => {
                if let Some(bi) = self.topbars.iter().position(|_| true) {
                    let hit = match kind {
                        PanelKind::Network => Hit::Network,
                        PanelKind::Brightness => Hit::Brightness,
                        PanelKind::Volume => Hit::Volume,
                        PanelKind::Battery => Hit::Battery,
                        PanelKind::Ime => Hit::Ime,
                        PanelKind::Clock => Hit::Clock,
                        PanelKind::Control => Hit::Control,
                    };
                    self.toggle_status_panel(bi, kind, hit);
                }
            }
        }
    }

    pub fn paint_preview(&mut self) {
        self.window_preview.paint(&self.icons);
    }

    pub fn preview_mouse_move(&mut self, x: i32, y: i32) {
        self.preview_leave_at = None;
        self.mouse_in_dock = true;
        self.hide_at = None;
        self.window_preview.on_mouse_move(x, y);
    }

    pub fn preview_mouse_leave(&mut self) {
        self.window_preview.on_mouse_leave();
    }

    pub fn preview_lbutton_up(&mut self, x: i32, y: i32) {
        match self.window_preview.on_lbutton_up(x, y) {
            PreviewClick::Activate(h) => {
                self.window_preview.close();
                items::activate_window(HWND(h as *mut _));
                self.sync_edge_timer();
            }
            PreviewClick::Close(h) => {
                self.window_preview.close();
                items::close_window(HWND(h as *mut _));
                self.sync_edge_timer();
            }
            PreviewClick::None => {}
        }
    }

    fn open_popover(&mut self, bi: usize) {
        if let TraySeed::Loading(pending) = &mut self.tray_seed {
            let hwnd = self.topbars[bi].hwnd;
            *pending = (*pending != Some(hwnd)).then_some(hwnd);
            return;
        }
        self.context_menu.close();
        self.status_panel.close();
        self.window_preview.close();
        self.sync_shelf_items();
        let (anchor, items, pinned) = {
            let bar = &self.topbars[bi];
            let mut wr = RECT::default();
            let _ = unsafe { GetWindowRect(bar.hwnd, &mut wr) };
            let r = bar.rect_of(&Hit::TrayToggle).unwrap_or_default();
            (
                RECT {
                    left: wr.left + r.left,
                    top: wr.top + r.top,
                    right: wr.left + r.right,
                    bottom: wr.top + r.bottom,
                },
                bar.tray_items.clone(),
                bar.pinned.clone(),
            )
        };
        let ptr = app_ptr();
        let appearance = self.config.appearance.clone();
        let bar = &mut self.topbars[bi];
        if bar.popover.is_none() {
            match Popover::new(ptr, bar.dpi) {
                Ok(p) => bar.popover = Some(p),
                Err(e) => {
                    crate::log::log(&format!("托盘浮层创建失败: {e}"));
                    return;
                }
            }
        }
        if let Some(p) = bar.popover.as_mut() {
            p.open(anchor, items, pinned, &appearance);
        }
        self.set_topbar_active(Some(bi), Some(Hit::TrayToggle));
        self.refresh_tray();
    }

    pub fn close_popover(&mut self) {
        if self.tray_tip.owner().is_some_and(|owner| self.popover_owner(owner).is_some()) {
            self.tray_tip.clear(self.hwnd);
        }
        self.tray_context = None;
        unsafe { let _ = KillTimer(self.hwnd, TIMER_TRAY_CONTEXT); }
        if let TraySeed::Loading(pending) = &mut self.tray_seed {
            *pending = None;
        }
        let ptr = app_ptr() as isize;
        let mut closed = false;
        for bar in &mut self.topbars {
            if let Some(p) = bar.popover.as_mut() {
                if p.visible {
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, 0);
                    }
                    p.close();
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, ptr);
                    }
                    closed = true;
                }
            }
        }
        if closed && !self.status_panel.visible {
            self.set_topbar_active(None, None);
        }
    }

    /// Explorer 或应用菜单接管焦点时收纳格仍然可见，菜单自行关闭后再收起。
    pub fn popover_deactivated(&mut self, hwnd: HWND, next: HWND) {
        let next = if next.0.is_null() { unsafe { GetForegroundWindow() } } else { next };
        if matches!(self.tray_work, Some(TrayWork::Refresh { reading_overflow: true, .. }))
            && (next.0.is_null()
                || taskbar::primary_taskbar().is_some_and(|tray| taskbar::window_pid(tray) == taskbar::window_pid(next)))
        {
            return;
        }
        if !self.tray_context.as_ref().is_some_and(|ctx| ctx.popover == hwnd) {
            self.close_popover();
        }
    }

    fn track_tray_context(&mut self) {
        if self.tray_context.as_ref().and_then(|ctx| ctx.menu).is_some_and(|menu| !taskbar::is_visible(menu)) {
            self.close_popover();
        }
    }

    pub fn popover_mouse_move(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            p.on_mouse_move(x, y);
        }
        self.update_tray_tip(hwnd, x, y);
    }

    pub fn popover_mouse_leave(&mut self, hwnd: HWND) {
        if self.tray_tip.owner() == Some(hwnd) {
            self.tray_tip.clear(self.hwnd);
        }
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            p.on_mouse_leave();
        }
    }

    pub fn popover_wheel(&mut self, hwnd: HWND, delta: i16) {
        self.tray_tip.clear(self.hwnd);
        let Some(bi) = self.popover_owner(hwnd) else { return };
        let bar = &mut self.topbars[bi];
        if let Some(p) = bar.popover.as_mut() {
            let step = if delta > 0 { -1i32 } else { 1i32 };
            let next = (p.scroll as i32 + step).clamp(0, p.scroll_max() as i32) as usize;
            if next != p.scroll {
                p.press = None;
                p.scroll = next;
                p.render();
            }
        }
    }

    pub fn popover_lbutton_down(&mut self, hwnd: HWND, x: i32, y: i32) {
        self.tray_tip.clear(self.hwnd);
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            p.press = p.index_at(x, y);
            p.render();
        }
    }

    pub fn popover_lbutton_up(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(bi) = self.popover_owner(hwnd) else { return };
        let Some(p) = self.topbars[bi].popover.as_mut() else { return };
        let Some(index) = p.press.take() else { return };
        // 只在原图标格内完成点击时切换固定状态。
        if p.index_at(x, y) == Some(index) {
            let key = p.items[index].key.clone();
            self.toggle_tray_pin(&key);
        } else {
            p.render();
        }
    }

    pub fn popover_rclick(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(bi) = self.popover_owner(hwnd) else { return };
        let Some(p) = self.topbars[bi].popover.as_ref() else { return };
        let Some(index) = p.index_at(x, y) else { return };
        let Some(item) = p.items.get(index).cloned() else { return };
        let Some(anchor) = p.cell_rect(index).and_then(|icon| tray_anchor(hwnd, icon)) else { return };
        let Some(p) = self.topbars[bi].popover.as_mut() else { return };
        p.set_context_key(Some(item.key.clone()));
        self.tray_context = Some(TrayContext { popover: hwnd, menu: None });
        self.invoke_tray(TrayClick { item, anchor: Some(anchor), popover: Some(hwnd) });
    }

    /// 切换某应用/托盘图标「固定到顶部栏 / 收进折叠区」。
    pub fn toggle_tray_pin(&mut self, key: &str) {
        let mut keys = self.config.topbar.pinned_tray_keys.clone();
        if let Some(pos) = keys.iter().position(|k| k == key) {
            keys.remove(pos);
        } else {
            keys.push(key.to_string());
        }
        self.apply_pinned_tray_keys(keys);
    }

}

// ---------- 窗口过程 ----------

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED.load(Ordering::SeqCst);
    match msg {
        WM_APP_IPC => {
            let mut result = LRESULT(0);
            with_app(hwnd, |app| result = app.on_ipc(wparam.0));
            return result;
        }
        WM_CLOSE => {
            with_app(hwnd, |app| app.quit());
            return LRESULT(0);
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                with_app(hwnd, |app| app.shutdown());
            }
            return LRESULT(1);
        }
        _ => {}
    }
    with_app(hwnd, |app| match msg {
        WM_APP_TRAY => app.on_tray(lparam.0 as u32),
        WM_APP_COLOR => app.on_color(lparam.0),
        crate::dock::icons::WM_APP_ICON => app.on_icon(lparam.0),
        crate::monitor::WM_APP_MONITOR => app.on_monitor_event(),
        WM_APP_TASKBAR_RESHOW => app.taskbar.run_rehide(),
        crate::topbar::status::WM_APP_STATUS => {}
        crate::topbar::tray_mirror::WM_APP_TRAY_ITEMS => {
            if lparam.0 != 0 {
                app.on_tray_refresh(*Box::from_raw(lparam.0 as *mut TraySnapshot));
            }
        }
        crate::topbar::tray_mirror::WM_APP_TRAY_SEED => {
            if lparam.0 != 0 {
                app.on_tray_seed(*Box::from_raw(lparam.0 as *mut TraySnapshot));
            }
        }
        crate::topbar::tray_mirror::WM_APP_TRAY_INVOKED => {
            if lparam.0 != 0 {
                app.on_tray_invoked(*Box::from_raw(lparam.0 as *mut TrayInvocation));
            }
        }
        crate::topbar::tray_mirror::WM_APP_LIVE_ICON_CHANGED => app.redraw_shelf_views(),
        crate::topbar::panel::WM_APP_PANEL_DATA => app.on_panel_data(lparam.0),
        crate::topbar::WM_APP_APPBAR => {}
        WM_APP_UPDATE => app.on_update(lparam.0),
        crate::update::WM_APP_UPDATE_PROGRESS => app.on_update_progress(wparam.0),
        WM_HOTKEY => app.on_hotkey(wparam.0 as i32),
        WM_TIMER => app.on_timer(wparam.0),
        WM_DISPLAYCHANGE => {
            app.display_dirty_at = Some(Instant::now());
        }
        WM_SETTINGCHANGE => {
            // 仅在系统主题切换时刷新配色，严格忽略 SPI_SETWORKAREA(0x002F) 防止 AppBar 死循环闪烁
            if crate::topbar::is_theme_setting_change(lparam.0) {
                app.on_theme_changed();
            }
        }
        WM_DPICHANGED => app.display_dirty_at = Some(Instant::now()),
        _ if msg == taskbar_created && taskbar_created != 0 => app.on_taskbar_created(),
        _ => {}
    });
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
