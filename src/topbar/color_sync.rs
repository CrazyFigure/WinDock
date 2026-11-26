//! 最大化窗口标题栏微条带采样、色彩平滑插值与对比度文字适配。
//!
//! 事件驱动：仅在显示器前台窗口进入最大化态时触发一次轻量取色（50ms 防抖），
//! 绝不每帧截屏。采样区域避开顶部栏下沿 2..8px、右侧标题栏按钮区与左侧菜单图标区。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, GENERIC_READ, HWND, RECT};
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
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SystemParametersInfoW, PW_RENDERFULLCONTENT, SPI_GETDESKWALLPAPER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
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

/// 桌面壁纸采样调色板：用于生成桌面态统一不刺眼的顶部栏颜色及 Dock 明暗基调。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopPalette {
    pub topbar_rgb: Rgb,
    pub overall_luma: f32,
}

impl DesktopPalette {
    /// 壁纸整体偏暗时返回 true（深色桌面应配沉稳深色顶栏与深色 Dock，避免刺眼纯白）。
    pub fn is_dark(&self) -> bool {
        self.overall_luma < 0.38
    }
}

struct CachedWallpaper {
    path: String,
    mtime: Option<SystemTime>,
    palette: DesktopPalette,
}

static WALLPAPER_CACHE: OnceLock<Mutex<Option<CachedWallpaper>>> = OnceLock::new();

/// 清空桌面壁纸采样缓存（系统广播壁纸变更时调用）。
pub fn invalidate_wallpaper_cache() {
    if let Some(lock) = WALLPAPER_CACHE.get() {
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 根据壁纸顶部条带平均色与整体亮度生成均匀、克制、不刺眼的桌面顶部栏配色。
pub fn harmonize_desktop_palette(top_avg: Rgb, overall_luma: f32) -> DesktopPalette {
    let luma = overall_luma.clamp(0.0, 1.0);
    // 先适度降低饱和度（向灰度收敛 55%），避免霓虹壁纸让整条顶栏偏紫偏粉
    let gray = 0.299 * top_avg.r as f32 + 0.587 * top_avg.g as f32 + 0.114 * top_avg.b as f32;
    let desat = |c: u8| -> f32 { gray + (c as f32 - gray) * 0.45 };
    let (mut dr, mut dg, mut db) = (desat(top_avg.r), desat(top_avg.g), desat(top_avg.b));

    let topbar_rgb = if luma < 0.38 {
        // 深色壁纸：限制顶部霓虹高光亮度，再与沉稳曜石炭灰 #191B22 融合，保证深邃护眼且 100% 均匀
        let max_c = dr.max(dg).max(db);
        if max_c > 84.0 {
            let scale = 84.0 / max_c;
            dr *= scale;
            dg *= scale;
            db *= scale;
        }
        let muted = Rgb::new(dr.round() as u8, dg.round() as u8, db.round() as u8);
        Rgb::lerp(Rgb::new(0x19, 0x1B, 0x22), muted, 0.26)
    } else {
        // 浅色壁纸：与克制的柔和银灰 #DCE0E8 融合，清透耐看不刺眼
        let muted = Rgb::new(
            dr.clamp(145.0, 235.0).round() as u8,
            dg.clamp(145.0, 235.0).round() as u8,
            db.clamp(145.0, 235.0).round() as u8,
        );
        Rgb::lerp(Rgb::new(0xDC, 0xE0, 0xE8), muted, 0.24)
    };
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

/// 通过 WIC 将壁纸缩放到 32×18 采样顶部均色与整图平均亮度。
fn sample_wallpaper_file(path: &str) -> Option<DesktopPalette> {
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
        const SW: u32 = 32;
        const SH: u32 = 18;
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&frame, SW, SH, WICBitmapInterpolationModeFant)
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
        let mut px = vec![0u8; (SW * SH * 4) as usize];
        converter.CopyPixels(std::ptr::null(), SW * 4, &mut px).ok()?;

        let mut luma_sum = 0.0f64;
        let mut top_r = 0u64;
        let mut top_g = 0u64;
        let mut top_b = 0u64;
        let mut top_cnt = 0u64;
        let top_rows = (SH / 4).max(2);
        for y in 0..SH {
            for x in 0..SW {
                let off = ((y * SW + x) * 4) as usize;
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
        let overall_luma = (luma_sum / (SW * SH) as f64) as f32;
        let top_avg = if top_cnt > 0 {
            Rgb::new(
                (top_r / top_cnt) as u8,
                (top_g / top_cnt) as u8,
                (top_b / top_cnt) as u8,
            )
        } else {
            Rgb::new(0x19, 0x1B, 0x22)
        };
        Some(harmonize_desktop_palette(top_avg, overall_luma))
    }
}

/// 纯色桌面或壁纸读取失败时，回退系统桌面背景色 `COLOR_BACKGROUND`。
fn solid_desktop_palette() -> DesktopPalette {
    let c = unsafe { GetSysColor(COLOR_BACKGROUND) };
    let bg = Rgb::new((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8);
    harmonize_desktop_palette(bg, bg.luminance())
}

/// 获取当前桌面壁纸调色板（按路径 + 修改时间自动缓存，热路径零 I/O）。
pub fn desktop_wallpaper_palette(force_refresh: bool) -> DesktopPalette {
    let path = current_wallpaper_path();
    let mtime = if path.is_empty() {
        None
    } else {
        std::fs::metadata(&path).and_then(|m| m.modified()).ok()
    };
    let lock = WALLPAPER_CACHE.get_or_init(|| Mutex::new(None));
    if !force_refresh {
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.path == path && cached.mtime == mtime {
                return cached.palette;
            }
        }
    }
    let palette = sample_wallpaper_file(&path).unwrap_or_else(solid_desktop_palette);
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedWallpaper {
        path,
        mtime,
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
        // 深色赛博朋克壁纸（即使顶部含霓虹高光，结果也应沉稳护眼并用浅色字）
        let dark = harmonize_desktop_palette(Rgb::new(160, 80, 220), 0.08);
        assert!(dark.is_dark());
        assert!(!dark.topbar_rgb.use_dark_text());
        assert!(dark.topbar_rgb.luminance() < 0.06);

        // 浅色亮壁纸（柔和银灰不惨白，使用深色字）
        let light = harmonize_desktop_palette(Rgb::new(240, 245, 255), 0.78);
        assert!(!light.is_dark());
        assert!(light.topbar_rgb.use_dark_text());
        assert!(light.topbar_rgb.r < 245);

        // 实机壁纸采样不崩溃
        let live = desktop_wallpaper_palette(true);
        assert!((0.0..=1.0).contains(&live.overall_luma));
    }
}
