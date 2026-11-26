//! 系统托盘 UIA 发现、图标缓存、点击/右键透传、展开/固定状态管理。
//!
//! 零注入方案：不向 Explorer 注入 DLL，通过 UI Automation 枚举本会话
//! `Shell_TrayWnd`（TrayNotifyWnd）与溢出窗口中的托盘按钮；遍历在受控工作线程
//! 执行。采集线程只读取已经显示的托盘，用户点击时才请求原生任务栏展开。


use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, Interface, VARIANT};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::GdiFlush;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationInvokePattern, TreeScope_Descendants,
    UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId, UIA_ControlTypePropertyId, UIA_ImageControlTypeId,
    UIA_InvokePatternId, UIA_NamePropertyId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    mouse_event, GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    VIRTUAL_KEY, VK_B, VK_F10, VK_LWIN, VK_RBUTTON, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, FindWindowExW, GetAncestor, GetClassLongPtrW, GetCursorPos,
    GetForegroundWindow, GetWindowRect, IsHungAppWindow, IsWindow, IsWindowVisible, PostMessageW, SendMessageTimeoutW,
    SetCursorPos, SetForegroundWindow, WindowFromPoint, GA_ROOT, GCLP_HICON, GCLP_HICONSM, HICON,
    ICON_BIG, ICON_SMALL, ICON_SMALL2, PW_RENDERFULLCONTENT, SMTO_ABORTIFHUNG, WM_GETICON,
};

use crate::ui::{DibTarget, IconPixels};
use crate::win32::fnv1a64;
use crate::taskbar::{primary_taskbar, TrayPeek};
use super::tray_popup::{self, DesktopCover, MenuPlacement};

pub const WM_APP_TRAY_ITEMS: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 31;
pub const WM_APP_LIVE_ICON_CHANGED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 56;
pub const WM_APP_TRAY_INVOKED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 57;

/// 收纳项只保存可跨线程传递的标识，点击时重新定位当前的原生控件。
#[derive(Clone, PartialEq, Eq)]
pub struct TrayItem {
    pub key: String,
    pub name: String,
    pub uia_key: Option<String>,
    pub overflow: bool,
    pub app_exe: Option<String>,
    pub app_hwnds: Vec<isize>,
}

fn stable_tray_base_name(name: &str) -> &str {
    let line = name.lines().map(str::trim).find(|s| !s.is_empty()).unwrap_or("");
    let base = line.split(" - ").next().unwrap_or(line).trim();
    if base.is_empty() { line } else { base }
}

/// 允许托盘提示带状态前后缀，只匹配完整的应用名称，避免将相似名称的应用合并。
fn name_match(tooltip: &str, label: &str) -> Option<(bool, usize)> {
    let label = label.trim().to_lowercase();
    if label.is_empty() {
        return None;
    }
    if stable_tray_base_name(tooltip).to_lowercase() == label {
        return Some((true, label.len()));
    }
    if label.chars().count() < 2 {
        return None;
    }
    let tooltip = tooltip.lines().find(|line| !line.trim().is_empty())?.to_lowercase();
    tooltip.match_indices(&label).any(|(start, matched)| {
        !tooltip[..start].ends_with(char::is_alphanumeric)
            && !tooltip[start + matched.len()..].starts_with(char::is_alphanumeric)
    }).then_some((false, label.len()))
}

/// 选择唯一的最佳匹配。同名或同优先级的候选无法可靠区分，保留各自入口。
fn unique_best<T>(candidates: impl Iterator<Item = (T, (bool, usize))>) -> Option<T> {
    let mut best = None;
    let mut score = None;
    for (candidate, rank) in candidates {
        if score.is_none_or(|current| rank > current) {
            score = Some(rank);
            best = Some(candidate);
        } else if score == Some(rank) {
            best = None;
        }
    }
    best
}

/// 合并运行中应用和实际托盘按钮，保留已保存的应用固定键。
pub fn build_shelf_items(apps: &[crate::monitor::RuntimeApp], uia_items: &[TrayItem]) -> Vec<TrayItem> {
    let mut items: Vec<_> = apps.iter().filter(|app| !app.key.is_empty()).map(|app| TrayItem {
        key: format!("app:{}", app.key),
        name: app.label.clone(),
        uia_key: None,
        overflow: false,
        app_exe: Some(app.exe.clone()),
        app_hwnds: app.windows.iter().map(|w| w.hwnd.0 as isize).collect(),
    }).collect();
    let app_count = items.len();
    for tray in uia_items {
        let match_index = unique_best(items[..app_count].iter().enumerate().filter_map(|(index, app)| {
            name_match(&tray.name, &app.name).map(|rank| (index, rank))
        }));
        if let Some(app) = match_index.map(|index| &mut items[index]).filter(|app| app.uia_key.is_none()) {
            app.uia_key = tray.uia_key.clone();
            app.overflow = tray.overflow;
        } else {
            items.push(tray.clone());
        }
    }
    items
}

