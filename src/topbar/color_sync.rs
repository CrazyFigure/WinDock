//! 最大化窗口标题栏微条带采样、色彩平滑插值与对比度文字适配。
//!
//! 事件驱动：仅在显示器前台窗口进入最大化态时触发一次轻量取色（50ms 防抖），
//! 绝不每帧截屏。采样区域避开顶部栏下沿 2..8px、右侧标题栏按钮区与左侧菜单图标区。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, GENERIC_READ, HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BitBlt, GdiFlush, GetDC, GetSysColor, ReleaseDC,
    SelectObject, SRCCOPY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLOR_BACKGROUND, DIB_RGB_COLORS, HDC, HBITMAP, HGDIOBJ,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    DesktopWallpaper, IDesktopWallpaper, DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL,
    DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH, DWPOS_TILE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetWindowRect, IsChild, SystemParametersInfoW, WindowFromPoint, GA_ROOT,
    PW_RENDERFULLCONTENT, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// WCAG 相对亮度。
    pub fn luminance(&self) -> f32 {
        let lin = |c: u8| {
            let v = c as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// 背景亮度 > 0.45 用深色字 #1D1D1F，否则浅色字 #F5F5F7。
    pub fn use_dark_text(&self) -> bool {
        self.luminance() > 0.45
    }

    pub fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        Rgb {
            r: (a.r as f32 + (b.r as f32 - a.r as f32) * t).round() as u8,
            g: (a.g as f32 + (b.g as f32 - a.g as f32) * t).round() as u8,
            b: (a.b as f32 + (b.b as f32 - a.b as f32) * t).round() as u8,
        }
    }
}

fn dib_section(w: i32, h: i32) -> Option<(HDC, HBITMAP, HGDIOBJ, *mut std::ffi::c_void)> {
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut ptr, None, 0) else {
            let _ = DeleteDC(hdc);
            return None;
        };
        if ptr.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc);
            return None;
        }
        let old = SelectObject(hdc, bmp);
        Some((hdc, bmp, old, ptr))
    }
}

/// 判断屏幕坐标点是否直接落在目标最大化窗口及其子控件上，自动剔除菜单/浮窗/气泡等顶层遮挡。
unsafe fn point_belongs_to_window(hwnd: HWND, screen_x: i32, screen_y: i32) -> bool {
    if hwnd.0.is_null() {
        return true;
    }
    let pt_hwnd = WindowFromPoint(POINT {
        x: screen_x,
        y: screen_y,
    });
    if pt_hwnd.0.is_null() {
        return false;
    }
    pt_hwnd == hwnd || GetAncestor(pt_hwnd, GA_ROOT) == hwnd || IsChild(hwnd, pt_hwnd).as_bool()
}

/// 从抓取到的标题栏像素条带中按右/左分区归桶提取主色，并跳过被顶层浮窗遮挡的采样列（纯函数，便于单测）。
fn extract_dominant_strip_color(slice: &[u32], w: i32, h: i32, col_visible: &[bool]) -> Option<Rgb> {
    if w <= 0 || h <= 0 || slice.len() < (w * h) as usize || col_visible.len() < w as usize {
        return None;
    }
    // 跳过最左侧菜单与应用名；优先在右侧空白标题栏区域（60%..100%）查找，避免左侧标签栏干扰
    let mid_split = (w as f32 * 0.60) as i32;
    let ranges = [(mid_split, w), (0, mid_split)];
    for (x0, x1) in ranges {
        let mut bins: HashMap<(u8, u8, u8), (u32, u64, u64, u64)> = HashMap::new();
        let step = ((x1 - x0) / 80).max(1);
        for y in 0..h {
            let mut x = x0;
            while x < x1 {
                if col_visible[x as usize] {
                    let p = slice[(y * w + x) as usize];
                    let b = (p & 0xFF) as u8;
                    let g = ((p >> 8) & 0xFF) as u8;
                    let r = ((p >> 16) & 0xFF) as u8;
                    // 按 8 级量化归桶，精准抓出占大面积的纯色标题栏背景，自动忽略细字与图标噪点
                    let key = (r >> 3, g >> 3, b >> 3);
                    let e = bins.entry(key).or_insert((0, 0, 0, 0));
                    e.0 += 1;
                    e.1 += r as u64;
                    e.2 += g as u64;
                    e.3 += b as u64;
                }
                x += step;
            }
        }
        if let Some((_, &(cnt, sr, sg, sb))) = bins.iter().max_by_key(|(_, v)| v.0) {
            if cnt >= 24 {
                return Some(Rgb::new(
                    (sr / cnt as u64) as u8,
                    (sg / cnt as u64) as u8,
                    (sb / cnt as u64) as u8,
                ));
            }
        }
    }
    None
}

/// 主路径：仅截取目标最大化窗口顶部栏紧下方未受遮挡的标题栏条带（兼容 GPU 加速软件并避免采到浮窗黑底）。
pub fn sample_below_topbar_for_window(hwnd: HWND, mon: RECT, topbar_h: i32) -> Option<Rgb> {
    let mon_w = (mon.right - mon.left).clamp(128, 8192);
    let x_start = (mon_w as f32 * 0.30) as i32;
    let x_end = (mon_w as f32 * 0.84) as i32;
    let w = (x_end - x_start).max(64);
    // 跳过最顶部 3px 窗口细边，采样 y = topbar_h + 4 .. topbar_h + 14 的 10px 条带
    let y_start = mon.top + topbar_h.max(1) + 4;
    let h = 10;
    unsafe {
        // 先创建离线 DIB，避免 GetDC 后提前返回造成桌面 DC 泄漏
        let (hdc, bmp, old, ptr) = dib_section(w, h)?;
        let screen = GetDC(None);
        if screen.is_invalid() {
            let _ = SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc);
            return None;
        }
        let ok = BitBlt(hdc, 0, 0, w, h, screen, mon.left + x_start, y_start, SRCCOPY).is_ok();
        ReleaseDC(None, screen);
        let _ = GdiFlush();
        let result = if ok {
            let mut col_visible = vec![false; w as usize];
            let mid_split = (w as f32 * 0.60) as i32;
            for (x0, x1) in [(mid_split, w), (0, mid_split)] {
                let step = ((x1 - x0) / 80).max(1);
                let mut x = x0;
                while x < x1 {
                    let sx = mon.left + x_start + x;
                    col_visible[x as usize] =
                        point_belongs_to_window(hwnd, sx, y_start)
                            && point_belongs_to_window(hwnd, sx, y_start + h - 1);
                    x += step;
                }
            }
            let slice = std::slice::from_raw_parts(ptr as *const u32, (w * h) as usize);
            extract_dominant_strip_color(slice, w, h, &col_visible)
        } else {
            None
        };
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        result
    }
}

/// 辅助路径：PrintWindow(PW_RENDERFULLCONTENT) 渲染窗口顶部条带并取中位色。
pub fn sample_caption(hwnd: HWND) -> Option<Rgb> {
    unsafe {
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let w = (r.right - r.left).clamp(64, 4096);
        let h = 20;
        let (hdc, bmp, old, ptr) = dib_section(w, h)?;
        // 先填洋红作为“未渲染”哨兵
        let slice = std::slice::from_raw_parts_mut(ptr as *mut u32, (w * h) as usize);
        slice.fill(0xFFFF00FF);
        let ok = PrintWindow(hwnd, hdc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
        // 必须先刷新 GDI 并读取完像素，再释放 DIB 位图内存，防止访问已释放指针触发 0xC0000005
        let _ = GdiFlush();
        let result = if ok {
            let x0 = (w as f32 * 0.2) as i32;
            let x1 = (w as f32 * 0.8) as i32;
            let mut rs = Vec::new();
            let mut gs = Vec::new();
            let mut bs = Vec::new();
            for y in 4..14 {
                for x in x0..x1 {
                    let p = slice[(y * w + x) as usize];
                    if (p & 0x00FF00FF) == 0x00FF00FF && (p >> 16) & 0xFF == 0xFF {
                        continue; // 哨兵：未渲染
                    }
                    let b = (p & 0xFF) as u8;
                    let g = ((p >> 8) & 0xFF) as u8;
                    let rr = ((p >> 16) & 0xFF) as u8;
                    rs.push(rr);
                    gs.push(g);
                    bs.push(b);
                }
            }
            if rs.len() < 32 {
                None
            } else {
                rs.sort_unstable();
                gs.sort_unstable();
                bs.sort_unstable();
                let m = rs.len() / 2;
                Some(Rgb::new(rs[m], gs[m], bs[m]))
            }
        } else {
            None
        };
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        result
    }
}

/// 读取 DWM 窗口自身声明的标题栏色或深浅模式属性（不含系统全局降级，便于区分单窗提示与 OS 主题）。
fn dwm_window_color_hint(hwnd: HWND) -> Option<Rgb> {
    if hwnd.0.is_null() {
        return None;
    }
    unsafe {
        let mut color = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &mut color as *mut u32 as *mut _, 4).is_ok() {
            if color != 0xFFFF_FFFF {
                let c = COLORREF(color);
                return Some(Rgb::new((c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8));
            }
        }
        let mut dark = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &mut dark as *mut u32 as *mut _, 4).is_ok() {
            return Some(if dark != 0 { Rgb::new(0x20, 0x20, 0x20) } else { Rgb::new(0xF3, 0xF3, 0xF3) });
        }
    }
    None
}

