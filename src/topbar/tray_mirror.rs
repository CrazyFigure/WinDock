//! 系统托盘 UIA 发现、图标缓存、点击/右键透传、展开/固定状态管理。
//!
//! 零注入方案：不向 Explorer 注入 DLL，通过 UI Automation 枚举本会话
//! `Shell_TrayWnd`（TrayNotifyWnd）与溢出窗口中的托盘按钮；遍历在受控工作线程
//! 执行（带超时与代次丢弃），后台绝不高频轮询。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern, TreeScope_Children,
    TreeScope_Descendants, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId, UIA_InvokePatternId,
    UIA_ButtonControlTypeId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    mouse_event, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, FindWindowW, GetClassLongPtrW, GetCursorPos, IsWindow, PostMessageW,
    SendMessageTimeoutW, SetCursorPos, ShowWindow, GCLP_HICON, GCLP_HICONSM, HICON, ICON_BIG,
    ICON_SMALL, ICON_SMALL2, SMTO_ABORTIFHUNG, SW_HIDE, SW_SHOWNA, WM_GETICON,
};

use crate::ui::IconPixels;
use crate::win32::fnv1a64;

pub const WM_APP_TRAY_ITEMS: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 31;
pub const WM_APP_LIVE_ICON_CHANGED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 56;

/// UIA 元素跨线程包装（UIA 代理对象不要求线程亲和）。
#[derive(Clone)]
pub struct SendElement(pub IUIAutomationElement);
unsafe impl Send for SendElement {}
unsafe impl Sync for SendElement {}

#[derive(Clone)]
pub struct TrayItem {
    pub key: String,
    pub name: String,
    pub rect: RECT,
    pub offscreen: bool,
    pub element: Option<SendElement>,
    /// 对应系统托盘条目的原始 key（合并应用时用于优先查找动态托盘截图）
    pub uia_key: Option<String>,
    /// 若来自当前打开的桌面应用，保存其原始 exe 路径与窗口句柄列表
    pub app_exe: Option<String>,
    pub app_hwnds: Vec<isize>,
}

fn stable_tray_base_name(name: &str) -> &str {
    let line = name
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| name.trim());
    let base = line.split(" - ").next().map(str::trim).unwrap_or(line);
    if base.is_empty() { line } else { base }
}

/// 生成顶部栏收纳列表：包含所有已打开的应用以及系统托盘图标；
/// 若运行中的应用同时拥有系统托盘图标，则合并保留其 UIA 托盘句柄与动态托盘图标键。
pub fn build_shelf_items(apps: &[crate::monitor::RuntimeApp], uia_items: &[TrayItem]) -> Vec<TrayItem> {
    let mut out: Vec<TrayItem> = Vec::with_capacity(apps.len() + uia_items.len());
    let mut name_to_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for app in apps {
        if app.key.is_empty() {
            continue;
        }
        let lower_name = app.label.trim().to_lowercase();
        let idx = out.len();
        name_to_idx.insert(lower_name, idx);
        out.push(TrayItem {
            key: format!("app:{}", app.key),
            name: app.label.clone(),
            rect: RECT::default(),
            offscreen: false,
            element: None,
            uia_key: None,
            app_exe: Some(app.exe.clone()),
            app_hwnds: app.windows.iter().map(|w| w.hwnd.0 as isize).collect(),
        });
    }
    for u in uia_items {
        let lower = u.name.trim().to_lowercase();
        let base_lower = stable_tray_base_name(&u.name).to_lowercase();
        if lower.contains("windockfinder") {
            continue;
        }
        let matched = name_to_idx
            .get(&lower)
            .or_else(|| name_to_idx.get(&base_lower))
            .copied()
            .or_else(|| {
                name_to_idx.iter().find_map(|(app_name, &idx)| {
                    if app_name.chars().count() >= 2
                        && (base_lower.starts_with(app_name) || app_name.starts_with(&base_lower))
                    {
                        Some(idx)
                    } else {
                        None
                    }
                })
            });
        if let Some(idx) = matched {
            // 同名运行应用合并 UIA 托盘元素，保留应用窗口句柄、动态托盘 key 与原生右键能力
            if out[idx].uia_key.is_none() {
                out[idx].uia_key = Some(u.key.clone());
            }
            if out[idx].element.is_none() && u.element.is_some() {
                out[idx].element = u.element.clone();
                out[idx].rect = u.rect;
                out[idx].offscreen = u.offscreen;
            }
            continue;
        }
        out.push(u.clone());
    }
    out
}

static GENERATION: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LIVE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

pub fn tray_key(name: &str, ordinal: usize) -> String {
    let clean: String = stable_tray_base_name(name)
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    format!("{:016x}{}", fnv1a64(clean.as_bytes()), if ordinal > 0 { format!("#{ordinal}") } else { String::new() })
}

