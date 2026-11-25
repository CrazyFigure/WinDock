//! 版本化配置模型、校验限域、原子落盘与备份恢复。
//!
//! 所有用户数据保存在 `%LOCALAPPDATA%\WinDockFinder\`，写入统一采用
//! 同目录临时文件 + FlushFileBuffers + ReplaceFileW 原子替换 + `.bak` 可读备份。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, MoveFileExW, ReplaceFileW, REPLACE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

use crate::log;
use crate::win32::{data_dir, wide};

pub const SCHEMA_VERSION: u32 = 1;

fn default_true() -> bool {
    true
}

// ---------- 通用 ----------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct General {
    pub hide_taskbar_on_start: bool,
    /// 隐藏任务栏时同步设置系统 ABS_AUTOHIDE，消除最大化窗口底部死区。
    pub taskbar_autohide: bool,
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
    pub toggle_dock_hotkey: String,
    pub toggle_topbar_hotkey: String,
    pub emergency_restore_hotkey: String,
    pub winlaunchpad_path: String,
    pub winlaunchpad_auto_searched: bool,
    pub startup_initialized: bool,
}

impl General {
    pub fn normalized(mut self) -> Self {
        if self.toggle_dock_hotkey.is_empty() {
            self.toggle_dock_hotkey = "Ctrl+Alt+Shift+D".into();
        }
        if self.toggle_topbar_hotkey.is_empty() {
            self.toggle_topbar_hotkey = "Ctrl+Alt+Shift+T".into();
        }
        if self.emergency_restore_hotkey.is_empty() {
            self.emergency_restore_hotkey = "Ctrl+Alt+Shift+F12".into();
        }
        if !crate::hotkey::parse(&self.toggle_dock_hotkey).is_some() {
            self.toggle_dock_hotkey = "Ctrl+Alt+Shift+D".into();
        }
        if !crate::hotkey::parse(&self.toggle_topbar_hotkey).is_some() {
            self.toggle_topbar_hotkey = "Ctrl+Alt+Shift+T".into();
        }
        if !crate::hotkey::parse(&self.emergency_restore_hotkey).is_some() {
            self.emergency_restore_hotkey = "Ctrl+Alt+Shift+F12".into();
        }
        self
    }
}

// ---------- Dock ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DockCfg {
    pub enabled: bool,
    /// primary | cursor | all
    pub display_mode: String,
    /// smart_maximized | always | auto
    pub hide_mode: String,
    pub trigger_edge_px: i32,
    pub show_delay_ms: u32,
    pub hide_delay_ms: u32,
    pub bottom_offset_dp: i32,
    pub icon_size_dp: i32,
    pub icon_gap_dp: i32,
    pub magnify_enabled: bool,
    pub magnify_scale: f32,
    /// 波浪影响范围（两侧各影响的图标数）。
    pub magnify_range: f32,
    pub show_running_apps: bool,
    pub show_running_dot: bool,
    pub show_trash: bool,
    pub show_folders: bool,
    #[serde(default = "default_true")]
    pub show_window_preview: bool,
}

impl Default for DockCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            display_mode: "primary".into(),
            hide_mode: "smart_maximized".into(),
            trigger_edge_px: 3,
            show_delay_ms: 120,
            hide_delay_ms: 250,
            bottom_offset_dp: 5,
            icon_size_dp: 48,
            icon_gap_dp: 8,
            magnify_enabled: true,
            magnify_scale: 1.45,
            magnify_range: 2.5,
            show_running_apps: true,
            show_running_dot: true,
            show_trash: true,
            show_folders: true,
            show_window_preview: true,
        }
    }
}

