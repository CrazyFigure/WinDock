//! Dock 窗口管理：纯净 macOS 分层图标交互窗 + 底部边缘触发窗、
//! 智能避让、波浪放大动画、拖放排序/取消固定/外部拖入、OLE 回收站拖放。

pub mod icons;
pub mod items;
pub mod layout;
pub mod preview;
pub mod render;

use std::time::Instant;
use windows::core::{implement, Result};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{HBRUSH, HFONT, HGDIOBJ, AC_SRC_OVER, BLENDFUNCTION, HDC};
use windows::Win32::System::Com::{IDataObject, FORMATETC, TYMED_HGLOBAL, DVASPECT_CONTENT};
use windows::Win32::System::Ole::{RegisterDragDrop, IDropTarget, IDropTarget_Impl, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, CF_HDROP};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Appearance, DockCfg};
use crate::dock::items::{EntryList, FuncKind};
use crate::dock::layout::DockLayout;
use crate::topbar::color_sync::{harmonize_dock_backdrop, DockGlassStrip, Rgb};
use crate::ui::{make_font, DibTarget};
use crate::win32::{dp, MonitorInfo};

pub const CLASS_OVERLAY: &str = "WinDockDockOverlay";
pub const CLASS_TRIGGER: &str = "WinDockDockTrigger";
pub const WM_APP_FRAME: u32 = WM_APP + 10;

#[derive(Clone, Debug)]
pub struct DragState {
    /// 拖拽条目在 `entries.apps` 中的索引
    pub from: usize,
    /// 当前鼠标在窗口坐标系下的位置
    pub x: f32,
    pub y: f32,
    /// 按下瞬间图标中心相对鼠标的偏移，避免起拖瞬间图标中心跳变
    pub grab_dx: f32,
    pub grab_dy: f32,
    /// 当前预览插入的固定项槽位索引（处于删除区域时为 None）
    pub target: Option<usize>,
    /// 当前鼠标是否悬停在回收站上
    pub over_trash: bool,
    /// 释放后是否从固定项移除
    pub remove: bool,
}

/// 收集当前应用区中所有固定项的索引。
pub fn pinned_app_indices(apps: &[crate::dock::items::Entry]) -> Vec<usize> {
    apps.iter()
        .enumerate()
        .filter_map(|(i, e)| e.pinned.map(|_| i))
        .collect()
}