static GENERATION: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LIVE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

fn tray_key(name: &str, ordinal: usize) -> String {
    let clean: String = stable_tray_base_name(name).chars().filter(|c| !c.is_control()).take(64).collect();
    let hash = fnv1a64(clean.as_bytes());
    if ordinal == 0 { format!("{hash:016x}") } else { format!("{hash:016x}#{ordinal}") }
}

/// 主托盘和溢出区独立更新。Windows 隐藏其中一个时，保留它上一次可见的条目。
#[derive(Default)]
pub struct TraySnapshot {
    main: Option<Vec<TrayItem>>,
    overflow: Option<Vec<TrayItem>>,
    generation: u64,
    icons_changed: bool,
}

impl TraySnapshot {
    /// 忽略晚到的旧扫描，只有列表或原生像素有变化时才刷新 UI。
    pub fn apply(self, items: &mut Vec<TrayItem>, generation: &mut u64) -> bool {
        if self.generation < *generation || (self.main.is_none() && self.overflow.is_none()) {
            return false;
        }
        *generation = self.generation;
        let mut next: Vec<_> = items.iter().filter(|item| {
            if item.overflow { self.overflow.is_none() } else { self.main.is_none() }
        }).cloned().collect();
        next.extend(self.main.into_iter().flatten());
        next.extend(self.overflow.into_iter().flatten());
        next.sort_by_key(|item| item.overflow);
        let mut keys = std::collections::HashSet::new();
        next.retain(|item| keys.insert(item.key.clone()));
        let changed = self.icons_changed || *items != next;
        *items = next;
        changed
    }
}

pub struct TrayInvocation {
    pub target: TrayItem,
    pub ordinary_window: bool,
    pub snapshot: TraySnapshot,
}

pub struct TrayRefresh {
    pub snapshot: TraySnapshot,
    pub peeked: bool,
}

struct NativeButton {
    item: TrayItem,
    element: IUIAutomationElement,
    root: HWND,
    button_rect: RECT,
    image_rect: RECT,
}

const OVERFLOW_CLASSES: &[&str] = &["TopLevelWindowForOverflowXamlIsland", "NotifyIconOverflowWindow", "Xaml_WindowedPopupClass"];

/// 使用线程本地的 COM 客户端，避免跨线程调用已失效的 UIA 元素。
fn with_uia<T>(run: impl FnOnce(&IUIAutomation) -> T) -> Option<T> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }
        let _com = ComGuard;
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        // Explorer 无响应时限制单次 UIA 请求的等待，避免任务栏长时间留在临时显示状态。
        if let Ok(client) = uia.cast::<IUIAutomation2>() {
            let _ = client.SetConnectionTimeout(1000);
            let _ = client.SetTransactionTimeout(1000);
        }
        Some(run(&uia))
    }
}

/// 将工作线程结果交给主窗口，若窗口已退出则由本线程释放。
fn post_result<T>(target: HWND, message: u32, result: T) {
    let ptr = Box::into_raw(Box::new(result));
    unsafe {
        if PostMessageW(target, message, WPARAM(0), LPARAM(ptr as isize)).is_err() {
            drop(Box::from_raw(ptr));
        }
    }
}

/// 采集原生图标。Finder 持有隐藏任务栏时，在遮罩后用无焦点提示完成采样。
pub fn spawn_refresh(target: HWND, cache: Arc<TrayIconCache>, peek: Option<TrayPeek>) -> bool {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return false;
    }
    let target = target.0 as isize;
    let peeked = peek.is_some();
    let spawn = thread::Builder::new().name("tray-uia".into()).spawn(move || {
        let generation = bump_generation();
        let snapshot = with_uia(|uia| {
            let Some(tray) = primary_taskbar() else { return TraySnapshot::default() };
            let reveal = if let Some(peek) = peek {
                let Some(reveal) = TrayReveal::start(peek, true) else { return TraySnapshot::default() };
                wait_until(|| unsafe { IsWindowVisible(tray).as_bool() });
                Some(reveal)
            } else {
                None
            };
            let (mut main, mut overflow) = scan_buttons(uia, tray);
            if main.as_ref().is_none_or(Vec::is_empty) {
                if let Some(cover) = reveal.as_ref().and_then(|display| display.cover.as_ref()) {
                    if cover.wake_without_focus() {
                        (main, overflow) = wait_for_buttons(uia, tray, false, Duration::from_millis(1000));
                    }
                }
            }
            collect_snapshot(main.as_deref(), overflow.as_deref(), &cache, generation, peeked)
        }).unwrap_or_default();
        post_result(HWND(target as *mut _), WM_APP_TRAY_ITEMS, TrayRefresh { snapshot, peeked });
        IN_FLIGHT.store(false, Ordering::SeqCst);
    });
    if spawn.is_err() {
        IN_FLIGHT.store(false, Ordering::SeqCst);
    }
    spawn.is_ok()
}

