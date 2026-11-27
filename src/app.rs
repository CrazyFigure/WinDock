//! 主实例：单实例锁、IPC 转发、全局状态机与生命周期协调。
//!
//! 生命周期顺序（安全前置门禁）：创建双栏窗口 → 守护进程握手 → 落盘恢复记录 → 隐藏任务栏。
//! 退出顺序：撤销 AppBar → 恢复任务栏 → 通知守护进程 → 清除恢复标记。

use std::cell::{Cell, RefCell};
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_SHIFT, VK_DOWN, VK_ESCAPE, VK_LWIN, VK_RETURN, VK_RWIN, VK_UP,
};
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
use crate::topbar::tray_popup::{CancelSignal, CoverContext, WM_APP_TRAY_COVER_ZORDER};
use crate::topbar::tray_tooltip::{TipTarget, TrayTooltip, TIMER_TRAY_TIP};
use crate::topbar::{Hit, TopBar};
use crate::tray::Tray;
use crate::ui::DibTarget;
use crate::win32::{all_monitors, class_name, monitor_at_cursor, wide, wide_static, window_dpi, MonitorInfo};

pub const CLASS_MAIN: &str = "WinDockMain";
pub const MUTEX_NAME: &str = "Local\\WinDock-SingleInstance";
pub const WM_APP_IPC: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;
pub const WM_APP_COLOR: u32 = WM_APP + 32;
pub const WM_APP_DOCK_COLOR: u32 = WM_APP + 33;
pub const WM_APP_UPDATE: u32 = WM_APP + 50;
pub const WM_APP_OVERLAY_DISMISS: u32 = WM_APP + 60;
pub const WM_APP_OVERLAY_WHEEL: u32 = WM_APP + 61;
pub const WM_APP_OVERLAY_KEY: u32 = WM_APP + 62;
pub const WM_APP_PANEL_DRAG_MOVE: u32 = WM_APP + 63;
pub const WM_APP_PANEL_DRAG_UP: u32 = WM_APP + 64;

pub const IPC_SETTINGS: usize = 1;
pub const IPC_TOGGLE_DOCK: usize = 2;
pub const IPC_TOGGLE_TOPBAR: usize = 3;
pub const IPC_RESTORE_TASKBAR: usize = 4;

const TIMER_FRAME: usize = 1;
const TIMER_SLOW: usize = 2;
const TIMER_EDGE: usize = 3;
const TIMER_TRAY_CONTEXT: usize = 4;
const TIMER_TRAY_FLASH: usize = 6;
/// Win+D 切换桌面或窗口还原动画结束后的状态复查定时器。
const TIMER_MONITOR_SETTLE: usize = 7;
/// 最大化窗口取色二次校验定时器（覆盖还原动画滞后导致首次采到壁纸残影的场景）。
const TIMER_COLOR_RECHECK: usize = 8;
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
const M_RESTART_APP: usize = 14;
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
const M_OPEN_FILE_MANAGER: usize = 307;
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

/// 在已持有 `&mut App` 的自定义窗口过程中执行回调，按嵌套深度保存并恢复标志，防止重入修改 `App`。
pub fn hold_app<R>(f: impl FnOnce() -> R) -> R {
    struct DepthGuard(u32);
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            HANDLER_DEPTH.with(|d| d.set(self.0));
        }
    }
    let prev = HANDLER_DEPTH.with(|d| {
        let p = d.get();
        d.set(p + 1);
        p
    });
    let _guard = DepthGuard(prev);
    f()
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
    matches!(
        class,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Shell_IconView"
            | "#32768"
            | "tooltips_class32"
            | "Windows.UI.Input.InputSite.WindowClass"
            | "Windows.UI.Composition.DesktopWindowContentBridge"
    ) || class.starts_with("NotifyIconOverflowWindow")
        || class.starts_with("TopLevelWindowForOverflowXamlIsland")
        || class.starts_with("Xaml_WindowedPopupClass")
        || class.contains("QWindowPopup")
        // 仅过滤内部壳层与浮窗；设置窗口作为独立应用窗口允许前台菜单与窗口管理操作
        || (class.starts_with("WinDock") && class != crate::settings::CLASS_SETTINGS)
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
    owner: HWND,
    is_popover: bool,
}

enum TrayWork {
    // 图标采样中的用户点击排在其后，避免两个后台线程同时操作同一个原生托盘。
    Refresh { cancel: Arc<CancelSignal>, reading_overflow: bool, queued_click: Option<TrayClick> },
    Click { right: bool, screen: POINT, owner: HWND, is_popover: bool, queued_click: Option<TrayClick> },
}

/// 判断光标是否正处于指定显示器底部任务栏与 Dock 交互热区（用于延后后台托盘遮罩采样）。
fn cursor_in_bottom_taskbar_zone(pt: POINT, mon: MonitorInfo) -> bool {
    let band = crate::win32::dp(96, mon.dpi);
    pt.x >= mon.rect.left
        && pt.x < mon.rect.right
        && pt.y >= mon.rect.bottom - band
        && pt.y <= mon.rect.bottom
}

struct TrayContext {
    /// 发起右键菜单的顶部栏或收纳浮层窗口
    owner: HWND,
    /// 发起右键菜单的图标键，用于仅屏蔽该图标自身的悬浮提示
    key: String,
    /// 是否由收纳浮层发起（菜单关闭后需联动收起收纳浮层）
    is_popover: bool,
    /// 已捕获到的原生或自定义右键菜单窗口句柄
    menu: Option<HWND>,
}

/// 跟踪顶部栏托盘右键菜单及其对应图标的悬浮预览抑制状态。
#[derive(Default)]
struct TrayTipGate {
    context: Option<TrayContext>,
    /// 未捕获菜单窗口句柄时的兜底抑制 `(宿主窗口, 图标键)`，鼠标离开原图标即解除
    suppressed: Option<(HWND, String)>,
}

impl TrayTipGate {
    fn has_active_menu(&self) -> bool {
        self.context.is_some()
    }

    fn is_popover_owner(&self, hwnd: HWND) -> bool {
        self.context
            .as_ref()
            .is_some_and(|ctx| ctx.is_popover && ctx.owner == hwnd)
    }

    /// 仅抑制当前右键菜单所属的 `(宿主窗口, 图标键)`，移动到其他图标时照常显示预览。
    fn is_suppressed(&self, owner: HWND, key: &str) -> bool {
        self.context
            .as_ref()
            .is_some_and(|ctx| ctx.owner == owner && ctx.key == key)
            || self
                .suppressed
                .as_ref()
                .is_some_and(|(sup_owner, sup_key)| *sup_owner == owner && sup_key == key)
    }

    fn clear_suppressed(&mut self) {
        self.suppressed = None;
    }

    /// 鼠标移出兜底抑制的图标（进入其他图标、空白区或另一窗口）后解除抑制。
    fn update_hover(&mut self, owner: HWND, hovered_key: Option<&str>) {
        if self
            .suppressed
            .as_ref()
            .is_some_and(|(sup_owner, sup_key)| *sup_owner != owner || hovered_key != Some(sup_key.as_str()))
        {
            self.suppressed = None;
        }
    }
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

/// 非激活顶部栏浮层（菜单/收纳格/状态面板）的底层输入钩子共享快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OverlayHookState {
    main_hwnd: isize,
    generation: usize,
    overlay_rects: Vec<RECT>,
    topbar_toggle_rects: Vec<RECT>,
    popover_rect: Option<RECT>,
    context_menu_open: bool,
    context_menu_has_hover: bool,
    panel_dragging_slider: bool,
    panel_win_rect: Option<RECT>,
    tray_menu_active: bool,
}