/// 兜底路径：读取窗口 DWMWA_CAPTION_COLOR / 深色模式属性。
pub fn caption_color_fallback(hwnd: HWND) -> Option<Rgb> {
    if let Some(c) = dwm_window_color_hint(hwnd) {
        return Some(c);
    }
    if crate::win32::system_uses_dark_theme() {
        Some(Rgb::new(0x20, 0x20, 0x20))
    } else {
        Some(Rgb::new(0xF3, 0xF3, 0xF3))
    }
}

/// 综合取色：优先采顶部栏紧下方未受遮挡的真实标题栏条带，失败再回退 PrintWindow 与 DWM 属性。
pub fn caption_color(hwnd: HWND, mon: Option<RECT>, topbar_h: i32) -> Rgb {
    mon.and_then(|r| sample_below_topbar_for_window(hwnd, r, topbar_h))
        .or_else(|| sample_caption(hwnd))
        .or_else(|| caption_color_fallback(hwnd))
        .unwrap_or(Rgb::new(0xF3, 0xF3, 0xF3))
}

/// 判断屏幕坐标是否归属于目标背后应用窗口集合之一。
unsafe fn point_belongs_to_any(roots: &[HWND], screen_x: i32, screen_y: i32) -> bool {
    let pt_hwnd = WindowFromPoint(POINT {
        x: screen_x,
        y: screen_y,
    });
    if pt_hwnd.0.is_null() {
        return false;
    }
    let root = GetAncestor(pt_hwnd, GA_ROOT);
    roots.iter().any(|&hwnd| {
        !hwnd.0.is_null() && (pt_hwnd == hwnd || root == hwnd || IsChild(hwnd, pt_hwnd).as_bool())
    })
}

/// 从已过滤后的 BGRA 像素样本中提取主色：先按 8 级量化取占比最高纯色背景，若纹理复杂则取居中 60% 均值。
fn dominant_bgra_color(pixels: &[u32], min_count: usize) -> Option<Rgb> {
    if pixels.len() < min_count {
        return None;
    }
    let mut bins: HashMap<(u8, u8, u8), (u32, u64, u64, u64)> = HashMap::new();
    for &p in pixels {
        let b = (p & 0xFF) as u8;
        let g = ((p >> 8) & 0xFF) as u8;
        let r = ((p >> 16) & 0xFF) as u8;
        let e = bins.entry((r >> 3, g >> 3, b >> 3)).or_insert((0, 0, 0, 0));
        e.0 += 1;
        e.1 += r as u64;
        e.2 += g as u64;
        e.3 += b as u64;
    }
    if let Some((_, &(cnt, sr, sg, sb))) = bins.iter().max_by_key(|(_, v)| v.0) {
        if cnt as usize >= (min_count / 2).max(1) && (cnt as usize * 3 >= pixels.len() || cnt >= 12) {
            return Some(Rgb::new(
                (sr / cnt as u64) as u8,
                (sg / cnt as u64) as u8,
                (sb / cnt as u64) as u8,
            ));
        }
    }
    // 对半透明磨砂/壁纸照片背景：按亮度排序后裁去极端文字/按钮高光，保留中段 60% 求平均
    let mut sorted: Vec<(u16, u8, u8, u8)> = pixels
        .iter()
        .map(|&p| {
            let b = (p & 0xFF) as u8;
            let g = ((p >> 8) & 0xFF) as u8;
            let r = ((p >> 16) & 0xFF) as u8;
            let luma_fast = (r as u16 * 77 + g as u16 * 150 + b as u16 * 29) >> 8;
            (luma_fast, r, g, b)
        })
        .collect();
    sorted.sort_unstable_by_key(|v| v.0);
    let trim = sorted.len() / 5;
    let mid = &sorted[trim..sorted.len().saturating_sub(trim).max(trim + 1)];
    let (mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64);
    for &(_, r, g, b) in mid {
        sr += r as u64;
        sg += g as u64;
        sb += b as u64;
    }
    let cnt = mid.len() as u64;
    Some(Rgb::new((sr / cnt) as u8, (sg / cnt) as u8, (sb / cnt) as u8))
}

/// 从屏幕区域按网格采集仅属于 `roots` 应用窗口的可见像素，彻底屏蔽桌面与 Dock 自身叠影。
fn collect_strip_pixels(roots: &[HWND], rect: RECT, max_cols: i32, max_rows: i32, out: &mut Vec<u32>) {
    let w = (rect.right - rect.left).min(4096);
    let h = (rect.bottom - rect.top).min(512);
    if w < 4 || h < 2 || roots.is_empty() {
        return;
    }
    unsafe {
        let Some((hdc, bmp, old, ptr)) = dib_section(w, h) else { return };
        let screen = GetDC(None);
        if screen.is_invalid() {
            let _ = SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc);
            return;
        }
        let ok = BitBlt(hdc, 0, 0, w, h, screen, rect.left, rect.top, SRCCOPY).is_ok();
        ReleaseDC(None, screen);
        let _ = GdiFlush();
        if ok {
            let slice = std::slice::from_raw_parts(ptr as *const u32, (w * h) as usize);
            let step_x = (w / max_cols.max(1)).max(1);
            let step_y = (h / max_rows.max(1)).max(1);
            let mut y = step_y / 2;
            while y < h {
                let mut x = step_x / 2;
                while x < w {
                    if point_belongs_to_any(roots, rect.left + x, rect.top + y) {
                        out.push(slice[(y * w + x) as usize]);
                    }
                    x += step_x;
                }
                y += step_y;
            }
        }
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
    }
}

/// 采集 Dock 背后应用窗口的实时主色（无背后外部应用窗口时返回 None，以便降级系统主题）。
///
/// - Dock 隐藏期间直接采样正后方胶囊区域（零误差）；
/// - Dock 可见期间优先采样窗口左右紧邻侧翼及紧上方窄条带，彻底避免自采样黑化正反馈环路。
pub fn sample_dock_backdrop(cap_screen: RECT, dock_win_rect: RECT, mon_rect: RECT, dock_visible: bool) -> Option<Rgb> {
    let roots = crate::monitor::dock_backdrop_windows(cap_screen);
    if roots.is_empty() {
        return None;
    }
    let mut pixels = Vec::with_capacity(256);
    if !dock_visible {
        collect_strip_pixels(&roots, cap_screen, 28, 5, &mut pixels);
    } else {
        let left_flank = RECT {
            left: (dock_win_rect.left - 96).max(mon_rect.left),
            top: cap_screen.top,
            right: (dock_win_rect.left - 4).max(mon_rect.left),
            bottom: cap_screen.bottom,
        };
        let right_flank = RECT {
            left: (dock_win_rect.right + 4).min(mon_rect.right),
            top: cap_screen.top,
            right: (dock_win_rect.right + 96).min(mon_rect.right),
            bottom: cap_screen.bottom,
        };
        collect_strip_pixels(&roots, left_flank, 14, 4, &mut pixels);
        collect_strip_pixels(&roots, right_flank, 14, 4, &mut pixels);
        if pixels.len() < 12 {
            let above_strip = RECT {
                left: cap_screen.left.max(mon_rect.left),
                top: (dock_win_rect.top - 14).max(mon_rect.top),
                right: cap_screen.right.min(mon_rect.right),
                bottom: (dock_win_rect.top - 2).max(mon_rect.top),
            };
            collect_strip_pixels(&roots, above_strip, 24, 3, &mut pixels);
        }
    }
    dominant_bgra_color(&pixels, 12).or_else(|| dwm_window_color_hint(roots[0]))
}