/// 按刚刚扫描到的原生按钮执行点击，并把临时显示期间看到的图标同步给 Finder。
pub fn spawn_invocation(
    target: HWND, item: TrayItem, anchor: Option<RECT>, peek: Option<TrayPeek>, cache: Arc<TrayIconCache>,
) -> bool {
    let target = target.0 as isize;
    thread::Builder::new().name("tray-click".into()).spawn(move || {
        let (ordinary_window, snapshot) = with_uia(|uia| invoke(uia, &item, anchor, peek, &cache))
            .unwrap_or_else(|| (item.uia_key.is_none(), TraySnapshot::default()));
        post_result(HWND(target as *mut _), WM_APP_TRAY_INVOKED, TrayInvocation { target: item, ordinary_window, snapshot });
    }).is_ok()
}

/// 在 DWM 确认遮罩后借用主任务栏，恢复隐藏后才移除遮罩。
struct TrayReveal {
    peek: TrayPeek,
    cover: Option<DesktopCover>,
    overflow_cover: Option<DesktopCover>,
}

impl TrayReveal {
    fn start(peek: TrayPeek, conceal: bool) -> Option<Self> {
        let cover = if conceal { Some(tray_popup::taskbar_cover(peek.hwnd())?) } else { None };
        peek.set_visible(true).then_some(Self { peek, cover, overflow_cover: None })
    }
}

impl Drop for TrayReveal {
    fn drop(&mut self) {
        if self.peek.set_visible(false) {
            wait_until(|| !unsafe { IsWindowVisible(self.peek.hwnd()).as_bool() });
            if self.cover.is_some() {
                unsafe { let _ = DwmFlush(); }
            }
        }
    }
}

/// 自动隐藏时窗口有时仍被标记为可见，但实际仅剩屏幕边缘的唤出条。
fn surface_on_screen(hwnd: HWND) -> bool {
    let mut bounds = RECT::default();
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowRect(hwnd, &mut bounds).is_err() {
            return false;
        }
    }
    let Some(monitor) = crate::win32::monitor_of_window(hwnd) else { return false };
    let r = monitor.rect;
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let visible_width = bounds.right.min(r.right) - bounds.left.max(r.left);
    let visible_height = bounds.bottom.min(r.bottom) - bounds.top.max(r.top);
    width > 0 && height > 0 && visible_width >= width / 2 && visible_height >= height / 2
}

fn overflow_window(tray: HWND, visible: bool) -> Option<HWND> {
    let pid = crate::taskbar::window_pid(tray);
    for &class in OVERFLOW_CLASSES {
        // 通用 XAML 弹窗类只用于已经显示的溢出面板，不绑定隐藏的其他 Shell 弹窗。
        if !visible && class == "Xaml_WindowedPopupClass" {
            continue;
        }
        let mut previous = HWND::default();
        while let Ok(hwnd) = unsafe { FindWindowExW(None, previous, crate::win32::wide_static(class), None) } {
            if crate::taskbar::window_pid(hwnd) == pid && (!visible || surface_on_screen(hwnd)) {
                return Some(hwnd);
            }
            previous = hwnd;
        }
    }
    None
}

/// 只调用 Explorer 的隐藏图标箭头，避免误触输入法、快捷设置等同级控件。
fn overflow_chevron(uia: &IUIAutomation, tray: HWND) -> Option<IUIAutomationElement> {
    unsafe {
        let root = uia.ElementFromHandle(tray).ok()?;
        ["显示隐藏的图标", "隐藏的图标菜单", "Show hidden icons"].iter().find_map(|name| {
            let condition = uia.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(*name)).ok()?;
            root.FindFirst(TreeScope_Descendants, &condition).ok()
        })
    }
}

/// Win11 XAML 托盘位于任务栏的另一棵子树。以控件 ID 识别通知图标，旧版才回退到通知工具栏。
fn scan_buttons(uia: &IUIAutomation, tray: HWND) -> (Option<Vec<NativeButton>>, Option<Vec<NativeButton>>) {
    let mut ordinals = HashMap::new();
    let main = surface_on_screen(tray).then(|| enumerate_surface(uia, tray, false, &mut ordinals)).flatten();
    let overflow = overflow_window(tray, true).and_then(|hwnd| enumerate_surface(uia, hwnd, true, &mut ordinals));
    (main, overflow)
}

