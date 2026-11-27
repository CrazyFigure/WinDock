//! 配置字段描述：标题、提示、读写与取值范围（设置页数据驱动模型）。

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    HideTaskbar,
    TaskbarAutohide,
    AutoCheckUpdate,
    HotkeyDock,
    HotkeyTopbar,
    HotkeyRestore,
    LaunchpadPath,
    DockEnabled,
    DockDisplayMode,
    DockHideMode,
    TriggerEdge,
    ShowDelay,
    HideDelay,
    BottomOffset,
    IconSize,
    IconGap,
    Magnify,
    MagnifyScale,
    MagnifyRange,
    ShowRunning,
    ShowDot,
    ShowTrash,
    ShowFolders,
    ShowWindowPreview,
    TopbarEnabled,
    TopbarDisplayMode,
    TopbarHeight,
    AdaptiveColor,
    ColorSpeed,
    ShowActiveApp,
    ClockFormat,
    StIme,
    StNetwork,
    StBrightness,
    StVolume,
    StBattery,
    FullscreenYield,
    Theme,
    DockMaterial,
    Opacity,
    DockRadius,
    IconRoundness,
    AccentColor,
    ReduceMotion,
}

pub type Fmt = fn(f32) -> String;

pub type Options = &'static [(&'static str, &'static str)];

const DISPLAY_DOCK: Options = &[("primary", "仅主屏"), ("cursor", "跟随鼠标屏"), ("all", "所有屏")];
const DISPLAY_BAR: Options = &[("primary", "仅主屏"), ("all", "所有屏")];
const HIDE_MODE: Options = &[("smart_maximized", "智能避让"), ("always", "始终显示"), ("auto", "自动隐藏")];
const THEME: Options = &[("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")];
const DOCK_MATERIAL: Options = &[("liquid_glass", "液态玻璃"), ("classic", "经典磨砂")];
const ROUNDNESS: Options = &[("macos", "圆角"), ("none", "原始")];
const ACCENT: Options = &[
    ("system", "系统"),
    ("#0A84FF", "蓝"),
    ("#30D158", "绿"),
    ("#FF9F0A", "橙"),
    ("#FF453A", "红"),
    ("#BF5AF2", "紫"),
];
const CLOCK: Options = &[
    ("M月d日 E HH:mm", "月日 周 时分"),
    ("M月d日 HH:mm", "月日 时分"),
    ("E HH:mm", "周 时分"),
    ("HH:mm", "24小时"),
    ("12h:mm", "12小时"),
];
const TRIGGER_EDGE_OPTS: Options = &[("2", "2 px"), ("4", "4 px"), ("6", "6 px"), ("8", "8 px"), ("10", "10 px")];
// 弹出延迟：默认 80ms，按 40ms 等差递增保留「立即 / 快 / 默认 / 慢」四档
const SHOW_DELAY_OPTS: Options = &[("0", "立即"), ("40", "快"), ("80", "默认"), ("120", "慢")];
const HIDE_DELAY_OPTS: Options = &[("150", "快"), ("250", "默认"), ("500", "较慢"), ("800", "慢")];
// 底部间距：0dp 为真正贴底，其余四档按 6dp 等差递增，默认使用 6dp（紧凑）
const BOTTOM_OFFSET_OPTS: Options = &[
    ("0", "贴底"),
    ("6", "紧凑"),
    ("12", "适中"),
    ("18", "宽松"),
    ("24", "极宽"),
];
const ICON_SIZE_OPTS: Options = &[
    ("36", "小"),
    ("42", "较小"),
    ("48", "中"),
    ("54", "大"),
    ("60", "超大"),
];
const ICON_GAP_OPTS: Options = &[("4", "紧凑"), ("8", "默认"), ("12", "适中"), ("16", "宽松")];
const MAGNIFY_SCALE_OPTS: Options = &[("1.2", "轻微"), ("1.45", "适中"), ("1.7", "明显"), ("2.0", "强烈")];
const MAGNIFY_RANGE_OPTS: Options = &[("1.5", "窄"), ("2.5", "默认"), ("3.5", "宽"), ("4.5", "极宽")];
const TOPBAR_HEIGHT_OPTS: Options = &[("26", "紧凑"), ("30", "标准"), ("34", "宽松"), ("38", "大号")];
const COLOR_SPEED_OPTS: Options = &[("0", "立即"), ("100", "快"), ("150", "默认"), ("300", "柔和")];
const OPACITY_OPTS: Options = &[("0.55", "通透"), ("0.78", "标准"), ("0.90", "浓郁"), ("1.0", "不透明")];
const DOCK_RADIUS_OPTS: Options = &[("10", "小"), ("15", "标准"), ("22", "大"), ("30", "胶囊")];

