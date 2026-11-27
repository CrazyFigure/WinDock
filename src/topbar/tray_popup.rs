//! 托盘采集与右键交互使用的桌面遮罩和原生菜单定位。

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{mpsc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BitBlt, GetDC, MonitorFromRect, ReleaseDC, CAPTUREBLT, MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::window_pid;
use crate::ui::DibTarget;
use crate::win32::{class_name, monitor_info, monitor_of_window};

/// 遮罩只复制目标区域的原桌面，Explorer 临时展开期间继续显示这块内容。
/// WinDock 创建遮罩，短暂以原生面板作为所有者，使其保持在面板前方。
/// 自己的消息线程持续派发窗口消息，Explorer 切换焦点和 Z 序时无需等待 UIA 线程。
pub(super) struct DesktopCover {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

// 只在遮罩线程内处理，唤出请求不会派发给任何其他窗口。
const WM_WAKE_TRAY: u32 = WM_APP + 1;

impl DesktopCover {
    fn new(owner: HWND, bounds: RECT) -> Option<Self> {
        let owner = owner.0 as isize;
        let (ready, status) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("tray-cover".into())
            .spawn(move || {
                let Some(_window) = CoverWindow::new(HWND(owner as *mut _), bounds) else {
                    return;
                };
                unsafe {
                    if ready.send(GetCurrentThreadId()).is_err() {
                        return;
                    }
                    let mut msg = MSG::default();
                    let mut wake = None;
                    let mut wake_timer = 0;
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        match msg.message {
                            WM_WAKE_TRAY if wake.is_none() => {
                                wake = AttentionWindow::new(bounds);
                                if wake.is_some() {
                                    // 等 Explorer 为临时窗口创建任务栏按钮，再请求一次提示。
                                    wake_timer = SetTimer(None, 0, 260, None);
                                }
                            }
                            WM_TIMER if wake_timer != 0 && msg.wParam.0 == wake_timer => {
                                let _ = KillTimer(None, wake_timer);
                                if let Some(wake) = &wake {
                                    wake.flash(FLASHW_TRAY);
                                }
                                wake_timer = 0;
                            }
                            _ => {
                                DispatchMessageW(&msg);
                            }
                        }
                    }
                    if wake_timer != 0 {
                        let _ = KillTimer(None, wake_timer);
                    }
                }
            })
            .ok()?;
        let thread_id = status.recv_timeout(Duration::from_millis(500)).ok()?;
        Some(Self {
            thread_id,
            thread: Some(thread),
        })
    }

    /// 用 WinDock 自己的临时任务栏按钮请求展开，不激活 Explorer 或移动光标。
    pub fn wake_without_focus(&self) -> bool {
        unsafe { PostThreadMessageW(self.thread_id, WM_WAKE_TRAY, WPARAM(0), LPARAM(0)).is_ok() }
    }
}

impl Drop for DesktopCover {
    fn drop(&mut self) {
        unsafe {
            if PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok() {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
    }
}

struct CoverWindow(HWND);

impl CoverWindow {
    fn new(owner: HWND, bounds: RECT) -> Option<Self> {
        let w = bounds.right - bounds.left;
        let h = bounds.bottom - bounds.top;
        // 单块遮罩最多 8 MiB，防止异常窗口尺寸分配过大的位图。
        let mut image = DibTarget::default();
        if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > 2_097_152 || !image.ensure(w, h) {
            return None;
        }
        let memory = image.mem_dc?;
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let copied = BitBlt(
                memory,
                0,
                0,
                w,
                h,
                screen,
                bounds.left,
                bounds.top,
                SRCCOPY | CAPTUREBLT,
            );
            ReleaseDC(None, screen);
            copied.ok()?;
            // Owned 窗口随对应的 Explorer 面板调整 Z 序，但输入仍穿过屏幕复制区域。
            let cover = Self(
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST
                        | WS_EX_NOACTIVATE
                        | WS_EX_TRANSPARENT,
                    w!("Static"),
                    w!("WinDock tray cover"),
                    WS_POPUP | WS_DISABLED,
                    bounds.left,
                    bounds.top,
                    w,
                    h,
                    owner,
                    None,
                    crate::win32::hinstance(),
                    None,
                )
                .ok()?,
            );
            let point = POINT {
                x: bounds.left,
                y: bounds.top,
            };
            let size = SIZE { cx: w, cy: h };
            UpdateLayeredWindow(
                cover.0,
                None,
                Some(&point),
                Some(&size),
                memory,
                Some(&POINT::default()),
                COLORREF(0),
                None,
                ULW_OPAQUE,
            )
            .ok()?;
            SetWindowPos(
                cover.0,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .ok()?;
            // 遮罩交给 DWM 后才允许 Explorer 显示，避免底部先显示一帧任务栏。
            let _ = DwmFlush();
            Some(cover)
        }
    }
}

impl Drop for CoverWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// 只有任务栏按钮需要接收 FlashWindowEx。临时窗口放在所有屏幕之外，禁用激活。
struct AttentionWindow(HWND);

impl AttentionWindow {
    fn new(bounds: RECT) -> Option<Self> {
        static CLASS: OnceLock<bool> = OnceLock::new();
        let name = w!("WinDockTrayWake");
        let instance = crate::win32::hinstance();
        unsafe {
            if !CLASS.get_or_init(|| {
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(attention_proc),
                    hInstance: instance,
                    lpszClassName: name,
                    ..Default::default()
                }) != 0
            }) {
                return None;
            }
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_NOACTIVATE,
                name,
                w!("WinDock"),
                WS_VISIBLE | WS_POPUP,
                bounds.left + (bounds.right - bounds.left) / 2,
                GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN) + 1,
                1,
                1,
                None,
                None,
                instance,
                None,
            )
            .ok()
            .map(Self)
        }
    }

    fn flash(&self, action: FLASHWINFO_FLAGS) {
        unsafe {
            let _ = FlashWindowEx(&FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd: self.0,
                dwFlags: action,
                uCount: 1,
                dwTimeout: 0,
            });
        }
    }
}