/// 任务栏的位置可能已更新，但 XAML 子树仍在加载。等实际通知项出现后再决定是否切换到溢出区。
fn wait_for_buttons(
    uia: &IUIAutomation, tray: HWND, in_overflow: bool, timeout: Duration,
) -> (Option<Vec<NativeButton>>, Option<Vec<NativeButton>>) {
    let deadline = Instant::now() + timeout;
    loop {
        let buttons = scan_buttons(uia, tray);
        let relevant = if in_overflow { &buttons.1 } else { &buttons.0 };
        if relevant.as_ref().is_some_and(|items| !items.is_empty()) || Instant::now() >= deadline {
            return buttons;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn enumerate_surface(
    uia: &IUIAutomation,
    hwnd: HWND,
    overflow: bool,
    ordinals: &mut HashMap<String, usize>,
) -> Option<Vec<NativeButton>> {
    unsafe {
        let root = uia.ElementFromHandle(hwnd).ok()?;
        let notify_id = uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("NotifyItemIcon")).ok()?;
        let mut buttons = root.FindAll(TreeScope_Descendants, &notify_id).ok()?;
        if buttons.Length().ok()? == 0 {
            let legacy = if overflow {
                (crate::win32::class_name(hwnd) == "NotifyIconOverflowWindow").then_some(hwnd)
            } else {
                let notify = FindWindowExW(hwnd, None, w!("TrayNotifyWnd"), None).ok();
                notify.filter(|notify| {
                    let pager = FindWindowExW(*notify, None, w!("SysPager"), None).ok();
                    FindWindowExW(pager.unwrap_or(*notify), None, w!("ToolbarWindow32"), None).is_ok()
                })
            };
            if let Some(legacy) = legacy {
                let root = uia.ElementFromHandle(legacy).ok()?;
                let condition = uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ButtonControlTypeId.0)).ok()?;
                buttons = root.FindAll(TreeScope_Descendants, &condition).ok()?;
            }
        }

        let image_cond = uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ImageControlTypeId.0)).ok()?;
        let mut items = Vec::new();
        for i in 0..buttons.Length().ok()? {
            let Ok(element) = buttons.GetElement(i) else { continue };
            let name = element.CurrentName().unwrap_or_default().to_string();
            if name.trim().is_empty() || name.to_lowercase().contains("windockfinder")
                || element.CurrentIsOffscreen().map_or(true, |hidden| hidden.as_bool())
            {
                continue;
            }
            let button_rect = element.CurrentBoundingRectangle().unwrap_or_default();
            if button_rect.right <= button_rect.left || button_rect.bottom <= button_rect.top {
                continue;
            }
            // 高 DPI 下按钮远大于图标。只采样其中的 Image，旧工具栏按钮则使用自身范围。
            let image_rect = element.FindFirst(TreeScope_Descendants, &image_cond)
                .and_then(|image| image.CurrentBoundingRectangle()).unwrap_or(button_rect);
            let base = stable_tray_base_name(&name).to_string();
            let ordinal = ordinals.entry(base).or_default();
            let key = tray_key(&name, *ordinal);
            *ordinal += 1;
            let item = TrayItem { key: key.clone(), name, uia_key: Some(key), overflow, app_exe: None, app_hwnds: Vec::new() };
            items.push(NativeButton { item, element, root: hwnd, button_rect, image_rect });
        }
        Some(items)
    }
}

fn center(rect: RECT) -> POINT {
    POINT { x: rect.left + (rect.right - rect.left) / 2, y: rect.top + (rect.bottom - rect.top) / 2 }
}

/// 屏幕采样和鼠标透传前校验落点仍属于该托盘窗口，避免读到遮挡窗口或误点击。
fn belongs_to(root: HWND, point: POINT) -> bool {
    unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) == root }
}

