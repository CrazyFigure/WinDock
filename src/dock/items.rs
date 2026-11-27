//! 固定项/运行项归并、启动/激活、拖放添加、文件夹堆栈、回收站交互。

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    SHQueryRecycleBinW, SHFILEOPSTRUCTW, SHQUERYRBINFO, ShellExecuteExW, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SEE_MASK_NOASYNC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, IsWindow, PostMessageW, ShowWindowAsync, SC_MINIMIZE, SC_RESTORE, SHOW_WINDOW_CMD,
    SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WM_SYSCOMMAND,
};

use crate::config::{DockItem, DockItems};
use crate::dock::icons::running_image_key;
use crate::monitor::RuntimeApp;
use crate::win32::{bring_to_front, fnv1a64, open_shell_path, wide};

/// 功能区条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FuncKind {
    Folder(usize),
    Trash,
    Launchpad,
    Settings,
}

#[derive(Clone, Debug)]
pub struct FuncEntry {
    pub kind: FuncKind,
    pub label: String,
    pub icon_key: String,
}

/// 应用区条目（固定项与运行项归并后）。
#[derive(Clone, Debug)]
pub struct Entry {
    /// 稳定标识：固定项 id 或 `run:<exe>`
    pub id: String,
    /// 固定项在 dock_items 中的索引
    pub pinned: Option<usize>,
    /// 运行应用在快照中的 exe（小写路径）
    pub running_exe: Option<String>,
    pub label: String,
    pub icon_key: String,
    pub windows: Vec<(isize, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct EntryList {
    pub apps: Vec<Entry>,
    pub right: Vec<FuncEntry>,
}

pub fn trash_icon_key() -> &'static str {
    "special:trash"
}

pub fn settings_icon_key() -> &'static str {
    "special:settings"
}

pub fn launchpad_icon_key() -> &'static str {
    "special:launchpad"
}

pub fn new_item(kind: &str, target: &str, label: &str) -> DockItem {
    DockItem {
        id: format!("{:016x}", fnv1a64(format!("{kind}|{target}").as_bytes())),
        kind: kind.into(),
        target: target.into(),
        args: String::new(),
        aumid: String::new(),
        label: label.into(),
        custom_icon: String::new(),
        order: next_order_placeholder(),
    }
}

