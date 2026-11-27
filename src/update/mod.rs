//! 版本检测（GitHub 优先 / CNB 回退）、流式下载、Ed25519 验签、启动原生更新协调器。
//!
//! 签名格式（v2）：Ed25519( b"WinDock update v2\0" || filename || 0 || sha256(setup) )。
//! 下载使用 64 KB 有限缓冲流式写盘并同步计算 SHA-256，全程内存增量 < 1 MB。

pub mod apply;
pub mod http;

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::win32::data_dir;

pub const WM_APP_UPDATE: u32 = WM_APP + 50;
pub const WM_APP_UPDATE_PROGRESS: u32 = WM_APP + 51;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const PUBLIC_KEY: &str = include_str!("../../assets/update-public-key.hex");
const SIGNATURE_CONTEXT: &[u8] = b"WinDock update v2\0";
const RELEASE_LIST_LIMIT: usize = 1024 * 1024;
const SETUP_LIMIT: u64 = 32 * 1024 * 1024;
const SIG_LIMIT: usize = 64;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static DOWNLOADING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    GitHub,
    Cnb,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Cnb => "腾讯云 CNB",
        }
    }

    fn from_name(s: &str) -> Self {
        if s == "cnb" { Self::Cnb } else { Self::GitHub }
    }

    fn key(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Cnb => "cnb",
        }
    }

    /// 固定官方路径；客户端忽略元数据中的任何外部跳转 URL。
    fn download(self, version: &str, name: &str) -> (&'static str, String) {
        match self {
            Self::GitHub => ("github.com", format!("/CrazyFigure/WinDock/releases/download/v{version}/{name}")),
            Self::Cnb => ("cnb.cool", format!("/crazy-figure/WinDock/-/releases/download/v{version}/{name}")),
        }
    }

    fn fallback(self) -> Self {
        match self {
            Self::GitHub => Self::Cnb,
            Self::Cnb => Self::GitHub,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ReleaseSection {
    pub title: String,
    pub items: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ReleaseNote {
    pub version: String,
    pub date: String,
    pub sections: Vec<ReleaseSection>,
}

/// 可安装更新：目标版本、渠道与所有未安装版本的更新日志。
#[derive(Clone, Debug)]
pub struct UpdateBundle {
    pub version: String,
    pub channel: Channel,
    pub notes: Vec<ReleaseNote>,
    /// 下载完成后的本地安装包路径（检测阶段为 None）。
    pub local_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum CheckResult {
    Latest,
    Available(Box<UpdateBundle>),
    Failed(String),
}

/// 内置更新日志（关于页展示）。
pub fn bundled_notes() -> Vec<ReleaseNote> {
    serde_json::from_slice::<Vec<ReleaseNote>>(include_bytes!("../../assets/release-notes.json")).unwrap_or_default()
}

#[derive(Deserialize)]
struct RemoteRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<RemoteAsset>,
}

#[derive(Deserialize)]
struct RemoteAsset {
    name: String,
    #[serde(default)]
    state: Option<String>,
}

/// 只接受三段正式版本，按数值比较。
pub fn stable_version(value: &str) -> Option<[u64; 3]> {
    let mut numbers = value.split('.');
    let mut next = || {
        let part = numbers.next()?;
        let number = part.parse::<u64>().ok()?;
        (number.to_string() == part).then_some(number)
    };
    let result = [next()?, next()?, next()?];
    numbers.next().is_none().then_some(result)
}

fn newer(a: [u64; 3], b: [u64; 3]) -> bool {
    a > b
}

pub fn asset_name(version: &str) -> String {
    format!("WinDock_{version}_windows_x64_setup.exe")
}

/// 解析 Release 正文 Markdown 为结构化日志分组。
pub fn parse_release_body(body: &str) -> Vec<ReleaseSection> {
    let mut sections: Vec<ReleaseSection> = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("<!--") {
            continue;
        }
        if let Some(title) = line.strip_prefix("##") {
            let title = title.trim_start_matches('#').trim();
            if !title.is_empty() {
                sections.push(ReleaseSection { title: title.to_string(), items: Vec::new() });
            }
            continue;
        }
        let item = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line).trim();
        if item.is_empty() {
            continue;
        }
        if sections.is_empty() {
            sections.push(ReleaseSection { title: "更新内容".into(), items: Vec::new() });
        }
        if let Some(last) = sections.last_mut() {
            last.items.push(item.to_string());
        }
    }
    sections.retain(|s| !s.items.is_empty());
    sections
}

fn release_date(published_at: Option<&str>) -> String {
    let prefix = published_at.unwrap_or("").get(..10).unwrap_or("");
    let b = prefix.as_bytes();
    let digits = |r: std::ops::Range<usize>| r.clone().all(|i| b.get(i).is_some_and(u8::is_ascii_digit));
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' && digits(0..4) && digits(5..7) && digits(8..10) {
        prefix.to_string()
    } else {
        String::new()
    }
}