/// 托盘被覆盖时，从它自己的窗口取像素，避免采进上方窗口的内容。
fn print_surface(hwnd: HWND) -> Option<(RECT, DibTarget)> {
    let mut bounds = RECT::default();
    unsafe {
        if IsHungAppWindow(hwnd).as_bool() {
            return None;
        }
        GetWindowRect(hwnd, &mut bounds).ok()?;
    }
    let w = bounds.right - bounds.left;
    let h = bounds.bottom - bounds.top;
    let mut dib = DibTarget::default();
    if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > 2_097_152 || !dib.ensure(w, h) {
        return None;
    }
    unsafe {
        if !PrintWindow(hwnd, dib.mem_dc?, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool() {
            return None;
        }
        let _ = GdiFlush();
    }
    Some((bounds, dib))
}

fn icon_from_surface(bounds: &RECT, dib: &DibTarget, rect: RECT) -> Option<IconPixels> {
    if rect.left < bounds.left || rect.top < bounds.top || rect.right > bounds.right || rect.bottom > bounds.bottom {
        return None;
    }
    let mut pixels = Vec::new();
    let start = (rect.left - bounds.left) as usize * 4;
    let end = (rect.right - bounds.left) as usize * 4;
    let stride = dib.w as usize * 4;
    let src = unsafe { std::slice::from_raw_parts(dib.ptr as *const u8, stride * dib.h as usize) };
    for y in (rect.top - bounds.top)..(rect.bottom - bounds.top) {
        let offset = y as usize * stride;
        pixels.extend_from_slice(&src[offset + start..offset + end]);
    }
    crate::dock::icons::key_out_icon_background(pixels, rect.right - rect.left, rect.bottom - rect.top)
}

fn collect_snapshot(
    main: Option<&[NativeButton]>,
    overflow: Option<&[NativeButton]>,
    cache: &TrayIconCache,
    generation: u64,
    covered: bool,
) -> TraySnapshot {
    let mut icons_changed = false;
    let mut collect = |buttons: &[NativeButton]| {
        let mut printed = None;
        buttons.iter().map(|button| {
            let icon = button.image_rect;
            let width = icon.right - icon.left;
            let height = icon.bottom - icon.top;
            if (8..=128).contains(&width) && (8..=128).contains(&height) {
                // 多取一像素作为原生背景色键，避免从图标内部取背景色。
                let area = RECT { left: icon.left - 1, top: icon.top - 1, right: icon.right + 1, bottom: icon.bottom + 1 };
                let pixels = if !covered && belongs_to(button.root, center(icon)) {
                    crate::dock::icons::capture_screen_rect(area.left, area.top, width + 2, height + 2)
                } else {
                    printed.get_or_insert_with(|| print_surface(button.root)).as_ref()
                        .and_then(|(bounds, dib)| icon_from_surface(bounds, dib, area))
                };
                if let Some(pixels) = pixels.and_then(|p| crop_opaque(&p)).and_then(|p| normalize_square(&p, 32)) {
                    icons_changed |= cache.insert_if_changed(&button.item.key, pixels, generation);
                }
            }
            button.item.clone()
        }).collect()
    };
    let main = main.map(&mut collect);
    let overflow = overflow.map(collect);
    TraySnapshot { main, overflow, generation, icons_changed }
}

fn invoke_element(element: &IUIAutomationElement) -> bool {
    unsafe {
        element.GetCurrentPattern(UIA_InvokePatternId)
            .and_then(|pattern| pattern.cast::<IUIAutomationInvokePattern>())
            .and_then(|pattern| pattern.Invoke()).is_ok()
    }
}

/// 一次发送组合键的按下和释放事件，以保持修饰键状态完整。
fn send_chord(modifier: VIRTUAL_KEY, action: VIRTUAL_KEY) -> bool {
    let key = |vk: VIRTUAL_KEY, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    };
    let down = KEYBD_EVENT_FLAGS(0);
    let input = [key(modifier, down), key(action, down), key(action, KEYEVENTF_KEYUP), key(modifier, KEYEVENTF_KEYUP)];
    unsafe { SendInput(&input, std::mem::size_of::<INPUT>() as i32) as usize == input.len() }
}

fn wait_until(test: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while !test() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
}

fn find_target<'a>(
    item: &TrayItem,
    buttons: impl Iterator<Item = &'a NativeButton> + Clone,
) -> Option<&'a NativeButton> {
    if let Some(key) = item.uia_key.as_deref() {
        if let Some(button) = buttons.clone().find(|button| button.item.key == key && button.item.overflow == item.overflow) {
            return Some(button);
        }
    }
    item.app_exe.as_ref()?;
    unique_best(buttons.filter_map(|button| name_match(&button.item.name, &item.name).map(|rank| (button, rank))))
}

/// 原生按钮能命中时透传同一鼠标动作。被遮住的按钮可用焦点键盘菜单。
fn click_button(button: &NativeButton, right: bool) -> bool {
    if !right && invoke_element(&button.element) {
        return true;
    }
    // 优先用 Explorer 的焦点命令，让菜单请求仍是原生托盘操作，鼠标留在 Finder。
    if right && unsafe { button.element.SetFocus().is_ok() } {
        let focused = || unsafe { button.element.CurrentHasKeyboardFocus().is_ok_and(|focus| focus.as_bool()) };
        wait_until(focused);
        if focused() && send_chord(VK_SHIFT, VK_F10) {
            return true;
        }
    }
    let point = center(button.button_rect);
    if belongs_to(button.root, point) {
        unsafe {
            let mut old = POINT::default();
            if GetCursorPos(&mut old).is_ok() && SetCursorPos(point.x, point.y).is_ok() {
                thread::sleep(Duration::from_millis(35));
                // 光标移动后重查遮挡，防止唤出的系统浮层截获模拟点击。
                if belongs_to(button.root, point) {
                    let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) }
                        else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
                    mouse_event(down, 0, 0, 0, 0);
                    mouse_event(up, 0, 0, 0, 0);
                    if !right {
                        thread::sleep(Duration::from_millis(35));
                        let _ = SetCursorPos(old.x, old.y);
                    }
                    return true;
                }
                let _ = SetCursorPos(old.x, old.y);
            }
        }
    }
    false
}