fn nearest_opt(val: f32, opts: Options) -> usize {
    opts.iter()
        .enumerate()
        .min_by(|(_, (a, _)), (_, (b, _))| {
            let da = (a.parse::<f32>().unwrap_or(0.0) - val).abs();
            let db = (b.parse::<f32>().unwrap_or(0.0) - val).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn fmt_int(v: f32) -> String {
    format!("{v:.0}")
}

fn fmt_ms(v: f32) -> String {
    format!("{v:.0} ms")
}

fn fmt_dp(v: f32) -> String {
    format!("{v:.0} dp")
}

fn fmt_px(v: f32) -> String {
    format!("{v:.0} px")
}

fn fmt_x1(v: f32) -> String {
    format!("{v:.2}×")
}

fn fmt_pct(v: f32) -> String {
    format!("{:.0}%", v * 100.0)
}

impl Field {
    pub fn title(&self) -> &'static str {
        match self {
            Field::HideTaskbar => "启动时隐藏 Windows 任务栏",
            Field::TaskbarAutohide => "同步开启系统任务栏自动隐藏",
            Field::AutoCheckUpdate => "自动检查更新",
            Field::HotkeyDock => "显示 / 隐藏 Dock",
            Field::HotkeyTopbar => "显示 / 隐藏顶部栏",
            Field::HotkeyRestore => "紧急恢复任务栏",
            Field::LaunchpadPath => "WinLaunchpad 路径",
            Field::DockEnabled => "启用 Dock",
            Field::DockDisplayMode => "显示位置",
            Field::DockHideMode => "显隐模式",
            Field::TriggerEdge => "触发边缘高度",
            Field::ShowDelay => "弹出延迟",
            Field::HideDelay => "隐藏延迟",
            Field::BottomOffset => "底部间距",
            Field::IconSize => "图标尺寸",
            Field::IconGap => "图标间距",
            Field::Magnify => "悬停放大",
            Field::MagnifyScale => "放大倍率",
            Field::MagnifyRange => "波浪影响范围",
            Field::ShowRunning => "显示运行中的应用",
            Field::ShowDot => "显示运行指示点",
            Field::ShowTrash => "显示回收站",
            Field::ShowFolders => "显示文件夹堆栈",
            Field::ShowWindowPreview => "悬停预览应用窗口",
            Field::TopbarEnabled => "启用顶部栏",
            Field::TopbarDisplayMode => "多屏策略",
            Field::TopbarHeight => "栏高",
            Field::AdaptiveColor => "最大化窗口自动取色适配",
            Field::ColorSpeed => "取色过渡速度",
            Field::ShowActiveApp => "显示前台应用名称",
            Field::ClockFormat => "时钟格式",
            Field::StIme => "输入法",
            Field::StNetwork => "网络",
            Field::StBrightness => "屏幕亮度",
            Field::StVolume => "音量",
            Field::StBattery => "电量",
            Field::FullscreenYield => "全屏时自动让位",
            Field::Theme => "主题模式",
            Field::DockMaterial => "Dock 材质",
            Field::Opacity => "背景不透明度",
            Field::DockRadius => "Dock 圆角",
            Field::IconRoundness => "图标圆角裁切",
            Field::AccentColor => "强调色",
            Field::ReduceMotion => "减少动态效果",
        }
    }

    pub fn tip(&self) -> &'static str {
        match self {
            Field::HideTaskbar => "启动后隐藏任务栏；退出或异常时由守护进程自动恢复。",
            Field::TaskbarAutohide => "消除最大化窗口底部 48px 空白，退出时还原原始状态。",
            Field::AutoCheckUpdate => "启动 30 秒后低频检测一次正式版本。",
            Field::HotkeyDock => "点击后按下新组合键（需含 Ctrl/Alt/Win）；Esc 取消，Backspace 恢复默认。",
            Field::HotkeyTopbar => "点击后按下新组合键（需含 Ctrl/Alt/Win）；Esc 取消，Backspace 恢复默认。",
            Field::HotkeyRestore => "由守护进程监听，主进程卡死或崩溃时也能呼出任务栏。",
            Field::LaunchpadPath => "选择 WinLaunchpad.exe 后 Dock 右侧出现「启动台」入口。",
            Field::DockHideMode => "智能避让：有最大化窗口时下沉，鼠标触底呼出。",
            Field::TriggerEdge => "屏幕底部可触发 Dock 浮出的边缘高度。",
            Field::MagnifyRange => "悬停时两侧受波浪影响的图标数量。",
            Field::AdaptiveColor => "前台窗口最大化时，顶部栏自动同步其标题栏颜色。",
            Field::ColorSpeed => "进入 / 退出最大化时颜色过渡的时长。",
            Field::FullscreenYield => "全屏游戏或视频时隐藏顶部栏与 Dock。",
            Field::ShowWindowPreview => "悬停在运行中的应用图标上时，显示其打开窗口的实时缩略图。",
            Field::DockMaterial => "液态玻璃：通透光学折射与菲涅尔亮边；经典磨砂：纯净柔和半透明托盘。",
            Field::Opacity => "调节顶部栏壁纸融合通透度与 Dock 托盘不透明度。",
            Field::AccentColor => "自定义控件与高亮强调色，选择首项自动跟随 Windows 主题色。",
            Field::ReduceMotion => "关闭波浪弹簧与显隐滑动动画。",
            _ => "",
        }
    }

    pub fn is_hotkey(&self) -> bool {
        matches!(self, Field::HotkeyDock | Field::HotkeyTopbar | Field::HotkeyRestore)
    }

    pub fn get_bool(&self, c: &Config) -> Option<bool> {
        let g = &c.general;
        let d = &c.dock;
        let t = &c.topbar;
        Some(match self {
            Field::HideTaskbar => g.hide_taskbar_on_start,
            Field::TaskbarAutohide => g.taskbar_autohide,
            Field::AutoCheckUpdate => g.auto_check_update,
            Field::DockEnabled => d.enabled,
            Field::Magnify => d.magnify_enabled,
            Field::ShowRunning => d.show_running_apps,
            Field::ShowDot => d.show_running_dot,
            Field::ShowTrash => d.show_trash,
            Field::ShowFolders => d.show_folders,
            Field::ShowWindowPreview => d.show_window_preview,
            Field::TopbarEnabled => t.enabled,
            Field::AdaptiveColor => t.adaptive_maximized_color,
            Field::ShowActiveApp => t.show_active_app,
            Field::StIme => t.has_status("ime"),
            Field::StNetwork => t.has_status("network"),
            Field::StBrightness => t.has_status("brightness"),
            Field::StVolume => t.has_status("volume"),
            Field::StBattery => t.has_status("battery"),
            Field::FullscreenYield => t.fullscreen_yield,
            Field::ReduceMotion => c.appearance.reduce_motion,
            _ => return None,
        })
    }

    pub fn set_bool(&self, c: &mut Config, v: bool) {
        let name = match self {
            Field::StIme => Some("ime"),
            Field::StNetwork => Some("network"),
            Field::StBrightness => Some("brightness"),
            Field::StVolume => Some("volume"),
            Field::StBattery => Some("battery"),
            _ => None,
        };
        if let Some(name) = name {
            c.topbar.status_items.retain(|s| s != name);
            if v {
                // 保持固定顺序：输入法、网络、音量、亮度、电量
                let order = ["ime", "network", "volume", "brightness", "battery"];
                let pos = order.iter().position(|o| *o == name).unwrap_or(0);
                let insert_at = c
                    .topbar
                    .status_items
                    .iter()
                    .position(|s| order.iter().position(|o| *o == s.as_str()).unwrap_or(9) > pos)
                    .unwrap_or(c.topbar.status_items.len());
                c.topbar.status_items.insert(insert_at, name.to_string());
            }
            return;
        }
        match self {
            Field::HideTaskbar => c.general.hide_taskbar_on_start = v,
            Field::TaskbarAutohide => c.general.taskbar_autohide = v,
            Field::AutoCheckUpdate => c.general.auto_check_update = v,
            Field::DockEnabled => c.dock.enabled = v,
            Field::Magnify => c.dock.magnify_enabled = v,
            Field::ShowRunning => c.dock.show_running_apps = v,
            Field::ShowDot => c.dock.show_running_dot = v,
            Field::ShowTrash => c.dock.show_trash = v,
            Field::ShowFolders => c.dock.show_folders = v,
            Field::ShowWindowPreview => c.dock.show_window_preview = v,
            Field::TopbarEnabled => c.topbar.enabled = v,
            Field::AdaptiveColor => c.topbar.adaptive_maximized_color = v,
            Field::ShowActiveApp => c.topbar.show_active_app = v,
            Field::FullscreenYield => c.topbar.fullscreen_yield = v,
            Field::ReduceMotion => c.appearance.reduce_motion = v,
            _ => {}
        }
    }

    /// (当前值, 最小, 最大, 步长, 格式化)
    pub fn get_num(&self, c: &Config) -> Option<(f32, f32, f32, f32, Fmt)> {
        let d = &c.dock;
        let t = &c.topbar;
        Some(match self {
            Field::TriggerEdge => (d.trigger_edge_px as f32, 1.0, 12.0, 1.0, fmt_px),
            Field::ShowDelay => (d.show_delay_ms as f32, 0.0, 400.0, 10.0, fmt_ms),
            Field::HideDelay => (d.hide_delay_ms as f32, 0.0, 1500.0, 10.0, fmt_ms),
            Field::BottomOffset => (d.bottom_offset_dp as f32, 0.0, 40.0, 1.0, fmt_dp),
            Field::IconSize => (d.icon_size_dp as f32, 28.0, 96.0, 2.0, fmt_dp),
            Field::IconGap => (d.icon_gap_dp as f32, 2.0, 24.0, 1.0, fmt_dp),
            Field::MagnifyScale => (d.magnify_scale, 1.1, 2.5, 0.05, fmt_x1),
            Field::MagnifyRange => (d.magnify_range, 1.0, 5.0, 0.5, fmt_x1),
            Field::TopbarHeight => (t.height_dp as f32, 24.0, 38.0, 1.0, fmt_dp),
            Field::ColorSpeed => (t.color_transition_ms as f32, 0.0, 1000.0, 10.0, fmt_ms),
            Field::Opacity => (c.appearance.opacity, 0.2, 1.0, 0.01, fmt_pct),
            Field::DockRadius => (c.appearance.dock_corner_radius_dp as f32, 4.0, 40.0, 1.0, fmt_dp),
            _ => return None,
        })
    }

    pub fn set_num(&self, c: &mut Config, v: f32) {
        match self {
            Field::TriggerEdge => c.dock.trigger_edge_px = v.round() as i32,
            Field::ShowDelay => c.dock.show_delay_ms = v.round() as u32,
            Field::HideDelay => c.dock.hide_delay_ms = v.round() as u32,
            Field::BottomOffset => c.dock.bottom_offset_dp = v.round() as i32,
            Field::IconSize => c.dock.icon_size_dp = v.round() as i32,
            Field::IconGap => c.dock.icon_gap_dp = v.round() as i32,
            Field::MagnifyScale => c.dock.magnify_scale = v,
            Field::MagnifyRange => c.dock.magnify_range = v,
            Field::TopbarHeight => c.topbar.height_dp = v.round() as i32,
            Field::ColorSpeed => c.topbar.color_transition_ms = v.round() as u32,
            Field::Opacity => c.appearance.opacity = v,
            Field::DockRadius => c.appearance.dock_corner_radius_dp = v.round() as i32,
            _ => {}
        }
    }

    /// (当前索引, 选项)
    pub fn get_choice(&self, c: &Config) -> Option<(usize, Options)> {
        match self {
            Field::TriggerEdge => return Some((nearest_opt(c.dock.trigger_edge_px as f32, TRIGGER_EDGE_OPTS), TRIGGER_EDGE_OPTS)),
            Field::ShowDelay => return Some((nearest_opt(c.dock.show_delay_ms as f32, SHOW_DELAY_OPTS), SHOW_DELAY_OPTS)),
            Field::HideDelay => return Some((nearest_opt(c.dock.hide_delay_ms as f32, HIDE_DELAY_OPTS), HIDE_DELAY_OPTS)),
            Field::BottomOffset => return Some((nearest_opt(c.dock.bottom_offset_dp as f32, BOTTOM_OFFSET_OPTS), BOTTOM_OFFSET_OPTS)),
            Field::IconSize => return Some((nearest_opt(c.dock.icon_size_dp as f32, ICON_SIZE_OPTS), ICON_SIZE_OPTS)),
            Field::IconGap => return Some((nearest_opt(c.dock.icon_gap_dp as f32, ICON_GAP_OPTS), ICON_GAP_OPTS)),
            Field::MagnifyScale => return Some((nearest_opt(c.dock.magnify_scale, MAGNIFY_SCALE_OPTS), MAGNIFY_SCALE_OPTS)),
            Field::MagnifyRange => return Some((nearest_opt(c.dock.magnify_range, MAGNIFY_RANGE_OPTS), MAGNIFY_RANGE_OPTS)),
            Field::TopbarHeight => return Some((nearest_opt(c.topbar.height_dp as f32, TOPBAR_HEIGHT_OPTS), TOPBAR_HEIGHT_OPTS)),
            Field::ColorSpeed => return Some((nearest_opt(c.topbar.color_transition_ms as f32, COLOR_SPEED_OPTS), COLOR_SPEED_OPTS)),
            Field::Opacity => return Some((nearest_opt(c.appearance.opacity, OPACITY_OPTS), OPACITY_OPTS)),
            Field::DockRadius => return Some((nearest_opt(c.appearance.dock_corner_radius_dp as f32, DOCK_RADIUS_OPTS), DOCK_RADIUS_OPTS)),
            _ => {}
        }
        let (cur, opts): (String, Options) = match self {
            Field::DockDisplayMode => (c.dock.display_mode.clone(), DISPLAY_DOCK),
            Field::DockHideMode => (c.dock.hide_mode.clone(), HIDE_MODE),
            Field::TopbarDisplayMode => (c.topbar.display_mode.clone(), DISPLAY_BAR),
            Field::ClockFormat => (c.topbar.clock_format.clone(), CLOCK),
            Field::Theme => (c.appearance.theme.clone(), THEME),
            Field::DockMaterial => (c.appearance.dock_material.clone(), DOCK_MATERIAL),
            Field::IconRoundness => (c.appearance.icon_roundness.clone(), ROUNDNESS),
            Field::AccentColor => (c.appearance.accent_color.clone(), ACCENT),
            _ => return None,
        };
        Some((opts.iter().position(|(k, _)| *k == cur).unwrap_or(0), opts))
    }

    pub fn set_choice(&self, c: &mut Config, idx: usize) {
        let pick = |opts: Options| opts.get(idx).map(|(k, _)| k.to_string());
        let pick_f = |opts: Options| opts.get(idx).and_then(|(k, _)| k.parse::<f32>().ok());
        match self {
            Field::DockDisplayMode => { pick(DISPLAY_DOCK).map(|v| c.dock.display_mode = v); }
            Field::DockHideMode => { pick(HIDE_MODE).map(|v| c.dock.hide_mode = v); }
            Field::TopbarDisplayMode => { pick(DISPLAY_BAR).map(|v| c.topbar.display_mode = v); }
            Field::ClockFormat => { pick(CLOCK).map(|v| c.topbar.clock_format = v); }
            Field::Theme => { pick(THEME).map(|v| c.appearance.theme = v); }
            Field::DockMaterial => { pick(DOCK_MATERIAL).map(|v| c.appearance.dock_material = v); }
            Field::IconRoundness => { pick(ROUNDNESS).map(|v| c.appearance.icon_roundness = v); }
            Field::AccentColor => { pick(ACCENT).map(|v| c.appearance.accent_color = v); }
            Field::TriggerEdge => if let Some(v) = pick_f(TRIGGER_EDGE_OPTS) { c.dock.trigger_edge_px = v.round() as i32; },
            Field::ShowDelay => if let Some(v) = pick_f(SHOW_DELAY_OPTS) { c.dock.show_delay_ms = v.round() as u32; },
            Field::HideDelay => if let Some(v) = pick_f(HIDE_DELAY_OPTS) { c.dock.hide_delay_ms = v.round() as u32; },
            Field::BottomOffset => if let Some(v) = pick_f(BOTTOM_OFFSET_OPTS) { c.dock.bottom_offset_dp = v.round() as i32; },
            Field::IconSize => if let Some(v) = pick_f(ICON_SIZE_OPTS) { c.dock.icon_size_dp = v.round() as i32; },
            Field::IconGap => if let Some(v) = pick_f(ICON_GAP_OPTS) { c.dock.icon_gap_dp = v.round() as i32; },
            Field::MagnifyScale => if let Some(v) = pick_f(MAGNIFY_SCALE_OPTS) { c.dock.magnify_scale = v; },
            Field::MagnifyRange => if let Some(v) = pick_f(MAGNIFY_RANGE_OPTS) { c.dock.magnify_range = v; },
            Field::TopbarHeight => if let Some(v) = pick_f(TOPBAR_HEIGHT_OPTS) { c.topbar.height_dp = v.round() as i32; },
            Field::ColorSpeed => if let Some(v) = pick_f(COLOR_SPEED_OPTS) { c.topbar.color_transition_ms = v.round() as u32; },
            Field::Opacity => if let Some(v) = pick_f(OPACITY_OPTS) { c.appearance.opacity = v; },
            Field::DockRadius => if let Some(v) = pick_f(DOCK_RADIUS_OPTS) { c.appearance.dock_corner_radius_dp = v.round() as i32; },
            _ => {}
        };
    }

    pub fn get_text(&self, c: &Config) -> Option<String> {
        let g = &c.general;
        Some(match self {
            Field::HotkeyDock => g.toggle_dock_hotkey.clone(),
            Field::HotkeyTopbar => g.toggle_topbar_hotkey.clone(),
            Field::HotkeyRestore => g.emergency_restore_hotkey.clone(),
            Field::LaunchpadPath => g.winlaunchpad_path.clone(),
            _ => return None,
        })
    }

    pub fn set_text(&self, c: &mut Config, v: String) {
        match self {
            Field::HotkeyDock => c.general.toggle_dock_hotkey = v,
            Field::HotkeyTopbar => c.general.toggle_topbar_hotkey = v,
            Field::HotkeyRestore => c.general.emergency_restore_hotkey = v,
            Field::LaunchpadPath => c.general.winlaunchpad_path = v,
            _ => {}
        }
    }

    /// 本页包含的全部可配置字段（用于「恢复本页默认」）。
    pub fn set_from(&self, dst: &mut Config, src: &Config) {
        if let Some(v) = self.get_bool(src) {
            self.set_bool(dst, v);
        }
        if let Some((v, _, _, _, _)) = self.get_num(src) {
            self.set_num(dst, v);
        } else if let Some((i, _)) = self.get_choice(src) {
            self.set_choice(dst, i);
        }
        if let Some(v) = self.get_text(src) {
            self.set_text(dst, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_presets_map_numeric_fields() {
        let mut cfg = Config::default();
        for (idx, dp_val) in [36, 42, 48, 54, 60].into_iter().enumerate() {
            Field::IconSize.set_choice(&mut cfg, idx);
            assert_eq!(cfg.dock.icon_size_dp, dp_val);
            assert_eq!(Field::IconSize.get_choice(&cfg).map(|(i, _)| i), Some(idx));
        }
        Field::Opacity.set_choice(&mut cfg, 0);
        assert!((cfg.appearance.opacity - 0.55).abs() < 0.01);
        assert_eq!(Field::Opacity.get_choice(&cfg).map(|(i, _)| i), Some(0));

        assert_eq!(Field::DockMaterial.get_choice(&cfg).map(|(i, _)| i), Some(0));
        Field::DockMaterial.set_choice(&mut cfg, 1);
        assert_eq!(cfg.appearance.dock_material, "classic");
        assert_eq!(Field::DockMaterial.get_choice(&cfg).map(|(i, _)| i), Some(1));
        Field::DockMaterial.set_from(&mut cfg, &Config::default());
        assert_eq!(cfg.appearance.dock_material, "liquid_glass");

        // 底部间距：默认值 6dp 对应「紧凑」（索引 1），五档按 0/6/12/18/24 双向映射无偏差
        let def = Config::default();
        assert_eq!(Field::BottomOffset.get_choice(&def).map(|(i, _)| i), Some(1));
        for (idx, (dp_val, label)) in [
            (0, "贴底"),
            (6, "紧凑"),
            (12, "适中"),
            (18, "宽松"),
            (24, "极宽"),
        ]
        .into_iter()
        .enumerate()
        {
            Field::BottomOffset.set_choice(&mut cfg, idx);
            assert_eq!(cfg.dock.bottom_offset_dp, dp_val);
            let (got_idx, opts) = Field::BottomOffset.get_choice(&cfg).unwrap();
            assert_eq!(got_idx, idx);
            assert_eq!(opts[got_idx].1, label);
        }

        // 弹出延迟：默认值 80ms 对应「默认」（索引 2），四档按 0/40/80/120 双向映射无偏差
        assert_eq!(Field::ShowDelay.get_choice(&def).map(|(i, _)| i), Some(2));
        for (idx, (ms_val, label)) in [
            (0, "立即"),
            (40, "快"),
            (80, "默认"),
            (120, "慢"),
        ]
        .into_iter()
        .enumerate()
        {
            Field::ShowDelay.set_choice(&mut cfg, idx);
            assert_eq!(cfg.dock.show_delay_ms, ms_val);
            let (got_idx, opts) = Field::ShowDelay.get_choice(&cfg).unwrap();
            assert_eq!(got_idx, idx);
            assert_eq!(opts[got_idx].1, label);
        }
    }
}
