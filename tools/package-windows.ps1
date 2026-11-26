param(
    [Parameter(Mandatory)][string]$Executable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../dist'),
    [string]$Makensis = 'makensis.exe',
    [string]$Icon = (Join-Path $PSScriptRoot '../assets/app.ico')
)

$ErrorActionPreference = 'Stop'
$AppExe = Get-Item -LiteralPath $Executable
$AppIcon = Get-Item -LiteralPath $Icon
$Info = $AppExe.VersionInfo
$Version = $Info.FileVersion
$FileVersion = "$($Info.FileMajorPart).$($Info.FileMinorPart).$($Info.FileBuildPart)"
$ProductVersion = "$($Info.ProductMajorPart).$($Info.ProductMinorPart).$($Info.ProductBuildPart)"
$PeVersion = "$Version.0"

# 统一安装包文件名和 PE 版本，原生更新器会比较安装包与安装后的应用版本。
if ($Version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or
    $FileVersion -cne $Version -or $ProductVersion -cne $Version -or
    $Info.FilePrivatePart -ne 0 -or $Info.ProductPrivatePart -ne 0) {
    throw 'WinDockFinder EXE 缺少匹配的正式版 PE 版本资源。'
}

$Output = New-Item -Path $OutputDirectory -ItemType Directory -Force
$SetupExe = Join-Path $Output.FullName "WinDockFinder_${Version}_windows_x64_setup.exe"
$Script = Join-Path $PSScriptRoot '../installer/windows.nsi'
$Compiler = (Get-Command -Name $Makensis -CommandType Application).Source
& $Compiler /V2 /WX /INPUTCHARSET UTF8 "/DAPP_EXE=$($AppExe.FullName)" "/DAPP_ICON=$($AppIcon.FullName)" "/DSETUP_EXE=$SetupExe" "/DAPP_VERSION=$Version" "/DPE_VERSION=$PeVersion" $Script | Out-Host
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $SetupExe -PathType Leaf)) {
    throw 'NSIS 安装包构建失败。'
}

Get-Item -LiteralPath $SetupExe
