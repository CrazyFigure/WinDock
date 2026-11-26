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
use crate::ui::{make_font, DibTarget};
use crate::win32::{dp, MonitorInfo};

pub const CLASS_OVERLAY: &str = "WinDockFinderDockOverlay";
pub const CLASS_TRIGGER: &str = "WinDockFinderDockTrigger";
pub const WM_APP_FRAME: u32 = WM_APP + 10;

#[derive(Clone, Debug)]
pub struct DragState {
    pub from: usize,
    pub x: f32,
    pub y: f32,
    pub over: Option<usize>,
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
                icon_px_cached: 48,
                gap_px: 8,
                corner_dp: 18,
                bottom_offset_px: 6,
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

    pub fn apply_appearance(&mut self, _appearance: &Appearance) {
        self.dirty = true;
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

    /// 胶囊在窗口内的矩形。
    pub fn capsule_rect(&self) -> RECT {
        let (w, h) = self.win_size;
        let cw = self.layout.capsule_w.round() as i32;
        let ch = self.layout.capsule_h.round() as i32;
        let left = ((w - cw) as f32 * 0.5).round() as i32;
        RECT {
            left,
            top: h - self.margin_y() - ch,
            right: left + cw,
            bottom: h - self.margin_y(),
        }
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
        self.layout = base.clone();
        // 窗口尺寸：基础胶囊 + 最大放大增量 + 边距；高度：胶囊 + 向上突破 + Tooltip
        let grow_w = (self.icon_px() as f32 * (self.magnify_scale - 1.0) * self.magnify_range * 2.0 * 0.6).round() as i32;
        let w = base.capsule_w.round() as i32 + grow_w + self.margin_x() * 2;
        let h = base.capsule_h.round() as i32 + base.grow_top.round() as i32 + self.tooltip_h() + self.margin_y() * 2;
        let mon = self.monitor;
        let mut left = mon.rect.left + (mon.width() - w) / 2;
        left = left.clamp(mon.rect.left, (mon.rect.right - w).max(mon.rect.left));
        let top = mon.rect.bottom - self.bottom_offset_px - h;
        self.win_size = (w.max(1), h.max(1));
        self.win_rect = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
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
        self.hover = self.hover.filter(|h| *h < n);
        self.dirty = true;
    }

    pub fn set_want_visible(&mut self, v: bool) {
        if self.want_visible != v {
            self.want_visible = v;
            self.dirty = true;
            if !v {
                self.hover = None;
                self.tooltip_visible = false;
                self.drag = None;
                self.press = None;
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
    pub fn tick(&mut self, dt_ms: f32, _cfg: &DockCfg, appearance: &Appearance) -> bool {
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
            return false;
        }
        // 放大弹簧
        let mouse_x = if self.mouse_in { Some(self.mouse.0) } else { None };
        let pitch = (self.icon_px() + self.gap_px) as f32;
        let cap_left = self.capsule_rect().left as f32;
        let n = self.entry_count();
        for i in 0..n.min(self.scales.len()) {
            let base = *self.base_centers.get(i).unwrap_or(&0.0);
            let target = match (mouse_x, self.drag.as_ref()) {
                (Some(mx), None) if self.magnify_scale > 1.0 => {
                    let d = (mx - cap_left - base).abs();
                    1.0 + (self.magnify_scale - 1.0) * layout::influence(d, pitch, self.magnify_range)
                }
                _ => 1.0,
            };
            let _v = self.vels[i];
            let before = self.scales[i];
            self.scales[i] = layout::spring_step(before, &mut self.vels[i], target, dt, 260.0, 0.84);
            if (self.scales[i] - target).abs() > 0.002 || self.vels[i].abs() > 0.02 {
                animating = true;
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
        // 布局
        let seps = self.separator_after();
        self.layout = layout::compute(
            n,
            &self.scales,
            &seps,
            self.icon_px() as f32,
            self.gap_px as f32,
            self.pad_x_px() as f32,
            self.pad_y_px() as f32,
            self.magnify_scale,
        );
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
    }

    // ---------- 命中 ----------

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let cap = self.capsule_rect();
        layout::hit_slot(&self.layout, x - cap.left as f32, y - cap.top as f32, 2.0)
    }

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
