# WinDockFinder 开发计划

> 项目署名：**CrazyFigure** · 目标平台：**Windows 11 x64** · 源仓库：[GitHub](https://github.com/CrazyFigure/WinDockFinder) · 镜像仓库：[CNB](https://cnb.cool/crazy-figure/WinDockFinder)

WinDockFinder 是一款面向 Windows 11 的轻量原生桌面工具，使用 **Rust + Win32 + DWM/GDI** 复刻 macOS 桌面核心体验：

- **底部 Dock**：紧贴图标宽度的紧凑悬浮毛玻璃胶囊（两侧完全穿透展示桌面与其他应用），默认「最大化智能避让」，悬停触发波浪放大与平滑浮出。
- **顶部菜单栏（Finder）**：沿屏幕顶部铺满的轻量状态栏，接管并收纳隐藏任务栏后的系统托盘图标，支持**随最大化窗口自动取色融合**以获得视觉一体感。
- **桌面接管与安全兜底**：安装后默认随当前用户登录启动并隐藏 Windows 任务栏，退出或异常崩溃时由极简守护进程可靠恢复任务栏。

本文档面向下游开发 Agent 编写，包含产品范围、核心技术方案、模块边界、数据与 CLI 契约、实施阶段与发布门禁。

---

## 0. 硬性质量红线

| 指标 | 硬性要求 | 统计与验证口径 |
| --- | --- | --- |
| 内存占用 | **绝对不超过 100 MB**（`100,000,000` 字节，约 `95.37 MiB`） | 同一时刻 WinDockFinder 主进程、守护进程、短时辅助进程及更新/安装进程求和，`WorkingSetSize` 与 `PrivateUsage` **分别** ≤ 100 MB；**85 MB 触发内部主动降载**。 |
| 稳定性 | **零崩溃、零不可恢复卡死、零任务栏永久失踪** | 禁止任何 panic 跨越 Win32/COM/Hook FFI 边界；禁止对主 UI 进程套用会触发 `STATUS_NO_MEMORY` 的 OS 强制内存硬杀限额。 |
| 桌面任务栏生命周期 | 启动默认隐藏，退出/关闭必恢复 | 默认开启登录启动与隐藏任务栏（均可在设置/托盘中关闭）；异常强杀、卡死、Explorer 重启均有闭环恢复。 |
| 视觉与流畅度 | 仿照 macOS、流畅美观、自定义丰富 | 默认 DWM 原生毛玻璃、VSync/高精度动画驱动（静止 0 唤醒）、抗锯齿图标与圆角、100ms 延迟自定义 Tooltip、兼容 100%–200% 混合 DPI 及浅/深色主题。 |
| 安装与分发 | 独立 EXE 安装包、一键签名更新、双渠道镜像 | 当前用户安装无需管理员权限；GitHub Actions 自动构建、Ed25519 签名、同步 CNB；应用内支持检测更新、更新日志与流式一键更新。 |

---

## 1. 产品范围与交互设计

> 本文中 **Finder** 指 macOS 风格的顶部菜单栏及桌面快捷控制入口（不包含重写整个 Windows 文件资源管理器）；文件夹与磁盘入口调用 Windows 默认方式打开，Dock 内提供「文件管理器」与「回收站」图标。

### 1.1 底部 Dock

1. **紧凑悬浮形态（无整行背景条）**：
   - 仅包裹图标区与功能区的圆角毛玻璃胶囊，**胶囊左右两侧完全透明穿透**，直接露出壁纸或其他应用窗口。
   - 默认采用 **Windows 11 原生 DWM 毛玻璃**（设置中支持调节不透明度、圆角、边框微光或切换轻量半透明模式）。
   - 图标悬停放大时允许图标**向上突破胶囊上边界**而不被裁切，胶囊宽度随当前总图标宽度与放大波浪平滑伸缩。
2. **默认智能隐藏与显隐模式**：
   - **默认模式：智能避让（Intelligent Auto-Hide）**
     - 当前屏幕**无最大化窗口**（且无普通窗口遮挡 Dock 区域）时，Dock 常驻显示；
     - 当有软件窗口**最大化**（或重叠 Dock 区域）时，Dock 自动平滑下沉隐藏，将底部空间完整让给软件；
     - 鼠标移至屏幕底部触发边缘（默认 3px 高、可配停留延迟 0–400ms）时，Dock 平滑浮出显示；鼠标移出 Dock 区域后再次自动隐藏。
   - 设置中同时支持切换为：**始终显示**、**始终自动隐藏**。
   - 遇到全屏游戏/视频（独占或无边框全屏）时默认静默让位，不误触发弹出。
3. **分区布局与核心能力**：
   - **左侧应用区**：固定应用 + 分隔线（可选合并）+ 当前运行中的未固定应用。采用稳定应用标识（AUMID / 规范化路径）归并固定项与运行窗口，运行中的应用下方显示状态指示点。
   - **右侧功能区**（以竖向细分隔线隔开）：
     - **快捷文件夹 / 堆栈**：支持拖入常用文件夹，点击可展开轻量列表/网格浮层（或按设置直接打开资源管理器）；
     - **回收站（Trash）**：实时区分空/满图标状态，支持将桌面/资源管理器文件拖放至回收站直接移入回收站，右键提供「打开回收站」「清空回收站」；
     - **可选联动入口**：支持一键添加/联动 [WinLaunchpad](https://github.com/CrazyFigure/WinLaunchpad)（调用 `WinLaunchpad.exe --toggle` 作为启动台图标）。
   - **完整交互**：
     - 左键单击启动或激活窗口；同应用多窗口时弹出轻量窗口标题选择菜单（支持关闭单个窗口）；
     - 拖拽重排固定项顺序，拖出 Dock 区域一段距离释放可取消固定；拖入 `.exe`、`.lnk`、文件、文件夹添加快捷项；
     - 右键菜单：打开/新建窗口、固定/取消固定、修改显示名称与自定义图标、以管理员身份运行、打开所在位置、退出应用；空白/产品入口右键提供「设置」「显示/隐藏 Windows 任务栏」「退出 WinDockFinder」；
     - 项目超出屏幕可用宽度时自动缩放图标尺寸，达到最小阈值后启用溢出菜单；
     - 悬停 100ms 显示自定义胶囊 Tooltip（应用名），减少动态效果时切换瞬时过渡；完整支持键盘导航。

### 1.2 顶部菜单栏（Finder）

1. **布局与常驻功能**：
   - 沿屏幕顶部横向铺满（默认高 28–32dp，可配置），默认通过 AppBar 预留顶部工作区使最大化窗口紧贴顶部栏下沿、绝不遮挡软件标题栏。
   - **左侧**：
     - 原创产品图标与**产品菜单**（关于 WinDockFinder、系统偏好设置、显示/隐藏 Windows 任务栏、显示/隐藏桌面图标、快速锁屏/睡眠/重启/关机、任务管理器、退出 WinDockFinder）；
     - **当前前台活动应用名称**（桌面空闲时显示「桌面」），点击展开常用上下文快捷动作（最小化/最大化/居中/关闭窗口，桌面态下提供快速打开此电脑/桌面/下载/文档）。
   - **右侧**：
     - **系统托盘接管区**（见下文 1.3）；
     - **系统状态项**：输入法指示、网络、音量、电量（无电池设备自动隐藏）、控制中心/系统快捷入口、日期与时钟（按分钟刷新，点击可打开对应 Windows 设置或日历浮层）。
   - 空间不足时按优先级自动收起低优先状态项，保证产品菜单、展开按钮与时钟始终可达。
2. **随最大化窗口自动取色融合（沉浸式适配）**：
   - **桌面 / 非最大化态**：顶部菜单栏呈现通透 DWM 毛玻璃质感，文字与单色图标根据壁纸顶部采样亮度自动适配浅色或深色。
   - **软件最大化态（核心特性，默认开启）**：
     - 当当前显示器存在前台最大化窗口时，顶部栏自动检测并采样该窗口标题栏顶边色彩，在 **150ms 内平滑插值过渡**为与该最大化窗口标题栏一致的背景色，消除顶部栏与最大化窗口之间的色差割裂感；
     - 同时根据目标背景色的相对亮度（WCAG luminance）自动切换顶部栏文字、状态图标为高对比度白色或深灰色；
     - 当最大化窗口切走、还原、最小化或关闭时，立即平滑恢复为毛玻璃态；
     - 遇到全屏视频/游戏时隐藏顶部栏让位，不引起后台窗口反复重排。
   - 设置项中支持开关「最大化窗口自动取色适配」、微调过渡速度以及配置不参与取色的应用排除列表。

### 1.3 隐藏任务栏后的系统托盘接管与展开收纳

当 Windows 桌面任务栏被隐藏后，所有第三方系统托盘图标（微信、QQ、代理软件、截图工具、安全软件等）统一由顶部菜单栏右侧接管：

1. **顶部栏右侧常驻一个「展开/收起」按钮**（例如 `<` / `⌄` 胶囊图标，悬停有 100ms Tooltip「托盘应用」）。
2. **点击展开按钮**：弹出 macOS 风格的紧凑垂直浮层列表：
   - **每一行展示一个系统托盘应用**，包含：应用图标 + 应用名称/提示文案 + 是否展示到顶部栏的切换态（勾选/固定指示）。
   - **点击某一行应用图标（或勾选区域）**：切换该应用的显示归属——
     - **开启（展示到顶部）**：该应用图标立即出现在顶部菜单栏右侧，且按用户选择持久化记忆；
     - **取消（收进折叠区）**：该应用图标从顶部菜单栏移除，重新收纳回展开面板中。
   - **原生交互透传**：
     - 在顶部栏已展示的托盘图标上，支持**左键单击/双击**激活对应应用、**右键单击**弹出该应用的原生托盘菜单；
     - 在展开面板的每一行中，同样提供直接操作（如右键或行尾操作打开原应用窗口/菜单），无需先固定到顶部即可使用。
3. **本应用托盘与恢复入口兜底**：
   - WinDockFinder 自身同时注册标准 Windows 托盘图标（`Shell_NotifyIconW`，含完整右键菜单：显示/隐藏 Dock、显示/隐藏顶部栏、暂时显示/隐藏 Windows 任务栏、设置、随 Windows 登录启动、退出）。
   - 当 Windows 任务栏隐藏时，上述全部控制入口在顶部栏产品菜单、展开面板顶部与 Dock 产品入口中 100% 可达；当任务栏显示时，原生托盘右键菜单立即可用。

### 1.4 设置窗口（仿 macOS 风格与丰富自定义）

采用仿 macOS 系统设置布局：**左侧分组导航栏 + 右侧圆角卡片分区**，交互元素统一具备 Hover / Active / Focus 状态反馈与 **100ms 延迟自定义 Tooltip**（不使用原生 `title`），界面文案精简（不堆砌副标题和无意义的计数徽标）。视觉调整支持**实时预览**，支持恢复单页默认设置。

| 导航页 | 自定义配置项 |
| --- | --- |
| **通用** | 随 Windows 登录启动（同步检测任务管理器 `StartupApproved` 禁用态）、启动时隐藏 Windows 任务栏、双栏显隐全局快捷键、任务栏紧急恢复快捷键、启动台联动路径（可选接入 WinLaunchpad）。 |
| **Dock** | 显示位置（仅主屏 / 跟随鼠标屏 / 所有屏）、显隐模式（默认**智能避让最大化窗口** / 始终显示 / 始终自动隐藏）、触发边缘高度与弹出/隐藏延迟、底部悬浮间距、基础图标尺寸、图标间距、悬停放大开关与放大倍率、波浪影响范围、动画流畅度/减少动态效果、显示运行中未固定应用、显示运行指示点、显示回收站、显示文件夹堆栈、固定项排序与自定义名称/图标。 |
| **顶部栏 (Finder)** | 启用开关、多屏策略（主屏 / 所有屏）、栏高（24–38dp）、**最大化窗口自动取色适配开关与排除名单**、前台应用名称显示、时钟格式（24h/12h、星期、秒数）、系统状态项（输入法/网络/音量/电量）显隐与排序、托盘应用固定/收纳管理列表、全屏让位开关。 |
| **外观** | 主题模式（跟随系统 / 浅色 / 深色）、材质模式（默认 **Win11 毛玻璃 Acrylic** / 轻量半透明）、背景不透明度、毛玻璃叠加色调、Dock 胶囊圆角与微光边框、强调色、图标统一圆角裁切（可复用 WinLaunchpad 圆角算法）。 |
| **关于与更新** | 当前版本、检查更新（手动检测 + 可选低频自动检测）、更新渠道状态（GitHub / CNB）、一键更新按钮与进度条、按版本可展开的更新日志（源自内置 `assets/release-notes.json`）。 |

---

## 2. 技术架构与关键难点攻关

### 2.1 技术栈与复用策略

- **语言与依赖**：Rust stable（`x86_64-pc-windows-msvc`）+ `windows = "=0.58.0"`（按需启用 Feature，提交 `Cargo.lock`）+ `serde`/`serde_json` + `ed25519-dalek` + `sha2` + `tempfile`。Release 编译配置 `opt-level = "s"`、`lto = true`、`codegen-units = 1`、`strip = true`。
- **直接复用 [WinLaunchpad](file:///c:/Software/WorkSpace/WinLaunchpad) 已验证资产**：
  - [src/ui.rs](file:///c:/Software/WorkSpace/WinLaunchpad/src/ui.rs)：32bpp DIB 软件抗锯齿绘图原语（SDF 圆角矩形、抗锯齿圆/线、双线性图标缩放、阴影预烘焙、现代 Toggle/Checkbox/Button/Scrollbar、100ms Tooltip 浮层），**设置窗口和自绘菜单直接基于此库构建 macOS 视觉风格**，不走笨重的原生 Win32 控件重绘，也不用 WebView/Electron；
  - [src/startup.rs](file:///c:/Software/WorkSpace/WinLaunchpad/src/startup.rs)：`FOLDERID_Startup` 快捷方式管理 + 读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder` 二进制首字节奇偶位识别系统级禁用状态；
  - [src/update/http.rs](file:///c:/Software/WorkSpace/WinLaunchpad/src/update/http.rs)、[build.rs](file:///c:/Software/WorkSpace/WinLaunchpad/build.rs)、[installer/windows.nsi](file:///c:/Software/WorkSpace/WinLaunchpad/installer/windows.nsi)、[tools/package-windows.ps1](file:///c:/Software/WorkSpace/WinLaunchpad/tools/package-windows.ps1)、[scripts/sync-cnb-release.mjs](file:///c:/Software/WorkSpace/WinLaunchpad/scripts/sync-cnb-release.mjs)、[.github/workflows/release.yml](file:///c:/Software/WorkSpace/WinLaunchpad/.github/workflows/release.yml)：复用成熟的 WinHTTP、RC 版本资源嵌入、NSIS 打包与 GitHub → CNB 同步链条。

### 2.2 精简进程模型（单 EXE 多模式）

为了在 **100 MB 整组内存红线**下兼顾极高稳定性，只编译**单一可执行文件 `WinDockFinder.exe`**，通过 CLI 子命令切换运行模式：

| 进程角色 | 启动参数 | 生命周期与职责 | 内存预算 |
| --- | --- | --- | --- |
| **主进程（UI）** | 无参 / `--background` | 单实例主进程。负责消息循环、Dock 与顶部栏渲染、窗口事件监听、正常任务栏显隐、托盘与设置窗口、配置持久化。 | 常驻空闲 ≤ 35 MB，活跃交互 ≤ 60 MB |
| **守护进程（保险丝 Watchdog）** | `--guard --parent-pid <pid> --hotkey <spec>` | **极简低底噪常驻进程**：只链接最小 `kernel32`+`user32`（不初始化 COM/GDI/WIC/Shell）。持有主进程句柄 `WaitForSingleObject`，并监听 UI 心跳（Named Event/共享时间戳）与紧急恢复快捷键（默认 `Ctrl+Alt+Shift+F12`）。**正常路径零 IPC 干预**；仅当主进程崩溃、强杀、卡死超时或触发紧急快捷键时，立即读取恢复记录还原 Windows 任务栏与工作区。 | 常驻 ≤ 6 MB |
| **短时工作模式（可选隔离）** | `--worker-icons` / `--worker-scan` | 仅在执行高风险第三方 Shell 扩展（如批量解析不明 `.lnk`/UWP 清单）时按需拉起，单并发、带超时强杀、任务完成立即退出释放内存，防止劣质 Shell 扩展拖垮或泄漏主进程。 | 峰值 ≤ 18 MB（用完即退） |
| **原生更新协调模式** | `--apply-update --parent-pid <pid> --setup <path> --target <exe>` | **取代 PowerShell 更新脚本**：将当前 `WinDockFinder.exe` 复制一份到临时目录执行，避免拉起 PowerShell 5.1 带来 40–60 MB 瞬时峰值。等待旧主进程与守护进程退出并确认任务栏恢复 → 备份旧 EXE → 静默执行已验签 NSIS 安装包 → 校验版本并拉起新版（失败自动回滚）。 | 峰值 ≤ 10 MB |
| **命令转发 / 紧急修复** | `--settings` / `--toggle-dock` / `--toggle-topbar` / `--restore-taskbar` / `--installer-refresh-startup` / `--installer-remove-startup` | 瞬时短命调用。转发命令至当前会话已有主实例后立刻退出；`--restore-taskbar` 直接独立执行任务栏恢复并清理残留状态文件，作为开始菜单及用户自助兜底入口。 | < 8 MB（毫秒级退出） |

### 2.3 五大核心技术难点与工程方案

#### 难点 1：Windows 11 任务栏可靠隐藏、防回弹与零死区恢复

1. **安全前置门禁**：主进程必须先完成顶部栏/Dock 窗口创建、守护进程握手与恢复记录落盘（`taskbar_recovery.json`，记录 Explorer PID、各屏任务栏原始可见性、系统原始 `ABS_AUTOHIDE` 状态），确认至少存在一个可见桌面入口和有效紧急快捷键后，才执行任务栏隐藏。
2. **多屏任务栏枚举与身份校验**：枚举当前桌面所有 `Shell_TrayWnd`（主屏）和 `Shell_SecondaryTrayWnd`（副屏），校验其归属进程确为本会话 `explorer.exe`。绝不向 Explorer 注入 DLL，也不篡改未公开注册表项。
3. **防 Explorer 自动回弹**：注册进程外 `SetWinEventHook(EVENT_OBJECT_SHOW)` 监听上述任务栏句柄。处于「任务栏隐藏态」且非用户主动暂时显示时，若 Explorer 因通知/切屏偷偷将任务栏设回可见，立即异步重新 `ShowWindowAsync(hwnd, SW_HIDE)`（加 50ms 防抖与计数熔断，防止异常循环）。
4. **底部 48px 工作区死区消除**：
   - 在 **Dock 智能避让 / 自动隐藏**模式下，记录系统原有 `SHAppBarMessage(ABM_GETSTATE)` 值；若原为常驻，可按设置同步切换任务栏状态或协同隐藏，使最大化窗口无缝延伸至屏幕最底端；退出或恢复任务栏时按 `taskbar_recovery.json` 100% 还原原值。
   - **P0 原型必测项**：在 Windows 11 23H2 与 24H2 实机分别验证 `ABM_SETSTATE(ABS_AUTOHIDE)` + `SW_HIDE` 组合对工作区、`Win` 键开始菜单、`Alt+Tab` 与恢复一致性的表现。
5. **全故障恢复矩阵**：
   - 正常退出 / 卸载 / 更新：撤销所有顶部栏 AppBar（`ABM_REMOVE`）→ 还原任务栏 `SW_SHOW` 与原始 `ABS_AUTOHIDE` → 通知守护进程正常退出 → 清除恢复标记。
   - 主进程被强杀 / 崩溃：守护进程 `WaitForSingleObject` 毫秒级唤醒 → 5 秒内恢复任务栏。
   - 主进程 UI 卡死：守护进程心跳超时（默认 10 秒，≤ 15 秒完成恢复）→ 恢复任务栏并通知用户。
   - 守护进程意外退出：主进程检测守护句柄信号，先恢复任务栏或自动重建守护，避免失去保险丝。
   - Explorer 重启：监听 `TaskbarCreated` 广播，重建托盘、重登 AppBar 并按当前开关重应用隐藏状态。
   - 双进程同杀 / 断电：保留磁盘 `taskbar_recovery.json`，下次登录启动或点击开始菜单「恢复 Windows 任务栏」（`--restore-taskbar`）自动修复。异常恢复后本会话自动暂停隐藏，等待用户手动开启。

#### 难点 2：Dock 紧凑毛玻璃胶囊 + 悬停波浪突破边界

1. **双层协同窗口方案（P0 首选验证）**：
   - **底层毛玻璃胶囊窗（Backdrop Window）**：无边框工具窗（`WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST`），启用 Windows 11 原生 DWM Acrylic（`SetWindowCompositionAttribute` / `DWMWA_SYSTEMBACKDROP_TYPE`）与原生圆角（`DWMWA_WINDOW_CORNER_PREFERENCE`），尺寸仅覆盖图标胶囊区域（两侧无窗口、完全穿透），随图标总宽动态平滑 resize，几乎不占 CPU 与内存。
   - **顶层图标交互窗（Overlay Layered Window）**：同 Z-order 紧贴其上方，`WS_EX_LAYERED` + 32bpp 预乘 Alpha DIB + `UpdateLayeredWindow`，窗口高度向上多预留最大放大增量 + Tooltip 高度，鼠标透明区域 Alpha=0 自动点击穿透。所有图标、波浪放大动画、阴影、运行指示点、100ms Tooltip 均在此层高质量抗锯齿绘制，图标放大向上弹出胶囊顶部时自然顺滑、绝不裁切。
   - **低耗回退方案（P0 同步备选）**：若部分老旧显卡驱动下双窗口同步 resize 存在轻微滞后，可在设置或驱动黑名单中切换为单层 `UpdateLayeredWindow`（半透明磨砂底板或壁纸区域缓存模糊），保证零撕裂。
2. **丝滑物理动画与静止零功耗**：
   - 悬停放大采用 macOS 风格的余弦/高斯波形放大曲线（鼠标中心图标最大，邻近 2–3 个图标平滑衰减），使用弹簧阻尼插值（基于 `QueryPerformanceCounter` 真实 `dt`）驱动图标位置、缩放与胶囊宽度；
   - 动画期间采用独立轻量 VSync 信号线程（`DwmFlush` 发消息给 UI 线程）或 `CreateWaitableTimerEx(HIGH_RESOLUTION)` 驱动，动画收敛即彻底停止唤醒（空闲 CPU ≤ 0.5%）。

#### 难点 3：隐藏任务栏后的系统托盘镜像与展开收纳

1. **零注入采集方案（不注入 Explorer）**：
   - 优先使用 **Windows UI Automation（`IUIAutomation`）** 枚举本会话 `Shell_TrayWnd` 通知区域及 Win11 托盘溢出窗口（`TopLevelWindowForOverflowXamlIsland` / `NotifyIconOverflowWindow`）中的系统托盘按钮项，提取每项的 `Name`、关联窗口/进程标识（生成稳定 `tray_item_key`，用于持久化记忆「固定顶部 / 收进折叠区」）与图标图像。
   - 图标获取分级策略：① 通过关联进程/句柄或 UIA 元素信息提取高清晰度进程图标（WIC 缓存）；② 必要时在不可见刷新周期内安全采样图标位图；③ 受限高权限/UWP 无法读取时提供带应用首字母的优雅兜底徽标，绝不报错崩溃。
   - **交互透传策略**：用户点击顶部栏或展开面板中的托盘应用时，通过 UIA `InvokePattern` / 转发标准鼠标消息到对应托盘按钮节点（或临时定位唤出原生右键上下文菜单），保证微信/QQ/代理软件等原生托盘功能可用；若个别 UWP/高权限托盘无法远程触发，在展开面板顶部提供一键「临时唤出系统托盘」兜底入口。
2. **事件驱动增量同步**：
   - 仅在展开面板打开、顶部栏可见且检测到托盘通知树变更（UIA `StructureChanged` / 低频 2–5 秒节流校对）时刷新列表，后台绝不高频轮询；所有 UIA 遍历放在受控工作线程（带超时与代次丢弃），避免阻塞主 UI 线程。

#### 难点 4：顶部菜单栏随最大化窗口自动取色融合

1. **事件驱动触发（绝不每帧截屏）**：
   - 复用共享窗口监控（`SetWinEventHook` 监听 `EVENT_SYSTEM_FOREGROUND`、`EVENT_OBJECT_LOCATIONCHANGE`、`EVENT_SYSTEM_MOVESIZEEND`、`EVENT_OBJECT_DESTROY` 及 `WM_DWMCOLORIZATIONCOLORCHANGED`）。
   - 仅当某显示器上的前台窗口进入/切换为「最大化态」（`IsZoomed(hwnd)` 且覆盖该屏工作区，排除全屏独占窗口和桌面窗口）时，触发一次轻量取色任务（50ms 防抖，丢弃过时请求）。
2. **微成本标题栏采样算法（< 1ms，内存 < 16 KB）**：
   - **主路径**：针对最大化窗口的客户区/非客户区顶部边缘（避开顶部栏高度下方紧邻的 `y = 2..8px`、避开右侧最小化/最大化/关闭按钮区与左侧菜单图标区，取中间安全区 16–32 个水平采样点），使用轻量 `PrintWindow(..., PW_RENDERFULLCONTENT)` 或桌面窗口 DC 极小条带采样，计算中位色/众数主色（过滤掉高亮分割线或文字噪点）。
   - **快速兜底路径**：当窗口拒绝 `PrintWindow`（如受 DRM 保护或跨权限窗口）时，回退读取窗口 `DWMWA_CAPTION_COLOR` / `DWMWA_USE_IMMERSIVE_DARK_MODE` 及系统主题色，保证始终有自然协调的标题栏颜色。
3. **视觉融合与文字对比度自适应**：
   - 顶部栏窗口在最大化融合态下切换为平滑插值纯色（150ms RGB/OKLab 渐变过渡），与下方窗口标题栏无缝衔接；
   - 按 WCAG 相对亮度公式计算前景文字/图标颜色（`L > 0.45` 用深色字 `#1D1D1F`，否则用浅色字 `#F5F5F7`），确保任何深色 IDE（VSCode、Terminal）或浅色浏览器/Office 窗口下文字都清晰锐利；
   - 最大化窗口恢复/最小化/关闭时，150ms 平滑退回 DWM 毛玻璃态。

#### 难点 5：AppBar 多屏协调与全屏精准检测

- 每个启用顶部菜单栏的显示器各自创建独立顶部栏 `HWND`，并各自调用 `SHAppBarMessage(ABM_NEW / ABM_QUERYPOS / ABM_SETPOS)` 预留顶部工作区；退出或关闭对应屏顶部栏时必须 `ABM_REMOVE`。
- **全屏判定**结合 `SHQueryUserNotificationState` + 前台窗口矩形（`GetWindowRect`）完全覆盖所在显示器完整区域（`MONITORINFO.rcMonitor`）且非 `Progman`/`WorkerW`。进入全屏时采用 `SW_HIDE` 暂时隐藏顶部栏与 Dock，**不调用 `ABM_REMOVE`**（防止触发后台所有窗口重排抖动），退出全屏后即时恢复显示。

---

## 3. 工程蓝图（面向下游开发 Agent）

### 3.1 推荐仓库与源码目录结构

沿用 [WinLaunchpad](file:///c:/Software/WorkSpace/WinLaunchpad) 的清晰扁平单 crate 结构，避免过度拆分：

```text
WinDockFinder/
├── .github/workflows/
│   ├── ci.yml                     # PR / 分支：fmt、clippy、单元测试、Windows release 编译检查
│   └── release.yml                # 标签 vX.Y.Z：版本核对、构建、NSIS打包、Ed25519签名、GitHub Release、CNB 镜像与同步
├── assets/
│   ├── app.ico                    # 原创多尺寸应用图标
│   ├── app.manifest               # PerMonitorV2 DPI 感知 + CommonControls v6
│   ├── app.rc                     # Windows PE 版本资源与图标绑定
│   ├── release-notes.json         # 单一信源更新日志（关于页 + GitHub/CNB Release 共用）
│   └── update-public-key.hex      # 本项目专用 Ed25519 公钥（64 字符 hex）
├── installer/
│   └── windows.nsi                # NSIS 当前用户安装器（含开机启动勾选、任务栏恢复入口、静默更新支持）
├── scripts/
│   ├── sign-update.mjs            # 流式 SHA-256 + Ed25519 签名脚本（校验与公钥匹配）
│   └── sync-cnb-release.mjs       # CNB OpenAPI Release 与附件哈希校验同步
├── tools/
│   ├── package-windows.ps1        # 读取 EXE 版本资源并调用 makensis 打包
│   └── mem-probe.ps1              # 整组进程内存（WorkingSet/PrivateUsage）+ GDI/USER 句柄 100ms 采样脚本
├── build.rs                       # 自动查找 Windows SDK rc.exe 嵌入版本与图标资源
├── Cargo.toml
├── Cargo.lock
└── src/
    ├── main.rs                    # CLI 参数解析与角色分发（主进程/guard/worker/updater/命令转发）
    ├── app.rs                     # 主实例单实例锁、IPC 转发、全局状态机与生命周期协调
    ├── win32.rs                   # Win32 安全封装、RAII 句柄、DPI/显示器工具、FFI panic 防护宏
    ├── config.rs                  # 版本化配置模型、校验限域、原子落盘与备份恢复
    ├── log.rs                     # 有界轮转本地日志
    ├── ui.rs                      # 32bpp DIB 抗锯齿组件库（复用并升级自 WinLaunchpad，含 macOS 双主题 + 100ms Tooltip）
    ├── taskbar.rs                 # Win11 多屏任务栏发现、显隐、防回弹 Hook、恢复记录读写
    ├── guard.rs                   # 极简 Watchdog 守护进程（进程句柄监控、心跳超时、紧急快捷键恢复）
    ├── monitor.rs                 # 共享窗口状态跟踪（前台应用、最大化检测、全屏检测、运行应用列表）
    ├── dock/
    │   ├── mod.rs                 # Dock 窗口管理（双层毛玻璃+图标层、智能避让、边缘触发）
    │   ├── layout.rs              # 紧凑胶囊尺寸、悬停波浪曲线、物理弹簧插值、命中检测
    │   ├── render.rs              # 图标绘制、阴影、运行点、分隔线、回收站、100ms Tooltip
    │   ├── items.rs               # 固定项/运行项归并、拖放排序、文件夹堆栈、回收站交互
    │   └── icons.rs               # WIC 图标提取、LRU 8MB 字节预算缓存、代次丢弃
    ├── topbar/
    │   ├── mod.rs                 # 多屏 AppBar 顶部栏窗口、菜单调度与焦点归还
    │   ├── color_sync.rs          # 最大化窗口标题栏微条带采样、色彩平滑插值与对比度文字适配
    │   ├── tray_mirror.rs         # UIA 系统托盘发现、图标缓存、点击/右键透传、展开/固定状态管理
    │   ├── tray_popover.rs        # 顶部栏「展开按钮」垂直浮层（每行一个应用，点击切换顶部展示/收纳）
    │   ├── status.rs              # 时钟、输入法、网络、音量、电量事件驱动状态采集
    │   └── render.rs              # 顶部栏渲染（毛玻璃态 / 最大化纯色融合态）
    ├── settings/
    │   ├── mod.rs                 # macOS 风格左侧导航 + 右侧圆角卡片设置窗口、实时预览
    │   ├── general.rs             # 通用与快捷键录入页
    │   ├── dock_page.rs           # Dock 外观、智能隐藏与固定项管理页
    │   ├── topbar_page.rs         # 顶部栏、取色融合与托盘收纳管理页
    │   ├── appearance.rs          # 主题、毛玻璃材质、圆角与强调色页
    │   └── about.rs               # 版本信息、检测更新、一键更新与可展开更新日志页
    ├── tray.rs                    # 本应用标准 Windows 托盘图标与右键菜单（含 TaskbarCreated 重建）
    ├── startup.rs                 # FOLDERID_Startup 快捷方式 + StartupApproved 系统禁用检测
    ├── hotkey.rs                  # 全局热键注册与冲突回退
    └── update/
        ├── mod.rs                 # 版本检测（GitHub 优先 / CNB 回退）、流式下载、Ed25519 验签
        ├── http.rs                # WinHTTP 客户端（严格超时、流式写入、大小上限）
        └── apply.rs               # 原生自拷贝更新协调器（零 PowerShell 依赖，失败回滚）
```

### 3.2 数据目录与核心配置文件契约

所有用户数据统一保存在 `%LOCALAPPDATA%\WinDockFinder\`，写入统一采用**同目录临时文件 + `FlushFileBuffers` + `ReplaceFileW` 原子替换 + `.bak` 可读备份**。

1. **`config.json`**（主配置，带 `schema_version: 1`）：
   ```json
   {
     "schema_version": 1,
     "general": {
       "hide_taskbar_on_start": true,
       "auto_check_update": true,
       "toggle_dock_hotkey": "Ctrl+Alt+Shift+D",
       "toggle_topbar_hotkey": "Ctrl+Alt+Shift+T",
       "emergency_restore_hotkey": "Ctrl+Alt+Shift+F12",
       "winlaunchpad_path": ""
     },
     "dock": {
       "enabled": true,
       "display_mode": "primary",
       "hide_mode": "smart_maximized",
       "trigger_edge_px": 3,
       "show_delay_ms": 120,
       "hide_delay_ms": 250,
       "bottom_offset_dp": 6,
       "icon_size_dp": 48,
       "icon_gap_dp": 8,
       "magnify_enabled": true,
       "magnify_scale": 1.45,
       "show_running_apps": true,
       "show_running_dot": true,
       "show_trash": true
     },
     "topbar": {
       "enabled": true,
       "display_mode": "primary",
       "height_dp": 30,
       "adaptive_maximized_color": true,
       "adaptive_color_exclude": [],
       "show_active_app": true,
       "clock_format": "M月d日 E HH:mm",
       "status_items": ["ime", "network", "volume", "battery", "clock"],
       "pinned_tray_keys": [],
       "fullscreen_yield": true
     },
     "appearance": {
       "theme": "system",
       "backdrop": "acrylic",
       "opacity": 0.78,
       "dock_corner_radius_dp": 18,
       "icon_roundness": "macos",
       "accent_color": "system",
       "reduce_motion": false
     }
   }
   ```
2. **`dock_items.json`**（固定应用与文件夹列表，独立持久化便于拖拽高频更新）：
   - 每条记录含 `id`、`kind`（`app` / `uwp` / `folder` / `file` / `winlaunchpad`）、`target`、`args`、`aumid`、`label`、`custom_icon`、`order`。
3. **`taskbar_recovery.json`**（任务栏原子恢复凭证）：
   - 含 `session_id`、`main_pid`、`explorer_pid`、`original_autohide_state`、`hidden_hwnds`、`hidden_at_unix`、`abnormal_pause`。

### 3.3 单 EXE CLI 协议表

| 命令 | 行为 | 退出码 |
| --- | --- | --- |
| `WinDockFinder.exe` | 若已有实例则拉起设置或提示；否则启动主实例并按配置加载双栏与隐藏任务栏 | `0` 正常，非 `0` 失败 |
| `WinDockFinder.exe --background` | 开机静默启动，进入桌面双栏与托盘常驻 | `0` |
| `WinDockFinder.exe --settings` | 转发至主实例打开设置窗口（未运行时先启动并打开设置） | `0` |
| `WinDockFinder.exe --toggle-dock` / `--toggle-topbar` | 切换 Dock / 顶部栏显示状态 | `0` |
| `WinDockFinder.exe --guard --parent-pid <pid>` | 内部启动极简任务栏守护保险丝 | `0` |
| `WinDockFinder.exe --restore-taskbar` | 独立紧急恢复 Windows 任务栏与原始自动隐藏状态（开始菜单快捷方式指向此命令） | `0` 成功，非 `0` 重试提示 |
| `WinDockFinder.exe --apply-update ...` | 原生更新协调器，完成旧进程等待、备份、静默安装、版本校验与回滚 | `0` 成功 |
| `WinDockFinder.exe --installer-refresh-startup` | NSIS 安装/升级阶段调用：仅在已开启启动项时更新快捷方式路径，尊重用户关闭及 `StartupApproved` 状态 | `0` |
| `WinDockFinder.exe --installer-remove-startup` | NSIS 卸载阶段调用：仅清理指向当前被卸载 EXE 的开机启动快捷方式 | `0` |

---

## 4. 内存预算、性能与防崩溃规范

### 4.1 整组进程分级预算（硬红线 ≤ 100 MB）

| 运行场景 | 整组进程目标（主 + 守护 + 辅助） | 关键控制指标 |
| --- | --- | --- |
| 登录后常驻空闲（双栏就绪） | WorkingSet ≤ **45 MB**，PrivateUsage ≤ **35 MB** | 60 秒平均空闲 CPU ≤ **0.5%**，0 活跃动画定时器 |
| Dock 持续悬停波浪动画 + 顶部栏显示 | WorkingSet ≤ **60 MB** | 60Hz 基线机交互帧间隔 P95 ≤ **20 ms** |
| 双栏 + 托盘展开浮层 + 设置窗口同时打开 | WorkingSet ≤ **80 MB** | 前台应用名称与最大化取色切换 P95 ≤ **150 ms** |
| 高压峰值（50+ 固定项、混合 DPI 双屏、流式更新） | WorkingSet & PrivateUsage **各自 ≤ 100 MB** | 达到 **85 MB 软水位**立即触发降载 |

### 4.2 显式预算与降载阶梯（不硬杀主进程）

1. **分池字节限额**：
   - 图标像素缓存（WIC 解码后 BGRA）：LRU 上限 **8 MB**；
   - Dock + 顶部栏 + 弹窗 DIB 渲染缓冲合计上限 **10 MB**；
   - 托盘镜像图标缓存上限 **2 MB**；设置窗口关闭时立即销毁所有 DIB 缓冲与临时字体。
2. **85 MB 软水位自动降载阶梯**：
   - 定期（每 10 秒及窗口关闭/资源分配后）采样自身与守护进程 `GetProcessMemoryInfo`；
   - 超过 85 MB 时依序：① 停止非可见图标预取 → ② 清理不可见/隐藏面板的 DIB 与 LRU 图标缓存至 50% → ③ 临时降级复杂动画与高频采样 → ④ 拒绝新大内存缓存分配并在日志中记录水位事件。
3. **CI / 压测专用门禁**：
   - 使用 `tools/mem-probe.ps1` 以 ≤ 100ms 间隔归集全部 `WinDockFinder` 相关 PID 的 `WorkingSetSize`、`PrivateUsage`、`PeakWorkingSetSize` 及 GDI/USER 句柄数；Job Object 内存限制仅用于压测环境与短时隔离工作进程，绝不施加到生产主 UI 进程。

---

## 5. 安装、更新、GitHub Actions 与 CNB 镜像

### 5.1 NSIS 安装与卸载

- 安装路径：`%LOCALAPPDATA%\Programs\WinDockFinder`（`RequestExecutionLevel user`，无需管理员权限）。
- 创建开始菜单项：**WinDockFinder** 与 **恢复 Windows 任务栏**（`--restore-taskbar`）。
- 安装向导默认勾选「随 Windows 登录启动」；升级时通过 `--installer-refresh-startup` 尊重用户此前在应用或任务管理器中关闭的选择。
- 安装/卸载前检测主进程窗口类 `WinDockFinderMain`，通知优雅退出并**等待任务栏恢复确认**后再覆盖或删除文件；卸载默认保留 `%LOCALAPPDATA%\WinDockFinder` 用户配置。

### 5.2 流式 Ed25519 签名更新（零 PowerShell 峰值）

沿用 [WinLaunchpad](file:///c:/Software/WorkSpace/WinLaunchpad) 的**极简双附件签名体系**，同时升级为**流式 SHA-256 验签 + 原生自拷贝更新协调器**：

- **发布附件**（每个正式标签 `vX.Y.Z` 必须与 `Cargo.toml`、EXE 版本资源、`assets/release-notes.json` 完全一致）：
  1. `WinDockFinder_<version>_windows_x64_setup.exe`
  2. `WinDockFinder_<version>_windows_x64_setup.exe.sig`
- **签名算法升级为流式安全格式**：
  - CI 计算安装包 SHA-256（32 字节），对 `b"WinDockFinder update v2\0" || filename || b"\0" || sha256_bytes` 执行 Ed25519 签名（64 字节 `.sig`）；
  - 客户端内置 `assets/update-public-key.hex`，下载安装包时使用 **64 KB 有限缓冲**流式写入当前用户私有暂存目录并同步计算 SHA-256，**整个下载与验签阶段内存增加 < 1 MB**，彻底消除整包读入内存的问题。
- **固定官方双渠道**：默认查询并下载 GitHub（`api.github.com` / `github.com`），网络失败自动回退腾讯云 CNB（`cnb.cool/crazy-figure/WinDockFinder`）；客户端始终拼装固定官方路径，忽略元数据中的外部跳转 URL；仅展示比当前版本更高的正式版本且安装包与 `.sig` 同时就绪的 Release。
- **一键更新原子流程**：用户点击更新 → 流式下载并 Ed25519 验签 → 刷盘未落盘配置 → 拷贝自身到临时目录并以 `--apply-update` 启动 → 主进程撤销顶部 AppBar、恢复 Windows 任务栏并退出 → 原生更新器等待旧进程退出 → 备份旧 EXE → 执行 `setup.exe /S /D=<InstallDir>` → 校验新版本并拉起新主程序 → 清理临时文件（任一步失败自动还原旧 EXE、保持任务栏可见并弹窗提示）。

### 5.3 CI 与 CNB 镜像流水线

- **源仓库**：`CrazyFigure/WinDockFinder`（GitHub）。
- **镜像仓库**：`crazy-figure/WinDockFinder`（CNB）。
- **工作流分工**：
  1. `ci.yml`：所有 PR 与普通 push 执行 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test` 与 Windows release 编译验证。
  2. `release.yml`：
     - `mirror` 任务：分支/标签变更时串行强制单向同步到 CNB（使用 `concurrency` 队列防止旧快照覆盖新镜像）；
     - `build` 任务：推送 `v*` 标签或手动 `workflow_dispatch` 时核对版本一致性、编译 release EXE、校验 PE 版本号、按固定 SHA-256 校验下载 NSIS 3.11 打包、调用 `scripts/sign-update.mjs` 生成 `.sig`、从 `assets/release-notes.json` 生成发布说明并发布 GitHub Release（重试时复用已上传安装包防止签名错配）；
     - `sync` 任务：核对 CNB 标签 commit 与当前构建一致后，调用 `scripts/sync-cnb-release.mjs` 同步 Release 文案及两份附件（按 SHA-256 幂等跳过未变附件）。
- **GitHub Secrets 规范**：
  - `CNB_TOKEN`：CNB 最小权限访问令牌（Git 推送 + Release/附件读写）；
  - `WINDOCKFINDER_UPDATE_PRIVATE_KEY`：本项目专用 Ed25519 PKCS#8 DER Base64 私钥（对应公钥入库至 `assets/update-public-key.hex`）。

---

## 6. 实施阶段与验收门禁

### 前置基建（P0-Prep，已完成）

- [x] 初始化本地 Git 仓库并关联 `https://github.com/CrazyFigure/WinDockFinder`。
- [x] 在 CNB 组织 `crazy-figure` 下创建公开空镜像仓库 `WinDockFinder`（`https://cnb.cool/crazy-figure/WinDockFinder`）。
- [x] 生成本项目专用 Ed25519 密钥对：公钥已写入 `assets/update-public-key.hex`，私钥已写入 GitHub Secret `WINDOCKFINDER_UPDATE_PRIVATE_KEY`。
- [x] 创建仅限 `crazy-figure/WinDockFinder` 仓库的最小权限 `CNB_TOKEN`（`repo-code` 读写 + `repo-release` 读写）并写入 GitHub Secret `CNB_TOKEN`，已完成 CNB OpenAPI 验权（HTTP 200）。

### 开发阶段划分

| 阶段 | 核心交付 | 完成验收条件（Gate） |
| --- | --- | --- |
| **P0 技术原型与实测** | Win11 任务栏隐藏 + 防回弹 Hook + 极简 Watchdog 守护原型；双层毛玻璃 Dock 原型；顶部栏 AppBar + 最大化取色原型；`tools/mem-probe.ps1` 内存采样脚本。 | 在两个不同 Win11 Build（含 23H2/24H2）与单/双屏混合 DPI 实测通过：强杀主进程 ≤ 5s 恢复任务栏、卡死 ≤ 15s 恢复、Explorer 重启后正常自愈、最大化窗口无底部 48px 死区、双栏原型内存 ≤ 45 MB。 |
| **P1 桌面接管与 Dock** | 单实例与 CLI 转发、登录启动（含 `StartupApproved` 检测）、原生托盘、完整 Dock（紧凑毛玻璃胶囊、智能避让最大化、波浪放大、固定/运行项归并、拖放排序、回收站、文件夹堆栈、多窗口菜单）。 | 重登自启生效；最大化软件时 Dock 自动隐藏、鼠标触底平滑呼出、还原后常驻；拖放文件到回收站及清空正常；交互帧间隔 P95 ≤ 20ms，常驻内存 ≤ 50 MB。 |
| **P2 顶部栏 Finder 与托盘收纳** | 多屏顶部菜单栏、产品菜单、前景应用名称、系统状态项、**最大化窗口自动取色融合**、**系统托盘 UIA 接管 + 展开按钮垂直面板（逐行点击固定顶部/取消收纳 + 原生点击/右键透传）**、全屏让位。 | 深/浅色应用最大化时顶部栏 150ms 内平滑融合且文字高对比可读；展开面板可自由勾选/取消应用固定到顶部且重启保留；第三方托盘应用左/右键交互可用；全屏游戏/视频零遮挡零抖动。 |
| **P3 macOS 风格设置页** | 基于 `ui.rs` 实现 5 页 macOS 风格设置窗口（通用、Dock、顶部栏、外观、关于）、实时预览、100ms Tooltip、快捷键冲突校验、键盘无障碍操作、配置原子保存与损坏回滚。 | 全部自定义项生效且重启保留；关闭设置窗后内存回落；深浅主题、100%/150%/200% 混合 DPI 下文字与图标清晰无锯齿、无热区错位。 |
| **P4 安装包、一键更新与 CI/CNB** | `build.rs` 资源嵌入、`installer/windows.nsi`、原生自拷贝更新协调器、流式 Ed25519 验签、`assets/release-notes.json`、`ci.yml` + `release.yml` + `sync-cnb-release.mjs`。 | 真机验证干净安装、卸载保留配置、用户关闭开机启动后升级不强行开启、GitHub 不可用自动回退 CNB、篡改包验签拦截、实际跨版本一键更新（全程峰值 ≤ 100 MB 且任务栏正常过渡）、CNB 镜像附件哈希与 GitHub 一致。 |
| **P5 发布候选（RC）压测** | 24 小时连续运行压测报告、异常故障注入记录、真机录屏/截图与已验证 Win11 Build 清单。 | 连续运行 24h、200 次 Dock 显隐 + 100 次顶部栏/托盘展开开关 + 反复最大化取色与 DPI 切换后，内存与 GDI/USER 句柄无持续泄漏，峰值严格 ≤ 100 MB，零崩溃。 |
