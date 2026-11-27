//! 顶部栏与收纳格共用的多行状态提示。

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, MonitorFromRect, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::ui::{
    draw_transparent_text_aligned, make_font, measure_text, rgb, DibTarget, TextAlign,
};
use crate::win32::{dp, hinstance, monitor_info};

pub const TIMER_TRAY_TIP: usize = 5;

#[derive(PartialEq, Eq)]
pub struct TipTarget {
    pub owner: HWND,
    pub key: String,
    pub anchor: RECT,
    pub dpi: u32,
    pub text: String,
    // 标记该项是否位于原生托盘溢出区，悬停稳定后才按需抓溢出详情
    pub hidden_icon: bool,
}

#[derive(Default)]
pub struct TrayTooltip {
    target: Option<TipTarget>,
    hwnd: HWND,
    visible: bool,
    bitmap: DibTarget,
    scratch: DibTarget,
}

impl TrayTooltip {
    pub fn owner(&self) -> Option<HWND> {
        self.target.as_ref().map(|target| target.owner)
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 返回当前正在跟踪或显示的提示目标 `(宿主窗口, 图标键)`。
    pub fn target_id(&self) -> Option<(HWND, &str)> {
        self.target.as_ref().map(|target| (target.owner, target.key.as_str()))
    }

    pub fn tracks(&self, target: &TipTarget) -> bool {
        self.target
            .as_ref()
            .is_some_and(|current| current.owner == target.owner && current.key == target.key)
    }

    /// 同一图标内移动光标不会重置悬停延迟，后台状态更新可以实时替换内容。
    pub fn track(&mut self, main: HWND, next: Option<TipTarget>, dark: bool) {
        if next == self.target {
            return;
        }
        if next.as_ref().is_some_and(|target| self.tracks(target)) {
            self.target = next;
            if self.visible {
                self.render(dark);
            }
            return;
        }
        self.clear(main);
        self.target = next;
        if self.target.is_some() {
            unsafe {
                let _ = SetTimer(main, TIMER_TRAY_TIP, 100, None);
            }
        }
    }

    pub fn clear(&mut self, main: HWND) {
        self.target = None;
        unsafe {
            let _ = KillTimer(main, TIMER_TRAY_TIP);
            if self.visible {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.visible = false;
    }

    /// 悬停防抖计时器到点后显示提示；若指向溢出区托盘项，返回其所属宿主窗口以便按需刷新详情。
    pub fn on_timer(&mut self, main: HWND, dark: bool) -> Option<HWND> {
        unsafe {
            let _ = KillTimer(main, TIMER_TRAY_TIP);
        }
        let mut cursor = POINT::default();
        if self.target.as_ref().is_some_and(|target| unsafe {
            GetCursorPos(&mut cursor).is_ok()
                && (target.anchor.left..target.anchor.right).contains(&cursor.x)
                && (target.anchor.top..target.anchor.bottom).contains(&cursor.y)
                && WindowFromPoint(cursor) == target.owner
        }) {
            self.render(dark);
            self.target.as_ref().filter(|t| t.hidden_icon).map(|t| t.owner)
        } else {
            self.clear(main);
            None
        }
    }

    fn render(&mut self, dark: bool) {
        let Some(target) = &self.target else { return };
        // 此窗口不接管焦点或鼠标，适用于原生菜单与顶部栏附近。
        if self.hwnd.0.is_null() {
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_NOACTIVATE
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST
                        | WS_EX_TRANSPARENT,
                    w!("Static"),
                    w!("WinDockFinder tray tooltip"),
                    WS_POPUP | WS_DISABLED,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    hinstance(),
                    None,
                )
            };
            let Ok(hwnd) = hwnd else { return };
            self.hwnd = hwnd;
        }
        let Some(monitor) =
            monitor_info(unsafe { MonitorFromRect(&target.anchor, MONITOR_DEFAULTTONEAREST) })
        else {
            return;
        };
        let scale = target.dpi as f32 / 96.0;
        if !self.scratch.ensure_at_least(64, 64) {
            return;
        }
        let Some(dc) = self.scratch.mem_dc else {
            return;
        };
        let font = make_font(dp(12, target.dpi), 400);
        let px = dp(10, target.dpi);
        let py = dp(6, target.dpi);
        let line_h = measure_text(dc, "国A", font).1.max(dp(15, target.dpi));
        let max_text = (monitor.work.right - monitor.work.left - px * 2)
            .min(dp(320, target.dpi))
            .max(1);
        let mut rows = Vec::new();
        for line in target
            .text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(6)
        {
            let mut line: String = line.chars().take(160).collect();
            if measure_text(dc, &line, font).0 > max_text {
                while !line.is_empty() && measure_text(dc, &format!("{line}…"), font).0 > max_text
                {
                    line.pop();
                }
                line.push('…');
            }
            rows.push(line);
        }
        let w = rows
            .iter()
            .map(|line| measure_text(dc, line, font).0)
            .max()
            .unwrap_or(1)
            + px * 2;
        let h = rows.len() as i32 * line_h + py * 2;
        if self.bitmap.ensure(w, h) {
            if let Some(mut surface) = self.bitmap.surface() {
                surface.clear_rect_transparent(w, h);
                let bg = if dark {
                    rgb(0x2C2C2E, 0.99)
                } else {
                    rgb(0xFAFAFC, 0.99)
                };
                let edge = if dark {
                    rgb(0xFFFFFF, 0.18)
                } else {
                    rgb(0x202024, 0.16)
                };
                let fg = if dark {
                    rgb(0xF5F5F7, 1.0)
                } else {
                    rgb(0x1D1D1F, 1.0)
                };
                let bounds = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                surface.fill_rrect(&bounds, 6.0 * scale, bg);
                surface.stroke_rrect(&bounds, 6.0 * scale, 1.0, edge);
                for (index, text) in rows.iter().enumerate() {
                    let top = py + index as i32 * line_h;
                    let line = RECT {
                        left: px,
                        top,
                        right: w - px,
                        bottom: top + line_h,
                    };
                    draw_transparent_text_aligned(
                        &mut surface,
                        &mut self.scratch,
                        text,
                        font,
                        &line,
                        fg,
                        TextAlign::Left,
                        false,
                        scale,
                    );
                }
            }
            let gap = dp(5, target.dpi);
            let cursor_x = (target.anchor.left + target.anchor.right) / 2;
            let work = monitor.work;
            let x = (cursor_x - w / 2).clamp(work.left, (work.right - w).max(work.left));
            let below = target.anchor.bottom + gap;
            let y = if below + h <= work.bottom {
                below
            } else {
                (target.anchor.top - gap - h).max(work.top)
            };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            unsafe {
                if let Some(source) = self.bitmap.mem_dc {
                    if UpdateLayeredWindow(
                        self.hwnd,
                        None,
                        Some(&POINT { x, y }),
                        Some(&SIZE { cx: w, cy: h }),
                        source,
                        Some(&POINT::default()),
                        COLORREF(0),
                        Some(&blend),
                        ULW_ALPHA,
                    )
                    .is_ok()
                    {
                        let _ = SetWindowPos(
                            self.hwnd,
                            HWND_TOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        );
                        self.visible = true;
                    }
                }
            }
        }
        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }
}

impl Drop for TrayTooltip {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
