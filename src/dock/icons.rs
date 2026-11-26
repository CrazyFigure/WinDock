//! Dock 图标：WIC/Shell 图标提取（短时 `--worker-icons` 子进程）、LRU 8MB 内存缓存、
//! 回收站/文件夹/托盘截图等特例图标，代次丢弃。
//!
//! 提取算法复用 WinLaunchpad：内容边界归一化、尖角矩形圆角化、纯暗色透明图标自动衬浅色卡片。

use std::collections::{HashSet, VecDeque};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
    BitBlt, GetDC, ReleaseDC, SRCCOPY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIBSECTION,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, STGM_READ,
};
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, IShellLinkW, ShellLink, SHCreateItemFromParsingName,
    SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::ui::IconPixels;
use crate::win32::{exe_path, fnv1a64, wide};

pub const WM_APP_ICON: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;
const ICON_CACHE_MAGIC: &[u8; 4] = b"WDF2";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 图标像素缓存字节预算（8 MB）。
pub const ICON_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Dock 图标统一高清源图尺寸（悬停放大不跨档重提，杜绝退回字母块闪烁）。
pub const DOCK_ICON_SRC_SIZE: u32 = 128;

pub type IconKey = (String, u32);

/// 提取结果（通过 WM_APP_ICON 投递到 UI 线程，lParam 为 Box::into_raw 指针）。
pub struct IconResult {
    pub key: String,
    pub size: u32,
    pub pixels: Option<Arc<Vec<u8>>>,
    pub w: u32,
    pub h: u32,
}

struct Queue {
    items: VecDeque<IconKey>,
    pending: HashSet<IconKey>,
}

/// 后台提取调度器：命中磁盘缓存直接回传，未命中交给短时子进程提取。
pub struct IconWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    cache_dir: PathBuf,
}

impl IconWorker {
    pub fn start(hwnd: HWND, cache_dir: PathBuf) -> Arc<IconWorker> {
        let _ = std::fs::create_dir_all(&cache_dir);
        let queue = Arc::new((
            Mutex::new(Queue {
                items: VecDeque::new(),
                pending: HashSet::new(),
            }),
            Condvar::new(),
        ));
        let q = queue.clone();
        let dir = cache_dir.clone();
        let hwnd_raw = hwnd.0 as isize;
        std::thread::Builder::new()
            .name("icon-worker".into())
            .spawn(move || icon_thread(q, hwnd_raw, dir))
            .ok();
        Arc::new(IconWorker { queue, cache_dir })
    }

    /// 归一化缓存尺寸档位：统一使用 128px 高清源图，悬停放大时直接高质量双线性缩放。
    pub fn bucket_size(_size: u32) -> u32 {
        DOCK_ICON_SRC_SIZE
    }

    pub fn cache_path(cache_dir: &Path, key: &str, size: u32) -> PathBuf {
        let bucket = Self::bucket_size(size);
        let sig = format!("wdf2|{}|{}", key.to_lowercase(), bucket);
        let h = fnv1a64(sig.as_bytes());
        cache_dir.join(format!("wdf2_{h:016x}_{bucket}.icache"))
    }