const SYSTEM_NAMES: &[&str] = &[
    "网络", "音量", "电池", "操作中心", "通知中心", "通知", "时钟", "显示隐藏的图标", "隐藏的图标菜单",
    "开始", "搜索", "任务视图", "任务栏", "小组件", "聊天", "Network", "Volume", "Battery", "Action Center",
    "Show hidden icons", "Notification Center", "Clock", "Start", "Search", "Task View",
];

fn is_system_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() {
        return true;
    }
    if SYSTEM_NAMES.iter().any(|s| n.eq_ignore_ascii_case(s) || n.starts_with(s) && n.len() <= s.len() + 2) {
        return true;
    }
    if n.contains("隐藏的图标") || n.to_lowercase().contains("hidden icons") || n == "显示桌面" {
        return true;
    }
    // 时间类名称（如 "9:05" / "2026/11/25 9:05"）
    let b = n.as_bytes();
    if b.len() >= 4 && b[0].is_ascii_digit() && (n.contains(':') && !n.contains(' ')) {
        return true;
    }
    false
}

/// 在工作线程枚举托盘按钮；结果通过 WM_APP_TRAY_ITEMS 投递。
pub fn spawn_refresh(target: HWND) {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let gen = GENERATION.load(Ordering::SeqCst);
    // HWND 非 Send：先转为 isize 再跨线程传递，线程内重建
    let target_raw = target.0 as isize;
    std::thread::Builder::new()
        .name("tray-uia".into())
        .spawn(move || {
            let target = HWND(target_raw as *mut _);
            let hr = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED) };
            let need_uninit = hr.is_ok();
            let result = std::panic::catch_unwind(enumerate).unwrap_or_else(|_| Err("UIA 枚举 panic".into()));
            if need_uninit {
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
            IN_FLIGHT.store(false, Ordering::SeqCst);
            if gen != GENERATION.load(Ordering::SeqCst) {
                return; // 代次已更新，丢弃过时结果
            }
            let items = result.unwrap_or_default();
            let boxed = Box::new(items);
            unsafe {
                let _ = PostMessageW(target, WM_APP_TRAY_ITEMS, WPARAM(0), LPARAM(Box::into_raw(boxed) as isize));
            }
        })
        .ok();
}

fn class_of(e: &IUIAutomationElement) -> String {
    unsafe {
        match e.GetCurrentPropertyValue(UIA_ClassNamePropertyId) {
            Ok(v) => v.to_string(),
            Err(_) => String::new(),
        }
    }
}

fn find_child_by_class(uia: &IUIAutomation, root: &IUIAutomationElement, class: &str) -> Option<IUIAutomationElement> {
    unsafe {
        let cond = uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(class)).ok()?;
        root.FindFirst(TreeScope_Children, &cond).ok()
    }
}

fn enumerate() -> Result<Vec<TrayItem>, String> {
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        let root = uia.GetRootElement().map_err(|e| e.to_string())?;
        let button_cond = uia
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ButtonControlTypeId.0))
            .map_err(|e| e.to_string())?;
        let mut items: Vec<TrayItem> = Vec::new();
        let mut roots: Vec<(IUIAutomationElement, bool)> = Vec::new();
        if let Some(tray) = find_child_by_class(&uia, &root, "Shell_TrayWnd") {
            // 仅枚举通知区域子树（TrayNotifyWnd），避免把任务栏应用按钮当成托盘图标
            match find_child_by_class(&uia, &tray, "TrayNotifyWnd") {
                Some(notify) => roots.push((notify, false)),
                None => roots.push((tray, false)),
            }
        }
        for cls in ["TopLevelWindowForOverflowXamlIsland", "NotifyIconOverflowWindow", "Xaml_WindowedPopupClass"] {
            if let Some(w) = find_child_by_class(&uia, &root, cls) {
                roots.push((w, true));
            }
        }
        if roots.is_empty() {
            return Ok(items);
        }
        let mut ordinals: Vec<(String, usize)> = Vec::new();
        for (r, overflow) in roots {
            let Ok(arr) = r.FindAll(TreeScope_Descendants, &button_cond) else { continue };
            let Ok(len) = arr.Length() else { continue };
            for i in 0..len {
                let Ok(e) = arr.GetElement(i) else { continue };
                let name = e.CurrentName().unwrap_or_default().to_string();
                if is_system_name(&name) {
                    continue;
                }
                let rect = e.CurrentBoundingRectangle().unwrap_or_default();
                let offscreen = e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(true) || overflow;
                let same = ordinals.iter().filter(|(n, _)| n == &name).count();
                ordinals.push((name.clone(), same));
                let key = tray_key(&name, same);
                items.push(TrayItem {
                    key: key.clone(),
                    name,
                    rect,
                    offscreen,
                    element: Some(SendElement(e)),
                    uia_key: Some(key),
                    app_exe: None,
                    app_hwnds: Vec::new(),
                });
            }
        }
        // 去重（溢出窗口关闭时元素仍可能重复出现）
        let mut seen = std::collections::HashSet::new();
        items.retain(|i| seen.insert(i.key.clone()));
        Ok(items)
    }
}

