//! Dock 渲染：图标绘制、阴影、运行点、分隔线、回收站、100ms Tooltip、拖拽幽灵。
//!
//! 所有内容绘制到 32bpp 预乘 Alpha DIB，再 `UpdateLayeredWindow` 到纯净 macOS 分层交互窗。

use std::sync::Arc;

use windows::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HDC, HFONT, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
use windows::Win32::UI::Shell::{SHSTOCKICONID, SIID_RECYCLER, SIID_RECYCLERFULL};

use crate::config::{Appearance, DockCfg};
use crate::dock::icons::{extract_stock_icon_hd, IconCache, IconWorker, DOCK_ICON_SRC_SIZE};
use crate::dock::items::FuncKind;
use crate::dock::Dock;
use crate::ui::*;

pub struct RenderCtx<'a> {
    pub cfg: &'a DockCfg,
    pub appearance: &'a Appearance,
    pub icons: &'a IconCache,
    pub worker: &'a Arc<IconWorker>,
    pub text_scratch: &'a mut DibTarget,
    pub tooltip_font: HFONT,
    pub trash_full: bool,
    pub suppress_tooltip: bool,
}

/// 回收站高清系统图标（256px JUMBO 归一化到 128px 并缓存）。
fn trash_pixels(icons: &IconCache, full: bool) -> Option<IconPixels> {
    let key = if full { "special:trash:full" } else { "special:trash:empty" };
    if let Some(p) = icons.pixels(key, DOCK_ICON_SRC_SIZE) {
        return Some(p);
    }
    let id = if full { SIID_RECYCLERFULL } else { SIID_RECYCLER };
    if let Some(p) = extract_stock_icon_hd(SHSTOCKICONID(id.0), DOCK_ICON_SRC_SIZE) {
        icons.insert(key, DOCK_ICON_SRC_SIZE, p.px.clone(), p.w, p.h, crate::dock::icons::current_generation());
        return Some(p);
    }
    None
}

/// 请求/获取统一 128px 高清图标像素；未命中返回 None 并触发后台提取。
fn icon_pixels(key: &str, ctx: &RenderCtx) -> Option<IconPixels> {
    if key.starts_with("special:settings") || key.starts_with("special:launchpad") {
        return None;
    }
    if key == "special:trash" {
        return trash_pixels(ctx.icons, ctx.trash_full);
    }
    if let Some(p) = ctx.icons.pixels(key, DOCK_ICON_SRC_SIZE) {
        return Some(p);
    }
    ctx.worker.request(key, DOCK_ICON_SRC_SIZE);
    None
}

fn letter_badge(
    surface: &mut Surface,
    scratch: &mut DibTarget,
    font: HFONT,
    cx: f32,
    cy: f32,
    size: f32,
    label: &str,
    dark: bool,
    scale: f32,
) {
    let ch: String = label.chars().next().unwrap_or('?').to_uppercase().collect();
    let r = RECT {
        left: (cx - size * 0.45) as i32,
        top: (cy - size * 0.45) as i32,
        right: (cx + size * 0.45) as i32,
        bottom: (cy + size * 0.45) as i32,
    };
    let (top, bot) = if dark { (rgb(0x4A4A4F, 1.0), rgb(0x2E2E32, 1.0)) } else { (rgb(0xF7F8FA, 1.0), rgb(0xDFE2E8, 1.0)) };
    surface.fill_rrect_vgrad(&r, size * 0.21, top, bot);
    surface.stroke_rrect(&r, size * 0.21, 1.0, (1.0, 1.0, 1.0, if dark { 0.18 } else { 0.55 }));
    let trc = RECT {
        left: r.left,
        top: (cy - size * 0.36) as i32,
        right: r.right,
        bottom: (cy + size * 0.36) as i32,
    };
    draw_transparent_text_aligned(
        surface,
        scratch,
        &ch,
        font,
        &trc,
        if dark { rgb(0xF5F5F7, 1.0) } else { rgb(0x1D1D1F, 1.0) },
        TextAlign::Center,
        false,
        scale,
    );
}

