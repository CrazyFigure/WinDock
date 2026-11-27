Unicode true
RequestExecutionLevel user
!include "MUI2.nsh"
!include "WinMessages.nsh"

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\WinDock"
!define RECOVERY_FILE "$LOCALAPPDATA\WinDock\taskbar_recovery.json"

Name "WinDock"
OutFile "${SETUP_EXE}"
InstallDir "$LOCALAPPDATA\Programs\WinDock"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
SetCompressor /SOLID lzma
VIProductVersion "${PE_VERSION}"
VIAddVersionKey /LANG=2052 "ProductName" "WinDock"
VIAddVersionKey /LANG=2052 "FileDescription" "WinDock 安装程序"
VIAddVersionKey /LANG=2052 "CompanyName" "CrazyFigure"
VIAddVersionKey /LANG=2052 "FileVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "Copyright (C) CrazyFigure"

!define MUI_ICON "${APP_ICON}"
!define MUI_WELCOMEPAGE_TITLE "安装 WinDock"
!define MUI_WELCOMEPAGE_TEXT "将 WinDock 安装到当前用户账户，并在开始菜单中添加程序和恢复 Windows 任务栏的入口。"
!define MUI_FINISHPAGE_RUN "$INSTDIR\WinDock.exe"
!define MUI_FINISHPAGE_RUN_TEXT "打开 WinDock"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

; 等待窗口所属进程退出，确保主程序已完成退出清理且守护进程释放 EXE。
!macro WaitForProcess ready close
    StrCmp $0 0 ${ready}
    System::Call 'user32::GetWindowThreadProcessId(p r0, *i .r1)i'
    ; SYNCHRONIZE 仅取得等待权限，不终止用户进程。
    System::Call 'kernel32::OpenProcess(i 0x00100000, i 0, i r1)p.r2'
    StrCmp $2 0 blocked
    !if ${close} == 1
        ; 让应用先撤销顶部栏、恢复 Windows 任务栏并保存配置。
        System::Call 'user32::PostMessageW(p r0, i ${WM_CLOSE}, p 0, p 0)i'
    !endif
    System::Call 'kernel32::WaitForSingleObject(p r2, i 10000)i.r3'
    System::Call 'kernel32::CloseHandle(p r2)i'
    StrCmp $3 0 ${ready} blocked
!macroend

!macro RequireClosed
retry:
    StrCpy $4 0
    IfFileExists "${RECOVERY_FILE}" 0 check_legacy_recovery
    StrCpy $4 1
check_legacy_recovery:
    IfFileExists "$LOCALAPPDATA\WinDockFinder\taskbar_recovery.json" 0 find_main
    StrCpy $4 1
find_main:
    FindWindow $0 "WinDockMain"
    !insertmacro WaitForProcess wait_guard 1
wait_guard:
    FindWindow $0 "WinDockGuard"
    !insertmacro WaitForProcess find_legacy_main 0
find_legacy_main:
    FindWindow $0 "WinDockFinderMain"
    !insertmacro WaitForProcess wait_legacy_guard 1
wait_legacy_guard:
    FindWindow $0 "WinDockFinderGuard"
    !insertmacro WaitForProcess check_recovery 0
check_recovery:
    ; 异常退出可能遗留恢复记录，复用已有命令恢复任务栏后再替换程序。
    IfFileExists "${RECOVERY_FILE}" 0 check_legacy_recovery_restore
    IfFileExists "$INSTDIR\WinDock.exe" 0 blocked
    ExecWait '"$INSTDIR\WinDock.exe" --restore-taskbar' $0
    StrCmp $0 0 0 blocked
    IfFileExists "${RECOVERY_FILE}" blocked
    Goto check_taskbar
check_legacy_recovery_restore:
    IfFileExists "$LOCALAPPDATA\WinDockFinder\taskbar_recovery.json" 0 check_taskbar
    IfFileExists "$LOCALAPPDATA\Programs\WinDockFinder\WinDockFinder.exe" 0 check_taskbar
    ExecWait '"$LOCALAPPDATA\Programs\WinDockFinder\WinDockFinder.exe" --restore-taskbar' $0