fn next_order_placeholder() -> u32 {
    u32::MAX
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

/// 首次运行种子固定项：文件管理器与已安装的常见浏览器、编辑器。
pub fn seed_items() -> DockItems {
    let mut items = DockItems::default();
    let mut order = 0u32;
    let mut push = |kind: &str, target: &str, label: &str, items: &mut DockItems| {
        let mut it = new_item(kind, target, label);
        it.order = order;
        order += 1;
        items.items.push(it);
    };
    push("app", "C:\\Windows\\explorer.exe", crate::win32::FILE_MANAGER_NAME, &mut items);
    let candidates: [(PathBuf, &str); 5] = [
        (
            env_path("ProgramFiles(x86)")
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"))
                .join(r"Microsoft\Edge\Application\msedge.exe"),
            "Microsoft Edge",
        ),
        (
            env_path("ProgramFiles")
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
                .join(r"Google\Chrome\Application\chrome.exe"),
            "Google Chrome",
        ),
        (
            env_path("LocalAppData")
                .unwrap_or_else(|| PathBuf::from(""))
                .join(r"Google\Chrome\Application\chrome.exe"),
            "Google Chrome",
        ),
        (
            env_path("LocalAppData")
                .unwrap_or_else(|| PathBuf::from(""))
                .join(r"Programs\Microsoft VS Code\Code.exe"),
            "Visual Studio Code",
        ),
        (
            env_path("ProgramFiles")
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
                .join(r"Microsoft VS Code\Code.exe"),
            "Visual Studio Code",
        ),
    ];
    for (p, label) in candidates {
        if p.is_file() {
            let t = p.to_string_lossy().to_string();
            if !items.items.iter().any(|i| i.target.eq_ignore_ascii_case(&t)) {
                push("app", &t, label, &mut items);
            }
        }
    }
    items
}

/// 由固定项 + 运行应用构建 Dock 条目。
pub fn build_entries(items: &DockItems, apps: &[RuntimeApp], show_running: bool) -> EntryList {
    let mut list = EntryList::default();
    let pinned = items.sorted();
    for (idx, item) in pinned.iter().enumerate() {
        match item.kind.as_str() {
            "folder" | "file" => {
                list.right.push(FuncEntry {
                    kind: FuncKind::Folder(idx),
                    label: resolve_dock_item_label(item, None),
                    icon_key: if item.custom_icon.is_empty() {
                        item.target.clone()
                    } else {
                        item.custom_icon.clone()
                    },
                });
            }
            "winlaunchpad" => {
                list.apps.insert(
                    0,
                    Entry {
                        id: item.id.clone(),
                        pinned: Some(idx),
                        running_exe: None,
                        label: resolve_dock_item_label(item, None),
                        icon_key: launchpad_icon_key().into(),
                        windows: Vec::new(),
                    },
                );
            }
            _ => {
                let exe = resolve_item_exe(item);
                let key = exe.to_lowercase();
                let app = apps.iter().find(|a| a.key == key);
                let label = resolve_dock_item_label(item, app.map(|a| a.label.as_str()));
                list.apps.push(Entry {
                    id: item.id.clone(),
                    pinned: Some(idx),
                    running_exe: app.map(|_| key.clone()),
                    label,
                    // 自定义图标优先，运行中的固定项也使用映像资源策略。
                    icon_key: if !item.custom_icon.is_empty() {
                        item.custom_icon.clone()
                    } else if app.is_some() {
                        running_image_key(&exe)
                    } else {
                        exe
                    },
                    windows: app.map(|a| a.windows.iter().map(|w| (w.hwnd.0 as isize, w.title.clone())).collect()).unwrap_or_default(),
                });
            }
        }
    }
    if show_running {
        for app in apps {
            if list.apps.iter().any(|e| e.running_exe.as_deref() == Some(app.key.as_str())) {
                continue;
            }
            list.apps.push(Entry {
                id: format!("run:{}", app.key),
                pinned: None,
                running_exe: Some(app.key.clone()),
                label: app.label.clone(),
                icon_key: running_image_key(&app.exe),
                windows: app.windows.iter().map(|w| (w.hwnd.0 as isize, w.title.clone())).collect(),
            });
        }
    }
    list
}

/// 固定项的图标/标识键：.lnk 解析目标，否则自身路径；UWP 为 aumid。
pub fn resolve_item_exe(item: &DockItem) -> String {
    if item.kind == "uwp" {
        return format!("uwp:{}", item.aumid);
    }
    if item.target.to_lowercase().ends_with(".lnk") {
        if let Some(t) = crate::dock::icons::read_lnk_target(Path::new(&item.target)) {
            return t;
        }
    }
    item.target.clone()
}

/// 判断已有固定项的 `label` 是否属于可自动优化的旧默认名称或全小写/裸文件名，而非用户手动重命名。
fn should_upgrade_dock_label(item: &DockItem, exe: &str) -> bool {
    if crate::win32::is_legacy_file_manager_label(exe, &item.label)
        || crate::win32::is_legacy_file_manager_label(&item.target, &item.label)
    {
        return true;
    }
    let label = item.label.trim();
    if label.is_empty() || crate::win32::is_all_lowercase_ascii(label) {
        return true;
    }
    let target_stem = Path::new(&item.target)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let exe_stem = Path::new(exe)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    (!target_stem.is_empty() && label.eq_ignore_ascii_case(&target_stem))
        || (!exe_stem.is_empty() && label.eq_ignore_ascii_case(&exe_stem))
}

/// 在自动解析名称与运行态提示名之间择优（中文优先，规避回退成纯 exe 裸名或全小写）。
fn pick_best_resolved_label(resolved: &str, hint: Option<&str>, reference_path: &str) -> String {
    let resolved_trim = resolved.trim();
    let stem = Path::new(reference_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Some(h) = hint.map(str::trim).filter(|s| !s.is_empty()) {
        let resolved_has_cjk = crate::win32::has_cjk(resolved_trim);
        let hint_has_cjk = crate::win32::has_cjk(h);
        let resolved_is_plain_stem = !stem.is_empty() && resolved_trim.eq_ignore_ascii_case(&stem);
        if resolved_trim.is_empty()
            || (!resolved_has_cjk && hint_has_cjk)
            || (resolved_is_plain_stem
                && !crate::win32::is_all_lowercase_ascii(h)
                && !h.eq_ignore_ascii_case(&stem))
        {
            return h.to_string();
        }
    }
    resolved_trim.to_string()
}

/// 解析固定项面向 Dock 与设置页的最终展示名称（优先中文名并修正全小写名称，同时保留用户自定义标签）。
pub fn resolve_dock_item_label(item: &DockItem, running_label: Option<&str>) -> String {
    match item.kind.as_str() {
        "folder" | "file" => {
            if item.label.is_empty() {
                Path::new(&item.target)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "文件夹".into())
            } else {
                item.label.clone()
            }
        }
        "winlaunchpad" => "启动台".into(),
        _ => {
            let exe = resolve_item_exe(item);
            if crate::win32::is_legacy_file_manager_label(&exe, &item.label)
                || crate::win32::is_legacy_file_manager_label(&item.target, &item.label)
            {
                return crate::win32::FILE_MANAGER_NAME.into();
            }
            if !should_upgrade_dock_label(item, &exe) {
                return item.label.clone();
            }
            let target_p = Path::new(&item.target);
            let raw_resolved = if item.target.to_lowercase().ends_with(".lnk") && target_p.is_file() {
                crate::monitor::shortcut_display_name(target_p)
            } else {
                let real_exe = crate::win32::real_path_case(&exe);
                crate::monitor::exe_display_name(&real_exe)
            };
            let best = pick_best_resolved_label(&raw_resolved, running_label, &exe);
            if best.is_empty() {
                item.label.clone()
            } else {
                best
            }
        }
    }
}

/// 启动加载时自动迁移旧版或全小写/裸文件名固定项标签，返回是否发生变更。
pub fn migrate_dock_item_label(item: &mut DockItem) -> bool {
    if matches!(item.kind.as_str(), "folder" | "file" | "winlaunchpad") {
        return false;
    }
    let exe = resolve_item_exe(item);
    if !should_upgrade_dock_label(item, &exe) {
        return false;
    }
    let next = resolve_dock_item_label(item, None);
    if next.is_empty() || next == item.label {
        return false;
    }
    // 非全小写旧标签仅在解析出更佳的非全小写/中文名称时覆盖，避免误降级。
    if !crate::win32::is_all_lowercase_ascii(&item.label) && crate::win32::is_all_lowercase_ascii(&next) {
        return false;
    }
    item.label = next;
    true
}

/// 是否有运行窗口。
pub fn has_windows(entry: &Entry) -> bool {
    !entry.windows.is_empty()
}

/// 请求窗口切换显示状态。部分窗口拒绝直接切换，改由目标处理原生系统菜单命令。
fn change_window_state(hwnd: HWND, state: SHOW_WINDOW_CMD, command: u32) {
    unsafe {
        // 异步请求避免目标窗口未响应时阻塞 Dock 的鼠标处理。
        if !ShowWindowAsync(hwnd, state).as_bool() {
            let _ = PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(command as usize), LPARAM(0));
        }
    }
}