    /// 清空磁盘图标缓存。
    pub fn clear_disk_cache(cache_dir: &Path) {
        let Ok(rd) = std::fs::read_dir(cache_dir) else { return };
        for ent in rd.flatten() {
            if ent.path().extension().map(|e| e.eq_ignore_ascii_case("icache")).unwrap_or(false) {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }

    /// 请求一个图标；已在队列/提取中的请求会合并。
    pub fn request(&self, key: &str, size: u32) {
        if key.is_empty() {
            return;
        }
        let bucket = Self::bucket_size(size);
        let k = (key.to_string(), bucket);
        let (lock, cv) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.pending.insert(k.clone()) {
            q.items.push_back(k);
            cv.notify_one();
        }
    }

    /// 清空尚未开始的请求（固定项变化后丢弃过期任务）。
    pub fn clear(&self) {
        let (lock, _) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.items.clear();
        q.pending.clear();
    }
}

fn read_cache_file(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || &data[0..4] != ICON_CACHE_MAGIC {
        return None;
    }
    let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let need = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if w == 0 || h == 0 || data.len() != 12 + need {
        return None;
    }
    Some((data[12..].to_vec(), w, h))
}

fn write_cache_file(path: &Path, px: &[u8], w: u32, h: u32) {
    let mut buf = Vec::with_capacity(12 + px.len());
    buf.extend_from_slice(ICON_CACHE_MAGIC);
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(px);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn post_icon(hwnd: HWND, key: String, size: u32, extracted: Option<(Vec<u8>, u32, u32)>) {
    let result = Box::new(IconResult {
        key,
        size,
        pixels: extracted.as_ref().map(|e| Arc::new(e.0.clone())),
        w: extracted.as_ref().map(|e| e.1).unwrap_or(0),
        h: extracted.as_ref().map(|e| e.2).unwrap_or(0),
    });
    unsafe {
        let _ = PostMessageW(
            hwnd,
            WM_APP_ICON,
            WPARAM(0),
            LPARAM(Box::into_raw(result) as isize),
        );
    }
}

fn icon_thread(queue: Arc<(Mutex<Queue>, Condvar)>, hwnd_raw: isize, cache_dir: PathBuf) {
    let hwnd = HWND(hwnd_raw as *mut _);
    loop {
        let mut batch: Vec<IconKey> = Vec::new();
        {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            while q.items.is_empty() {
                q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            if q.items.len() < 16 {
                let (nq, _) = cv
                    .wait_timeout(q, Duration::from_millis(12))
                    .unwrap_or_else(|e| e.into_inner());
                q = nq;
            }
            while batch.len() < 32 {
                if let Some(k) = q.items.pop_front() {
                    batch.push(k);
                } else {
                    break;
                }
            }
        }

        let mut misses: Vec<(String, u32, PathBuf)> = Vec::new();
        for (key, size) in &batch {
            let p = IconWorker::cache_path(&cache_dir, key, *size);
            if let Some(hit) = read_cache_file(&p) {
                post_icon(hwnd, key.clone(), *size, Some(hit));
            } else {
                misses.push((key.clone(), *size, p));
            }
        }

        if !misses.is_empty() {
            run_worker_icons(&misses);
            for (key, size, path) in misses {
                let loaded = read_cache_file(&path);
                post_icon(hwnd, key, size, loaded);
            }
        }

        {
            let (lock, _) = &*queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            for k in batch {
                q.pending.remove(&k);
            }
        }
    }
}

/// 启动短时 `--worker-icons` 子进程批量提取缺失图标（超时 15 秒自动回收）。
fn run_worker_icons(items: &[(String, u32, PathBuf)]) {
    let Some(first_dir) = items.first().and_then(|x| x.2.parent()) else {
        return;
    };
    let req_path = first_dir.join(format!("req_{}.txt", std::process::id()));
    let mut lines = String::new();
    for (key, size, out) in items {
        let clean_key = key.replace(['\r', '\n', '\t'], " ");
        lines.push_str(&format!("{}\t{}\t{}\n", clean_key, size, out.display()));
    }
    if std::fs::write(&req_path, lines.as_bytes()).is_err() {
        return;
    }
    let exe = exe_path();
    if let Ok(mut child) = std::process::Command::new(exe)
        .arg("--worker-icons")
        .arg(&req_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > Duration::from_secs(15) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
    }
    let _ = std::fs::remove_file(&req_path);
}

/// 短时工作进程入口：读取请求清单并提取所有图标落盘。
pub fn worker_extract_from_file(req_path: &Path) {
    let Ok(text) = std::fs::read_to_string(req_path) else {
        return;
    };
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let need_uninit = hr.is_ok();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(key) = parts.next() else { continue };
        let Some(sz_str) = parts.next() else { continue };
        let Some(out_str) = parts.next() else { continue };
        let size = sz_str.parse::<u32>().unwrap_or(96);
        let out_path = Path::new(out_str);
        if out_path.exists() {
            continue;
        }
        if let Some((px, w, h)) = std::panic::catch_unwind(|| extract(key, size)).ok().flatten() {
            write_cache_file(out_path, &px, w, h);
        }
    }
    if need_uninit {
        unsafe { CoUninitialize() };
    }
}

/// 读取 .lnk 的目标路径（不解析网络路径）。
pub fn read_lnk_target(path: &Path) -> Option<String> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = link.cast().ok()?;
        if pf.Load(PCWSTR(wide(path).as_ptr()), STGM_READ).is_err() {
            return None;
        }
        let mut buf = [0u16; 1024];
        if link
            .GetPath(&mut buf, std::ptr::null_mut(), windows::Win32::UI::Shell::SLGP_RAWPATH.0 as u32)
            .is_ok()
        {
            let s = crate::win32::from_wide(&buf);
            if s.is_empty() { None } else { Some(s) }
        } else {
            None
        }
    }
}

/// 返回 (归一化预乘 BGRA 像素, 宽, 高)。仅在工作进程中调用。
pub fn extract(key: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let target = size.clamp(32, 192);
    // 请求 256px 高清原图后再经精确区域重采样缩小，显著提升细节锐度与抗锯齿质量
    let fetch_sz = target.max(256);
    let raw = extract_item(key, fetch_sz).or_else(|| {
        if Path::new(key).extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            if let Some(t) = read_lnk_target(Path::new(key)) {
                if Path::new(&t).exists() {
                    return extract_item(&t, fetch_sz);
                }
            }
        }
        None
    })?;
    Some(normalize_and_premultiply(raw.0, raw.1, raw.2, target))
}

fn extract_item(key: &str, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let size = size.clamp(16, 256) as i32;
    unsafe {
        let w = wide(key);
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let fac: IShellItemImageFactory = item.cast().ok()?;
        let hbm = fac
            .GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT)
            .or_else(|_| fac.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY))
            .ok()?;
        let out = hbitmap_to_bgra(HBITMAP(hbm.0), true);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        out
    }
}

/// HBITMAP → BGRA 像素；32bpp 无 alpha 时补齐不透明。
pub fn hbitmap_to_bgra(hbm: HBITMAP, fill_alpha: bool) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut dib = DIBSECTION::default();
        let got = GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<DIBSECTION>() as i32,
            Some(&mut dib as *mut _ as *mut _),
        );
        if got == 0 {
            return None;
        }
        let w = dib.dsBmih.biWidth.max(1);
        let h = dib.dsBmih.biHeight.abs().max(1);
        let has_alpha = dib.dsBmih.biBitCount == 32;
        let hdc = CreateCompatibleDC(windows::Win32::Graphics::Gdi::HDC::default());
        let mut bi = BITMAPINFO {
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
        let mut px = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(px.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        if lines == 0 {
            return None;
        }
        let mut any_alpha = false;
        for p in px.chunks_exact_mut(4) {
            if p[3] != 0 {
                any_alpha = true;
                break;
            }
        }
        if fill_alpha && (!has_alpha || !any_alpha) {
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }
        Some((px, w as u32, h as u32))
    }
}

