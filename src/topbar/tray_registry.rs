//! Windows 11 通知区的只读元数据，用于首屏图标和托盘进程定位。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{BOOL, ERROR_NO_MORE_ITEMS, HWND, LPARAM, RECT, WIN32_ERROR};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{
    SHCreateMemStream, SHGetKnownFolderPath, Shell_NotifyIconGetRect, KF_FLAG_DONT_VERIFY,
    NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, IsHungAppWindow, HWND_MESSAGE,
};

use crate::taskbar::{canonical_exe_lower, process_image, window_pid};
use crate::ui::IconPixels;
use crate::win32::from_wide;

const SETTINGS: PCWSTR = w!("Control Panel\\NotifyIconSettings");
const MAX_SETTINGS: u32 = 512;
const MAX_ICON_BYTES: u32 = 256 * 1024;

/// 注册项和当前屏幕位置都有效时才使用该项，历史记录不会出现在顶部栏中。
pub(super) struct Registration {
    pub id: u64,
    pub exe: String,
    pub names: Vec<String>,
    pub initial_tip: Option<String>,
    pub pids: Vec<u32>,
    pub rect: RECT,
    pub overflow: bool,
    pub order: Option<usize>,
    pub pixels: Option<IconPixels>,
    identity: (isize, u32, GUID),
}

impl Registration {
    pub fn key(&self) -> String {
        format!("tray:{}", self.id)
    }

    pub fn title(&self) -> &str {
        self.names
            .first()
            .map(String::as_str)
            .unwrap_or("Windows 通知区域")
    }

    pub fn system(&self) -> bool {
        self.exe
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
    }

    /// 点击时直接核对目标的 Shell 坐标与显隐设置，防止使用已经调整过的旧槽位。
    pub fn current_placement(&self) -> Option<(RECT, bool)> {
        let (hwnd, uid, guid) = self.identity;
        let rect = active_rect(HWND(hwnd as *mut _), uid, guid)?;
        let key = settings()?;
        let child: Vec<_> = self.id.to_string().encode_utf16().chain([0]).collect();
        let overflow = dword(key.0, PCWSTR(child.as_ptr()), w!("IsPromoted")) != Some(1);
        Some((rect, overflow))
    }
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn settings() -> Option<RegistryKey> {
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, SETTINGS, 0, KEY_READ, &mut key)
            .ok()
            .ok()?
    };
    Some(RegistryKey(key))
}

fn string(key: HKEY, child: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            key,
            child,
            value,
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(from_wide(&buffer[..size as usize / 2]))
}

fn dword(key: HKEY, child: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            key,
            child,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(data)
}

fn binary(key: HKEY, child: PCWSTR, value: PCWSTR, limit: u32) -> Option<Vec<u8>> {
    let mut size = 0;
    unsafe {
        RegGetValueW(
            key,
            child,
            value,
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        if size == 0 || size > limit {
            return None;
        }
        let mut data = vec![0; size as usize];
        RegGetValueW(
            key,
            child,
            value,
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        data.truncate(size as usize);
        Some(data)
    }
}

fn parse_guid(text: &str) -> Option<GUID> {
    let hex: String = text
        .trim_matches(['{', '}'])
        .chars()
        .filter(|&c| c != '-')
        .collect();
    if hex.len() != 32 {
        return None;
    }
    Some(GUID::from_u128(u128::from_str_radix(&hex, 16).ok()?))
}

/// 部分应用使用 Windows 已知目录 GUID 存储可执行路径，展开后才能与实际进程路径比较。
fn executable_path(raw: String) -> String {
    let Some((guid, relative)) = raw
        .strip_prefix('{')
        .and_then(|value| value.split_once("}\\"))
        .and_then(|(guid, relative)| parse_guid(guid).map(|guid| (guid, relative)))
    else {
        return raw;
    };
    let path = unsafe { SHGetKnownFolderPath(&guid, KF_FLAG_DONT_VERIFY, None) };
    path.map(|path| {
        std::path::Path::new(&crate::win32::take_pwstr(path))
            .join(relative)
            .to_string_lossy()
            .into_owned()
    })
    .unwrap_or(raw)
}

/// 通知图标的回调窗口可以是普通隐藏窗或消息窗。每个进程只查一次实际映像路径。
fn windows_by_exe() -> HashMap<String, Vec<HWND>> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        let windows = &mut *(data.0 as *mut HashMap<u32, Vec<HWND>>);
        windows.entry(window_pid(hwnd)).or_default().push(hwnd);
        true.into()
    }
    let mut by_pid = HashMap::<u32, Vec<HWND>>::new();
    unsafe {
        let data = LPARAM(&mut by_pid as *mut _ as isize);
        let _ = EnumWindows(Some(collect), data);
        let mut previous = HWND::default();
        while let Ok(hwnd) = FindWindowExW(HWND_MESSAGE, previous, None, None) {
            collect(hwnd, data);
            previous = hwnd;
        }
    }
    let mut paths = HashMap::<String, Vec<HWND>>::new();
    for (pid, windows) in by_pid {
        let exe = process_image(pid);
        if !exe.is_empty() {
            paths.entry(exe).or_default().extend(windows);
        }
    }
    paths
}

