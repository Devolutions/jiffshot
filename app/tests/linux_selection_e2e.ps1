#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise editor clipboard imports, classic region selection, and tool switching on a private X11 display.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

function New-SelectionFixture {
    param([string]$Path)
    $width = 240
    $height = 140
    $stride = $width * 3 + 1
    $rows = [byte[]]::new($height * $stride)
    for ($y = 0; $y -lt $height; $y++) {
        for ($x = 0; $x -lt $width; $x++) {
            $offset = $y * $stride + 1 + $x * 3
            $red = $x -ge 60 -and $x -lt 120 -and $y -ge 40 -and $y -lt 80
            $rows[$offset] = 255
            $rows[$offset + 1] = $red ? 0 : 255
            $rows[$offset + 2] = $red ? 0 : 255
        }
    }
    [System.IO.File]::WriteAllBytes($Path, [JiffshotE2E.Png]::Encode($width, $height, 2, $rows))
}

function Get-RedBounds {
    param([JiffshotE2E.Frame]$Frame)
    $left = $Frame.Width
    $top = $Frame.Height
    $right = $bottom = -1
    for ($y = 0; $y -lt $Frame.Height; $y++) {
        for ($x = 0; $x -lt $Frame.Width; $x++) {
            $offset = $y * $Frame.Stride + $x * 4
            if ($Frame.Data[$offset + 2] -gt 240 -and $Frame.Data[$offset + 1] -lt 10 -and $Frame.Data[$offset] -lt 10) {
                $left = [Math]::Min($left, $x)
                $top = [Math]::Min($top, $y)
                $right = [Math]::Max($right, $x)
                $bottom = [Math]::Max($bottom, $y)
            }
        }
    }
    [pscustomobject]@{ Left = $left; Top = $top + 280; Width = $right - $left + 1; Height = $bottom - $top + 1 }
}

function Invoke-Drag {
    param([JiffshotE2E.X11Driver]$Driver, [int]$X1, [int]$Y1, [int]$X2, [int]$Y2, [switch]$Alt)
    $Driver.Move($X1, $Y1)
    if ($Alt) { $Driver.Key('Alt_L', $true) }
    $Driver.Button(1, $true)
    $Driver.Sync()
    Wait-E2ESeconds 0.1
    $Driver.Move($X2, $Y2)
    $Driver.Sync()
    Wait-E2ESeconds 0.1
    $Driver.Button(1, $false)
    if ($Alt) { $Driver.Key('Alt_L', $false) }
    $Driver.Sync()
}

function Invoke-NewFromClipboardShortcut {
    param([JiffshotE2E.X11Driver]$Driver)
    $Driver.Key('Control_L', $true)
    $Driver.Key('Shift_L', $true)
    $Driver.Press('v')
    $Driver.Key('Shift_L', $false)
    $Driver.Key('Control_L', $false)
    $Driver.Sync()
}

function Wait-ClipboardImage {
    param([System.Collections.IDictionary]$Environment, [int]$Width, [int]$Height)
    $script:clipboardImage = $null
    Wait-E2EUntil {
        $result = Invoke-E2ECommand @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-o') $Environment -CaptureOutput -Timeout 3
        if ($result.ExitCode -ne 0 -or $result.Output.Length -eq 0) { return $false }
        $image = [JiffshotE2E.Png]::Decode($result.Output)
        if ($image.Width -ne $Width -or $image.Height -ne $Height) { return $false }
        $script:clipboardImage = $image
        $true
    } "a ${Width}x${Height} clipboard image"
    $script:clipboardImage
}