impl DockCfg {
    pub fn normalized(mut self) -> Self {
        if !matches!(self.display_mode.as_str(), "primary" | "cursor" | "all") {
            self.display_mode = "primary".into();
        }
        if !matches!(self.hide_mode.as_str(), "smart_maximized" | "always" | "auto") {
            self.hide_mode = "smart_maximized".into();
        }
        self.trigger_edge_px = self.trigger_edge_px.clamp(1, 12);
        self.show_delay_ms = self.show_delay_ms.min(2000);
        self.hide_delay_ms = self.hide_delay_ms.min(2000);
        self.bottom_offset_dp = self.bottom_offset_dp.clamp(0, 40);
        self.icon_size_dp = self.icon_size_dp.clamp(28, 96);
        self.icon_gap_dp = self.icon_gap_dp.clamp(2, 24);
        if !self.magnify_scale.is_finite() {
            self.magnify_scale = 1.45;
        }
        self.magnify_scale = self.magnify_scale.clamp(1.1, 2.5);
        if !self.magnify_range.is_finite() {
            self.magnify_range = 2.5;
        }
        self.magnify_range = self.magnify_range.clamp(1.0, 5.0);
        self
    }
}

// ---------- 顶部栏 ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TopbarCfg {
    pub enabled: bool,
    /// primary | all
    pub display_mode: String,
    pub height_dp: i32,
    pub adaptive_maximized_color: bool,
    pub adaptive_color_exclude: Vec<String>,
    pub color_transition_ms: u32,
    pub show_active_app: bool,
    /// 可选 token：M d E HH mm ss 12h
    pub clock_format: String,
    pub status_items: Vec<String>,
    pub pinned_tray_keys: Vec<String>,
    pub fullscreen_yield: bool,
}

impl Default for TopbarCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            display_mode: "primary".into(),
            height_dp: 30,
            adaptive_maximized_color: true,
            adaptive_color_exclude: Vec::new(),
            color_transition_ms: 150,
            show_active_app: true,
            clock_format: "M月d日 E HH:mm".into(),
            status_items: vec!["ime".into(), "network".into(), "brightness".into(), "volume".into(), "battery".into()],
            pinned_tray_keys: Vec::new(),
            fullscreen_yield: true,
        }
    }
}

impl TopbarCfg {
    pub fn normalized(mut self) -> Self {
        if !matches!(self.display_mode.as_str(), "primary" | "all") {
            self.display_mode = "primary".into();
        }
        self.height_dp = self.height_dp.clamp(24, 38);
        self.color_transition_ms = self.color_transition_ms.clamp(0, 1000);
        if self.clock_format.is_empty() {
            self.clock_format = "M月d日 E HH:mm".into();
        }
        self.status_items.retain(|s| matches!(s.as_str(), "ime" | "network" | "brightness" | "volume" | "battery"));
        self
    }

    pub fn has_status(&self, name: &str) -> bool {
        self.status_items.iter().any(|s| s == name)
    }
}

// ---------- 外观 ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// system | light | dark
    pub theme: String,
    /// acrylic | light
    pub backdrop: String,
    pub opacity: f32,
    pub dock_corner_radius_dp: i32,
    /// macos | none
    pub icon_roundness: String,
    /// system | #RRGGBB
    pub accent_color: String,
    pub reduce_motion: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            // 默认使用纯净 macOS 磨砂托盘（不依赖浑浊发灰的 Win11 Acrylic，用户仍可在外观设置切换）
            backdrop: "light".into(),
            opacity: 0.78,
            dock_corner_radius_dp: 15,
            icon_roundness: "macos".into(),
            accent_color: "system".into(),
            reduce_motion: false,
        }
    }
}

impl Appearance {
    pub fn normalized(mut self) -> Self {
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".into();
        }
        if !matches!(self.backdrop.as_str(), "acrylic" | "light") {
            self.backdrop = "acrylic".into();
        }
        if !self.opacity.is_finite() {
            self.opacity = 0.78;
        }
        self.opacity = self.opacity.clamp(0.2, 1.0);
        self.dock_corner_radius_dp = self.dock_corner_radius_dp.clamp(4, 40);
        if !matches!(self.icon_roundness.as_str(), "macos" | "none") {
            self.icon_roundness = "macos".into();
        }
        if self.accent_color != "system" && !parse_hex_color(&self.accent_color).is_some() {
            self.accent_color = "system".into();
        }
        self
    }

    /// 当前是否使用深色文字/图标主题（深色模式 → 浅色文字）。
    pub fn is_dark(&self) -> bool {
        match self.theme.as_str() {
            "light" => false,
            "dark" => true,
            _ => crate::win32::system_uses_dark_theme(),
        }
    }

    /// Dock 毛玻璃胶囊底色（深色模式：深灰半透明；浅色：亮白半透明）。
    pub fn dock_tint(&self) -> (f32, f32, f32, f32) {
        let a = self.opacity;
        if self.is_dark() {
            (0.09, 0.10, 0.12, a)
        } else {
            (0.97, 0.97, 0.98, a)
        }
    }
}

pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

// ---------- 主配置 ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub general: General,
    pub dock: DockCfg,
    pub topbar: TopbarCfg,
    pub appearance: Appearance,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            general: General {
                hide_taskbar_on_start: true,
                taskbar_autohide: true,
                auto_check_update: true,
                toggle_dock_hotkey: "Ctrl+Alt+Shift+D".into(),
                toggle_topbar_hotkey: "Ctrl+Alt+Shift+T".into(),
                emergency_restore_hotkey: "Ctrl+Alt+Shift+F12".into(),
                winlaunchpad_path: String::new(),
                winlaunchpad_auto_searched: false,
                startup_initialized: false,
            },
            dock: DockCfg::default(),
            topbar: TopbarCfg::default(),
            appearance: Appearance::default(),
        }
    }
}

impl Config {
    fn normalized(mut self) -> Self {
        self.schema_version = SCHEMA_VERSION;
        self.general = self.general.normalized();
        self.dock = self.dock.normalized();
        self.topbar = self.topbar.normalized();
        self.appearance = self.appearance.normalized();
        self
    }
}

// ---------- Dock 固定项 ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DockItem {
    pub id: String,
    /// app | uwp | folder | file | winlaunchpad
    pub kind: String,
    pub target: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub aumid: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub custom_icon: String,
    #[serde(default)]
    pub order: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DockItems {
    #[serde(default)]
    pub items: Vec<DockItem>,
}

impl DockItems {
    pub fn sorted(&self) -> Vec<&DockItem> {
        let mut v: Vec<&DockItem> = self.items.iter().collect();
        v.sort_by_key(|i| i.order);
        v
    }
}

// ---------- 原子写入 ----------

/// 原子写入：临时文件 → FlushFileBuffers → 备份 .bak → ReplaceFileW / MoveFileExW。
pub fn write_atomic(path: &Path, bytes: &[u8], keep_bak: bool) -> bool {
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, bytes) {
        log::log(&format!("写入失败 {}: {e}", tmp.display()));
        return false;
    }
    // 刷盘
    if let Ok(f) = std::fs::File::open(&tmp) {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            let _ = FlushFileBuffers(HANDLE(f.as_raw_handle()));
        }
    }
    if keep_bak && path.exists() {
        let bak = bak_of(path);
        let _ = std::fs::copy(path, &bak);
    }
    unsafe {
        let wpath = wide(path);
        let wtmp = wide(&tmp);
        if path.exists() {
            let ok = ReplaceFileW(
                PCWSTR(wpath.as_ptr()),
                PCWSTR(wtmp.as_ptr()),
                PCWSTR::null(),
                REPLACE_FILE_FLAGS(0),
                None,
                None,
            )
            .is_ok();
            if ok {
                return true;
            }
        }
        if MoveFileExW(
            PCWSTR(wtmp.as_ptr()),
            PCWSTR(wpath.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .is_ok()
        {
            return true;
        }
    }
    let _ = std::fs::remove_file(&tmp);
    log::log(&format!("替换失败 {}", path.display()));
    false
}

fn bak_of(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap().to_os_string();
    name.push(".bak");
    p.with_file_name(name)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            log::log(&format!("{what} 解析失败 {}: {e}", path.display()));
            None
        }
    }
}