/// 按可执行文件路径收集当前拥有窗口的进程 ID，供未绑定注册项的托盘回退识别归属。
pub(super) fn exe_pids(exe: &str) -> Vec<u32> {
    let key = canonical_exe_lower(exe);
    let mut pids: Vec<u32> = windows_by_exe()
        .get(&key)
        .into_iter()
        .flatten()
        .map(|&hwnd| window_pid(hwnd))
        .filter(|&pid| pid != 0)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn active_rect(hwnd: HWND, uid: u32, guid: GUID) -> Option<RECT> {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: uid,
        guidItem: guid,
    };
    let rect = unsafe { Shell_NotifyIconGetRect(&id).ok()? };
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

/// 使用 Windows 保存的真实托盘图像。只解码有尺寸上限的 PNG，并保存预乘透明像素。
fn decode_snapshot(factory: &IWICImagingFactory, data: &[u8]) -> Option<IconPixels> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    unsafe {
        let stream = SHCreateMemStream(Some(data))?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h).ok()?;
        if !(1..=512).contains(&w) || !(1..=512).contains(&h) {
            return None;
        }
        let scale = w.max(h);
        let (w, h) = if scale > 64 {
            ((w * 64 / scale).max(1), (h * 64 / scale).max(1))
        } else {
            (w, h)
        };
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&frame, w, h, WICBitmapInterpolationModeFant)
            .ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        let mut pixels = vec![0; (w * h * 4) as usize];
        converter
            .CopyPixels(std::ptr::null(), w * 4, &mut pixels)
            .ok()?;
        pixels
            .chunks_exact(4)
            .any(|px| px[3] > 0)
            .then(|| IconPixels {
                w,
                h,
                px: Arc::new(pixels),
            })
    }
}

/// 从 Explorer 的设置读取候选项，再由 Shell 查询实际注册状态，避免把历史图标当成活动项。
pub(super) fn active_registrations(
    tray: HWND,
    has_pixels: impl Fn(u64) -> bool,
    canceled: impl Fn() -> bool,
) -> Option<Vec<Registration>> {
    if unsafe { IsHungAppWindow(tray).as_bool() } {
        return None;
    }
    let key = settings()?;
    let by_exe = windows_by_exe();
    let order: HashMap<_, _> = binary(key.0, PCWSTR::null(), w!("UIOrderList"), MAX_SETTINGS * 8)
        .unwrap_or_default()
        .chunks_exact(8)
        .enumerate()
        .map(|(index, bytes)| (u64::from_le_bytes(bytes.try_into().unwrap()), index))
        .collect();
    let factory: Option<IWICImagingFactory> =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
    let mut seen = HashSet::new();
    let mut registered = Vec::new();
    for index in 0..MAX_SETTINGS {
        if canceled() {
            return None;
        }
        let mut subkey = [0u16; 32];
        let mut len = subkey.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                PWSTR(subkey.as_mut_ptr()),
                &mut len,
                None,
                PWSTR::null(),
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != WIN32_ERROR(0) {
            continue;
        }
        let Ok(id) = from_wide(&subkey).parse::<u64>() else {
            continue;
        };
        let child = PCWSTR(subkey.as_ptr());
        let Some(exe) = string(key.0, child, w!("ExecutablePath"))
            .filter(|s| !s.is_empty())
            .map(executable_path)
        else {
            continue;
        };
        let exe_key = canonical_exe_lower(&exe);
        let file = exe_key.rsplit(['\\', '/']).next().unwrap_or(&exe_key);
        if matches!(file, "windock.exe" | "windockfinder.exe") {
            continue;
        }
        let windows = by_exe.get(&exe_key).map(Vec::as_slice).unwrap_or_default();
        let (rect, pids, identity) = if let Some(guid) = string(key.0, child, w!("IconGuid"))
            .as_deref()
            .and_then(parse_guid)
            .filter(|guid| *guid != GUID::zeroed())
        {
            let Some(rect) = active_rect(HWND::default(), 0, guid) else {
                continue;
            };
            if !seen.insert((0, u128::from(guid))) {
                continue;
            }
            (rect, windows.iter().map(|&hwnd| window_pid(hwnd)).collect(), (0, 0, guid))
        } else {
            let Some(uid) = dword(key.0, child, w!("UID")) else {
                continue;
            };
            let Some((hwnd, rect)) = windows
                .iter()
                .find_map(|&hwnd| active_rect(hwnd, uid, GUID::zeroed()).map(|rect| (hwnd, rect)))
            else {
                continue;
            };
            if !seen.insert((hwnd.0 as isize, u128::from(uid))) {
                continue;
            }
            let mut owners: Vec<_> = windows.iter().map(|&window| window_pid(window)).collect();
            owners.sort_unstable();
            owners.dedup();
            (rect, owners, (hwnd.0 as isize, uid, GUID::zeroed()))
        };
        let mut names = Vec::new();
        let initial_tip = string(key.0, child, w!("InitialTooltip")).filter(|tip| !tip.trim().is_empty());
        if let Some(tip) = &initial_tip {
            names.push(tip.clone());
        }
        if file != "explorer.exe" {
            names.push(crate::monitor::exe_display_name(&exe));
            names.push(file.trim_end_matches(".exe").to_string());
        }
        let pixels = if has_pixels(id) {
            None
        } else {
            factory.as_ref().and_then(|factory| {
                let data = binary(key.0, child, w!("IconSnapshot"), MAX_ICON_BYTES)?;
                decode_snapshot(factory, &data)
            })
        };
        let overflow = dword(key.0, child, w!("IsPromoted")) != Some(1);
        registered.push(Registration {
            id,
            exe,
            names,
            initial_tip,
            pids,
            rect,
            overflow,
            order: order.get(&id).copied(),
            pixels,
            identity,
        });
    }
    registered.sort_by_key(|item| (item.overflow, item.order.unwrap_or(usize::MAX), item.id));
    Some(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_the_known_folder_in_packaged_application_paths() {
        let raw = r"{6D809377-6AF0-444B-8957-A3773F02200E}\WindowsApps\Example\Tray.exe";
        let exe = executable_path(raw.into());
        assert!(std::path::Path::new(&exe).is_absolute());
        assert!(exe.ends_with(r"WindowsApps\Example\Tray.exe"));
    }
}