unsafe extern "system" fn attention_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

impl Drop for AttentionWindow {
    fn drop(&mut self) {
        self.flash(FLASHW_STOP);
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// 自动隐藏的任务栏定位可能只剩屏幕边缘的两像素，将完整尺寸投影回所属屏幕内。
pub(super) fn taskbar_cover(tray: HWND) -> Option<DesktopCover> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(tray, &mut bounds).ok()? };
    let screen = monitor_of_window(tray)?.rect;
    DesktopCover::new(tray, revealed_bounds(bounds, screen))
}

fn revealed_bounds(bounds: RECT, screen: RECT) -> RECT {
    let width = (bounds.right - bounds.left).min(screen.right - screen.left);
    let height = (bounds.bottom - bounds.top).min(screen.bottom - screen.top);
    let left = bounds.left.clamp(screen.left, screen.right - width);
    let top = bounds.top.clamp(screen.top, screen.bottom - height);
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 从顶部栏图标下方打开原生菜单，空间不足时向上弹，并限制在该屏幕的工作区内。
fn menu_position(anchor: RECT, work: RECT, width: i32, height: i32, gap: i32) -> POINT {
    let left = anchor
        .left
        .clamp(work.left, (work.right - width).max(work.left));
    let bottom = anchor.bottom.saturating_add(gap).max(work.top);
    let max_top = (work.bottom - height).max(work.top);
    let top = if bottom <= max_top {
        bottom
    } else {
        (anchor.top - gap - height).clamp(work.top, max_top)
    };
    POINT { x: left, y: top }
}

#[derive(Debug, PartialEq, Eq)]
enum PopupKind {
    NativeMenu,
    ToolkitMenu,
    ToolWindow,
}

/// 直接识别标准菜单和 Qt 菜单；其他工具浮窗还需结合所属进程及生成位置确认。
fn popup_kind(class: &str, style: WINDOW_STYLE, ex_style: WINDOW_EX_STYLE) -> Option<PopupKind> {
    if class == "#32768" {
        Some(PopupKind::NativeMenu)
    } else if !style.contains(WS_CAPTION)
        && ex_style.contains(WS_EX_TOOLWINDOW)
        && !ex_style.contains(WS_EX_APPWINDOW)
    {
        if class.starts_with("Qt") && class.contains("QWindowPopup") {
            Some(PopupKind::ToolkitMenu)
        } else if style.contains(WS_POPUP) {
            Some(PopupKind::ToolWindow)
        } else {
            None
        }
    } else {
        None
    }
}

fn window_popup_kind(hwnd: HWND) -> Option<PopupKind> {
    unsafe {
        popup_kind(
            &class_name(hwnd),
            WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32),
            WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32),
        )
    }
}

