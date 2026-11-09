#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise classic-editor region selection, clipboard shortcuts, and tool switching on a private X11 display.
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

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $null
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

        Invoke-E2EHotkey $driver '2'
        Invoke-Drag $driver ($bounds.Left - 40) ($bounds.Top - 18) ($bounds.Left + 25) ($bounds.Top - 18)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt $baseline + 50 } 'the first arrow'
        $withArrow = $driver.RedCanvasPixels()
        Invoke-E2EHotkey $driver '1'
        Invoke-Drag $driver ($bounds.Left - 20) ($bounds.Top - 18) ($bounds.Left + 10) ($bounds.Top + 3) -Alt
        Invoke-E2EHotkey $driver 'c'
        $image = Wait-ClipboardImage $environment 30 21
        Assert-E2E (($image.Pixel(0, 0) -join ',') -eq '255,255,255,255') 'Alt-selection included the editable arrow'
        Invoke-E2EHotkey $driver '2'
        Invoke-Drag $driver ($bounds.Left - 20) ($bounds.Top - 18) ($bounds.Left - 25) ($bounds.Top + 55)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt $withArrow + 50 } 'an arrow drawn after switching from a region'
        Write-E2ELine 'Classic selection clipboard and Select-to-Arrow transitions passed'
    }
    catch {
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Remove-E2EWorkDirectory $work
}
