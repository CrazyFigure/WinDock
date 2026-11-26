//! 共享软件抗锯齿绘图与现代界面组件库（32bpp DIB 像素缓冲），复用并升级自 WinLaunchpad。
//!
//! 统一封装圆角矩形、抗锯齿圆/线、图标双线性缩放与阴影、文字阴影与测量、
//! 现代复选框/开关/按钮/滚动条、macOS 双主题色板、状态矢量字形与 100ms Tooltip 浮层。

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_EMERGENCY_BREAK, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GdiFlush, GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, HDC, HFONT, HGDIOBJ, TRANSPARENT,
};

pub type Rgba = (f32, f32, f32, f32);

/// RGB u32 颜色常量辅助构造（alpha 0..1）。
#[inline]
pub const fn rgb(hex: u32, a: f32) -> Rgba {
    (
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
        a,
    )
}

pub fn to_colorref(c: Rgba) -> COLORREF {
    let r = (c.0.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.1.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.2.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF((b << 16) | (g << 8) | r)
}

#[inline]
fn pack_bgra(r: f32, g: f32, b: f32) -> u32 {
    let ri = (r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let gi = (g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let bi = (b.clamp(0.0, 1.0) * 255.0).round() as u32;
    0xFF00_0000 | (ri << 16) | (gi << 8) | bi
}

#[derive(Clone)]
pub struct IconPixels {
    pub w: u32,
    pub h: u32,
    pub px: std::sync::Arc<Vec<u8>>,
}

/// 判断预乘 BGRA 图标是否为「带透明底的纯暗色低饱和单色图标」（如 ChatGPT / Seer 黑色线稿）。
/// 这类图标在深色背景上几乎不可见，绘制时应自动提亮为柔和浅白。
pub fn is_dark_monochrome_icon(icon: &IconPixels) -> bool {
    let total = (icon.w as usize).saturating_mul(icon.h as usize);
    if total == 0 || icon.px.len() < total * 4 {
        return false;
    }
    // 对高分辨率图标按步长采样，单次检测仅数百像素、微秒级开销
    let step = (total / 512).max(1);
    let mut trans = 0usize;
    let mut sampled = 0usize;
    let mut vis_w = 0.0f32;
    let mut luma_sum = 0.0f32;
    let mut bright_or_color = 0.0f32;
    let px = icon.px.as_slice();
    let mut idx = 0usize;
    while idx < total {
        let off = idx * 4;
        let a = px[off + 3];
        sampled += 1;
        if a <= 20 {
            trans += 1;
        } else if a >= 40 {
            let af = a as f32;
            let b = (px[off] as f32 / af).clamp(0.0, 1.0);
            let g = (px[off + 1] as f32 / af).clamp(0.0, 1.0);
            let r = (px[off + 2] as f32 / af).clamp(0.0, 1.0);
            let luma = 0.114 * b + 0.587 * g + 0.299 * r;
            let sat = b.max(g).max(r) - b.min(g).min(r);
            let w = af / 255.0;
            vis_w += w;
            luma_sum += luma * w;
            if luma > 0.42 || sat > 0.24 {
                bright_or_color += w;
            }
        }
        idx += step;
    }
    if sampled == 0 || vis_w <= 0.5 {
        return false;
    }
    let trans_ratio = trans as f32 / sampled as f32;
    let avg_luma = luma_sum / vis_w;
    let bright_ratio = bright_or_color / vis_w;
    trans_ratio > 0.12 && avg_luma < 0.34 && bright_ratio < 0.18
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    Checked,
    Indeterminate,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnStyle {
    Primary,
    Secondary,
    Ghost,
    DangerGhost,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}


/// 创建 ClearType 抗锯齿字体（像素高度，负值表示字符高度）。
pub fn make_font(px: i32, weight: i32) -> HFONT {
    let px = px.max(9);
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0,
            0,
            5, // CLEARTYPE_QUALITY
            0,
            w!("Microsoft YaHei UI"),
        )
    }
}

/// 可重用的 32bpp DIB 后备缓冲，提供显式释放以严格控制常驻内存。
pub struct DibTarget {
    pub mem_dc: Option<HDC>,
    pub dib: Option<HGDIOBJ>,
    pub old_bmp: Option<HGDIOBJ>,
    pub ptr: *mut std::ffi::c_void,
    pub w: i32,
    pub h: i32,
}

impl Default for DibTarget {
    fn default() -> Self {
        Self {
            mem_dc: None,
            dib: None,
            old_bmp: None,
            ptr: std::ptr::null_mut(),
            w: 0,
            h: 0,
        }
    }
}

impl DibTarget {
    /// 确保后备缓冲至少达到指定宽高，按 128px 分档扩容以避免实时手势拖动期间频繁重建 DIB。
    pub fn ensure_at_least(&mut self, w: i32, h: i32) -> bool {
        let w = w.max(1);
        let h = h.max(1);
        if self.mem_dc.is_some() && self.w >= w && self.h >= h && !self.ptr.is_null() {
            return true;
        }
        let bw = (((w + 127) / 128) * 128).max(128);
        let bh = (((h + 127) / 128) * 128).max(128);
        self.ensure(bw, bh)
    }

    pub fn ensure(&mut self, w: i32, h: i32) -> bool {
        let w = w.max(1);
        let h = h.max(1);
        if self.mem_dc.is_some() && self.w == w && self.h == h && !self.ptr.is_null() {
            return true;
        }
        self.release();
        unsafe {
            let dc = match CreateCompatibleDC(None) {
                dc if !dc.is_invalid() => dc,
                _ => return false,
            };
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
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut ptr, None, 0).unwrap_or_default();
            if bmp.is_invalid() || ptr.is_null() {
                let _ = DeleteDC(dc);
                return false;
            }
            let old = SelectObject(dc, bmp);
            self.mem_dc = Some(dc);
            self.dib = Some(HGDIOBJ(bmp.0));
            self.old_bmp = Some(old);
            self.ptr = ptr;
            self.w = w;
            self.h = h;
            true
        }
    }

    pub fn release(&mut self) {
        unsafe {
            if let (Some(dc), Some(old)) = (self.mem_dc, self.old_bmp) {
                SelectObject(dc, old);
            }
            if let Some(d) = self.dib {
                let _ = DeleteObject(d);
            }
            if let Some(dc) = self.mem_dc {
                let _ = DeleteDC(dc);
            }
        }
        self.mem_dc = None;
        self.dib = None;
        self.old_bmp = None;
        self.ptr = std::ptr::null_mut();
        self.w = 0;
        self.h = 0;
    }

    pub fn surface(&mut self) -> Option<Surface> {
        if self.ptr.is_null() || self.w <= 0 || self.h <= 0 {
            return None;
        }
        Some(Surface {
            w: self.w as usize,
            h: self.h as usize,
            ptr: self.ptr as *mut u32,
            clip: None,
        })
    }
}

impl Drop for DibTarget {
    fn drop(&mut self) {
        self.release();
    }
}

/// 指向 32bpp BGRA 内存的轻量绘制表面，支持可选矩形裁剪。
pub struct Surface {
    pub w: usize,
    pub h: usize,
    pub ptr: *mut u32,
    pub clip: Option<RECT>,
}

impl Surface {
    pub fn set_clip(&mut self, r: Option<RECT>) {
        self.clip = r;
    }

    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return false;
        }
        if let Some(c) = self.clip {
            return x >= c.left && x < c.right && y >= c.top && y < c.bottom;
        }
        true
    }

    pub fn clear(&mut self, c: Rgba) {
        let px = pack_bgra(c.0, c.1, c.2);
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        slice.fill(px);
    }

    /// 将指定矩形区域清空为全透明像素（0x00000000），供 UpdateLayeredWindow 透明浮窗复用缓冲。
    pub fn clear_rect_transparent(&mut self, w: i32, h: i32) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        if cw == 0 || ch == 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let row = y * self.w;
            slice[row..row + cw].fill(0);
        }
    }

    /// 将指定矩形区域清空为纯黑不透明像素，供 GDI 文字生成灰度 Alpha 遮罩使用。
    pub fn clear_rect_black(&mut self, w: i32, h: i32) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        if cw == 0 || ch == 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let row = y * self.w;
            slice[row..row + cw].fill(0xFF00_0000);
        }
    }

    /// 将预烘焙好的不透明 BGRA 像素块（w×h）直接拷贝到 Surface 左上角。
    pub fn blit_opaque_bgra(&mut self, w: i32, h: i32, src: &[u32]) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        let sw = w.max(0) as usize;
        if cw == 0 || ch == 0 || src.len() < ch.saturating_mul(sw) {
            return;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let srow = y * sw;
            let drow = y * self.w;
            dst[drow..drow + cw].copy_from_slice(&src[srow..srow + cw]);
        }
    }

    /// 将预烘焙的壁纸条带与目标纯色按 `t (0..1)` 逐像素线性混合后写入 Surface。
    pub fn blit_lerped_bgra(&mut self, w: i32, h: i32, src: &[u32], to: (u8, u8, u8), t: f32) {
        let cw = (w.max(0) as usize).min(self.w);
        let ch = (h.max(0) as usize).min(self.h);
        let sw = w.max(0) as usize;
        if cw == 0 || ch == 0 || src.len() < ch.saturating_mul(sw) {
            return;
        }
        let t256 = (t.clamp(0.0, 1.0) * 256.0).round() as u32;
        let inv256 = 256 - t256;
        let tr = to.0 as u32 * t256;
        let tg = to.1 as u32 * t256;
        let tb = to.2 as u32 * t256;
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..ch {
            let srow = y * sw;
            let drow = y * self.w;
            for x in 0..cw {
                let p = src[srow + x];
                let sb = p & 0xFF;
                let sg = (p >> 8) & 0xFF;
                let sr = (p >> 16) & 0xFF;
                let r = ((sr * inv256 + tr + 128) >> 8).min(255);
                let g = ((sg * inv256 + tg + 128) >> 8).min(255);
                let b = ((sb * inv256 + tb + 128) >> 8).min(255);
                dst[drow + x] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
    }

    /// 在透明表面上执行标准预乘 BGRA Porter-Duff OVER 合成，保证 RGB 始终 <= A。
    #[inline]
    pub fn blend_premul_over(&mut self, x: i32, y: i32, b: u32, g: u32, r: u32, a: u32) {
        if a == 0 || !self.in_bounds(x, y) {
            return;
        }
        let p = unsafe { &mut *self.ptr.add(y as usize * self.w + x as usize) };
        if a >= 255 {
            *p = 0xFF00_0000 | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
            return;
        }
        let dst = *p;
        if dst == 0 {
            *p = (a << 24) | (r.min(a) << 16) | (g.min(a) << 8) | b.min(a);
            return;
        }
        let ia = 255 - a;
        let da = (dst >> 24) & 0xFF;
        let dr = (dst >> 16) & 0xFF;
        let dg = (dst >> 8) & 0xFF;
        let db = dst & 0xFF;
        let out_a = (a + (da * ia + 127) / 255).min(255);
        let out_r = (r + (dr * ia + 127) / 255).min(out_a);
        let out_g = (g + (dg * ia + 127) / 255).min(out_a);
        let out_b = (b + (db * ia + 127) / 255).min(out_a);
        *p = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
    }

    pub fn clear_vertical_gradient(&mut self, top: (f32, f32, f32), mid: (f32, f32, f32), bottom: (f32, f32, f32)) {
        let h = self.h.max(1);
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
        for y in 0..self.h {
            let t = y as f32 / h as f32;
            let (r, g, b) = if t < 0.55 {
                let k = t / 0.55;
                (
                    top.0 + (mid.0 - top.0) * k,
                    top.1 + (mid.1 - top.1) * k,
                    top.2 + (mid.2 - top.2) * k,
                )
            } else {
                let k = (t - 0.55) / 0.45;
                (
                    mid.0 + (bottom.0 - mid.0) * k,
                    mid.1 + (bottom.1 - mid.1) * k,
                    mid.2 + (bottom.2 - mid.2) * k,
                )
            };
            let c = pack_bgra(r, g, b);
            let row = y * self.w;
            slice[row..row + self.w].fill(c);
        }
    }

    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        let a = (c.3.clamp(0.0, 1.0) * 255.0).round() as u32;
        if a == 0 {
            return;
        }
        // 统一走标准预乘 Porter-Duff OVER，兼顾不透明窗口与透明分层/毛玻璃窗口，杜绝黑边锯齿
        let r = ((c.0.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        let g = ((c.1.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        let b = ((c.2.clamp(0.0, 1.0) * a as f32).round() as u32).min(a);
        self.blend_premul_over(x, y, b, g, r, a);
    }

    /// 用整数混合预乘 BGRA 像素到指定坐标（0..255），完整保留透明通道。
    #[inline]
    fn blend_premul_u8(&mut self, x: i32, y: i32, b: u32, g: u32, r: u32, a: u32) {
        self.blend_premul_over(x, y, b, g, r, a);
    }

    /// 计算当前裁剪区与目标矩形的交集边界。
    fn clamp_bounds(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
        let mut lx0 = x0.max(0);
        let mut ly0 = y0.max(0);
        let mut lx1 = x1.min(self.w as i32);
        let mut ly1 = y1.min(self.h as i32);
        if let Some(c) = self.clip {
            lx0 = lx0.max(c.left);
            ly0 = ly0.max(c.top);
            lx1 = lx1.min(c.right);
            ly1 = ly1.min(c.bottom);
        }
        (lx0, ly0, lx1, ly1)
    }

    pub fn fill_rect(&mut self, r: &RECT, c: Rgba) {
        if c.3 <= 0.001 {
            return;
        }
        let (x0, y0, x1, y1) = self.clamp_bounds(r.left, r.top, r.right, r.bottom);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        if c.3 >= 0.999 {
            let packed = pack_bgra(c.0, c.1, c.2);
            let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.w * self.h) };
            for y in y0..y1 {
                let row = y as usize * self.w;
                slice[row + x0 as usize..row + x1 as usize].fill(packed);
            }
            return;
        }
        let sa = (c.3.clamp(0.0, 1.0) * 255.0).round() as u32;
        if sa == 0 {
            return;
        }
        let sr = ((c.0.clamp(0.0, 1.0) * sa as f32).round() as u32).min(sa);
        let sg = ((c.1.clamp(0.0, 1.0) * sa as f32).round() as u32).min(sa);
        let sb = ((c.2.clamp(0.0, 1.0) * sa as f32).round() as u32).min(sa);
        let ia = 255 - sa;
        let premul_solid = (sa << 24) | (sr << 16) | (sg << 8) | sb;
        for y in y0..y1 {
            let row = y as usize * self.w;
            for x in x0..x1 {
                let p = unsafe { &mut *self.ptr.add(row + x as usize) };
                let dst = *p;
                if dst == 0 {
                    *p = premul_solid;
                    continue;
                }
                let da = (dst >> 24) & 0xFF;
                let dr = (dst >> 16) & 0xFF;
                let dg = (dst >> 8) & 0xFF;
                let db = dst & 0xFF;
                let out_a = (sa + (da * ia + 127) / 255).min(255);
                let out_r = (sr + (dr * ia + 127) / 255).min(out_a);
                let out_g = (sg + (dg * ia + 127) / 255).min(out_a);
                let out_b = (sb + (db * ia + 127) / 255).min(out_a);
                *p = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
            }
        }
    }

    /// 竖向平滑渐变抗锯齿圆角矩形（用于 macOS 玻璃胶囊、质感图标等）。
    pub fn fill_rrect_vgrad(&mut self, r: &RECT, radius: f32, top: Rgba, bot: Rgba) {
        if r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let h = (b - t).max(1.0);
        let max_r = ((rr - l).min(h) * 0.5).max(0.0);
        let radius = radius.min(max_r);
        let (x0, y0, x1, y1) = self.clamp_bounds(r.left - 1, r.top - 1, r.right + 1, r.bottom + 1);
        for y in y0..y1 {
            let fy = y as f32 + 0.5;
            let k = ((fy - t) / h).clamp(0.0, 1.0);
            let c = (
                top.0 + (bot.0 - top.0) * k,
                top.1 + (bot.1 - top.1) * k,
                top.2 + (bot.2 - top.2) * k,
                top.3 + (bot.3 - top.3) * k,
            );
            if c.3 <= 0.001 {
                continue;
            }
            for x in x0..x1 {
                let d = Self::sd_rrect(x as f32 + 0.5, fy, l, t, rr, b, radius);
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 抗锯齿填充小三角形（用于 macOS 风格 Tooltip 指向角标）。
    pub fn fill_triangle_aa(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, x2: f32, y2: f32, c: Rgba) {
        if c.3 <= 0.001 {
            return;
        }
        let (bx0, by0, bx1, by1) = self.clamp_bounds(
            x0.min(x1).min(x2).floor() as i32 - 1,
            y0.min(y1).min(y2).floor() as i32 - 1,
            x0.max(x1).max(x2).ceil() as i32 + 1,
            y0.max(y1).max(y2).ceil() as i32 + 1,
        );
        // 统一为逆时针朝向以用有符号边距计算抗锯齿覆盖率
        let area = (x1 - x0) * (y2 - y0) - (y1 - y0) * (x2 - x0);
        let (ax, ay, bx, by, cx, cy) = if area >= 0.0 {
            (x0, y0, x1, y1, x2, y2)
        } else {
            (x0, y0, x2, y2, x1, y1)
        };
        let edge = |px: f32, py: f32, ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
            let ex = vx - ux;
            let ey = vy - uy;
            let len = (ex * ex + ey * ey).sqrt().max(1e-4);
            ((px - ux) * ey - (py - uy) * ex) / len
        };
        for y in by0..by1 {
            let py = y as f32 + 0.5;
            for x in bx0..bx1 {
                let px = x as f32 + 0.5;
                let d0 = edge(px, py, ax, ay, bx, by);
                let d1 = edge(px, py, bx, by, cx, cy);
                let d2 = edge(px, py, cx, cy, ax, ay);
                let d = d0.max(d1).max(d2);
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    #[inline]
    fn sd_rrect(px: f32, py: f32, left: f32, top: f32, right: f32, bottom: f32, r: f32) -> f32 {
        let cx = (left + right) * 0.5;
        let cy = (top + bottom) * 0.5;
        let hw = ((right - left) * 0.5 - r).max(0.0);
        let hh = ((bottom - top) * 0.5 - r).max(0.0);
        let qx = (px - cx).abs() - hw;
        let qy = (py - cy).abs() - hh;
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// 填充抗锯齿圆角矩形（中间核心矩形走高速填充，仅对边缘/四角做 SDF 抗锯齿）。
    pub fn fill_rrect(&mut self, r: &RECT, radius: f32, c: Rgba) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let max_r = ((rr - l).min(b - t) * 0.5).max(0.0);
        let radius = radius.min(max_r);
        if radius <= 0.5 {
            self.fill_rect(r, c);
            return;
        }
        let pad = radius.ceil() as i32 + 1;
        let inner_top = r.top + pad;
        let inner_bot = r.bottom - pad;
        let inner_left = r.left + pad;
        let inner_right = r.right - pad;

        // 中心大块区域直接填充，无需逐像素算 SDF
        if inner_top < inner_bot && inner_left < inner_right {
            let core = RECT {
                left: r.left + 1,
                top: inner_top,
                right: r.right - 1,
                bottom: inner_bot,
            };
            self.fill_rect(&core, c);
        }

        let (x0, y0, x1, y1) = self.clamp_bounds(r.left - 1, r.top - 1, r.right + 1, r.bottom + 1);
        for y in y0..y1 {
            let in_core_y = inner_top < inner_bot && y >= inner_top && y < inner_bot;
            for x in x0..x1 {
                if in_core_y && x >= r.left + 1 && x < r.right - 1 {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, radius);
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    pub fn stroke_rrect(&mut self, r: &RECT, radius: f32, width: f32, c: Rgba) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (
            r.left as f32 + width * 0.5,
            r.top as f32 + width * 0.5,
            r.right as f32 - width * 0.5,
            r.bottom as f32 - width * 0.5,
        );
        let rad = (radius - width * 0.5).max(0.0);
        let (x0, y0, x1, y1) = self.clamp_bounds(
            l.floor() as i32 - 2,
            t.floor() as i32 - 2,
            rr.ceil() as i32 + 2,
            b.ceil() as i32 + 2,
        );
        let half = width * 0.5;
        let pad = (radius + width).ceil() as i32 + 2;
        let skip_l = r.left + pad;
        let skip_r = r.right - pad;
        let skip_t = r.top + pad;
        let skip_b = r.bottom - pad;
        for y in y0..y1 {
            let in_skip_y = skip_t < skip_b && y >= skip_t && y < skip_b;
            for x in x0..x1 {
                if in_skip_y && skip_l < skip_r && x >= skip_l && x < skip_r {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, rad).abs();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 虚线圆角矩形描边（用于拖放区提示）。
    pub fn stroke_rrect_dashed(&mut self, r: &RECT, radius: f32, width: f32, dash: i32, c: Rgba) {
        if c.3 <= 0.001 || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let (l, t, rr, b) = (
            r.left as f32 + width * 0.5,
            r.top as f32 + width * 0.5,
            r.right as f32 - width * 0.5,
            r.bottom as f32 - width * 0.5,
        );
        let rad = (radius - width * 0.5).max(0.0);
        let (x0, y0, x1, y1) = self.clamp_bounds(
            l.floor() as i32 - 2,
            t.floor() as i32 - 2,
            rr.ceil() as i32 + 2,
            b.ceil() as i32 + 2,
        );
        let half = width * 0.5;
        let pad = (radius + width).ceil() as i32 + 2;
        let skip_l = r.left + pad;
        let skip_r = r.right - pad;
        let skip_t = r.top + pad;
        let skip_b = r.bottom - pad;
        let period = (dash * 2).max(2);
        for y in y0..y1 {
            let in_skip_y = skip_t < skip_b && y >= skip_t && y < skip_b;
            for x in x0..x1 {
                if in_skip_y && skip_l < skip_r && x >= skip_l && x < skip_r {
                    continue;
                }
                let d = Self::sd_rrect(x as f32 + 0.5, y as f32 + 0.5, l, t, rr, b, rad).abs();
                if d < half + 0.5 {
                    let phase = (x + y).rem_euclid(period);
                    if phase < dash {
                        let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                        self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                    }
                }
            }
        }
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba) {
        if c.3 <= 0.001 || radius <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = self.clamp_bounds(
            (cx - radius - 1.0).floor() as i32,
            (cy - radius - 1.0).floor() as i32,
            (cx + radius + 1.0).ceil() as i32,
            (cy + radius + 1.0).ceil() as i32,
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() - radius;
                if d < 0.5 {
                    let cov = (0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    pub fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, width: f32, c: Rgba) {
        let outer = radius + width * 0.5;
        let (x0, y0, x1, y1) = self.clamp_bounds(
            (cx - outer - 1.0).floor() as i32,
            (cy - outer - 1.0).floor() as i32,
            (cx + outer + 1.0).ceil() as i32,
            (cy + outer + 1.0).ceil() as i32,
        );
        let half = width * 0.5;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = (((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() - radius).abs();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 抗锯齿粗线段（圆头端点）。
    pub fn thick_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Rgba) {
        let half = width * 0.5;
        let pad = half + 1.5;
        let (bx0, by0, bx1, by1) = self.clamp_bounds(
            (x0.min(x1) - pad).floor() as i32,
            (y0.min(y1) - pad).floor() as i32,
            (x0.max(x1) + pad).ceil() as i32,
            (y0.max(y1) + pad).ceil() as i32,
        );
        let vx = x1 - x0;
        let vy = y1 - y0;
        let len2 = vx * vx + vy * vy;
        for y in by0..by1 {
            for x in bx0..bx1 {
                let px = x as f32 + 0.5 - x0;
                let py = y as f32 + 0.5 - y0;
                let h = if len2 > 1e-4 { ((px * vx + py * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let dx = px - vx * h;
                let dy = py - vy * h;
                let d = (dx * dx + dy * dy).sqrt();
                if d < half + 0.5 {
                    let cov = (half + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, (c.0, c.1, c.2, c.3 * cov));
                }
            }
        }
    }

    /// 高质量缩放并绘制预乘 BGRA 图标：缩小时使用精确像素足迹区域平均消除锯齿，放大时使用预乘双线性插值。
    pub fn draw_icon_bilinear(&mut self, icon: &IconPixels, cx: f32, cy: f32, target_size: f32, with_shadow: bool, shadow_offset: f32) {
        self.draw_icon_resampled(icon, cx, cy, target_size, with_shadow, shadow_offset, false);
    }

    /// 自适应背景深浅的高质量图标绘制：深色背景上遇到纯暗色透明图标（如 ChatGPT / Seer）时自动提亮保证清晰可读。
    pub fn draw_icon_adaptive(
        &mut self,
        icon: &IconPixels,
        cx: f32,
        cy: f32,
        target_size: f32,
        dark_bg: bool,
        with_shadow: bool,
        shadow_offset: f32,
    ) {
        let lift_dark = dark_bg && is_dark_monochrome_icon(icon);
        self.draw_icon_resampled(icon, cx, cy, target_size, with_shadow, shadow_offset, lift_dark);
    }

    /// 核心图标缩放渲染实现（全预乘空间，支持区域盒滤缩小、柔和双线性投影与暗色图标提亮）。
    fn draw_icon_resampled(
        &mut self,
        icon: &IconPixels,
        cx: f32,
        cy: f32,
        target_size: f32,
        with_shadow: bool,
        shadow_offset: f32,
        lift_dark: bool,
    ) {
        if icon.w == 0 || icon.h == 0 || target_size <= 1.0 {
            return;
        }
        let k = (target_size / icon.w as f32).min(target_size / icon.h as f32);
        let dw = (icon.w as f32 * k).round().max(1.0) as i32;
        let dh = (icon.h as f32 * k).round().max(1.0) as i32;
        let dx0 = (cx - dw as f32 * 0.5).round() as i32;
        let dy0 = (cy - dh as f32 * 0.5).round() as i32;

        let iw = icon.w as i32;
        let ih = icon.h as i32;
        let src = icon.px.as_slice();
        let inv_x = iw as f32 / dw as f32;
        let inv_y = ih as f32 / dh as f32;

        // 柔和双线性投影（仅用于浅色模式等需要轻微层次感的场景，避免最近邻硬边锯齿）
        if with_shadow && shadow_offset >= 1.0 {
            let so = shadow_offset.round() as i32;
            let (sx0, sy0, sx1, sy1) = self.clamp_bounds(dx0, dy0 + so, dx0 + dw, dy0 + dh + so);
            for y in sy0..sy1 {
                let fy = ((y - (dy0 + so)) as f32 + 0.5) * inv_y - 0.5;
                let y0 = (fy.floor() as i32).clamp(0, ih - 1);
                let y1 = (y0 + 1).min(ih - 1);
                let ty = (fy - y0 as f32).clamp(0.0, 1.0);
                let wy1 = (ty * 256.0).round() as u32;
                let wy0 = 256 - wy1;
                let row0 = (y0 as usize * icon.w as usize) * 4;
                let row1 = (y1 as usize * icon.w as usize) * 4;
                for x in sx0..sx1 {
                    let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
                    let x0 = (fx.floor() as i32).clamp(0, iw - 1);
                    let x1 = (x0 + 1).min(iw - 1);
                    let tx = (fx - x0 as f32).clamp(0.0, 1.0);
                    let wx1 = (tx * 256.0).round() as u32;
                    let wx0 = 256 - wx1;
                    let w00 = (wx0 * wy0) >> 8;
                    let w10 = (wx1 * wy0) >> 8;
                    let w01 = (wx0 * wy1) >> 8;
                    let w11 = (wx1 * wy1) >> 8;
                    let a = (src[row0 + x0 as usize * 4 + 3] as u32 * w00
                        + src[row0 + x1 as usize * 4 + 3] as u32 * w10
                        + src[row1 + x0 as usize * 4 + 3] as u32 * w01
                        + src[row1 + x1 as usize * 4 + 3] as u32 * w11
                        + 128)
                        >> 8;
                    if a > 12 {
                        let sa = (a * 36) >> 8; // ~14% 柔和投影
                        self.blend_premul_u8(x, y, 0, 0, 0, sa);
                    }
                }
            }
        }

        let (bx0, by0, bx1, by1) = self.clamp_bounds(dx0, dy0, dx0 + dw, dy0 + dh);
        if bx0 >= bx1 || by0 >= by1 {
            return;
        }

        // 缩小渲染（如 128px → 42px / 18px / 16px）：使用精确像素足迹区域盒滤，杜绝锯齿与线条断裂
        if inv_x > 1.05 || inv_y > 1.05 {
            for y in by0..by1 {
                let sy0 = ((y - dy0) as f32 * inv_y).clamp(0.0, ih as f32);
                let sy1 = ((y - dy0 + 1) as f32 * inv_y).clamp(0.0, ih as f32);
                let iy0 = (sy0.floor() as i32).clamp(0, ih - 1);
                let iy1 = ((sy1 - 1e-4).floor() as i32).clamp(0, ih - 1);
                for x in bx0..bx1 {
                    let sx0 = ((x - dx0) as f32 * inv_x).clamp(0.0, iw as f32);
                    let sx1 = ((x - dx0 + 1) as f32 * inv_x).clamp(0.0, iw as f32);
                    let ix0 = (sx0.floor() as i32).clamp(0, iw - 1);
                    let ix1 = ((sx1 - 1e-4).floor() as i32).clamp(0, iw - 1);
                    let (mut sb, mut sg, mut sr, mut sa, mut wsum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
                    for iy in iy0..=iy1 {
                        let wy = ((iy as f32 + 1.0).min(sy1) - (iy as f32).max(sy0)).max(0.0);
                        if wy <= 0.0 {
                            continue;
                        }
                        let row = (iy as usize * icon.w as usize) * 4;
                        for ix in ix0..=ix1 {
                            let wx = ((ix as f32 + 1.0).min(sx1) - (ix as f32).max(sx0)).max(0.0);
                            let w = wx * wy;
                            if w <= 0.0 {
                                continue;
                            }
                            let idx = row + ix as usize * 4;
                            sb += src[idx] as f32 * w;
                            sg += src[idx + 1] as f32 * w;
                            sr += src[idx + 2] as f32 * w;
                            sa += src[idx + 3] as f32 * w;
                            wsum += w;
                        }
                    }
                    if wsum <= 1e-6 {
                        continue;
                    }
                    let inv_w = 1.0 / wsum;
                    let a = (sa * inv_w).round().clamp(0.0, 255.0) as u32;
                    if a == 0 {
                        continue;
                    }
                    let (b, g, r) = if lift_dark {
                        // 将深色单色透明图标提亮为柔和浅白，保留完整抗锯齿边缘
                        let af = a as f32;
                        let lb = ((sb * inv_w) / af).clamp(0.0, 1.0);
                        let lg = ((sg * inv_w) / af).clamp(0.0, 1.0);
                        let lr = ((sr * inv_w) / af).clamp(0.0, 1.0);
                        let nb = ((0.96 - 0.45 * lb) * af).round().clamp(0.0, af) as u32;
                        let ng = ((0.95 - 0.45 * lg) * af).round().clamp(0.0, af) as u32;
                        let nr = ((0.95 - 0.45 * lr) * af).round().clamp(0.0, af) as u32;
                        (nb, ng, nr)
                    } else {
                        (
                            (sb * inv_w).round().clamp(0.0, a as f32) as u32,
                            (sg * inv_w).round().clamp(0.0, a as f32) as u32,
                            (sr * inv_w).round().clamp(0.0, a as f32) as u32,
                        )
                    };
                    self.blend_premul_u8(x, y, b, g, r, a);
                }
            }
            return;
        }

        // 放大或 1:1：使用预乘双线性插值
        for y in by0..by1 {
            let fy = ((y - dy0) as f32 + 0.5) * inv_y - 0.5;
            let y0 = (fy.floor() as i32).clamp(0, ih - 1);
            let y1 = (y0 + 1).min(ih - 1);
            let ty = (fy - y0 as f32).clamp(0.0, 1.0);
            let wy1 = (ty * 256.0).round() as u32;
            let wy0 = 256 - wy1;
            let row0 = (y0 as usize * icon.w as usize) * 4;
            let row1 = (y1 as usize * icon.w as usize) * 4;
            for x in bx0..bx1 {
                let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
                let x0 = (fx.floor() as i32).clamp(0, iw - 1);
                let x1 = (x0 + 1).min(iw - 1);
                let tx = (fx - x0 as f32).clamp(0.0, 1.0);
                let wx1 = (tx * 256.0).round() as u32;
                let wx0 = 256 - wx1;

                let i00 = row0 + x0 as usize * 4;
                let i10 = row0 + x1 as usize * 4;
                let i01 = row1 + x0 as usize * 4;
                let i11 = row1 + x1 as usize * 4;

                let w00 = (wx0 * wy0) >> 8;
                let w10 = (wx1 * wy0) >> 8;
                let w01 = (wx0 * wy1) >> 8;
                let w11 = (wx1 * wy1) >> 8;

                let a = ((src[i00 + 3] as u32 * w00 + src[i10 + 3] as u32 * w10 + src[i01 + 3] as u32 * w01 + src[i11 + 3] as u32 * w11 + 128) >> 8).min(255);
                if a == 0 {
                    continue;
                }
                let raw_b = ((src[i00] as u32 * w00 + src[i10] as u32 * w10 + src[i01] as u32 * w01 + src[i11] as u32 * w11 + 128) >> 8).min(a);
                let raw_g = ((src[i00 + 1] as u32 * w00 + src[i10 + 1] as u32 * w10 + src[i01 + 1] as u32 * w01 + src[i11 + 1] as u32 * w11 + 128) >> 8).min(a);
                let raw_r = ((src[i00 + 2] as u32 * w00 + src[i10 + 2] as u32 * w10 + src[i01 + 2] as u32 * w01 + src[i11 + 2] as u32 * w11 + 128) >> 8).min(a);
                let (b, g, r) = if lift_dark {
                    let af = a as f32;
                    let nb = ((0.96 - 0.45 * (raw_b as f32 / af)) * af).round().clamp(0.0, af) as u32;
                    let ng = ((0.95 - 0.45 * (raw_g as f32 / af)) * af).round().clamp(0.0, af) as u32;
                    let nr = ((0.95 - 0.45 * (raw_r as f32 / af)) * af).round().clamp(0.0, af) as u32;
                    (nb, ng, nr)
                } else {
                    (raw_b, raw_g, raw_r)
                };
                self.blend_premul_u8(x, y, b, g, r, a);
            }
        }
    }

    /// 1:1 直接贴图（用于预烘焙含阴影图标，避免逐帧双线性重采样，保障左右滑页丝滑 60+ FPS）。
    pub fn draw_icon_1to1(&mut self, sprite: &IconPixels, cx: f32, cy: f32, icon_h: i32) {
        if sprite.w == 0 || sprite.h == 0 {
            return;
        }
        let dx0 = (cx - sprite.w as f32 * 0.5).round() as i32;
        let dy0 = (cy - icon_h as f32 * 0.5).round() as i32;
        let (bx0, by0, bx1, by1) = self.clamp_bounds(dx0, dy0, dx0 + sprite.w as i32, dy0 + sprite.h as i32);
        if bx0 >= bx1 || by0 >= by1 {
            return;
        }
        let src = sprite.px.as_slice();
        let sw = sprite.w as usize;
        for y in by0..by1 {
            let sy = (y - dy0) as usize;
            let row = sy * sw;
            for x in bx0..bx1 {
                let sx = (x - dx0) as usize;
                let si = (row + sx) * 4;
                let a = src[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                self.blend_premul_over(x, y, src[si] as u32, src[si + 1] as u32, src[si + 2] as u32, a);
            }
        }
    }
}

/// 检测已缩放到目标正方形区域的图标是否为卡片型（四边中段呈直线分布），避免误裁圆形或异形图标。
fn is_card_shape(px: &[u8], w: usize, h: usize) -> Option<(f32, f32, f32, f32)> {
    let at_a = |x: i32, y: i32| -> u8 {
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            return 0;
        }
        px[(y as usize * w + x as usize) * 4 + 3]
    };
    let mut min_y = h as i32;
    let mut max_y = -1i32;
    'top: for y in 0..h as i32 {
        let row = y as usize * w * 4;
        for x in 0..w {
            if px[row + x * 4 + 3] > 120 {
                min_y = y;
                break 'top;
            }
        }
    }
    if min_y >= h as i32 {
        return None;
    }
    'bot: for y in (min_y..h as i32).rev() {
        let row = y as usize * w * 4;
        for x in 0..w {
            if px[row + x * 4 + 3] > 120 {
                max_y = y;
                break 'bot;
            }
        }
    }
    let mut min_x = w as i32;
    let mut max_x = -1i32;
    for y in min_y..=max_y {
        let row = y as usize * w * 4;
        for x in 0..min_x {
            if px[row + x as usize * 4 + 3] > 120 {
                min_x = x;
                break;
            }
        }
        for x in (max_x + 1..w as i32).rev() {
            if px[row + x as usize * 4 + 3] > 120 {
                max_x = x;
                break;
            }
        }
    }
    if max_x - min_x < 12 || max_y - min_y < 12 {
        return None;
    }
    let cw = (max_x - min_x + 1) as f32;
    let ch = (max_y - min_y + 1) as f32;
    // 检测上下左右四条边的中间 24% 区间是否平坦贴边不透明（正圆形在左右偏移 12% 时边缘已向内回落，卡片则始终满透明阈值）
    let ox = ((cw * 0.14).round() as i32).max(2);
    let oy = ((ch * 0.14).round() as i32).max(2);
    let cx = (min_x + max_x) / 2;
    let cy = (min_y + max_y) / 2;
    let top_flat = at_a(cx - ox, min_y + 2) > 180 && at_a(cx, min_y + 2) > 180 && at_a(cx + ox, min_y + 2) > 180;
    let bot_flat = at_a(cx - ox, max_y - 2) > 180 && at_a(cx, max_y - 2) > 180 && at_a(cx + ox, max_y - 2) > 180;
    let left_flat = at_a(min_x + 2, cy - oy) > 180 && at_a(min_x + 2, cy) > 180 && at_a(min_x + 2, cy + oy) > 180;
    let right_flat = at_a(max_x - 2, cy - oy) > 180 && at_a(max_x - 2, cy) > 180 && at_a(max_x - 2, cy + oy) > 180;
    let flat_sides = [top_flat, bot_flat, left_flat, right_flat].iter().filter(|&&b| b).count();
    if flat_sides >= 3 {
        Some((min_x as f32, min_y as f32, cw, ch))
    } else {
        None
    }
}

/// 按给定圆角弧度对正方形前景层（卡片型图标）就地执行抗锯齿圆角遮罩裁切（仅计算四角区域）。
fn apply_card_roundness(fg: &mut [u8], w: usize, h: usize, round_ratio: f32) {
    let Some((bx, by, cw, ch)) = is_card_shape(fg, w, h) else {
        return;
    };
    let rad = (cw.min(ch) * round_ratio).max(2.5);
    let rcx = bx + cw * 0.5;
    let rcy = by + ch * 0.5;
    let half_w = (cw * 0.5 - rad).max(0.0);
    let half_h = (ch * 0.5 - rad).max(0.0);
    let pad = rad.ceil() as i32 + 2;
    let inner_l = (bx as i32 + pad).max(0);
    let inner_r = ((bx + cw).ceil() as i32 - pad).min(w as i32);
    let inner_t = (by as i32 + pad).max(0);
    let inner_b = ((by + ch).ceil() as i32 - pad).min(h as i32);
    for y in 0..h as i32 {
        let in_mid_y = inner_t < inner_b && y >= inner_t && y < inner_b;
        let fy = y as f32 + 0.5;
        for x in 0..w as i32 {
            // 卡片中心十字区一定在圆角内部，跳过无谓的逐像素 SDF 开方运算
            if in_mid_y && inner_l < inner_r && x >= inner_l && x < inner_r {
                continue;
            }
            if (x as f32 + 0.5 >= bx + 1.0 && (x as f32 + 0.5) < bx + cw - 1.0)
                && (fy >= by + 1.0 && fy < by + ch - 1.0)
                && ((x >= inner_l && x < inner_r) || (y >= inner_t && y < inner_b))
            {
                continue;
            }
            let fx = x as f32 + 0.5;
            let qx = (fx - rcx).abs() - half_w;
            let qy = (fy - rcy).abs() - half_h;
            let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad;
            if sd > -0.5 {
                let cov = (0.5 - sd).clamp(0.0, 1.0);
                let oi = (y as usize * w + x as usize) * 4;
                if cov <= 0.0 {
                    fg[oi..oi + 4].fill(0);
                } else if cov < 1.0 {
                    let c256 = (cov * 256.0).round() as u32;
                    fg[oi] = ((fg[oi] as u32 * c256) >> 8) as u8;
                    fg[oi + 1] = ((fg[oi + 1] as u32 * c256) >> 8) as u8;
                    fg[oi + 2] = ((fg[oi + 2] as u32 * c256) >> 8) as u8;
                    fg[oi + 3] = ((fg[oi + 3] as u32 * c256) >> 8) as u8;
                }
            }
        }
    }
}

/// 生成按指定圆润度实时裁切后的图标副本（用于悬停放大等直接双线性绘制场景）。
pub fn clip_icon_roundness(src: &IconPixels, round_ratio: f32) -> IconPixels {
    if round_ratio <= 0.125 || is_card_shape(&src.px, src.w as usize, src.h as usize).is_none() {
        return src.clone();
    }
    let mut buf = src.px.as_ref().clone();
    apply_card_roundness(&mut buf, src.w as usize, src.h as usize, round_ratio);
    IconPixels {
        w: src.w,
        h: src.h,
        px: std::sync::Arc::new(buf),
    }
}

/// 将源图标一次性缩放到目标尺寸、按配置圆润度裁切并烘焙柔和投影，供主网格高速 1:1 渲染使用。
pub fn bake_icon_sprite(src: &IconPixels, target_size: i32, shadow_offset: i32, round_ratio: f32) -> IconPixels {
    let sz = target_size.max(8) as u32;
    let so = shadow_offset.max(0) as u32;
    let bw = sz;
    let bh = sz + so + 2;
    let mut fg = vec![0u8; (sz * sz * 4) as usize];
    let iw = src.w as i32;
    let ih = src.h as i32;
    let sdata = src.px.as_slice();
    let k = (sz as f32 / src.w.max(1) as f32).min(sz as f32 / src.h.max(1) as f32);
    let dw = (src.w as f32 * k).round().max(1.0) as i32;
    let dh = (src.h as f32 * k).round().max(1.0) as i32;
    let dx0 = (sz as i32 - dw) / 2;
    let dy0 = (sz as i32 - dh) / 2;
    let inv_x = iw as f32 / dw as f32;
    let inv_y = ih as f32 / dh as f32;

    // 1. 双线性采样源图标到前景正方形图层
    for y in dy0.max(0)..(dy0 + dh).min(sz as i32) {
        let fy = ((y - dy0) as f32 + 0.5) * inv_y - 0.5;
        let y0 = (fy.floor() as i32).clamp(0, ih - 1);
        let y1 = (y0 + 1).min(ih - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        let wy1 = (ty * 256.0).round() as u32;
        let wy0 = 256 - wy1;
        let row0 = (y0 as usize * src.w as usize) * 4;
        let row1 = (y1 as usize * src.w as usize) * 4;
        for x in dx0.max(0)..(dx0 + dw).min(sz as i32) {
            let fx = ((x - dx0) as f32 + 0.5) * inv_x - 0.5;
            let x0 = (fx.floor() as i32).clamp(0, iw - 1);
            let x1 = (x0 + 1).min(iw - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let wx1 = (tx * 256.0).round() as u32;
            let wx0 = 256 - wx1;

            let i00 = row0 + x0 as usize * 4;
            let i10 = row0 + x1 as usize * 4;
            let i01 = row1 + x0 as usize * 4;
            let i11 = row1 + x1 as usize * 4;
            let w00 = (wx0 * wy0) >> 8;
            let w10 = (wx1 * wy0) >> 8;
            let w01 = (wx0 * wy1) >> 8;
            let w11 = (wx1 * wy1) >> 8;

            let oi = (y as usize * sz as usize + x as usize) * 4;
            fg[oi] = ((sdata[i00] as u32 * w00 + sdata[i10] as u32 * w10 + sdata[i01] as u32 * w01 + sdata[i11] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 1] = ((sdata[i00 + 1] as u32 * w00 + sdata[i10 + 1] as u32 * w10 + sdata[i01 + 1] as u32 * w01 + sdata[i11 + 1] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 2] = ((sdata[i00 + 2] as u32 * w00 + sdata[i10 + 2] as u32 * w10 + sdata[i01 + 2] as u32 * w01 + sdata[i11 + 2] as u32 * w11 + 128) >> 8).min(255) as u8;
            fg[oi + 3] = ((sdata[i00 + 3] as u32 * w00 + sdata[i10 + 3] as u32 * w10 + sdata[i01 + 3] as u32 * w01 + sdata[i11 + 3] as u32 * w11 + 128) >> 8).min(255) as u8;
        }
    }

    // 2. 卡片型图标按当前选定的圆角圆润度实时抗锯齿裁切四角
    apply_card_roundness(&mut fg, sz as usize, sz as usize, round_ratio);

    // 3. 基于裁切后的前景 alpha 生成同步轮廓的柔和黑影并合成前景
    let mut buf = vec![0u8; (bw * bh * 4) as usize];
    if so >= 1 {
        let so_i = so as i32;
        for y in 0..sz as i32 {
            let dy = y + so_i;
            if dy < 0 || dy >= bh as i32 {
                continue;
            }
            for x in 0..sz as i32 {
                let si = (y as usize * sz as usize + x as usize) * 4;
                let a = fg[si + 3] as u32;
                if a > 16 {
                    let di = (dy as usize * bw as usize + x as usize) * 4;
                    buf[di + 3] = ((a * 64) >> 8) as u8;
                }
            }
        }
    }

    for y in 0..sz as usize {
        for x in 0..sz as usize {
            let i = (y * sz as usize + x) * 4;
            let a = fg[i + 3] as u32;
            if a == 0 {
                continue;
            }
            let da = buf[i + 3] as u32;
            let ia = 255 - a;
            buf[i] = fg[i];
            buf[i + 1] = fg[i + 1];
            buf[i + 2] = fg[i + 2];
            buf[i + 3] = (a + (da * ia + 127) / 255).min(255) as u8;
        }
    }

    IconPixels {
        w: bw,
        h: bh,
        px: std::sync::Arc::new(buf),
    }
}

/// 在 GDI DC 上绘制单行文字（支持左/中/右对齐与可选投影）。
pub fn draw_text(
    dc: HDC,
    s: &str,
    font: HFONT,
    r: &RECT,
    c: Rgba,
    align: TextAlign,
    _ellipsis: bool,
    shadow: bool,
) {
    if s.is_empty() {
        return;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let align_flag = match align {
        TextAlign::Left => 0x0,
        TextAlign::Center => 0x1,
        TextAlign::Right => 0x2,
    };
    let fmt = DRAW_TEXT_FORMAT(
        0x20 // DT_SINGLELINE
            | 0x4 // DT_VCENTER
            | 0x800 // DT_NOPREFIX
            | align_flag,
    );
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        if shadow {
            let mut sr = RECT {
                left: r.left,
                top: r.top + 1,
                right: r.right,
                bottom: r.bottom + 1,
            };
            SetTextColor(dc, COLORREF(0x0000_0000));
            let _ = DrawTextW(dc, &mut w, &mut sr, fmt);
        }
        let mut rc = *r;
        SetTextColor(dc, to_colorref(c));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
}

/// 在 GDI DC 上测量单行文字像素宽度与高度。
pub fn measure_text(dc: HDC, s: &str, font: HFONT) -> (i32, i32) {
    if s.is_empty() {
        return (0, 0);
    }
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz = SIZE::default();
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
    }
    (sz.cx, sz.cy)
}

/// 在 GDI DC 上测量多行自动换行文字的实际高度。
pub fn measure_wrapped_text_height(dc: HDC, s: &str, font: HFONT, max_w: i32) -> i32 {
    if s.is_empty() || max_w <= 0 {
        return 0;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: max_w.max(1),
        bottom: 0,
    };
    let fmt = DRAW_TEXT_FORMAT(0x10 | 0x400 | 0x800 | 0x2000); // DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX | DT_EDITCONTROL
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
    (rc.bottom - rc.top).max(1)
}

/// 在 GDI DC 上绘制支持自动换行的多行文字（顶部左对齐）。
pub fn draw_wrapped_text(dc: HDC, s: &str, font: HFONT, r: &RECT, c: Rgba) {
    if s.is_empty() || r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let fmt = DRAW_TEXT_FORMAT(0x10 | 0x800 | 0x2000); // DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        let mut rc = *r;
        SetTextColor(dc, to_colorref(c));
        let _ = DrawTextW(dc, &mut w, &mut rc, fmt);
        SelectObject(dc, old);
    }
}

/// 基于 Direct2D / DirectWrite 的彩色文本渲染器。
/// GDI `DrawTextW` 不支持彩色字体表（COLR），会回退为黑白线框字形；
/// 启用 `D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT` 后可在现有 DIB DC 上直接渲染原生彩色 Emoji。
pub struct ColorTextRenderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    cached_fmt: Option<(i32, IDWriteTextFormat)>,
    cached_wrap_fmt: Option<(i32, IDWriteTextFormat)>,
}

impl ColorTextRenderer {
    /// 创建可绑定 GDI HDC 的 Direct2D 渲染目标与 DirectWrite 文字工厂。
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let (rt, brush) = Self::create_target_and_brush(&d2d)?;
            Ok(Self {
                d2d,
                dwrite,
                rt,
                brush,
                cached_fmt: None,
                cached_wrap_fmt: None,
            })
        }
    }

    unsafe fn create_target_and_brush(
        d2d: &ID2D1Factory,
    ) -> windows::core::Result<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)> {
        // 使用 96 DPI 与 DIB 像素坐标 1:1 映射，alphaMode 设为 IGNORE 以直接合成到不透明窗口表面。
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let brush = rt.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            None,
        )?;
        Ok((rt, brush))
    }

    /// 缓存指定像素字号的单行居中格式（不添加省略号）。
    unsafe fn text_format(&mut self, px: i32) -> windows::core::Result<&IDWriteTextFormat> {
        let px = px.max(9);
        let needs_rebuild = !matches!(self.cached_fmt, Some((cur, _)) if cur == px);
        if needs_rebuild {
            let fmt = self.dwrite.CreateTextFormat(
                w!("Microsoft YaHei UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px as f32,
                w!("zh-CN"),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            self.cached_fmt = Some((px, fmt));
        }
        Ok(&self.cached_fmt.as_ref().expect("format cached").1)
    }

    /// 缓存指定像素字号的自动换行格式（顶部对齐，防止长更新条目被截断）。
    unsafe fn wrapped_format(&mut self, px: i32) -> windows::core::Result<&IDWriteTextFormat> {
        let px = px.max(9);
        let needs_rebuild = !matches!(self.cached_wrap_fmt, Some((cur, _)) if cur == px);
        if needs_rebuild {
            let fmt = self.dwrite.CreateTextFormat(
                w!("Microsoft YaHei UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px as f32,
                w!("zh-CN"),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            self.cached_wrap_fmt = Some((px, fmt));
        }
        Ok(&self.cached_wrap_fmt.as_ref().expect("wrap format cached").1)
    }

    /// 测量多行自动换行文本在给定最大宽度下的像素高度，用于精准计算折叠日志展开高度。
    pub fn measure_wrapped_height(&mut self, px: i32, max_w: i32, text: &str) -> Option<i32> {
        if text.is_empty() || max_w <= 0 {
            return Some(0);
        }
        unsafe {
            let dwrite = self.dwrite.clone();
            let fmt = self.wrapped_format(px).ok()?.clone();
            let utf16: Vec<u16> = text.encode_utf16().collect();
            let layout = dwrite
                .CreateTextLayout(&utf16, &fmt, max_w as f32, 10_000.0)
                .ok()?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics).ok()?;
            Some(metrics.height.ceil() as i32)
        }
    }

    fn draw_batch(&mut self, dc: HDC, clip: &RECT, px: i32, c: Rgba, items: &[(RECT, &str)], wrap: bool) -> bool {
        if items.is_empty() || clip.right <= clip.left || clip.bottom <= clip.top {
            return true;
        }
        unsafe {
            // 先刷新前置 GDI/DIB 绘制，确保 BindDC 读取到最新背景像素。
            let _ = GdiFlush();
            if self.rt.BindDC(dc, clip).is_err() {
                if let Ok((rt, brush)) = Self::create_target_and_brush(&self.d2d) {
                    self.rt = rt;
                    self.brush = brush;
                }
                if self.rt.BindDC(dc, clip).is_err() {
                    return false;
                }
            }
            let dwrite = self.dwrite.clone();
            let fmt = if wrap {
                self.wrapped_format(px).cloned()
            } else {
                self.text_format(px).cloned()
            };
            let Ok(fmt) = fmt else {
                return false;
            };
            self.brush.SetColor(&D2D1_COLOR_F {
                r: c.0,
                g: c.1,
                b: c.2,
                a: c.3,
            });
            let opts = D2D1_DRAW_TEXT_OPTIONS(
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT.0 | D2D1_DRAW_TEXT_OPTIONS_CLIP.0,
            );
            self.rt.BeginDraw();
            for (rc, text) in items {
                if text.is_empty() || rc.right <= rc.left || rc.bottom <= rc.top {
                    continue;
                }
                let utf16: Vec<u16> = text.encode_utf16().collect();
                let max_w = (rc.right - rc.left) as f32;
                let max_h = (rc.bottom - rc.top) as f32;
                if let Ok(layout) = dwrite.CreateTextLayout(&utf16, &fmt, max_w, max_h) {
                    // BindDC 将渲染目标原点映射到 clip 左上角，需把坐标平移到 clip 局部空间。
                    let origin = D2D_POINT_2F {
                        x: (rc.left - clip.left) as f32,
                        y: (rc.top - clip.top) as f32,
                    };
                    self.rt.DrawTextLayout(origin, &layout, &self.brush, opts);
                }
            }
            if self.rt.EndDraw(None, None).is_err() {
                if let Ok((rt, brush)) = Self::create_target_and_brush(&self.d2d) {
                    self.rt = rt;
                    self.brush = brush;
                }
                return false;
            }
        }
        true
    }

    /// 在裁剪区 `clip` 内批量绘制带彩色 Emoji 的单行文本；若渲染目标失效则返回 false 供调用方回退 GDI。
    #[allow(dead_code)]
    pub fn draw_lines(
        &mut self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        c: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
        self.draw_batch(dc, clip, px, c, items, false)
    }

    /// 在裁剪区 `clip` 内批量绘制支持自动换行与彩色 Emoji 的多行文本。
    pub fn draw_wrapped_lines(
        &mut self,
        dc: HDC,
        clip: &RECT,
        px: i32,
        c: Rgba,
        items: &[(RECT, &str)],
    ) -> bool {
        self.draw_batch(dc, clip, px, c, items, true)
    }
}

/// 现代复选框绘制（支持选中、半选、未选及 Hover/Active 状态）。
pub fn draw_checkbox(
    surface: &mut Surface,
    r: &RECT,
    state: CheckState,
    hover: bool,
    pressed: bool,
    scale: f32,
) {
    let rad = 5.0 * scale;
    let active_blue = if pressed {
        rgb(0x0958D9, 1.0)
    } else if hover {
        rgb(0x4096FF, 1.0)
    } else {
        rgb(0x1677FF, 1.0)
    };
    match state {
        CheckState::Unchecked => {
            let bg = if pressed {
                rgb(0xE5E7EB, 1.0)
            } else if hover {
                rgb(0xF3F4F6, 1.0)
            } else {
                rgb(0xFFFFFF, 1.0)
            };
            let border = if hover || pressed {
                rgb(0x1677FF, 1.0)
            } else {
                rgb(0xCBD5E1, 1.0)
            };
            surface.fill_rrect(r, rad, bg);
            surface.stroke_rrect(r, rad, (1.4 * scale).max(1.0), border);
        }
        CheckState::Checked => {
            surface.fill_rrect(r, rad, active_blue);
            let l = r.left as f32;
            let t = r.top as f32;
            let w = (r.right - r.left) as f32;
            let h = (r.bottom - r.top) as f32;
            let stroke = (1.8 * scale).max(1.2);
            let white = rgb(0xFFFFFF, 1.0);
            surface.thick_line(l + w * 0.24, t + h * 0.52, l + w * 0.43, t + h * 0.71, stroke, white);
            surface.thick_line(l + w * 0.43, t + h * 0.71, l + w * 0.76, t + h * 0.30, stroke, white);
        }
        CheckState::Indeterminate => {
            surface.fill_rrect(r, rad, active_blue);
            let l = r.left as f32;
            let t = r.top as f32;
            let w = (r.right - r.left) as f32;
            let h = (r.bottom - r.top) as f32;
            surface.thick_line(
                l + w * 0.26,
                t + h * 0.50,
                l + w * 0.74,
                t + h * 0.50,
                (1.8 * scale).max(1.2),
                rgb(0xFFFFFF, 1.0),
            );
        }
    }
}

/// 现代胶囊拨动开关（Toggle Switch）。
pub fn draw_toggle(surface: &mut Surface, r: &RECT, on: bool, hover: bool, pressed: bool, scale: f32) {
    let h = (r.bottom - r.top) as f32;
    let rad = h * 0.5;
    let track = if on {
        if pressed {
            rgb(0x0958D9, 1.0)
        } else if hover {
            rgb(0x4096FF, 1.0)
        } else {
            rgb(0x1677FF, 1.0)
        }
    } else if pressed {
        rgb(0x9CA3AF, 1.0)
    } else if hover {
        rgb(0xB8C0CC, 1.0)
    } else {
        rgb(0xCBD5E1, 1.0)
    };
    surface.fill_rrect(r, rad, track);
    let pad = 2.5 * scale;
    let kr = (h * 0.5 - pad).max(2.0);
    let cx = if on {
        r.right as f32 - pad - kr
    } else {
        r.left as f32 + pad + kr
    };
    let cy = (r.top + r.bottom) as f32 * 0.5;
    surface.fill_circle(cx, cy + 0.6 * scale, kr, (0.0, 0.0, 0.0, 0.14));
    surface.fill_circle(cx, cy, kr, rgb(0xFFFFFF, 1.0));
}

/// 现代按钮（支持主色、描边、幽灵态及 Hover/Active 反馈）。
pub fn draw_button(
    surface: &mut Surface,
    dc: HDC,
    r: &RECT,
    label: &str,
    font: HFONT,
    style: BtnStyle,
    hover: bool,
    pressed: bool,
    scale: f32,
) {
    let rad = 8.0 * scale;
    let (bg, border, fg) = match style {
        BtnStyle::Primary => {
            let b = if pressed {
                rgb(0x0958D9, 1.0)
            } else if hover {
                rgb(0x3B82F6, 1.0)
            } else {
                rgb(0x1677FF, 1.0)
            };
            (b, None, rgb(0xFFFFFF, 1.0))
        }
        BtnStyle::Secondary => {
            let b = if pressed {
                rgb(0xE5E7EB, 1.0)
            } else if hover {
                rgb(0xF3F4F6, 1.0)
            } else {
                rgb(0xFFFFFF, 1.0)
            };
            let br = if hover || pressed {
                rgb(0xCBD5E1, 1.0)
            } else {
                rgb(0xE2E8F0, 1.0)
            };
            (b, Some(br), rgb(0x1F2937, 1.0))
        }
        BtnStyle::Ghost => {
            let b = if pressed {
                rgb(0xE2E8F0, 1.0)
            } else if hover {
                rgb(0xF1F5F9, 1.0)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            (b, None, rgb(0x334155, 1.0))
        }
        BtnStyle::DangerGhost => {
            let b = if pressed {
                rgb(0xFEE2E2, 1.0)
            } else if hover {
                rgb(0xFEF2F2, 1.0)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            let fg = if hover || pressed { rgb(0xDC2626, 1.0) } else { rgb(0x64748B, 1.0) };
            (b, None, fg)
        }
    };
    if bg.3 > 0.001 {
        surface.fill_rrect(r, rad, bg);
    }
    if let Some(br) = border {
        surface.stroke_rrect(r, rad, (1.0 * scale).max(1.0), br);
    }
    draw_text(dc, label, font, r, fg, TextAlign::Center, true, false);
}

/// 现代细长圆角滚动条。
pub fn draw_scrollbar(
    surface: &mut Surface,
    track: &RECT,
    total: usize,
    visible: usize,
    offset: usize,
    hover: bool,
    scale: f32,
) {
    if total <= visible || visible == 0 {
        return;
    }
    let th = (track.bottom - track.top) as f32;
    let min_thumb = 24.0 * scale;
    let thumb_h = ((visible as f32 / total as f32) * th).max(min_thumb).min(th);
    let max_off = (total - visible) as f32;
    let frac = (offset as f32 / max_off).clamp(0.0, 1.0);
    let top = track.top as f32 + frac * (th - thumb_h);
    let r = RECT {
        left: track.left,
        top: top.round() as i32,
        right: track.right,
        bottom: (top + thumb_h).round() as i32,
    };
    let w = (r.right - r.left) as f32;
    let c = if hover { rgb(0x94A3B8, 0.9) } else { rgb(0xCBD5E1, 0.8) };
    surface.fill_rrect(&r, w * 0.5, c);
}

/// 绘制自定义圆角深色磨砂 Tooltip（自动约束在窗口边界内，替代原生 title）。
pub fn draw_tooltip(
    surface: &mut Surface,
    dc: HDC,
    font: HFONT,
    anchor: &RECT,
    text: &str,
    bounds_w: i32,
    bounds_h: i32,
    scale: f32,
) {
    if text.is_empty() {
        return;
    }
    let (tw, th) = measure_text(dc, text, font);
    let pad_x = (10.0 * scale).round() as i32;
    let pad_y = (5.0 * scale).round() as i32;
    let max_w = (bounds_w - (24.0 * scale) as i32).max((120.0 * scale) as i32);
    let box_w = (tw + pad_x * 2).min(max_w);
    let box_h = th + pad_y * 2;
    let margin = (8.0 * scale).round() as i32;

    let cx = (anchor.left + anchor.right) / 2;
    let mut left = (cx - box_w / 2).clamp(margin, (bounds_w - box_w - margin).max(margin));
    let gap = (6.0 * scale).round() as i32;
    // 优先放在锚点上方，空间不足时放到下方
    let top = if anchor.top - box_h - gap >= margin {
        anchor.top - box_h - gap
    } else {
        (anchor.bottom + gap).min((bounds_h - box_h - margin).max(margin))
    };
    if left < margin {
        left = margin;
    }
    let rect = RECT {
        left,
        top,
        right: left + box_w,
        bottom: top + box_h,
    };
    let shadow_rect = RECT {
        left: rect.left,
        top: rect.top + (2.0 * scale).round() as i32,
        right: rect.right,
        bottom: rect.bottom + (3.0 * scale).round() as i32,
    };
    surface.fill_rrect(&shadow_rect, 7.0 * scale, (0.0, 0.0, 0.0, 0.25));
    surface.fill_rrect(&rect, 7.0 * scale, rgb(0x181C24, 0.94));
    surface.stroke_rrect(&rect, 7.0 * scale, (1.0 * scale).max(1.0), (1.0, 1.0, 1.0, 0.16));

    let text_rc = RECT {
        left: rect.left + pad_x,
        top: rect.top,
        right: rect.right - pad_x,
        bottom: rect.bottom,
    };
    draw_text(dc, text, font, &text_rc, rgb(0xF8FAFC, 0.98), TextAlign::Center, true, false);
}
/// 将 GDI 文字按灰度遮罩合成到全透明/DWM 毛玻璃 Surface，保留正确预乘 Alpha（支持对齐与可选阴影）。
pub fn draw_transparent_text_aligned(
    surface: &mut Surface,
    scratch: &mut DibTarget,
    text: &str,
    font: HFONT,
    rect: &RECT,
    col: Rgba,
    align: TextAlign,
    with_shadow: bool,
    scale: f32,
) {
    if text.is_empty() {
        return;
    }
    let tw = (rect.right - rect.left).max(1);
    let th = (rect.bottom - rect.top).max(1);
    if !scratch.ensure_at_least(tw, th) {
        return;
    }
    if let Some(mut ssf) = scratch.surface() {
        ssf.clear_rect_black(tw, th);
    }
    let Some(sdc) = scratch.mem_dc else { return };
    let local_rc = RECT {
        left: 0,
        top: 0,
        right: tw,
        bottom: th,
    };
    draw_text(sdc, text, font, &local_rc, rgb(0xFFFFFF, 1.0), align, true, false);
    // 刷新 GDI 绘制队列，确保直接读取 DIB 像素前文字已写入内存
    unsafe {
        let _ = GdiFlush();
    }
    if scratch.ptr.is_null() {
        return;
    }
    let src_stride = scratch.w as usize;
    let src_slice = unsafe {
        std::slice::from_raw_parts(scratch.ptr as *const u32, src_stride * scratch.h as usize)
    };
    let sdy = (1.0 * scale).round().max(1.0) as i32;
    let cr_f = col.0.clamp(0.0, 1.0);
    let cg_f = col.1.clamp(0.0, 1.0);
    let cb_f = col.2.clamp(0.0, 1.0);
    let ca_f = col.3.clamp(0.0, 1.0);

    if with_shadow {
        // 先叠加轻微下沉投影，确保在极暗或极亮壁纸上都能锐利识别
        for ly in 0..th {
            let row = ly as usize * src_stride;
            for lx in 0..tw {
                let mask = (src_slice[row + lx as usize] >> 8) & 0xFF;
                if mask > 12 {
                    let sa = (mask * 72) >> 8;
                    surface.blend_premul_over(rect.left + lx, rect.top + ly + sdy, 0, 0, 0, sa);
                }
            }
        }
    }
    for ly in 0..th {
        let row = ly as usize * src_stride;
        for lx in 0..tw {
            let mask = (src_slice[row + lx as usize] >> 8) & 0xFF;
            if mask == 0 {
                continue;
            }
            let a = ((mask as f32 * ca_f).round() as u32).min(255);
            if a == 0 {
                continue;
            }
            let r = ((cr_f * a as f32).round() as u32).min(a);
            let g = ((cg_f * a as f32).round() as u32).min(a);
            let b = ((cb_f * a as f32).round() as u32).min(a);
            surface.blend_premul_over(rect.left + lx, rect.top + ly, b, g, r, a);
        }
    }
}

/// 将 GDI 文字按灰度遮罩合成到全透明分层窗口 Surface（带柔和微阴影，兼顾深浅壁纸清晰度）。
pub fn draw_transparent_text(
    surface: &mut Surface,
    scratch: &mut DibTarget,
    text: &str,
    font: HFONT,
    rect: &RECT,
    col: Rgba,
    scale: f32,
) {
    draw_transparent_text_aligned(surface, scratch, text, font, rect, col, TextAlign::Left, true, scale);
}

// ---------- macOS 风格主题色板 ----------

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub window: Rgba,
    pub sidebar: Rgba,
    pub card: Rgba,
    pub card_hover: Rgba,
    pub text: Rgba,
    pub text_secondary: Rgba,
    pub separator: Rgba,
    pub accent: Rgba,
    pub control_bg: Rgba,
    pub toggle_off: Rgba,
    pub input_bg: Rgba,
}

/// 系统强调色（DWM 取色），失败回退 macOS 蓝。
pub fn system_accent() -> (f32, f32, f32) {
    unsafe {
        let mut c = 0u32;
        let mut opaque = windows::Win32::Foundation::BOOL(0);
        // DwmGetColorizationColor 返回 0xAARRGGBB 格式
        if windows::Win32::Graphics::Dwm::DwmGetColorizationColor(&mut c, &mut opaque).is_ok() {
            let r = ((c >> 16) & 0xFF) as f32 / 255.0;
            let g = ((c >> 8) & 0xFF) as f32 / 255.0;
            let b = (c & 0xFF) as f32 / 255.0;
            // 过滤接近灰色的取色结果
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            if mx - mn > 0.08 {
                return (r, g, b);
            }
        }
    }
    (0.04, 0.52, 1.0)
}

pub fn accent_from_config(cfg: &crate::config::Appearance) -> (f32, f32, f32) {
    if cfg.accent_color != "system" {
        if let Some((r, g, b)) = crate::config::parse_hex_color(&cfg.accent_color) {
            return (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        }
    }
    system_accent()
}

pub fn palette(dark: bool, accent: (f32, f32, f32)) -> Palette {
    let (r, g, b) = accent;
    if dark {
        Palette {
            window: rgb(0x1E1E20, 1.0),
            sidebar: rgb(0x2A2A2D, 1.0),
            card: rgb(0x2C2C2F, 1.0),
            card_hover: rgb(0x353539, 1.0),
            text: rgb(0xF5F5F7, 1.0),
            text_secondary: rgb(0x98989D, 1.0),
            separator: rgb(0x3A3A3C, 1.0),
            accent: (r, g, b, 1.0),
            control_bg: rgb(0x3A3A3C, 1.0),
            toggle_off: rgb(0x636366, 1.0),
            input_bg: rgb(0x1C1C1E, 1.0),
        }
    } else {
        Palette {
            window: rgb(0xF2F2F4, 1.0),
            sidebar: rgb(0xE6E6EA, 1.0),
            card: rgb(0xFFFFFF, 1.0),
            card_hover: rgb(0xF5F5F7, 1.0),
            text: rgb(0x1D1D1F, 1.0),
            text_secondary: rgb(0x6E6E73, 1.0),
            separator: rgb(0xD8D8DC, 1.0),
            accent: (r, g, b, 1.0),
            control_bg: rgb(0xE5E5EA, 1.0),
            toggle_off: rgb(0xC7C7CC, 1.0),
            input_bg: rgb(0xFFFFFF, 1.0),
        }
    }
}

// ---------- 状态/功能矢量字形 ----------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Wifi,
    WifiOff,
    Speaker,
    SpeakerMute,
    Battery,
    ChevronDown,
    ChevronLeft,
    Gear,
    TrashEmpty,
    TrashFull,
    Folder,
    ControlCenter,
    AppGrid,
    Lock,
    Moon,
    Search,
    Power,
    Sun,
}

/// 简易扫描线填充三角形（无抗锯齿，仅用于小尺寸字形）。
fn fill_triangle(sf: &mut Surface, x0: f32, y0: f32, x1: f32, y1: f32, x2: f32, y2: f32, c: Rgba) {
    let ymin = y0.min(y1).min(y2).floor().max(0.0) as i32;
    let ymax = y0.max(y1).max(y2).ceil() as i32;
    for y in ymin..=ymax {
        let fy = y as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::new();
        let edges = [(x0, y0, x1, y1), (x1, y1, x2, y2), (x2, y2, x0, y0)];
        for (ax, ay, bx, by) in edges {
            if (ay <= fy && by > fy) || (by <= fy && ay > fy) {
                xs.push(ax + (fy - ay) * (bx - ax) / (by - ay));
            }
        }
        if xs.len() >= 2 {
            let l = xs.iter().cloned().fold(f32::MAX, f32::min).round() as i32;
            let r = xs.iter().cloned().fold(f32::MIN, f32::max).round() as i32;
            for x in l..=r {
                sf.blend(x, y, c);
            }
        }
    }
}

fn arc_segments(cx: f32, cy: f32, radius: f32, a0: f32, a1: f32, steps: usize) -> Vec<(f32, f32)> {
    (0..=steps)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            (cx + a.cos() * radius, cy + a.sin() * radius)
        })
        .collect()
}

/// 以单色矢量绘制状态字形（size 为字形外接方框边长）。
pub fn draw_glyph(sf: &mut Surface, glyph: Glyph, cx: f32, cy: f32, size: f32, c: Rgba) {
    if c.3 <= 0.001 || size <= 0.0 {
        return;
    }
    let sw = (size * 0.1).max(1.2);
    match glyph {
        Glyph::Wifi | Glyph::WifiOff => {
            let base_y = cy + size * 0.32;
            for (i, rad) in [0.18, 0.38, 0.58].iter().enumerate() {
                let r = size * rad;
                let pts = arc_segments(cx, base_y, r, -3.0 * std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_4, 10);
                for w in pts.windows(2) {
                    let col = if glyph == Glyph::WifiOff && i == 2 { (c.0, c.1, c.2, c.3 * 0.35) } else { c };
                    sf.thick_line(w[0].0, w[0].1, w[1].0, w[1].1, sw, col);
                }
            }
            sf.fill_circle(cx, base_y, sw * 0.75, c);
            if glyph == Glyph::WifiOff {
                sf.thick_line(cx - size * 0.4, cy - size * 0.4, cx + size * 0.4, cy + size * 0.4, sw, c);
            }
        }
        Glyph::Speaker | Glyph::SpeakerMute => {
            let bx = cx - size * 0.28;
            sf.fill_rrect(
                &RECT {
                    left: (cx - size * 0.42) as i32,
                    top: (cy - size * 0.14) as i32,
                    right: (bx + 1.0) as i32,
                    bottom: (cy + size * 0.14) as i32,
                },
                1.0,
                c,
            );
            fill_triangle(sf, bx, cy, cx + size * 0.05, cy - size * 0.36, cx + size * 0.05, cy + size * 0.36, c);
            if glyph == Glyph::Speaker {
                for rad in [0.25, 0.42] {
                    let pts = arc_segments(cx - size * 0.05, cy, size * rad, -0.9, 0.9, 8);
                    for w in pts.windows(2) {
                        sf.thick_line(w[0].0, w[0].1, w[1].0, w[1].1, sw, c);
                    }
                }
            } else {
                sf.thick_line(cx + size * 0.16, cy - size * 0.16, cx + size * 0.42, cy + size * 0.16, sw, c);
                sf.thick_line(cx + size * 0.16, cy + size * 0.16, cx + size * 0.42, cy - size * 0.16, sw, c);
            }
        }
        Glyph::Battery => {
            let w = size * 0.9;
            let h = size * 0.45;
            let body = RECT {
                left: (cx - w * 0.5) as i32,
                top: (cy - h * 0.5) as i32,
                right: (cx + w * 0.5) as i32,
                bottom: (cy + h * 0.5) as i32,
            };
            sf.stroke_rrect(&body, h * 0.3, 1.2, (c.0, c.1, c.2, c.3 * 0.7));
            let nub = RECT {
                left: (cx + w * 0.5 + 1.0) as i32,
                top: (cy - h * 0.2) as i32,
                right: (cx + w * 0.5 + 3.5) as i32,
                bottom: (cy + h * 0.2) as i32,
            };
            sf.fill_rrect(&nub, 1.0, (c.0, c.1, c.2, c.3 * 0.7));
            let _ = &nub;
        }
        Glyph::ChevronDown => {
            sf.thick_line(cx - size * 0.28, cy - size * 0.12, cx, cy + size * 0.16, sw, c);
            sf.thick_line(cx, cy + size * 0.16, cx + size * 0.28, cy - size * 0.12, sw, c);
        }
        Glyph::ChevronLeft => {
            sf.thick_line(cx + size * 0.12, cy - size * 0.28, cx - size * 0.16, cy, sw, c);
            sf.thick_line(cx - size * 0.16, cy, cx + size * 0.12, cy + size * 0.28, sw, c);
        }
        Glyph::Gear => {
            sf.stroke_circle(cx, cy, size * 0.22, sw, c);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (dx, dy) = (a.cos(), a.sin());
                sf.thick_line(cx + dx * size * 0.3, cy + dy * size * 0.3, cx + dx * size * 0.46, cy + dy * size * 0.46, sw, c);
            }
        }
        Glyph::TrashEmpty | Glyph::TrashFull => {
            let w = size * 0.62;
            let lid = RECT {
                left: (cx - w * 0.55) as i32,
                top: (cy - size * 0.42) as i32,
                right: (cx + w * 0.55) as i32,
                bottom: (cy - size * 0.3) as i32,
            };
            sf.fill_rrect(&lid, 2.0, c);
            let body = RECT {
                left: (cx - w * 0.42) as i32,
                top: (cy - size * 0.24) as i32,
                right: (cx + w * 0.42) as i32,
                bottom: (cy + size * 0.42) as i32,
            };
            sf.stroke_rrect(&body, 3.0, sw, c);
            if glyph == Glyph::TrashFull {
                for k in [-0.22, 0.0, 0.22] {
                    sf.thick_line(cx + k * w, cy - size * 0.12, cx + k * w, cy + size * 0.3, sw * 0.8, c);
                }
            }
        }
        Glyph::Folder => {
            let tab = RECT {
                left: (cx - size * 0.42) as i32,
                top: (cy - size * 0.3) as i32,
                right: (cx - size * 0.05) as i32,
                bottom: (cy - size * 0.12) as i32,
            };
            sf.fill_rrect(&tab, 3.0, (c.0, c.1, c.2, c.3 * 0.8));
            let body = RECT {
                left: (cx - size * 0.44) as i32,
                top: (cy - size * 0.16) as i32,
                right: (cx + size * 0.44) as i32,
                bottom: (cy + size * 0.38) as i32,
            };
            sf.fill_rrect(&body, 4.0, c);
        }
        Glyph::ControlCenter => {
            // macOS 风格双胶囊拨动开关控制中心图标
            let pill_w = size * 0.88;
            let pill_h = size * 0.36;
            let r_top = RECT {
                left: (cx - pill_w * 0.5).round() as i32,
                top: (cy - size * 0.40).round() as i32,
                right: (cx + pill_w * 0.5).round() as i32,
                bottom: (cy - size * 0.40 + pill_h).round() as i32,
            };
            let r_bot = RECT {
                left: (cx - pill_w * 0.5).round() as i32,
                top: (cy + size * 0.04).round() as i32,
                right: (cx + pill_w * 0.5).round() as i32,
                bottom: (cy + size * 0.04 + pill_h).round() as i32,
            };
            sf.stroke_rrect(&r_top, pill_h * 0.5, (size * 0.085).max(1.0), c);
            sf.fill_circle(r_top.right as f32 - pill_h * 0.5, (r_top.top + r_top.bottom) as f32 * 0.5, pill_h * 0.34, c);
            sf.fill_rrect(&r_bot, pill_h * 0.5, (c.0, c.1, c.2, c.3 * 0.32));
            sf.stroke_rrect(&r_bot, pill_h * 0.5, (size * 0.085).max(1.0), c);
            sf.fill_circle(r_bot.left as f32 + pill_h * 0.5, (r_bot.top + r_bot.bottom) as f32 * 0.5, pill_h * 0.34, c);
        }
        Glyph::AppGrid => {
            for row in 0..3 {
                for col in 0..3 {
                    let x = cx + (col as f32 - 1.0) * size * 0.32;
                    let y = cy + (row as f32 - 1.0) * size * 0.32;
                    sf.fill_circle(x, y, size * 0.08, c);
                }
            }
        }
        Glyph::Lock => {
            sf.stroke_rrect(
                &RECT {
                    left: (cx - size * 0.3) as i32,
                    top: (cy - size * 0.05) as i32,
                    right: (cx + size * 0.3) as i32,
                    bottom: (cy + size * 0.4) as i32,
                },
                3.0,
                sw,
                c,
            );
            let pts = arc_segments(cx, cy - size * 0.05, size * 0.2, std::f32::consts::PI, 2.0 * std::f32::consts::PI, 10);
            for w in pts.windows(2) {
                sf.thick_line(w[0].0, w[0].1, w[1].0, w[1].1, sw, c);
            }
        }
        Glyph::Moon => {
            let outer = arc_segments(cx, cy, size * 0.38, 0.6, 5.6, 14);
            let inner = arc_segments(cx + size * 0.2, cy - size * 0.1, size * 0.3, 5.4, 0.9, 10);
            let mut pts = outer;
            pts.extend(inner);
            for w in pts.windows(2) {
                sf.thick_line(w[0].0, w[0].1, w[1].0, w[1].1, sw, c);
            }
        }
        Glyph::Search => {
            sf.stroke_circle(cx - size * 0.08, cy - size * 0.08, size * 0.26, sw, c);
            sf.thick_line(cx + size * 0.12, cy + size * 0.12, cx + size * 0.38, cy + size * 0.38, sw, c);
        }
        Glyph::Power => {
            let pts = arc_segments(cx, cy + size * 0.04, size * 0.34, -1.1, std::f32::consts::PI + 1.1, 18);
            for w in pts.windows(2) {
                sf.thick_line(w[0].0, w[0].1, w[1].0, w[1].1, sw, c);
            }
            sf.thick_line(cx, cy - size * 0.4, cx, cy - size * 0.02, sw, c);
        }
        Glyph::Sun => {
            // 太阳矢量图标：中心实心圆 + 8 条放射短光线
            sf.fill_circle(cx, cy, size * 0.21, c);
            let r0 = size * 0.31;
            let r1 = size * 0.43;
            let ray_w = (sw * 0.85).max(1.1);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (dx, dy) = (a.cos(), a.sin());
                sf.thick_line(cx + dx * r0, cy + dy * r0, cx + dx * r1, cy + dy * r1, ray_w, c);
            }
        }
    }
}

/// 绘制带实时电量填充与充电标识的 macOS 风格电池图标。
pub fn draw_battery_filled(sf: &mut Surface, cx: f32, cy: f32, size: f32, pct: u8, charging: bool, c: Rgba) {
    let w = size * 1.15;
    let h = size * 0.56;
    let body = RECT {
        left: (cx - w * 0.5).round() as i32,
        top: (cy - h * 0.5).round() as i32,
        right: (cx + w * 0.5).round() as i32,
        bottom: (cy + h * 0.5).round() as i32,
    };
    let rad = (h * 0.30).max(2.0);
    sf.stroke_rrect(&body, rad, (size * 0.08).max(1.1), (c.0, c.1, c.2, c.3 * 0.72));
    let nub = RECT {
        left: body.right + 1,
        top: (cy - h * 0.22).round() as i32,
        right: body.right + (size * 0.14).round().max(2.0) as i32,
        bottom: (cy + h * 0.22).round() as i32,
    };
    sf.fill_rrect(&nub, 1.2, (c.0, c.1, c.2, c.3 * 0.72));
    let pad = (size * 0.12).round().max(2.0) as i32;
    let inner_w = (body.right - body.left - pad * 2).max(1);
    let fill_w = ((inner_w as f32 * (pct.min(100) as f32 / 100.0)).round() as i32).max(if pct > 0 { 2 } else { 0 });
    let fill_col = if charging {
        rgb(0x30D158, 0.96)
    } else if pct <= 20 {
        rgb(0xFF453A, 0.96)
    } else {
        (c.0, c.1, c.2, c.3 * 0.92)
    };
    if fill_w > 0 {
        let fill = RECT {
            left: body.left + pad,
            top: body.top + pad,
            right: (body.left + pad + fill_w).min(body.right - pad),
            bottom: body.bottom - pad,
        };
        sf.fill_rrect(&fill, (rad - 1.2).max(1.0), fill_col);
    }
}

/// 绘制顶部栏简洁单色原创徽标（类 macOS 顶栏图标质感，自动跟随深浅前景色）。
pub fn draw_topbar_logo(sf: &mut Surface, cx: f32, cy: f32, size: f32, c: Rgba) {
    let r = RECT {
        left: (cx - size * 0.44).round() as i32,
        top: (cy - size * 0.44).round() as i32,
        right: (cx + size * 0.44).round() as i32,
        bottom: (cy + size * 0.44).round() as i32,
    };
    let sw = (size * 0.11).max(1.3);
    sf.stroke_rrect(&r, size * 0.24, sw, c);
    // 顶部栏微条
    let top_bar = RECT {
        left: r.left + (size * 0.16) as i32,
        top: r.top + (size * 0.17) as i32,
        right: r.right - (size * 0.16) as i32,
        bottom: r.top + (size * 0.27).max(2.0) as i32,
    };
    sf.fill_rrect(&top_bar, 1.5, c);
    // 底部 Dock 三点
    let dy = r.bottom as f32 - size * 0.24;
    let dr = (size * 0.08).max(1.2);
    for k in [-1.0f32, 0.0, 1.0] {
        sf.fill_circle(cx + k * size * 0.20, dy, dr, c);
    }
}

/// 绘制精致仿 macOS 风格「访达」圆角图标（原创蓝白双色极简笑脸 + 玻璃高光）。
pub fn draw_finder_icon(sf: &mut Surface, cx: f32, cy: f32, size: f32) {
    let half = size * 0.46;
    let r = RECT {
        left: (cx - half).round() as i32,
        top: (cy - half).round() as i32,
        right: (cx + half).round() as i32,
        bottom: (cy + half).round() as i32,
    };
    let w = (r.right - r.left).max(1);
    let rad = size * 0.21;
    // 柔和底部投影
    let sh = RECT { left: r.left, top: r.top + (size * 0.05) as i32, right: r.right, bottom: r.bottom + (size * 0.06) as i32 };
    sf.fill_rrect(&sh, rad, (0.0, 0.0, 0.0, 0.22));
    // 右半侧：柔和瓷白渐变
    sf.fill_rrect_vgrad(&r, rad, rgb(0xF6F8FC, 1.0), rgb(0xDCE4F0, 1.0));
    // 左半侧：经典海洋蓝渐变（带中线微妙弧切）
    let prev_clip = sf.clip;
    let split = RECT {
        left: r.left,
        top: r.top,
        right: r.left + (w as f32 * 0.52).round() as i32,
        bottom: r.bottom,
    };
    sf.set_clip(Some(split));
    sf.fill_rrect_vgrad(&r, rad, rgb(0x4CB3FF, 1.0), rgb(0x1677F2, 1.0));
    sf.set_clip(prev_clip);
    // 鼻梁折线与双眼
    let sw = (size * 0.048).max(1.5);
    let ink = rgb(0x1B2B44, 0.92);
    let eye_y = cy - size * 0.11;
    let eye_h = size * 0.11;
    sf.thick_line(cx - size * 0.17, eye_y - eye_h * 0.5, cx - size * 0.17, eye_y + eye_h * 0.5, sw, ink);
    sf.thick_line(cx + size * 0.17, eye_y - eye_h * 0.5, cx + size * 0.17, eye_y + eye_h * 0.5, sw, ink);
    // 鼻梁
    let nx = cx + size * 0.01;
    sf.thick_line(nx, cy - size * 0.16, nx - size * 0.045, cy + size * 0.06, sw * 0.85, rgb(0x0E4EA8, 0.72));
    sf.thick_line(nx - size * 0.045, cy + size * 0.06, nx + size * 0.05, cy + size * 0.06, sw * 0.85, rgb(0x0E4EA8, 0.72));
    // 微笑曲线
    let smile_pts = arc_segments(cx, cy - size * 0.02, size * 0.25, 0.45, std::f32::consts::PI - 0.45, 16);
    for seg in smile_pts.windows(2) {
        sf.thick_line(seg[0].0, seg[0].1, seg[1].0, seg[1].1, sw, ink);
    }
    // 顶部柔和微高光描边
    sf.stroke_rrect(&r, rad, 1.0, (1.0, 1.0, 1.0, 0.42));
}

/// 绘制精致仿 macOS 风格「系统设置」圆角金属齿轮图标。
pub fn draw_settings_icon(sf: &mut Surface, cx: f32, cy: f32, size: f32) {
    let half = size * 0.46;
    let r = RECT {
        left: (cx - half).round() as i32,
        top: (cy - half).round() as i32,
        right: (cx + half).round() as i32,
        bottom: (cy + half).round() as i32,
    };
    let rad = size * 0.21;
    let sh = RECT { left: r.left, top: r.top + (size * 0.05) as i32, right: r.right, bottom: r.bottom + (size * 0.06) as i32 };
    sf.fill_rrect(&sh, rad, (0.0, 0.0, 0.0, 0.24));
    // 银灰金属外壳
    sf.fill_rrect_vgrad(&r, rad, rgb(0xE8EAEF, 1.0), rgb(0xA6ABB5, 1.0));
    sf.stroke_rrect(&r, rad, 1.0, (1.0, 1.0, 1.0, 0.55));
    // 内凹深灰圆盘
    sf.fill_circle(cx, cy, size * 0.34, rgb(0x464952, 0.96));
    sf.stroke_circle(cx, cy, size * 0.34, 1.0, (0.0, 0.0, 0.0, 0.25));
    // 12 齿精密亮银齿轮
    let gear_col = rgb(0xE5E7EC, 1.0);
    let tooth_w = (size * 0.072).max(2.0);
    for i in 0..12 {
        let a = i as f32 * (std::f32::consts::TAU / 12.0);
        let (dx, dy) = (a.cos(), a.sin());
        sf.thick_line(cx + dx * size * 0.19, cy + dy * size * 0.19, cx + dx * size * 0.29, cy + dy * size * 0.29, tooth_w, gear_col);
    }
    sf.fill_circle(cx, cy, size * 0.22, gear_col);
    sf.fill_circle(cx, cy, size * 0.115, rgb(0x5A5E68, 1.0));
    sf.stroke_circle(cx, cy, size * 0.115, 1.0, (1.0, 1.0, 1.0, 0.35));
}

/// 绘制精致仿 macOS 风格「启动台」多彩圆角九宫格图标。
pub fn draw_launchpad_icon(sf: &mut Surface, cx: f32, cy: f32, size: f32) {
    let half = size * 0.46;
    let r = RECT {
        left: (cx - half).round() as i32,
        top: (cy - half).round() as i32,
        right: (cx + half).round() as i32,
        bottom: (cy + half).round() as i32,
    };
    let rad = size * 0.21;
    let sh = RECT { left: r.left, top: r.top + (size * 0.05) as i32, right: r.right, bottom: r.bottom + (size * 0.06) as i32 };
    sf.fill_rrect(&sh, rad, (0.0, 0.0, 0.0, 0.24));
    sf.fill_rrect_vgrad(&r, rad, rgb(0x3B404C, 1.0), rgb(0x1E222B, 1.0));
    sf.stroke_rrect(&r, rad, 1.0, (1.0, 1.0, 1.0, 0.28));
    let palette9 = [
        0xFF5F57u32, 0xFEBC2E, 0x28C840,
        0x32ADE6, 0x0A84FF, 0xBF5AF2,
        0xFF375F, 0xFF9F0A, 0x64D2FF,
    ];
    let tile = size * 0.18;
    let step = size * 0.23;
    for row in 0..3 {
        for col in 0..3 {
            let tx = cx + (col as f32 - 1.0) * step;
            let ty = cy + (row as f32 - 1.0) * step;
            let tr = RECT {
                left: (tx - tile * 0.5).round() as i32,
                top: (ty - tile * 0.5).round() as i32,
                right: (tx + tile * 0.5).round() as i32,
                bottom: (ty + tile * 0.5).round() as i32,
            };
            sf.fill_rrect(&tr, tile * 0.26, rgb(palette9[row * 3 + col], 1.0));
        }
    }
}

/// 绘制 WinDockFinder 自身徽标（与 assets/app.ico 一致：顶栏条 + Dock 胶囊 + 三色方块）。
pub fn draw_app_badge(surface: &mut Surface, cx: f32, cy: f32, size: f32, scale: f32) {
    let half = size * 0.5;
    let r = RECT {
        left: (cx - half).round() as i32,
        top: (cy - half).round() as i32,
        right: (cx + half).round() as i32,
        bottom: (cy + half).round() as i32,
    };
    let rad = size * 0.22;
    surface.fill_rrect(&r, rad, rgb(0x0E6FD8, 1.0));
    let bar = RECT {
        left: r.left + (size * 0.15) as i32,
        top: r.top + (size * 0.15) as i32,
        right: r.right - (size * 0.15) as i32,
        bottom: r.top + (size * 0.15) as i32 + (size * 0.09).max(2.0) as i32,
    };
    surface.fill_rrect(&bar, bar.bottom as f32 - bar.top as f32, rgb(0xFFFFFF, 0.95));
    let dock = RECT {
        left: r.left + (size * 0.2) as i32,
        top: r.bottom - (size * 0.42) as i32,
        right: r.right - (size * 0.2) as i32,
        bottom: r.bottom - (size * 0.15) as i32,
    };
    surface.fill_rrect(&dock, dock.bottom as f32 - dock.top as f32, rgb(0xFFFFFF, 0.95));
    let colors = [0xFF5F57u32, 0xFEBC2E, 0x28C840];
    for (i, col) in colors.iter().enumerate() {
        let sq = size * 0.14;
        let x = cx + (i as f32 - 1.0) * size * 0.16 - sq * 0.5;
        let y = (dock.top + dock.bottom) as f32 * 0.5 - sq * 0.5;
        let s = RECT {
            left: x.round() as i32,
            top: y.round() as i32,
            right: (x + sq).round() as i32,
            bottom: (y + sq).round() as i32,
        };
        surface.fill_rrect(&s, sq * 0.3, rgb(*col, 1.0));
    }
    let _ = scale;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_badge_renders_dock_capsule_in_lower_half() {
        let (w, h) = (128, 128);
        let mut buf = vec![0xFFFF_FFFFu32; w * h];
        let mut sf = Surface {
            w,
            h,
            ptr: buf.as_mut_ptr(),
            clip: None,
        };
        draw_app_badge(&mut sf, 64.0, 64.0, 64.0, 1.0);
        // 顶部条与 Dock 胶囊之间（徽标中心附近）应为蓝色底板
        let mid = buf[64 * w + 40];
        assert!((mid & 0xFF) > ((mid >> 16) & 0xFF));
        // Dock 胶囊中部（避开三色方块）应为白色
        let dock = buf[78 * w + 46];
        assert!(((dock >> 16) & 0xFF) > 240 && (dock & 0xFF) > 240);
        // 徽标外角保持原背景（白色不透明）
        assert_eq!(buf[0], 0xFFFF_FFFF);
    }

    #[test]
    fn glyph_and_palette_render_without_panic() {
        let (w, h) = (96, 32);
        let mut buf = vec![0u32; w * h];
        let mut sf = Surface {
            w,
            h,
            ptr: buf.as_mut_ptr(),
            clip: None,
        };
        for g in [Glyph::Wifi, Glyph::Speaker, Glyph::Battery, Glyph::Gear, Glyph::TrashFull] {
            draw_glyph(&mut sf, g, 16.0, 16.0, 16.0, rgb(0xFFFFFF, 1.0));
        }
        assert!(buf.iter().any(|&p| p != 0));
        let p = palette(true, (0.04, 0.52, 1.0));
        assert!(p.text.0 > 0.9);
    }

    #[test]
    fn color_text_renderer_draws_colored_emoji_pixels() {
        let mut target = DibTarget::default();
        assert!(target.ensure(360, 48));
        if let Some(mut sf) = target.surface() {
            sf.clear(rgb(0xFFFFFF, 1.0));
        }
        let dc = target.mem_dc.expect("测试 DIB DC 应已创建");
        let mut renderer = ColorTextRenderer::new().expect("Direct2D 彩色文本渲染器应创建成功");
        let clip = RECT { left: 0, top: 0, right: 360, bottom: 48 };
        let text_rc = RECT { left: 8, top: 8, right: 352, bottom: 40 };
        let ok = renderer.draw_lines(
            dc,
            &clip,
            20,
            rgb(0x52647D, 1.0),
            &[(text_rc, "📦 🚀 🎨 ✨ 测试")],
        );
        assert!(ok, "Direct2D 彩色文本渲染应返回成功");

        let slice = unsafe {
            std::slice::from_raw_parts(target.ptr as *const u32, (target.w * target.h) as usize)
        };
        // 前景色 #52647D 满足 R < B；若出现明显的暖色像素（R - B > 30），则证明 Emoji 已按彩色字形输出。
        let warm_colored_pixels = slice
            .iter()
            .filter(|&&px| {
                let r = ((px >> 16) & 0xFF) as i32;
                let b = (px & 0xFF) as i32;
                r - b > 30
            })
            .count();
        assert!(
            warm_colored_pixels > 20,
            "期望渲染出彩色 Emoji 像素，实际暖色像素数: {warm_colored_pixels}"
        );

        let single_h = renderer
            .measure_wrapped_height(13, 400, "📦 短文本")
            .unwrap();
        let multi_h = renderer
            .measure_wrapped_height(
                13,
                120,
                "📦 这是一段足够长的测试文本用于验证自动换行计算高度确实大于单行高度",
            )
            .unwrap();
        assert!(multi_h > single_h, "期望换行高度 {multi_h} > 单行高度 {single_h}");
        assert!(renderer.draw_wrapped_lines(
            dc,
            &clip,
            13,
            rgb(0x52647D, 1.0),
            &[(text_rc, "🔔 自动换行彩色文本测试")],
        ));
        target.release();
    }
}