impl OverlayHookState {
    fn has_active_overlay(&self) -> bool {
        self.context_menu_open
            || self.popover_rect.is_some()
            || self.panel_dragging_slider
            || !self.overlay_rects.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseHookDecision {
    PassThrough,
    DismissOverlay,
    ForwardPanelDragMove,
    ForwardPanelDragUp,
    ConsumePopoverWheel(i16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyHookDecision {
    PassThrough,
    ConsumeAndDispatch(u32),
    PassAndDismiss,
}

/// 判断顶部栏命中区域是否为展开/切换浮层或下拉菜单的按钮（点击时交由 TopBar 自身切换逻辑）。
fn is_topbar_overlay_toggle_hit(hit: &Hit) -> bool {
    matches!(
        hit,
        Hit::ProductMenu
            | Hit::AppMenu
            | Hit::MenuFile
            | Hit::MenuWindow
            | Hit::MenuGo
            | Hit::TrayToggle
            | Hit::Ime
            | Hit::Network
            | Hit::Brightness
            | Hit::Volume
            | Hit::Battery
            | Hit::Control
            | Hit::Clock
    )
}

/// 将屏幕坐标 `(x, y)` 打包为 `LPARAM`（支持多屏负坐标）。
#[inline]
fn pack_point_lparam(x: i32, y: i32) -> isize {
    let lx = (x.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16) as usize;
    let ly = (y.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16) as usize;
    (lx | (ly << 16)) as isize
}

/// 评估底层鼠标事件在非激活浮层开启时的路由决策（纯逻辑，便于单测覆盖全部边界）。
fn eval_mouse_ll(state: &OverlayHookState, msg: u32, pt: POINT, wheel_delta: i16) -> MouseHookDecision {
    if !state.has_active_overlay() {
        return MouseHookDecision::PassThrough;
    }
    // 拖拽亮度/音量滑块期间：非激活窗口的 SetCapture 无法接收窗外事件，通过钩子补传移动与抬起
    if state.panel_dragging_slider {
        match msg {
            WM_MOUSEMOVE => {
                if state.panel_win_rect.is_none_or(|r| !crate::win32::rect_contains(r, pt.x, pt.y)) {
                    return MouseHookDecision::ForwardPanelDragMove;
                }
                return MouseHookDecision::PassThrough;
            }
            WM_LBUTTONUP => return MouseHookDecision::ForwardPanelDragUp,
            WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => return MouseHookDecision::PassThrough,
            _ => {}
        }
    }
    if msg == WM_MOUSEWHEEL {
        if !state.tray_menu_active
            && state.popover_rect.is_some_and(|r| crate::win32::rect_contains(r, pt.x, pt.y))
        {
            return MouseHookDecision::ConsumePopoverWheel(wheel_delta);
        }
        return MouseHookDecision::PassThrough;
    }
    if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN) {
        if state.tray_menu_active {
            return MouseHookDecision::PassThrough;
        }
        if state.overlay_rects.iter().any(|&r| crate::win32::rect_contains(r, pt.x, pt.y)) {
            return MouseHookDecision::PassThrough;
        }
        if state.topbar_toggle_rects.iter().any(|&r| crate::win32::rect_contains(r, pt.x, pt.y)) {
            return MouseHookDecision::PassThrough;
        }
        return MouseHookDecision::DismissOverlay;
    }
    MouseHookDecision::PassThrough
}

/// 评估底层键盘事件在非激活浮层开启时的路由决策（Esc 关闭、菜单方向/回车导航、Win 键自动收起）。
fn eval_keyboard_ll(state: &OverlayHookState, msg: u32, vk: u32, alt_down: bool) -> KeyHookDecision {
    if !state.has_active_overlay() || state.tray_menu_active {
        return KeyHookDecision::PassThrough;
    }
    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return KeyHookDecision::PassThrough;
    }
    if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
        return KeyHookDecision::PassAndDismiss;
    }
    if msg != WM_KEYDOWN || alt_down {
        return KeyHookDecision::PassThrough;
    }
    if vk == VK_ESCAPE.0 as u32 {
        return KeyHookDecision::ConsumeAndDispatch(vk);
    }
    if state.context_menu_open
        && (vk == VK_UP.0 as u32 || vk == VK_DOWN.0 as u32 || vk == VK_RETURN.0 as u32)
    {
        return KeyHookDecision::ConsumeAndDispatch(vk);
    }
    KeyHookDecision::PassThrough
}

#[derive(Default)]
struct OverlayHooks {
    mouse: HHOOK,
    keyboard: HHOOK,
    state: OverlayHookState,
}

thread_local! {
    // 底层钩子在主消息线程安装并回调，使用线程局部存储避免跨线程锁竞争
    static OVERLAY_HOOKS: RefCell<OverlayHooks> = RefCell::new(OverlayHooks::default());
}

/// 卸载当前线程安装的非激活浮层输入钩子并清空共享状态。
fn uninstall_overlay_hooks() {
    OVERLAY_HOOKS.with(|slot| {
        if let Ok(mut hooks) = slot.try_borrow_mut() {
            unsafe {
                if !hooks.mouse.is_invalid() {
                    let _ = UnhookWindowsHookEx(hooks.mouse);
                    hooks.mouse = HHOOK::default();
                }
                if !hooks.keyboard.is_invalid() {
                    let _ = UnhookWindowsHookEx(hooks.keyboard);
                    hooks.keyboard = HHOOK::default();
                }
            }
            hooks.state = OverlayHookState::default();
        }
    });
}

unsafe extern "system" fn overlay_mouse_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let wheel_delta = crate::win32::hiword(info.mouseData as usize) as i16;
        let picked = OVERLAY_HOOKS.with(|slot| {
            slot.try_borrow().ok().map(|hooks| {
                (
                    hooks.state.main_hwnd,
                    hooks.state.generation,
                    eval_mouse_ll(&hooks.state, msg, info.pt, wheel_delta),
                )
            })
        });
        if let Some((main_hwnd, generation, decision)) = picked {
            if main_hwnd != 0 {
                let target = HWND(main_hwnd as *mut _);
                match decision {
                    MouseHookDecision::PassThrough => {}
                    MouseHookDecision::DismissOverlay => {
                        let _ = PostMessageW(target, WM_APP_OVERLAY_DISMISS, WPARAM(generation), LPARAM(0));
                    }
                    MouseHookDecision::ForwardPanelDragMove => {
                        let lp = pack_point_lparam(info.pt.x, info.pt.y);
                        let _ = PostMessageW(target, WM_APP_PANEL_DRAG_MOVE, WPARAM(generation), LPARAM(lp));
                    }
                    MouseHookDecision::ForwardPanelDragUp => {
                        let lp = pack_point_lparam(info.pt.x, info.pt.y);
                        let _ = PostMessageW(target, WM_APP_PANEL_DRAG_UP, WPARAM(generation), LPARAM(lp));
                    }
                    MouseHookDecision::ConsumePopoverWheel(delta) => {
                        let _ = PostMessageW(target, WM_APP_OVERLAY_WHEEL, WPARAM(generation), LPARAM(delta as isize));
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn overlay_keyboard_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let alt_down = (info.flags.0 & LLKHF_ALTDOWN.0) != 0;
        let picked = OVERLAY_HOOKS.with(|slot| {
            slot.try_borrow().ok().map(|hooks| {
                (
                    hooks.state.main_hwnd,
                    hooks.state.generation,
                    eval_keyboard_ll(&hooks.state, msg, info.vkCode, alt_down),
                )
            })
        });
        if let Some((main_hwnd, generation, decision)) = picked {
            if main_hwnd != 0 {
                let target = HWND(main_hwnd as *mut _);
                match decision {
                    KeyHookDecision::PassThrough => {}
                    KeyHookDecision::PassAndDismiss => {
                        let _ = PostMessageW(target, WM_APP_OVERLAY_DISMISS, WPARAM(generation), LPARAM(0));
                    }
                    KeyHookDecision::ConsumeAndDispatch(vk) => {
                        let _ = PostMessageW(target, WM_APP_OVERLAY_KEY, WPARAM(generation), LPARAM(vk as isize));
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
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
    pub topbar_menu: Option<(usize, Hit)>,
    pub status_panel_bar: Option<(usize, Hit)>,
    pub overlay_generation: usize,
    pub overlay_fg: Option<isize>,
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
    tray_gate: TrayTipGate,
    tray_tip: TrayTooltip,
    pub temp_show_until: Option<Instant>,
    pub display_dirty_at: Option<Instant>,
    pub cursor_monitor: isize,
    // 当前正在后台采样的 (显示器句柄, 最大化窗口句柄)
    pub color_sampling: Option<(isize, isize)>,
    pub color_token: u64,
    pub color_busy: bool,
    pub dock_color_token: u64,
    pub dock_color_busy: bool,
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
        CreateWindowExW(WS_EX_TOOLWINDOW, wide_static(CLASS_MAIN), w!("WinDock"), WS_POPUP, 0, 0, 0, 0, None, None, hinst, None)
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
        topbar_menu: None,
        status_panel_bar: None,
        overlay_generation: 0,
        overlay_fg: None,
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
        tray_gate: TrayTipGate::default(),
        tray_tip: TrayTooltip::default(),
        temp_show_until: None,
        display_dirty_at: None,
        cursor_monitor: 0,
        color_sampling: None,
        color_token: 0,
        color_busy: false,
        dock_color_token: 0,
        dock_color_busy: false,
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
                    if unsafe { FindWindowW(w!("WinDockGuard"), None) }.is_ok() {
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
            let _ = KillTimer(self.hwnd, TIMER_MONITOR_SETTLE);
            let _ = KillTimer(self.hwnd, TIMER_COLOR_RECHECK);
        }
        self.frame_on = false;
        self.edge_timer_on = false;
        uninstall_overlay_hooks();
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

    /// 构造重启协调子进程的命令行参数（等待当前主进程与守护进程退出后再拉起新实例）。
    fn restart_wait_args(pid: u32, guard_pid: u32) -> Vec<String> {
        let mut args = vec![
            "--restart-wait".to_string(),
            "--parent-pid".to_string(),
            pid.to_string(),
        ];
        if guard_pid != 0 {
            args.push("--guard-pid".to_string());
            args.push(guard_pid.to_string());
        }
        args
    }

    /// 重启 WinDock：先拉起等待子进程再正常退出清理，确保新实例接管前已释放单实例锁与恢复任务栏。
    pub fn restart(&mut self) {
        if self.exiting {
            return;
        }
        let exe = crate::win32::exe_path();
        let pid = std::process::id();
        let guard_pid = self
            .guard_handle
            .map(|h| unsafe { windows::Win32::System::Threading::GetProcessId(h) })
            .unwrap_or(0);
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(Self::restart_wait_args(pid, guard_pid));
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        cmd.creation_flags(CREATE_NO_WINDOW);
        match cmd.spawn() {
            Ok(_) => {
                crate::log::log("用户触发重启 WinDock");
                self.quit();
            }
            Err(e) => {
                crate::log::log(&format!("重启 WinDock 启动新实例失败: {e}"));
                self.notify_tray("重启 WinDock 失败，请手动重新打开。");
            }
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
        self.close_popover_immediate();
        self.clear_tray_context();
        self.tray_gate.clear_suppressed();
        self.tray_tip.clear(self.hwnd);
        self.close_context_menu_immediate();
        self.close_panel_immediate();
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
            for m in mons {
                if let Ok(mut bar) = TopBar::new(app_ptr(), m, &self.config.topbar, &self.config.appearance) {
                    bar.pinned = self.config.topbar.pinned_tray_keys.clone();
                    bar.active_app = label.clone();
                    bar.dark_text = bar.desktop_dark_text;
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
                // 仅在文件管理器未固定时自动紧随启动台，避免覆盖用户自定义的固定排序
                if real_pos != 1 && list.apps[real_pos].pinned.is_none() {
                    let exp = list.apps.remove(real_pos);
                    list.apps.insert(1, exp);
                }
            }
        }
        // 在用户自定义文件夹之后追加内置功能区（回收站与最右侧的 WinDock 设置）。
        Self::append_builtin_right_entries(&mut list.right, cfg.show_trash);
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

    /// 追加 Dock 右侧内置功能条目，并将 WinDock 设置固定在最右侧。
    fn append_builtin_right_entries(right: &mut Vec<items::FuncEntry>, show_trash: bool) {
        if show_trash {
            right.push(items::FuncEntry {
                kind: FuncKind::Trash,
                label: "回收站".into(),
                icon_key: items::trash_icon_key().into(),
            });
        }
        // 使用明确的产品名称，避免在 Dock Tooltip 中与 Windows 系统“设置”混淆。
        right.push(items::FuncEntry {
            kind: FuncKind::Settings,
            label: "WinDock 设置".into(),
            icon_key: items::settings_icon_key().into(),
        });
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

    /// 判断当前是否有顶部栏菜单、状态面板、收纳浮层或托盘提示遮盖顶部采样区。
    fn has_topbar_overlay(&self) -> bool {
        self.context_menu.visible
            || self.status_panel.visible
            || self.tray_gate.has_active_menu()
            || self.tray_tip.is_visible()
            || self.topbars.iter().any(|b| b.popover.as_ref().is_some_and(|p| p.visible))
    }

    /// 计算指定显示器顶部栏应保持或进入融合取色的目标最大化窗口句柄。
    fn bar_fusion_target(snap: &MonitorState, hmon: isize, cfg: &crate::config::TopbarCfg) -> Option<isize> {
        if !cfg.adaptive_maximized_color || snap.fullscreen {
            return None;
        }
        snap.maximized_on(hmon)
            .filter(|w| !color_sync::is_excluded(&w.exe, &cfg.adaptive_color_exclude))
            .map(|w| w.hwnd)
    }

    fn on_state_changed(&mut self) {
        let label = self.active_app_label();
        let fullscreen = self.snap.fullscreen;
        if fullscreen {
            self.close_popover_immediate();
            self.close_context_menu_immediate();
            self.close_panel_immediate();
        }
        let cfg = &self.config.topbar;
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

        // 最大化窗口自动取色融合（按显示器独立跟踪最上层可见最大化窗口，点击顶部栏或切出浮窗不失真）
        let cfg = &self.config.topbar;
        let mut need_sample = false;
        let mut any_glass_changed = false;
        let mut invalidate_sampling = false;
        for bar in &mut self.topbars {
            let hmon = bar.monitor.hmonitor.0 as isize;
            if let Some(target_hwnd) = Self::bar_fusion_target(&self.snap, hmon, cfg) {
                if bar.fused_hwnd != target_hwnd || bar.mode != crate::topbar::ColorMode::Fused {
                    bar.fused_hwnd = target_hwnd;
                    bar.color_dirty = true;
                }
                if bar.color_dirty {
                    need_sample = true;
                }
            } else {
                let had_fused = bar.fused_hwnd != 0 || bar.mode == crate::topbar::ColorMode::Fused;
                bar.fused_hwnd = 0;
                bar.color_dirty = false;
                if self.color_sampling.is_some_and(|(m, _)| m == hmon) {
                    self.color_sampling = None;
                    invalidate_sampling = true;
                }
                let base_rgb = bar.desktop_base;
                let desktop_dark = bar.desktop_dark_text;
                bar.set_fused_target(None, base_rgb, true);
                bar.dark_text = desktop_dark;
                invalidate(bar.hwnd);
                if had_fused {
                    any_glass_changed = true;
                }
            }
        }
        if invalidate_sampling {
            self.color_token += 1;
        }
        let any_fused = self.topbars.iter().any(|b| b.fused_hwnd != 0);
        if !any_fused {
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_COLOR_RECHECK);
            }
        } else if need_sample {
            if !self.has_topbar_overlay() {
                self.sample_next_dirty_topbar(true);
            }
            // 延迟 280ms 复查一次标题栏颜色，避免 Win+D 还原动画期间首次采样采到壁纸
            unsafe {
                let _ = SetTimer(self.hwnd, TIMER_COLOR_RECHECK, 280, None);
            }
        }
        if any_glass_changed {
            self.ensure_frame();
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
            self.sync_dock_backdrops();
        }
    }

    /// 协调 Dock 背后配色与液态玻璃条带采样：优先跟随背后窗口色，其次 Windows 系统主题，最后降级壁纸。
    fn sync_dock_backdrops(&mut self) {
        let appearance = self.config.appearance.clone();
        let is_liquid = appearance.dock_material == "liquid_glass";
        if (appearance.theme != "system" && !is_liquid) || self.snap.fullscreen {
            self.dock_color_token += 1;
            let mut dirty = false;
            for d in &mut self.docks {
                d.apply_backdrop_color(None, &appearance, true);
                if !is_liquid {
                    d.apply_glass_strip(None, &appearance);
                }
                dirty |= d.dirty;
            }
            if dirty {
                self.ensure_frame();
            }
            return;
        }
        for d in &mut self.docks {
            d.backdrop_dirty = true;
            if is_liquid {
                d.glass_dirty = true;
            }
        }
        self.spawn_dock_backdrop_sample();
    }

    /// 为下一个已显示且待更新背后配色/液态玻璃背景的 Dock 异步拉起采样线程（隐藏中将直接在显出首帧零遮挡采样）。
    fn spawn_dock_backdrop_sample(&mut self) {
        let is_liquid = self.config.appearance.dock_material == "liquid_glass";
        if (self.config.appearance.theme != "system" && !is_liquid)
            || self.snap.fullscreen
            || self.dock_color_busy
        {
            return;
        }
        let target = self
            .docks
            .iter()
            .find(|d| (d.backdrop_dirty || (is_liquid && d.glass_dirty)) && d.window_visible && d.want_visible)
            .map(|d| {
                (
                    d.monitor.hmonitor.0 as isize,
                    d.glass_zone_rect(),
                    d.capsule_screen_rect(),
                    d.win_rect,
                    d.monitor.rect,
                    d.glass_above_y_hint(),
                    d.glass_flank_x_bounds(),
                )
            });
        let Some((hmon, zone_rect, cap_screen, win_rect, mon_rect, above_y_hint, flank_bounds)) = target else {
            return;
        };
        self.dock_color_busy = true;
        self.dock_color_token += 1;
        let token = self.dock_color_token;
        let main = self.hwnd.0 as isize;
        let spawned = std::thread::Builder::new()
            .name("dock-color".into())
            .spawn(move || {
                let (rgb, glass) = color_sync::sample_dock_backdrop_bundle(
                    zone_rect,
                    cap_screen,
                    win_rect,
                    mon_rect,
                    true,
                    is_liquid,
                    above_y_hint,
                    flank_bounds,
                );
                let payload = Box::new((token, hmon, rgb, glass));
                unsafe {
                    let _ = PostMessageW(
                        HWND(main as *mut _),
                        WM_APP_DOCK_COLOR,
                        WPARAM(0),
                        LPARAM(Box::into_raw(payload) as isize),
                    );
                }
            });
        if spawned.is_err() {
            self.dock_color_busy = false;
        }
    }

    fn on_dock_color(&mut self, lparam: isize) {
        self.dock_color_busy = false;
        let (token, hmon, rgb, glass) =
            *unsafe { Box::from_raw(lparam as *mut (u64, isize, Option<Rgb>, Option<color_sync::DockGlassStrip>)) };
        let is_liquid = self.config.appearance.dock_material == "liquid_glass";
        if token != self.dock_color_token || (self.config.appearance.theme != "system" && !is_liquid) {
            return;
        }
        let appearance = self.config.appearance.clone();
        let instant = self.config.topbar.color_transition_ms == 0 || appearance.reduce_motion;
        let mut need_frame = false;
        for d in &mut self.docks {
            if d.monitor.hmonitor.0 as isize == hmon {
                d.apply_backdrop_color(rgb, &appearance, instant);
                d.apply_glass_strip(glass.clone(), &appearance);
                need_frame |= d.dirty;
            }
        }
        if need_frame {
            self.ensure_frame();
        }
        self.spawn_dock_backdrop_sample();
    }

    /// 浮窗/菜单关闭后若仍无其它遮罩且存在待取色顶部栏，立即补采一次颜色。
    fn flush_pending_color_samples(&mut self) {
        if !self.has_topbar_overlay()
            && self.topbars.iter().any(|b| b.fused_hwnd != 0 && b.color_dirty)
        {
            self.sample_next_dirty_topbar(false);
        }
    }

    /// 无遮罩时将已进入融合态的顶部栏标记为待复查并触发新一轮采样。
    fn schedule_fused_color_recheck(&mut self) {
        if !self.config.topbar.adaptive_maximized_color || self.snap.fullscreen {
            return;
        }
        let mut any = false;
        for bar in &mut self.topbars {
            if bar.fused_hwnd != 0 {
                bar.color_dirty = true;
                any = true;
            }
        }
        if any && !self.has_topbar_overlay() {
            self.sample_next_dirty_topbar(false);
        }
    }

    /// 依次为下一个待取色/待复查的融合顶部栏启动后台采样；目标变更时递增值以废止旧结果。
    fn sample_next_dirty_topbar(&mut self, initial_delay: bool) {
        if !self.config.topbar.adaptive_maximized_color || self.snap.fullscreen {
            return;
        }
        let next = self
            .topbars
            .iter()
            .find(|b| b.fused_hwnd != 0 && b.color_dirty)
            .map(|b| (b.monitor.hmonitor.0 as isize, b.fused_hwnd, b.monitor.rect, b.height));
        let Some((hmon, target, mon_rect, bar_h)) = next else {
            self.color_sampling = None;
            return;
        };
        if self.color_busy {
            // 若目标窗口在采样中途发生变化，立即推进 token 使旧线程返回结果自然作废
            if self.color_sampling != Some((hmon, target)) {
                self.color_token += 1;
            }
            return;
        }
        self.color_busy = true;
        self.color_sampling = Some((hmon, target));
        self.color_token += 1;
        let token = self.color_token;
        let main = self.hwnd.0 as isize;
        let spawned = std::thread::Builder::new()
            .name("caption-color".into())
            .spawn(move || {
                if initial_delay {
                    std::thread::sleep(Duration::from_millis(60));
                }
                let rgb = color_sync::caption_color(HWND(target as *mut _), Some(mon_rect), bar_h);
                let payload = Box::new((token, hmon, target, rgb));
                unsafe {
                    let _ = PostMessageW(
                        HWND(main as *mut _),
                        WM_APP_COLOR,
                        WPARAM(0),
                        LPARAM(Box::into_raw(payload) as isize),
                    );
                }
            });
        if spawned.is_err() {
            self.color_busy = false;
            self.color_sampling = None;
        }
    }

    fn on_color(&mut self, lparam: isize) {
        self.color_busy = false;
        self.color_sampling = None;
        let (token, hmon, target, rgb) = *unsafe { Box::from_raw(lparam as *mut (u64, isize, isize, Rgb)) };
        if token != self.color_token {
            if !self.has_topbar_overlay() {
                self.sample_next_dirty_topbar(false);
            }
            return;
        }
        let instant = self.config.topbar.color_transition_ms == 0 || self.config.appearance.reduce_motion;
        let mut applied = false;
        for bar in &mut self.topbars {
            if bar.monitor.hmonitor.0 as isize == hmon && bar.fused_hwnd == target && target != 0 {
                bar.color_dirty = false;
                let base_rgb = bar.desktop_base;
                bar.set_fused_target(Some(rgb), base_rgb, instant);
                invalidate(bar.hwnd);
                applied = true;
            }
        }
        if applied {
            self.ensure_frame();
        }
        if !self.has_topbar_overlay() {
            self.sample_next_dirty_topbar(false);
        }
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

    /// 收集需要在原生任务栏遮罩截屏时排除、且显示期间始终保持在遮罩上方的底部浮窗句柄。
    fn tray_cover_context(&self) -> CoverContext {
        let mut keep_above = Vec::with_capacity(self.docks.len() * 2 + 3);
        for d in &self.docks {
            if !d.hwnd_overlay.0.is_null() {
                keep_above.push(d.hwnd_overlay.0 as isize);
            }
            if !d.hwnd_trigger.0.is_null() {
                keep_above.push(d.hwnd_trigger.0 as isize);
            }
        }
        if !self.window_preview.hwnd.0.is_null() {
            keep_above.push(self.window_preview.hwnd.0 as isize);
        }
        if !self.context_menu.hwnd.0.is_null() {
            keep_above.push(self.context_menu.hwnd.0 as isize);
        }
        if !self.status_panel.hwnd.0.is_null() {
            keep_above.push(self.status_panel.hwnd.0 as isize);
        }
        CoverContext {
            main_hwnd: self.hwnd.0 as isize,
            keep_above,
        }
    }

    /// 托盘遮罩线程发现自身升到 Dock / 预览窗前方时，由主线程按精确层级立即把底部浮窗置顶。
    fn raise_dock_above_tray_cover(&self) {
        unsafe {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            for d in &self.docks {
                if d.window_visible && !d.hwnd_overlay.0.is_null() {
                    let _ = SetWindowPos(d.hwnd_overlay, HWND_TOPMOST, 0, 0, 0, 0, flags);
                }
                if d.trigger_visible && !d.hwnd_trigger.0.is_null() {
                    let _ = SetWindowPos(d.hwnd_trigger, HWND_TOPMOST, 0, 0, 0, 0, flags);
                }
            }
            if self.window_preview.visible && !self.window_preview.hwnd.0.is_null() {
                let _ = SetWindowPos(self.window_preview.hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            }
            if self.context_menu.visible && !self.context_menu.hwnd.0.is_null() {
                let _ = SetWindowPos(self.context_menu.hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            }
            if self.status_panel.visible && !self.status_panel.hwnd.0.is_null() {
                let _ = SetWindowPos(self.status_panel.hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            }
        }
    }

    /// 立即取消进行中的非溢出后台托盘采样，让遮罩与原生任务栏毫秒级退出。
    fn cancel_background_tray_refresh(&self) {
        if let Some(TrayWork::Refresh { cancel, reading_overflow: false, .. }) = &self.tray_work {
            cancel.cancel();
        }
    }

    /// 判断当前是否处于 Dock 活跃交互或屏幕底部任务栏热区，避免此时触发后台托盘遮罩采样。
    fn dock_busy_for_tray_reveal(&self) -> bool {
        if self.mouse_in_dock || self.edge_since.is_some() || self.window_preview.visible {
            return true;
        }
        if self.frame_on && self.docks.iter().any(Dock::animating) {
            return true;
        }
        let hide_mode_non_always = self.config.dock.hide_mode != "always";
        if self.docks.iter().any(|d| {
            d.mouse_in
                || d.hover.is_some()
                || d.drag.is_some()
                || d.drop_settle
                || d.bounce.is_some()
                || (hide_mode_non_always && (d.window_visible || d.want_visible))
        }) {
            return true;
        }
        let pt = cursor_pos();
        if self.docks.iter().any(|d| cursor_in_bottom_taskbar_zone(pt, d.monitor)) {
            return true;
        }
        if let Some(primary) = crate::win32::primary_monitor() {
            if cursor_in_bottom_taskbar_zone(pt, primary) {
                return true;
            }
        }
        false
    }

    pub fn refresh_tray(&mut self) {
        if self.topbars.is_empty() || self.tray_work.is_some() || self.snap.fullscreen
            || self.tray_gate.has_active_menu() || matches!(self.tray_seed, TraySeed::Loading(_))
        {
            return;
        }
        let peek = self.taskbar.begin_tray_peek();
        let cancel = Arc::new(CancelSignal::new());
        let reading_overflow = self.tray_details_pending;
        let pinned = self.topbars.iter().flat_map(|bar| bar.tray_items.iter().filter_map(|item| {
            bar.pinned.contains(&item.key).then(|| item.uia_key.clone()).flatten()
        })).collect();
        let cover = self.tray_cover_context();
        let request = TrayRefreshRequest { peek, cancel: cancel.clone(), overflow: reading_overflow, pinned, cover };
        if tray_mirror::spawn_refresh(self.hwnd, self.tray_icons.clone(), request) {
            self.last_tray_refresh = Some(Instant::now());
            if reading_overflow {
                self.tray_details_pending = false;
                self.last_tray_details = self.last_tray_refresh;
            }
            self.tray_work = Some(TrayWork::Refresh { cancel, reading_overflow, queued_click: None });
        } else {
            self.taskbar.end_tray_peek();
        }
    }

    /// 后台采样退还主任务栏后，立即处理这段时间收到的图标点击。
    fn on_tray_refresh(&mut self, snapshot: TraySnapshot) {
        let Some(TrayWork::Refresh { queued_click, .. }) = self.tray_work.take() else { return };
        self.taskbar.end_tray_peek();
        // 从采样完成时刻重新计算冷却周期，防止连续采样长时间占用底部遮罩。
        self.last_tray_refresh = Some(Instant::now());
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
                if p.is_open() {
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
        self.sync_overlay_hooks();
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
            _ => self.snap.is_monitor_maximized(dock.monitor.hmonitor.0 as isize),
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
        if !self.topbar_visible {
            self.close_popover_immediate();
            self.close_context_menu_immediate();
            self.close_panel_immediate();
        }
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
            self.cancel_background_tray_refresh();
            if self.edge_since.is_none() {
                self.edge_since = Some(Instant::now());
            }
            self.hide_at = None;
            self.ensure_frame();
        } else if self.edge_since.is_some() {
            self.edge_since = None;
        }

        if in_dock_zone {
            self.cancel_background_tray_refresh();
            self.mouse_in_dock = true;
            self.hide_at = None;
        } else if self.mouse_in_dock {
            self.cancel_background_tray_refresh();
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
            self.cancel_background_tray_refresh();
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
                let dock_dark = self.docks[di].color_dark;
                self.window_preview.show_for(ptr, hi, &exe, &windows, anchor_cx, anchor_top, mon, appearance, dock_dark);
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
        let transition = self.config.topbar.color_transition_ms;
        for i in 0..n {
            let (want, hidden) = self.dock_state(i);
            let cfg = &self.config.dock;
            let appearance = &self.config.appearance;
            let a = {
                let dock = &mut self.docks[i];
                dock.set_want_visible(want);
                dock.update_trigger(hidden);
                dock.tick(dt, cfg, appearance, transition)
            };
            let need_draw = a || self.docks[i].dirty;
            animating |= a;
            if need_draw {
                self.render_dock(i);
            }
        }
        let reduce = self.config.appearance.reduce_motion;
        let ptr = app_ptr() as isize;
        let mut any_overlay_closed = false;
        let mut any_overlay_geometry_changed = false;
        for bar in &mut self.topbars {
            animating |= bar.tick_color(dt, transition);
            animating |= bar.tick_tray_anim(dt, reduce);
            if let Some(p) = bar.popover.as_mut() {
                if p.is_animating() {
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, 0);
                    }
                    let (p_anim, p_changed, just_closed) = p.tick(dt, reduce);
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, ptr);
                    }
                    if p_changed {
                        invalidate(bar.hwnd);
                        any_overlay_geometry_changed = true;
                    }
                    if just_closed {
                        any_overlay_closed = true;
                    }
                    animating |= p_anim;
                }
            }
        }
        // 统一推进顶部栏右侧状态面板与左侧下拉菜单的抽屉滑出/收起帧动画
        if self.status_panel.is_animating() {
            let sp_hwnd = self.status_panel.hwnd;
            if !sp_hwnd.0.is_null() {
                unsafe {
                    let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, 0);
                }
            }
            let (sp_anim, sp_changed, just_closed) = self.status_panel.tick(dt, reduce);
            if !sp_hwnd.0.is_null() {
                unsafe {
                    let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, ptr);
                }
            }
            if sp_changed {
                any_overlay_geometry_changed = true;
            }
            if just_closed {
                self.status_panel_bar = None;
                any_overlay_closed = true;
            }
            animating |= sp_anim;
        }
        if self.context_menu.is_animating() {
            let cm_hwnd = self.context_menu.hwnd;
            if !cm_hwnd.0.is_null() {
                unsafe {
                    let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, 0);
                }
            }
            let (cm_anim, cm_changed, just_closed) = self.context_menu.tick(dt, reduce);
            if !cm_hwnd.0.is_null() {
                unsafe {
                    let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, ptr);
                }
            }
            if cm_changed {
                any_overlay_geometry_changed = true;
            }
            if just_closed {
                self.topbar_menu = None;
                any_overlay_closed = true;
            }
            animating |= cm_anim;
        }
        if any_overlay_closed {
            self.sync_topbar_active();
            self.flush_pending_color_samples();
        }
        if any_overlay_geometry_changed || any_overlay_closed {
            self.sync_overlay_hooks();
        }
        if !animating {
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_FRAME);
            }
            self.frame_on = false;
        }
    }

    fn render_dock(&mut self, i: usize) {
        let appearance_snapshot = self.config.appearance.clone();
        let is_liquid = appearance_snapshot.dock_material == "liquid_glass";
        // 首次从隐藏浮出前同步采样正后方画面（此时 Dock 尚未显示，完全无自遮挡且首帧零闪烁）
        if !self.docks[i].window_visible
            && self.docks[i].want_visible
            && (self.docks[i].backdrop_dirty || (is_liquid && self.docks[i].glass_dirty))
        {
            let (bg, glass) = if (appearance_snapshot.theme == "system" || is_liquid) && !self.snap.fullscreen {
                color_sync::sample_dock_backdrop_bundle(
                    self.docks[i].glass_zone_rect(),
                    self.docks[i].capsule_screen_rect(),
                    self.docks[i].win_rect,
                    self.docks[i].monitor.rect,
                    false,
                    is_liquid,
                    None,
                    self.docks[i].glass_flank_x_bounds(),
                )
            } else {
                (None, None)
            };
            self.docks[i].apply_backdrop_color(bg, &appearance_snapshot, true);
            self.docks[i].apply_glass_strip(glass, &appearance_snapshot);
        } else if is_liquid && self.docks[i].want_visible && self.docks[i].glass_strip.is_none() {
            // 仅当背后确认为纯桌面无外部窗口遮挡时才先填入缓存壁纸条带，有应用窗口遮挡时等待异步真实采样
            if monitor::dock_backdrop_windows(self.docks[i].capsule_screen_rect()).is_empty() {
                let strip = color_sync::dock_wallpaper_glass_strip(
                    self.docks[i].monitor.rect,
                    self.docks[i].glass_zone_rect(),
                );
                self.docks[i].apply_glass_strip(Some(strip), &appearance_snapshot);
            }
            self.docks[i].glass_dirty = true;
            self.spawn_dock_backdrop_sample();
        }
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
        // 1Hz 轻量状态校对：兜底 Win+D 等系统级切换偶发漏掉 WinEvent 的场景
        if monitor::refresh() {
            self.snap = monitor::snapshot();
            self.refresh_entries();
            self.sync_shelf_items();
            self.on_state_changed();
            self.dismiss_overlays_on_foreground_change();
            self.ensure_frame();
        }
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
        // 每 5 秒轻量校验壁纸签名，覆盖 Windows 聚焦/幻灯片换壁纸未广播 WM_SETTINGCHANGE 的场景
        if self.slow_ticks.is_multiple_of(5) && color_sync::check_wallpaper_changed() {
            self.on_theme_changed();
        }
        // 最大化融合态每 10 秒低频复查一次标题栏颜色（浮窗展开期间自动跳过，避免采到黑底阴影）
        if self.slow_ticks % 10 == 0 {
            self.schedule_fused_color_recheck();
        }
        // 可见 Dock 每 2 秒低频复查背后内容（覆盖应用内滚屏/标签页切换或壁纸切换等无窗口焦点变化场景）
        let need_dock_sample = self.config.appearance.theme == "system"
            || self.config.appearance.dock_material == "liquid_glass";
        if self.slow_ticks % 2 == 0
            && need_dock_sample
            && !self.snap.fullscreen
            && !self.dock_color_busy
        {
            let is_liquid = self.config.appearance.dock_material == "liquid_glass";
            let mut marked = false;
            for d in &mut self.docks {
                // 悬停放大/Tooltip/拖拽期间跳过低频复查，防止自身放大图标或气泡遮挡破坏已有采样
                let interacting = d.hover.is_some() || d.tooltip_visible || d.drag.is_some();
                if d.window_visible && d.want_visible && !interacting {
                    d.backdrop_dirty = true;
                    if is_liquid {
                        d.glass_dirty = true;
                    }
                    marked = true;
                }
            }
            if marked {
                self.spawn_dock_backdrop_sample();
            }
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
            let popover_open = self.topbars.iter().any(|b| b.popover.as_ref().is_some_and(|p| p.is_open()));
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
                && (taskbar_open || !self.dock_busy_for_tray_reveal())
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
        self.close_popover_immediate();
        self.close_context_menu_immediate();
        self.close_panel_immediate();
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
        if let Some(settings_hwnd) = self.settings.as_ref().filter(|s| s.is_open()).map(|s| s.hwnd()) {
            let settings_id = settings_hwnd.0 as isize;
            let mut fused_to_settings = false;
            for bar in &mut self.topbars {
                if bar.fused_hwnd == settings_id {
                    bar.color_dirty = true;
                    fused_to_settings = true;
                }
            }
            if fused_to_settings {
                unsafe {
                    // 设置页切换主题后等待本帧重绘完成再采样顶部栏融合色，避免采到旧色调
                    let _ = SetTimer(self.hwnd, TIMER_COLOR_RECHECK, 80, None);
                }
            }
        }
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
                // 计时器到点前再次确认当前悬停目标未被右键菜单抑制，避免弹窗前后竞态误显
                let suppressed = self
                    .tray_tip
                    .target_id()
                    .is_some_and(|(owner, key)| self.tray_gate.is_suppressed(owner, key));
                if suppressed {
                    self.tray_tip.clear(self.hwnd);
                } else if let Some(owner) = self.tray_tip.on_timer(self.hwnd, self.config.appearance.is_dark()) {
                    // 鼠标在顶部栏固定溢出图标上悬停稳定 100ms 后再按需拉取系统溢出详情，避免扫过即抢焦点
                    if self.topbar_index(owner).is_some()
                        && !self.tray_gate.has_active_menu()
                        && self.last_tray_details.is_none_or(|last| last.elapsed() >= Duration::from_secs(3))
                    {
                        self.tray_details_pending = true;
                        self.cancel_background_tray_refresh();
                        self.refresh_tray();
                    }
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
            TIMER_MONITOR_SETTLE => {
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_MONITOR_SETTLE);
                }
                if monitor::refresh() {
                    self.snap = monitor::snapshot();
                    self.refresh_entries();
                    self.sync_shelf_items();
                    self.on_state_changed();
                    self.dismiss_overlays_on_foreground_change();
                    self.ensure_frame();
                }
            }
            TIMER_COLOR_RECHECK => {
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_COLOR_RECHECK);
                }
                self.schedule_fused_color_recheck();
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
        let prev_fg = self.live_external_foreground();
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
        self.clear_tray_context();
        self.close_popover_immediate();
        self.close_panel_immediate();
        self.topbar_menu = None;
        self.sync_topbar_active();
        self.window_preview.close();
        self.menu_ctx = ctx;
        let ptr = app_ptr();
        let appearance = self.config.appearance.clone();
        self.context_menu.open(ptr, items, x, y, prev_fg, &appearance);
        self.mark_overlay_opened();
    }

    /// 在顶部栏按钮下方弹出带抽屉滑出动效的下拉菜单并点亮对应按钮激活态。
    fn popup_topbar_menu(
        &mut self,
        bi: usize,
        hit: Hit,
        items: Vec<MenuItem>,
        ctx: MenuCtx,
        anchor: RECT,
        dpi: u32,
        align_right: bool,
    ) {
        let prev_fg = self.live_external_foreground();
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
        self.clear_tray_context();
        self.close_popover_immediate();
        self.close_panel_immediate();
        self.window_preview.close();
        self.menu_ctx = ctx;
        self.topbar_menu = Some((bi, hit));
        self.sync_topbar_active();
        let ptr = app_ptr();
        let appearance = self.config.appearance.clone();
        self.context_menu
            .open_anchored(ptr, items, anchor, dpi, align_right, prev_fg, &appearance);
        self.mark_overlay_opened();
        if self.context_menu.is_animating() {
            self.ensure_frame();
        }
    }

    pub fn close_context_menu(&mut self) {
        let reduce = self.config.appearance.reduce_motion;
        let ptr = app_ptr() as isize;
        let cm_hwnd = self.context_menu.hwnd;
        if !cm_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, 0);
            }
        }
        let animating = self.context_menu.begin_close(reduce);
        if !cm_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, ptr);
            }
        }
        if !animating {
            self.topbar_menu = None;
        }
        self.sync_topbar_active();
        self.sync_overlay_hooks();
        if animating {
            self.ensure_frame();
        } else {
            self.flush_pending_color_samples();
        }
    }

    /// 立即隐藏自绘菜单并清理顶部栏菜单激活态（用于切换独占面板、退出或全屏让位）。
    pub fn close_context_menu_immediate(&mut self) {
        let cm_hwnd = self.context_menu.hwnd;
        let ptr = app_ptr() as isize;
        if !cm_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, 0);
            }
        }
        self.context_menu.close();
        if !cm_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(cm_hwnd, GWLP_USERDATA, ptr);
            }
        }
        self.topbar_menu = None;
        self.sync_topbar_active();
        self.sync_overlay_hooks();
        self.flush_pending_color_samples();
    }

    pub fn menu_mouse_move(&mut self, x: i32, y: i32) {
        let had_hover = self.context_menu.has_hover();
        self.context_menu.on_mouse_move(x, y);
        if self.context_menu.has_hover() != had_hover {
            self.sync_overlay_hooks();
        }
    }

    pub fn menu_mouse_leave(&mut self) {
        let had_hover = self.context_menu.has_hover();
        self.context_menu.on_mouse_leave();
        if self.context_menu.has_hover() != had_hover {
            self.sync_overlay_hooks();
        }
    }

    pub fn menu_lbutton_down(&mut self, x: i32, y: i32) {
        self.context_menu.on_lbutton_down(x, y);
    }

    pub fn menu_lbutton_up(&mut self, x: i32, y: i32) {
        if let Some(id) = self.context_menu.on_lbutton_up(x, y) {
            self.topbar_menu = None;
            self.sync_topbar_active();
            self.sync_overlay_hooks();
            self.on_menu(id);
        }
        self.flush_pending_color_samples();
    }

    pub fn menu_key_down(&mut self, vk: u32) {
        let had_hover = self.context_menu.has_hover();
        let was_open = self.context_menu.is_open();
        if let Some(id) = self.context_menu.on_key_down(vk) {
            self.topbar_menu = None;
            self.sync_topbar_active();
            self.sync_overlay_hooks();
            self.on_menu(id);
        } else if self.context_menu.is_open() != was_open || self.context_menu.has_hover() != had_hover {
            self.sync_overlay_hooks();
        }
        self.flush_pending_color_samples();
    }

    fn taskbar_menu_label_for(active: bool, temporary_show: bool) -> &'static str {
        if active && temporary_show {
            "重新隐藏 Windows 任务栏"
        } else if active {
            "显示 Windows 任务栏"
        } else {
            "隐藏 Windows 任务栏"
        }
    }

    fn taskbar_menu_label(&self) -> &'static str {
        Self::taskbar_menu_label_for(self.taskbar.active(), self.taskbar.temporary_show())
    }

    /// 构建产品菜单（顶部栏左上角图标 & Dock 空白区右键菜单）条目列表。
    fn product_menu_items(dock_visible: bool, topbar_visible: bool, taskbar_label: &'static str) -> Vec<MenuItem> {
        vec![
            MenuItem::normal(M_SETTINGS, "WinDock 设置"),
            MenuItem::normal(M_ABOUT, "关于 WinDock"),
            MenuItem::sep(),
            MenuItem::normal(M_TOGGLE_DOCK, if dock_visible { "隐藏 Dock" } else { "显示 Dock" }),
            MenuItem::normal(M_TOGGLE_TOPBAR, if topbar_visible { "隐藏顶部栏" } else { "显示顶部栏" }),
            MenuItem::normal(M_TOGGLE_TASKBAR, taskbar_label),
            MenuItem::sep(),
            MenuItem::normal(M_LOCK, "锁定屏幕"),
            MenuItem::normal(M_SLEEP, "睡眠"),
            MenuItem::danger(M_RESTART, "重新启动"),
            MenuItem::danger(M_SHUTDOWN, "关机"),
            MenuItem::sep(),
            MenuItem::normal(M_TASKMGR, "任务管理器"),
            MenuItem::normal(M_RESTART_APP, "重启 WinDock"),
            MenuItem::danger(M_EXIT, "退出 WinDock"),
        ]
    }

    /// 构建系统托盘图标菜单条目列表。
    fn tray_menu_items(
        dock_visible: bool,
        topbar_visible: bool,
        taskbar_label: &'static str,
        startup_item: MenuItem,
    ) -> Vec<MenuItem> {
        vec![
            MenuItem::normal(M_SETTINGS, "WinDock 设置"),
            MenuItem::sep(),
            MenuItem::normal(M_TOGGLE_DOCK, if dock_visible { "隐藏 Dock" } else { "显示 Dock" }),
            MenuItem::normal(M_TOGGLE_TOPBAR, if topbar_visible { "隐藏顶部栏" } else { "显示顶部栏" }),
            MenuItem::normal(M_TOGGLE_TASKBAR, taskbar_label),
            MenuItem::sep(),
            startup_item,
            MenuItem::sep(),
            MenuItem::normal(M_RESTART_APP, "重启 WinDock"),
            MenuItem::danger(M_EXIT, "退出 WinDock"),
        ]
    }

    fn product_menu(&mut self) {
        let pt = cursor_pos();
        self.product_menu_at(pt.x, pt.y);
    }

    fn product_menu_at(&mut self, x: i32, y: i32) {
        let v = Self::product_menu_items(self.dock_visible, self.topbar_visible, self.taskbar_menu_label());
        self.popup_at(v, MenuCtx::None, x, y);
    }

    // ---------- 消息入口 ----------

    pub fn on_monitor_event(&mut self) {
        monitor::refresh();
        self.snap = monitor::snapshot();
        self.refresh_entries();
        self.sync_shelf_items();
        self.on_state_changed();
        self.dismiss_overlays_on_foreground_change();
        self.ensure_frame();
        // 安排 200ms 尾随状态复查，确保 Win+D 还原动画结束后前台与最大化态完整同步
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_MONITOR_SETTLE, 200, None);
        }
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
        fill_buf(&mut title, "WinDock");
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
        let v = Self::tray_menu_items(
            self.dock_visible,
            self.topbar_visible,
            self.taskbar_menu_label(),
            startup_item,
        );
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
            M_RESTART_APP => self.restart(),
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
            M_OPEN_FILE_MANAGER => {
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
            // 「打开」优先切回已运行窗口，避免把便携应用的映像路径当作文件交给 Shell。
            M_APP_OPEN if items::has_windows(&e) => {
                if let [(hwnd, _)] = e.windows.as_slice() {
                    items::activate_window(HWND(*hwnd as *mut _));
                } else {
                    self.activate_entry(i);
                }
            }
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
                // 优先读取运行态保留的真实大小写 exe 路径，避免固定项存入全小写路径与名称。
                let target = item.as_ref().map(|it| it.target.clone()).or_else(|| {
                    e.running_exe.as_ref().map(|key| {
                        crate::monitor::running_apps()
                            .into_iter()
                            .find(|a| a.key == *key)
                            .map(|a| a.exe)
                            .unwrap_or_else(|| crate::win32::real_path_case(key))
                    })
                });
                if let Some(t) = target {
                    if let Some(it) = items::item_from_path_with_hint(&t, Some(&e.label)) {
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
        // Dock 放大动画与交互优先于后台托盘采样，右键和悬停详情仍照常处理。
        self.cancel_background_tray_refresh();
        self.mouse_in_dock = true;
        self.hide_at = None;
        self.edge_since = None;
        let mut render_now = false;
        {
            let dock = &mut self.docks[i];
            dock.mouse = (x, y);
            dock.mouse_in = true;
            // 按住固定应用拖动超过阈值后自动启动拖拽排序/拖出移除
            if dock.drag.is_none() {
                if let Some((pi, px, py)) = dock.press {
                    let dist = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
                    let is_pinned = dock.entries.apps.get(pi).and_then(|e| e.pinned).is_some();
                    if is_pinned && dist >= 5.0 * dock.dpi_scale() {
                        dock.begin_drag(pi, px, py, x, y);
                    }
                }
            }
            if dock.drag.is_some() {
                dock.update_drag(x, y);
                // 拖拽期间立即刷新分层窗口，规避 16ms 帧定时器节流导致的拖影滞后
                if dock.last_drag_render.is_none_or(|t| t.elapsed() >= Duration::from_millis(4)) {
                    dock.last_drag_render = Some(Instant::now());
                    render_now = true;
                }
            } else {
                let hit = dock.hit(x, y);
                if hit != dock.hover {
                    dock.hover = hit;
                    dock.hover_since = hit.map(|_| Instant::now());
                    if hit.is_none() {
                        dock.tooltip_visible = false;
                    }
                    dock.dirty = true;
                }
            }
        }
        if render_now {
            self.render_dock(i);
        }
        self.ensure_frame();
    }

    pub fn dock_mouse_leave(&mut self, hwnd: HWND) {
        let Some(i) = self.dock_index(hwnd) else { return };
        self.cancel_background_tray_refresh();
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
        self.cancel_background_tray_refresh();
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
            if dock.drag.is_some() {
                dock.update_drag(x, y);
            }
            let hit = dock.hit(x, y);
            (dock.drag.take(), dock.press.take(), hit)
        };
        if let Some(d) = drag {
            let entry = self.docks[i].entries.apps.get(d.from).cloned();
            if let Some(e) = entry.filter(|e| e.pinned.is_some()) {
                if d.remove {
                    self.docks[i].slot_draw_x.clear();
                    self.docks[i].drop_settle = false;
                    items::remove_pinned(&mut self.dock_items, &e.id);
                    self.after_items_change();
                } else if let Some(target) = d.target {
                    let pinned = crate::dock::pinned_app_indices(&self.docks[i].entries.apps);
                    if let (Some(pf), Some(pt)) = (
                        pinned.iter().position(|&k| k == d.from),
                        pinned.iter().position(|&k| k == target),
                    ) {
                        let drop_gx = d.x + d.grab_dx;
                        self.docks[i].prepare_drop_settle(d.from, target, drop_gx);
                        let mut ordered_ids: Vec<String> = pinned
                            .iter()
                            .filter_map(|&k| self.docks[i].entries.apps.get(k).map(|x| x.id.clone()))
                            .collect();
                        if pf < ordered_ids.len() && pt < ordered_ids.len() && pf != pt {
                            let moved = ordered_ids.remove(pf);
                            ordered_ids.insert(pt, moved);
                            if items::reorder_app_entries(&mut self.dock_items, &ordered_ids) {
                                self.after_items_change();
                            }
                        }
                    }
                }
            }
            self.render_dock(i);
        } else if let Some((pi, _, _)) = press {
            if hit == Some(pi) {
                self.activate_entry(pi);
            }
        }
        self.ensure_frame();
    }

    pub fn dock_rbutton_up(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.dock_index(hwnd) else { return };
        self.cancel_background_tray_refresh();
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
        self.docks[i].drop_settle = false;
        self.docks[i].slot_draw_x.clear();
        self.docks[i].dirty = true;
        self.ensure_frame();
    }

    pub fn dock_edge_hovered(&mut self, _hwnd: HWND) {
        self.cancel_background_tray_refresh();
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
                    items::minimize_window(target);
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
        let mut start_drag = false;
        let mut paint_now = false;
        {
            let bar = &mut self.topbars[i];
            if let Some((pkey, px, py)) = bar.press_icon.clone() {
                if bar.drag.is_none() {
                    let dist = (((x - px).pow(2) + (y - py).pow(2)) as f32).sqrt();
                    if dist >= 4.0 * (bar.dpi as f32 / 96.0) && bar.begin_tray_drag(&pkey, px, x) {
                        bar.last_drag_paint = Some(Instant::now());
                        start_drag = true;
                        paint_now = true;
                    }
                }
            }
            if !start_drag && bar.drag.is_some() {
                bar.update_tray_drag(x);
                if bar.last_drag_paint.is_none_or(|t| t.elapsed() >= Duration::from_millis(4)) {
                    bar.last_drag_paint = Some(Instant::now());
                    paint_now = true;
                }
            }
        }
        if start_drag {
            self.tray_tip.clear(self.hwnd);
        }
        if self.topbars[i].drag.is_some() {
            if paint_now {
                let t = &self.config.topbar;
                let a = &self.config.appearance;
                let icons = &self.tray_icons;
                let bar = &mut self.topbars[i];
                crate::topbar::render::paint_immediate(hwnd, bar, t, a, icons);
            }
            self.ensure_frame();
            return;
        }
        let hit = self.topbars[i].hit_test(x, y);
        self.topbars[i].set_hover(hit);
        self.update_tray_tip(hwnd, x, y);
    }

    /// 根据当前命中的格子提取实际托盘提示，而非替换成窗口的显示名称。
    fn resolve_tray_tip_target(
        topbars: &[TopBar],
        gate: &TrayTipGate,
        hwnd: HWND,
        x: i32,
        y: i32,
    ) -> (Option<TipTarget>, bool, bool, Option<String>) {
        let mut hidden_icon = false;
        let mut from_popover = false;
        let mut hovered_key: Option<String> = None;
        let target = if let Some(index) = topbars.iter().position(|b| b.hwnd == hwnd) {
            let bar = &topbars[index];
            match bar.hit_test(x, y) {
                Some(hit @ Hit::TrayIcon(_)) if bar.press_icon.is_none() && bar.drag.is_none() => {
                    let Hit::TrayIcon(key) = &hit else {
                        return (None, false, false, None);
                    };
                    hovered_key = Some(key.clone());
                    if gate.is_suppressed(hwnd, key) {
                        None
                    } else {
                        bar.tray_items.iter().find(|item| &item.key == key).and_then(|item| {
                            hidden_icon = item.overflow;
                            Some(TipTarget {
                                owner: hwnd,
                                key: key.clone(),
                                anchor: tray_anchor(hwnd, bar.rect_of(&hit)?)?,
                                dpi: bar.dpi,
                                text: item.hint().to_string(),
                                hidden_icon,
                            })
                        })
                    }
                }
                _ => None,
            }
        } else {
            from_popover = true;
            topbars
                .iter()
                .position(|b| b.popover.as_ref().map(|p| p.hwnd) == Some(hwnd))
                .and_then(|index| topbars[index].popover.as_ref())
                .and_then(|p| {
                    if !p.is_open() || p.press.is_some() {
                        return None;
                    }
                    let index = p.index_at(x, y)?;
                    let item = p.items.get(index)?;
                    hovered_key = Some(item.key.clone());
                    if gate.is_suppressed(hwnd, &item.key) {
                        return None;
                    }
                    hidden_icon = item.overflow;
                    Some(TipTarget {
                        owner: hwnd,
                        key: item.key.clone(),
                        anchor: tray_anchor(hwnd, p.cell_rect(index)?)?,
                        dpi: p.dpi,
                        text: item.hint().to_string(),
                        hidden_icon,
                    })
                })
        };
        (target, from_popover, hidden_icon, hovered_key)
    }

    fn tray_item_screen_rect(&self, owner: HWND, key: &str) -> Option<RECT> {
        let local = if let Some(index) = self.topbar_index(owner) {
            self.topbars[index].rect_of(&Hit::TrayIcon(key.to_string()))?
        } else if let Some(index) = self.popover_owner(owner) {
            let p = self.topbars[index].popover.as_ref().filter(|p| p.is_open())?;
            let item_idx = p.items.iter().position(|it| it.key == key)?;
            p.cell_rect(item_idx)?
        } else {
            return None;
        };
        tray_anchor(owner, local)
    }

    /// 判断物理光标当前是否仍停留在指定顶部栏图标区域内。
    fn cursor_on_tray_item(&self, owner: HWND, key: &str) -> bool {
        let Some(rect) = self.tray_item_screen_rect(owner, key) else {
            return false;
        };
        let pt = cursor_pos();
        (rect.left..rect.right).contains(&pt.x) && (rect.top..rect.bottom).contains(&pt.y)
    }

    fn set_tray_context(&mut self, owner: HWND, key: String, is_popover: bool) {
        if let Some(prev) = self.tray_gate.context.take() {
            if prev.is_popover && (!is_popover || prev.owner != owner) {
                if let Some(index) = self.popover_owner(prev.owner) {
                    if let Some(p) = self.topbars[index].popover.as_mut() {
                        p.set_context_key(None);
                    }
                }
            }
        }
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TRAY_CONTEXT);
        }
        self.tray_gate.context = Some(TrayContext {
            owner,
            key,
            is_popover,
            menu: None,
        });
        self.sync_overlay_hooks();
    }

    fn clear_tray_context(&mut self) {
        let prev = self.tray_gate.context.take();
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TRAY_CONTEXT);
        }
        if let Some(ctx) = prev {
            if ctx.is_popover {
                if let Some(index) = self.popover_owner(ctx.owner) {
                    if let Some(p) = self.topbars[index].popover.as_mut() {
                        p.set_context_key(None);
                    }
                }
            }
            self.flush_pending_color_samples();
            self.sync_overlay_hooks();
        }
    }

    fn update_tray_tip(&mut self, hwnd: HWND, x: i32, y: i32) {
        // 每次悬停更新前即时检查外部右键菜单是否已关闭，避免等待轮询定时器造成恢复延迟
        self.track_tray_context();
        let (target, from_popover, hidden_icon, hovered_key) =
            Self::resolve_tray_tip_target(&self.topbars, &self.tray_gate, hwnd, x, y);
        self.tray_gate.update_hover(hwnd, hovered_key.as_deref());
        // 收纳格内直接触发托盘溢出详情刷新；右键菜单打开期间跳过溢出刷新以免惊动 Explorer 关掉当前菜单
        if from_popover
            && hidden_icon
            && !self.tray_gate.has_active_menu()
            && target.as_ref().is_some_and(|target| !self.tray_tip.tracks(target))
            && self.last_tray_details.is_none_or(|last| last.elapsed() >= Duration::from_secs(3))
        {
            self.tray_details_pending = true;
            self.cancel_background_tray_refresh();
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

    pub fn topbar_mouse_leave(&mut self, hwnd: HWND) {
        if self.tray_tip.owner() == Some(hwnd) {
            self.tray_tip.clear(self.hwnd);
        }
        // 忽略右键兜底归还光标期间滞后到达的 WM_MOUSELEAVE，仅在物理光标确实移出图标后解除抑制
        if self
            .tray_gate
            .suppressed
            .as_ref()
            .is_some_and(|(owner, key)| *owner == hwnd && !self.cursor_on_tray_item(*owner, key))
        {
            self.tray_gate.clear_suppressed();
        }
        let Some(i) = self.topbar_index(hwnd) else { return };
        if self.topbars[i].drag.is_none() {
            self.topbars[i].set_hover(None);
        }
    }

    pub fn topbar_lbutton_down(&mut self, hwnd: HWND, x: i32, y: i32) {
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
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
        if self.topbars[i].drag.is_some() {
            self.topbars[i].update_tray_drag(x);
        }
        let press = self.topbars[i].press_icon.take();
        let drag = self.topbars[i].drag.take();
        unsafe {
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == hwnd {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            }
        }
        if let Some(d) = drag {
            let drop_cx = (d.x + d.grab_dx) as f32;
            self.topbars[i].prepare_tray_drop_settle(&d.key, drop_cx);
            self.reorder_topbar_by_visual(i, &d.key, d.target_idx);
            let hit = self.topbars[i].hit_test(x, y);
            self.topbars[i].set_hover(hit);
            let t = &self.config.topbar;
            let a = &self.config.appearance;
            let icons = &self.tray_icons;
            crate::topbar::render::paint_immediate(hwnd, &mut self.topbars[i], t, a, icons);
            self.ensure_frame();
            return;
        }
        if let Some((pkey, _, _)) = press {
            if matches!(self.topbars[i].hit_test(x, y), Some(Hit::TrayIcon(ref k)) if k == &pkey) {
                if let Some(item) = self.topbars[i].tray_items.iter().find(|item| item.key == pkey).cloned() {
                    self.invoke_tray_native(hwnd, item, None);
                }
            }
        }
    }

    pub fn topbar_capture_lost(&mut self, hwnd: HWND) {
        if let Some(i) = self.topbar_index(hwnd) {
            self.topbars[i].press_icon = None;
            self.topbars[i].tray_draw_x.clear();
            self.topbars[i].tray_drop_settle = false;
            if self.topbars[i].drag.take().is_some() {
                invalidate(hwnd);
            }
        }
    }

    /// 顶部栏和收纳格共用原生托盘交互。普通窗口仍有自己的窗口菜单。
    fn invoke_tray_native(&mut self, owner: HWND, item: TrayItem, anchor: Option<RECT>) {
        self.invoke_tray(TrayClick { item, anchor, owner, is_popover: false });
    }

    fn invoke_tray(&mut self, click: TrayClick) {
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
        let TrayClick { item, anchor, owner, is_popover } = click;
        let right = anchor.is_some();
        let screen = anchor.map_or(POINT::default(), |icon| POINT { x: icon.left, y: icon.bottom });
        if !right && self.tray_gate.context.as_ref().is_some_and(|ctx| ctx.menu.is_none()) {
            self.clear_tray_context();
        }
        if !right && !item.app_hwnds.is_empty() {
            self.invoke_window_shelf_item(&item, false, screen);
            return;
        }
        if right {
            // 右键请求发起即锁定当前来源图标的提示抑制，覆盖光标临时迁出与归还全过程
            self.set_tray_context(owner, item.key.clone(), is_popover);
        }
        if let Some(work) = &mut self.tray_work {
            let queued_click = match work {
                TrayWork::Refresh { cancel, queued_click, .. } => {
                    cancel.cancel();
                    queued_click
                }
                TrayWork::Click { queued_click, .. } => queued_click,
            };
            *queued_click = Some(TrayClick { item, anchor, owner, is_popover });
            return;
        }
        let peek = self.taskbar.begin_tray_peek();
        let cover = self.tray_cover_context();
        if tray_mirror::spawn_invocation(self.hwnd, item.clone(), anchor, peek, self.tray_icons.clone(), cover) {
            self.tray_work = Some(TrayWork::Click { right, screen, owner, is_popover, queued_click: None });
        } else {
            self.taskbar.end_tray_peek();
            self.invoke_window_shelf_item(&item, right, screen);
            if is_popover {
                self.close_popover();
            } else if right {
                self.clear_tray_context();
            }
        }
    }

    fn on_tray_invoked(&mut self, result: TrayInvocation) {
        let Some(TrayWork::Click { right, screen, owner, is_popover, queued_click }) = self.tray_work.take() else { return };
        self.taskbar.end_tray_peek();
        self.apply_tray_snapshot(result.snapshot);
        if let Some(click) = queued_click {
            if right && click.anchor.is_none() {
                self.clear_tray_context();
            }
            self.invoke_tray(click);
            return;
        }
        // 普通窗口才回退到窗口菜单。
        if result.ordinary_window {
            self.invoke_window_shelf_item(&result.target, right, screen);
            if is_popover {
                self.close_popover();
            } else if right {
                self.clear_tray_context();
            }
            return;
        }
        if right
            && self
                .tray_gate
                .context
                .as_ref()
                .is_some_and(|ctx| ctx.owner == owner && ctx.key == result.target.key)
        {
            if let Some(menu) = result.menu {
                if let Some(ctx) = &mut self.tray_gate.context {
                    ctx.menu = Some(HWND(menu as *mut _));
                }
                // 原生/自定义菜单拥有自己的消息循环：持续监测其可见性，菜单开启期间仅屏蔽来源图标的悬浮预览。
                unsafe { let _ = SetTimer(self.hwnd, TIMER_TRAY_CONTEXT, 45, None); }
                self.track_tray_context();
            } else {
                self.clear_tray_context();
                // 未能捕获菜单窗口句柄时，只要光标仍停留在该图标上则继续抑制提示直到离开
                if self.cursor_on_tray_item(owner, &result.target.key) {
                    self.tray_gate.suppressed = Some((owner, result.target.key));
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

    fn reorder_topbar_by_visual(&mut self, bar_idx: usize, key: &str, target_idx: usize) {
        let ordered: Vec<String> = crate::topbar::tray_slots_sorted(&self.topbars[bar_idx].hits)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let merged = crate::topbar::reorder_pinned_tray_keys(
            &ordered,
            &self.config.topbar.pinned_tray_keys,
            key,
            target_idx,
        );
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

    /// 系统浅/深主题或壁纸变化时仅更新外观与重绘，不销毁重建 AppBar，彻底杜绝闪烁与画面抖动。
    pub fn on_theme_changed(&mut self) {
        for d in &mut self.docks {
            d.glass_strip = None;
            d.glass_dirty = self.config.appearance.dock_material == "liquid_glass";
        }
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
        let (anchor, dpi) = {
            let bar = &self.topbars[i];
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
        if matches!(
            hit,
            Hit::ProductMenu | Hit::AppMenu | Hit::MenuFile | Hit::MenuWindow | Hit::MenuGo
        ) && self.context_menu.is_open()
            && self.topbar_menu.as_ref() == Some(&(i, hit.clone()))
        {
            self.close_context_menu();
            return;
        }
        match hit {
            Hit::ProductMenu => {
                let v = Self::product_menu_items(self.dock_visible, self.topbar_visible, self.taskbar_menu_label());
                self.popup_topbar_menu(i, hit, v, MenuCtx::None, anchor, dpi, false);
            }
            Hit::AppMenu => {
                let (v, ctx) = match self.foreground_window() {
                    Some(fg) => {
                        let zoomed = unsafe { IsZoomed(fg).as_bool() };
                        (
                            vec![
                                MenuItem::normal(M_FG_MIN, "最小化"),
                                MenuItem::normal(M_FG_MAX, if zoomed { "还原" } else { "最大化" }),
                                MenuItem::normal(M_FG_CENTER, "居中"),
                                MenuItem::sep(),
                                MenuItem::danger(M_FG_CLOSE, "关闭窗口"),
                            ],
                            MenuCtx::Foreground(fg.0 as isize),
                        )
                    }
                    None => (
                        vec![
                            MenuItem::normal(M_OPEN_FILE_MANAGER, "打开文件管理器"),
                            MenuItem::normal(M_SETTINGS, "WinDock 设置"),
                        ],
                        MenuCtx::None,
                    ),
                };
                self.popup_topbar_menu(i, hit, v, ctx, anchor, dpi, false);
            }
            Hit::MenuFile => {
                let mut v = vec![
                    MenuItem::normal(M_OPEN_FILE_MANAGER, "新建文件管理器窗口"),
                    MenuItem::normal(M_OPEN_TERMINAL, "打开终端"),
                ];
                let ctx = if let Some(fg) = self.foreground_window() {
                    v.push(MenuItem::sep());
                    v.push(MenuItem::danger(M_FG_CLOSE, "关闭当前窗口"));
                    MenuCtx::Foreground(fg.0 as isize)
                } else {
                    MenuCtx::None
                };
                self.popup_topbar_menu(i, hit, v, ctx, anchor, dpi, false);
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
                self.popup_topbar_menu(i, hit, v, ctx, anchor, dpi, false);
            }
            Hit::MenuGo => {
                let v = vec![
                    MenuItem::normal(M_OPEN_BASE, "此电脑"),
                    MenuItem::normal(M_OPEN_BASE + 1, "桌面"),
                    MenuItem::normal(M_OPEN_BASE + 2, "下载"),
                    MenuItem::normal(M_OPEN_BASE + 3, "文档"),
                    MenuItem::sep(),
                    MenuItem::normal(M_OPEN_BASE + 4, "回收站"),
                    MenuItem::normal(M_OPEN_BASE + 5, "Windows 设置"),
                ];
                self.popup_topbar_menu(i, hit, v, MenuCtx::None, anchor, dpi, false);
            }
            Hit::TrayToggle => {
                if self.topbars[i].popover.as_ref().is_some_and(|p| p.is_open()) {
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
                    if right {
                        // 右键托盘图标前先立即收起既有的收纳浮层、自绘菜单与状态面板，防止上下文错乱
                        self.close_popover_immediate();
                        self.close_context_menu_immediate();
                        self.close_panel_immediate();
                    }
                    self.invoke_tray_native(hwnd, item, anchor);
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

    /// 根据当前仍处于打开态的顶部栏面板/下拉菜单/收纳浮层统一刷新按钮高亮。
    fn sync_topbar_active(&mut self) {
        let target: Option<(usize, Hit)> = if self.status_panel.is_open() {
            self.status_panel_bar.clone()
        } else if self.context_menu.is_open() {
            self.topbar_menu.clone()
        } else {
            self.topbars
                .iter()
                .position(|b| b.popover.as_ref().is_some_and(|p| p.is_open()))
                .map(|bi| (bi, Hit::TrayToggle))
        };
        match target {
            Some((bi, hit)) => self.set_topbar_active(Some(bi), Some(hit)),
            None => self.set_topbar_active(None, None),
        }
    }

    fn has_open_overlay(&self) -> bool {
        self.context_menu.is_open()
            || self.status_panel.is_open()
            || self.topbars.iter().any(|b| b.popover.as_ref().is_some_and(|p| p.is_open()))
    }

    /// 读取当前真实的外部前台窗口（过滤 WinDock 浮窗与瞬态壳层弹窗，避免切换失真）。
    fn live_external_foreground(&self) -> HWND {
        let fg = unsafe { GetForegroundWindow() };
        if !fg.0.is_null() {
            let class = class_name(fg);
            if !monitor::is_our_window(&class) && !monitor::is_transient_tray_or_popup_class(&class) {
                return fg;
            }
        }
        HWND(self.snap.foreground as *mut _)
    }

    /// 记录新一轮非激活浮层开启代数与背景前台窗口，并立即同步底层输入钩子。
    fn mark_overlay_opened(&mut self) {
        self.overlay_generation = self.overlay_generation.wrapping_add(1);
        self.overlay_fg = Some(self.live_external_foreground().0 as isize);
        self.sync_overlay_hooks();
    }

    /// 根据当前开启的非激活浮层状态动态安装、更新或卸载 `WH_MOUSE_LL` / `WH_KEYBOARD_LL` 钩子。
    fn sync_overlay_hooks(&self) {
        if self.hwnd.0.is_null() || self.exiting || !self.has_open_overlay() {
            uninstall_overlay_hooks();
            return;
        }
        let mut overlay_rects = Vec::new();
        if self.context_menu.is_open() {
            if let Some(r) = self.context_menu.visible_screen_rect() {
                overlay_rects.push(r);
            }
        }
        let mut panel_win_rect = None;
        if self.status_panel.is_open() {
            if let Some(r) = self.status_panel.visible_screen_rect() {
                overlay_rects.push(r);
            }
            panel_win_rect = crate::win32::window_rect(self.status_panel.hwnd);
        }
        let mut popover_rect = None;
        for bar in &self.topbars {
            if let Some(p) = bar.popover.as_ref().filter(|p| p.is_open()) {
                if let Some(r) = p.visible_screen_rect() {
                    overlay_rects.push(r);
                    popover_rect = Some(r);
                }
            }
        }
        let mut topbar_toggle_rects = Vec::new();
        for bar in &self.topbars {
            if !bar.visible || bar.hidden_by_fullscreen {
                continue;
            }
            let Some(wr) = crate::win32::window_rect(bar.hwnd) else { continue };
            for (r, hit) in &bar.hits {
                if is_topbar_overlay_toggle_hit(hit) {
                    topbar_toggle_rects.push(RECT {
                        left: wr.left + r.left,
                        top: wr.top + r.top,
                        right: wr.left + r.right,
                        bottom: wr.top + r.bottom,
                    });
                }
            }
        }
        let next_state = OverlayHookState {
            main_hwnd: self.hwnd.0 as isize,
            generation: self.overlay_generation,
            overlay_rects,
            topbar_toggle_rects,
            popover_rect,
            context_menu_open: self.context_menu.is_open(),
            context_menu_has_hover: self.context_menu.has_hover(),
            panel_dragging_slider: self.status_panel.is_open() && self.status_panel.is_dragging_slider(),
            panel_win_rect,
            tray_menu_active: self.tray_gate.has_active_menu(),
        };
        OVERLAY_HOOKS.with(|slot| {
            if let Ok(mut hooks) = slot.try_borrow_mut() {
                hooks.state = next_state;
                unsafe {
                    if hooks.mouse.is_invalid() {
                        hooks.mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(overlay_mouse_ll_proc), None, 0)
                            .unwrap_or_default();
                    }
                    if hooks.keyboard.is_invalid() {
                        hooks.keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(overlay_keyboard_ll_proc), None, 0)
                            .unwrap_or_default();
                    }
                }
            }
        });
    }

    /// 当非激活浮层开启期间外部前台应用发生切换（如 Alt+Tab / Win+D）时自动收起浮层。
    fn dismiss_overlays_on_foreground_change(&mut self) {
        if !self.has_open_overlay()
            || self.tray_gate.has_active_menu()
            || matches!(
                self.tray_work,
                Some(TrayWork::Refresh { reading_overflow: true, .. } | TrayWork::Click { .. })
            )
        {
            return;
        }
        let cur_fg = self.live_external_foreground().0 as isize;
        if self.overlay_fg.is_some_and(|prev| prev != cur_fg) {
            self.close_popover();
            self.close_context_menu();
            self.close_panel();
        }
    }

    fn on_overlay_dismiss(&mut self, generation: usize) {
        if generation != self.overlay_generation || self.tray_gate.has_active_menu() {
            return;
        }
        self.close_popover();
        self.close_context_menu();
        self.close_panel();
    }

    fn on_overlay_wheel(&mut self, generation: usize, delta: i16) {
        if generation != self.overlay_generation || self.tray_gate.has_active_menu() {
            return;
        }
        let open_hwnd = self
            .topbars
            .iter()
            .find_map(|b| b.popover.as_ref().filter(|p| p.is_open()).map(|p| p.hwnd));
        if let Some(hwnd) = open_hwnd {
            self.popover_wheel(hwnd, delta);
        }
    }

    fn on_overlay_key(&mut self, generation: usize, vk: u32) {
        if generation != self.overlay_generation || self.tray_gate.has_active_menu() {
            return;
        }
        if vk == VK_ESCAPE.0 as u32 {
            self.close_popover();
            self.close_context_menu();
            self.close_panel();
            return;
        }
        if self.context_menu.is_open() {
            self.menu_key_down(vk);
        }
    }

    fn panel_drag_move_screen(&mut self, generation: usize, sx: i32, sy: i32) {
        if generation != self.overlay_generation
            || !self.status_panel.is_open()
            || !self.status_panel.is_dragging_slider()
        {
            return;
        }
        let Some(wr) = crate::win32::window_rect(self.status_panel.hwnd) else {
            return;
        };
        self.panel_mouse_move(sx - wr.left, sy - wr.top);
    }

    fn panel_drag_up_screen(&mut self, generation: usize, sx: i32, sy: i32) {
        if generation != self.overlay_generation
            || !self.status_panel.is_open()
            || !self.status_panel.is_dragging_slider()
        {
            return;
        }
        let Some(wr) = crate::win32::window_rect(self.status_panel.hwnd) else {
            self.status_panel.cancel_drag();
            self.sync_overlay_hooks();
            return;
        };
        self.panel_lbutton_up(sx - wr.left, sy - wr.top);
    }

    fn toggle_status_panel(&mut self, bi: usize, kind: PanelKind, hit: Hit) {
        if self.status_panel.is_open()
            && self.status_panel.kind == kind
            && self.status_panel_bar.as_ref().is_some_and(|(idx, _)| *idx == bi)
        {
            self.close_panel();
            return;
        }
        let prev_fg = self.live_external_foreground();
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
        self.clear_tray_context();
        self.close_popover_immediate();
        self.close_context_menu_immediate();
        self.window_preview.close();
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
        let ptr = app_ptr();
        let main_hwnd = self.hwnd;
        let appearance = self.config.appearance.clone();
        self.status_panel_bar = Some((bi, hit));
        self.status_panel.open(ptr, main_hwnd, kind, anchor, dpi, prev_fg, &appearance);
        self.sync_topbar_active();
        self.mark_overlay_opened();
        if self.status_panel.is_animating() {
            self.ensure_frame();
        }
    }

    /// 触发状态面板平滑收起（若开启减弱动态效果则立即隐藏）。
    pub fn close_panel(&mut self) {
        let reduce = self.config.appearance.reduce_motion;
        let ptr = app_ptr() as isize;
        let sp_hwnd = self.status_panel.hwnd;
        if !sp_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, 0);
            }
        }
        let animating = self.status_panel.begin_close(reduce);
        if !sp_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, ptr);
            }
        }
        if !animating {
            self.status_panel_bar = None;
        }
        self.sync_topbar_active();
        self.sync_overlay_hooks();
        if animating {
            self.ensure_frame();
        } else {
            self.flush_pending_color_samples();
        }
    }

    /// 立即隐藏状态面板并清理高亮状态（用于切换独占浮层、退出、全屏让位或锁屏睡眠）。
    pub fn close_panel_immediate(&mut self) {
        let sp_hwnd = self.status_panel.hwnd;
        let ptr = app_ptr() as isize;
        if !sp_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, 0);
            }
        }
        self.status_panel.close();
        if !sp_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowLongPtrW(sp_hwnd, GWLP_USERDATA, ptr);
            }
        }
        self.status_panel_bar = None;
        self.sync_topbar_active();
        self.sync_overlay_hooks();
        self.flush_pending_color_samples();
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
        self.sync_overlay_hooks();
    }

    pub fn panel_lbutton_up(&mut self, x: i32, y: i32) {
        let act = self.status_panel.on_lbutton_up(self.hwnd, x, y);
        self.handle_panel_action(act);
        self.sync_overlay_hooks();
    }

    pub fn panel_capture_lost(&mut self) {
        // 非激活状态面板失捕时仅重置滑块拖拽态，避免误关面板
        self.status_panel.cancel_drag();
        self.sync_overlay_hooks();
    }

    fn on_panel_data(&mut self, lparam: isize) {
        let (token, data) = *unsafe { Box::from_raw(lparam as *mut (u64, NetworkData)) };
        self.status_panel.on_network_data(token, data);
        self.sync_overlay_hooks();
    }

    fn handle_panel_action(&mut self, act: PanelAction) {
        match act {
            PanelAction::None => {}
            PanelAction::Close => self.close_panel(),
            PanelAction::OpenSettings => {
                self.close_panel_immediate();
                self.open_settings(None);
            }
            PanelAction::NotificationCenter => {
                self.close_panel();
                crate::topbar::panel::trigger_notification_center();
            }
            PanelAction::LockScreen => {
                self.close_panel_immediate();
                self.on_menu(M_LOCK);
            }
            PanelAction::Sleep => {
                self.close_panel_immediate();
                self.on_menu(M_SLEEP);
            }
            PanelAction::VolumeChanged(v, m) => {
                for bar in &mut self.topbars {
                    bar.status.volume = Some((v, m));
                    invalidate(bar.hwnd);
                }
            }
            PanelAction::ImeChanged { close } => {
                let cur = status::ime_label(self.live_external_foreground());
                self.last_ime = cur.clone();
                for bar in &mut self.topbars {
                    bar.status.ime = cur.clone();
                    bar.layout(&self.config.topbar);
                    invalidate(bar.hwnd);
                }
                if close {
                    self.close_panel();
                }
            }
            PanelAction::ToggleTheme => {
                let next = if self.config.appearance.is_dark() { "light" } else { "dark" };
                self.config.appearance.theme = next.into();
                let _ = self.store.save_config(&self.config);
                self.apply_config(false);
                if self.status_panel.is_open() {
                    let prev_fg = self.live_external_foreground();
                    let bi = self.status_panel_bar.as_ref().map(|(idx, _)| *idx).unwrap_or(0);
                    let (anchor, dpi) = self
                        .topbars
                        .get(bi)
                        .map(|b| {
                            let mut wr = RECT::default();
                            let _ = unsafe { GetWindowRect(b.hwnd, &mut wr) };
                            let r = b.rect_of(&Hit::Control).unwrap_or_default();
                            (
                                RECT {
                                    left: wr.left + r.left,
                                    top: wr.top + r.top,
                                    right: wr.left + r.right,
                                    bottom: wr.top + r.bottom,
                                },
                                b.dpi,
                            )
                        })
                        .unwrap_or((RECT::default(), 96));
                    let ptr = app_ptr();
                    let main_hwnd = self.hwnd;
                    let appearance = self.config.appearance.clone();
                    self.status_panel.open(ptr, main_hwnd, PanelKind::Control, anchor, dpi, prev_fg, &appearance);
                    self.mark_overlay_opened();
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
        self.tray_tip.clear(self.hwnd);
        self.tray_gate.clear_suppressed();
        self.clear_tray_context();
        self.close_context_menu_immediate();
        self.close_panel_immediate();
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
        let prev_fg = self.live_external_foreground();
        let ptr = app_ptr();
        let appearance = self.config.appearance.clone();
        // 多屏模式下先立即收起其他显示器上的收纳浮层，保持单一活动浮层
        for (idx, other) in self.topbars.iter_mut().enumerate() {
            if idx != bi {
                if let Some(p) = other.popover.as_mut() {
                    if p.visible {
                        unsafe {
                            let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, 0);
                        }
                        p.close();
                        unsafe {
                            let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, ptr as isize);
                        }
                    }
                }
            }
        }
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
        let need_anim = if let Some(p) = bar.popover.as_mut() {
            p.open(anchor, items, pinned, prev_fg, &appearance);
            p.is_animating()
        } else {
            false
        };
        self.sync_topbar_active();
        self.mark_overlay_opened();
        if need_anim {
            self.ensure_frame();
        }
        self.refresh_tray();
    }

    fn clear_popover_tracking(&mut self) {
        if self.tray_tip.owner().is_some_and(|owner| self.popover_owner(owner).is_some()) {
            self.tray_tip.clear(self.hwnd);
        }
        if self
            .tray_gate
            .suppressed
            .as_ref()
            .is_some_and(|(owner, _)| self.popover_owner(*owner).is_some())
        {
            self.tray_gate.clear_suppressed();
        }
        // 仅当右键菜单确实来自收纳浮层时才联动清理上下文，避免关闭收纳格误伤顶部栏菜单跟踪
        if self.tray_gate.context.as_ref().is_some_and(|ctx| ctx.is_popover) {
            self.tray_gate.context = None;
            unsafe { let _ = KillTimer(self.hwnd, TIMER_TRAY_CONTEXT); }
        }
        if let TraySeed::Loading(pending) = &mut self.tray_seed {
            *pending = None;
        }
    }

    /// 触发收纳浮层平滑收起（若开启减弱动态效果则立即隐藏）。
    pub fn close_popover(&mut self) {
        self.clear_popover_tracking();
        let reduce = self.config.appearance.reduce_motion;
        let ptr = app_ptr() as isize;
        let mut animating = false;
        let mut finished_immediately = false;
        for bar in &mut self.topbars {
            if let Some(p) = bar.popover.as_mut() {
                if p.visible {
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, 0);
                    }
                    let still_anim = p.begin_close(reduce);
                    unsafe {
                        let _ = SetWindowLongPtrW(p.hwnd, GWLP_USERDATA, ptr);
                    }
                    invalidate(bar.hwnd);
                    if still_anim {
                        animating = true;
                    } else {
                        finished_immediately = true;
                    }
                }
            }
        }
        self.sync_topbar_active();
        self.sync_overlay_hooks();
        if animating {
            self.ensure_frame();
        }
        if finished_immediately && !self.topbars.iter().any(|b| b.popover.as_ref().is_some_and(|p| p.visible)) {
            self.flush_pending_color_samples();
        }
    }

    /// 立即隐藏所有收纳浮层并释放绘图缓冲（用于退出、重建与独占面板切换）。
    pub fn close_popover_immediate(&mut self) {
        self.clear_popover_tracking();
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
                    invalidate(bar.hwnd);
                    closed = true;
                }
            }
        }
        if closed {
            self.sync_topbar_active();
            self.flush_pending_color_samples();
        }
        self.sync_overlay_hooks();
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
        if !self.tray_gate.is_popover_owner(hwnd) {
            self.close_popover();
        }
    }

    fn track_tray_context(&mut self) {
        let Some(ctx) = self.tray_gate.context.as_ref() else { return };
        let Some(menu) = ctx.menu else { return };
        if !taskbar::is_visible(menu) {
            if ctx.is_popover {
                self.close_popover();
            } else {
                self.clear_tray_context();
            }
        }
    }

    pub fn popover_mouse_move(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            if !p.is_open() {
                return;
            }
            p.on_mouse_move(x, y);
        }
        self.update_tray_tip(hwnd, x, y);
    }

    pub fn popover_mouse_leave(&mut self, hwnd: HWND) {
        if self.tray_tip.owner() == Some(hwnd) {
            self.tray_tip.clear(self.hwnd);
        }
        // 忽略右键兜底归还光标期间滞后到达的 WM_MOUSELEAVE，仅在物理光标确实移出图标后解除抑制
        if self
            .tray_gate
            .suppressed
            .as_ref()
            .is_some_and(|(owner, key)| *owner == hwnd && !self.cursor_on_tray_item(*owner, key))
        {
            self.tray_gate.clear_suppressed();
        }
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            if p.is_open() {
                p.on_mouse_leave();
            }
        }
    }

    pub fn popover_wheel(&mut self, hwnd: HWND, delta: i16) {
        self.tray_tip.clear(self.hwnd);
        let Some(bi) = self.popover_owner(hwnd) else { return };
        let bar = &mut self.topbars[bi];
        if let Some(p) = bar.popover.as_mut() {
            if !p.is_open() {
                return;
            }
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
        self.tray_gate.clear_suppressed();
        let Some(bi) = self.popover_owner(hwnd) else { return };
        if let Some(p) = self.topbars[bi].popover.as_mut() {
            if !p.is_open() {
                return;
            }
            p.press = p.index_at(x, y);
            p.render();
        }
    }

    pub fn popover_rbutton_down(&mut self) {
        self.tray_tip.clear(self.hwnd);
    }

    pub fn popover_lbutton_up(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(bi) = self.popover_owner(hwnd) else { return };
        let Some(p) = self.topbars[bi].popover.as_mut() else { return };
        if !p.is_open() {
            return;
        }
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
        if !p.is_open() {
            return;
        }
        let Some(index) = p.index_at(x, y) else { return };
        let Some(item) = p.items.get(index).cloned() else { return };
        let Some(anchor) = p.cell_rect(index).and_then(|icon| tray_anchor(hwnd, icon)) else { return };
        let Some(p) = self.topbars[bi].popover.as_mut() else { return };
        p.set_context_key(Some(item.key.clone()));
        self.invoke_tray(TrayClick { item, anchor: Some(anchor), owner: hwnd, is_popover: true });
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
        WM_APP_DOCK_COLOR => app.on_dock_color(lparam.0),
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
        WM_APP_TRAY_COVER_ZORDER => app.raise_dock_above_tray_cover(),
        crate::topbar::tray_mirror::WM_APP_LIVE_ICON_CHANGED => app.redraw_shelf_views(),
        crate::topbar::panel::WM_APP_PANEL_DATA => app.on_panel_data(lparam.0),
        WM_APP_OVERLAY_DISMISS => app.on_overlay_dismiss(wparam.0),
        WM_APP_OVERLAY_WHEEL => app.on_overlay_wheel(wparam.0, lparam.0 as i16),
        WM_APP_OVERLAY_KEY => app.on_overlay_key(wparam.0, lparam.0 as u32),
        WM_APP_PANEL_DRAG_MOVE => app.panel_drag_move_screen(
            wparam.0,
            crate::win32::get_x_lparam(lparam.0),
            crate::win32::get_y_lparam(lparam.0),
        ),
        WM_APP_PANEL_DRAG_UP => app.panel_drag_up_screen(
            wparam.0,
            crate::win32::get_x_lparam(lparam.0),
            crate::win32::get_y_lparam(lparam.0),
        ),
        crate::topbar::WM_APP_APPBAR => {}
        WM_APP_UPDATE => app.on_update(lparam.0),
        crate::update::WM_APP_UPDATE_PROGRESS => app.on_update_progress(wparam.0),
        WM_HOTKEY => app.on_hotkey(wparam.0 as i32),
        WM_TIMER => app.on_timer(wparam.0),
        WM_DISPLAYCHANGE => {
            app.display_dirty_at = Some(Instant::now());
        }
        WM_SETTINGCHANGE => {
            // 仅在系统主题或壁纸切换时刷新配色，严格忽略 SPI_SETWORKAREA(0x002F) 防止 AppBar 死循环闪烁
            if crate::topbar::is_wallpaper_setting_change(wparam.0, lparam.0) {
                color_sync::invalidate_wallpaper_cache();
                if app.topbars.is_empty() {
                    app.on_theme_changed();
                }
            } else if crate::topbar::is_theme_setting_change(lparam.0) {
                app.on_theme_changed();
            }
        }
        WM_DPICHANGED => app.display_dirty_at = Some(Instant::now()),
        _ if msg == taskbar_created && taskbar_created != 0 => app.on_taskbar_created(),
        _ => {}
    });
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TopbarCfg;
    use crate::monitor::MaximizedWindow;

    #[test]
    fn transient_popups_do_not_break_maximized_bar_fusion() {
        let mut snap = MonitorState::default();
        snap.maximized_windows.push(MaximizedWindow {
            monitor: 1001,
            hwnd: 5001,
            exe: r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
            rect: RECT { left: 0, top: 30, right: 1920, bottom: 1080 },
        });
        snap.maximized_windows.push(MaximizedWindow {
            monitor: 1002,
            hwnd: 5002,
            exe: r"C:\Users\demo\AppData\Local\Programs\Microsoft VS Code\Code.exe".into(),
            rect: RECT { left: 1920, top: 30, right: 3840, bottom: 1080 },
        });

        let mut cfg = TopbarCfg::default();
        cfg.adaptive_maximized_color = true;
        cfg.adaptive_color_exclude = vec!["code.exe".into()];

        // Monitor 1001 有最大化 Chrome 且未被排除，即使前台瞬态变为 0 或系统浮窗，仍保持融合 Chrome
        assert_eq!(App::bar_fusion_target(&snap, 1001, &cfg), Some(5001));
        // Monitor 1002 为排除列表内的 VS Code，返回 None
        assert_eq!(App::bar_fusion_target(&snap, 1002, &cfg), None);
        // 无最大化窗口的显示器返回 None
        assert_eq!(App::bar_fusion_target(&snap, 9999, &cfg), None);

        // 全屏或关闭自适应融合开关时应统一退回玻璃色
        snap.fullscreen = true;
        assert_eq!(App::bar_fusion_target(&snap, 1001, &cfg), None);
        snap.fullscreen = false;
        cfg.adaptive_maximized_color = false;
        assert_eq!(App::bar_fusion_target(&snap, 1001, &cfg), None);
    }

    #[test]
    fn desktop_and_tray_classes_are_filtered_from_foreground_actions() {
        assert!(is_desktop_class("WinDockTopBar"));
        assert!(is_desktop_class("WinDockContextMenu"));
        assert!(!is_desktop_class(crate::settings::CLASS_SETTINGS));
        assert!(is_desktop_class("#32768"));
        assert!(is_desktop_class("Windows.UI.Input.InputSite.WindowClass"));
        assert!(is_desktop_class("TopLevelWindowForOverflowXamlIsland"));
        assert!(is_desktop_class("Xaml_WindowedPopupClass"));
        assert!(!is_desktop_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn hold_app_restores_nested_reentrancy_depth() {
        assert!(!handler_busy());
        hold_app(|| {
            assert!(handler_busy());
            hold_app(|| {
                assert!(handler_busy());
            });
            assert!(handler_busy());
        });
        assert!(!handler_busy());
    }

    #[test]
    fn restart_item_is_placed_directly_above_exit_in_menus() {
        assert_eq!(App::taskbar_menu_label_for(true, false), "显示 Windows 任务栏");
        assert_eq!(App::taskbar_menu_label_for(true, true), "重新隐藏 Windows 任务栏");
        assert_eq!(App::taskbar_menu_label_for(false, false), "隐藏 Windows 任务栏");

        let product = App::product_menu_items(true, true, "显示 Windows 任务栏");
        let exit_idx = product.iter().position(|it| !it.separator && it.id == M_EXIT).unwrap();
        assert_eq!(exit_idx, product.len() - 1);
        assert_eq!(product[exit_idx - 1].id, M_RESTART_APP);
        assert_eq!(product[exit_idx - 1].label, "重启 WinDock");
        assert!(!product[exit_idx - 1].danger);
        assert!(product[exit_idx].danger);

        let tray = App::tray_menu_items(true, true, "显示 Windows 任务栏", MenuItem::normal(M_STARTUP, "开机自启"));
        let tray_exit_idx = tray.iter().position(|it| !it.separator && it.id == M_EXIT).unwrap();
        assert_eq!(tray_exit_idx, tray.len() - 1);
        assert_eq!(tray[tray_exit_idx - 1].id, M_RESTART_APP);
        assert_eq!(tray[tray_exit_idx - 1].label, "重启 WinDock");
        assert!(!tray[tray_exit_idx - 1].danger);
    }

    #[test]
    fn restart_wait_args_include_guard_pid_only_when_nonzero() {
        assert_eq!(
            App::restart_wait_args(1234, 0),
            vec!["--restart-wait", "--parent-pid", "1234"]
        );
        assert_eq!(
            App::restart_wait_args(1234, 5678),
            vec!["--restart-wait", "--parent-pid", "1234", "--guard-pid", "5678"]
        );
    }

    #[test]
    fn dock_rightmost_settings_entry_uses_product_name() {
        let mut with_trash = Vec::new();
        App::append_builtin_right_entries(&mut with_trash, true);
        assert_eq!(with_trash.len(), 2);
        assert_eq!(with_trash[0].kind, FuncKind::Trash);
        assert_eq!(with_trash[0].label, "回收站");
        assert_eq!(with_trash[1].kind, FuncKind::Settings);
        assert_eq!(with_trash[1].label, "WinDock 设置");

        let mut without_trash = Vec::new();
        App::append_builtin_right_entries(&mut without_trash, false);
        assert_eq!(without_trash.len(), 1);
        assert_eq!(without_trash[0].kind, FuncKind::Settings);
        assert_eq!(without_trash[0].label, "WinDock 设置");
    }

    #[test]
    fn topbar_right_click_suppresses_only_source_icon_tooltip_and_allows_other_icons() {
        use crate::config::Appearance;
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
        let mk_item = |key: &str, name: &str, tip: &str| TrayItem {
            key: key.into(),
            name: name.into(),
            native_tip: Some(tip.into()),
            uia_key: Some(key.into()),
            registration: None,
            legacy_keys: Vec::new(),
            overflow: false,
            app_exe: None,
            app_hwnds: Vec::new(),
        };
        let items = vec![
            mk_item("k_a", "App A", "应用 A 状态"),
            mk_item("k_b", "App B", "应用 B 状态"),
        ];
        bar.tray_items = items.clone();
        bar.pinned = vec!["k_a".into(), "k_b".into()];
        bar.layout(&cfg);

        let mut popover = Popover::new(std::ptr::null_mut(), bar.dpi)
            .expect("Popover test window should create");
        popover.items = items;
        popover.visible = true;
        let pop_hwnd = popover.hwnd;
        bar.popover = Some(popover);

        let topbars = [bar];
        let bar_hwnd = topbars[0].hwnd;
        let rect_a = topbars[0].rect_of(&Hit::TrayIcon("k_a".into())).unwrap();
        let rect_b = topbars[0].rect_of(&Hit::TrayIcon("k_b".into())).unwrap();
        let center_of = |r: RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let (ax, ay) = center_of(rect_a);
        let (bx, by) = center_of(rect_b);

        let pop_ref = topbars[0].popover.as_ref().unwrap();
        let pop_rect_a = pop_ref.cell_rect(0).unwrap();
        let pop_rect_b = pop_ref.cell_rect(1).unwrap();
        let (pax, pay) = center_of(pop_rect_a);
        let (pbx, pby) = center_of(pop_rect_b);

        let menu_hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("Static"),
                w!(""),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                120,
                80,
                None,
                None,
                crate::win32::hinstance(),
                None,
            )
            .unwrap()
        };

        let mut gate = TrayTipGate::default();
        // 1) 无右键菜单时，顶部栏与收纳格的各图标都能正常解析出悬浮提示
        let (t, _, _, _) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, ax, ay);
        assert_eq!(t.as_ref().map(|v| v.key.as_str()), Some("k_a"));
        let (t, _, _, _) = App::resolve_tray_tip_target(&topbars, &gate, pop_hwnd, pax, pay);
        assert_eq!(t.as_ref().map(|v| v.key.as_str()), Some("k_a"));

        // 2) 顶部栏图标 k_a 弹出右键菜单期间：鼠标留在 k_a 不显示，移动到 k_b 正常显示
        gate.context = Some(TrayContext {
            owner: bar_hwnd,
            key: "k_a".into(),
            is_popover: false,
            menu: Some(menu_hwnd),
        });
        assert!(!gate.is_popover_owner(pop_hwnd));
        let (t_a, _, _, hk_a) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, ax, ay);
        assert!(t_a.is_none());
        assert_eq!(hk_a.as_deref(), Some("k_a"));
        let (t_b, _, _, hk_b) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, bx, by);
        assert_eq!(t_b.as_ref().map(|v| v.key.as_str()), Some("k_b"));
        assert_eq!(hk_b.as_deref(), Some("k_b"));
        // 同名图标在收纳格（不同宿主窗口）不受顶部栏右键菜单影响
        let (t_pop_a, _, _, _) = App::resolve_tray_tip_target(&topbars, &gate, pop_hwnd, pax, pay);
        assert_eq!(t_pop_a.as_ref().map(|v| v.key.as_str()), Some("k_a"));

        // 3) 菜单关闭后清理上下文，原图标恢复正常悬浮预览
        unsafe {
            let _ = ShowWindow(menu_hwnd, SW_HIDE);
        }
        assert!(!taskbar::is_visible(menu_hwnd));
        gate.context = None;
        let (t_a_after, _, _, _) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, ax, ay);
        assert_eq!(t_a_after.as_ref().map(|v| v.key.as_str()), Some("k_a"));

        // 4) 收纳格图标 k_a 弹出右键菜单期间：留在 k_a 不显示，移动到 k_b 正常显示
        unsafe {
            let _ = ShowWindow(menu_hwnd, SW_SHOWNA);
        }
        gate.context = Some(TrayContext {
            owner: pop_hwnd,
            key: "k_a".into(),
            is_popover: true,
            menu: Some(menu_hwnd),
        });
        assert!(gate.is_popover_owner(pop_hwnd));
        let (t_pa, from_pop, _, _) = App::resolve_tray_tip_target(&topbars, &gate, pop_hwnd, pax, pay);
        assert!(from_pop);
        assert!(t_pa.is_none());
        let (t_pb, _, _, _) = App::resolve_tray_tip_target(&topbars, &gate, pop_hwnd, pbx, pby);
        assert_eq!(t_pb.as_ref().map(|v| v.key.as_str()), Some("k_b"));

        // 5) 未捕获到菜单窗口句柄的兜底抑制：停留在 k_a 抑制，移动到 k_b 正常显示并解除旧抑制
        gate.context = None;
        gate.suppressed = Some((bar_hwnd, "k_a".into()));
        let (t_sup_a, _, _, hk_sup_a) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, ax, ay);
        assert!(t_sup_a.is_none());
        gate.update_hover(bar_hwnd, hk_sup_a.as_deref());
        assert!(gate.suppressed.is_some());
        let (t_sup_b, _, _, hk_sup_b) = App::resolve_tray_tip_target(&topbars, &gate, bar_hwnd, bx, by);
        assert_eq!(t_sup_b.as_ref().map(|v| v.key.as_str()), Some("k_b"));
        gate.update_hover(bar_hwnd, hk_sup_b.as_deref());
        assert!(gate.suppressed.is_none());

        // 6) 验证 TrayTooltip::track 与 target_id 在抑制和恢复路径下的真实状态联动
        let mut tip = TrayTooltip::default();
        tip.track(bar_hwnd, t_a_after, false);
        assert_eq!(tip.target_id(), Some((bar_hwnd, "k_a")));
        tip.track(bar_hwnd, t_a, false);
        assert_eq!(tip.target_id(), None);
        tip.track(bar_hwnd, t_b, false);
        assert_eq!(tip.target_id(), Some((bar_hwnd, "k_b")));
        tip.clear(bar_hwnd);
        assert_eq!(tip.target_id(), None);

        unsafe {
            let _ = DestroyWindow(menu_hwnd);
        }
        for mut b in topbars {
            b.destroy();
        }
    }

    #[test]
    fn non_activating_overlay_mouse_hook_routes_clicks_wheel_and_slider_drag() {
        let card = RECT { left: 1600, top: 34, right: 1880, bottom: 260 };
        let toggle = RECT { left: 1700, top: 0, right: 1740, bottom: 30 };
        let mut st = OverlayHookState {
            main_hwnd: 100,
            generation: 7,
            overlay_rects: vec![card],
            topbar_toggle_rects: vec![toggle],
            popover_rect: Some(card),
            context_menu_open: false,
            context_menu_has_hover: false,
            panel_dragging_slider: false,
            panel_win_rect: Some(RECT { left: 1600, top: 30, right: 1880, bottom: 260 }),
            tray_menu_active: false,
        };

        // 点击浮层卡片内部与顶部栏切换按钮本身不提前关闭浮层，点透明间隙或外部应用则关闭
        assert_eq!(eval_mouse_ll(&st, WM_LBUTTONDOWN, POINT { x: 1700, y: 100 }, 0), MouseHookDecision::PassThrough);
        assert_eq!(eval_mouse_ll(&st, WM_LBUTTONDOWN, POINT { x: 1720, y: 15 }, 0), MouseHookDecision::PassThrough);
        assert_eq!(eval_mouse_ll(&st, WM_LBUTTONDOWN, POINT { x: 1700, y: 31 }, 0), MouseHookDecision::DismissOverlay);
        assert_eq!(eval_mouse_ll(&st, WM_RBUTTONDOWN, POINT { x: 400, y: 500 }, 0), MouseHookDecision::DismissOverlay);

        // 收纳浮层悬停滚轮直接转发并消费，外部滚轮透传
        assert_eq!(eval_mouse_ll(&st, WM_MOUSEWHEEL, POINT { x: 1700, y: 100 }, -120), MouseHookDecision::ConsumePopoverWheel(-120));
        assert_eq!(eval_mouse_ll(&st, WM_MOUSEWHEEL, POINT { x: 500, y: 500 }, -120), MouseHookDecision::PassThrough);

        // 原生托盘右键菜单接管期间放行外部点击与滚轮，避免误关收纳格
        st.tray_menu_active = true;
        assert_eq!(eval_mouse_ll(&st, WM_LBUTTONDOWN, POINT { x: 400, y: 500 }, 0), MouseHookDecision::PassThrough);
        assert_eq!(eval_mouse_ll(&st, WM_MOUSEWHEEL, POINT { x: 1700, y: 100 }, 120), MouseHookDecision::PassThrough);

        // 滑块拖拽期间鼠标移出面板范围仍补传移动与抬起，移回面板内部则走窗口原生消息
        st.tray_menu_active = false;
        st.panel_dragging_slider = true;
        assert_eq!(eval_mouse_ll(&st, WM_MOUSEMOVE, POINT { x: 1900, y: 120 }, 0), MouseHookDecision::ForwardPanelDragMove);
        assert_eq!(eval_mouse_ll(&st, WM_MOUSEMOVE, POINT { x: 1700, y: 120 }, 0), MouseHookDecision::PassThrough);
        assert_eq!(eval_mouse_ll(&st, WM_LBUTTONUP, POINT { x: 1900, y: 120 }, 0), MouseHookDecision::ForwardPanelDragUp);
    }

    #[test]
    fn non_activating_overlay_keyboard_hook_routes_escape_win_and_menu_nav() {
        let mut st = OverlayHookState {
            main_hwnd: 100,
            generation: 3,
            overlay_rects: vec![RECT { left: 10, top: 34, right: 200, bottom: 200 }],
            topbar_toggle_rects: Vec::new(),
            popover_rect: Some(RECT { left: 10, top: 34, right: 200, bottom: 200 }),
            context_menu_open: false,
            context_menu_has_hover: false,
            panel_dragging_slider: false,
            panel_win_rect: None,
            tray_menu_active: false,
        };

        // 非菜单浮层仅拦截 Esc，并允许 Win 键在收起浮层后继续打开系统开始菜单
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_ESCAPE.0 as u32, false), KeyHookDecision::ConsumeAndDispatch(VK_ESCAPE.0 as u32));
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_DOWN.0 as u32, false), KeyHookDecision::PassThrough);
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_LWIN.0 as u32, false), KeyHookDecision::PassAndDismiss);
        assert_eq!(eval_keyboard_ll(&st, WM_SYSKEYDOWN, VK_RWIN.0 as u32, false), KeyHookDecision::PassAndDismiss);
        assert_eq!(eval_keyboard_ll(&st, WM_KEYUP, VK_ESCAPE.0 as u32, false), KeyHookDecision::PassThrough);

        // 下拉菜单打开后拦截上下/回车，但保留 Alt 组合键；原生托盘菜单接管期间完全放行
        st.context_menu_open = true;
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_DOWN.0 as u32, false), KeyHookDecision::ConsumeAndDispatch(VK_DOWN.0 as u32));
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_UP.0 as u32, false), KeyHookDecision::ConsumeAndDispatch(VK_UP.0 as u32));
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_RETURN.0 as u32, false), KeyHookDecision::ConsumeAndDispatch(VK_RETURN.0 as u32));
        assert_eq!(eval_keyboard_ll(&st, WM_SYSKEYDOWN, VK_RETURN.0 as u32, true), KeyHookDecision::PassThrough);
        st.tray_menu_active = true;
        assert_eq!(eval_keyboard_ll(&st, WM_KEYDOWN, VK_ESCAPE.0 as u32, false), KeyHookDecision::PassThrough);
    }

    #[test]
    fn topbar_toggle_hits_point_packing_and_ll_hooks_work() {
        for hit in [
            Hit::ProductMenu,
            Hit::AppMenu,
            Hit::MenuFile,
            Hit::MenuWindow,
            Hit::MenuGo,
            Hit::TrayToggle,
            Hit::Ime,
            Hit::Network,
            Hit::Brightness,
            Hit::Volume,
            Hit::Battery,
            Hit::Control,
            Hit::Clock,
        ] {
            assert!(is_topbar_overlay_toggle_hit(&hit));
        }
        assert!(!is_topbar_overlay_toggle_hit(&Hit::TrayIcon("k".into())));

        let packed = pack_point_lparam(-1920, -300);
        assert_eq!(crate::win32::get_x_lparam(packed), -1920);
        assert_eq!(crate::win32::get_y_lparam(packed), -300);

        unsafe {
            let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(overlay_mouse_ll_proc), None, 0)
                .expect("WH_MOUSE_LL hook should install");
            let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(overlay_keyboard_ll_proc), None, 0)
                .expect("WH_KEYBOARD_LL hook should install");
            assert!(UnhookWindowsHookEx(mh).is_ok());
            assert!(UnhookWindowsHookEx(kh).is_ok());
        }
    }

    #[test]
    fn bottom_taskbar_zone_matches_monitor_dpi_and_bounds() {
        let primary = MonitorInfo {
            hmonitor: windows::Win32::Graphics::Gdi::HMONITOR::default(),
            rect: RECT { left: 0, top: 0, right: 2560, bottom: 1600 },
            work: RECT { left: 0, top: 0, right: 2560, bottom: 1600 },
            dpi: 144, // 150% DPI -> 96dp = 144px
            primary: true,
        };
        assert!(cursor_in_bottom_taskbar_zone(POINT { x: 1200, y: 1599 }, primary));
        assert!(cursor_in_bottom_taskbar_zone(POINT { x: 1200, y: 1456 }, primary));
        assert!(!cursor_in_bottom_taskbar_zone(POINT { x: 1200, y: 1455 }, primary));
        assert!(!cursor_in_bottom_taskbar_zone(POINT { x: -1, y: 1580 }, primary));

        let left_monitor = MonitorInfo {
            hmonitor: windows::Win32::Graphics::Gdi::HMONITOR::default(),
            rect: RECT { left: -1920, top: 0, right: 0, bottom: 1080 },
            work: RECT { left: -1920, top: 0, right: 0, bottom: 1080 },
            dpi: 96, // 100% DPI -> 96dp = 96px
            primary: false,
        };
        assert!(cursor_in_bottom_taskbar_zone(POINT { x: -500, y: 984 }, left_monitor));
        assert!(!cursor_in_bottom_taskbar_zone(POINT { x: -500, y: 983 }, left_monitor));
    }
}