fn invoke(
    uia: &IUIAutomation, item: &TrayItem, anchor: Option<RECT>, peek: Option<TrayPeek>, cache: &TrayIconCache,
) -> (bool, TraySnapshot) {
    let Some(tray) = primary_taskbar() else { return (item.uia_key.is_none(), TraySnapshot::default()) };
    let right = anchor.is_some();
    if right {
        // 顶部栏在按下时收到点击，交接前等待物理右键释放，避免释放消息关掉原生菜单。
        wait_until(|| unsafe { GetAsyncKeyState(i32::from(VK_RBUTTON.0)) >= 0 });
    }
    let foreground = unsafe { GetForegroundWindow() };
    let mut reveal = if let Some(peek) = peek {
        let Some(reveal) = TrayReveal::start(peek, right) else { return (false, TraySnapshot::default()) };
        Some(reveal)
    } else {
        None
    };
    // 等待异步显示生效，再处理系统的自动隐藏状态。
    wait_until(|| unsafe { IsWindowVisible(tray).as_bool() });
    let mut buttons = scan_buttons(uia, tray);
    let revealed = buttons.0.as_ref().is_none_or(Vec::is_empty);
    if revealed {
        // Win+B 让 Explorer 展开自动隐藏的任务栏。
        send_chord(VK_LWIN, VK_B);
        buttons = wait_for_buttons(uia, tray, false, Duration::from_millis(500));
    }
    let (mut main, mut overflow) = buttons;
    let mut chevron = None;
    if find_target(item, main.iter().flatten().chain(overflow.iter().flatten())).is_none() && overflow.is_none() {
        // 溢出列表只有展开时才发布 UIA 元素，使用原生箭头打开并重新枚举。
        if let Some(display) = reveal.as_mut().filter(|display| display.cover.is_some()) {
            display.overflow_cover = overflow_window(tray, false).and_then(tray_popup::taskbar_cover);
        }
        chevron = overflow_chevron(uia, tray).filter(invoke_element);
        if chevron.is_some() {
            (main, overflow) = wait_for_buttons(uia, tray, true, Duration::from_millis(500));
        }
    }

    // 菜单和鼠标高亮可能遮挡托盘，先采集像素再派发点击。
    let covered = reveal.as_ref().is_some_and(|display| display.cover.is_some());
    let mut snapshot = collect_snapshot(main.as_deref(), overflow.as_deref(), cache, bump_generation(), covered);
    let target = find_target(item, main.iter().flatten().chain(overflow.iter().flatten()));
    let mut cursor = POINT::default();
    let have_cursor = right && unsafe { GetCursorPos(&mut cursor).is_ok() };
    let invoked = target.is_some_and(|button| {
        let Some(anchor) = anchor else { return click_button(button, false) };
        let source = center(button.button_rect);
        let opened = MenuPlacement::new(anchor, source, &item.app_hwnds)
            .open_at(|| click_button(button, true));
        // 只有鼠标兜底确实移动了光标，且用户尚未挪动时，才还原到点击位置。
        let mut current = POINT::default();
        if have_cursor && unsafe { GetCursorPos(&mut current).is_ok() }
            && current == source && current != cursor
        {
            unsafe { let _ = SetCursorPos(cursor.x, cursor.y); }
        }
        opened
    });
    if invoked {
        if !right {
            // 左键后留出应用更新通知图标的时间。
            thread::sleep(Duration::from_millis(180));
            let (main, overflow) = scan_buttons(uia, tray);
            let after = collect_snapshot(main.as_deref(), overflow.as_deref(), cache, bump_generation(), covered);
            if after.main.is_some() || after.overflow.is_some() {
                snapshot.icons_changed |= after.icons_changed;
                snapshot.generation = after.generation;
                snapshot.main = after.main.or(snapshot.main);
                snapshot.overflow = after.overflow.or(snapshot.overflow);
            }
        }
    } else {
        if let Some(chevron) = chevron.filter(|_| overflow_window(tray, true).is_some()) {
            invoke_element(&chevron);
        }
        if revealed {
            unsafe { let _ = SetForegroundWindow(foreground); }
        }
    }
    // 找到原生托盘时，即使系统拒绝派发输入，也不以窗口系统菜单代替托盘菜单。
    (target.is_none() && item.uia_key.is_none(), snapshot)
}

