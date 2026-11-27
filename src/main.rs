//! CLI 参数解析与角色分发：主进程 / guard / worker / updater / 命令转发。
//!
//! 单一可执行文件通过子命令切换运行模式（见 DEVELOPMENT_PLAN.md 2.2 / 3.3）。

#![cfg(windows)]
// 非测试构建统一使用 Windows GUI 子系统，避免 Debug/Release 启动时弹出控制台或终端窗口
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod config;
mod dock;
mod guard;
mod hotkey;
mod log;
mod menu;
mod monitor;
mod settings;
mod startup;
mod taskbar;
mod topbar;
mod tray;
mod ui;
mod update;
mod win32;

use std::path::Path;

use windows::Win32::Foundation::HWND;

use app::{forward, IPC_RESTORE_TASKBAR, IPC_SETTINGS, IPC_TOGGLE_DOCK, IPC_TOGGLE_TOPBAR};

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

fn has(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = dispatch(&args);
    std::process::exit(code);
}

fn dispatch(args: &[String]) -> i32 {
    // 守护进程（保险丝）
    if has(args, "--guard") {
        let Some(pid) = arg_value(args, "--parent-pid").and_then(|v| v.parse::<u32>().ok()) else {
            return 2;
        };
        let hotkey = arg_value(args, "--hotkey").and_then(|s| hotkey::parse(&s));
        return guard::run(pid, hotkey);
    }

    // 应用重启协调：等待旧主进程与旧守护进程完全退出后再启动新实例，避免单实例锁与恢复记录竞态
    if has(args, "--restart-wait") {
        let Some(parent) = arg_value(args, "--parent-pid").and_then(|v| v.parse::<u32>().ok()) else {
            return 2;
        };
        if !win32::wait_process(parent, std::time::Duration::from_secs(15)) {
            log::init(&win32::data_dir(), "restart");
            log::log(&format!("--restart-wait 等待旧主进程({parent})退出超时"));
            return 1;
        }
        if let Some(guard_pid) = arg_value(args, "--guard-pid").and_then(|v| v.parse::<u32>().ok()).filter(|&p| p != 0) {
            let _ = win32::wait_process(guard_pid, std::time::Duration::from_secs(5));
        }
        return app::run(false);
    }

    // 短时图标提取工作进程
    if let Some(req) = arg_value(args, "--worker-icons") {
        dock::icons::worker_extract_from_file(Path::new(&req));
        return 0;
    }

    // 独立紧急恢复：清理残留状态并还原任务栏，同时通知运行中的主实例暂停隐藏
    if has(args, "--restore-taskbar") {
        log::init(&win32::data_dir(), "restore");
        let store = config::Store::open();
        match app::find_main_window() {
            Some(hwnd) if !hwnd.0.is_null() && forward(hwnd, IPC_RESTORE_TASKBAR) => {}
            _ => {
                if let Err(e) = taskbar::standalone_restore(&store) {
                    log::log(&format!("--restore-taskbar 失败: {e}"));
                    return 1;
                }
            }
        }
        return 0;
    }

    // 命令转发 / 瞬时调用
    let other: HWND = app::find_main_window().unwrap_or_default();
    if has(args, "--settings") {
        return forward_or_launch(other, IPC_SETTINGS, true);
    }
    if has(args, "--toggle-dock") {
        return forward_or_launch(other, IPC_TOGGLE_DOCK, false);
    }
    if has(args, "--toggle-topbar") {
        return forward_or_launch(other, IPC_TOGGLE_TOPBAR, false);
    }

    // 安装器钩子
    if has(args, "--installer-refresh-startup") {
        log::init(&win32::data_dir(), "installer");
        let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
        match startup::refresh_existing_shortcut() {
            Ok(()) => 0,
            Err(e) => {
                log::log(&format!("--installer-refresh-startup 失败: {e}"));
                1
            }
        }
    } else if has(args, "--installer-remove-startup") {
        log::init(&win32::data_dir(), "installer");
        let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
        match startup::remove_current_shortcut() {
            Ok(()) => 0,
            Err(e) => {
                log::log(&format!("--installer-remove-startup 失败: {e}"));
                1
            }
        }
    } else if has(args, "--apply-update") {
        let Some(parent) = arg_value(args, "--parent-pid").and_then(|v| v.parse::<u32>().ok()) else {
            return 2;
        };
        let Some(setup) = arg_value(args, "--setup") else { return 2 };
        let Some(target) = arg_value(args, "--target") else { return 2 };
        update::apply::run(parent, Path::new(&setup), Path::new(&target))
    } else {
        // 主进程
        app::run(has(args, "--settings"))
    }
}

fn forward_or_launch(other: HWND, cmd: usize, open_settings: bool) -> i32 {
    if !other.0.is_null() && forward(other, cmd) {
        return 0;
    }
    // 未运行：先启动主实例（--settings 同时打开设置窗口）
    app::run(open_settings)
}