/// 筛选最高可一键安装的正式版，并按版本号降序收集未安装版本日志。
fn collect_updates(releases: Vec<RemoteRelease>, channel: Channel, current: [u64; 3]) -> Option<UpdateBundle> {
    let mut candidates: Vec<([u64; 3], RemoteRelease, bool)> = releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.prerelease {
                return None;
            }
            let version = release.tag_name.strip_prefix('v')?;
            let parts = stable_version(version)?;
            if !newer(parts, current) {
                return None;
            }
            let setup = asset_name(version);
            let uploaded = |name: &str| {
                release.assets.iter().any(|a| a.name == name && a.state.as_deref().is_none_or(|s| s == "uploaded"))
            };
            let ok = uploaded(&setup) && uploaded(&format!("{setup}.sig"));
            Some((parts, release, ok))
        })
        .collect();
    let latest = candidates.iter().filter(|(_, _, ok)| *ok).max_by_key(|(p, _, _)| *p).map(|(p, _, _)| *p)?;
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    let notes = candidates
        .iter()
        .filter(|(p, _, _)| *p <= latest)
        .map(|(_, rel, _)| ReleaseNote {
            version: rel.tag_name.trim_start_matches('v').to_string(),
            date: release_date(rel.published_at.as_deref()),
            sections: parse_release_body(rel.body.as_deref().unwrap_or("")),
        })
        .collect();
    let version = candidates.iter().find(|(p, _, _)| *p == latest).map(|(_, r, _)| r.tag_name.trim_start_matches('v').to_string())?;
    Some(UpdateBundle { version, channel, notes, local_path: None })
}

fn check_channel(channel: Channel) -> Result<Option<UpdateBundle>, String> {
    let (host, path, accept) = match channel {
        Channel::GitHub => ("api.github.com", "/repos/CrazyFigure/WinDock/releases?per_page=30", w!("application/vnd.github+json")),
        Channel::Cnb => ("cnb.cool", "/crazy-figure/WinDock/-/releases?page=1&page_size=30", w!("application/json")),
    };
    let data = http::get(host, path, accept, RELEASE_LIST_LIMIT)?;
    let releases = serde_json::from_slice::<Vec<RemoteRelease>>(&data).map_err(|e| format!("发布列表格式错误：{e}"))?;
    let current = stable_version(VERSION).ok_or("当前不是正式版本")?;
    Ok(collect_updates(releases, channel, current))
}

/// 默认检查 GitHub，连接或接口失败时自动回退 CNB。
pub fn check() -> Result<(Option<UpdateBundle>, Channel), String> {
    match check_channel(Channel::GitHub) {
        Ok(bundle) => Ok((bundle, Channel::GitHub)),
        Err(github) => check_channel(Channel::Cnb)
            .map(|bundle| (bundle, Channel::Cnb))
            .map_err(|cnb| format!("GitHub：{github}；腾讯云 CNB：{cnb}")),
    }
}

/// 后台检测更新，结果通过 WM_APP_UPDATE（lParam = Box<CheckResult>）投递。
pub fn spawn_check(hwnd: HWND) {
    let target = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let result = match check() {
                Ok((Some(bundle), _)) => CheckResult::Available(Box::new(bundle)),
                Ok((None, _)) => CheckResult::Latest,
                Err(e) => CheckResult::Failed(e),
            };
            let boxed = Box::new(result);
            unsafe {
                let _ = PostMessageW(HWND(target as *mut _), WM_APP_UPDATE, WPARAM(0), LPARAM(Box::into_raw(boxed) as isize));
            }
        })
        .ok();
}

/// 解析公钥（64 字符 hex）。
fn public_key() -> Result<VerifyingKey, String> {
    let hex = PUBLIC_KEY.trim();
    if hex.len() != 64 {
        return Err("内置更新公钥格式错误".into());
    }
    let mut bytes = [0u8; 32];
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).map_err(|_| "内置更新公钥格式错误")?, 16)
            .map_err(|_| "内置更新公钥格式错误")?;
    }
    VerifyingKey::from_bytes(&bytes).map_err(|_| "内置更新公钥无效".into())
}