/// 通过 SHGetImageList(SHIL_JUMBO 256px) 提取高清系统内置图标（如回收站），失败回退 SHGSI_LARGEICON。
pub fn extract_stock_icon_hd(stock_id: windows::Win32::UI::Shell::SHSTOCKICONID, target: u32) -> Option<IconPixels> {
    use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
    use windows::Win32::UI::Shell::{
        SHGetImageList, SHGetStockIconInfo, SHGSI_ICON, SHGSI_LARGEICON, SHGSI_SYSICONINDEX,
        SHSTOCKICONINFO, SHIL_EXTRALARGE, SHIL_JUMBO,
    };
    unsafe {
        let mut info = SHSTOCKICONINFO {
            cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
            ..Default::default()
        };
        if SHGetStockIconInfo(stock_id, SHGSI_SYSICONINDEX, &mut info).is_ok() {
            for list_id in [SHIL_JUMBO, SHIL_EXTRALARGE] {
                if let Ok(list) = SHGetImageList::<IImageList>(list_id as i32) {
                    if let Ok(hicon) = list.GetIcon(info.iSysImageIndex, ILD_TRANSPARENT.0) {
                        let raw_sz = if list_id == SHIL_JUMBO { 256 } else { 48 };
                        // 直接提取未预乘源像素再归一化，杜绝双重预乘导致的边缘黑边与粗糙锯齿
                        if let Some((raw_px, rw, rh)) = hicon_to_straight_bgra(hicon, raw_sz) {
                            let (norm, w, h) = normalize_and_premultiply(raw_px, rw, rh, target);
                            return Some(IconPixels { w, h, px: Arc::new(norm) });
                        }
                    }
                }
            }
        }
        if SHGetStockIconInfo(stock_id, SHGSI_ICON | SHGSI_LARGEICON, &mut info).is_ok() {
            if let Some((raw_px, rw, rh)) = hicon_to_straight_bgra(info.hIcon, target) {
                let (norm, w, h) = normalize_and_premultiply(raw_px, rw, rh, target);
                return Some(IconPixels { w, h, px: Arc::new(norm) });
            }
        }
    }
    None
}

/// 读取 HICON 原始未预乘 BGRA 像素（优先直接读取 hbmColor + hbmMask，保留原生抗锯齿 Alpha；失败回退 DrawIconEx）。
/// 调用后自动销毁传入的 `hicon`。
fn hicon_to_straight_bgra(hicon: windows::Win32::UI::WindowsAndMessaging::HICON, fallback_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, GetIconInfo, DI_NORMAL, ICONINFO};
    unsafe {
        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_ok() {
            let color_res = if !ii.hbmColor.is_invalid() {
                hbitmap_to_bgra(ii.hbmColor, false)
            } else {
                None
            };
            let mask_res = if !ii.hbmMask.is_invalid() {
                hbitmap_to_bgra(ii.hbmMask, false)
            } else {
                None
            };
            if !ii.hbmColor.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            }
            if !ii.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
            }
            if let Some((mut px, w, h)) = color_res {
                let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
                if !has_alpha {
                    // 无内置 Alpha 通道的老式图标：用 hbmMask 白色(=透明)/黑色(=不透明)还原精确轮廓
                    if let Some((mpx, mw, mh)) = mask_res {
                        if mw == w && mh >= h {
                            for y in 0..h as usize {
                                for x in 0..w as usize {
                                    let idx = (y * w as usize + x) * 4;
                                    let mask_white = mpx[idx] > 127;
                                    px[idx + 3] = if mask_white { 0 } else { 255 };
                                    if mask_white {
                                        px[idx] = 0;
                                        px[idx + 1] = 0;
                                        px[idx + 2] = 0;
                                    }
                                }
                            }
                        } else {
                            for p in px.chunks_exact_mut(4) {
                                p[3] = 255;
                            }
                        }
                    } else {
                        for p in px.chunks_exact_mut(4) {
                            p[3] = 255;
                        }
                    }
                }
                let _ = DestroyIcon(hicon);
                return Some((px, w, h));
            }
        }

        // 回退路径：DrawIconEx 绘制到 32bpp DIB
        let size = fallback_size.max(16);
        let hdc = CreateCompatibleDC(windows::Win32::Graphics::Gdi::HDC::default());
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut ptr, windows::Win32::Foundation::HANDLE::default(), 0).ok()?;
        let old = windows::Win32::Graphics::Gdi::SelectObject(hdc, bmp);
        let ok = DrawIconEx(hdc, 0, 0, hicon, size as i32, size as i32, 0, windows::Win32::Graphics::Gdi::HBRUSH::default(), DI_NORMAL).is_ok();
        let _ = windows::Win32::Graphics::Gdi::SelectObject(hdc, old);
        let _ = DeleteDC(hdc);
        if !ok || ptr.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DestroyIcon(hicon);
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, (size * size * 4) as usize);
        let mut px = slice.to_vec();
        let any_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
        if !any_alpha {
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DestroyIcon(hicon);
        Some((px, size, size))
    }
}

