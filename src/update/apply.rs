//! 原生自拷贝更新协调器（`--apply-update --parent-pid <pid> --setup <path> --target <exe>`）。
//!
//! 取代 PowerShell 更新脚本，峰值 ≤ 10 MB：
//! 等待旧主进程与守护进程退出 → 确认任务栏已恢复 → 备份旧 EXE → 静默执行已验签安装包
//! → 校验版本并拉起新版（任一步失败自动回滚，保持任务栏可见并弹窗提示）。

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST};

use crate::config::Store;
use crate::taskbar::{self, RecoveryRecord};
use crate::win32::wide;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 读取 PE 版本资源中的文件版本（主.次.修订.构建）。
pub fn file_version(path: &Path) -> Option<(u32, u32, u32, u32)> {
    unsafe {
        let p = wide(path);
        let size = GetFileVersionInfoSizeW(PCWSTR(p.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(p.as_ptr()), 0, size, buf.as_mut_ptr().cast()).ok()?;
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(buf.as_ptr().cast(), windows::core::w!("\\"), &mut ptr, &mut len).as_bool() {
            return None;
        }
        if ptr.is_null() || len < 20 {
            return None;
        }
        let v = std::slice::from_raw_parts(ptr as *const u32, 5);
        if v[0] != 0xFEEF_04BD {
            return None;
        }
        Some((v[2] >> 16, v[2] & 0xFFFF, v[3] >> 16, v[3] & 0xFFFF))
    }
}

fn notify(text: &str, error: bool) {
    let m = wide(text);
    unsafe {
        MessageBoxW(
            windows::Win32::Foundation::HWND::default(),
            PCWSTR(m.as_ptr()),
            windows::core::w!("WinDockFinder 更新"),
            MB_OK | MB_SETFOREGROUND | MB_TOPMOST | if error { MB_ICONERROR } else { MB_ICONINFORMATION },
        );
    }
}

/// 等待指定路径的 EXE 不再被占用（可重命名）。
fn wait_unlocked(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => {}
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn restore_backup(backup: &Path, target: &Path) {
    if !backup.exists() {
        return;
    }
    let _ = std::fs::remove_file(target);
    let _ = std::fs::rename(backup, target);
}

fn launch(target: &Path, args: &[&str]) {
    if !target.is_file() {
        return;
    }
    let _ = std::process::Command::new(target)
        .args(args)
        .current_dir(target.parent().unwrap_or(Path::new(".")))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// 更新协调器主流程。返回进程退出码。
pub fn run(parent_pid: u32, setup: &Path, target: &Path) -> i32 {
    crate::log::init(&crate::win32::data_dir(), "updater");
    crate::update::cleanup_stale_workdirs();

    // 1. 等待旧主进程退出（其退出前已撤销 AppBar、恢复任务栏并清除恢复记录）
    if !crate::win32::wait_process(parent_pid, Duration::from_secs(30)) {
        notify("WinDockFinder 旧版本未能退出，更新已取消。", true);
        launch(target, &["--settings"]);
        return 1;
    }
    // 2. 确认任务栏恢复可见；残留记录则兜底修复
    if !taskbar::wait_taskbar_visible(8_000) {
        let store = Store::open();
        if let Some(rec) = RecoveryRecord::load(&store) {
            rec.restore();
            RecoveryRecord::clear(&store);
        }
    }
    if !setup.is_file() {
        notify("更新安装包不存在，更新已取消。", true);
        launch(target, &["--background"]);
        return 1;
    }

    // 3. 备份旧 EXE（同卷重命名）
    if !wait_unlocked(target, Duration::from_secs(15)) {
        notify("旧版程序仍被占用，更新已取消。", true);
        launch(target, &["--background"]);
        return 1;
    }
    let backup = target.with_extension("exe.bak");
    let _ = std::fs::remove_file(&backup);
    let had_backup = std::fs::rename(target, &backup).is_ok();
    if !had_backup && target.exists() {
        notify("无法备份旧版程序，更新已取消。", true);
        launch(target, &["--background"]);
        return 1;
    }
    let install_dir = target.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

    // 4. 静默安装（/D 必须位于末尾且不加引号）
    let status = std::process::Command::new(setup)
        .arg("/S")
        .arg(format!("/D={install_dir}"))
        .current_dir(&install_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    // 5. 校验版本
    let want = file_version(setup);
    let got = if target.is_file() { file_version(target) } else { None };
    let ok = matches!(&status, Ok(s) if s.success()) && got.is_some() && want.map_or(true, |w| Some(w) == got);
    if ok {
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_file(setup);
        launch(target, &["--background"]);
        0
    } else {
        // 6. 回滚
        if had_backup {
            restore_backup(&backup, target);
        }
        let code = status.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1);
        notify(&format!("更新失败（安装程序退出码 {code}），已恢复旧版本。"), true);
        launch(target, &["--settings"]);
        1
    }
}
