//! 五个设置页的行定义（macOS 系统设置：左侧分组导航 + 右侧圆角卡片）。

use super::fields::Field;
use super::Page;

/// 右侧内容区的一行。
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// 分组小标题
    Section(&'static str),
    Toggle(Field),
    /// 右侧分段选项（Sub(row, 选项索引)）
    Choice(Field),
    Slider(Field),
    Hotkey(Field),
    /// 单行文本 + 「浏览」按钮
    Text(Field),
    /// 右侧按钮：(动作 id, 标签)。动作 id：1 打开启动文件夹，2 切换开机启动，9 恢复本页默认
    Button(u32, &'static str),
    /// 开机启动状态行（动态文案）+ 切换按钮
    Startup,
    /// 说明文字（自动换行）
    Info(String),
    /// Dock 固定项管理（Sub(row, 1..5)：上移/下移/移除/添加/重置）
    DockList,
    /// 托盘应用固定管理（Sub(row, 行号 + 10)）
    TrayList,
    /// 最大化取色排除名单
    Exclude,
    /// 关于与更新
    About,
}

pub fn rows(page: Page) -> Vec<Row> {
    match page {
        Page::General => vec![
            Row::Section("任务栏"),
            Row::Toggle(Field::HideTaskbar),
            Row::Toggle(Field::TaskbarAutohide),
            Row::Section("启动"),
            Row::Startup,
            Row::Toggle(Field::AutoCheckUpdate),
            Row::Section("快捷键"),
            Row::Hotkey(Field::HotkeyDock),
            Row::Hotkey(Field::HotkeyTopbar),
            Row::Hotkey(Field::HotkeyRestore),
            Row::Section("启动台联动"),
            Row::Text(Field::LaunchpadPath),
            Row::Button(9, "恢复本页默认设置"),
        ],
        Page::Dock => vec![
            Row::Toggle(Field::DockEnabled),
            Row::Choice(Field::DockDisplayMode),
            Row::Choice(Field::DockHideMode),
            Row::Choice(Field::TriggerEdge),
            Row::Choice(Field::ShowDelay),
            Row::Choice(Field::HideDelay),
            Row::Choice(Field::BottomOffset),
            Row::Section("图标"),
            Row::Choice(Field::IconSize),
            Row::Choice(Field::IconGap),
            Row::Choice(Field::IconRoundness),
            Row::Toggle(Field::Magnify),
            Row::Choice(Field::MagnifyScale),
            Row::Choice(Field::MagnifyRange),
            Row::Section("内容"),
            Row::Toggle(Field::ShowRunning),
            Row::Toggle(Field::ShowDot),
            Row::Toggle(Field::ShowWindowPreview),
            Row::Toggle(Field::ShowTrash),
            Row::Toggle(Field::ShowFolders),
            Row::DockList,
            Row::Button(9, "恢复本页默认设置"),
        ],
        Page::Topbar => vec![
            Row::Toggle(Field::TopbarEnabled),
            Row::Choice(Field::TopbarDisplayMode),
            Row::Choice(Field::TopbarHeight),
            Row::Section("取色融合"),
            Row::Toggle(Field::AdaptiveColor),
            Row::Choice(Field::ColorSpeed),
            Row::Exclude,
            Row::Section("菜单与状态"),
            Row::Toggle(Field::ShowActiveApp),
            Row::Choice(Field::ClockFormat),
            Row::Toggle(Field::StIme),
            Row::Toggle(Field::StNetwork),
            Row::Toggle(Field::StVolume),
            Row::Toggle(Field::StBrightness),
            Row::Toggle(Field::StBattery),
            Row::Toggle(Field::FullscreenYield),
            Row::Section("托盘应用"),
            Row::TrayList,
            Row::Button(9, "恢复本页默认设置"),
        ],
        Page::Appearance => vec![
            Row::Choice(Field::Theme),
            Row::Choice(Field::Opacity),
            Row::Section("Dock 胶囊"),
            Row::Choice(Field::DockRadius),
            Row::Section("强调色与动效"),
            Row::Choice(Field::AccentColor),
            Row::Toggle(Field::ReduceMotion),
            Row::Button(9, "恢复本页默认设置"),
        ],
        Page::About => vec![Row::About],
    }
}