/// HICON → 预乘 BGRA 正方形像素（托盘/窗口实时图标使用）。
pub fn hicon_to_pixels(hicon: windows::Win32::UI::WindowsAndMessaging::HICON, size: u32) -> Option<IconPixels> {
    let (raw, rw, rh) = hicon_to_straight_bgra(hicon, size)?;
    let target = size.clamp(16, 256);
    if rw == target && rh == target {
        let mut px = raw;
        let is_straight = detect_straight_alpha(&px);
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if is_straight {
                p[0] = ((p[0] as u32 * a + 127) / 255).min(a) as u8;
                p[1] = ((p[1] as u32 * a + 127) / 255).min(a) as u8;
                p[2] = ((p[2] as u32 * a + 127) / 255).min(a) as u8;
            } else {
                p[0] = p[0].min(p[3]);
                p[1] = p[1].min(p[3]);
                p[2] = p[2].min(p[3]);
            }
        }
        return Some(IconPixels { w: rw, h: rh, px: Arc::new(px) });
    }
    let premul = to_premul_f32(&raw);
    let mut out = vec![0u8; (target * target * 4) as usize];
    for y in 0..target as i32 {
        let sy0 = (y as f32 / target as f32) * rh as f32;
        let sy1 = ((y + 1) as f32 / target as f32) * rh as f32;
        for x in 0..target as i32 {
            let sx0 = (x as f32 / target as f32) * rw as f32;
            let sx1 = ((x + 1) as f32 / target as f32) * rw as f32;
            let [b, g, r, a] = sample_area_premul(&premul, rw as i32, rh as i32, 0, 0, rw as i32 - 1, rh as i32 - 1, sx0, sy0, sx1, sy1);
            let oi = ((y as u32 * target + x as u32) * 4) as usize;
            let ai = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            out[oi] = (b * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 1] = (g * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 2] = (r * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 3] = ai;
        }
    }
    Some(IconPixels { w: target, h: target, px: Arc::new(out) })
}

/// 截取屏幕矩形为预乘 BGRA 像素（用于托盘图标镜像）。
pub fn capture_screen_rect(x: i32, y: i32, w: i32, h: i32) -> Option<IconPixels> {
    if w <= 0 || h <= 0 || w > 256 || h > 256 {
        return None;
    }
    unsafe {
        let screen = GetDC(HWND::default());
        if screen.is_invalid() {
            return None;
        }
        let hdc = CreateCompatibleDC(windows::Win32::Graphics::Gdi::HDC::default());
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
        let bmp = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut ptr, windows::Win32::Foundation::HANDLE::default(), 0).ok()?;
        let old = windows::Win32::Graphics::Gdi::SelectObject(hdc, bmp);
        let ok = BitBlt(hdc, 0, 0, w, h, screen, x, y, SRCCOPY).is_ok();
        let _ = windows::Win32::Graphics::Gdi::SelectObject(hdc, old);
        let _ = DeleteDC(hdc);
        ReleaseDC(HWND::default(), screen);
        if !ok {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, (w * h * 4) as usize);
        let px = slice.to_vec();
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        key_out_icon_background(px, w, h)
    }
}

/// 将不透明的原生托盘截图转为带透明背景的图标像素。
pub fn key_out_icon_background(mut px: Vec<u8>, w: i32, h: i32) -> Option<IconPixels> {
    let background = px.get(..3)?;
    let (kb, kg, kr) = (background[0], background[1], background[2]);
    for pixel in px.chunks_exact_mut(4) {
        let distance = pixel[0].abs_diff(kb) as u16 + pixel[1].abs_diff(kg) as u16 + pixel[2].abs_diff(kr) as u16;
        if distance < 24 {
            pixel.fill(0);
        } else {
            pixel[3] = 255;
        }
    }
    Some(IconPixels { w: w as u32, h: h as u32, px: Arc::new(px) })
}

/// 检测 32bpp BGRA 像素缓冲是否为未预乘（straight alpha）。
/// 若存在任意半透明像素的 RGB 明显大于 Alpha，则必为 straight alpha。
fn detect_straight_alpha(px: &[u8]) -> bool {
    let mut has_semi = false;
    for p in px.chunks_exact(4) {
        let a = p[3];
        if a > 0 && a < 252 {
            has_semi = true;
            if p[0] > a.saturating_add(2) || p[1] > a.saturating_add(2) || p[2] > a.saturating_add(2) {
                return true;
            }
        }
    }
    // 纯二值透明图标（全 0 或全 255）视为 straight（等价且保证透明像素 RGB 归零）
    !has_semi
}

