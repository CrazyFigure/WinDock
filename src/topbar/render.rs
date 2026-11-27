//! 顶部栏渲染（毛玻璃态 / 最大化纯色融合态），GDI 双缓冲到 DIB 后 BitBlt。

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GetDC, ReleaseDC, ValidateRect, HDC, PAINTSTRUCT, SRCCOPY,
};

use crate::config::{Appearance, TopbarCfg};
use crate::topbar::tray_mirror::TrayIconCache;
use crate::topbar::{tray_slots_sorted, ColorMode, Hit, TopBar};
use crate::ui::*;

/// 将顶部栏内容绘制到离线 DIB 缓冲，供 `WM_PAINT` 与拖拽即时直绘共用。
pub(crate) fn render_buffer(bar: &mut TopBar, _cfg: &TopbarCfg, appearance: &Appearance, tray_icons: &TrayIconCache) -> Option<(HDC, i32, i32)> {
    let w = bar.monitor.width();
    let h = bar.height;
    if !bar.target.ensure(w.max(1), h.max(1)) {
        return None;
    }
    let dc = bar.target.mem_dc?;
    let mut sf = bar.target.surface()?;
    // 全透明黑 = 透出 DWM 毛玻璃
    sf.clear_rect_transparent(w, h);
    let s = bar.dpi as f32 / 96.0;
    let text: Rgba = if bar.dark_text { rgb(0x1D1D1F, 1.0) } else { rgb(0xF5F5F7, 1.0) };
    let text_dim: Rgba = if bar.dark_text { rgb(0x3A3A3C, 1.0) } else { rgb(0xD1D1D6, 1.0) };

    let has_strip = bar.desktop_w == w && bar.desktop_h == h && bar.desktop_bgra.len() == (w as usize).saturating_mul(h as usize);
    if bar.mode == ColorMode::Fused {
        if bar.fused_from_desktop && bar.fused_t < 1.0 && has_strip {
            sf.blit_lerped_bgra(w, h, &bar.desktop_bgra, (bar.fused_to.r, bar.fused_to.g, bar.fused_to.b), bar.fused_t);
        } else {
            let bg_rgb = bar.cur_fused;
            sf.fill_rect(
                &RECT { left: 0, top: 0, right: w, bottom: h },
                (bg_rgb.r as f32 / 255.0, bg_rgb.g as f32 / 255.0, bg_rgb.b as f32 / 255.0, 1.0),
            );
        }
    } else if has_strip {
        // 桌面态直接使用高斯模糊 + macOS Vibrancy 调色的本地壁纸顶部条带，自然融于背景
        sf.blit_opaque_bgra(w, h, &bar.desktop_bgra);
    } else {
        let bg_rgb = appearance.topbar_base_rgb();
        sf.fill_rect(
            &RECT { left: 0, top: 0, right: w, bottom: h },
            (bg_rgb.r as f32 / 255.0, bg_rgb.g as f32 / 255.0, bg_rgb.b as f32 / 255.0, 1.0),
        );
    }

    // 激活态高亮（面板/浮层展开时的常驻胶囊底色）
    if let Some(ah) = &bar.active_hit {
        if let Some(r) = bar.rect_of(ah) {
            let hr = RECT {
                left: r.left + (1.0 * s) as i32,
                top: (3.0 * s) as i32,
                right: r.right - (1.0 * s) as i32,
                bottom: h - (3.0 * s) as i32,
            };
            let bg = if bar.dark_text {
                (0.0, 0.0, 0.0, 0.14)
            } else {
                (1.0, 1.0, 1.0, 0.24)
            };
            sf.fill_rrect(&hr, 5.0 * s, bg);
        }
    }

    // 悬停高亮（拖拽期间隐藏原槽位悬停底色，改为目标落位提示）
    if bar.drag.is_none() {
        if let Some(hv) = &bar.hover {
            if bar.active_hit.as_ref() != Some(hv) {
                if let Some(r) = bar.rect_of(hv) {
                    let hr = RECT {
                        left: r.left + (1.0 * s) as i32,
                        top: (3.0 * s) as i32,
                        right: r.right - (1.0 * s) as i32,
                        bottom: h - (3.0 * s) as i32,
                    };
                    let hl = if bar.dark_text {
                        (0.0, 0.0, 0.0, 0.10)
                    } else {
                        (1.0, 1.0, 1.0, 0.18)
                    };
                    sf.fill_rrect(&hr, 5.0 * s, hl);
                }
            }
        }
    }

    // 拖拽时的目标落位胶囊指示（紧贴空出槽位，与释放实际落位 1:1 对齐）
    let accent = accent_from_config(appearance);
    if let Some(drag) = &bar.drag {
        let tr = RECT {
            left: drag.target_rect.left + (1.0 * s) as i32,
            top: (3.0 * s) as i32,
            right: drag.target_rect.right - (1.0 * s) as i32,
            bottom: h - (3.0 * s) as i32,
        };
        let fill_a = if bar.dark_text { 0.16 } else { 0.24 };
        sf.fill_rrect(&tr, 5.0 * s, (accent.0, accent.1, accent.2, fill_a));
        sf.stroke_rrect(&tr, 5.0 * s, (1.1 * s).max(1.0), (accent.0, accent.1, accent.2, 0.78));
    }

    // 左侧：单色徽标 + 前台应用名 + 文件/窗口/前往 菜单；右侧：托盘收纳/状态图标/时钟
    let text_shadow = false;
    let popover_open = bar.popover.as_ref().map(|p| p.visible).unwrap_or(false);
    let app_icons = crate::app::app_icon_cache();
    for (r, hit) in bar.hits.iter() {
        match hit {
            Hit::ProductMenu => {
                let cx = (r.left + r.right) as f32 * 0.5;
                draw_topbar_logo(&mut sf, cx, h as f32 * 0.5, 16.0 * s, text);
            }
            Hit::AppMenu => {
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, &bar.active_app, bar.font_bold, r, text, TextAlign::Center, text_shadow, s);
            }
            Hit::MenuFile => {
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, "文件", bar.font, r, text, TextAlign::Center, text_shadow, s);
            }
            Hit::MenuWindow => {
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, "窗口", bar.font, r, text, TextAlign::Center, text_shadow, s);
            }
            Hit::MenuGo => {
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, "前往", bar.font, r, text, TextAlign::Center, text_shadow, s);
            }
            Hit::TrayToggle => {
                let cx = (r.left + r.right) as f32 * 0.5;
                let glyph = if popover_open { Glyph::ChevronDown } else { Glyph::ChevronLeft };
                draw_glyph(&mut sf, glyph, cx, h as f32 * 0.5, 11.0 * s, text_dim);
            }
            Hit::TrayIcon(key) => {
                if bar.drag.as_ref().is_some_and(|d| &d.key == key) {
                    continue;
                }
                let cx = bar.tray_draw_center(key).unwrap_or((r.left + r.right) as f32 * 0.5);
                let cy = h as f32 * 0.5;
                if let Some(item) = bar.tray_items.iter().find(|t| &t.key == key) {
                    crate::topbar::tray_popover::draw_shelf_item_icon(&mut sf, &mut bar.scratch, bar.font, item, &app_icons, tray_icons, cx, cy, 16.0 * s, !bar.dark_text, s);
                } else if let Some(p) = tray_icons.get(key) {
                    sf.draw_icon_adaptive(&p, cx, cy, 16.0 * s, !bar.dark_text, false, 0.0);
                }
            }
            Hit::Ime => {
                // macOS 紧凑输入法微边框胶囊（居中贴合文字宽度）
                let cx = (r.left + r.right) / 2;
                let half_w = (12.0 * s).round() as i32;
                let badge = RECT {
                    left: (cx - half_w).max(r.left + 2),
                    top: (5.5 * s).round() as i32,
                    right: (cx + half_w).min(r.right - 2),
                    bottom: h - (5.5 * s).round() as i32,
                };
                sf.stroke_rrect(&badge, 4.0 * s, 1.0, (text.0, text.1, text.2, 0.42));
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, &bar.status.ime, bar.font, r, text, TextAlign::Center, text_shadow, s);
            }
            Hit::Network => {
                draw_glyph(&mut sf, if bar.status.network { Glyph::Wifi } else { Glyph::WifiOff }, (r.left + r.right) as f32 * 0.5, h as f32 * 0.5, 15.0 * s, text);
            }
            Hit::Brightness => {
                draw_glyph(&mut sf, Glyph::Sun, (r.left + r.right) as f32 * 0.5, h as f32 * 0.5, 15.0 * s, text);
            }
            Hit::Volume => {
                let (vol, mute) = bar.status.volume.unwrap_or((0, false));
                let cx = (r.left + r.right) as f32 * 0.5;
                draw_glyph(&mut sf, if mute || vol == 0 { Glyph::SpeakerMute } else { Glyph::Speaker }, cx, h as f32 * 0.5, 15.0 * s, text);
            }
            Hit::Battery => {
                if let Some((pct, charging)) = bar.status.battery {
                    let cx = r.left as f32 + 16.0 * s;
                    draw_battery_filled(&mut sf, cx, h as f32 * 0.5, 15.0 * s, pct, charging, text);
                    let rc = RECT {
                        left: r.left + (28.0 * s) as i32,
                        top: 0,
                        right: r.right,
                        bottom: h,
                    };
                    draw_transparent_text_aligned(&mut sf, &mut bar.scratch, &format!("{pct}%"), bar.font, &rc, text, TextAlign::Left, text_shadow, s);
                }
            }
            Hit::Control => {
                draw_glyph(&mut sf, Glyph::ControlCenter, (r.left + r.right) as f32 * 0.5, h as f32 * 0.5, 14.0 * s, text);
            }
            Hit::Clock => {
                draw_transparent_text_aligned(&mut sf, &mut bar.scratch, &bar.status.clock, bar.font, r, text, TextAlign::Center, text_shadow, s);
            }
        }
    }

    // 顶部栏拖拽重排时的跟随浮起图标（限定在托盘图标区横向区间内，紧贴抓手偏移跟随鼠标）
    if let Some(drag) = &bar.drag {
        let slots = tray_slots_sorted(&bar.hits);
        let draw_cx = if let (Some((_, first)), Some((_, last))) = (slots.first(), slots.last()) {
            let min_cx = (first.left + first.right) as f32 * 0.5 - 4.0 * s;
            let max_cx = (last.left + last.right) as f32 * 0.5 + 4.0 * s;
            ((drag.x + drag.grab_dx) as f32).clamp(min_cx, max_cx)
        } else {
            (drag.x + drag.grab_dx) as f32
        };
        let half_pill = (12.0 * s).round() as i32;
        let pill = RECT {
            left: (draw_cx.round() as i32) - half_pill,
            top: (3.0 * s) as i32,
            right: (draw_cx.round() as i32) + half_pill,
            bottom: h - (3.0 * s) as i32,
        };
        let pill_bg = if bar.dark_text {
            (1.0, 1.0, 1.0, 0.72)
        } else {
            (0.16, 0.17, 0.20, 0.72)
        };
        sf.fill_rrect(&pill, 6.0 * s, pill_bg);
        sf.stroke_rrect(&pill, 6.0 * s, (1.0 * s).max(1.0), (accent.0, accent.1, accent.2, 0.75));
        if let Some(item) = bar.tray_items.iter().find(|t| t.key == drag.key) {
            crate::topbar::tray_popover::draw_shelf_item_icon(&mut sf, &mut bar.scratch, bar.font, item, &app_icons, tray_icons, draw_cx, h as f32 * 0.5, 17.5 * s, !bar.dark_text, s);
        } else if let Some(p) = tray_icons.get(&drag.key) {
            sf.draw_icon_adaptive(&p, draw_cx, h as f32 * 0.5, 17.5 * s, !bar.dark_text, false, 0.0);
        }
    }

    // 桌面壁纸融合态底边仅保留极微弱层次线，避免硬边割裂壁纸连续感
    if bar.mode == ColorMode::Glass {
        let line_col = if bar.dark_text { (0.0, 0.0, 0.0, 0.06) } else { (1.0, 1.0, 1.0, 0.04) };
        sf.fill_rect(&RECT { left: 0, top: h - 1, right: w, bottom: h }, line_col);
    }
    drop(sf);
    Some((dc, w, h))
}

pub fn paint(hwnd: HWND, bar: &mut TopBar, cfg: &TopbarCfg, appearance: &Appearance, tray_icons: &TrayIconCache) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() {
        return;
    }
    if let Some((dc, w, h)) = render_buffer(bar, cfg, appearance, tray_icons) {
        unsafe {
            let _ = BitBlt(hdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
        }
    }
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 在鼠标拖拽回调内直接同步刷屏，规避 `handler_busy()` 与 `WM_PAINT` 合并导致的拖影滞后。
pub fn paint_immediate(hwnd: HWND, bar: &mut TopBar, cfg: &TopbarCfg, appearance: &Appearance, tray_icons: &TrayIconCache) {
    if let Some((dc, w, h)) = render_buffer(bar, cfg, appearance, tray_icons) {
        unsafe {
            let wdc = GetDC(hwnd);
            if !wdc.is_invalid() {
                let _ = BitBlt(wdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
                let _ = ReleaseDC(hwnd, wdc);
                let _ = ValidateRect(hwnd, None);
            }
        }
    }
}