/// Ed25519 验签：context || filename || 0 || sha256_bytes。
fn verify(name: &str, sha256: &[u8], signature: &[u8]) -> Result<(), String> {
    let key = public_key()?;
    let signature = Signature::from_slice(signature).map_err(|_| "更新签名格式错误")?;
    let mut signed = Vec::with_capacity(SIGNATURE_CONTEXT.len() + name.len() + 1 + sha256.len());
    signed.extend_from_slice(SIGNATURE_CONTEXT);
    signed.extend_from_slice(name.as_bytes());
    signed.push(0);
    signed.extend_from_slice(sha256);
    key.verify_strict(&signed, &signature).map_err(|_| "更新包签名校验失败".into())
}

/// 暂存目录（当前用户私有）。
pub fn staging_dir() -> PathBuf {
    data_dir().join("updates")
}

/// 流式下载 + 验签，写入暂存目录。进度回调 (已接收字节, 总字节)。
/// 同一时刻只允许一个下载。
pub fn download(
    version: &str,
    channel: Channel,
    on_progress: impl Fn(u64, Option<u64>) + Send,
) -> Result<PathBuf, String> {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("已有更新正在下载".into());
    }
    let result = download_inner(version, channel, on_progress);
    DOWNLOADING.store(false, Ordering::SeqCst);
    result
}

fn download_inner(version: &str, channel: Channel, mut on_progress: impl Fn(u64, Option<u64>)) -> Result<PathBuf, String> {
    let name = asset_name(version);
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建更新暂存目录：{e}"))?;
    let setup_path = dir.join(&name);
    let sig_path = dir.join(format!("{name}.sig"));
    let fetch = |ch: Channel| -> Result<(), String> {
        let (shost, spath) = ch.download(version, &format!("{name}.sig"));
        let signature = http::get(shost, &spath, w!("application/octet-stream"), SIG_LIMIT)?;
        if signature.len() != 64 {
            return Err("更新签名长度错误".into());
        }
        let (host, path) = ch.download(version, &name);
        let tmp = dir.join(format!("{name}.part"));
        let mut file = std::fs::File::create(&tmp).map_err(|e| format!("创建安装包文件失败：{e}"))?;
        let mut hasher = Sha256::new();
        http::stream(host, &path, SETUP_LIMIT, |chunk, received, total| {
            hasher.update(chunk);
            file.write_all(chunk).map_err(|e| format!("写入安装包失败：{e}"))?;
            on_progress(received, total);
            Ok(())
        })?;
        file.flush().map_err(|e| format!("写入安装包失败：{e}"))?;
        drop(file);
        let digest = hasher.finalize();
        verify(&name, &digest, &signature)?;
        std::fs::write(&sig_path, &signature).ok();
        if setup_path.exists() {
            let _ = std::fs::remove_file(&setup_path);
        }
        std::fs::rename(&tmp, &setup_path).map_err(|e| format!("保存安装包失败：{e}"))?;
        Ok(())
    };
    fetch(channel).or_else(|first: String| {
        let fallback = channel.fallback();
        fetch(fallback).map_err(|second| format!("{}：{first}；{}：{second}", channel.name(), fallback.name()))
    })?;
    Ok(setup_path)
}

/// 下载完成后的安装包路径。
#[derive(Clone, Debug)]
pub struct Downloaded {
    pub path: PathBuf,
    pub version: String,
}

/// 后台流式下载；进度通过 WM_APP_UPDATE_PROGRESS（wParam = 万分比 0..=10000，失败为 usize::MAX）投递，
/// 完成后通过 WM_APP_UPDATE（lParam = Box<CheckResult>）投递 Available（含本地安装包路径）。
pub fn spawn_download(hwnd: HWND, mut bundle: UpdateBundle) {
    let target = hwnd.0 as isize;
    let version = bundle.version.clone();
    let channel = bundle.channel;
    std::thread::Builder::new()
        .name("update-download".into())
        .spawn(move || {
            let result = match download(&version, channel, |received, total| {
                let permille = match total {
                    Some(t) if t > 0 => ((received as u128 * 10_000) / t as u128).min(10_000) as usize,
                    _ => 0,
                };
                unsafe {
                    let _ = PostMessageW(HWND(target as *mut _), WM_APP_UPDATE_PROGRESS, WPARAM(permille), LPARAM(0));
                }
            }) {
                Ok(path) => {
                    unsafe {
                        let _ = PostMessageW(HWND(target as *mut _), WM_APP_UPDATE_PROGRESS, WPARAM(10_000), LPARAM(0));
                    }
                    bundle.local_path = Some(path);
                    CheckResult::Available(Box::new(bundle))
                }
                Err(e) => {
                    unsafe {
                        let _ = PostMessageW(HWND(target as *mut _), WM_APP_UPDATE_PROGRESS, WPARAM(usize::MAX), LPARAM(0));
                    }
                    CheckResult::Failed(e)
                }
            };
            let boxed = Box::new(result);
            unsafe {
                let _ = PostMessageW(HWND(target as *mut _), WM_APP_UPDATE, WPARAM(0), LPARAM(Box::into_raw(boxed) as isize));
            }
        })
        .ok();
}