fn popup_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        if IsWindowVisible(hwnd).as_bool() && window_popup_kind(hwnd).is_some() {
            (*(data.0 as *mut Vec<HWND>)).push(hwnd);
        }
        true.into()
    }
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

/// 判断光标在等待菜单期间是否仅发生轻微抖动（未被用户主动大幅移走）。
fn within_cursor_jitter(current: POINT, target: POINT, tolerance: i32) -> bool {
    (current.x - target.x).abs() <= tolerance && (current.y - target.y).abs() <= tolerance
}

/// 若光标仍在临时点击位置附近，将其还原到用户原先在顶部栏点击的位置。
fn restore_cursor_near(original: POINT, moved_to: POINT, tolerance: i32) -> bool {
    let mut current = POINT::default();
    unsafe {
        if GetCursorPos(&mut current).is_ok()
            && current != original
            && within_cursor_jitter(current, moved_to, tolerance)
        {
            return SetCursorPos(original.x, original.y).is_ok();
        }
    }
    false
}

/// 未知进程的托盘项只接受紧邻真实托盘按钮的新菜单。
fn touches_source(rect: RECT, source: POINT, tolerance: i32) -> bool {
    let within_x = (rect.left - tolerance..=rect.right + tolerance).contains(&source.x);
    let within_y = (rect.top - tolerance..=rect.bottom + tolerance).contains(&source.y);
    within_x && within_y
}

pub(super) struct MenuPlacement {
    old_windows: Vec<HWND>,
    owner_pids: HashSet<u32>,
    anchor: RECT,
    source: POINT,
    live_source: Option<POINT>,
    cursor: Option<POINT>,
    placed: bool,
    observed: Option<HWND>,
}

pub(super) struct OpenedMenu {
    pub dispatched: bool,
    pub window: Option<HWND>,
    pub used_mouse: bool,
}

thread_local! {
    // 进程外 WinEvent 在注册它的托盘工作线程回调，只跟踪当前一次菜单请求。
    static PENDING_MENU: RefCell<Option<MenuPlacement>> = const { RefCell::new(None) };
}

unsafe extern "system" fn on_popup(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    PENDING_MENU.with(|slot| {
        if let Ok(mut pending) = slot.try_borrow_mut() {
            if let Some(placement) = pending.as_mut() {
                placement.try_move(hwnd);
            }
        }
    });
}

impl MenuPlacement {
    pub fn new(
        anchor: RECT,
        source: POINT,
        cursor: Option<POINT>,
        app_windows: &[isize],
        tray_owners: &[u32],
    ) -> Self {
        Self {
            old_windows: popup_windows(),
            owner_pids: tray_owners
                .iter()
                .copied()
                .chain(
                    app_windows
                        .iter()
                        .map(|raw| window_pid(HWND(*raw as *mut _))),
                )
                .filter(|pid| *pid != 0)
                .collect(),
            anchor,
            source,
            live_source: None,
            cursor,
            placed: false,
            observed: None,
        }
    }