check_taskbar:
    ; 只有程序接管过任务栏时，才要求安装前确认主任务栏已恢复。
    StrCmp $4 0 ready
    FindWindow $0 "Shell_TrayWnd"
    System::Call 'user32::IsWindowVisible(p r0)i.r1'
    StrCmp $1 0 blocked ready
blocked:
    IfSilent failed
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "请从系统托盘退出 WinDock，并确认 Windows 任务栏已恢复，再点击「重试」。" IDRETRY retry
failed:
    SetErrorLevel 2
    Abort
ready:
!macroend

Section "WinDock"
    SetShellVarContext current
    !insertmacro RequireClosed
    SetOutPath "$INSTDIR"
    ClearErrors
    File /oname=WinDock.exe "${APP_EXE}"
    IfErrors 0 application_written
    SetErrorLevel 3
    Abort "无法写入 WinDock.exe。请关闭 WinDock 后重试。"
application_written:
    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateDirectory "$SMPROGRAMS\WinDock"
    CreateShortCut "$SMPROGRAMS\WinDock\WinDock.lnk" "$INSTDIR\WinDock.exe"
    CreateShortCut "$SMPROGRAMS\WinDock\恢复 Windows 任务栏.lnk" "$INSTDIR\WinDock.exe" "--restore-taskbar"

    ; 已安装或运行过的用户沿用启动选择（兼容从旧版 WinDockFinder 迁移）。全新安装默认随 Windows 登录启动。
    ReadRegStr $0 HKCU "${UNINSTALL_KEY}" "InstallLocation"
    StrCmp $0 "" 0 refresh_startup
    IfFileExists "$LOCALAPPDATA\WinDock\config.json" refresh_startup
    IfFileExists "$LOCALAPPDATA\WinDock\config.json.bak" refresh_startup
    IfFileExists "$SMSTARTUP\WinDock.lnk" refresh_startup
    IfFileExists "$LOCALAPPDATA\WinDockFinder\config.json" check_legacy_startup
    IfFileExists "$LOCALAPPDATA\WinDockFinder\config.json.bak" check_legacy_startup
    IfFileExists "$SMSTARTUP\WinDockFinder.lnk" migrate_legacy_startup
    CreateShortCut "$SMSTARTUP\WinDock.lnk" "$INSTDIR\WinDock.exe" "--background"
    Goto register
check_legacy_startup:
    IfFileExists "$SMSTARTUP\WinDockFinder.lnk" migrate_legacy_startup
    Goto register
migrate_legacy_startup:
    Delete "$SMSTARTUP\WinDockFinder.lnk"
    CreateShortCut "$SMSTARTUP\WinDock.lnk" "$INSTDIR\WinDock.exe" "--background"
    Goto register
refresh_startup:
    ExecWait '"$INSTDIR\WinDock.exe" --installer-refresh-startup' $0
    StrCmp $0 0 register
    DetailPrint "已有开机启动项未能更新，可在 WinDock 设置中重新启用。"

register:
    ; 仅注册当前 Windows 用户的卸载入口。
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "WinDock"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "CrazyFigure"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\WinDock.exe"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
    WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '$\"$INSTDIR\Uninstall.exe$\" /S'
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
    SetShellVarContext current
    !insertmacro RequireClosed
    IfFileExists "$INSTDIR\WinDock.exe" 0 cleanup
    ExecWait '"$INSTDIR\WinDock.exe" --installer-remove-startup' $0
    StrCmp $0 0 cleanup
    DetailPrint "开机启动项未能清理。"

cleanup:
    ; 保留用户的配置和安装路径中的其他文件。
    Delete "$SMPROGRAMS\WinDock\WinDock.lnk"
    Delete "$SMPROGRAMS\WinDock\恢复 Windows 任务栏.lnk"
    RMDir "$SMPROGRAMS\WinDock"
    Delete "$INSTDIR\WinDock.exe"
    Delete "$INSTDIR\Uninstall.exe"
    DeleteRegKey HKCU "${UNINSTALL_KEY}"
    RMDir "$INSTDIR"
SectionEnd
