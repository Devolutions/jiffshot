#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify daily capture scheduling with a mock or real desktop portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir,
    [switch]$RealPortal
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

$PngEndMarker = [byte[]](0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)
$ScheduledClipboardText = 'clipboard must remain unchanged'

function Get-SafeCurrentMinute {
    while ($true) {
        $now = [DateTime]::Now
        if ($now.Second -lt 35) {
            return $now
        }
        Wait-E2ESeconds 1
    }
}

function Format-ScheduleDay {
    param([Parameter(Mandatory)][DateTime]$Time)
    '{0:0000}-{1:000}' -f $Time.Year, $Time.DayOfYear
}

function Set-ScheduleTime {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Value)
    $Driver.Click(195, 182)
    $Driver.SelectAll()
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq ':') {
            $Driver.Key('Shift_L', $true)
            $Driver.Press('semicolon')
            $Driver.Key('Shift_L', $false)
        }
        else {
            $Driver.Press([string]$character)
        }
    }
    $Driver.Sync()
}

function Show-ScheduleOptions {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.5
    $Driver.Click(215, 182)
    Wait-E2ESeconds 0.3
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.5
}

function Get-SavedImages {
    <# Completed Screenshot-*.png files in Directory, sorted by path. #>
    param([Parameter(Mandatory)][string]$Directory)
    $images = [System.Collections.Generic.List[string]]::new()
    foreach ($path in [System.IO.Directory]::GetFiles($Directory, 'Screenshot-*.png')) {
        $bytes = [System.IO.File]::ReadAllBytes($path)
        if ($bytes.Length -ge $PngEndMarker.Length -and
            [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[($bytes.Length - $PngEndMarker.Length)..($bytes.Length - 1)], $PngEndMarker)) {
            $images.Add($path)
        }
    }
    $images.Sort([System.StringComparer]::Ordinal)
    , $images.ToArray()
}

function Assert-SavedImage {
    <# Checks a 1280x720 PNG whose center pixel is within 8 of the expected RGB color. #>
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][int[]]$Expected)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $signature = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
    Assert-E2E ($bytes.Length -ge 24 -and [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[0..7], $signature)) "$Path is not a PNG"
    $width = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 16, 4))
    $height = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 20, 4))
    Assert-E2E ($width -eq 1280 -and $height -eq 720) "($width, $height)"
    $image = [JiffshotE2E.Png]::Decode($bytes)
    Assert-E2E ($image.Rgba.Length -eq $width * $height * 4) "$Path decoded to $($image.Rgba.Length) bytes"
    $color = $image.Pixel([int][Math]::Floor($width / 2), [int][Math]::Floor($height / 2))
    $close = $true
    for ($channel = 0; $channel -lt 3; $channel++) {
        if ([Math]::Abs($color[$channel] - $Expected[$channel]) -gt 8) {
            $close = $false
        }
    }
    Assert-E2E $close "($Path, [$($color[0..2] -join ', ')], ($($Expected -join ', ')))"
}

function Test-IdenticalCopy {
    param([string]$Source, [string]$Copied)
    if (-not [System.IO.File]::Exists($Copied)) {
        return $false
    }
    [System.Linq.Enumerable]::SequenceEqual([System.IO.File]::ReadAllBytes($Copied), [System.IO.File]::ReadAllBytes($Source))
}

function Test-RealScheduledCaptureIsCopiedByteIdentically {
    param([Parameter(Mandatory)][string]$Source, [Parameter(Mandatory)][string]$Destination)
    $copied = Join-Path $Destination ([System.IO.Path]::GetFileName($Source))
    Wait-E2EUntil { Test-IdenticalCopy $Source $copied } "the byte-identical copy of $([System.IO.Path]::GetFileName($Source))" 15
    Assert-E2E (Test-IdenticalCopy $Source $copied) "$copied must be byte-identical to $Source"
}