/// 将 32bpp BGRA（自动识别 straight 或已预乘）转为统一浮点预乘 BGRA `[b, g, r, a]`（0..1）。
fn to_premul_f32(px: &[u8]) -> Vec<[f32; 4]> {
    let straight = detect_straight_alpha(px);
    let mut out = Vec::with_capacity(px.len() / 4);
    for p in px.chunks_exact(4) {
        let a = p[3] as f32 / 255.0;
        if a <= 0.0 {
            out.push([0.0, 0.0, 0.0, 0.0]);
        } else if straight {
            out.push([
                (p[0] as f32 / 255.0) * a,
                (p[1] as f32 / 255.0) * a,
                (p[2] as f32 / 255.0) * a,
                a,
            ]);
        } else {
            out.push([
                (p[0] as f32 / 255.0).min(a),
                (p[1] as f32 / 255.0).min(a),
                (p[2] as f32 / 255.0).min(a),
                a,
            ]);
        }
    }
    out
}

/// 在预乘浮点图像上执行精确区域重采样（缩小用精确像素足迹盒滤，放大用预乘双线性插值），杜绝锯齿与黑边。
#[allow(clippy::too_many_arguments)]
fn sample_area_premul(
    premul: &[[f32; 4]],
    iw: i32,
    _ih: i32,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    sx0: f32,
    sy0: f32,
    sx1: f32,
    sy1: f32,
) -> [f32; 4] {
    let span_x = (sx1 - sx0).max(1e-4);
    let span_y = (sy1 - sy0).max(1e-4);
    // 放大或近似 1:1：使用预乘双线性插值保证平滑
    if span_x <= 1.05 && span_y <= 1.05 {
        let cx = (sx0 + sx1) * 0.5 - 0.5;
        let cy = (sy0 + sy1) * 0.5 - 0.5;
        let x0 = (cx.floor() as i32).clamp(min_x, max_x);
        let x1 = (x0 + 1).min(max_x);
        let y0 = (cy.floor() as i32).clamp(min_y, max_y);
        let y1 = (y0 + 1).min(max_y);
        let tx = (cx - x0 as f32).clamp(0.0, 1.0);
        let ty = (cy - y0 as f32).clamp(0.0, 1.0);
        let p00 = premul[(y0 * iw + x0) as usize];
        let p10 = premul[(y0 * iw + x1) as usize];
        let p01 = premul[(y1 * iw + x0) as usize];
        let p11 = premul[(y1 * iw + x1) as usize];
        let w00 = (1.0 - tx) * (1.0 - ty);
        let w10 = tx * (1.0 - ty);
        let w01 = (1.0 - tx) * ty;
        let w11 = tx * ty;
        return [
            p00[0] * w00 + p10[0] * w10 + p01[0] * w01 + p11[0] * w11,
            p00[1] * w00 + p10[1] * w10 + p01[1] * w01 + p11[1] * w11,
            p00[2] * w00 + p10[2] * w10 + p01[2] * w01 + p11[2] * w11,
            p00[3] * w00 + p10[3] * w10 + p01[3] * w01 + p11[3] * w11,
        ];
    }

    // 缩小：按目标像素在源图像上的精确覆盖面积加权积分（避免 2x2 双线性跳点混叠）
    let ix0 = (sx0.floor() as i32).clamp(min_x, max_x);
    let ix1 = ((sx1 - 1e-4).floor() as i32).clamp(min_x, max_x);
    let iy0 = (sy0.floor() as i32).clamp(min_y, max_y);
    let iy1 = ((sy1 - 1e-4).floor() as i32).clamp(min_y, max_y);
    let mut sb = 0.0f32;
    let mut sg = 0.0f32;
    let mut sr = 0.0f32;
    let mut sa = 0.0f32;
    let mut wsum = 0.0f32;
    for iy in iy0..=iy1 {
        let wy = ((iy as f32 + 1.0).min(sy1) - (iy as f32).max(sy0)).max(0.0);
        if wy <= 0.0 {
            continue;
        }
        let row = (iy * iw) as usize;
        for ix in ix0..=ix1 {
            let wx = ((ix as f32 + 1.0).min(sx1) - (ix as f32).max(sx0)).max(0.0);
            let w = wx * wy;
            if w <= 0.0 {
                continue;
            }
            let p = premul[row + ix as usize];
            sb += p[0] * w;
            sg += p[1] * w;
            sr += p[2] * w;
            sa += p[3] * w;
            wsum += w;
        }
    }
    if wsum <= 1e-6 {
        return [0.0; 4];
    }
    let inv = 1.0 / wsum;
    [sb * inv, sg * inv, sr * inv, sa * inv]
}