/// 图标像素缓存（托盘镜像与窗口实时图标，预算 2 MB）。
pub struct TrayIconCache {
    inner: std::sync::Mutex<std::collections::HashMap<String, (IconPixels, u64, u64)>>,
    bytes: std::sync::atomic::AtomicUsize,
}

pub const TRAY_ICON_BUDGET: usize = 2 * 1024 * 1024;

impl Default for TrayIconCache {
    fn default() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
            bytes: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl TrayIconCache {
    /// 写入图标并在像素哈希变化时返回 true，避免重复触发重绘。
    pub fn insert_if_changed(&self, key: &str, px: IconPixels, generation: u64) -> bool {
        let hash = fnv1a64(&px.px);
        let cost = px.px.len() + 64;
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = map.get_mut(key) {
            if cur.1 > generation {
                return false;
            }
            if cur.2 == hash {
                cur.1 = generation;
                return false;
            }
        }
        if let Some(old) = map.insert(key.to_string(), (px, generation, hash)) {
            let _ = self.bytes.fetch_sub(old.0.px.len() + 64, Ordering::SeqCst);
        }
        let _ = self.bytes.fetch_add(cost, Ordering::SeqCst);
        while self.bytes.load(Ordering::SeqCst) > TRAY_ICON_BUDGET {
            let Some(k) = map.iter().min_by_key(|(_, (_, g, _))| *g).map(|(k, _)| k.clone()) else { break };
            if let Some(old) = map.remove(&k) {
                let _ = self.bytes.fetch_sub(old.0.px.len() + 64, Ordering::SeqCst);
            }
        }
        true
    }

    pub fn insert(&self, key: &str, px: IconPixels, generation: u64) {
        let _ = self.insert_if_changed(key, px, generation);
    }

    pub fn get(&self, key: &str) -> Option<IconPixels> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).get(key).map(|(p, _, _)| p.clone())
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 采样应用窗口当前动态 HICON（支持状态变化切换图标的应用，不销毁原窗口图标句柄）。
pub fn capture_window_live_icon(hwnd: HWND) -> Option<IconPixels> {
    unsafe {
        if hwnd.0.is_null() || !IsWindow(hwnd).as_bool() {
            return None;
        }
        let mut hicon = HICON::default();
        for kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
            let mut res = 0usize;
            let ok = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                80,
                Some(&mut res),
            );
            if ok.0 != 0 && res != 0 {
                hicon = HICON(res as *mut _);
                break;
            }
        }
        if hicon.is_invalid() || hicon.0.is_null() {
            let cls_big = GetClassLongPtrW(hwnd, GCLP_HICON);
            if cls_big != 0 {
                hicon = HICON(cls_big as *mut _);
            } else {
                let cls_sm = GetClassLongPtrW(hwnd, GCLP_HICONSM);
                if cls_sm != 0 {
                    hicon = HICON(cls_sm as *mut _);
                }
            }
        }
        if hicon.is_invalid() || hicon.0.is_null() {
            return None;
        }
        let Ok(copied) = CopyIcon(hicon) else {
            return None;
        };
        crate::dock::icons::hicon_to_pixels(copied, 32)
    }
}

/// 后台轻量刷新指定运行应用的实时窗口图标；仅像素确实变更时才向主窗口发送重绘通知。
pub fn spawn_live_window_icon_refresh(target: HWND, jobs: Vec<(String, Vec<isize>)>, cache: Arc<TrayIconCache>) {
    if jobs.is_empty() || LIVE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let target_raw = target.0 as isize;
    std::thread::Builder::new()
        .name("live-icons".into())
        .spawn(move || {
            let gen = GENERATION.load(Ordering::SeqCst);
            let mut changed = false;
            for (key, hwnds) in jobs {
                for raw in hwnds {
                    if let Some(px) = capture_window_live_icon(HWND(raw as *mut _)) {
                        changed |= cache.insert_if_changed(&format!("win:{key}"), px, gen);
                        break;
                    }
                }
            }
            LIVE_IN_FLIGHT.store(false, Ordering::SeqCst);
            if changed {
                unsafe {
                    let _ = PostMessageW(HWND(target_raw as *mut _), WM_APP_LIVE_ICON_CHANGED, WPARAM(0), LPARAM(0));
                }
            }
        })
        .ok();
}