/// 根据背后色彩或回退策略生成和谐的 Dock 胶囊明暗极性与上下渐变 RGB（含防抖滞回区间）。
pub fn harmonize_dock_backdrop(bg: Option<Rgb>, prev_dark: bool, fallback_dark: bool) -> (bool, Rgb, Rgb) {
    let canonical_light = (Rgb::new(227, 231, 237), Rgb::new(209, 214, 224));
    let canonical_dark = (Rgb::new(36, 38, 46), Rgb::new(23, 25, 33));
    let Some(raw) = bg else {
        let (t, b) = if fallback_dark { canonical_dark } else { canonical_light };
        return (fallback_dark, t, b);
    };

    let luma = raw.luminance();
    // 滞回区间 0.24..0.33（约 sRGB 52%..61% 灰）：避免中灰画面滚过 Dock 边缘时明暗频繁跳变
    let dark = if luma < 0.24 {
        true
    } else if luma > 0.33 {
        false
    } else {
        prev_dark
    };

    let gray = 0.299 * raw.r as f32 + 0.587 * raw.g as f32 + 0.114 * raw.b as f32;
    let desat = |c: u8| -> f32 { gray + (c as f32 - gray) * 0.42 };
    let (dr, dg, db) = (desat(raw.r), desat(raw.g), desat(raw.b));

    if dark {
        let max_c = dr.max(dg).max(db);
        let scale = if max_c > 76.0 { 76.0 / max_c } else { 1.0 };
        let muted = Rgb::new(
            (dr * scale).clamp(14.0, 76.0).round() as u8,
            (dg * scale).clamp(14.0, 76.0).round() as u8,
            (db * scale).clamp(14.0, 76.0).round() as u8,
        );
        (
            true,
            Rgb::lerp(canonical_dark.0, muted, 0.34),
            Rgb::lerp(canonical_dark.1, muted, 0.34),
        )
    } else {
        let muted = Rgb::new(
            dr.clamp(192.0, 248.0).round() as u8,
            dg.clamp(192.0, 248.0).round() as u8,
            db.clamp(192.0, 248.0).round() as u8,
        );
        (
            false,
            Rgb::lerp(canonical_light.0, muted, 0.36),
            Rgb::lerp(canonical_light.1, muted, 0.36),
        )
    }
}

/// 判断应用是否位于最大化自动取色排除名单中。
pub fn is_excluded(exe: &str, exclude: &[String]) -> bool {
    let lower = exe.to_lowercase();
    exclude.iter().any(|e| {
        let e = e.trim().to_lowercase();
        !e.is_empty() && (lower.contains(&e) || exe.to_lowercase().contains(&e))
    })
}

/// 桌面壁纸采样调色板：用于生成桌面态统一不刺眼的顶部栏颜色及 Dock 明暗基调。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopPalette {
    pub topbar_rgb: Rgb,
    pub overall_luma: f32,
}

impl DesktopPalette {
    /// 壁纸整体偏暗时返回 true（深色桌面应配沉稳深色顶栏与深色 Dock，避免刺眼纯白）。
    pub fn is_dark(&self) -> bool {
        self.overall_luma < 0.40
    }
}

/// 顶部栏桌面态背景条带（经多显示器壁纸几何映射、线性空间高斯模糊与 macOS Vibrancy 调色生成）。
#[derive(Clone, Debug)]
pub struct DesktopTopbarBackdrop {
    pub base_rgb: Rgb,
    pub dark_text: bool,
    pub strip_w: i32,
    pub strip_h: i32,
    pub strip_bgra: Vec<u32>,
}