/// 图标内容边界归一化：裁剪透明留白、尖角矩形圆角化、
/// 纯暗色透明图标自动衬 macOS 浅色圆角卡片、全预乘精确区域重采样（绝不双重预乘）。
pub fn normalize_and_premultiply(px: Vec<u8>, w: u32, h: u32, target: u32) -> (Vec<u8>, u32, u32) {
    let iw = w as i32;
    let ih = h as i32;
    let premul = to_premul_f32(&px);
    let at = |x: i32, y: i32| -> [u8; 4] {
        let i = ((y * iw + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    };

    let (mut cmin_x, mut cmin_y, mut cmax_x, mut cmax_y) = (iw, ih, -1i32, -1i32);
    let (mut lmin_x, mut lmin_y, mut lmax_x, mut lmax_y) = (iw, ih, -1i32, -1i32);
    for y in 0..ih {
        for x in 0..iw {
            let a = at(x, y)[3];
            if a >= 170 {
                cmin_x = cmin_x.min(x);
                cmin_y = cmin_y.min(y);
                cmax_x = cmax_x.max(x);
                cmax_y = cmax_y.max(y);
            }
            if a > 18 {
                lmin_x = lmin_x.min(x);
                lmin_y = lmin_y.min(y);
                lmax_x = lmax_x.max(x);
                lmax_y = lmax_y.max(y);
            }
        }
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = if cmax_x >= cmin_x && cmax_y >= cmin_y {
        (
            (cmin_x - 1).max(0),
            (cmin_y - 1).max(0),
            (cmax_x + 1).min(iw - 1),
            (cmax_y + 1).min(ih - 1),
        )
    } else if lmax_x >= lmin_x && lmax_y >= lmin_y {
        (lmin_x, lmin_y, lmax_x, lmax_y)
    } else {
        (0, 0, iw - 1, ih - 1)
    };

    let cw0 = (max_x - min_x + 1).max(1);
    let ch0 = (max_y - min_y + 1).max(1);
    let inset_c = (cw0.min(ch0) / 24).max(1);
    let sharp_corners = [
        at(min_x + inset_c, min_y + inset_c)[3] > 200,
        at(max_x - inset_c, min_y + inset_c)[3] > 200,
        at(min_x + inset_c, max_y - inset_c)[3] > 200,
        at(max_x - inset_c, max_y - inset_c)[3] > 200,
    ];
    let is_sharp_rect = sharp_corners.iter().filter(|&&b| b).count() >= 3;

    if is_sharp_rect && cw0 > 24 && ch0 > 24 {
        let is_white_ring = |d: i32| -> bool {
            let samples = [
                at(min_x + d, min_y + ch0 / 2),
                at(max_x - d, min_y + ch0 / 2),
                at(min_x + cw0 / 2, min_y + d),
                at(min_x + cw0 / 2, max_y - d),
            ];
            samples
                .iter()
                .all(|p| p[3] > 235 && p[0] > 240 && p[1] > 240 && p[2] > 240)
        };
        let inner = at(min_x + cw0 / 4, min_y + ch0 / 4);
        let inner_not_white = inner[0] < 230 || inner[1] < 230 || inner[2] < 230;
        if inner_not_white && is_white_ring(0) && is_white_ring(1) && !is_white_ring(4) {
            min_x += 2;
            min_y += 2;
            max_x -= 2;
            max_y -= 2;
        }
    }

    let cw_i = (max_x - min_x + 1).max(1);
    let ch_i = (max_y - min_y + 1).max(1);
    let cw = cw_i as f32;
    let ch = ch_i as f32;
    let edge_opaque = at(min_x + cw_i / 2, min_y + 2)[3] > 200 && at(min_x + 2, min_y + ch_i / 2)[3] > 200;

    // 基于还原后的真实非预乘色彩按 Alpha 权重统计亮度与饱和度，精准识别 ChatGPT / Seer 等暗色透明图标
    let mut trans_px = 0usize;
    let mut vis_weight = 0.0f32;
    let mut bright_or_color_weight = 0.0f32;
    let mut luma_sum = 0.0f32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = premul[(y * iw + x) as usize];
            let a = p[3];
            if a <= 0.08 {
                trans_px += 1;
            } else if a >= 0.16 {
                let b = (p[0] / a).clamp(0.0, 1.0);
                let g = (p[1] / a).clamp(0.0, 1.0);
                let r = (p[2] / a).clamp(0.0, 1.0);
                let luma = 0.114 * b + 0.587 * g + 0.299 * r;
                let sat = b.max(g).max(r) - b.min(g).min(r);
                vis_weight += a;
                luma_sum += luma * a;
                if luma > 0.42 || sat > 0.26 {
                    bright_or_color_weight += a;
                }
            }
        }
    }
    let total_box = (cw_i * ch_i).max(1) as f32;
    let trans_ratio = trans_px as f32 / total_box;
    let avg_luma = if vis_weight > 0.0 { luma_sum / vis_weight } else { 1.0 };
    let bright_ratio = if vis_weight > 0.0 { bright_or_color_weight / vis_weight } else { 1.0 };
    let needs_light_card = !is_sharp_rect && trans_ratio > 0.16 && avg_luma < 0.38 && bright_ratio < 0.22;

    let mut out = vec![0u8; (target * target * 4) as usize];
    if needs_light_card {
        let card_sz = (target as f32 * 0.86).round().max(16.0);
        let card_x0 = (target as f32 - card_sz) * 0.5;
        let card_y0 = (target as f32 - card_sz) * 0.5;
        let card_rcx = card_x0 + card_sz * 0.5;
        let card_rcy = card_y0 + card_sz * 0.5;
        // 采用与 macOS 原生图标一致的 22% 柔和圆角卡片
        let card_rad = (card_sz * 0.22).max(4.0);
        let card_hw = card_sz * 0.5 - card_rad;
        let card_hh = card_sz * 0.5 - card_rad;
        let inner_fit = card_sz * 0.66;
        let iscale = (inner_fit / cw).min(inner_fit / ch);
        let dw = (cw * iscale).round().max(1.0);
        let dh = (ch * iscale).round().max(1.0);
        let dx0 = (target as f32 - dw) * 0.5;
        let dy0 = (target as f32 - dh) * 0.5;
        for y in 0..target as i32 {
            let fy = y as f32 + 0.5;
            let grad_t = ((fy - card_y0) / card_sz).clamp(0.0, 1.0);
            let base_r = (252.0 - 17.0 * grad_t) / 255.0;
            let base_g = (253.0 - 15.0 * grad_t) / 255.0;
            let base_b = (255.0 - 11.0 * grad_t) / 255.0;
            let sy0 = min_y as f32 + ((y as f32 - dy0) / dh) * ch;
            let sy1 = min_y as f32 + ((y as f32 + 1.0 - dy0) / dh) * ch;
            for x in 0..target as i32 {
                let fx = x as f32 + 0.5;
                let qx = (fx - card_rcx).abs() - card_hw;
                let qy = (fy - card_rcy).abs() - card_hh;
                let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - card_rad;
                let card_cov = (0.5 - sd).clamp(0.0, 1.0);
                if card_cov <= 0.0 {
                    continue;
                }
                let rim = if sd > -1.3 && sd < 0.2 { 1.0 - ((sd + 0.55).abs() / 0.85).clamp(0.0, 1.0) } else { 0.0 };
                let mut r = base_r + (1.0 - base_r) * rim * 0.55;
                let mut g = base_g + (1.0 - base_g) * rim * 0.55;
                let mut b = base_b + (1.0 - base_b) * rim * 0.55;
                if fx >= dx0 && fx <= dx0 + dw && fy >= dy0 && fy <= dy0 + dh {
                    let sx0 = min_x as f32 + ((x as f32 - dx0) / dw) * cw;
                    let sx1 = min_x as f32 + ((x as f32 + 1.0 - dx0) / dw) * cw;
                    let fg = sample_area_premul(&premul, iw, ih, min_x, min_y, max_x, max_y, sx0, sy0, sx1, sy1);
                    let inv_a = 1.0 - fg[3].clamp(0.0, 1.0);
                    // 标准预乘 Porter-Duff OVER：前景已预乘，直接叠加在卡片底色上
                    b = fg[0] + b * inv_a;
                    g = fg[1] + g * inv_a;
                    r = fg[2] + r * inv_a;
                }
                let oi = ((y as u32 * target + x as u32) * 4) as usize;
                let ai = (card_cov * 255.0).round().clamp(0.0, 255.0) as u8;
                out[oi] = (b * card_cov * 255.0).round().clamp(0.0, ai as f32) as u8;
                out[oi + 1] = (g * card_cov * 255.0).round().clamp(0.0, ai as f32) as u8;
                out[oi + 2] = (r * card_cov * 255.0).round().clamp(0.0, ai as f32) as u8;
                out[oi + 3] = ai;
            }
        }
        return (out, target, target);
    }

    let fill_ratio = if is_sharp_rect || edge_opaque { 0.86 } else { 0.90 };
    let fit = target as f32 * fill_ratio;
    let scale = (fit / cw).min(fit / ch);
    let dw = (cw * scale).round().max(1.0);
    let dh = (ch * scale).round().max(1.0);
    let dx0 = (target as f32 - dw) * 0.5;
    let dy0 = (target as f32 - dh) * 0.5;
    // macOS 风格柔和圆润圆角（约 22% 边长），避免 Windows 尖角方卡显得生硬
    let rad = (dw.min(dh) * 0.22).max(4.0);
    let half_w = dw * 0.5 - rad;
    let half_h = dh * 0.5 - rad;
    let rcx = dx0 + dw * 0.5;
    let rcy = dy0 + dh * 0.5;

    for y in 0..target as i32 {
        let fy_out = y as f32 + 0.5;
        if fy_out < dy0 - 0.5 || fy_out > dy0 + dh + 0.5 {
            continue;
        }
        let sy0 = min_y as f32 + ((y as f32 - dy0) / dh) * ch;
        let sy1 = min_y as f32 + ((y as f32 + 1.0 - dy0) / dh) * ch;
        for x in 0..target as i32 {
            let fx_out = x as f32 + 0.5;
            if fx_out < dx0 - 0.5 || fx_out > dx0 + dw + 0.5 {
                continue;
            }
            let sx0 = min_x as f32 + ((x as f32 - dx0) / dw) * cw;
            let sx1 = min_x as f32 + ((x as f32 + 1.0 - dx0) / dw) * cw;
            let [mut b, mut g, mut r, mut a] = sample_area_premul(&premul, iw, ih, min_x, min_y, max_x, max_y, sx0, sy0, sx1, sy1);
            // 若采样框越出目标图像边缘，平滑裁切边缘覆盖率
            let clip_x = ((x as f32 + 1.0).min(dx0 + dw) - (x as f32).max(dx0)).clamp(0.0, 1.0);
            let clip_y = ((y as f32 + 1.0).min(dy0 + dh) - (y as f32).max(dy0)).clamp(0.0, 1.0);
            let mut cov = clip_x * clip_y;
            if is_sharp_rect {
                let qx = (fx_out - rcx).abs() - half_w;
                let qy = (fy_out - rcy).abs() - half_h;
                let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad;
                cov *= (0.5 - sd).clamp(0.0, 1.0);
            }
            b *= cov;
            g *= cov;
            r *= cov;
            a *= cov;
            let ai = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            let oi = ((y as u32 * target + x as u32) * 4) as usize;
            // b/g/r 已处于预乘空间，直接写出不再二次乘以 alpha
            out[oi] = (b * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 1] = (g * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 2] = (r * 255.0).round().clamp(0.0, ai as f32) as u8;
            out[oi + 3] = ai;
        }
    }
    (out, target, target)
}

// ---------- 内存 LRU 缓存 ----------

static GENERATION: AtomicU64 = AtomicU64::new(1);

pub fn current_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

struct CacheInner {
    map: std::collections::HashMap<IconKey, (Arc<Vec<u8>>, u32, u32, u64)>,
    order: VecDeque<IconKey>,
    bytes: usize,
}

/// 带字节预算的 LRU 图标缓存（预算 8 MB）。
pub struct IconCache {
    inner: Mutex<CacheInner>,
    budget: usize,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new(ICON_BUDGET_BYTES)
    }
}