/// 写入 JSON（原子 + 可选备份）。
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T, keep_bak: bool) -> bool {
    match serde_json::to_string_pretty(value) {
        Ok(t) => write_atomic(path, t.as_bytes(), keep_bak),
        Err(e) => {
            log::log(&format!("序列化失败 {}: {e}", path.display()));
            false
        }
    }
}

// ---------- 存储目录 ----------

pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    pub fn open() -> Self {
        Self::new(data_dir())
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn dock_items_path(&self) -> PathBuf {
        self.dir.join("dock_items.json")
    }

    pub fn recovery_path(&self) -> PathBuf {
        self.dir.join("taskbar_recovery.json")
    }

    pub fn icons_dir(&self) -> PathBuf {
        self.dir.join("icons")
    }

    /// 读取配置；主文件损坏时回退 .bak，再回退默认值，并限域归一化。
    pub fn load_config(&self) -> Config {
        let p = self.config_path();
        read_json::<Config>(&p, "配置")
            .or_else(|| read_json::<Config>(&bak_of(&p), "配置备份"))
            .unwrap_or_default()
            .normalized()
    }

    pub fn save_config(&self, c: &Config) -> bool {
        write_json(&self.config_path(), c, true)
    }

    pub fn load_dock_items(&self) -> DockItems {
        let p = self.dock_items_path();
        let mut items = read_json::<DockItems>(&p, "Dock 固定项")
            .or_else(|| read_json::<DockItems>(&bak_of(&p), "Dock 固定项备份"))
            .unwrap_or_default();
        // 迁移旧版本保存的 Explorer 默认名称，保留用户自定义的标签。
        let mut migrated = false;
        for it in &mut items.items {
            if crate::win32::is_legacy_file_manager_label(&it.target, &it.label) {
                it.label = crate::win32::FILE_MANAGER_NAME.into();
                migrated = true;
            }
        }
        if migrated && p.exists() {
            let _ = self.save_dock_items(&items);
        }
        items
    }

    pub fn save_dock_items(&self, items: &DockItems) -> bool {
        write_json(&self.dock_items_path(), items, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_missing_fields_and_clamp() {
        let c = serde_json::from_str::<Config>(
            r#"{"dock":{"icon_size_dp":500,"display_mode":"weird"},"topbar":{"height_dp":99}}"#,
        )
        .unwrap()
        .normalized();
        assert_eq!(c.dock.icon_size_dp, 96);
        assert_eq!(c.dock.display_mode, "primary");
        assert_eq!(c.topbar.height_dp, 38);
        assert!(c.general.hide_taskbar_on_start);
        assert!(c.dock.show_window_preview);
        assert!(c.topbar.has_status("brightness"));
        assert!(!c.general.startup_initialized);
        assert_eq!(c.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn atomic_write_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        assert!(write_atomic(&p, b"{\"a\":1}", true));
        assert!(write_atomic(&p, b"{\"a\":2}", true));
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\":2}");
        assert_eq!(std::fs::read(bak_of(&p)).unwrap(), b"{\"a\":1}");
        assert!(!p.with_extension("tmp").exists());
    }

    #[test]
    fn load_dock_items_migrates_only_legacy_explorer_labels() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let items: DockItems = serde_json::from_str(r#"{"items":[
            {"id":"old","kind":"app","target":"C:/Windows/EXPLORER.EXE","label":"应用管理器"},
            {"id":"older","kind":"app","target":"explorer.exe","label":"访达"},
            {"id":"custom","kind":"app","target":"explorer.exe","label":"我的文件"},
            {"id":"other","kind":"app","target":"myexplorer.exe","label":"应用管理器"}
        ]}"#).unwrap();
        assert!(store.save_dock_items(&items));
        let migrated = store.load_dock_items();
        let labels: Vec<_> = migrated.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["文件管理器", "文件管理器", "我的文件", "应用管理器"]);
        let saved: DockItems = read_json(&store.dock_items_path(), "Dock 固定项").unwrap();
        assert_eq!(saved.items[0].label, "文件管理器");
    }
}