/// 激活有效窗口，最小化时先请求还原。
pub fn activate_window(hwnd: HWND) {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        if IsIconic(hwnd).as_bool() {
            change_window_state(hwnd, SW_RESTORE, SC_RESTORE);
        }
    }
    bring_to_front(hwnd);
}

/// 最小化当前 Dock 条目对应的窗口。
pub fn minimize_window(hwnd: HWND) {
    change_window_state(hwnd, SW_MINIMIZE, SC_MINIMIZE);
}

/// 关闭窗口（WM_CLOSE）。
pub fn close_window(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// 启动固定项。
pub fn launch_item(item: &DockItem) -> bool {
    match item.kind.as_str() {
        "uwp" => open_shell_path("open", "explorer.exe", Some(&format!("shell:AppsFolder\\{}", item.aumid))),
        _ => {
            // 文件管理器（explorer.exe 无参数）直接打开「此电脑」窗口。
            let is_explorer = item.target.to_lowercase().ends_with("\\explorer.exe") || item.target.eq_ignore_ascii_case("explorer.exe");
            if is_explorer && item.args.is_empty() {
                return open_shell_path("open", "explorer.exe", Some("shell:MyComputerFolder"));
            }
            let args = item.args.clone();
            open_shell_path("open", &item.target, if args.is_empty() { None } else { Some(&args) })
        }
    }
}

/// 以管理员身份运行。
pub fn run_as_admin(item: &DockItem) {
    let exe = if item.kind == "uwp" { String::new() } else { resolve_item_exe(item) };
    if exe.is_empty() {
        return;
    }
    let _ = open_shell_path("runas", &exe, None);
}

/// 打开条目所在位置。
pub fn open_file_location(item: &DockItem) {
    let exe = resolve_item_exe(item);
    let _ = open_shell_path("open", "explorer.exe", Some(&format!("/select,\"{exe}\"")));
}

/// 从路径创建固定项，并允许传入运行态展示名称作为辅助兜底。
pub fn item_from_path_with_hint(path: &str, label_hint: Option<&str>) -> Option<DockItem> {
    let p = Path::new(path);
    if !p.exists() {
        return None;
    }
    // 尽量还原磁盘上的真实大小写，避免从运行态小写路径固定后保存全小写 target。
    let normalized = crate::win32::real_path_case(path);
    let norm_path = Path::new(&normalized);
    let lower = normalized.to_lowercase();
    if p.is_dir() {
        let label = norm_path
            .file_name()
            .or_else(|| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Some(new_item("folder", &normalized, &label));
    }
    if lower.ends_with(".lnk") {
        // 快捷方式优先取本地化展示名与目标程序中文名。
        let resolved = crate::monitor::shortcut_display_name(norm_path);
        let picked = pick_best_resolved_label(&resolved, label_hint, &normalized);
        let label = if picked.is_empty() {
            norm_path
                .file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            picked
        };
        return Some(new_item("app", &normalized, &label));
    }
    if lower.ends_with(".exe") || lower.ends_with(".bat") || lower.ends_with(".cmd") {
        // 可执行文件优先走内置中文名/开始菜单/PE 版本描述，避免退化为全小写文件名。
        let resolved = crate::monitor::exe_display_name(&normalized);
        let picked = pick_best_resolved_label(&resolved, label_hint, &normalized);
        let label = if picked.is_empty() {
            norm_path
                .file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            picked
        };
        return Some(new_item("app", &normalized, &label));
    }
    Some(new_item("file", &normalized, ""))
}

/// 从拖入的路径创建固定项。
pub fn item_from_path(path: &str) -> Option<DockItem> {
    item_from_path_with_hint(path, None)
}

/// 按 `order` 对固定项就地归一化排序并重写连续序号，保证 Vec 顺序与 `sorted()` 始终一致。
fn normalize_orders(items: &mut DockItems) {
    items.items.sort_by_key(|i| i.order);
    for (i, it) in items.items.iter_mut().enumerate() {
        it.order = i as u32;
    }
}

/// 添加固定项并保存（返回新条目 id）。
pub fn add_pinned(items: &mut DockItems, item: DockItem) -> String {
    normalize_orders(items);
    let mut item = item;
    item.order = items.items.len() as u32;
    let id = item.id.clone();
    items.items.push(item);
    id
}

/// 取消固定（从固定项中删除）。
pub fn remove_pinned(items: &mut DockItems, id: &str) {
    items.items.retain(|i| i.id != id);
    normalize_orders(items);
}

/// 重排：把 `id` 移动到 `before_id` 之前（before_id 为空则放到末尾）。
#[allow(dead_code)]
pub fn reorder(items: &mut DockItems, id: &str, before_id: Option<&str>) {
    normalize_orders(items);
    let Some(pos) = items.items.iter().position(|i| i.id == id) else { return };
    let item = items.items.remove(pos);
    let target = before_id
        .and_then(|b| items.items.iter().position(|i| i.id == b))
        .unwrap_or(items.items.len());
    items.items.insert(target.min(items.items.len()), item);
    for (i, it) in items.items.iter_mut().enumerate() {
        it.order = i as u32;
    }
}

/// 按 `sorted()` 索引把固定项从 `from_idx` 移动到 `target_idx`（设置页列表拖拽与上下移共用）。
pub fn reorder_sorted(items: &mut DockItems, from_idx: usize, target_idx: usize) -> bool {
    normalize_orders(items);
    let len = items.items.len();
    if from_idx >= len || target_idx >= len || from_idx == target_idx {
        return false;
    }
    let item = items.items.remove(from_idx);
    items.items.insert(target_idx, item);
    for (i, it) in items.items.iter_mut().enumerate() {
        it.order = i as u32;
    }
    true
}

/// 按 Dock 应用区展示顺序重排固定应用，保留文件夹/文件等其他条目相对次序。
pub fn reorder_app_entries(items: &mut DockItems, ordered_app_ids: &[String]) -> bool {
    if ordered_app_ids.len() < 2 {
        return false;
    }
    normalize_orders(items);
    let mut positions = Vec::with_capacity(ordered_app_ids.len());
    let mut by_id = Vec::with_capacity(ordered_app_ids.len());
    for (idx, it) in items.items.iter().enumerate() {
        if ordered_app_ids.iter().any(|id| id == &it.id) {
            positions.push(idx);
        }
    }
    if positions.len() != ordered_app_ids.len() {
        return false;
    }
    let unchanged = positions
        .iter()
        .zip(ordered_app_ids.iter())
        .all(|(&pos, id)| items.items[pos].id == *id);
    if unchanged {
        return false;
    }
    for id in ordered_app_ids {
        if let Some(it) = items.items.iter().find(|x| &x.id == id).cloned() {
            by_id.push(it);
        }
    }
    if by_id.len() != positions.len() {
        return false;
    }
    for (slot, item) in positions.into_iter().zip(by_id.into_iter()) {
        items.items[slot] = item;
    }
    for (i, it) in items.items.iter_mut().enumerate() {
        it.order = i as u32;
    }
    true
}

// ---------- 回收站 ----------

pub fn recycle_bin_full() -> bool {
    unsafe {
        let mut info = SHQUERYRBINFO {
            cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
            ..Default::default()
        };
        if SHQueryRecycleBinW(PCWSTR::null(), &mut info).is_ok() {
            info.i64Size > 0
        } else {
            false
        }
    }
}

pub fn open_recycle_bin() {
    let _ = open_shell_path("open", "explorer.exe", Some("shell:RecycleBinFolder"));
}

pub fn empty_recycle_bin() {
    unsafe {
        let _ = windows::Win32::UI::Shell::SHEmptyRecycleBinW(windows::Win32::Foundation::HWND::default(), PCWSTR::null(), 0);
    }
}

/// 清洗注册表 `DisplayIcon` / `Uninstall` 值中的引号与 `,0` 图标索引后缀。
pub fn clean_exe_icon_path(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix('"') {
        s = rest.split('"').next().unwrap_or("");
    } else if let Some((left, _)) = s.rsplit_once(',') {
        if left.to_lowercase().ends_with(".exe") {
            s = left;
        }
    }
    let s = s.trim();
    if s.to_lowercase().ends_with(".exe") && Path::new(s).is_file() {
        Some(s.to_string())
    } else {
        None
    }
}

fn read_reg_string(root: windows::Win32::System::Registry::HKEY, subkey: &str, value: Option<&str>) -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
    let wsub = wide(subkey);
    let wval = value.map(wide);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root,
            PCWSTR(wsub.as_ptr()),
            wval.as_ref().map(|v| PCWSTR(v.as_ptr())).unwrap_or(PCWSTR::null()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r.is_ok() {
        let s = crate::win32::from_wide(&buf);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// 自动探测已安装的 WinLaunchpad.exe（依次检查卸载注册表、App Paths、开始菜单、常见安装目录及运行中进程）。
pub fn detect_winlaunchpad() -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    // 1. 注册表卸载信息与 App Paths
    let uninstall_keys = [
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad"),
    ];
    for (root, key) in uninstall_keys {
        if let Some(loc) = read_reg_string(root, key, Some("InstallLocation")) {
            let exe = Path::new(loc.trim().trim_matches('"')).join("WinLaunchpad.exe");
            if exe.is_file() {
                return Some(exe.to_string_lossy().to_string());
            }
        }
        if let Some(icon) = read_reg_string(root, key, Some("DisplayIcon")) {
            if let Some(exe) = clean_exe_icon_path(&icon) {
                return Some(exe);
            }
        }
    }
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(p) = read_reg_string(root, r"Software\Microsoft\Windows\CurrentVersion\App Paths\WinLaunchpad.exe", None) {
            if let Some(exe) = clean_exe_icon_path(&p) {
                return Some(exe);
            }
        }
    }

    // 2. 开始菜单与开机启动快捷方式
    let mut lnk_candidates = Vec::new();
    if let Some(appdata) = env_path("APPDATA") {
        let sm = appdata.join(r"Microsoft\Windows\Start Menu\Programs");
        lnk_candidates.push(sm.join("WinLaunchpad.lnk"));
        lnk_candidates.push(sm.join(r"WinLaunchpad\WinLaunchpad.lnk"));
        lnk_candidates.push(sm.join(r"Startup\WinLaunchpad.lnk"));
    }
    if let Some(pd) = env_path("ProgramData") {
        let sm = pd.join(r"Microsoft\Windows\Start Menu\Programs");
        lnk_candidates.push(sm.join("WinLaunchpad.lnk"));
        lnk_candidates.push(sm.join(r"WinLaunchpad\WinLaunchpad.lnk"));
    }
    for lnk in lnk_candidates {
        if lnk.is_file() {
            if let Some(target) = crate::dock::icons::read_lnk_target(&lnk) {
                if Path::new(&target).is_file() {
                    return Some(target);
                }
            }
        }
    }

    // 3. 标准安装路径与同级便携路径
    let mut exe_candidates = Vec::new();
    if let Some(local) = env_path("LOCALAPPDATA") {
        exe_candidates.push(local.join(r"Programs\WinLaunchpad\WinLaunchpad.exe"));
        exe_candidates.push(local.join(r"WinLaunchpad\WinLaunchpad.exe"));
    }
    if let Some(pf) = env_path("ProgramFiles") {
        exe_candidates.push(pf.join(r"WinLaunchpad\WinLaunchpad.exe"));
    }
    if let Some(pf86) = env_path("ProgramFiles(x86)") {
        exe_candidates.push(pf86.join(r"WinLaunchpad\WinLaunchpad.exe"));
    }
    let cur_exe = crate::win32::exe_path();
    if let Some(dir) = cur_exe.parent() {
        exe_candidates.push(dir.join("WinLaunchpad.exe"));
        if let Some(parent) = dir.parent() {
            exe_candidates.push(parent.join(r"WinLaunchpad\WinLaunchpad.exe"));
        }
    }
    for p in exe_candidates {
        if p.is_file() {
            return Some(p.to_string_lossy().to_string());
        }
    }

    // 4. 正在运行的 WinLaunchpad 实例
    if let Ok(hwnd) = unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(windows::core::w!("WinLaunchpadMain"), None) } {
        if !hwnd.0.is_null() {
            let pid = crate::taskbar::window_pid(hwnd);
            let img = crate::taskbar::process_image_raw(pid);
            if !img.is_empty() && Path::new(&img).is_file() {
                return Some(img);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_exe_icon_path_handles_quotes_and_index() {
        let exe = crate::win32::exe_path().to_string_lossy().to_string();
        assert_eq!(clean_exe_icon_path(&format!("\"{exe}\",0")), Some(exe.clone()));
        assert_eq!(clean_exe_icon_path(&format!("{exe},0")), Some(exe));
        assert_eq!(clean_exe_icon_path(r"C:\__not_exist_12345\WinLaunchpad.exe,0"), None);
    }

    #[test]
    fn seed_items_uses_file_manager_label() {
        let seeded = seed_items();
        let first = seeded.items.first().expect("seeded items should not be empty");
        assert!(first.target.to_lowercase().ends_with("explorer.exe"));
        assert_eq!(first.label, "文件管理器");
    }

    #[test]
    fn resolve_and_migrate_upgrade_lowercase_and_stem_labels() {
        let mut explorer_low = new_item("app", r"c:\windows\explorer.exe", "explorer");
        assert_eq!(resolve_dock_item_label(&explorer_low, None), "文件管理器");
        assert!(migrate_dock_item_label(&mut explorer_low));
        assert_eq!(explorer_low.label, "文件管理器");

        let mut notepad_stem = new_item("app", r"C:\Windows\System32\notepad.exe", "Notepad");
        assert_eq!(resolve_dock_item_label(&notepad_stem, None), "记事本");
        assert!(migrate_dock_item_label(&mut notepad_stem));
        assert_eq!(notepad_stem.label, "记事本");

        let mut edge_low = new_item("app", r"c:\fake\msedge.exe", "msedge");
        assert_eq!(resolve_dock_item_label(&edge_low, None), "Microsoft Edge");
        assert!(migrate_dock_item_label(&mut edge_low));
        assert_eq!(edge_low.label, "Microsoft Edge");

        let mut custom = new_item("app", r"c:\windows\explorer.exe", "我的文件");
        assert_eq!(resolve_dock_item_label(&custom, None), "我的文件");
        assert!(!migrate_dock_item_label(&mut custom));

        let mut mixed_custom = new_item("app", r"c:\fake\customtool.exe", "My Custom App");
        assert_eq!(resolve_dock_item_label(&mixed_custom, None), "My Custom App");
        assert!(!migrate_dock_item_label(&mut mixed_custom));

        let mut low_fallback = new_item("app", r"c:\fake\cc-switch.exe", "cc-switch");
        assert_eq!(resolve_dock_item_label(&low_fallback, None), "Cc-Switch");
        assert!(migrate_dock_item_label(&mut low_fallback));
        assert_eq!(low_fallback.label, "Cc-Switch");
    }

    #[test]
    fn item_from_path_resolves_real_system_chinese_name_and_case() {
        let win_dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let explorer_lower = format!(r"{}\explorer.exe", win_dir.to_lowercase());
        if Path::new(&explorer_lower).is_file() {
            let item = item_from_path(&explorer_lower).expect("explorer item");
            assert_eq!(item.label, "文件管理器");
            assert!(item.target.ends_with("explorer.exe"));
        }
        let dir = tempfile::tempdir().unwrap();
        let bat = dir.path().join("MyTestTool.bat");
        std::fs::write(&bat, b"@echo off\n").unwrap();
        let lower_bat = bat.to_string_lossy().to_lowercase();
        let bat_item = item_from_path(&lower_bat).expect("bat item");
        assert_eq!(bat_item.label, "MyTestTool");
        assert!(bat_item.target.ends_with("MyTestTool.bat"));
    }

    #[test]
    fn reorder_sorted_and_app_entries_keep_order_consistent() {
        let mut di = DockItems::default();
        let mut a = new_item("app", r"C:\a.exe", "A");
        a.order = 2;
        let mut b = new_item("app", r"C:\b.exe", "B");
        b.order = 0;
        let mut f = new_item("folder", r"C:\Docs", "Docs");
        f.order = 1;
        let mut c = new_item("app", r"C:\c.exe", "C");
        c.order = 3;
        // 故意打乱 Vec 顺序，验证重排函数会先按 order 归一化
        di.items = vec![a.clone(), f.clone(), b.clone(), c.clone()];

        assert!(reorder_sorted(&mut di, 0, 3)); // B (0) 移到最后 -> [Docs, A, C, B]
        let labels: Vec<_> = di.sorted().into_iter().map(|it| it.label.as_str()).collect();
        assert_eq!(labels, ["Docs", "A", "C", "B"]);
        assert_eq!(
            di.items.iter().map(|it| (it.label.as_str(), it.order)).collect::<Vec<_>>(),
            vec![("Docs", 0), ("A", 1), ("C", 2), ("B", 3)]
        );

        // 重排应用区顺序 [C, B, A] 时应保留文件夹 Docs 的相对位置
        assert!(reorder_app_entries(&mut di, &[c.id.clone(), b.id.clone(), a.id.clone()]));
        let labels2: Vec<_> = di.sorted().into_iter().map(|it| it.label.as_str()).collect();
        assert_eq!(labels2, ["Docs", "C", "B", "A"]);
        assert!(!reorder_app_entries(&mut di, &[c.id.clone(), b.id.clone(), a.id.clone()]));
    }
}

/// 把文件/文件夹移入回收站（FO_DELETE + FOF_ALLOWUNDO）。
pub fn move_to_recycle_bin(paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    // pFrom 以双零结尾的多字符串
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(wide(p));
        // wide() 已含一个 0
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        fAnyOperationsAborted: false.into(),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };
    unsafe { windows::Win32::UI::Shell::SHFileOperationW(&mut op) == 0 }
}

/// 打开文件夹堆栈（按设置直接打开资源管理器或展开浮层由调用方决定）。
pub fn open_folder(path: &str) {
    let _ = open_shell_path("open", path, None);
}

/// 启动 WinLaunchpad（--toggle）。
pub fn toggle_launchpad(path: &str) {
    if path.is_empty() || !Path::new(path).is_file() {
        return;
    }
    unsafe {
        let exe = wide(path);
        let args = wide("--toggle");
        let mut ei = windows::Win32::UI::Shell::SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<windows::Win32::UI::Shell::SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC,
            lpFile: PCWSTR(exe.as_ptr()),
            lpParameters: PCWSTR(args.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let _ = ShellExecuteExW(&mut ei);
    }
}