impl IconCache {
    pub fn new(budget: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: std::collections::HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
            budget,
        }
    }

    pub fn insert(&self, key: &str, size: u32, px: Arc<Vec<u8>>, w: u32, h: u32, generation: u64) {
        let k: IconKey = (key.to_string(), IconWorker::bucket_size(size));
        let cost = px.len() + 64;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = inner.map.remove(&k) {
            inner.bytes = inner.bytes.saturating_sub(old.0.len() + 64);
            inner.order.retain(|x| x != &k);
        }
        inner.bytes += cost;
        inner.map.insert(k.clone(), (px, w, h, generation));
        inner.order.push_back(k);
        while inner.bytes > self.budget {
            match inner.order.pop_front() {
                Some(evict) => {
                    if let Some(old) = inner.map.remove(&evict) {
                        inner.bytes = inner.bytes.saturating_sub(old.0.len() + 64);
                    }
                }
                None => break,
            }
        }
    }

    pub fn get(&self, key: &str, size: u32) -> Option<Arc<Vec<u8>>> {
        let k: IconKey = (key.to_string(), IconWorker::bucket_size(size));
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = inner.map.get(&k)?.clone();
        // 刷新 LRU
        inner.order.retain(|x| x != &k);
        inner.order.push_back(k);
        Some(v.0)
    }

    pub fn pixels(&self, key: &str, size: u32) -> Option<IconPixels> {
        let k: IconKey = (key.to_string(), IconWorker::bucket_size(size));
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = inner.map.get(&k)?;
        Some(IconPixels {
            w: v.1,
            h: v.2,
            px: v.0.clone(),
        })
    }

    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }

    /// 降载：淘汰旧代次条目直至达到目标字节。
    pub fn trim_to(&self, target: usize, keep_generation: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while inner.bytes > target {
            let Some(evict) = inner.order.pop_front() else { break };
            let is_old = inner.map.get(&evict).map(|v| v.3 != keep_generation).unwrap_or(true);
            if is_old {
                if let Some(old) = inner.map.remove(&evict) {
                    inner.bytes = inner.bytes.saturating_sub(old.0.len() + 64);
                }
            } else {
                inner.order.push_back(evict);
                // 所有条目均为当前代次：强制淘汰队首
                if let Some(evict) = inner.order.pop_front() {
                    if let Some(old) = inner.map.remove(&evict) {
                        inner.bytes = inner.bytes.saturating_sub(old.0.len() + 64);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_respects_budget() {
        let cache = IconCache::new(4096);
        let data = Arc::new(vec![1u8; 2048]);
        for i in 0..10 {
            cache.insert(&format!("k{i}"), 64, data.clone(), 64, 64, 1);
        }
        assert!(cache.bytes() <= 4096);
        assert!(cache.pixels("k9", 64).is_some());
        assert!(cache.pixels("k0", 64).is_none());
        cache.trim_to(0, 1);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn normalize_keeps_alpha_premultiplied() {
        let sz = 32u32;
        let mut px = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..sz {
            for x in 0..sz {
                let i = ((y * sz + x) * 4) as usize;
                px[i + 2] = 200;
                px[i + 3] = 255;
            }
        }
        let (out, w, h) = normalize_and_premultiply(px, sz, sz, 48);
        assert_eq!((w, h), (48, 48));
        for p in out.chunks_exact(4) {
            assert!(p[2] <= p[3]);
        }
    }

    #[test]
    fn dark_monochrome_transparent_icon_gets_light_card_backdrop() {
        // 模拟 ChatGPT / Seer 类纯黑线稿透明图标（中心画黑色环形，其余全透明）
        let sz = 64u32;
        let mut px = vec![0u8; (sz * sz * 4) as usize];
        for y in 0..sz {
            for x in 0..sz {
                let dx = x as f32 - 31.5;
                let dy = y as f32 - 31.5;
                let dist = (dx * dx + dy * dy).sqrt();
                if (12.0..=20.0).contains(&dist) {
                    let i = ((y * sz + x) * 4) as usize;
                    px[i] = 20;
                    px[i + 1] = 20;
                    px[i + 2] = 20;
                    px[i + 3] = 255;
                }
            }
        }
        let (out, w, h) = normalize_and_premultiply(px, sz, sz, 64);
        assert_eq!((w, h), (64, 64));
        // 卡片中心透明镂空处应已被衬上柔和浅乳白底色，在深色 Dock 上清晰可见
        let center = ((32 * 64 + 32) * 4) as usize;
        assert!(out[center + 3] > 240);
        assert!(out[center + 2] > 220 && out[center + 1] > 220 && out[center] > 220);
    }
}