function Format-E2EJson {
    param([AllowNull()]$Value)
    ConvertTo-Json -InputObject $Value -Depth 16 -Compress
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = $null
if (-not $RealPortal) {
    $portal = New-MockPortal
    $portal.ScreenMode = 'hold-start'
}
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-schedule-ui-'
try {
    $runtimeDirectory = New-E2EPrivateDirectory (Join-Path $work 'runtime')
    $config = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')).FullName
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $automaticCopies = Join-Path $work 'automatic-copies'
    if ($RealPortal) {
        New-Item -ItemType Directory -Path $automaticCopies | Out-Null
    }
    $settings = Join-Path $config 'settings.json'
    $image = Join-Path $work 'red.png'
    [System.IO.File]::WriteAllBytes($image, (Get-E2ESolidPng 220 20 20))
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        if ($RealPortal) {
            $runtimeDirectory = $env:XDG_RUNTIME_DIR
        }
        $environment = New-E2EEnvironment $displayName $work $runtime -RuntimeDirectory $runtimeDirectory
        $now = Get-SafeCurrentMinute
        $scheduled = $now.ToString('HH:mm', [System.Globalization.CultureInfo]::InvariantCulture)
        $today = Format-ScheduleDay $now
        $original = [ordered]@{
            theme             = 'Light'
            capture_directory = $captures
            copy_to_clipboard = $true
            capture_cursor    = $false
            schedule          = $scheduled
            schedule_default  = $scheduled
            other             = [ordered]@{ keep = $true }
        }
        if ($RealPortal) {
            $original['linux_auto_copy_directory'] = $automaticCopies
        }
        Write-E2EJson $settings $original
        if ($RealPortal) {
            $null = Invoke-E2ECommand @('xclip', '-selection', 'clipboard') -Environment $environment -InputBytes ([System.Text.Encoding]::UTF8.GetBytes($ScheduledClipboardText)) -Check
        }

        $startApp = { Start-E2EProcess @($binaryPath, $image) $environment $work $appLog }

        $app = & $startApp
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        if ($RealPortal) {
            Wait-E2EUntil { (Get-SavedImages $captures).Count -eq 1 } 'daily screenshot' 25
            Assert-SavedImage (Get-SavedImages $captures)[0] @(0x20, 0x40, 0x60)
            Test-RealScheduledCaptureIsCopiedByteIdentically (Get-SavedImages $captures)[0] $automaticCopies
            Write-E2EPass 'test_real_scheduled_capture_is_copied_byte_identically'
            Wait-E2EUntil { $driver.RedCanvasPixels() -gt 2000 } 'original editor image'
            $clipboard = Invoke-E2ECommand @('xclip', '-selection', 'clipboard', '-o') -Environment $environment -CaptureOutput -Timeout 10 -Check
            Assert-E2E ([System.Text.Encoding]::UTF8.GetString($clipboard.Output) -ceq $ScheduledClipboardText) 'the clipboard must remain unchanged'
            Wait-E2ESeconds 2
            Assert-E2E ((Get-SavedImages $captures).Count -eq 1) 'daily capture must not repeat'
            Write-E2ELine 'Real portal saved one image without changing the editor or clipboard'
        }
        else {
            Wait-E2EUntil { $portal.ScreenStarts.Count -eq 1 } 'scheduled chooser'
            Assert-E2E ($portal.ScreenSelectionOptions[-1]['cursor_mode'].Value -eq 1) 'the saved capture setting must hide the portal cursor'
            Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) (Copy-E2EMap $original @{ linux_schedule_last_fired = $today })) 'the scheduled attempt must record today'
            $driver.Click(80, 15)
            $driver.Click(300, 15)
            $driver.Click(300, 45)
            Wait-E2ESeconds 0.5
            Assert-E2E ($portal.CaptureRequests -eq 1) 'another screenshot, scrolling capture, or recording must not start while the scheduled chooser is open'
            $scheduledSession, $scheduledRequest = $portal.ScreenStarts[-1]
            Send-PortalResponse $portal $scheduledRequest 1 @{}
            Wait-E2EUntil { $portal.Closes.Contains($scheduledSession) } 'cancelled scheduled session'
            Wait-E2ESeconds 1.5
            Assert-E2E ($portal.CaptureRequests -eq 1) 'a cancelled attempt must not repeat'
            Assert-E2E ((Get-SavedImages $captures).Count -eq 0) 'a cancelled scheduled capture must not save'
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = & $startApp
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the restarted editor' 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2ESeconds 1.5
            Assert-E2E ($portal.CaptureRequests -eq 1) "a restart must not repeat today's capture"
            Show-ScheduleOptions $driver
            Write-E2ETrace $driver 'schedule-enabled-options'
            $unchanged = Read-E2EJson $settings
            Set-E2EText $driver 195 182 '2500'
            $driver.Click(480, 182)
            Wait-E2ESeconds 0.3
            Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) $unchanged) 'invalid time must not overwrite saved preferences'

            $otherTime = if ($scheduled -cne '00:00') { '00:00' } else { '23:59' }
            Set-ScheduleTime $driver $otherTime
            $driver.Click(245, 182)
            $driver.Click(480, 182)
            Wait-E2EUntil { $null -eq (Read-E2EJson $settings)['schedule'] } 'disabling the daily schedule'
            $disabled = Read-E2EJson $settings
            $expectedDisabled = Copy-E2EMap $original @{ schedule = $null; schedule_default = $otherTime; linux_schedule_last_fired = $today }
            Assert-E2E (Test-E2EDeepEqual $disabled $expectedDisabled) (Format-E2EJson $disabled)
            $driver.Click(245, 182)
            $driver.Click(480, 182)
            Wait-E2EUntil { (Read-E2EJson $settings)['schedule'] -ceq $otherTime } 're-enabling the daily schedule'
            Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) (Copy-E2EMap $disabled @{ schedule = $otherTime })) 're-enabling must only restore the schedule'
            Wait-E2ESeconds 1
            Assert-E2E ($portal.CaptureRequests -eq 1) 'changing a schedule must not duplicate it'

            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $now = Get-SafeCurrentMinute
            $scheduled = $now.ToString('HH:mm', [System.Globalization.CultureInfo]::InvariantCulture)
            $today = Format-ScheduleDay $now
            $busySettings = Copy-E2EMap $original @{ schedule = $null; schedule_default = $scheduled }
            Write-E2EJson $settings $busySettings
            $app = & $startApp
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor for a busy schedule' 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2ESeconds 0.5
            $driver.Click(300, 45)
            Wait-E2EUntil { $portal.ScreenStarts.Count -eq 2 } 'recording chooser'
            Show-ScheduleOptions $driver
            $driver.Click(245, 182)
            $driver.Click(480, 182)
            Wait-E2EUntil { (Read-E2EJson $settings)['linux_schedule_last_fired'] -ceq $today } 'skipping the scheduled capture while recording'
            Assert-E2E ($portal.CaptureRequests -eq 2) 'a scheduled capture must not open another chooser during recording'
            Invoke-E2EScroll $driver up
            Wait-E2ESeconds 0.4
            $driver.Click(560, 140)
            $recordingSession = $portal.ScreenStarts[-1][0]
            Wait-E2EUntil { $portal.Closes.Contains($recordingSession) } 'cancelled recording'
            Wait-E2ESeconds 1.5
            Assert-E2E ($portal.CaptureRequests -eq 2) 'a busy skipped capture must not retry'
            Write-E2ELine 'Scheduled chooser, mutual exclusion, cancellation, restart, settings validation and busy-schedule behavior verified'
        }
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'schedule-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    if ($null -ne $portal) {
        Unregister-E2EService $portal
    }
    Remove-E2EWorkDirectory $work
}