    /// 鼠标兜底在等待 Explorer 动画稳定后更新真实落点，供自绘菜单位置校验使用。
    pub fn set_live_source(point: POINT) {
        PENDING_MENU.with(|slot| {
            if let Ok(mut pending) = slot.try_borrow_mut() {
                if let Some(placement) = pending.as_mut() {
                    placement.live_source = Some(point);
                }
            }
        });
    }

    /// 先监听应用菜单的出现事件再发送输入，尽早搬移窗口。旧版菜单缺少事件时短时补查。
    pub fn open_at(
        self,
        mouse_first: bool,
        uia: impl FnOnce() -> bool,
        mouse: impl FnOnce() -> (bool, Option<POINT>),
    ) -> OpenedMenu {
        let pid = if self.owner_pids.len() == 1 {
            *self.owner_pids.iter().next().unwrap_or(&0)
        } else {
            0
        };
        let original_cursor = self.cursor;
        let dpi = unsafe { MonitorFromRect(&self.anchor, MONITOR_DEFAULTTONEAREST) };
        let jitter = monitor_info(dpi).map_or(24, |m| crate::win32::dp(24, m.dpi));
        PENDING_MENU.with(|slot| slot.replace(Some(self)));
        let hooks = unsafe {
            [EVENT_SYSTEM_MENUPOPUPSTART, EVENT_OBJECT_SHOW].map(|event| {
                SetWinEventHook(
                    event,
                    event,
                    None,
                    Some(on_popup),
                    pid,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            })
        };
        // 先走该图标最近成功的方式。收到真实菜单窗口后立即结束等待。
        let (first, second, moved_to) = if mouse_first {
            let (sent, moved) = mouse();
            let observed = sent && Self::wait_for_menu(Duration::from_millis(370), original_cursor, moved, jitter);
            let retried = !observed && uia();
            if retried {
                Self::wait_for_menu(Duration::from_millis(180), None, None, jitter);
            }
            (sent, retried, moved)
        } else {
            let sent = uia();
            let observed = sent && Self::wait_for_menu(Duration::from_millis(180), None, None, jitter);
            let (retried, moved) = if observed {
                (false, None)
            } else {
                mouse()
            };
            if retried {
                Self::wait_for_menu(Duration::from_millis(370), original_cursor, moved, jitter);
            }
            (sent, retried, moved)
        };
        if let (Some(cursor), Some(moved)) = (original_cursor, moved_to) {
            restore_cursor_near(cursor, moved, jitter);
        }
        for hook in hooks.into_iter().filter(|hook| !hook.is_invalid()) {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
        let state = PENDING_MENU.with(|slot| slot.replace(None));
        OpenedMenu {
            dispatched: first || second,
            window: state.and_then(|state| state.observed),
            used_mouse: if second { !mouse_first } else { mouse_first },
        }
    }

    fn wait_for_menu(
        timeout: Duration,
        original_cursor: Option<POINT>,
        moved_to: Option<POINT>,
        jitter: i32,
    ) -> bool {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut cursor_restored = false;
        let mut msg = MSG::default();
        loop {
            // 消息检查也会分发排队的进程外 WinEvent。
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
            let (placed, observed) = PENDING_MENU.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(placement) = slot.as_mut() else {
                    return (false, false);
                };
                if !placement.placed {
                    for hwnd in popup_windows() {
                        if placement.try_move(hwnd) {
                            break;
                        }
                    }
                }
                (placement.placed, placement.observed.is_some())
            });
            // 菜单已定位或短暂宽限期已过时立刻归还光标，避免鼠标长期停留在底部任务栏或溢出区。
            if !cursor_restored && (placed || start.elapsed() >= Duration::from_millis(65)) {
                if let (Some(cursor), Some(moved)) = (original_cursor, moved_to) {
                    restore_cursor_near(cursor, moved, jitter);
                }
                cursor_restored = true;
            }
            if placed || Instant::now() >= deadline {
                return observed;
            }
            unsafe { MsgWaitForMultipleObjectsEx(None, 8, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        }
    }

    fn try_move(&mut self, hwnd: HWND) -> bool {
        if self.placed
            || self.old_windows.contains(&hwnd)
            || !unsafe { IsWindowVisible(hwnd).as_bool() }
        {
            return false;
        }
        let Some(kind) = window_popup_kind(hwnd) else {
            return false;
        };
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect).is_err() }
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return false;
        }
        let monitor = unsafe { MonitorFromRect(&self.anchor, MONITOR_DEFAULTTONEAREST) };
        let Some(monitor) = monitor_info(monitor) else {
            return false;
        };
        let gap = crate::win32::dp(4, monitor.dpi);
        let tolerance = crate::win32::dp(48, monitor.dpi);
        let anchor = POINT {
            x: self.anchor.left,
            y: self.anchor.bottom,
        };
        let nearby = self
            .live_source
            .is_some_and(|point| touches_source(rect, point, tolerance))
            || touches_source(rect, self.source, tolerance)
            || touches_source(rect, anchor, tolerance)
            || self
                .cursor
                .is_some_and(|cursor| touches_source(rect, cursor, tolerance));
        let owned = self.owner_pids.contains(&window_pid(hwnd));
        // 通用工具窗同时需要进程归属和相关落点，避免误移动同进程的通知。
        let matches = match kind {
            PopupKind::NativeMenu => owned || (self.owner_pids.is_empty() && nearby),
            PopupKind::ToolkitMenu => owned,
            PopupKind::ToolWindow => owned && nearby,
        };
        if !matches || unsafe { IsHungAppWindow(hwnd).as_bool() } {
            return false;
        }
        self.observed = Some(hwnd);
        let point = menu_position(
            self.anchor,
            monitor.work,
            rect.right - rect.left,
            rect.bottom - rect.top,
            gap,
        );
        self.placed = unsafe {
            SetWindowPos(
                hwnd,
                None,
                point.x,
                point.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            )
            .is_ok()
        };
        self.placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_menu_stays_inside_the_clicked_monitor() {
        let work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let top_icon = RECT {
            left: -60,
            top: 0,
            right: -32,
            bottom: 32,
        };
        let point = menu_position(top_icon, work, 250, 300, 4);
        assert_eq!((point.x, point.y), (-250, 36));
        let bottom_icon = RECT {
            top: 1030,
            bottom: 1060,
            ..top_icon
        };
        assert_eq!(menu_position(bottom_icon, work, 250, 300, 4).y, 726);
    }