fn crop_opaque(px: &IconPixels) -> Option<IconPixels> {
    let (w, h) = (px.w as usize, px.h as usize);
    let data = px.px.as_slice();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if data[(y * w + x) * 4 + 3] > 40 {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut out = vec![0u8; cw * ch * 4];
    for y in 0..ch {
        for x in 0..cw {
            let si = ((y0 + y) * w + (x0 + x)) * 4;
            let di = (y * cw + x) * 4;
            out[di..di + 4].copy_from_slice(&data[si..si + 4]);
        }
    }
    Some(IconPixels {
        w: cw as u32,
        h: ch as u32,
        px: Arc::new(out),
    })
}

/// 双线性缩放为正方形（居中、保持比例）。
fn normalize_square(px: &IconPixels, size: u32) -> Option<IconPixels> {
    let (w, h) = (px.w as usize, px.h as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let k = (size as f32 / w as f32).min(size as f32 / h as f32);
    let dw = ((w as f32 * k).round() as usize).max(1);
    let dh = ((h as f32 * k).round() as usize).max(1);
    let dx = ((size as usize - dw) / 2) as i32;
    let dy = ((size as usize - dh) / 2) as i32;
    let src = px.px.as_slice();
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size as usize {
        for x in 0..size as usize {
            let sx = x as i32 - dx;
            let sy = y as i32 - dy;
            if sx < 0 || sy < 0 || sx >= dw as i32 || sy >= dh as i32 {
                continue;
            }
            let fx = (sx as f32 + 0.5) * w as f32 / dw as f32 - 0.5;
            let fy = (sy as f32 + 0.5) * h as f32 / dh as f32 - 0.5;
            let x0 = (fx.floor() as i32).clamp(0, w as i32 - 1);
            let y0 = (fy.floor() as i32).clamp(0, h as i32 - 1);
            let x1 = (x0 + 1).min(w as i32 - 1);
            let y1 = (y0 + 1).min(h as i32 - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let ty = (fy - y0 as f32).clamp(0.0, 1.0);
            let i00 = ((y0 as usize) * w + x0 as usize) * 4;
            let i10 = ((y0 as usize) * w + x1 as usize) * 4;
            let i01 = ((y1 as usize) * w + x0 as usize) * 4;
            let i11 = ((y1 as usize) * w + x1 as usize) * 4;
            let oi = (y * size as usize + x) * 4;
            for c in 0..4 {
                let v = src[i00 + c] as f32 * (1.0 - tx) * (1.0 - ty)
                    + src[i10 + c] as f32 * tx * (1.0 - ty)
                    + src[i01 + c] as f32 * (1.0 - tx) * ty
                    + src[i11 + c] as f32 * tx * ty;
                out[oi + c] = v.round().clamp(0.0, 255.0) as u8;
            }
            // 保证预乘不变量
            let a = out[oi + 3] as u32;
            for c in 0..3 {
                if out[oi + c] as u32 > a {
                    out[oi + c] = a as u8;
                }
            }
        }
    }
    Some(IconPixels {
        w: size,
        h: size,
        px: Arc::new(out),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::RuntimeApp;

    fn tray_item(tooltip: &str, overflow: bool) -> TrayItem {
        let key = tray_key(tooltip, 0);
        TrayItem {
            key: key.clone(),
            name: tooltip.into(),
            uia_key: Some(key),
            overflow,
            app_exe: None,
            app_hwnds: Vec::new(),
        }
    }

    #[test]
    fn tooltip_status_preserves_the_running_apps_pinned_key() {
        let apps = [RuntimeApp {
            key: "example.exe".into(),
            label: "宝可梦加速器".into(),
            exe: "Example.exe".into(),
            windows: Vec::new(),
        }];
        for tooltip in ["me 宝可梦加速器", "宝可梦加速器 - 已连接", "在线 - 宝可梦加速器"] {
            let tray = tray_item(tooltip, true);
            let results = build_shelf_items(&apps, std::slice::from_ref(&tray));
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].key, "app:example.exe");
            assert_eq!(results[0].uia_key, tray.uia_key);
            assert!(results[0].overflow);
        }
        assert!(name_match("宝可梦加速器助手", "宝可梦加速器").is_none());
    }

    #[test]
    fn scanning_a_visible_tray_retains_the_closed_overflow() {
        let overflow = tray_item("后台应用", true);
        let mut items = vec![tray_item("已退出应用", false), overflow.clone()];
        let mut generation = 1;
        let updated = TraySnapshot { main: Some(Vec::new()), generation: 2, ..Default::default() };
        assert!(updated.apply(&mut items, &mut generation));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, overflow.key);
        let stale = TraySnapshot { overflow: Some(Vec::new()), generation: 1, ..Default::default() };
        assert!(!stale.apply(&mut items, &mut generation));
        assert_eq!(items.len(), 1);
    }
}