/// 根据拖拽图标视觉中心 `preview_cx` 计算在静止 1.0x 基准槽位中最近的固定项目标索引。
pub fn compute_dock_drag_target(
    base_centers: &[f32],
    base_cap_left: f32,
    pinned_indices: &[usize],
    preview_cx: f32,
) -> Option<usize> {
    pinned_indices
        .iter()
        .copied()
        .filter(|&idx| idx < base_centers.len())
        .min_by(|&a, &b| {
            let da = (preview_cx - (base_cap_left + base_centers[a])).abs();
            let db = (preview_cx - (base_cap_left + base_centers[b])).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// 计算拖拽过程中条目 `orig_idx` 应该呈现的目标槽位索引（其他固定项顺滑让位）。
pub fn visual_slot_for_entry(
    orig_idx: usize,
    from: usize,
    target: Option<usize>,
    pinned_indices: &[usize],
) -> usize {
    let Some(target_idx) = target else {
        return orig_idx;
    };
    let Some(pos_from) = pinned_indices.iter().position(|&k| k == from) else {
        return orig_idx;
    };
    let Some(pos_target) = pinned_indices.iter().position(|&k| k == target_idx) else {
        return orig_idx;
    };
    let Some(pos_orig) = pinned_indices.iter().position(|&k| k == orig_idx) else {
        return orig_idx;
    };
    if orig_idx == from {
        return target_idx;
    }
    if pos_target > pos_from && pos_orig > pos_from && pos_orig <= pos_target {
        pinned_indices[pos_orig - 1]
    } else if pos_target < pos_from && pos_orig >= pos_target && pos_orig < pos_from {
        pinned_indices[pos_orig + 1]
    } else {
        orig_idx
    }
}

pub struct Dock {
    pub hwnd_overlay: HWND,
    pub hwnd_trigger: HWND,
    pub monitor: MonitorInfo,
    pub dpi: u32,
    pub target: DibTarget,
    pub text_scratch: DibTarget,
    pub trigger_target: DibTarget,
    pub tooltip_font: HFONT,
    pub entries: EntryList,
    pub scales: Vec<f32>,
    pub vels: Vec<f32>,
    pub layout: DockLayout,
    pub base_centers: Vec<f32>,
    pub base_capsule_w: f32,
    pub slot_draw_x: Vec<f32>,
    pub drop_settle: bool,
    pub last_drag_render: Option<Instant>,
    pub win_rect: RECT,
    pub win_size: (i32, i32),
    pub window_visible: bool,
    pub trigger_visible: bool,
    pub dirty: bool,
    pub show_t: f32,
    pub want_visible: bool,
    pub hover: Option<usize>,
    pub mouse: (f32, f32),
    pub mouse_in: bool,
    pub tooltip_visible: bool,
    pub hover_since: Option<Instant>,
    pub drag: Option<DragState>,
    pub press: Option<(usize, f32, f32)>,
    /// 启动应用时的经典弹跳反馈（槽位索引，进度 0..1）
    pub bounce: Option<(usize, f32)>,
    pub last_tick: Instant,
    pub drop_cookie: Option<u32>,
    // Dock 自适应背后颜色状态（优先跟随背后窗色，其次 Windows 系统主题，最后降级壁纸）
    pub backdrop_rgb: Option<Rgb>,
    pub backdrop_dirty: bool,
    // 液态玻璃低分辨率背景条带缓存（classic 模式下为 None，零常驻内存）
    pub glass_strip: Option<DockGlassStrip>,
    pub glass_dirty: bool,
    pub color_dark: bool,
    color_from_top: Rgb,
    color_to_top: Rgb,
    color_from_bot: Rgb,
    color_to_bot: Rgb,
    cur_top: Rgb,
    cur_bot: Rgb,
    color_t: f32,
    // 缓存的像素尺寸（relayout 时按 DPI 刷新）
    icon_px_cached: i32,
    gap_px: i32,
    corner_dp: i32,
    bottom_offset_px: i32,
    trigger_edge_px: i32,
    magnify_scale: f32,
    magnify_range: f32,
    hide_mode: String,
}

static CLASSES_REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn register_classes(hinst: windows::Win32::Foundation::HINSTANCE) {
    if CLASSES_REGISTERED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    unsafe {
        let hCursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default();
        let overlay = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(overlay_proc),
            hInstance: hinst,
            hCursor,
            hbrBackground: HBRUSH::default(),
            lpszClassName: crate::win32::wide_static(CLASS_OVERLAY),
            ..Default::default()
        };
        let _ = RegisterClassExW(&overlay);
        let trigger = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(trigger_proc),
            hInstance: hinst,
            hCursor,
            lpszClassName: crate::win32::wide_static(CLASS_TRIGGER),
            ..Default::default()
        };
        let _ = RegisterClassExW(&trigger);
    }
}

impl Dock {
    pub fn new(
        app_ptr: *mut crate::app::App,
        monitor: MonitorInfo,
        cfg: &DockCfg,
        appearance: &Appearance,
    ) -> std::result::Result<Dock, String> {
        let hinst = crate::win32::hinstance();
        register_classes(hinst);
        unsafe {
            let ex_common = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
            let overlay = CreateWindowExW(
                ex_common | WS_EX_LAYERED,
                crate::win32::wide_static(CLASS_OVERLAY),
                windows::core::w!(""),
                WS_POPUP,
                0,
                0,
                10,
                10,
                HWND::default(),
                HMENU::default(),
                hinst,
                Some(app_ptr as *const _),
            )
            .map_err(crate::win32::error_string)?;
            let trigger = CreateWindowExW(
                ex_common | WS_EX_LAYERED,
                crate::win32::wide_static(CLASS_TRIGGER),
                windows::core::w!(""),
                WS_POPUP,
                monitor.rect.left,
                monitor.rect.bottom - 1,
                monitor.width(),
                1,
                HWND::default(),
                HMENU::default(),
                hinst,
                Some(app_ptr as *const _),
            )
            .map_err(crate::win32::error_string)?;
            let fallback_dark = appearance.dock_fallback_dark();
            let (init_dark, init_top, init_bot) = harmonize_dock_backdrop(None, fallback_dark, fallback_dark);
            let mut dock = Dock {
                hwnd_overlay: overlay,
                hwnd_trigger: trigger,
                dpi: monitor.dpi,
                monitor,
                target: DibTarget::default(),
                text_scratch: DibTarget::default(),
                trigger_target: DibTarget::default(),
                tooltip_font: make_font(dp(13, monitor.dpi), 400),
                entries: EntryList::default(),
                scales: Vec::new(),
                vels: Vec::new(),
                layout: DockLayout::default(),
                base_centers: Vec::new(),
                base_capsule_w: 0.0,
                slot_draw_x: Vec::new(),
                drop_settle: false,
                last_drag_render: None,
                win_rect: RECT::default(),
                win_size: (0, 0),
                window_visible: false,
                trigger_visible: false,
                dirty: true,
                show_t: 0.0,
                want_visible: false,
                hover: None,
                mouse: (0.0, 0.0),
                mouse_in: false,
                tooltip_visible: false,
                hover_since: None,
                drag: None,
                press: None,
                bounce: None,
                last_tick: Instant::now(),
                drop_cookie: None,
                backdrop_rgb: None,
                backdrop_dirty: true,
                glass_strip: None,
                glass_dirty: appearance.dock_material == "liquid_glass",
                color_dark: init_dark,
                color_from_top: init_top,
                color_to_top: init_top,
                color_from_bot: init_bot,
                color_to_bot: init_bot,
                cur_top: init_top,
                cur_bot: init_bot,
                color_t: 1.0,
                icon_px_cached: 48,
                gap_px: 8,
                corner_dp: 18,
                bottom_offset_px: dp(cfg.bottom_offset_dp, monitor.dpi),
                trigger_edge_px: 3,
                magnify_scale: 1.45,
                magnify_range: 2.5,
                hide_mode: String::new(),
            };
            let _ = SetWindowLongPtrW(overlay, GWLP_USERDATA, app_ptr as isize);
            let _ = SetWindowLongPtrW(trigger, GWLP_USERDATA, app_ptr as isize);
            dock.relayout(cfg, appearance);
            Ok(dock)
        }
    }

    pub fn apply_appearance(&mut self, appearance: &Appearance) {
        self.apply_backdrop_color(self.backdrop_rgb, appearance, true);
        self.backdrop_dirty = true;
        if appearance.dock_material == "liquid_glass" {
            self.glass_dirty = true;
        } else {
            // 切回经典磨砂后立即释放液态玻璃缓存，零多余常驻内存
            self.glass_strip = None;
            self.glass_dirty = false;
        }
        self.dirty = true;
    }

    /// 刷新液态玻璃背景条带缓存（由后台/隐藏首帧采样回调写入）。
    pub fn apply_glass_strip(&mut self, strip: Option<DockGlassStrip>, appearance: &Appearance) {
        if appearance.dock_material != "liquid_glass" {
            self.glass_strip = None;
            self.glass_dirty = false;
            return;
        }
        if let Some(s) = strip {
            self.glass_strip = Some(s);
            self.glass_dirty = false;
            self.dirty = true;
        }
    }

    /// 返回液态玻璃所需的屏幕取样区域（涵盖窗口最大波浪宽度与胶囊高度及折射边距）。
    pub fn glass_zone_rect(&self) -> RECT {
        let cap = self.capsule_screen_rect();
        RECT {
            left: self.win_rect.left.max(self.monitor.rect.left),
            top: (cap.top - 12).max(self.monitor.rect.top),
            right: self.win_rect.right.min(self.monitor.rect.right),
            bottom: (self.win_rect.bottom + 8).min(self.monitor.rect.bottom),
        }
    }

    /// 返回当前未受 Tooltip、放大图标与投影遮挡的上方安全采样下边界。
    pub fn glass_above_y_hint(&self) -> Option<i32> {
        if self.window_visible
            && self.show_t >= 0.99
            && !self.tooltip_visible
            && self.hover.is_none()
            && self.bounce.is_none()
            && self.drag.is_none()
        {
            // ui::fill_liquid_glass_capsule 顶部外投影扩展 ceil(5 * s_dpi)，再留 3px 安全透明余量
            let safe_pad = (6.0 * self.dpi_scale()).ceil() as i32 + 2;
            Some(self.capsule_screen_rect().top - safe_pad)
        } else {
            None
        }
    }

    /// 返回左右侧翼紧贴采样可安全使用的屏幕 X 区间 `(左翼右边界, 右翼左边界)`，避开阴影与悬停放大区。
    pub fn glass_flank_x_bounds(&self) -> (i32, i32) {
        if self.window_visible
            && self.show_t >= 0.99
            && self.hover.is_none()
            && self.bounce.is_none()
            && self.drag.is_none()
        {
            let cap = self.capsule_screen_rect();
            let sh_pad = (12.0 * self.dpi_scale()).ceil() as i32 + 4;
            (
                (cap.left - sh_pad).max(self.win_rect.left - 4),
                (cap.right + sh_pad).min(self.win_rect.right + 4),
            )
        } else {
            (self.win_rect.left - 4, self.win_rect.right + 4)
        }
    }

    /// 根据背后采样色与外观回退链刷新 Dock 胶囊目标配色，并控制是否瞬切或平滑插值过渡。
    pub fn apply_backdrop_color(&mut self, backdrop: Option<Rgb>, appearance: &Appearance, instant: bool) {
        let effective = if appearance.theme == "system" { backdrop } else { None };
        let fallback_dark = appearance.dock_fallback_dark();
        let (dark, top, bot) = harmonize_dock_backdrop(effective, self.color_dark, fallback_dark);
        self.backdrop_rgb = effective;
        self.backdrop_dirty = false;
        if dark == self.color_dark && top == self.color_to_top && bot == self.color_to_bot && self.color_t >= 1.0 {
            return;
        }
        self.color_dark = dark;
        self.color_from_top = self.cur_top;
        self.color_from_bot = self.cur_bot;
        self.color_to_top = top;
        self.color_to_bot = bot;
        if instant || appearance.reduce_motion {
            self.color_t = 1.0;
            self.cur_top = top;
            self.cur_bot = bot;
        } else {
            self.color_t = 0.0;
        }
        self.dirty = true;
    }

    /// 返回当前帧渲染使用的 Dock 明暗极性与上下渐变色。
    pub fn capsule_colors(&self) -> (bool, Rgb, Rgb) {
        (self.color_dark, self.cur_top, self.cur_bot)
    }

    /// 返回胶囊在屏幕坐标系下的矩形区域（用于定位背后覆盖的应用窗口）。
    pub fn capsule_screen_rect(&self) -> RECT {
        let cap = self.capsule_rect();
        RECT {
            left: self.win_rect.left + cap.left,
            top: self.win_rect.top + cap.top,
            right: self.win_rect.left + cap.right,
            bottom: self.win_rect.top + cap.bottom,
        }
    }

    // ---------- 尺寸工具 ----------

    pub fn dpi_scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    pub fn icon_px(&self) -> i32 {
        self.icon_px_cached
    }

    pub fn pad_x_px(&self) -> i32 {
        dp(10, self.dpi)
    }
    pub fn pad_y_px(&self) -> i32 {
        dp(8, self.dpi)
    }
    pub fn margin_x(&self) -> i32 {
        dp(22, self.dpi)
    }
    pub fn tooltip_h(&self) -> i32 {
        dp(38, self.dpi)
    }
    pub fn margin_y(&self) -> i32 {
        dp(12, self.dpi)
    }
    /// 窗口内胶囊下方用于承载底部阴影的实际留白（不超过胶囊到屏幕底边的真实间距）。
    pub fn bottom_margin_y(&self) -> i32 {
        self.bottom_offset_px.max(0).min(self.margin_y())
    }
    pub fn corner_radius(&self) -> f32 {
        dp(self.corner_dp, self.dpi) as f32
    }
    pub fn monitor_offset_px(&self) -> i32 {
        self.bottom_offset_px
    }
    pub fn capsule_h(&self) -> f32 {
        self.layout.capsule_h
    }

    pub fn entry_count(&self) -> usize {
        self.entries.apps.len() + self.entries.right.len()
    }

    pub fn entry_key_label(&self, i: usize) -> (String, String) {
        let n = self.entries.apps.len();
        if i < n {
            let e = &self.entries.apps[i];
            (e.icon_key.clone(), e.label.clone())
        } else {
            let f = &self.entries.right[i - n];
            (f.icon_key.clone(), f.label.clone())
        }
    }

    pub fn entry_is_running(&self, i: usize) -> bool {
        let n = self.entries.apps.len();
        i < n && !self.entries.apps[i].windows.is_empty()
    }

    pub fn func_kind(&self, i: usize) -> Option<FuncKind> {
        let n = self.entries.apps.len();
        if i < n {
            None
        } else {
            Some(self.entries.right[i - n].kind.clone())
        }
    }

    fn separator_after(&self) -> Vec<usize> {
        let n = self.entries.apps.len();
        if n > 0 && !self.entries.right.is_empty() {
            vec![n - 1]
        } else {
            Vec::new()
        }
    }

    /// 触发指定槽位的启动弹跳动效。
    pub fn trigger_bounce(&mut self, index: usize) {
        self.bounce = Some((index, 0.0));
    }

    /// 胶囊在窗口内的矩形（底部仅预留 bottom_margin_y，确保真正贴底时零多余缝隙）。
    pub fn capsule_rect(&self) -> RECT {
        let (w, h) = self.win_size;
        let cw = self.layout.capsule_w.round() as i32;
        let ch = self.layout.capsule_h.round() as i32;
        let left = ((w - cw) as f32 * 0.5).round() as i32;
        let bot_margin = self.bottom_margin_y();
        RECT {
            left,
            top: h - bot_margin - ch,
            right: left + cw,
            bottom: h - bot_margin,
        }
    }

    /// 1.0x 静止状态下的胶囊基准矩形（用于拖拽目标判定，避免放大收缩期间坐标漂移）。
    pub fn base_capsule_rect(&self) -> RECT {
        let (w, h) = self.win_size;
        let cw = self.base_capsule_w.round() as i32;
        let ch = self.layout.capsule_h.round() as i32;
        let left = ((w - cw) as f32 * 0.5).round() as i32;
        let bot_margin = self.bottom_margin_y();
        RECT {
            left,
            top: h - bot_margin - ch,
            right: left + cw,
            bottom: h - bot_margin,
        }
    }

    /// 计算 Dock 覆盖窗的垂直位置与高度：返回 `(win_top, win_h, bot_margin)`。
    ///
    /// 保证胶囊底边精确落在 `mon_bottom - bottom_offset_px`，同时窗口底边绝不超过 `mon_bottom`。
    fn compute_vertical_bounds(
        mon_bottom: i32,
        capsule_h: i32,
        grow_top: i32,
        tooltip_h: i32,
        top_margin_y: i32,
        bottom_offset_px: i32,
    ) -> (i32, i32, i32) {
        let offset = bottom_offset_px.max(0);
        let bot_margin = offset.min(top_margin_y);
        let win_h = capsule_h + grow_top + tooltip_h + top_margin_y + bot_margin;
        let win_offset = (offset - bot_margin).max(0);
        let win_top = mon_bottom - win_offset - win_h;
        (win_top, win_h, bot_margin)
    }

    /// 重新计算基础布局与窗口尺寸/位置（配置或条目变化时调用）。
    pub fn relayout(&mut self, cfg: &DockCfg, appearance: &Appearance) {
        self.icon_px_cached = dp(cfg.icon_size_dp, self.dpi);
        self.gap_px = dp(cfg.icon_gap_dp, self.dpi);
        self.corner_dp = appearance.dock_corner_radius_dp;
        self.bottom_offset_px = dp(cfg.bottom_offset_dp, self.dpi);
        self.trigger_edge_px = cfg.trigger_edge_px.clamp(1, 12);
        self.magnify_scale = if cfg.magnify_enabled { cfg.magnify_scale } else { 1.0 };
        self.magnify_range = cfg.magnify_range;
        self.hide_mode = cfg.hide_mode.clone();
        let n = self.entry_count();
        self.scales.resize(n.max(self.scales.len()), 1.0);
        self.scales.truncate(n);
        self.vels.resize(n.max(self.vels.len()), 0.0);
        self.vels.truncate(n);
        let seps = self.separator_after();
        let base = layout::compute(
            n,
            &vec![1.0; n],
            &seps,
            self.icon_px() as f32,
            self.gap_px as f32,
            self.pad_x_px() as f32,
            self.pad_y_px() as f32,
            self.magnify_scale,
        );
        self.base_centers = base.slots.iter().map(|s| s.center_x).collect();
        self.base_capsule_w = base.capsule_w;
        self.layout = base.clone();
        // 窗口尺寸：基础胶囊 + 最大放大增量 + 边距（并保证极端精简 Dock 下也能完整容纳“WinDock 设置”Tooltip）；高度：胶囊 + 向上突破 + Tooltip
        let grow_w = (self.icon_px() as f32 * (self.magnify_scale - 1.0) * self.magnify_range * 2.0 * 0.6).round() as i32;
        let w = (base.capsule_w.round() as i32 + grow_w + self.margin_x() * 2).max(dp(180, self.dpi));
        let mon = self.monitor;
        let (top, h, _) = Self::compute_vertical_bounds(
            mon.rect.bottom,
            base.capsule_h.round() as i32,
            base.grow_top.round() as i32,
            self.tooltip_h(),
            self.margin_y(),
            self.bottom_offset_px,
        );
        let mut left = mon.rect.left + (mon.width() - w) / 2;
        left = left.clamp(mon.rect.left, (mon.rect.right - w).max(mon.rect.left));
        let prev_rect = self.win_rect;
        self.win_size = (w.max(1), h.max(1));
        self.win_rect = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        if self.drag.is_none() && !self.drop_settle {
            let cap_left = self.capsule_rect().left as f32;
            self.slot_draw_x = self.layout.slots.iter().map(|s| cap_left + s.center_x).collect();
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd_overlay,
                HWND_TOPMOST,
                left,
                top,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
            // 触发窗：严格贴合屏幕底部边缘并保持置顶（不可加 SWP_NOMOVE，否则位置不更新）
            let _ = SetWindowPos(
                self.hwnd_trigger,
                HWND_TOPMOST,
                mon.rect.left,
                mon.rect.bottom - self.trigger_edge_px,
                mon.width(),
                self.trigger_edge_px,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        }
        self.dirty = true;
        if self.win_rect != prev_rect {
            self.backdrop_dirty = true;
            if appearance.dock_material == "liquid_glass" {
                self.glass_dirty = true;
            }
        }
        if !self.target.ensure_at_least(w, h) {
            crate::log::log("Dock DIB 分配失败");
        }
        let _ = self.trigger_target.ensure(mon.width().max(1), self.trigger_edge_px.max(1));
        self.update_trigger_pixels();
    }

    fn update_trigger_pixels(&mut self) {
        let (w, h) = (self.monitor.width().max(1), self.trigger_edge_px.max(1));
        let Some(dc) = self.trigger_target.mem_dc else { return };
        let Some(mut sf) = self.trigger_target.surface() else { return };
        sf.clear_rect_transparent(w, h);
        for y in 0..h {
            for x in 0..w {
                sf.blend_premul_over(x, y, 0, 0, 0, 1);
            }
        }
        let dst = POINT {
            x: self.monitor.rect.left,
            y: self.monitor.rect.bottom - h,
        };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };
        unsafe {
            let _ = UpdateLayeredWindow(self.hwnd_trigger, HDC::default(), Some(&dst), Some(&size), dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    pub fn set_entries(&mut self, list: EntryList) {
        self.entries = list;
        let n = self.entry_count();
        self.scales.resize(n, 1.0);
        self.vels.resize(n, 0.0);
        if !self.drop_settle && self.drag.is_none() {
            let cap_left = self.capsule_rect().left as f32;
            self.slot_draw_x.resize(n, 0.0);
            for i in 0..n.min(self.layout.slots.len()) {
                self.slot_draw_x[i] = cap_left + self.layout.slots[i].center_x;
            }
        }
        self.hover = self.hover.filter(|h| *h < n);
        self.dirty = true;
        self.backdrop_dirty = true;
    }

    /// 获取条目当前用于渲染的水平中心坐标（拖拽让位或落位过渡期间返回平滑插值坐标）。
    pub fn slot_x(&self, i: usize) -> f32 {
        if (self.drag.is_some() || self.drop_settle) && i < self.slot_draw_x.len() {
            return self.slot_draw_x[i];
        }
        self.capsule_rect().left as f32 + self.layout.slots.get(i).map(|s| s.center_x).unwrap_or(0.0)
    }

    /// 获取当前拖拽状态下某条目的视觉目标槽位索引。
    pub fn visual_slot(&self, orig_idx: usize) -> usize {
        let Some(d) = &self.drag else { return orig_idx };
        let pinned = pinned_app_indices(&self.entries.apps);
        visual_slot_for_entry(orig_idx, d.from, d.target, &pinned)
    }

    /// 进入 Dock 拖拽模式，记录按下偏移并锁定当前所有图标初始绘制位置。
    pub fn begin_drag(&mut self, from: usize, press_x: f32, press_y: f32, x: f32, y: f32) {
        let Some(slot) = self.layout.slots.get(from).copied() else { return };
        if self.entries.apps.get(from).and_then(|e| e.pinned).is_none() {
            return;
        }
        let cap = self.capsule_rect();
        let n = self.entry_count();
        self.slot_draw_x = (0..n)
            .map(|i| cap.left as f32 + self.layout.slots.get(i).map(|s| s.center_x).unwrap_or(0.0))
            .collect();
        let slot_cx = cap.left as f32 + slot.center_x;
        let slot_cy = cap.top as f32 + slot.center_y;
        let half = slot.size * 0.5;
        let grab_dx = (slot_cx - press_x).clamp(-half, half);
        let grab_dy = (slot_cy - press_y).clamp(-half, half);
        self.hover = None;
        self.hover_since = None;
        self.tooltip_visible = false;
        self.drop_settle = false;
        self.last_drag_render = None;
        self.drag = Some(DragState {
            from,
            x,
            y,
            grab_dx,
            grab_dy,
            target: Some(from),
            over_trash: false,
            remove: false,
        });
        self.update_drag(x, y);
    }

    /// 根据当前鼠标坐标刷新 Dock 拖拽目标槽位与移除判定，保证预览与落点完全一致。
    pub fn update_drag(&mut self, x: f32, y: f32) {
        let s_dpi = self.dpi_scale();
        let base_cap = self.base_capsule_rect();
        let cap = self.capsule_rect();
        let app_n = self.entries.apps.len();
        let mut over_trash = false;
        for (ri, f) in self.entries.right.iter().enumerate() {
            if matches!(f.kind, FuncKind::Trash) {
                let si = app_n + ri;
                if let Some(slot) = self.layout.slots.get(si) {
                    let cx = cap.left as f32 + slot.center_x;
                    let half = slot.size * 0.5 + 4.0 * s_dpi;
                    if x >= cx - half
                        && x <= cx + half
                        && y >= base_cap.top as f32 - 14.0 * s_dpi
                        && y <= base_cap.bottom as f32 + 14.0 * s_dpi
                    {
                        over_trash = true;
                        break;
                    }
                }
            }
        }
        let below = y > base_cap.bottom as f32 + 12.0 * s_dpi;
        let far_above = y < base_cap.top as f32 - (self.icon_px() as f32 * 0.95).max(36.0 * s_dpi);
        let remove = over_trash || below || far_above;
        let pinned = pinned_app_indices(&self.entries.apps);
        let Some(d) = self.drag.as_mut() else { return };
        d.x = x;
        d.y = y;
        d.over_trash = over_trash;
        d.remove = remove;
        if remove {
            d.target = None;
        } else {
            let preview_cx = x + d.grab_dx;
            d.target = compute_dock_drag_target(&self.base_centers, base_cap.left as f32, &pinned, preview_cx)
                .or(Some(d.from));
        }
        self.dirty = true;
    }

    /// 在重排后映射 `slot_draw_x`，使被拖放的图标从释放位置平滑落入目标槽位且其余图标无跳变。
    pub fn prepare_drop_settle(&mut self, from: usize, target: usize, drop_gx: f32) {
        let n = self.entry_count();
        if from >= n || target >= n || self.slot_draw_x.len() != n {
            return;
        }
        let pinned = pinned_app_indices(&self.entries.apps);
        let mut new_draw_x = self.slot_draw_x.clone();
        let mut new_scales = self.scales.clone();
        let mut new_vels = self.vels.clone();
        for orig in 0..n {
            let vis = visual_slot_for_entry(orig, from, Some(target), &pinned);
            if vis < n {
                new_draw_x[vis] = if orig == from { drop_gx } else { self.slot_draw_x[orig] };
                if orig < self.scales.len() && vis < new_scales.len() {
                    new_scales[vis] = self.scales[orig];
                    new_vels[vis] = self.vels[orig];
                }
            }
        }
        self.slot_draw_x = new_draw_x;
        self.scales = new_scales;
        self.vels = new_vels;
        self.drop_settle = true;
    }

    pub fn set_want_visible(&mut self, v: bool) {
        if self.want_visible != v {
            self.want_visible = v;
            self.dirty = true;
            if v {
                self.backdrop_dirty = true;
                self.glass_dirty = true;
            } else {
                self.hover = None;
                self.tooltip_visible = false;
                self.drag = None;
                self.press = None;
                self.drop_settle = false;
                unsafe {
                    if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == self.hwnd_overlay {
                        let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    }
                }
            }
        }
    }

    /// 触发窗显隐：仅在自动隐藏/智能避让且 Dock 当前隐藏时显示，并刷新置顶层级防止被遮挡。
    pub fn update_trigger(&mut self, hidden_by_mode: bool) {
        let want = hidden_by_mode && !self.want_visible && self.hide_mode != "always";
        if want != self.trigger_visible {
            self.trigger_visible = want;
            unsafe {
                if want {
                    let _ = SetWindowPos(
                        self.hwnd_trigger,
                        HWND_TOPMOST,
                        self.monitor.rect.left,
                        self.monitor.rect.bottom - self.trigger_edge_px,
                        self.monitor.width(),
                        self.trigger_edge_px,
                        SWP_NOACTIVATE | SWP_NOSENDCHANGING,
                    );
                    let _ = ShowWindow(self.hwnd_trigger, SW_SHOWNA);
                } else {
                    let _ = ShowWindow(self.hwnd_trigger, SW_HIDE);
                }
            }
        }
    }

    /// 推进一帧动画；返回是否仍需继续动画。
    pub fn tick(&mut self, dt_ms: f32, _cfg: &DockCfg, appearance: &Appearance, transition_ms: u32) -> bool {
        let dt = dt_ms / 1000.0;
        let reduce = appearance.reduce_motion;
        let mut animating = false;
        // 显隐
        let target_t = if self.want_visible { 1.0 } else { 0.0 };
        let new_t = layout::ease_to(self.show_t, target_t, dt_ms, if reduce { 0.0 } else { 200.0 }, reduce);
        if (new_t - self.show_t).abs() > 0.0005 {
            animating = true;
        }
        self.show_t = new_t;
        if !self.want_visible && self.show_t <= 0.001 {
            if self.window_visible {
                unsafe {
                    let _ = ShowWindow(self.hwnd_overlay, SW_HIDE);
                }
                self.window_visible = false;
            }
            self.backdrop_dirty = true;
            self.glass_dirty = true;
            if self.color_t < 1.0 {
                self.color_t = 1.0;
                self.cur_top = self.color_to_top;
                self.cur_bot = self.color_to_bot;
            }
            return false;
        }
        // 背景融合色彩平滑过渡
        if self.color_t < 1.0 {
            let step = if reduce || transition_ms == 0 {
                1.0
            } else {
                dt_ms / transition_ms.max(1) as f32
            };
            self.color_t = (self.color_t + step).min(1.0);
            self.cur_top = Rgb::lerp(self.color_from_top, self.color_to_top, self.color_t);
            self.cur_bot = Rgb::lerp(self.color_from_bot, self.color_to_bot, self.color_t);
            self.dirty = true;
            if self.color_t < 1.0 {
                animating = true;
            }
        }
        // 放大弹簧（拖拽期间无过冲平滑回落到 1.0x，避免胶囊边缘抖动）
        let mouse_x = if self.mouse_in { Some(self.mouse.0) } else { None };
        let pitch = (self.icon_px() + self.gap_px) as f32;
        let cap_left = self.capsule_rect().left as f32;
        let n = self.entry_count();
        for i in 0..n.min(self.scales.len()) {
            let base = *self.base_centers.get(i).unwrap_or(&0.0);
            if self.drag.is_some() {
                let before = self.scales[i];
                let next = layout::ease_to(before, 1.0, dt_ms, if reduce { 0.0 } else { 80.0 }, reduce);
                self.scales[i] = next;
                self.vels[i] = 0.0;
                if (next - 1.0).abs() > 0.002 {
                    animating = true;
                }
            } else {
                let target = match mouse_x {
                    Some(mx) if self.magnify_scale > 1.0 => {
                        let d = (mx - cap_left - base).abs();
                        1.0 + (self.magnify_scale - 1.0) * layout::influence(d, pitch, self.magnify_range)
                    }
                    _ => 1.0,
                };
                let before = self.scales[i];
                self.scales[i] = layout::spring_step(before, &mut self.vels[i], target, dt, 260.0, 0.84);
                if (self.scales[i] - target).abs() > 0.002 || self.vels[i].abs() > 0.02 {
                    animating = true;
                }
            }
        }
        // 启动弹跳动画（480ms 完成一次柔和起落）
        if let Some((idx, prog)) = self.bounce {
            let next = if reduce { 1.0 } else { prog + dt_ms / 480.0 };
            if next >= 1.0 {
                self.bounce = None;
            } else {
                self.bounce = Some((idx, next));
                animating = true;
            }
        }
        // 布局（拖拽期间把各条目缩放映射至其让位后的视觉槽位，防止让位图标借用旧槽位缩放瞬间变大）
        let seps = self.separator_after();
        let layout_scales: Vec<f32> = if let Some(d) = &self.drag {
            let pinned = pinned_app_indices(&self.entries.apps);
            let mut vs = vec![1.0f32; n];
            for orig in 0..n.min(self.scales.len()) {
                if orig == d.from {
                    continue;
                }
                let vis = visual_slot_for_entry(orig, d.from, d.target, &pinned);
                if vis < n {
                    vs[vis] = self.scales[orig];
                }
            }
            vs
        } else {
            self.scales.clone()
        };
        self.layout = layout::compute(
            n,
            &layout_scales,
            &seps,
            self.icon_px() as f32,
            self.gap_px as f32,
            self.pad_x_px() as f32,
            self.pad_y_px() as f32,
            self.magnify_scale,
        );
        // 拖拽让位与松手落位水平平滑插值
        let cap_left = self.capsule_rect().left as f32;
        if self.slot_draw_x.len() != n {
            self.slot_draw_x.resize(n, 0.0);
            for i in 0..n.min(self.layout.slots.len()) {
                self.slot_draw_x[i] = cap_left + self.layout.slots[i].center_x;
            }
        }
        if let Some(d) = self.drag.clone() {
            let pinned = pinned_app_indices(&self.entries.apps);
            for i in 0..n.min(self.layout.slots.len()) {
                let vis = visual_slot_for_entry(i, d.from, d.target, &pinned);
                let target_x = cap_left + self.layout.slots[vis].center_x;
                let next = layout::ease_to(self.slot_draw_x[i], target_x, dt_ms, if reduce { 0.0 } else { 85.0 }, reduce);
                if (next - target_x).abs() > 0.25 {
                    animating = true;
                }
                self.slot_draw_x[i] = next;
            }
        } else if self.drop_settle {
            let mut still = false;
            for i in 0..n.min(self.layout.slots.len()) {
                let target_x = cap_left + self.layout.slots[i].center_x;
                let next = layout::ease_to(self.slot_draw_x[i], target_x, dt_ms, if reduce { 0.0 } else { 90.0 }, reduce);
                if (next - target_x).abs() > 0.25 {
                    still = true;
                    animating = true;
                }
                self.slot_draw_x[i] = next;
            }
            self.drop_settle = still;
        } else {
            for i in 0..n.min(self.layout.slots.len()) {
                self.slot_draw_x[i] = cap_left + self.layout.slots[i].center_x;
            }
        }
        // Tooltip 延迟
        if let Some(t) = self.hover_since {
            if self.hover.is_some() && !self.tooltip_visible && t.elapsed().as_millis() >= 100 {
                self.tooltip_visible = true;
            }
            if self.hover.is_some() && !self.tooltip_visible {
                animating = true;
            }
        }
        self.last_tick = Instant::now();
        animating
    }

    pub fn animating(&self) -> bool {
        let target_t = if self.want_visible { 1.0 } else { 0.0 };
        (self.show_t - target_t).abs() > 0.001
            || self.drag.is_some()
            || self.drop_settle
            || self.scales.iter().enumerate().any(|(i, s)| {
                let base = *self.base_centers.get(i).unwrap_or(&0.0);
                let d = if self.mouse_in { (self.mouse.0 - self.capsule_rect().left as f32 - base).abs() } else { f32::MAX };
                let t = if self.mouse_in && self.magnify_scale > 1.0 {
                    1.0 + (self.magnify_scale - 1.0) * layout::influence(d, (self.icon_px() + self.gap_px) as f32, self.magnify_range)
                } else {
                    1.0
                };
                (s - t).abs() > 0.003
            })
            || (self.hover.is_some() && !self.tooltip_visible)
            || self.color_t < 1.0
    }

    // ---------- 命中 ----------

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let cap = self.capsule_rect();
        layout::hit_slot(&self.layout, x - cap.left as f32, y - cap.top as f32, 2.0)
    }

    #[allow(dead_code)]
    pub fn in_capsule(&self, x: f32, y: f32) -> bool {
        let cap = self.capsule_rect();
        x >= cap.left as f32 && x <= cap.right as f32 && y >= cap.top as f32 - 10.0 && y <= cap.bottom as f32
    }

    // ---------- OLE 拖放 ----------

    pub fn register_drop(&mut self) {
        unsafe {
            let target = DropTarget { hwnd: self.hwnd_overlay };
            let obj: IDropTarget = target.into();
            if RegisterDragDrop(self.hwnd_overlay, &obj).is_ok() {
                self.drop_cookie = Some(1);
            }
        }
    }

    pub fn destroy(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Ole::RevokeDragDrop(self.hwnd_overlay);
            let _ = DestroyWindow(self.hwnd_overlay);
            let _ = DestroyWindow(self.hwnd_trigger);
            if !self.tooltip_font.is_invalid() {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(self.tooltip_font.0));
            }
        }
        self.target.release();
        self.text_scratch.release();
        self.trigger_target.release();
    }
}

// ---------- OLE IDropTarget ----------

#[implement(IDropTarget)]
struct DropTarget {
    hwnd: HWND,
}

fn data_object_files(data: Option<&IDataObject>) -> Vec<String> {
    let Some(data) = data else { return Vec::new() };
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0 as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    if unsafe { data.QueryGetData(&fmt) }.is_err() {
        return Vec::new();
    }
    let mut files = Vec::new();
    unsafe {
        if let Ok(mut medium) = data.GetData(&fmt) {
            if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                let hdrop = windows::Win32::Foundation::HGLOBAL(medium.u.hGlobal.0);
                let count = DragQueryFileW(HDROP(hdrop.0), u32::MAX, None);
                for i in 0..count {
                    let mut buf = [0u16; 1024];
                    let n = DragQueryFileW(HDROP(hdrop.0), i, Some(&mut buf));
                    if n > 0 {
                        files.push(crate::win32::from_wide(&buf[..n as usize]));
                    }
                }
            }
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
        }
    }
    files
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, pdataobj: Option<&IDataObject>, _grfkeystate: MODIFIERKEYS_FLAGS, pt: &windows::Win32::Foundation::POINTL, pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT) -> Result<()> {
        let files = data_object_files(pdataobj);
        unsafe {
            *pdweffect = if files.is_empty() { DROPEFFECT_NONE } else { DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE };
        }
        if !files.is_empty() {
            crate::app::with_app(self.hwnd, |app| app.dock_external_drag(self.hwnd, pt.x, pt.y, Some(files)));
        }
        Ok(())
    }

    fn DragOver(&self, _grfkeystate: MODIFIERKEYS_FLAGS, pt: &windows::Win32::Foundation::POINTL, pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT) -> Result<()> {
        unsafe {
            *pdweffect = DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE;
        }
        crate::app::with_app(self.hwnd, |app| app.dock_external_drag(self.hwnd, pt.x, pt.y, None));
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        crate::app::with_app(self.hwnd, |app| app.dock_external_drag_leave(self.hwnd));
        Ok(())
    }

    fn Drop(&self, pdataobj: Option<&IDataObject>, _grfkeystate: MODIFIERKEYS_FLAGS, pt: &windows::Win32::Foundation::POINTL, pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT) -> Result<()> {
        let files = data_object_files(pdataobj);
        unsafe {
            *pdweffect = if files.is_empty() { DROPEFFECT_NONE } else { DROPEFFECT_COPY };
        }
        crate::app::with_app(self.hwnd, |app| app.dock_external_drop(self.hwnd, pt.x, pt.y, files));
        Ok(())
    }
}