    #[test]
    fn cover_includes_the_fully_revealed_taskbar() {
        let screen = RECT {
            left: 0,
            top: 0,
            right: 3072,
            bottom: 1920,
        };
        let tucked = RECT {
            left: 0,
            top: 1918,
            right: 3072,
            bottom: 2014,
        };
        assert_eq!(revealed_bounds(tucked, screen).top, 1824);
    }

    #[test]
    fn recognizes_toolkit_popup_without_accepting_normal_windows() {
        assert_eq!(
            popup_kind("#32768", WS_POPUP, WS_EX_TOOLWINDOW),
            Some(PopupKind::NativeMenu)
        );
        assert_eq!(
            popup_kind(
                "Qt683QWindowPopupSaveBits",
                WS_VISIBLE,
                WS_EX_TOOLWINDOW | WS_EX_LAYERED
            ),
            Some(PopupKind::ToolkitMenu)
        );
        assert_eq!(
            popup_kind("Application", WS_POPUP | WS_CAPTION, WS_EX_TOOLWINDOW),
            None
        );
        assert_eq!(
            popup_kind("Application", WS_POPUP, WS_EX_TOOLWINDOW | WS_EX_APPWINDOW),
            None
        );
    }

    #[test]
    fn cursor_jitter_tolerance_allows_small_hand_tremor_and_rejects_intentional_move() {
        let moved = POINT { x: 495, y: 1621 };
        assert!(within_cursor_jitter(moved, moved, 48));
        assert!(within_cursor_jitter(POINT { x: 503, y: 1615 }, moved, 48));
        assert!(!within_cursor_jitter(POINT { x: 600, y: 1621 }, moved, 48));
    }