/// 把自身复制到临时目录并以 `--apply-update` 启动原生更新协调器（零 PowerShell 依赖）。
pub fn start_installation(setup: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序：{e}"))?;
    let dir = std::env::temp_dir().join(format!("WinDock-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建更新工作目录：{e}"))?;
    let copy = dir.join("WinDock.exe");
    std::fs::copy(&exe, &copy).map_err(|e| format!("复制更新协调器失败：{e}"))?;
    let install_dir = exe.parent().ok_or("无法定位安装目录")?;
    std::process::Command::new(&copy)
        .arg("--apply-update")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--setup")
        .arg(setup)
        .arg("--target")
        .arg(&exe)
        .current_dir(install_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动更新协调器失败：{e}"))?;
    Ok(())
}

/// 清理上次更新遗留的临时工作目录（保留当前进程所在目录）。
pub fn cleanup_stale_workdirs() {
    let temp = std::env::temp_dir();
    let Ok(rd) = std::fs::read_dir(&temp) else { return };
    let current = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if (name.starts_with("WinDock-update-") || name.starts_with("WinDockFinder-update-"))
            && current.as_ref().map(|c| *c != ent.path()).unwrap_or(true)
        {
            let _ = std::fs::remove_dir_all(ent.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_highest_signed_release_and_collects_notes() {
        let make = |tag: &str, signed: bool, pre: bool| RemoteRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: pre,
            published_at: Some("2026-11-25T00:00:00Z".into()),
            body: Some("## Dock\n\n- 🚀 改进\n* 第二条".into()),
            assets: [asset_name(&tag[1..]), format!("{}.sig", asset_name(&tag[1..]))]
                .into_iter()
                .take(if signed { 2 } else { 1 })
                .map(|name| RemoteAsset { name, state: None })
                .collect(),
        };
        let releases = vec![
            make("v0.1.1", true, false),
            make("v0.2.0", false, false),
            make("v0.1.5", true, true),
            make("v0.1.0", true, false),
            make("v0.1.2", true, false),
        ];
        let bundle = collect_updates(releases, Channel::GitHub, [0, 1, 0]).unwrap();
        assert_eq!(bundle.version, "0.1.2");
        assert_eq!(bundle.notes.iter().map(|n| n.version.as_str()).collect::<Vec<_>>(), vec!["0.1.2", "0.1.1"]);
        assert_eq!(bundle.notes[0].sections[0].items, vec!["🚀 改进", "第二条"]);
        assert_eq!(bundle.notes[0].date, "2026-11-25");
    }

    #[test]
    fn stable_versions_compare_numerically() {
        assert!(newer([0, 10, 0], [0, 9, 9]));
        assert!(stable_version("1.2.3-beta").is_none());
        assert!(stable_version("01.2.3").is_none());
    }

    #[test]
    #[ignore = "需要连接 GitHub / CNB 网络"]
    fn live_check_and_stream_verify_work() {
        let (bundle, ch) = check().expect("live update::check failed");
        println!("check succeeded via {:?}: bundle={:?}", ch, bundle.as_ref().map(|b| &b.version));
        let cnb = check_channel(Channel::Cnb).expect("CNB check_channel failed");
        println!("CNB check_channel succeeded: {:?}", cnb.as_ref().map(|b| &b.version));

        // 顺带验证 CNB 与 GitHub 下载重定向及 Ed25519 验签（取当前可用更新版本或当前版本）。
        let target_ver = bundle.as_ref().map(|b| b.version.as_str()).unwrap_or(VERSION);
        for channel in [Channel::Cnb, Channel::GitHub] {
            let name = asset_name(target_ver);
            let (shost, spath) = channel.download(target_ver, &format!("{name}.sig"));
            let sig = http::get(shost, &spath, w!("application/octet-stream"), SIG_LIMIT)
                .unwrap_or_else(|e| panic!("{channel:?} sig download failed: {e}"));
            assert_eq!(sig.len(), 64);

            let (host, path) = channel.download(target_ver, &name);
            let mut hasher = Sha256::new();
            let mut got = 0u64;
            http::stream(host, &path, SETUP_LIMIT, |chunk, received, total| {
                hasher.update(chunk);
                got = received;
                if let Some(t) = total {
                    assert!(received <= t);
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{channel:?} setup stream failed: {e}"));
            assert!(got > 0);
            let digest = hasher.finalize();
            verify(&name, &digest, &sig).unwrap_or_else(|e| panic!("{channel:?} verify failed: {e}"));
            println!("{channel:?} stream + verify ok ({got} bytes)");
        }
    }
}