function Get-VisiblePopups {
    param([string]$Display, [int]$MinimumHeight = 50)
    $tree = & xwininfo -display $Display -root -tree
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the private X11 display.' }
    foreach ($line in $tree) {
        if ($line -notmatch '^\s*(0x[0-9a-f]+) \(has no name\): \(\)\s+(\d+)x(\d+)' -or
            [int]$Matches[2] -lt 50 -or [int]$Matches[3] -lt $MinimumHeight) {
            continue
        }
        $id = $Matches[1]
        $info = (& xwininfo -display $Display -id $id 2>&1) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            if ($info -match 'No such window with id') { continue }
            throw "Could not inspect popup ${id}: $info"
        }
        if ($info.Contains('Map State: IsViewable')) { $id }
    }
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $clipboard = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-selection-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    Write-E2EJson (Join-Path $config 'settings.json') @{ theme = 'Light' }
    $source = Join-Path $work 'selection.png'
    New-SelectionFixture $source
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $environment = New-E2EEnvironment $server.Display $work $runtime
        $environment['JIFFSHOT_CLASSIC_UI'] = '1'
        $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the classic editor'
        $driver = New-E2EX11Driver $server.Display (Get-E2EWindowInfo $environment)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 2000 } 'the fixture image'

        $red = Join-Path $work 'clipboard-red.png'
        [System.IO.File]::WriteAllBytes($red, (Get-E2ESolidPng 220 20 20))
        $clipboardLog = Join-Path $work 'clipboard.log'
        $clipboard = Start-E2EProcess @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', '-quiet', $red) $environment $work $clipboardLog
        $image = Wait-ClipboardImage $environment 240 140
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '220,20,20,255') 'the private clipboard did not contain the new image'
        Invoke-NewFromClipboardShortcut $driver
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 10000 } 'New from clipboard to load the image'
        $reader = New-E2ETextReader
        Wait-E2EStatus $reader $driver 'New image from the clipboard'

        Stop-E2EProcess $clipboard
        $clipboard = $null
        $green = Join-Path $work 'clipboard-green.png'
        [System.IO.File]::WriteAllBytes($green, (Get-E2ESolidPng 20 220 20))
        $clipboard = Start-E2EProcess @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', '-quiet', $green) $environment $work $clipboardLog
        $image = Wait-ClipboardImage $environment 240 140
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '20,220,20,255') 'the private clipboard did not contain the replacement image'
        Invoke-NewFromClipboardShortcut $driver
        Wait-E2EStatus $reader $driver 'Save the current document'
        Assert-E2E ($driver.RedCanvasPixels() -gt 10000) 'an unsaved clipboard image was replaced'
        Write-E2ELine 'Classic New from clipboard imports image pixels and preserves unsaved images'

        Stop-E2EProcess $clipboard
        $clipboard = $null
        Close-E2EX11Driver $driver
        $driver = $null
        Stop-E2EProcess $app
        $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the reopened classic editor'
        $driver = New-E2EX11Driver $server.Display (Get-E2EWindowInfo $environment)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 2000 } 'the selection fixture'
        Invoke-E2EHotkey $driver '0'
        Wait-E2ESeconds 0.5
        $bounds = Get-RedBounds (Get-E2ECanvasFrame $driver)
        Assert-E2E ($bounds.Width -eq 60 -and $bounds.Height -eq 40) "unexpected fixture bounds: $bounds"
        $baseline = $driver.RedCanvasPixels()

        Invoke-E2EHotkey $driver '1'
        Invoke-Drag $driver ($bounds.Left - 10) ($bounds.Top - 10) ($bounds.Left + 70) ($bounds.Top + 50)
        Invoke-E2EHotkey $driver 'c'
        $image = Wait-ClipboardImage $environment 80 60
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '255,255,255,255') 'region copy lost the white margin'
        Assert-E2E (($image.Pixel(10, 10) -join ',') -eq '255,0,0,255') 'region copy lost the red pixels'

        $driver.Key('Control_L', $true)
        $driver.Key('Shift_L', $true)
        $driver.Press('c')
        $driver.Key('Shift_L', $false)
        $driver.Key('Control_L', $false)
        $driver.Sync()
        $null = Wait-ClipboardImage $environment 240 140
        Invoke-E2EHotkey $driver 'c'
        $null = Wait-ClipboardImage $environment 80 60
        Invoke-E2EHotkey $driver 'x'
        Wait-E2EUntil { $driver.RedCanvasPixels() -lt 50 } 'Cut to remove the selected red pixels'
        Invoke-E2EHotkey $driver 'z'
        Wait-E2EUntil { $driver.RedCanvasPixels() -ge $baseline * 0.95 } 'Undo to restore the cut pixels'
        Invoke-E2EHotkey $driver 'v'
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt $baseline + 500 } 'Paste to insert the copied region'
        Invoke-E2EHotkey $driver 'z'
        Wait-E2EUntil { $driver.RedCanvasPixels() -lt $baseline * 1.05 } 'Undo to remove the pasted region'

        Invoke-E2EHotkey $driver '3'
        Invoke-Drag $driver ($bounds.Left - 40) ($bounds.Top - 18) ($bounds.Left + 25) ($bounds.Top - 18)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt $baseline + 50 } 'the first arrow'
        $withArrow = $driver.RedCanvasPixels()
        Invoke-E2EHotkey $driver '1'
        Invoke-Drag $driver ($bounds.Left - 20) ($bounds.Top - 18) ($bounds.Left + 10) ($bounds.Top + 3) -Alt
        Invoke-E2EHotkey $driver 'c'
        $image = Wait-ClipboardImage $environment 30 21
        Assert-E2E ($image.CountColor(229, 57, 53, 255) -gt 0) 'region Copy omitted the visible editable arrow'
        Invoke-E2EHotkey $driver '3'
        Invoke-Drag $driver ($bounds.Left - 20) ($bounds.Top - 18) ($bounds.Left - 25) ($bounds.Top + 55)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt $withArrow + 50 } 'an arrow drawn after switching from a region'
        Write-E2ELine 'Classic selection clipboard and Select-to-Arrow transitions passed'

        Close-E2EX11Driver $driver
        $driver = $null
        Stop-E2EProcess $app
        $environment.Remove('JIFFSHOT_CLASSIC_UI')
        $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
        $sharedTitle = 'selection.png — Jiffshot Editor'
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title $sharedTitle) } 'the shared editor'
        $driver = New-E2EX11Driver $server.Display (Get-E2EWindowInfo $environment -Title $sharedTitle)
        Write-E2ETrace $driver 'toolbar-compact' 'selection'
        $script:moreButton = $null
        Wait-E2EUntil {
            $script:moreButton = Find-E2ELabel $reader $driver 'More'
            $null -ne $script:moreButton
        } 'the compact toolbar More button'
        $captionX = [Math]::Max(0, $script:moreButton[0] - 240)
        $captionY = [Math]::Max(0, $script:moreButton[1] - 10)
        $captionWidth = [Math]::Min(260, 800 - $captionX)
        Wait-E2EUntil {
            $captions = $reader.Recognize($driver.Capture($captionX, $captionY, $captionWidth, 25), 7, $false)
            $captions -match '\bSelect\b' -and $captions -match '\bArrow\b'
        } 'the Select and Arrow tools in the compact toolbar'
        $existingPopups = @(Get-VisiblePopups $server.Display)
        $driver.Click($script:moreButton[0], $script:moreButton[1])
        $driver.Sync()
        $script:morePopup = $null
        Wait-E2EUntil {
            $script:morePopup = @(Get-VisiblePopups $server.Display | Where-Object { $_ -notin $existingPopups }) | Select-Object -First 1
            $null -ne $script:morePopup
        } 'the More tools menu'
        $driver.Press('Escape')
        $driver.Sync()
        Wait-E2EUntil { $script:morePopup -notin @(Get-VisiblePopups $server.Display) } 'the More tools menu to close'
        $driver.Resize(1200, 700)
        $driver.Resize(900, 600)
        $clipboardImage = Join-Path $work 'clipboard-shared.png'
        [System.IO.File]::WriteAllBytes($clipboardImage, (Get-E2ESolidPng 20 200 40 96 48))
        $clipboard = Start-E2EProcess @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', '-quiet', $clipboardImage) $environment $work $clipboardLog
        $image = Wait-ClipboardImage $environment 96 48
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '20,200,40,255') 'the private clipboard did not contain the shared-editor fixture'
        $existingPopups = @(Get-VisiblePopups $server.Display)
        $fileMenu = Find-E2ELabel $reader $driver 'File'
        Assert-E2E ($null -ne $fileMenu) 'the shared editor did not show a File menu'
        $driver.Click($fileMenu[0], $fileMenu[1])
        $driver.Sync()
        $script:filePopup = $null
        Wait-E2EUntil {
            $script:filePopup = @(Get-VisiblePopups $server.Display | Where-Object { $_ -notin $existingPopups }) | Select-Object -First 1
            $null -ne $script:filePopup
        } 'the File menu to open'
        $info = (& xwininfo -display $server.Display -id $script:filePopup) -join "`n"
        if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the File menu popup.' }
        $x = [regex]::Match($info, 'Absolute upper-left X:\s*(-?\d+)')
        $y = [regex]::Match($info, 'Absolute upper-left Y:\s*(-?\d+)')
        $width = [regex]::Match($info, 'Width:\s*(\d+)')
        $height = [regex]::Match($info, 'Height:\s*(\d+)')
        if (-not ($x.Success -and $y.Success -and $width.Success -and $height.Success)) {
            throw "Could not read the File menu popup geometry: $info"
        }
        $popupWindow = [Convert]::ToInt64($script:filePopup, 16)
        $popupWidth = [int]$width.Groups[1].Value
        $popupHeight = [int]$height.Groups[1].Value
        $script:clipboardMenuItem = $null
        Wait-E2EUntil {
            $script:clipboardMenuItem = Find-E2ELabel $reader $driver 'New from clipboard' -Window $popupWindow -Width $popupWidth -Height $popupHeight
            $null -ne $script:clipboardMenuItem
        } 'the New from clipboard command in the File menu'
        if ($env:JIFFSHOT_GUI_TRACE) {
            $png = $driver.CaptureWindow($popupWindow, 0, 0, $popupWidth, $popupHeight).ToPng()
            [System.IO.File]::WriteAllBytes((Join-Path $env:JIFFSHOT_GUI_TRACE 'selection-file-menu.png'), $png)
        }
        $menuX = [int]$x.Groups[1].Value - $driver.OriginX + $script:clipboardMenuItem[0]
        $menuY = [int]$y.Groups[1].Value - $driver.OriginY + $script:clipboardMenuItem[1]
        $driver.Click($menuX, $menuY)
        $driver.Sync()
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title '• Untitled — Jiffshot Editor') } 'the new unsaved clipboard document'
        Wait-E2EUntil { $driver.Capture(0, 0, 800, 500).CountNear(20, 200, 40, 2) -gt 1000 } 'the clipboard pixels in the shared editor'
        Stop-E2EProcess $clipboard
        $replacement = Join-Path $work 'clipboard-replacement.png'
        [System.IO.File]::WriteAllBytes($replacement, (Get-E2ESolidPng 200 30 180 72 36))
        $clipboard = Start-E2EProcess @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', '-quiet', $replacement) $environment $work $clipboardLog
        $image = Wait-ClipboardImage $environment 72 36
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '200,30,180,255') 'the replacement fixture was not copied'
        Invoke-NewFromClipboardShortcut $driver
        Wait-E2EUntil {
            $text = $reader.Recognize($driver.Capture(0, 400, 800, 200), 6, $false)
            $text -match 'Save\s*the\s*current\s*document'
        } 'the shared editor clipboard shortcut to preserve the unsaved image'
        $existingTooltips = @(Get-VisiblePopups $server.Display -MinimumHeight 10)
        $driver.Move(150, 408)
        $driver.Sync()
        Wait-E2EUntil {
            $script:statusTooltip = @(Get-VisiblePopups $server.Display -MinimumHeight 10 | Where-Object { $_ -notin $existingTooltips }) | Select-Object -First 1
            $null -ne $script:statusTooltip
        } 'the full status warning tooltip'
        $info = (& xwininfo -display $server.Display -id $script:statusTooltip) -join "`n"
        if ($LASTEXITCODE -ne 0 -or $info -notmatch 'Width:\s*(\d+)') { throw 'Could not inspect the status tooltip width.' }
        $width = [int]$Matches[1]
        if ($info -notmatch 'Height:\s*(\d+)') { throw 'Could not inspect the status tooltip height.' }
        $height = [int]$Matches[1]
        $tooltipWindow = [Convert]::ToInt64($script:statusTooltip, 16)
        $script:tooltipText = ''
        try {
            Wait-E2EUntil {
                $script:tooltipText = $reader.Recognize($driver.CaptureWindow($tooltipWindow, 0, 0, $width, $height), 6, $false)
                $script:tooltipText -match 'Jiffshot\s*document'
            } 'the full save warning in the status tooltip' 12
        }
        catch [System.TimeoutException] {
            throw "AssertionError: the status tooltip did not show the full save warning: $script:tooltipText"
        }
        Assert-E2E ($driver.Capture(0, 0, 800, 500).CountNear(20, 200, 40, 2) -gt 1000) 'the shortcut replaced an unsaved image'
        Assert-E2E ($driver.Capture(0, 0, 800, 500).CountNear(200, 30, 180, 2) -lt 1000) 'the replacement clipboard image was displayed despite the warning'
        Write-E2ELine 'Shared editor File menu and shortcut import the clipboard image safely'
    }
    catch {
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        if ($null -ne $reader) { $reader.Dispose() }
        Stop-E2EProcess $clipboard
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Remove-E2EWorkDirectory $work
}