/// 左键：优先 UIA InvokePattern，失败时实时获取边界矩形（必要时展开溢出浮层）并模拟原生左键。
pub fn invoke_left(item: &TrayItem) -> bool {
    if let Some(el) = &item.element {
        unsafe {
            if let Ok(unk) = el.0.GetCurrentPattern(UIA_InvokePatternId) {
                if let Ok(inv) = unk.cast::<IUIAutomationInvokePattern>() {
                    if inv.Invoke().is_ok() {
                        return true;
                    }
                }
            }
        }
    }
    physical_click_live(item, false)
}

/// 右键：实时定位系统托盘图标并模拟物理右键，弹出应用原版托盘菜单。
pub fn invoke_right(item: &TrayItem) -> bool {
    physical_click_live(item, true)
}

/// 查询元素实时可见矩形；若位于折叠溢出区且尚未展开，则先唤醒溢出浮层后再重试一次。
fn live_tray_rect(item: &TrayItem) -> Option<RECT> {
    let valid = |r: RECT| r.right > r.left && r.bottom > r.top;
    if let Some(el) = &item.element {
        if let Ok(r) = unsafe { el.0.CurrentBoundingRectangle() } {
            if valid(r) {
                return Some(r);
            }
        }
    }
    if !item.offscreen && valid(item.rect) {
        return Some(item.rect);
    }
    ensure_overflow_open();
    std::thread::sleep(std::time::Duration::from_millis(140));
    if let Some(el) = &item.element {
        if let Ok(r) = unsafe { el.0.CurrentBoundingRectangle() } {
            if valid(r) {
                return Some(r);
            }
        }
    }
    if valid(item.rect) { Some(item.rect) } else { None }
}

fn physical_click_live(item: &TrayItem, right: bool) -> bool {
    let Some(r) = live_tray_rect(item) else {
        return false;
    };
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let mut old = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut old);
        let _ = SetCursorPos(cx, cy);
        std::thread::sleep(std::time::Duration::from_millis(35));
        if right {
            mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
            mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
        } else {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            std::thread::sleep(std::time::Duration::from_millis(120));
            let _ = SetCursorPos(old.x, old.y);
        }
    }
    true
}

/// 溢出窗口中的隐藏图标：尝试通过 UIA 调出溢出面板后点击。
pub fn ensure_overflow_open() {
    unsafe {
        let Ok(uia): Result<IUIAutomation, _> = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) else { return };
        let Ok(root) = uia.GetRootElement() else { return };
        let Some(tray) = find_child_by_class(&uia, &root, "Shell_TrayWnd") else { return };
        let Ok(cond) = uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_ButtonControlTypeId.0)) else { return };
        let Ok(arr) = tray.FindAll(TreeScope_Descendants, &cond) else { return };
        let Ok(len) = arr.Length() else { return };
        for i in 0..len {
            if let Ok(e) = arr.GetElement(i) {
                let name = e.CurrentName().unwrap_or_default().to_string();
                if name.contains("隐藏") || name.to_lowercase().contains("hidden") {
                    if let Ok(unk) = e.GetCurrentPattern(UIA_InvokePatternId) {
                        if let Ok(inv) = unk.cast::<IUIAutomationInvokePattern>() {
                            let _ = inv.Invoke();
                            std::thread::sleep(std::time::Duration::from_millis(350));
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// 临时显示系统任务栏以便采样托盘图标/与原生托盘交互。
pub fn show_taskbar_temporarily(show: bool) {
    for hwnd in crate::taskbar::find_taskbars() {
        unsafe {
            let _ = ShowWindow(hwnd, if show { SW_SHOWNA } else { SW_HIDE });
        }
    }
    if show {
        if let Ok(w) = unsafe { FindWindowW(windows::core::w!("NotifyIconOverflowWindow"), None) } {
            unsafe {
                let _ = ShowWindow(w, SW_SHOWNA);
            }
        }
    }
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

/// 对当前可见的托盘按钮截图，作为顶部栏镜像图标（仅任务栏可见时有效），返回是否有像素更新。
pub fn capture_icons(items: &[TrayItem], cache: &Arc<TrayIconCache>, generation: u64) -> bool {
    let mut any_changed = false;
    for item in items {
        if item.offscreen {
            continue;
        }
        let w = item.rect.right - item.rect.left;
        let h = item.rect.bottom - item.rect.top;
        if w < 12 || h < 12 || w > 64 || h > 64 {
            continue;
        }
        if let Some(mut px) = crate::dock::icons::capture_screen_rect(item.rect.left, item.rect.top, w, h) {
            // 裁掉色键后的透明边界，提取内容方形
            if let Some(cropped) = crop_opaque(&px) {
                px = cropped;
            }
            if let Some(p) = normalize_square(&px, 32) {
                let target_key = item.uia_key.as_deref().unwrap_or(&item.key);
                any_changed |= cache.insert_if_changed(target_key, p, generation);
            }
        }
    }
    any_changed
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
