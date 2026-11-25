# 生成 assets/app.ico：原创徽标 = 顶部菜单栏条 + 底部圆角胶囊 Dock + 三个应用方块。
# 用法：powershell -ExecutionPolicy Bypass -File tools\make_icon.ps1
Add-Type -AssemblyName System.Drawing

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function New-IconPng([int]$size) {
    $base = 256
    $bmp = New-Object System.Drawing.Bitmap $base, $base
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)

    # 外层渐变圆角方板
    $board = New-RoundedPath 8 8 240 240 56
    $c1 = [System.Drawing.Color]::FromArgb(255, 0x1E, 0x3A, 0x8A)
    $c2 = [System.Drawing.Color]::FromArgb(255, 0x00, 0xC6, 0xFF)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush ([System.Drawing.RectangleF]::new(8, 8, 240, 240)), $c1, $c2, 45.0
    $g.FillPath($brush, $board)

    # 顶部菜单栏
    $bar = New-RoundedPath 40 46 176 22 11
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 255, 255, 255))), $bar)

    # 底部 Dock 胶囊
    $dock = New-RoundedPath 52 158 152 56 28
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 255, 255, 255))), $dock)

    # 三个应用方块
    $colors = @(
        [System.Drawing.Color]::FromArgb(255, 0xFF, 0x5F, 0x57),
        [System.Drawing.Color]::FromArgb(255, 0xFE, 0xBC, 0x2E),
        [System.Drawing.Color]::FromArgb(255, 0x28, 0xC8, 0x40)
    )
    for ($i = 0; $i -lt 3; $i++) {
        $x = 72 + $i * 40
        $sq = New-RoundedPath $x 168 32 32 9
        $g.FillPath((New-Object System.Drawing.SolidBrush $colors[$i]), $sq)
        $sq.Dispose()
    }
    $board.Dispose(); $bar.Dispose(); $dock.Dispose(); $brush.Dispose()
    $g.Dispose()

    if ($size -ne $base) {
        $small = New-Object System.Drawing.Bitmap $size, $size
        $g2 = [System.Drawing.Graphics]::FromImage($small)
        $g2.InterpolationMode = 'HighQualityBicubic'
        $g2.SmoothingMode = 'AntiAlias'
        $g2.PixelOffsetMode = 'HighQuality'
        $g2.DrawImage($bmp, 0, 0, $size, $size)
        $g2.Dispose()
        $bmp.Dispose()
        $bmp = $small
    }
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    ,$ms.ToArray()
}

$sizes = @(16, 24, 32, 48, 64, 128, 256)
$images = @()
foreach ($s in $sizes) { $png = New-IconPng $s; $images += ,@($s, $png) }

$out = Join-Path $PSScriptRoot '..\assets\app.ico'
$fs = [System.IO.File]::Create($out)
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($img in $images) {
    $s = $img[0]; $data = $img[1]
    $dim = $s; if ($s -ge 256) { $dim = 0 }
    $bw.Write([byte]$dim)
    $bw.Write([byte]$dim)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([uint16]1)
    $bw.Write([uint16]32)
    $bw.Write([uint32]$data.Length)
    $bw.Write([uint32]$offset)
    $offset += $data.Length
}
foreach ($img in $images) { $bw.Write($img[1]) }
$bw.Flush()
$fs.Dispose()
Write-Host "wrote $out"
