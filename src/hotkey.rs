//! 全局快捷键：字符串规格解析（"Ctrl+Alt+Shift+D"）、显示名、注册/注销、录入。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeySpec {
    pub mods: u32,
    pub vk: u32,
}

/// 解析 "Ctrl+Alt+Shift+D" / "Ctrl+Alt+F12" / "Win+D" 形式的快捷键。
pub fn parse(spec: &str) -> Option<HotkeySpec> {
    let mut mods = 0u32;
    let mut vk: Option<u32> = None;
    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            return None;
        }
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL.0 as u32,
            "alt" => mods |= MOD_ALT.0 as u32,
            "shift" => mods |= MOD_SHIFT.0 as u32,
            "win" | "super" | "meta" => mods |= MOD_WIN.0 as u32,
            _ => {
                if vk.is_some() {
                    return None;
                }
                let upper = part.to_ascii_uppercase();
                let fkey = upper
                    .strip_prefix('F')
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| (1..=24).contains(n));
                let code = match fkey {
                    Some(n) => VK_F1.0 as u32 + n - 1,
                    None => match upper.as_str() {
                        "SPACE" => VK_SPACE.0 as u32,
                        "TAB" => VK_TAB.0 as u32,
                        "ENTER" => VK_RETURN.0 as u32,
                        "ESC" | "ESCAPE" => VK_ESCAPE.0 as u32,
                        "HOME" => VK_HOME.0 as u32,
                        "END" => VK_END.0 as u32,
                        "LEFT" => VK_LEFT.0 as u32,
                        "RIGHT" => VK_RIGHT.0 as u32,
                        "UP" => VK_UP.0 as u32,
                        "DOWN" => VK_DOWN.0 as u32,
                        "INSERT" => VK_INSERT.0 as u32,
                        "DELETE" => VK_DELETE.0 as u32,
                        "PAGEUP" => VK_PRIOR.0 as u32,
                        "PAGEDOWN" => VK_NEXT.0 as u32,
                        s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => {
                            s.as_bytes()[0] as u32
                        }
                        _ => return None,
                    },
                };
                vk = Some(code);
            }
        }
    }
    // 至少需要一个修饰键 + 一个主键，避免抢占普通按键。
    let vk = vk?;
    let plain_mods = mods & !(MOD_NOREPEAT.0 as u32);
    if plain_mods == 0 || plain_mods == MOD_SHIFT.0 as u32 {
        return None;
    }
    Some(HotkeySpec { mods: plain_mods, vk })
}

impl HotkeySpec {
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods & MOD_WIN.0 as u32 != 0 {
            parts.push("Win".into());
        }
        if self.mods & MOD_CONTROL.0 as u32 != 0 {
            parts.push("Ctrl".into());
        }
        if self.mods & MOD_ALT.0 as u32 != 0 {
            parts.push("Alt".into());
        }
        if self.mods & MOD_SHIFT.0 as u32 != 0 {
            parts.push("Shift".into());
        }
        parts.push(key_name(self.vk));
        parts.join("+")
    }

    /// 显示用（带空格）。
    pub fn display_pretty(&self) -> String {
        self.display().replace('+', " + ")
    }
}

/// 当前按下的修饰键（用于设置窗口录入）。
pub fn current_mods() -> u32 {
    unsafe {
        let mut m = 0u32;
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            m |= MOD_CONTROL.0 as u32;
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            m |= MOD_ALT.0 as u32;
        }
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            m |= MOD_SHIFT.0 as u32;
        }
        if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
            m |= MOD_WIN.0 as u32;
        }
        m
    }
}

pub fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        x if x == VK_SHIFT.0 as u32
            || x == VK_CONTROL.0 as u32
            || x == VK_MENU.0 as u32
            || x == VK_LWIN.0 as u32
            || x == VK_RWIN.0 as u32
    )
}

/// 读取系统按键名称，例如 "A"、"F5"、"Tab"。
pub fn key_name(vk: u32) -> String {
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW(((sc as i32) << 16) as i32, &mut buf);
        if n > 0 {
            let s = crate::win32::from_wide(&buf);
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
        if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
            return ((vk as u8) as char).to_string();
        }
        format!("VK_{vk}")
    }
}

/// 由当前键盘状态生成规格（设置窗口录入）。返回 None 表示组合不完整。
pub fn spec_from_current(vk: u32) -> Option<HotkeySpec> {
    let spec = HotkeySpec {
        mods: current_mods(),
        vk,
    };
    let plain = spec.mods & !(MOD_NOREPEAT.0 as u32);
    if plain == 0 || plain == MOD_SHIFT.0 as u32 {
        return None;
    }
    Some(spec)
}

/// 注册全局热键。成功返回 true。
pub fn register(hwnd: HWND, id: i32, spec: HotkeySpec) -> bool {
    unsafe {
        RegisterHotKey(
            hwnd,
            id,
            HOT_KEY_MODIFIERS(spec.mods | MOD_NOREPEAT.0 as u32),
            spec.vk,
        )
        .is_ok()
    }
}

pub fn unregister(hwnd: HWND, id: i32) {
    unsafe {
        let _ = UnregisterHotKey(hwnd, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects() {
        let s = parse("Ctrl+Alt+Shift+D").unwrap();
        assert_eq!(s.vk, 'D' as u32);
        assert_eq!(s.display(), "Ctrl+Alt+Shift+D");
        let f = parse("ctrl + f12").unwrap();
        assert_eq!(f.vk, VK_F12.0 as u32);
        assert!(parse("D").is_none());
        assert!(parse("Shift+D").is_none());
        assert!(parse("Ctrl+Nope").is_none());
        assert!(parse("Ctrl+").is_none());
    }
}