/// 绘制单个 Dock 图标（高清源图双线性缩放 + 柔和阴影 + 精美 macOS 内置图标）。
fn draw_entry_icon(
    surface: &mut Surface,
    ctx: &mut RenderCtx,
    key: &str,
    label: &str,
    cx: f32,
    cy: f32,
    size: f32,
    dark: bool,
) {
    if key == "special:settings" {
        draw_settings_icon(surface, cx, cy, size);
        return;
    }
    if key == "special:launchpad" {
        draw_launchpad_icon(surface, cx, cy, size);
        return;
    }
    if let Some(p) = icon_pixels(key, ctx) {
        let s = size.max(8.0);
        // 浅色模式保留轻柔微投影，深色模式关闭黑影并启用暗色透明图标自适应可见性保护
        surface.draw_icon_adaptive(&p, cx, cy, s, dark, !dark, (s * 0.045).max(1.2));
        return;
    }
    let font = ctx.tooltip_font;
    let scale = size / 48.0;
    let scratch = &mut *ctx.text_scratch;
    letter_badge(surface, scratch, font, cx, cy, size, label, dark, scale.max(0.8));
}

/// 绘制完整 Dock 帧到 DIB 并提交到分层窗口。
pub fn render(dock: &mut Dock, ctx: &mut RenderCtx) {
    let (w, h) = dock.win_size;
    if w <= 0 || h <= 0 {
        return;
    }
    let Some(dc) = dock.target.mem_dc else { return };
    // Dock 明暗极性与胶囊色调：优先跟随背后窗口色，其次 Windows 系统主题，最后降级壁纸
    let (dark, top_rgb, bot_rgb) = dock.capsule_colors();
    let fade = dock.show_t;
    if fade <= 0.001 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(dock.hwnd_overlay, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
        }
        dock.window_visible = false;
        return;
    }
    // 显隐滑动：show_t 0→1 自下而上浮出
    let slide = (1.0 - fade) * (dock.layout.capsule_h + dock.monitor_offset_px() as f32 + 10.0);
    let alpha = fade.min(1.0);

    let Some(mut sf) = dock.target.surface() else { return };
    sf.clear_rect_transparent(w, h);
    let cap = dock.capsule_rect();
    let cap_f = RECT {
        left: cap.left,
        top: (cap.top as f32 + slide).round() as i32,
        right: cap.right,
        bottom: (cap.bottom as f32 + slide).round() as i32,
    };

    // macOS 原生质感圆角托盘：纯净浅乳白/深炭灰柔和半透明层 + 细腻玻璃内光边 + 轻柔双层环境底阴影
    let rad = dock.corner_radius();
    let s_dpi = dock.dpi_scale();
    let is_liquid_glass = ctx.appearance.dock_material == "liquid_glass";
    // 液态玻璃模式由 SourceConstantAlpha 统一淡入淡出；经典模式沿用像素 alpha 混合
    let fg_alpha = if is_liquid_glass { 1.0 } else { alpha };

    if is_liquid_glass {
        let top_col = (top_rgb.r as f32 / 255.0, top_rgb.g as f32 / 255.0, top_rgb.b as f32 / 255.0, 1.0);
        let bot_col = (bot_rgb.r as f32 / 255.0, bot_rgb.g as f32 / 255.0, bot_rgb.b as f32 / 255.0, 1.0);
        sf.fill_liquid_glass_capsule(
            &cap_f,
            rad,
            s_dpi,
            dark,
            ctx.appearance.opacity,
            (dock.win_rect.left, dock.win_rect.top),
            dock.glass_strip.as_ref(),
            top_col,
            bot_col,
        );
    } else {
        // 底部柔和环境阴影（克制不发黑）
        for (expand_dp, dy_dp, sa_base) in [(4.0f32, 4.5f32, 0.08f32), (1.5f32, 2.0f32, 0.12f32)] {
            let ex = (expand_dp * s_dpi).round() as i32;
            let dy = (dy_dp * s_dpi).round() as i32;
            let sh = RECT {
                left: cap_f.left - ex,
                top: cap_f.top + dy,
                right: cap_f.right + ex,
                bottom: cap_f.bottom + dy + ex / 2,
            };
            sf.fill_rrect(&sh, rad + ex as f32 * 0.4, (0.0, 0.0, 0.0, sa_base * alpha));
        }
        // 纯净 macOS 托盘不透明度与背景协调色（轻透不发泥）
        let op = ctx.appearance.opacity.clamp(0.35, 0.95) * alpha;
        let (top_a, bot_a) = if dark {
            ((op * 0.94).min(0.88), (op * 1.00).min(0.92))
        } else {
            ((op * 0.92).min(0.84), (op * 0.88).min(0.80))
        };
        let top_tint = (top_rgb.r as f32 / 255.0, top_rgb.g as f32 / 255.0, top_rgb.b as f32 / 255.0, top_a);
        let bot_tint = (bot_rgb.r as f32 / 255.0, bot_rgb.g as f32 / 255.0, bot_rgb.b as f32 / 255.0, bot_a);
        sf.fill_rrect_vgrad(&cap_f, rad, top_tint, bot_tint);
        // 外极细柔和界线 + 内 1px 细腻高光边（复刻 macOS Dock 立体托盘边缘）
        sf.stroke_rrect(&cap_f, rad, 1.0, (0.0, 0.0, 0.0, (if dark { 0.42 } else { 0.14 }) * alpha));
        let inner = RECT {
            left: cap_f.left + 1,
            top: cap_f.top + 1,
            right: cap_f.right - 1,
            bottom: cap_f.bottom - 1,
        };
        sf.stroke_rrect(&inner, (rad - 1.0).max(2.0), 1.0, (1.0, 1.0, 1.0, (if dark { 0.16 } else { 0.46 }) * alpha));
    }

    // 分隔线（纤细居中，cap_f 已包含 slide 偏移）
    for sx in &dock.layout.separators {
        let x = cap.left as f32 + sx + 0.5;
        let y0 = cap_f.top as f32 + dock.capsule_h() * 0.20;
        let y1 = cap_f.bottom as f32 - dock.capsule_h() * 0.20;
        let sep_col = if dark { (1.0, 1.0, 1.0, 0.24 * fg_alpha) } else { (0.0, 0.0, 0.0, 0.20 * fg_alpha) };
        sf.thick_line(x, y0, x, y1, (1.0 * s_dpi).max(1.0), sep_col);
    }

    // 拖拽目标让位空槽提示（柔和半透明槽位指示，与释放后落位完全对齐）
    let n = dock.entry_count();
    if let Some(d) = &dock.drag {
        if let Some(t) = d.target.filter(|&t| t < dock.layout.slots.len()) {
            let slot = dock.layout.slots[t];
            let tcx = cap.left as f32 + slot.center_x;
            let tcy = cap.top as f32 + slot.center_y + slide;
            let half = dock.icon_px() as f32 * 0.46;
            let accent = accent_from_config(ctx.appearance);
            let slot_rc = RECT {
                left: (tcx - half).round() as i32,
                top: (tcy - half).round() as i32,
                right: (tcx + half).round() as i32,
                bottom: (tcy + half).round() as i32,
            };
            sf.fill_rrect(&slot_rc, half * 0.44, (accent.0, accent.1, accent.2, 0.14 * fg_alpha));
            sf.stroke_rrect_dashed(
                &slot_rc,
                half * 0.44,
                (1.2 * s_dpi).max(1.0),
                (4.0 * s_dpi).round().max(3.0) as i32,
                (accent.0, accent.1, accent.2, 0.62 * fg_alpha),
            );
        }
    }

    // 图标
    let bounce_state = dock.bounce;
    let pressed_idx = dock.press.map(|(idx, _, _)| idx);
    for i in 0..n {
        if i >= dock.layout.slots.len() {
            break;
        }
        if let Some(d) = &dock.drag {
            if d.from == i {
                continue;
            }
        }
        let vis = dock.visual_slot(i);
        let slot = dock.layout.slots[vis.min(dock.layout.slots.len() - 1)];
        let cx = dock.slot_x(i);
        let mut cy = cap.top as f32 + slot.center_y + slide;
        // 启动弹跳（平滑抛物线起跳）
        if let Some((bi, prog)) = bounce_state {
            if bi == i {
                let jump = (prog * std::f32::consts::PI).sin().max(0.0);
                cy -= jump * 14.0 * s_dpi;
            }
        }
        // 按下微下沉反馈
        let mut draw_size = slot.size;
        if pressed_idx == Some(i) && dock.drag.is_none() {
            cy += 2.0 * s_dpi;
            draw_size *= 0.94;
        }
        let (key, label) = dock.entry_key_label(i);
        let key = key.to_string();
        let label = label.to_string();
        draw_entry_icon(&mut sf, ctx, &key, &label, cx, cy, draw_size, dark);
        // 精致 macOS 运行指示点（浅色底深灰点，深色底纯白点）
        if ctx.cfg.show_running_dot && dock.entry_is_running(i) {
            let dy = cap_f.bottom as f32 - (4.5 * s_dpi);
            let dot_col = if dark { (1.0, 1.0, 1.0, 0.94 * fg_alpha) } else { (0.14, 0.14, 0.16, 0.88 * fg_alpha) };
            sf.fill_circle(cx, dy, (2.1 * s_dpi).max(1.8), dot_col);
        }
        // 拖拽悬停在回收站上的柔和红环反馈
        if let Some(d) = &dock.drag {
            if d.over_trash && matches!(dock.func_kind(i), Some(FuncKind::Trash)) {
                let hl = RECT {
                    left: (cx - slot.size * 0.56) as i32,
                    top: (cy - slot.size * 0.56) as i32,
                    right: (cx + slot.size * 0.56) as i32,
                    bottom: (cy + slot.size * 0.56) as i32,
                };
                sf.stroke_rrect(&hl, slot.size * 0.28, (2.0 * s_dpi).max(1.5), (1.0, 0.27, 0.23, 0.92 * fg_alpha));
            }
        }
    }

    // 拖拽幽灵（按抓手偏移紧贴鼠标绘制，处于移除区时展示红色移除环提示）
    if let Some(d) = dock.drag.clone() {
        let (key, label) = dock.entry_key_label(d.from);
        let key = key.to_string();
        let label = label.to_string();
        let ghost_size = dock.icon_px() as f32 * 1.14;
        let half = ghost_size * 0.52;
        let gx = (d.x + d.grab_dx).clamp(half + 2.0, (w as f32 - half - 2.0).max(half + 2.0));
        let gy = (d.y + d.grab_dy).clamp(half + 2.0, (h as f32 - half - 2.0).max(half + 2.0));
        if d.remove {
            let rm_rc = RECT {
                left: (gx - half).round() as i32,
                top: (gy - half).round() as i32,
                right: (gx + half).round() as i32,
                bottom: (gy + half).round() as i32,
            };
            sf.fill_rrect(&rm_rc, ghost_size * 0.24, (1.0, 0.23, 0.19, 0.20 * fg_alpha));
            sf.stroke_rrect(&rm_rc, ghost_size * 0.24, (1.8 * s_dpi).max(1.4), (1.0, 0.27, 0.23, 0.92 * fg_alpha));
        }
        draw_entry_icon(&mut sf, ctx, &key, &label, gx, gy, ghost_size, dark);
    }

    // macOS 风格 Tooltip 气泡（窗口预览开启时由预览浮窗替代，避免视觉重叠）
    let tooltip_font = ctx.tooltip_font;
    let scratch = &mut *ctx.text_scratch;
    if let (Some(i), true) = (dock.hover, dock.tooltip_visible && !ctx.suppress_tooltip) {
        if dock.drag.is_none() && i < dock.layout.slots.len() {
            let slot = dock.layout.slots[i];
            let cx = cap.left as f32 + slot.center_x;
            let icon_top = cap.top as f32 + slot.center_y - slot.size * 0.5 + slide;
            let (_, label) = dock.entry_key_label(i);
            let label = label.to_string();
            let (tw, th) = measure_text(dc, &label, tooltip_font);
            let pad_x = (11.0 * s_dpi).round() as i32;
            let pad_y = (5.0 * s_dpi).round() as i32;
            let bw = tw + pad_x * 2;
            let bh = th + pad_y * 2;
            let arrow_h = (5.0 * s_dpi).round() as i32;
            let mut left = (cx - bw as f32 * 0.5).round() as i32;
            left = left.clamp(4, (w - bw - 4).max(4));
            let top = (icon_top - bh as f32 - arrow_h as f32 - 6.0 * s_dpi).round() as i32;
            let top = top.max(2);
            let box_r = RECT {
                left,
                top,
                right: left + bw,
                bottom: top + bh,
            };
            let tip_rad = 7.0 * s_dpi;
            let shadow = RECT {
                left: box_r.left,
                top: box_r.top + 2,
                right: box_r.right,
                bottom: box_r.bottom + 4,
            };
            let bg = if dark {
                rgb(0x23252A, (if is_liquid_glass { 0.84 } else { 0.92 }) * fg_alpha)
            } else {
                rgb(0xF6F7FA, (if is_liquid_glass { 0.88 } else { 0.94 }) * fg_alpha)
            };
            let fg = if dark { rgb(0xF5F5F7, 0.98 * fg_alpha) } else { rgb(0x1D1D1F, 0.96 * fg_alpha) };
            let bd = if dark { (1.0, 1.0, 1.0, (if is_liquid_glass { 0.28 } else { 0.16 }) * fg_alpha) } else { (0.0, 0.0, 0.0, 0.12 * fg_alpha) };
            sf.fill_rrect(&shadow, tip_rad, (0.0, 0.0, 0.0, 0.22 * fg_alpha));
            sf.fill_rrect(&box_r, tip_rad, bg);
            sf.stroke_rrect(&box_r, tip_rad, 1.0, bd);
            if is_liquid_glass && !dark {
                let inner_tip = RECT {
                    left: box_r.left + 1,
                    top: box_r.top + 1,
                    right: box_r.right - 1,
                    bottom: box_r.bottom - 1,
                };
                sf.stroke_rrect(&inner_tip, (tip_rad - 1.0).max(2.0), 1.0, (1.0, 1.0, 1.0, 0.55 * fg_alpha));
            }
            // 底部居中指向小三角
            let ax = cx.clamp(box_r.left as f32 + tip_rad, box_r.right as f32 - tip_rad);
            let ay = box_r.bottom as f32 - 1.0;
            sf.fill_triangle_aa(ax - 5.5 * s_dpi, ay, ax + 5.5 * s_dpi, ay, ax, ay + arrow_h as f32, bg);
            let trc = RECT {
                left: box_r.left + pad_x,
                top: box_r.top,
                right: box_r.right - pad_x,
                bottom: box_r.bottom,
            };
            draw_transparent_text_aligned(&mut sf, scratch, &label, tooltip_font, &trc, fg, TextAlign::Center, false, s_dpi);
        }
    }

    // 提交分层窗口
    let dst = POINT {
        x: dock.win_rect.left,
        y: dock.win_rect.top,
    };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: (alpha * 255.0).round() as u8,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        let _ = UpdateLayeredWindow(dock.hwnd_overlay, HDC::default(), Some(&dst), Some(&size), dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
    }
    if !dock.window_visible {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                dock.hwnd_overlay,
                windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST,
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(dock.hwnd_overlay, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNA);
        }
        dock.window_visible = true;
    }
    dock.dirty = false;
}

/// 将 DIB 内容 Blit 到普通窗口 DC（设置页/浮层等非分层窗口使用）。
pub fn blit_to_window(hdc: HDC, target: &DibTarget, w: i32, h: i32) {
    let Some(src) = target.mem_dc else { return };
    unsafe {
        let _ = BitBlt(hdc, 0, 0, w, h, src, 0, 0, SRCCOPY);
    }
}