// ---------- 窗口过程 ----------

unsafe extern "system" fn trigger_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE | WM_NCMOUSEMOVE => {
            crate::app::with_app(hwnd, |app| app.dock_edge_hovered(hwnd));
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
        }
        windows::Win32::UI::Controls::WM_MOUSELEAVE => {
            crate::app::with_app(hwnd, |app| app.dock_edge_left(hwnd));
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let x = crate::win32::get_x_lparam(lparam.0) as f32;
            let y = crate::win32::get_y_lparam(lparam.0) as f32;
            crate::app::with_app(hwnd, |app| app.dock_mouse_move(hwnd, x, y));
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            return LRESULT(0);
        }
        windows::Win32::UI::Controls::WM_MOUSELEAVE => {
            crate::app::with_app(hwnd, |app| app.dock_mouse_leave(hwnd));
        }
        WM_LBUTTONDOWN => {
            let x = crate::win32::get_x_lparam(lparam.0) as f32;
            let y = crate::win32::get_y_lparam(lparam.0) as f32;
            let _ = SetCapture(hwnd);
            crate::app::with_app(hwnd, |app| app.dock_lbutton_down(hwnd, x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = crate::win32::get_x_lparam(lparam.0) as f32;
            let y = crate::win32::get_y_lparam(lparam.0) as f32;
            // 必须先处理抬起事件再释放捕获，否则 ReleaseCapture 同步触发 WM_CAPTURECHANGED 会提前清空 press/drag
            crate::app::with_app(hwnd, |app| app.dock_lbutton_up(hwnd, x, y));
            let _ = ReleaseCapture();
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            let x = crate::win32::get_x_lparam(lparam.0) as f32;
            let y = crate::win32::get_y_lparam(lparam.0) as f32;
            crate::app::with_app(hwnd, |app| app.dock_rbutton_up(hwnd, x, y));
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            crate::app::with_app(hwnd, |app| app.dock_capture_lost(hwnd));
        }
        WM_SETCURSOR => {
            let _ = SetCursor(LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default());
            return LRESULT(1);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_bounds_align_capsule_bottom_across_all_offsets() {
        let mon_bottom = 1600;
        let capsule_h = 87;
        let grow_top = 28;
        let tooltip_h = 57;
        let top_margin_y = 18; // 12dp @ 150% DPI

        for (offset_px, expected_bot_margin) in [(0, 0), (8, 8), (18, 18), (26, 18), (33, 18), (42, 18)] {
            let (win_top, win_h, bot_margin) = Dock::compute_vertical_bounds(
                mon_bottom,
                capsule_h,
                grow_top,
                tooltip_h,
                top_margin_y,
                offset_px,
            );
            assert_eq!(bot_margin, expected_bot_margin);
            let win_bottom = win_top + win_h;
            let capsule_screen_bottom = win_top + (win_h - bot_margin);
            // 胶囊底边必须精确位于 mon_bottom - offset_px，且窗口底边永不越出显示器
            assert_eq!(capsule_screen_bottom, mon_bottom - offset_px);
            assert!(win_bottom <= mon_bottom);
        }
    }

    #[test]
    fn dock_drag_target_and_visual_slots_match_final_order() {
        use crate::dock::items::Entry;
        let mk = |id: &str, pinned: Option<usize>| Entry {
            id: id.into(),
            pinned,
            running_exe: None,
            label: id.into(),
            icon_key: id.into(),
            windows: Vec::new(),
        };
        // 含前置未固定启动台(0)、3个固定应用(1..=3)、1个未固定运行应用(4)
        let apps = vec![
            mk("special:launchpad", None),
            mk("a", Some(0)),
            mk("b", Some(1)),
            mk("c", Some(2)),
            mk("run:x", None),
        ];
        let pinned = pinned_app_indices(&apps);
        assert_eq!(pinned, vec![1, 2, 3]);

        // 5 个应用槽位中心（步距 56px）
        let base_centers = [34.0, 90.0, 146.0, 202.0, 258.0];
        let cap_left = 100.0;

        // 拖动 a(1) 越过 b(2) 中心后落在槽位 2，预览槽位为 [b->1, gap->2, c->3]
        let t1 = compute_dock_drag_target(&base_centers, cap_left, &pinned, cap_left + 150.0);
        assert_eq!(t1, Some(2));
        assert_eq!(visual_slot_for_entry(0, 1, t1, &pinned), 0);
        assert_eq!(visual_slot_for_entry(2, 1, t1, &pinned), 1);
        assert_eq!(visual_slot_for_entry(3, 1, t1, &pinned), 3);
        assert_eq!(visual_slot_for_entry(4, 1, t1, &pinned), 4);

        // 拖动最右 c(3) 向左越过 a(1) 中心，落在槽位 1，a/b 依次右移让位
        let t2 = compute_dock_drag_target(&base_centers, cap_left, &pinned, cap_left + 85.0);
        assert_eq!(t2, Some(1));
        assert_eq!(visual_slot_for_entry(1, 3, t2, &pinned), 2);
        assert_eq!(visual_slot_for_entry(2, 3, t2, &pinned), 3);
        assert_eq!(visual_slot_for_entry(0, 3, t2, &pinned), 0);
        assert_eq!(visual_slot_for_entry(4, 3, t2, &pinned), 4);
    }

    #[test]
    fn dock_drag_sweep_preview_gap_always_matches_committed_order() {
        use crate::config::DockItems;
        use crate::dock::items::{new_item, reorder_app_entries, Entry};
        use crate::dock::layout;

        let items_vec = vec![
            new_item("app", r"C:\1.exe", "One"),
            new_item("app", r"C:\2.exe", "Two"),
            new_item("app", r"C:\3.exe", "Three"),
            new_item("app", r"C:\4.exe", "Four"),
        ];
        let mut di_base = DockItems::default();
        for (idx, mut it) in items_vec.into_iter().enumerate() {
            it.order = idx as u32;
            di_base.items.push(it);
        }
        let entries_base: Vec<Entry> = di_base
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| Entry {
                id: it.id.clone(),
                pinned: Some(i),
                running_exe: None,
                label: it.label.clone(),
                icon_key: it.target.clone(),
                windows: Vec::new(),
            })
            .collect();
        let pinned = pinned_app_indices(&entries_base);

        // 在 100% 和 150% DPI 下全区间扫掠鼠标位置与抓手偏移
        for scale in [1.0f32, 1.5f32] {
            let icon = 48.0 * scale;
            let gap = 8.0 * scale;
            let pad_x = 10.0 * scale;
            let pad_y = 8.0 * scale;
            let l = layout::compute(4, &[1.0; 4], &[], icon, gap, pad_x, pad_y, 1.45);
            let centers: Vec<f32> = l.slots.iter().map(|s| s.center_x).collect();
            let cap_left = 120.0;
            for from in 0..4 {
                for grab_dx in [-12.0 * scale, 0.0, 12.0 * scale] {
                    let mut x = cap_left - 20.0;
                    while x <= cap_left + l.capsule_w + 20.0 {
                        let preview_cx = x + grab_dx;
                        let target = compute_dock_drag_target(&centers, cap_left, &pinned, preview_cx).unwrap();
                        // 1) 所有其他固定项映射后的槽位互不冲突且恰好空出 target
                        let mut occupied = [false; 4];
                        for orig in 0..4 {
                            if orig == from {
                                continue;
                            }
                            let vis = visual_slot_for_entry(orig, from, Some(target), &pinned);
                            assert!(!occupied[vis], "视觉槽位发生重叠");
                            occupied[vis] = true;
                        }
                        assert!(!occupied[target], "目标槽位未空出");

                        // 2) 提交重排后的最终下标必须严格等于预览空出的 target
                        let mut di = di_base.clone();
                        let mut ordered: Vec<String> = entries_base.iter().map(|e| e.id.clone()).collect();
                        let moved = ordered.remove(from);
                        ordered.insert(target, moved);
                        let _ = reorder_app_entries(&mut di, &ordered);
                        let final_pos = di.sorted().iter().position(|it| it.id == entries_base[from].id).unwrap();
                        assert_eq!(final_pos, target, "预览目标 {target} 与提交落点 {final_pos} 不一致");
                        x += 7.0;
                    }
                }
            }
        }
    }

    #[test]
    fn dock_stateful_drag_preserves_grab_offset_animates_slots_and_detects_remove_zones() {
        use crate::dock::items::{Entry, EntryList, FuncEntry, FuncKind};
        let mon = crate::win32::primary_monitor().unwrap_or(MonitorInfo {
            hmonitor: windows::Win32::Graphics::Gdi::HMONITOR::default(),
            rect: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            work: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            dpi: 96,
            primary: true,
        });
        let cfg = DockCfg::default();
        let app_cfg = Appearance::default();
        let mut dock = Dock::new(std::ptr::null_mut(), mon, &cfg, &app_cfg).expect("Dock test window should create");
        let mk = |id: &str, p: usize| Entry {
            id: id.into(),
            pinned: Some(p),
            running_exe: None,
            label: id.into(),
            icon_key: "special:settings".into(),
            windows: Vec::new(),
        };
        dock.set_entries(EntryList {
            apps: vec![mk("a", 0), mk("b", 1), mk("c", 2)],
            right: vec![FuncEntry {
                kind: FuncKind::Trash,
                label: "回收站".into(),
                icon_key: "special:trash".into(),
            }],
        });
        dock.relayout(&cfg, &app_cfg);
        dock.want_visible = true;
        dock.show_t = 1.0;

        // 模拟按下第 0 项右侧偏 12px 处起拖
        let cap = dock.capsule_rect();
        let slot0 = dock.layout.slots[0];
        let slot0_cx = cap.left as f32 + slot0.center_x;
        let slot0_cy = cap.top as f32 + slot0.center_y;
        let press_x = slot0_cx + 12.0;
        let press_y = slot0_cy + 4.0;
        dock.begin_drag(0, press_x, press_y, press_x + 6.0, press_y);
        let d0 = dock.drag.as_ref().expect("drag should start");
        assert!((d0.grab_dx - (-12.0)).abs() < 0.01);
        assert!((d0.grab_dy - (-4.0)).abs() < 0.01);
        // 拖拽图标视觉中心从 slot0_cx + 6 开始，零跳变
        assert!(((d0.x + d0.grab_dx) - (slot0_cx + 6.0)).abs() < 0.01);
        assert_eq!(d0.target, Some(0));

        // 拖到第 2 项位置，应选中 target=2 并触发第 1、2 项向左平滑让位
        let base_cap = dock.base_capsule_rect();
        let slot2_cx = base_cap.left as f32 + dock.base_centers[2];
        dock.update_drag(slot2_cx - d0.grab_dx, slot0_cy);
        assert_eq!(dock.drag.as_ref().unwrap().target, Some(2));
        assert!(!dock.drag.as_ref().unwrap().remove);
        for _ in 0..30 {
            let _ = dock.tick(16.0, &cfg, &app_cfg, 150);
        }
        // 第 1 项应已滑到槽位 0，第 2 项应已滑到槽位 1
        let final_cap = dock.capsule_rect();
        assert!((dock.slot_x(1) - (final_cap.left as f32 + dock.layout.slots[0].center_x)).abs() < 1.0);
        assert!((dock.slot_x(2) - (final_cap.left as f32 + dock.layout.slots[1].center_x)).abs() < 1.0);

        // 拖到回收站上方应触发 over_trash 与 remove
        let trash_cx = final_cap.left as f32 + dock.layout.slots[3].center_x;
        dock.update_drag(trash_cx, slot0_cy);
        assert!(dock.drag.as_ref().unwrap().over_trash);
        assert!(dock.drag.as_ref().unwrap().remove);
        assert_eq!(dock.drag.as_ref().unwrap().target, None);

        // 拖回 Dock 内再向上拖出移除阈值，应识别为 remove，拖回则恢复重排
        dock.update_drag(slot2_cx, base_cap.top as f32 - 60.0 * dock.dpi_scale());
        assert!(dock.drag.as_ref().unwrap().remove);
        dock.update_drag(slot2_cx, slot0_cy);
        assert!(!dock.drag.as_ref().unwrap().remove);
        assert!(dock.drag.as_ref().unwrap().target.is_some());

        dock.destroy();
    }
}
