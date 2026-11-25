//! 顶部栏系统状态项：时钟、输入法、网络、音量、电量（事件驱动 + 低频校对）。

use windows::Win32::Foundation::{HWND, LPARAM, SYSTEMTIME, WPARAM};
use windows::Win32::Globalization::{GetLocaleInfoW, LOCALE_SLANGUAGE};
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd, ImmReleaseContext, IME_CMODE_NATIVE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, HKL};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, PostMessageW};

pub const WM_APP_STATUS: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 30;

#[derive(Clone, Debug, Default)]
pub struct StatusSnapshot {
    pub clock: String,
    pub ime: String,
    pub network: bool,
    /// (音量 0..100, 静音)
    pub volume: Option<(i32, bool)>,
    /// (电量百分比, 接通电源)
    pub battery: Option<(u8, bool)>,
}

fn now_local() -> (i32, i32, i32, i32, i32, i32, i32) {
    // (年, 月, 日, 时, 分, 秒, 星期 0=周日)
    unsafe {
        let t: SYSTEMTIME = GetLocalTime();
        (
            t.wYear as i32,
            t.wMonth as i32,
            t.wDay as i32,
            t.wHour as i32,
            t.wMinute as i32,
            t.wSecond as i32,
            t.wDayOfWeek as i32,
        )
    }
}

/// 自定义时钟格式：M d E HH mm ss 12h（非 token 字符原样输出）。
pub fn format_clock(fmt: &str, t: (i32, i32, i32, i32, i32, i32, i32)) -> String {
    let (_, m, d, h, mi, s, wd) = t;
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let weekday = ["日", "一", "二", "三", "四", "五", "六"];
    while let Some(c) = chars.next() {
        match c {
            'M' => out.push_str(&m.to_string()),
            'd' => out.push_str(&d.to_string()),
            'E' => out.push_str(&format!("周{}", weekday[wd.clamp(0, 6) as usize])),
            'H' => {
                if chars.peek() == Some(&'H') {
                    chars.next();
                    out.push_str(&format!("{h:02}"));
                } else {
                    out.push_str(&h.to_string());
                }
            }
            'm' => {
                if chars.peek() == Some(&'m') {
                    chars.next();
                    out.push_str(&format!("{mi:02}"));
                } else {
                    out.push_str(&mi.to_string());
                }
            }
            's' => {
                if chars.peek() == Some(&'s') {
                    chars.next();
                    out.push_str(&format!("{s:02}"));
                } else {
                    out.push_str(&s.to_string());
                }
            }
            '1' if fmt.contains("12h") => {
                if chars.peek() == Some(&'2') {
                    chars.next();
                    if chars.peek() == Some(&'h') {
                        chars.next();
                        let hh = match h % 12 {
                            0 => 12,
                            x => x,
                        };
                        out.push_str(&hh.to_string());
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_tokens_and_literals() {
        let t = (2026, 11, 25, 9, 5, 7, 3);
        assert_eq!(format_clock("M月d日 E HH:mm", t), "11月25日 周三 09:05");
        assert_eq!(format_clock("HH:mm:ss", t), "09:05:07");
        assert_eq!(format_clock("12h:mm", t), "9:05");
    }
}

pub fn clock_string(fmt: &str) -> String {
    format_clock(fmt, now_local())
}

/// 当前秒（用于分钟对齐定时）。
pub fn current_second() -> i32 {
    now_local().5
}

fn language_name(lang_id: u32) -> String {
    unsafe {
        let mut buf = [0u16; 64];
        if GetLocaleInfoW(lang_id, LOCALE_SLANGUAGE, Some(&mut buf)) > 0 {
            let s = crate::win32::from_wide(&buf);
            if !s.is_empty() {
                return s;
            }
        }
    }
    format!("{lang_id:04X}")
}

fn language_tag(lang_id: u32) -> String {
    match lang_id & 0xFFFF {
        0x0804 | 0x0404 | 0x0C04 | 0x1004 => "中".into(),
        0x0409 => "英".into(),
        0x0411 => "日".into(),
        0x0412 => "한".into(),
        id => language_name(id),
    }
}

/// 输入法指示：中文布局时按转换模式显示「中/英」，其他显示语言标签。
pub fn ime_label(foreground: HWND) -> String {
    unsafe {
        let thread = if foreground.0.is_null() {
            windows::Win32::System::Threading::GetCurrentThreadId()
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let hkl: HKL = GetKeyboardLayout(thread);
        let lang = hkl.0 as u32 & 0xFFFF;
        if matches!(lang, 0x0804 | 0x0404 | 0x0C04 | 0x1004) {
            let ime = ImmGetDefaultIMEWnd(foreground);
            if !ime.is_invalid() {
                let ctx = ImmGetContext(ime);
                if !ctx.is_invalid() {
                    let mut conv = windows::Win32::UI::Input::Ime::IME_CONVERSION_MODE(0);
                    let ok = ImmGetConversionStatus(ctx, Some(&mut conv), None).as_bool();
                    let _ = ImmReleaseContext(ime, ctx);
                    if ok {
                        return if conv.0 & IME_CMODE_NATIVE.0 != 0 { "中".into() } else { "英".into() };
                    }
                }
            }
            return "中".into();
        }
        language_tag(lang)
    }
}

pub fn network_connected() -> bool {
    unsafe {
        let mut flags = windows::Win32::Networking::WinInet::INTERNET_CONNECTION(0);
        windows::Win32::Networking::WinInet::InternetGetConnectedState(&mut flags, 0).is_ok()
    }
}

/// 音量（COM 端点音量，调用线程需已初始化 COM）。
pub fn volume() -> Option<(i32, bool)> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        let level = endpoint.GetMasterVolumeLevelScalar().ok()?;
        let mute = endpoint.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        Some(((level * 100.0).round() as i32, mute))
    }
}

pub fn battery() -> Option<(u8, bool)> {
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        GetSystemPowerStatus(&mut status).ok()?;
        if status.BatteryFlag & 128 != 0 || status.BatteryLifePercent == 255 {
            return None;
        }
        let pct = status.BatteryLifePercent.min(100);
        Some((pct, status.ACLineStatus == 1))
    }
}

/// 快照（时钟总是刷新；其余按需）。
pub fn snapshot(fmt: &str, foreground: HWND, ime: bool, net: bool, vol: bool, bat: bool) -> StatusSnapshot {
    StatusSnapshot {
        clock: clock_string(fmt),
        ime: if ime { ime_label(foreground) } else { String::new() },
        network: if net { network_connected() } else { true },
        volume: if vol { volume() } else { None },
        battery: if bat { battery() } else { None },
    }
}

/// 后台刷新线程：按分钟对齐刷新时钟，网络/音量/电量低频校对。
pub fn spawn_status_thread(hwnd: HWND, interval_secs: u64) {
    // HWND 非 Send：先转为 isize 再跨线程传递，线程内重建
    let hwnd_raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("status".into())
        .spawn(move || {
            let hwnd = HWND(hwnd_raw as *mut _);
            loop {
            unsafe {
                let hr = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
                let need_uninit = hr.is_ok();
                let _ = PostMessageW(hwnd, WM_APP_STATUS, WPARAM(0), LPARAM(0));
                if need_uninit {
                    windows::Win32::System::Com::CoUninitialize();
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(interval_secs.max(5)));
            }
        })
        .ok();
}