impl DesktopTopbarBackdrop {
    /// 双线性平滑放大为顶部栏实际像素尺寸，供 `WM_PAINT` 直接高速拷贝。
    pub fn render_to_size(&self, w: i32, h: i32) -> Vec<u32> {
        let w = w.max(1);
        let h = h.max(1);
        let total = (w as usize).saturating_mul(h as usize);
        if self.strip_w <= 1 && self.strip_h <= 1 {
            let px = self.strip_bgra.first().copied().unwrap_or_else(|| pack_opaque_rgb(self.base_rgb));
            return vec![px; total];
        }
        let sw = self.strip_w.max(1);
        let sh = self.strip_h.max(1);
        if self.strip_bgra.len() < (sw as usize) * (sh as usize) {
            return vec![pack_opaque_rgb(self.base_rgb); total];
        }
        let mut x_lut = Vec::with_capacity(w as usize);
        for x in 0..w {
            let fx = (((x as f32 + 0.5) * sw as f32 / w as f32) - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min((sw - 1) as usize);
            let wx1 = ((fx - x0 as f32) * 256.0).round() as u32;
            x_lut.push((x0, x1, 256 - wx1, wx1));
        }
        let mut out = vec![0u32; total];
        for y in 0..h {
            let fy = (((y as f32 + 0.5) * sh as f32 / h as f32) - 0.5).clamp(0.0, (sh - 1) as f32);
            let y0 = fy.floor() as usize;
            let y1 = (y0 + 1).min((sh - 1) as usize);
            let wy1 = ((fy - y0 as f32) * 256.0).round() as u32;
            let wy0 = 256 - wy1;
            let row0 = y0 * sw as usize;
            let row1 = y1 * sw as usize;
            let dst_row = y as usize * w as usize;
            for (x, &(x0, x1, wx0, wx1)) in x_lut.iter().enumerate() {
                let p00 = self.strip_bgra[row0 + x0];
                let p10 = self.strip_bgra[row0 + x1];
                let p01 = self.strip_bgra[row1 + x0];
                let p11 = self.strip_bgra[row1 + x1];
                let w00 = (wx0 * wy0) >> 8;
                let w10 = (wx1 * wy0) >> 8;
                let w01 = (wx0 * wy1) >> 8;
                let w11 = (wx1 * wy1) >> 8;
                let b = (((p00 & 0xFF) * w00 + (p10 & 0xFF) * w10 + (p01 & 0xFF) * w01 + (p11 & 0xFF) * w11 + 128) >> 8).min(255);
                let g = ((((p00 >> 8) & 0xFF) * w00 + ((p10 >> 8) & 0xFF) * w10 + ((p01 >> 8) & 0xFF) * w01 + ((p11 >> 8) & 0xFF) * w11 + 128) >> 8).min(255);
                let r = ((((p00 >> 16) & 0xFF) * w00 + ((p10 >> 16) & 0xFF) * w10 + ((p01 >> 16) & 0xFF) * w01 + ((p11 >> 16) & 0xFF) * w11 + 128) >> 8).min(255);
                out[dst_row + x] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WallpaperPosition {
    Center,
    Tile,
    Stretch,
    Fit,
    Fill,
    Span,
}

impl WallpaperPosition {
    fn from_win32(pos: DESKTOP_WALLPAPER_POSITION) -> Self {
        match pos {
            DWPOS_CENTER => Self::Center,
            DWPOS_TILE => Self::Tile,
            DWPOS_STRETCH => Self::Stretch,
            DWPOS_FIT => Self::Fit,
            DWPOS_SPAN => Self::Span,
            DWPOS_FILL => Self::Fill,
            _ => Self::Fill,
        }
    }
}

#[inline]
fn pack_opaque_rgb(c: Rgb) -> u32 {
    0xFF00_0000 | ((c.r as u32) << 16) | ((c.g as u32) << 8) | (c.b as u32)
}

#[inline]
fn srgb_to_linear(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

#[inline]
fn rgb_to_linear(c: [f32; 3]) -> [f32; 3] {
    [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2])]
}

#[inline]
fn linear_luminance(lin: [f32; 3]) -> f32 {
    0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]
}

/// 色域保护饱和度增强：补偿高斯模糊带来的灰暗衰减，同时避免通道溢出偏色。
fn boost_vibrancy_srgb(srgb: [f32; 3], sat_boost: f32) -> [f32; 3] {
    let luma = 0.299 * srgb[0] + 0.587 * srgb[1] + 0.114 * srgb[2];
    let deltas = [srgb[0] - luma, srgb[1] - luma, srgb[2] - luma];
    let mut eff = sat_boost.max(1.0);
    for (&c, &d) in srgb.iter().zip(deltas.iter()) {
        if d > 1e-5 {
            eff = eff.min((1.0 - c) / d);
        } else if d < -1e-5 {
            eff = eff.min(c / (-d));
        }
    }
    eff = eff.max(1.0);
    [
        (luma + deltas[0] * eff).clamp(0.0, 1.0),
        (luma + deltas[1] * eff).clamp(0.0, 1.0),
        (luma + deltas[2] * eff).clamp(0.0, 1.0),
    ]
}

/// 将单调亮度压缩后的线性 RGB 安全拉回 [0, 1] 色域，同时保持目标相对亮度 `y_out` 严格不变。
fn fit_linear_gamut_preserving_luma(mut lin: [f32; 3], y_out: f32) -> [f32; 3] {
    let max_c = lin[0].max(lin[1]).max(lin[2]);
    if max_c > 1.0 && max_c > y_out + 1e-5 {
        let k = ((1.0 - y_out) / (max_c - y_out)).clamp(0.0, 1.0);
        for c in &mut lin {
            *c = y_out + (*c - y_out) * k;
        }
    }
    let min_c = lin[0].min(lin[1]).min(lin[2]);
    if min_c < 0.0 && y_out > min_c + 1e-5 {
        let k = (y_out / (y_out - min_c)).clamp(0.0, 1.0);
        for c in &mut lin {
            *c = y_out + (*c - y_out) * k;
        }
    }
    [lin[0].clamp(0.0, 1.0), lin[1].clamp(0.0, 1.0), lin[2].clamp(0.0, 1.0)]
}

/// macOS Vibrancy 像素着色：
/// 保留壁纸真实色相与饱和度，深色背景适度提亮告别死黑，同时用平滑膝点曲线约束亮度以确保文字满足 WCAG 对比度。
pub fn vibrant_topbar_pixel(srgb: [f32; 3], avg_srgb: [f32; 3], dark_mode: bool) -> Rgb {
    let sat_boost = if dark_mode { 1.38 } else { 1.25 };
    let vib = boost_vibrancy_srgb(srgb, sat_boost);
    let lin = rgb_to_linear(vib);
    let y = linear_luminance(lin);

    // 构造带壁纸环境色调的磨砂基底，防止极暗像素放大色噪或出现纯黑空洞
    let avg_vib = boost_vibrancy_srgb(avg_srgb, 1.18);
    let avg_lin = rgb_to_linear(avg_vib);
    let avg_y = linear_luminance(avg_lin);
    let base_slate = if dark_mode {
        [0.92f32, 0.96, 1.10]
    } else {
        [0.97f32, 0.985, 1.02]
    };
    let ambient_unit = if avg_y > 0.002 {
        let inv = 1.0 / avg_y;
        [
            base_slate[0] * 0.55 + (avg_lin[0] * inv) * 0.45,
            base_slate[1] * 0.55 + (avg_lin[1] * inv) * 0.45,
            base_slate[2] * 0.55 + (avg_lin[2] * inv) * 0.45,
        ]
    } else {
        base_slate
    };

    let (y_out, chroma_t) = if dark_mode {
        // 深色玻璃：亮度严格限定在 [0.026, 0.105]，既告别 #191B22 死黑，又保证 #F5F5F7 文字 >= 6.4:1 对比度
        const DARK_FLOOR: f32 = 0.026;
        const DARK_CEIL: f32 = 0.105;
        let yo = DARK_FLOOR + (DARK_CEIL - DARK_FLOOR) * (y / (y + 0.16));
        (yo, (y / 0.015).clamp(0.0, 1.0))
    } else {
        // 浅色玻璃：亮度限定在 [0.47, 0.85]，清透保留天空/柔彩质感且保证 #1D1D1F 文字 >= 7.9:1 对比度（始终 > 0.45 阈值）
        const LIGHT_FLOOR: f32 = 0.47;
        const LIGHT_CEIL: f32 = 0.85;
        let yo = LIGHT_FLOOR + (LIGHT_CEIL - LIGHT_FLOOR) * (y / (y + 0.25));
        (yo, (y / 0.020).clamp(0.0, 1.0))
    };

    let unit = if y > 1e-4 {
        let inv = 1.0 / y;
        [
            ambient_unit[0] * (1.0 - chroma_t) + (lin[0] * inv) * chroma_t,
            ambient_unit[1] * (1.0 - chroma_t) + (lin[1] * inv) * chroma_t,
            ambient_unit[2] * (1.0 - chroma_t) + (lin[2] * inv) * chroma_t,
        ]
    } else {
        ambient_unit
    };
    let unit_y = linear_luminance(unit).max(1e-5);
    let scaled = [
        unit[0] * (y_out / unit_y),
        unit[1] * (y_out / unit_y),
        unit[2] * (y_out / unit_y),
    ];
    let fitted = fit_linear_gamut_preserving_luma(scaled, y_out);
    Rgb::new(
        (linear_to_srgb(fitted[0]) * 255.0).round().clamp(0.0, 255.0) as u8,
        (linear_to_srgb(fitted[1]) * 255.0).round().clamp(0.0, 255.0) as u8,
        (linear_to_srgb(fitted[2]) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// 在线形 RGB 缓冲区上执行三遍可分离盒式模糊（等效高斯模糊，边缘夹紧防止黑边）。
fn separable_box_blur_3x(buf: &mut [[f32; 3]], w: usize, h: usize, rx: usize, ry: usize) {
    if w == 0 || h == 0 || buf.len() < w * h {
        return;
    }
    let mut tmp = vec![[0.0f32; 3]; w * h];
    let norm_x = 1.0 / (2 * rx + 1) as f32;
    let norm_y = 1.0 / (2 * ry + 1) as f32;
    for _ in 0..3 {
        // 水平模糊：buf -> tmp
        if rx > 0 {
            for y in 0..h {
                let row = y * w;
                let mut acc = [0.0f32; 3];
                for dx in -(rx as isize)..=(rx as isize) {
                    let sx = dx.clamp(0, w as isize - 1) as usize;
                    let p = buf[row + sx];
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                }
                tmp[row] = [acc[0] * norm_x, acc[1] * norm_x, acc[2] * norm_x];
                for x in 1..w {
                    let add_x = (x + rx).min(w - 1);
                    let sub_x = x.saturating_sub(rx + 1);
                    let pa = buf[row + add_x];
                    let ps = buf[row + sub_x];
                    acc[0] += pa[0] - ps[0];
                    acc[1] += pa[1] - ps[1];
                    acc[2] += pa[2] - ps[2];
                    tmp[row + x] = [acc[0] * norm_x, acc[1] * norm_x, acc[2] * norm_x];
                }
            }
        } else {
            tmp.copy_from_slice(&buf[..w * h]);
        }

        // 垂直模糊：tmp -> buf
        if ry > 0 {
            for x in 0..w {
                let mut acc = [0.0f32; 3];
                for dy in -(ry as isize)..=(ry as isize) {
                    let sy = dy.clamp(0, h as isize - 1) as usize;
                    let p = tmp[sy * w + x];
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                }
                buf[x] = [acc[0] * norm_y, acc[1] * norm_y, acc[2] * norm_y];
                for y in 1..h {
                    let add_y = (y + ry).min(h - 1);
                    let sub_y = y.saturating_sub(ry + 1);
                    let pa = tmp[add_y * w + x];
                    let ps = tmp[sub_y * w + x];
                    acc[0] += pa[0] - ps[0];
                    acc[1] += pa[1] - ps[1];
                    acc[2] += pa[2] - ps[2];
                    buf[y * w + x] = [acc[0] * norm_y, acc[1] * norm_y, acc[2] * norm_y];
                }
            }
        } else {
            buf[..w * h].copy_from_slice(&tmp);
        }
    }
}

#[derive(Clone, Copy)]
struct WallpaperLayout {
    mon_w: f32,
    mon_h: f32,
    virt_ox: f32,
    virt_oy: f32,
    virt_w: f32,
    virt_h: f32,
    orig_w: f32,
    orig_h: f32,
    pos: WallpaperPosition,
}

/// 将显示器内相对坐标 `(sx_rel, sy_rel)` 按 Windows 桌面壁纸铺设方式映射到归一化图像坐标 `(u, v)`。
fn map_monitor_strip_uv(sx_rel: f32, sy_rel: f32, l: &WallpaperLayout) -> Option<(f32, f32)> {
    if l.mon_w <= 0.0 || l.mon_h <= 0.0 || l.orig_w <= 0.0 || l.orig_h <= 0.0 {
        return None;
    }
    match l.pos {
        WallpaperPosition::Stretch => Some(((sx_rel / l.mon_w).clamp(0.0, 1.0), (sy_rel / l.mon_h).clamp(0.0, 1.0))),
        WallpaperPosition::Fill => {
            let scale = (l.mon_w / l.orig_w).max(l.mon_h / l.orig_h);
            let sw = l.orig_w * scale;
            let sh = l.orig_h * scale;
            let ox = (l.mon_w - sw) * 0.5;
            let oy = (l.mon_h - sh) * 0.5;
            Some((((sx_rel - ox) / sw).clamp(0.0, 1.0), ((sy_rel - oy) / sh).clamp(0.0, 1.0)))
        }
        WallpaperPosition::Fit => {
            let scale = (l.mon_w / l.orig_w).min(l.mon_h / l.orig_h);
            let sw = l.orig_w * scale;
            let sh = l.orig_h * scale;
            let ox = (l.mon_w - sw) * 0.5;
            let oy = (l.mon_h - sh) * 0.5;
            let u = (sx_rel - ox) / sw;
            let v = (sy_rel - oy) / sh;
            ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then_some((u, v))
        }
        WallpaperPosition::Center => {
            let ox = (l.mon_w - l.orig_w) * 0.5;
            let oy = (l.mon_h - l.orig_h) * 0.5;
            let u = (sx_rel - ox) / l.orig_w;
            let v = (sy_rel - oy) / l.orig_h;
            ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then_some((u, v))
        }
        WallpaperPosition::Tile => {
            let u = (sx_rel / l.orig_w).rem_euclid(1.0);
            let v = (sy_rel / l.orig_h).rem_euclid(1.0);
            Some((u, v))
        }
        WallpaperPosition::Span => {
            let vw = l.virt_w.max(l.mon_w);
            let vh = l.virt_h.max(l.mon_h);
            let scale = (vw / l.orig_w).max(vh / l.orig_h);
            let sw = l.orig_w * scale;
            let sh = l.orig_h * scale;
            let ox = (vw - sw) * 0.5;
            let oy = (vh - sh) * 0.5;
            let vx = l.virt_ox + sx_rel;
            let vy = l.virt_oy + sy_rel;
            Some((((vx - ox) / sw).clamp(0.0, 1.0), ((vy - oy) / sh).clamp(0.0, 1.0)))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorStripKey {
    mon_rect: (i32, i32, i32, i32),
    virt_rect: (i32, i32, i32, i32),
    bar_h: i32,
    path: String,
    mtime: Option<SystemTime>,
    file_len: u64,
    pos: WallpaperPosition,
    bg_rgb: Rgb,
}

#[derive(Clone, Debug)]
struct CachedLinearStrip {
    key: MonitorStripKey,
    strip_w: i32,
    strip_h: i32,
    blurred_srgb: Vec<[f32; 3]>,
    avg_srgb: [f32; 3],
    strip_luma: f32,
}

struct CachedWallpaper {
    path: String,
    mtime: Option<SystemTime>,
    file_len: u64,
    palette: DesktopPalette,
}

static WALLPAPER_CACHE: OnceLock<Mutex<Option<CachedWallpaper>>> = OnceLock::new();
static STRIP_CACHE: OnceLock<Mutex<Vec<CachedLinearStrip>>> = OnceLock::new();
static LAST_WALLPAPER_SIG: OnceLock<Mutex<u64>> = OnceLock::new();

/// 清空桌面壁纸采样与顶栏条带缓存（系统广播壁纸变更时调用）。
pub fn invalidate_wallpaper_cache() {
    if let Some(lock) = WALLPAPER_CACHE.get() {
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    if let Some(lock) = STRIP_CACHE.get() {
        lock.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// 根据壁纸顶部条带平均色与整体亮度生成和谐且不再死黑的桌面顶部栏配色。
pub fn harmonize_desktop_palette(top_avg: Rgb, overall_luma: f32) -> DesktopPalette {
    let luma = overall_luma.clamp(0.0, 1.0);
    let dark_mode = luma < 0.40;
    let avg_f = [
        top_avg.r as f32 / 255.0,
        top_avg.g as f32 / 255.0,
        top_avg.b as f32 / 255.0,
    ];
    let topbar_rgb = vibrant_topbar_pixel(avg_f, avg_f, dark_mode);
    DesktopPalette {
        topbar_rgb,
        overall_luma: luma,
    }
}

/// 读取当前 Windows 桌面壁纸路径；纯色桌面时返回空字符串。
fn current_wallpaper_path() -> String {
    let mut buf = [0u16; 520];
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
    };
    if !ok {
        return String::new();
    }
    crate::win32::from_wide(&buf)
}

fn file_meta(path: &str) -> (Option<SystemTime>, u64) {
    if path.is_empty() {
        return (None, 0);
    }
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    }
}

fn sys_background_rgb() -> Rgb {
    let c = unsafe { GetSysColor(COLOR_BACKGROUND) };
    Rgb::new((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
}

/// 查询目标显示器当前生效的壁纸路径、铺设方式与系统桌面背景底色。
fn query_monitor_wallpaper(mon_rect: RECT) -> (String, WallpaperPosition, Rgb) {
    let fallback_bg = sys_background_rgb();
    let fallback_path = current_wallpaper_path();
    unsafe {
        let com_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        struct ComUninit(bool);
        impl Drop for ComUninit {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let _guard = ComUninit(com_hr.is_ok());

        let Ok(dw): windows::core::Result<IDesktopWallpaper> =
            CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)
        else {
            return (fallback_path, WallpaperPosition::Fill, fallback_bg);
        };
        let pos = dw
            .GetPosition()
            .map(WallpaperPosition::from_win32)
            .unwrap_or(WallpaperPosition::Fill);
        let bg_rgb = dw
            .GetBackgroundColor()
            .map(|c| Rgb::new((c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8))
            .unwrap_or(fallback_bg);
        let count = dw.GetMonitorDevicePathCount().unwrap_or(0);
        for idx in 0..count {
            let Ok(id_pwstr) = dw.GetMonitorDevicePathAt(idx) else { continue };
            let rc = dw.GetMonitorRECT(PCWSTR(id_pwstr.0));
            let matched = rc.is_ok_and(|r| {
                r.left == mon_rect.left
                    && r.top == mon_rect.top
                    && r.right == mon_rect.right
                    && r.bottom == mon_rect.bottom
            });
            if matched {
                let wp = dw
                    .GetWallpaper(PCWSTR(id_pwstr.0))
                    .map(crate::win32::take_pwstr)
                    .unwrap_or_default();
                let _ = crate::win32::take_pwstr(id_pwstr);
                let final_path = if wp.is_empty() { fallback_path } else { wp };
                return (final_path, pos, bg_rgb);
            }
            let _ = crate::win32::take_pwstr(id_pwstr);
        }
        let global_wp = dw
            .GetWallpaper(PCWSTR::null())
            .map(crate::win32::take_pwstr)
            .unwrap_or_default();
        let final_path = if global_wp.is_empty() { fallback_path } else { global_wp };
        (final_path, pos, bg_rgb)
    }
}

fn virtual_desktop_rect(fallback: RECT) -> RECT {
    let mons = crate::win32::all_monitors();
    if mons.is_empty() {
        return fallback;
    }
    let mut r = mons[0].rect;
    for m in &mons[1..] {
        r.left = r.left.min(m.rect.left);
        r.top = r.top.min(m.rect.top);
        r.right = r.right.max(m.rect.right);
        r.bottom = r.bottom.max(m.rect.bottom);
    }
    r
}

/// 轻量检测当前壁纸文件签名是否发生变化（供 5s 定时校对幻灯片/Windows 聚焦自动换壁纸，无需解码图像）。
pub fn check_wallpaper_changed() -> bool {
    let path = current_wallpaper_path();
    let (mtime, len) = file_meta(&path);
    let bg = sys_background_rgb();
    let mut data = Vec::with_capacity(path.len() + 24);
    data.extend_from_slice(path.as_bytes());
    let secs = mtime
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    data.extend_from_slice(&secs.to_le_bytes());
    data.extend_from_slice(&len.to_le_bytes());
    data.extend_from_slice(&[bg.r, bg.g, bg.b]);
    let sig = crate::win32::fnv1a64(&data);
    let lock = LAST_WALLPAPER_SIG.get_or_init(|| Mutex::new(sig));
    let mut prev = lock.lock().unwrap_or_else(|e| e.into_inner());
    if *prev != sig {
        *prev = sig;
        invalidate_wallpaper_cache();
        return true;
    }
    false
}

/// 通过 WIC 将壁纸缩放到紧凑尺寸，输出 (缩略宽, 缩略高, 原图宽, 原图高, BGRA 像素)。
fn decode_scaled_wallpaper(path: &str, max_dim: u32) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
    if path.is_empty() || !std::path::Path::new(path).is_file() {
        return None;
    }
    let wpath = crate::win32::wide(path);
    unsafe {
        // 兼容未初始化 COM 的测试/工作线程；若当前线程已以不同并发模型初始化则沿用现有套间
        let com_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        struct ComUninit(bool);
        impl Drop for ComUninit {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let _guard = ComUninit(com_hr.is_ok());

        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wpath.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let mut orig_w = 0u32;
        let mut orig_h = 0u32;
        frame.GetSize(&mut orig_w, &mut orig_h).ok()?;
        if orig_w == 0 || orig_h == 0 {
            return None;
        }
        let long = orig_w.max(orig_h);
        let (sw, sh) = if long <= max_dim {
            (orig_w, orig_h)
        } else {
            let scale = max_dim as f32 / long as f32;
            (
                ((orig_w as f32 * scale).round() as u32).max(16),
                ((orig_h as f32 * scale).round() as u32).max(16),
            )
        };
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&frame, sw, sh, WICBitmapInterpolationModeFant)
            .ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        let mut px = vec![0u8; (sw * sh * 4) as usize];
        converter.CopyPixels(std::ptr::null(), sw * 4, &mut px).ok()?;
        Some((sw, sh, orig_w, orig_h, px))
    }
}

/// 从已解码壁纸中按屏幕几何映射截取顶部条带，并在线性 RGB 空间执行宽半径高斯模糊。
fn build_blurred_strip(
    img: Option<(u32, u32, u32, u32, Vec<u8>)>,
    mon_rect: RECT,
    virt_rect: RECT,
    bar_h: i32,
    pos: WallpaperPosition,
    bg_rgb: Rgb,
    key: MonitorStripKey,
) -> CachedLinearStrip {
    const STRIP_W: usize = 160;
    const STRIP_H: usize = 12;
    const BLUR_H: usize = 18;
    let bg_srgb = [bg_rgb.r as f32 / 255.0, bg_rgb.g as f32 / 255.0, bg_rgb.b as f32 / 255.0];
    let bg_lin = rgb_to_linear(bg_srgb);

    let Some((sw, sh, orig_w, orig_h, px)) = img else {
        let luma = linear_luminance(bg_lin);
        return CachedLinearStrip {
            key,
            strip_w: STRIP_W as i32,
            strip_h: STRIP_H as i32,
            blurred_srgb: vec![bg_srgb; STRIP_W * STRIP_H],
            avg_srgb: bg_srgb,
            strip_luma: luma,
        };
    };

    let mon_w = (mon_rect.right - mon_rect.left).max(1) as f32;
    let mon_h = (mon_rect.bottom - mon_rect.top).max(1) as f32;
    let layout = WallpaperLayout {
        mon_w,
        mon_h,
        virt_ox: (mon_rect.left - virt_rect.left) as f32,
        virt_oy: (mon_rect.top - virt_rect.top) as f32,
        virt_w: (virt_rect.right - virt_rect.left).max(1) as f32,
        virt_h: (virt_rect.bottom - virt_rect.top).max(1) as f32,
        orig_w: orig_w as f32,
        orig_h: orig_h as f32,
        pos,
    };
    let bar_hf = bar_h.max(12) as f32;

    let sample_bilinear_lin = |u: f32, v: f32| -> [f32; 3] {
        let fx = (u * (sw - 1) as f32).clamp(0.0, (sw - 1) as f32);
        let fy = (v * (sh - 1) as f32).clamp(0.0, (sh - 1) as f32);
        let x0 = fx.floor() as usize;
        let y0 = fy.floor() as usize;
        let x1 = (x0 + 1).min(sw as usize - 1);
        let y1 = (y0 + 1).min(sh as usize - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;
        let get = |x: usize, y: usize| -> [f32; 3] {
            let off = (y * sw as usize + x) * 4;
            rgb_to_linear([
                px[off + 2] as f32 / 255.0,
                px[off + 1] as f32 / 255.0,
                px[off] as f32 / 255.0,
            ])
        };
        let p00 = get(x0, y0);
        let p10 = get(x1, y0);
        let p01 = get(x0, y1);
        let p11 = get(x1, y1);
        let w00 = (1.0 - tx) * (1.0 - ty);
        let w10 = tx * (1.0 - ty);
        let w01 = (1.0 - tx) * ty;
        let w11 = tx * ty;
        [
            p00[0] * w00 + p10[0] * w10 + p01[0] * w01 + p11[0] * w11,
            p00[1] * w00 + p10[1] * w10 + p01[1] * w01 + p11[1] * w11,
            p00[2] * w00 + p10[2] * w10 + p01[2] * w01 + p11[2] * w11,
        ]
    };

    // 多采样 y = 0..1.5*bar_h，让底部边界在垂直模糊时自然衔接到下方壁纸
    let mut lin_grid = vec![[0.0f32; 3]; STRIP_W * BLUR_H];
    for row in 0..BLUR_H {
        let sy_rel = ((row as f32 + 0.5) / STRIP_H as f32 * bar_hf).min(mon_h - 0.5);
        for col in 0..STRIP_W {
            let sx_rel = (col as f32 + 0.5) / STRIP_W as f32 * mon_w;
            let c = match map_monitor_strip_uv(sx_rel, sy_rel, &layout) {
                Some((u, v)) => sample_bilinear_lin(u, v),
                None => bg_lin,
            };
            lin_grid[row * STRIP_W + col] = c;
        }
    }

    // 宽半径三遍可分离盒式模糊：水平平滑区域色带过渡，垂直平滑顶栏内纵向细节
    separable_box_blur_3x(&mut lin_grid, STRIP_W, BLUR_H, 10, 3);

    let crop_len = STRIP_W * STRIP_H;
    let mut blurred_srgb = Vec::with_capacity(crop_len);
    let mut lumas = Vec::with_capacity(crop_len);
    let mut sum_lin = [0.0f64; 3];
    for &lin in &lin_grid[..crop_len] {
        sum_lin[0] += lin[0] as f64;
        sum_lin[1] += lin[1] as f64;
        sum_lin[2] += lin[2] as f64;
        lumas.push(linear_luminance(lin));
        blurred_srgb.push([
            linear_to_srgb(lin[0]),
            linear_to_srgb(lin[1]),
            linear_to_srgb(lin[2]),
        ]);
    }
    let avg_lin = [
        (sum_lin[0] / crop_len as f64) as f32,
        (sum_lin[1] / crop_len as f64) as f32,
        (sum_lin[2] / crop_len as f64) as f32,
    ];
    let avg_srgb = [
        linear_to_srgb(avg_lin[0]),
        linear_to_srgb(avg_lin[1]),
        linear_to_srgb(avg_lin[2]),
    ];
    lumas.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean_luma = linear_luminance(avg_lin);
    let p75_luma = lumas[(lumas.len() * 3) / 4];
    let strip_luma = mean_luma * 0.65 + p75_luma * 0.35;

    CachedLinearStrip {
        key,
        strip_w: STRIP_W as i32,
        strip_h: STRIP_H as i32,
        blurred_srgb,
        avg_srgb,
        strip_luma,
    }
}

/// 获取指定显示器顶部栏的 macOS Vibrancy 壁纸背景条带（带缓存，切换透明度/主题时零 I/O）。
pub fn desktop_topbar_backdrop(
    mon_rect: RECT,
    bar_h: i32,
    theme: &str,
    opacity: f32,
    force_refresh: bool,
) -> DesktopTopbarBackdrop {
    let virt_rect = virtual_desktop_rect(mon_rect);
    let (path, pos, bg_rgb) = query_monitor_wallpaper(mon_rect);
    let (mtime, file_len) = file_meta(&path);
    let key = MonitorStripKey {
        mon_rect: (mon_rect.left, mon_rect.top, mon_rect.right, mon_rect.bottom),
        virt_rect: (virt_rect.left, virt_rect.top, virt_rect.right, virt_rect.bottom),
        bar_h: bar_h.max(12),
        path: path.clone(),
        mtime,
        file_len,
        pos,
        bg_rgb,
    };

    let cache = STRIP_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let cached = {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        let existing = if !force_refresh {
            guard.iter().find(|c| c.key == key).cloned()
        } else {
            None
        };
        if let Some(hit) = existing {
            hit
        } else {
            drop(guard);
            let img = decode_scaled_wallpaper(&path, 320);
            let built = build_blurred_strip(img, mon_rect, virt_rect, bar_h, pos, bg_rgb, key.clone());
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|c| c.key.mon_rect != key.mon_rect);
            if guard.len() >= 8 {
                guard.remove(0);
            }
            guard.push(built.clone());
            built
        }
    };

    // 根据外观设置决定顶栏极性（system 模式下依据顶栏背后条带真实亮度自动切换）
    let dark_mode = match theme {
        "dark" => true,
        "light" => false,
        _ => cached.strip_luma < 0.36,
    };

    let base_rgb = vibrant_topbar_pixel(cached.avg_srgb, cached.avg_srgb, dark_mode);

    // 依据用户不透明度平滑调节壁纸空间色彩透出度：0.55 通透最贴合壁纸，1.0 过渡到均匀单色
    let op = opacity.clamp(0.2, 1.0);
    let mut spatial_mix = if op >= 0.999 {
        0.0
    } else {
        (1.0 - ((op - 0.55).max(0.0) / 0.45).powf(1.45)).clamp(0.0, 0.92)
    };
    // 强制浅/深主题与壁纸明暗相反时适度提升罩层均色比重，保持克制一体感
    let wallpaper_dark = cached.strip_luma < 0.36;
    if theme != "system" && dark_mode != wallpaper_dark {
        spatial_mix = spatial_mix.min(0.45);
    }

    let mut strip_bgra = Vec::with_capacity(cached.blurred_srgb.len());
    for &px_srgb in &cached.blurred_srgb {
        let sp = vibrant_topbar_pixel(px_srgb, cached.avg_srgb, dark_mode);
        let final_rgb = Rgb::lerp(base_rgb, sp, spatial_mix);
        strip_bgra.push(pack_opaque_rgb(final_rgb));
    }

    DesktopTopbarBackdrop {
        base_rgb,
        dark_text: !dark_mode,
        strip_w: cached.strip_w,
        strip_h: cached.strip_h,
        strip_bgra,
    }
}

/// 通过 WIC 将壁纸缩放到 32×18 采样顶部均色与整图平均亮度。
fn sample_wallpaper_file(path: &str) -> Option<DesktopPalette> {
    let (sw, sh, _, _, px) = decode_scaled_wallpaper(path, 64)?;
    let mut luma_sum = 0.0f64;
    let mut top_r = 0u64;
    let mut top_g = 0u64;
    let mut top_b = 0u64;
    let mut top_cnt = 0u64;
    let top_rows = (sh / 4).max(2);
    for y in 0..sh {
        for x in 0..sw {
            let off = ((y * sw + x) * 4) as usize;
            let b = px[off];
            let g = px[off + 1];
            let r = px[off + 2];
            luma_sum += Rgb::new(r, g, b).luminance() as f64;
            if y < top_rows {
                top_r += r as u64;
                top_g += g as u64;
                top_b += b as u64;
                top_cnt += 1;
            }
        }
    }
    let overall_luma = (luma_sum / (sw * sh) as f64) as f32;
    let top_avg = if top_cnt > 0 {
        Rgb::new(
            (top_r / top_cnt) as u8,
            (top_g / top_cnt) as u8,
            (top_b / top_cnt) as u8,
        )
    } else {
        Rgb::new(0x28, 0x2B, 0x35)
    };
    Some(harmonize_desktop_palette(top_avg, overall_luma))
}

/// 纯色桌面或壁纸读取失败时，回退系统桌面背景色 `COLOR_BACKGROUND`。
fn solid_desktop_palette() -> DesktopPalette {
    let bg = sys_background_rgb();
    harmonize_desktop_palette(bg, bg.luminance())
}

/// 获取当前桌面壁纸调色板（按路径 + 修改时间 + 大小自动缓存，热路径零 I/O）。
pub fn desktop_wallpaper_palette(force_refresh: bool) -> DesktopPalette {
    let path = current_wallpaper_path();
    let (mtime, file_len) = file_meta(&path);
    let lock = WALLPAPER_CACHE.get_or_init(|| Mutex::new(None));
    if !force_refresh {
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.path == path && cached.mtime == mtime && cached.file_len == file_len {
                return cached.palette;
            }
        }
    }
    let palette = sample_wallpaper_file(&path).unwrap_or_else(solid_desktop_palette);
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedWallpaper {
        path,
        mtime,
        file_len,
        palette,
    });
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance_and_text_choice() {
        assert!(Rgb::new(255, 255, 255).use_dark_text());
        assert!(!Rgb::new(0x20, 0x20, 0x20).use_dark_text());
        let l = Rgb::lerp(Rgb::new(0, 0, 0), Rgb::new(200, 100, 50), 0.5);
        assert_eq!(l, Rgb::new(100, 50, 25));
    }

    #[test]
    fn exclude_list_matches_path() {
        let ex = vec!["code.exe".to_string()];
        assert!(is_excluded(r"C:\x\Code.EXE", &ex));
        assert!(!is_excluded(r"C:\x\notepad.exe", &ex));
    }

    #[test]
    fn desktop_palette_harmonizes_dark_and_light_wallpapers() {
        // 深色赛博朋克壁纸：应保留紫色氛围、告别纯黑死黑，且保持浅色字高可读性（>= WCAG AA）
        let dark = harmonize_desktop_palette(Rgb::new(160, 80, 220), 0.08);
        assert!(dark.is_dark());
        assert!(!dark.topbar_rgb.use_dark_text());
        assert!(dark.topbar_rgb.b > dark.topbar_rgb.r && dark.topbar_rgb.r > dark.topbar_rgb.g);
        assert!(dark.topbar_rgb.luminance() >= 0.03 && dark.topbar_rgb.luminance() <= 0.12);

        // 纯黑壁纸也应适度提亮为沉稳磨砂深灰，而不是死黑 #000
        let pitch_black = harmonize_desktop_palette(Rgb::new(0, 0, 0), 0.0);
        assert!(pitch_black.is_dark());
        assert!(pitch_black.topbar_rgb.r >= 35 && pitch_black.topbar_rgb.luminance() >= 0.02);

        // 浅色亮壁纸（柔和水彩不过曝，使用深色字）
        let light = harmonize_desktop_palette(Rgb::new(240, 245, 255), 0.78);
        assert!(!light.is_dark());
        assert!(light.topbar_rgb.use_dark_text());
        assert!(light.topbar_rgb.r < 245);

        // 实机壁纸采样不崩溃
        let live = desktop_wallpaper_palette(true);
        assert!((0.0..=1.0).contains(&live.overall_luma));
    }

    #[test]
    fn vibrant_topbar_preserves_contrast_and_wallpaper_hues() {
        let white_text_luma = Rgb::new(0xF5, 0xF5, 0xF7).luminance();
        let dark_text_luma = Rgb::new(0x1D, 0x1D, 0x1F).luminance();
        let contrast = |a: f32, b: f32| (a.max(b) + 0.05) / (a.min(b) + 0.05);

        // 赛博朋克左侧霓虹紫 + 右侧青蓝：分别保留各自色相，不发灰不发黑，且均达到 >= 5.5:1 对比度
        let avg = [0.22, 0.20, 0.38];
        let purple = vibrant_topbar_pixel([0.30, 0.16, 0.55], avg, true);
        let teal = vibrant_topbar_pixel([0.07, 0.20, 0.26], avg, true);
        let bright_glare = vibrant_topbar_pixel([0.95, 0.98, 1.0], avg, true);
        assert!(purple.b > purple.r && purple.r > purple.g);
        assert!(teal.b > teal.r && teal.g > teal.r);
        assert!(contrast(white_text_luma, purple.luminance()) >= 5.5);
        assert!(contrast(white_text_luma, teal.luminance()) >= 5.5);
        assert!(contrast(white_text_luma, bright_glare.luminance()) >= 5.5);

        // 浅色模式下遇到暗部像素也能提亮保持深色字高对比度
        let light_shadow = vibrant_topbar_pixel([0.04, 0.05, 0.08], [0.75, 0.80, 0.88], false);
        assert!(light_shadow.use_dark_text());
        assert!(contrast(light_shadow.luminance(), dark_text_luma) >= 6.5);

        // 实机壁纸条带生成（验证对比度与非空输出）
        let live_bd = desktop_topbar_backdrop(
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
            30,
            "system",
            0.55,
            true,
        );
        assert_eq!(live_bd.strip_bgra.len(), (live_bd.strip_w * live_bd.strip_h) as usize);
        let fg_luma = if live_bd.dark_text { dark_text_luma } else { white_text_luma };
        assert!(contrast(live_bd.base_rgb.luminance(), fg_luma) >= 5.5);
    }

    #[test]
    fn box_blur_and_strip_mapping_preserve_zones() {
        // 左侧纯紫、右侧纯青图像，经横向高斯模糊后中间形成平滑过渡，首尾保持原色方向
        let w = 40usize;
        let h = 6usize;
        let mut grid = Vec::with_capacity(w * h);
        for _y in 0..h {
            for x in 0..w {
                grid.push(if x < w / 2 { [0.4, 0.1, 0.7] } else { [0.05, 0.35, 0.45] });
            }
        }
        separable_box_blur_3x(&mut grid, w, h, 4, 1);
        assert!(grid[0][2] > grid[0][1]);
        assert!(grid[w - 1][1] > grid[w - 1][0]);
        assert!(grid[w / 2][0] > grid[w - 1][0] && grid[w / 2][0] < grid[0][0]);

        // WallpaperPosition::Fit 在宽屏下左右留边返回 None，Fill 完整铺满返回 Some
        let fit_layout = WallpaperLayout {
            mon_w: 2560.0,
            mon_h: 1080.0,
            virt_ox: 0.0,
            virt_oy: 0.0,
            virt_w: 2560.0,
            virt_h: 1080.0,
            orig_w: 1000.0,
            orig_h: 1000.0,
            pos: WallpaperPosition::Fit,
        };
        let fill_layout = WallpaperLayout { pos: WallpaperPosition::Fill, ..fit_layout };
        assert!(map_monitor_strip_uv(2.0, 10.0, &fit_layout).is_none());
        assert!(map_monitor_strip_uv(2.0, 10.0, &fill_layout).is_some());

        // 条带放大渲染尺寸精确匹配且完全不透明
        let bd = DesktopTopbarBackdrop {
            base_rgb: Rgb::new(48, 36, 92),
            dark_text: false,
            strip_w: 4,
            strip_h: 2,
            strip_bgra: vec![0xFF50_2480; 8],
        };
        let rendered = bd.render_to_size(120, 30);
        assert_eq!(rendered.len(), 120 * 30);
        assert!(rendered.iter().all(|&px| (px >> 24) == 0xFF));
    }

    #[test]
    fn strip_sampling_skips_occluded_columns() {
        let w = 200i32;
        let h = 10i32;
        // 整条标题栏实际为浅白色，但右半区被深色浮窗遮挡；标记右半区为遮挡后应从左中未遮挡区得到白色
        let white_px = 0x00FF_FFFFu32;
        let dark_popup_px = 0x0018_1818u32;
        let mut slice = vec![white_px; (w * h) as usize];
        let mut visible = vec![true; w as usize];
        let split = (w as f32 * 0.60) as i32;
        for x in split..w {
            visible[x as usize] = false;
            for y in 0..h {
                slice[(y * w + x) as usize] = dark_popup_px;
            }
        }
        assert_eq!(
            extract_dominant_strip_color(&slice, w, h, &visible),
            Some(Rgb::new(255, 255, 255))
        );

        // 若整条条带均被浮窗完全遮挡，应返回 None 以触发 PrintWindow 回退而不是返回黑色
        let all_hidden = vec![false; w as usize];
        assert_eq!(extract_dominant_strip_color(&slice, w, h, &all_hidden), None);
        assert!(unsafe { point_belongs_to_window(HWND::default(), 0, 0) });
    }

    #[test]
    fn dock_backdrop_harmonization_and_sampling() {
        // 1. 无背后窗口时严格按回退极性（Windows 系统主题 -> 壁纸）输出标准亮/暗银黑托盘
        let (d1, t1, _b1) = harmonize_dock_backdrop(None, true, false);
        assert!(!d1);
        assert!(t1.r > 210);
        let (d2, t2, _b2) = harmonize_dock_backdrop(None, false, true);
        assert!(d2);
        assert!(t2.r < 60);

        // 2. 背后为亮白/浅灰应用窗口时应自动切到浅色磨砂托盘
        let (d_light, tl, bl) = harmonize_dock_backdrop(Some(Rgb::new(245, 246, 248)), true, true);
        assert!(!d_light);
        assert!(tl.r > 220 && bl.r > 205);

        // 3. 背后为深色窗口时应切换深色曜石托盘
        let (d_dark, td, bd) = harmonize_dock_backdrop(Some(Rgb::new(26, 28, 34)), false, false);
        assert!(d_dark);
        assert!(td.r < 55 && bd.r < 40);

        // 4. 中灰滞回区（0.24..0.33，例如 #8C8C8C ≈ 0.26）保持原极性不闪烁
        let (d_keep_dark, _, _) = harmonize_dock_backdrop(Some(Rgb::new(0x8C, 0x8C, 0x8C)), true, false);
        assert!(d_keep_dark);
        let (d_keep_light, _, _) = harmonize_dock_backdrop(Some(Rgb::new(0x8C, 0x8C, 0x8C)), false, true);
        assert!(!d_keep_light);

        // 5. 纯色/复杂纹理样本颜色提取
        let bgra_white = vec![0x00F4_F6F8u32; 40];
        assert_eq!(dominant_bgra_color(&bgra_white, 12), Some(Rgb::new(0xF4, 0xF6, 0xF8)));
        // 复杂纹理（色彩分散但亮度偏亮）退化为中段均值仍稳定返回亮色
        let mixed: Vec<u32> = (0..30).map(|i| {
            let v = 210 + (i % 10) * 4;
            ((v as u32) << 16) | ((v as u32) << 8) | (v as u32)
        }).collect();
        let mixed_rgb = dominant_bgra_color(&mixed, 12).unwrap();
        assert!(mixed_rgb.r >= 210 && mixed_rgb.luminance() > 0.6);
        assert_eq!(dominant_bgra_color(&bgra_white[..5], 12), None);
    }
}
