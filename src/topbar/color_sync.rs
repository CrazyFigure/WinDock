//! 最大化窗口标题栏微条带采样、色彩平滑插值与对比度文字适配。
//!
//! 事件驱动：仅在显示器前台窗口进入最大化态时触发一次轻量取色（50ms 防抖），
//! 绝不每帧截屏。采样区域避开顶部栏下沿 2..8px、右侧标题栏按钮区与左侧菜单图标区。

use std::collections::HashMap;

use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BitBlt, GdiFlush, GetDC, ReleaseDC,
    SelectObject, SRCCOPY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HBITMAP, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, PW_RENDERFULLCONTENT};

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

/// 主路径：仅截取标题栏中间待采样区屏幕条带（节省内存与带宽，兼容 Chrome/VS Code/Electron 等 GPU 加速软件）。
pub fn sample_below_topbar(mon: RECT, topbar_h: i32) -> Option<Rgb> {
    let mon_w = (mon.right - mon.left).clamp(128, 8192);
    let x_start = (mon_w as f32 * 0.30) as i32;
    let x_end = (mon_w as f32 * 0.84) as i32;
    let w = (x_end - x_start).max(64);
    // 跳过最顶部 3px 窗口细边，采样 y = topbar_h + 4 .. topbar_h + 14 的 10px 条带
    let y_start = mon.top + topbar_h.max(1) + 4;
    let h = 10;
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let (hdc, bmp, old, ptr) = dib_section(w, h)?;
        let ok = BitBlt(hdc, 0, 0, w, h, screen, mon.left + x_start, y_start, SRCCOPY).is_ok();
        ReleaseDC(None, screen);
        let _ = GdiFlush();
        let result = if ok {
            let slice = std::slice::from_raw_parts(ptr as *const u32, (w * h) as usize);
            let mid_split = (w as f32 * 0.60) as i32;
            let ranges = [
                (mid_split, w),
                (0, mid_split),
            ];
            let mut best: Option<Rgb> = None;
            for (x0, x1) in ranges {
                let mut bins: HashMap<(u8, u8, u8), (u32, u64, u64, u64)> = HashMap::new();
                let step = ((x1 - x0) / 80).max(1);
                for y in 0..h {
                    let mut x = x0;
                    while x < x1 {
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
                        x += step;
                    }
                }
                if let Some((_, &(cnt, sr, sg, sb))) = bins.iter().max_by_key(|(_, v)| v.0) {
                    if cnt >= 24 {
                        best = Some(Rgb::new((sr / cnt as u64) as u8, (sg / cnt as u64) as u8, (sb / cnt as u64) as u8));
                        break;
                    }
                }
            }
            best
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

/// 兜底路径：读取窗口 DWMWA_CAPTION_COLOR / 深色模式属性。
pub fn caption_color_fallback(hwnd: HWND) -> Option<Rgb> {
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
        if crate::win32::system_uses_dark_theme() {
            Some(Rgb::new(0x20, 0x20, 0x20))
        } else {
            Some(Rgb::new(0xF3, 0xF3, 0xF3))
        }
    }
}

/// 综合取色：优先采顶部栏紧下方真实标题栏条带，失败再回退 PrintWindow 与 DWM 属性。
pub fn caption_color(hwnd: HWND, mon: Option<RECT>, topbar_h: i32) -> Rgb {
    mon.and_then(|r| sample_below_topbar(r, topbar_h))
        .or_else(|| sample_caption(hwnd))
        .or_else(|| caption_color_fallback(hwnd))
        .unwrap_or(Rgb::new(0xF3, 0xF3, 0xF3))
}

/// 判断应用是否位于最大化自动取色排除名单中。
pub fn is_excluded(exe: &str, exclude: &[String]) -> bool {
    let lower = exe.to_lowercase();
    exclude.iter().any(|e| {
        let e = e.trim().to_lowercase();
        !e.is_empty() && (lower.contains(&e) || exe.to_lowercase().contains(&e))
    })
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
}