    #[test]
    fn tool_window_menu_moves_to_topbar_when_overflow_button_shifts_before_click() {
        unsafe {
            let stale_source = POINT { x: 495, y: 1740 };
            let settled_click = POINT { x: 495, y: 1621 };
            let anchor = RECT {
                left: 641,
                top: 13,
                right: 701,
                bottom: 59,
            };
            let menu_w = 268;
            let menu_h = 312;

            // 先创建同进程不相关位置的通知工具窗，再创建点击位置处的自定义托盘菜单。
            let unrelated = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("Static"),
                w!(""),
                WS_POPUP | WS_VISIBLE,
                2500,
                1600,
                200,
                80,
                None,
                None,
                crate::win32::hinstance(),
                None,
            )
            .unwrap();
            let custom_menu = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("Static"),
                w!(""),
                WS_POPUP | WS_VISIBLE,
                settled_click.x,
                settled_click.y - menu_h,
                menu_w,
                menu_h,
                None,
                None,
                crate::win32::hinstance(),
                None,
            )
            .unwrap();

            let mut placement = MenuPlacement::new(
                anchor,
                stale_source,
                Some(POINT { x: 670, y: 30 }),
                &[custom_menu.0 as isize],
                &[],
            );
            // 仅将与按钮在动画前创建并存在于 old_windows 之外的窗口纳入测试。
            placement.old_windows.retain(|&h| h != unrelated && h != custom_menu);
            placement.live_source = Some(settled_click);

            assert!(!placement.try_move(unrelated));
            assert!(placement.try_move(custom_menu));

            let mut moved = RECT::default();
            GetWindowRect(custom_menu, &mut moved).unwrap();
            let monitor = monitor_info(MonitorFromRect(&anchor, MONITOR_DEFAULTTONEAREST)).unwrap();
            let expected = menu_position(
                anchor,
                monitor.work,
                menu_w,
                menu_h,
                crate::win32::dp(4, monitor.dpi),
            );
            assert_eq!((moved.left, moved.top), (expected.x, expected.y));

            let _ = DestroyWindow(unrelated);
            let _ = DestroyWindow(custom_menu);
        }
    }

    #[test]
    fn open_at_falls_back_to_mouse_and_captures_shifted_tool_window() {
        unsafe {
            let stale_source = POINT { x: 495, y: 1740 };
            let settled_click = POINT { x: 495, y: 1621 };
            let anchor = RECT {
                left: 641,
                top: 13,
                right: 701,
                bottom: 59,
            };
            let menu_w = 268;
            let menu_h = 312;

            let custom_menu = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("Static"),
                w!(""),
                WS_POPUP,
                settled_click.x,
                settled_click.y - menu_h,
                menu_w,
                menu_h,
                None,
                None,
                crate::win32::hinstance(),
                None,
            )
            .unwrap();

            let mut placement = MenuPlacement::new(
                anchor,
                stale_source,
                Some(POINT { x: 670, y: 30 }),
                &[custom_menu.0 as isize],
                &[],
            );
            placement.old_windows.retain(|&h| h != custom_menu);

            let opened = placement.open_at(
                false,
                || false,
                || {
                    MenuPlacement::set_live_source(settled_click);
                    let _ = ShowWindow(custom_menu, SW_SHOWNA);
                    (true, Some(settled_click))
                },
            );

            assert!(opened.dispatched);
            assert!(opened.used_mouse);
            assert_eq!(opened.window, Some(custom_menu));

            let mut moved = RECT::default();
            GetWindowRect(custom_menu, &mut moved).unwrap();
            let monitor = monitor_info(MonitorFromRect(&anchor, MONITOR_DEFAULTTONEAREST)).unwrap();
            let expected = menu_position(
                anchor,
                monitor.work,
                menu_w,
                menu_h,
                crate::win32::dp(4, monitor.dpi),
            );
            assert_eq!((moved.left, moved.top), (expected.x, expected.y));

            let _ = DestroyWindow(custom_menu);
        }
    }
}
